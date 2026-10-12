use super::*;
use tokio::{process::Command, sync::broadcast};

fn request() -> ThinkRequest {
    let engine = crate::engine::Engine::memory("castle", "en", 3).unwrap();
    ThinkRequest {
        actor_id: 102,
        branch: engine.world.branch.clone(),
        token: DecisionToken {
            job_id: 7,
            context_generation: 2,
        },
        identity: Some(engine.npc_identity(102).unwrap()),
        input: json!({"world":{"self":{"id":102}},"execution":{"reason":"current"}}),
        listener: false,
    }
}
fn fixture() -> (
    Arc<CodexDriver>,
    Arc<Rpc>,
    broadcast::Receiver<Value>,
    mpsc::Receiver<RecallRequest>,
    tempfile::TempDir,
) {
    let directory = tempfile::tempdir().unwrap();
    let mut command = Command::new("sh");
    command.args(["-c", "while IFS= read -r line; do :; done"]);
    let rpc = Rpc::spawn(command).unwrap();
    let outgoing = rpc.requests();
    let (recalls, receiver) = mpsc::channel(8);
    let driver = Arc::new(
        CodexDriver::new(
            directory.path().into(),
            "castle",
            "en",
            WorkerOptions::default(),
            recalls,
        )
        .unwrap(),
    );
    driver.inner.state.lock().expect("worker state").rpc = Some(rpc.clone());
    (driver, rpc, outgoing, receiver, directory)
}
async fn next(outgoing: &mut broadcast::Receiver<Value>, method: &str) -> Value {
    let value = tokio::time::timeout(Duration::from_secs(2), outgoing.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value["method"], method, "{value}");
    value
}
async fn reply(rpc: &Rpc, request: &Value, result: Value) {
    rpc.inject(json!({"id":request["id"],"result":result}))
        .await
        .unwrap();
}
async fn thread_ack(rpc: &Rpc, start: &Value, thread: &str, model: &str) {
    reply(rpc,start,json!({"model":model,"reasoningEffort":EFFORT,"serviceTier":null,"instructionSources":[],"thread":{"id":thread}})).await;
}
async fn completed(rpc: &Rpc, thread: &str, turn: &str) {
    rpc.inject(json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"type":"agentMessage","text":json!({"summary":"Current concern.","goals":[],"utterance":null,"plan":[{"kind":"wait","seconds":600}],"memories":[],"relationships":[],"acceptPlots":[],"acceptHeardProposals":[]}).to_string()}}})).await.unwrap();
    rpc.inject(json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":"completed"}}})).await.unwrap();
}
async fn close_thread(rpc: &Rpc, outgoing: &mut broadcast::Receiver<Value>, thread: &str) {
    let unsubscribe = next(outgoing, "thread/unsubscribe").await;
    assert_eq!(unsubscribe["params"]["threadId"], thread);
    reply(rpc, &unsubscribe, json!({"status":"unsubscribed"})).await;
    rpc.inject(json!({"method":"thread/closed","params":{"threadId":thread}}))
        .await
        .unwrap();
}

#[tokio::test]
async fn completed_npc_decisions_reuse_their_thread_but_revoke_recall_between_turns() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let original = request();
    for suffix in ["first", "second"] {
        let copy = driver.clone();
        let request = original.clone();
        let thinking =
            tokio::spawn(async move { copy.think(request, CancellationToken::new()).await });
        if suffix == "first" {
            let start = next(&mut outgoing, "thread/start").await;
            assert_eq!(start["params"]["ephemeral"], true);
            thread_ack(&rpc, &start, "actor-thread", MODEL).await;
        }
        let turn = next(&mut outgoing, "turn/start").await;
        assert_eq!(turn["params"]["threadId"], "actor-thread");
        reply(&rpc, &turn, json!({"turn":{"id":suffix}})).await;
        completed(&rpc, "actor-thread", suffix).await;
        assert!(thinking.await.unwrap().is_ok());
        assert!(
            outgoing.try_recv().is_err(),
            "Successful NPC context must remain loaded"
        );
        assert!(!rpc.permits_recall("actor-thread", suffix).await);
        assert!(
            driver
                .inner
                .state
                .lock()
                .expect("worker state")
                .owners
                .is_empty()
        );
        assert_eq!(
            driver
                .inner
                .state
                .lock()
                .expect("worker state")
                .threads
                .len(),
            1
        );
    }
    driver.reset().await;
    assert!(
        driver
            .inner
            .state
            .lock()
            .expect("worker state")
            .threads
            .is_empty()
    );
}

