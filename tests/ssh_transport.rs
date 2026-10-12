use anyhow::{Result, anyhow};
use below_the_crown::{
    runtime::{GameHandle, HostInfo, PlayerConnection, RuntimeRequest},
    ssh::{self, SshOptions},
};
use russh::{ChannelMsg, ChannelWriteHalf, Disconnect, client};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, watch},
    task::JoinHandle,
};

fn public_view(id: u16, name: &str) -> Value {
    json!({
        "language":"en","branch":"test-castle","roomVisitId":format!("visit-{id}"),"clock":"Day 1 · 18:00","paused":[],
        "room":{"id":1,"name":"Dungeon","description":"Damp stone."},
        "self":{"id":id,"name":name,"role":"Prisoner","health":100,"hunger":35,"thirst":25,"fatigue":20,"activity":"watching","coins":8},
        "actors":[{"id":102,"name":"Edric","role":"Dungeon guard","condition":"apparently well","activity":"watching"}],
        "events":[],"actions":[],"listener":{"enabled":false},"combat":null,
        "inventory":[],"doors":[],"journal":[],"map":[]
    })
}
#[derive(Default)]
struct Records {
    requests: Vec<(u16, String, Value)>,
    connected: Vec<u16>,
    disconnects: Vec<u16>,
}
struct Fixture {
    directory: TempDir,
    server: ssh::SshServer,
    records: Arc<Mutex<Records>>,
    task: JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_keys(None).await
    }
    async fn with_keys(keys: Option<&str>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let authorized_keys = keys.map(|keys| {
            let path = directory.path().join("authorized_keys");
            std::fs::write(&path, keys).unwrap();
            path
        });
        let (sender, mut receiver) = mpsc::channel(64);
        let (manager_tx, manager) = watch::channel(json!({"language":"en"}));
        let game = GameHandle {
            sender,
            manager,
            info: Arc::new(HostInfo {
                data_dir: directory.path().into(),
                address: "127.0.0.1".into(),
                port: 0,
                maximum_players: 4,
            }),
        };
        let game_copy = game.clone();
        let records = Arc::new(Mutex::new(Records::default()));
        let records_copy = records.clone();
        let task = tokio::spawn(async move {
            let _manager = manager_tx;
            let mut actors: BTreeMap<u16, (String, watch::Sender<Value>)> = BTreeMap::new();
            let mut next = 1028u16;
            let mut event_id = 0u64;
            let mut session_id = 0u64;
            while let Some(request) = receiver.recv().await {
                match request {
                    RuntimeRequest::Join {
                        identity: _,
                        name,
                        reply,
                    } => {
                        let name = name.trim().to_owned();
                        if name.is_empty() {
                            let _ = reply.send(Err(anyhow!("Choose your name.")));
                            continue;
                        }
                        let old = actors
                            .iter()
                            .find(|(_, value)| value.0.eq_ignore_ascii_case(&name))
                            .map(|(id, _)| *id);
                        let id = old.unwrap_or_else(|| {
                            let id = next;
                            next += 1;
                            id
                        });
                        if records_copy.lock().unwrap().connected.contains(&id) {
                            let _ = reply.send(Err(anyhow!("That player is already connected.")));
                            continue;
                        }
                        let (views_tx, views) = watch::channel(public_view(id, &name));
                        session_id += 1;
                        actors.insert(id, (name, views_tx));
                        records_copy.lock().unwrap().connected.push(id);
                        if reply
                            .send(Ok(PlayerConnection {
                                actor_id: id,
                                session_id,
                                views,
                                handle: game_copy.clone(),
                            }))
                            .is_err()
                        {
                            records_copy
                                .lock()
                                .unwrap()
                                .connected
                                .retain(|actor| *actor != id);
                        }
                    }
                    RuntimeRequest::Player {
                        actor_id,
                        session_id: _,
                        method,
                        params,
                        reply,
                    } => {
                        records_copy.lock().unwrap().requests.push((
                            actor_id,
                            method.clone(),
                            params.clone(),
                        ));
                        if method == "listener"
                            && let Some((_, view)) = actors.get(&actor_id)
                        {
                            let mut next = view.borrow().clone();
                            next["listener"]["enabled"] = params["enabled"].clone();
                            view.send_replace(next);
                        }
                        let speech = if method == "input" {
                            params["text"].as_str()
                        } else if method == "act" && params["intent"]["kind"] == "speak" {
                            params["intent"]["text"].as_str()
                        } else {
                            None
                        };
                        if let Some(text) = speech {
                            event_id += 1;
                            let name = &actors[&actor_id].0;
                            let volume = below_the_crown::model::player_speech_volume(text);
                            let event = json!({"id":event_id,"kind":"speech","actorId":actor_id,"text":format!("{name}: “{text}”"),"details":{"type":"speech","speakerName":name,"recipientId":null,"recipientName":null,"recipientKnown":true,"volume":volume}});
                            for (_, view) in actors.values() {
                                let mut next = view.borrow().clone();
                                next["events"].as_array_mut().unwrap().push(event.clone());
                                view.send_replace(next);
                            }
                        }
                        let _ = reply.send(Ok(json!({"ok":true})));
                    }
                    RuntimeRequest::Disconnect {
                        actor_id,
                        session_id: _,
                    } => {
                        let mut records = records_copy.lock().unwrap();
                        records.connected.retain(|id| *id != actor_id);
                        records.disconnects.push(actor_id);
                    }
                    RuntimeRequest::Manager { reply, .. } => {
                        let _ = reply.send(Ok(json!({"ok":true})));
                    }
                }
            }
        });
        let server = ssh::serve(
            game,
            SshOptions {
                bind: Ipv4Addr::LOCALHOST,
                port: 0,
                subnet: "127.0.0.0/8".parse().unwrap(),
                maximum_players: 4,
                authorized_keys,
            },
        )
        .await
        .unwrap();
        Self {
            directory,
            server,
            records,
            task,
        }
    }
    async fn close(mut self) {
        self.server.close().await;
        self.task.abort();
    }
}
struct Client;
impl client::Handler for Client {
    type Error = anyhow::Error;
    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool> {
        Ok(true)
    }
}
struct Remote {
    handle: client::Handle<Client>,
    writer: ChannelWriteHalf<client::Msg>,
    output: Arc<Mutex<Vec<u8>>>,
    closed: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}
