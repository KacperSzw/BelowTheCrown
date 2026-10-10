//! Local host controls. The player transport remains SSH, including on the host.
use crate::{
    localization::tr,
    paths,
    runtime::GameHandle,
    ui::{self, AMBER, ASH, BLOOD, PALE, SAGE},
};
use anyhow::{Context, Result, bail, ensure};
use crossterm::{
    event::{self, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ipnet::Ipv4Net;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{self, IsTerminal, Stdout},
    net::Ipv4Addr,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

const REQUEST_LIMIT: usize = 64 * 1024;
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub enum Link {
    Local(GameHandle),
    Remote(PathBuf),
}
impl Link {
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Self::Local(handle) => handle.request(method, params).await,
            Self::Remote(directory) => request(directory, method, params).await,
        }
    }
    async fn view(&self) -> Result<Value> {
        match self {
            Self::Local(handle) => Ok(handle.manager.borrow().clone()),
            Self::Remote(_) => self.request("view", json!({})).await,
        }
    }
}

pub struct IpcServer {
    pub path: PathBuf,
    inode: u64,
    device: u64,
    owns_socket: bool,
    cancel: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
}
impl IpcServer {
    pub async fn open(directory: &Path, handle: GameHandle) -> Result<Self> {
        let path = paths::manager_socket(directory)?;
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                metadata.file_type().is_socket(),
                "The manager socket path is occupied by a non-socket"
            );
            match UnixStream::connect(&path).await {
                Ok(_) => bail!("A manager already owns this socket"),
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(&path)?
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            };
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let metadata = fs::symlink_metadata(&path)?;
        let cancel = CancellationToken::new();
        let stopped = cancel.clone();
        let task = tokio::spawn(async move {
            let permits = Arc::new(Semaphore::new(16));
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    _=stopped.cancelled()=>break,
                    accepted=listener.accept()=>{let(stream,_)=accepted?;let Ok(permit)=permits.clone().try_acquire_owned()else{continue;};let game=handle.clone();clients.spawn(async move{let _permit=permit;let _=serve_request(stream,game).await;});},
                    _=clients.join_next(),if !clients.is_empty()=>{},
                }
            }
            clients.abort_all();
            while clients.join_next().await.is_some() {}
            Ok(())
        });
        Ok(Self {
            path,
            inode: metadata.ino(),
            device: metadata.dev(),
            owns_socket: true,
            cancel,
            task: Some(task),
        })
    }
    pub async fn close(&mut self) {
        self.cancel.cancel();
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(Duration::from_secs(2), &mut task)
                .await
                .is_err()
        {
            task.abort();
            let _ = task.await;
        }
        self.unlink_owned();
    }
    fn unlink_owned(&mut self) {
        if self.owns_socket {
            if fs::symlink_metadata(&self.path)
                .is_ok_and(|metadata| metadata.ino() == self.inode && metadata.dev() == self.device)
            {
                let _ = fs::remove_file(&self.path);
            }
            self.owns_socket = false;
        }
    }
}
impl Drop for IpcServer {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.unlink_owned();
    }
}

async fn frame<R: AsyncBufRead + Unpin>(reader: &mut R, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        ensure!(
            !chunk.is_empty(),
            "The manager closed an incomplete response"
        );
        let end = chunk.iter().position(|b| *b == b'\n');
        let size = end.map_or(chunk.len(), |n| n + 1);
        ensure!(
            bytes.len() + size <= limit,
            "Manager message exceeds its limit"
        );
        bytes.extend_from_slice(&chunk[..size]);
        reader.consume(size);
        if end.is_some() {
            bytes.pop();
            return Ok(bytes);
        }
    }
}

async fn serve_request(stream: UnixStream, handle: GameHandle) -> Result<()> {
    ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "Manager access belongs to the host account"
    );
    let (mut reader, mut writer) = stream.into_split();
    let raw = tokio::time::timeout(
        Duration::from_secs(5),
        frame(&mut BufReader::new(&mut reader), REQUEST_LIMIT),
    )
    .await??;
    let result = async {
        let request: Value = serde_json::from_slice(&raw)?;
        let object = request.as_object().context("Expected a manager request")?;
        ensure!(
            object.len() == 2 && object.contains_key("method") && object.contains_key("params"),
            "Invalid manager request fields"
        );
        let method = request["method"]
            .as_str()
            .filter(|s| s.len() <= 32)
            .context("Invalid manager operation")?;
        ensure!(request["params"].is_object(), "Expected manager parameters");
        handle.request(method, request["params"].clone()).await
    }
    .await;
    let response = match result {
        Ok(value) => json!({"result":value}),
        Err(error) => json!({"error":crate::codex::safe_error(error)}),
    };
    let mut bytes = serde_json::to_vec(&response)?;
    ensure!(
        bytes.len() < RESPONSE_LIMIT,
        "Manager response exceeds its limit"
    );
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), writer.write_all(&bytes)).await??;
    Ok(())
}

