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
pub const PHYSIOLOGY_RATE: f64 = 5.0;
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
        #[serde(default)]
        quantity: u8,
    },
    Steal {
        item_id: u16,
        #[serde(default)]
        quantity: u8,
    },
    Drop {
        item_id: u16,
        #[serde(default)]
        quantity: u8,
    },
    Equip {
        item_id: u16,
    },
    Give {
        item_id: u16,
        target_id: u16,
        #[serde(default)]
        quantity: u8,
    },
    Offer {
        item_id: u16,
        target_id: u16,
        #[serde(default)]
        quantity: u8,
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
        // Zero is a plan-local reference, never an entity ID. The engine resolves
        // it only after a successful acquisition in the same active plan.
        for id in [self.door_id(), self.target_id()].into_iter().flatten() {
            ensure!(id > 0, "Entity IDs must be positive.");
        }
        match self {
            Self::AcceptOffer { offer_id } | Self::DeclineOffer { offer_id } => {
                ensure!(*offer_id > 0, "Invalid offer ID.")
            }
            Self::Pay { amount, .. } => {
                ensure!((1..=1000).contains(amount), "Payment must be 1–1000 coins.")
            }
            Self::Rest { seconds } => ensure!(
                (5..=8640).contains(seconds),
                "Rest duration must be 5–8640 calendar seconds."
            ),
            Self::Wait { seconds }
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
            Self::Take { item_id, .. }
            | Self::Steal { item_id, .. }
            | Self::Drop { item_id, .. }
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
    pub fn quantity(&self) -> Option<u8> {
        match self {
            Self::Take { quantity, .. }
            | Self::Steal { quantity, .. }
            | Self::Drop { quantity, .. }
            | Self::Give { quantity, .. }
            | Self::Offer { quantity, .. } => Some(*quantity),
            _ => None,
        }
    }
    pub fn resolve_item(&mut self, acquired: u16) -> Result<()> {
        if self.item_id() != Some(0) {
            return Ok(());
        }
        ensure!(acquired > 0, "No item has been acquired in this plan.");
        match self {
            Self::Take { item_id, .. }
            | Self::Steal { item_id, .. }
            | Self::Drop { item_id, .. }
            | Self::Equip { item_id }
            | Self::Give { item_id, .. }
            | Self::Offer { item_id, .. }
            | Self::Eat { item_id }
            | Self::Drink { item_id }
            | Self::Use { item_id, .. } => *item_id = acquired,
            _ => unreachable!(),
        }
        Ok(())
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

/// Player speech uses visible emphasis; NPCs choose their own explicit volume.
pub fn player_speech_volume(text: &str) -> Volume {
    let text = text.trim_end();
    if text.ends_with("!!") {
        return Volume::Shout;
    }
    let mut cased = 0u16;
    for character in text.chars() {
        if character.is_lowercase() {
            return Volume::Normal;
        }
        if character.is_uppercase() {
            cased = cased.saturating_add(1);
        }
    }
    if cased >= 2 {
        Volume::Shout
    } else {
        Volume::Normal
    }
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
    #[serde(default)]
    pub accept_heard_proposals: Vec<HeardProposal>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HeardProposal {
    pub memory_id: u64,
    pub victim_id: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NpcIdentity {
    pub actor_id: u16,
    pub name: String,
    pub role: String,
    pub biography: String,
    pub personality: String,
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
        check_new_plan(&interpretation.plan)?;
        Ok(interpretation)
    }
}
pub fn check_new_plan(plan: &[Intent]) -> Result<()> {
    let mut acquired = false;
    for intent in plan {
        ensure!(
            !matches!(
                intent,
                Intent::Guard { .. }
                    | Intent::Use {
                        mode: UseMode::Disguise | UseMode::CutBonds,
                        ..
                    }
            ),
            "Use wait, equip, or release instead of a retired action."
        );
        ensure!(
            intent.item_id() != Some(0) || acquired,
            "An item reference must follow an acquisition in this plan."
        );
        acquired |= matches!(
            intent,
            Intent::Take { .. } | Intent::Steal { .. } | Intent::AcceptOffer { .. }
        );
    }
    Ok(())
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
        let mut decision: Self = serde_json::from_value(raw)?;
        check_text(&decision.summary, 300)?;
        ensure!(
            decision.goals.len() <= 5
                && decision.plan.len() <= 6
                && decision.memories.len() <= 4
                && decision.relationships.len() <= 4
                && decision.accept_plots.len() <= 3
                && decision.accept_heard_proposals.len() <= 3,
            "Invalid decision lengths."
        );
        for goal in &decision.goals {
            check_text(goal, 240)?;
        }
        for intent in &decision.plan {
            intent.check()?;
        }
        check_new_plan(&decision.plan)?;
        // Exact adjacent copies add no distinct words or intention. Keep one;
        // physical actions and differing speeches still go through validation.
        decision
            .plan
            .dedup_by(|a, b| matches!(a, Intent::Speak { .. }) && a == b);
        ensure!(
            decision
                .plan
                .iter()
                .filter(|intent| matches!(intent, Intent::Speak { .. }))
                .count()
                <= 1,
            "Speak at most once per decision; combine your words into one speech."
        );
        ensure!(
            !decision.plan.windows(2).any(|pair| {
                pair[0] == pair[1]
                    && matches!(
                        pair[0],
                        Intent::Wait { .. }
                            | Intent::Guard { .. }
                            | Intent::Rest { .. }
                            | Intent::Hide { .. }
                    )
            }),
            "Choose one duration for a waiting activity instead of repeating identical steps."
        );
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
                decision
                    .relationships
                    .iter()
                    .filter(|r| r.target_id == update.target_id)
                    .count()
                    == 1,
                "Each relationship target may occur only once."
            );
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
        for proposal in &decision.accept_heard_proposals {
            ensure!(
                proposal.memory_id > 0 && proposal.victim_id > 0,
                "Invalid heard proposal."
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_vitals: Option<[f64; 4]>,
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
    #[serde(default)]
    pub last_acquired_item_id: u16,
    pub accepted_plots: Vec<u16>,
    pub decision_version: u64,
    pub unconscious_until: Option<f64>,
    #[serde(default)]
    pub entered_event_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action_outcome: Option<ActionOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_plan_failure: Option<ActionOutcome>,
    #[serde(default)]
    pub correction_used: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionOutcome {
    pub intent: Intent,
    pub ok: bool,
    pub reason: String,
    pub time: f64,
    #[serde(default)]
    pub stage: OutcomeStage,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum OutcomeStage {
    #[default]
    Legacy,
    Accepted,
    Started,
    Resolved,
    Failed,
    Interrupted,
}
impl Actor {
    pub fn clear_plan(&mut self) {
        self.plan.clear();
        self.last_acquired_item_id = 0;
    }
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
    Object,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum TextOrigin {
    Authored(u16),
    Recipe(Recipe),
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
    pub text_origin: Option<TextOrigin>,
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
    #[serde(default)]
    pub quantity: u8,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perceived: Option<PerceivedEvent>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PerceivedEvent {
    pub room_id: u16,
    #[serde(flatten)]
    pub details: EventDetails,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum EventDetails {
    Speech {
        speaker_name: Option<String>,
        recipient_id: Option<u16>,
        recipient_name: Option<String>,
        #[serde(default)]
        recipient_known: bool,
        volume: Volume,
    },
    Entered,
    Left,
    Injury,
    Death,
    ActivityStarted,
    ExecutionFeedback,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Perception {
    pub actor_id: u16,
    pub room_id: u16,
    pub source_id: Option<u16>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<EventDetails>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<EventDetails>,
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
            result.details = perception.details.clone();
        }
        result.audience = vec![observer];
        result.perceptions.clear();
        Some(result)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum WakePriority {
    Routine,
    Conversation,
    Danger,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum WakeCause {
    #[default]
    External,
    Initialization,
    ActivityEnded,
    PlanFailed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wake {
    pub actor_id: u16,
    pub reason: String,
    pub priority: WakePriority,
    pub version: u64,
    #[serde(default)]
    pub cause: WakeCause,
}
impl Wake {
    pub fn urgent(&self) -> bool {
        self.priority == WakePriority::Danger
    }
    pub fn merge(&mut self, newer: Self) {
        self.version = newer.version;
        // Failure feedback remains private on the actor even when conversation wins.
        if newer.cause == WakeCause::External || self.cause != WakeCause::External {
            self.cause = newer.cause;
        }
        if newer.priority >= self.priority {
            self.reason = newer.reason;
            self.priority = newer.priority;
        }
    }
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
            hunger_per_hour: 12.0,
            thirst_per_hour: 18.0,
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
pub fn number_text(n: f64) -> String {
    if n > 0.0 && n < 0.01 {
        return "<0.01".into();
    }
    let text = format!("{n:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}
pub fn attack_damage(
    combat: f64,
    weapon: f64,
    hidden: bool,
    fatigue: f64,
    target_combat: f64,
    defending: bool,
) -> f64 {
    let base = (8.0 + combat * 0.24 + weapon + if hidden { 18.0 } else { 0.0 }
        - fatigue * 0.06
        - target_combat * 0.1)
        .round()
        .max(4.0);
    if defending { base / 2.0 } else { base }
}
pub fn subdued_health(health: f64, damage: f64) -> f64 {
    (health - damage).max(10.0).min(health)
}
pub fn theft_chance(
    stealth: f64,
    hidden: bool,
    owner_stealth: f64,
    owner_combat: f64,
    owner_fatigue: f64,
) -> f64 {
    (0.45
        + (stealth - owner_stealth - owner_combat / 3.0 + owner_fatigue / 2.0) / 120.0
        + if hidden { 0.25 } else { 0.0 })
    .clamp(0.08, 0.9)
}
pub fn lockpick_chance(craft: f64, stealth: f64, royal: bool) -> f64 {
    (0.3 + craft / 150.0 + stealth / 300.0 - if royal { 0.3 } else { 0.0 }).clamp(0.1, 0.9)
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
