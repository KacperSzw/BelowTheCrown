use crate::paths::{find_executable, private_dir, write_private};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

const HELPER: &[u8] = include_bytes!(env!("BTC_SANDBOX_HELPER"));

#[derive(Clone, Debug, Default)]
pub struct WorkerOptions {
    pub codex_bin: Option<PathBuf>,
    pub sandbox_helper: Option<PathBuf>,
}

pub struct WorkerSpec {
    pub helper: PathBuf,
    pub args: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    pub codex: PathBuf,
    pub private_home: PathBuf,
    version: String,
}

fn native(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    std::io::Read::read_exact(&mut file, &mut magic).is_ok() && magic == *b"\x7fELF"
}

pub fn resolve_codex(options: &WorkerOptions) -> Result<PathBuf> {
    let selected = options.codex_bin.clone().or_else(|| find_executable("codex")).context("Install Codex CLI, then run codex login. An explicit native path is accepted with --codex-bin.")?;
    let selected = fs::canonicalize(&selected)
        .with_context(|| format!("Cannot resolve Codex executable {}", selected.display()))?;
    if native(&selected) {
        return Ok(selected);
    }
    let (package, triple) = if cfg!(target_arch = "aarch64") {
        ("codex-linux-arm64", "aarch64-unknown-linux-musl")
    } else {
        ("codex-linux-x64", "x86_64-unknown-linux-musl")
    };
    for ancestor in selected.ancestors().skip(1).take(8) {
        let roots = [
            ancestor.to_path_buf(),
            ancestor.join(package),
            ancestor.join("node_modules/@openai").join(package),
            ancestor.join("@openai").join(package),
        ];
        for root in roots {
            for suffix in [
                format!("vendor/{triple}/bin/codex"),
                format!("vendor/{triple}/codex/codex"),
            ] {
                let candidate = root.join(suffix);
                if native(&candidate) {
                    return Ok(fs::canonicalize(candidate)?);
                }
            }
        }
    }
    bail!(
        "Codex launcher could not be resolved to its native executable. Use --codex-bin with the native Codex binary."
    )
}

pub fn login_command(private_home: &Path) -> String {
    let quoted = private_home.to_string_lossy().replace('\'', "'\\''");
    format!(
        "CODEX_HOME='{quoted}' codex -c 'cli_auth_credentials_store=\"file\"' login --device-auth"
    )
}

fn copy_auth(home: &Path, source: &Path) -> Result<()> {
    let private = home.join("auth.json");
    if private.is_file() {
        ensure!(
            !fs::symlink_metadata(&private)?.file_type().is_symlink(),
            "Private credentials cannot be a symlink"
        );
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600))?;
        return Ok(());
    }
    let original = source.join("auth.json");
    if original.is_file() {
        write_private(&private, &fs::read(&original)?, false)?;
        return Ok(());
    }
    bail!(
        "File-backed Codex credentials are unavailable. A keyring login may still exist. Log into the game's private home:\n{}\nOmit --device-auth to use browser login.",
        login_command(home)
    );
}

pub fn worker_config() -> String {
    let disabled = [
        "apps",
        "plugins",
        "remote_plugin",
        "browser_use",
        "browser_use_external",
        "computer_use",
        "hooks",
        "memories",
        "multi_agent",
        "multi_agent_v2",
        "image_generation",
        "goals",
        "sleep_tool",
        "shell_tool",
        "shell_snapshot",
        "unified_exec",
        "view_image",
        "skill_search",
        "skill_mcp_dependency_install",
        "code_mode",
        "code_mode_host",
        "tool_suggest",
        "workspace_dependencies",
        "fast_mode",
        "code_mode_only",
        "current_time_reminder",
        "token_budget",
        "deferred_executor",
        "send_message_to_user_async",
        "recommended_plugins",
    ];
    format!(
        "model = \"gpt-6-luna\"\nmodel_reasoning_effort = \"high\"\nservice_tier = \"default\"\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\nproject_doc_max_bytes = 0\ncli_auth_credentials_store = \"file\"\ninclude_permissions_instructions = false\ninclude_collaboration_mode_instructions = false\ninclude_environment_context = false\ninclude_apps_instructions = false\nthread_unload_delay_secs = 0\n[agents]\nenabled = false\n[skills]\ninclude_instructions = false\n[skills.bundled]\nenabled = false\n[cloud.skills]\nenabled = false\n[orchestrator.mcp]\nenabled = false\n[tools.experimental_request_user_input]\nenabled = false\n[features]\n{}\nskip_host_skill_discovery = true\n[tools.update_plan]\nenabled = false\n",
        disabled.map(|name| format!("{name} = false")).join("\n")
    )
}

