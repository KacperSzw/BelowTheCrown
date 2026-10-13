//! The same non-paid checks power startup, the host menu and `doctor`.
use crate::{
    codex::{CodexDriver, DecisionDriver, RecallRequest, WorkerOptions, WorkerSpec, safe_error},
    model::ReasoningEffort,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tokio::sync::mpsc;

pub struct PreparedWorker {
    pub driver: Arc<CodexDriver>,
    pub options: WorkerOptions,
    pub recall_sender: mpsc::Sender<RecallRequest>,
    pub recall_requests: mpsc::Receiver<RecallRequest>,
    pub report: Value,
}

pub async fn prepare(
    directory: &Path,
    scenario: &str,
    language: &str,
    options: WorkerOptions,
    effort: ReasoningEffort,
) -> Result<PreparedWorker> {
    let (recall_sender, recall_requests) = mpsc::channel(64);
    let driver = Arc::new(CodexDriver::new(
        directory.into(),
        scenario,
        language,
        options.clone(),
        recall_sender.clone(),
    )?);
    let report = match check(directory, &options, driver.as_ref(), effort).await {
        Ok(report) => report,
        Err(error) => {
            driver.reset().await;
            return Err(error);
        }
    };
    Ok(PreparedWorker {
        driver,
        options,
        recall_sender,
        recall_requests,
        report,
    })
}

pub async fn check(
    directory: &Path,
    options: &WorkerOptions,
    driver: &dyn DecisionDriver,
    effort: ReasoningEffort,
) -> Result<Value> {
    let spec = WorkerSpec::prepare(directory, options)?;
    let sandbox = spec.probe().await?;
    let mut report = driver.inspect().await?;
    report["effortSupported"] = json!(driver.supported_efforts().contains(&effort));
    report["codex"] = json!(spec.codex);
    report["sandbox"] = json!(sandbox);
    report["effort"] = json!(effort);
    report["isolation"] = json!("Private worker home; no host tools or instructions");
    Ok(report)
}

pub fn require_effort(report: &Value) -> Result<()> {
    ensure!(
        report["effortSupported"] == true,
        "Luna does not support the selected reasoning effort '{}'. Choose an available effort in Settings.",
        report["effort"].as_str().unwrap_or("unknown")
    );
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub summary: String,
    pub action: String,
    pub details: String,
}

impl Failure {
    pub fn from_error(error: &anyhow::Error) -> Self {
        Self::from_details(&format!("{error:#}"))
    }
    pub fn from_details(details: &str) -> Self {
        let lower = details.to_ascii_lowercase();
        let (summary, action) = if lower.contains("already owns")
            || lower.contains("server already")
        {
            (
                "A game is already running in this data directory.",
                "Use Play to join it. Leave the existing host running.",
            )
        } else if lower.contains("address already in use") || lower.contains("could not bind") {
            (
                "The game could not open its network port.",
                "Another game or application may use this port. Choose another port in Settings and retry.",
            )
        } else if lower.contains("credentials")
            || lower.contains("no login")
            || lower.contains("authentication")
            || lower.contains("login")
        {
            (
                "Codex login is not ready.",
                "Follow the login command in Details, then retry Diagnostics.",
            )
        } else if lower.contains("sandbox")
            || lower.contains("bubblewrap")
            || lower.contains("user namespace")
        {
            (
                "The isolated NPC worker could not start.",
                "Check the sandbox guidance in Details, then retry Diagnostics.",
            )
        } else if lower.contains("codex executable") || lower.contains("native codex") {
            (
                "The native Codex application is unavailable.",
                "Install a compatible Codex CLI, then retry Diagnostics.",
            )
        } else if lower.contains("reasoning")
            || lower.contains("luna descriptor")
            || lower.contains("model descriptor")
        {
            (
                "The selected Luna configuration is unavailable.",
                "Run Diagnostics to refresh model choices, then choose an available effort in Settings.",
            )
        } else if lower.contains("interface")
            || lower.contains("subnet")
            || lower.contains("lan address")
        {
            (
                "The selected network is unavailable.",
                "Choose an active LAN interface or localhost in Settings, then retry.",
            )
        } else if lower.contains("server-settings")
            || lower.contains("settings are")
            || lower.contains("invalid server settings")
        {
            (
                "Saved settings could not be read.",
                "Check the settings file shown in Details, correct it, then retry.",
            )
        } else if lower.contains("host key")
            || lower.contains("host identification")
            || lower.contains("host key verification")
        {
            (
                "The player could not verify this game's identity.",
                "Retry Play from the host menu to use the game's own host-key file.",
            )
        } else if lower.contains("permission denied") {
            (
                "Access to the game was denied.",
                "Check the host's authorized keys and file permissions in Diagnostics.",
            )
        } else if lower.contains("connection refused")
            || lower.contains("no host is running")
            || lower.contains("connection closed")
            || lower.contains("connection reset")
        {
            (
                "The player could not connect to the running game.",
                "Return to the host menu, check Diagnostics, then retry Play.",
            )
        } else if lower.contains("timed out") {
            (
                "The game did not answer in time.",
                "Check Diagnostics and the selected network, then retry.",
            )
        } else {
            (
                "The requested action could not finish.",
                "Open Details to see the cause, correct it, then retry.",
            )
        };
        Self {
            summary: summary.into(),
            action: action.into(),
            details: safe_error(details),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failures_keep_the_cause_and_give_an_action() {
        let cases = [
            (
                "Could not bind localhost SSH: Address already in use",
                "port",
                "Settings",
            ),
            ("The private Codex worker has no login", "login", "login"),
            (
                "ssh: connect to host 127.0.0.1 port 2222: Connection refused",
                "connect",
                "Play",
            ),
            ("Host key verification failed", "identity", "host-key"),
            (
                "The selected address is not an active local interface",
                "network",
                "interface",
            ),
        ];
        for (cause, summary, action) in cases {
            let failure = Failure::from_details(cause);
            assert!(failure.summary.contains(summary));
            assert!(failure.action.contains(action));
            assert!(failure.details.contains(cause));
        }
        assert!(
            !Failure::from_details("Login failed sk-secret123456")
                .details
                .contains("sk-secret123456")
        );
    }
}