#[tokio::test]
async fn completed_context_cancelled_by_scheduler_cannot_be_reused() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let original = request();
    let cancel = CancellationToken::new();
    let copy = driver.clone();
    let child = cancel.clone();
    let first_request = original.clone();
    let first = tokio::spawn(async move { copy.think(first_request, child).await });
    let start = next(&mut outgoing, "thread/start").await;
    thread_ack(&rpc, &start, "obsolete", MODEL).await;
    let start = next(&mut outgoing, "turn/start").await;
    reply(&rpc, &start, json!({"turn":{"id":"first"}})).await;
    completed(&rpc, "obsolete", "first").await;
    assert!(first.await.unwrap().is_ok());
    cancel.cancel();
    let copy = driver.clone();
    let second = tokio::spawn(async move { copy.think(original, CancellationToken::new()).await });
    let unsubscribe = next(&mut outgoing, "thread/unsubscribe").await;
    assert_eq!(unsubscribe["params"]["threadId"], "obsolete");
    reply(&rpc, &unsubscribe, json!({"status":"unsubscribed"})).await;
    assert!(!second.is_finished());
    rpc.inject(json!({"method":"thread/closed","params":{"threadId":"obsolete"}}))
        .await
        .unwrap();
    let start = next(&mut outgoing, "thread/start").await;
    thread_ack(&rpc, &start, "current", MODEL).await;
    let start = next(&mut outgoing, "turn/start").await;
    reply(&rpc, &start, json!({"turn":{"id":"second"}})).await;
    completed(&rpc, "current", "second").await;
    assert!(second.await.unwrap().is_ok());
    driver.reset().await;
}

#[tokio::test]
async fn cancelling_a_completed_context_retires_it_without_another_request() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let cancel = CancellationToken::new();
    let copy = driver.clone();
    let token = cancel.clone();
    let first = tokio::spawn(async move { copy.think(request(), token).await });
    let start = next(&mut outgoing, "thread/start").await;
    thread_ack(&rpc, &start, "finished", MODEL).await;
    let turn = next(&mut outgoing, "turn/start").await;
    reply(&rpc, &turn, json!({"turn":{"id":"first"}})).await;
    completed(&rpc, "finished", "first").await;
    first.await.unwrap().unwrap();
    cancel.cancel();
    let unsubscribe = next(&mut outgoing, "thread/unsubscribe").await;
    assert_eq!(unsubscribe["params"]["threadId"], "finished");
    let mut retirement = {
        let state = driver.inner.state.lock().expect("worker state");
        assert!(state.threads.is_empty());
        assert_eq!(state.owners.len(), 1);
        state.owners.values().next().unwrap().finished.clone()
    };
    reply(&rpc, &unsubscribe, json!({"status":"unsubscribed"})).await;
    assert!(
        !*retirement.borrow(),
        "The acknowledgement alone is not closure"
    );
    rpc.inject(json!({"method":"thread/closed","params":{"threadId":"finished"}}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), retirement.wait_for(|done| *done))
        .await
        .unwrap()
        .unwrap();
    assert!(
        driver
            .inner
            .state
            .lock()
            .expect("worker state")
            .owners
            .is_empty()
    );
    assert!(!rpc.closed());
    driver.reset().await;
}