impl Remote {
    async fn connect(port: u16, term: &str, width: u32, height: u32) -> Self {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(10)),
            ..client::Config::default()
        });
        let mut handle = client::connect(config, (Ipv4Addr::LOCALHOST, port), Client)
            .await
            .unwrap();
        assert!(
            handle
                .authenticate_none("same-local-username")
                .await
                .unwrap()
                .success()
        );
        let channel = handle.channel_open_session().await.unwrap();
        channel
            .request_pty(true, term, width, height, 0, 0, &[])
            .await
            .unwrap();
        channel.request_shell(true).await.unwrap();
        let (mut read, writer) = channel.split();
        let output = Arc::new(Mutex::new(Vec::new()));
        let output_copy = output.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_copy = closed.clone();
        let reader = tokio::spawn(async move {
            while let Some(message) = read.wait().await {
                match message {
                    ChannelMsg::Data { data } => {
                        output_copy.lock().unwrap().extend_from_slice(&data)
                    }
                    ChannelMsg::Close | ChannelMsg::Eof => break,
                    _ => {}
                }
            }
            closed_copy.store(true, Ordering::Relaxed);
        });
        Self {
            handle,
            writer,
            output,
            closed,
            reader,
        }
    }
    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }
    fn plain(&self) -> String {
        below_the_crown::ui::clean(&self.raw(), false)
    }
    async fn send(&self, bytes: &[u8]) {
        self.writer.data_bytes(bytes.to_vec()).await.unwrap();
    }
    async fn close(self) {
        let _ = self
            .handle
            .disconnect(Disconnect::ByApplication, "", "")
            .await;
        self.reader.abort();
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(6), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Timed out waiting for game SSH");
}