fn helper(options: &WorkerOptions) -> Result<PathBuf> {
    if let Some(path) = &options.sandbox_helper {
        ensure!(
            native(path),
            "The sandbox override must be a native ELF executable"
        );
        return Ok(fs::canonicalize(path)?);
    }
    ensure!(
        !HELPER.is_empty(),
        "This development build has no bundled sandbox helper. Build with BTC_SANDBOX_HELPER or supply --sandbox-helper. Release binaries bundle it."
    );
    let checksum = format!("{:x}", Sha256::digest(HELPER));
    let cache = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("below-the-crown")
        .join("helpers")
        .join(&checksum[..24]);
    private_dir(&cache)?;
    let path = cache.join("bwrap");
    let valid = fs::read(&path).is_ok_and(|bytes| Sha256::digest(&bytes) == Sha256::digest(HELPER))
        && fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_symlink());
    if !valid {
        write_private(&path, HELPER, true)?;
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

impl WorkerSpec {
    pub fn prepare(directory: &Path, options: &WorkerOptions) -> Result<Self> {
        let codex = resolve_codex(options)?;
        let version = std::process::Command::new(&codex)
            .arg("--version")
            .output()
            .context("Cannot execute native Codex CLI")?;
        ensure!(version.status.success(), "Native Codex CLI could not start");
        let text = String::from_utf8_lossy(&version.stdout);
        let re = regex::Regex::new(r"(\d+)\.(\d+)\.(\d+)").expect("constant version regex");
        let captures = re
            .captures(&text)
            .context("Cannot read Codex CLI version")?;
        let parts = (
            captures[1].parse::<u32>()?,
            captures[2].parse::<u32>()?,
            captures[3].parse::<u32>()?,
        );
        ensure!(
            parts >= (0, 160, 1),
            "Codex CLI 0.160.1 or newer is required"
        );
        let helper = helper(options)?;
        let worker = private_dir(&directory.join("worker"))?;
        let private_home = private_dir(&worker.join("codex"))?;
        let fake_home = private_dir(&worker.join("home"))?;
        private_dir(&fake_home.join(".config"))?;
        private_dir(&fake_home.join(".agents"))?;
        let work = private_dir(&worker.join("work"))?;
        let source = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(std::env::temp_dir)
                    .join(".codex")
            });
        copy_auth(&private_home, &source)?;
        let version = format!("{}.{}.{}", parts.0, parts.1, parts.2);
        // Catalog discovery happens in this private worker, never by copying a host model cache.
        let valid_catalog = catalog_matches_login(&private_home, &version);
        if !valid_catalog && private_home.join("models_cache.json").exists() {
            fs::remove_file(private_home.join("models_cache.json"))?;
        }
        let config = if valid_catalog {
            format!(
                "model_catalog_json = \"/home/codex/.codex/npc-models.json\"\n{}",
                worker_config()
            )
        } else {
            worker_config()
        };
        write_private(&private_home.join("config.toml"), config.as_bytes(), false)?;
        let mut args: Vec<OsString> = [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--die-with-parent",
            "--new-session",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        for path in [
            "/nix/store",
            "/run/current-system/sw",
            "/usr",
            "/lib",
            "/lib64",
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/passwd",
            "/etc/nsswitch.conf",
        ] {
            if Path::new(path).exists() {
                args.extend(["--ro-bind".into(), path.into(), path.into()]);
            }
        }
        for path in [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/ssl/certs/ca-bundle.crt",
        ] {
            if let Ok(real) = fs::canonicalize(path) {
                args.extend(["--ro-bind".into(), real.into_os_string(), path.into()]);
            }
        }
        args.extend([
            "--dir".into(),
            "/runtime".into(),
            "--dir".into(),
            "/home".into(),
            "--ro-bind".into(),
            codex.clone().into_os_string(),
            "/runtime/codex".into(),
            "--bind".into(),
            fake_home.into_os_string(),
            "/home/codex".into(),
            "--bind".into(),
            private_home.clone().into_os_string(),
            "/home/codex/.codex".into(),
            "--ro-bind".into(),
            work.into_os_string(),
            "/work".into(),
            "--chdir".into(),
            "/work".into(),
            "--".into(),
            "/runtime/codex".into(),
            "app-server".into(),
            "--strict-config".into(),
            "--listen".into(),
            "stdio://".into(),
        ]);
        let env = BTreeMap::from([
            (
                "PATH".into(),
                "/run/current-system/sw/bin:/usr/bin:/bin".into(),
            ),
            ("HOME".into(), "/home/codex".into()),
            ("CODEX_HOME".into(), "/home/codex/.codex".into()),
            ("LANG".into(), "C.UTF-8".into()),
            ("TERM".into(), "dumb".into()),
        ]);
        Ok(Self {
            helper,
            args,
            env,
            codex,
            private_home,
            version,
        })
    }
    pub fn has_catalog(&self) -> bool {
        catalog_matches_login(&self.private_home, &self.version)
    }
    pub fn freeze_discovered_catalog(&self) -> Result<()> {
        let cache = fs::read(self.private_home.join("models_cache.json"))
            .context("Private discovery did not supply a model descriptor")?;
        let catalog = sanitized_catalog(&cache, &self.version)?;
        write_private(&self.private_home.join("npc-models.json"), &catalog, false)?;
        let metadata = json!({"version":self.version,"authDigest":auth_digest(&self.private_home)?,"catalogDigest":format!("{:x}",Sha256::digest(&catalog))});
        write_private(
            &self.private_home.join("npc-models.meta.json"),
            &serde_json::to_vec(&metadata)?,
            false,
        )?;
        write_private(
            &self.private_home.join("config.toml"),
            format!(
                "model_catalog_json = \"/home/codex/.codex/npc-models.json\"\n{}",
                worker_config()
            )
            .as_bytes(),
            false,
        )
    }
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.helper);
        command.args(&self.args).env_clear().envs(&self.env);
        command
    }
    pub async fn probe(&self) -> Result<String> {
        let separator = self
            .args
            .iter()
            .rposition(|arg| arg == "--")
            .context("Invalid sandbox command")?;
        let mut command = Command::new(&self.helper);
        command
            .args(&self.args[..=separator])
            .args(["/runtime/codex", "--version"])
            .env_clear()
            .envs(&self.env)
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .context("Sandbox probe timed out")??;
        ensure!(
            output.status.success(),
            "Sandbox could not start. Linux must permit unprivileged user namespaces; check the application-specific AppArmor policy where applicable. {}",
            super::safe_error(String::from_utf8_lossy(&output.stderr))
        );
        Ok(String::from_utf8_lossy(&output.stdout).trim().into())
    }
}

