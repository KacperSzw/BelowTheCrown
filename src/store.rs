use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

fn normalized_words(text: &str) -> Vec<String> {
    let text = text
        .nfc()
        .collect::<String>()
        .to_lowercase()
        .nfc()
        .collect::<String>();
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_owned)
        .collect()
}
fn recall_words(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    normalized_words(query)
        .into_iter()
        .filter(|word| {
            !matches!(
                word.as_str(),
                "the"
                    | "and"
                    | "for"
                    | "you"
                    | "your"
                    | "with"
                    | "that"
                    | "this"
                    | "have"
                    | "has"
                    | "was"
                    | "are"
                    | "from"
                    | "into"
                    | "someone"
                    | "entered"
                    | "room"
                    | "reassess"
                    | "situation"
                    | "routine"
                    | "decide"
                    | "decision"
                    | "seconds"
                    | "observation"
                    | "character"
                    | "want"
                    | "will"
                    | "but"
                    | "been"
                    | "their"
                    | "they"
                    | "them"
                    | "what"
                    | "when"
                    | "where"
                    | "only"
                    | "other"
                    | "some"
                    | "than"
                    | "then"
                    | "heard"
                    | "słyszysz"
                    | "słowa"
                    | "osoby"
                    | "usłyszane"
                    | "chcę"
                    | "muszę"
                    | "moje"
                    | "moja"
                    | "mój"
                    | "się"
                    | "jest"
                    | "nie"
                    | "dla"
                    | "jak"
                    | "oraz"
                    | "przez"
                    | "który"
                    | "która"
                    | "twoje"
                    | "twoja"
                    | "twój"
                    | "ktoś"
                    | "wszedł"
                    | "pomieszczenia"
                    | "oceń"
                    | "sytuację"
                    | "decyzja"
                    | "postać"
                    | "sekund"
            ) && seen.insert(word.clone())
        })
        .take(12)
        .collect()
}