#[tokio::test]
async fn cancelling_an_older_success_cannot_retire_the_reused_context() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let original = request();
    let old = CancellationToken::new();
    for (turn_id, token) in [("first", old.clone()), ("second", CancellationToken::new())] {
        let copy = driver.clone();
        let request = original.clone();
        let thinking = tokio::spawn(async move { copy.think(request, token).await });
        if turn_id == "first" {
            let start = next(&mut outgoing, "thread/start").await;
            thread_ack(&rpc, &start, "continuous", MODEL).await;
        }
        let turn = next(&mut outgoing, "turn/start").await;
        assert_eq!(turn["params"]["threadId"], "continuous");
        reply(&rpc, &turn, json!({"turn":{"id":turn_id}})).await;
        completed(&rpc, "continuous", turn_id).await;
        thinking.await.unwrap().unwrap();
    }
    old.cancel();
    let copy = driver.clone();
    let third = tokio::spawn(async move { copy.think(original, CancellationToken::new()).await });
    let turn = next(&mut outgoing, "turn/start").await;
    assert_eq!(turn["params"]["threadId"], "continuous");
    reply(&rpc, &turn, json!({"turn":{"id":"third"}})).await;
    completed(&rpc, "continuous", "third").await;
    third.await.unwrap().unwrap();
    driver.reset().await;
}

#[tokio::test]
async fn reset_cancels_cached_retirement_and_invalidates_a_waiting_replacement() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let cancel = CancellationToken::new();
    let copy = driver.clone();
    let token = cancel.clone();
    let original = request();
    let first_request = original.clone();
    let first = tokio::spawn(async move { copy.think(first_request, token).await });
    let start = next(&mut outgoing, "thread/start").await;
    thread_ack(&rpc, &start, "old", MODEL).await;
    let turn = next(&mut outgoing, "turn/start").await;
    reply(&rpc, &turn, json!({"turn":{"id":"first"}})).await;
    completed(&rpc, "old", "first").await;
    first.await.unwrap().unwrap();
    cancel.cancel();
    let unsubscribe = next(&mut outgoing, "thread/unsubscribe").await;
    assert_eq!(unsubscribe["params"]["threadId"], "old");
    let mut replacement = driver.think(original, CancellationToken::new());
    assert!(futures::poll!(&mut replacement).is_pending());
    driver.reset().await;
    assert!(replacement.await.is_err());
    assert!(rpc.closed());
    assert!(outgoing.try_recv().is_err());
    let state = driver.inner.state.lock().expect("worker state");
    assert!(state.threads.is_empty());
    assert!(state.owners.is_empty());
}

#[tokio::test]
async fn changed_branch_generation_or_identity_retires_the_old_context() {
    for change in ["branch", "generation", "identity"] {
        let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
        let original = request();
        let copy = driver.clone();
        let first_request = original.clone();
        let first =
            tokio::spawn(async move { copy.think(first_request, CancellationToken::new()).await });
        let start = next(&mut outgoing, "thread/start").await;
        thread_ack(&rpc, &start, "old", MODEL).await;
        let start = next(&mut outgoing, "turn/start").await;
        reply(&rpc, &start, json!({"turn":{"id":"first"}})).await;
        completed(&rpc, "old", "first").await;
        first.await.unwrap().unwrap();
        let mut changed = original;
        match change {
            "branch" => changed.branch = "new-branch".into(),
            "generation" => changed.token.context_generation += 1,
            _ => changed.identity.as_mut().unwrap().personality = "My custom disposition.".into(),
        }
        let copy = driver.clone();
        let second =
            tokio::spawn(async move { copy.think(changed, CancellationToken::new()).await });
        close_thread(&rpc, &mut outgoing, "old").await;
        let start = next(&mut outgoing, "thread/start").await;
        thread_ack(&rpc, &start, "new", MODEL).await;
        let start = next(&mut outgoing, "turn/start").await;
        reply(&rpc, &start, json!({"turn":{"id":"second"}})).await;
        completed(&rpc, "new", "second").await;
        second.await.unwrap().unwrap();
        driver.reset().await;
    }
}

