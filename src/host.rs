//! Own only the host started by this menu. Existing hosts are attached through IPC.
use crate::{
    codex::{DecisionDriver, WorkerOptions},
    content::WorldContent,
    diagnostics,
    engine::Engine,
    manager::{self, IpcServer, Network},
    paths,
    runtime::HostInfo,
    settings::HostSettings,
    simulation::Simulation,
    ssh::{SshListener, SshOptions, SshServer},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::task::JoinHandle;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Start {
    #[default]
    Resume,
    New,
    Save(String),
}

#[derive(Clone, Default)]
pub struct HostOptions {
    pub worker: WorkerOptions,
    pub authorized_keys: Option<PathBuf>,
    pub import_snapshot: Option<PathBuf>,
}

pub struct HostSession {
    pub handle: crate::runtime::GameHandle,
    pub report: Value,
    ssh: SshServer,
    ipc: IpcServer,
    running: JoinHandle<Result<()>>,
    manifest: PathBuf,
    _lock: paths::ServerLock,
    closed: bool,
}

pub fn selected_network(settings: &HostSettings) -> Result<Network> {
    let available = manager::networks()?;
    if let Some(address) = settings.bind {
        if let Some(network) = available.iter().find(|network| network.address == address) {
            return Ok(Network {
                name: network.name.clone(),
                address,
                subnet: settings.subnet.unwrap_or(network.subnet),
            });
        }
        ensure!(
            address.is_loopback(),
            "The selected address is not an active local interface. Choose another interface in Settings."
        );
        return Ok(Network {
            name: "localhost".into(),
            address,
            subnet: settings.subnet.unwrap_or("127.0.0.0/8".parse()?),
        });
    }
    ensure!(
        settings.subnet.is_none(),
        "Select a LAN interface with the subnet"
    );
    available
        .into_iter()
        .next()
        .context("No local IPv4 interface")
}

pub fn idle_view(directory: &Path, settings: &HostSettings) -> Result<Value> {
    let (saves, world) = if directory.join("world.sqlite").exists() {
        let store = Store::open(directory)?;
        (store.save_summaries()?, store.load()?)
    } else {
        (vec![], None)
    };
    Ok(
        json!({"language":settings.language,"pendingLanguage":settings.language,"pendingScenario":settings.scenario,"settings":settings,
        "scenarioId":world.as_ref().map(|world|&world.scenario_id),"saves":saves,"actors":[],"clock":"—","paused":[],
        "model":crate::model::MODEL,"effort":settings.reasoning_effort,"maxAgents":settings.max_agents,"npcUpdateRate":settings.npc_update_rate,
        "metrics":{"active":0,"calls":0,"tokens":0},"error":null}),
    )
}

impl HostSession {
    pub async fn start(
        directory: &Path,
        settings: &HostSettings,
        start: &Start,
        options: &HostOptions,
    ) -> Result<Self> {
        settings.validate()?;
        let lock = paths::lock_server(directory)?;
        let network = selected_network(settings)?;
        let mut chosen = settings.clone();
        chosen.bind = Some(network.address);
        chosen.subnet = Some(network.subnet);
        let listener = SshListener::bind(
            directory,
            SshOptions {
                bind: network.address,
                port: chosen.port,
                subnet: network.subnet,
                maximum_players: usize::from(chosen.max_players),
                authorized_keys: options.authorized_keys.clone(),
            },
        )
        .await?;
        let mut ipc = IpcServer::bind(directory).await?;
        // Read and validate the selected world before touching its checkpoint.
        let candidate = if let Some(path) = &options.import_snapshot {
            Some(Store::parse_snapshot(&fs::read_to_string(path)?)?)
        } else if let Start::Save(name) = start {
            Some(Store::open(directory)?.snapshot(name)?)
        } else {
            None
        };
        let scenario = if let Some(candidate) = &candidate {
            candidate.world.scenario_id.clone()
        } else if *start == Start::Resume && directory.join("world.sqlite").exists() {
            Store::open(directory)?
                .load()?
                .map_or_else(|| chosen.scenario.clone(), |world| world.scenario_id)
        } else {
            chosen.scenario.clone()
        };
        WorldContent::load(&scenario)?;
        let worker = diagnostics::prepare(
            directory,
            &scenario,
            &chosen.language,
            options.worker.clone(),
            chosen.reasoning_effort,
        )
        .await?;
        let report = worker.report.clone();
        if let Err(error) = diagnostics::require_effort(&report) {
            worker.driver.reset().await;
            return Err(error);
        }
        let manifest = directory.join("server.json");
        let mut manifest_written = false;
        let prepared = (|| -> Result<Engine> {
            chosen.save(directory)?;
            paths::write_private(
                &manifest,
                &serde_json::to_vec(
                    &json!({"version":env!("CARGO_PKG_VERSION"),"pid":std::process::id(),"managerSocket":ipc.path,
                "address":network.address,"subnet":network.subnet,"port":listener.port,"fingerprint":listener.fingerprint,"knownHosts":listener.known_hosts}),
                )?,
                false,
            )?;
            manifest_written = true;
            let mut engine = Engine::open(
                directory,
                &chosen.scenario,
                &chosen.language,
                manager::seed(),
            )?;
            if let Some(path) = &options.import_snapshot {
                engine.save("before-import")?;
                engine.import_snapshot(path, &chosen.language)?;
            } else {
                match start {
                    Start::New => {
                        engine.save("before-retry")?;
                        engine.new_run(&chosen.scenario, &chosen.language, manager::seed())?;
                    }
                    Start::Save(name) => {
                        engine.save(if name == "before-restore" {
                            "before-restore-backup"
                        } else {
                            "before-restore"
                        })?;
                        engine.load(name, &chosen.language)?;
                    }
                    Start::Resume if chosen.language != engine.language => {
                        engine.save("before-language")?;
                        engine.load("before-language", &chosen.language)?;
                    }
                    Start::Resume => {}
                }
            }
            engine.set_pause(false)?;
            Ok(engine)
        })();
        let engine = match prepared {
            Ok(engine) => engine,
            Err(error) => {
                if manifest_written {
                    let _ = fs::remove_file(&manifest);
                }
                worker.driver.reset().await;
                return Err(error);
            }
        };
        let info = HostInfo {
            data_dir: directory.into(),
            address: network.address.to_string(),
            port: listener.port,
            maximum_players: usize::from(chosen.max_players),
        };
        let (mut simulation, handle) = Simulation::production(engine, info, worker);
        simulation.apply_host_settings(chosen)?;
        let ssh = listener.serve(handle.clone());
        ipc.serve(handle.clone());
        let running = tokio::spawn(simulation.run());
        Ok(Self {
            handle,
            report,
            ssh,
            ipc,
            running,
            manifest,
            _lock: lock,
            closed: false,
        })
    }

    pub fn is_finished(&self) -> bool {
        self.running.is_finished()
    }

    #[cfg(test)]
    pub(crate) async fn fixture(directory: &Path) -> Self {
        struct NoCalls;
        impl DecisionDriver for NoCalls {
            fn think(
                &self,
                _: crate::codex::ThinkRequest,
                _: tokio_util::sync::CancellationToken,
            ) -> futures::future::BoxFuture<'_, Result<crate::codex::ThinkResult>> {
                Box::pin(async { panic!("An empty fixture host must not call a model") })
            }
            fn reset(&self) -> futures::future::BoxFuture<'_, ()> {
                Box::pin(async {})
            }
        }
        let lock = paths::lock_server(directory).unwrap();
        let listener = SshListener::bind(
            directory,
            SshOptions {
                bind: std::net::Ipv4Addr::LOCALHOST,
                subnet: "127.0.0.0/8".parse().unwrap(),
                port: 0,
                maximum_players: 2,
                authorized_keys: None,
            },
        )
        .await
        .unwrap();
        let mut ipc = IpcServer::bind(directory).await.unwrap();
        let engine = Engine::open(directory, "castle", "en", 73).unwrap();
        let (simulation, handle) = Simulation::with_driver(
            engine,
            HostInfo {
                data_dir: directory.into(),
                address: "127.0.0.1".into(),
                port: listener.port,
                maximum_players: 2,
            },
            std::sync::Arc::new(NoCalls),
        );
        let manifest = directory.join("server.json");
        paths::write_private(&manifest, b"{}", false).unwrap();
        let ssh = listener.serve(handle.clone());
        ipc.serve(handle.clone());
        Self {
            handle,
            report: json!({}),
            ssh,
            ipc,
            running: tokio::spawn(simulation.run()),
            manifest,
            _lock: lock,
            closed: false,
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if !self.running.is_finished() {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                self.handle.request("stop", json!({})),
            )
            .await;
        }
        self.ssh.close().await;
        self.ipc.close().await;
        let _ = fs::remove_file(&self.manifest);
        match tokio::time::timeout(Duration::from_secs(15), &mut self.running).await {
            Ok(result) => result.context("The simulation task stopped")?,
            Err(_) => {
                self.running.abort();
                let _ = (&mut self.running).await;
                bail!("Host shutdown exceeded its deadline; inspect the last autosave")
            }
        }
    }
}

