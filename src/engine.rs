use crate::content::WorldContent;
use crate::model::*;
use crate::store::{Store, assert_world};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActorSeed {
    id: u16,
    name: String,
    role: String,
    biography: String,
    personality: String,
    room_id: u16,
    hunger: f64,
    thirst: f64,
    fatigue: f64,
    coins: u16,
    skills: Skills,
    goals: Vec<String>,
    known_rooms: Vec<u16>,
    known_actors: Vec<u16>,
    relationships: BTreeMap<u16, [f64; 4]>,
    knowledge: Vec<String>,
}
#[derive(Deserialize)]
struct Blueprint {
    introduction: String,
    time: f64,
    rooms: Vec<Room>,
    doors: Vec<Door>,
    actors: Vec<ActorSeed>,
    items: Vec<Item>,
}

#[derive(Clone)]
struct RecipeSpec {
    room: u16,
    minimum: f64,
    medicine: bool,
    ingredient: ItemKind,
    kind: ItemKind,
    name: &'static str,
    polish_name: &'static str,
    description: &'static str,
    polish_description: &'static str,
    power: f64,
}
fn recipe_spec(recipe: Recipe) -> RecipeSpec {
    match recipe {
        Recipe::Meal => RecipeSpec {
            room: 4,
            minimum: 20.0,
            medicine: false,
            ingredient: ItemKind::Ingredient,
            kind: ItemKind::Food,
            name: "Fresh barley stew",
            polish_name: "Świeża potrawka jęczmienna",
            description: "A bowl made from one actual ration of barley.",
            polish_description: "Miska przygotowana z jednej prawdziwej porcji jęczmienia.",
            power: 45.0,
        },
        Recipe::Lockpick => RecipeSpec {
            room: 8,
            minimum: 25.0,
            medicine: false,
            ingredient: ItemKind::Metal,
            kind: ItemKind::Lockpick,
            name: "Soft iron lockpick",
            polish_name: "Wytrych z miękkiego żelaza",
            description: "A workable pick, made from one iron scrap.",
            polish_description: "Użyteczny wytrych wykonany z jednego kawałka żelaza.",
            power: 0.0,
        },
        Recipe::Medicine => RecipeSpec {
            room: 7,
            minimum: 50.0,
            medicine: true,
            ingredient: ItemKind::Herb,
            kind: ItemKind::Medicine,
            name: "Herbal medicine",
            polish_name: "Lek ziołowy",
            description: "One prepared dose for wounds or poison.",
            polish_description: "Jedna przygotowana dawka na rany lub zatrucie.",
            power: 40.0,
        },
    }
}

struct NeedsProjection {
    hunger: f64,
    thirst: f64,
    fatigue: f64,
    health_before_damage: f64,
    damage: f64,
}
/// Pure physiology shared by advancement and conditional duration forecasts.
fn project_needs(actor: &Actor, tuning: &Tuning, seconds: f64, resting: bool) -> NeedsProjection {
    if !actor.alive() {
        return NeedsProjection {
            hunger: actor.hunger,
            thirst: actor.thirst,
            fatigue: actor.fatigue,
            health_before_damage: actor.health,
            damage: 0.0,
        };
    }
    let hours = seconds / 3600.0;
    let hunger = clamp(actor.hunger + hours * tuning.hunger_per_hour);
    let thirst = clamp(actor.thirst + hours * tuning.thirst_per_hour);
    let fatigue = clamp(
        actor.fatigue
            + hours
                * if resting {
                    -tuning.rest_per_hour
                } else {
                    tuning.fatigue_per_hour
                },
    );
    let health_before_damage =
        if resting && actor.flags & UNCONSCIOUS == 0 && hunger < 90.0 && thirst < 90.0 {
            clamp(actor.health + hours * 8.0)
        } else {
            actor.health
        };
    let beyond = |before: f64, rate: f64| {
        if rate > 0.0 {
            (hours - (100.0 - before) / rate).max(0.0)
        } else if before >= 100.0 {
            hours
        } else {
            0.0
        }
    };
    NeedsProjection {
        hunger,
        thirst,
        fatigue,
        health_before_damage,
        damage: beyond(actor.hunger, tuning.hunger_per_hour) * 12.0
            + beyond(actor.thirst, tuning.thirst_per_hour) * 25.0,
    }
}

/// Sole owner of physical state. Models and terminal clients only submit validated intents.
pub struct Engine {
    pub world: WorldState,
    pub store: Store,
    pub content: WorldContent,
    pub language: String,
    pub tuning: Tuning,
    wakes: BTreeMap<u16, Wake>,
    deferred_events: Vec<WorldEvent>,
    resolving_combat: bool,
}

pub fn player_name(input: &str) -> Result<String> {
    let name: String = input.trim().nfc().collect();
    ensure!(
        (2..=32).contains(&name.chars().count())
            && !name.chars().any(|c| c.is_control()
                || matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}'|'#'|'/')),
        "Choose a plain name of 2–32 characters."
    );
    Ok(name)
}

