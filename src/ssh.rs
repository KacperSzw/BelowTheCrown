//! The only player transport: a bounded game PTY on localhost and the selected LAN.
use crate::{
    paths,
    runtime::{GameHandle, PlayerConnection, RuntimeRequest},
    ui::{self, Draft, Operation, PlayerUi, Submission},
};
use anyhow::{Context, Result, anyhow, ensure};
use ipnet::Ipv4Net;
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Rect, Size},
};
use russh::{
    Channel, ChannelId, ChannelWriteHalf, Disconnect, Pty,
    keys::{self, HashAlg, PrivateKey, PublicKey},
    server::{self, Auth, Session},
};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use termwiz::input::{InputEvent, InputParser, KeyCode, Modifiers};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct SshOptions {
    pub bind: Ipv4Addr,
    pub port: u16,
    pub subnet: Ipv4Net,
    pub maximum_players: usize,
    pub authorized_keys: Option<PathBuf>,
}
impl SshOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.bind.is_private() || self.bind.is_loopback() || self.bind.is_link_local(),
            "Select a private LAN IPv4 address or localhost."
        );
        ensure!(
            !self.bind.is_unspecified() && self.subnet.contains(&self.bind),
            "The selected LAN address must belong to its subnet."
        );
        let private = |ip: Ipv4Addr| ip.is_private() || ip.is_loopback() || ip.is_link_local();
        ensure!(
            private(self.subnet.network()) && private(self.subnet.broadcast()),
            "The guest subnet must stay inside a private LAN network."
        );
        ensure!(
            (1..=32).contains(&self.maximum_players),
            "Choose a player limit between 1 and 32."
        );
        Ok(())
    }
    pub fn permits(&self, address: IpAddr) -> bool {
        match address {
            IpAddr::V4(ip) => ip.is_loopback() || self.subnet.contains(&ip),
            IpAddr::V6(_) => false,
        }
    }
}

