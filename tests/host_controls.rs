use anyhow::{Result, anyhow};
use below_the_crown::{
    codex::{DecisionDriver, ThinkRequest, ThinkResult},
    engine::Engine,
    manager::{self, IpcServer, MenuUi, Page},
    model::CLOCK_RATE,
    paths,
    runtime::{GameHandle, HostInfo, RuntimeRequest},
    simulation::Simulation,
    store::Store,
};
use futures::future::BoxFuture;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::oneshot,
    task::JoinHandle,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

/// Host-control tests never request a paid model or introduce scripted NPC behavior.
struct FixtureDriver;
impl DecisionDriver for FixtureDriver {
    fn think(
        &self,
        _request: ThinkRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ThinkResult>> {
        Box::pin(async move {
            cancel.cancelled().await;
            Err(anyhow!("Fixture turn cancelled"))
        })
    }
    fn reset(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

struct Host {
    directory: TempDir,
    ipc: IpcServer,
    handle: GameHandle,
    task: Option<JoinHandle<Result<()>>>,
}
impl Host {
    async fn new(maximum_players: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path(), "castle", "en", 73).unwrap();
        engine.set_pause(true).unwrap();
        let (mut simulation, handle) = Simulation::with_driver(
            engine,
            HostInfo {
                data_dir: directory.path().into(),
                address: "127.0.0.1".into(),
                port: 2222,
                maximum_players,
            },
            Arc::new(FixtureDriver),
        );
        // Isolate host contracts from every tunable NPC cadence and model completion.
        simulation.scheduling.maximum_calls = 0;
        simulation.scheduling.background_interval = f64::MAX;
        let mut ipc = IpcServer::bind(directory.path()).await.unwrap();
        ipc.serve(handle.clone());
        let task = Some(tokio::spawn(simulation.run()));
        Self {
            directory,
            ipc,
            handle,
            task,
        }
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        timeout(
            Duration::from_secs(4),
            manager::request(self.directory.path(), method, params),
        )
        .await
        .map_err(|_| anyhow!("Test manager request timed out"))?
    }
    async fn view(&self) -> Value {
        self.request("view", json!({})).await.unwrap()
    }
    async fn export(&self) -> Value {
        self.request("export", json!({})).await.unwrap()
    }
    async fn raw(&self, bytes: &[u8]) -> Vec<u8> {
        let mut stream = UnixStream::connect(&self.ipc.path).await.unwrap();
        let _ = stream.write_all(bytes).await;
        let _ = stream.shutdown().await;
        let mut output = Vec::new();
        // Closing a rejected frame may reset unread bytes instead of returning EOF.
        let _ = timeout(Duration::from_secs(3), stream.read_to_end(&mut output))
            .await
            .unwrap();
        output
    }
    async fn finish(&mut self) {
        if let Some(task) = self.task.take() {
            self.request("stop", json!({})).await.unwrap();
            timeout(Duration::from_secs(4), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        self.ipc.close().await;
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
fn connected(view: &Value) -> Vec<u16> {
    view["actors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|actor| actor["connected"] == true)
        .map(|actor| actor["id"].as_u64().unwrap() as u16)
        .collect()
}

#[tokio::test]
async fn local_ipc_is_private_bounded_and_rejects_malformed_envelopes() {
    let mut host = Host::new(2).await;
    assert_eq!(
        fs::metadata(host.directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&host.ipc.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(host.ipc.path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let original = host.export().await;
    for raw in [
        b"[]\n".as_slice(),
        b"{\"method\":\"stop\",\"params\":{},\"identity\":\"host\"}\n",
        b"{\"method\":\"stop\",\"params\":[]}\n",
        b"{\"params\":{}}\n",
        b"not json\n",
        b"{\"method\":12,\"params\":{}}\n",
    ] {
        let answer: Value = serde_json::from_slice(&host.raw(raw).await).unwrap();
        assert!(answer["error"].is_string(), "{answer}");
        assert_eq!(host.export().await, original);
    }
    assert!(host.raw(b"{\"method\":\"stop\"").await.is_empty());
    let oversized = vec![b'x'; 65_537];
    assert!(host.raw(&oversized).await.is_empty());
    assert_eq!(host.export().await, original);
    assert!(
        host.request(
            "input",
            json!({"text":"manager cannot impersonate a player"})
        )
        .await
        .is_err()
    );
    host.finish().await;
    assert!(!host.ipc.path.exists());
}

#[tokio::test]
async fn malformed_manager_parameters_do_not_mutate_or_stop_the_host() {
    let mut host = Host::new(2).await;
    let original = host.export().await;
    let original_saves = host.view().await["saves"].clone();
    for (method, params) in [
        ("view", json!({"surprise":true})),
        ("pause", json!({"paused":false,"surprise":true})),
        ("observe", json!({"enabled":true,"surprise":true})),
        ("save", json!({"name":13})),
        ("save", json!({"name":"test","surprise":true})),
        ("settings", json!({"language":13})),
        ("settings", json!({"scenario":false})),
        ("settings", json!({"language":"de"})),
        ("settings", json!({"scenario":"missing-scenario"})),
        ("settings", json!({"reasoningEffort":"ultra"})),
        ("settings", json!({"reasoningEffort":null})),
        ("select", json!({"actorId":65_536})),
        ("load", json!({"name":"missing-save"})),
        ("new", json!({"seed":-1})),
        ("new", json!({"seed":u64::MAX})),
        ("new", json!({"seed":19,"surprise":true})),
        ("recover", json!({"surprise":true})),
        ("export", json!({"surprise":true})),
        ("stop", json!({"surprise":true})),
    ] {
        assert!(
            host.request(method, params.clone()).await.is_err(),
            "Accepted {method} {params}"
        );
        assert_eq!(
            host.export().await,
            original,
            "Rejected {method} changed world or journal"
        );
        assert_eq!(
            host.view().await["saves"],
            original_saves,
            "Rejected {method} wrote a save"
        );
    }
    host.finish().await;
}

#[tokio::test]
async fn pending_settings_apply_on_new_or_restore_and_failed_restore_preserves_sessions() {
    let mut host = Host::new(3).await;
    fs::write(
        host.directory.path().join("server-settings.json"),
        br#"{"bind":"192.168.5.9","subnet":"192.168.5.0/24","port":3456}"#,
    )
    .unwrap();
    let player = host.handle.join(None, "Ansel".into()).await.unwrap();
    let original = host.export().await;
    host.request("save", json!({"name":"first-night"}))
        .await
        .unwrap();
    host.request(
        "settings",
        json!({"language":"pl","scenario":"siege","reasoningEffort":"low"}),
    )
    .await
    .unwrap();
    let settings: Value = serde_json::from_slice(
        &fs::read(host.directory.path().join("server-settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["bind"], "192.168.5.9");
    assert_eq!(settings["port"], 3456);
    let view = host.view().await;
    assert_eq!(view["language"], "en");
    assert_eq!(view["scenarioId"], "castle");
    assert_eq!(view["pendingLanguage"], "pl");
    assert_eq!(view["pendingScenario"], "siege");
    assert_eq!(view["settings"]["reasoningEffort"], "low");
    assert_eq!(host.export().await, original);
    assert!(
        host.request("load", json!({"name":"missing-save"}))
            .await
            .is_err()
    );
    assert_eq!(host.export().await, original);
    assert_eq!(connected(&host.view().await), vec![player.actor_id]);
    assert_ne!(player.views.borrow()["closed"], true);
    host.request("new", json!({"seed":91})).await.unwrap();
    assert_eq!(player.views.borrow()["closed"], true);
    let view = host.view().await;
    assert_eq!(view["language"], "pl");
    assert_eq!(view["scenarioId"], "siege");
    assert_eq!(view["manualPause"], true);
    assert!(connected(&view).is_empty());
    assert_ne!(view["branch"], original["world"]["branch"]);
    host.request("load", json!({"name":"first-night"}))
        .await
        .unwrap();
    let view = host.view().await;
    assert_eq!(view["scenarioId"], "castle");
    assert_eq!(view["language"], "pl");
    assert_eq!(view["manualPause"], true);
    assert_eq!(view["effort"], "low");
    assert_eq!(
        host.export().await["world"]["players"],
        original["world"]["players"]
    );
    host.finish().await;
}

#[tokio::test]
async fn settings_write_failure_preserves_pending_and_current_world() {
    let mut host = Host::new(1).await;
    let original = host.export().await;
    let target = host.directory.path().join("fixture-original.json");
    fs::write(&target, b"{\"language\":\"en\",\"scenario\":\"castle\"}").unwrap();
    std::os::unix::fs::symlink(&target, host.directory.path().join("server-settings.json"))
        .unwrap();
    assert!(
        host.request(
            "settings",
            json!({"language":"pl","scenario":"siege","reasoningEffort":"max"})
        )
        .await
        .is_err()
    );
    assert_eq!(host.export().await, original);
    let view = host.view().await;
    assert_eq!(view["pendingLanguage"], "en");
    assert_eq!(view["pendingScenario"], "castle");
    assert_eq!(view["settings"]["reasoningEffort"], "high");
    assert_eq!(
        fs::read(target).unwrap(),
        b"{\"language\":\"en\",\"scenario\":\"castle\"}"
    );
    host.finish().await;
}

#[tokio::test]
async fn corrupt_save_metadata_and_rejected_restore_cannot_end_the_current_castle() {
    let mut host = Host::new(2).await;
    let player = host.handle.join(None, "Ansel".into()).await.unwrap();
    host.request("save", json!({"name":"healthy-save"}))
        .await
        .unwrap();
    let original = host.export().await;
    let store = Store::open(host.directory.path()).unwrap();
    for corrupt in ["not JSON", "{\"format\":4,\"world\":{}}"] {
        store.db.execute(
            "INSERT INTO saves(name,created,data) VALUES('corrupted-save','2030-01-01',?) ON CONFLICT(name) DO UPDATE SET data=excluded.data",
            [corrupt],
        ).unwrap();
        assert!(
            host.request("load", json!({"name":"corrupted-save"}))
                .await
                .is_err()
        );
        assert_eq!(host.export().await, original);
        let view = host.view().await;
        assert_eq!(connected(&view), vec![player.actor_id]);
        assert!(
            view["saves"]
                .as_array()
                .unwrap()
                .iter()
                .any(|save| save["name"] == "healthy-save")
        );
        assert!(
            view["saves"]
                .as_array()
                .unwrap()
                .iter()
                .any(|save| save["name"] == "corrupted-save" && save["valid"] == false)
        );
        assert_ne!(player.views.borrow()["closed"], true);
    }
    player
        .request("listener", json!({"enabled":false}))
        .await
        .unwrap();
    drop(store);
    host.finish().await;
}

#[tokio::test]
async fn replaced_connection_cannot_control_or_disconnect_a_reused_actor() {
    let mut host = Host::new(2).await;
    let old = host.handle.join(None, "Ansel".into()).await.unwrap();
    host.request("new", json!({"seed":31})).await.unwrap();
    assert_eq!(old.views.borrow()["closed"], true);
    let fresh = host.handle.join(None, "Boris".into()).await.unwrap();
    assert_eq!(
        fresh.actor_id, old.actor_id,
        "Fixture must exercise actual actor ID reuse"
    );
    assert!(
        old.request("listener", json!({"enabled":true}))
            .await
            .is_err()
    );
    assert!(old.request("cancel", json!({})).await.is_err());
    assert_eq!(fresh.views.borrow()["listener"]["enabled"], false);
    old.disconnect().await;
    assert_eq!(connected(&host.view().await), vec![fresh.actor_id]);
    fresh
        .request("listener", json!({"enabled":true}))
        .await
        .unwrap();
    assert_eq!(fresh.views.borrow()["listener"]["enabled"], true);
    // Session identity is renewed on a same-branch reconnect as well.
    fresh.disconnect().await;
    assert!(connected(&host.view().await).is_empty());
    let reconnect = host.handle.join(None, "Boris".into()).await.unwrap();
    assert_eq!(reconnect.actor_id, fresh.actor_id);
    assert!(
        fresh
            .request("listener", json!({"enabled":false}))
            .await
            .is_err()
    );
    fresh.disconnect().await;
    assert_eq!(connected(&host.view().await), vec![reconnect.actor_id]);
    host.finish().await;
}

#[tokio::test]
async fn abandoned_join_releases_capacity_and_players_cannot_use_host_controls() {
    let mut host = Host::new(1).await;
    let (reply, answer) = oneshot::channel();
    drop(answer);
    host.handle
        .sender
        .send(RuntimeRequest::Join {
            identity: None,
            name: "Ansel".into(),
            reply,
        })
        .await
        .unwrap();
    assert!(connected(&host.view().await).is_empty());
    let player = host.handle.join(None, "Ansel".into()).await.unwrap();
    assert_eq!(connected(&host.view().await), vec![player.actor_id]);
    assert!(host.handle.join(None, "Boris".into()).await.is_err());
    for method in [
        "save",
        "load",
        "new",
        "settings",
        "pause",
        "observe",
        "stop",
        "export",
        "diagnostics",
    ] {
        assert!(
            player.request(method, json!({})).await.is_err(),
            "Player got host control {method}"
        );
    }
    assert!(
        player
            .request("listener", json!({"enabled":true,"actorId":102}))
            .await
            .is_err()
    );
    player.disconnect().await;
    assert!(connected(&host.view().await).is_empty());
    assert!(
        player
            .request("listener", json!({"enabled":true}))
            .await
            .is_err()
    );
    host.finish().await;
}

#[tokio::test]
async fn stale_room_context_rejects_speech_and_action_before_any_mutation() {
    let mut host = Host::new(2).await;
    let player = host.handle.join(None, "Ansel".into()).await.unwrap();
    host.request("pause", json!({"paused":false}))
        .await
        .unwrap();
    let visit = player.views.borrow()["roomVisitId"].clone();
    let before = host.export().await;
    for (method, params) in [
        (
            "input",
            json!({"text":"Help!","context":{"roomVisitId":"obsolete-visit"}}),
        ),
        (
            "act",
            json!({"intent":{"kind":"speak","text":"Help!","volume":"shout","targetId":null,"plot":null},"context":{"roomVisitId":"obsolete-visit"}}),
        ),
        (
            "input",
            json!({"text":"Help!","context":{"roomVisitId":visit,"combat":{"encounterId":900,"round":1}}}),
        ),
    ] {
        assert!(player.request(method, params).await.is_err());
    }
    let after = host.export().await;
    assert_eq!(after["events"], before["events"]);
    assert_eq!(after["world"]["items"], before["world"]["items"]);
    let physical = |snapshot: &Value| {
        snapshot["world"]["actors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|actor| {
                json!([
                    actor["id"],
                    actor["roomId"],
                    actor["health"],
                    actor["equipment"],
                    actor["coins"]
                ])
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(physical(&after), physical(&before));
    assert_eq!(connected(&host.view().await), vec![player.actor_id]);
    assert_eq!(
        player
            .request(
                "input",
                json!({"text":"A valid room message","context":{"roomVisitId":visit}})
            )
            .await
            .unwrap()["ok"],
        true
    );
    host.finish().await;
}

#[tokio::test]
async fn closing_a_manager_endpoint_does_not_stop_the_simulation_and_stop_saves_guests() {
    let mut host = Host::new(2).await;
    let player = host.handle.join(None, "Ansel".into()).await.unwrap();
    let branch = host.view().await["branch"].clone();
    host.ipc.close().await;
    assert!(
        manager::request(host.directory.path(), "view", json!({}))
            .await
            .is_err()
    );
    assert_eq!(
        host.handle.request("view", json!({})).await.unwrap()["branch"],
        branch
    );
    player
        .request("listener", json!({"enabled":true}))
        .await
        .unwrap();
    host.ipc = IpcServer::bind(host.directory.path()).await.unwrap();
    host.ipc.serve(host.handle.clone());
    host.finish().await;
    assert_eq!(player.views.borrow()["closed"], true);
    assert!(
        player
            .request("listener", json!({"enabled":false}))
            .await
            .is_err()
    );
    let snapshot = Store::open(host.directory.path())
        .unwrap()
        .snapshot("autosave")
        .unwrap();
    assert_eq!(snapshot.world.branch, branch.as_str().unwrap());
    assert!(snapshot.world.manual_pause);
    assert!(
        snapshot
            .world
            .players
            .iter()
            .any(|p| p.actor_id == player.actor_id && p.listener)
    );
}

#[tokio::test]
async fn polling_the_remote_manager_cannot_slow_or_unpause_the_world_clock() {
    let mut host = Host::new(1).await;
    let before = host.view().await["time"].as_f64().unwrap();
    for _ in 0..3 {
        host.view().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(host.view().await["time"].as_f64().unwrap(), before);
    host.request("observe", json!({"enabled":true}))
        .await
        .unwrap();
    let started = Instant::now();
    let world_start = host.view().await["time"].as_f64().unwrap();
    for _ in 0..20 {
        host.view().await;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    host.request("pause", json!({"paused":true})).await.unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    let view = host.view().await;
    let game_elapsed = (view["time"].as_f64().unwrap() - world_start) / CLOCK_RATE;
    // This tolerance covers IPC scheduling, rather than fixing NPC or tick tuning.
    assert!(
        (game_elapsed - elapsed).abs() < 0.15,
        "Manager polls changed time: {game_elapsed:.3}s game versus {elapsed:.3}s real"
    );
    assert_eq!(view["manualPause"], true);
    host.finish().await;
}

#[tokio::test]
async fn host_menu_render_remains_bounded_for_every_page_and_small_terminal() {
    let mut host = Host::new(2).await;
    let player = host.handle.join(None, "Ansel".into()).await.unwrap();
    host.request("select", json!({"actorId":player.actor_id}))
        .await
        .unwrap();
    host.request("save", json!({"name":"fixture-save"}))
        .await
        .unwrap();
    let mut ui = MenuUi::default();
    ui.set_view(host.view().await);
    for (width, height) in [(120, 40), (80, 24), (30, 8), (1, 1)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        for page in Page::ALL {
            ui.page = page;
            for detail in 0..5 {
                ui.detail = detail;
                terminal.draw(|frame| ui.render(frame)).unwrap();
                assert_eq!(terminal.backend().buffer().area.width, width);
                assert_eq!(terminal.backend().buffer().area.height, height);
            }
        }
    }
    // Selecting a human does not prevent a usable manager after that human disappears.
    host.request("new", json!({"seed":99})).await.unwrap();
    ui.set_view(host.view().await);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| ui.render(frame)).unwrap();
    host.finish().await;
}

#[tokio::test]
async fn a_second_ipc_server_cannot_replace_a_live_manager_endpoint() {
    let mut host = Host::new(1).await;
    assert!(IpcServer::bind(host.directory.path()).await.is_err());
    assert!(host.view().await.is_object());
    host.ipc.close().await;
    // A leftover non-listening socket can be cleaned up safely.
    let stale = std::os::unix::net::UnixListener::bind(&host.ipc.path).unwrap();
    drop(stale);
    let mut replacement = IpcServer::bind(host.directory.path()).await.unwrap();
    replacement.serve(host.handle.clone());
    // A second close and the old server's drop cannot unlink the new endpoint.
    host.ipc.close().await;
    assert!(host.view().await.is_object());
    host.ipc = replacement;
    assert!(host.view().await.is_object());
    host.finish().await;
}

#[test]
fn server_ownership_is_exclusive_per_directory_and_never_aliases_another_castle() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let owner = paths::lock_server(first.path()).unwrap();
    assert!(paths::lock_server(first.path()).is_err());
    let other = paths::lock_server(second.path()).unwrap();
    assert_ne!(
        paths::manager_socket(first.path()).unwrap(),
        paths::manager_socket(second.path()).unwrap()
    );
    drop(owner);
    assert!(paths::lock_server(first.path()).is_ok());
    drop(other);
}

#[tokio::test]
async fn auto_commands_belong_to_each_player_and_public_rosters_hide_controllers() {
    let mut host = Host::new(3).await;
    let first = host.handle.join(None, "Ansel".into()).await.unwrap();
    let second = host.handle.join(None, "Boris".into()).await.unwrap();
    first
        .request("listener", json!({"enabled":true}))
        .await
        .unwrap();
    assert_eq!(first.views.borrow()["listener"], json!({"enabled":true}));
    assert_eq!(second.views.borrow()["listener"], json!({"enabled":false}));
    let view = second.views.borrow().clone();
    assert!(
        view["actors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|actor| actor["id"] == first.actor_id)
    );
    for actor in view["actors"].as_array().unwrap() {
        assert!(actor.get("controller").is_none());
        assert!(actor.get("identity").is_none());
        assert!(actor.get("memories").is_none());
        assert!(actor.get("goals").is_none());
    }
    assert_eq!(view["self"]["id"], second.actor_id);
    let private = host.view().await;
    assert!(
        private["actors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|actor| actor["id"] == first.actor_id && actor["controller"] == "human")
    );
    assert!(
        private["actors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|actor| actor["id"] == 102 && actor["controller"] == "ai")
    );
    first.disconnect().await;
    assert_eq!(connected(&host.view().await), vec![second.actor_id]);
    let reconnect = host.handle.join(None, "Ansel".into()).await.unwrap();
    assert_eq!(
        reconnect.views.borrow()["listener"],
        json!({"enabled":true})
    );
    assert_eq!(second.views.borrow()["listener"], json!({"enabled":false}));
    host.finish().await;
}

#[tokio::test]
async fn restoring_a_save_renews_sessions_even_when_the_same_character_returns() {
    let mut host = Host::new(2).await;
    let old = host.handle.join(None, "Ansel".into()).await.unwrap();
    host.request("save", json!({"name":"before-restore"}))
        .await
        .unwrap();
    old.request("listener", json!({"enabled":true}))
        .await
        .unwrap();
    host.request("load", json!({"name":"before-restore"}))
        .await
        .unwrap();
    assert_eq!(old.views.borrow()["closed"], true);
    let fresh = host.handle.join(None, "Ansel".into()).await.unwrap();
    assert_eq!(old.actor_id, fresh.actor_id);
    assert_ne!(old.session_id, fresh.session_id);
    assert_eq!(
        fresh.views.borrow()["listener"],
        json!({"enabled":false}),
        "Restoring the automatic backup must use its selected snapshot, not the new backup of current state"
    );
    assert!(
        old.request("listener", json!({"enabled":true}))
            .await
            .is_err()
    );
    old.disconnect().await;
    assert_eq!(connected(&host.view().await), vec![fresh.actor_id]);
    fresh
        .request("listener", json!({"enabled":true}))
        .await
        .unwrap();
    assert_eq!(fresh.views.borrow()["listener"], json!({"enabled":true}));
    host.finish().await;
}

#[tokio::test]
async fn native_cli_rejects_invalid_launches_and_cannot_stop_a_different_castle() {
    let mut host = Host::new(1).await;
    let player = host.handle.join(None, "Ansel".into()).await.unwrap();
    let other = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_below-the-crown");
    let owner = paths::lock_server(host.directory.path()).unwrap();
    let duplicate = std::process::Command::new(binary)
        .arg("--data-dir")
        .arg(host.directory.path())
        .args([
            "host",
            "--headless",
            "--bind",
            "127.0.0.1",
            "--subnet",
            "127.0.0.0/8",
        ])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already owns"));
    assert_eq!(connected(&host.view().await), vec![player.actor_id]);
    let unrelated_stop = std::process::Command::new(binary)
        .arg("--data-dir")
        .arg(other.path())
        .arg("stop")
        .output()
        .unwrap();
    assert!(!unrelated_stop.status.success());
    assert!(String::from_utf8_lossy(&unrelated_stop.stderr).contains("No host is running"));
    assert_eq!(connected(&host.view().await), vec![player.actor_id]);
    for arguments in [
        vec!["play"],
        vec!["host", "--speed", "5"],
        vec!["host", "--port", "0"],
        vec!["host", "--max-players", "33"],
        vec!["host", "--language", "de"],
        vec!["host", "--new", "--save", "another-save"],
    ] {
        let result = std::process::Command::new(binary)
            .arg("--data-dir")
            .arg(other.path())
            .args(&arguments)
            .output()
            .unwrap();
        assert!(
            !result.status.success(),
            "Accepted invalid CLI args {arguments:?}"
        );
    }
    assert_eq!(connected(&host.view().await), vec![player.actor_id]);
    assert!(!other.path().join("server.json").exists());
    assert!(!other.path().join("codex").exists());
    drop(owner);
    host.finish().await;
}