fn sql_id(id: u64) -> Result<i64> {
    i64::try_from(id).context("Journal ID exceeds SQLite's supported range.")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SaveData {
    pub format: u8,
    pub world: WorldState,
    pub memories: Vec<Memory>,
    pub events: Vec<WorldEvent>,
}

/// SQLite owns the journal and checkpoint. The engine commits physical and mental changes together.
pub struct Store {
    pub db: Connection,
    pub path: PathBuf,
    save_catalog: RefCell<BTreeMap<String, ([u8; 32], Value)>>,
}

pub fn migrate_world(mut raw: Value) -> Result<WorldState> {
    let format = raw
        .get("format")
        .and_then(Value::as_u64)
        .context("Missing world format.")?;
    ensure!((1..=5).contains(&format), "Unsupported world format.");
    let time = raw
        .get("time")
        .and_then(Value::as_f64)
        .context("Missing clock.")?;
    let speed = raw
        .get("speed")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(5.0);
    let interaction = raw
        .get("interactionTime")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let object = raw.as_object_mut().context("World must be an object.")?;
    if format == 1 {
        object.insert("mode".into(), json!("solo"));
        object.insert(
            "players".into(),
            json!([{"identity":"local","actorId":PLAYER,"listener":false}]),
        );
        object.insert(
            "winnerId".into(),
            if object.get("ending").and_then(Value::as_str) == Some("victory") {
                json!(PLAYER)
            } else {
                Value::Null
            },
        );
    }
    object.entry("language").or_insert(json!("en"));
    object.entry("scenarioId").or_insert(json!("castle"));
    object
        .entry("interactionTime")
        .or_insert(json!(interaction));
    object.entry("encounters").or_insert(json!([]));
    object.insert("format".into(), json!(5));
    object.remove("speed");
    let mut world: WorldState = serde_json::from_value(raw).context("Corrupt world snapshot.")?;
    if format < 5 {
        for actor in &mut world.actors {
            for intent in actor.plan.iter_mut().chain(
                actor
                    .activity
                    .iter_mut()
                    .map(|activity| &mut activity.intent),
            ) {
                if let Intent::Guard { seconds } = intent {
                    *intent = Intent::Wait { seconds: *seconds };
                }
            }
        }
    }
    if format < 4 {
        for actor in &mut world.actors {
            if format < 3
                && let Some(deadline) = actor.unconscious_until
            {
                actor.unconscious_until = Some(interaction + (deadline - time).clamp(0.0, 30.0));
            }
            if let Some(activity) = &mut actor.activity
                && !activity.intent.long()
                && !matches!(activity.intent, Intent::Attack { .. })
            {
                let remaining = ((activity.ends - time) / speed).max(0.0);
                let remaining = if activity.intent.travel() {
                    remaining
                        .max(activity.interaction_ends.unwrap_or(interaction + 2.0) - interaction)
                } else {
                    remaining
                };
                activity.interaction_ends = Some(interaction + remaining);
            }
        }
        for offer in &mut world.offers {
            offer.interaction_expires =
                Some(interaction + ((offer.expires - time) / speed).max(0.0));
        }
        for effect in &mut world.poison {
            effect.interaction_next = Some(interaction + ((effect.next - time) / speed).max(0.0));
        }
    }
    // Formats 1 and 2 represented attacks as activities; preserve their choice in a real opening round.
    if format < 3 {
        let attacks: Vec<_> = world
            .actors
            .iter()
            .filter_map(|actor| {
                actor
                    .activity
                    .as_ref()
                    .filter(|a| matches!(a.intent, Intent::Attack { .. }))
                    .map(|activity| (actor.id, activity.clone()))
            })
            .collect();
        for (source, activity) in attacks {
            let target = activity
                .intent
                .target_id()
                .context("Attack has no target.")?;
            if let Some(actor) = world.actors.iter_mut().find(|a| a.id == source) {
                actor.activity = None;
                actor.clear_plan();
            }
            let Some(attacker) = world.actors.iter().find(|a| a.id == source) else {
                continue;
            };
            let Some(defender) = world.actors.iter().find(|a| a.id == target) else {
                continue;
            };
            if !attacker.awake() || !defender.alive() || attacker.room_id != defender.room_id {
                continue;
            }
            let room = attacker.room_id;
            let position = world
                .encounters
                .iter()
                .position(|e| e.room_id == room)
                .unwrap_or_else(|| {
                    world.encounters.push(CombatEncounter {
                        id: Uuid::new_v4().to_string(),
                        room_id: room,
                        round: 1,
                        deadline: interaction + 30.0,
                        participants: vec![],
                        hostility: vec![],
                    });
                    world.encounters.len() - 1
                });
            let encounter = &mut world.encounters[position];
            for id in [source, target] {
                if !encounter.participants.iter().any(|p| p.actor_id == id) {
                    encounter.participants.push(CombatParticipant {
                        actor_id: id,
                        joins_round: 1,
                        choice: None,
                        causes: vec![],
                    });
                }
            }
            let participant = encounter
                .participants
                .iter_mut()
                .find(|p| p.actor_id == source)
                .context("Missing combat participant.")?;
            participant.choice = Some(activity.intent);
            participant.causes = activity.causes;
            if !encounter.hostility.contains(&[source, target]) {
                encounter.hostility.push([source, target]);
            }
        }
    }
    assert_world(&world)?;
    Ok(world)
}

pub fn assert_world(world: &WorldState) -> Result<()> {
    ensure!(
        world.format == 5
            && matches!(world.language.as_str(), "en" | "pl")
            && matches!(world.mode.as_str(), "solo" | "multiplayer")
            && matches!(world.ending.as_str(), "playing" | "victory" | "death"),
        "Unsupported or corrupt world snapshot."
    );
    ensure!(
        !world.scenario_id.is_empty()
            && !world.branch.is_empty()
            && world.time.is_finite()
            && world.time >= 0.0
            && world.interaction_time.is_finite()
            && world.interaction_time >= 0.0,
        "Invalid world clock or identity."
    );
    let ids: Vec<_> = world
        .rooms
        .iter()
        .map(|x| x.id)
        .chain(world.doors.iter().map(|x| x.id))
        .chain(world.actors.iter().map(|x| x.id))
        .chain(world.items.iter().map(|x| x.id))
        .collect();
    ensure!(
        ids.iter().all(|id| *id > 0)
            && ids.iter().copied().collect::<HashSet<_>>().len() == ids.len(),
        "Invalid or duplicate world IDs."
    );
    let rooms: HashSet<_> = world.rooms.iter().map(|r| r.id).collect();
    let actors: HashSet<_> = world.actors.iter().map(|a| a.id).collect();
    ensure!(
        actors.contains(&KING) && (world.mode != "solo" || actors.contains(&PLAYER)),
        "Snapshot is missing the prisoner or king."
    );
    ensure!(
        world.next_item_id > u32::from(*ids.iter().max().unwrap_or(&0))
            && world.next_item_id <= 65536,
        "Invalid next entity ID."
    );
    ensure!(
        world.next_offer_id > 0
            && world.next_offer_id <= 65536
            && world.next_plot_id > 0
            && world.next_plot_id <= 65536,
        "Invalid offer or plot sequence."
    );
    let mut identities = HashSet::new();
    let mut owners = HashSet::new();
    for player in &world.players {
        ensure!(
            !player.identity.is_empty()
                && actors.contains(&player.actor_id)
                && identities.insert(&player.identity)
                && owners.insert(player.actor_id),
            "Invalid character ownership."
        );
    }
    ensure!(
        world.winner_id.is_none_or(|id| owners.contains(&id)),
        "Invalid winner."
    );
    let finite_meter = |n: f64| n.is_finite() && (0.0..=100.0).contains(&n);
    for actor in &world.actors {
        ensure!(
            rooms.contains(&actor.room_id)
                && actor.flags & !15 == 0
                && [
                    actor.health,
                    actor.hunger,
                    actor.thirst,
                    actor.fatigue,
                    actor.skills.combat,
                    actor.skills.stealth,
                    actor.skills.craft,
                    actor.skills.medicine
                ]
                .iter()
                .all(|n| finite_meter(*n)),
            "Invalid actor state."
        );
        ensure!(
            actor.known_rooms.iter().all(|id| rooms.contains(id))
                && actor.known_actors.iter().all(|id| actors.contains(id))
                && actor.relationships.keys().all(|id| actors.contains(id)),
            "Invalid actor knowledge references."
        );
        for relationship in actor.relationships.values() {
            ensure!(
                [
                    relationship.trust,
                    relationship.affection,
                    relationship.respect
                ]
                .iter()
                .all(|n| n.is_finite() && (-100.0..=100.0).contains(n))
                    && finite_meter(relationship.fear),
                "Invalid relationship."
            );
        }
        ensure!(
            actor
                .unconscious_until
                .is_none_or(|n| n.is_finite() && n >= 0.0),
            "Invalid recovery deadline."
        );
        if let Some(key) = &actor.portrait_key {
            ensure!(
                key.len() <= 40
                    && key.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "Invalid portrait key."
            );
        }
        if let Some(activity) = &actor.activity {
            activity.intent.check()?;
            ensure!(
                activity.started.is_finite()
                    && activity.ends.is_finite()
                    && activity.started >= 0.0
                    && activity.ends >= activity.started
                    && activity
                        .interaction_ends
                        .is_none_or(|n| n.is_finite() && n >= 0.0),
                "Invalid activity deadline."
            );
        }
        for intent in &actor.plan {
            intent.check()?;
        }
        for outcome in actor
            .last_action_outcome
            .iter()
            .chain(&actor.last_plan_failure)
        {
            outcome.intent.check()?;
            ensure!(
                outcome.time.is_finite() && outcome.time >= 0. && outcome.time <= world.time,
                "Invalid action outcome time."
            );
        }
        ensure!(
            actor
                .last_plan_failure
                .as_ref()
                .is_none_or(|outcome| !outcome.ok),
            "A plan failure cannot report success."
        );
        ensure!(
            actor
                .equipment
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len()
                == actor.equipment.len()
                && actor.equipment.iter().all(|id| world
                    .items
                    .iter()
                    .any(|i| i.id == *id && i.owner_id == Some(actor.id) && i.equipped)),
            "Invalid equipment custody."
        );
    }
    let mut encounter_ids = HashSet::new();
    let mut engaged = HashSet::new();
    for encounter in &world.encounters {
        ensure!(
            !encounter.id.is_empty()
                && encounter_ids.insert(&encounter.id)
                && rooms.contains(&encounter.room_id)
                && encounter.round > 0
                && encounter.deadline.is_finite()
                && encounter.deadline >= 0.0,
            "Invalid combat encounter."
        );
        for participant in &encounter.participants {
            ensure!(
                actors.contains(&participant.actor_id)
                    && engaged.insert(participant.actor_id)
                    && participant.joins_round >= 1
                    && participant.joins_round <= encounter.round.saturating_add(1),
                "Invalid combat participant."
            );
            if let Some(choice) = &participant.choice {
                choice.check()?;
                ensure!(choice.combat_move(), "Invalid combat choice.");
            }
        }
        ensure!(
            encounter.hostility.iter().all(|pair| pair[0] != pair[1]
                && pair
                    .iter()
                    .all(|id| encounter.participants.iter().any(|p| p.actor_id == *id))),
            "Invalid combat hostility."
        );
    }
    for item in &world.items {
        ensure!(
            item.quantity > 0
                && item.owner_id.is_none() != item.room_id.is_none()
                && item.owner_id.is_none_or(|id| actors.contains(&id))
                && item.room_id.is_none_or(|id| rooms.contains(&id))
                && finite_meter(item.power),
            "Invalid item custody or quantity."
        );
        ensure!(
            !item.equipped
                || item.owner_id.is_some_and(|id| world
                    .actors
                    .iter()
                    .any(|a| a.id == id && a.equipment.contains(&item.id))),
            "Invalid item equipment."
        );
        ensure!(
            item.poisoned_by.is_none_or(|id| actors.contains(&id)),
            "Invalid poison source."
        );
    }
    for door in &world.doors {
        ensure!(
            door.rooms[0] != door.rooms[1]
                && door.rooms.iter().all(|id| rooms.contains(id))
                && door.flags & !7 == 0
                && door.flags & (OPEN | LOCKED) != OPEN | LOCKED
                && (door.flags & LOCKED == 0 || door.lock.is_some()),
            "Invalid door connection or lock."
        );
    }
    let mut offer_ids = HashSet::new();
    for offer in &world.offers {
        ensure!(
            offer.id > 0
                && u32::from(offer.id) < world.next_offer_id
                && offer_ids.insert(offer.id)
                && actors.contains(&offer.from)
                && actors.contains(&offer.to)
                && offer.from != offer.to
                && offer.expires.is_finite()
                && offer.expires >= 0.0
                && offer
                    .interaction_expires
                    .is_none_or(|n| n.is_finite() && n >= 0.0)
                && matches!(
                    offer.status.as_str(),
                    "open" | "accepted" | "declined" | "expired"
                ),
            "Invalid offer."
        );
    }
    let mut plot_ids = HashSet::new();
    for plot in &world.plots {
        ensure!(
            plot.id > 0
                && u32::from(plot.id) < world.next_plot_id
                && plot_ids.insert(plot.id)
                && [plot.proposer, plot.recipient, plot.victim]
                    .iter()
                    .all(|id| actors.contains(id)),
            "Invalid plot."
        );
    }
    let mut trap_doors = HashSet::new();
    for trap in &world.traps {
        ensure!(
            world.doors.iter().any(|d| d.id == trap.door_id)
                && trap_doors.insert(trap.door_id)
                && actors.contains(&trap.setter)
                && finite_meter(trap.power),
            "Invalid trap."
        );
    }
    for effect in &world.poison {
        ensure!(
            actors.contains(&effect.actor_id)
                && actors.contains(&effect.source_id)
                && effect.damage.is_finite()
                && effect.damage > 0.0
                && effect.remaining > 0
                && effect.next.is_finite()
                && effect.next >= 0.0
                && effect
                    .interaction_next
                    .is_none_or(|n| n.is_finite() && n >= 0.0),
            "Invalid poison effect."
        );
    }
    Ok(())
}

impl Store {
    pub fn open(directory: &Path) -> Result<Self> {
        crate::paths::private_dir(directory)?;
        for name in ["world.sqlite", "world.sqlite-wal", "world.sqlite-shm"] {
            let candidate = directory.join(name);
            if let Ok(metadata) = std::fs::symlink_metadata(&candidate) {
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "SQLite state must be a real file: {}",
                    candidate.display()
                );
            }
        }
        let path = directory.join("world.sqlite");
        let db = Connection::open_with_flags(
            &path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Self::initialize(db, path)
    }
    pub fn memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?, PathBuf::from(":memory:"))
    }
    fn initialize(db: Connection, path: PathBuf) -> Result<Self> {
        db.create_scalar_function(
            "npc_memory_match",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |context| {
                let text = normalized_words(&context.get::<String>(0)?);
                let query = context.get::<String>(1)?;
                Ok(query
                    .split('\u{1f}')
                    .filter(|word| !word.is_empty() && text.iter().any(|actual| actual == word))
                    .count() as i64)
            },
        )?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;
            CREATE TABLE IF NOT EXISTS checkpoint(singleton INTEGER PRIMARY KEY CHECK(singleton=1),data TEXT NOT NULL) STRICT;
            CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY,time REAL NOT NULL,data TEXT NOT NULL) STRICT;
            CREATE TABLE IF NOT EXISTS memories(id INTEGER PRIMARY KEY,actor_id INTEGER NOT NULL,time REAL NOT NULL,importance INTEGER NOT NULL CHECK(importance BETWEEN 1 AND 10),data TEXT NOT NULL) STRICT;
            CREATE INDEX IF NOT EXISTS memory_actor_time ON memories(actor_id,time DESC);
            CREATE INDEX IF NOT EXISTS memory_actor_importance ON memories(actor_id,importance DESC);
            CREATE TABLE IF NOT EXISTS saves(name TEXT PRIMARY KEY,created TEXT NOT NULL,data TEXT NOT NULL) STRICT;")?;
        Ok(Self {
            db,
            path,
            save_catalog: RefCell::new(BTreeMap::new()),
        })
    }
    pub fn begin(&self) -> Result<bool> {
        if !self.db.is_autocommit() {
            return Ok(false);
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        Ok(true)
    }
    pub fn commit(&self, own: bool) -> Result<()> {
        if own {
            self.db.execute_batch("COMMIT")?;
        }
        Ok(())
    }
    pub fn rollback(&self, own: bool) {
        if own {
            let _ = self.db.execute_batch("ROLLBACK");
        }
    }
    pub fn load(&self) -> Result<Option<WorldState>> {
        let row: Option<String> = self
            .db
            .query_row("SELECT data FROM checkpoint WHERE singleton=1", [], |row| {
                row.get(0)
            })
            .optional()?;
        row.map(|text| migrate_world(serde_json::from_str(&text)?))
            .transpose()
    }
    pub fn checkpoint(&self, world: &WorldState) -> Result<()> {
        assert_world(world)?;
        self.db.execute("INSERT INTO checkpoint(singleton,data) VALUES(1,?) ON CONFLICT(singleton) DO UPDATE SET data=excluded.data",[serde_json::to_string(world)?])?;
        Ok(())
    }
    pub fn new_world(&self, world: &WorldState, memories: &[Memory]) -> Result<()> {
        assert_world(world)?;
        let own = self.begin()?;
        let result = (|| {
            self.db
                .execute_batch("DELETE FROM events;DELETE FROM memories;")?;
            for memory in memories {
                self.remember(memory.clone())?;
            }
            self.checkpoint(world)
        })();
        match result {
            Ok(()) => self.commit(own),
            Err(error) => {
                self.rollback(own);
                Err(error)
            }
        }
    }
    pub fn remember(&self, mut memory: Memory) -> Result<Memory> {
        ensure!(
            (1..=10).contains(&memory.importance) && memory.actor_id > 0 && memory.time.is_finite(),
            "Invalid memory."
        );
        memory.id = u64::try_from(self.db.query_row(
            "SELECT COALESCE(MAX(id),0)+1 FROM memories",
            [],
            |row| row.get::<_, i64>(0),
        )?)?;
        self.db.execute(
            "INSERT INTO memories(id,actor_id,time,importance,data) VALUES(?,?,?,?,?)",
            params![
                sql_id(memory.id)?,
                memory.actor_id,
                memory.time,
                memory.importance,
                serde_json::to_string(&memory)?
            ],
        )?;
        Ok(memory)
    }
    pub fn event(&self, mut event: WorldEvent) -> Result<WorldEvent> {
        event.id = u64::try_from(self.db.query_row(
            "SELECT COALESCE(MAX(id),0)+1 FROM events",
            [],
            |row| row.get::<_, i64>(0),
        )?)?;
        self.db.execute(
            "INSERT INTO events(id,time,data) VALUES(?,?,?)",
            params![
                sql_id(event.id)?,
                event.time,
                serde_json::to_string(&event)?
            ],
        )?;
        Ok(event)
    }
    pub fn update_event(&self, event: &WorldEvent) -> Result<()> {
        self.db.execute(
            "UPDATE events SET data=? WHERE id=?",
            params![serde_json::to_string(event)?, sql_id(event.id)?],
        )?;
        Ok(())
    }
    pub fn events(&self, actor: Option<u16>, limit: usize) -> Result<Vec<WorldEvent>> {
        let mut values = Vec::new();
        if let Some(id) = actor {
            let mut stmt=self.db.prepare("SELECT data FROM events WHERE EXISTS(SELECT 1 FROM json_each(events.data,'$.audience') WHERE value=?) ORDER BY id DESC LIMIT ?")?;
            for row in stmt.query_map(params![id, limit as i64], |r| r.get::<_, String>(0))? {
                let event: WorldEvent = serde_json::from_str(&row?)?;
                if let Some(event) = event.projected(id) {
                    values.push(event);
                }
            }
        } else {
            let mut stmt = self
                .db
                .prepare("SELECT data FROM events ORDER BY id DESC LIMIT ?")?;
            for row in stmt.query_map([limit as i64], |r| r.get::<_, String>(0))? {
                values.push(serde_json::from_str(&row?)?);
            }
        }
        values.reverse();
        Ok(values)
    }
    pub fn observed_event(&self, actor: u16, event_id: u64) -> Result<Option<WorldEvent>> {
        let row: Option<String> = self
            .db
            .query_row(
                "SELECT data FROM events WHERE id=?",
                [sql_id(event_id)?],
                |r| r.get(0),
            )
            .optional()?;
        row.map(|text| {
            serde_json::from_str::<WorldEvent>(&text).map(|event| event.projected(actor))
        })
        .transpose()
        .map(|event| event.flatten())
        .map_err(Into::into)
    }
    pub fn memories(&self, actor: u16, limit: usize) -> Result<Vec<Memory>> {
        let mut stmt = self
            .db
            .prepare("SELECT data FROM memories WHERE actor_id=? ORDER BY id DESC LIMIT ?")?;
        let mut values = Vec::new();
        for row in stmt.query_map(params![actor, limit as i64], |r| r.get::<_, String>(0))? {
            values.push(serde_json::from_str(&row?)?);
        }
        values.reverse();
        Ok(values)
    }
    pub fn owned_memory(&self, actor: u16, id: u64) -> Result<Option<Memory>> {
        let text: Option<String> = self
            .db
            .query_row(
                "SELECT data FROM memories WHERE actor_id=? AND id=?",
                params![actor, sql_id(id)?],
                |row| row.get(0),
            )
            .optional()?;
        text.map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(Into::into)
    }
    /// Keep others' words available even when this actor has spoken repeatedly.
    pub fn recent_dialogue(
        &self,
        actor: u16,
        other_limit: usize,
        own_limit: usize,
    ) -> Result<Vec<Memory>> {
        let mut stmt = self.db.prepare(
            "WITH speech AS (
                SELECT id, data, json_extract(data,'$.sourceId') AS speaker
                FROM memories WHERE actor_id=?1 AND json_extract(data,'$.kind')='heard'
            ), others AS (
                SELECT id, data FROM speech WHERE speaker IS NULL OR speaker != ?1 ORDER BY id DESC LIMIT ?2
            ), own AS (
                SELECT id, data FROM speech WHERE speaker = ?1 ORDER BY id DESC LIMIT ?3
            )
            SELECT data FROM (SELECT * FROM others UNION ALL SELECT * FROM own) ORDER BY id",
        )?;
        let mut values = Vec::new();
        for row in stmt.query_map(params![actor, other_limit as i64, own_limit as i64], |r| {
            r.get::<_, String>(0)
        })? {
            values.push(serde_json::from_str(&row?)?);
        }
        Ok(values)
    }
    pub fn recall(&self, actor: u16, query: &str, limit: usize) -> Result<Vec<Memory>> {
        self.recall_excluding(actor, query, limit, &[])
    }
    pub fn recall_excluding(
        &self,
        actor: u16,
        query: &str,
        limit: usize,
        excluded: &[u64],
    ) -> Result<Vec<Memory>> {
        let recent = limit / 4;
        let relevant = limit / 12 * 5 + (limit % 12) * 5 / 12;
        self.recall_with_budget(
            actor,
            query,
            [recent, relevant, limit - recent - relevant],
            excluded,
        )
    }
    /// Explicit allocation lets tests and callers tune diversity without changing defaults.
    pub fn recall_with_budget(
        &self,
        actor: u16,
        query: &str,
        allocation: [usize; 3],
        excluded: &[u64],
    ) -> Result<Vec<Memory>> {
        self.recall_with_ranges(
            actor,
            query,
            allocation,
            &excluded.iter().map(|id| [*id, *id]).collect::<Vec<_>>(),
        )
    }
    pub fn recall_with_ranges(
        &self,
        actor: u16,
        query: &str,
        allocation: [usize; 3],
        excluded: &[[u64; 2]],
    ) -> Result<Vec<Memory>> {
        let limit = allocation
            .iter()
            .try_fold(0usize, |sum, n| sum.checked_add(*n))
            .context("Recall budget overflow")?;
        if limit == 0 {
            return Ok(vec![]);
        }
        let query = recall_words(query).join("\u{1f}");
        let excluded = serde_json::to_string(excluded)?;
        let candidates = limit.saturating_mul(3).min(i64::MAX as usize) as i64;
        let load = |ranking: &str| -> Result<Vec<Memory>> {
            let sql = format!(
                "SELECT data FROM memories WHERE actor_id=?1 AND NOT EXISTS (SELECT 1 FROM json_each(?2) WHERE memories.id BETWEEN json_extract(value,'$[0]') AND json_extract(value,'$[1]')) {ranking} LIMIT ?4"
            );
            let mut statement = self.db.prepare(&sql)?;
            statement
                .query_map(params![actor, excluded, query, candidates], |row| {
                    row.get::<_, String>(0)
                })?
                .map(|row| Ok(serde_json::from_str(&row?)?))
                .collect()
        };
        // Relevant candidates still scan this actor's text in SQLite; only bounded pools deserialize.
        let recent = load("AND length(?3)>=0 ORDER BY id DESC")?;
        let relevant = load(
            "AND npc_memory_match(json_extract(data,'$.text'),?3)>0 ORDER BY npc_memory_match(json_extract(data,'$.text'),?3) DESC, importance DESC, id DESC",
        )?;
        let important = load("AND length(?3)>=0 ORDER BY importance DESC, id DESC")?;
        let mut seen = HashSet::new();
        let mut selected = Vec::new();
        for (pool, quota) in [
            (&recent, allocation[0]),
            (&relevant, allocation[1]),
            (&important, allocation[2]),
        ] {
            selected.extend(
                pool.iter()
                    .filter(|m| seen.insert(m.id))
                    .take(quota)
                    .cloned(),
            );
        }
        selected.extend(
            relevant
                .iter()
                .chain(&important)
                .chain(&recent)
                .filter(|m| seen.insert(m.id))
                .take(limit.saturating_sub(selected.len()))
                .cloned(),
        );
        selected.sort_by(|a, b| a.time.total_cmp(&b.time).then(a.id.cmp(&b.id)));
        Ok(selected)
    }
    pub fn owns_evidence(&self, actor: u16, ids: &[u64]) -> Result<bool> {
        if ids.is_empty() {
            return Ok(false);
        }
        for id in ids {
            if !self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id=? AND actor_id=?)",
                params![sql_id(*id)?, actor],
                |r| r.get::<_, bool>(0),
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub fn saves(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .db
            .prepare("SELECT name FROM saves ORDER BY created DESC,name")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
    pub fn save_summaries(&self) -> Result<Vec<Value>> {
        let mut stmt = self
            .db
            .prepare("SELECT name,created,data FROM saves ORDER BY created DESC,name")?;
        let mut output = Vec::new();
        let mut seen = HashSet::new();
        let mut catalog = self.save_catalog.borrow_mut();
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (name, created, data) = row?;
            seen.insert(name.clone());
            let digest: [u8; 32] = Sha256::digest(data.as_bytes()).into();
            if let Some((cached_digest, summary)) = catalog.get(&name)
                && *cached_digest == digest
                && summary["created"] == created
            {
                output.push(summary.clone());
                continue;
            }
            let summary = match Self::parse_snapshot(&data) {
                Ok(snapshot) => json!({
                    "name":name,"created":created,"valid":true,
                    "scenarioId":snapshot.world.scenario_id,"language":snapshot.world.language,
                    "time":snapshot.world.time,"ending":snapshot.world.ending,
                }),
                Err(_) => json!({
                    "name":name,"created":created,"valid":false,
                    "scenarioId":null,"language":null,"time":null,"ending":null,
                }),
            };
            catalog.insert(name, (digest, summary.clone()));
            output.push(summary);
        }
        catalog.retain(|name, _| seen.contains(name));
        Ok(output)
    }
    pub fn save(&self, world: &WorldState, name: &str) -> Result<()> {
        ensure!(
            (1..=32).contains(&name.len())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Save names use 1–32 letters, numbers, hyphens, or underscores."
        );
        let own = self.begin()?;
        let result = (|| {
            self.checkpoint(world)?;
            let mut stmt = self.db.prepare("SELECT data FROM memories ORDER BY id")?;
            let mut memories = Vec::new();
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                memories.push(serde_json::from_str(&row?)?);
            }
            let save = SaveData {
                format: 5,
                world: world.clone(),
                memories,
                events: self.events(None, i64::MAX as usize)?,
            };
            self.db.execute("INSERT INTO saves(name,created,data) VALUES(?,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?) ON CONFLICT(name) DO UPDATE SET created=excluded.created,data=excluded.data",params![name,serde_json::to_string(&save)?])?;
            Ok(())
        })();
        match result {
            Ok(()) => self.commit(own),
            Err(e) => {
                self.rollback(own);
                Err(e)
            }
        }
    }
    pub fn snapshot(&self, name: &str) -> Result<SaveData> {
        let row: Option<String> = self
            .db
            .query_row("SELECT data FROM saves WHERE name=?", [name], |r| r.get(0))
            .optional()?;
        Self::parse_snapshot(&row.with_context(|| format!("No save named {name}."))?)
    }
    pub fn parse_snapshot(source: &str) -> Result<SaveData> {
        let raw: Value = serde_json::from_str(source)?;
        ensure!(
            raw.get("format")
                .and_then(Value::as_u64)
                .is_some_and(|n| (1..=5).contains(&n)),
            "Unsupported save format."
        );
        let world = migrate_world(raw.get("world").context("Missing saved world.")?.clone())?;
        let memories: Vec<Memory> =
            serde_json::from_value(raw.get("memories").context("Missing saved minds.")?.clone())?;
        let events: Vec<WorldEvent> =
            serde_json::from_value(raw.get("events").context("Missing saved journal.")?.clone())?;
        let save = SaveData {
            format: 5,
            world,
            memories,
            events,
        };
        validate_snapshot(&save)?;
        Ok(save)
    }
    pub fn replace(&self, save: &SaveData) -> Result<()> {
        validate_snapshot(save)?;
        let own = self.begin()?;
        let result = (|| {
            self.db
                .execute_batch("DELETE FROM events;DELETE FROM memories;")?;
            for memory in &save.memories {
                self.db.execute(
                    "INSERT INTO memories(id,actor_id,time,importance,data) VALUES(?,?,?,?,?)",
                    params![
                        sql_id(memory.id)?,
                        memory.actor_id,
                        memory.time,
                        memory.importance,
                        serde_json::to_string(memory)?
                    ],
                )?;
            }
            for event in &save.events {
                self.db.execute(
                    "INSERT INTO events(id,time,data) VALUES(?,?,?)",
                    params![sql_id(event.id)?, event.time, serde_json::to_string(event)?],
                )?;
            }
            self.checkpoint(&save.world)
        })();
        match result {
            Ok(()) => self.commit(own),
            Err(e) => {
                self.rollback(own);
                Err(e)
            }
        }
    }
}

