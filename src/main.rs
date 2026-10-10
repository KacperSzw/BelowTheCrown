use anyhow::{Context, Result, bail, ensure};
use below_the_crown::{
    codex::{
        CodexDriver, DecisionDriver, RecallRequest, ThinkRequest, WorkerOptions, WorkerSpec,
        safe_error,
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
    let mut engine = Engine::open(directory, &scenario, &language, manager::seed())?;
    let mut setup = Setup {
        language,
        scenario,
        network,
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
    paths::write_private(
        &directory.join("server-settings.json"),
        &serde_json::to_vec(
            &json!({"language":setup.language,"scenario":setup.scenario,"bind":setup.network.address,"subnet":setup.network.subnet,"port":port}),
        )?,
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
    let (simulation, handle) = Simulation::production(engine, info, worker)?;
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
        below_the_crown::model::Decision::parse(decision.decision)?;
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
    fn cli_has_only_host_controls_and_checks_input() {
        assert!(Cli::try_parse_from(["below-the-crown", "play"]).is_err());
        assert!(Cli::try_parse_from(["btc", "host", "--new", "--save", "s"]).is_err());
        assert!(Cli::try_parse_from(["btc", "host", "--max-players", "33"]).is_err());
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
