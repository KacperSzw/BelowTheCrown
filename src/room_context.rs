//! A bounded chronology of one actor's own perceptions, not shared dialogue state.
use crate::model::*;
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

#[derive(Clone, Copy)]
pub struct ContextBudget {
    pub speech: usize,
    pub events: usize,
    pub bytes: usize,
}
impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            speech: 32,
            events: 8,
            bytes: 16 * 1024,
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomContext {
    pub room_id: u16,
    pub visit_event_id: u64,
    pub omitted_older_entries: bool,
    pub entries: Vec<ContextEntry>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEntry {
    pub id: u64,
    pub event_id: u64,
    pub last_event_id: u64,
    pub time: f64,
    pub last_time: f64,
    pub clock: String,
    pub age_calendar_seconds: f64,
    pub last_age_calendar_seconds: f64,
    pub kind: String,
    pub source_id: Option<u16>,
    pub text: String,
    pub perceived: Option<PerceivedEvent>,
    pub legacy_metadata_unknown: bool,
    pub count: u64,
    pub evidence_ids: Vec<u64>,
    #[serde(skip)]
    last_memory_id: u64,
}
impl RoomContext {
    pub fn evidence_ids(&self) -> Vec<u64> {
        self.entries
            .iter()
            .flat_map(|entry| entry.evidence_ids.iter().copied())
            .collect()
    }
    pub fn excluded_ranges(&self) -> Vec<[u64; 2]> {
        self.entries
            .iter()
            .map(|entry| [entry.id, entry.last_memory_id])
            .collect()
    }
}
pub fn room_context(
    store: &Store,
    actor: &Actor,
    now: f64,
    language: &str,
    budget: ContextBudget,
) -> Result<RoomContext> {
    let mut context = RoomContext {
        room_id: actor.room_id,
        visit_event_id: actor.entered_event_id,
        omitted_older_entries: false,
        entries: vec![],
    };
    anyhow::ensure!(
        serde_json::to_vec(&context)?.len() <= budget.bytes,
        "Room context budget is smaller than its header."
    );
    // Keep non-event memories as grouping barriers: a grouped ID range must not
    // swallow an intervening private belief when automatic recall excludes it.
    let mut statement = store.db.prepare("SELECT m.data, e.data FROM memories m LEFT JOIN events e ON e.id=json_extract(m.data,'$.eventId') WHERE m.actor_id=?1 AND (e.id>=?2 OR e.id IS NULL) ORDER BY m.id")?;
    let rows = statement.query_map(
        params![actor.id, i64::try_from(actor.entered_event_id)?],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
    )?;
    let mut consecutive = false;
    for row in rows {
        let (memory, event) = row?;
        let Some(event) = event else {
            consecutive = false;
            continue;
        };
        let memory: Memory = serde_json::from_str(&memory)?;
        let event: WorldEvent = serde_json::from_str(&event)?;
        let Some(projected) = event.projected(actor.id) else {
            consecutive = false;
            continue;
        };
        let public_event = matches!(
            projected.details,
            Some(
                EventDetails::Entered
                    | EventDetails::Left
                    | EventDetails::Injury
                    | EventDetails::Death
            )
        );
        if projected.room_id != Some(actor.room_id) || !(memory.kind == "heard" || public_event) {
            consecutive = false;
            continue;
        }
        let perceived = projected
            .room_id
            .zip(projected.details)
            .map(|(room_id, details)| PerceivedEvent { room_id, details });
        if consecutive
            && memory.kind == "heard"
            && memory.source_id.is_some()
            && let Some(last) = context.entries.last_mut()
            && last.kind == memory.kind
            && last.source_id == memory.source_id
            && last.text == memory.text
            && last.perceived == perceived
        {
            last.count += 1;
            last.last_event_id = event.id;
            last.last_memory_id = memory.id;
            last.last_time = memory.time;
            last.last_age_calendar_seconds = (now - memory.time).max(0.0);
            last.evidence_ids.push(memory.id);
            if last.evidence_ids.len() > 4 {
                last.evidence_ids.remove(2);
            }
            continue;
        }
        let speech = memory.kind == "heard";
        context.entries.push(ContextEntry {
            id: memory.id,
            event_id: event.id,
            last_event_id: event.id,
            time: memory.time,
            last_time: memory.time,
            clock: clock_text(memory.time, language),
            age_calendar_seconds: (now - memory.time).max(0.0),
            last_age_calendar_seconds: (now - memory.time).max(0.0),
            kind: memory.kind,
            source_id: memory.source_id,
            text: memory.text,
            legacy_metadata_unknown: perceived.is_none(),
            perceived,
            count: 1,
            evidence_ids: vec![memory.id],
            last_memory_id: memory.id,
        });
        let limit = if speech { budget.speech } else { budget.events };
        if context
            .entries
            .iter()
            .filter(|entry| (entry.kind == "heard") == speech)
            .count()
            > limit
        {
            if let Some(index) = context
                .entries
                .iter()
                .position(|entry| (entry.kind == "heard") == speech)
            {
                context.entries.remove(index);
            }
            context.omitted_older_entries = true;
        }
        consecutive = speech;
    }
    while serde_json::to_vec(&context)?.len() > budget.bytes && !context.entries.is_empty() {
        context.entries.remove(0);
        context.omitted_older_entries = true;
    }
    Ok(context)
}