pub async fn request(directory: &Path, method: &str, params: Value) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(35), async {
        let path = paths::manager_socket(directory)?;
        let mut stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("No host is running for {}", directory.display()))?;
        let mut bytes = serde_json::to_vec(&json!({"method":method,"params":params}))?;
        ensure!(
            bytes.len() < REQUEST_LIMIT,
            "Manager request exceeds its limit"
        );
        bytes.push(b'\n');
        stream.write_all(&bytes).await?;
        let response: Value =
            serde_json::from_slice(&frame(&mut BufReader::new(stream), RESPONSE_LIMIT).await?)?;
        if let Some(error) = response["error"].as_str() {
            bail!("{error}");
        }
        response
            .get("result")
            .cloned()
            .context("Invalid manager response")
    })
    .await
    .context("The manager operation timed out")?
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    pub address: Ipv4Addr,
    pub subnet: Ipv4Net,
}

/// Discover actual interface netmasks; never guess a /24 for a selected LAN.
pub fn networks() -> Result<Vec<Network>> {
    let mut first: *mut libc::ifaddrs = std::ptr::null_mut();
    ensure!(
        unsafe { libc::getifaddrs(&mut first) } == 0,
        "Cannot enumerate LAN interfaces"
    );
    struct Interfaces(*mut libc::ifaddrs);
    impl Drop for Interfaces {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) }
        }
    }
    let _owned = Interfaces(first);
    let mut row = first;
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    while !row.is_null() {
        let interface = unsafe { &*row };
        row = interface.ifa_next;
        if interface.ifa_addr.is_null()
            || interface.ifa_netmask.is_null()
            || interface.ifa_flags & libc::IFF_UP as u32 == 0
        {
            continue;
        }
        if unsafe { (*interface.ifa_addr).sa_family as i32 } != libc::AF_INET {
            continue;
        }
        let address = unsafe { &*(interface.ifa_addr.cast::<libc::sockaddr_in>()) };
        let mask = unsafe { &*(interface.ifa_netmask.cast::<libc::sockaddr_in>()) };
        let address = Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes());
        let bits = u32::from_ne_bytes(mask.sin_addr.s_addr.to_ne_bytes()).to_be();
        let prefix = bits.leading_ones();
        if bits
            != if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            }
        {
            continue;
        }
        if !(address.is_private() || address.is_link_local() || address.is_loopback())
            || !seen.insert(address)
        {
            continue;
        }
        let subnet = Ipv4Net::new(address, prefix as u8)?.trunc();
        let name = unsafe { std::ffi::CStr::from_ptr(interface.ifa_name) }
            .to_string_lossy()
            .into_owned();
        found.push(Network {
            name,
            address,
            subnet,
        });
    }
    found.sort_by_key(|n| (n.address.is_loopback(), n.name.clone(), n.address));
    if !found.iter().any(|n| n.address == Ipv4Addr::LOCALHOST) {
        found.push(Network {
            name: "localhost".into(),
            address: Ipv4Addr::LOCALHOST,
            subnet: "127.0.0.0/8".parse()?,
        });
    }
    Ok(found)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Start {
    #[default]
    Resume,
    New,
    Save(String),
}
#[derive(Clone, Debug)]
pub struct Setup {
    pub language: String,
    pub scenario: String,
    pub network: Network,
    pub start: Start,
    pub saves: Vec<String>,
}

type NativeTerminal = Terminal<CrosstermBackend<Stdout>>;
struct Screen;
impl Screen {
    fn open() -> Result<(Self, NativeTerminal)> {
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "Use --headless outside a terminal"
        );
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            event::EnableBracketedPaste
        )?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok((guard, terminal))
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            event::DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

