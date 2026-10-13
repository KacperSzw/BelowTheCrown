use anyhow::{Context, Result, bail, ensure};
use below_the_crown::{
    codex::{
        CodexDriver, DecisionDriver, DecisionToken, RecallRequest, ThinkRequest, WorkerOptions,
        WorkerSpec, safe_error,
    },
    content::WorldContent,
    engine::Engine,
    localization,
    manager::{self, Link, Network, Setup, Start},
    paths,
    runtime::HostInfo,
    simulation::Simulation,
    ssh::{self, SshOptions},
    store::Store,
};
use clap::{Args, Parser, Subcommand};
use ipnet::Ipv4Net;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, IsTerminal},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "below-the-crown",
    version,
    about = "A living castle, played through SSH on your LAN"
)]
struct Cli {
    /// Private native save directory (defaults to XDG_DATA_HOME/below-the-crown).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Operation>,
}
#[derive(Subcommand)]
enum Operation {
    /// Choose a castle and LAN interface, then open its manager.
    Host(HostArgs),
    /// Open the private manager for a running host.
    Manager {
        #[arg(long)]
        audio: bool,
    },
    /// Check native Codex, file login, sandbox and standard model configuration.
    Doctor(DoctorArgs),
    /// Save and stop the host in the selected data directory.
    Stop,
}
#[derive(Args, Default)]
struct WorkerArgs {
    /// Override the native Codex executable (not a JavaScript launcher).
    #[arg(long)]
    codex_bin: Option<PathBuf>,
    /// Development override; release binaries already embed static bubblewrap.
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
    /// Run without the local manager; select a LAN address and subnet explicitly.
    #[arg(long)]
    headless: bool,
    /// Selected private IPv4 interface; localhost is also supported.
    #[arg(long)]
    bind: Option<Ipv4Addr>,
    /// Allowed private IPv4 subnet, checked before every SSH handshake.
    #[arg(long)]
    subnet: Option<Ipv4Net>,
    #[arg(long,value_parser=clap::value_parser!(u16).range(1..))]
    port: Option<u16>,
    #[arg(long,value_parser=clap::value_parser!(u8).range(1..=32))]
    max_players: Option<u8>,
    /// Maximum simultaneous NPC and speech-interpreter decisions.
    #[arg(long,value_parser=clap::value_parser!(u8).range(1..=32))]
    max_agents: Option<u8>,
    /// Passive NPC thinking rate: 1, 5 or 15. Events remain immediate.
    #[arg(long, value_parser=parse_npc_update_rate)]
    npc_update_rate: Option<u8>,
    #[arg(long,value_parser=["en","pl"])]
    language: Option<String>,
    #[arg(long)]
    scenario: Option<String>,
    /// Restore a named save, validated before changing the live castle.
    #[arg(long,conflicts_with_all=["new","import_snapshot"])]
    save: Option<String>,
    /// Start a fresh castle; the previous one is saved first.
    #[arg(long, conflicts_with = "import_snapshot")]
    new: bool,
    /// Import a legacy or native JSON snapshot into separate native storage.
    #[arg(long)]
    import_snapshot: Option<PathBuf>,
    /// Require a signed key listed here, while retaining the LAN restriction.
    #[arg(long)]
    authorized_keys: Option<PathBuf>,
    /// Optional quiet local music via pw-play. SSH sessions remain silent.
    #[arg(long)]
    audio: bool,
    #[command(flatten)]
    worker: WorkerArgs,
}
#[derive(Args, Default)]
struct DoctorArgs {
    /// Also request one real Luna/high decision (uses your Codex allowance).
    #[arg(long)]
    live: bool,
    #[command(flatten)]
    worker: WorkerArgs,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{}", safe_error(error));
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let cli = Cli::parse();
    let directory = paths::private_dir(&cli.data_dir.unwrap_or_else(paths::data_dir))?;
    let directory = fs::canonicalize(directory)?;
    match cli.command.unwrap_or(Operation::Host(HostArgs::default())) {
        Operation::Host(options) => host(&directory, options).await,
        Operation::Manager { audio } => {
            manager::run(Link::Remote(directory.clone()), &directory, false, audio).await
        }
        Operation::Doctor(options) => doctor(&directory, options).await,
        Operation::Stop => {
            manager::request(&directory, "stop", json!({})).await?;
            println!("Shutdown requested.");
            Ok(())
        }
    }
}