impl Drop for HostSession {
    fn drop(&mut self) {
        if !self.closed {
            // A completed startup dropped before the UI accepts it still owns its host.
            let handle = self.handle.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = handle.request("stop", json!({})).await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    #[tokio::test]
    async fn occupied_port_fails_before_touching_the_saved_castle() {
        let directory = tempfile::tempdir().unwrap();
        let engine = Engine::open(directory.path(), "castle", "en", 41).unwrap();
        let before = serde_json::to_value(&engine.world).unwrap();
        drop(engine);
        let occupied = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let settings = HostSettings {
            bind: Some(Ipv4Addr::LOCALHOST),
            subnet: Some("127.0.0.0/8".parse().unwrap()),
            port: occupied.local_addr().unwrap().port(),
            scenario: "siege".into(),
            ..Default::default()
        };
        assert!(
            HostSession::start(
                directory.path(),
                &settings,
                &Start::New,
                &HostOptions::default()
            )
            .await
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(Store::open(directory.path()).unwrap().load().unwrap()).unwrap(),
            before
        );
        assert!(!directory.path().join("server-settings.json").exists());
    }
    #[test]
    fn idle_menu_does_not_create_a_castle() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            idle_view(directory.path(), &HostSettings::default()).unwrap()["saves"],
            json!([])
        );
        assert!(!directory.path().join("world.sqlite").exists());
    }
}
