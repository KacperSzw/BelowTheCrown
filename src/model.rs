use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PLAYER: u16 = 101;
pub const KING: u16 = 103;
pub const DUNGEON: u16 = 1;
pub const MODEL: &str = "gpt-6-luna";
pub const EFFORT: &str = "high";
pub const CLOCK_RATE: f64 = 24.0;
pub const HIDDEN: u8 = 1;
pub const DISGUISED: u8 = 2;
pub const RESTRAINED: u8 = 4;
pub const UNCONSCIOUS: u8 = 8;
pub const OPEN: u8 = 1;
pub const LOCKED: u8 = 2;
pub const BARS: u8 = 4;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Intent {
    Move {
        door_id: u16,
    },
    Flee {
        door_id: u16,
    },
    Inspect {
        target_id: u16,
    },
    Take {
        item_id: u16,
    },
    Steal {
        item_id: u16,
    },
    Drop {
        item_id: u16,
    },
    Equip {
        item_id: u16,
    },
    Give {
        item_id: u16,
        target_id: u16,
    },
    Offer {
        item_id: u16,
        target_id: u16,
    },
    Pay {
        target_id: u16,
        amount: u16,
    },
    AcceptOffer {
        offer_id: u16,
    },
    DeclineOffer {
        offer_id: u16,
    },
    Unlock {
        door_id: u16,
    },
    Lock {
        door_id: u16,
    },
    Open {
        door_id: u16,
    },
    Close {
        door_id: u16,
    },
    Eat {
        item_id: u16,
    },
    Drink {
        item_id: u16,
    },
    Rest {
        seconds: u16,
    },
    Wait {
        seconds: u16,
    },
    Guard {
        seconds: u16,
    },
    Defend {
        seconds: u16,
    },
    Hide {
        seconds: u16,
    },
    Work {
        recipe: Recipe,
    },
    Attack {
        target_id: u16,
        style: AttackStyle,
    },
    Bind {
        target_id: u16,
    },
    Release {
        target_id: u16,
    },
    Escort {
        target_id: u16,
        door_id: u16,
    },
    Use {
        item_id: u16,
        target_id: u16,
        mode: UseMode,
    },
    Speak {
        text: String,
        target_id: Option<u16>,
        volume: Volume,
        plot: Option<PlotIntent>,
    },
}