pub async fn setup(mut settings: Setup) -> Result<Setup> {
    let (mut _screen, mut terminal) = Screen::open()?;
    let mut events = EventStream::new();
    let mut selected = 0usize;
    let mut interfaces = networks()?;
    if !interfaces.contains(&settings.network) {
        interfaces.insert(0, settings.network.clone());
    }
    loop {
        terminal.draw(|frame| {
            let width = frame.area().width;
            let title = ui::ascii_title(usize::from(width));
            let top = title.len() as u16;
            let areas = Layout::vertical([
                Constraint::Length(top + 1),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(frame.area());
            frame.render_widget(
                Paragraph::new(
                    title
                        .into_iter()
                        .map(|s| Line::styled(s, Style::default().fg(AMBER)))
                        .collect::<Vec<_>>(),
                ),
                areas[0],
            );
            let rows = [
                (
                    tr(&settings.language, "Language", &[]),
                    if settings.language == "pl" {
                        "Polski".into()
                    } else {
                        "English".into()
                    },
                ),
                (
                    tr(&settings.language, "Scenario", &[]),
                    scenario_name(&settings.scenario, &settings.language),
                ),
                (
                    tr(&settings.language, "LAN interface", &[]),
                    format!(
                        "{} · {} · {}",
                        settings.network.name, settings.network.address, settings.network.subnet
                    ),
                ),
                (
                    tr(&settings.language, "Castle", &[]),
                    match &settings.start {
                        Start::Resume => tr(&settings.language, "Resume", &[]),
                        Start::New => tr(&settings.language, "New castle", &[]),
                        Start::Save(name) => {
                            format!("{} · {name}", tr(&settings.language, "Restore", &[]))
                        }
                    },
                ),
                (tr(&settings.language, "Start", &[]), String::new()),
            ];
            let mut lines = vec![Line::from("")];
            for (index, (label, value)) in rows.into_iter().enumerate() {
                let prefix = if index == selected { "› " } else { "  " };
                lines.push(Line::styled(
                    format!("{prefix}{label:<18} {value}"),
                    Style::default().fg(if index == selected { AMBER } else { PALE }),
                ));
                lines.push(Line::from(""));
            }
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), areas[1]);
            frame.render_widget(
                Paragraph::new(tr(
                    &settings.language,
                    "↑↓ select · ←→ change · Enter start · Esc leave",
                    &[],
                ))
                .style(Style::default().fg(ASH)),
                areas[2],
            );
        })?;
        match events.next().await.context("Terminal closed")?? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Esc => bail!("Setup cancelled"),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    bail!("Setup cancelled")
                }
                KeyCode::Up => selected = (selected + 4) % 5,
                KeyCode::Down | KeyCode::Tab => selected = (selected + 1) % 5,
                KeyCode::Enter => return Ok(settings),
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                    let reverse = key.code == KeyCode::Left;
                    match selected {
                        0 => {
                            settings.language = if settings.language == "pl" {
                                "en"
                            } else {
                                "pl"
                            }
                            .into()
                        }
                        1 => {
                            settings.scenario = cycle(&scenario_ids(), &settings.scenario, reverse)
                        }
                        2 => {
                            let index = interfaces
                                .iter()
                                .position(|n| *n == settings.network)
                                .unwrap_or(0);
                            settings.network = interfaces[(index
                                + if reverse { interfaces.len() - 1 } else { 1 })
                                % interfaces.len()]
                            .clone();
                        }
                        3 => {
                            settings.start = cycle_start(&settings.start, &settings.saves, reverse);
                        }
                        _ => return Ok(settings),
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn cycle(options: &[String], current: &str, reverse: bool) -> String {
    let index = options.iter().position(|s| s == current).unwrap_or(0);
    options[(index + if reverse { options.len() - 1 } else { 1 }) % options.len()].clone()
}
fn cycle_start(current: &Start, saves: &[String], reverse: bool) -> Start {
    let mut options = vec![Start::Resume, Start::New];
    options.extend(saves.iter().cloned().map(Start::Save));
    let index = options
        .iter()
        .position(|choice| choice == current)
        .unwrap_or(0);
    options[(index + if reverse { options.len() - 1 } else { 1 }) % options.len()].clone()
}
pub fn scenario_name(id: &str, language: &str) -> String {
    crate::content::available_scenarios()
        .into_iter()
        .find(|(key, _, _)| key == id)
        .map(|(_, english, polish)| if language == "pl" { polish } else { english })
        .unwrap_or_else(|| id.to_owned())
}
fn scenario_ids() -> Vec<String> {
    crate::content::available_scenarios()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect()
}

#[derive(Default)]
pub struct ManagerUi {
    pub view: Value,
    pub page: usize,
    pub selected: usize,
    pub save_selected: usize,
    pub detail: usize,
    pub scroll: u16,
    pub message: String,
    save_draft: Option<ui::Draft>,
    confirm_new: bool,
}
impl ManagerUi {
    fn language(&self) -> &str {
        self.view["language"].as_str().unwrap_or("en")
    }
    fn text(&self, text: &str) -> String {
        tr(self.language(), text, &[])
    }
    fn actors(&self) -> &[Value] {
        self.view["actors"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    fn saves(&self) -> &[Value] {
        self.view["saves"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    pub fn set_view(&mut self, view: Value) {
        self.view = view;
        self.selected = self.selected.min(self.actors().len().saturating_sub(1));
        self.save_selected = self.save_selected.min(self.saves().len().saturating_sub(1));
    }
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let title = ui::ascii_title(usize::from(area.width));
        let top = title.len() as u16 + 3;
        let areas = Layout::vertical([
            Constraint::Length(top),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .split(area);
        let mut header = title
            .into_iter()
            .map(|s| Line::styled(s, Style::default().fg(AMBER)))
            .collect::<Vec<_>>();
        header.push(Line::from(vec![
            Span::styled(self.text("Private inspection"), Style::default().fg(AMBER)),
            Span::raw(format!(
                " · {} · {} · {} / {}",
                string(&self.view, "clock"),
                scenario_name(string(&self.view, "scenarioId"), self.language()),
                string(&self.view, "model"),
                string(&self.view, "effort")
            )),
        ]));
        let pages = ["Overview", "People", "Saves", "Settings"];
        header.push(Line::from(
            pages
                .iter()
                .enumerate()
                .map(|(i, page)| {
                    Span::styled(
                        format!(" {} {} ", i + 1, self.text(page)),
                        Style::default().fg(if i == self.page { AMBER } else { ASH }),
                    )
                })
                .collect::<Vec<_>>(),
        ));
        header.push(Line::styled(
            self.view["paused"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .map(|s| self.text(s))
                        .collect::<Vec<_>>()
                        .join(" · ")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| self.text("Running")),
            Style::default().fg(ASH),
        ));
        frame.render_widget(Paragraph::new(header), areas[0]);
        match self.page {
            0 => self.overview(frame, areas[1]),
            1 => self.people(frame, areas[1]),
            2 => self.save_page(frame, areas[1]),
            _ => self.settings(frame, areas[1]),
        }
        let metrics = &self.view["metrics"];
        let percent = |key: &str| {
            metrics[key]
                .as_f64()
                .map(|n| format!("{:.1}s", n / 1000.))
                .unwrap_or_else(|| "—".into())
        };
        let footer = vec![
            Line::styled(
                format!(
                    "{} · {} {} · {} {} · p50 {} / p95 {}",
                    self.text("Thinking") + " " + &metrics["active"].to_string(),
                    metrics["calls"],
                    self.text("calls"),
                    metrics["tokens"],
                    self.text("tokens"),
                    percent("p50"),
                    percent("p95")
                ),
                Style::default().fg(ASH),
            ),
            Line::styled(
                self.text(
                    "c connect player · p pause · o observe · s save · n new · r recover · q leave",
                ),
                Style::default().fg(ASH),
            ),
            Line::styled(
                ui::clean(&self.message, false),
                Style::default().fg(if self.view["error"].is_null() {
                    SAGE
                } else {
                    BLOOD
                }),
            ),
        ];
        frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), areas[2]);
        if let Some(draft) = &self.save_draft {
            self.modal(
                frame,
                &format!(
                    "{}: {}▌\n{}",
                    self.text("Save name"),
                    draft.text,
                    self.text("Enter save · Esc cancel")
                ),
            );
        }
        if self.confirm_new {
            self.modal(
                frame,
                &format!(
                    "{} · {} · {}\n{}",
                    self.text("New castle"),
                    scenario_name(string(&self.view, "pendingScenario"), self.language()),
                    string(&self.view, "pendingLanguage"),
                    self.text("Enter start · Esc cancel. Connected players must rejoin.")
                ),
            );
        }
    }
    fn modal(&self, frame: &mut Frame, text: &str) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(80);
        let height = 5.min(area.height);
        let rect = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(ratatui::widgets::Clear, rect);
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(PALE).bg(Color::Rgb(35, 31, 26)))
                .wrap(Wrap { trim: false }),
            rect,
        );
    }
    fn overview(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![
            Line::styled(self.text("Join with SSH"), Style::default().fg(AMBER)),
            Line::styled(
                string(&self.view["ssh"], "command").to_owned(),
                Style::default().fg(PALE),
            ),
            Line::from(""),
        ];
        let actors = self.actors();
        let connected = actors.iter().filter(|a| a["connected"] == true).count();
        lines.push(Line::styled(
            format!("{}: {connected}", self.text("Connected")),
            Style::default().fg(SAGE),
        ));
        lines.push(Line::from(""));
        let mut rooms = std::collections::BTreeMap::<String, Vec<&Value>>::new();
        for actor in actors {
            rooms
                .entry(string(actor, "room").into())
                .or_default()
                .push(actor);
        }
        for (room, people) in rooms {
            lines.push(Line::styled(
                format!("{room} · {}", people.len()),
                Style::default().fg(AMBER),
            ));
            lines.push(Line::from(
                people
                    .iter()
                    .map(|actor| {
                        Span::styled(
                            format!(
                                "  {}{}",
                                string(actor, "name"),
                                if actor["connected"] == true {
                                    " ●"
                                } else {
                                    ""
                                }
                            ),
                            Style::default()
                                .fg(ui::actor_color(actor["id"].as_u64().unwrap_or(0) as u16, 0)),
                        )
                    })
                    .collect::<Vec<_>>(),
            ));
            lines.push(Line::from(""));
        }
        if let Some(error) = self.view["error"].as_str() {
            lines.push(Line::styled(
                ui::clean(error, false),
                Style::default().fg(BLOOD),
            ));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }
    fn people(&self, frame: &mut Frame, area: Rect) {
        let list_height = (self.actors().len() as u16 + 2).min(area.height / 2).max(1);
        let parts =
            Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)]).split(area);
        let mut rows = vec![Line::styled(
            self.text("↑↓ person · Tab detail · PgUp/PgDn scroll"),
            Style::default().fg(ASH),
        )];
        let start = self
            .selected
            .saturating_sub(usize::from(list_height.saturating_sub(3)));
        let available = usize::from(list_height.saturating_sub(1));
        for (index, actor) in self.actors().iter().enumerate().skip(start).take(available) {
            let text = format!(
                "{} {:<16} {:<23} {:<10} {}",
                if self.selected == index { "›" } else { " " },
                ui::clip(string(actor, "name"), 16),
                ui::clip(string(actor, "room"), 23),
                self.text(string(&actor["status"], "state")),
                string(actor, "activity")
            );
            rows.push(Line::styled(
                text,
                Style::default().fg(if self.selected == index {
                    AMBER
                } else {
                    ui::actor_color(actor["id"].as_u64().unwrap_or(0) as u16, 0)
                }),
            ));
        }
        frame.render_widget(Paragraph::new(rows), parts[0]);
        let selected = &self.view["selected"];
        let actor = &selected["actor"];
        let titles = [
            "Character",
            "Intent",
            "Memories",
            "Inventory",
            "Automatic actions",
        ];
        let mut detail = vec![
            Line::styled(
                format!(
                    "{} · {}",
                    string(actor, "name"),
                    self.text(titles[self.detail % titles.len()])
                ),
                Style::default().fg(AMBER),
            ),
            Line::from(""),
        ];
        match self.detail % 5 {
            0 => {
                if let Some(key) = actor["portraitKey"].as_str()
                    && area.width >= 64
                    && unicode_terminal()
                {
                    for line in crate::portraits::render(key, 21, 9) {
                        detail.push(Line::styled(line, Style::default().fg(PALE)));
                    }
                    detail.push(Line::from(""));
                }
                for key in ["biography", "personality"] {
                    if let Some(text) = actor[key].as_str() {
                        detail.push(Line::raw(ui::clean(text, true)));
                        detail.push(Line::from(""));
                    }
                }
                detail.push(Line::raw(format!(
                    "{} · {}",
                    string(actor, "condition"),
                    string(actor, "activity")
                )));
            }
            1 => {
                detail.push(Line::raw(string(actor, "summary").to_owned()));
                detail.push(Line::from(""));
                if let Some(goals) = actor["goals"].as_array() {
                    for goal in goals {
                        detail.push(Line::raw(format!(
                            "· {}",
                            ui::clean(goal.as_str().unwrap_or(""), false)
                        )));
                    }
                }
                if let Some(error) = self
                    .actors()
                    .get(self.selected)
                    .and_then(|a| a["status"]["error"].as_str())
                {
                    detail.push(Line::styled(
                        ui::clean(error, false),
                        Style::default().fg(BLOOD),
                    ));
                }
            }
            2 => {
                if let Some(memories) = selected["memories"].as_array() {
                    for memory in memories.iter().rev() {
                        detail.push(Line::raw(format!(
                            "{} · {}",
                            self.text(string(memory, "kind")),
                            ui::clean(string(memory, "text"), false)
                        )));
                        detail.push(Line::from(""));
                    }
                }
            }
            3 => {
                if let Some(items) = selected["inventory"].as_array() {
                    for item in items {
                        detail.push(Line::raw(format!(
                            "{} ×{}",
                            string(item, "name"),
                            item["quantity"]
                        )));
                    }
                }
            }
            _ => {
                let listener = &self.view["listener"];
                detail.push(Line::raw(format!(
                    "{} · {}",
                    if listener["enabled"] == true {
                        "on"
                    } else {
                        "off"
                    },
                    string(listener, "state")
                )));
                detail.push(Line::raw(ui::clean(string(listener, "text"), true)));
                detail.push(Line::raw(ui::clean(string(listener, "summary"), true)));
                detail.push(Line::raw(listener["plan"].to_string()));
                if let Some(error) = listener["error"].as_str() {
                    detail.push(Line::styled(
                        ui::clean(error, false),
                        Style::default().fg(BLOOD),
                    ));
                }
            }
        }
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            parts[1],
        );
    }
    fn save_page(&self, frame: &mut Frame, area: Rect) {
        let mut rows = vec![
            Line::styled(
                self.text("↑↓ save · Enter restore · s save current castle"),
                Style::default().fg(ASH),
            ),
            Line::from(""),
        ];
        if self.saves().is_empty() {
            rows.push(Line::raw(self.text("No saves yet.")));
        }
        for (index, save) in self.saves().iter().enumerate() {
            rows.push(Line::styled(
                format!(
                    "{} {:<24} {} · {} · {}",
                    if index == self.save_selected {
                        "›"
                    } else {
                        " "
                    },
                    string(save, "name"),
                    scenario_name(string(save, "scenarioId"), self.language()),
                    string(save, "language"),
                    if save["valid"] == false {
                        self.text("Invalid save")
                    } else {
                        string(save, "created").into()
                    }
                ),
                Style::default().fg(if index == self.save_selected {
                    AMBER
                } else {
                    PALE
                }),
            ));
        }
        frame.render_widget(Paragraph::new(rows).scroll((self.scroll, 0)), area);
    }
    fn settings(&self, frame: &mut Frame, area: Rect) {
        let rows = vec![
            Line::styled(
                format!(
                    "{}: {}",
                    self.text("Language"),
                    string(&self.view, "pendingLanguage")
                ),
                Style::default().fg(AMBER),
            ),
            Line::styled(
                format!(
                    "{}: {}",
                    self.text("Scenario"),
                    scenario_name(string(&self.view, "pendingScenario"), self.language())
                ),
                Style::default().fg(AMBER),
            ),
            Line::from(""),
            Line::raw(self.text("l change language · ←→ change scenario · n new castle")),
            Line::from(""),
            Line::styled(
                self.text("Language applies when starting or restoring a castle."),
                Style::default().fg(ASH),
            ),
            Line::from(""),
            Line::raw(self.text("One game day = one real hour. Pauses freeze the world.")),
            Line::raw(self.text("NPCs use Luna/high at standard speed.")),
            Line::raw(self.text("Sound is local to this manager; SSH stays silent.")),
        ];
        frame.render_widget(Paragraph::new(rows).wrap(Wrap { trim: false }), area);
    }
    fn movement(&mut self, up: bool) -> Option<u16> {
        self.scroll = 0;
        if self.page == 1 {
            let len = self.actors().len();
            if len > 0 {
                self.selected = (self.selected + if up { len - 1 } else { 1 }) % len;
                return self.actors()[self.selected]["id"]
                    .as_u64()
                    .map(|n| n as u16);
            }
        } else if self.page == 2 {
            let len = self.saves().len();
            if len > 0 {
                self.save_selected = (self.save_selected + if up { len - 1 } else { 1 }) % len;
            }
        }
        None
    }
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
fn unicode_terminal() -> bool {
    std::env::var("TERM").is_ok_and(|s| s != "dumb" && !s.starts_with("linux"))
        && ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .next()
            .is_some_and(|v| v.to_ascii_uppercase().contains("UTF"))
}

