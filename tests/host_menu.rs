//! Exercise the actual CLI/key loop in a test-owned PTY, without paid model calls or GUI windows.
use anyhow::Result;
use below_the_crown::{
    codex::{DecisionDriver, ThinkRequest, ThinkResult},
    engine::Engine,
    manager::{self, IpcServer},
    model::ReasoningEffort,
    runtime::HostInfo,
    settings::HostSettings,
    simulation::Simulation,
    store::Store,
};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{Read, Write},
    net::Ipv4Addr,
    os::fd::{FromRawFd, OwnedFd},
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use termwiz::escape::{
    Action, CSI, ControlCode,
    csi::{Cursor, Edit, EraseInDisplay, EraseInLine},
    parser::Parser,
};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;

// Reconstruct what the player sees; ratatui emits cursor-positioned screen differences.
struct TestScreen {
    parser: Parser,
    cells: [[char; 100]; 32],
    x: usize,
    y: usize,
}
impl TestScreen {
    fn new() -> Self {
        Self {
            parser: Parser::new(),
            cells: [[' '; 100]; 32],
            x: 0,
            y: 0,
        }
    }
    fn print(&mut self, c: char) {
        let width = c.width().unwrap_or(0);
        if width == 0 {
            return;
        }
        if self.x + width > 100 {
            self.x = 0;
            self.y = (self.y + 1).min(31);
        }
        self.cells[self.y][self.x] = c;
        if width == 2 {
            self.cells[self.y][self.x + 1] = ' ';
        }
        self.x += width;
    }
    fn feed(&mut self, bytes: &[u8]) {
        for action in self.parser.parse_as_vec(bytes) {
            match action {
                Action::Print(c) => self.print(c),
                Action::PrintString(s) => {
                    for c in s.chars() {
                        self.print(c);
                    }
                }
                Action::Control(ControlCode::CarriageReturn) => self.x = 0,
                Action::Control(ControlCode::LineFeed) => self.y = (self.y + 1).min(31),
                Action::Control(ControlCode::Backspace) => self.x = self.x.saturating_sub(1),
                Action::CSI(CSI::Cursor(
                    Cursor::Position { line, col } | Cursor::CharacterAndLinePosition { line, col },
                )) => {
                    self.x = (col.as_zero_based() as usize).min(99);
                    self.y = (line.as_zero_based() as usize).min(31);
                }
                Action::CSI(CSI::Cursor(
                    Cursor::CharacterAbsolute(col) | Cursor::CharacterPositionAbsolute(col),
                )) => self.x = (col.as_zero_based() as usize).min(99),
                Action::CSI(CSI::Cursor(Cursor::Left(n))) => {
                    self.x = self.x.saturating_sub(n as usize)
                }
                Action::CSI(CSI::Cursor(Cursor::Right(n))) => {
                    self.x = (self.x + n as usize).min(99)
                }
                Action::CSI(CSI::Cursor(Cursor::Up(n))) => {
                    self.y = self.y.saturating_sub(n as usize)
                }
                Action::CSI(CSI::Cursor(Cursor::Down(n))) => self.y = (self.y + n as usize).min(31),
                Action::CSI(CSI::Edit(Edit::EraseInLine(mode))) => {
                    let x = self.x.min(99);
                    match mode {
                        EraseInLine::EraseToEndOfLine => self.cells[self.y][x..].fill(' '),
                        EraseInLine::EraseToStartOfLine => self.cells[self.y][..=x].fill(' '),
                        EraseInLine::EraseLine => self.cells[self.y].fill(' '),
                    }
                }
                Action::CSI(CSI::Edit(Edit::EraseInDisplay(mode))) => {
                    let x = self.x.min(99);
                    match mode {
                        EraseInDisplay::EraseDisplay => self.cells.fill([' '; 100]),
                        EraseInDisplay::EraseToEndOfDisplay => {
                            self.cells[self.y][x..].fill(' ');
                            self.cells[self.y + 1..].fill([' '; 100]);
                        }
                        EraseInDisplay::EraseToStartOfDisplay => {
                            self.cells[..self.y].fill([' '; 100]);
                            self.cells[self.y][..=x].fill(' ');
                        }
                        EraseInDisplay::EraseScrollback => {}
                    }
                }
                _ => {}
            }
        }
    }
    fn text(&self) -> String {
        self.cells
            .iter()
            .map(|row| row.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct NoCalls;
impl DecisionDriver for NoCalls {
    fn think(&self, _: ThinkRequest, _: CancellationToken) -> BoxFuture<'_, Result<ThinkResult>> {
        Box::pin(async { panic!("A menu without players must not make model decisions") })
    }
    fn reset(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn supported_efforts(&self) -> Vec<ReasoningEffort> {
        vec![ReasoningEffort::Low, ReasoningEffort::High]
    }
}

struct MenuProcess {
    child: Child,
    terminal: File,
    screen: TestScreen,
}
impl MenuProcess {
    fn spawn(directory: &Path, args: &[&str]) -> Self {
        let (mut master, mut slave) = (0, 0);
        let size = libc::winsize {
            ws_row: 32,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let terminal = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
        assert_eq!(
            unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let child = Command::new(env!("CARGO_BIN_EXE_below-the-crown"))
            .args(["--data-dir"])
            .arg(directory)
            .args(args)
            .env("TERM", "xterm-256color")
            .env("LANG", "en_US.UTF-8")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave))
            .spawn()
            .unwrap();
        Self {
            child,
            terminal,
            screen: TestScreen::new(),
        }
    }
    fn send(&mut self, keys: &[u8]) {
        self.terminal.write_all(keys).unwrap();
        self.terminal.flush().unwrap();
    }
    fn read(&mut self) -> String {
        let mut bytes = [0u8; 8192];
        loop {
            match self.terminal.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => self.screen.feed(&bytes[..n]),
                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock) => break,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("PTY read: {error}"),
            }
        }
        self.screen.text()
    }
    async fn wait_for(&mut self, text: &str) {
        self.wait_until(|screen| screen.contains(text), text).await;
    }
    async fn wait_until(&mut self, check: impl Fn(&str) -> bool, description: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let output = self.read();
            if check(&output) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Menu never showed {description:?}: {output}"
            );
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "Menu exited before {description:?}: {output}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    async fn exited(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.read();
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "Menu did not exit");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
impl Drop for MenuProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

async fn until(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "Menu operation never finished");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn attached_menu_edits_shared_settings_saves_q_names_and_leaves_host_running() {
    let directory = tempfile::tempdir().unwrap();
    let settings = HostSettings {
        reasoning_effort: ReasoningEffort::Low,
        ..Default::default()
    };
    settings.save(directory.path()).unwrap();
    let engine = Engine::open(directory.path(), "castle", "en", 53).unwrap();
    let (mut sim, handle) = Simulation::with_driver(
        engine,
        HostInfo {
            data_dir: directory.path().into(),
            address: "127.0.0.1".into(),
            port: 22341,
            maximum_players: 2,
        },
        Arc::new(NoCalls),
    );
    sim.apply_host_settings(settings).unwrap();
    let mut ipc = IpcServer::bind(directory.path()).await.unwrap();
    ipc.serve(handle.clone());
    let running = tokio::spawn(sim.run());
    let mut menu = MenuProcess::spawn(directory.path(), &["manager"]);
    menu.wait_for("Connected to the existing host").await;
    menu.send(b"2\x1b[B\x1b[B\x1b[C");
    until(|| {
        HostSettings::load(directory.path())
            .is_ok_and(|settings| settings.reasoning_effort == ReasoningEffort::High)
    })
    .await;
    assert_eq!(
        manager::request(directory.path(), "view", json!({}))
            .await
            .unwrap()["settings"]["reasoningEffort"],
        "high"
    );
    menu.wait_for("Done.").await;
    menu.send(b"3s");
    menu.wait_for("Save name").await;
    menu.send(&[127; 9]);
    menu.send(b"queen\r");
    until(|| {
        Store::open(directory.path())
            .unwrap()
            .saves()
            .unwrap()
            .iter()
            .any(|name| name == "queen")
    })
    .await;
    menu.wait_until(
        |screen| {
            screen.contains("queen") && screen.contains("Done.") && !screen.contains("Save name")
        },
        "completed save",
    )
    .await;
    menu.send(b"1\r");
    menu.wait_for("Connection instructions are shown in Diagnostics.")
        .await;
    assert!(menu.read().contains("22341"));
    assert_eq!(
        manager::request(directory.path(), "view", json!({}))
            .await
            .unwrap()["metrics"]["calls"],
        0
    );
    menu.send(b"q");
    menu.exited().await;
    assert!(
        manager::request(directory.path(), "view", json!({}))
            .await
            .is_ok(),
        "Leaving an attached menu stopped the existing host"
    );
    handle.request("stop", json!({})).await.unwrap();
    running.await.unwrap().unwrap();
    ipc.close().await;
}

#[tokio::test]
async fn startup_failures_stay_in_menu_and_preserve_the_existing_castle() {
    for port_conflict in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let engine = Engine::open(directory.path(), "castle", "en", 67).unwrap();
        let before = serde_json::to_value(&engine.world).unwrap();
        drop(engine);
        let reservation = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = reservation.local_addr().unwrap().port().to_string();
        if !port_conflict {
            drop(reservation);
        }
        let args = [
            "host",
            "--new",
            "--bind",
            "127.0.0.1",
            "--subnet",
            "127.0.0.0/8",
            "--port",
            &port,
            "--codex-bin",
            "/test-owned/nonexistent-codex",
        ];
        let mut menu = MenuProcess::spawn(directory.path(), &args);
        menu.wait_for("Ready to start").await;
        menu.wait_until(
            |screen| !screen.contains("Looking for a running game"),
            "completed host probe",
        )
        .await;
        menu.send(b"\r");
        menu.wait_for(if port_conflict {
            "The game could not open its network port."
        } else {
            "The native Codex application is unavailable."
        })
        .await;
        menu.wait_for(if port_conflict {
            "Choose another port in Settings and retry."
        } else {
            "Install a compatible Codex CLI, then retry Diagnostics."
        })
        .await;
        assert!(menu.child.try_wait().unwrap().is_none());
        let after: Value = serde_json::to_value(
            Store::open(directory.path())
                .unwrap()
                .load()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(after, before);
        assert!(!directory.path().join("server.json").exists());
        menu.send(b"q");
        menu.exited().await;
    }
}
