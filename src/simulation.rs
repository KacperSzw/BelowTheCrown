use crate::{
    codex::{
        CodexDriver, DecisionDriver, RecallRequest, ThinkRequest, ThinkResult, WorkerOptions,
        safe_error,
    },
    content::WorldContent,
    engine::Engine,
    model::{EFFORT, MODEL, Wake},
    paths::write_private,
    runtime::{GameHandle, HostInfo, PlayerConnection, RuntimeRequest},
};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct Scheduling {
    pub maximum_calls: usize,
    pub speech_batch: f64,
    pub routine_cooldown: f64,
    pub background_interval: f64,
    pub aging: f64,
    pub timeout: Duration,
}
impl Default for Scheduling {
    fn default() -> Self {
        Self {
            maximum_calls: 4,
            speech_batch: 2.,
            routine_cooldown: 4.,
            background_interval: 30.,
            aging: 30.,
            timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Clone)]
struct Queued {
    wake: Wake,
    order: u64,
    created: f64,
    ready: f64,
    attempt: u8,
    feedback: Option<String>,
}
#[derive(Clone)]
struct Message {
    actor: u16,
    text: String,
    volume: String,
    event_id: u64,
    branch: String,
    visit: String,
    combat: Option<Value>,
    order: u64,
}
struct Job {
    id: u64,
    actor: u16,
    listener: bool,
    branch: String,
    version: u64,
    combat: Option<Value>,
    visit: String,
    started: f64,
    cancel: CancellationToken,
    queued: Option<Queued>,
    message: Option<Message>,
    allowed: HashSet<u16>,
    evidence: Vec<u64>,
}
struct Completion {
    id: u64,
    actor: u16,
    listener: bool,
    result: Result<ThinkResult>,
}
#[derive(Default)]
struct Status {
    calls: u64,
    tokens: u64,
    latency: Option<f64>,
    error: Option<String>,
    reason: String,
    state: String,
    summary: String,
    plan: Value,
}

pub struct Simulation {
    pub engine: Engine,
    handle: GameHandle,
    manager: watch::Sender<Value>,
    requests: mpsc::Receiver<RuntimeRequest>,
    recall_requests: mpsc::Receiver<RecallRequest>,
    recall_sender: mpsc::Sender<RecallRequest>,
    done: mpsc::Receiver<Completion>,
    done_sender: mpsc::Sender<Completion>,
    pub scheduling: Scheduling,
    driver: Arc<dyn DecisionDriver>,
    production: Option<WorkerOptions>,
    connected: BTreeMap<u16, watch::Sender<Value>>,
    sessions: HashMap<u16, u64>,
    queued: BTreeMap<u16, Queued>,
    jobs: HashMap<(u16, bool), Job>,
    inflight: HashMap<u64, (u16, bool)>,
    deferred: VecDeque<Completion>,
    messages: VecDeque<Message>,
    statuses: HashMap<(u16, bool), Status>,
    last_started: HashMap<u16, f64>,
    room_last: HashMap<u16, u64>,
    sequence: u64,
    wall: f64,
    last_idle: f64,
    observing: bool,
    selected: Option<u16>,
    pending_language: String,
    pending_scenario: String,
    error: Option<String>,
    stopped: bool,
    latencies: VecDeque<f64>,
}

impl Simulation {
    pub fn production(
        engine: Engine,
        info: HostInfo,
        options: WorkerOptions,
    ) -> Result<(Self, GameHandle)> {
        let (recall_sender, recall_requests) = mpsc::channel(64);
        let driver = Arc::new(CodexDriver::from_content(
            info.data_dir.clone(),
            engine.content.clone(),
            &engine.language,
            options.clone(),
            recall_sender.clone(),
        ));
        Ok(Self::construct(
            engine,
            info,
            driver,
            Some(options),
            recall_sender,
            recall_requests,
        ))
    }
    pub fn with_driver(
        engine: Engine,
        info: HostInfo,
        driver: Arc<dyn DecisionDriver>,
    ) -> (Self, GameHandle) {
        let (recall_sender, recall_requests) = mpsc::channel(64);
        Self::construct(engine, info, driver, None, recall_sender, recall_requests)
    }
    fn construct(
        mut engine: Engine,
        info: HostInfo,
        driver: Arc<dyn DecisionDriver>,
        production: Option<WorkerOptions>,
        recall_sender: mpsc::Sender<RecallRequest>,
        recall_requests: mpsc::Receiver<RecallRequest>,
    ) -> (Self, GameHandle) {
        let (sender, requests) = mpsc::channel(128);
        let (manager, views) = watch::channel(json!({}));
        let handle = GameHandle {
            sender,
            manager: views,
            info: Arc::new(info),
        };
        let (done_sender, done) = mpsc::channel(64);
        let language = engine.language.clone();
        let scenario = engine.world.scenario_id.clone();
        let initialization_error = engine.initialize_minds().err().map(safe_error);
        let mut simulation = Self {
            engine,
            handle: handle.clone(),
            manager,
            requests,
            recall_requests,
            recall_sender,
            done,
            done_sender,
            scheduling: Scheduling::default(),
            driver,
            production,
            connected: BTreeMap::new(),
            sessions: HashMap::new(),
            queued: BTreeMap::new(),
            jobs: HashMap::new(),
            inflight: HashMap::new(),
            deferred: VecDeque::new(),
            messages: VecDeque::new(),
            statuses: HashMap::new(),
            last_started: HashMap::new(),
            room_last: HashMap::new(),
            sequence: 0,
            wall: 0.,
            last_idle: 0.,
            observing: false,
            selected: Some(102),
            pending_language: language,
            pending_scenario: scenario,
            error: None,
            stopped: false,
            latencies: VecDeque::new(),
        };
        simulation.error = initialization_error;
        simulation.collect();
        let _ = simulation.publish();
        (simulation, handle)
    }
    fn next(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
    pub fn can_run(&self) -> bool {
        !self.stopped
            && self.error.is_none()
            && !self.engine.world.manual_pause
            && self.engine.world.ending == "playing"
            && (!self.connected.is_empty() || self.observing)
    }
    pub async fn preflight(&self) -> Result<Value> {
        self.driver.inspect().await
    }
    fn visit(&self, actor: u16) -> String {
        format!(
            "{}:{}:{}",
            self.engine.world.branch,
            actor,
            self.engine.actor(actor).map_or(0, |a| a.entered_event_id)
        )
    }
    fn combat(&self, actor: u16) -> Option<Value> {
        self.engine.world.encounters.iter().find_map(|e| {
            e.participants
                .iter()
                .find(|p| p.actor_id == actor)
                .map(|p| json!({"encounterId":e.id,"round":e.round.max(p.joins_round)}))
        })
    }
    fn enabled(&self, actor: u16) -> bool {
        self.connected.contains_key(&actor)
            && self
                .engine
                .world
                .players
                .iter()
                .any(|p| p.actor_id == actor && p.listener)
    }
    fn foreground(&self, actor: u16) -> bool {
        self.engine.actor(actor).is_ok_and(|a| {
            self.connected
                .keys()
                .any(|id| self.engine.actor(*id).is_ok_and(|p| p.room_id == a.room_id))
        })
    }
    fn job_current(&self, job: &Job) -> bool {
        job.branch == self.engine.world.branch
            && self
                .engine
                .actor(job.actor)
                .is_ok_and(|a| a.awake() && a.decision_version == job.version)
            && job.combat == self.combat(job.actor)
            && (!job.listener || self.enabled(job.actor) && job.visit == self.visit(job.actor))
    }
    fn message_current(&self, message: &Message) -> bool {
        self.enabled(message.actor)
            && self.engine.actor(message.actor).is_ok_and(|a| a.awake())
            && message.branch == self.engine.world.branch
            && message.visit == self.visit(message.actor)
            && message.combat == self.combat(message.actor)
    }
    fn collect(&mut self) {
        self.engine.drain_events();
        if self.engine.world.ending != "playing" {
            self.cancel_all();
            self.engine.drain_wakes();
            return;
        }
        for wake in self.engine.drain_wakes() {
            if self.engine.is_player(wake.actor_id) {
                continue;
            }
            let id = wake.actor_id;
            if let Some(queued) = self.queued.get_mut(&id) {
                let urgent = queued.wake.urgent || wake.urgent;
                queued.wake = wake;
                queued.wake.urgent = urgent;
                if queued.wake.urgent {
                    queued.ready = self.wall;
                }
            } else {
                let ready = self.wall
                    + if wake.urgent {
                        0.
                    } else {
                        self.scheduling.speech_batch
                    };
                let order = self.next();
                self.queued.insert(
                    id,
                    Queued {
                        wake,
                        order,
                        created: self.wall,
                        ready,
                        attempt: 0,
                        feedback: None,
                    },
                );
            }
        }
        let stale = self
            .jobs
            .iter()
            .filter(|(_, job)| !self.job_current(job))
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        for key in stale {
            if let Some(job) = self.jobs.remove(&key) {
                job.cancel.cancel();
            }
        }
        let engine = &self.engine;
        self.queued
            .retain(|id, _| engine.actor(*id).is_ok_and(|a| a.awake()));
        let valid = self
            .messages
            .iter()
            .filter(|m| self.message_current(m))
            .cloned()
            .collect::<VecDeque<_>>();
        self.messages = valid;
    }
    fn paused(&self) -> Vec<&str> {
        let mut reasons = vec![];
        if self.engine.world.manual_pause {
            reasons.push("Paused");
        }
        if self.connected.is_empty() && !self.observing {
            reasons.push("No player connected");
        }
        if self.error.is_some() {
            reasons.push("A mind needs attention");
        }
        if self.engine.world.ending != "playing" {
            reasons.push("Run ended");
        }
        reasons
    }
    fn pump(&mut self) -> Result<()> {
        self.collect();
        if !self.can_run() {
            return Ok(());
        }
        while let Some(completion) = self.deferred.pop_front() {
            self.accept(completion)?;
        }
        loop {
            if self.inflight.len() >= self.scheduling.maximum_calls {
                break;
            }
            let mut candidates = Vec::new();
            for (id, q) in &self.queued {
                if self.jobs.contains_key(&(*id, false))
                    || self.inflight.values().any(|key| *key == (*id, false))
                    || self.wall < q.ready
                {
                    continue;
                }
                let danger = q.wake.urgent || self.combat(*id).is_some();
                if !danger
                    && self
                        .last_started
                        .get(id)
                        .is_some_and(|last| self.wall - *last < self.scheduling.routine_cooldown)
                {
                    continue;
                }
                let foreground = self.foreground(*id);
                let aged = self.wall - q.created >= self.scheduling.aging;
                if !foreground
                    && !danger
                    && !aged
                    && self
                        .jobs
                        .values()
                        .filter(|j| !j.listener && !self.foreground(j.actor))
                        .count()
                        >= self.scheduling.maximum_calls.saturating_sub(1)
                {
                    continue;
                }
                let priority = if danger {
                    0
                } else if foreground || aged {
                    2
                } else {
                    3
                };
                let room = self.engine.actor(*id)?.room_id;
                candidates.push((
                    priority,
                    self.room_last.get(&room).copied().unwrap_or(0),
                    q.order,
                    *id,
                    false,
                ));
            }
            for m in &self.messages {
                if self.jobs.contains_key(&(m.actor, true))
                    || self.inflight.values().any(|key| *key == (m.actor, true))
                    || self.engine.actor(m.actor)?.activity.is_some()
                    || !self.engine.actor(m.actor)?.plan.is_empty()
                {
                    continue;
                }
                candidates.push((
                    if m.combat.is_some() { 0 } else { 1 },
                    0,
                    m.order,
                    m.actor,
                    true,
                ));
            }
            candidates.sort();
            let Some((_, _, _, id, listener)) = candidates.first().copied() else {
                break;
            };
            if listener {
                let index = self.messages.iter().position(|m| m.actor == id).unwrap();
                let m = self.messages.remove(index).unwrap();
                self.launch_listener(m)?;
            } else {
                let q = self.queued.remove(&id).unwrap();
                self.launch_npc(q)?;
            }
        }
        Ok(())
    }
    fn launch_npc(&mut self, q: Queued) -> Result<()> {
        let actor = q.wake.actor_id;
        let reason=q.feedback.as_ref().map_or_else(||q.wake.reason.clone(),|feedback|format!("{}\nYour last decision was rejected: {feedback}. Correct it using current facts.",q.wake.reason));
        let observation = self.engine.observation(actor, &reason)?;
        let evidence = observation["context"]["evidenceIds"]
            .as_array()
            .map(|values| values.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        let input = observation.get("input").cloned().unwrap_or(observation);
        let request = ThinkRequest {
            actor_id: actor,
            branch: self.engine.world.branch.clone(),
            input,
            listener: false,
        };
        let id = self.next();
        let cancel = CancellationToken::new();
        let job = Job {
            id,
            actor,
            listener: false,
            branch: request.branch.clone(),
            version: self.engine.actor(actor)?.decision_version,
            combat: self.combat(actor),
            visit: self.visit(actor),
            started: self.wall,
            cancel: cancel.clone(),
            queued: Some(q.clone()),
            message: None,
            allowed: HashSet::new(),
            evidence,
        };
        let room = self.engine.actor(actor)?.room_id;
        self.room_last.insert(room, id);
        self.last_started.insert(actor, self.wall);
        self.jobs.insert((actor, false), job);
        let status = self.statuses.entry((actor, false)).or_default();
        status.calls += 1;
        status.reason = reason;
        status.state = "thinking".into();
        status.error = None;
        self.spawn_turn(id, request, cancel);
        Ok(())
    }
    fn launch_listener(&mut self, message: Message) -> Result<()> {
        let actor = message.actor;
        let view = self.player_view(actor)?;
        let mut allowed = HashSet::from([actor]);
        gather_ids(&view, &mut allowed);
        let request = ThinkRequest {
            actor_id: actor,
            branch: message.branch.clone(),
            listener: true,
            input: json!({"text":message.text,"channel":{"targetId":null,"volume":message.volume},"world":view}),
        };
        let id = self.next();
        let cancel = CancellationToken::new();
        let job = Job {
            id,
            actor,
            listener: true,
            branch: message.branch.clone(),
            version: self.engine.actor(actor)?.decision_version,
            combat: message.combat.clone(),
            visit: message.visit.clone(),
            started: self.wall,
            cancel: cancel.clone(),
            queued: None,
            message: Some(message.clone()),
            allowed,
            evidence: vec![],
        };
        self.jobs.insert((actor, true), job);
        let status = self.statuses.entry((actor, true)).or_default();
        status.calls += 1;
        status.state = "thinking".into();
        status.error = None;
        status.reason = message.text;
        self.spawn_turn(id, request, cancel);
        Ok(())
    }
    fn spawn_turn(&mut self, id: u64, request: ThinkRequest, cancel: CancellationToken) {
        self.inflight
            .insert(id, (request.actor_id, request.listener));
        let driver = self.driver.clone();
        let sender = self.done_sender.clone();
        let timeout = self.scheduling.timeout;
        tokio::spawn(async move {
            let actor = request.actor_id;
            let listener = request.listener;
            let mut turn = driver.think(request, cancel.clone());
            let result = tokio::select! {
                result=&mut turn=>result,
                _=tokio::time::sleep(timeout)=>{cancel.cancel();let _=tokio::time::timeout(Duration::from_secs(3),&mut turn).await;Err(anyhow::anyhow!("The decision timed out"))},
            };
            let _ = sender
                .send(Completion {
                    id,
                    actor,
                    listener,
                    result,
                })
                .await;
        });
    }
    fn accept(&mut self, completion: Completion) -> Result<()> {
        let key = (completion.actor, completion.listener);
        let Some(current) = self.jobs.get(&key) else {
            return Ok(());
        };
        if current.id != completion.id {
            return Ok(());
        }
        if !self.job_current(current) {
            let job = self.jobs.remove(&key).unwrap();
            job.cancel.cancel();
            return Ok(());
        }
        let job = self.jobs.remove(&key).unwrap();
        let result = match completion.result {
            Ok(result) => {
                let status = self.statuses.entry(key).or_default();
                status.tokens += result.tokens;
                status.latency = Some(result.latency_ms);
                self.latencies.push_back(result.latency_ms);
                if self.latencies.len() > 200 {
                    self.latencies.pop_front();
                }
                if job.listener {
                    let parsed = crate::model::Interpretation::parse(result.decision.clone());
                    match parsed {
                        Ok(interpretation) => {
                            let plan = json!(interpretation.plan);
                            if referenced_ids(&plan)
                                .iter()
                                .any(|id| !job.allowed.contains(id))
                            {
                                Err(anyhow::anyhow!(
                                    "The listener referred to something outside your visible world"
                                ))
                            } else {
                                let message = job.message.as_ref().unwrap();
                                let accepted = self.engine.start_player_plan(
                                    job.actor,
                                    result.decision.clone(),
                                    message.event_id,
                                    job.combat.clone(),
                                )?;
                                let status = self.statuses.entry(key).or_default();
                                status.summary = interpretation.summary;
                                status.plan = plan;
                                Ok(accepted)
                            }
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    let mut evidence = job.evidence.clone();
                    evidence.extend(result.recalled);
                    evidence.sort_unstable();
                    evidence.dedup();
                    self.engine
                        .apply_decision(job.actor, result.decision, job.version, &evidence)
                }
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(answer) if answer["ok"] == true => {
                let status = self.statuses.entry(key).or_default();
                status.error = None;
                status.state = "idle".into();
            }
            answer => {
                let error = match answer {
                    Err(error) => safe_error(error),
                    Ok(value) => value["message"]
                        .as_str()
                        .unwrap_or("Decision rejected")
                        .into(),
                };
                let status = self.statuses.entry(key).or_default();
                status.error = Some(error.clone());
                status.state = "failed".into();
                if let Some(mut q) = job.queued.filter(|q| q.attempt < 1) {
                    q.attempt += 1;
                    q.feedback = Some(error);
                    q.ready = self.wall + self.scheduling.routine_cooldown;
                    self.queued.insert(job.actor, q);
                }
            }
        }
        Ok(())
    }
    fn cancel_listener(&mut self, actor: u16) {
        self.messages.retain(|m| m.actor != actor);
        if let Some(job) = self.jobs.remove(&(actor, true)) {
            job.cancel.cancel();
        }
        if let Some(status) = self.statuses.get_mut(&(actor, true)) {
            status.error = None;
            status.summary.clear();
            status.plan = json!([]);
            status.state = "idle".into();
        }
    }
    fn cancel_all(&mut self) {
        for (_, job) in self.jobs.drain() {
            job.cancel.cancel();
        }
        self.messages.clear();
        self.queued.clear();
        self.deferred.clear();
    }
    fn disconnect(&mut self, actor: u16) -> Result<()> {
        self.connected.remove(&actor);
        self.sessions.remove(&actor);
        self.cancel_listener(actor);
        self.engine.cancel_player_plan(actor)?;
        self.engine.save("autosave")?;
        Ok(())
    }
    fn join(&mut self, identity: Option<String>, name: String) -> Result<PlayerConnection> {
        ensure!(
            self.connected.len() < self.handle.info.maximum_players,
            "This castle is full"
        );
        let name = crate::engine::player_name(&name)?;
        let existing = if let Some(identity) = identity.as_ref() {
            self.engine
                .world
                .players
                .iter()
                .find(|p| p.identity == *identity)
                .map(|p| p.actor_id)
        } else {
            self.engine
                .world
                .players
                .iter()
                .find(|p| {
                    self.engine.actor(p.actor_id).is_ok_and(|a| {
                        crate::ui::normalized(&a.name) == crate::ui::normalized(&name)
                    })
                })
                .map(|p| p.actor_id)
        };
        let actor = if let Some(actor) = existing {
            ensure!(
                !self.connected.contains_key(&actor),
                "That character is already connected"
            );
            actor
        } else if self.engine.world.mode == "solo" {
            ensure!(
                self.connected.is_empty(),
                "This imported solo castle allows one player"
            );
            crate::model::PLAYER
        } else {
            self.engine.join_player(
                &identity.unwrap_or_else(|| format!("guest:{}", uuid::Uuid::new_v4())),
                &name,
                self.handle.info.maximum_players,
            )?
        };
        ensure!(
            !self.connected.contains_key(&actor),
            "That character is already connected"
        );
        let (sender, views) = watch::channel(json!({}));
        let session_id = self.next();
        self.sessions.insert(actor, session_id);
        self.connected.insert(actor, sender);
        let view = self.player_view(actor)?;
        self.connected.get(&actor).unwrap().send_replace(view);
        Ok(PlayerConnection {
            actor_id: actor,
            session_id,
            views,
            handle: self.handle.clone(),
        })
    }
    fn player_request(&mut self, actor: u16, method: &str, params: Value) -> Result<Value> {
        ensure!(
            self.connected.contains_key(&actor),
            "This character is not connected"
        );
        match method {
            "listener" => {
                ensure_keys(&params, &["enabled"])?;
                let enabled = params["enabled"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("Expected enabled true or false"))?;
                self.engine.set_listener(actor, enabled)?;
                if !enabled {
                    self.cancel_listener(actor);
                    self.engine.cancel_player_plan(actor)?;
                }
                Ok(json!({"ok":true}))
            }
            "cancel" => {
                ensure_keys(&params, &[])?;
                self.cancel_listener(actor);
                self.engine.cancel_player_plan(actor)?;
                Ok(json!({"ok":true}))
            }
            "input" | "act" => {
                ensure!(
                    self.error.is_none(),
                    "The castle needs attention. See the manager."
                );
                ensure!(
                    !self.engine.world.manual_pause,
                    "The castle is paused. The manager can resume it."
                );
                let (intent, combat) = if method == "input" {
                    ensure_keys(&params, &["text", "context"])?;
                    let text = params["text"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Expected room speech"))?;
                    if let Some(context) = params.get("context") {
                        ensure_keys(context, &["roomVisitId", "combat"])?;
                        ensure!(
                            context["roomVisitId"] == self.visit(actor),
                            "You changed rooms before this message arrived. Send it again."
                        );
                        ensure!(
                            context.get("combat").cloned().filter(|v| !v.is_null())
                                == self.combat(actor),
                            "The combat round changed before this message arrived"
                        );
                    }
                    (
                        json!({"kind":"speak","text":text.trim(),"targetId":null,"volume":"normal","plot":null}),
                        None,
                    )
                } else {
                    ensure_keys(&params, &["intent", "combat", "context"])?;
                    if let Some(context) = params.get("context") {
                        ensure_keys(context, &["roomVisitId", "combat"])?;
                        ensure!(
                            context["roomVisitId"] == self.visit(actor),
                            "You changed rooms before this command arrived. Send it again."
                        );
                        ensure!(
                            context.get("combat").cloned().filter(|v| !v.is_null())
                                == self.combat(actor),
                            "The combat round changed before this command arrived"
                        );
                    }
                    (
                        params["intent"].clone(),
                        params.get("combat").cloned().filter(|v| !v.is_null()),
                    )
                };
                let speech = intent["kind"] == "speak";
                if speech {
                    ensure!(
                        intent["targetId"].is_null()
                            && intent["plot"].is_null()
                            && matches!(intent["volume"].as_str(), Some("normal" | "shout")),
                        "Speech stays in the room; use /shout to call adjoining rooms"
                    );
                }
                if speech && self.enabled(actor) {
                    ensure!(
                        self.messages.iter().filter(|m| m.actor == actor).count()
                            + usize::from(self.jobs.contains_key(&(actor, true)))
                            < 16,
                        "The listener queue is full. Wait or use /cancel."
                    );
                }
                let answer = self
                    .engine
                    .dispatch(actor, intent.clone(), vec![], combat)?;
                if answer["ok"] == true {
                    if speech && self.enabled(actor) {
                        let order = self.next();
                        self.messages.push_back(Message {
                            actor,
                            text: intent["text"].as_str().unwrap_or("").into(),
                            volume: intent["volume"].as_str().unwrap_or("normal").into(),
                            event_id: answer["eventId"].as_u64().ok_or_else(|| {
                                anyhow::anyhow!("Committed speech has no event ID")
                            })?,
                            branch: self.engine.world.branch.clone(),
                            visit: self.visit(actor),
                            combat: self.combat(actor),
                            order,
                        });
                    } else if !speech {
                        self.cancel_listener(actor);
                        self.engine.clear_player_plan(actor)?;
                    }
                }
                Ok(answer)
            }
            _ => bail!("That control belongs to the host manager"),
        }
    }
    async fn manager_request(&mut self, method: &str, params: Value) -> Result<Value> {
        ensure_keys(
            &params,
            match method {
                "view" | "export" | "stop" | "recover" => &[],
                "pause" => &["paused"],
                "observe" => &["enabled"],
                "save" | "load" => &["name"],
                "select" => &["actorId"],
                "settings" => &["language", "scenario"],
                "new" => &["seed"],
                _ => bail!("Unknown manager operation"),
            },
        )?;
        match method {
            "view" => return self.manager_view(),
            "pause" => {
                self.engine.set_pause(
                    params["paused"]
                        .as_bool()
                        .ok_or_else(|| anyhow::anyhow!("Expected paused true or false"))?,
                )?;
            }
            "observe" => {
                self.observing = params["enabled"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("Expected enabled true or false"))?;
                if self.observing {
                    self.engine.set_pause(false)?;
                }
            }
            "save" => self
                .engine
                .save(optional_text(&params, "name")?.unwrap_or("quicksave"))?,
            "select" => {
                let id = params["actorId"]
                    .as_u64()
                    .and_then(|id| u16::try_from(id).ok())
                    .ok_or_else(|| anyhow::anyhow!("Invalid character ID"))?;
                self.engine.actor(id)?;
                self.selected = Some(id);
            }
            "settings" => {
                let language = optional_text(&params, "language")?
                    .unwrap_or(&self.pending_language)
                    .to_owned();
                ensure!(
                    matches!(language.as_str(), "en" | "pl"),
                    "Choose English or Polish"
                );
                let scenario = optional_text(&params, "scenario")?
                    .unwrap_or(&self.pending_scenario)
                    .to_owned();
                WorldContent::load(&scenario)?;
                let path = self.handle.info.data_dir.join("server-settings.json");
                let mut settings = std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                settings["language"] = json!(language);
                settings["scenario"] = json!(scenario);
                write_private(&path, &serde_json::to_vec(&settings)?, false)?;
                self.pending_language = language;
                self.pending_scenario = scenario;
            }
            "load" | "new" => {
                if method == "load" {
                    let name = params["name"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Choose a save"))?;
                    let candidate = self.engine.store.snapshot(name)?;
                    WorldContent::load(&candidate.world.scenario_id)?;
                    self.engine.save(if name == "before-restore" {
                        "before-restore-backup"
                    } else {
                        "before-restore"
                    })?;
                    self.engine.load(name, &self.pending_language)?;
                } else {
                    let seed = match params.get("seed") {
                        Some(value) => u32::try_from(value.as_u64().ok_or_else(|| {
                            anyhow::anyhow!("Seed must be an unsigned 32-bit number")
                        })?)?,
                        None => crate::manager::seed(),
                    };
                    WorldContent::load(&self.pending_scenario)?;
                    self.engine.save("before-retry")?;
                    self.engine
                        .new_run(&self.pending_scenario, &self.pending_language, seed)?;
                }
                self.replace_context().await?;
            }
            "recover" => {
                self.cancel_all();
                self.error = None;
                self.driver.reset().await;
                self.engine.initialize_minds()?;
                for status in self.statuses.values_mut() {
                    status.error = None;
                    status.state = "idle".into();
                }
            }
            "export" => {
                return Ok(
                    json!({"world":self.engine.world,"events":self.engine.store.events(None,i64::MAX as usize)?}),
                );
            }
            "stop" => {
                self.stopped = true;
            }
            _ => bail!("Unknown manager operation"),
        }
        Ok(json!({"ok":true}))
    }
    async fn replace_context(&mut self) -> Result<()> {
        self.cancel_all();
        self.driver.reset().await;
        for (_, sender) in std::mem::take(&mut self.connected) {
            sender.send_replace(
                json!({"closed":true,"message":"The castle was restored. Connect again."}),
            );
        }
        self.sessions.clear();
        if let Some(options) = &self.production {
            self.driver = Arc::new(CodexDriver::from_content(
                self.handle.info.data_dir.clone(),
                self.engine.content.clone(),
                &self.engine.language,
                options.clone(),
                self.recall_sender.clone(),
            ));
        }
        self.engine.set_pause(true)?;
        self.error = None;
        self.statuses.clear();
        self.last_started.clear();
        self.room_last.clear();
        self.last_idle = self.wall;
        self.engine.initialize_minds()?;
        Ok(())
    }
    pub fn player_view(&self, actor: u16) -> Result<Value> {
        let mut view = self.engine.player_view(actor)?;
        view["paused"] = json!(self.paused());
        view["listener"] = json!({"enabled":self.enabled(actor)});
        view["error"] = if self.error.is_some() {
            json!("The castle needs attention. See the manager.")
        } else {
            Value::Null
        };
        Ok(view)
    }
    fn manager_view(&self) -> Result<Value> {
        let mut view = self.engine.manager_view(self.selected)?;
        view["paused"] = json!(self.paused());
        view["model"] = json!(MODEL);
        view["effort"] = json!(EFFORT);
        view["error"] = json!(self.error);
        view["pendingLanguage"] = json!(self.pending_language);
        view["pendingScenario"] = json!(self.pending_scenario);
        view["manualPause"] = json!(self.engine.world.manual_pause);
        view["observing"] = json!(self.observing);
        view["ssh"] = json!({"host":self.handle.info.address,"port":self.handle.info.port,"command":format!("ssh -p {} {}",self.handle.info.port,self.handle.info.address),"localCommand":format!("ssh -p {} 127.0.0.1",self.handle.info.port)});
        if let Some(actors) = view["actors"].as_array_mut() {
            for actor in actors {
                let id = actor["id"].as_u64().unwrap_or(0) as u16;
                let status = self.statuses.get(&(id, false));
                let job = self.jobs.get(&(id, false));
                let state = if actor["health"].as_f64().unwrap_or(0.) <= 0. {
                    "dead"
                } else if job.is_some_and(|job| self.inflight.contains_key(&job.id)) {
                    "thinking"
                } else if job.is_some() {
                    "ready"
                } else if self.queued.contains_key(&id) {
                    "queued"
                } else if status.is_some_and(|s| s.state == "failed") {
                    "failed"
                } else if self.engine.actor(id)?.activity.is_some() {
                    "acting"
                } else {
                    "idle"
                };
                actor["connected"] = json!(self.connected.contains_key(&id));
                actor["controller"] = json!(if self.engine.is_player(id) {
                    "human"
                } else {
                    "ai"
                });
                actor["status"] = json!({"state":state,"reason":status.map(|s|s.reason.as_str()).unwrap_or(""),"calls":status.map_or(0,|s|s.calls),"tokens":status.map_or(0,|s|s.tokens),"elapsedMs":job.map_or(0.,|j|(self.wall-j.started)*1000.),"lastLatencyMs":status.and_then(|s|s.latency),"error":status.and_then(|s|s.error.as_ref())});
            }
        }
        let mut latencies = self.latencies.iter().copied().collect::<Vec<_>>();
        latencies.sort_by(f64::total_cmp);
        let percentile = |p: f64| {
            if latencies.is_empty() {
                None
            } else {
                Some(latencies[((latencies.len() as f64 * p).ceil() as usize).saturating_sub(1)])
            }
        };
        view["metrics"] = json!({"calls":self.statuses.values().map(|s|s.calls).sum::<u64>(),"tokens":self.statuses.values().map(|s|s.tokens).sum::<u64>(),"active":self.inflight.len(),"p50":percentile(0.5),"p95":percentile(0.95)});
        let selected = self.selected.unwrap_or(0);
        let status = self.statuses.get(&(selected, true));
        view["listener"] = json!({"enabled":self.enabled(selected),"state":if self.jobs.contains_key(&(selected,true)){"thinking"}else{"idle"},"summary":status.map(|s|s.summary.as_str()).unwrap_or(""),"plan":status.map(|s|s.plan.clone()).unwrap_or(json!([])),"text":status.map(|s|s.reason.as_str()).unwrap_or(""),"error":status.and_then(|s|s.error.as_ref()),"calls":status.map_or(0,|s|s.calls),"tokens":status.map_or(0,|s|s.tokens),"queued":self.messages.iter().filter(|m|m.actor==selected).count()});
        Ok(view)
    }
    fn publish(&mut self) -> Result<()> {
        let ids = self.connected.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let view = self.player_view(id)?;
            if let Some(sender) = self.connected.get(&id) {
                sender.send_replace(view);
            }
        }
        self.manager.send_replace(self.manager_view()?);
        Ok(())
    }
    pub fn tick(&mut self, elapsed: f64) -> Result<()> {
        ensure!(
            elapsed.is_finite() && elapsed >= 0.,
            "Invalid tick duration"
        );
        self.wall += elapsed;
        self.pump()?;
        if self.can_run() {
            self.engine.advance(elapsed)?;
        }
        if self.wall - self.last_idle >= self.scheduling.background_interval {
            self.last_idle = self.wall;
            self.engine.initialize_minds()?;
        }
        self.pump()?;
        self.publish()
    }
    pub async fn run(mut self) -> Result<()> {
        let mut ticker = tokio::time::interval(Duration::from_millis(250));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = Instant::now();
        while !self.stopped {
            tokio::select! {
                _=ticker.tick()=>{let now=Instant::now();let elapsed=now.duration_since(last).as_secs_f64();last=now;if let Err(error)=self.tick(elapsed){self.error=Some(safe_error(error));self.engine.set_pause(true)?;self.publish()?;}},
                Some(request)=self.requests.recv()=>{
                    let changes_clock=matches!(&request,RuntimeRequest::Join{..})||matches!(&request,RuntimeRequest::Disconnect{actor_id,session_id}if self.sessions.get(actor_id)==Some(session_id))||matches!(&request,RuntimeRequest::Manager{method,..}if matches!(method.as_str(),"pause"|"observe"|"new"|"load"|"stop"|"recover"));
                    if changes_clock {let now=Instant::now();let elapsed=now.duration_since(last).as_secs_f64();last=now;if let Err(error)=self.tick(elapsed){self.error=Some(safe_error(error));self.engine.set_pause(true)?;}}
                    match request {
                        RuntimeRequest::Join{identity,name,reply}=>{if !reply.is_closed(){let result=self.join(identity,name);if let Err(Ok(player))=reply.send(result)&&self.sessions.get(&player.actor_id)==Some(&player.session_id){self.disconnect(player.actor_id)?;}}},
                        RuntimeRequest::Player{actor_id,session_id,method,params,reply}=>{let answer=if self.sessions.get(&actor_id)==Some(&session_id){self.player_request(actor_id,&method,params)}else{Err(anyhow::anyhow!("This player connection belongs to an earlier session"))};let _=reply.send(answer);},
                        RuntimeRequest::Disconnect{actor_id,session_id}=>{if self.sessions.get(&actor_id)==Some(&session_id){self.disconnect(actor_id)?;}},
                        RuntimeRequest::Manager{method,params,reply}=>{let answer=self.manager_request(&method,params).await;let _=reply.send(answer);if changes_clock{last=Instant::now();}},
                    }
                    if let Err(error)=self.pump(){self.error=Some(safe_error(error));self.engine.set_pause(true)?;}self.publish()?;
                }
                Some(request)=self.recall_requests.recv()=>{let valid=request.branch==self.engine.world.branch && self.jobs.contains_key(&(request.actor_id,false));let answer=if valid{self.engine.recall(request.actor_id,&request.query)}else{Err(anyhow::anyhow!("This recall belongs to an obsolete decision"))};let _=request.reply.send(answer);},
                Some(completion)=self.done.recv()=>{self.inflight.remove(&completion.id);if self.can_run(){if let Err(error)=self.accept(completion){self.error=Some(safe_error(error));self.engine.set_pause(true)?;}}else if self.jobs.get(&(completion.actor,completion.listener)).is_some_and(|job|job.id==completion.id){self.deferred.push_back(completion);}self.pump()?;self.publish()?;},
            }
        }
        self.cancel_all();
        self.engine.set_pause(true)?;
        self.engine.save("autosave")?;
        for (_, sender) in self.connected {
            sender.send_replace(json!({"closed":true,"message":"The host stopped the castle."}));
        }
        self.driver.reset().await;
        Ok(())
    }
}

fn ensure_keys(value: &Value, allowed: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Expected an object"))?;
    ensure!(
        object.keys().all(|key| allowed.contains(&key.as_str())),
        "Unknown request field"
    );
    Ok(())
}
fn optional_text<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>> {
    params
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{key} must be text"))
        })
        .transpose()
}
fn gather_ids(value: &Value, ids: &mut HashSet<u16>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if (key == "id" || key.ends_with("Id"))
                    && value.as_u64().is_some_and(|id| id > 0 && id <= 65535)
                {
                    ids.insert(value.as_u64().unwrap() as u16);
                } else {
                    gather_ids(value, ids);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                gather_ids(value, ids);
            }
        }
        _ => {}
    }
}
fn referenced_ids(value: &Value) -> HashSet<u16> {
    let mut ids = HashSet::new();
    gather_ids(value, &mut ids);
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use tokio::sync::oneshot;
    type ControlledTurn = (ThinkRequest, oneshot::Sender<Result<ThinkResult>>);
    struct Controlled {
        sender: mpsc::UnboundedSender<ControlledTurn>,
    }
    impl DecisionDriver for Controlled {
        fn think(
            &self,
            request: ThinkRequest,
            cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<ThinkResult>> {
            Box::pin(async move {
                let (reply, answer) = oneshot::channel();
                self.sender
                    .send((request, reply))
                    .map_err(|_| anyhow::anyhow!("Fixture stopped"))?;
                tokio::select! {_=cancel.cancelled()=>bail!("Fixture cancelled"),answer=answer=>answer?}
            })
        }
        fn reset(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }
    fn fixture() -> (
        Simulation,
        mpsc::UnboundedReceiver<ControlledTurn>,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let (sender, requests) = mpsc::unbounded_channel();
        let engine = Engine::memory("castle", "en", 27).unwrap();
        let (simulation, _) = Simulation::with_driver(
            engine,
            HostInfo {
                data_dir: directory.path().into(),
                address: "127.0.0.1".into(),
                port: 1,
                maximum_players: 8,
            },
            Arc::new(Controlled { sender }),
        );
        (simulation, requests, directory)
    }
    fn decision(plan: Value) -> ThinkResult {
        ThinkResult {
            decision: json!({"summary":"A controlled intention.","goals":[],"plan":plan,"memories":[],"relationships":[],"acceptPlots":[]}),
            tokens: 10,
            latency_ms: 100.,
            recalled: vec![],
        }
    }
    async fn finish(sim: &mut Simulation) {
        let completion = tokio::time::timeout(Duration::from_secs(1), sim.done.recv())
            .await
            .unwrap()
            .unwrap();
        sim.inflight.remove(&completion.id);
        sim.accept(completion).unwrap();
    }
    async fn started(requests: &mut mpsc::UnboundedReceiver<ControlledTurn>) -> ControlledTurn {
        tokio::time::timeout(Duration::from_secs(1), requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
    fn only(sim: &mut Simulation, actors: &[u16]) {
        sim.queued.retain(|id, _| actors.contains(id));
    }
    fn wake(sim: &mut Simulation, id: u16, created: f64, urgent: bool) {
        let order = sim.next();
        let version = sim.engine.actor(id).unwrap().decision_version;
        sim.queued.insert(
            id,
            Queued {
                wake: Wake {
                    actor_id: id,
                    version,
                    reason: "A controlled wake.".into(),
                    urgent,
                },
                order,
                created,
                ready: sim.wall,
                attempt: 0,
                feedback: None,
            },
        );
    }

    #[tokio::test]
    async fn empty_or_paused_castles_neither_advance_nor_call_models() {
        let (mut sim, mut requests, _) = fixture();
        let time = sim.engine.world.time;
        sim.tick(50.).unwrap();
        assert_eq!(sim.engine.world.time, time);
        assert!(requests.try_recv().is_err());
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        sim.engine.set_pause(true).unwrap();
        sim.tick(100.).unwrap();
        assert_eq!(sim.engine.world.time, time);
        assert!(requests.try_recv().is_err());
        sim.engine.set_pause(false).unwrap();
        sim.tick(0.5).unwrap();
        assert!((sim.engine.world.time - time - 0.5 * crate::model::CLOCK_RATE).abs() < 1e-7);
        sim.disconnect(player.actor_id).unwrap();
        let stopped = sim.engine.world.time;
        sim.tick(20.).unwrap();
        assert_eq!(sim.engine.world.time, stopped);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn speech_batches_and_does_not_interrupt_a_valid_turn() {
        let (mut sim, mut requests, _) = fixture();
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.player_request(player.actor_id, "input", json!({"text":"Hello."}))
            .unwrap();
        sim.tick(1.5).unwrap();
        assert!(requests.try_recv().is_err());
        sim.tick(0.5).unwrap();
        let (request, reply) = started(&mut requests).await;
        assert_eq!(request.actor_id, 102);
        let id = sim.jobs[&(102, false)].id;
        let version = sim.engine.actor(102).unwrap().decision_version;
        sim.player_request(player.actor_id, "input", json!({"text":"Still here."}))
            .unwrap();
        sim.collect();
        assert_eq!(sim.jobs[&(102, false)].id, id);
        assert_eq!(sim.engine.actor(102).unwrap().decision_version, version);
        assert!(!sim.jobs[&(102, false)].cancel.is_cancelled());
        reply
            .send(Ok(decision(json!([{"kind":"wait","seconds":1800}]))))
            .unwrap();
        finish(&mut sim).await;
        assert_eq!(sim.statuses[&(102, false)].state, "idle");
        sim.cancel_all();
    }

    #[tokio::test]
    async fn routine_starts_respect_test_owned_cooldown_values() {
        for cooldown in [3., 7.] {
            let (mut sim, mut requests, _) = fixture();
            sim.scheduling.routine_cooldown = cooldown;
            let player = sim.join(None, "Fixture Guest".into()).unwrap();
            only(&mut sim, &[102]);
            sim.tick(2.).unwrap();
            let (_, reply) = started(&mut requests).await;
            reply
                .send(Ok(decision(json!([{"kind":"wait","seconds":1800}]))))
                .unwrap();
            finish(&mut sim).await;
            sim.player_request(
                player.actor_id,
                "input",
                json!({"text":"Tell me your name."}),
            )
            .unwrap();
            sim.collect();
            sim.tick(cooldown - 0.01).unwrap();
            assert!(requests.try_recv().is_err());
            sim.tick(0.02).unwrap();
            assert_eq!(started(&mut requests).await.0.actor_id, 102);
            sim.cancel_all();
        }
    }

    #[tokio::test]
    async fn current_player_rooms_get_priority_and_background_requests_age() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 1;
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        let workshop = sim
            .engine
            .world
            .actors
            .iter()
            .find(|a| a.room_id == 8 && !sim.engine.is_player(a.id))
            .unwrap()
            .id;
        sim.queued.clear();
        let now = sim.wall;
        wake(&mut sim, 102, now, false);
        wake(&mut sim, workshop, now, false);
        sim.engine.actor_mut(player.actor_id).unwrap().room_id = 8;
        sim.pump().unwrap();
        assert_eq!(started(&mut requests).await.0.actor_id, workshop);
        sim.cancel_all();
        finish(&mut sim).await;
        sim.queued.clear();
        let old = sim.wall - sim.scheduling.aging - 1.;
        wake(&mut sim, 102, old, false);
        sim.pump().unwrap();
        assert_eq!(started(&mut requests).await.0.actor_id, 102);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn room_fairness_serves_both_occupied_rooms() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 2;
        let first = sim.join(None, "Fixture One".into()).unwrap();
        let second = sim.join(None, "Fixture Two".into()).unwrap();
        sim.engine.actor_mut(second.actor_id).unwrap().room_id = 8;
        let workshop = sim
            .engine
            .world
            .actors
            .iter()
            .find(|a| a.room_id == 8 && !sim.engine.is_player(a.id))
            .unwrap()
            .id;
        sim.queued.clear();
        let now = sim.wall;
        wake(&mut sim, 102, now, false);
        wake(&mut sim, workshop, now, false);
        assert_eq!(sim.engine.actor(first.actor_id).unwrap().room_id, 1);
        sim.pump().unwrap();
        let mut actors = vec![
            started(&mut requests).await.0.actor_id,
            started(&mut requests).await.0.actor_id,
        ];
        actors.sort_unstable();
        let mut expected = vec![102, workshop];
        expected.sort_unstable();
        assert_eq!(actors, expected);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn cancelled_work_holds_capacity_until_it_finishes() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 2;
        sim.join(None, "Fixture Guest".into()).unwrap();
        sim.tick(2.).unwrap();
        let first = started(&mut requests).await;
        let _second = started(&mut requests).await;
        assert_eq!(sim.inflight.len(), 2);
        let actor = first.0.actor_id;
        sim.engine.actor_mut(actor).unwrap().decision_version += 1;
        sim.collect();
        let now = sim.wall;
        wake(&mut sim, actor, now, true);
        sim.pump().unwrap();
        assert_eq!(sim.inflight.len(), 2);
        assert!(!sim.jobs.contains_key(&(actor, false)));
        assert!(requests.try_recv().is_err());
        finish(&mut sim).await;
        sim.pump().unwrap();
        assert_eq!(started(&mut requests).await.0.actor_id, actor);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn danger_bypasses_routine_batch_and_cooldown() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.routine_cooldown = 20.;
        sim.join(None, "Fixture Guest".into()).unwrap();
        sim.queued.clear();
        sim.last_started.insert(102, sim.wall);
        let now = sim.wall;
        wake(&mut sim, 102, now, true);
        sim.pump().unwrap();
        assert_eq!(started(&mut requests).await.0.actor_id, 102);
        sim.cancel_all();
    }
    #[tokio::test]
    async fn a_cancelled_actor_cannot_overlap_its_replacement_even_with_spare_slots() {
        let (mut sim, mut requests, _) = fixture();
        sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.tick(2.).unwrap();
        let _old = started(&mut requests).await;
        assert_eq!(sim.inflight.len(), 1);
        sim.engine.actor_mut(102).unwrap().decision_version += 1;
        sim.collect();
        let now = sim.wall;
        wake(&mut sim, 102, now, true);
        sim.pump().unwrap();
        assert!(requests.try_recv().is_err());
        assert_eq!(sim.inflight.len(), 1);
        finish(&mut sim).await;
        sim.pump().unwrap();
        assert_eq!(started(&mut requests).await.0.actor_id, 102);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn initial_observation_evidence_is_allowed_without_a_recall_call() {
        let (mut sim, mut requests, _) = fixture();
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.tick(2.).unwrap();
        let (request, reply) = started(&mut requests).await;
        let evidence = request.input["memories"]
            .as_array()
            .unwrap()
            .first()
            .unwrap()["id"]
            .as_u64()
            .unwrap();
        let mut result = decision(json!([{"kind":"wait","seconds":1800}]));
        result.decision["relationships"] = json!([{"targetId":player.actor_id,"trust":1,"affection":0,"fear":0,"respect":0,"evidenceIds":[evidence]}]);
        reply.send(Ok(result)).unwrap();
        finish(&mut sim).await;
        assert_eq!(sim.statuses[&(102, false)].state, "idle");
        assert_eq!(
            sim.engine.actor(102).unwrap().relationships[&player.actor_id].trust,
            1.
        );
        sim.cancel_all();
    }

    #[tokio::test]
    async fn auto_is_per_player_bounded_and_invisible() {
        let (mut sim, mut requests, _) = fixture();
        let first = sim.join(None, "Fixture One".into()).unwrap();
        let second = sim.join(None, "Fixture Two".into()).unwrap();
        sim.player_request(first.actor_id, "listener", json!({"enabled":true}))
            .unwrap();
        sim.player_request(
            second.actor_id,
            "input",
            json!({"text":"Only ordinary speech."}),
        )
        .unwrap();
        assert!(sim.messages.is_empty());
        for i in 0..16 {
            sim.player_request(
                first.actor_id,
                "input",
                json!({"text":format!("A fixture statement {i}.")}),
            )
            .unwrap();
        }
        let events = sim.engine.store.events(None, 1000).unwrap().len();
        assert!(
            sim.player_request(
                first.actor_id,
                "input",
                json!({"text":"Rejected overflow speech."})
            )
            .is_err()
        );
        assert_eq!(sim.engine.store.events(None, 1000).unwrap().len(), events);
        sim.pump().unwrap();
        let (request, reply) = started(&mut requests).await;
        assert!(request.listener);
        assert_eq!(request.actor_id, first.actor_id);
        assert!(request.input.get("world").is_some());
        reply
            .send(Ok(ThinkResult {
                decision: json!({"summary":"No physical request.","plan":[]}),
                tokens: 10,
                latency_ms: 100.,
                recalled: vec![],
            }))
            .unwrap();
        finish(&mut sim).await;
        let view = sim.player_view(first.actor_id).unwrap();
        assert_eq!(view["listener"], json!({"enabled":true}));
        assert!(!view.to_string().contains("No physical request"));
        assert!(
            !sim.player_view(second.actor_id).unwrap()["listener"]["enabled"]
                .as_bool()
                .unwrap()
        );
        sim.cancel_all();
    }

    #[tokio::test]
    async fn manual_actions_keep_the_new_activity_and_cancel_listener_followups() {
        let (mut sim, _, _) = fixture();
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        sim.player_request(player.actor_id, "listener", json!({"enabled":true}))
            .unwrap();
        sim.player_request(player.actor_id, "input", json!({"text":"I wait here."}))
            .unwrap();
        assert_eq!(sim.messages.len(), 1);
        let result = sim
            .player_request(
                player.actor_id,
                "act",
                json!({"intent":{"kind":"wait","seconds":600},"combat":null}),
            )
            .unwrap();
        assert_eq!(result["ok"], true);
        assert!(sim.messages.is_empty());
        assert!(matches!(
            sim.engine
                .actor(player.actor_id)
                .unwrap()
                .activity
                .as_ref()
                .unwrap()
                .intent,
            crate::model::Intent::Wait { seconds: 600 }
        ));
        assert!(sim.engine.actor(player.actor_id).unwrap().plan.is_empty());
    }

    #[tokio::test]
    async fn stale_listener_results_cannot_act_after_a_room_change() {
        let (mut sim, mut requests, _) = fixture();
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        sim.player_request(player.actor_id, "listener", json!({"enabled":true}))
            .unwrap();
        sim.player_request(player.actor_id, "input", json!({"text":"I wait here."}))
            .unwrap();
        sim.pump().unwrap();
        let (_, reply) = started(&mut requests).await;
        sim.engine
            .actor_mut(player.actor_id)
            .unwrap()
            .entered_event_id += 1;
        reply
            .send(Ok(ThinkResult {
                decision: json!({"summary":"Wait.","plan":[{"kind":"wait","seconds":600}]}),
                tokens: 1,
                latency_ms: 1.,
                recalled: vec![],
            }))
            .unwrap();
        finish(&mut sim).await;
        assert!(
            sim.engine
                .actor(player.actor_id)
                .unwrap()
                .activity
                .is_none()
        );
        sim.cancel_all();
    }

    #[tokio::test]
    async fn settings_persist_network_and_reject_unknown_options() {
        let (mut sim, _, directory) = fixture();
        let path = directory.path().join("server-settings.json");
        write_private(&path, b"{\"bind\":\"127.0.0.1\",\"port\":2224}", false).unwrap();
        sim.manager_request("settings", json!({"language":"pl","scenario":"siege"}))
            .await
            .unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["port"], 2224);
        assert_eq!(saved["bind"], "127.0.0.1");
        assert_eq!(saved["language"], "pl");
        assert!(
            sim.manager_request("settings", json!({"speed":99}))
                .await
                .is_err()
        );
        assert_eq!(sim.engine.language, "en");
    }
}
