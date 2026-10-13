use crate::{content::WorldContent, localization, model::ReasoningEffort, paths::write_private};
use anyhow::{Context, Result, ensure};
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, net::Ipv4Addr, path::Path};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HostSettings {
    pub language: String,
    pub scenario: String,
    pub bind: Option<Ipv4Addr>,
    pub subnet: Option<Ipv4Net>,
    pub port: u16,
    pub max_players: u8,
    pub max_agents: u8,
    pub npc_update_rate: u8,
    pub reasoning_effort: ReasoningEffort,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            language: "en".into(),
            scenario: "castle".into(),
            bind: None,
            subnet: None,
            port: 2222,
            max_players: 8,
            max_agents: 5,
            npc_update_rate: 5,
            reasoning_effort: ReasoningEffort::default(),
            extra: BTreeMap::new(),
        }
    }
}

impl HostSettings {
    pub const MAX_PLAYERS: u8 = 32;
    pub const MAX_AGENTS: u8 = 32;
    pub const NPC_UPDATE_RATES: [u8; 3] = [1, 5, 15];
    pub const FIELDS: [&'static str; 9] = [
        "language",
        "scenario",
        "bind",
        "subnet",
        "port",
        "maxPlayers",
        "maxAgents",
        "npcUpdateRate",
        "reasoningEffort",
    ];

    pub fn load(directory: &Path) -> Result<Self> {
        Self::load_with_overrides(directory, &json!({}))
    }

    pub fn load_with_overrides(directory: &Path, overrides: &Value) -> Result<Self> {
        let path = directory.join("server-settings.json");
        let mut value = match fs::read(&path) {
            Ok(bytes) => {
                ensure!(bytes.len() <= 65536, "Server settings are too large");
                serde_json::from_slice(&bytes).context("Cannot read server settings")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(error) => return Err(error.into()),
        };
        Self::merge(&mut value, overrides)?;
        Self::from_value(value)
    }

    pub fn patched(&self, patch: &Value) -> Result<Self> {
        let mut value = serde_json::to_value(self)?;
        Self::merge(&mut value, patch)?;
        Self::from_value(value)
    }

    fn merge(value: &mut Value, patch: &Value) -> Result<()> {
        let object = value.as_object_mut().context("Invalid server settings")?;
        let patch = patch.as_object().context("Settings must be an object")?;
        for (key, value) in patch {
            ensure!(
                Self::FIELDS.contains(&key.as_str()),
                "Unknown setting: {key}"
            );
            object.insert(key.clone(), value.clone());
        }
        Ok(())
    }

    fn from_value(value: Value) -> Result<Self> {
        let settings: Self = serde_json::from_value(value).context("Invalid server settings")?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn validate(&self) -> Result<()> {
        localization::validate_language(&self.language)?;
        WorldContent::load(&self.scenario)?;
        validate_network(self.bind, self.subnet)?;
        ensure!(self.port > 0, "Choose a nonzero SSH port");
        ensure!(
            (1..=Self::MAX_PLAYERS).contains(&self.max_players),
            "Choose a player limit between 1 and {}.",
            Self::MAX_PLAYERS
        );
        ensure!(
            (1..=Self::MAX_AGENTS).contains(&self.max_agents),
            "Maximum agents must be between 1 and {}",
            Self::MAX_AGENTS
        );
        Self::validate_npc_update_rate(self.npc_update_rate)?;
        Ok(())
    }

    pub fn validate_npc_update_rate(rate: u8) -> Result<()> {
        ensure!(
            Self::NPC_UPDATE_RATES.contains(&rate),
            "NPC update rate must be 1, 5 or 15"
        );
        Ok(())
    }

    pub fn save(&self, directory: &Path) -> Result<()> {
        self.validate()?;
        write_private(
            &directory.join("server-settings.json"),
            &serde_json::to_vec(self)?,
            false,
        )
    }
}

pub fn validate_network(bind: Option<Ipv4Addr>, subnet: Option<Ipv4Net>) -> Result<()> {
    let private = |ip: Ipv4Addr| ip.is_private() || ip.is_loopback() || ip.is_link_local();
    if let Some(bind) = bind {
        ensure!(
            private(bind),
            "Select a private LAN IPv4 address or localhost."
        );
        if let Some(subnet) = subnet {
            ensure!(
                subnet.contains(&bind),
                "The selected LAN address must belong to its subnet."
            );
        }
    }
    if let Some(subnet) = subnet {
        ensure!(
            private(subnet.network()) && private(subnet.broadcast()),
            "The guest subnet must stay inside a private LAN network."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_saved_settings_keep_defaults_unknown_fields_and_cli_precedence() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("server-settings.json"), br#"{"language":"pl","reasoningEffort":"low","maxAgents":"invalid","extension":{"active":true}}"#).unwrap();
        assert!(HostSettings::load(directory.path()).is_err());
        let settings =
            HostSettings::load_with_overrides(directory.path(), &json!({"maxAgents":7})).unwrap();
        assert_eq!(settings.language, "pl");
        assert_eq!(settings.max_agents, 7);
        assert_eq!(settings.reasoning_effort, ReasoningEffort::Low);
        assert_eq!(settings.port, HostSettings::default().port);
        settings.save(directory.path()).unwrap();
        assert_eq!(HostSettings::load(directory.path()).unwrap(), settings);
        assert_eq!(settings.extra["extension"], json!({"active":true}));
    }

    #[test]
    fn invalid_patches_preserve_active_and_saved_settings() {
        let directory = tempfile::tempdir().unwrap();
        let settings = HostSettings::default();
        settings.save(directory.path()).unwrap();
        let before = fs::read(directory.path().join("server-settings.json")).unwrap();
        for patch in [
            json!({"language":"de"}),
            json!({"scenario":"missing"}),
            json!({"port":0}),
            json!({"maxPlayers":0}),
            json!({"maxPlayers":33}),
            json!({"maxAgents":0}),
            json!({"maxAgents":33}),
            json!({"npcUpdateRate":2}),
            json!({"reasoningEffort":"ultra"}),
            json!({"reasoningEffort":null}),
            json!({"maxAgents":2.5}),
            json!({"speed":99}),
            json!({"bind":"8.8.8.8"}),
            json!({"bind":"192.168.5.9","subnet":"192.168.6.0/24"}),
        ] {
            assert!(settings.patched(&patch).is_err(), "Accepted {patch}");
            assert_eq!(
                fs::read(directory.path().join("server-settings.json")).unwrap(),
                before
            );
        }
        for effort in ReasoningEffort::ALL {
            for rate in HostSettings::NPC_UPDATE_RATES {
                let next = settings
                    .patched(&json!({"reasoningEffort":effort,"npcUpdateRate":rate,"maxAgents":7}))
                    .unwrap();
                assert_eq!(next.reasoning_effort, effort);
                assert_eq!(next.npc_update_rate, rate);
            }
        }
    }

    #[test]
    fn rejected_write_preserves_the_original_file() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.json");
        fs::write(&original, b"original").unwrap();
        std::os::unix::fs::symlink(&original, directory.path().join("server-settings.json"))
            .unwrap();
        assert!(HostSettings::default().save(directory.path()).is_err());
        assert_eq!(fs::read(original).unwrap(), b"original");
    }
}
