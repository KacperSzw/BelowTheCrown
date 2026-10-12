use crate::{
    codex::{
        CodexDriver, DecisionDriver, DecisionToken, RecallRequest, ThinkRequest, ThinkResult,
        WorkerOptions, safe_error,
    },
    content::WorldContent,
    engine::Engine,
    model::{EFFORT, MODEL, Wake, WakeCause, WakePriority},
    paths::write_private,
    runtime::{GameHandle, HostInfo, PlayerConnection, RuntimeRequest},
};
use anyhow::{Context, Result, bail, ensure};
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
    pub npc_update_rate: u8,
    pub background_interval: f64,
    pub aging: f64,
    pub timeout: Duration,
}
impl Default for Scheduling {
    fn default() -> Self {
        Self {
            maximum_calls: 5,
            npc_update_rate: 5,
            background_interval: 60.,
            aging: 30.,
            timeout: Duration::from_secs(120),
        }
    }
}
impl Scheduling {
    pub fn set_npc_update_rate(&mut self, rate: u8) -> Result<()> {
        ensure!(
            matches!(rate, 1 | 5 | 15),
            "NPC update rate must be 1, 5 or 15"
        );
        self.npc_update_rate = rate;
        self.background_interval = 300. / f64::from(rate);
        Ok(())
    }
}