impl Intent {
    pub fn parse(value: Value) -> Result<Self> {
        let intent: Self = serde_json::from_value(value)?;
        intent.check()?;
        Ok(intent)
    }
    pub fn check(&self) -> Result<()> {
        for id in [self.item_id(), self.door_id(), self.target_id()]
            .into_iter()
            .flatten()
        {
            ensure!(id > 0, "Entity IDs must be positive.");
        }
        match self {
            Self::AcceptOffer { offer_id } | Self::DeclineOffer { offer_id } => {
                ensure!(*offer_id > 0, "Invalid offer ID.")
            }
            Self::Pay { amount, .. } => {
                ensure!((1..=1000).contains(amount), "Payment must be 1–1000 coins.")
            }
            Self::Rest { seconds }
            | Self::Wait { seconds }
            | Self::Guard { seconds }
            | Self::Defend { seconds }
            | Self::Hide { seconds } => ensure!(
                (5..=1800).contains(seconds),
                "Activity duration must be 5–1800 game seconds."
            ),
            Self::Speak { text, plot, .. } => {
                check_text(text, 1200)?;
                if let Some(plot) = plot {
                    ensure!(plot.victim_id > 0, "Invalid victim ID.");
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Move { .. } => "move",
            Self::Flee { .. } => "flee",
            Self::Inspect { .. } => "inspect",
            Self::Take { .. } => "take",
            Self::Steal { .. } => "steal",
            Self::Drop { .. } => "drop",
            Self::Equip { .. } => "equip",
            Self::Give { .. } => "give",
            Self::Offer { .. } => "offer",
            Self::Pay { .. } => "pay",
            Self::AcceptOffer { .. } => "accept_offer",
            Self::DeclineOffer { .. } => "decline_offer",
            Self::Unlock { .. } => "unlock",
            Self::Lock { .. } => "lock",
            Self::Open { .. } => "open",
            Self::Close { .. } => "close",
            Self::Eat { .. } => "eat",
            Self::Drink { .. } => "drink",
            Self::Rest { .. } => "rest",
            Self::Wait { .. } => "wait",
            Self::Guard { .. } => "guard",
            Self::Defend { .. } => "defend",
            Self::Hide { .. } => "hide",
            Self::Work { .. } => "work",
            Self::Attack { .. } => "attack",
            Self::Bind { .. } => "bind",
            Self::Release { .. } => "release",
            Self::Escort { .. } => "escort",
            Self::Use { .. } => "use",
            Self::Speak { .. } => "speak",
        }
    }
    pub fn item_id(&self) -> Option<u16> {
        match self {
            Self::Take { item_id }
            | Self::Steal { item_id }
            | Self::Drop { item_id }
            | Self::Equip { item_id }
            | Self::Give { item_id, .. }
            | Self::Offer { item_id, .. }
            | Self::Eat { item_id }
            | Self::Drink { item_id }
            | Self::Use { item_id, .. } => Some(*item_id),
            _ => None,
        }
    }
    pub fn door_id(&self) -> Option<u16> {
        match self {
            Self::Move { door_id }
            | Self::Flee { door_id }
            | Self::Unlock { door_id }
            | Self::Lock { door_id }
            | Self::Open { door_id }
            | Self::Close { door_id }
            | Self::Escort { door_id, .. } => Some(*door_id),
            _ => None,
        }
    }
    pub fn target_id(&self) -> Option<u16> {
        match self {
            Self::Inspect { target_id }
            | Self::Give { target_id, .. }
            | Self::Offer { target_id, .. }
            | Self::Pay { target_id, .. }
            | Self::Attack { target_id, .. }
            | Self::Bind { target_id }
            | Self::Release { target_id }
            | Self::Escort { target_id, .. }
            | Self::Use { target_id, .. } => Some(*target_id),
            Self::Speak { target_id, .. } => *target_id,
            _ => None,
        }
    }
    pub fn seconds(&self) -> Option<u16> {
        match self {
            Self::Rest { seconds }
            | Self::Wait { seconds }
            | Self::Guard { seconds }
            | Self::Defend { seconds }
            | Self::Hide { seconds } => Some(*seconds),
            _ => None,
        }
    }
    pub fn long(&self) -> bool {
        matches!(
            self,
            Self::Rest { .. }
                | Self::Wait { .. }
                | Self::Guard { .. }
                | Self::Defend { .. }
                | Self::Hide { .. }
        )
    }
    pub fn combat_move(&self) -> bool {
        matches!(
            self,
            Self::Attack { .. } | Self::Defend { .. } | Self::Flee { .. }
        )
    }
    pub fn travel(&self) -> bool {
        matches!(
            self,
            Self::Move { .. } | Self::Flee { .. } | Self::Escort { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Recipe {
    Meal,
    Lockpick,
    Medicine,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttackStyle {
    Lethal,
    Subdue,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UseMode {
    Heal,
    Poison,
    PickLock,
    Disguise,
    Trap,
    CutBonds,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Volume {
    Normal,
    Whisper,
    Shout,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlotIntent {
    pub victim_id: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Decision {
    pub summary: String,
    pub goals: Vec<String>,
    pub plan: Vec<Intent>,
    pub memories: Vec<MentalMemory>,
    pub relationships: Vec<RelationshipUpdate>,
    pub accept_plots: Vec<u16>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MentalMemory {
    pub text: String,
    pub kind: String,
    pub evidence_ids: Vec<u64>,
    pub importance: u8,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelationshipUpdate {
    pub target_id: u16,
    pub trust: i8,
    pub affection: i8,
    pub fear: i8,
    pub respect: i8,
    pub evidence_ids: Vec<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interpretation {
    pub summary: String,
    pub plan: Vec<Intent>,
}
impl Interpretation {
    pub fn parse(raw: Value) -> Result<Self> {
        let interpretation: Self = serde_json::from_value(raw)?;
        check_text(&interpretation.summary, 300)?;
        ensure!(
            interpretation.plan.len() <= 3,
            "Interpretations contain at most three physical actions."
        );
        for intent in &interpretation.plan {
            intent.check()?;
            ensure!(
                !matches!(intent, Intent::Speak { .. }),
                "An interpretation cannot invent speech."
            );
        }
        Ok(interpretation)
    }
}

pub fn check_text(text: &str, maximum: usize) -> Result<()> {
    ensure!(
        !text.trim().is_empty() && text.chars().count() <= maximum,
        "Text must contain 1–{maximum} characters."
    );
    ensure!(
        !text
            .chars()
            .any(|c| (c.is_control() && c != '\n' && c != '\t')
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
        "Text contains terminal control characters."
    );
    Ok(())
}
impl Decision {
    pub fn parse(raw: Value) -> Result<Self> {
        let decision: Self = serde_json::from_value(raw)?;
        check_text(&decision.summary, 300)?;
        ensure!(
            decision.goals.len() <= 5
                && (1..=6).contains(&decision.plan.len())
                && decision.memories.len() <= 4
                && decision.relationships.len() <= 4
                && decision.accept_plots.len() <= 3,
            "Invalid decision lengths."
        );
        for goal in &decision.goals {
            check_text(goal, 240)?;
        }
        for intent in &decision.plan {
            intent.check()?;
        }
        for memory in &decision.memories {
            check_text(&memory.text, 600)?;
            ensure!(
                matches!(memory.kind.as_str(), "belief" | "reflection")
                    && (1..=10).contains(&memory.importance)
                    && (1..=8).contains(&memory.evidence_ids.len())
                    && memory.evidence_ids.iter().all(|id| *id > 0),
                "Invalid mental memory."
            );
        }
        for update in &decision.relationships {
            ensure!(
                update.target_id > 0
                    && (1..=8).contains(&update.evidence_ids.len())
                    && update.evidence_ids.iter().all(|id| *id > 0),
                "Invalid relationship evidence."
            );
            ensure!(
                [update.trust, update.affection, update.fear, update.respect]
                    .iter()
                    .all(|n| (-15..=15).contains(n)),
                "Relationship changes must be between -15 and 15."
            );
        }
        Ok(decision)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    pub id: u16,
    pub name: String,
    pub description: String,
    pub cover: bool,
    pub prison: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Door {
    pub id: u16,
    pub name: String,
    pub rooms: [u16; 2],
    pub flags: u8,
    pub lock: Option<String>,
    pub restricted: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Relationship {
    pub trust: f64,
    pub affection: f64,
    pub fear: f64,
    pub respect: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skills {
    pub combat: f64,
    pub stealth: f64,
    pub craft: f64,
    pub medicine: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub intent: Intent,
    pub started: f64,
    pub ends: f64,
    pub causes: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_ends: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CombatContext {
    pub encounter_id: String,
    pub round: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CombatParticipant {
    pub actor_id: u16,
    pub joins_round: u32,
    pub choice: Option<Intent>,
    pub causes: Vec<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CombatEncounter {
    pub id: String,
    pub room_id: u16,
    pub round: u32,
    pub deadline: f64,
    pub participants: Vec<CombatParticipant>,
    pub hostility: Vec<[u16; 2]>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    pub id: u16,
    pub name: String,
    pub role: String,
    pub biography: String,
    pub personality: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portrait_key: Option<String>,
    pub room_id: u16,
    pub health: f64,
    pub hunger: f64,
    pub thirst: f64,
    pub fatigue: f64,
    pub flags: u8,
    pub coins: u16,
    pub equipment: Vec<u16>,
    pub skills: Skills,
    pub known_rooms: Vec<u16>,
    pub known_actors: Vec<u16>,
    pub goals: Vec<String>,
    pub relationships: BTreeMap<u16, Relationship>,
    pub summary: String,
    pub activity: Option<Activity>,
    pub plan: Vec<Intent>,
    pub accepted_plots: Vec<u16>,
    pub decision_version: u64,
    pub unconscious_until: Option<f64>,
    #[serde(default)]
    pub entered_event_id: u64,
}
impl Actor {
    pub fn invalidate_decision(&mut self) -> Result<()> {
        self.decision_version = self
            .decision_version
            .checked_add(1)
            .context("Decision version overflow.")?;
        Ok(())
    }
    pub fn alive(&self) -> bool {
        self.health > 0.0
    }
    pub fn awake(&self) -> bool {
        self.alive() && self.flags & UNCONSCIOUS == 0
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanPlayer {
    pub identity: String,
    pub actor_id: u16,
    pub listener: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Key,
    Weapon,
    Food,
    Water,
    Ingredient,
    Metal,
    Herb,
    Medicine,
    Poison,
    Lockpick,
    Clothes,
    Rope,
    Document,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: u16,
    pub name: String,
    pub description: String,
    pub kind: ItemKind,
    pub quantity: u8,
    pub owner_id: Option<u16>,
    pub room_id: Option<u16>,
    pub equipped: bool,
    pub visible: bool,
    pub key: Option<String>,
    pub power: f64,
    #[serde(default)]
    pub poisoned_by: Option<u16>,
    #[serde(default)]
    pub causes: Vec<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    pub id: u16,
    pub from: u16,
    pub to: u16,
    pub item_id: u16,
    pub expires: f64,
    pub event_id: u64,
    pub status: String,
    #[serde(default)]
    pub interaction_expires: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plot {
    pub id: u16,
    pub proposer: u16,
    pub recipient: u16,
    pub victim: u16,
    pub event_id: u64,
    pub accepted: bool,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub accepted_event_id: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trap {
    pub door_id: u16,
    pub setter: u16,
    pub power: f64,
    pub causes: Vec<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoisonEffect {
    pub actor_id: u16,
    pub source_id: u16,
    pub damage: f64,
    pub next: f64,
    pub remaining: u8,
    pub causes: Vec<u64>,
    #[serde(default)]
    pub interaction_next: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldState {
    pub language: String,
    pub scenario_id: String,
    pub format: u8,
    pub mode: String,
    pub players: Vec<HumanPlayer>,
    pub winner_id: Option<u16>,
    pub branch: String,
    pub time: f64,
    pub interaction_time: f64,
    pub encounters: Vec<CombatEncounter>,
    pub revision: u64,
    pub rng: u32,
    pub next_item_id: u32,
    pub next_offer_id: u32,
    pub next_plot_id: u32,
    pub rooms: Vec<Room>,
    pub doors: Vec<Door>,
    pub actors: Vec<Actor>,
    pub items: Vec<Item>,
    pub offers: Vec<Offer>,
    pub plots: Vec<Plot>,
    pub traps: Vec<Trap>,
    pub poison: Vec<PoisonEffect>,
    pub escaped: bool,
    pub king_killed_by_player: bool,
    pub ending: String,
    pub manual_pause: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub id: u64,
    pub actor_id: u16,
    pub time: f64,
    pub event_id: Option<u64>,
    pub kind: String,
    pub text: String,
    pub importance: u8,
    pub source_id: Option<u16>,
    pub evidence_ids: Vec<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Perception {
    pub actor_id: u16,
    pub room_id: u16,
    pub source_id: Option<u16>,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldEvent {
    pub id: u64,
    pub time: f64,
    pub kind: String,
    pub actor_id: Option<u16>,
    pub room_id: Option<u16>,
    pub text: String,
    pub audience: Vec<u16>,
    pub causes: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub perceptions: Vec<Perception>,
}
impl WorldEvent {
    pub fn projected(&self, observer: u16) -> Option<Self> {
        if !self.audience.contains(&observer) {
            return None;
        }
        let mut result = self.clone();
        if let Some(perception) = self.perceptions.iter().find(|p| p.actor_id == observer) {
            result.text = perception.text.clone();
            result.room_id = Some(perception.room_id);
            result.actor_id = perception.source_id;
        }
        result.audience = vec![observer];
        result.perceptions.clear();
        Some(result)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wake {
    pub actor_id: u16,
    pub reason: String,
    pub urgent: bool,
    pub version: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    pub ok: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<u64>,
}
impl ActionResult {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            event_id: None,
        }
    }
    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
            event_id: None,
        }
    }
    pub fn value(self) -> Value {
        serde_json::to_value(self).expect("action result serialization")
    }
}
#[derive(Clone, Debug)]
pub struct Tuning {
    pub hunger_per_hour: f64,
    pub thirst_per_hour: f64,
    pub fatigue_per_hour: f64,
    pub rest_per_hour: f64,
    pub need_thresholds: Vec<f64>,
    pub action_seconds: f64,
    pub travel_seconds: f64,
    pub combat_round_seconds: f64,
    pub recovery_seconds: f64,
    pub craft_seconds: f64,
    pub lockpick_seconds: f64,
    pub offer_seconds: f64,
    pub poison_seconds: f64,
}
impl Default for Tuning {
    fn default() -> Self {
        Self {
            hunger_per_hour: 10.0,
            thirst_per_hour: 10.0,
            fatigue_per_hour: 8.0,
            rest_per_hour: 55.0,
            need_thresholds: vec![40.0, 70.0, 90.0],
            action_seconds: 0.6,
            travel_seconds: 2.0,
            combat_round_seconds: 30.0,
            recovery_seconds: 30.0,
            craft_seconds: 36.0,
            lockpick_seconds: 4.0,
            offer_seconds: 120.0,
            poison_seconds: 6.0,
        }
    }
}
impl Tuning {
    pub fn check(&self) -> Result<()> {
        ensure!(
            [
                self.hunger_per_hour,
                self.thirst_per_hour,
                self.fatigue_per_hour
            ]
            .iter()
            .all(|n| n.is_finite() && *n >= 0.0 && *n <= 100.0),
            "Invalid need tuning."
        );
        ensure!(
            [
                self.rest_per_hour,
                self.action_seconds,
                self.travel_seconds,
                self.combat_round_seconds,
                self.recovery_seconds,
                self.craft_seconds,
                self.lockpick_seconds,
                self.offer_seconds,
                self.poison_seconds
            ]
            .iter()
            .all(|n| n.is_finite() && *n > 0.0),
            "Invalid duration tuning."
        );
        if self.need_thresholds.is_empty()
            || self
                .need_thresholds
                .iter()
                .any(|n| !n.is_finite() || *n <= 0.0 || *n >= 100.0)
        {
            bail!("Invalid need thresholds.");
        }
        Ok(())
    }
}
pub fn clamp(n: f64) -> f64 {
    n.clamp(0.0, 100.0)
}
pub fn clock_text(time: f64, language: &str) -> String {
    let seconds = time.floor() as u64;
    format!(
        "{} {} · {:02}:{:02}",
        if language == "pl" { "Dzień" } else { "Day" },
        seconds / 86400 + 1,
        seconds / 3600 % 24,
        seconds / 60 % 60
    )
}