pub fn validate_snapshot(save: &SaveData) -> Result<()> {
    assert_world(&save.world)?;
    ensure!(save.format == 5, "Unsupported save format.");
    ensure!(
        save.world.revision < u64::MAX,
        "World revision is exhausted."
    );
    let actors: HashSet<_> = save.world.actors.iter().map(|a| a.id).collect();
    let mut memory_ids = HashSet::new();
    let mut event_ids = HashSet::new();
    for event in &save.events {
        ensure!(
            event.id > 0
                && event.id <= i64::MAX as u64
                && event_ids.insert(event.id)
                && event.time.is_finite()
                && event.time >= 0.0
                && event.actor_id.is_none_or(|id| actors.contains(&id))
                && event.audience.iter().all(|id| actors.contains(id))
                && matches!(
                    event.kind.as_str(),
                    "action" | "speech" | "notice" | "injury" | "death" | "private"
                ),
            "Invalid saved event."
        );
        let mut observers = HashSet::new();
        for perception in &event.perceptions {
            ensure!(
                event.audience.contains(&perception.actor_id)
                    && observers.insert(perception.actor_id)
                    && save.world.rooms.iter().any(|r| r.id == perception.room_id)
                    && perception.source_id.is_none_or(|id| actors.contains(&id)),
                "Invalid hearing projection."
            );
        }
    }
    for memory in &save.memories {
        ensure!(
            memory.id > 0
                && memory.id <= i64::MAX as u64
                && memory_ids.insert(memory.id)
                && actors.contains(&memory.actor_id)
                && memory.time.is_finite()
                && memory.time >= 0.0
                && (1..=10).contains(&memory.importance)
                && matches!(
                    memory.kind.as_str(),
                    "background" | "seen" | "heard" | "belief" | "reflection"
                )
                && memory.event_id.is_none_or(|id| event_ids.contains(&id)),
            "Invalid saved memory."
        );
    }
    for memory in &save.memories {
        if memory.evidence_ids.iter().any(|id| {
            !save
                .memories
                .iter()
                .any(|m| m.id == *id && m.actor_id == memory.actor_id)
        }) {
            bail!("Saved mental evidence crosses character boundaries.");
        }
    }
    Ok(())
}