#[derive(Clone)]
struct Queued {
    wake: Wake,
    order: u64,
    created: f64,
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
    created: f64,
}
struct Job {
    id: u64,
    generation: u64,
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
    queued_ms: f64,
    duration_ms: f64,
    generation: u64,
}
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Timing {
    queue_ms: f64,
    inference_ms: f64,
    cleanup_ms: f64,
    total_ms: f64,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Failure {
    message: String,
    time: f64,
    phase: &'static str,
}
#[derive(Default)]
struct Status {
    calls: u64,
    tokens: u64,
    latency: Option<f64>,
    error: Option<String>,
    last_failure: Option<Failure>,
    reason: String,
    state: String,
    summary: String,
    plan: Value,
    timing: Option<Timing>,
    cancellations: u64,
    rejections: u64,
    execution_failures: u64,
    seen_failure: Option<Value>,
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
    background_inflight: HashSet<u64>,
    deferred: VecDeque<Completion>,
    messages: VecDeque<Message>,
    statuses: HashMap<(u16, bool), Status>,
    room_last: HashMap<u16, u64>,
    sequence: u64,
    context_generation: u64,
    wall: f64,
    last_idle: f64,
    observing: bool,
    selected: Option<u16>,
    pending_language: String,
    pending_scenario: String,
    error: Option<String>,
    stopped: bool,
    latencies: VecDeque<(bool, Timing)>,
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
        let last_idle = engine.world.interaction_time;
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
            background_inflight: HashSet::new(),
            deferred: VecDeque::new(),
            messages: VecDeque::new(),
            statuses: HashMap::new(),
            room_last: HashMap::new(),
            sequence: 0,
            context_generation: 0,
            wall: 0.,
            last_idle,
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
        job.generation == self.context_generation
            && job.branch == self.engine.world.branch
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
                let new_conversation = wake.cause == WakeCause::External;
                queued.wake.merge(wake);
                if new_conversation {
                    queued.attempt = 0;
                    queued.feedback = None;
                }
            } else {
                let order = self.next();
                self.queued.insert(
                    id,
                    Queued {
                        wake,
                        order,
                        created: self.wall,
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
                self.statuses.entry(key).or_default().cancellations += 1;
                if !job.listener
                    && job.branch == self.engine.world.branch
                    && self.engine.actor(job.actor).is_ok_and(|a| a.awake())
                    && let Some(mut queued) = job
                        .queued
                        .filter(|q| q.wake.priority == WakePriority::Conversation)
                {
                    if let Some(current) = self.queued.get_mut(&job.actor) {
                        queued.wake.merge(current.wake.clone());
                        current.wake = queued.wake;
                    } else {
                        self.queued.insert(job.actor, queued);
                    }
                }
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
        for actor in &self.engine.world.actors {
            if let Some(failure) = &actor.last_plan_failure {
                let value = json!(failure);
                let status = self.statuses.entry((actor.id, false)).or_default();
                if status.seen_failure.as_ref() != Some(&value) {
                    status.execution_failures += 1;
                    status.seen_failure = Some(value);
                    status.last_failure = Some(Failure {
                        message: safe_error(&failure.reason),
                        time: failure.time,
                        phase: "plan_execution",
                    });
                }
            }
        }
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
                {
                    continue;
                }
                let danger = q.wake.urgent() || self.combat(*id).is_some();
                if !danger
                    && self
                        .engine
                        .actor(*id)?
                        .activity
                        .as_ref()
                        .is_some_and(|a| !a.intent.long())
                {
                    continue;
                }
                let conversation = q.wake.priority == WakePriority::Conversation;
                let foreground = self.foreground(*id);
                let aged = self.wall - q.created >= self.scheduling.aging;
                if !foreground
                    && !danger
                    && !conversation
                    && self.background_inflight.len()
                        >= self.scheduling.maximum_calls.saturating_sub(1).max(1)
                {
                    continue;
                }
                let priority = if danger {
                    0
                } else if conversation {
                    2
                } else if foreground || aged {
                    3
                } else {
                    4
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
                let actor = self.engine.actor(m.actor)?;
                if self.jobs.contains_key(&(m.actor, true))
                    || self.inflight.values().any(|key| *key == (m.actor, true))
                    || actor.activity.as_ref().is_some_and(|a| !a.intent.long())
                    || (actor.activity.is_none() && !actor.plan.is_empty())
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
        let id = self.next();
        if !self.foreground(actor)
            && !q.wake.urgent()
            && q.wake.priority != WakePriority::Conversation
            && self.combat(actor).is_none()
        {
            self.background_inflight.insert(id);
        }
        let request = ThinkRequest {
            actor_id: actor,
            branch: self.engine.world.branch.clone(),
            input,
            listener: false,
            identity: Some(self.engine.npc_identity(actor)?),
            token: DecisionToken {
                job_id: id,
                context_generation: self.context_generation,
            },
        };
        let cancel = CancellationToken::new();
        let job = Job {
            id,
            generation: self.context_generation,
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
        let full = self.player_view(actor)?;
        let mut view = json!({});
        for field in [
            "self",
            "room",
            "doors",
            "actors",
            "items",
            "inventory",
            "actions",
            "combat",
            "map",
            "language",
            "time",
        ] {
            if let Some(value) = full.get(field) {
                view[field] = value.clone();
            }
        }
        if let Some(own) = view["self"].as_object_mut() {
            own.remove("portraitKey");
        }
        // Command discovery includes blocked doors; model suggestions stay executable.
        if let Some(actions) = view["actions"].as_array_mut() {
            actions.retain(|action| action["available"].as_bool() != Some(false));
        }
        let mut allowed = HashSet::from([actor]);
        gather_ids(&view, &mut allowed);
        let id = self.next();
        let request = ThinkRequest {
            actor_id: actor,
            branch: message.branch.clone(),
            listener: true,
            identity: None,
            token: DecisionToken {
                job_id: id,
                context_generation: self.context_generation,
            },
            input: json!({"text":message.text,"channel":{"targetId":null,"volume":message.volume},"world":view}),
        };
        let cancel = CancellationToken::new();
        let job = Job {
            id,
            generation: self.context_generation,
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
        let job = &self.jobs[&(request.actor_id, request.listener)];
        let created = job
            .queued
            .as_ref()
            .map(|q| q.created)
            .or_else(|| job.message.as_ref().map(|m| m.created))
            .unwrap_or(self.wall);
        let queued_ms = (self.wall - created).max(0.) * 1000.;
        tokio::spawn(async move {
            let started = Instant::now();
            let actor = request.actor_id;
            let listener = request.listener;
            let generation = request.token.context_generation;
            let mut turn = driver.think(request, cancel.clone());
            let result = tokio::select! {
                result=&mut turn=>result,
                _=tokio::time::sleep(timeout)=>{cancel.cancel();let _=turn.await;Err(anyhow::anyhow!("The decision timed out"))},
            };
            let _ = sender
                .send(Completion {
                    id,
                    actor,
                    listener,
                    result,
                    queued_ms,
                    duration_ms: started.elapsed().as_secs_f64() * 1000.,
                    generation,
                })
                .await;
        });
    }
    fn retire_slot(&mut self, completion: &Completion) {
        self.inflight.remove(&completion.id);
        self.background_inflight.remove(&completion.id);
        if completion.generation != self.context_generation {
            return;
        }
        if let Ok(result) = &completion.result {
            let timing = Timing {
                queue_ms: completion.queued_ms,
                inference_ms: result.latency_ms,
                cleanup_ms: result.cleanup_ms,
                total_ms: completion.queued_ms + completion.duration_ms,
            };
            self.statuses
                .entry((completion.actor, completion.listener))
                .or_default()
                .timing = Some(timing);
            self.latencies.push_back((completion.listener, timing));
            if self.latencies.len() > 200 {
                self.latencies.pop_front();
            }
        }
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
        let failure_phase = match &completion.result {
            Err(error) if error.is::<crate::codex::DecisionOutputError>() => "decision_validation",
            Err(_) => "inference",
            Ok(_) => "decision_validation",
        };
        let result = match completion.result {
            Ok(result) => {
                let status = self.statuses.entry(key).or_default();
                status.tokens += result.tokens;
                status.latency = Some(result.latency_ms);
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
                status.last_failure = Some(Failure {
                    message: error.clone(),
                    time: self.engine.world.time,
                    phase: failure_phase,
                });
                status.state = "failed".into();
                status.rejections += 1;
                if let Some(mut q) = job.queued.filter(|q| q.attempt < 1)
                    && self.engine.claim_correction(job.actor)?
                {
                    q.attempt += 1;
                    q.feedback = Some(error);
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
            self.statuses
                .entry((actor, true))
                .or_default()
                .cancellations += 1;
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
            self.statuses
                .entry((job.actor, job.listener))
                .or_default()
                .cancellations += 1;
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
                        json!({"kind":"speak","text":text.trim(),"targetId":null,"volume":crate::model::player_speech_volume(text),"plot":null}),
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
                        "Speech must be normal or shouted, without a private recipient or plot"
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
                            created: self.wall,
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
                "settings" => &["language", "scenario", "maxAgents", "npcUpdateRate"],
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
                let maximum = match params.get("maxAgents") {
                    Some(value) => value
                        .as_u64()
                        .filter(|n| (1..=32).contains(n))
                        .ok_or_else(|| anyhow::anyhow!("Maximum agents must be between 1 and 32"))?
                        as usize,
                    None => self.scheduling.maximum_calls,
                };
                let npc_rate = match params.get("npcUpdateRate") {
                    Some(value) => value
                        .as_u64()
                        .filter(|n| matches!(n, 1 | 5 | 15))
                        .context("NPC update rate must be 1, 5 or 15")?
                        as u8,
                    None => self.scheduling.npc_update_rate,
                };
                let path = self.handle.info.data_dir.join("server-settings.json");
                let mut settings = match std::fs::read(&path) {
                    Ok(bytes) => {
                        ensure!(bytes.len() <= 65536, "Server settings are too large");
                        let value: Value = serde_json::from_slice(&bytes)?;
                        ensure!(value.is_object(), "Invalid server settings");
                        value
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
                    Err(error) => return Err(error.into()),
                };
                settings["language"] = json!(language);
                settings["scenario"] = json!(scenario);
                settings["maxAgents"] = json!(maximum);
                settings["npcUpdateRate"] = json!(npc_rate);
                write_private(&path, &serde_json::to_vec(&settings)?, false)?;
                self.pending_language = language;
                self.pending_scenario = scenario;
                self.scheduling.maximum_calls = maximum;
                if npc_rate != self.scheduling.npc_update_rate {
                    self.scheduling.set_npc_update_rate(npc_rate)?;
                    self.last_idle = self.engine.world.interaction_time;
                }
                self.pump()?;
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
                self.context_generation = self
                    .context_generation
                    .checked_add(1)
                    .context("Decision generation exhausted")?;
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
        self.last_idle = self.engine.world.interaction_time;
        self.context_generation = self
            .context_generation
            .checked_add(1)
            .context("Decision generation exhausted")?;
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
        self.latencies.clear();
        self.room_last.clear();
        self.engine.initialize_minds()?;
        Ok(())
    }
    fn recall_for_job(&self, request: &RecallRequest) -> Result<Value> {
        ensure!(
            request.branch == self.engine.world.branch
                && request.token.context_generation == self.context_generation
                && self
                    .jobs
                    .get(&(request.actor_id, false))
                    .is_some_and(|job| job.id == request.token.job_id
                        && self.job_current(job)
                        && !job.cancel.is_cancelled()),
            "This recall belongs to an obsolete decision"
        );
        self.engine.recall(request.actor_id, &request.query)
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
        view["maxAgents"] = json!(self.scheduling.maximum_calls);
        view["npcUpdateRate"] = json!(self.scheduling.npc_update_rate);
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
                actor["status"] = json!({"state":state,"reason":status.map(|s|s.reason.as_str()).unwrap_or(""),"calls":status.map_or(0,|s|s.calls),"tokens":status.map_or(0,|s|s.tokens),"elapsedMs":job.map_or(0.,|j|(self.wall-j.started)*1000.),"lastLatencyMs":status.and_then(|s|s.latency),"timing":status.and_then(|s|s.timing),"cancellations":status.map_or(0,|s|s.cancellations),"rejections":status.map_or(0,|s|s.rejections),"executionFailures":status.map_or(0,|s|s.execution_failures),"error":status.and_then(|s|s.error.as_ref()),"lastFailure":status.and_then(|s|s.last_failure.as_ref())});
            }
        }
        view["metrics"] = self.metrics(None);
        view["metrics"]["npc"] = self.metrics(Some(false));
        view["metrics"]["listener"] = self.metrics(Some(true));
        let selected = self.selected.unwrap_or(0);
        let status = self.statuses.get(&(selected, true));
        view["listener"] = json!({"enabled":self.enabled(selected),"state":if self.jobs.contains_key(&(selected,true)){"thinking"}else{"idle"},"summary":status.map(|s|s.summary.as_str()).unwrap_or(""),"plan":status.map(|s|s.plan.clone()).unwrap_or(json!([])),"text":status.map(|s|s.reason.as_str()).unwrap_or(""),"error":status.and_then(|s|s.error.as_ref()),"calls":status.map_or(0,|s|s.calls),"tokens":status.map_or(0,|s|s.tokens),"queued":self.messages.iter().filter(|m|m.actor==selected).count()});
        Ok(view)
    }
    fn metrics(&self, listener: Option<bool>) -> Value {
        let statuses = self
            .statuses
            .iter()
            .filter(|((_, kind), _)| listener.is_none_or(|filter| *kind == filter))
            .map(|(_, status)| status)
            .collect::<Vec<_>>();
        let samples = self
            .latencies
            .iter()
            .filter(|(kind, _)| listener.is_none_or(|filter| *kind == filter))
            .map(|(_, timing)| timing)
            .collect::<Vec<_>>();
        let distribution = |select: fn(&Timing) -> f64| {
            let mut values = samples
                .iter()
                .map(|timing| select(timing))
                .collect::<Vec<_>>();
            values.sort_by(f64::total_cmp);
            let percentile = |p: f64| {
                values
                    .get(((values.len() as f64 * p).ceil() as usize).saturating_sub(1))
                    .copied()
            };
            json!({"p50":percentile(0.5),"p95":percentile(0.95)})
        };
        let inference = distribution(|t| t.inference_ms);
        json!({"calls":statuses.iter().map(|s| s.calls).sum::<u64>(),"tokens":statuses.iter().map(|s| s.tokens).sum::<u64>(),
            "active":self.inflight.values().filter(|(_, kind)| listener.is_none_or(|filter| *kind == filter)).count(),
            "cancellations":statuses.iter().map(|s| s.cancellations).sum::<u64>(),"rejections":statuses.iter().map(|s| s.rejections).sum::<u64>(),
            "executionFailures":statuses.iter().map(|s| s.execution_failures).sum::<u64>(),"samples":samples.len(),
            "p50":inference["p50"],"p95":inference["p95"],"queueMs":distribution(|t| t.queue_ms),"inferenceMs":inference,
            "cleanupMs":distribution(|t| t.cleanup_ms),"totalMs":distribution(|t| t.total_ms)})
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
        if self.can_run()
            && self.engine.world.interaction_time - self.last_idle
                >= self.scheduling.background_interval
        {
            self.last_idle = self.engine.world.interaction_time;
            let idle: Vec<_> = self
                .engine
                .world
                .actors
                .iter()
                .filter(|actor| {
                    !self.engine.is_player(actor.id)
                        && actor.awake()
                        && actor.activity.is_none()
                        && actor.plan.is_empty()
                        && !self.queued.contains_key(&actor.id)
                        && !self.inflight.values().any(|key| *key == (actor.id, false))
                })
                .map(|actor| actor.id)
                .collect();
            for actor in idle {
                self.engine.wake_idle(actor)?;
            }
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
                Some(request)=self.recall_requests.recv()=>{let answer=self.recall_for_job(&request);let _=request.reply.send(answer);},
                Some(completion)=self.done.recv()=>{self.retire_slot(&completion);if self.can_run(){if let Err(error)=self.accept(completion){self.error=Some(safe_error(error));self.engine.set_pause(true)?;}}else if self.jobs.get(&(completion.actor,completion.listener)).is_some_and(|job|job.id==completion.id){self.deferred.push_back(completion);}self.pump()?;self.publish()?;},
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
            model_output: None,
            decision: json!({"summary":"A controlled intention.","goals":[],"plan":plan,"memories":[],"relationships":[],"acceptPlots":[]}),
            tokens: 10,
            latency_ms: 100.,
            cleanup_ms: 0.,
            recalled: vec![],
        }
    }
    async fn finish(sim: &mut Simulation) {
        let completion = tokio::time::timeout(Duration::from_secs(1), sim.done.recv())
            .await
            .unwrap()
            .unwrap();
        sim.retire_slot(&completion);
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
                    priority: if urgent {
                        WakePriority::Danger
                    } else {
                        WakePriority::Routine
                    },
                    cause: WakeCause::External,
                },
                order,
                created,
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
    async fn player_speech_supersedes_inflight_routine_and_conversation_answers() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 1;
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.pump().unwrap();
        let (_, routine) = started(&mut requests).await;
        let stale = sim.jobs[&(102, false)].cancel.clone();
        routine.send(Ok(decision(json!([
            {"kind":"speak","text":"An obsolete routine warning.","targetId":null,"volume":"normal","plot":null},
            {"kind":"wait","seconds":1200}
        ])))).unwrap();
        sim.player_request(player.actor_id, "input", json!({"text":"Hello."}))
            .unwrap();
        sim.pump().unwrap();
        assert!(stale.is_cancelled());
        assert!(!sim.jobs.contains_key(&(102, false)));
        assert_eq!(sim.inflight.len(), 1);
        finish(&mut sim).await;
        sim.pump().unwrap();
        let (hello, obsolete) = started(&mut requests).await;
        assert!(
            hello.input["execution"]["reason"]
                .as_str()
                .unwrap()
                .contains("Hello.")
        );
        obsolete.send(Ok(decision(json!([
            {"kind":"speak","text":"An obsolete conversational answer.","targetId":null,"volume":"normal","plot":null}
        ])))).unwrap();
        sim.player_request(
            player.actor_id,
            "input",
            json!({"text":"Actually, I'm Halina."}),
        )
        .unwrap();
        sim.pump().unwrap();
        finish(&mut sim).await;
        sim.pump().unwrap();
        let (latest, reply) = started(&mut requests).await;
        assert!(
            latest.input["execution"]["reason"]
                .as_str()
                .unwrap()
                .contains("Halina")
        );
        assert_eq!(
            sim.wall, 0.,
            "Fresh conversation starts as soon as obsolete work releases its slot"
        );
        reply.send(Ok(decision(json!([
            {"kind":"speak","text":"Halina? The register says otherwise.","targetId":null,"volume":"normal","plot":null},
            {"kind":"wait","seconds":1200}
        ])))).unwrap();
        finish(&mut sim).await;
        let speech: Vec<_> = sim
            .engine
            .store
            .events(Some(player.actor_id), 100)
            .unwrap()
            .into_iter()
            .filter(|e| e.actor_id == Some(102) && e.kind == "speech")
            .collect();
        assert_eq!(speech.len(), 1);
        assert!(speech[0].text.contains("Halina?"));
        sim.cancel_all();
    }

    #[tokio::test]
    async fn routine_decisions_start_and_repeat_without_advancing_the_clock() {
        for started_at in [0., 7.25] {
            let (mut sim, mut requests, _) = fixture();
            sim.scheduling.maximum_calls = 1;
            sim.join(None, "Fixture Guest".into()).unwrap();
            only(&mut sim, &[102]);
            sim.tick(started_at).unwrap();
            let (request, mut reply) = started(&mut requests).await;
            assert_eq!(request.actor_id, 102);
            for _ in 0..2 {
                reply
                    .send(Ok(decision(json!([{"kind":"wait","seconds":1800}]))))
                    .unwrap();
                finish(&mut sim).await;
                sim.engine.wake_idle(102).unwrap();
                sim.pump().unwrap();
                let (request, next_reply) = started(&mut requests).await;
                assert_eq!(request.actor_id, 102);
                assert_eq!(sim.wall, started_at);
                assert_eq!(sim.inflight.len(), 1);
                reply = next_reply;
            }
            sim.cancel_all();
        }
    }

    #[tokio::test]
    async fn pending_conversation_survives_physical_completion_and_routine_wakes() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 1;
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        sim.queued.clear();
        sim.engine
            .dispatch(102, json!({"kind":"wait","seconds":60}), vec![], None)
            .unwrap();
        let physical = json!(sim.engine.actor(102).unwrap().activity);
        sim.player_request(
            player.actor_id,
            "input",
            json!({"text":"My name is Halina."}),
        )
        .unwrap();
        sim.collect();
        assert_eq!(json!(sim.engine.actor(102).unwrap().activity), physical);
        sim.engine.advance(60. / crate::model::CLOCK_RATE).unwrap();
        sim.engine.wake_idle(102).unwrap();
        sim.collect();
        let queued = &sim.queued[&102];
        assert_eq!(queued.wake.priority, WakePriority::Conversation);
        assert!(queued.wake.reason.contains("Halina"));
        sim.pump().unwrap();
        let (request, _) = started(&mut requests).await;
        assert!(
            request.input["execution"]["reason"]
                .as_str()
                .unwrap()
                .contains("Halina")
        );
        assert!(
            request.input["world"]["roomContext"]["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["text"].as_str().unwrap().contains("Halina"))
        );
        sim.cancel_all();
    }

    #[tokio::test]
    async fn physical_completion_can_start_thinking_without_advancing_scheduler_time() {
        for seconds in [240, 960] {
            let (mut sim, mut requests, _) = fixture();
            sim.scheduling.maximum_calls = 1;
            sim.join(None, "Fixture Guest".into()).unwrap();
            only(&mut sim, &[102]);
            sim.pump().unwrap();
            let (_, reply) = started(&mut requests).await;
            reply
                .send(Ok(decision(json!([{"kind":"wait","seconds":seconds}]))))
                .unwrap();
            finish(&mut sim).await;
            let real = sim.wall;
            let calendar = sim.engine.world.time;
            sim.engine
                .advance(f64::from(seconds) / crate::model::CLOCK_RATE)
                .unwrap();
            assert!((sim.engine.world.time - calendar - f64::from(seconds)).abs() < 1e-7);
            assert_eq!(sim.wall, real);
            sim.pump().unwrap();
            let (request, _) = started(&mut requests).await;
            assert_eq!(request.actor_id, 102);
            assert_eq!(sim.wall, real);
            assert!(sim.engine.actor(102).unwrap().activity.is_none());
            sim.cancel_all();
        }
    }

    #[tokio::test]
    async fn idle_refresh_does_not_create_a_second_request_while_a_mind_is_thinking() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 1;
        sim.scheduling.background_interval = 5.;
        sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.tick(2.).unwrap();
        let (request, _) = started(&mut requests).await;
        assert_eq!(request.actor_id, 102);
        sim.tick(6.).unwrap();
        assert!(!sim.queued.contains_key(&102));
        assert!(sim.jobs.contains_key(&(102, false)));
        assert!(requests.try_recv().is_err());
        sim.cancel_all();
    }

    #[tokio::test]
    async fn idle_refresh_starts_thinking_as_soon_as_its_interval_elapses() {
        for interval in [1., 7.] {
            let (mut sim, mut requests, _) = fixture();
            sim.scheduling.maximum_calls = 1;
            sim.scheduling.background_interval = interval;
            sim.join(None, "Fixture Guest".into()).unwrap();
            sim.collect();
            sim.queued.clear();
            sim.tick(interval / 2.).unwrap();
            assert!(requests.try_recv().is_err());
            assert!(sim.inflight.is_empty());
            sim.tick(interval / 2.).unwrap();
            assert_eq!(started(&mut requests).await.0.actor_id, 102);
            assert_eq!(sim.wall, interval);
            assert_eq!(sim.inflight.len(), 1);
            sim.cancel_all();
        }
    }

    #[tokio::test]
    async fn a_burst_of_speech_preserves_dialogue_and_starts_only_the_latest_observation() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 1;
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        for text in ["Hello.", "I'm Halina.", "Did you hear me?"] {
            sim.player_request(player.actor_id, "input", json!({"text":text}))
                .unwrap();
            sim.collect();
        }
        sim.pump().unwrap();
        let (request, _) = started(&mut requests).await;
        assert!(
            request.input["execution"]["reason"]
                .as_str()
                .unwrap()
                .contains("Did you hear me?")
        );
        let dialogue: Vec<_> = request.input["world"]["roomContext"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["text"].as_str().unwrap())
            .collect();
        assert!(dialogue[dialogue.len() - 3].contains("Hello."));
        assert!(dialogue[dialogue.len() - 2].contains("Halina"));
        assert!(dialogue[dialogue.len() - 1].contains("Did you hear me?"));
        assert!(requests.try_recv().is_err());
        assert_eq!(sim.inflight.len(), 1);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn conversational_validation_failure_gets_one_immediate_correction() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 1;
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.player_request(
            player.actor_id,
            "input",
            json!({"text":"Will you give me your key?"}),
        )
        .unwrap();
        sim.pump().unwrap();
        let (_, first) = started(&mut requests).await;
        let bad = json!([{"kind":"take","itemId":1001}]);
        first.send(Ok(decision(bad.clone()))).unwrap();
        finish(&mut sim).await;
        sim.pump().unwrap();
        let (retry, second) = started(&mut requests).await;
        assert!(
            retry.input["execution"]["reason"]
                .as_str()
                .unwrap()
                .contains("rejected")
        );
        assert_eq!(sim.wall, 0.);
        second.send(Ok(decision(bad))).unwrap();
        finish(&mut sim).await;
        sim.pump().unwrap();
        assert!(!sim.queued.contains_key(&102));
        assert!(sim.inflight.is_empty());
        assert!(requests.try_recv().is_err());
        assert_eq!(sim.statuses[&(102, false)].state, "failed");
        assert_eq!(sim.statuses[&(102, false)].calls, 2);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn routine_validation_failure_gets_one_immediate_correction() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 1;
        sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.pump().unwrap();
        let (_, first) = started(&mut requests).await;
        let bad = json!([{"kind":"take","itemId":1001}]);
        first.send(Ok(decision(bad.clone()))).unwrap();
        finish(&mut sim).await;
        sim.pump().unwrap();
        let (retry, second) = started(&mut requests).await;
        assert_eq!(retry.actor_id, 102);
        assert!(
            retry.input["execution"]["reason"]
                .as_str()
                .unwrap()
                .contains("rejected")
        );
        assert_eq!(sim.wall, 0.);
        second.send(Ok(decision(bad))).unwrap();
        finish(&mut sim).await;
        sim.pump().unwrap();
        assert!(sim.queued.is_empty());
        assert!(sim.inflight.is_empty());
        assert!(requests.try_recv().is_err());
        assert_eq!(sim.statuses[&(102, false)].state, "failed");
        assert_eq!(sim.statuses[&(102, false)].calls, 2);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn manager_retains_validation_failure_after_retry_and_success() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 1;
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.pump().unwrap();
        let (_, first) = started(&mut requests).await;
        first
            .send(Ok(decision(
                json!([{"kind":"take","itemId":1001,"quantity":1}]),
            )))
            .unwrap();
        finish(&mut sim).await;
        let failure = sim.manager_view().unwrap()["actors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == 102)
            .unwrap()["status"]["lastFailure"]
            .clone();
        assert_eq!(failure["phase"], "decision_validation");
        assert!(!failure["message"].as_str().unwrap().is_empty());
        assert_eq!(failure["time"], sim.engine.world.time);
        sim.pump().unwrap();
        let (_, second) = started(&mut requests).await;
        assert!(sim.statuses[&(102, false)].error.is_none());
        assert_eq!(json!(sim.statuses[&(102, false)].last_failure), failure);
        second.send(Ok(decision(json!([])))).unwrap();
        finish(&mut sim).await;
        assert_eq!(sim.statuses[&(102, false)].state, "idle");
        assert!(sim.statuses[&(102, false)].error.is_none());
        assert_eq!(json!(sim.statuses[&(102, false)].last_failure), failure);
        let view = sim.player_view(player.actor_id).unwrap();
        assert!(
            view["actors"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a.get("status").is_none())
        );
        assert!(!view.to_string().contains("\"lastFailure\""));
        sim.cancel_all();
    }

    #[tokio::test]
    async fn input_classifies_player_shouts_and_preserves_actual_words() {
        use crate::model::Volume;
        let (mut sim, _, _) = fixture();
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        for (text, expected) in [
            ("MIKOŁAJ TUTAJ DZIEDZINIEC", Volume::Shout),
            ("Pomocy!!", Volume::Shout),
            ("Pomocy!!  ", Volume::Shout),
            ("Pomocy!", Volume::Normal),
            ("Mikołaj, TUTAJ", Volume::Normal),
            ("1234!!!", Volume::Shout),
            ("1234", Volume::Normal),
            ("I", Volume::Normal),
        ] {
            let result = sim
                .player_request(player.actor_id, "input", json!({"text":text}))
                .unwrap();
            assert_eq!(result["ok"], true);
            let event = sim
                .engine
                .store
                .observed_event(player.actor_id, result["eventId"].as_u64().unwrap())
                .unwrap()
                .unwrap();
            assert!(event.text.contains(text.trim()));
            assert!(
                matches!(event.details, Some(crate::model::EventDetails::Speech { volume, .. }) if volume == expected)
            );
        }
        sim.cancel_all();
    }

    #[tokio::test]
    async fn three_room_npcs_and_an_enabled_listener_can_think_concurrently() {
        for auto in [false, true] {
            let (mut sim, mut requests, _) = fixture();
            sim.scheduling.maximum_calls = 4;
            let player = sim.join(None, "Fixture Guest".into()).unwrap();
            let room = sim.engine.actor(player.actor_id).unwrap().room_id;
            let npcs = [102, 103, 104];
            for id in npcs {
                sim.engine.actor_mut(id).unwrap().room_id = room;
            }
            sim.queued.clear();
            sim.player_request(player.actor_id, "listener", json!({"enabled":auto}))
                .unwrap();
            sim.player_request(
                player.actor_id,
                "input",
                json!({"text":"Who runs this room?"}),
            )
            .unwrap();
            sim.pump().unwrap();
            let calls = npcs.len() + usize::from(auto);
            assert_eq!(sim.inflight.len(), calls);
            assert_eq!(sim.jobs.len(), calls);
            let mut requested_npcs = Vec::new();
            let mut listener_calls = 0;
            for _ in 0..calls {
                let (request, reply) = started(&mut requests).await;
                if request.listener {
                    listener_calls += 1;
                    assert_eq!(request.actor_id, player.actor_id);
                    assert_eq!(request.input["text"], "Who runs this room?");
                    reply.send(Ok(ThinkResult {
            model_output: None,
                        decision: json!({"summary":"A question with no physical action.","plan":[]}),
                        tokens: 10,
                        latency_ms: 100.,
                        cleanup_ms: 0.,
                        recalled: vec![],
                    })).unwrap();
                } else {
                    requested_npcs.push(request.actor_id);
                    assert!(
                        request.input["execution"]["reason"]
                            .as_str()
                            .unwrap()
                            .contains("Who runs this room?")
                    );
                    reply.send(Ok(decision(json!([
                        {"kind":"speak","text":format!("Reply from {}.", request.actor_id),"targetId":null,"volume":"normal","plot":null},
                        {"kind":"wait","seconds":600}
                    ])))).unwrap();
                }
            }
            requested_npcs.sort_unstable();
            assert_eq!(requested_npcs, npcs);
            assert_eq!(listener_calls, usize::from(auto));
            assert!(requests.try_recv().is_err());
            for _ in 0..calls {
                finish(&mut sim).await;
            }
            let mut speakers: Vec<_> = sim
                .engine
                .store
                .events(Some(player.actor_id), 100)
                .unwrap()
                .into_iter()
                .filter(|event| {
                    event.kind == "speech" && event.actor_id.is_some_and(|id| npcs.contains(&id))
                })
                .map(|event| event.actor_id.unwrap())
                .collect();
            speakers.sort_unstable();
            assert_eq!(speakers, npcs);
            assert_eq!(sim.wall, 0.);
            sim.cancel_all();
        }
    }

    #[tokio::test]
    async fn ordinary_npc_speech_wakes_room_mates_without_a_delay() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 2;
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        let room = sim.engine.actor(player.actor_id).unwrap().room_id;
        for id in [103, 104] {
            sim.engine.actor_mut(id).unwrap().room_id = room;
        }
        sim.collect();
        sim.queued.clear();
        assert_eq!(
            sim.engine
                .dispatch(
                    102,
                    json!({
                        "kind":"speak","text":"Has anyone seen the captain?",
                        "targetId":null,"volume":"normal","plot":null
                    }),
                    vec![],
                    None
                )
                .unwrap()["ok"],
            true
        );
        sim.collect();
        assert_eq!(sim.queued.len(), 2);
        assert!(
            sim.queued
                .values()
                .all(|q| q.wake.priority == WakePriority::Routine)
        );
        sim.pump().unwrap();
        let mut actors = Vec::new();
        let mut pending = Vec::new();
        for _ in 0..2 {
            let (request, reply) = started(&mut requests).await;
            actors.push(request.actor_id);
            assert!(
                request.input["execution"]["reason"]
                    .as_str()
                    .unwrap()
                    .contains("Has anyone seen the captain?")
            );
            pending.push(reply);
        }
        actors.sort_unstable();
        assert_eq!(actors, [103, 104]);
        assert_eq!(sim.wall, 0.);
        assert_eq!(sim.inflight.len(), 2);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn a_freed_slot_starts_waiting_routine_work_without_advancing_time() {
        for capacity in [1, 2] {
            let (mut sim, mut requests, _) = fixture();
            sim.scheduling.maximum_calls = capacity;
            let player = sim.join(None, "Fixture Guest".into()).unwrap();
            let room = sim.engine.actor(player.actor_id).unwrap().room_id;
            let npcs = [102, 103, 104];
            sim.queued.clear();
            for id in npcs {
                sim.engine.actor_mut(id).unwrap().room_id = room;
                wake(&mut sim, id, 0., false);
            }
            sim.pump().unwrap();
            let mut active = Vec::new();
            for _ in 0..capacity {
                active.push(started(&mut requests).await);
            }
            assert_eq!(sim.inflight.len(), capacity);
            assert_eq!(sim.queued.len(), npcs.len() - capacity);
            sim.pump().unwrap();
            assert!(requests.try_recv().is_err());
            let (finished, reply) = active.remove(0);
            reply
                .send(Ok(decision(json!([{"kind":"wait","seconds":600}]))))
                .unwrap();
            finish(&mut sim).await;
            assert_eq!(sim.inflight.len(), capacity - 1);
            sim.pump().unwrap();
            let (next, _reply) = started(&mut requests).await;
            assert_ne!(next.actor_id, finished.actor_id);
            assert!(
                active
                    .iter()
                    .all(|(request, _)| request.actor_id != next.actor_id)
            );
            assert_eq!(sim.inflight.len(), capacity);
            assert_eq!(sim.queued.len(), npcs.len() - capacity - 1);
            assert_eq!(sim.wall, 0.);
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
    async fn danger_precedes_older_routine_work() {
        let (mut sim, mut requests, _) = fixture();
        sim.scheduling.maximum_calls = 1;
        sim.join(None, "Fixture Guest".into()).unwrap();
        sim.queued.clear();
        sim.engine.actor_mut(103).unwrap().room_id = sim.engine.actor(102).unwrap().room_id;
        let now = sim.wall;
        wake(&mut sim, 103, now, false);
        wake(&mut sim, 102, now, true);
        sim.pump().unwrap();
        assert_eq!(started(&mut requests).await.0.actor_id, 102);
        assert!(sim.queued.contains_key(&103));
        assert_eq!(sim.wall, now);
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
        let evidence = request.input["world"]["memories"]
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
        let offered = request.input["world"]["actions"].as_array().unwrap();
        assert!(!offered.is_empty());
        assert!(
            offered.iter().all(|action| action["available"] != false),
            "Blocked door suggestions are for player discovery, not model actions"
        );
        assert!(
            sim.player_view(first.actor_id).unwrap()["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|action| action["available"] == false)
        );
        reply
            .send(Ok(ThinkResult {
                model_output: None,
                decision: json!({"summary":"No physical request.","plan":[]}),
                tokens: 10,
                latency_ms: 100.,
                cleanup_ms: 0.,
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
    async fn listener_can_replace_long_duties_but_waits_for_short_physical_actions() {
        for first in [
            json!({"kind":"rest","seconds":1000}),
            json!({"kind":"wait","seconds":700}),
            json!({"kind":"take","itemId":1023}),
        ] {
            for queued in [false, true] {
                let (mut sim, mut requests, _directory) = fixture();
                sim.scheduling.maximum_calls = 1;
                let player = sim.join(None, "Fixture Guest".into()).unwrap();
                let id = player.actor_id;
                assert_eq!(
                    sim.engine
                        .dispatch(id, first.clone(), vec![], None)
                        .unwrap()["ok"],
                    true
                );
                if queued {
                    sim.engine.actor_mut(id).unwrap().plan =
                        vec![crate::model::Intent::Wait { seconds: 20 }];
                }
                sim.player_request(id, "listener", json!({"enabled":true}))
                    .unwrap();
                sim.player_request(
                    id,
                    "input",
                    json!({"text":"I stop what I'm doing and wait by the wall."}),
                )
                .unwrap();
                sim.collect();
                sim.queued.clear();
                sim.pump().unwrap();
                if first["kind"] == "take" {
                    assert!(requests.try_recv().is_err());
                    assert_eq!(sim.messages.len(), 1);
                    let ends = sim
                        .engine
                        .actor(id)
                        .unwrap()
                        .activity
                        .as_ref()
                        .unwrap()
                        .interaction_ends
                        .unwrap();
                    sim.engine
                        .advance(ends - sim.engine.world.interaction_time)
                        .unwrap();
                    assert_eq!(
                        sim.engine
                            .world
                            .items
                            .iter()
                            .find(|i| i.id == 1023)
                            .unwrap()
                            .owner_id,
                        Some(id)
                    );
                    sim.pump().unwrap();
                }
                let (request, reply) = started(&mut requests).await;
                assert_eq!(request.actor_id, id);
                assert!(request.listener);
                reply.send(Ok(ThinkResult {
            model_output: None,
                    decision: json!({"summary":"Wait by the wall.","plan":[{"kind":"wait","seconds":90}]}),
                    tokens: 1, latency_ms: 1., cleanup_ms: 0., recalled: vec![],
                })).unwrap();
                finish(&mut sim).await;
                assert!(matches!(
                    sim.engine
                        .actor(id)
                        .unwrap()
                        .activity
                        .as_ref()
                        .unwrap()
                        .intent,
                    crate::model::Intent::Wait { seconds: 90 }
                ));
                assert!(sim.engine.actor(id).unwrap().plan.is_empty());
                sim.cancel_all();
            }
        }
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
                model_output: None,
                decision: json!({"summary":"Wait.","plan":[{"kind":"wait","seconds":600}]}),
                tokens: 1,
                latency_ms: 1.,
                cleanup_ms: 0.,
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

    #[tokio::test]
    async fn agent_limit_is_validated_atomically_and_survives_restore() {
        let (mut sim, _, directory) = fixture();
        let path = directory.path().join("server-settings.json");
        for invalid in [json!(0), json!(33), json!(2.5), json!("5"), Value::Null] {
            let before = std::fs::read(&path).ok();
            let maximum = sim.scheduling.maximum_calls;
            assert!(
                sim.manager_request("settings", json!({"language":"pl","maxAgents":invalid}))
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).ok(), before);
            assert_eq!(sim.scheduling.maximum_calls, maximum);
            assert_eq!(sim.pending_language, "en");
        }
        for limit in [1, 7, 32] {
            sim.manager_request("settings", json!({"maxAgents":limit}))
                .await
                .unwrap();
            assert_eq!(sim.manager_view().unwrap()["maxAgents"], limit);
            assert_eq!(
                serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap()["maxAgents"],
                limit
            );
            sim.engine.save("limit").unwrap();
            sim.manager_request("load", json!({"name":"limit"}))
                .await
                .unwrap();
            assert_eq!(sim.scheduling.maximum_calls, limit as usize);
        }
    }

    #[tokio::test]
    async fn lowering_capacity_preserves_inflight_and_raising_starts_waiting_work() {
        let (mut sim, mut requests, _directory) = fixture();
        sim.join(None, "Fixture Guest".into()).unwrap();
        sim.queued.clear();
        sim.scheduling.maximum_calls = 2;
        for id in [102, 103, 104] {
            sim.engine.actor_mut(id).unwrap().room_id = 1;
            wake(&mut sim, id, 0.0, false);
        }
        sim.pump().unwrap();
        let first = started(&mut requests).await;
        let second = started(&mut requests).await;
        sim.manager_request("settings", json!({"maxAgents":1}))
            .await
            .unwrap();
        assert_eq!(sim.inflight.len(), 2);
        assert!(requests.try_recv().is_err());
        first
            .1
            .send(Ok(decision(json!([{"kind":"wait","seconds":600}]))))
            .unwrap();
        finish(&mut sim).await;
        sim.pump().unwrap();
        assert_eq!(sim.inflight.len(), 1);
        assert!(requests.try_recv().is_err());
        sim.manager_request("settings", json!({"maxAgents":3}))
            .await
            .unwrap();
        let next = started(&mut requests).await;
        assert_ne!(next.0.actor_id, second.0.actor_id);
        assert_eq!(sim.inflight.len(), 2);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn recall_requires_the_exact_current_job_and_generation_even_on_the_same_branch() {
        let (mut sim, mut requests, _directory) = fixture();
        sim.scheduling.maximum_calls = 1;
        sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.pump().unwrap();
        let (current, _reply) = started(&mut requests).await;
        let make = |actor, token| {
            let (reply, _) = oneshot::channel();
            RecallRequest {
                actor_id: actor,
                branch: current.branch.clone(),
                token,
                query: "daughter".into(),
                reply,
            }
        };
        assert!(sim.recall_for_job(&make(102, current.token)).is_ok());
        assert!(sim.recall_for_job(&make(103, current.token)).is_err());
        assert!(
            sim.recall_for_job(&make(
                102,
                DecisionToken {
                    job_id: current.token.job_id + 1,
                    ..current.token
                }
            ))
            .is_err()
        );
        sim.manager_request("recover", json!({})).await.unwrap();
        assert_eq!(sim.engine.world.branch, current.branch);
        assert!(sim.recall_for_job(&make(102, current.token)).is_err());
        sim.cancel_all();
    }
    #[tokio::test]
    async fn one_slot_still_allows_background_progress_when_nobody_is_waiting_in_foreground() {
        let (mut sim, mut requests, _directory) = fixture();
        sim.scheduling.maximum_calls = 1;
        sim.join(None, "Fixture Guest".into()).unwrap();
        sim.collect();
        sim.queued.clear();
        wake(&mut sim, 109, 0.0, false);
        sim.pump().unwrap();
        let (request, _reply) = started(&mut requests).await;
        assert_eq!(request.actor_id, 109);
        assert_eq!(sim.inflight.len(), 1);
        sim.cancel_all();
    }
    #[tokio::test]
    async fn quiet_and_speech_only_idle_decisions_do_not_call_again_without_a_new_event() {
        for plan in [
            json!([]),
            json!([{"kind":"speak","text":"I heard you.","targetId":null,"volume":"normal","plot":null}]),
            json!([{"kind":"inspect","targetId":102}]),
        ] {
            let (mut sim, mut requests, _directory) = fixture();
            sim.scheduling.maximum_calls = 1;
            sim.join(None, "Fixture Guest".into()).unwrap();
            only(&mut sim, &[102]);
            sim.pump().unwrap();
            let (_, reply) = started(&mut requests).await;
            reply.send(Ok(decision(plan))).unwrap();
            finish(&mut sim).await;
            sim.pump().unwrap();
            assert!(requests.try_recv().is_err());
            assert!(!sim.queued.contains_key(&102));
            assert_eq!(sim.statuses[&(102, false)].calls, 1);
            sim.cancel_all();
        }
    }

    #[tokio::test]
    async fn later_plan_failure_has_private_feedback_and_only_one_immediate_correction() {
        let (mut sim, mut requests, _directory) = fixture();
        sim.scheduling.maximum_calls = 1;
        let player = sim.join(None, "Fixture Guest".into()).unwrap();
        only(&mut sim, &[102]);
        sim.pump().unwrap();
        let (_, reply) = started(&mut requests).await;
        let failed_plan = json!([{"kind":"speak","text":"I will cook.","targetId":null,"volume":"normal","plot":null},{"kind":"work","recipe":"meal"}]);
        reply.send(Ok(decision(failed_plan.clone()))).unwrap();
        finish(&mut sim).await;
        sim.pump().unwrap();
        let (correction, reply) = started(&mut requests).await;
        assert_eq!(
            correction.input["execution"]["self"]["lastPlanFailure"]["intent"],
            json!({"kind":"work","recipe":"meal"})
        );
        assert_eq!(
            correction.input["execution"]["self"]["lastPlanFailure"]["ok"],
            false
        );
        reply.send(Ok(decision(failed_plan))).unwrap();
        finish(&mut sim).await;
        sim.pump().unwrap();
        assert!(requests.try_recv().is_err());
        assert_eq!(sim.statuses[&(102, false)].calls, 2);
        sim.player_request(
            player.actor_id,
            "input",
            json!({"text":"Then leave the meal. Who commands this watch?"}),
        )
        .unwrap();
        sim.pump().unwrap();
        assert_eq!(started(&mut requests).await.0.actor_id, 102);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn conversation_waits_for_short_physical_completion_and_preserves_every_heard_message() {
        for duration in [0.4, 1.3] {
            let (mut sim, mut requests, _directory) = fixture();
            sim.scheduling.maximum_calls = 1;
            sim.engine.tuning.action_seconds = duration;
            let player = sim.join(None, "Fixture Guest".into()).unwrap();
            sim.queued.clear();
            assert_eq!(
                sim.engine
                    .dispatch(102, json!({"kind":"take","itemId":1023}), vec![], None)
                    .unwrap()["ok"],
                true
            );
            for text in ["Who commands you?", "Can I talk to him?"] {
                sim.player_request(player.actor_id, "input", json!({"text":text}))
                    .unwrap();
            }
            sim.pump().unwrap();
            assert!(requests.try_recv().is_err());
            assert!(sim.queued.contains_key(&102));
            assert!(sim.inflight.is_empty());
            sim.engine.advance(duration).unwrap();
            sim.pump().unwrap();
            let (request, _reply) = started(&mut requests).await;
            assert_eq!(request.actor_id, 102);
            assert_eq!(
                request.input["execution"]["self"]["lastActionOutcome"]["intent"],
                json!({"kind":"take","itemId":1023,"quantity":0})
            );
            let heard = request.input["world"]["roomContext"]["entries"].to_string();
            assert!(heard.contains("Who commands you?"));
            assert!(heard.contains("Can I talk to him?"));
            assert_eq!(
                request.input["world"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["id"] == 1023)
                    .unwrap()["ownerId"],
                102
            );
            sim.cancel_all();
        }
    }

    #[tokio::test]
    async fn cancelled_background_work_and_aging_cannot_consume_the_reserved_reaction_slot() {
        let (mut sim, mut requests, _directory) = fixture();
        sim.scheduling.maximum_calls = 3;
        sim.join(None, "Fixture Guest".into()).unwrap();
        sim.collect();
        sim.queued.clear();
        let created = -sim.scheduling.aging - 1.;
        for actor in [106, 109, 103] {
            wake(&mut sim, actor, created, false);
        }
        sim.pump().unwrap();
        let (first, _reply) = started(&mut requests).await;
        let (_second, _reply) = started(&mut requests).await;
        assert!(requests.try_recv().is_err());
        assert_eq!(sim.inflight.len(), 2);
        assert_eq!(sim.background_inflight.len(), 2);
        sim.engine
            .actor_mut(first.actor_id)
            .unwrap()
            .decision_version += 1;
        sim.collect();
        sim.pump().unwrap();
        assert!(!sim.jobs.contains_key(&(first.actor_id, false)));
        assert_eq!(sim.background_inflight.len(), 2);
        assert!(requests.try_recv().is_err());
        wake(&mut sim, 102, 0., true);
        sim.pump().unwrap();
        assert_eq!(started(&mut requests).await.0.actor_id, 102);
        assert_eq!(sim.inflight.len(), 3);
        finish(&mut sim).await;
        sim.pump().unwrap();
        assert_eq!(started(&mut requests).await.0.actor_id, 103);
        assert_eq!(sim.inflight.len(), 3);
        sim.cancel_all();
    }

    #[tokio::test]
    async fn passive_thinking_uses_active_time_and_rate_changes_do_not_change_bodies_or_calendar() {
        for rate in [1, 5, 15] {
            let (mut sim, mut requests, _directory) = fixture();
            sim.scheduling.maximum_calls = 1;
            sim.scheduling.set_npc_update_rate(rate).unwrap();
            let player = sim.join(None, "Fixture Guest".into()).unwrap();
            sim.collect();
            sim.queued.clear();
            for actor in &mut sim.engine.world.actors {
                actor.hunger = 10.;
                actor.thirst = 10.;
                actor.fatigue = 10.;
                if actor.id != 102 && actor.id != player.actor_id {
                    actor.flags |= crate::model::UNCONSCIOUS;
                }
            }
            let interval = sim.scheduling.background_interval;
            let start = sim.engine.world.time;
            sim.tick(interval / 2.).unwrap();
            assert!(requests.try_recv().is_err());
            assert!(sim.inflight.is_empty());
            sim.engine.set_pause(true).unwrap();
            let bodies = json!(sim.engine.world.actors);
            sim.tick(1000.).unwrap();
            assert_eq!(json!(sim.engine.world.actors), bodies);
            assert_eq!(sim.engine.world.time, start + interval / 2. * 24.);
            sim.engine.set_pause(false).unwrap();
            sim.tick(interval / 2. - 0.01).unwrap();
            assert!(requests.try_recv().is_err());
            assert!(sim.inflight.is_empty());
            sim.tick(0.02).unwrap();
            assert_eq!(started(&mut requests).await.0.actor_id, 102);
            assert!((sim.engine.world.time - (start + (interval + 0.01) * 24.)).abs() < 1e-8);
            let before = json!(sim.engine.world.actors);
            sim.manager_request(
                "settings",
                json!({"npcUpdateRate": if rate == 1 { 15 } else { 1 }}),
            )
            .await
            .unwrap();
            assert_eq!(json!(sim.engine.world.actors), before);
            sim.cancel_all();
        }
    }

    #[tokio::test]
    async fn npc_rate_settings_are_atomic_persistent_and_survive_save_restore() {
        let (mut sim, _requests, directory) = fixture();
        let path = directory.path().join("server-settings.json");
        write_private(&path, b"{\"port\":2237,\"maxAgents\":7}", false).unwrap();
        for invalid in [
            json!(0),
            json!(2),
            json!(16),
            json!(5.5),
            json!("5"),
            Value::Null,
        ] {
            let before = std::fs::read(&path).unwrap();
            assert!(
                sim.manager_request("settings", json!({"language":"pl","npcUpdateRate":invalid}))
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(sim.pending_language, "en");
        }
        for rate in [1, 5, 15] {
            sim.manager_request("settings", json!({"npcUpdateRate":rate}))
                .await
                .unwrap();
            let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved["port"], 2237);
            assert_eq!(saved["npcUpdateRate"], rate);
            assert_eq!(sim.manager_view().unwrap()["npcUpdateRate"], rate);
            sim.engine.save("rate").unwrap();
            sim.engine.set_pause(true).unwrap();
            sim.tick(1200.).unwrap();
            sim.manager_request("load", json!({"name":"rate"}))
                .await
                .unwrap();
            assert_eq!(sim.scheduling.npc_update_rate, rate as u8);
            assert_eq!(sim.last_idle, sim.engine.world.interaction_time);
        }
    }
}