fn edit(draft: &mut ui::Draft, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            draft.insert(&c.to_string())
        }
        KeyCode::Backspace => draft.delete(false),
        KeyCode::Delete => draft.delete(true),
        KeyCode::Left => draft.cursor = draft.cursor.saturating_sub(1),
        KeyCode::Right => {
            use unicode_segmentation::UnicodeSegmentation;
            draft.cursor = (draft.cursor + 1).min(draft.text.graphemes(true).count());
        }
        KeyCode::Home => draft.boundary(false),
        KeyCode::End => draft.boundary(true),
        _ => {}
    }
}

pub async fn run(link: Link, directory: &Path, owns_host: bool, audio: bool) -> Result<()> {
    let (_screen, mut terminal) = Screen::open()?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ui = ManagerUi::default();
    ui.set_view(link.view().await?);
    let mut sound = Audio::new(directory, audio)?;
    loop {
        terminal.draw(|frame| ui.render(frame))?;
        sound.tick();
        let key = tokio::select! {_=ticker.tick()=>{ui.set_view(link.view().await?);continue;},event=events.next()=>match event.context("Terminal closed")??{Event::Key(key) if key.kind!=KeyEventKind::Release=>key,Event::Paste(text)=>{if let Some(draft)=ui.save_draft.as_mut(){draft.insert(&text);}continue;},_=>continue}};
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if owns_host {
                link.request("stop", json!({})).await?;
            }
            break;
        }
        let operation = if let Some(mut draft) = ui.save_draft.take() {
            match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => Some(("save", json!({"name":draft.text.trim()}))),
                _ => {
                    edit(&mut draft, key);
                    ui.save_draft = Some(draft);
                    None
                }
            }
        } else if ui.confirm_new {
            match key.code {
                KeyCode::Esc => {
                    ui.confirm_new = false;
                    None
                }
                KeyCode::Enter => {
                    ui.confirm_new = false;
                    Some(("new", json!({"seed":seed()})))
                }
                _ => None,
            }
        } else {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    if owns_host {
                        link.request("stop", json!({})).await?;
                    }
                    break;
                }
                KeyCode::Char('1'..='4') => {
                    if let KeyCode::Char(c) = key.code {
                        ui.page = (c as usize) - ('1' as usize);
                        ui.scroll = 0;
                    }
                    None
                }
                KeyCode::Up | KeyCode::Down => ui
                    .movement(key.code == KeyCode::Up)
                    .map(|id| ("select", json!({"actorId":id}))),
                KeyCode::Tab => {
                    if ui.page == 1 {
                        ui.detail = (ui.detail + 1) % 5;
                        ui.scroll = 0;
                    } else {
                        ui.page = (ui.page + 1) % 4;
                    }
                    None
                }
                KeyCode::PageUp => {
                    ui.scroll = ui.scroll.saturating_sub(8);
                    None
                }
                KeyCode::PageDown => {
                    ui.scroll = ui.scroll.saturating_add(8);
                    None
                }
                KeyCode::Char('p') => Some((
                    "pause",
                    json!({"paused":!ui.view["manualPause"].as_bool().unwrap_or_else(||ui.view["paused"].as_array().is_some_and(|v|v.iter().any(|s|s=="Paused")))}),
                )),
                KeyCode::Char('o') => Some((
                    "observe",
                    json!({"enabled":!ui.view["observing"].as_bool().unwrap_or(false)}),
                )),
                KeyCode::Char('r') => Some(("recover", json!({}))),
                KeyCode::Char('s') => {
                    let mut draft = ui::Draft::default();
                    draft.set("quicksave".into());
                    ui.save_draft = Some(draft);
                    None
                }
                KeyCode::Char('n') => {
                    ui.confirm_new = true;
                    None
                }
                KeyCode::Enter if ui.page == 2 => ui
                    .saves()
                    .get(ui.save_selected)
                    .map(|save| ("load", json!({"name":save["name"]}))),
                KeyCode::Char('l') if ui.page == 3 => Some((
                    "settings",
                    json!({"language":if ui.view["pendingLanguage"]=="pl"{"en"}else{"pl"}}),
                )),
                KeyCode::Left | KeyCode::Right if ui.page == 3 => Some((
                    "settings",
                    json!({"scenario":cycle(&scenario_ids(),string(&ui.view,"pendingScenario"),key.code==KeyCode::Left)}),
                )),
                KeyCode::Char('c') => {
                    ui.message = match launch_player(directory, &ui.view) {
                        Ok(true) => ui.text("Player terminal opened."),
                        Ok(false) => format!(
                            "{}: {}",
                            ui.text("Connect player"),
                            string(&ui.view["ssh"], "localCommand")
                        ),
                        Err(error) => crate::codex::safe_error(error),
                    };
                    None
                }
                _ => None,
            }
        };
        if let Some((method, params)) = operation {
            ui.message = match link.request(method, params).await {
                Ok(answer) => {
                    if answer["ok"] == false {
                        ui::clean(string(&answer, "message"), false)
                    } else {
                        tr(ui.language(), "Done.", &[])
                    }
                }
                Err(error) => crate::codex::safe_error(error),
            };
            ui.set_view(link.view().await?);
        }
    }
    Ok(())
}

