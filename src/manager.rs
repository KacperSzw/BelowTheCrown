//! Local host controls. The player transport remains SSH, including on the host.
use crate::{
    codex::DecisionDriver,
    diagnostics::Failure,
    host::{HostOptions, HostSession, Start},
    localization::tr,
    model::ReasoningEffort,
    paths,
    runtime::GameHandle,
    settings::HostSettings,
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
    listener: Option<UnixListener>,
}
impl IpcServer {
    pub async fn bind(directory: &Path) -> Result<Self> {
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
        Ok(Self {
            path,
            inode: metadata.ino(),
            device: metadata.dev(),
            owns_socket: true,
            cancel,
            task: None,
            listener: Some(listener),
        })
    }
    pub fn serve(&mut self, handle: GameHandle) {
        let listener = self.listener.take().expect("IPC listener already serving");
        let stopped = self.cancel.clone();
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
        self.task = Some(task);
    }
    pub async fn close(&mut self) {
        self.listener.take();
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Page {
    #[default]
    Home,
    Settings,
    Saves,
    Diagnostics,
    Admin,
}
impl Page {
    pub const ALL: [Self; 5] = [
        Self::Home,
        Self::Settings,
        Self::Saves,
        Self::Diagnostics,
        Self::Admin,
    ];
    fn title(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Settings => "Settings",
            Self::Saves => "Saves",
            Self::Diagnostics => "Diagnostics",
            Self::Admin => "Admin",
        }
    }
}

#[derive(Default)]
pub struct MenuOptions {
    pub overrides: Value,
    pub host: HostOptions,
    pub start: Start,
    pub audio: bool,
    pub attach_only: bool,
}

#[derive(Default)]
pub struct MenuUi {
    pub view: Value,
    pub settings: HostSettings,
    pub page: Page,
    pub selected: usize,
    pub save_selected: usize,
    pub detail: usize,
    pub scroll: u16,
    pub message: String,
    pub active: bool,
    pub owns_host: bool,
    pub report: Value,
    pub failure: Option<Failure>,
    pub busy: String,
    pub networks: Vec<Network>,
    pub efforts: Vec<ReasoningEffort>,
    directory: PathBuf,
    home_selected: usize,
    setting_selected: usize,
    show_details: bool,
    config_valid: bool,
    save_draft: Option<ui::Draft>,
    confirm_new: bool,
    confirm_stop: bool,
}
impl MenuUi {
    fn language(&self) -> &str {
        &self.settings.language
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
        if let Some(settings) = view.get("settings") {
            match serde_json::from_value::<HostSettings>(settings.clone()) {
                Ok(settings) => self.settings = settings,
                Err(error) => self.fail(anyhow::Error::from(error)),
            }
        } else {
            // A host started before the upgrade may keep running; only its existing controls are usable.
            let mut patch = json!({});
            for (target, source) in [
                ("language", "pendingLanguage"),
                ("scenario", "pendingScenario"),
                ("maxAgents", "maxAgents"),
                ("npcUpdateRate", "npcUpdateRate"),
            ] {
                if let Some(value) = view.get(source) {
                    patch[target] = value.clone();
                }
            }
            if let Ok(settings) = self.settings.patched(&patch) {
                self.settings = settings;
            }
        }
        if let Some(efforts) = view.get("supportedEfforts")
            && let Ok(efforts) = serde_json::from_value(efforts.clone())
        {
            self.efforts = efforts;
        }
        self.view = view;
        self.selected = self.selected.min(self.actors().len().saturating_sub(1));
        self.save_selected = self.save_selected.min(self.saves().len().saturating_sub(1));
    }
    fn fail(&mut self, error: anyhow::Error) {
        let failure = Failure::from_error(&error);
        self.message = self.text(&failure.summary);
        self.failure = Some(failure);
        self.page = Page::Diagnostics;
        self.scroll = 0;
    }
    fn home_actions(&self) -> [&'static str; 7] {
        [
            if self.active && self.view["manualPause"] == true {
                "Resume and play"
            } else {
                "Play"
            },
            "New game",
            "Settings",
            "Saves",
            "Diagnostics",
            "Admin",
            if self.owns_host {
                "Save and stop"
            } else {
                "Quit"
            },
        ]
    }
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let title = ui::title(
            usize::from(area.width),
            usize::from(area.height.saturating_sub(13)),
            ui::local_braille_support(),
        );
        let top = (title.len() as u16 + 3).min(area.height.saturating_sub(3));
        let areas = Layout::vertical([
            Constraint::Length(top),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(area);
        let mut header = title
            .into_iter()
            .map(|line| Line::styled(line, Style::default().fg(AMBER)))
            .collect::<Vec<_>>();
        header.push(Line::styled(
            if self.active {
                format!(
                    "{} · {} · {}",
                    self.text("Running game"),
                    string(&self.view, "clock"),
                    scenario_name(string(&self.view, "scenarioId"), self.language())
                )
            } else {
                self.text("Ready to start")
            },
            Style::default().fg(AMBER),
        ));
        header.push(Line::from(
            Page::ALL
                .iter()
                .enumerate()
                .map(|(index, page)| {
                    Span::styled(
                        format!(" {} {} ", index + 1, self.text(page.title())),
                        Style::default().fg(if *page == self.page { AMBER } else { ASH }),
                    )
                })
                .collect::<Vec<_>>(),
        ));
        if !self.busy.is_empty() {
            header.push(Line::styled(
                self.text(&self.busy),
                Style::default().fg(SAGE),
            ));
        } else {
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
                    .unwrap_or_default(),
                Style::default().fg(ASH),
            ));
        }
        frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), areas[0]);
        match self.page {
            Page::Home => self.home(frame, areas[1]),
            Page::Settings => self.settings_page(frame, areas[1]),
            Page::Saves => self.save_page(frame, areas[1]),
            Page::Diagnostics => self.diagnostics_page(frame, areas[1]),
            Page::Admin => {
                if self.active {
                    self.people(frame, areas[1]);
                } else {
                    frame.render_widget(
                        Paragraph::new(
                            self.text("Start a game to inspect its people and use Admin controls."),
                        )
                        .wrap(Wrap { trim: false }),
                        areas[1],
                    );
                }
            }
        }
        let help = match self.page {
            Page::Home => "↑↓ select · Enter open · Tab page · q quit",
            Page::Settings => "↑↓ select · ←→ change · Tab page · Esc home",
            Page::Saves => "↑↓ save · Enter restore · s save · Esc home",
            Page::Diagnostics => "Enter run checks · d details · r retry · Esc home",
            Page::Admin => "↑↓ person · ←→ detail · p pause · o observe · r recover · Esc home",
        };
        let footer = vec![
            Line::styled(self.text(help), Style::default().fg(ASH)),
            Line::styled(
                ui::clean(&self.message, false),
                Style::default().fg(if self.failure.is_some() { BLOOD } else { SAGE }),
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
                    "{} · {}\n{}",
                    self.text("New game"),
                    scenario_name(&self.settings.scenario, self.language()),
                    self.text("Enter start · Esc cancel. Connected players must rejoin.")
                ),
            );
        }
        if self.confirm_stop {
            self.modal(
                frame,
                &format!(
                    "{}\n{}",
                    self.text("Save and stop this game for every connected player?"),
                    self.text("Enter stop · Esc cancel")
                ),
            );
        }
    }
    fn home(&self, frame: &mut Frame, area: Rect) {
        let mut lines = self
            .home_actions()
            .iter()
            .enumerate()
            .map(|(index, label)| {
                Line::styled(
                    format!(
                        "{} {}",
                        if index == self.home_selected {
                            "›"
                        } else {
                            " "
                        },
                        self.text(label)
                    ),
                    Style::default().fg(if index == self.home_selected {
                        AMBER
                    } else {
                        PALE
                    }),
                )
            })
            .collect::<Vec<_>>();
        if self.active {
            lines.push(Line::from(""));
            lines.push(Line::raw(format!(
                "{}: {}",
                self.text("Connected"),
                self.actors()
                    .iter()
                    .filter(|actor| actor["connected"] == true)
                    .count()
            )));
            lines.push(Line::raw(format!(
                "{}: {}",
                self.text("LAN join command"),
                string(&self.view["ssh"], "command")
            )));
            if self.view["settings"].is_null() {
                lines.push(Line::styled(self.text("This game uses an older version. Save and restart it when ready to use the new settings."),Style::default().fg(ASH)));
            }
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }
    fn settings_page(&self, frame: &mut Frame, area: Rect) {
        let network = self.settings.bind.map_or_else(
            || self.text("Automatic LAN interface"),
            |bind| {
                self.networks
                    .iter()
                    .find(|network| network.address == bind)
                    .map_or_else(
                        || format!("{} · {bind}", self.text("Unavailable")),
                        |network| {
                            format!(
                                "{} · {} · {}",
                                network.name, network.address, network.subnet
                            )
                        },
                    )
            },
        );
        let values = [
            (
                "Language",
                if self.settings.language == "pl" {
                    "Polski".into()
                } else {
                    "English".into()
                },
            ),
            (
                "Scenario",
                scenario_name(&self.settings.scenario, self.language()),
            ),
            (
                "Luna reasoning effort",
                self.settings.reasoning_effort.as_str().into(),
            ),
            ("Maximum agents", self.settings.max_agents.to_string()),
            (
                "NPC thinking rate",
                format!("{}×", self.settings.npc_update_rate),
            ),
            ("LAN interface", network),
            ("SSH port", self.settings.port.to_string()),
            ("Maximum players", self.settings.max_players.to_string()),
        ];
        let mut lines = values
            .into_iter()
            .enumerate()
            .map(|(index, (label, value))| {
                Line::styled(
                    format!(
                        "{} {}: {value}",
                        if index == self.setting_selected {
                            "›"
                        } else {
                            " "
                        },
                        self.text(label)
                    ),
                    Style::default().fg(if index == self.setting_selected {
                        AMBER
                    } else {
                        PALE
                    }),
                )
            })
            .collect::<Vec<_>>();
        lines.push(Line::from(""));
        for text in [
            "Effort, agent limit and NPC rate apply to the next decisions.",
            "Language and scenario apply to a new or restored castle.",
            "Network and player limits apply to the next host launch.",
            "NPCs use Luna at standard speed.",
        ] {
            lines.push(Line::styled(self.text(text), Style::default().fg(ASH)));
        }
        if self.efforts.is_empty() {
            lines.push(Line::styled(
                self.text("Run Diagnostics to discover available Luna efforts."),
                Style::default().fg(ASH),
            ));
        }
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((
                self.setting_selected
                    .saturating_sub(area.height.saturating_sub(1) as usize) as u16,
                0,
            )),
            area,
        );
    }
    fn setting_patch(&self, reverse: bool) -> Result<Value> {
        let settings = &self.settings;
        ensure!(
            !self.active
                || !self.view["settings"].is_null()
                || matches!(self.setting_selected, 0 | 1 | 3 | 4),
            "This running host is an older version. Save and restart it when ready to use the new settings."
        );
        Ok(match self.setting_selected {
            0 => json!({"language":if settings.language=="pl"{"en"}else{"pl"}}),
            1 => json!({"scenario":cycle(&scenario_ids(),&settings.scenario,reverse)}),
            2 => {
                ensure!(
                    !self.efforts.is_empty(),
                    "Run Diagnostics to discover the supported Luna reasoning efforts"
                );
                ensure!(
                    !self.active || !self.view["settings"].is_null(),
                    "This running host is an older version and cannot change reasoning effort. Save and restart it when ready."
                );
                json!({"reasoningEffort":cycle(&self.efforts,&settings.reasoning_effort,reverse)})
            }
            3 => {
                json!({"maxAgents":if reverse{settings.max_agents.saturating_sub(1).max(1)}else{settings.max_agents.saturating_add(1).min(HostSettings::MAX_AGENTS)}})
            }
            4 => {
                json!({"npcUpdateRate":cycle(&HostSettings::NPC_UPDATE_RATES,&settings.npc_update_rate,reverse)})
            }
            5 => {
                ensure!(
                    !self.networks.is_empty(),
                    "Cannot enumerate LAN interfaces; retry Diagnostics"
                );
                let index = self
                    .networks
                    .iter()
                    .position(|network| Some(network.address) == settings.bind);
                let index =
                    index.map_or(if reverse { self.networks.len() - 1 } else { 0 }, |index| {
                        (index + if reverse { self.networks.len() - 1 } else { 1 })
                            % self.networks.len()
                    });
                let network = &self.networks[index];
                json!({"bind":network.address,"subnet":network.subnet})
            }
            6 => {
                json!({"port":if reverse{settings.port.saturating_sub(1).max(1)}else{settings.port.saturating_add(1)}})
            }
            _ => {
                json!({"maxPlayers":if reverse{settings.max_players.saturating_sub(1).max(1)}else{settings.max_players.saturating_add(1).min(HostSettings::MAX_PLAYERS)}})
            }
        })
    }
    fn diagnostics_page(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::styled(
            self.text("Startup and connection checks"),
            Style::default().fg(AMBER),
        )];
        if let Some(failure) = &self.failure {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                self.text(&failure.summary),
                Style::default().fg(BLOOD),
            ));
            lines.push(Line::raw(self.text(&failure.action)));
            if self.show_details {
                lines.push(Line::from(""));
                lines.push(Line::raw(format!(
                    "{}: {}",
                    self.text("Details"),
                    ui::clean(&failure.details, false)
                )));
            }
            lines.push(Line::from(""));
        }
        if self.report.is_null() {
            lines.push(Line::raw(self.text(
                "Run checks to verify Codex login, isolation and Luna availability.",
            )));
        } else {
            for (label, key) in [
                ("Native Codex", "codex"),
                ("Sandbox", "sandbox"),
                ("Model", "model"),
                ("Reasoning effort", "effort"),
                ("Isolation", "isolation"),
            ] {
                let value = if key == "effort" {
                    self.settings.reasoning_effort.as_str()
                } else {
                    string(&self.report, key)
                };
                lines.push(Line::raw(format!(
                    "{}: {}",
                    self.text(label),
                    self.text(value)
                )));
            }
            lines.push(Line::raw(format!(
                "{}: {}",
                self.text("Available efforts"),
                self.efforts
                    .iter()
                    .map(|effort| effort.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        if self.active {
            lines.push(Line::from(""));
            let local_command =
                crate::player::LocalConnection::from_view(&self.directory, &self.view)
                    .and_then(|connection| connection.command())
                    .unwrap_or_else(crate::codex::safe_error);
            for (label, command) in [
                ("LAN join command", string(&self.view["ssh"], "command")),
                ("Local join command", local_command.as_str()),
            ] {
                lines.push(Line::raw(format!("{}: {}", self.text(label), command)));
            }
            let metrics = &self.view["metrics"];
            lines.push(Line::raw(format!(
                "{}: {} / {} · {}: {} · {}: {}",
                self.text("Thinking"),
                metrics["active"],
                self.settings.max_agents,
                self.text("calls"),
                metrics["calls"],
                self.text("tokens"),
                metrics["tokens"]
            )));
            if let Some(error) = self.view["error"].as_str() {
                lines.push(Line::styled(
                    ui::clean(error, false),
                    Style::default().fg(BLOOD),
                ));
            }
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
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
    fn people(&self, frame: &mut Frame, area: Rect) {
        let list_height = (self.actors().len() as u16 + 2).min(area.height / 2).max(1);
        let parts =
            Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)]).split(area);
        let mut rows = vec![Line::styled(
            self.text("↑↓ person · ←→ detail · PgUp/PgDn scroll"),
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
                        format!(
                            "{}: {}",
                            self.text("Current failure"),
                            ui::clean(error, false)
                        ),
                        Style::default().fg(BLOOD),
                    ));
                }
                if let Some(failure) = self
                    .actors()
                    .get(self.selected)
                    .and_then(|a| a["status"]["lastFailure"].as_object())
                {
                    let time = failure.get("time").and_then(Value::as_f64).unwrap_or(0.);
                    let phase = failure.get("phase").and_then(Value::as_str).unwrap_or("");
                    let message = failure.get("message").and_then(Value::as_str).unwrap_or("");
                    detail.push(Line::styled(
                        format!(
                            "{} · {} · {}: {}",
                            self.text("Last failure"),
                            crate::model::clock_text(time, string(&self.view, "language")),
                            self.text(phase),
                            ui::clean(message, false)
                        ),
                        Style::default().fg(ASH),
                    ));
                }
                if let Some(failure) = actor["lastPlanFailure"].as_object() {
                    detail.push(Line::styled(
                        format!(
                            "{}: {}",
                            self.text("Last plan failure"),
                            failure.get("reason").and_then(Value::as_str).unwrap_or("")
                        ),
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

    fn movement(&mut self, up: bool) -> Option<u16> {
        self.scroll = 0;
        let step = |current: usize, len: usize| (current + if up { len - 1 } else { 1 }) % len;
        match self.page {
            Page::Home => self.home_selected = step(self.home_selected, self.home_actions().len()),
            Page::Settings => self.setting_selected = step(self.setting_selected, 8),
            Page::Admin if !self.actors().is_empty() => {
                self.selected = step(self.selected, self.actors().len());
                return self.actors()[self.selected]["id"]
                    .as_u64()
                    .map(|id| id as u16);
            }
            Page::Saves if !self.saves().is_empty() => {
                self.save_selected = step(self.save_selected, self.saves().len())
            }
            _ => {}
        }
        None
    }
}

fn cycle<T: Clone + PartialEq>(options: &[T], current: &T, reverse: bool) -> T {
    let index = options
        .iter()
        .position(|option| option == current)
        .unwrap_or(0);
    options[(index + if reverse { options.len() - 1 } else { 1 }) % options.len()].clone()
}

enum MenuResult {
    Attached(Option<Value>),
    Started(Box<HostSession>),
    Checked(Value),
    Updated(Value, bool),
}

async fn probe(directory: &Path) -> Result<Option<Value>> {
    match UnixStream::connect(paths::manager_socket(directory)?).await {
        Ok(stream) => {
            drop(stream);
            request(directory, "view", json!({})).await.map(Some)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn start_job(
    directory: &Path,
    settings: &HostSettings,
    start: Start,
    options: &HostOptions,
) -> JoinHandle<Result<MenuResult>> {
    let directory = directory.to_owned();
    let settings = settings.clone();
    let options = options.clone();
    tokio::spawn(async move {
        if let Some(view) = probe(&directory).await? {
            return Ok(MenuResult::Attached(Some(view)));
        }
        Ok(MenuResult::Started(Box::new(
            HostSession::start(&directory, &settings, &start, &options).await?,
        )))
    })
}

fn check_job(
    directory: &Path,
    settings: &HostSettings,
    options: &HostOptions,
    link: Option<Link>,
) -> JoinHandle<Result<MenuResult>> {
    let directory = directory.to_owned();
    let settings = settings.clone();
    let options = options.worker.clone();
    tokio::spawn(async move {
        if let Some(link) = link {
            return Ok(MenuResult::Checked(
                link.request("diagnostics", json!({})).await?,
            ));
        }
        let worker = crate::diagnostics::prepare(
            &directory,
            &settings.scenario,
            &settings.language,
            options,
            settings.reasoning_effort,
        )
        .await?;
        let report = worker.report.clone();
        worker.driver.reset().await;
        Ok(MenuResult::Checked(report))
    })
}

fn operation_job(
    link: Link,
    method: &'static str,
    params: Value,
    launch: bool,
) -> JoinHandle<Result<MenuResult>> {
    tokio::spawn(async move {
        let result = link.request(method, params).await?;
        ensure!(result["ok"] != false, "{}", string(&result, "message"));
        Ok(MenuResult::Updated(
            link.request("view", json!({})).await?,
            launch,
        ))
    })
}

fn play(ui: &mut MenuUi, directory: &Path) {
    match crate::player::launch(directory, &ui.view) {
        Ok(true) => ui.message = ui.text("Player window requested. Choose your name there."),
        Ok(false) => match crate::player::LocalConnection::from_view(directory, &ui.view)
            .and_then(|connection| connection.command())
        {
            Ok(command) => {
                ui.failure = Some(Failure {
                    summary: "A separate player window is unavailable.".into(),
                    action: "Open another terminal and run the local join command in Details."
                        .into(),
                    details: command,
                });
                ui.page = Page::Diagnostics;
                ui.show_details = true;
                ui.message = ui.text("Connection instructions are shown in Diagnostics.");
            }
            Err(error) => ui.fail(error),
        },
        Err(error) => ui.fail(error),
    }
}

pub async fn run(directory: &Path, options: MenuOptions) -> Result<()> {
    let (_screen, mut terminal) = Screen::open()?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ui = MenuUi {
        directory: directory.into(),
        ..Default::default()
    };
    let mut overrides = if options.overrides.is_null() {
        json!({})
    } else {
        options.overrides
    };
    match HostSettings::load_with_overrides(directory, &overrides) {
        Ok(settings) => {
            ui.settings = settings;
            ui.config_valid = true;
        }
        Err(error) => ui.fail(error.context(format!(
            "Cannot read server-settings.json in {}",
            directory.display()
        ))),
    }
    match networks() {
        Ok(networks) => ui.networks = networks,
        Err(error) => ui.fail(error),
    }
    match crate::host::idle_view(directory, &ui.settings) {
        Ok(view) => ui.set_view(view),
        Err(error) => ui.fail(error),
    }
    let mut sound = Audio::new(directory, options.audio)?;
    let mut session: Option<Box<HostSession>> = None;
    let mut link: Option<Link> = None;
    let initial_directory = directory.to_owned();
    let mut pending = Some(tokio::spawn(async move {
        Ok(MenuResult::Attached(probe(&initial_directory).await?))
    }));
    let mut poll: Option<JoinHandle<Result<Value>>> = None;
    ui.busy = "Looking for a running game…".into();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result:Result<()>=async {
    loop {
        if pending.as_ref().is_some_and(|job|job.is_finished()) {
            let result=pending.take().unwrap().await.context("Menu action stopped")?;
            ui.busy.clear();
            match result {
                Ok(MenuResult::Attached(Some(view)))=> {
                    ui.failure=None;
                    ui.active=true;ui.set_view(view);ui.config_valid=true;link=Some(Link::Remote(directory.into()));
                    ui.message=ui.text("Connected to the existing host. Play opens your player window.");
                }
                Ok(MenuResult::Attached(None))=> {
                    if options.attach_only {ui.fail(anyhow::anyhow!("No host is running for {}. Start it with below-the-crown.",directory.display()));}
                    else if options.host.import_snapshot.is_some()&&ui.config_valid {pending=Some(start_job(directory,&ui.settings,options.start.clone(),&options.host));ui.busy="Starting the game…".into();}
                }
                Ok(MenuResult::Started(owned))=> {
                    let view=owned.handle.request("view",json!({})).await?;
                    ui.report=owned.report.clone();ui.active=true;ui.owns_host=true;ui.set_view(view);
                    ui.efforts=serde_json::from_value(ui.report["supportedEfforts"].clone()).unwrap_or_default();
                    link=Some(Link::Local(owned.handle.clone()));session=Some(owned);ui.page=Page::Home;ui.home_selected=0;ui.failure=None;
                    play(&mut ui,directory);
                }
                Ok(MenuResult::Checked(report))=> {
                    ui.efforts=serde_json::from_value(report["supportedEfforts"].clone()).unwrap_or_default();
                    let validation=crate::diagnostics::require_effort(&report);
                    ui.report=report;
                    match validation { Ok(())=>{ui.failure=None;ui.message=ui.text("Startup checks passed. Play is ready.");}, Err(error)=>ui.fail(error) }
                    if let Ok(networks)=networks(){ui.networks=networks;}
                }
                Ok(MenuResult::Updated(view,launch))=> {
                    ui.failure=None;ui.set_view(view);ui.config_valid=true;ui.message=ui.text("Done.");
                    if launch {play(&mut ui,directory);}
                }
                Err(error)=>ui.fail(error),
            }
        }
        if poll.as_ref().is_some_and(|job|job.is_finished()) {
            match poll.take().unwrap().await.context("Host refresh stopped")? {
                Ok(view)=>ui.set_view(view),
                Err(error)=>{ui.fail(error);link=None;ui.active=false;}
            }
        }
        if session.as_ref().is_some_and(|host|host.is_finished()) {
            let mut owned=session.take().unwrap();
            if let Err(error)=owned.close().await {ui.fail(error);}
            link=None;ui.active=false;ui.owns_host=false;
        }
        terminal.draw(|frame|ui.render(frame))?;
        sound.tick();
        let event=tokio::select! {
            _=terminate.recv()=>break,
            _=ticker.tick()=> {
                if let Some(current)=&link&&poll.is_none()&&pending.is_none(){let current=current.clone();poll=Some(tokio::spawn(async move{current.view().await}));}
                continue;
            },
            event=events.next()=>event.context("Terminal closed")??,
        };
        let key=match event {
            Event::Key(key) if key.kind!=KeyEventKind::Release=>key,
            Event::Paste(text)=>{if let Some(draft)=ui.save_draft.as_mut(){draft.insert(&text);}continue;},
            _=>continue,
        };
        let quit=key.modifiers.contains(KeyModifiers::CONTROL)&&key.code==KeyCode::Char('c')||key.code==KeyCode::Char('q')&&ui.save_draft.is_none()&&!ui.confirm_new;
        if quit&&!ui.confirm_stop {
            if ui.owns_host {ui.confirm_stop=true;}else {break;}
            continue;
        }
        if ui.confirm_stop {
            match key.code {KeyCode::Enter=>break,KeyCode::Esc=>ui.confirm_stop=false,_=>{}}
            continue;
        }
        if let Some(mut draft)=ui.save_draft.take() {
            match key.code {
                KeyCode::Esc=>{},
                KeyCode::Enter if pending.is_none()=>{if let Some(current)=&link{pending=Some(operation_job(current.clone(),"save",json!({"name":draft.text.trim()}),false));ui.busy="Saving the castle…".into();}},
                _=>{edit(&mut draft,key);ui.save_draft=Some(draft);}
            }
            continue;
        }
        if ui.confirm_new {
            match key.code {
                KeyCode::Esc=>ui.confirm_new=false,
                KeyCode::Enter if pending.is_none()=> {
                    ui.confirm_new=false;
                    if let Some(current)=&link{pending=Some(operation_job(current.clone(),"new",json!({"seed":seed()}),false));}
                    else if ui.config_valid&&!options.attach_only {pending=Some(start_job(directory,&ui.settings,Start::New,&options.host));}
                    ui.busy="Starting a new castle…".into();ui.page=Page::Home;
                }
                _=>{}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('1'..='5')=>{if let KeyCode::Char(c)=key.code{ui.page=Page::ALL[(c as usize)-('1' as usize)];ui.scroll=0;}},
            KeyCode::Tab=>{ui.page=cycle(&Page::ALL,&ui.page,key.modifiers.contains(KeyModifiers::SHIFT));ui.scroll=0;},
            KeyCode::Esc=>{ui.page=Page::Home;ui.scroll=0;},
            KeyCode::Up|KeyCode::Down=> {
                if let Some(id)=ui.movement(key.code==KeyCode::Up)&&pending.is_none()&&let Some(current)=&link{pending=Some(operation_job(current.clone(),"select",json!({"actorId":id}),false));}
            }
            KeyCode::PageUp=>ui.scroll=ui.scroll.saturating_sub(8),
            KeyCode::PageDown=>ui.scroll=ui.scroll.saturating_add(8),
            KeyCode::Char('d')=>{ui.page=Page::Diagnostics;ui.show_details = !ui.show_details;},
            KeyCode::Left|KeyCode::Right if ui.page==Page::Admin=>{ui.detail=(ui.detail+if key.code==KeyCode::Left{4}else{1})%5;ui.scroll=0;},
            KeyCode::Left|KeyCode::Right if ui.page==Page::Settings&&pending.is_none()=> {
                match ui.setting_patch(key.code==KeyCode::Left) {
                    Ok(patch)=> {
                        if let Some(current)=&link {pending=Some(operation_job(current.clone(),"settings",patch,false));}
                        else {
                            for (key,value) in patch.as_object().unwrap(){overrides[key]=value.clone();}
                            let directory=directory.to_owned();let saved_overrides=overrides.clone();
                            pending=Some(tokio::spawn(async move{let settings=HostSettings::load_with_overrides(&directory,&saved_overrides)?;settings.save(&directory)?;Ok(MenuResult::Updated(crate::host::idle_view(&directory,&settings)?,false))}));
                        }
                        ui.busy="Applying settings…".into();
                    }
                    Err(error)=>ui.fail(error),
                }
            }
            KeyCode::Enter if ui.page==Page::Home&&pending.is_none()=> {
                match ui.home_selected {
                    0=> {
                        if let Some(current)=&link {
                            if ui.view["manualPause"]==true {pending=Some(operation_job(current.clone(),"pause",json!({"paused":false}),true));}
                            else {play(&mut ui,directory);}
                        } else if ui.config_valid&&!options.attach_only {pending=Some(start_job(directory,&ui.settings,options.start.clone(),&options.host));ui.busy="Checking login, model and network…".into();}
                        else {ui.page=Page::Diagnostics;ui.message=ui.text("Correct the startup problem and retry Diagnostics before playing.");}
                    }
                    1=> {if ui.config_valid&&(ui.active||!options.attach_only){ui.confirm_new=true;}else{ui.page=Page::Diagnostics;}},
                    2=>ui.page=Page::Settings,
                    3=>ui.page=Page::Saves,
                    4=>ui.page=Page::Diagnostics,
                    5=>ui.page=Page::Admin,
                    _=>{if ui.owns_host{ui.confirm_stop=true;}else{break;}}
                }
            }
            KeyCode::Enter if ui.page==Page::Saves&&pending.is_none()=> {
                if let Some(save)=ui.saves().get(ui.save_selected) {
                    let name=string(save,"name").to_owned();
                    if let Some(current)=&link {pending=Some(operation_job(current.clone(),"load",json!({"name":name}),false));}
                    else if ui.config_valid&&!options.attach_only{pending=Some(start_job(directory,&ui.settings,Start::Save(name),&options.host));}
                    ui.busy="Restoring the castle…".into();
                }
            }
            KeyCode::Char('s') if ui.page==Page::Saves&&ui.active=>{let mut draft=ui::Draft::default();draft.set("quicksave".into());ui.save_draft=Some(draft);},
            KeyCode::Char('p') if ui.page==Page::Admin&&pending.is_none()=>{if let Some(current)=&link{pending=Some(operation_job(current.clone(),"pause",json!({"paused":!ui.view["manualPause"].as_bool().unwrap_or(false)}),false));}},
            KeyCode::Char('o') if ui.page==Page::Admin&&pending.is_none()=>{if let Some(current)=&link{pending=Some(operation_job(current.clone(),"observe",json!({"enabled":!ui.view["observing"].as_bool().unwrap_or(false)}),false));}},
            KeyCode::Char('r') if ui.page==Page::Admin&&pending.is_none()=>{if let Some(current)=&link{pending=Some(operation_job(current.clone(),"recover",json!({}),false));}},
            KeyCode::Enter|KeyCode::Char('r') if ui.page==Page::Diagnostics&&pending.is_none()=> {
                if !ui.config_valid {
                    match HostSettings::load_with_overrides(directory,&overrides){Ok(settings)=>{ui.settings=settings;ui.config_valid=true;},Err(error)=>{ui.fail(error);continue;}}
                }
                let current=if ui.view["settings"].is_object(){link.clone()}else{None};
                pending=Some(check_job(directory,&ui.settings,&options.host,current));ui.busy="Checking Codex login and model…".into();
            }
            _=>{}
        }
        if ui.page==Page::Settings&&ui.efforts.is_empty()&&pending.is_none()&&ui.failure.is_none() {
            let current=if ui.view["settings"].is_object(){link.clone()}else{None};
            pending=Some(check_job(directory,&ui.settings,&options.host,current));ui.busy="Discovering Luna reasoning efforts…".into();
        }
        if pending.is_some()&&let Some(job)=poll.take(){job.abort();let _=job.await;}
    }
    Ok(())
    }.await;
    let pending_closed = close_pending(pending).await;
    if let Some(job) = poll {
        job.abort();
        let _ = job.await;
    }
    let closed = if let Some(mut owned) = session {
        owned.close().await
    } else {
        Ok(())
    };
    result?;
    pending_closed?;
    closed
}

async fn close_pending(pending: Option<JoinHandle<Result<MenuResult>>>) -> Result<()> {
    if let Some(job) = pending {
        job.abort();
        if let Ok(Ok(MenuResult::Started(mut host))) = job.await {
            host.close().await?;
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
        assert_eq!(cycle(&values, &"a".to_owned(), true), "c");
        assert_eq!(cycle(&values, &"c".to_owned(), false), "a");
        assert_eq!(cycle(&Page::ALL, &Page::Home, true), Page::Admin);
        let mut ui = MenuUi {
            page: Page::Admin,
            ..Default::default()
        };
        ui.set_view(json!({"actors":[{"id":10},{"id":11},{"id":12}]}));
        assert_eq!(ui.movement(true), Some(12));
        assert_eq!(ui.movement(false), Some(10));
    }
    #[test]
    fn shared_editor_emits_valid_patches_and_only_advertised_efforts() {
        let mut ui = MenuUi {
            efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
            ..Default::default()
        };
        ui.setting_selected = 2;
        assert_eq!(
            ui.setting_patch(false).unwrap(),
            json!({"reasoningEffort":"low"})
        );
        ui.efforts.clear();
        assert!(ui.setting_patch(false).is_err());
        ui.setting_selected = 3;
        ui.settings.max_agents = 1;
        assert_eq!(ui.setting_patch(true).unwrap(), json!({"maxAgents":1}));
        ui.setting_selected = 6;
        ui.settings.port = u16::MAX;
        assert_eq!(ui.setting_patch(false).unwrap(), json!({"port":u16::MAX}));
        ui.active = true;
        ui.view = json!({});
        ui.setting_selected = 2;
        ui.efforts = ReasoningEffort::ALL.into();
        assert!(ui.setting_patch(false).is_err());
    }
    #[test]
    fn discovered_networks_have_real_private_subnets() {
        let values = networks().unwrap();
        assert!(!values.is_empty());
        for network in values {
            assert!(network.subnet.contains(&network.address));
            assert!(
                network.address.is_private()
                    || network.address.is_loopback()
                    || network.address.is_link_local()
            );
        }
    }
    #[tokio::test]
    async fn quitting_awaits_cleanup_of_a_completed_pending_startup() {
        let directory = tempfile::tempdir().unwrap();
        let host = HostSession::fixture(directory.path()).await;
        let port = host.handle.info.port;
        let handle = host.handle.clone();
        let job = tokio::spawn(async move { Ok(MenuResult::Started(Box::new(host))) });
        while !job.is_finished() {
            tokio::task::yield_now().await;
        }
        close_pending(Some(job)).await.unwrap();
        assert!(handle.request("view", json!({})).await.is_err());
        assert!(!directory.path().join("server.json").exists());
        assert!(!paths::manager_socket(directory.path()).unwrap().exists());
        assert!(
            crate::store::Store::open(directory.path())
                .unwrap()
                .saves()
                .unwrap()
                .contains(&"autosave".into())
        );
        let _released_port = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let _released_lock = paths::lock_server(directory.path()).unwrap();
    }
    #[tokio::test]
    async fn ipc_frames_are_bounded_and_unicode_safe() {
        let mut valid = BufReader::new("{\"name\":\"Łucja\"}\n".as_bytes());
        let value: Value = serde_json::from_slice(&frame(&mut valid, 80).await.unwrap()).unwrap();
        assert_eq!(value["name"], "Łucja");
        let mut too_big = BufReader::new(&b"123456789\n"[..]);
        assert!(frame(&mut too_big, 8).await.is_err());
        let mut truncated = BufReader::new(&b"123"[..]);
        assert!(frame(&mut truncated, 20).await.is_err());
    }
    #[test]
    fn diagnostics_use_the_same_local_connection_for_an_older_host() {
        let directory = Path::new("/tmp/old-castle");
        let view = json!({"ssh":{"port":2281,"localCommand":"obsolete unpinned command"}});
        let ui = MenuUi {
            directory: directory.into(),
            active: true,
            page: Page::Diagnostics,
            view: view.clone(),
            ..Default::default()
        };
        let expected = crate::player::LocalConnection::from_view(directory, &view)
            .unwrap()
            .command()
            .unwrap();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 32)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        // Word wrapping adds padding between command arguments across screen rows.
        let visible = screen.split_whitespace().collect::<Vec<_>>().join(" ");
        let expected = expected.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(visible.contains(&expected), "{visible}");
        assert!(!screen.contains("obsolete unpinned command"));
    }
    #[test]
    fn diagnostics_show_current_effort_after_a_settings_update() {
        let mut ui = MenuUi {
            page: Page::Diagnostics,
            report: json!({"effort":"high","model":crate::model::MODEL}),
            ..Default::default()
        };
        let settings = HostSettings {
            reasoning_effort: ReasoningEffort::Low,
            ..Default::default()
        };
        ui.set_view(json!({"settings":settings}));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 32)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Reasoning effort: low"));
        assert!(!screen.contains("Reasoning effort: high"));
    }
    #[test]
    fn every_menu_state_and_page_stays_bounded_in_both_languages() {
        use ratatui::backend::TestBackend;
        for language in ["en", "pl"] {
            for active in [false, true] {
                let mut ui = MenuUi {
                    active,
                    settings: HostSettings {
                        language: language.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                ui.set_view(
                    json!({"actors":[],"saves":[],"metrics":{"active":0,"calls":0,"tokens":0}}),
                );
                for state in 0..3 {
                    ui.busy = if state == 1 {
                        "Starting the game…".into()
                    } else {
                        String::new()
                    };
                    ui.failure = if state == 2 {
                        Some(Failure::from_details(
                            "Could not bind localhost SSH: Address already in use",
                        ))
                    } else {
                        None
                    };
                    for (width, height) in [(1, 1), (12, 4), (30, 8), (80, 24), (160, 50)] {
                        for page in Page::ALL {
                            ui.page = page;
                            let mut terminal =
                                Terminal::new(TestBackend::new(width, height)).unwrap();
                            terminal.draw(|frame| ui.render(frame)).unwrap();
                            assert_eq!(
                                terminal.backend().buffer().area,
                                Rect::new(0, 0, width, height)
                            );
                        }
                    }
                }
            }
        }
    }
    #[tokio::test]
    async fn probe_never_replaces_an_idle_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        assert!(probe(directory.path()).await.unwrap().is_none());
        let server = IpcServer::bind(directory.path()).await.unwrap();
        let path = server.path.clone();
        drop(server);
        assert!(!path.exists());
    }
}