#[tokio::test]
async fn cancelled_start_keeps_the_late_thread_acknowledgement_owned() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let cancel = CancellationToken::new();
    let child = cancel.clone();
    let copy = driver.clone();
    let thinking = tokio::spawn(async move { copy.think(request(), child).await });
    let start = next(&mut outgoing, "thread/start").await;
    cancel.cancel();
    assert!(!thinking.is_finished());
    thread_ack(&rpc, &start, "late-thread", MODEL).await;
    close_thread(&rpc, &mut outgoing, "late-thread").await;
    assert!(thinking.await.unwrap().is_err());
    assert!(!rpc.closed());
    driver.reset().await;
}

#[tokio::test]
async fn cancelling_a_turn_start_interrupts_its_late_ack_and_waits_for_terminal() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let cancel = CancellationToken::new();
    let child = cancel.clone();
    let copy = driver.clone();
    let thinking = tokio::spawn(async move { copy.think(request(), child).await });
    let start = next(&mut outgoing, "thread/start").await;
    thread_ack(&rpc, &start, "thread", MODEL).await;
    let start = next(&mut outgoing, "turn/start").await;
    cancel.cancel();
    reply(&rpc, &start, json!({"turn":{"id":"late-turn"}})).await;
    let interrupt = next(&mut outgoing, "turn/interrupt").await;
    assert_eq!(interrupt["params"]["turnId"], "late-turn");
    reply(&rpc, &interrupt, json!({})).await;
    rpc.inject(json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"old-turn","status":"completed"}}})).await.unwrap();
    tokio::task::yield_now().await;
    assert!(outgoing.try_recv().is_err());
    assert!(!thinking.is_finished());
    rpc.inject(json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"late-turn","status":"interrupted"}}})).await.unwrap();
    close_thread(&rpc, &mut outgoing, "thread").await;
    assert!(thinking.await.unwrap().is_err());
    driver.reset().await;
}

#[tokio::test]
async fn dropping_the_public_future_does_not_drop_its_cleanup_owner() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let copy = driver.clone();
    let thinking =
        tokio::spawn(async move { copy.think(request(), CancellationToken::new()).await });
    let start = next(&mut outgoing, "thread/start").await;
    thinking.abort();
    let _ = thinking.await;
    assert_eq!(
        driver
            .inner
            .state
            .lock()
            .expect("worker state")
            .owners
            .len(),
        1
    );
    let mut finished = driver
        .inner
        .state
        .lock()
        .expect("worker state")
        .owners
        .values()
        .next()
        .unwrap()
        .finished
        .clone();
    thread_ack(&rpc, &start, "owned-thread", MODEL).await;
    close_thread(&rpc, &mut outgoing, "owned-thread").await;
    tokio::time::timeout(Duration::from_secs(2), finished.wait_for(|done| *done))
        .await
        .unwrap()
        .unwrap();
    assert!(
        driver
            .inner
            .state
            .lock()
            .expect("worker state")
            .owners
            .is_empty()
    );
    driver.reset().await;
}

#[tokio::test]
async fn rejected_metadata_still_unloads_the_created_thread() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let copy = driver.clone();
    let thinking =
        tokio::spawn(async move { copy.think(request(), CancellationToken::new()).await });
    let start = next(&mut outgoing, "thread/start").await;
    thread_ack(&rpc, &start, "bad-thread", "unexpected-model").await;
    close_thread(&rpc, &mut outgoing, "bad-thread").await;
    assert!(thinking.await.unwrap().is_err());
    assert!(!rpc.closed());
    driver.reset().await;
}