#[test]
fn lan_admission_boundaries_and_terminal_capabilities() {
    let options = SshOptions {
        bind: "192.168.7.20".parse().unwrap(),
        port: 2222,
        subnet: "192.168.7.0/24".parse().unwrap(),
        maximum_players: 8,
        authorized_keys: None,
    };
    assert!(options.validate().is_ok());
    for inside in ["192.168.7.1", "192.168.7.254", "127.0.0.1"] {
        assert!(options.permits(inside.parse().unwrap()));
    }
    for outside in ["192.168.6.255", "192.168.8.0", "10.0.0.1", "::1"] {
        assert!(!options.permits(outside.parse().unwrap()));
    }
    for (bind, subnet) in [
        ("0.0.0.0", "0.0.0.0/0"),
        ("8.8.8.8", "8.8.8.0/24"),
        ("192.168.7.20", "192.168.8.0/24"),
        ("192.168.7.20", "0.0.0.0/0"),
    ] {
        let mut invalid = options.clone();
        invalid.bind = bind.parse().unwrap();
        invalid.subnet = subnet.parse().unwrap();
        assert!(invalid.validate().is_err());
    }
    assert!(ssh::supports_braille("xterm-256color", Some("pl_PL.UTF-8")));
    assert!(!ssh::supports_braille("dumb", Some("C.UTF-8")));
    assert!(!ssh::supports_braille("xterm", Some("C")));
    assert_eq!(
        ssh::crlf("Żaba 🕯️\nAlready\r\nEnd".as_bytes()),
        "Żaba 🕯️\r\nAlready\r\nEnd".as_bytes()
    );
}