fn settings(directory: &Path) -> Result<Value> {
    let path = directory.join("server-settings.json");
    if !path.exists() {
        return Ok(json!({}));
    }
    let bytes = fs::read(path)?;
    ensure!(bytes.len() <= 65536, "Server settings are too large");
    let value: Value = serde_json::from_slice(&bytes)?;
    ensure!(value.is_object(), "Invalid server settings");
    Ok(value)
}
fn initial_agent_limit(cli: Option<u8>, settings: &Value) -> Result<usize> {
    let value = match cli {
        Some(value) => u64::from(value),
        None => match settings.get("maxAgents") {
            Some(value) => value
                .as_u64()
                .context("Maximum agents must be an integer between 1 and 32")?,
            None => 5,
        },
    };
    ensure!(
        (1..=32).contains(&value),
        "Maximum agents must be between 1 and 32"
    );
    Ok(value as usize)
}
fn parse_npc_update_rate(value: &str) -> std::result::Result<u8, String> {
    match value {
        "1" => Ok(1),
        "5" => Ok(5),
        "15" => Ok(15),
        _ => Err("NPC update rate must be 1, 5 or 15".into()),
    }
}
fn initial_npc_update_rate(cli: Option<u8>, settings: &Value) -> Result<u8> {
    let value = match cli {
        Some(value) => u64::from(value),
        None => match settings.get("npcUpdateRate") {
            Some(value) => value
                .as_u64()
                .context("NPC update rate must be an integer: 1, 5 or 15")?,
            None => 5,
        },
    };
    ensure!(
        matches!(value, 1 | 5 | 15),
        "NPC update rate must be 1, 5 or 15"
    );
    Ok(value as u8)
}

fn selected_network(options: &HostArgs, previous: &Value) -> Result<Network> {
    let networks = manager::networks()?;
    let address = options
        .bind
        .or_else(|| previous["bind"].as_str().and_then(|s| s.parse().ok()));
    let subnet = options
        .subnet
        .or_else(|| previous["subnet"].as_str().and_then(|s| s.parse().ok()));
    if let Some(address) = address {
        if let Some(network) = networks.iter().find(|n| n.address == address) {
            return Ok(Network {
                name: network.name.clone(),
                address,
                subnet: subnet.unwrap_or(network.subnet),
            });
        }
        ensure!(
            address.is_loopback(),
            "The selected address is not an active local interface"
        );
        return Ok(Network {
            name: "localhost".into(),
            address,
            subnet: subnet.unwrap_or("127.0.0.0/8".parse()?),
        });
    }
    ensure!(subnet.is_none(), "Select --bind with --subnet");
    networks
        .into_iter()
        .next()
        .context("No local IPv4 interface")
}