#[tokio::test]
async fn reset_retires_the_originating_worker_and_changes_epoch() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let copy = driver.clone();
    let thinking =
        tokio::spawn(async move { copy.think(request(), CancellationToken::new()).await });
    next(&mut outgoing, "thread/start").await;
    driver.reset().await;
    assert!(thinking.await.unwrap().is_err());
    assert!(rpc.closed());
    let state = driver.inner.state.lock().expect("worker state");
    assert_eq!(state.epoch, 1);
    assert!(state.rpc.is_none());
    assert!(state.owners.is_empty());
}

#[tokio::test]
async fn uncertain_unload_kills_only_its_origin_and_waits_for_exit() {
    let (driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let copy = driver.clone();
    let mut listener = request();
    listener.listener = true;
    listener.identity = None;
    let thinking =
        tokio::spawn(async move { copy.think(listener, CancellationToken::new()).await });
    let start = next(&mut outgoing, "thread/start").await;
    thread_ack(&rpc, &start, "stalled", MODEL).await;
    let start = next(&mut outgoing, "turn/start").await;
    reply(&rpc, &start, json!({"turn":{"id":"done"}})).await;
    completed(&rpc, "stalled", "done").await;
    let unsubscribe = next(&mut outgoing, "thread/unsubscribe").await;
    reply(&rpc, &unsubscribe, json!({"status":"unsubscribed"})).await;
    let mut command = Command::new("sh");
    command.args(["-c", "while IFS= read -r line; do :; done"]);
    let replacement = Rpc::spawn(command).unwrap();
    driver.inner.state.lock().expect("worker state").rpc = Some(replacement.clone());
    assert!(
        tokio::time::timeout(Duration::from_secs(4), thinking)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(rpc.closed());
    assert!(!replacement.closed());
    tokio::join!(rpc.close(), rpc.close());
    driver.reset().await;
}

#[tokio::test]
async fn recall_admission_buffers_early_calls_and_rejects_unknown_listener_old_or_revoked_turns() {
    let (_driver, rpc, mut outgoing, _recalls, _directory) = fixture();
    let mut events = rpc.subscribe();
    rpc.bind("npc", true).await;
    rpc.bind("listener", false).await;
    let call = |id: &str, thread: &str, turn: &str| json!({"id":id,"method":"item/tool/call","params":{"threadId":thread,"turnId":turn,"tool":"recall","arguments":{"query":"own history"}}});
    rpc.inject(call("early", "npc", "current")).await.unwrap();
    assert!(events.try_recv().is_err());
    rpc.confirm_turn("npc", "current").await.unwrap();
    assert_eq!(events.recv().await.unwrap()["id"], "early");
    for (id, thread, turn) in [
        ("unknown", "unknown", "current"),
        ("listener", "listener", "current"),
        ("old", "npc", "old"),
    ] {
        rpc.inject(call(id, thread, turn)).await.unwrap();
        assert_eq!(outgoing.recv().await.unwrap()["id"], id);
        assert!(events.try_recv().is_err());
    }
    rpc.unbind("npc").await;
    rpc.inject(call("revoked", "npc", "current")).await.unwrap();
    assert_eq!(outgoing.recv().await.unwrap()["id"], "revoked");
    assert!(!rpc.permits_recall("npc", "current").await);
    rpc.close().await;
}

#[tokio::test]
async fn a_cancelled_write_does_not_leave_a_partial_jsonl_frame() {
    let mut command = Command::new("sh");
    command.args(["-c", "sleep 0.1; exec cat"]);
    let rpc = Rpc::spawn(command).unwrap();
    let mut outgoing = rpc.requests();
    let copy = rpc.clone();
    let sending = tokio::spawn(async move { copy.send(json!({"text":"x".repeat(900_000)})).await });
    outgoing.recv().await.unwrap();
    tokio::task::yield_now().await;
    sending.abort();
    let _ = sending.await;
    let result = rpc
        .request("unsupported-fixture", json!({}))
        .await
        .unwrap_err();
    assert!(result.to_string().contains("Only recall"), "{result}");
    assert!(!rpc.closed());
    rpc.close().await;
}