#[tokio::test]
async fn guest_terminals_chat_resize_and_disconnect_independently() {
    let fixture = Fixture::new().await;
    assert_eq!(
        std::fs::metadata(fixture.directory.path().join("ssh/host_ed25519"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let alice = Remote::connect(fixture.server.port, "xterm-256color", 100, 32).await;
    until(|| alice.plain().contains("Your name")).await;
    alice.send("Alice Żaba\r".as_bytes()).await;
    until(|| alice.plain().contains("Dungeon")).await;
    let bob = Remote::connect(fixture.server.port, "xterm-256color", 60, 18).await;
    until(|| bob.plain().contains("Your name")).await;
    bob.send(b"Bob\r").await;
    until(|| bob.plain().contains("Dungeon")).await;
    alice.send("Good evening, Bob. 🕯️\r".as_bytes()).await;
    until(|| bob.plain().contains("Good evening, Bob. 🕯️")).await;
    assert!(alice.raw().contains("\x1b[?1049h"));
    assert!(alice.raw().contains("38;2;"));
    assert!(
        !alice
            .raw()
            .as_bytes()
            .windows(2)
            .any(|pair| pair[1] == b'\n' && pair[0] != b'\r')
    );
    alice.writer.window_change(40, 14, 0, 0).await.unwrap();
    alice
        .send(b"\x1b[200~A pasted message\nwith two lines.\x1b[201~")
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !fixture
            .records
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(_, method, params)| method == "input"
                && params["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("A pasted message"))
    );
    alice.send(b"\r").await;
    until(|| {
        fixture
            .records
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(_, _, params)| {
                params["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("with two lines.")
            })
    })
    .await;
    alice.send(b"/auto on\r").await;
    until(|| {
        fixture
            .records
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(id, method, params)| {
                *id == 1028 && method == "listener" && params["enabled"] == true
            })
    })
    .await;
    assert!(
        !fixture
            .records
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(id, method, _)| *id == 1029 && method == "listener")
    );
    alice.send(b"\x03").await;
    until(|| fixture.records.lock().unwrap().disconnects.contains(&1028)).await;
    assert!(fixture.records.lock().unwrap().connected.contains(&1029));
    bob.send(b"Missed words.\r").await;
    until(|| bob.plain().contains("Missed words.")).await;
    let resumed = Remote::connect(fixture.server.port, "dumb", 80, 24).await;
    until(|| resumed.plain().contains("Your name")).await;
    resumed.send("ALICE ŻABA\r".as_bytes()).await;
    until(|| resumed.plain().contains("Dungeon")).await;
    assert!(!resumed.plain().contains("Missed words."));
    assert!(
        !resumed
            .raw()
            .chars()
            .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
    );
    resumed.send(b"\x03").await;
    bob.send(b"\x03").await;
    until(|| fixture.records.lock().unwrap().connected.is_empty()).await;
    resumed.close().await;
    alice.close().await;
    bob.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn exec_subsystems_and_forwarding_are_rejected() {
    let fixture = Fixture::new().await;
    let mut handle = client::connect(
        Arc::new(client::Config::default()),
        (Ipv4Addr::LOCALHOST, fixture.server.port),
        Client,
    )
    .await
    .unwrap();
    assert!(handle.authenticate_none("guest").await.unwrap().success());
    let mut channel = handle.channel_open_session().await.unwrap();
    channel.exec(true, "cat /etc/passwd").await.unwrap();
    let rejected = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(message) = channel.wait().await {
            if matches!(message, ChannelMsg::Failure) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(rejected);
    assert!(
        handle
            .channel_open_direct_tcpip("127.0.0.1", 22, "127.0.0.1", 12345)
            .await
            .is_err()
    );
    assert!(handle.tcpip_forward("127.0.0.1", 8080).await.is_err());
    handle
        .disconnect(Disconnect::ByApplication, "", "")
        .await
        .unwrap();
    let mut handle = client::connect(
        Arc::new(client::Config::default()),
        (Ipv4Addr::LOCALHOST, fixture.server.port),
        Client,
    )
    .await
    .unwrap();
    handle.authenticate_none("guest").await.unwrap();
    let mut channel = handle.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    let rejected = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(message) = channel.wait().await {
            if matches!(message, ChannelMsg::Failure) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(rejected);
    assert!(fixture.records.lock().unwrap().connected.is_empty());
    let _ = handle.disconnect(Disconnect::ByApplication, "", "").await;
    fixture.close().await;
}

#[tokio::test]
async fn ordinary_openssh_joins_without_keys_or_guest_installation() {
    if tokio::process::Command::new("ssh")
        .arg("-V")
        .output()
        .await
        .is_err()
    {
        return;
    }
    let fixture = Fixture::new().await;
    let known = fixture.directory.path().join("openssh_known_hosts");
    let mut child = tokio::process::Command::new("ssh")
        .args([
            "-tt",
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentityAgent=none",
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
        ])
        .arg(format!("UserKnownHostsFile={}", known.display()))
        .arg("-p")
        .arg(fixture.server.port.to_string())
        .arg("127.0.0.1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let output = Arc::new(Mutex::new(String::new()));
    let output_copy = output.clone();
    let reader = tokio::spawn(async move {
        let mut buffer = [0u8; 8192];
        while let Ok(size) = stdout.read(&mut buffer).await {
            if size == 0 {
                break;
            }
            output_copy
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buffer[..size]));
        }
    });
    until(|| output.lock().unwrap().contains("Your name")).await;
    stdin.write_all(b"Rook\r").await.unwrap();
    until(|| output.lock().unwrap().contains("Dungeon")).await;
    assert!(!output.lock().unwrap().to_lowercase().contains("your story"));
    stdin.write_all(b"HELP!!\r").await.unwrap();
    until(|| {
        fixture
            .records
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(_, method, params)| method == "input" && params["text"] == "HELP!!")
    })
    .await;
    until(|| output.lock().unwrap().contains("You: “HELP!!”")).await;
    assert!(!output.lock().unwrap().contains("/shout"));
    stdin.write_all(b"\x03").await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(6), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    until(|| fixture.records.lock().unwrap().connected.is_empty()).await;
    reader.await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn host_close_disconnects_registered_players() {
    let mut fixture = Fixture::new().await;
    let player = Remote::connect(fixture.server.port, "xterm", 80, 24).await;
    until(|| player.plain().contains("Your name")).await;
    player.send(b"Wren\r").await;
    until(|| fixture.records.lock().unwrap().connected.len() == 1).await;
    fixture.server.close().await;
    until(|| fixture.records.lock().unwrap().connected.is_empty()).await;
    until(|| player.closed.load(Ordering::Relaxed)).await;
    player.close().await;
    fixture.close().await;
}

fn fixture_key(seed: u8) -> russh::keys::PrivateKey {
    let pair = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]);
    russh::keys::PrivateKey::new(
        russh::keys::ssh_key::private::KeypairData::Ed25519(pair),
        "Fixture key",
    )
    .unwrap()
}
#[tokio::test]
async fn optional_public_keys_require_a_verified_signature() {
    let approved = fixture_key(42);
    let unauthorized = fixture_key(43);
    let fixture = Fixture::with_keys(Some(&approved.public_key().to_openssh().unwrap())).await;
    let mut handle = client::connect(
        Arc::new(client::Config::default()),
        (Ipv4Addr::LOCALHOST, fixture.server.port),
        Client,
    )
    .await
    .unwrap();
    assert!(!handle.authenticate_none("guest").await.unwrap().success());
    assert!(
        !handle
            .authenticate_publickey(
                "guest",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(unauthorized), None)
            )
            .await
            .unwrap()
            .success()
    );
    assert!(
        handle
            .authenticate_publickey(
                "any-username",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(approved), None)
            )
            .await
            .unwrap()
            .success()
    );
    let mut channel = handle.channel_open_session().await.unwrap();
    channel.request_shell(true).await.unwrap();
    let rejected = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(message) = channel.wait().await {
            if matches!(message, ChannelMsg::Failure) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(rejected, "Game shells require a PTY");
    let _ = handle.disconnect(Disconnect::ByApplication, "", "").await;
    fixture.close().await;
}