pub fn seed() -> u32 {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u32)
        .max(1)
}

fn launch_player(directory: &Path, view: &Value) -> Result<bool> {
    let Some(ssh) = paths::find_executable("ssh") else {
        return Ok(false);
    };
    let Some(terminal) = ["kitty", "foot", "gnome-terminal", "xterm"]
        .iter()
        .find_map(|name| paths::find_executable(name))
    else {
        return Ok(false);
    };
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Ok(false);
    }
    let port = view["ssh"]["port"]
        .as_u64()
        .context("No SSH port")?
        .to_string();
    let known = directory.join("ssh/known_hosts");
    let mut args = Vec::<std::ffi::OsString>::new();
    match terminal.file_name().and_then(|s| s.to_str()) {
        Some("kitty") => args.extend(["--title".into(), "Below the Crown · player".into()]),
        Some("gnome-terminal") => args.push("--".into()),
        Some("xterm") => args.push("-e".into()),
        _ => {}
    }
    args.extend([
        ssh.into_os_string(),
        "-tt".into(),
        "-p".into(),
        port.into(),
        "-o".into(),
        format!("UserKnownHostsFile={}", known.display()).into(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "127.0.0.1".into(),
    ]);
    let child = if let (Some(launcher), Some(workspace)) = (
        paths::find_executable("workstation-desktop"),
        native_workspace(),
    ) {
        Command::new(launcher)
            .args(["launch", "--workspace", &workspace, "--background", "--"])
            .arg(&terminal)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?
    } else {
        Command::new(&terminal)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?
    };
    // Reap only the launcher we created; the player terminal manages its own SSH session.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(true)
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
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        pid = text
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
    clients.as_array()?.iter().find(|client|client["pid"].as_u64().is_some_and(|p|parents.contains(&p)))?["workspace"]["id"].as_i64().map(|id|id.to_string())
}

