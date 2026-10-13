//! One pinned localhost SSH command for automatic windows and manual joining.
use crate::{codex::safe_error, diagnostics::Failure, manager, paths, ui};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tokio::io::AsyncReadExt;

pub struct LocalConnection {
    port: u16,
    known_hosts: PathBuf,
}
impl LocalConnection {
    pub fn new(directory: &Path, port: u16) -> Result<Self> {
        ensure!(port > 0, "No host is running: missing SSH port");
        Ok(Self {
            port,
            known_hosts: directory.join("ssh/known_hosts"),
        })
    }
    pub fn from_view(directory: &Path, view: &Value) -> Result<Self> {
        let port = view["ssh"]["port"]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .context("No host is running: missing SSH port")?;
        Self::new(directory, port)
    }
    pub fn args(&self) -> Result<Vec<OsString>> {
        let key_file = self
            .known_hosts
            .to_str()
            .context("The SSH host-key path needs valid UTF-8")?;
        let quoted = key_file.replace('\\', "\\\\").replace('"', "\\\"");
        Ok(vec![
            "-F".into(),
            "/dev/null".into(),
            "-tt".into(),
            "-p".into(),
            self.port.to_string().into(),
            "-o".into(),
            format!("UserKnownHostsFile=\"{quoted}\"").into(),
            "-o".into(),
            "GlobalKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=8".into(),
            "-o".into(),
            "ConnectionAttempts=1".into(),
            "127.0.0.1".into(),
        ])
    }
    pub fn command(&self) -> Result<String> {
        Ok(std::iter::once(OsString::from("ssh"))
            .chain(self.args()?)
            .map(|arg| shell_quote(&arg.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" "))
    }
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-/_.:=@".contains(c))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

/// `false` means the caller should display the canonical connection command.
pub fn launch(directory: &Path, view: &Value) -> Result<bool> {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Ok(false);
    }
    let Some(terminal) = ["kitty", "foot", "gnome-terminal", "xterm"]
        .iter()
        .find_map(|name| paths::find_executable(name))
    else {
        return Ok(false);
    };
    if paths::find_executable("ssh").is_none() {
        return Ok(false);
    }
    LocalConnection::from_view(directory, view)?.args()?;
    let executable = std::env::current_exe()?;
    let mut args: Vec<OsString> = Vec::new();
    match terminal.file_name().and_then(|s| s.to_str()) {
        Some("kitty") | Some("foot") => {
            args.extend(["--title".into(), "Below the Crown · player".into()])
        }
        Some("gnome-terminal") => args.push("--".into()),
        Some("xterm") => args.push("-e".into()),
        _ => {}
    }
    args.extend([
        executable.into_os_string(),
        "--data-dir".into(),
        directory.as_os_str().into(),
        "local-player".into(),
    ]);
    let mut child = if let (Some(launcher), Some(workspace)) = (
        paths::find_executable("workstation-desktop"),
        native_workspace(),
    ) {
        Command::new(launcher)
            .args(["launch", "--workspace", &workspace, "--background", "--"])
            .arg(terminal)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?
    } else {
        Command::new(terminal)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?
    };
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(true)
}

pub async fn run(directory: &Path) -> Result<()> {
    let language = crate::settings::HostSettings::load(directory)
        .map_or_else(|_| "en".into(), |settings| settings.language);
    let text = |source: &str| crate::localization::tr(&language, source, &[]);
    let result = connect(directory).await;
    if let Err(error) = result {
        let failure = Failure::from_error(&error);
        eprintln!(
            "\n{}\n{}\n\n{}: {}",
            text(&failure.summary),
            text(&failure.action),
            text("Details"),
            ui::clean(&failure.details, false)
        );
        if io::stdin().is_terminal() {
            eprint!("\n{}", text("Press Enter to close this player window."));
            io::stderr().flush()?;
            let _ = io::stdin().read_line(&mut String::new());
        }
    }
    Ok(())
}

async fn connect(directory: &Path) -> Result<()> {
    let view = manager::request(directory, "view", json!({})).await?;
    let connection = LocalConnection::from_view(directory, &view)?;
    let ssh = paths::find_executable("ssh")
        .context("An SSH client is unavailable. Install OpenSSH to play locally.")?;
    let mut child = tokio::process::Command::new(ssh)
        .args(connection.args()?)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stderr = child.stderr.take().expect("piped SSH stderr");
    let errors = tokio::spawn(async move {
        let mut retained = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stderr.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            let keep = n.min(4096usize.saturating_sub(retained.len()));
            retained.extend_from_slice(&chunk[..keep]);
        }
        Ok::<_, io::Error>(retained)
    });
    let status = child.wait().await?;
    let errors = errors.await??;
    ensure!(
        status.success(),
        "{}",
        if errors.is_empty() {
            format!("The SSH player connection closed unsuccessfully ({status})")
        } else {
            safe_error(String::from_utf8_lossy(&errors))
        }
    );
    Ok(())
}

fn native_workspace() -> Option<String> {
    let hyprctl = paths::find_executable("hyprctl")?;
    let result = Command::new(hyprctl)
        .args(["clients", "-j"])
        .output()
        .ok()?;
    let clients: Value = serde_json::from_slice(&result.stdout).ok()?;
    let mut pid = std::process::id();
    let mut parents = HashSet::new();
    for _ in 0..24 {
        parents.insert(u64::from(pid));
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        pid = stat
            .rsplit_once(") ")?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        if pid == 0 {
            break;
        }
    }
    clients.as_array()?.iter().find(|client| {
        client["pid"]
            .as_u64()
            .is_some_and(|pid| parents.contains(&pid))
    })?["workspace"]["id"]
        .as_i64()
        .map(|id| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_and_automatic_connections_pin_the_same_host() {
        let connection = LocalConnection::from_view(
            Path::new("/tmp/King's castle"),
            &json!({"ssh":{"port":2237}}),
        )
        .unwrap();
        let args = connection.args().unwrap();
        assert_eq!(args.last().unwrap(), "127.0.0.1");
        assert!(args.contains(&OsString::from("StrictHostKeyChecking=yes")));
        assert!(args.contains(&OsString::from(
            "UserKnownHostsFile=\"/tmp/King's castle/ssh/known_hosts\""
        )));
        let command = connection.command().unwrap();
        assert!(command.contains("2237"));
        assert!(command.contains("King'\"'\"'s castle"));
        assert!(LocalConnection::from_view(Path::new("/tmp"), &json!({"ssh":{"port":0}})).is_err());
    }
}