#[tokio::test]
async fn fragmented_polish_utf8_waits_for_remaining_bytes() {
    let fixture = Fixture::new().await;
    let guest = Remote::connect(fixture.server.port, "xterm", 80, 24).await;
    until(|| guest.plain().contains("Your name")).await;
    guest.send(b"Wren\r").await;
    until(|| guest.plain().contains("Dungeon")).await;
    guest.send(b"Za\xc5").await;
    tokio::time::sleep(Duration::from_millis(170)).await;
    guest.send(b"\xbba\r").await;
    until(|| {
        fixture
            .records
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(_, method, params)| method == "input" && params["text"] == "ZaŻa")
    })
    .await;
    guest.send(b"\x03").await;
    until(|| fixture.records.lock().unwrap().connected.is_empty()).await;
    guest.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn unfinished_oversized_paste_disconnects_without_submitting() {
    let fixture = Fixture::new().await;
    let guest = Remote::connect(fixture.server.port, "xterm", 80, 24).await;
    until(|| guest.plain().contains("Your name")).await;
    guest.send(b"Wren\r").await;
    until(|| guest.plain().contains("Dungeon")).await;
    let mut paste = b"\x1b[200~".to_vec();
    paste.extend(std::iter::repeat_n(b'x', 16_385));
    guest.send(&paste).await;
    until(|| fixture.records.lock().unwrap().connected.is_empty()).await;
    assert!(fixture.records.lock().unwrap().requests.is_empty());
    guest.close().await;
    fixture.close().await;
}