struct Audio {
    path: Option<PathBuf>,
    child: Option<Child>,
}
impl Audio {
    fn new(directory: &Path, enabled: bool) -> Result<Self> {
        let path = if enabled && paths::find_executable("pw-play").is_some() {
            let path = directory.join("audio/dungeon.wav");
            paths::write_private(&path, include_bytes!("../content/audio/dungeon.wav"), false)?;
            Some(path)
        } else {
            None
        };
        Ok(Self { path, child: None })
    }
    fn tick(&mut self) {
        if self
            .child
            .as_mut()
            .is_some_and(|child| child.try_wait().ok().flatten().is_some())
        {
            self.child = None;
        }
        if self.child.is_none()
            && let Some(path) = &self.path
        {
            self.child = Command::new("pw-play")
                .arg("--volume=0.15")
                .arg(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
        }
    }
}
impl Drop for Audio {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menus_wrap_in_both_directions() {
        let values = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(cycle(&values, "a", true), "c");
        assert_eq!(cycle(&values, "c", false), "a");
        let mut ui = ManagerUi {
            page: 1,
            ..Default::default()
        };
        ui.set_view(json!({"actors":[{"id":10},{"id":11},{"id":12}]}));
        assert_eq!(ui.movement(true), Some(12));
        assert_eq!(ui.movement(false), Some(10));
        let saves = vec!["new".into(), "resume".into()];
        assert_eq!(
            cycle_start(&Start::New, &saves, false),
            Start::Save("new".into())
        );
        assert_eq!(
            cycle_start(&Start::Resume, &saves, true),
            Start::Save("resume".into())
        );
    }
    #[test]
    fn discovered_networks_have_real_private_subnets() {
        let networks = networks().unwrap();
        assert!(!networks.is_empty());
        for network in networks {
            assert!(network.subnet.contains(&network.address));
            assert!(
                network.address.is_private()
                    || network.address.is_loopback()
                    || network.address.is_link_local()
            );
        }
    }
    #[tokio::test]
    async fn ipc_frames_are_bounded_and_unicode_safe() {
        let mut valid = BufReader::new(&b"{\"name\":\"\xc5\x81ucja\"}\n"[..]);
        let value: Value = serde_json::from_slice(&frame(&mut valid, 80).await.unwrap()).unwrap();
        assert_eq!(value["name"], "Łucja");
        let mut too_big = BufReader::new(&b"123456789\n"[..]);
        assert!(frame(&mut too_big, 8).await.is_err());
        let mut truncated = BufReader::new(&b"123"[..]);
        assert!(frame(&mut truncated, 20).await.is_err());
    }
    #[test]
    fn manager_renders_without_clipping_panics() {
        use ratatui::backend::TestBackend;
        let mut ui = ManagerUi::default();
        ui.set_view(json!({"language":"pl","model":"gpt-6-luna","effort":"high","actors":[],"saves":[],"metrics":{"active":0,"calls":0,"tokens":0}}));
        for (width, height) in [(12, 4), (80, 24), (160, 50)] {
            for page in 0..4 {
                ui.page = page;
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| ui.render(frame)).unwrap();
            }
        }
    }
}