async fn host(directory: &Path, options: HostArgs) -> Result<()> {
    ensure!(
        options.headless || io::stdin().is_terminal() && io::stdout().is_terminal(),
        "Use host --headless --bind ADDRESS --subnet CIDR outside a terminal"
    );
    if options.headless {
        ensure!(
            options.bind.is_some() && options.subnet.is_some(),
            "Headless hosting requires --bind ADDRESS and --subnet CIDR"
        );
    }
    let _lock = paths::lock_server(directory)?;
    let previous = settings(directory)?;
    let language = options
        .language
        .clone()
        .or_else(|| previous["language"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "en".into());
    localization::validate_language(&language)?;
    let scenario = options
        .scenario
        .clone()
        .or_else(|| previous["scenario"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "castle".into());
    WorldContent::load(&scenario)?;
    let network = selected_network(&options, &previous)?;
    let port = options
        .port
        .or_else(|| {
            previous["port"]
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
        })
        .unwrap_or(2222);
    ensure!(port > 0, "Choose a nonzero SSH port");
    let maximum = usize::from(options.max_players.unwrap_or(8));
    let max_agents = initial_agent_limit(options.max_agents, &previous)?;
    let npc_update_rate = initial_npc_update_rate(options.npc_update_rate, &previous)?;
    let mut engine = Engine::open(directory, &scenario, &language, manager::seed())?;
    let mut setup = Setup {
        language,
        scenario,
        network,
        max_agents,
        npc_update_rate,
        start: options.save.as_ref().map_or(
            if options.new {
                Start::New
            } else {
                Start::Resume
            },
            |name| Start::Save(name.clone()),
        ),
        saves: engine.store.saves()?,
    };
    if !options.headless && options.import_snapshot.is_none() {
        setup = manager::setup(setup).await?;
    }
    WorldContent::load(&setup.scenario)?;
    localization::validate_language(&setup.language)?;
    let ssh_options = SshOptions {
        bind: setup.network.address,
        port,
        subnet: setup.network.subnet,
        maximum_players: maximum,
        authorized_keys: options.authorized_keys.clone(),
    };
    ssh_options.validate()?;
    // Check all dependencies before replacing a saved castle or admitting a guest.
    let worker = options.worker.options();
    let spec = WorkerSpec::prepare(directory, &worker)?;
    spec.probe().await?;
    if let Some(path) = &options.import_snapshot {
        let candidate = Store::parse_snapshot(&fs::read_to_string(path)?)?;
        WorldContent::load(&candidate.world.scenario_id)?;
        engine.save("before-import")?;
        engine.import_snapshot(path, &setup.language)?;
    } else {
        match &setup.start {
            Start::New => {
                engine.save("before-retry")?;
                engine.new_run(&setup.scenario, &setup.language, manager::seed())?;
            }
            Start::Save(name) => {
                let candidate = engine.store.snapshot(name)?;
                WorldContent::load(&candidate.world.scenario_id)?;
                engine.save(if name == "before-restore" {
                    "before-restore-backup"
                } else {
                    "before-restore"
                })?;
                engine.load(name, &setup.language)?;
            }
            Start::Resume if setup.language != engine.language => {
                engine.save("before-language")?;
                engine.load("before-language", &setup.language)?;
            }
            Start::Resume => {}
        }
    }
    let mut chosen_settings = previous;
    chosen_settings["language"] = json!(setup.language);
    chosen_settings["scenario"] = json!(setup.scenario);
    chosen_settings["bind"] = json!(setup.network.address);
    chosen_settings["subnet"] = json!(setup.network.subnet);
    chosen_settings["port"] = json!(port);
    chosen_settings["maxAgents"] = json!(setup.max_agents);
    chosen_settings["npcUpdateRate"] = json!(setup.npc_update_rate);
    paths::write_private(
        &directory.join("server-settings.json"),
        &serde_json::to_vec(&chosen_settings)?,
        false,
    )?;
    // Selecting Start begins the host; absence of players still freezes its clock.
    // A restore performed later through the manager remains explicitly paused.
    engine.set_pause(false)?;
    let info = HostInfo {
        data_dir: directory.into(),
        address: setup.network.address.to_string(),
        port,
        maximum_players: maximum,
    };
    let (mut simulation, handle) = Simulation::production(engine, info, worker)?;
    simulation.scheduling.maximum_calls = setup.max_agents;
    simulation
        .scheduling
        .set_npc_update_rate(setup.npc_update_rate)?;
    simulation.preflight().await?;
    let mut ssh = ssh::serve(handle.clone(), ssh_options).await?;
    let mut ipc = match manager::IpcServer::open(directory, handle.clone()).await {
        Ok(server) => server,
        Err(error) => {
            ssh.close().await;
            return Err(error);
        }
    };
    let manifest = directory.join("server.json");
    paths::write_private(
        &manifest,
        &serde_json::to_vec(
            &json!({"pid":std::process::id(),"managerSocket":ipc.path,"address":setup.network.address,"subnet":setup.network.subnet,"port":ssh.port,"fingerprint":ssh.fingerprint,"knownHosts":ssh.known_hosts}),
        )?,
        false,
    )?;
    println!(
        "BELOW THE CROWN\nssh -p {} {}\nHost key: {}\nManager: below-the-crown --data-dir '{}' manager",
        ssh.port,
        setup.network.address,
        ssh.fingerprint,
        directory.display()
    );
    let mut running = tokio::spawn(simulation.run());
    let mut terminated = false;
    let result = tokio::select! {
        result=&mut running=>{terminated=true;result.context("The simulation task stopped")?},
        result=manager::run(Link::Local(handle.clone()),directory,true,options.audio),if !options.headless=>result,
        result=shutdown_signal()=>result,
    };
    if !terminated {
        let _ =
            tokio::time::timeout(Duration::from_secs(5), handle.request("stop", json!({}))).await;
    }
    ssh.close().await;
    ipc.close().await;
    let _ = fs::remove_file(manifest);
    if !terminated {
        match tokio::time::timeout(Duration::from_secs(15), &mut running).await {
            Ok(result) => result.context("The simulation task stopped")??,
            Err(_) => {
                running.abort();
                let _ = running.await;
                bail!("Host shutdown exceeded its deadline; inspect the last autosave");
            }
        }
    }
    result
}

async fn shutdown_signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {result=tokio::signal::ctrl_c()=>result?,_=term.recv()=>{}}
    Ok(())
}

async fn doctor(directory: &Path, options: DoctorArgs) -> Result<()> {
    let worker = options.worker.options();
    let spec = WorkerSpec::prepare(directory, &worker)?;
    println!("Native Codex: {}", spec.codex.display());
    println!("Sandbox: {}", spec.probe().await?);
    let (sender, mut recalls) = mpsc::channel::<RecallRequest>(16);
    let driver = CodexDriver::new(directory.into(), "castle", "en", worker, sender)?;
    if let Err(error) = driver.inspect().await {
        driver.reset().await;
        return Err(error);
    }
    println!(
        "Login: ready\nModel: gpt-6-luna / high / standard\nIsolation: private home, no host tools or instructions"
    );
    if options.live {
        let engine = Engine::memory("castle", "en", manager::seed())?;
        let observation=engine.observation(102,"Connectivity check. Return a brief valid decision to wait for five game seconds, with no dialogue or mental changes.")?;
        let cancel = CancellationToken::new();
        let request = ThinkRequest {
            actor_id: 102,
            identity: Some(engine.npc_identity(102)?),
            token: DecisionToken {
                job_id: 1,
                context_generation: 0,
            },
            branch: engine.world.branch.clone(),
            input: observation["input"].clone(),
            listener: false,
        };
        let mut decision = driver.think(request, cancel.clone());
        let result=tokio::time::timeout(Duration::from_secs(120),async {
            loop{tokio::select!{answer=&mut decision=>break answer,Some(request)=recalls.recv()=>{let answer=engine.recall(request.actor_id,&request.query);let _=request.reply.send(answer);}}}
        }).await;
        cancel.cancel();
        drop(decision);
        driver.reset().await;
        let decision = result.context("The live Codex check timed out")??;
        below_the_crown::model::Decision::check(decision.decision.npc()?.clone())?;
        println!(
            "Live decision: valid · {} tokens · {:.1}s",
            decision.tokens,
            decision.latency_ms / 1000.
        );
    } else {
        driver.reset().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn npc_rate_uses_cli_then_saved_value_and_rejects_invalid_saved_settings() {
        assert_eq!(initial_npc_update_rate(None, &json!({})).unwrap(), 5);
        for rate in [1, 5, 15] {
            assert_eq!(
                initial_npc_update_rate(None, &json!({"npcUpdateRate":rate})).unwrap(),
                rate
            );
            assert_eq!(
                initial_npc_update_rate(Some(rate), &json!({"npcUpdateRate":"bad"})).unwrap(),
                rate
            );
            assert!(
                Cli::try_parse_from(["btc", "host", "--npc-update-rate", &rate.to_string()])
                    .is_ok()
            );
        }
        for value in [
            json!(0),
            json!(2),
            json!(16),
            json!(5.5),
            json!("5"),
            Value::Null,
        ] {
            assert!(initial_npc_update_rate(None, &json!({"npcUpdateRate":value})).is_err());
        }
        for value in ["0", "2", "16", "5.5"] {
            assert!(Cli::try_parse_from(["btc", "host", "--npc-update-rate", value]).is_err());
        }
    }
    #[test]
    fn agent_limit_uses_cli_then_saved_setting_then_five() {
        assert_eq!(initial_agent_limit(None, &json!({})).unwrap(), 5);
        assert_eq!(
            initial_agent_limit(None, &json!({"maxAgents":9})).unwrap(),
            9
        );
        assert_eq!(
            initial_agent_limit(Some(3), &json!({"maxAgents":9})).unwrap(),
            3
        );
        assert_eq!(
            initial_agent_limit(Some(3), &json!({"maxAgents":"bad"})).unwrap(),
            3
        );
        for value in [json!(0), json!(33), json!(2.5), json!("5"), Value::Null] {
            assert!(initial_agent_limit(None, &json!({"maxAgents":value})).is_err());
        }
    }
    #[test]
    fn cli_has_only_host_controls_and_checks_input() {
        assert!(Cli::try_parse_from(["below-the-crown", "play"]).is_err());
        assert!(Cli::try_parse_from(["btc", "host", "--new", "--save", "s"]).is_err());
        assert!(Cli::try_parse_from(["btc", "host", "--max-players", "33"]).is_err());
        for value in ["0", "33", "-1", "2.5"] {
            assert!(Cli::try_parse_from(["btc", "host", "--max-agents", value]).is_err());
        }
        for value in ["1", "7", "32"] {
            assert!(Cli::try_parse_from(["btc", "host", "--max-agents", value]).is_ok());
        }
        assert!(Cli::try_parse_from(["btc", "host", "--language", "de"]).is_err());
        assert!(
            Cli::try_parse_from([
                "btc",
                "host",
                "--headless",
                "--bind",
                "127.0.0.1",
                "--subnet",
                "127.0.0.0/8",
                "--data-dir",
                "/tmp/test-owned-btc"
            ])
            .is_ok()
        );
    }
}