struct StalledClient {
    blocked: tokio_util::sync::CancellationToken,
    release: Arc<tokio::sync::Notify>,
}
impl client::Handler for StalledClient {
    type Error = anyhow::Error;
    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool> {
        Ok(true)
    }
    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        _bytes: &[u8],
        _session: &mut client::Session,
    ) -> Result<()> {
        self.blocked.cancel();
        self.release.notified().await;
        Ok(())
    }
}
#[tokio::test]
async fn stalled_output_closes_before_more_frames_can_accumulate() {
    let fixture = Fixture::new().await;
    let socket = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, fixture.server.port))
        .await
        .unwrap()
        .into_std()
        .unwrap();
    let mut probe = tokio::net::TcpStream::from_std(socket.try_clone().unwrap()).unwrap();
    let socket = tokio::net::TcpStream::from_std(socket).unwrap();
    let blocked = tokio_util::sync::CancellationToken::new();
    let release = Arc::new(tokio::sync::Notify::new());
    let mut handle = client::connect_stream(
        Arc::new(client::Config {
            window_size: 64,
            ..client::Config::default()
        }),
        socket,
        StalledClient {
            blocked: blocked.clone(),
            release: release.clone(),
        },
    )
    .await
    .unwrap();
    handle.authenticate_none("guest").await.unwrap();
    let channel = handle.channel_open_session().await.unwrap();
    channel
        .request_pty(true, "xterm", 400, 160, 0, 0, &[])
        .await
        .unwrap();
    channel.request_shell(true).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), blocked.cancelled())
        .await
        .unwrap();
    // The client protocol is deliberately frozen after the first data packet.
    // Read its duplicated TCP descriptor to observe the server's actual close.
    let mut pending = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(8), probe.read_to_end(&mut pending)).await;
    assert!(
        ended.is_ok(),
        "A blocked window must not accumulate repaint output indefinitely"
    );
    assert!(fixture.records.lock().unwrap().connected.is_empty());
    release.notify_waiters();
    let _ = handle.disconnect(Disconnect::ByApplication, "", "").await;
    fixture.close().await;
}

#[tokio::test]
async fn ssh_private_files_refuse_symlinks_without_touching_the_target() {
    for destination in ["ssh", "ssh/known_hosts", "ssh/host_ed25519"] {
        let directory = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let original = target.path().join("private-fixture");
        std::fs::write(&original, b"unchanged test-owned bytes").unwrap();
        let linked = directory.path().join(destination);
        if destination == "ssh" {
            std::os::unix::fs::symlink(target.path(), &linked).unwrap();
        } else {
            std::fs::create_dir(directory.path().join("ssh")).unwrap();
            std::os::unix::fs::symlink(&original, &linked).unwrap();
        }
        let (sender, _receiver) = mpsc::channel(4);
        let (_manager, manager) = watch::channel(json!({"language":"en"}));
        let handle = GameHandle {
            sender,
            manager,
            info: Arc::new(HostInfo {
                data_dir: directory.path().into(),
                address: "127.0.0.1".into(),
                port: 0,
                maximum_players: 1,
            }),
        };
        assert!(
            ssh::serve(
                handle,
                SshOptions {
                    bind: Ipv4Addr::LOCALHOST,
                    port: 0,
                    subnet: "127.0.0.0/8".parse().unwrap(),
                    maximum_players: 1,
                    authorized_keys: None,
                }
            )
            .await
            .is_err(),
            "Accepted symlink at {destination}"
        );
        assert_eq!(
            std::fs::read(original).unwrap(),
            b"unchanged test-owned bytes"
        );
        assert!(!target.path().join("host_ed25519").exists());
        assert!(!target.path().join("known_hosts").exists());
    }
}

#[test]
fn headless_host_resizes_remote_sessions_without_terminal_environment() {
    // A separate process avoids changing environment variables while other tests
    // run. Reuse the complete multi-client scenario, keeping all its assertions.
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "guest_terminals_chat_resize_and_disconnect_independently",
            "--nocapture",
        ])
        .env_remove("TERM")
        .env_remove("COLORTERM")
        .env("RUST_BACKTRACE", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "Headless resize failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