pub struct SshServer {
    pub port: u16,
    pub fingerprint: String,
    pub known_hosts: PathBuf,
    pub endpoints: Vec<SocketAddr>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<Result<()>>>,
}
impl SshServer {
    pub async fn close(&mut self) {
        self.cancel.cancel();
        for mut task in self.tasks.drain(..) {
            if timeout(Duration::from_secs(4), &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}
impl Drop for SshServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub async fn serve(handle: GameHandle, options: SshOptions) -> Result<SshServer> {
    options.validate()?;
    // Host console environment flags must not remove remote voice/action colors.
    crossterm::style::force_color_output(true);
    let directory = handle.info.data_dir.join("ssh");
    paths::private_dir(&directory)?;
    let host_key = host_key(&directory.join("host_ed25519"))?;
    let fingerprint = host_key
        .public_key()
        .fingerprint(HashAlg::Sha256)
        .to_string();
    let authorized = match &options.authorized_keys {
        Some(path) => Some(Arc::new(authorized_keys(path)?)),
        None => None,
    };
    let listener = TcpListener::bind((options.bind, options.port))
        .await
        .context("Could not bind the selected LAN SSH address")?;
    let port = listener.local_addr()?.port();
    let mut listeners = vec![listener];
    if options.bind != Ipv4Addr::LOCALHOST {
        listeners.push(
            TcpListener::bind((Ipv4Addr::LOCALHOST, port))
                .await
                .context("Could not bind localhost SSH")?,
        );
    }
    let endpoints = listeners
        .iter()
        .map(TcpListener::local_addr)
        .collect::<std::io::Result<Vec<_>>>()?;
    let public = host_key.public_key().to_openssh()?;
    let known_hosts = directory.join("known_hosts");
    let lines = endpoints
        .iter()
        .map(|address| format!("[{}]:{} {public}\n", address.ip(), address.port()))
        .collect::<String>();
    paths::write_private(&known_hosts, lines.as_bytes(), false)?;
    let config = Arc::new(server::Config {
        keys: vec![host_key],
        max_auth_attempts: 8,
        auth_rejection_time: Duration::from_millis(200),
        auth_rejection_time_initial: Some(Duration::ZERO),
        window_size: 131_072,
        maximum_packet_size: 32_768,
        channel_buffer_size: 8,
        event_buffer_size: 8,
        inactivity_timeout: Some(Duration::from_secs(3600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..server::Config::default()
    });
    let cancel = CancellationToken::new();
    let counts = Arc::new(Mutex::new(HashMap::<IpAddr, usize>::new()));
    let mut tasks = Vec::new();
    for listener in listeners {
        let game = handle.clone();
        let options = options.clone();
        let config = config.clone();
        let authorized = authorized.clone();
        let cancel = cancel.clone();
        let counts = counts.clone();
        tasks.push(tokio::spawn(async move {
            accept_loop(listener, game, options, config, authorized, cancel, counts).await
        }));
    }
    Ok(SshServer {
        port,
        fingerprint,
        known_hosts,
        endpoints,
        cancel,
        tasks,
    })
}

fn host_key(path: &Path) -> Result<PrivateKey> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "The game SSH host key must be a regular private file"
        );
        ensure!(
            metadata.len() <= 65_536,
            "The game SSH host key is too large"
        );
    }
    if !path.exists() {
        let mut seed = [0u8; 32];
        fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
        let pair = keys::ssh_key::private::Ed25519Keypair::from_seed(&seed);
        let key = PrivateKey::new(
            keys::ssh_key::private::KeypairData::Ed25519(pair),
            "Below the Crown",
        )?;
        let pem = key.to_openssh(keys::ssh_key::LineEnding::LF)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(pem.as_bytes())?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let key = keys::load_secret_key(path, None).context("The game SSH host key is unreadable")?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(key)
}
fn authorized_keys(path: &Path) -> Result<Vec<PublicKey>> {
    let file = fs::read(path)?;
    ensure!(
        file.len() <= 65_536,
        "The authorized-key file is too large."
    );
    let text = std::str::from_utf8(&file)?;
    let keys = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            PublicKey::from_openssh(line)
                .context("Use plain public keys in the game authorized-key file.")
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        !keys.is_empty(),
        "The authorized-key file contains no public keys."
    );
    Ok(keys)
}
struct AddressGuard {
    address: IpAddr,
    counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
}
impl Drop for AddressGuard {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.counts.lock()
            && let Some(count) = counts.get_mut(&self.address)
        {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.address);
            }
        }
    }
}
struct SocketGuard(std::net::TcpStream);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

