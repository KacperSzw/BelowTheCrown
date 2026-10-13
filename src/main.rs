use anyhow::{Context, Result, ensure};
use below_the_crown::{
    codex::{DecisionDriver, DecisionToken, ThinkRequest, WorkerOptions, safe_error},
    diagnostics,
    engine::Engine,
    host::{HostOptions, HostSession, Start},
    manager::{self, MenuOptions},
    model::ReasoningEffort,
    paths, player,
    settings::HostSettings,
};
use clap::{Args, Parser, Subcommand};
use ipnet::Ipv4Net;
use serde_json::{Value, json};
use std::{
    fs,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "below-the-crown",
    version,
    about = "A living castle in your terminal, with LAN multiplayer"
)]
struct Cli {
    /// Private save directory (defaults to XDG_DATA_HOME/below-the-crown).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Operation>,
}
#[derive(Subcommand)]
enum Operation {
    /// Open the host menu: Play, Settings, Saves, Diagnostics and Admin.
    Host(HostArgs),
    /// Open the same host menu for a running game without owning its lifetime.
    Manager {
        #[arg(long)]
        audio: bool,
    },
    /// Check native Codex, login, sandbox and available Luna efforts.
    Doctor(DoctorArgs),
    /// Save and stop the host in the selected data directory.
    Stop,
    #[command(hide = true)]
    LocalPlayer,
}
#[derive(Args, Default)]
struct WorkerArgs {
    #[arg(long)]
    codex_bin: Option<PathBuf>,
    #[arg(long)]
    sandbox_helper: Option<PathBuf>,
}
impl WorkerArgs {
    fn options(&self) -> WorkerOptions {
        WorkerOptions {
            codex_bin: self.codex_bin.clone(),
            sandbox_helper: self.sandbox_helper.clone(),
        }
    }
}
#[derive(Args, Default)]
struct HostArgs {
    /// Start without the menu; requires --bind and --subnet.
    #[arg(long)]
    headless: bool,
    #[arg(long)]
    bind: Option<Ipv4Addr>,
    #[arg(long)]
    subnet: Option<Ipv4Net>,
    #[arg(long,value_parser=clap::value_parser!(u16).range(1..))]
    port: Option<u16>,
    #[arg(long,value_parser=clap::value_parser!(u8).range(1..=32))]
    max_players: Option<u8>,
    #[arg(long,value_parser=clap::value_parser!(u8).range(1..=32))]
    max_agents: Option<u8>,
    /// Passive NPC thinking rate: 1, 5 or 15. Events remain immediate.
    #[arg(long,value_parser=parse_npc_update_rate)]
    npc_update_rate: Option<u8>,
    /// Luna reasoning effort; applies to NPCs and speech interpretation.
    #[arg(long, value_enum)]
    reasoning_effort: Option<ReasoningEffort>,
    #[arg(long,value_parser=["en","pl"])]
    language: Option<String>,
    #[arg(long)]
    scenario: Option<String>,
    #[arg(long,conflicts_with_all=["new","import_snapshot"])]
    save: Option<String>,
    #[arg(long, conflicts_with = "import_snapshot")]
    new: bool,
    #[arg(long)]
    import_snapshot: Option<PathBuf>,
    #[arg(long)]
    authorized_keys: Option<PathBuf>,
    #[arg(long)]
    audio: bool,
    #[command(flatten)]
    worker: WorkerArgs,
}
impl HostArgs {
    fn overrides(&self) -> Value {
        let mut value = json!({"language":self.language,"scenario":self.scenario,"bind":self.bind,"subnet":self.subnet,"port":self.port,
            "maxPlayers":self.max_players,"maxAgents":self.max_agents,"npcUpdateRate":self.npc_update_rate,"reasoningEffort":self.reasoning_effort});
        value
            .as_object_mut()
            .unwrap()
            .retain(|_, value| !value.is_null());
        value
    }
    fn start(&self) -> Start {
        self.save
            .as_ref()
            .map_or(if self.new { Start::New } else { Start::Resume }, |name| {
                Start::Save(name.clone())
            })
    }
    fn host_options(&self) -> HostOptions {
        HostOptions {
            worker: self.worker.options(),
            authorized_keys: self.authorized_keys.clone(),
            import_snapshot: self.import_snapshot.clone(),
        }
    }
}
#[derive(Args, Default)]
struct DoctorArgs {
    /// Also request a real decision (uses your Codex allowance).
    #[arg(long)]
    live: bool,
    #[arg(long, value_enum)]
    reasoning_effort: Option<ReasoningEffort>,
    #[command(flatten)]
    worker: WorkerArgs,
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{}", safe_error(format!("{error:#}")));
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let cli = Cli::parse();
    let directory = fs::canonicalize(paths::private_dir(
        &cli.data_dir.unwrap_or_else(paths::data_dir),
    )?)?;
    match cli.command.unwrap_or(Operation::Host(HostArgs::default())) {
        Operation::Host(options) if options.headless => {
            ensure!(
                options.bind.is_some() && options.subnet.is_some(),
                "Headless hosting requires --bind ADDRESS and --subnet CIDR"
            );
            let settings = HostSettings::load_with_overrides(&directory, &options.overrides())?;
            let mut session = HostSession::start(
                &directory,
                &settings,
                &options.start(),
                &options.host_options(),
            )
            .await?;
            println!(
                "BELOW THE CROWN\nssh -p {} {}\nManager: below-the-crown --data-dir '{}' manager",
                session.handle.info.port,
                session.handle.info.address,
                directory.display()
            );
            let result = tokio::select! {result=shutdown_signal()=>result,_=async{while !session.is_finished(){tokio::time::sleep(Duration::from_millis(250)).await;}}=>Ok(())};
            let closed = session.close().await;
            result?;
            closed
        }
        Operation::Host(options) => {
            manager::run(
                &directory,
                MenuOptions {
                    overrides: options.overrides(),
                    host: options.host_options(),
                    start: options.start(),
                    audio: options.audio,
                    attach_only: false,
                },
            )
            .await
        }
        Operation::Manager { audio } => {
            manager::run(
                &directory,
                MenuOptions {
                    audio,
                    attach_only: true,
                    ..Default::default()
                },
            )
            .await
        }
        Operation::Doctor(options) => doctor(&directory, options).await,
        Operation::Stop => {
            manager::request(&directory, "stop", json!({})).await?;
            println!("Shutdown requested.");
            Ok(())
        }
        Operation::LocalPlayer => player::run(&directory).await,
    }
}
async fn shutdown_signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {result=tokio::signal::ctrl_c()=>result?,_=term.recv()=>{}}
    Ok(())
}
fn parse_npc_update_rate(value: &str) -> std::result::Result<u8, String> {
    let rate = value
        .parse::<u8>()
        .map_err(|_| "NPC update rate must be 1, 5 or 15".to_owned())?;
    HostSettings::validate_npc_update_rate(rate).map_err(|error| error.to_string())?;
    Ok(rate)
}
async fn doctor(directory: &Path, options: DoctorArgs) -> Result<()> {
    let effort = options.reasoning_effort.unwrap_or_default();
    let mut worker =
        diagnostics::prepare(directory, "castle", "en", options.worker.options(), effort).await?;
    println!(
        "Native Codex: {}\nSandbox: {}\nLogin: ready\nModel: gpt-6-luna / {} / standard\nAvailable efforts: {}\nIsolation: private home, no host tools or instructions",
        worker.report["codex"].as_str().unwrap_or(""),
        worker.report["sandbox"].as_str().unwrap_or(""),
        effort.as_str(),
        worker
            .driver
            .supported_efforts()
            .iter()
            .map(|effort| effort.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let result:Result<()>=async {
        diagnostics::require_effort(&worker.report)?;
        if options.live {
            let engine=Engine::memory("castle","en",manager::seed())?;
            let observation=engine.observation(102,"Connectivity check. Return a brief valid decision to wait for five game seconds, with no dialogue or mental changes.")?;
            let cancel=CancellationToken::new();
            let request=ThinkRequest {actor_id:102,identity:Some(engine.npc_identity(102)?),token:DecisionToken{job_id:1,context_generation:0},
                branch:engine.world.branch.clone(),input:observation["input"].clone(),listener:false,reasoning_effort:effort};
            let mut decision=worker.driver.think(request,cancel.clone());
            let result=tokio::time::timeout(Duration::from_secs(120),async{loop{tokio::select!{
                answer=&mut decision=>break answer,
                Some(request)=worker.recall_requests.recv()=>{let _=request.reply.send(engine.recall(request.actor_id,&request.query));}
            }}}).await;
            cancel.cancel();drop(decision);
            let decision=result.context("The live Codex check timed out")??;
            below_the_crown::model::Decision::check(decision.decision.npc()?.clone())?;
            println!("Live decision: valid · {} tokens · {:.1}s",decision.tokens,decision.latency_ms/1000.);
        }
        Ok(())
    }.await;
    worker.driver.reset().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_checks_input_and_serializes_only_explicit_overrides() {
        assert!(Cli::try_parse_from(["btc", "host", "--new", "--save", "s"]).is_err());
        assert!(Cli::try_parse_from(["btc", "host", "--language", "de"]).is_err());
        for value in ["0", "33", "-1", "2.5"] {
            assert!(Cli::try_parse_from(["btc", "host", "--max-agents", value]).is_err());
        }
        for value in ["0", "2", "16", "5.5"] {
            assert!(Cli::try_parse_from(["btc", "host", "--npc-update-rate", value]).is_err());
        }
        for rate in HostSettings::NPC_UPDATE_RATES {
            assert!(
                Cli::try_parse_from(["btc", "host", "--npc-update-rate", &rate.to_string()])
                    .is_ok()
            );
        }
        for effort in ReasoningEffort::ALL {
            assert!(
                Cli::try_parse_from(["btc", "host", "--reasoning-effort", effort.as_str()]).is_ok()
            );
        }
        assert!(Cli::try_parse_from(["btc", "host", "--reasoning-effort", "ultra"]).is_err());
        assert_eq!(HostArgs::default().overrides(), json!({}));
        let overrides = HostArgs {
            reasoning_effort: Some(ReasoningEffort::Low),
            max_agents: Some(7),
            ..Default::default()
        }
        .overrides();
        assert_eq!(overrides, json!({"reasoningEffort":"low","maxAgents":7}));
        assert!(Cli::try_parse_from(["btc", "local-player", "--data-dir", "/tmp/fixture"]).is_ok());
    }
}