impl Engine {
    pub fn open(directory: &Path, scenario: &str, language: &str, seed: u32) -> Result<Self> {
        Self::from_store(
            Store::open(directory)?,
            scenario,
            language,
            seed,
            "multiplayer",
            Tuning::default(),
        )
    }
    pub fn memory(scenario: &str, language: &str, seed: u32) -> Result<Self> {
        Self::from_store(
            Store::memory()?,
            scenario,
            language,
            seed,
            "multiplayer",
            Tuning::default(),
        )
    }
    pub fn memory_with_tuning(
        scenario: &str,
        language: &str,
        seed: u32,
        mode: &str,
        tuning: Tuning,
    ) -> Result<Self> {
        Self::from_store(Store::memory()?, scenario, language, seed, mode, tuning)
    }
    fn from_store(
        store: Store,
        scenario: &str,
        language: &str,
        seed: u32,
        mode: &str,
        tuning: Tuning,
    ) -> Result<Self> {
        ensure!(matches!(language, "en" | "pl"), "Choose English or Polish.");
        tuning.check()?;
        let saved = store.load()?;
        let content =
            WorldContent::load(saved.as_ref().map_or(scenario, |world| &world.scenario_id))?;
        let (mut world, memories) = if let Some(world) = saved {
            (world, Vec::new())
        } else {
            Self::create_world(&content, language, seed, mode)?
        };
        Self::restore_portraits(&mut world);
        Self::restore_item_origins(&mut world, &content);
        let locale = world.language.clone();
        if store.load()?.is_none() {
            store.new_world(&world, &memories)?;
        } else {
            store.checkpoint(&world)?;
        }
        let mut engine = Self {
            world,
            store,
            content,
            language: locale,
            tuning,
            wakes: BTreeMap::new(),
            deferred_events: vec![],
            resolving_combat: false,
        };
        if engine.world.mode == "solo" && engine.store.events(None, 1)?.is_empty() {
            let introduction: Blueprint =
                serde_json::from_value(engine.content.canonical_blueprint().clone())?;
            let text = engine.authored(&introduction.introduction);
            engine.mutate(|s| {
                s.record(
                    "notice",
                    None,
                    Some(DUNGEON),
                    text,
                    Some(vec![PLAYER]),
                    vec![],
                    None,
                )?;
                Ok(())
            })?;
        }
        Ok(engine)
    }
    fn create_world(
        content: &WorldContent,
        language: &str,
        seed: u32,
        mode: &str,
    ) -> Result<(WorldState, Vec<Memory>)> {
        ensure!(matches!(mode, "solo" | "multiplayer"), "Unknown game mode.");
        let source: Blueprint = serde_json::from_value(content.canonical_blueprint().clone())
            .context("Invalid physical scenario blueprint.")?;
        let next_item_id = source
            .rooms
            .iter()
            .map(|x| x.id)
            .chain(source.doors.iter().map(|x| x.id))
            .chain(source.actors.iter().map(|x| x.id))
            .chain(source.items.iter().map(|x| x.id))
            .max()
            .map_or(1, |id| u32::from(id) + 1);
        let mut memories = Vec::new();
        let mut actors = Vec::new();
        for actor in source
            .actors
            .into_iter()
            .filter(|a| mode == "solo" || a.id != PLAYER)
        {
            for knowledge in &actor.knowledge {
                memories.push(Memory {
                    id: 0,
                    actor_id: actor.id,
                    time: source.time,
                    event_id: None,
                    kind: "background".into(),
                    text: content.authored(knowledge, language),
                    importance: 8,
                    source_id: Some(actor.id),
                    evidence_ids: vec![],
                    perceived: None,
                });
            }
            let equipment = source
                .items
                .iter()
                .filter(|i| i.owner_id == Some(actor.id) && i.equipped)
                .map(|i| i.id)
                .collect();
            let relationships = actor
                .relationships
                .into_iter()
                .filter(|(id, _)| mode == "solo" || *id != PLAYER)
                .map(|(id, r)| {
                    (
                        id,
                        Relationship {
                            trust: r[0],
                            affection: r[1],
                            fear: r[2],
                            respect: r[3],
                        },
                    )
                })
                .collect();
            actors.push(Actor {
                id: actor.id,
                name: actor.name,
                role: actor.role,
                biography: actor.biography,
                personality: actor.personality,
                portrait_key: None,
                room_id: actor.room_id,
                health: 100.0,
                hunger: actor.hunger,
                thirst: actor.thirst,
                fatigue: actor.fatigue,
                coins: actor.coins,
                flags: 0,
                equipment,
                skills: actor.skills,
                known_rooms: actor.known_rooms,
                known_actors: actor
                    .known_actors
                    .into_iter()
                    .filter(|id| mode == "solo" || *id != PLAYER)
                    .collect(),
                goals: actor.goals,
                relationships,
                summary: "A new evening begins.".into(),
                activity: None,
                plan: vec![],
                last_acquired_item_id: 0,
                accepted_plots: vec![],
                decision_version: 0,
                unconscious_until: None,
                entered_event_id: 0,
                last_action_outcome: None,
                last_plan_failure: None,
                recent_action_outcomes: vec![],
                correction_used: false,
            });
        }
        let mut used = Vec::new();
        for actor in &mut actors {
            let key = crate::portraits::portrait_key(actor.id, &used);
            used.push(key.clone());
            actor.portrait_key = Some(key);
        }
        let world = WorldState {
            format: 6,
            language: language.into(),
            scenario_id: content.id.clone(),
            mode: mode.into(),
            players: if mode == "solo" {
                vec![HumanPlayer {
                    identity: "local".into(),
                    actor_id: PLAYER,
                    listener: false,
                }]
            } else {
                vec![]
            },
            winner_id: None,
            branch: Uuid::new_v4().to_string(),
            time: source.time,
            interaction_time: 0.0,
            encounters: vec![],
            revision: 0,
            rng: seed.max(1),
            next_item_id,
            next_offer_id: 1,
            next_plot_id: 1,
            rooms: source.rooms,
            doors: source.doors,
            actors,
            items: source
                .items
                .into_iter()
                .map(|mut item| {
                    item.text_origin = Some(TextOrigin::Authored(item.id));
                    item
                })
                .filter(|i| mode == "solo" || i.owner_id != Some(PLAYER))
                .collect(),
            offers: vec![],
            plots: vec![],
            traps: vec![],
            poison: vec![],
            escaped: false,
            king_killed_by_player: false,
            ending: "playing".into(),
            manual_pause: false,
        };
        assert_world(&world)?;
        Ok((world, memories))
    }
    fn mutate<T>(&mut self, run: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let previous = self.world.clone();
        let wakes = self.wakes.clone();
        let events = self.deferred_events.clone();
        let own = self.store.begin()?;
        let result = (|| {
            let result = run(self)?;
            self.world.revision = self
                .world
                .revision
                .checked_add(1)
                .context("World revision overflow.")?;
            self.store.checkpoint(&self.world)?;
            self.store.commit(own)?;
            Ok(result)
        })();
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.store.rollback(own);
                self.world = previous;
                self.wakes = wakes;
                self.deferred_events = events;
                Err(error)
            }
        }
    }
    pub fn text(&self, en: &str, pl: &str) -> String {
        if self.language == "pl" {
            pl.into()
        } else {
            en.into()
        }
    }
    pub fn authored(&self, source: &str) -> String {
        self.content.authored(source, &self.language)
    }
    pub fn actor(&self, id: u16) -> Result<&Actor> {
        self.world
            .actors
            .iter()
            .find(|a| a.id == id)
            .context("Unknown character.")
    }
    pub fn actor_mut(&mut self, id: u16) -> Result<&mut Actor> {
        self.world
            .actors
            .iter_mut()
            .find(|a| a.id == id)
            .context("Unknown character.")
    }
    pub fn room(&self, id: u16) -> Result<&Room> {
        self.world
            .rooms
            .iter()
            .find(|r| r.id == id)
            .context("Unknown room.")
    }
    pub fn door(&self, id: u16) -> Option<&Door> {
        self.world.doors.iter().find(|d| d.id == id)
    }
    pub fn item(&self, id: u16) -> Option<&Item> {
        self.world.items.iter().find(|i| i.id == id)
    }
    pub fn is_player(&self, id: u16) -> bool {
        self.world.players.iter().any(|p| p.actor_id == id)
    }
    pub fn inventory(&self, id: u16) -> Vec<&Item> {
        self.world
            .items
            .iter()
            .filter(|i| i.owner_id == Some(id))
            .collect()
    }
    pub fn exits(&self, room: u16) -> Vec<&Door> {
        self.world
            .doors
            .iter()
            .filter(|d| d.rooms.contains(&room))
            .collect()
    }
    pub fn destination(&self, door: &Door, origin: u16) -> u16 {
        if door.rooms[0] == origin {
            door.rooms[1]
        } else {
            door.rooms[0]
        }
    }
    pub fn name(&self, id: u16) -> String {
        if let Ok(actor) = self.actor(id) {
            if self.is_player(id) && !(id == PLAYER && self.world.mode == "solo") {
                return actor.name.clone();
            }
            return self
                .content
                .actor_text(id, "name", &actor.name, &self.language);
        }
        if let Some(item) = self.item(id) {
            return self.item_text(item, "name");
        }
        for (kind, value) in [
            (
                "rooms",
                self.world
                    .rooms
                    .iter()
                    .find(|r| r.id == id)
                    .map(|r| r.name.as_str()),
            ),
            ("doors", self.door(id).map(|d| d.name.as_str())),
        ] {
            if let Some(name) = value {
                return self
                    .content
                    .entity_text(kind, id, "name", &self.language)
                    .unwrap_or_else(|| self.authored(name));
            }
        }
        id.to_string()
    }
    fn item_text(&self, item: &Item, field: &str) -> String {
        let source = if field == "name" {
            &item.name
        } else {
            &item.description
        };
        match item.text_origin {
            Some(TextOrigin::Authored(id)) => {
                self.content
                    .saved_entity_text("items", id, field, source, &self.language)
            }
            Some(TextOrigin::Recipe(recipe)) => {
                let spec = recipe_spec(recipe);
                let (en, pl) = if field == "name" {
                    (spec.name, spec.polish_name)
                } else {
                    (spec.description, spec.polish_description)
                };
                if source == en || source == pl {
                    self.text(en, pl)
                } else {
                    source.clone()
                }
            }
            None => source.clone(),
        }
    }
    fn restore_item_origins(world: &mut WorldState, content: &WorldContent) {
        for item in &mut world.items {
            if item.text_origin.is_some() {
                continue;
            }
            let mut candidates = Vec::new();
            for authored in content.scenario["items"].as_array().into_iter().flatten() {
                let Some(id) = authored["id"]
                    .as_u64()
                    .and_then(|id| u16::try_from(id).ok())
                else {
                    continue;
                };
                // These templates did not exist in formats 1–4. An old instance
                // with their ID must never acquire their identity merely by ID.
                if (900..=905).contains(&id) {
                    continue;
                }
                if serde_json::to_value(item.kind).ok().as_ref() == Some(&authored["kind"])
                    && (content.item_field_is_authored(id, "name", &item.name)
                        || content.item_field_is_authored(id, "description", &item.description))
                {
                    candidates.push(TextOrigin::Authored(id));
                }
            }
            for recipe in Recipe::ALL {
                let spec = recipe_spec(recipe);
                if item.kind == spec.kind
                    && (item.name == spec.name
                        || item.name == spec.polish_name
                        || item.description == spec.description
                        || item.description == spec.polish_description)
                {
                    candidates.push(TextOrigin::Recipe(recipe));
                }
            }
            if candidates.len() == 1 {
                item.text_origin = candidates.pop();
            }
        }
    }
    pub fn role(&self, actor: &Actor) -> String {
        self.content
            .actor_text(actor.id, "role", &actor.role, &self.language)
    }
    pub fn npc_identity(&self, id: u16) -> Result<NpcIdentity> {
        let actor = self.actor(id)?;
        ensure!(!self.is_player(id), "Player has no NPC identity");
        Ok(NpcIdentity {
            actor_id: id,
            name: self.name(id),
            role: self.role(actor),
            biography: self
                .content
                .actor_text(id, "biography", &actor.biography, &self.language),
            personality: self.content.actor_text(
                id,
                "personality",
                &actor.personality,
                &self.language,
            ),
        })
    }
    fn nearby(&self, id: u16) -> Result<Vec<&Actor>> {
        let actor = self.actor(id)?;
        Ok(self
            .world
            .actors
            .iter()
            .filter(|a| a.room_id == actor.room_id && (a.id == id || a.flags & HIDDEN == 0))
            .collect())
    }
    fn visible_items(&self, id: u16) -> Result<Vec<&Item>> {
        let actor = self.actor(id)?;
        let visible: HashSet<_> = self.nearby(id)?.iter().map(|a| a.id).collect();
        Ok(self
            .world
            .items
            .iter()
            .filter(|item| {
                item.owner_id == Some(id)
                    || item.room_id == Some(actor.room_id)
                    || item.owner_id.is_some_and(|owner| {
                        visible.contains(&owner)
                            && (item.visible || self.actor(owner).is_ok_and(|a| !a.alive()))
                    })
            })
            .collect())
    }
    fn same_room(&self, id: u16, target: u16) -> Option<&Actor> {
        self.nearby(id)
            .ok()?
            .into_iter()
            .find(|a| a.id == target && a.alive())
    }
    fn accessible(&self, id: u16, item: u16) -> Option<&Item> {
        self.visible_items(id)
            .ok()?
            .into_iter()
            .find(|i| i.id == item)
    }
    fn ingredient(&self, id: u16, kind: ItemKind) -> Option<&Item> {
        let room = self.actor(id).ok()?.room_id;
        self.world
            .items
            .iter()
            .find(|i| i.kind == kind && (i.owner_id == Some(id) || i.room_id == Some(room)))
    }
    fn witnesses(&self, room: u16) -> Vec<u16> {
        self.world
            .actors
            .iter()
            .filter(|a| a.room_id == room && a.awake())
            .map(|a| a.id)
            .collect()
    }
    pub fn random(&mut self) -> f64 {
        let mut x = self.world.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.world.rng = x;
        f64::from(x) / 4294967296.0
    }
    fn allocate_id(&mut self) -> Result<u16> {
        ensure!(
            self.world.next_item_id <= 65535,
            "The castle has reached its entity limit."
        );
        let id = self.world.next_item_id as u16;
        self.world.next_item_id += 1;
        Ok(id)
    }
    pub fn condition(&self, actor: &Actor) -> String {
        if !actor.alive() {
            return self.text("dead", "martwy");
        }
        if actor.flags & UNCONSCIOUS != 0 {
            return self.text("unconscious", "nieprzytomny");
        }
        let mut words = Vec::new();
        if actor.flags & RESTRAINED != 0 {
            words.push(self.text("restrained", "związany"));
        }
        if actor.health < 35.0 {
            words.push(self.text("badly wounded", "ciężko ranny"));
        } else if actor.health < 80.0 {
            words.push(self.text("wounded", "ranny"));
        }
        if actor.fatigue > 70.0 {
            words.push(self.text("exhausted", "wyczerpany"));
        }
        if words.is_empty() {
            self.text("apparently well", "wygląda zdrowo")
        } else {
            words.join(", ")
        }
    }
    fn felt_needs(&self, actor: &Actor) -> Value {
        let describe =
            |value: f64, mild: (&str, &str), strong: (&str, &str), urgent: (&str, &str)| {
                let crossed = self
                    .tuning
                    .need_thresholds
                    .iter()
                    .filter(|threshold| value >= **threshold)
                    .count();
                let words = match crossed {
                    0 => return self.text("not pressing", "nie dokucza"),
                    n if n == self.tuning.need_thresholds.len() => urgent,
                    1 => mild,
                    _ => strong,
                };
                self.text(words.0, words.1)
            };
        json!({
            "hunger":describe(actor.hunger, ("an empty stomach", "pusty żołądek"), ("persistent hunger", "dokuczliwy głód"), ("painful hunger", "bolesny głód")),
            "thirst":describe(actor.thirst, ("a dry mouth", "suchość w ustach"), ("persistent thirst", "dokuczliwe pragnienie"), ("intense thirst", "silne pragnienie")),
            "fatigue":describe(actor.fatigue, ("tired", "zmęczenie"), ("heavy limbs", "ciężkie kończyny"), ("exhausted", "wyczerpanie")),
        })
    }
    fn needs_forecast(&self, actor: &Actor, real_seconds: f64, resting: bool) -> Value {
        let projected = project_needs(actor, &self.tuning, real_seconds * PHYSIOLOGY_RATE, resting);
        json!({"hungerDelta":projected.hunger-actor.hunger,"thirstDelta":projected.thirst-actor.thirst,"fatigueDelta":projected.fatigue-actor.fatigue,"healthDelta":clamp(projected.health_before_damage-projected.damage)-actor.health})
    }
    pub fn activity_text(&self, actor: &Actor) -> String {
        let Some(activity) = &actor.activity else {
            return if !actor.alive() {
                self.text("dead", "martwy")
            } else if actor.flags & UNCONSCIOUS != 0 {
                self.text("unconscious", "nieprzytomny")
            } else {
                self.text("watching", "obserwuje")
            };
        };
        match &activity.intent {
            Intent::Rest { .. } => self.text("resting", "odpoczywa"),
            Intent::Wait { .. } => self.text("waiting", "czeka"),
            Intent::Guard { .. } => self.text("keeping watch", "pilnuje"),
            Intent::Defend { .. } => self.text("defending", "broni się"),
            Intent::Hide { .. } => self.text("hiding", "ukrywa się"),
            Intent::Move { .. } => self.text("walking", "idzie"),
            Intent::Flee { .. } => self.text("fleeing", "ucieka"),
            Intent::Escort { .. } => self.text("escorting", "eskortuje"),
            Intent::Work { recipe } => self.text(
                &format!("making {}", recipe_spec(*recipe).name),
                &format!("przygotowuje {}", recipe_spec(*recipe).polish_name),
            ),
            other => self.action_verb(other),
        }
    }
    fn action_verb(&self, intent: &Intent) -> String {
        self.text(
            intent.kind().replace('_', " ").as_str(),
            match intent {
                Intent::Take { .. } => "zabrać",
                Intent::Steal { .. } => "ukraść",
                Intent::Drop { .. } => "odłożyć",
                Intent::Equip { .. } => "wyposażyć",
                Intent::Give { .. } => "przekazać",
                Intent::Offer { .. } => "zaoferować",
                Intent::Pay { .. } => "zapłacić",
                Intent::AcceptOffer { .. } => "przyjąć ofertę",
                Intent::DeclineOffer { .. } => "odrzucić ofertę",
                Intent::Unlock { .. } => "odblokować",
                Intent::Lock { .. } => "zamknąć na klucz",
                Intent::Open { .. } => "otworzyć",
                Intent::Close { .. } => "zamknąć",
                Intent::Eat { .. } => "zjeść",
                Intent::Drink { .. } => "wypić",
                Intent::Bind { .. } => "związać",
                Intent::Release { .. } => "uwolnić",
                Intent::Use { .. } => "użyć",
                Intent::Inspect { .. } => "obejrzeć",
                Intent::Move { .. } => "przejść",
                Intent::Flee { .. } => "uciec",
                Intent::Escort { .. } => "eskortować",
                Intent::Attack { .. } => "zaatakować",
                Intent::Work { .. } => "pracować",
                Intent::Rest { .. } => "odpocząć",
                Intent::Wait { .. } => "czekać",
                Intent::Guard { .. } => "pilnować",
                Intent::Defend { .. } => "bronić się",
                Intent::Hide { .. } => "ukryć się",
                Intent::Speak { .. } => "powiedzieć",
            },
        )
    }
    fn describe_room(&self, id: u16) -> Result<Value> {
        let room = self.room(id)?;
        Ok(
            json!({"id":room.id,"name":self.name(id),"description":self.authored(&room.description),"cover":room.cover,"prison":room.prison}),
        )
    }
    fn describe_actor(&self, actor: &Actor) -> Value {
        let mut value = json!(actor);
        value["name"] = json!(self.name(actor.id));
        value["role"] = json!(self.role(actor));
        value["biography"] =
            json!(
                self.content
                    .actor_text(actor.id, "biography", &actor.biography, &self.language)
            );
        value["personality"] = json!(self.content.actor_text(
            actor.id,
            "personality",
            &actor.personality,
            &self.language
        ));
        value["goals"] = json!(
            actor
                .goals
                .iter()
                .map(|g| self.authored(g))
                .collect::<Vec<_>>()
        );
        value["summary"] = json!(self.authored(&actor.summary));
        value
    }
    fn describe_memory(&self, memory: &Memory) -> Value {
        let mut value = json!(memory);
        if memory.kind == "background" {
            value["text"] = json!(self.authored(&memory.text));
        }
        value
    }
    fn visible_item(&self, item: &Item) -> Value {
        json!({"id":item.id,"name":self.name(item.id),"kind":item.kind,"quantity":item.quantity,"ownerId":item.owner_id,"description":self.item_text(item, "description")})
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        kind: &str,
        source: Option<u16>,
        room: Option<u16>,
        text: String,
        audience: Option<Vec<u16>>,
        causes: Vec<u64>,
        sound: Option<&str>,
    ) -> Result<WorldEvent> {
        self.record_with_perceptions(
            kind,
            source,
            room,
            text,
            audience,
            causes,
            sound,
            vec![],
            match kind {
                "injury" => Some(EventDetails::Injury),
                "death" => Some(EventDetails::Death),
                "activity" => Some(EventDetails::ActivityStarted),
                "feedback" => Some(EventDetails::ExecutionFeedback),
                _ => None,
            },
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn record_with_perceptions(
        &mut self,
        kind: &str,
        source: Option<u16>,
        room: Option<u16>,
        text: String,
        audience: Option<Vec<u16>>,
        causes: Vec<u64>,
        sound: Option<&str>,
        perceptions: Vec<Perception>,
        details: Option<EventDetails>,
    ) -> Result<WorldEvent> {
        let mut heard =
            audience.unwrap_or_else(|| room.map_or_else(Vec::new, |id| self.witnesses(id)));
        heard.sort_unstable();
        heard.dedup();
        let cue = sound.or(match kind {
            "injury" => Some("hit"),
            "death" => Some("death"),
            _ => None,
        });
        let event = self.store.event(WorldEvent {
            id: 0,
            time: self.world.time,
            kind: if matches!(kind, "activity" | "feedback") {
                "private"
            } else {
                kind
            }
            .into(),
            actor_id: source,
            room_id: room,
            text,
            audience: heard,
            causes,
            sound: cue.map(str::to_owned),
            perceptions,
            details,
        })?;
        for observer in event.audience.iter().filter(|_| {
            !matches!(
                event.details,
                Some(EventDetails::ActivityStarted | EventDetails::ExecutionFeedback)
            )
        }) {
            let projected = event
                .projected(*observer)
                .context("Missing event perception.")?;
            if let Some(id) = projected.actor_id {
                let actor = self.actor_mut(*observer)?;
                if !actor.known_actors.contains(&id) {
                    actor.known_actors.push(id);
                }
            }
            self.store.remember(Memory {
                id: 0,
                actor_id: *observer,
                time: event.time,
                event_id: Some(event.id),
                kind: if kind == "speech" { "heard" } else { "seen" }.into(),
                text: projected.text,
                source_id: projected.actor_id,
                importance: if matches!(kind, "death" | "injury") {
                    10
                } else if kind == "speech" {
                    6
                } else {
                    4
                },
                evidence_ids: vec![],
                perceived: projected
                    .room_id
                    .zip(projected.details)
                    .map(|(room_id, details)| PerceivedEvent { room_id, details }),
            })?;
        }
        self.deferred_events.push(event.clone());
        Ok(event)
    }
    fn request_decision(
        &mut self,
        id: u16,
        reason: String,
        urgent: bool,
        invalidate: bool,
    ) -> Result<()> {
        self.queue_decision(
            id,
            reason,
            if urgent {
                WakePriority::Danger
            } else {
                WakePriority::Routine
            },
            invalidate,
        )
    }
    fn queue_decision(
        &mut self,
        id: u16,
        reason: String,
        priority: WakePriority,
        invalidate: bool,
    ) -> Result<()> {
        self.queue_decision_for(id, reason, priority, invalidate, WakeCause::External)
    }
    fn queue_decision_for(
        &mut self,
        id: u16,
        reason: String,
        priority: WakePriority,
        invalidate: bool,
        cause: WakeCause,
    ) -> Result<()> {
        let actor = self.actor(id)?;
        if self.is_player(id)
            || !actor.awake()
            || self.world.ending != "playing"
            || actor
                .activity
                .as_ref()
                .is_some_and(|a| matches!(a.intent, Intent::Flee { .. }))
        {
            return Ok(());
        }
        if let Some(encounter) = self.encounter(id)
            && (self.resolving_combat
                || encounter
                    .participants
                    .iter()
                    .find(|p| p.actor_id == id)
                    .is_some_and(|p| p.choice.is_some()))
        {
            return Ok(());
        }
        if invalidate && priority == WakePriority::Danger {
            self.interrupt_activity(id)?;
        }
        let actor = self.actor_mut(id)?;
        if cause == WakeCause::External {
            actor.correction_used = false;
        }
        if invalidate {
            actor.invalidate_decision()?;
            if priority == WakePriority::Danger {
                actor.clear_plan();
            }
        }
        let version = actor.decision_version;
        let wake = Wake {
            actor_id: id,
            reason,
            priority,
            version,
            cause,
        };
        if let Some(prior) = self.wakes.get_mut(&id) {
            prior.merge(wake);
        } else {
            self.wakes.insert(id, wake);
        }
        Ok(())
    }
    pub fn initialize_minds(&mut self) -> Result<()> {
        self.mutate(|s| {
            let ids: Vec<_> = s
                .world
                .actors
                .iter()
                .filter(|a| !s.is_player(a.id) && a.activity.is_none() && a.plan.is_empty())
                .map(|a| a.id)
                .collect();
            for id in ids {
                s.queue_decision_for(
                    id,
                    s.text(
                        "Choose your next activity for this evening.",
                        "Wybierz następne zajęcie na ten wieczór.",
                    ),
                    WakePriority::Routine,
                    false,
                    WakeCause::Initialization,
                )?;
            }
            Ok(())
        })
    }
    pub fn wake_idle(&mut self, id: u16) -> Result<()> {
        self.request_decision(
            id,
            self.text(
                "Reconsider what you need and intend to do.",
                "Przemyśl swoje potrzeby i zamiary.",
            ),
            false,
            false,
        )
    }
    pub fn drain_wakes(&mut self) -> Vec<Wake> {
        std::mem::take(&mut self.wakes).into_values().collect()
    }
    pub fn drain_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.deferred_events)
            .into_iter()
            .map(|e| json!(e))
            .collect()
    }

    pub fn join_player(&mut self, identity: &str, name: &str, maximum: usize) -> Result<u16> {
        ensure!(
            self.world.mode == "multiplayer",
            "This castle is a solo game."
        );
        if let Some(player) = self.world.players.iter().find(|p| p.identity == identity) {
            return Ok(player.actor_id);
        }
        ensure!(
            self.world.ending == "playing",
            "This night has ended. The host must start a new one."
        );
        ensure!(
            self.world.players.len() < maximum,
            "This castle has reached its player limit."
        );
        let name = player_name(name)?;
        ensure!(
            !identity.is_empty()
                && identity.len() <= 300
                && !identity.chars().any(char::is_control),
            "Invalid player identity."
        );
        let key: String = name.nfc().collect::<String>().to_lowercase();
        ensure!(
            !self.content.reserved_name(&name)
                && !self.world.actors.iter().any(|actor| actor
                    .name
                    .nfc()
                    .collect::<String>()
                    .to_lowercase()
                    == key),
            "Someone already bears that name."
        );
        let source: Blueprint = serde_json::from_value(self.content.canonical_blueprint().clone())?;
        let template = source
            .actors
            .into_iter()
            .find(|a| a.id == PLAYER)
            .context("Scenario has no prisoner template.")?;
        self.mutate(|s|{
            let id=s.allocate_id()?;
            let knowledge=template.knowledge.clone();
            let used:Vec<_>=s.world.actors.iter().filter_map(|a|a.portrait_key.clone()).collect();
            let actor=Actor{id,name:name.clone(),role:"Prisoner".into(),biography:String::new(),personality:template.personality,portrait_key:Some(crate::portraits::portrait_key(id,&used)),room_id:DUNGEON,health:100.0,hunger:template.hunger,thirst:template.thirst,fatigue:template.fatigue,flags:0,coins:template.coins,equipment:vec![],skills:template.skills,known_rooms:template.known_rooms,known_actors:template.known_actors.into_iter().filter(|id|s.world.actors.iter().any(|a|a.id==*id)).collect(),goals:vec![s.content.objective(&s.language)],relationships:BTreeMap::new(),summary:"A prisoner arrives.".into(),activity:None,plan:vec![],last_acquired_item_id:0,accepted_plots:vec![],decision_version:0,unconscious_until:None,entered_event_id:0,last_action_outcome:None,last_plan_failure:None,recent_action_outcomes:vec![],correction_used:false};
            s.world.actors.push(actor);s.world.players.push(HumanPlayer{identity:identity.into(),actor_id:id,listener:false});
            for mut item in source.items.into_iter().filter(|item|item.owner_id==Some(PLAYER)){item.id=s.allocate_id()?;item.owner_id=Some(id);item.poisoned_by=None;item.causes.clear();if item.equipped{s.actor_mut(id)?.equipment.push(item.id);}s.world.items.push(item);}
            for text in knowledge{s.store.remember(Memory{id:0,actor_id:id,time:s.world.time,event_id:None,kind:"background".into(),text:s.authored(&text),importance:8,source_id:Some(id),evidence_ids:vec![],
                perceived: None,
            })?;}
            let event=s.record("action",Some(id),Some(DUNGEON),s.text(&format!("{name} is brought into the dungeon."),&format!("{name} trafia do lochu.")),None,vec![],Some("step"))?;
            s.actor_mut(id)?.entered_event_id=event.id;
            let witnesses:Vec<_>=s.nearby(id)?.iter().filter(|a|a.id!=id).map(|a|a.id).collect();
            for witness in witnesses{s.request_decision(witness,s.text(&format!("{name} has been brought into your room. Assess the newcomer."),&format!("Do pomieszczenia przyprowadzono osobę o imieniu {name}. Oceń przybysza.")),false,true)?;}
            Ok(id)
        })
    }
    pub fn set_listener(&mut self, id: u16, enabled: bool) -> Result<()> {
        self.mutate(|s| {
            let player = s
                .world
                .players
                .iter_mut()
                .find(|p| p.actor_id == id)
                .context("Unknown player.")?;
            player.listener = enabled;
            Ok(())
        })
    }

    pub fn validate(&self, id: u16, raw: Value) -> Result<Intent> {
        let intent = Intent::parse(raw).map_err(|_| {
            anyhow::anyhow!(self.text(
                "That action is malformed.",
                "To działanie ma nieprawidłową formę."
            ))
        })?;
        check_new_plan(std::slice::from_ref(&intent))?;
        self.validate_intent(id, &intent)?;
        Ok(intent)
    }
    fn validate_intent(&self, id: u16, intent: &Intent) -> Result<()> {
        ensure!(
            intent.item_id() != Some(0),
            "An acquired-item reference is only available inside an active plan."
        );
        let actor = self.actor(id)?;
        ensure!(
            actor.awake(),
            "{}",
            self.text(
                "You cannot act while dead or unconscious.",
                "Nie możesz działać, gdy nie żyjesz lub jesteś nieprzytomny."
            )
        );
        ensure!(
            self.world.ending == "playing",
            "{}",
            self.text(
                "This run has ended. Load a save to return.",
                "Ta rozgrywka dobiegła końca."
            )
        );
        ensure!(
            self.encounter(id).is_none()
                || matches!(
                    intent,
                    Intent::Speak { .. }
                        | Intent::Inspect { .. }
                        | Intent::Attack { .. }
                        | Intent::Defend { .. }
                        | Intent::Flee { .. }
                ),
            "{}",
            self.text(
                "Choose attack, subdue, defend, or flee while in combat.",
                "Podczas walki wybierz atak, obezwładnienie, obronę lub ucieczkę."
            )
        );
        ensure!(
            actor.flags & RESTRAINED == 0
                || matches!(
                    intent,
                    Intent::Speak { .. }
                        | Intent::Inspect { .. }
                        | Intent::Wait { .. }
                        | Intent::Rest { .. }
                ),
            "{}",
            self.text(
                "You are restrained. Someone must cut or release your bonds.",
                "Jesteś związany. Ktoś musi przeciąć lub rozwiązać więzy."
            )
        );
        let door = intent.door_id().and_then(|id| self.door(id));
        if intent.door_id().is_some() {
            ensure!(
                door.is_some_and(|d| d.rooms.contains(&actor.room_id)),
                "{}",
                self.text("That door is out of reach.", "Te drzwi są poza zasięgiem.")
            );
        }
        let item = intent.item_id().and_then(|item| self.accessible(id, item));
        if intent.item_id().is_some() {
            ensure!(
                item.is_some(),
                "{}",
                self.text(
                    "That item is out of reach or unseen.",
                    "Ten przedmiot jest niewidoczny lub poza zasięgiem."
                )
            );
        }
        if let Some(quantity) = intent.quantity() {
            let item = intent.item_id().context("Missing item ID")?;
            if matches!(intent, Intent::Offer { .. }) {
                self.quantity_count(item, quantity)?;
            } else {
                self.transfer_count(item, quantity)?;
            }
        }
        let error = match intent {
            Intent::Move { .. } | Intent::Flee { .. }
                if door.is_some_and(|d| d.flags & LOCKED != 0) =>
            {
                Some((
                    "The door is locked. Unlock it with a matching key before passing.",
                    "Drzwi są zamknięte na klucz. Odblokuj je pasującym kluczem, zanim przejdziesz.",
                ))
            }
            Intent::Move { .. } | Intent::Flee { .. }
                if door.is_none_or(|d| d.flags & OPEN == 0) =>
            {
                Some((
                    "The door is closed. Open it before passing.",
                    "Drzwi są zamknięte. Otwórz je, zanim przejdziesz.",
                ))
            }
            Intent::Escort { target_id, .. }
                if self
                    .same_room(id, *target_id)
                    .is_none_or(|a| a.id == id || a.flags & RESTRAINED == 0) =>
            {
                Some((
                    "You can escort only a restrained person beside you.",
                    "Możesz eskortować tylko związaną osobę obok ciebie.",
                ))
            }
            Intent::Escort { .. } if door.is_none_or(|d| d.flags & OPEN == 0) => Some((
                "Open the door before escorting anyone through it.",
                "Otwórz drzwi, zanim kogoś przez nie przeprowadzisz.",
            )),
            Intent::Unlock { .. } | Intent::Lock { .. }
                if door.is_none_or(|d| !self.has_key(id, d)) =>
            {
                Some((
                    "You need a matching key in your possession.",
                    "Musisz posiadać pasujący klucz.",
                ))
            }
            Intent::Lock { .. } if door.is_some_and(|d| d.flags & OPEN != 0) => Some((
                "Close the door before locking it.",
                "Zamknij drzwi, zanim przekręcisz klucz.",
            )),
            Intent::Open { .. } if door.is_some_and(|d| d.flags & LOCKED != 0) => Some((
                "The door is locked. Unlock it with a matching key before opening it.",
                "Drzwi są zamknięte na klucz. Odblokuj je pasującym kluczem, zanim je otworzysz.",
            )),
            Intent::Take { .. }
                if item.is_some_and(|i| {
                    i.owner_id
                        .is_some_and(|owner| self.actor(owner).is_ok_and(|a| a.alive()))
                }) =>
            {
                Some((
                    "That belongs to someone alive. Ask, offer a trade, or attempt theft.",
                    "To należy do żyjącej osoby. Poproś, zaproponuj wymianę lub spróbuj kradzieży.",
                ))
            }
            Intent::Steal { .. }
                if item.is_none_or(|i| {
                    i.owner_id.is_none_or(|owner| {
                        owner == id || self.actor(owner).is_ok_and(|a| !a.alive())
                    })
                }) =>
            {
                Some((
                    "There is no living owner to steal this from.",
                    "Nie ma żyjącego właściciela, któremu można to ukraść.",
                ))
            }
            Intent::Drop { .. }
            | Intent::Equip { .. }
            | Intent::Give { .. }
            | Intent::Offer { .. }
            | Intent::Eat { .. }
            | Intent::Drink { .. }
            | Intent::Use { .. }
                if item.is_none_or(|i| i.owner_id != Some(id)) =>
            {
                Some((
                    "You do not own that item.",
                    "Nie posiadasz tego przedmiotu.",
                ))
            }
            Intent::Give { target_id, .. } | Intent::Offer { target_id, .. }
                if *target_id == id || self.same_room(id, *target_id).is_none() =>
            {
                Some((
                    "The recipient must be beside you.",
                    "Odbiorca musi stać obok ciebie.",
                ))
            }
            Intent::Offer { .. } if self.world.next_offer_id > 65535 => Some((
                "The world has reached its offer limit.",
                "Świat osiągnął limit ofert.",
            )),
            Intent::Equip { .. }
                if item.is_none_or(|i| !matches!(i.kind, ItemKind::Weapon | ItemKind::Clothes)) =>
            {
                Some((
                    "Only weapons and clothes can be equipped.",
                    "Możesz wyposażyć tylko broń lub ubranie.",
                ))
            }
            Intent::Eat { .. } if item.is_none_or(|i| i.kind != ItemKind::Food) => {
                Some(("That is not food.", "To nie jest jedzenie."))
            }
            Intent::Drink { .. } if item.is_none_or(|i| i.kind != ItemKind::Water) => {
                Some(("That is not a drink.", "To nie jest napój."))
            }
            Intent::Use {
                target_id,
                mode: UseMode::Poison,
                ..
            } if item.is_none_or(|i| i.kind != ItemKind::Poison)
                || self
                    .accessible(id, *target_id)
                    .is_none_or(|i| !matches!(i.kind, ItemKind::Food | ItemKind::Water)) =>
            {
                Some((
                    "You need poison and accessible food or water.",
                    "Potrzebujesz trucizny oraz dostępnego jedzenia lub wody.",
                ))
            }
            Intent::Use {
                target_id,
                mode: UseMode::Heal,
                ..
            } if item.is_none_or(|i| i.kind != ItemKind::Medicine)
                || self.same_room(id, *target_id).is_none() =>
            {
                Some((
                    "You need medicine and a living patient beside you.",
                    "Potrzebujesz lekarstwa i żyjącego pacjenta obok siebie.",
                ))
            }
            Intent::Use {
                target_id,
                mode: UseMode::PickLock,
                ..
            } if item.is_none_or(|i| i.kind != ItemKind::Lockpick)
                || !self
                    .exits(actor.room_id)
                    .iter()
                    .any(|d| d.id == *target_id && d.lock.is_some() && d.flags & LOCKED != 0) =>
            {
                Some((
                    "You need a lockpick and a locked door beside you.",
                    "Potrzebujesz wytrycha i zamkniętych na klucz drzwi obok siebie.",
                ))
            }
            Intent::Use {
                target_id,
                mode: UseMode::Disguise,
                ..
            } if item.is_none_or(|i| i.kind != ItemKind::Clothes) || *target_id != id => Some((
                "You can disguise yourself with clothes you own.",
                "Możesz przebrać się we własne ubranie.",
            )),
            Intent::Use {
                target_id,
                mode: UseMode::Trap,
                ..
            } if item.is_none_or(|i| i.kind != ItemKind::Rope)
                || !self.exits(actor.room_id).iter().any(|d| d.id == *target_id)
                || self.world.traps.iter().any(|t| t.door_id == *target_id) =>
            {
                Some((
                    "A trap needs rope and an untrapped doorway beside you.",
                    "Pułapka wymaga liny i przejścia bez innej pułapki.",
                ))
            }
            Intent::Use {
                target_id,
                mode: UseMode::CutBonds,
                ..
            } if item.is_none_or(|i| i.kind != ItemKind::Weapon)
                || self
                    .same_room(id, *target_id)
                    .is_none_or(|a| a.flags & RESTRAINED == 0) =>
            {
                Some((
                    "You need a weapon and a restrained person beside you.",
                    "Potrzebujesz broni i związanej osoby obok siebie.",
                ))
            }
            Intent::Pay { target_id, amount }
                if *target_id == id
                    || actor.coins < *amount
                    || self
                        .same_room(id, *target_id)
                        .is_none_or(|a| a.coins.checked_add(*amount).is_none()) =>
            {
                Some((
                    "You need enough coins and a recipient beside you.",
                    "Potrzebujesz wystarczającej liczby monet i odbiorcy obok siebie.",
                ))
            }
            Intent::AcceptOffer { offer_id } | Intent::DeclineOffer { offer_id }
                if !self.world.offers.iter().any(|o| {
                    o.id == *offer_id
                        && o.to == id
                        && o.status == "open"
                        && !self.offer_expired(o)
                        && self.same_room(id, o.from).is_some()
                        && self
                            .item(o.item_id)
                            .is_some_and(|i| i.owner_id == Some(o.from))
                        && (!matches!(intent, Intent::AcceptOffer { .. })
                            || self.transfer_count(o.item_id, o.quantity).is_ok())
                }) =>
            {
                Some((
                    "That offer is no longer available to you.",
                    "Ta oferta nie jest już dostępna.",
                ))
            }
            Intent::Attack { target_id, .. }
            | Intent::Bind { target_id }
            | Intent::Release { target_id }
                if *target_id == id || self.same_room(id, *target_id).is_none() =>
            {
                Some((
                    "The person must be beside you.",
                    "Ta osoba musi być obok ciebie.",
                ))
            }
            Intent::Attack { target_id, .. }
                if self.actor(*target_id).is_ok_and(|a| {
                    a.activity
                        .as_ref()
                        .is_some_and(|activity| matches!(activity.intent, Intent::Flee { .. }))
                }) =>
            {
                Some((
                    "That person is already withdrawing through the doorway.",
                    "Ta osoba już wycofuje się przez drzwi.",
                ))
            }
            Intent::Attack {
                target_id,
                style: AttackStyle::Subdue,
            } if self
                .actor(*target_id)
                .is_ok_and(|a| a.flags & (UNCONSCIOUS | RESTRAINED) != 0) =>
            {
                Some((
                    "That person is already subdued or restrained.",
                    "Ta osoba jest już obezwładniona lub związana.",
                ))
            }
            Intent::Bind { target_id }
                if self
                    .actor(*target_id)
                    .is_ok_and(|a| a.flags & UNCONSCIOUS == 0)
                    || self.ingredient(id, ItemKind::Rope).is_none() =>
            {
                Some((
                    "Binding requires an unconscious person and reachable rope.",
                    "Związanie wymaga nieprzytomnej osoby i dostępnej liny.",
                ))
            }
            Intent::Release { target_id }
                if self
                    .actor(*target_id)
                    .is_ok_and(|a| a.flags & RESTRAINED == 0) =>
            {
                Some((
                    "That person is not restrained.",
                    "Ta osoba nie jest związana.",
                ))
            }
            Intent::Hide { .. } if !self.room(actor.room_id)?.cover => Some((
                "There is nowhere useful to hide here.",
                "Nie ma tu odpowiedniej kryjówki.",
            )),
            Intent::Work { recipe } => {
                let r = recipe_spec(*recipe);
                if actor.room_id != r.room
                    || (if r.medicine {
                        actor.skills.medicine
                    } else {
                        actor.skills.craft
                    }) < r.minimum
                    || self.ingredient(id, r.ingredient).is_none()
                {
                    Some((
                        "You need the right workplace, skill, and a real ingredient.",
                        "Potrzebujesz właściwego miejsca, umiejętności i prawdziwego składnika.",
                    ))
                } else if self.world.next_item_id > 65535 {
                    Some((
                        "The world has reached its item limit.",
                        "Świat osiągnął limit przedmiotów.",
                    ))
                } else {
                    None
                }
            }
            Intent::Speak {
                volume: Volume::Whisper,
                ..
            } if self.world.mode == "multiplayer" => Some((
                "Private chat is unavailable. Speak to the room or shout.",
                "Prywatne rozmowy są niedostępne. Mów do pomieszczenia lub krzycz.",
            )),
            Intent::Speak {
                target_id: Some(target),
                ..
            } if *target == id || self.same_room(id, *target).is_none() => Some((
                "You cannot address someone absent, unseen, or yourself.",
                "Nie możesz zwracać się do nieobecnej lub niewidocznej osoby ani do siebie.",
            )),
            Intent::Speak {
                target_id: None,
                volume: Volume::Whisper,
                ..
            } => Some((
                "Choose a person to whisper to.",
                "Wybierz osobę, do której szepczesz.",
            )),
            Intent::Speak {
                target_id,
                plot: Some(plot),
                ..
            } if target_id.is_none()
                || !actor.known_actors.contains(&plot.victim_id)
                || !self.actor(plot.victim_id).is_ok_and(|a| a.alive()) =>
            {
                Some((
                    "A plot needs a known living victim and a listener beside you.",
                    "Spisek wymaga znanej żyjącej ofiary i słuchacza obok ciebie.",
                ))
            }
            Intent::Speak { plot: Some(_), .. } if self.world.next_plot_id > 65535 => Some((
                "The world has reached its plot limit.",
                "Świat osiągnął limit spisków.",
            )),
            Intent::Inspect { target_id }
                if *target_id != actor.room_id
                    && !self.nearby(id)?.iter().any(|a| a.id == *target_id)
                    && !self.exits(actor.room_id).iter().any(|d| d.id == *target_id)
                    && self.accessible(id, *target_id).is_none() =>
            {
                Some((
                    "You cannot inspect something absent or unseen.",
                    "Nie możesz oglądać rzeczy nieobecnych lub niewidocznych.",
                ))
            }
            _ => None,
        };
        if let Some((en, pl)) = error {
            bail!("{}", self.text(en, pl));
        }
        Ok(())
    }
    fn offer_expired(&self, offer: &Offer) -> bool {
        offer
            .interaction_expires
            .map_or(offer.expires <= self.world.time, |deadline| {
                deadline <= self.world.interaction_time
            })
    }
    pub fn dispatch(
        &mut self,
        id: u16,
        raw: Value,
        causes: Vec<u64>,
        combat: Option<Value>,
    ) -> Result<Value> {
        if let Some(raw) = combat {
            let Ok(context) = serde_json::from_value::<CombatContext>(raw) else {
                return Ok(ActionResult::fail(
                    self.text("Malformed combat context.", "Nieprawidłowy kontekst walki."),
                )
                .value());
            };
            if !self.matches_combat(id, &context) {
                return Ok(ActionResult::fail(self.text(
                    "That combat round has already ended.",
                    "Ta runda walki już się zakończyła.",
                ))
                .value());
            }
        }
        let intent = match self.validate(id, raw) {
            Ok(intent) => intent,
            Err(error) => return Ok(ActionResult::fail(error.to_string()).value()),
        };
        if let Err(error) = self.check_busy(id, &intent) {
            return Ok(ActionResult::fail(error.to_string()).value());
        }
        self.mutate(|s| {
            if !matches!(intent, Intent::Speak { .. }) {
                s.actor_mut(id)?.last_acquired_item_id = 0;
            }
            s.begin(id, intent, causes)
        })
        .map(ActionResult::value)
    }
    fn has_key(&self, id: u16, door: &Door) -> bool {
        door.lock.is_some()
            && self
                .inventory(id)
                .iter()
                .any(|item| item.kind == ItemKind::Key && item.key == door.lock)
    }
    fn check_busy(&self, id: u16, intent: &Intent) -> Result<()> {
        ensure!(
            !self
                .actor(id)?
                .activity
                .as_ref()
                .is_some_and(|a| !a.intent.long())
                || matches!(
                    intent,
                    Intent::Speak { .. }
                        | Intent::Inspect { .. }
                        | Intent::Defend { .. }
                        | Intent::Flee { .. }
                ),
            "{}",
            self.text(
                "Finish your current action first, or defend/flee to interrupt it.",
                "Dokończ bieżące działanie albo przerwij je obroną lub ucieczką.",
            )
        );
        Ok(())
    }
    fn action_duration(&self, intent: &Intent) -> (f64, f64) {
        if matches!(intent, Intent::Speak { .. } | Intent::Inspect { .. }) {
            return (0.0, 0.0);
        }
        let real = if intent.travel() {
            self.tuning.travel_seconds
        } else if matches!(intent, Intent::Work { .. }) {
            self.tuning.craft_seconds
        } else if matches!(
            intent,
            Intent::Use {
                mode: UseMode::PickLock,
                ..
            }
        ) {
            self.tuning.lockpick_seconds
        } else if intent.combat_move() {
            self.tuning.combat_round_seconds
        } else {
            self.tuning.action_seconds
        };
        intent
            .seconds()
            .map_or((real * CLOCK_RATE, real), |seconds| {
                (f64::from(seconds), f64::from(seconds) / CLOCK_RATE)
            })
    }
    fn begin(&mut self, id: u16, intent: Intent, causes: Vec<u64>) -> Result<ActionResult> {
        if matches!(intent, Intent::Speak { .. } | Intent::Inspect { .. }) {
            return self.execute(id, intent, causes);
        }
        self.interrupt_activity(id)?;
        if matches!(intent, Intent::Attack { .. })
            || self.encounter(id).is_some() && intent.combat_move()
        {
            return self.choose_combat(id, intent, causes);
        }
        let (game_duration, real) = self.action_duration(&intent);
        let body = self.actor(id)?;
        let activity = Activity {
            intent: intent.clone(),
            started: self.world.time,
            ends: self.world.time + game_duration,
            causes: causes.clone(),
            started_vitals: intent.long().then_some([
                body.health,
                body.hunger,
                body.thirst,
                body.fatigue,
            ]),
            interaction_ends: if intent.long() {
                None
            } else {
                Some(self.world.interaction_time + real)
            },
        };
        let room = self.actor(id)?.room_id;
        {
            let actor = self.actor_mut(id)?;
            actor.activity = Some(activity);
            if intent.travel() {
                actor.invalidate_decision()?;
                actor.plan.retain(|p| !matches!(p, Intent::Attack { .. }));
            }
        }
        if let Intent::Escort { target_id, door_id } = intent {
            self.interrupt_activity(target_id)?;
            let target = self.actor_mut(target_id)?;
            target.invalidate_decision()?;
            target.clear_plan();
            let destination = self.destination(self.door(door_id).context("Missing door.")?, room);
            let mut audience = self.witnesses(room);
            audience.push(target_id);
            self.record(
                "action",
                Some(id),
                Some(room),
                self.text(
                    &format!(
                        "{} begins escorting {} toward {}.",
                        self.name(id),
                        self.name(target_id),
                        self.name(destination)
                    ),
                    &format!(
                        "{} rozpoczyna eskortę osoby {} do {}.",
                        self.name(id),
                        self.name(target_id),
                        self.name(destination)
                    ),
                ),
                Some(audience),
                causes.clone(),
                None,
            )?;
        }
        if matches!(intent, Intent::Hide { .. }) {
            let actor = self.actor(id)?.clone();
            let watchers: Vec<_> = self
                .nearby(id)?
                .into_iter()
                .filter(|a| a.id != id && a.awake())
                .cloned()
                .collect();
            let noticed = watchers.into_iter().any(|a| {
                self.random()
                    < ((a.skills.stealth + a.skills.combat / 2.0
                        - a.fatigue / 2.0
                        - actor.skills.stealth
                        + 40.0)
                        / 100.0)
                        .clamp(0.05, 0.8)
            });
            if noticed {
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("{} tries to hide, but is noticed.", self.name(id)),
                        &format!(
                            "{} próbuje się ukryć, ale zostaje zauważony.",
                            self.name(id)
                        ),
                    ),
                    None,
                    causes.clone(),
                    None,
                )?;
            } else {
                self.actor_mut(id)?.flags |= HIDDEN;
                self.record(
                    "private",
                    Some(id),
                    Some(room),
                    self.text("You slip into cover.", "Znikasz w kryjówce."),
                    Some(vec![id]),
                    causes.clone(),
                    None,
                )?;
            }
        }
        self.record(
            "activity",
            Some(id),
            Some(room),
            self.text(
                &format!("You begin to {}.", self.action_verb(&intent)),
                &format!("Zaczynasz: {}.", self.action_verb(&intent)),
            ),
            Some(vec![id]),
            causes,
            if matches!(intent, Intent::Rest { .. } | Intent::Hide { .. }) {
                Some("cloth")
            } else {
                None
            },
        )?;
        let result = ActionResult::ok(self.text(
            &format!(
                "{} begins {}.",
                self.name(id),
                self.activity_text(self.actor(id)?)
            ),
            &format!(
                "{}: {}.",
                self.name(id),
                self.activity_text(self.actor(id)?)
            ),
        ));
        self.remember_outcome(id, intent, &result, false)?;
        Ok(result)
    }

    fn encounter(&self, id: u16) -> Option<&CombatEncounter> {
        self.world
            .encounters
            .iter()
            .find(|e| e.participants.iter().any(|p| p.actor_id == id))
    }
    pub fn combat_context(&self, id: u16) -> Option<CombatContext> {
        let encounter = self.encounter(id)?;
        let participant = encounter.participants.iter().find(|p| p.actor_id == id)?;
        Some(CombatContext {
            encounter_id: encounter.id.clone(),
            round: encounter.round.max(participant.joins_round),
        })
    }
    fn matches_combat(&self, id: u16, context: &CombatContext) -> bool {
        self.combat_context(id).as_ref() == Some(context)
    }
    fn choose_combat(&mut self, id: u16, choice: Intent, causes: Vec<u64>) -> Result<ActionResult> {
        let room = self.actor(id)?.room_id;
        let encounter_id = self.encounter(id).map(|e| e.id.clone()).or_else(|| {
            self.world
                .encounters
                .iter()
                .find(|e| e.room_id == room)
                .map(|e| e.id.clone())
        });
        let opening = encounter_id.is_none();
        let encounter_id = if let Some(encounter_id) = encounter_id {
            encounter_id
        } else {
            if !matches!(choice, Intent::Attack { .. }) {
                return Ok(ActionResult::fail(
                    self.text("There is no fight here.", "Nie ma tu walki."),
                ));
            }
            let encounter_id = Uuid::new_v4().to_string();
            self.world.encounters.push(CombatEncounter {
                id: encounter_id.clone(),
                room_id: room,
                round: 1,
                deadline: self.world.interaction_time + self.tuning.combat_round_seconds,
                participants: vec![],
                hostility: vec![],
            });
            self.record(
                "notice",
                Some(id),
                Some(room),
                self.text(
                    "A fight breaks out. Choose attack, subdue, defend, or flee.",
                    "Wybucha walka. Wybierz atak, obezwładnienie, obronę lub ucieczkę.",
                ),
                None,
                causes.clone(),
                Some("notice"),
            )?;
            encounter_id
        };
        let mut joins = vec![id];
        if let Intent::Attack { target_id, .. } = &choice {
            joins.push(*target_id);
        }
        for member in joins {
            let index = self
                .world
                .encounters
                .iter()
                .position(|e| e.id == encounter_id)
                .context("Combat disappeared.")?;
            if self.world.encounters[index]
                .participants
                .iter()
                .any(|p| p.actor_id == member)
            {
                continue;
            }
            let actor = self.actor(member)?;
            let defended = if !actor.awake() || actor.flags & RESTRAINED != 0 {
                Some(Intent::Defend { seconds: 30 })
            } else {
                None
            };
            let round = self.world.encounters[index].round;
            self.world.encounters[index]
                .participants
                .push(CombatParticipant {
                    actor_id: member,
                    joins_round: if opening {
                        round
                    } else {
                        round.checked_add(1).context("Combat round overflow.")?
                    },
                    choice: defended,
                    causes: vec![],
                });
            self.interrupt_activity(member)?;
            let actor = self.actor_mut(member)?;
            actor.clear_plan();
            actor.invalidate_decision()?;
            self.request_decision(
                member,
                self.text(
                    if opening {
                        "A fight has begun. Choose exactly one move for this round."
                    } else {
                        "You are joining the fight next round. Choose exactly one move."
                    },
                    if opening {
                        "Rozpoczęła się walka. Wybierz dokładnie jeden ruch na tę rundę."
                    } else {
                        "Dołączysz do walki w następnej rundzie. Wybierz jeden ruch."
                    },
                ),
                true,
                true,
            )?;
        }
        if let Intent::Attack { target_id, style } = &choice {
            let encounter = self
                .world
                .encounters
                .iter_mut()
                .find(|e| e.id == encounter_id)
                .context("Combat disappeared.")?;
            if !encounter.hostility.contains(&[id, *target_id]) {
                encounter.hostility.push([id, *target_id]);
            }
            let verb = if *style == AttackStyle::Subdue {
                ("subdue", "obezwładnienia")
            } else {
                ("attack", "ataku")
            };
            self.record(
                "action",
                Some(id),
                Some(room),
                self.text(
                    &format!(
                        "{} prepares to {} {}.",
                        self.name(id),
                        verb.0,
                        self.name(*target_id)
                    ),
                    &format!(
                        "{} przygotowuje się do {} osoby {}.",
                        self.name(id),
                        verb.1,
                        self.name(*target_id)
                    ),
                ),
                None,
                causes.clone(),
                Some("swing"),
            )?;
        }
        self.interrupt_activity(id)?;
        self.actor_mut(id)?.clear_plan();
        self.wakes.remove(&id);
        let participant = self
            .world
            .encounters
            .iter_mut()
            .find(|e| e.id == encounter_id)
            .and_then(|e| e.participants.iter_mut().find(|p| p.actor_id == id))
            .context("Missing participant.")?;
        participant.choice = Some(choice.clone());
        participant.causes = causes.clone();
        if !matches!(choice, Intent::Attack { .. }) {
            self.record(
                "feedback",
                Some(id),
                Some(room),
                if let Intent::Flee { door_id } = choice {
                    self.text(
                        &format!(
                            "You choose to flee through {} this round.",
                            self.name(door_id)
                        ),
                        &format!(
                            "W tej rundzie wybierasz ucieczkę przez {}.",
                            self.name(door_id)
                        ),
                    )
                } else {
                    self.text(
                        "You choose to defend this round.",
                        "W tej rundzie wybierasz obronę.",
                    )
                },
                Some(vec![id]),
                causes,
                None,
            )?;
        }
        let result = ActionResult::ok(self.text(
            "Your combat choice is submitted.",
            "Twój ruch w walce został przyjęty.",
        ));
        self.remember_outcome(id, choice, &result, false)?;
        self.maybe_resolve_combat(&encounter_id)?;
        Ok(result)
    }
    fn maybe_resolve_combat(&mut self, id: &str) -> Result<()> {
        if self.resolving_combat {
            return Ok(());
        }
        let Some(encounter) = self.world.encounters.iter().find(|e| e.id == id) else {
            return Ok(());
        };
        let current: Vec<_> = encounter
            .participants
            .iter()
            .filter(|p| p.joins_round <= encounter.round)
            .collect();
        if !current.is_empty()
            && (current.iter().all(|p| p.choice.is_some())
                || encounter.deadline <= self.world.interaction_time)
        {
            self.resolve_combat(id)?;
        }
        Ok(())
    }
    fn end_combat(&mut self, encounter: &CombatEncounter) -> Result<()> {
        self.world.encounters.retain(|e| e.id != encounter.id);
        self.record(
            "notice",
            None,
            Some(encounter.room_id),
            self.text("The fight ends.", "Walka dobiega końca."),
            None,
            vec![],
            Some("notice"),
        )?;
        for participant in &encounter.participants {
            self.actor_mut(participant.actor_id)?
                .invalidate_decision()?;
            self.request_decision(
                participant.actor_id,
                self.text(
                    "The fight has ended. Reassess your injuries and the room.",
                    "Walka się skończyła. Oceń swoje rany i sytuację.",
                ),
                false,
                false,
            )?;
        }
        Ok(())
    }
    fn resolve_combat(&mut self, id: &str) -> Result<()> {
        let mut encounter = self
            .world
            .encounters
            .iter()
            .find(|e| e.id == id)
            .context("Combat disappeared.")?
            .clone();
        let snapshot: BTreeMap<_, _> = encounter
            .participants
            .iter()
            .filter(|p| p.joins_round <= encounter.round)
            .map(|p| {
                Ok((
                    p.actor_id,
                    (
                        self.actor(p.actor_id)?.clone(),
                        p.choice.clone().unwrap_or(Intent::Defend { seconds: 30 }),
                        p.causes.clone(),
                    ),
                ))
            })
            .collect::<Result<_>>()?;
        let mut strikes = Vec::new();
        for (source, (actor, choice, causes)) in &snapshot {
            let Intent::Attack { target_id, style } = choice else {
                continue;
            };
            if !actor.awake() || actor.room_id != encounter.room_id {
                continue;
            }
            let Some((target, target_choice, _)) = snapshot.get(target_id) else {
                continue;
            };
            if !target.alive()
                || target.room_id != encounter.room_id
                || target
                    .activity
                    .as_ref()
                    .is_some_and(|a| matches!(a.intent, Intent::Flee { .. }))
                || *style == AttackStyle::Subdue && target.flags & (UNCONSCIOUS | RESTRAINED) != 0
            {
                continue;
            }
            let weapon = self
                .inventory(*source)
                .iter()
                .filter(|i| i.kind == ItemKind::Weapon && i.equipped)
                .map(|i| i.power)
                .fold(0.0, f64::max);
            let damage = attack_damage(
                actor.skills.combat,
                weapon,
                actor.flags & HIDDEN != 0,
                actor.fatigue,
                target.skills.combat,
                target.awake() && matches!(target_choice, Intent::Defend { .. }),
            );
            let mut all_causes = causes.clone();
            all_causes.extend(self.plot_causes(*source, *target_id));
            strikes.push((*source, *target_id, damage, *style, all_causes));
        }
        self.resolving_combat = true;
        let result: Result<()> = (|| {
            self.record(
                "notice",
                None,
                Some(encounter.room_id),
                self.text(
                    &format!("Round {} resolves.", encounter.round),
                    &format!("Rozstrzygnięcie rundy {}.", encounter.round),
                ),
                None,
                vec![],
                Some("notice"),
            )?;
            for (source, target_id, damage, style, causes) in strikes {
                self.actor_mut(source)?.flags &= !HIDDEN;
                if !self.actor(target_id)?.alive() {
                    continue;
                }
                let health_before = self.actor(target_id)?.health;
                if style == AttackStyle::Subdue && self.actor(target_id)?.health - damage <= 20.0 {
                    self.interrupt_activity(target_id)?;
                    let deadline = self.world.interaction_time + self.tuning.recovery_seconds;
                    let target = self.actor_mut(target_id)?;
                    let before = target.health;
                    target.health = subdued_health(before, damage);
                    let dealt = before - target.health;
                    target.flags |= UNCONSCIOUS;
                    target.unconscious_until = Some(deadline);
                    target.clear_plan();
                    target.invalidate_decision()?;
                    let mut audience = self.witnesses(encounter.room_id);
                    audience.push(target_id);
                    self.record(
                        "injury",
                        Some(source),
                        Some(encounter.room_id),
                        self.text(
                            &format!(
                                "{} subdues {}: −{} HP; unconscious.",
                                self.name(source),
                                self.name(target_id),
                                number_text(dealt)
                            ),
                            &format!(
                                "{} obezwładnia osobę {}: −{} HP; nieprzytomność.",
                                self.name(source),
                                self.name(target_id),
                                number_text(dealt)
                            ),
                        ),
                        Some(audience),
                        causes,
                        None,
                    )?;
                } else {
                    self.damage(
                        target_id,
                        damage,
                        source,
                        true,
                        causes,
                        self.text(
                            &format!("{}’s attack", self.name(source)),
                            &format!("atak osoby {}", self.name(source)),
                        ),
                    )?;
                }
                let dealt = health_before - self.actor(target_id)?.health;
                let result = ActionResult::ok(self.text(
                    &format!(
                        "{} → {}: −{} HP{}.",
                        self.name(source),
                        self.name(target_id),
                        number_text(dealt),
                        if self.actor(target_id)?.flags & UNCONSCIOUS != 0 {
                            "; unconscious"
                        } else {
                            ""
                        }
                    ),
                    &format!(
                        "{} → {}: −{} HP{}.",
                        self.name(source),
                        self.name(target_id),
                        number_text(dealt),
                        if self.actor(target_id)?.flags & UNCONSCIOUS != 0 {
                            "; nieprzytomność"
                        } else {
                            ""
                        }
                    ),
                ));
                self.remember_outcome(source, Intent::Attack { target_id, style }, &result, true)?;
            }
            for (actor_id, (before, choice, _)) in &snapshot {
                if matches!(choice, Intent::Defend { .. }) {
                    let loss = before.health - self.actor(*actor_id)?.health;
                    let result = ActionResult::ok(self.text(
                        &format!("Defended this round: −{} HP received.", number_text(loss)),
                        &format!("Obrona w tej rundzie: otrzymano −{} HP.", number_text(loss)),
                    ));
                    self.remember_outcome(*actor_id, choice.clone(), &result, true)?;
                }
            }
            for (actor_id, (_, choice, causes)) in &snapshot {
                let Intent::Flee { door_id } = choice else {
                    continue;
                };
                if self.actor(*actor_id)?.awake() && self.validate_intent(*actor_id, choice).is_ok()
                {
                    let activity = Activity {
                        intent: choice.clone(),
                        started: self.world.time,
                        ends: self.world.time + self.tuning.travel_seconds * CLOCK_RATE,
                        interaction_ends: Some(
                            self.world.interaction_time + self.tuning.travel_seconds,
                        ),
                        causes: causes.clone(),
                        started_vitals: None,
                    };
                    let actor = self.actor_mut(*actor_id)?;
                    actor.activity = Some(activity);
                    actor.clear_plan();
                    actor.invalidate_decision()?;
                    let destination = self.destination(
                        self.door(*door_id).context("Missing door.")?,
                        encounter.room_id,
                    );
                    self.record(
                        "action",
                        Some(*actor_id),
                        Some(encounter.room_id),
                        self.text(
                            &format!(
                                "{} withdraws toward {}.",
                                self.name(*actor_id),
                                self.name(destination)
                            ),
                            &format!(
                                "{} wycofuje się w stronę {}.",
                                self.name(*actor_id),
                                self.name(destination)
                            ),
                        ),
                        None,
                        causes.clone(),
                        Some("step"),
                    )?;
                }
            }
            Ok(())
        })();
        self.resolving_combat = false;
        result?;
        let removed: Vec<_> = encounter
            .participants
            .iter()
            .filter_map(|p| {
                self.actor(p.actor_id)
                    .ok()
                    .filter(|a| {
                        !a.awake()
                            || a.flags & RESTRAINED != 0
                            || a.room_id != encounter.room_id
                            || a.activity
                                .as_ref()
                                .is_some_and(|a| matches!(a.intent, Intent::Flee { .. }))
                    })
                    .map(|_| p.actor_id)
            })
            .collect();
        encounter
            .participants
            .retain(|p| !removed.contains(&p.actor_id));
        let members: HashSet<_> = encounter.participants.iter().map(|p| p.actor_id).collect();
        encounter
            .hostility
            .retain(|pair| pair.iter().all(|id| members.contains(id)));
        for member in removed {
            self.actor_mut(member)?.invalidate_decision()?;
        }
        let all_defend = snapshot
            .values()
            .all(|(_, choice, _)| matches!(choice, Intent::Defend { .. }))
            && !encounter
                .participants
                .iter()
                .any(|p| p.joins_round > encounter.round);
        if self.world.ending != "playing" || encounter.hostility.is_empty() || all_defend {
            return self.end_combat(&encounter);
        }
        encounter.round = encounter
            .round
            .checked_add(1)
            .context("Combat round overflow.")?;
        encounter.deadline = self.world.interaction_time + self.tuning.combat_round_seconds;
        for participant in &mut encounter.participants {
            if participant.joins_round < encounter.round {
                participant.choice = None;
            }
            if participant.choice.is_none() {
                participant.causes.clear();
            }
            self.actor_mut(participant.actor_id)?
                .invalidate_decision()?;
        }
        let position = self
            .world
            .encounters
            .iter()
            .position(|e| e.id == id)
            .context("Combat disappeared.")?;
        self.world.encounters[position] = encounter.clone();
        for participant in &encounter.participants {
            self.request_decision(
                participant.actor_id,
                self.text(
                    &format!(
                        "Choose exactly one move for combat round {}.",
                        encounter.round
                    ),
                    &format!("Wybierz dokładnie jeden ruch na rundę {}.", encounter.round),
                ),
                true,
                false,
            )?;
        }
        Ok(())
    }
    fn transfer(&mut self, item_id: u16, owner: Option<u16>, room: Option<u16>) -> Result<()> {
        let item = self.item(item_id).context("Missing item.")?.clone();
        if let Some(old) = item.owner_id {
            let other_clothes = self
                .inventory(old)
                .iter()
                .any(|i| i.id != item_id && i.kind == ItemKind::Clothes && i.equipped);
            let actor = self.actor_mut(old)?;
            actor.equipment.retain(|id| *id != item_id);
            if item.kind == ItemKind::Clothes && !other_clothes {
                actor.flags &= !DISGUISED;
            }
        }
        let item = self
            .world
            .items
            .iter_mut()
            .find(|i| i.id == item_id)
            .context("Missing item.")?;
        item.owner_id = owner;
        item.room_id = room;
        item.equipped = false;
        Ok(())
    }
    fn transfer_count(&self, item_id: u16, quantity: u8) -> Result<u8> {
        let count = self.quantity_count(item_id, quantity)?;
        let item = self.item(item_id).context("Missing item.")?;
        ensure!(
            count == item.quantity || self.world.next_item_id <= u32::from(u16::MAX),
            "{}",
            self.text(
                "The world has reached its item limit.",
                "Świat osiągnął limit przedmiotów."
            )
        );
        Ok(count)
    }
    fn quantity_count(&self, item_id: u16, quantity: u8) -> Result<u8> {
        let item = self.item(item_id).context("Missing item.")?;
        let count = if quantity == 0 {
            item.quantity
        } else {
            quantity
        };
        ensure!(
            count > 0 && count <= item.quantity,
            "{}",
            self.text(
                "Not enough units in this stack.",
                "W stosie nie ma tylu sztuk."
            )
        );
        Ok(count)
    }
    fn transfer_quantity(
        &mut self,
        item_id: u16,
        quantity: u8,
        owner: Option<u16>,
        room: Option<u16>,
    ) -> Result<(u16, u8)> {
        let count = self.transfer_count(item_id, quantity)?;
        let mut part = self.item(item_id).context("Missing item.")?.clone();
        if count == part.quantity {
            self.transfer(item_id, owner, room)?;
            return Ok((item_id, count));
        }
        part.id = self.allocate_id()?;
        self.world
            .items
            .iter_mut()
            .find(|i| i.id == item_id)
            .context("Missing item.")?
            .quantity -= count;
        part.quantity = count;
        part.owner_id = owner;
        part.room_id = room;
        part.equipped = false;
        let moved = part.id;
        self.world.items.push(part);
        Ok((moved, count))
    }
    fn consume(&mut self, id: u16) -> Result<()> {
        let item = self
            .world
            .items
            .iter_mut()
            .find(|i| i.id == id)
            .context("Missing item.")?;
        ensure!(item.quantity > 0, "Cannot consume an empty resource.");
        item.quantity -= 1;
        let gone = item.quantity == 0;
        let owner = item.owner_id;
        if gone {
            if let Some(owner) = owner {
                self.actor_mut(owner)?.equipment.retain(|item| *item != id);
            }
            self.world.items.retain(|i| i.id != id);
        }
        Ok(())
    }
    fn plot_causes(&self, source: u16, victim: u16) -> Vec<u64> {
        self.world
            .plots
            .iter()
            .filter(|p| p.accepted && p.recipient == source && p.victim == victim)
            .map(|p| p.event_id)
            .collect()
    }
    fn credited_player(&self, source: u16, victim: u16) -> Option<u16> {
        if self.is_player(source) {
            return Some(source);
        }
        self.world
            .plots
            .iter()
            .filter(|p| {
                p.accepted
                    && self.is_player(p.proposer)
                    && p.recipient == source
                    && p.victim == victim
            })
            .min_by_key(|p| p.accepted_event_id.unwrap_or(p.event_id))
            .map(|p| p.proposer)
    }
    fn damage(
        &mut self,
        target_id: u16,
        amount: f64,
        source: u16,
        visible_source: bool,
        causes: Vec<u64>,
        description: String,
    ) -> Result<()> {
        if !self.actor(target_id)?.alive() {
            return Ok(());
        }
        let target = self.actor_mut(target_id)?;
        let before = target.health;
        target.health = clamp(target.health - amount);
        let dealt = before - target.health;
        target.flags &= !HIDDEN;
        let room = target.room_id;
        let mut audience = self.witnesses(room);
        audience.push(target_id);
        let injury_text = self.text(
            &format!(
                "{} is hurt by {description}: −{} HP ({} HP left).",
                self.name(target_id),
                number_text(dealt),
                number_text(self.actor(target_id)?.health)
            ),
            &format!(
                "{} odnosi obrażenia: {description}: −{} HP (pozostało {} HP).",
                self.name(target_id),
                number_text(dealt),
                number_text(self.actor(target_id)?.health)
            ),
        );
        let perceptions = if visible_source {
            vec![]
        } else {
            audience
                .iter()
                .map(|id| Perception {
                    actor_id: *id,
                    room_id: room,
                    source_id: None,
                    text: injury_text.clone(),
                    details: Some(EventDetails::Injury),
                })
                .collect()
        };
        let event = self.record_with_perceptions(
            "injury",
            Some(source),
            Some(room),
            injury_text,
            Some(audience.clone()),
            causes.clone(),
            None,
            perceptions,
            Some(EventDetails::Injury),
        )?;
        if !self.actor(target_id)?.alive() {
            self.interrupt_activity(target_id)?;
            let target = self.actor_mut(target_id)?;
            target.clear_plan();
            target.invalidate_decision()?;
            target.unconscious_until = None;
            target.flags &= !UNCONSCIOUS;
            let inventory: Vec<_> = self
                .inventory(target_id)
                .into_iter()
                .map(|i| i.id)
                .collect();
            for item in inventory {
                self.transfer(item, None, Some(room))?;
                self.world
                    .items
                    .iter_mut()
                    .find(|i| i.id == item)
                    .context("Missing item.")?
                    .visible = true;
            }
            let mut causal = vec![event.id];
            causal.extend(causes);
            let death_text = self.text(
                &format!("{} dies.", self.name(target_id)),
                &format!("{} umiera.", self.name(target_id)),
            );
            let perceptions = if visible_source {
                vec![]
            } else {
                audience
                    .iter()
                    .map(|id| Perception {
                        actor_id: *id,
                        room_id: room,
                        source_id: None,
                        text: death_text.clone(),
                        details: Some(EventDetails::Death),
                    })
                    .collect()
            };
            self.record_with_perceptions(
                "death",
                Some(source),
                Some(room),
                death_text,
                Some(audience.clone()),
                causal,
                None,
                perceptions,
                Some(EventDetails::Death),
            )?;
            if target_id == KING {
                self.world.winner_id = self.credited_player(source, target_id);
                self.world.king_killed_by_player = self.world.winner_id.is_some();
            }
            for witness in audience.into_iter().filter(|id| *id != target_id) {
                self.request_decision(
                    witness,
                    self.text(
                        &format!("You witnessed {}’s death.", self.name(target_id)),
                        &format!("Widzisz śmierć osoby {}.", self.name(target_id)),
                    ),
                    true,
                    true,
                )?;
            }
        } else {
            self.request_decision(
                target_id,
                self.text(
                    &format!("You were hurt by {description}. React to the danger."),
                    &format!("Odnosisz obrażenia: {description}. Zareaguj na zagrożenie."),
                ),
                true,
                true,
            )?;
        }
        self.update_ending()?;
        Ok(())
    }
    fn update_ending(&mut self) -> Result<()> {
        if self.world.mode == "multiplayer" {
            if !self.actor(KING)?.alive() {
                self.world.ending = if self.world.winner_id.is_some() {
                    "victory"
                } else {
                    "death"
                }
                .into();
            }
            return Ok(());
        }
        let player = self.actor(PLAYER)?;
        if !player.alive() {
            self.world.ending = "death".into();
        } else if self.world.escaped
            && self.world.king_killed_by_player
            && player.flags & RESTRAINED == 0
            && !self.room(player.room_id)?.prison
        {
            self.world.ending = "victory".into();
        }
        Ok(())
    }

    fn execute(&mut self, id: u16, intent: Intent, causes: Vec<u64>) -> Result<ActionResult> {
        let start = self.deferred_events.len();
        let mut result = self.execute_action(id, &intent, causes)?;
        if result.ok
            && !matches!(intent, Intent::Speak { .. } | Intent::Inspect { .. })
            && let Some(event) = self
                .deferred_events
                .iter()
                .skip(start)
                .rev()
                .find(|event| event.actor_id == Some(id) && event.audience.contains(&id))
        {
            result.message = event
                .projected(id)
                .context("Missing result perception")?
                .text;
            result.event_id = Some(event.id);
        }
        if result.ok && !matches!(intent, Intent::Inspect { .. }) {
            let sound = match &intent {
                Intent::Move { .. } | Intent::Flee { .. } | Intent::Escort { .. } => Some("step"),
                Intent::Open { .. } | Intent::Close { .. } => Some("door"),
                Intent::Unlock { .. } | Intent::Lock { .. } => Some("lock"),
                Intent::Take { .. }
                | Intent::Drop { .. }
                | Intent::Equip { .. }
                | Intent::Give { .. }
                | Intent::Offer { .. }
                | Intent::AcceptOffer { .. } => Some("item"),
                Intent::Steal { .. } | Intent::Bind { .. } | Intent::Release { .. } => {
                    Some("cloth")
                }
                Intent::DeclineOffer { .. } => Some("notice"),
                Intent::Pay { .. } => Some("coin"),
                Intent::Eat { .. } => Some("eat"),
                Intent::Drink { .. } => Some("water"),
                Intent::Work { .. } => Some("craft"),
                Intent::Speak {
                    volume: Volume::Whisper,
                    ..
                } => Some("whisper"),
                Intent::Speak { .. } => Some("speech"),
                Intent::Use { mode, .. } => Some(match mode {
                    UseMode::Poison => "water",
                    UseMode::PickLock => "lock",
                    _ => "cloth",
                }),
                _ => None,
            };
            if let Some(sound) = sound {
                let mut changed = 0;
                for event in self.deferred_events.iter_mut().skip(start).filter(|e| {
                    e.actor_id == Some(id)
                        && matches!(e.kind.as_str(), "action" | "speech" | "private")
                }) {
                    event.sound = Some(sound.into());
                    self.store.update_event(event)?;
                    changed += 1;
                    if !intent.travel() && changed >= 1 {
                        break;
                    }
                }
            }
        }
        self.remember_outcome(id, intent, &result, true)?;
        Ok(result)
    }
    fn execute_action(
        &mut self,
        id: u16,
        intent: &Intent,
        causes: Vec<u64>,
    ) -> Result<ActionResult> {
        if let Err(error) = self.validate_intent(id, intent) {
            let room = self.actor(id)?.room_id;
            self.actor_mut(id)?.clear_plan();
            self.record(
                "feedback",
                Some(id),
                Some(room),
                self.text(
                    &format!("Your attempt fails: {error}"),
                    &format!("Próba nieudana: {error}"),
                ),
                Some(vec![id]),
                causes,
                None,
            )?;
            return Ok(ActionResult::fail(error.to_string()));
        }
        let actor = self.actor(id)?.clone();
        let room = actor.room_id;
        let name = self.name(id);
        let item = intent.item_id().and_then(|id| self.item(id)).cloned();
        let door = intent.door_id().and_then(|id| self.door(id)).cloned();
        match intent {
            Intent::Move { .. } | Intent::Flee { .. } | Intent::Escort { .. } => {
                let door = door.context("Missing door.")?;
                let destination = self.destination(&door, room);
                let hidden = actor.flags & HIDDEN != 0;
                self.record_with_perceptions(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("{name} leaves through {}.", self.name(door.id)),
                        &format!("{name} wychodzi przez {}.", self.name(door.id)),
                    ),
                    if hidden { Some(vec![id]) } else { None },
                    causes.clone(),
                    None,
                    vec![],
                    Some(EventDetails::Left),
                )?;
                {
                    let actor = self.actor_mut(id)?;
                    actor.room_id = destination;
                    if !actor.known_rooms.contains(&destination) {
                        actor.known_rooms.push(destination);
                    }
                }
                if let Intent::Escort { target_id, .. } = intent {
                    self.interrupt_activity(*target_id)?;
                    let target = self.actor_mut(*target_id)?;
                    target.room_id = destination;
                    target.clear_plan();
                    target.invalidate_decision()?;
                    if !target.known_rooms.contains(&destination) {
                        target.known_rooms.push(destination);
                    }
                    let mut audience = self.witnesses(destination);
                    audience.push(*target_id);
                    let event = self.record_with_perceptions(
                        "action",
                        Some(id),
                        Some(destination),
                        self.text(
                            &format!(
                                "{name} escorts {} through {}.",
                                self.name(*target_id),
                                self.name(door.id)
                            ),
                            &format!(
                                "{name} eskortuje osobę {} przez {}.",
                                self.name(*target_id),
                                self.name(door.id)
                            ),
                        ),
                        Some(audience),
                        causes.clone(),
                        None,
                        vec![],
                        Some(EventDetails::Entered),
                    )?;
                    self.actor_mut(*target_id)?.entered_event_id = event.id;
                }
                let watchers: Vec<_> = self
                    .world
                    .actors
                    .iter()
                    .filter(|a| a.room_id == destination && a.id != id && a.awake())
                    .cloned()
                    .collect();
                let noticed = !hidden
                    || watchers.iter().any(|a| {
                        self.random()
                            < ((a.skills.stealth + 40.0 - actor.skills.stealth - a.fatigue / 2.0)
                                / 100.0)
                                .clamp(0.08, 0.8)
                    });
                if noticed {
                    self.actor_mut(id)?.flags &= !HIDDEN;
                }
                let event = self.record_with_perceptions(
                    "action",
                    Some(id),
                    Some(destination),
                    self.text(
                        &format!("{name} enters {}.", self.name(destination)),
                        &format!("{name} wchodzi do {}.", self.name(destination)),
                    ),
                    if noticed { None } else { Some(vec![id]) },
                    causes.clone(),
                    None,
                    vec![],
                    Some(EventDetails::Entered),
                )?;
                {
                    let actor = self.actor_mut(id)?;
                    actor.entered_event_id = event.id;
                    actor.invalidate_decision()?;
                    actor.plan.retain(|p| !matches!(p, Intent::Attack { .. }));
                }
                let neighbors: Vec<_> = self
                    .nearby(id)?
                    .into_iter()
                    .map(|a| (a.id, a.role.clone()))
                    .collect();
                for (witness, _role) in neighbors {
                    if !self.actor(id)?.known_actors.contains(&witness) {
                        self.actor_mut(id)?.known_actors.push(witness);
                    }
                    if witness != id && noticed {
                        self.request_decision(
                            witness,
                            self.text(
                                "Someone has entered your room. Reassess the situation.",
                                "Ktoś wszedł do pomieszczenia. Oceń sytuację.",
                            ),
                            false,
                            true,
                        )?;
                    }
                }
                // Departure invalidates inference based on an actor who is no longer present.
                let witnesses = self.witnesses(room);
                for witness in witnesses {
                    if witness != id {
                        self.request_decision(
                            witness,
                            self.text("Someone has left your room.", "Ktoś opuścił pomieszczenie."),
                            false,
                            true,
                        )?;
                    }
                }
                if self.is_player(id) && room == DUNGEON && !self.room(destination)?.prison {
                    self.world.escaped = true;
                    self.record(
                        "notice",
                        Some(id),
                        Some(destination),
                        self.text(
                            "You have escaped the dungeon. The king is still alive.",
                            "Uciekasz z lochu. Król wciąż żyje.",
                        ),
                        Some(vec![id]),
                        vec![],
                        None,
                    )?;
                }
                if let Some(position) = self.world.traps.iter().position(|t| t.door_id == door.id) {
                    let trap = self.world.traps.remove(position);
                    self.damage(
                        id,
                        trap.power,
                        trap.setter,
                        false,
                        trap.causes,
                        self.text("a hidden tripline", "ukryta lina-pułapka"),
                    )?;
                }
            }
            Intent::Inspect { target_id } => {
                let text = if let Some(item) = self.accessible(id, *target_id) {
                    let mut text = self.text(
                        &format!(
                            "{}: {} ({} remaining).",
                            self.name(item.id),
                            self.item_text(item, "description"),
                            item.quantity
                        ),
                        &format!(
                            "{}: {} (pozostało: {}).",
                            self.name(item.id),
                            self.item_text(item, "description"),
                            item.quantity
                        ),
                    );
                    if item.poisoned_by.is_some()
                        && (actor.skills.medicine >= 60.0 || item.poisoned_by == Some(id))
                    {
                        text.push_str(
                            &self.text(" You recognize poison in it.", " Rozpoznajesz truciznę."),
                        );
                    }
                    text
                } else if let Some(door) = self.exits(room).into_iter().find(|d| d.id == *target_id)
                {
                    self.text(
                        &format!(
                            "{}: {}{}. It connects to {}.",
                            self.name(door.id),
                            if door.flags & OPEN != 0 {
                                "open"
                            } else {
                                "closed"
                            },
                            if door.flags & LOCKED != 0 {
                                ", locked"
                            } else {
                                ""
                            },
                            self.name(self.destination(door, room))
                        ),
                        &format!(
                            "{}: {}{}. Prowadzi do {}.",
                            self.name(door.id),
                            if door.flags & OPEN != 0 {
                                "otwarte"
                            } else {
                                "zamknięte"
                            },
                            if door.flags & LOCKED != 0 {
                                ", na klucz"
                            } else {
                                ""
                            },
                            self.name(self.destination(door, room))
                        ),
                    )
                } else if let Some(other) =
                    self.nearby(id)?.into_iter().find(|a| a.id == *target_id)
                {
                    let equipment = self
                        .visible_items(id)?
                        .iter()
                        .filter(|i| i.owner_id == Some(other.id))
                        .map(|i| format!("{} [{}]", self.name(i.id), i.id))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.text(
                        &format!(
                            "{}, {}: {}, {}. Visible belongings: {}.",
                            self.name(other.id),
                            self.role(other),
                            self.condition(other),
                            self.activity_text(other),
                            if equipment.is_empty() {
                                "none"
                            } else {
                                &equipment
                            }
                        ),
                        &format!(
                            "{}, {}: {}, {}. Widoczne przedmioty: {}.",
                            self.name(other.id),
                            self.role(other),
                            self.condition(other),
                            self.activity_text(other),
                            if equipment.is_empty() {
                                "brak"
                            } else {
                                &equipment
                            }
                        ),
                    )
                } else {
                    self.authored(&self.room(room)?.description)
                };
                self.record(
                    "private",
                    Some(id),
                    Some(room),
                    text.clone(),
                    Some(vec![id]),
                    causes,
                    None,
                )?;
                return Ok(ActionResult::ok(text));
            }
            Intent::Take {
                item_id, quantity, ..
            } => {
                let (moved, count) = self.transfer_quantity(*item_id, *quantity, Some(id), None)?;
                self.actor_mut(id)?.last_acquired_item_id = moved;
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("{name} takes {} ×{count}.", self.name(moved)),
                        &format!("{name} zabiera {} ×{count}.", self.name(moved)),
                    ),
                    None,
                    causes,
                    None,
                )?;
            }
            Intent::Steal {
                item_id, quantity, ..
            } => {
                let item = item.context("Missing item.")?;
                let owner = self
                    .actor(item.owner_id.context("Missing owner.")?)?
                    .clone();
                let chance = theft_chance(
                    actor.skills.stealth,
                    actor.flags & HIDDEN != 0,
                    owner.skills.stealth,
                    owner.skills.combat,
                    owner.fatigue,
                );
                if self.random() < chance {
                    let (moved, count) =
                        self.transfer_quantity(*item_id, *quantity, Some(id), None)?;
                    self.actor_mut(id)?.last_acquired_item_id = moved;
                    self.record(
                        "private",
                        Some(id),
                        Some(room),
                        self.text(
                            &format!(
                                "You steal {} ×{count} without being noticed.",
                                self.name(moved)
                            ),
                            &format!("Kradniesz {} ×{count} niezauważenie.", self.name(moved)),
                        ),
                        Some(vec![id]),
                        causes,
                        None,
                    )?;
                } else {
                    self.actor_mut(id)?.flags &= !HIDDEN;
                    self.record(
                        "action",
                        Some(id),
                        Some(room),
                        self.text(
                            &format!(
                                "{} catches {name} reaching for {}.",
                                self.name(owner.id),
                                self.name(*item_id)
                            ),
                            &format!(
                                "{} przyłapuje osobę {name} na próbie kradzieży {}.",
                                self.name(owner.id),
                                self.name(*item_id)
                            ),
                        ),
                        None,
                        causes,
                        None,
                    )?;
                    self.request_decision(
                        owner.id,
                        self.text(
                            &format!("{name} tried to steal from you."),
                            &format!("{name} próbował cię okraść."),
                        ),
                        true,
                        true,
                    )?;
                    self.actor_mut(id)?.clear_plan();
                    return Ok(ActionResult::fail(
                        self.text("Your theft is noticed.", "Kradzież zostaje zauważona."),
                    ));
                }
            }
            Intent::Drop {
                item_id, quantity, ..
            } => {
                let (moved, count) =
                    self.transfer_quantity(*item_id, *quantity, None, Some(room))?;
                self.world
                    .items
                    .iter_mut()
                    .find(|i| i.id == moved)
                    .context("Missing item.")?
                    .visible = true;
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("{name} puts down {} ×{count}.", self.name(moved)),
                        &format!("{name} odkłada {} ×{count}.", self.name(moved)),
                    ),
                    None,
                    causes,
                    None,
                )?;
            }
            Intent::Equip { item_id } => {
                let item = item.context("Missing item.")?;
                if matches!(item.kind, ItemKind::Weapon | ItemKind::Clothes) {
                    let old: Vec<_> = self
                        .inventory(id)
                        .into_iter()
                        .filter(|i| i.kind == item.kind && i.equipped)
                        .map(|i| i.id)
                        .collect();
                    for previous in old {
                        self.world
                            .items
                            .iter_mut()
                            .find(|i| i.id == previous)
                            .context("Missing equipment.")?
                            .equipped = false;
                        self.actor_mut(id)?
                            .equipment
                            .retain(|item| *item != previous);
                    }
                }
                let item_mut = self
                    .world
                    .items
                    .iter_mut()
                    .find(|i| i.id == *item_id)
                    .context("Missing item.")?;
                item_mut.equipped = true;
                item_mut.visible = true;
                let actor = self.actor_mut(id)?;
                if !actor.equipment.contains(item_id) {
                    actor.equipment.push(*item_id);
                }
                if item.kind == ItemKind::Clothes {
                    actor.flags |= DISGUISED;
                }
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("{name} equips {}.", self.name(*item_id)),
                        &format!("{name} wyposaża {}.", self.name(*item_id)),
                    ),
                    None,
                    causes,
                    None,
                )?;
            }
            Intent::Give {
                item_id,
                target_id,
                quantity,
                ..
            } => {
                let (_, count) =
                    self.transfer_quantity(*item_id, *quantity, Some(*target_id), None)?;
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!(
                            "{name} gives {} ×{count} to {}.",
                            self.name(*item_id),
                            self.name(*target_id)
                        ),
                        &format!(
                            "{name} przekazuje {} ×{count} osobie {}.",
                            self.name(*item_id),
                            self.name(*target_id)
                        ),
                    ),
                    None,
                    causes,
                    None,
                )?;
                self.request_decision(
                    *target_id,
                    self.text(
                        &format!("{name} has given you {}.", self.name(*item_id)),
                        &format!("{name} przekazuje ci {}.", self.name(*item_id)),
                    ),
                    false,
                    true,
                )?;
            }
            Intent::Offer {
                item_id,
                target_id,
                quantity,
                ..
            } => {
                let count = self.quantity_count(*item_id, *quantity)?;
                let event = self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!(
                            "{name} offers {} ×{count} to {}.",
                            self.name(*item_id),
                            self.name(*target_id)
                        ),
                        &format!(
                            "{name} oferuje {} ×{count} osobie {}.",
                            self.name(*item_id),
                            self.name(*target_id)
                        ),
                    ),
                    None,
                    causes,
                    None,
                )?;
                let offer = Offer {
                    id: self.world.next_offer_id as u16,
                    from: id,
                    to: *target_id,
                    item_id: *item_id,
                    quantity: count,
                    expires: self.world.time + self.tuning.offer_seconds * CLOCK_RATE,
                    interaction_expires: Some(
                        self.world.interaction_time + self.tuning.offer_seconds,
                    ),
                    event_id: event.id,
                    status: "open".into(),
                };
                self.world.next_offer_id += 1;
                self.world.offers.push(offer);
                self.request_decision(
                    *target_id,
                    self.text(
                        &format!(
                            "{name} offers you {}. Accept or decline.",
                            self.name(*item_id)
                        ),
                        &format!(
                            "{name} oferuje ci {}. Przyjmij lub odrzuć.",
                            self.name(*item_id)
                        ),
                    ),
                    false,
                    true,
                )?;
            }
            Intent::Pay { target_id, amount } => {
                self.actor_mut(id)?.coins -= *amount;
                self.actor_mut(*target_id)?.coins += *amount;
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("{name} pays {} {amount} coins.", self.name(*target_id)),
                        &format!(
                            "{name} płaci osobie {}: {amount} monet.",
                            self.name(*target_id)
                        ),
                    ),
                    None,
                    causes,
                    None,
                )?;
                self.request_decision(
                    *target_id,
                    self.text(
                        &format!("{name} paid you {amount} actual coins."),
                        &format!("{name} zapłacił ci {amount} prawdziwych monet."),
                    ),
                    false,
                    true,
                )?;
            }
            Intent::AcceptOffer { offer_id } | Intent::DeclineOffer { offer_id } => {
                let offer = self
                    .world
                    .offers
                    .iter()
                    .find(|o| o.id == *offer_id)
                    .context("Missing offer.")?
                    .clone();
                let status = if matches!(intent, Intent::AcceptOffer { .. }) {
                    let (moved, _) =
                        self.transfer_quantity(offer.item_id, offer.quantity, Some(id), None)?;
                    self.actor_mut(id)?.last_acquired_item_id = moved;
                    "accepted"
                } else {
                    "declined"
                };
                self.world
                    .offers
                    .iter_mut()
                    .find(|o| o.id == *offer_id)
                    .context("Missing offer.")?
                    .status = status.into();
                let mut causal = vec![offer.event_id];
                causal.extend(causes);
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!(
                            "{name} {status} {}’s offer of {} ×{}.",
                            self.name(offer.from),
                            self.name(offer.item_id),
                            if offer.quantity == 0 {
                                self.item(offer.item_id).map_or(0, |i| i.quantity)
                            } else {
                                offer.quantity
                            }
                        ),
                        &format!(
                            "{name} {} ofertę osoby {}: {} ×{}.",
                            if status == "accepted" {
                                "przyjmuje"
                            } else {
                                "odrzuca"
                            },
                            self.name(offer.from),
                            self.name(offer.item_id),
                            if offer.quantity == 0 {
                                self.item(offer.item_id).map_or(0, |i| i.quantity)
                            } else {
                                offer.quantity
                            }
                        ),
                    ),
                    None,
                    causal,
                    None,
                )?;
                self.request_decision(
                    offer.from,
                    self.text(
                        &format!("{name} {status} your offer."),
                        &format!(
                            "{name} {} twoją ofertę.",
                            if status == "accepted" {
                                "przyjmuje"
                            } else {
                                "odrzuca"
                            }
                        ),
                    ),
                    false,
                    true,
                )?;
            }
            Intent::Unlock { door_id }
            | Intent::Lock { door_id }
            | Intent::Open { door_id }
            | Intent::Close { door_id } => {
                let target = self
                    .world
                    .doors
                    .iter_mut()
                    .find(|d| d.id == *door_id)
                    .context("Missing door.")?;
                let (verb, polish) = match intent {
                    Intent::Unlock { .. } => {
                        target.flags &= !LOCKED;
                        ("unlocks", "odblokowuje")
                    }
                    Intent::Lock { .. } => {
                        target.flags |= LOCKED;
                        ("locks", "zamyka na klucz")
                    }
                    Intent::Open { .. } => {
                        target.flags |= OPEN;
                        ("opens", "otwiera")
                    }
                    _ => {
                        target.flags &= !OPEN;
                        ("closes", "zamyka")
                    }
                };
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("{name} {verb} {}.", self.name(*door_id)),
                        &format!("{name} {polish} {}.", self.name(*door_id)),
                    ),
                    None,
                    causes,
                    None,
                )?;
                let witnesses = self.witnesses(room);
                for witness in witnesses {
                    if witness != id {
                        self.request_decision(
                            witness,
                            self.text("A doorway has changed.", "Stan przejścia się zmienił."),
                            false,
                            true,
                        )?;
                    }
                }
            }
            Intent::Eat { item_id } | Intent::Drink { item_id } => {
                let item = item.context("Missing item.")?;
                let item_name = self.name(*item_id);
                let eating = matches!(intent, Intent::Eat { .. });
                let before_need = if eating { actor.hunger } else { actor.thirst };
                self.consume(*item_id)?;
                let actor = self.actor_mut(id)?;
                if matches!(intent, Intent::Eat { .. }) {
                    actor.hunger = clamp(actor.hunger - item.power);
                } else {
                    actor.thirst = clamp(actor.thirst - item.power);
                }
                let delta = before_need - if eating { actor.hunger } else { actor.thirst };
                let event = self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!(
                            "{name} {} {item_name} ×1.",
                            if matches!(intent, Intent::Eat { .. }) {
                                "eats"
                            } else {
                                "drinks"
                            }
                        ),
                        &format!(
                            "{name} {} {item_name} ×1.",
                            if matches!(intent, Intent::Eat { .. }) {
                                "je"
                            } else {
                                "pije"
                            }
                        ),
                    ),
                    None,
                    causes,
                    None,
                )?;
                self.record(
                    "feedback",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!(
                            "{}: −{}; consumed 1 unit.",
                            if eating { "Hunger" } else { "Thirst" },
                            number_text(delta)
                        ),
                        &format!(
                            "{}: −{}; zużyto 1 sztukę.",
                            if eating { "Głód" } else { "Pragnienie" },
                            number_text(delta)
                        ),
                    ),
                    Some(vec![id]),
                    vec![event.id],
                    None,
                )?;
                if let Some(source) = item.poisoned_by {
                    let mut causal = item.causes;
                    causal.push(event.id);
                    causal.extend(self.plot_causes(source, id));
                    self.world.poison.push(PoisonEffect {
                        actor_id: id,
                        source_id: source,
                        damage: 35.0,
                        next: self.world.time + self.tuning.poison_seconds * CLOCK_RATE,
                        interaction_next: Some(
                            self.world.interaction_time + self.tuning.poison_seconds,
                        ),
                        remaining: 4,
                        causes: causal,
                    });
                }
            }
            Intent::Rest { .. }
            | Intent::Wait { .. }
            | Intent::Guard { .. }
            | Intent::Defend { .. }
            | Intent::Hide { .. } => {
                self.record(
                    "feedback",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("Your {} ends.", intent.kind()),
                        &format!("Kończysz: {}.", self.action_verb(intent)),
                    ),
                    Some(vec![id]),
                    causes,
                    None,
                )?;
            }
            Intent::Work { recipe } => {
                let origin = TextOrigin::Recipe(*recipe);
                let recipe = recipe_spec(*recipe);
                let ingredient = self
                    .ingredient(id, recipe.ingredient)
                    .context("Missing ingredient.")?
                    .id;
                let ingredient_name = self.name(ingredient);
                self.consume(ingredient)?;
                let item_id = self.allocate_id()?;
                let created = Item {
                    id: item_id,
                    name: self.text(recipe.name, recipe.polish_name),
                    description: self.text(recipe.description, recipe.polish_description),
                    kind: recipe.kind,
                    quantity: 1,
                    owner_id: Some(id),
                    room_id: None,
                    equipped: false,
                    visible: true,
                    key: None,
                    power: recipe.power,
                    text_origin: Some(origin),
                    poisoned_by: None,
                    causes: vec![],
                };
                self.world.items.push(created);
                let event = self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!(
                            "{name} makes {}, using one {ingredient_name}.",
                            self.name(item_id)
                        ),
                        &format!(
                            "{name} przygotowuje {}, zużywając jeden składnik: {ingredient_name}.",
                            self.name(item_id)
                        ),
                    ),
                    None,
                    causes,
                    None,
                )?;
                self.world
                    .items
                    .iter_mut()
                    .find(|i| i.id == item_id)
                    .context("Missing crafted item.")?
                    .causes = vec![event.id];
            }
            Intent::Attack { .. } => return self.choose_combat(id, intent.clone(), causes),
            Intent::Bind { target_id } => {
                let rope = self
                    .ingredient(id, ItemKind::Rope)
                    .context("Missing rope.")?
                    .id;
                self.consume(rope)?;
                self.actor_mut(*target_id)?.flags |= RESTRAINED;
                self.actor_mut(*target_id)?.invalidate_decision()?;
                let mut audience = self.witnesses(room);
                audience.push(*target_id);
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("{name} binds {}.", self.name(*target_id)),
                        &format!("{name} wiąże osobę {}.", self.name(*target_id)),
                    ),
                    Some(audience),
                    causes,
                    None,
                )?;
            }
            Intent::Release { target_id } => {
                self.actor_mut(*target_id)?.flags &= !RESTRAINED;
                self.actor_mut(*target_id)?.invalidate_decision()?;
                self.record(
                    "action",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("{name} releases {}’s bonds.", self.name(*target_id)),
                        &format!("{name} rozwiązuje więzy osoby {}.", self.name(*target_id)),
                    ),
                    None,
                    causes,
                    None,
                )?;
                self.request_decision(
                    *target_id,
                    self.text(
                        "Your bonds have been released.",
                        "Twoje więzy zostały rozwiązane.",
                    ),
                    false,
                    false,
                )?;
            }
            Intent::Use {
                item_id,
                target_id,
                mode,
            } => {
                let item = item.context("Missing item.")?;
                match mode {
                    UseMode::Heal => {
                        let target = self.actor_mut(*target_id)?;
                        let before = target.health;
                        let was_unconscious = target.flags & UNCONSCIOUS != 0;
                        target.health = clamp(target.health + item.power);
                        let healed = target.health - before;
                        target.flags &= !UNCONSCIOUS;
                        target.unconscious_until = None;
                        target.invalidate_decision()?;
                        let had_poison = self.world.poison.iter().any(|p| p.actor_id == *target_id);
                        self.world.poison.retain(|p| p.actor_id != *target_id);
                        let item_name = self.name(*item_id);
                        self.consume(*item_id)?;
                        self.record(
                            "action",
                            Some(id),
                            Some(room),
                            self.text(
                                &format!(
                                    "{name} treats {} with {item_name}: +{} HP{}; consumed 1 unit.",
                                    self.name(*target_id), number_text(healed), if was_unconscious { "; conscious" } else { "" }
                                ),
                                &format!(
                                    "{name} leczy osobę {} przy użyciu {item_name}: +{} HP{}; zużyto 1 sztukę.",
                                    self.name(*target_id), number_text(healed), if was_unconscious { "; przytomność" } else { "" }
                                ),
                            ),
                            None,
                            causes,
                            None,
                        )?;
                        if had_poison {
                            self.record(
                                "private",
                                Some(id),
                                Some(room),
                                self.text(
                                    "Treatment removed your poisoning.",
                                    "Leczenie usunęło twoje zatrucie.",
                                ),
                                Some(vec![*target_id]),
                                vec![],
                                None,
                            )?;
                        }
                        self.request_decision(
                            *target_id,
                            self.text(
                                &format!(
                                    "{name} has treated your wounds. Reconsider what you need."
                                ),
                                &format!("{name} opatrzył twoje rany. Przemyśl swoje potrzeby."),
                            ),
                            false,
                            false,
                        )?;
                    }
                    UseMode::Poison => {
                        self.consume(*item_id)?;
                        let event = self.record(
                            "private",
                            Some(id),
                            Some(room),
                            self.text(
                                &format!("You put poison in {}.", self.name(*target_id)),
                                &format!("Dodajesz truciznę do {}.", self.name(*target_id)),
                            ),
                            Some(vec![id]),
                            causes,
                            None,
                        )?;
                        let target = self
                            .world
                            .items
                            .iter_mut()
                            .find(|i| i.id == *target_id)
                            .context("Missing poison target.")?;
                        target.poisoned_by = Some(id);
                        target.causes.push(event.id);
                        let witnesses: Vec<_> = self
                            .nearby(id)?
                            .into_iter()
                            .filter(|a| a.id != id && a.awake())
                            .map(|a| a.id)
                            .collect();
                        for witness in witnesses {
                            if actor.flags & HIDDEN == 0 && self.random() < 0.45 {
                                self.record(
                                    "notice",
                                    Some(id),
                                    Some(room),
                                    self.text(
                                        &format!(
                                            "{name} puts something into {}.",
                                            self.name(*target_id)
                                        ),
                                        &format!("{name} dodaje coś do {}.", self.name(*target_id)),
                                    ),
                                    Some(vec![witness]),
                                    vec![],
                                    None,
                                )?;
                                self.request_decision(
                                    witness,
                                    self.text(
                                        &format!(
                                            "You saw {name} put something into food or water."
                                        ),
                                        &format!(
                                            "Widzisz, jak {name} dodaje coś do jedzenia lub wody."
                                        ),
                                    ),
                                    false,
                                    true,
                                )?;
                            }
                        }
                    }
                    UseMode::PickLock => {
                        let lock = self.door(*target_id).context("Missing lock.")?.lock.clone();
                        let chance = lockpick_chance(
                            actor.skills.craft,
                            actor.skills.stealth,
                            lock.as_deref() == Some("royal"),
                        );
                        if self.random() < chance {
                            self.world
                                .doors
                                .iter_mut()
                                .find(|d| d.id == *target_id)
                                .context("Missing door.")?
                                .flags &= !LOCKED;
                            self.record(
                                "action",
                                Some(id),
                                Some(room),
                                self.text(
                                    &format!("{name} picks {}.", self.name(*target_id)),
                                    &format!("{name} otwiera wytrychem {}.", self.name(*target_id)),
                                ),
                                None,
                                causes,
                                None,
                            )?;
                        } else {
                            self.record(
                                "action",
                                Some(id),
                                Some(room),
                                self.text(
                                    &format!(
                                        "{name} fails to pick {}; metal scrapes loudly.",
                                        self.name(*target_id)
                                    ),
                                    &format!(
                                        "{name} nie otwiera zamka {}; metal głośno zgrzyta.",
                                        self.name(*target_id)
                                    ),
                                ),
                                None,
                                causes,
                                None,
                            )?;
                            let witnesses = self.witnesses(room);
                            for witness in witnesses {
                                if witness != id {
                                    self.request_decision(
                                        witness,
                                        self.text(
                                            &format!(
                                                "Someone is tampering with {}.",
                                                self.name(*target_id)
                                            ),
                                            &format!(
                                                "Ktoś manipuluje przy {}.",
                                                self.name(*target_id)
                                            ),
                                        ),
                                        false,
                                        true,
                                    )?;
                                }
                            }
                            self.actor_mut(id)?.clear_plan();
                            return Ok(ActionResult::fail(self.text(
                                "The lock resists your attempt.",
                                "Zamek opiera się twojej próbie.",
                            )));
                        }
                    }
                    UseMode::Disguise => {
                        return self.execute_action(
                            id,
                            &Intent::Equip { item_id: *item_id },
                            causes,
                        );
                    }
                    UseMode::Trap => {
                        self.consume(*item_id)?;
                        let event = self.record(
                            "private",
                            Some(id),
                            Some(room),
                            self.text(
                                &format!("You rig a rope across {}.", self.name(*target_id)),
                                &format!("Rozciągasz linę w przejściu {}.", self.name(*target_id)),
                            ),
                            Some(vec![id]),
                            causes.clone(),
                            None,
                        )?;
                        let mut causal = vec![event.id];
                        causal.extend(causes);
                        self.world.traps.push(Trap {
                            door_id: *target_id,
                            setter: id,
                            power: item.power,
                            causes: causal,
                        });
                    }
                    UseMode::CutBonds => {
                        return self.execute_action(
                            id,
                            &Intent::Release {
                                target_id: *target_id,
                            },
                            causes,
                        );
                    }
                }
            }
            Intent::Speak {
                text,
                target_id,
                volume,
                plot,
            } => return self.speak(id, text, *target_id, *volume, plot.as_ref(), causes),
        }
        self.update_ending()?;
        Ok(ActionResult::ok(
            self.text("The action is complete.", "Działanie zakończone."),
        ))
    }
    fn speak(
        &mut self,
        id: u16,
        text: &str,
        target: Option<u16>,
        volume: Volume,
        plot: Option<&PlotIntent>,
        causes: Vec<u64>,
    ) -> Result<ActionResult> {
        let actor = self.actor(id)?.clone();
        let room = actor.room_id;
        self.actor_mut(id)?.flags &= !HIDDEN;
        let name = self.name(id);
        let qualifier = match volume {
            Volume::Normal => "",
            Volume::Whisper => {
                if self.language == "pl" {
                    " szepcze"
                } else {
                    " whispers"
                }
            }
            Volume::Shout => {
                if self.language == "pl" {
                    " krzyczy"
                } else {
                    " shouts"
                }
            }
        };
        let canonical = format!("{name}{qualifier}: “{text}”");
        let mut audience = if volume == Volume::Whisper {
            vec![id, target.context("Whisper has no recipient.")?]
        } else {
            self.witnesses(room)
        };
        let mut perceptions = Vec::new();
        if volume == Volume::Shout {
            let mut adjacent: BTreeMap<u16, f64> = BTreeMap::new();
            for door in self.exits(room) {
                let loss = if door.flags & (OPEN | BARS) != 0 {
                    0.35
                } else {
                    0.65
                };
                let destination = self.destination(door, room);
                adjacent
                    .entry(destination)
                    .and_modify(|current| *current = f64::min(*current, loss))
                    .or_insert(loss);
            }
            let next_event = u64::try_from(self.store.db.query_row(
                "SELECT COALESCE(MAX(id),0)+1 FROM events",
                [],
                |r| r.get::<_, i64>(0),
            )?)?;
            for listener in &self.world.actors {
                if listener.room_id == room || !listener.awake() {
                    continue;
                }
                let Some(loss) = adjacent.get(&listener.room_id) else {
                    continue;
                };
                let recognized = listener.known_actors.contains(&id);
                let heard = mask_speech(text, *loss, next_event, id, listener.id);
                let speaker = if recognized {
                    name.clone()
                } else {
                    self.text("A distant voice", "Odległy głos")
                };
                perceptions.push(Perception {
                    actor_id: listener.id,
                    room_id: listener.room_id,
                    source_id: if recognized { Some(id) } else { None },
                    text: format!("{speaker}{qualifier}: “{heard}”"),
                    details: Some(EventDetails::Speech {
                        speaker_name: recognized.then(|| name.clone()),
                        recipient_id: None,
                        recipient_name: None,
                        recipient_known: false,
                        volume,
                    }),
                });
                audience.push(listener.id);
            }
        }
        let event = self.record_with_perceptions(
            "speech",
            Some(id),
            Some(room),
            canonical,
            Some(audience.clone()),
            causes,
            Some(if volume == Volume::Whisper {
                "whisper"
            } else {
                "speech"
            }),
            perceptions,
            Some(EventDetails::Speech {
                speaker_name: Some(name.clone()),
                recipient_id: target,
                recipient_name: target.map(|id| self.name(id)),
                recipient_known: true,
                volume,
            }),
        )?;
        if let Some(plot) = plot {
            let plot_id = self.world.next_plot_id as u16;
            self.world.next_plot_id += 1;
            self.world.plots.push(Plot {
                id: plot_id,
                proposer: id,
                recipient: target.context("Plot has no recipient.")?,
                victim: plot.victim_id,
                event_id: event.id,
                accepted: false,
                explicit: true,
                accepted_event_id: None,
            });
        }
        if volume == Volume::Whisper {
            let bystanders: Vec<_> = self
                .witnesses(room)
                .into_iter()
                .filter(|id| !audience.contains(id))
                .collect();
            self.record(
                "notice",
                Some(id),
                Some(room),
                self.text(
                    &format!(
                        "{name} whispers to {}.",
                        self.name(target.context("Missing whisper recipient.")?)
                    ),
                    &format!(
                        "{name} szepcze do osoby {}.",
                        self.name(target.context("Missing whisper recipient.")?)
                    ),
                ),
                Some(bystanders),
                vec![],
                None,
            )?;
        }
        let targets = if volume == Volume::Shout || target.is_none() {
            audience
                .into_iter()
                .filter(|listener| *listener != id)
                .collect::<Vec<_>>()
        } else {
            vec![target.context("Missing speech recipient.")?]
        };
        for listener in targets {
            if !self.is_player(listener) {
                let projected = event
                    .projected(listener)
                    .context("Missing speech perception.")?;
                let reason = self.text(
                    &format!(
                        "You heard {}. Respond if it merits a response.",
                        projected.text
                    ),
                    &format!(
                        "Słyszysz: {}. Odpowiedz, jeśli to wymaga odpowiedzi.",
                        projected.text
                    ),
                );
                let priority =
                    if self.is_player(id) || target == Some(listener) || volume == Volume::Shout {
                        WakePriority::Conversation
                    } else {
                        WakePriority::Routine
                    };
                // Hearing a direct conversation invalidates an older observation,
                // without cancelling the physical activity it described.
                self.queue_decision(
                    listener,
                    reason,
                    priority,
                    priority == WakePriority::Conversation,
                )?;
            }
        }
        Ok(ActionResult {
            ok: true,
            message: self.text(
                "Your words are spoken.",
                "Twoje słowa zostały wypowiedziane.",
            ),
            event_id: Some(event.id),
        })
    }

    /// Calendar progression is fixed; physical reaction windows use the unscaled interaction clock.
    pub fn advance(&mut self, real_seconds: f64) -> Result<()> {
        ensure!(
            real_seconds.is_finite() && real_seconds >= 0.0,
            "Time must advance by finite nonnegative durations."
        );
        if real_seconds == 0.0 || self.world.manual_pause || self.world.ending != "playing" {
            return Ok(());
        }
        self.mutate(|s| {
            let until = s.world.interaction_time + real_seconds;
            ensure!(until.is_finite(), "World clock overflow.");
            let mut steps = 0;
            while s.world.interaction_time < until && s.world.ending == "playing" {
                steps += 1;
                ensure!(steps <= 10000, "Too many events in one advance.");
                let current = s.world.interaction_time;
                let calendar = s.world.time;
                let mut next = until;
                for actor in &s.world.actors {
                    if let Some(activity) = &actor.activity {
                        let deadline = ((activity.ends - calendar) / CLOCK_RATE + current)
                            .max(activity.interaction_ends.unwrap_or(current));
                        if deadline > current {
                            next = next.min(deadline);
                        }
                    }
                    if let Some(deadline) = actor.unconscious_until
                        && deadline > current
                    {
                        next = next.min(deadline);
                    }
                }
                for encounter in &s.world.encounters {
                    if encounter.deadline > current {
                        next = next.min(encounter.deadline);
                    }
                }
                for effect in &s.world.poison {
                    let deadline = effect
                        .interaction_next
                        .unwrap_or(current + (effect.next - calendar) / CLOCK_RATE);
                    if deadline > current {
                        next = next.min(deadline);
                    }
                }
                for offer in &s.world.offers {
                    if offer.status == "open" {
                        let deadline = offer
                            .interaction_expires
                            .unwrap_or(current + (offer.expires - calendar) / CLOCK_RATE);
                        if deadline > current {
                            next = next.min(deadline);
                        }
                    }
                }
                s.world.interaction_time = next;
                s.world.time = calendar + (next - current) * CLOCK_RATE;
                s.update_needs((next - current) * PHYSIOLOGY_RATE)?;
                let recovery: Vec<_> = s
                    .world
                    .actors
                    .iter()
                    .filter(|a| {
                        a.alive()
                            && a.unconscious_until
                                .is_some_and(|deadline| deadline <= next + 1e-9)
                    })
                    .map(|a| a.id)
                    .collect();
                for actor_id in recovery {
                    let actor = s.actor_mut(actor_id)?;
                    actor.flags &= !UNCONSCIOUS;
                    actor.unconscious_until = None;
                    actor.invalidate_decision()?;
                    let room = actor.room_id;
                    let mut audience = s.witnesses(room);
                    audience.push(actor_id);
                    s.record(
                        "notice",
                        Some(actor_id),
                        Some(room),
                        s.text(
                            &format!("{} regains consciousness.", s.name(actor_id)),
                            &format!("{} odzyskuje przytomność.", s.name(actor_id)),
                        ),
                        Some(audience),
                        vec![],
                        Some("notice"),
                    )?;
                    s.request_decision(
                        actor_id,
                        s.text(
                            "You have regained consciousness.",
                            "Odzyskujesz przytomność.",
                        ),
                        false,
                        false,
                    )?;
                }
                let mut due: Vec<_> = s
                    .world
                    .actors
                    .iter()
                    .filter_map(|a| {
                        a.activity
                            .as_ref()
                            .filter(|activity| {
                                activity.ends <= s.world.time + 1e-9
                                    && activity
                                        .interaction_ends
                                        .is_none_or(|deadline| deadline <= next + 1e-9)
                            })
                            .map(|activity| (a.id, activity.ends))
                    })
                    .collect();
                due.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                for (actor_id, _) in due {
                    let activity = s.actor_mut(actor_id)?.activity.take();
                    let Some(activity) = activity else {
                        continue;
                    };
                    s.actor_mut(actor_id)?.invalidate_decision()?;
                    let failure = if s.actor(actor_id)?.awake() {
                        let result =
                            s.execute(actor_id, activity.intent, activity.causes.clone())?;
                        if result.ok
                            && let Some(before) = activity.started_vitals
                        {
                            let actor = s.actor(actor_id)?;
                            let after = [actor.health, actor.hunger, actor.thirst, actor.fatigue];
                            let changes = before
                                .iter()
                                .zip(after)
                                .zip(["HP", "hunger", "thirst", "fatigue"])
                                .map(|((before, after), label)| {
                                    format!(
                                        "{} {:+.2}",
                                        s.text(
                                            label,
                                            match label {
                                                "hunger" => "głód",
                                                "thirst" => "pragnienie",
                                                "fatigue" => "zmęczenie",
                                                _ => "HP",
                                            }
                                        ),
                                        after - before
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(" · ");
                            let message = s.text(
                                &format!("Net changes during the activity: {changes}."),
                                &format!("Łączne zmiany w czasie działania: {changes}."),
                            );
                            let room = actor.room_id;
                            s.record(
                                "feedback",
                                Some(actor_id),
                                Some(room),
                                message.clone(),
                                Some(vec![actor_id]),
                                activity.causes.clone(),
                                None,
                            )?;
                            let actor = s.actor_mut(actor_id)?;
                            if let Some(outcome) = &mut actor.last_action_outcome {
                                outcome.reason.push_str(&format!(" {message}"));
                            }
                            if let Some(outcome) = actor.recent_action_outcomes.last_mut() {
                                outcome.reason.push_str(&format!(" {message}"));
                            }
                        }
                        (!result.ok).then_some(result.message)
                    } else {
                        None
                    };
                    s.run_plan(actor_id, activity.causes, failure, true)?;
                }
                let encounters: Vec<_> = s.world.encounters.iter().map(|e| e.id.clone()).collect();
                for encounter in encounters {
                    s.maybe_resolve_combat(&encounter)?;
                }
                let mut position = 0;
                while position < s.world.poison.len() {
                    let effect = s.world.poison[position].clone();
                    let deadline = effect
                        .interaction_next
                        .unwrap_or(next + (effect.next - s.world.time) / CLOCK_RATE);
                    if deadline > next + 1e-9 {
                        position += 1;
                        continue;
                    }
                    s.damage(
                        effect.actor_id,
                        effect.damage,
                        effect.source_id,
                        false,
                        effect.causes.clone(),
                        s.text("poison", "trucizna"),
                    )?;
                    if effect.remaining <= 1 || !s.actor(effect.actor_id)?.alive() {
                        s.world.poison.remove(position);
                    } else {
                        let period = s.tuning.poison_seconds;
                        let effect = &mut s.world.poison[position];
                        effect.remaining -= 1;
                        effect.next += period * CLOCK_RATE;
                        effect.interaction_next = Some(deadline + period);
                        position += 1;
                    }
                }
                let expired: Vec<_> = s
                    .world
                    .offers
                    .iter()
                    .filter(|offer| offer.status == "open" && s.offer_expired(offer))
                    .map(|offer| offer.id)
                    .collect();
                for id in expired {
                    if let Some(offer) = s.world.offers.iter_mut().find(|offer| offer.id == id) {
                        offer.status = "expired".into();
                    }
                }
            }
            Ok(())
        })
    }
    fn update_needs(&mut self, game_seconds: f64) -> Result<()> {
        let mut wakes = Vec::new();
        let mut failing = Vec::new();
        let tuning = self.tuning.clone();
        for actor in &mut self.world.actors {
            if !actor.alive() {
                continue;
            }
            let previous = [actor.hunger, actor.thirst, actor.fatigue];
            let resting = actor
                .activity
                .as_ref()
                .is_some_and(|a| matches!(a.intent, Intent::Rest { .. }))
                || actor.flags & UNCONSCIOUS != 0;
            let projected = project_needs(actor, &tuning, game_seconds, resting);
            actor.hunger = projected.hunger;
            actor.thirst = projected.thirst;
            actor.fatigue = projected.fatigue;
            actor.health = projected.health_before_damage;
            let damage = projected.damage;
            if damage > 0.0 {
                failing.push((actor.id, damage, actor.thirst >= 100.0));
            }
            let current = [actor.hunger, actor.thirst, actor.fatigue];
            if current.iter().enumerate().any(|(index, value)| {
                tuning
                    .need_thresholds
                    .iter()
                    .any(|threshold| previous[index] < *threshold && *value >= *threshold)
            }) {
                wakes.push(actor.id);
            }
        }
        for id in wakes {
            self.request_decision(
                id,
                self.text(
                    "A bodily need has become difficult to ignore.",
                    "Jedna z potrzeb ciała staje się trudna do zignorowania.",
                ),
                false,
                false,
            )?;
        }
        for (id, damage, thirst) in failing {
            let before = self.actor(id)?.health;
            let after = clamp(before - damage);
            if [80.0, 35.0, 0.0]
                .iter()
                .any(|threshold| before > *threshold && after <= *threshold)
            {
                self.damage(
                    id,
                    damage,
                    id,
                    true,
                    vec![],
                    self.text(
                        if thirst { "dehydration" } else { "starvation" },
                        if thirst { "odwodnienie" } else { "głód" },
                    ),
                )?;
            } else {
                self.actor_mut(id)?.health = after;
            }
        }
        Ok(())
    }
    fn remember_outcome(
        &mut self,
        id: u16,
        intent: Intent,
        result: &ActionResult,
        completed: bool,
    ) -> Result<()> {
        let progress = completed
            && result.ok
            && !matches!(
                intent,
                Intent::Speak { .. }
                    | Intent::Inspect { .. }
                    | Intent::Wait { .. }
                    | Intent::Guard { .. }
                    | Intent::Hide { .. }
                    | Intent::Defend { .. }
            );
        let stage = if !result.ok {
            OutcomeStage::Failed
        } else if completed {
            OutcomeStage::Resolved
        } else if intent.combat_move() && self.encounter(id).is_some() {
            OutcomeStage::Accepted
        } else {
            OutcomeStage::Started
        };
        let outcome = ActionOutcome {
            intent,
            ok: result.ok,
            reason: result.message.clone(),
            time: self.world.time,
            stage,
        };
        let actor = self.actor_mut(id)?;
        actor.remember_action_outcome(outcome.clone());
        if !result.ok {
            actor.last_plan_failure = Some(outcome);
        }
        if progress {
            actor.correction_used = false;
        }
        Ok(())
    }
    fn interrupt_activity(&mut self, id: u16) -> Result<()> {
        let Some(activity) = self.actor_mut(id)?.activity.take() else {
            return Ok(());
        };
        let message = self.text(
            &format!("Interrupted: {}.", self.action_verb(&activity.intent)),
            &format!("Przerwano: {}.", self.action_verb(&activity.intent)),
        );
        let room = self.actor(id)?.room_id;
        self.record(
            "feedback",
            Some(id),
            Some(room),
            message.clone(),
            Some(vec![id]),
            activity.causes,
            None,
        )?;
        let time = self.world.time;
        self.actor_mut(id)?.remember_action_outcome(ActionOutcome {
            intent: activity.intent,
            ok: false,
            reason: message,
            time,
            stage: OutcomeStage::Interrupted,
        });
        Ok(())
    }
    pub fn claim_correction(&mut self, id: u16) -> Result<bool> {
        self.mutate(|s| {
            let actor = s.actor_mut(id)?;
            let available = !actor.correction_used;
            actor.correction_used = true;
            Ok(available)
        })
    }
    fn run_plan(
        &mut self,
        id: u16,
        causes: Vec<u64>,
        mut failure: Option<String>,
        completed: bool,
    ) -> Result<()> {
        if failure.is_some() {
            self.actor_mut(id)?.clear_plan();
        }
        let mut steps = 0;
        while !self.actor(id)?.plan.is_empty()
            && self.actor(id)?.activity.is_none()
            && self.actor(id)?.awake()
            && self.world.ending == "playing"
        {
            steps += 1;
            ensure!(steps <= 6, "Action plan exceeded its maximum length.");
            let mut intent = self.actor_mut(id)?.plan.remove(0);
            if let Err(error) = intent
                .resolve_item(self.actor(id)?.last_acquired_item_id)
                .and_then(|()| self.validate_intent(id, &intent))
            {
                self.remember_outcome(id, intent, &ActionResult::fail(error.to_string()), false)?;
                failure = Some(error.to_string());
                self.actor_mut(id)?.clear_plan();
                let room = self.actor(id)?.room_id;
                self.record(
                    "feedback",
                    Some(id),
                    Some(room),
                    self.text(
                        &format!("Your plan cannot continue: {error}"),
                        &format!("Nie możesz kontynuować planu: {error}"),
                    ),
                    Some(vec![id]),
                    causes.clone(),
                    None,
                )?;
                break;
            }
            let result = self.begin(id, intent.clone(), causes.clone())?;
            if !result.ok {
                self.remember_outcome(id, intent, &result, false)?;
                failure = Some(result.message);
                self.actor_mut(id)?.clear_plan();
                break;
            }
            if self.encounter(id).is_some() {
                break;
            }
        }
        if self.actor(id)?.activity.is_none()
            && self.actor(id)?.awake()
            && self.encounter(id).is_none()
        {
            if self.actor(id)?.plan.is_empty() {
                self.actor_mut(id)?.last_acquired_item_id = 0;
            }
            let cause = if failure.is_some() {
                WakeCause::PlanFailed
            } else {
                WakeCause::ActivityEnded
            };
            if failure.is_some() {
                let actor = self.actor_mut(id)?;
                if actor.correction_used {
                    return Ok(());
                }
                actor.correction_used = true;
            } else if !completed {
                return Ok(());
            }
            self.queue_decision_for(
                id,
                failure.map_or_else(
                    || self.text("Your planned activity has ended.", "Twoje zaplanowane zajęcie dobiegło końca."),
                    |error| self.text(
                        &format!("Your plan stopped: {error} Choose a legal action using current facts."),
                        &format!("Twój plan został przerwany: {error} Wybierz legalne działanie na podstawie aktualnych faktów."),
                    ),
                ),
                WakePriority::Routine,
                false,
                cause,
            )?;
        }
        Ok(())
    }
    pub fn start_player_plan(
        &mut self,
        id: u16,
        raw: Value,
        source_id: u64,
        combat: Option<Value>,
    ) -> Result<Value> {
        let interpretation = match Interpretation::parse(raw) {
            Ok(value) => value,
            Err(_) => {
                return Ok(ActionResult::fail(self.text(
                    "The listener returned an invalid action sequence.",
                    "Interpretator zwrócił nieprawidłową sekwencję działań.",
                ))
                .value());
            }
        };
        self.start_interpreted_plan(id, interpretation, source_id, combat)
    }
    pub fn start_interpreted_plan(
        &mut self,
        id: u16,
        interpretation: Interpretation,
        source_id: u64,
        combat: Option<Value>,
    ) -> Result<Value> {
        let context: Option<CombatContext> = match combat {
            Some(value) => match serde_json::from_value(value) {
                Ok(value) => Some(value),
                Err(_) => {
                    return Ok(ActionResult::fail(
                        self.text("Malformed combat context.", "Nieprawidłowy kontekst walki."),
                    )
                    .value());
                }
            },
            None => None,
        };
        if context != self.combat_context(id) {
            return Ok(ActionResult::fail(self.text(
                "That interpretation belongs to an obsolete combat round.",
                "Ta interpretacja dotyczy nieaktualnej rundy walki.",
            ))
            .value());
        }
        if interpretation.check().is_err() {
            return Ok(ActionResult::fail(self.text(
                "The listener returned an invalid action sequence.",
                "Interpretator zwrócił nieprawidłową sekwencję działań.",
            ))
            .value());
        }
        let source = self.store.observed_event(id, source_id)?;
        let actor = self.actor(id)?;
        if !self.is_player(id)
            || !source.as_ref().is_some_and(|event| {
                event.actor_id == Some(id)
                    && event.kind == "speech"
                    && event.room_id == Some(actor.room_id)
                    && event.id >= actor.entered_event_id
            })
        {
            return Ok(ActionResult::fail(self.text(
                "The sequence has no player speech source.",
                "Sekwencja nie ma aktualnego źródła w wypowiedzi gracza.",
            ))
            .value());
        }
        let Some(first) = interpretation.plan.first().cloned() else {
            return Ok(ActionResult::ok(self.text(
                "No physical action was inferred.",
                "Nie wywnioskowano fizycznego działania.",
            ))
            .value());
        };
        if let Err(error) = self.validate_intent(id, &first) {
            return Ok(ActionResult::fail(error.to_string()).value());
        }
        if actor.activity.as_ref().is_some_and(|a| !a.intent.long())
            && !matches!(
                first,
                Intent::Inspect { .. } | Intent::Defend { .. } | Intent::Flee { .. }
            )
        {
            return Ok(ActionResult::fail(self.text(
                "Finish your current action first, or defend/flee to interrupt it.",
                "Dokończ bieżące działanie albo przerwij je obroną lub ucieczką.",
            ))
            .value());
        }
        self.mutate(|s| {
            s.actor_mut(id)?.last_acquired_item_id = 0;
            s.actor_mut(id)?.plan =
                if s.encounter(id).is_some() || matches!(first, Intent::Attack { .. }) {
                    vec![]
                } else {
                    interpretation.plan.into_iter().skip(1).collect()
                };
            let result = s.begin(id, first, vec![source_id])?;
            if s.actor(id)?.activity.is_none() {
                s.run_plan(id, vec![source_id], None, false)?;
            }
            Ok(result)
        })
        .map(ActionResult::value)
    }
    pub fn cancel_player_plan(&mut self, id: u16) -> Result<()> {
        self.mutate(|s| {
            if s.actor(id)?
                .activity
                .as_ref()
                .is_some_and(|a| a.intent.long())
            {
                s.interrupt_activity(id)?;
            }
            let player = s.actor_mut(id)?;
            player.clear_plan();
            Ok(())
        })
    }
    pub fn clear_player_plan(&mut self, id: u16) -> Result<()> {
        self.mutate(|s| {
            s.actor_mut(id)?.clear_plan();
            Ok(())
        })
    }
    pub fn apply_decision(
        &mut self,
        id: u16,
        raw: Value,
        expected_version: u64,
        recalled: &[u64],
    ) -> Result<Value> {
        let decision = match Decision::parse(raw) {
            Ok(value) => value,
            Err(error) => {
                return Ok(ActionResult::fail(self.text(
                    &format!("Invalid NPC decision: {error}"),
                    &format!("Nieprawidłowa decyzja postaci: {error}"),
                ))
                .value());
            }
        };
        self.apply_npc_decision(id, decision, expected_version, recalled)
    }
    pub fn apply_npc_decision(
        &mut self,
        id: u16,
        decision: Decision,
        expected_version: u64,
        recalled: &[u64],
    ) -> Result<Value> {
        let actor = self.actor(id)?.clone();
        if self.is_player(id) || expected_version != actor.decision_version || !actor.awake() {
            return Ok(ActionResult::fail(self.text(
                "That decision belongs to an obsolete situation.",
                "Ta decyzja dotyczy nieaktualnej sytuacji.",
            ))
            .value());
        }
        let decision = match Decision::check(decision) {
            Ok(decision) => decision,
            Err(error) => {
                return Ok(ActionResult::fail(self.text(
                    &format!("Invalid NPC decision: {error}"),
                    &format!("Nieprawidłowa decyzja postaci: {error}"),
                ))
                .value());
            }
        };
        for evidence in decision
            .memories
            .iter()
            .map(|m| &m.evidence_ids)
            .chain(decision.relationships.iter().map(|r| &r.evidence_ids))
        {
            if !evidence.iter().all(|id| recalled.contains(id))
                || !self.store.owns_evidence(id, evidence)?
            {
                return Ok(ActionResult::fail(self.text(
                    "A mental update cited memories outside this character’s experience.",
                    "Aktualizacja umysłu przywołuje wspomnienia spoza doświadczenia tej postaci.",
                ))
                .value());
            }
        }
        if decision
            .relationships
            .iter()
            .any(|r| !actor.known_actors.contains(&r.target_id))
        {
            return Ok(ActionResult::fail(self.text(
                "A relationship update targets an unknown person.",
                "Aktualizacja relacji dotyczy nieznanej osoby.",
            ))
            .value());
        }
        for plot_id in &decision.accept_plots {
            let valid = self.world.plots.iter().find(|p| {
                p.id == *plot_id
                    && p.recipient == id
                    && (p.explicit || p.accepted)
                    && actor.known_actors.contains(&p.victim)
            });
            let Some(plot) = valid else {
                return Ok(ActionResult::fail(self.text(
                    "A commitment must come from a conversation this character actually heard.",
                    "Zobowiązanie musi wynikać z rozmowy, którą ta postać naprawdę słyszała.",
                ))
                .value());
            };
            if self
                .store
                .observed_event(id, plot.event_id)?
                .is_none_or(|e| e.actor_id != Some(plot.proposer))
            {
                return Ok(ActionResult::fail(self.text(
                    "A commitment must come from a conversation this character actually heard.",
                    "Zobowiązanie musi wynikać z rozmowy, którą ta postać naprawdę słyszała.",
                ))
                .value());
            }
        }
        let mut heard = Vec::new();
        for proposal in &decision.accept_heard_proposals {
            let memory = self.store.owned_memory(id, proposal.memory_id)?;
            let valid = recalled.contains(&proposal.memory_id)
                && actor.known_actors.contains(&proposal.victim_id)
                && self.actor(proposal.victim_id).is_ok_and(Actor::alive)
                && memory.as_ref().is_some_and(|m| {
                    m.kind == "heard" && m.source_id.is_some_and(|source| source != id)
                });
            let event = match memory.as_ref().and_then(|m| m.event_id) {
                Some(event) => self.store.observed_event(id, event)?,
                None => None,
            };
            if !valid
                || event.as_ref().is_none_or(|e| {
                    e.kind != "speech" || e.actor_id != memory.as_ref().and_then(|m| m.source_id)
                })
            {
                return Ok(ActionResult::fail(self.text("A proposal must cite an identified speaker’s words supplied from your own hearing.", "Propozycja musi odwoływać się do podanych słów rozpoznanego rozmówcy, które samodzielnie słyszysz.")).value());
            }
            let event = event.context("Missing heard event")?;
            heard.push((
                event.actor_id.context("Missing proposer")?,
                event.id,
                proposal.victim_id,
            ));
        }
        let in_combat = self.encounter(id).is_some();
        let busy = actor
            .activity
            .as_ref()
            .is_some_and(|activity| !activity.intent.long());
        let preserve_activity = actor.activity.is_some()
            && decision
                .plan
                .iter()
                .all(|intent| matches!(intent, Intent::Speak { .. } | Intent::Inspect { .. }));
        if !preserve_activity && actor.activity.as_ref().is_some_and(|a| a.intent.travel()) {
            return Ok(ActionResult::fail(self.text(
                "Finish travelling before choosing another activity.",
                "Dokończ przejście, zanim wybierzesz inne zajęcie.",
            ))
            .value());
        }
        let interruption = decision
            .plan
            .iter()
            .find(|intent| !matches!(intent, Intent::Speak { .. } | Intent::Inspect { .. }));
        if busy
            && !preserve_activity
            && !interruption
                .is_some_and(|intent| matches!(intent, Intent::Defend { .. } | Intent::Flee { .. }))
        {
            return Ok(ActionResult::fail(self.text(
                "Finish your current action first, or defend/flee to interrupt it.",
                "Dokończ bieżące działanie albo przerwij je obroną lub ucieczką.",
            ))
            .value());
        }
        let physical: Vec<_> = decision
            .plan
            .iter()
            .filter(|i| !matches!(i, Intent::Speak { .. }))
            .cloned()
            .collect();
        if in_combat
            && (physical.len() != 1
                || !physical[0].combat_move()
                || decision
                    .plan
                    .iter()
                    .filter(|i| matches!(i, Intent::Speak { .. }))
                    .count()
                    > 1)
        {
            return Ok(ActionResult::fail(self.text("Choose exactly one attack, subdue, defend, or flee, with at most one optional speech.","Wybierz dokładnie jeden atak, obezwładnienie, obronę lub ucieczkę, z najwyżej jedną wypowiedzią.")).value());
        }
        if let Some(first) = decision.plan.first()
            && let Err(error) = self.validate_intent(id, first)
        {
            return Ok(ActionResult::fail(error.to_string()).value());
        }
        if in_combat && let Err(error) = self.validate_intent(id, &physical[0]) {
            return Ok(ActionResult::fail(error.to_string()).value());
        }
        self.mutate(|s| {
            if !preserve_activity {
                s.interrupt_activity(id)?;
            }
            let actor = s.actor_mut(id)?;
            if let Some(summary) = decision.summary {
                actor.summary = summary;
            }
            actor.goals = decision.goals;
            if !preserve_activity {
                actor.last_acquired_item_id = 0;
                actor.activity = None;
                actor.plan = if in_combat {
                    vec![]
                } else {
                    decision.plan.clone()
                };
            }
            for memory in decision.memories {
                s.store.remember(Memory {
                    id: 0,
                    actor_id: id,
                    time: s.world.time,
                    event_id: None,
                    kind: memory.kind,
                    text: memory.text,
                    importance: memory.importance,
                    source_id: Some(id),
                    evidence_ids: memory.evidence_ids,
                    perceived: None,
                })?;
            }
            for update in decision.relationships {
                let relationship = s
                    .actor_mut(id)?
                    .relationships
                    .entry(update.target_id)
                    .or_default();
                relationship.trust =
                    (relationship.trust + f64::from(update.trust)).clamp(-100.0, 100.0);
                relationship.affection =
                    (relationship.affection + f64::from(update.affection)).clamp(-100.0, 100.0);
                relationship.fear = clamp(relationship.fear + f64::from(update.fear));
                relationship.respect =
                    (relationship.respect + f64::from(update.respect)).clamp(-100.0, 100.0);
            }
            let mut accept_plots = decision.accept_plots;
            for (proposer, event_id, victim) in heard {
                let plot_id = if let Some(plot) = s.world.plots.iter().find(|p| {
                    p.proposer == proposer
                        && p.recipient == id
                        && p.victim == victim
                        && p.event_id == event_id
                }) {
                    plot.id
                } else {
                    ensure!(
                        s.world.next_plot_id <= u32::from(u16::MAX),
                        "Plot IDs exhausted"
                    );
                    let plot_id = s.world.next_plot_id as u16;
                    s.world.next_plot_id += 1;
                    s.world.plots.push(Plot {
                        id: plot_id,
                        proposer,
                        recipient: id,
                        victim,
                        event_id,
                        accepted: false,
                        explicit: false,
                        accepted_event_id: None,
                    });
                    plot_id
                };
                if !accept_plots.contains(&plot_id) {
                    accept_plots.push(plot_id);
                }
            }
            for plot_id in accept_plots {
                let plot = s
                    .world
                    .plots
                    .iter()
                    .find(|p| p.id == plot_id)
                    .context("Missing plot.")?
                    .clone();
                if plot.accepted {
                    continue;
                }
                let room = s.actor(id)?.room_id;
                let event = s.record(
                    "private",
                    Some(id),
                    Some(room),
                    s.text(
                        &format!(
                            "You commit to {}’s plot against {}.",
                            s.name(plot.proposer),
                            s.name(plot.victim)
                        ),
                        &format!(
                            "Zobowiązujesz się do spisku osoby {} przeciw osobie {}.",
                            s.name(plot.proposer),
                            s.name(plot.victim)
                        ),
                    ),
                    Some(vec![id]),
                    vec![plot.event_id],
                    None,
                )?;
                let plot = s
                    .world
                    .plots
                    .iter_mut()
                    .find(|p| p.id == plot_id)
                    .context("Missing plot.")?;
                plot.accepted = true;
                plot.accepted_event_id = Some(event.id);
                if !s.actor(id)?.accepted_plots.contains(&plot_id) {
                    s.actor_mut(id)?.accepted_plots.push(plot_id);
                }
            }
            if preserve_activity {
                for intent in decision.plan {
                    let result = s.execute(id, intent, vec![])?;
                    ensure!(result.ok, "{}", result.message);
                }
            } else if in_combat {
                if let Some(speech) = decision
                    .plan
                    .into_iter()
                    .find(|i| matches!(i, Intent::Speak { .. }))
                {
                    s.begin(id, speech, vec![])?;
                }
                s.begin(id, physical[0].clone(), vec![])?;
            } else {
                s.run_plan(id, vec![], None, false)?;
            }
            Ok(ActionResult::ok(s.text(
                "The character’s decision is accepted.",
                "Decyzja postaci została przyjęta.",
            )))
        })
        .map(ActionResult::value)
        .or_else(|error| Ok(ActionResult::fail(error.to_string()).value()))
    }
    pub fn recall(&self, id: u16, query: &str) -> Result<Value> {
        let aliases = self.known_aliases(id)?;
        Ok(json!(
            self.store
                .recall_with_aliases(id, query, [6, 10, 8], &[], &aliases)?
                .iter()
                .map(|m| self.describe_memory(m))
                .collect::<Vec<_>>()
        ))
    }
    fn known_aliases(&self, id: u16) -> Result<Vec<Vec<String>>> {
        let actor = self.actor(id)?;
        Ok(self
            .world
            .actors
            .iter()
            .filter(|a| a.id == id || actor.known_actors.contains(&a.id))
            .filter(|a| !self.is_player(a.id))
            .map(|a| self.content.aliases(a.id))
            .collect())
    }
    pub fn observation(&self, id: u16, reason: &str) -> Result<Value> {
        let actor = self.actor(id)?;
        let described = self.describe_actor(actor);
        let nearby_names = self
            .nearby(id)?
            .iter()
            .filter(|a| a.id != id)
            .map(|a| self.name(a.id))
            .collect::<Vec<_>>()
            .join(" ");
        let query = crate::store::context_query(&[
            actor
                .goals
                .iter()
                .map(|goal| self.authored(goal))
                .collect::<Vec<_>>()
                .join(" "),
            nearby_names,
            reason.to_owned(),
            actor.summary.clone(),
            self.name(actor.room_id),
        ]);
        let room_context = crate::room_context::room_context(
            &self.store,
            actor,
            self.world.time,
            &self.language,
            crate::room_context::ContextBudget::default(),
        )?;
        let memories = self.store.recall_with_aliases(
            id,
            &query,
            [6, 10, 8],
            &room_context.excluded_ranges(),
            &self.known_aliases(id)?,
        )?;
        let mut evidence = room_context.evidence_ids();
        evidence.extend(memories.iter().map(|m| m.id));
        evidence.sort_unstable();
        evidence.dedup();
        let doors = self.door_views(id)?;
        let items=self.visible_items(id)?.iter().map(|item|{
            let mut value=json!({"id":item.id,"name":self.name(item.id),"description":self.item_text(item, "description"),"kind":item.kind,"quantity":item.quantity,"ownerId":item.owner_id,"equipped":item.equipped,"poisonRecognized":item.poisoned_by.is_some() && (item.poisoned_by==Some(id) || actor.skills.medicine>=60.0)});
            if item.owner_id==Some(id){value["key"]=json!(item.key);}value
        }).collect::<Vec<_>>();
        let people=self.nearby(id)?.iter().filter(|a|a.id!=id).map(|person|json!({"id":person.id,"name":self.name(person.id),"role":self.role(person),"condition":self.condition(person),"activity":self.activity_text(person),"equipment":self.visible_items(id).unwrap_or_default().iter().filter(|i|i.owner_id==Some(person.id)).map(|i|json!({"id":i.id,"name":self.name(i.id)})).collect::<Vec<_>>()})).collect::<Vec<_>>();
        let known_people=self.world.actors.iter().filter(|a|actor.known_actors.contains(&a.id)).map(|a|json!({"id":a.id,"name":self.name(a.id),"aliases":if self.is_player(a.id){vec![]}else{self.content.aliases(a.id)},"role":self.role(a)})).collect::<Vec<_>>();
        let known_map=self.world.rooms.iter().filter(|room|actor.known_rooms.contains(&room.id)).map(|room|json!({"id":room.id,"name":self.name(room.id),"connections":self.exits(room.id).iter().map(|door|(door.id,self.destination(door,room.id))).filter(|(_,to)|actor.known_rooms.contains(to)).map(|(door,to)|json!({"doorId":door,"to":to})).collect::<Vec<_>>()})).collect::<Vec<_>>();
        let plots = self
            .world
            .plots
            .iter()
            .filter(|p| p.recipient == id && p.explicit && !p.accepted)
            .rev()
            .take(8)
            .map(|plot| {
                let mut value = json!(plot);
                value["speech"] = json!(
                    self.store
                        .observed_event(id, plot.event_id)
                        .ok()
                        .flatten()
                        .map(|e| e.text)
                );
                value
            })
            .collect::<Vec<_>>();
        let commitments = self
            .world
            .plots
            .iter()
            .filter(|p| p.recipient == id && p.accepted)
            .map(|plot| {
                let mut value = json!(plot);
                value["speech"] = json!(
                    self.store
                        .observed_event(id, plot.event_id)
                        .ok()
                        .flatten()
                        .map(|e| e.text)
                );
                value
            })
            .collect::<Vec<_>>();
        let actions = self.action_choices(id, true)?;
        let recipes = Recipe::ALL.into_iter().map(|recipe| {
            let spec = recipe_spec(recipe);
            let intent = Intent::Work { recipe };
            json!({"recipe":recipe,"roomId":spec.room,"skill":if spec.medicine {"medicine"} else {"craft"},"minimum":spec.minimum,"consumes":spec.ingredient,
                "availableNow":self.validate_intent(id, &intent).and_then(|_| self.check_busy(id, &intent)).is_ok()})
        }).collect::<Vec<_>>();
        let mut combat = self.combat_view(id)?;
        if let Some(combat) = combat.as_object_mut() {
            combat.remove("choices");
        }
        let continuing = actor.activity.as_ref().map(|activity| {
            let seconds = self.activity_remaining(activity);
            json!({"durationRealSeconds":seconds,"needs":self.needs_forecast(actor, seconds, matches!(activity.intent, Intent::Rest { .. }) || actor.flags & UNCONSCIOUS != 0)})
        });
        Ok(json!({
            "context":{"branch":self.world.branch,"version":actor.decision_version,"evidenceIds":evidence,"combat":self.combat_context(id)},
            "input":{
                "language":self.language,"scenarioId":self.world.scenario_id,
                "world":{
                    "time":self.world.time,
                    "self":{"id":id,"name":described["name"],"aliases":if self.is_player(id){vec![]}else{self.content.aliases(id)},"role":described["role"],"biography":described["biography"],"personality":described["personality"],"coins":actor.coins,"condition":self.condition(actor),"goals":described["goals"],"relationships":actor.relationships,"notebook":actor.summary,"feltNeeds":self.felt_needs(actor),"activity":self.activity_text(actor),"commitments":commitments},
                    "room":self.describe_room(actor.room_id)?,"doors":doors,"people":people,"items":items,
                    "knownPeople":known_people,"knownMap":known_map,
                    "offers":self.world.offers.iter().filter(|o|o.to==id && o.status=="open" && !self.offer_expired(o)).collect::<Vec<_>>(),
                    "plots":plots,"memories":memories.iter().map(|m|self.describe_memory(m)).collect::<Vec<_>>(),"roomContext":room_context
                },
                "execution":{
                    "reason":reason,
                    "timeRules":{"calendarSecondsPerRealSecond":CLOCK_RATE,"physiologySecondsPerRealSecond":PHYSIOLOGY_RATE,"activitySecondsAre":"calendar","reactionSchedulingUses":"real seconds"},
                    "speechRule":if self.world.mode=="multiplayer"{"Normal speech stays in this room. Shouts reach adjoining rooms, with partially heard words. Never whisper."}else{"Speech follows normal, whisper and shout perception."},
                    "combat":combat,
                    "people":self.nearby(id)?.iter().filter(|a|a.id!=id).map(|person|json!({"id":person.id,"health":person.health})).collect::<Vec<_>>(),
                    "self":{"health":actor.health,"hunger":actor.hunger,"thirst":actor.thirst,"fatigue":actor.fatigue,"skills":actor.skills,"currentActivity":self.current_activity(actor),"remainingPlan":actor.plan,"lastActionOutcome":actor.last_action_outcome,"lastPlanFailure":actor.last_plan_failure,"recentActionOutcomes":actor.recent_action_outcomes,"continuingActivityForecast":continuing},
                    "recipes":recipes,"actions":actions
                }
            }
        }))
    }

    fn door_views(&self, id: u16) -> Result<Vec<Value>> {
        let room = self.actor(id)?.room_id;
        Ok(self.exits(room).iter().map(|door|json!({"id":door.id,"name":self.name(door.id),"destination":self.name(self.destination(door,room)),"destinationId":self.destination(door,room),"open":door.flags&OPEN!=0,"locked":door.flags&LOCKED!=0})).collect())
    }
    pub fn travel_view(&self, id: u16) -> Result<Value> {
        let actor = self.actor(id)?;
        let escort = self.world.actors.iter().find(|a| {
            a.activity.as_ref().is_some_and(
                |activity| matches!(activity.intent,Intent::Escort{target_id,..} if target_id==id),
            )
        });
        let traveller = escort.unwrap_or(actor);
        let Some(activity) = &traveller.activity else {
            return Ok(Value::Null);
        };
        if !activity.intent.travel() {
            return Ok(Value::Null);
        }
        let door = self
            .door(activity.intent.door_id().context("Travel has no door.")?)
            .context("Travel door missing.")?;
        if !door.rooms.contains(&traveller.room_id) {
            return Ok(Value::Null);
        }
        let remaining = self.activity_remaining(activity);
        let mut value = json!({"origin":self.name(traveller.room_id),"destination":self.name(self.destination(door,traveller.room_id)),"remainingSeconds":remaining});
        if let Some(escort) = escort {
            value["escortedBy"] = json!(self.name(escort.id));
        }
        Ok(value)
    }
    fn activity_remaining(&self, activity: &Activity) -> f64 {
        (activity
            .interaction_ends
            .unwrap_or(self.world.interaction_time)
            - self.world.interaction_time)
            .max((activity.ends - self.world.time) / CLOCK_RATE)
            .max(0.0)
    }
    fn current_activity(&self, actor: &Actor) -> Value {
        actor.activity.as_ref().map_or(Value::Null, |activity| {
            json!({
                "intent":activity.intent,"started":activity.started,"ends":activity.ends,
                "remainingCalendarSeconds":(activity.ends-self.world.time).max(0.0),
                "remainingRealSeconds":self.activity_remaining(activity)
            })
        })
    }
    pub fn combat_view(&self, id: u16) -> Result<Value> {
        let Some(encounter) = self.encounter(id) else {
            return Ok(Value::Null);
        };
        let actor = self.actor(id)?;
        if !actor.awake() || actor.flags & RESTRAINED != 0 {
            return Ok(Value::Null);
        }
        let participant = encounter
            .participants
            .iter()
            .find(|p| p.actor_id == id)
            .context("Missing combat participant.")?;
        let opponents = encounter
            .participants
            .iter()
            .filter(|p| p.actor_id != id)
            .filter(|p| {
                encounter.hostility.contains(&[id, p.actor_id])
                    || encounter.hostility.contains(&[p.actor_id, id])
            })
            .filter_map(|p| self.actor(p.actor_id).ok())
            .filter(|a| a.alive() && a.room_id == encounter.room_id)
            .map(|a| json!({"id":a.id,"name":self.name(a.id),"condition":self.condition(a),"health":a.health}))
            .collect::<Vec<_>>();
        let participants = encounter
            .participants
            .iter()
            .filter(|p| p.actor_id != id)
            .filter_map(|p| self.actor(p.actor_id).ok())
            .filter(|a| a.alive() && a.room_id == encounter.room_id)
            .map(|a| json!({"id":a.id,"name":self.name(a.id),"condition":self.condition(a),"health":a.health}))
            .collect::<Vec<_>>();
        let mut choices = Vec::new();
        for other in self
            .nearby(id)?
            .into_iter()
            .filter(|a| a.id != id && a.alive())
        {
            for style in [AttackStyle::Lethal, AttackStyle::Subdue] {
                if let Some(choice) = self.action_choice(
                    id,
                    Intent::Attack {
                        target_id: other.id,
                        style,
                    },
                    "Combat",
                    "Walka",
                ) {
                    choices.push(choice);
                }
            }
        }
        if let Some(choice) =
            self.action_choice(id, Intent::Defend { seconds: 30 }, "Combat", "Walka")
        {
            choices.push(choice);
        }
        for door in self.exits(actor.room_id) {
            if let Some(choice) =
                self.action_choice(id, Intent::Flee { door_id: door.id }, "Combat", "Walka")
            {
                choices.push(choice);
            }
        }
        Ok(
            json!({"encounterId":encounter.id,"round":encounter.round.max(participant.joins_round),"remainingSeconds":(encounter.deadline-self.world.interaction_time).max(0.0),"choice":participant.choice,"joining":participant.joins_round>encounter.round,"participants":participants,"opponents":opponents,"choices":choices}),
        )
    }
    fn action_command(&self, id: u16, intent: &Intent) -> String {
        let verb = match intent {
            Intent::Move { .. } => "go",
            Intent::AcceptOffer { .. } => "accept",
            Intent::DeclineOffer { .. } => "decline",
            Intent::Work { .. } => "make",
            Intent::Attack {
                style: AttackStyle::Subdue,
                ..
            } => "subdue",
            Intent::Use { mode, .. } => match mode {
                UseMode::Heal => "heal",
                UseMode::Poison => "poison",
                UseMode::PickLock => "pick",
                UseMode::Disguise => "disguise",
                UseMode::Trap => "trap",
                UseMode::CutBonds => "cut-bonds",
            },
            Intent::Speak { .. } => "say",
            _ => intent.kind(),
        };
        let mut command = format!("/{verb}");
        for argument in self.action_arguments(id, intent) {
            if let Some(prefix) = argument["prefix"]
                .as_str()
                .filter(|prefix| !prefix.is_empty())
            {
                command.push(' ');
                command.push_str(prefix);
            }
            if let Some(value) = argument["value"].as_str() {
                command.push(' ');
                command.push_str(value);
            }
        }
        command
    }
    fn action_applicable(&self, id: u16, intent: &Intent) -> bool {
        let item = intent.item_id().and_then(|id| self.item(id));
        let target = intent.target_id().and_then(|id| self.actor(id).ok());
        match intent {
            Intent::Guard { .. }
            | Intent::Use {
                mode: UseMode::Disguise | UseMode::CutBonds,
                ..
            } => false,
            Intent::Steal { .. } => item.is_some_and(|i| {
                i.owner_id
                    .is_some_and(|owner| owner != id && self.actor(owner).is_ok_and(Actor::alive))
            }),
            Intent::Equip { .. } => {
                item.is_some_and(|i| matches!(i.kind, ItemKind::Weapon | ItemKind::Clothes))
            }
            Intent::Eat { .. } => item.is_some_and(|i| i.kind == ItemKind::Food),
            Intent::Drink { .. } => item.is_some_and(|i| i.kind == ItemKind::Water),
            Intent::Attack { .. } => target.is_some_and(|a| a.id != id && a.alive()),
            Intent::Bind { .. } => {
                target.is_some_and(|a| a.flags & UNCONSCIOUS != 0 && a.flags & RESTRAINED == 0)
            }
            Intent::Release { .. } | Intent::Escort { .. } => {
                target.is_some_and(|a| a.flags & RESTRAINED != 0)
            }
            Intent::Defend { .. } => self.encounter(id).is_some(),
            Intent::Work { recipe } => self
                .actor(id)
                .is_ok_and(|a| a.room_id == recipe_spec(*recipe).room),
            Intent::Use {
                mode: UseMode::PickLock,
                target_id,
                ..
            } => self
                .door(*target_id)
                .is_some_and(|door| door.flags & LOCKED != 0),
            _ => true,
        }
    }
    fn action_arguments(&self, id: u16, intent: &Intent) -> Vec<Value> {
        let entity = |entity_id, prefix: &str, value: String| json!({"value":value,"prefix":prefix,"entityId":entity_id});
        let named = |entity_id, prefix| entity(entity_id, prefix, self.name(entity_id));
        let destination = |door_id, prefix| {
            entity(
                door_id,
                prefix,
                self.door(door_id).map_or_else(
                    || door_id.to_string(),
                    |door| {
                        self.name(
                            self.destination(door, self.actor(id).map_or(DUNGEON, |a| a.room_id)),
                        )
                    },
                ),
            )
        };
        match intent {
            Intent::Move { door_id } | Intent::Flee { door_id } => vec![destination(*door_id, "")],
            Intent::Give {
                item_id, target_id, ..
            }
            | Intent::Offer {
                item_id, target_id, ..
            } => vec![named(*item_id, ""), named(*target_id, "to")],
            Intent::Use {
                item_id, target_id, ..
            } => vec![named(*target_id, ""), named(*item_id, "with")],
            Intent::Escort { target_id, door_id } => {
                vec![named(*target_id, ""), destination(*door_id, "to")]
            }
            Intent::Pay { target_id, amount } => vec![
                named(*target_id, ""),
                json!({"value":amount.to_string(),"prefix":"","field":"amount","minimum":1,"maximum":1000}),
            ],
            Intent::AcceptOffer { offer_id } | Intent::DeclineOffer { offer_id } => vec![entity(
                *offer_id,
                "",
                self.world
                    .offers
                    .iter()
                    .find(|offer| offer.id == *offer_id)
                    .map_or_else(|| offer_id.to_string(), |offer| self.name(offer.from)),
            )],
            Intent::Inspect { target_id }
            | Intent::Attack { target_id, .. }
            | Intent::Bind { target_id }
            | Intent::Release { target_id } => vec![named(*target_id, "")],
            Intent::Unlock { door_id }
            | Intent::Lock { door_id }
            | Intent::Open { door_id }
            | Intent::Close { door_id } => vec![named(*door_id, "")],
            Intent::Work { recipe } => vec![json!({"value":recipe.command(),"prefix":""})],
            _ => intent
                .item_id()
                .map_or_else(Vec::new, |item| vec![named(item, "")]),
        }
    }
    fn action_forecast(&self, id: u16, intent: &Intent) -> Value {
        let Ok(actor) = self.actor(id) else {
            return Value::Null;
        };
        let item = intent.item_id().and_then(|id| self.item(id));
        let mut effects = Vec::new();
        let mut hints = Vec::new();
        let (mut calendar, mut real) = self.action_duration(intent);
        let encounter = self.encounter(id).or_else(|| {
            self.world
                .encounters
                .iter()
                .find(|e| e.room_id == actor.room_id)
        });
        let combat_timing = intent.combat_move()
            && (encounter.is_some() || matches!(intent, Intent::Attack { .. }));
        if combat_timing {
            real = encounter.map_or(self.tuning.combat_round_seconds, |encounter| {
                let next_round = encounter
                    .participants
                    .iter()
                    .find(|p| p.actor_id == id)
                    .is_none_or(|p| p.joins_round > encounter.round);
                (encounter.deadline - self.world.interaction_time).max(0.0)
                    + if next_round {
                        self.tuning.combat_round_seconds
                    } else {
                        0.0
                    }
            });
            if matches!(intent, Intent::Flee { .. }) {
                real += self.tuning.travel_seconds;
            }
            calendar = real * CLOCK_RATE;
        }
        match intent {
            Intent::Eat { .. } | Intent::Drink { .. } => {
                if let Some(item) = item {
                    let eating = matches!(intent, Intent::Eat { .. });
                    let before = if eating { actor.hunger } else { actor.thirst };
                    let delta = clamp(before - item.power) - before;
                    effects.push(json!({"stat":if eating {"hunger"} else {"thirst"},"delta":delta,"consumes":1}));
                    hints.push(self.text(
                        &format!(
                            "{} −{} · 1 unit",
                            if eating { "hunger" } else { "thirst" },
                            number_text(-delta)
                        ),
                        &format!(
                            "{} −{} · 1 szt.",
                            if eating { "głód" } else { "pragnienie" },
                            number_text(-delta)
                        ),
                    ));
                }
            }
            Intent::Use {
                target_id,
                mode: UseMode::Heal,
                ..
            } => {
                if let (Some(item), Ok(target)) = (item, self.actor(*target_id)) {
                    let delta = clamp(target.health + item.power) - target.health;
                    effects.push(
                        json!({"stat":"health","targetId":target_id,"delta":delta,"consumes":1}),
                    );
                    effects
                        .push(json!({"clears":["unconscious","poison"],"condition":"if_present"}));
                    hints.push(self.text(
                        &format!(
                            "HP +{} · treats unconsciousness/poison if present · 1 unit",
                            number_text(delta)
                        ),
                        &format!(
                            "HP +{} · leczy nieprzytomność/zatrucie, jeśli występują · 1 szt.",
                            number_text(delta)
                        ),
                    ));
                }
            }
            Intent::Attack { target_id, style } => {
                if let Ok(target) = self.actor(*target_id) {
                    let weapon = self
                        .inventory(id)
                        .iter()
                        .filter(|i| i.kind == ItemKind::Weapon && i.equipped)
                        .map(|i| i.power)
                        .fold(0.0, f64::max);
                    let projected_loss = |skill, defending| {
                        let damage = attack_damage(
                            actor.skills.combat,
                            weapon,
                            actor.flags & HIDDEN != 0,
                            actor.fatigue,
                            skill,
                            defending,
                        );
                        target.health
                            - if *style == AttackStyle::Subdue && target.health - damage <= 20.0 {
                                subdued_health(target.health, damage)
                            } else {
                                clamp(target.health - damage)
                            }
                    };
                    // Opponent skill and chosen move remain private. These bounds use
                    // the skill domain, not this opponent's actual hidden values.
                    let (lo, hi) = (projected_loss(100.0, false), projected_loss(0.0, false));
                    let (def_lo, def_hi) = if target.awake() {
                        (projected_loss(100.0, true), projected_loss(0.0, true))
                    } else {
                        (lo, hi)
                    };
                    effects.push(json!({"stat":"health","targetId":target_id,"loss":{"minimum":lo,"maximum":hi},"ifDefending":{"minimum":def_lo,"maximum":def_hi},"prediction":"conditional"}));
                    hints.push(self.text(
                        &format!(
                            "damage {}–{} HP · if defending {}–{}",
                            number_text(lo),
                            number_text(hi),
                            number_text(def_lo),
                            number_text(def_hi)
                        ),
                        &format!(
                            "obrażenia {}–{} HP · przy obronie {}–{}",
                            number_text(lo),
                            number_text(hi),
                            number_text(def_lo),
                            number_text(def_hi)
                        ),
                    ));
                    if *style == AttackStyle::Subdue {
                        hints.push(self.text(
                            "nonlethal; may knock unconscious",
                            "nieśmiertelne; możliwa nieprzytomność",
                        ));
                    }
                }
            }
            Intent::Steal { .. } => {
                let lo = theft_chance(
                    actor.skills.stealth,
                    actor.flags & HIDDEN != 0,
                    100.0,
                    100.0,
                    0.0,
                );
                let hi = theft_chance(
                    actor.skills.stealth,
                    actor.flags & HIDDEN != 0,
                    0.0,
                    0.0,
                    100.0,
                );
                effects.push(json!({"successChance":{"minimum":lo,"maximum":hi},"prediction":"owner_ability_unknown"}));
                hints.push(self.text(
                    &format!(
                        "success {}–{}% · risk of discovery",
                        number_text(lo * 100.0),
                        number_text(hi * 100.0)
                    ),
                    &format!(
                        "powodzenie {}–{}% · ryzyko wykrycia",
                        number_text(lo * 100.0),
                        number_text(hi * 100.0)
                    ),
                ));
            }
            Intent::Use {
                mode: UseMode::PickLock,
                ..
            } => {
                let lo = lockpick_chance(actor.skills.craft, actor.skills.stealth, true);
                let hi = lockpick_chance(actor.skills.craft, actor.skills.stealth, false);
                effects.push(json!({"successChance":{"minimum":lo,"maximum":hi},"consumes":0}));
                hints.push(self.text(
                    &format!(
                        "success {}–{}% · reusable tool",
                        number_text(lo * 100.0),
                        number_text(hi * 100.0)
                    ),
                    &format!(
                        "powodzenie {}–{}% · narzędzie pozostaje",
                        number_text(lo * 100.0),
                        number_text(hi * 100.0)
                    ),
                ));
            }
            Intent::Equip { .. } => {
                if let Some(item) = item {
                    hints.push(if item.kind == ItemKind::Clothes {
                        self.text("disguise", "przebranie")
                    } else {
                        self.text(
                            &format!("weapon power {}", number_text(item.power)),
                            &format!("moc broni {}", number_text(item.power)),
                        )
                    });
                    effects.push(json!({"equipped":true,"weaponPower":if item.kind==ItemKind::Weapon {Some(item.power)} else {None}}));
                }
            }
            Intent::Defend { .. } => {
                effects.push(json!({"incomingDamageMultiplier":0.5}));
                hints.push(self.text(
                    "incoming damage ×0.5 this round",
                    "obrażenia otrzymane ×0,5 w tej rundzie",
                ));
            }
            Intent::Rest { .. } => hints.push(self.text(
                "fatigue falls; healing depends on needs and interruptions",
                "zmęczenie maleje; leczenie zależy od potrzeb i przerwań",
            )),
            Intent::Wait { .. } => hints.push(self.text("time passes", "upływ czasu")),
            Intent::Hide { .. } => hints.push(self.text(
                "attempt concealment; may be noticed",
                "próba ukrycia; możliwe wykrycie",
            )),
            Intent::Use {
                mode: UseMode::Poison,
                ..
            } => hints.push(self.text(
                "poison food/water · 1 dose",
                "zatrucie jedzenia/wody · 1 dawka",
            )),
            Intent::Use {
                mode: UseMode::Trap,
                ..
            } => hints.push(self.text("set tripline · 1 rope", "założenie pułapki · 1 lina")),
            Intent::Bind { .. } => {
                hints.push(self.text("restrained · 1 rope", "związanie · 1 lina"))
            }
            Intent::Release { .. } => {
                hints.push(self.text("remove restraints", "rozwiązanie więzów"))
            }
            Intent::Work { recipe } => {
                let spec = recipe_spec(*recipe);
                hints.push(self.text(
                    &format!("1 {:?} → 1 {}", spec.ingredient, spec.name),
                    &format!("1 składnik → 1 {}", spec.polish_name),
                ));
            }
            Intent::Pay { amount, .. } => hints.push(self.text(
                &format!("transfer {amount} coins"),
                &format!("przekazanie {amount} monet"),
            )),
            Intent::Move { .. } | Intent::Flee { .. } | Intent::Escort { .. } => {
                hints.push(self.text("change room", "przejście do pomieszczenia"))
            }
            Intent::Open { .. } => hints.push(self.text("open door", "otwarcie drzwi")),
            Intent::Close { .. } => hints.push(self.text("close door", "zamknięcie drzwi")),
            Intent::Unlock { .. } => {
                hints.push(self.text("unlock; key retained", "odblokowanie; klucz pozostaje"))
            }
            Intent::Lock { .. } => {
                hints.push(self.text("lock; key retained", "zamknięcie na klucz; klucz pozostaje"))
            }
            Intent::Inspect { .. } => hints.push(self.text("observe", "oglądanie")),
            Intent::AcceptOffer { .. } => {
                hints.push(self.text("receive offered quantity", "otrzymanie oferowanej ilości"))
            }
            Intent::DeclineOffer { .. } => {
                hints.push(self.text("decline; no transfer", "odrzucenie; bez przekazania"))
            }
            _ => {}
        }
        if let Some(quantity) = intent.quantity() {
            let count = item
                .map(|i| if quantity == 0 { i.quantity } else { quantity })
                .unwrap_or(quantity);
            effects
                .push(json!({"quantity":count,"transfer":!matches!(intent, Intent::Offer { .. })}));
            hints.push(self.text(&format!("{count} unit(s)"), &format!("{count} szt.")));
        }
        if real > 0.0 {
            hints.push(self.text(
                &format!(
                    "{}{}s real / {}s calendar{}",
                    if combat_timing { "up to " } else { "" },
                    number_text(real),
                    number_text(calendar),
                    if combat_timing {
                        "; round may resolve earlier"
                    } else {
                        ""
                    }
                ),
                &format!(
                    "{}{}s rzeczywistych / {}s świata{}",
                    if combat_timing { "do " } else { "" },
                    number_text(real),
                    number_text(calendar),
                    if combat_timing {
                        "; runda może skończyć się wcześniej"
                    } else {
                        ""
                    }
                ),
            ));
        }
        json!({"effects":effects,"durationCalendarSeconds":calendar,"durationRealSeconds":real,"durationKind":if combat_timing {"maximum"} else {"scheduled"},"needsDuringDuration":self.needs_forecast(actor, real, matches!(intent, Intent::Rest { .. }) || actor.flags & UNCONSCIOUS != 0),"hint":hints.join(" · ")})
    }
    fn action_choice(
        &self,
        id: u16,
        intent: Intent,
        group: &str,
        polish_group: &str,
    ) -> Option<Value> {
        self.action_description(id, intent, group, polish_group, false)
    }
    fn action_description(
        &self,
        id: u16,
        intent: Intent,
        group: &str,
        polish_group: &str,
        compact: bool,
    ) -> Option<Value> {
        if !self.action_applicable(id, &intent) {
            return None;
        }
        let readiness = self
            .validate_intent(id, &intent)
            .and_then(|()| self.check_busy(id, &intent));
        if compact {
            let mut forecast = self.action_forecast(id, &intent);
            forecast.as_object_mut()?.remove("hint");
            let mut choice =
                json!({"intent":intent,"available":readiness.is_ok(),"forecast":forecast});
            if let Err(error) = readiness {
                choice["reason"] = json!(error.to_string());
            }
            return Some(choice);
        }
        let door_command = matches!(
            intent,
            Intent::Move { .. }
                | Intent::Open { .. }
                | Intent::Close { .. }
                | Intent::Unlock { .. }
                | Intent::Lock { .. }
        );
        let available = readiness.is_ok();
        let command = self.action_command(id, &intent);
        let label = if self.language == "pl" {
            let mut label = self.action_verb(&intent);
            if let Some(item) = intent.item_id() {
                label.push(' ');
                label.push_str(&self.name(item));
            }
            if let Some(target) = intent.target_id() {
                label.push_str(" → ");
                label.push_str(&self.name(target));
            }
            if let Some(door) = intent.door_id() {
                label.push(' ');
                label.push_str(&self.name(door));
            }
            label
        } else {
            command.trim_start_matches('/').into()
        };
        let mut choice = json!({"label":label,"group":self.text(group,polish_group),
            "intent":intent,"command":command,"available":available,"arguments":self.action_arguments(id, &intent),
            "forecast":self.action_forecast(id, &intent)});
        if intent.quantity().is_some() {
            choice["quantityAvailable"] = json!(
                intent
                    .item_id()
                    .and_then(|id| self.item(id))
                    .map_or(0, |item| item.quantity)
            );
        }
        choice["hint"] = choice["forecast"]["hint"].clone();
        if let Err(error) = &readiness {
            choice["status"] = json!("blocked");
            choice["reason"] = json!(error.to_string());
        }
        if door_command {
            let door = self.door(intent.door_id()?)?;
            let actor = self.actor(id).ok()?;
            let (status, en, pl) = if !available
                && (!actor.awake()
                    || self.world.ending != "playing"
                    || actor.flags & RESTRAINED != 0
                    || self.encounter(id).is_some()
                    || self.check_busy(id, &intent).is_err())
            {
                ("blocked", "blocked", "niedostępne")
            } else if matches!(intent, Intent::Unlock { .. } | Intent::Lock { .. })
                && !self.has_key(id, door)
            {
                (
                    "needs_key",
                    "needs matching key",
                    "wymaga pasującego klucza",
                )
            } else if matches!(intent, Intent::Lock { .. }) && door.flags & OPEN != 0 {
                ("close_first", "close first", "najpierw zamknij")
            } else if matches!(intent, Intent::Unlock { .. }) && available {
                ("ready", "ready", "gotowe")
            } else if door.flags & LOCKED != 0 {
                ("locked", "locked", "zamknięte na klucz")
            } else if door.flags & OPEN != 0 {
                ("open", "open", "otwarte")
            } else {
                ("closed", "closed", "zamknięte")
            };
            choice["status"] = json!(status);
            choice["hint"] = json!(self.text(en, pl));
            if let Err(error) = readiness {
                choice["reason"] = json!(error.to_string());
            }
        }
        choice["subLabel"] = json!(
            [
                choice["reason"].as_str().unwrap_or(""),
                choice["forecast"]["hint"].as_str().unwrap_or("")
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
        );
        Some(choice)
    }
    pub fn actions(&self, id: u16) -> Result<Vec<Value>> {
        self.action_choices(id, false)
    }
    fn action_choices(&self, id: u16, compact: bool) -> Result<Vec<Value>> {
        let actor = self.actor(id)?;
        let mut choices = Vec::new();
        let mut add = |intent: Intent, group: &str, polish: &str| {
            if let Some(choice) = self.action_description(id, intent, group, polish, compact) {
                choices.push(choice);
            }
        };
        for door in self.exits(actor.room_id) {
            add(Intent::Move { door_id: door.id }, "Move", "Ruch");
            add(
                Intent::Inspect { target_id: door.id },
                "Inspect",
                "Oglądanie",
            );
            if door.flags & OPEN == 0 {
                add(Intent::Open { door_id: door.id }, "Doors", "Drzwi");
            } else {
                add(Intent::Close { door_id: door.id }, "Doors", "Drzwi");
            }
            if door.flags & LOCKED != 0 {
                add(Intent::Unlock { door_id: door.id }, "Doors", "Drzwi");
            } else if door.lock.is_some() {
                add(Intent::Lock { door_id: door.id }, "Doors", "Drzwi");
            }
            if self.encounter(id).is_some() {
                add(Intent::Flee { door_id: door.id }, "Combat", "Walka");
            }
        }
        let people = self.nearby(id)?;
        for item in self.visible_items(id)? {
            add(
                Intent::Inspect { target_id: item.id },
                "Inspect",
                "Oglądanie",
            );
            add(Intent::Eat { item_id: item.id }, "Needs", "Potrzeby");
            add(Intent::Drink { item_id: item.id }, "Needs", "Potrzeby");
            if !item.equipped {
                add(Intent::Equip { item_id: item.id }, "Items", "Przedmioty");
            }
            if item.owner_id != Some(id) {
                add(
                    Intent::Take {
                        item_id: item.id,
                        quantity: 1,
                    },
                    "Items",
                    "Przedmioty",
                );
                add(
                    Intent::Steal {
                        item_id: item.id,
                        quantity: 1,
                    },
                    "Items",
                    "Przedmioty",
                );
                continue;
            }
            add(
                Intent::Drop {
                    item_id: item.id,
                    quantity: 1,
                },
                "Items",
                "Przedmioty",
            );
            for target in people.iter().filter(|a| a.id != id && a.alive()) {
                add(
                    Intent::Offer {
                        item_id: item.id,
                        target_id: target.id,
                        quantity: 1,
                    },
                    "Offers",
                    "Oferty",
                );
                add(
                    Intent::Give {
                        item_id: item.id,
                        target_id: target.id,
                        quantity: 1,
                    },
                    "Offers",
                    "Oferty",
                );
            }
            match item.kind {
                ItemKind::Medicine => {
                    for target in &people {
                        add(
                            Intent::Use {
                                item_id: item.id,
                                target_id: target.id,
                                mode: UseMode::Heal,
                            },
                            "Use",
                            "Używanie",
                        );
                    }
                }
                ItemKind::Poison => {
                    for target in self
                        .visible_items(id)?
                        .into_iter()
                        .filter(|i| matches!(i.kind, ItemKind::Food | ItemKind::Water))
                    {
                        add(
                            Intent::Use {
                                item_id: item.id,
                                target_id: target.id,
                                mode: UseMode::Poison,
                            },
                            "Use",
                            "Używanie",
                        );
                    }
                }
                ItemKind::Lockpick => {
                    for door in self.exits(actor.room_id) {
                        add(
                            Intent::Use {
                                item_id: item.id,
                                target_id: door.id,
                                mode: UseMode::PickLock,
                            },
                            "Use",
                            "Używanie",
                        );
                    }
                }
                ItemKind::Rope => {
                    for door in self.exits(actor.room_id) {
                        add(
                            Intent::Use {
                                item_id: item.id,
                                target_id: door.id,
                                mode: UseMode::Trap,
                            },
                            "Use",
                            "Używanie",
                        );
                    }
                }
                _ => {}
            }
        }
        for person in people.into_iter().filter(|a| a.id != id) {
            add(
                Intent::Inspect {
                    target_id: person.id,
                },
                "Inspect",
                "Oglądanie",
            );
            add(
                Intent::Attack {
                    target_id: person.id,
                    style: AttackStyle::Lethal,
                },
                "Combat",
                "Walka",
            );
            add(
                Intent::Attack {
                    target_id: person.id,
                    style: AttackStyle::Subdue,
                },
                "Combat",
                "Walka",
            );
            add(
                Intent::Bind {
                    target_id: person.id,
                },
                "Combat",
                "Walka",
            );
            add(
                Intent::Release {
                    target_id: person.id,
                },
                "Combat",
                "Walka",
            );
            for door in self.exits(actor.room_id) {
                add(
                    Intent::Escort {
                        target_id: person.id,
                        door_id: door.id,
                    },
                    "Combat",
                    "Walka",
                );
            }
            if actor.coins > 0 {
                add(
                    Intent::Pay {
                        target_id: person.id,
                        amount: actor.coins.min(5),
                    },
                    "Offers",
                    "Oferty",
                );
            }
        }
        for offer in self
            .world
            .offers
            .iter()
            .filter(|o| o.to == id && o.status == "open")
        {
            add(
                Intent::AcceptOffer { offer_id: offer.id },
                "Offers",
                "Oferty",
            );
            add(
                Intent::DeclineOffer { offer_id: offer.id },
                "Offers",
                "Oferty",
            );
        }
        add(Intent::Rest { seconds: 600 }, "Needs", "Potrzeby");
        add(Intent::Wait { seconds: 60 }, "Needs", "Potrzeby");
        add(Intent::Hide { seconds: 300 }, "Stealth", "Ukrywanie");
        if self.encounter(id).is_some() {
            add(Intent::Defend { seconds: 60 }, "Combat", "Walka");
        }
        for recipe in Recipe::ALL {
            add(Intent::Work { recipe }, "Work", "Praca");
        }
        if compact {
            return Ok(choices);
        }
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for choice in &choices {
            let key = choice["command"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            *counts.entry(key).or_default() += 1;
        }
        for choice in &mut choices {
            let command = choice["command"].as_str().unwrap_or_default().to_owned();
            let key = command
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            if counts.get(&key).is_some_and(|count| *count > 1) {
                let ids = choice["intent"]
                    .as_object()
                    .into_iter()
                    .flat_map(|object| object.iter())
                    .filter(|(key, _)| key.ends_with("Id"))
                    .filter_map(|(_, value)| value.as_u64())
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join("-");
                choice["command"] = json!(format!("{command} #{ids}"));
            }
        }
        let normalized = |text: &str| {
            text.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .nfc()
                .flat_map(char::to_lowercase)
                .collect::<String>()
        };
        let mut argument_ids: BTreeMap<String, HashSet<u16>> = BTreeMap::new();
        for choice in &choices {
            let verb = choice["command"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let mut prefix = verb.to_owned();
            for argument in choice["arguments"].as_array().into_iter().flatten() {
                let value = argument["value"].as_str().unwrap_or_default();
                let fixed = argument["prefix"].as_str().unwrap_or_default();
                prefix.push_str(&format!(" {fixed} {value}"));
                if let Some(id) = argument["entityId"].as_u64() {
                    argument_ids
                        .entry(normalized(&prefix))
                        .or_default()
                        .insert(id as u16);
                }
            }
        }
        for choice in &mut choices {
            let verb = choice["command"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
            let mut key = verb.clone();
            let mut completed = verb;
            for argument in choice["arguments"].as_array_mut().into_iter().flatten() {
                let mut value = argument["value"].as_str().unwrap_or_default().to_owned();
                let fixed = argument["prefix"].as_str().unwrap_or_default().to_owned();
                key.push_str(&format!(" {fixed} {value}"));
                if argument_ids
                    .get(&normalized(&key))
                    .is_some_and(|ids| ids.len() > 1)
                    && let Some(id) = argument["entityId"].as_u64()
                {
                    value.push_str(&format!(" #{id}"));
                    argument["value"] = json!(value);
                }
                if !fixed.is_empty() {
                    completed.push_str(&format!(" {fixed}"));
                }
                completed.push_str(&format!(" {value}"));
            }
            choice["completionCommand"] = json!(completed);
        }
        Ok(choices)
    }
    /// Physical projection shared by the UI and the listener; no journal queries or minds.
    fn physical_view(&self, id: u16) -> Result<Value> {
        let actor = self.actor(id)?;
        ensure!(
            self.is_player(id),
            "Player view requires an owned character."
        );
        let visible = self.visible_items(id)?;
        let actors=self.nearby(id)?.into_iter().filter(|a|a.id!=id).map(|a|json!({"id":a.id,"name":self.name(a.id),"role":self.role(a),"portraitKey":a.portrait_key,"condition":self.condition(a),"health":a.health,"activity":self.activity_text(a),"equipment":visible.iter().filter(|i|i.owner_id==Some(a.id) && i.equipped).map(|i|self.name(i.id)).collect::<Vec<_>>()})).collect::<Vec<_>>();
        let map=self.world.rooms.iter().filter(|room|actor.known_rooms.contains(&room.id)).map(|room|json!({"id":room.id,"name":self.name(room.id),"connections":self.exits(room.id).iter().map(|door|self.destination(door,room.id)).filter(|room|actor.known_rooms.contains(room)).map(|room|self.name(room)).collect::<Vec<_>>()})).collect::<Vec<_>>();
        Ok(
            json!({"language":self.language,"time":self.world.time,"room":self.describe_room(actor.room_id)?,"actors":actors,"items":visible.iter().filter(|i|i.owner_id!=Some(id)).map(|i|self.visible_item(i)).collect::<Vec<_>>(),"doors":self.door_views(id)?,"self":{"id":id,"name":self.name(id),"role":self.role(actor),"portraitKey":actor.portrait_key,"condition":self.condition(actor),"unconscious":actor.flags&UNCONSCIOUS!=0,"recoverySeconds":actor.unconscious_until.map(|deadline|(deadline-self.world.interaction_time).max(0.0)),"travel":self.travel_view(id)?,"health":actor.health,"hunger":actor.hunger,"thirst":actor.thirst,"fatigue":actor.fatigue,"activity":self.activity_text(actor),"coins":actor.coins,"restrained":actor.flags&RESTRAINED!=0},"combat":self.combat_view(id)?,"inventory":self.inventory(id).into_iter().map(|i|self.visible_item(i)).collect::<Vec<_>>(),"actions":self.actions(id)?,"map":map}),
        )
    }
    pub fn listener_view(&self, id: u16) -> Result<Value> {
        let mut view = self.physical_view(id)?;
        view["self"]
            .as_object_mut()
            .expect("self projection")
            .remove("portraitKey");
        view["actions"]
            .as_array_mut()
            .expect("action projection")
            .retain(|action| action["available"].as_bool() != Some(false));
        Ok(view)
    }
    pub fn player_view(&self, id: u16) -> Result<Value> {
        let mut view = self.physical_view(id)?;
        let actor = self.actor(id)?;
        let events = self
            .store
            .events(Some(id), 80)?
            .into_iter()
            .filter(|event| {
                event.room_id == Some(actor.room_id) && event.id >= actor.entered_event_id
            })
            .collect::<Vec<_>>();
        let listener = self
            .world
            .players
            .iter()
            .find(|p| p.actor_id == id)
            .is_some_and(|p| p.listener);
        let extra = json!({"scenarioId":self.world.scenario_id,"multiplayer":self.world.mode=="multiplayer","roomVisitId":format!("{}:{id}:{}",self.world.branch,actor.entered_event_id),"winner":self.world.winner_id.map(|id|self.name(id)),"branch":self.world.branch,"revision":self.world.revision,"clock":clock_text(self.world.time,&self.language),"paused":if self.world.manual_pause{vec![self.text("Paused","Wstrzymano")]}else{vec![]},"events":events,"journal":self.store.memories(id,80)?.iter().map(|m|self.describe_memory(m)).collect::<Vec<_>>(),"saves":[],"objective":self.content.objective(&self.language),"ending":if self.world.ending=="playing" && !actor.alive(){"death"}else{&self.world.ending},"error":null,"listener":{"enabled":listener,"state":"idle","queued":0,"inputId":null,"text":"","summary":"","plan":[],"error":null,"resultId":0,"elapsedMs":0,"calls":0,"tokens":0,"lastLatencyMs":null}});
        let Value::Object(extra) = extra else {
            unreachable!("UI fields are an object")
        };
        view.as_object_mut()
            .expect("physical projection")
            .extend(extra);
        Ok(view)
    }
    pub fn manager_view(&self, selected: Option<u16>) -> Result<Value> {
        let actors=self.world.actors.iter().filter(|a|self.world.mode=="multiplayer" || a.id!=PLAYER).map(|actor|{let mut value=self.describe_actor(actor);value["room"]=json!(self.name(actor.room_id));value["controller"]=json!(if self.is_player(actor.id){"human"}else{"ai"});value["connected"]=json!(false);value["status"]=json!({"actorId":actor.id,"state":if !actor.alive(){"dead"}else if actor.activity.is_some(){"acting"}else{"idle"},"reason":self.activity_text(actor),"startedAt":null,"elapsedMs":0,"calls":0,"tokens":0,"lastLatencyMs":null,"error":null});value}).collect::<Vec<_>>();
        let selected=selected.and_then(|id|self.actor(id).ok()).map(|actor|->Result<Value>{let inventory=self.inventory(actor.id).iter().map(|item|{let mut value=json!(item);value["name"]=json!(self.name(item.id));value["description"]=json!(self.item_text(item, "description"));value}).collect::<Vec<_>>();Ok(json!({"actor":self.describe_actor(actor),"memories":self.store.memories(actor.id,100)?.iter().map(|m|self.describe_memory(m)).collect::<Vec<_>>(),"inventory":inventory}))}).transpose()?.unwrap_or(Value::Null);
        Ok(
            json!({"language":self.language,"scenarioId":self.world.scenario_id,"branch":self.world.branch,"time":self.world.time,"clock":clock_text(self.world.time,&self.language),"paused":if self.world.manual_pause{vec![self.text("Paused","Wstrzymano")]}else{vec![]},"ending":self.world.ending,"actors":actors,"events":self.store.events(None,50)?,"selected":selected,"saves":self.store.save_summaries()?,"metrics":{"calls":0,"tokens":0,"p50":null,"p95":null,"active":0},"error":null,"model":MODEL,"serviceTier":"default","listener":{"enabled":false,"state":"idle","queued":0,"text":"","summary":"","plan":[],"error":null}}),
        )
    }

    pub fn set_pause(&mut self, paused: bool) -> Result<()> {
        self.mutate(|s| {
            s.world.manual_pause = paused;
            Ok(())
        })
    }
    pub fn save(&self, name: &str) -> Result<()> {
        self.store.save(&self.world, name)
    }
    fn restore_portraits(world: &mut WorldState) {
        let mut used: Vec<_> = world
            .actors
            .iter()
            .filter_map(|actor| actor.portrait_key.clone())
            .collect();
        for actor in &mut world.actors {
            if actor.portrait_key.is_none() {
                let key = crate::portraits::portrait_key(actor.id, &used);
                used.push(key.clone());
                actor.portrait_key = Some(key);
            }
        }
    }
    pub fn load(&mut self, name: &str, language: &str) -> Result<()> {
        ensure!(matches!(language, "en" | "pl"), "Choose English or Polish.");
        let mut candidate = self.store.snapshot(name)?;
        let content = WorldContent::load(&candidate.world.scenario_id)?;
        candidate.world.branch = Uuid::new_v4().to_string();
        candidate.world.language = language.into();
        candidate.world.manual_pause = true;
        Self::restore_portraits(&mut candidate.world);
        Self::restore_item_origins(&mut candidate.world, &content);
        self.store.replace(&candidate)?;
        self.world = candidate.world;
        self.content = content;
        self.language = language.into();
        self.wakes.clear();
        self.deferred_events.clear();
        self.initialize_minds()?;
        Ok(())
    }
    pub fn import_snapshot(&mut self, path: &Path, language: &str) -> Result<()> {
        ensure!(matches!(language, "en" | "pl"), "Choose English or Polish.");
        let mut candidate = Store::parse_snapshot(&std::fs::read_to_string(path)?)?;
        let content = WorldContent::load(&candidate.world.scenario_id)?;
        candidate.world.branch = Uuid::new_v4().to_string();
        candidate.world.manual_pause = true;
        candidate.world.language = language.into();
        Self::restore_portraits(&mut candidate.world);
        Self::restore_item_origins(&mut candidate.world, &content);
        self.store.replace(&candidate)?;
        self.world = candidate.world;
        self.content = content;
        self.language = language.into();
        self.wakes.clear();
        self.deferred_events.clear();
        self.initialize_minds()?;
        Ok(())
    }
    pub fn new_run(&mut self, scenario: &str, language: &str, seed: u32) -> Result<()> {
        ensure!(matches!(language, "en" | "pl"), "Choose English or Polish.");
        let content = WorldContent::load(scenario)?;
        let (mut candidate, memories) =
            Self::create_world(&content, language, seed, "multiplayer")?;
        candidate.manual_pause = true;
        self.store.new_world(&candidate, &memories)?;
        self.world = candidate;
        self.content = content;
        self.language = language.into();
        self.wakes.clear();
        self.deferred_events.clear();
        self.initialize_minds()?;
        Ok(())
    }
}

/// Cosmetic hearing uses a private deterministic stream, never the physical world RNG.
pub fn mask_speech(text: &str, loss: f64, event: u64, source: u16, listener: u16) -> String {
    let mut state =
        event ^ (u64::from(source) << 32) ^ (u64::from(listener) << 16) ^ 0x9e3779b97f4a7c15;
    text.graphemes(true)
        .map(|grapheme| {
            if !grapheme.chars().any(char::is_alphabetic) {
                return grapheme.to_owned();
            }
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let sample =
                (state.wrapping_mul(0x2545f4914f6cdd1d) >> 11) as f64 / ((1u64 << 53) as f64);
            if sample < loss {
                "-".into()
            } else {
                grapheme.into()
            }
        })
        .collect()
}