fn auth_digest(home: &Path) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(fs::read(home.join("auth.json"))?)
    ))
}
fn catalog_matches_login(home: &Path, version: &str) -> bool {
    let valid = || -> Result<bool> {
        for name in ["npc-models.json", "npc-models.meta.json", "auth.json"] {
            let metadata = fs::symlink_metadata(home.join(name))?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Private catalog must use regular files"
            );
        }
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.join("npc-models.meta.json"))?)?;
        let bytes = fs::read(home.join("npc-models.json"))?;
        Ok(metadata["version"] == version
            && metadata["authDigest"] == auth_digest(home)?
            && metadata["catalogDigest"] == format!("{:x}", Sha256::digest(bytes)))
    };
    valid().unwrap_or(false)
}

fn sanitized_catalog(cache: &[u8], version: &str) -> Result<Vec<u8>> {
    let cache: Value = serde_json::from_slice(cache)?;
    ensure!(
        cache["client_version"] == version
            && cache["identity"].as_str().is_some_and(|s| !s.is_empty())
            && cache["fetched_at"].is_string(),
        "Model descriptor lacks matching discovery metadata"
    );
    let models = cache["models"]
        .as_array()
        .context("Missing discovered models")?;
    let matches = models
        .iter()
        .filter(|m| m["slug"] == crate::model::MODEL)
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "Discovery must contain exactly one Luna descriptor"
    );
    let mut model = matches[0].clone();
    ensure!(
        model["context_window"].as_u64().is_some_and(|n| n > 0)
            && model["model_messages"].is_object()
            && model["supported_reasoning_levels"]
                .as_array()
                .is_some_and(|levels| levels
                    .iter()
                    .any(|level| level["effort"] == crate::model::EFFORT)),
        "Luna descriptor is incomplete or does not support high reasoning"
    );
    model["tool_mode"] = json!("direct");
    model["experimental_supported_tools"] = json!([]);
    model["shell_type"] = json!("disabled");
    model["apply_patch_tool_type"] = Value::Null;
    model["supports_search_tool"] = json!(false);
    model["multi_agent_version"] = Value::Null;
    model["include_skills_usage_instructions"] = json!(false);
    model["include_plugin_usage_instructions"] = json!(false);
    model["include_apps_usage_instructions"] = json!(false);
    model["model_messages"]["persistent_instructions"] = json!("");
    serde_json::to_vec(&json!({"models":[model]})).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"\x7fELFtest-fixture").unwrap();
    }
    #[test]
    fn resolves_native_nix_and_modern_npm_layouts_without_exposing_home() {
        let temp = tempfile::tempdir().unwrap();
        let native = temp.path().join("nix/store/codex/bin/codex");
        fixture(&native);
        assert_eq!(
            resolve_codex(&WorkerOptions {
                codex_bin: Some(native.clone()),
                sandbox_helper: None
            })
            .unwrap(),
            native
        );
        let launcher = temp
            .path()
            .join("npm/lib/node_modules/@openai/codex/bin/codex.js");
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        fs::write(&launcher, b"#!/usr/bin/env node").unwrap();
        let triple = if cfg!(target_arch = "aarch64") {
            "aarch64-unknown-linux-musl"
        } else {
            "x86_64-unknown-linux-musl"
        };
        let package = if cfg!(target_arch = "aarch64") {
            "codex-linux-arm64"
        } else {
            "codex-linux-x64"
        };
        let payload = temp.path().join(format!(
            "npm/lib/node_modules/@openai/{package}/vendor/{triple}/bin/codex"
        ));
        fixture(&payload);
        assert_eq!(
            resolve_codex(&WorkerOptions {
                codex_bin: Some(launcher),
                sandbox_helper: None
            })
            .unwrap(),
            payload
        );
    }
    #[test]
    fn private_refreshed_login_survives_missing_host_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        private_dir(&private).unwrap();
        write_private(
            &private.join("auth.json"),
            b"test-owned-private-auth",
            false,
        )
        .unwrap();
        copy_auth(&private, &temp.path().join("missing-host-home")).unwrap();
        assert_eq!(
            fs::read(private.join("auth.json")).unwrap(),
            b"test-owned-private-auth"
        );
    }
    #[test]
    fn imports_file_auth_and_gives_precise_private_login_guidance() {
        let temp = tempfile::tempdir().unwrap();
        let private = private_dir(&temp.path().join("private")).unwrap();
        let source = private_dir(&temp.path().join("source")).unwrap();
        let error = copy_auth(&private, &source).unwrap_err().to_string();
        assert!(error.contains("keyring"));
        assert!(error.contains("CODEX_HOME="));
        fs::write(source.join("auth.json"), b"test-owned-host-auth").unwrap();
        copy_auth(&private, &source).unwrap();
        assert_eq!(
            fs::metadata(private.join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[test]
    fn worker_configuration_forces_standard_tier_and_excludes_host_features() {
        let config = worker_config();
        for item in [
            "service_tier = \"default\"",
            "fast_mode = false",
            "apps = false",
            "shell_tool = false",
            "multi_agent = false",
            "project_doc_max_bytes = 0",
        ] {
            assert!(config.contains(item));
        }
    }
}