async fn accept_loop(
    listener: TcpListener,
    game: GameHandle,
    options: SshOptions,
    config: Arc<server::Config>,
    authorized: Option<Arc<Vec<PublicKey>>>,
    cancel: CancellationToken,
    counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
) -> Result<()> {
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            _=cancel.cancelled()=>break,
            result=sessions.join_next(),if !sessions.is_empty()=>{let _=result;},
            incoming=listener.accept()=>{
                let (socket,address)=incoming?;
                if !options.permits(address.ip()){continue;}
                let accepted={let mut counts=counts.lock().map_err(|_|anyhow!("SSH connection registry failed"))?;let total:usize=counts.values().sum();let existing=counts.get(&address.ip()).copied().unwrap_or(0);if total>=64 || existing>=8{false}else{counts.insert(address.ip(),existing+1);true}};
                if !accepted{continue;}
                let guard=AddressGuard{address:address.ip(),counts:counts.clone()};
                let config=config.clone();let game=game.clone();let keys=authorized.clone();let token=cancel.child_token();
                sessions.spawn(async move{
                    let _address=guard;
                    let standard=socket.into_std()?;let _socket=SocketGuard(standard.try_clone()?);let socket=tokio::net::TcpStream::from_std(standard)?;
                    let setup_done=CancellationToken::new();
                    let handler=GuestHandler{game,identity:None,authorized:keys,channel:None,token:token.clone(),setup_done:setup_done.clone()};
                    let session=timeout(Duration::from_secs(10),server::run_stream(config,socket,handler)).await??;
                    let session_handle=session.handle();
                    let setup_handle=session_handle.clone();
                    let setup_task=tokio::spawn(async move{tokio::select!{_=setup_done.cancelled()=>{},_=tokio::time::sleep(Duration::from_secs(15))=>{let _=setup_handle.disconnect(Disconnect::ByApplication,"Open a game PTY to join.".into(),"".into()).await;}}});
                    tokio::select!{
                        _=token.cancelled()=>{let _=_socket.0.shutdown(Shutdown::Both);},
                        result=session=>{let _=result;},
                    }
                    setup_task.abort();token.cancel();
                    Ok::<(),anyhow::Error>(())
                });
            }
        }
    }
    cancel.cancel();
    if timeout(Duration::from_secs(3), async {
        while sessions.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        sessions.abort_all();
        while sessions.join_next().await.is_some() {}
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct PtyInfo {
    width: u16,
    height: u16,
    term: String,
    locale: Option<String>,
}
fn dimensions(width: u32, height: u32) -> (u16, u16) {
    (
        if width == 0 { 80 } else { width.clamp(20, 400) } as u16,
        if height == 0 {
            24
        } else {
            height.clamp(8, 160)
        } as u16,
    )
}
pub fn supports_braille(term: &str, locale: Option<&str>) -> bool {
    let term = term.to_lowercase();
    let modern = [
        "xterm",
        "kitty",
        "wezterm",
        "alacritty",
        "foot",
        "screen",
        "tmux",
        "rxvt",
    ]
    .iter()
    .any(|prefix| term.starts_with(prefix));
    modern
        && locale
            .map(|value| {
                let value = value.to_lowercase();
                value.contains("utf-8") || value.contains("utf8")
            })
            .unwrap_or(true)
}
fn locale_hint(name: &str, value: &str) -> bool {
    matches!(name, "LANG" | "LC_CTYPE")
        && !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.@-".contains(&b))
}
struct GuestChannel {
    id: ChannelId,
    pty: Option<PtyInfo>,
    writer: Option<ChannelWriteHalf<server::Msg>>,
    input: mpsc::Sender<Vec<u8>>,
    receiver: Option<mpsc::Receiver<Vec<u8>>>,
    size: watch::Sender<Option<PtyInfo>>,
    started: bool,
    bytes: usize,
    window: Instant,
}
struct GuestHandler {
    game: GameHandle,
    identity: Option<String>,
    authorized: Option<Arc<Vec<PublicKey>>>,
    channel: Option<GuestChannel>,
    token: CancellationToken,
    setup_done: CancellationToken,
}
impl Drop for GuestHandler {
    fn drop(&mut self) {
        self.token.cancel();
    }
}
impl server::Handler for GuestHandler {
    type Error = anyhow::Error;
    async fn auth_none(&mut self, _user: &str) -> Result<Auth> {
        Ok(if self.authorized.is_none() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
    async fn auth_publickey_offered(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        Ok(
            if self.authorized.as_ref().is_some_and(|keys| {
                keys.iter()
                    .any(|allowed| allowed.key_data() == key.key_data())
            }) {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }
    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        if self.authorized.as_ref().is_some_and(|keys| {
            keys.iter()
                .any(|allowed| allowed.key_data() == key.key_data())
        }) {
            self.identity = Some(key.fingerprint(HashAlg::Sha256).to_string());
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        if self.channel.is_some() {
            return Ok(());
        }
        let id = channel.id();
        let (_reader, writer) = channel.split();
        let (input, receiver) = mpsc::channel(16);
        let (size, _) = watch::channel(None);
        self.channel = Some(GuestChannel {
            id,
            pty: None,
            writer: Some(writer),
            input,
            receiver: Some(receiver),
            size,
            started: false,
            bytes: 0,
            window: Instant::now(),
        });
        reply.accept().await;
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        width: u32,
        height: u32,
        _pixels_width: u32,
        _pixels_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<()> {
        let Some(current) = self.channel.as_mut().filter(|c| c.id == channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        if current.pty.is_some()
            || current.started
            || term.len() > 64
            || !term
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            session.channel_failure(channel)?;
            return Ok(());
        }
        let (width, height) = dimensions(width, height);
        current.pty = Some(PtyInfo {
            width,
            height,
            term: term.into(),
            locale: None,
        });
        current.size.send_replace(current.pty.clone());
        session.channel_success(channel)?;
        Ok(())
    }
    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<()> {
        let Some(current) = self.channel.as_mut().filter(|c| c.id == channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        if !current.started && locale_hint(name, value) {
            if let Some(pty) = &mut current.pty {
                pty.locale = Some(value.into());
                current.size.send_replace(current.pty.clone());
            }
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }
    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        width: u32,
        height: u32,
        _pixels_width: u32,
        _pixels_height: u32,
        session: &mut Session,
    ) -> Result<()> {
        let Some(current) = self.channel.as_mut().filter(|c| c.id == channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        if let Some(pty) = &mut current.pty {
            (pty.width, pty.height) = dimensions(width, height);
            current.size.send_replace(current.pty.clone());
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }
    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<()> {
        let Some(current) = self.channel.as_mut().filter(|c| c.id == channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        if current.pty.is_none() || current.started {
            session.channel_failure(channel)?;
            return Ok(());
        }
        current.started = true;
        self.setup_done.cancel();
        session.channel_success(channel)?;
        let writer = current.writer.take().unwrap();
        let input = current.receiver.take().unwrap();
        let size = current.size.subscribe();
        let game = self.game.clone();
        let identity = self.identity.clone();
        let token = self.token.clone();
        let session_handle = session.handle();
        tokio::spawn(async move {
            if terminal(writer, input, size, game, identity, token.clone())
                .await
                .is_err()
            {
                // Pending data also blocks russh's normal disconnect queue.
                // Cancellation shuts the owned TCP socket down directly.
                token.cancel();
                return;
            }
            let _ = session_handle.exit_status_request(channel, 0).await;
            let _ = session_handle.eof(channel).await;
            let _ = session_handle.close(channel).await;
            // Let the channel exit status reach OpenSSH before transport close.
            // The handler cancels the connection after its close acknowledgement.
        });
        Ok(())
    }
    async fn data(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<()> {
        let Some(current) = self
            .channel
            .as_mut()
            .filter(|c| c.id == channel && c.started)
        else {
            session.close(channel)?;
            return Ok(());
        };
        if current.window.elapsed() > Duration::from_secs(1) {
            current.window = Instant::now();
            current.bytes = 0;
        }
        current.bytes += data.len();
        if current.bytes > 65_536
            || data.len() > 32_768
            || current.input.try_send(data.to_vec()).is_err()
        {
            self.token.cancel();
            session.close(channel)?;
        }
        Ok(())
    }
    async fn channel_close(&mut self, _channel: ChannelId, _session: &mut Session) -> Result<()> {
        self.token.cancel();
        Ok(())
    }
    async fn channel_eof(&mut self, _channel: ChannelId, _session: &mut Session) -> Result<()> {
        self.token.cancel();
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        session.channel_failure(channel)?;
        session.close(channel)?;
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<()> {
        session.channel_failure(channel)?;
        session.close(channel)?;
        Ok(())
    }
    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single: bool,
        _protocol: &str,
        _cookie: &str,
        _screen: u32,
        session: &mut Session,
    ) -> Result<()> {
        session.channel_failure(channel)?;
        Ok(())
    }
    async fn agent_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<bool> {
        session.channel_failure(channel)?;
        Ok(false)
    }
}

#[derive(Clone, Default)]
struct FrameBytes(Arc<Mutex<Vec<u8>>>);
impl Write for FrameBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut buffer = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("Terminal output failed"))?;
        if buffer.len() + bytes.len() > 2_000_000 {
            return Err(std::io::Error::other("Terminal output is too large"));
        }
        buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl FrameBytes {
    fn take(&self) -> Result<Vec<u8>> {
        let mut bytes = self
            .0
            .lock()
            .map_err(|_| anyhow!("Terminal output failed"))?;
        Ok(std::mem::take(&mut *bytes))
    }
}

/// ANSI output belongs to this SSH PTY. Geometry and cursor queries must never
/// reach the host console, which may be absent or used by the manager.
struct RemoteBackend {
    ansi: CrosstermBackend<FrameBytes>,
    size: Size,
    cursor: Position,
}
impl RemoteBackend {
    fn new(bytes: FrameBytes, width: u16, height: u16) -> Self {
        Self {
            ansi: CrosstermBackend::new(bytes),
            size: Size::new(width, height),
            cursor: Position::ORIGIN,
        }
    }
    fn resize(&mut self, width: u16, height: u16) {
        self.size = Size::new(width, height);
        self.cursor.x = self.cursor.x.min(width.saturating_sub(1));
        self.cursor.y = self.cursor.y.min(height.saturating_sub(1));
    }
}
impl Backend for RemoteBackend {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let cursor = &mut self.cursor;
        let size = self.size;
        self.ansi.draw(content.inspect(|(x, y, cell)| {
            let width = unicode_width::UnicodeWidthStr::width(cell.symbol());
            *cursor = Position::new(
                x.saturating_add(width.min(u16::MAX as usize) as u16)
                    .min(size.width.saturating_sub(1)),
                (*y).min(size.height.saturating_sub(1)),
            );
        }))
    }
    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        self.ansi.append_lines(n)?;
        self.cursor = Position::new(
            0,
            self.cursor
                .y
                .saturating_add(n)
                .min(self.size.height.saturating_sub(1)),
        );
        Ok(())
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.ansi.hide_cursor()
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.ansi.show_cursor()
    }
    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        Ok(self.cursor)
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        let position = position.into();
        self.cursor = Position::new(
            position.x.min(self.size.width.saturating_sub(1)),
            position.y.min(self.size.height.saturating_sub(1)),
        );
        self.ansi.set_cursor_position(self.cursor)
    }
    fn clear(&mut self) -> std::io::Result<()> {
        self.ansi.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> std::io::Result<()> {
        self.ansi.clear_region(clear_type)
    }
    fn size(&self) -> std::io::Result<Size> {
        Ok(self.size)
    }
    fn window_size(&mut self) -> std::io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::ZERO,
        })
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Backend::flush(&mut self.ansi)
    }
}

struct PlayerGuard {
    actor: u16,
    session: u64,
    game: GameHandle,
}
impl Drop for PlayerGuard {
    fn drop(&mut self) {
        if let Err(tokio::sync::mpsc::error::TrySendError::Full(request)) =
            self.game.sender.try_send(RuntimeRequest::Disconnect {
                actor_id: self.actor,
                session_id: self.session,
            })
        {
            let sender = self.game.sender.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = sender.send(request).await;
                });
            }
        }
    }
}
/// Normalize bare LF without damaging UTF-8 bytes or existing CRLF.
pub fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut previous = 0;
    for byte in bytes {
        if *byte == b'\n' && previous != b'\r' {
            output.push(b'\r');
        }
        output.push(*byte);
        previous = *byte;
    }
    output
}
async fn send(writer: &ChannelWriteHalf<server::Msg>, bytes: Vec<u8>) -> Result<()> {
    if !bytes.is_empty() {
        timeout(Duration::from_secs(3), writer.data_bytes(crlf(&bytes)))
            .await
            .context("Terminal output is too slow")??;
    }
    Ok(())
}

async fn terminal(
    writer: ChannelWriteHalf<server::Msg>,
    mut input: mpsc::Receiver<Vec<u8>>,
    mut size: watch::Receiver<Option<PtyInfo>>,
    game: GameHandle,
    identity: Option<String>,
    token: CancellationToken,
) -> Result<()> {
    let info = size
        .borrow()
        .clone()
        .ok_or_else(|| anyhow!("The game needs a PTY."))?;
    let bytes = FrameBytes::default();
    let mut terminal = Terminal::with_options(
        RemoteBackend::new(bytes.clone(), info.width, info.height),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, info.width, info.height)),
        },
    )?;
    send(
        &writer,
        b"\x1b[?1049h\x1b[?2004h\x1b[?25l\x1b[2J\x1b[H".to_vec(),
    )
    .await?;
    let mut parser = InputParser::new();
    let mut buffered = 0usize;
    let mut isolated_escape = None::<Instant>;
    let mut dirty = true;
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut draft = Draft::default();
    let mut join_error = None::<String>;
    let mut player = None::<PlayerConnection>;
    let mut guard = None::<PlayerGuard>;
    let mut ui = PlayerUi::new(supports_braille(&info.term, info.locale.as_deref()));
    let mut request = None::<JoinHandle<(Submission, Result<serde_json::Value>)>>;
    let mut render_error = None;
    'running: loop {
        tokio::select! {
            _=token.cancelled()=>break,
            changed=size.changed()=>{
                if changed.is_err(){break;}
                if let Some(info)=size.borrow_and_update().clone(){terminal.backend_mut().resize(info.width,info.height);terminal.resize(Rect::new(0,0,info.width,info.height))?;ui.utf8=supports_braille(&info.term,info.locale.as_deref());dirty=true;}
            }
            changed=async{match player.as_mut(){Some(player)=>player.views.changed().await,None=>std::future::pending().await}}=>{
                if changed.is_err(){break;}
                let view=player.as_mut().unwrap().views.borrow_and_update().clone();
                if view["closed"].as_bool()==Some(true){break;}
                ui.set_view(view);dirty=true;
            }
            finished=async{match request.as_mut(){Some(request)=>request.await,None=>std::future::pending().await}}=>{
                request=None;
                match finished{Ok((submission,result))=>ui.finish(&submission,result),Err(error)=>{ui.sending=false;ui.write(&error.to_string(),ui::BLOOD);}}
                dirty=true;
            }
            received=input.recv()=>{
                let Some(received)=received else{break;};buffered+=received.len();
                isolated_escape=if received==b"\x1b"{Some(Instant::now())}else{None};
                if buffered>16_384{render_error=Some(anyhow!("Paste or terminal input is too large."));break;}
                let events=parser.parse_as_vec(&received,true);
                if !events.is_empty(){buffered=0;}
                for event in events{
                    if player.is_none(){
                        match event{
                            InputEvent::Paste(text)=>{let plain=ui::clean(&text,false);if (draft.text.chars().count()+plain.chars().count())<=32{draft.insert(&plain);}},
                            InputEvent::Key(key)=>{
                                let ctrl=key.modifiers.contains(Modifiers::CTRL);
                                match key.key{
                                    KeyCode::Char('c') if ctrl=>break 'running,
                                    KeyCode::Char('u') if ctrl=>draft=Draft::default(),
                                    KeyCode::Backspace=>draft.delete(false),KeyCode::Delete=>draft.delete(true),
                                    KeyCode::LeftArrow=>draft.cursor=draft.cursor.saturating_sub(1),
                                    KeyCode::RightArrow=>draft.cursor=(draft.cursor+1).min(unicode_segmentation::UnicodeSegmentation::graphemes(draft.text.as_str(),true).count()),
                                    KeyCode::Home=>draft.boundary(false),KeyCode::End=>draft.boundary(true),
                                    KeyCode::Escape=>join_error=None,
                                    KeyCode::Enter=>{
                                        match timeout(Duration::from_secs(10),game.join(identity.clone(),draft.text.clone())).await{
                                            Ok(Ok(connection))=>{guard=Some(PlayerGuard{actor:connection.actor_id,session:connection.session_id,game:game.clone()});ui.set_view(connection.views.borrow().clone());player=Some(connection);},
                                            Ok(Err(error))=>join_error=Some(error.to_string()),
                                            Err(_)=>join_error=Some("The castle did not answer. Try again.".into()),
                                        }
                                    }
                                    KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(Modifiers::ALT) && draft.text.chars().count()<32=>{draft.insert(&ch.to_string());},
                                    _=>{}
                                }
                            }
                            _=>{}
                        }
                    }else if let Some(player)=player.as_ref() && let Some(submission)=ui.input(event){
                        match &submission.operation{
                            Operation::Quit=>break 'running,
                            Operation::Request{method,params}=>{
                                let actor=player.actor_id;let session=player.session_id;let game=player.handle.clone();let method=method.clone();let params=params.clone();
                                request=Some(tokio::spawn(async move{
                                    let (reply,answer)=tokio::sync::oneshot::channel();
                                    let result=async{
                                        game.sender.send(RuntimeRequest::Player{actor_id:actor,session_id:session,method,params,reply}).await.map_err(|_|anyhow!("Server stopped"))?;
                                        timeout(Duration::from_secs(10),answer).await.context("The castle did not answer")?.map_err(|_|anyhow!("Server stopped"))?
                                    }.await;
                                    (submission,result)
                                }));
                            }
                            _=>{}
                        }
                    }
                }
                dirty=true;
            }
            _=interval.tick()=>{
                // Flush only an isolated Escape. Incomplete UTF-8 and bracketed
                // paste remain intact even when packets arrive slowly.
                if isolated_escape.is_some_and(|start|start.elapsed()>=Duration::from_millis(50)){
                    isolated_escape=None;
                    for event in parser.parse_as_vec(&[],false){
                        if player.is_some(){let _=ui.input(event);}else{join_error=None;}
                        dirty=true;
                    }
                }
                if dirty{
                    let language={let manager=game.manager.borrow();if manager["language"].as_str()==Some("pl"){"pl"}else{"en"}};
                    terminal.draw(|frame|{if player.is_some(){ui.render(frame);}else{ui::render_join(frame,&draft,language,join_error.as_deref());}})?;
                    let output=bytes.take()?;
                    if let Err(error)=send(&writer,output).await{render_error=Some(error);break;}
                    dirty=false;
                }
            }
        }
    }
    if let Some(request) = request {
        request.abort();
    }
    drop(guard);
    let _ = send(
        &writer,
        b"\x1b[0m\x1b[?2004l\x1b[?25h\x1b[?1049l\r\n".to_vec(),
    )
    .await;
    if let Some(error) = render_error {
        Err(error)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_geometry_cursor_and_resize_are_owned_by_each_pty() {
        let bytes = FrameBytes::default();
        let backend = RemoteBackend::new(bytes.clone(), 100, 32);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 100, 32)),
            },
        )
        .unwrap();
        let other = RemoteBackend::new(FrameBytes::default(), 60, 18);
        terminal.set_cursor_position((80, 24)).unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            Position::new(80, 24)
        );
        for (width, height) in [(40, 14), (120, 36), (20, 8), (80, 24)] {
            terminal.backend_mut().resize(width, height);
            terminal.resize(Rect::new(0, 0, width, height)).unwrap();
            assert_eq!(terminal.size().unwrap(), Size::new(width, height));
            assert_eq!(
                terminal.backend_mut().window_size().unwrap().columns_rows,
                Size::new(width, height)
            );
            assert_eq!(other.size().unwrap(), Size::new(60, 18));
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new("Żaba 🕯️")
                            .style(ratatui::style::Style::default().fg(ui::PALE)),
                        frame.area(),
                    );
                    frame.set_cursor_position((width - 1, height - 1));
                })
                .unwrap();
            assert_eq!(
                terminal.get_cursor_position().unwrap(),
                Position::new(width - 1, height - 1)
            );
            terminal.clear().unwrap();
            assert_eq!(
                terminal.get_cursor_position().unwrap(),
                Position::new(width - 1, height - 1)
            );
        }
        let output = bytes.take().unwrap();
        assert!(
            !output.windows(4).any(|part| part == b"\x1b[6n"),
            "Remote rendering emitted a host cursor query"
        );
        let plain = ui::clean(&String::from_utf8(output).unwrap(), false);
        assert!(
            plain.contains("Żaba 🕯️"),
            "Missing rendered Unicode: {plain:?}"
        );
    }
}
