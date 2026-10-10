//! Authored world packages. Player speech and model memories never pass through this renderer.
use anyhow::{Context, Result, anyhow, bail, ensure};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

pub const SCENARIOS: &[(&str, &str, &str)] = &[
    ("castle", "Below the Crown", "Pod koroną"),
    ("siege", "The Last Ration", "Ostatnia racja"),
    ("succession", "The False Oath", "Fałszywa przysięga"),
];

/// Builtins plus successfully validated installed packages, for host-side menus.
pub fn available_scenarios() -> Vec<(String, String, String)> {
    let mut entries: Vec<_> = SCENARIOS
        .iter()
        .map(|(id, en, pl)| (id.to_string(), en.to_string(), pl.to_string()))
        .collect();
    if let Some(directory) = installed_directory()
        && let Ok(children) = std::fs::read_dir(directory)
    {
        for child in children.flatten() {
            if !child.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Ok(content) = WorldContent::from_directory(&child.path()) else {
                continue;
            };
            if entries.iter().any(|(id, _, _)| *id == content.id)
                || child.file_name().to_str() != Some(content.id.as_str())
            {
                continue;
            }
            entries.push((
                content.id.clone(),
                content.texts["en"]["title"].as_str().unwrap().to_string(),
                content.texts["pl"]["title"].as_str().unwrap().to_string(),
            ));
        }
    }
    entries[SCENARIOS.len()..].sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

fn installed_directory() -> Option<PathBuf> {
    std::env::var_os("BELOW_THE_CROWN_SCENARIOS_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|path| path.join("below-the-crown/scenarios")))
}

#[derive(Clone, Debug)]
pub struct WorldContent {
    pub id: String,
    pub scenario: Value,
    texts: BTreeMap<String, Value>,
    prompts: BTreeMap<String, String>,
    authored_paths: BTreeMap<String, String>,
    reserved: BTreeMap<String, u16>,
}

fn normalized(source: &str) -> String {
    source.trim().nfc().flat_map(char::to_lowercase).collect()
}

fn reference() -> &'static Regex {
    static REFERENCE: OnceLock<Regex> = OnceLock::new();
    REFERENCE.get_or_init(|| Regex::new(r"\{actor:(\d+):(nom|gen|dat|acc|inst|loc|voc)\}").unwrap())
}

fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |value, field| match value {
        Value::Array(entries) => field.parse::<usize>().ok().and_then(|i| entries.get(i)),
        _ => value.get(field),
    })
}

fn render_string(source: &str, texts: &Value) -> Result<String> {
    let mut problem = None;
    let rendered = reference()
        .replace_all(source, |capture: &regex::Captures<'_>| {
            let actor = &capture[1];
            let form = &capture[2];
            match texts
                .get("actors")
                .and_then(|actors| actors.get(actor))
                .and_then(|entry| entry.get("forms"))
                .and_then(|forms| forms.get(form))
                .and_then(Value::as_str)
            {
                Some(name) => name.to_owned(),
                None => {
                    problem = Some(format!("Unknown actor reference {actor}:{form}"));
                    String::new()
                }
            }
        })
        .into_owned();
    if let Some(problem) = problem {
        bail!(problem);
    }
    ensure!(
        !rendered.contains("{actor:"),
        "Malformed actor reference in {source}"
    );
    Ok(rendered)
}

fn render_value(source: &Value, texts: &Value) -> Result<Value> {
    Ok(match source {
        Value::String(text) => Value::String(render_string(text, texts)?),
        Value::Array(entries) => Value::Array(
            entries
                .iter()
                .map(|entry| render_value(entry, texts))
                .collect::<Result<_>>()?,
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| Ok((key.clone(), render_value(value, texts)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

fn remember(value: &Value, path: &str, paths: &mut BTreeMap<String, String>) {
    match value {
        Value::String(text) => {
            paths.insert(text.clone(), path.to_owned());
        }
        Value::Array(entries) => {
            for (index, entry) in entries.iter().enumerate() {
                remember(entry, &format!("{path}.{index}"), paths);
            }
        }
        Value::Object(fields) => {
            for (field, entry) in fields {
                if field != "forms" && field != "aliases" {
                    remember(
                        entry,
                        &if path.is_empty() {
                            field.clone()
                        } else {
                            format!("{path}.{field}")
                        },
                        paths,
                    );
                }
            }
        }
        _ => {}
    }
}

fn ids(blueprint: &Value, kind: &str) -> Result<BTreeSet<u16>> {
    let entries = blueprint[kind]
        .as_array()
        .ok_or_else(|| anyhow!("Missing world {kind}"))?;
    let mut ids = BTreeSet::new();
    for entry in entries {
        let id = entry["id"]
            .as_u64()
            .filter(|id| *id > 0 && *id <= u16::MAX as u64)
            .ok_or_else(|| anyhow!("Invalid {kind} ID"))? as u16;
        ensure!(ids.insert(id), "Duplicate {kind} ID {id}");
    }
    Ok(ids)
}

fn validate_blueprint(value: &Value) -> Result<()> {
    ensure!(
        value["time"]
            .as_f64()
            .is_some_and(|time| time.is_finite() && time >= 0.0),
        "Invalid starting calendar"
    );
    let rooms = ids(value, "rooms")?;
    let actors = ids(value, "actors")?;
    let doors = ids(value, "doors")?;
    let items = ids(value, "items")?;
    let mut all = BTreeSet::new();
    for group in [&rooms, &actors, &doors, &items] {
        for id in group {
            ensure!(all.insert(*id), "Entity ID {id} is reused across types");
        }
    }
    ensure!(
        rooms.contains(&1)
            && actors.contains(&101)
            && actors.contains(&102)
            && actors.contains(&103),
        "World needs dungeon 1, prisoner 101, guard 102 and king 103"
    );
    for door in value["doors"].as_array().unwrap() {
        ensure!(
            door["flags"].as_u64().is_some_and(|flags| flags <= 7),
            "Invalid door flags"
        );
        let ends = door["rooms"]
            .as_array()
            .ok_or_else(|| anyhow!("Door needs two rooms"))?;
        ensure!(
            ends.len() == 2 && ends[0] != ends[1],
            "Door needs two distinct rooms"
        );
        for end in ends {
            ensure!(
                end.as_u64()
                    .is_some_and(|id| id <= u16::MAX as u64 && rooms.contains(&(id as u16))),
                "Door references unknown room"
            );
        }
    }
    for actor in value["actors"].as_array().unwrap() {
        ensure!(
            actor["coins"]
                .as_u64()
                .is_some_and(|coins| coins <= u16::MAX as u64),
            "Invalid actor coins"
        );
        for skill in ["combat", "stealth", "craft", "medicine"] {
            ensure!(
                actor["skills"][skill]
                    .as_f64()
                    .is_some_and(|n| n.is_finite() && (0.0..=100.0).contains(&n)),
                "Invalid actor skill {skill}"
            );
        }
        ensure!(
            actor["roomId"]
                .as_u64()
                .is_some_and(|id| id <= u16::MAX as u64 && rooms.contains(&(id as u16))),
            "Actor references unknown room"
        );
        for field in ["knownRooms", "knownActors"] {
            let permitted = if field == "knownRooms" {
                &rooms
            } else {
                &actors
            };
            for id in actor[field]
                .as_array()
                .ok_or_else(|| anyhow!("Actor needs {field}"))?
            {
                ensure!(id.as_u64().is_some_and(|id| id <= u16::MAX as u64 && permitted.contains(&(id as u16))), "Invalid actor {field} reference");
            }
        }
        for field in ["hunger", "thirst", "fatigue"] {
            ensure!(
                actor[field]
                    .as_f64()
                    .is_some_and(|n| n.is_finite() && (0.0..=100.0).contains(&n)),
                "Invalid actor {field}"
            );
        }
        if let Some(relations) = actor["relationships"].as_object() {
            for (target, relation) in relations {
                ensure!(
                    target.parse::<u16>().is_ok_and(|id| actors.contains(&id)),
                    "Unknown relationship target"
                );
                let entries = relation
                    .as_array()
                    .ok_or_else(|| anyhow!("Relationship must contain four values"))?;
                ensure!(
                    entries.len() == 4
                        && entries.iter().all(|v| v
                            .as_f64()
                            .is_some_and(|n| n.is_finite() && (-100.0..=100.0).contains(&n))),
                    "Invalid relationship values"
                );
            }
        }
    }
    for item in value["items"].as_array().unwrap() {
        ensure!(
            item["kind"].as_str().is_some_and(|kind| matches!(
                kind,
                "key"
                    | "food"
                    | "water"
                    | "weapon"
                    | "ingredient"
                    | "clothes"
                    | "metal"
                    | "herb"
                    | "medicine"
                    | "poison"
                    | "rope"
                    | "document"
                    | "lockpick"
            )),
            "Unknown item kind"
        );
        ensure!(
            item["equipped"].is_boolean() && item["visible"].is_boolean(),
            "Invalid item visibility/equipment flags"
        );
        ensure!(
            item["power"]
                .as_f64()
                .is_some_and(|power| power.is_finite() && power >= 0.0),
            "Invalid item power"
        );
        let owner = item["ownerId"].as_u64();
        let room = item["roomId"].as_u64();
        ensure!(
            owner.is_some() != room.is_some(),
            "Item needs exactly one owner or room"
        );
        ensure!(
            owner.is_none_or(|id| id <= u16::MAX as u64 && actors.contains(&(id as u16))),
            "Unknown item owner"
        );
        ensure!(
            room.is_none_or(|id| id <= u16::MAX as u64 && rooms.contains(&(id as u16))),
            "Unknown item room"
        );
        ensure!(
            item["quantity"]
                .as_u64()
                .is_some_and(|n| n > 0 && n <= u16::MAX as u64),
            "Invalid item quantity"
        );
    }
    let prisoner = value["actors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|actor| actor["id"] == 101)
        .unwrap();
    let guard = value["actors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|actor| actor["id"] == 102)
        .unwrap();
    let king = value["actors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|actor| actor["id"] == 103)
        .unwrap();
    ensure!(
        prisoner["roomId"] == 1
            && guard["roomId"] == 1
            && guard["role"] == "Dungeon guard"
            && king["role"] == "King",
        "Invalid prisoner, guard or king template"
    );
    ensure!(
        value["actors"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|actor| actor["roomId"] == 1)
            .count()
            == 2,
        "Dungeon must begin with one prisoner and one guard"
    );
    Ok(())
}

impl WorldContent {
    pub fn load(id: &str) -> Result<Self> {
        let embedded = match id {
            "castle" => Some((
                include_str!("../content/worlds/castle/world.json"),
                include_str!("../content/worlds/castle/locales/en.json"),
                include_str!("../content/worlds/castle/locales/pl.json"),
                include_str!("../content/worlds/castle/prompts/npc.en.md"),
                include_str!("../content/worlds/castle/prompts/npc.pl.md"),
                include_str!("../content/worlds/castle/prompts/listener.en.md"),
                include_str!("../content/worlds/castle/prompts/listener.pl.md"),
                include_str!("../content/worlds/castle/legacy-text.json"),
            )),
            "siege" => Some((
                include_str!("../content/worlds/siege/world.json"),
                include_str!("../content/worlds/siege/locales/en.json"),
                include_str!("../content/worlds/siege/locales/pl.json"),
                include_str!("../content/worlds/siege/prompts/npc.en.md"),
                include_str!("../content/worlds/siege/prompts/npc.pl.md"),
                include_str!("../content/worlds/siege/prompts/listener.en.md"),
                include_str!("../content/worlds/siege/prompts/listener.pl.md"),
                "{}",
            )),
            "succession" => Some((
                include_str!("../content/worlds/succession/world.json"),
                include_str!("../content/worlds/succession/locales/en.json"),
                include_str!("../content/worlds/succession/locales/pl.json"),
                include_str!("../content/worlds/succession/prompts/npc.en.md"),
                include_str!("../content/worlds/succession/prompts/npc.pl.md"),
                include_str!("../content/worlds/succession/prompts/listener.en.md"),
                include_str!("../content/worlds/succession/prompts/listener.pl.md"),
                "{}",
            )),
            _ => None,
        };
        if let Some((world, en, pl, npc_en, npc_pl, listener_en, listener_pl, legacy)) = embedded {
            return Self::from_parts(
                id,
                world,
                en,
                pl,
                [npc_en, npc_pl, listener_en, listener_pl],
                legacy,
            );
        }
        let path = Path::new(id);
        if (path.is_absolute() || path.components().count() > 1) && path.is_dir() {
            return Self::from_directory(path);
        }
        ensure!(
            !id.is_empty()
                && id.len() <= 40
                && id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "Invalid scenario ID"
        );
        let directory =
            installed_directory().ok_or_else(|| anyhow!("No scenario installation directory"))?;
        let content = Self::from_directory(&directory.join(id))
            .with_context(|| format!("Unknown or invalid scenario {id}"))?;
        ensure!(content.id == id, "Installed scenario directory ID mismatch");
        Ok(content)
    }

    pub fn from_directory(directory: &Path) -> Result<Self> {
        let read = |name: &str| {
            std::fs::read_to_string(directory.join(name))
                .with_context(|| format!("Reading scenario {name}"))
        };
        let world = read("world.json")?;
        let value: Value = serde_json::from_str(&world)?;
        let id = value["scenarioId"]
            .as_str()
            .ok_or_else(|| anyhow!("Installed world needs scenarioId"))?;
        ensure!(
            !id.is_empty()
                && id.len() <= 40
                && id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "Invalid installed scenario ID"
        );
        ensure!(
            !SCENARIOS.iter().any(|(builtin, _, _)| *builtin == id),
            "Installed scenario cannot replace builtin {id}"
        );
        let prompts = [
            read("prompts/npc.en.md")?,
            read("prompts/npc.pl.md")?,
            read("prompts/listener.en.md")?,
            read("prompts/listener.pl.md")?,
        ];
        let legacy = match std::fs::read_to_string(directory.join("legacy-text.json")) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_owned(),
            Err(error) => return Err(error.into()),
        };
        Self::from_parts(
            id,
            &world,
            &read("locales/en.json")?,
            &read("locales/pl.json")?,
            [&prompts[0], &prompts[1], &prompts[2], &prompts[3]],
            &legacy,
        )
    }

    fn from_parts(
        id: &str,
        world: &str,
        en: &str,
        pl: &str,
        prompts: [&str; 4],
        legacy: &str,
    ) -> Result<Self> {
        let scenario: Value = serde_json::from_str(world)?;
        ensure!(
            scenario["scenarioId"]
                .as_str()
                .is_none_or(|stored| stored == id),
            "Scenario ID mismatch"
        );
        validate_blueprint(&scenario)?;
        let raw = BTreeMap::from([
            ("en".to_owned(), serde_json::from_str::<Value>(en)?),
            ("pl".to_owned(), serde_json::from_str::<Value>(pl)?),
        ]);
        let mut reserved = BTreeMap::new();
        for (language, locale) in &raw {
            for field in ["title", "introduction"] {
                ensure!(
                    locale[field].as_str().is_some_and(|s| !s.trim().is_empty()),
                    "Missing {language} {field}"
                );
            }
            for mode in ["solo", "multiplayer"] {
                ensure!(
                    locale["objectives"][mode]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty()),
                    "Missing {language} objective"
                );
            }
            for kind in ["rooms", "doors", "actors", "items"] {
                let expected = ids(&scenario, kind)?;
                let entries = locale[kind]
                    .as_object()
                    .ok_or_else(|| anyhow!("Missing {language} {kind}"))?;
                ensure!(
                    entries.len() == expected.len()
                        && expected
                            .iter()
                            .all(|id| entries.contains_key(&id.to_string())),
                    "Incomplete {language} {kind}"
                );
                for (entity, entry) in entries {
                    let fields = entry
                        .as_object()
                        .ok_or_else(|| anyhow!("Invalid {language} {kind}.{entity}"))?;
                    ensure!(
                        fields.keys().all(|field| match kind {
                            "actors" => matches!(
                                field.as_str(),
                                "name"
                                    | "role"
                                    | "biography"
                                    | "personality"
                                    | "goals"
                                    | "knowledge"
                                    | "forms"
                                    | "aliases"
                            ),
                            "doors" => field == "name",
                            _ => matches!(field.as_str(), "name" | "description"),
                        }),
                        "Locale cannot define physical fields: {language} {kind}.{entity}"
                    );
                    for field in if kind == "doors" {
                        &["name"][..]
                    } else if kind == "actors" {
                        &["name", "role", "biography", "personality"][..]
                    } else {
                        &["name", "description"][..]
                    } {
                        ensure!(
                            entry[field].as_str().is_some_and(|s| !s.trim().is_empty()),
                            "Missing {language} {kind}.{entity}.{field}"
                        );
                    }
                    if kind != "actors" {
                        continue;
                    }
                    ensure!(
                        entry["forms"]["nom"] == entry["name"],
                        "Nominative must match {language} actor {entity}"
                    );
                    for form in ["nom", "gen", "dat", "acc", "inst", "loc", "voc"] {
                        ensure!(
                            entry["forms"][form]
                                .as_str()
                                .is_some_and(|s| !s.trim().is_empty()),
                            "Missing {language} {entity}:{form}"
                        );
                    }
                    for field in ["goals", "knowledge", "aliases"] {
                        let values = entry[field]
                            .as_array()
                            .ok_or_else(|| anyhow!("Missing {language} actor {field}"))?;
                        ensure!(
                            values
                                .iter()
                                .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())),
                            "Invalid {language} actor {field}"
                        );
                        if field == "goals" {
                            ensure!(
                                !values.is_empty() && values.len() <= 5,
                                "Actor needs 1–5 goals"
                            );
                        }
                    }
                    if entity == "101" {
                        continue;
                    }
                    let actor_id: u16 = entity.parse()?;
                    let names = std::iter::once(&entry["name"])
                        .chain(entry["forms"].as_object().unwrap().values())
                        .chain(entry["aliases"].as_array().unwrap().iter());
                    for name in names {
                        let key = normalized(name.as_str().unwrap());
                        ensure!(
                            reserved.get(&key).is_none_or(|owner| *owner == actor_id),
                            "Character names collide: {name}"
                        );
                        reserved.insert(key, actor_id);
                    }
                }
            }
        }
        let texts = raw
            .iter()
            .map(|(lang, value)| Ok((lang.clone(), render_value(value, value)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut prompt_map = BTreeMap::new();
        for ((kind, lang), source) in [
            ("npc", "en"),
            ("npc", "pl"),
            ("listener", "en"),
            ("listener", "pl"),
        ]
        .into_iter()
        .zip(prompts)
        {
            ensure!(!source.trim().is_empty(), "Missing {kind} {lang} prompt");
            prompt_map.insert(
                format!("{kind}.{lang}"),
                render_string(source.trim(), &raw[lang])?,
            );
        }
        let mut authored_paths = BTreeMap::new();
        for value in texts.values() {
            remember(value, "", &mut authored_paths);
        }
        for kind in ["rooms", "doors", "actors", "items"] {
            for entity in scenario[kind].as_array().unwrap() {
                let entity_id = entity["id"].as_u64().unwrap();
                for field in if kind == "actors" {
                    &[
                        "name",
                        "role",
                        "biography",
                        "personality",
                        "goals",
                        "knowledge",
                    ][..]
                } else if kind == "doors" {
                    &["name"][..]
                } else {
                    &["name", "description"][..]
                } {
                    remember(
                        &entity[field],
                        &format!("{kind}.{entity_id}.{field}"),
                        &mut authored_paths,
                    );
                }
                if kind == "actors"
                    && entity_id != 101
                    && let Some(name) = entity["name"].as_str()
                {
                    let key = normalized(name);
                    ensure!(
                        reserved
                            .get(&key)
                            .is_none_or(|owner| *owner == entity_id as u16),
                        "Canonical name collision"
                    );
                    reserved.insert(key, entity_id as u16);
                }
            }
        }
        remember(
            &scenario["introduction"],
            "introduction",
            &mut authored_paths,
        );
        let legacy: BTreeMap<String, String> = serde_json::from_str(legacy)?;
        for (source, path) in legacy {
            ensure!(
                texts
                    .values()
                    .all(|text| at(text, &path).is_some_and(Value::is_string)),
                "Invalid legacy text path {path}"
            );
            authored_paths.insert(source, path);
        }
        Ok(Self {
            id: id.to_owned(),
            scenario,
            texts,
            prompts: prompt_map,
            authored_paths,
            reserved,
        })
    }

    pub fn canonical_blueprint(&self) -> &Value {
        &self.scenario
    }

    pub fn localized_blueprint(&self, language: &str) -> Result<Value> {
        crate::localization::validate_language(language)?;
        let text = &self.texts[language];
        let mut blueprint = self.scenario.clone();
        blueprint["scenarioId"] = self.id.clone().into();
        blueprint["title"] = text["title"].clone();
        blueprint["introduction"] = text["introduction"].clone();
        for kind in ["rooms", "doors", "actors", "items"] {
            for entity in blueprint[kind].as_array_mut().unwrap() {
                let id = entity["id"].as_u64().unwrap().to_string();
                for (field, value) in text[kind][&id].as_object().unwrap() {
                    // The engine's role checks use permanent canonical English roles.
                    if field == "role" || field == "forms" || field == "aliases" {
                        continue;
                    }
                    entity[field] = value.clone();
                }
            }
        }
        Ok(blueprint)
    }

    pub fn name(&self, id: u16, language: &str) -> String {
        self.entity_text("actors", id, "name", language)
            .unwrap_or_else(|| format!("#{id}"))
    }

    pub fn entity_text(&self, kind: &str, id: u16, field: &str, language: &str) -> Option<String> {
        self.texts
            .get(language)?
            .get(kind)?
            .get(id.to_string())?
            .get(field)?
            .as_str()
            .map(str::to_owned)
    }

    pub fn authored(&self, source: &str, language: &str) -> String {
        self.authored_paths
            .get(source)
            .and_then(|path| self.texts.get(language).and_then(|texts| at(texts, path)))
            .and_then(Value::as_str)
            .unwrap_or(source)
            .to_owned()
    }
    /// Saved custom fields remain literal; only values authored for this exact field localize.
    pub fn actor_text(&self, id: u16, field: &str, source: &str, language: &str) -> String {
        let path = format!("actors.{id}.{field}");
        if self.authored_paths.get(source) == Some(&path)
            || self
                .texts
                .values()
                .any(|text| at(text, &path).and_then(Value::as_str) == Some(source))
            || self.scenario["actors"].as_array().is_some_and(|actors| {
                actors
                    .iter()
                    .any(|actor| actor["id"] == id && actor[field].as_str() == Some(source))
            })
        {
            self.entity_text("actors", id, field, language)
                .unwrap_or_else(|| source.to_owned())
        } else {
            source.to_owned()
        }
    }

    pub fn instructions(&self, kind: &str, language: &str) -> String {
        self.prompts
            .get(&format!("{kind}.{language}"))
            .cloned()
            .unwrap_or_default()
    }

    pub fn reserved_name(&self, name: &str) -> bool {
        self.reserved.contains_key(&normalized(name))
    }

    pub fn aliases(&self, id: u16) -> Vec<String> {
        let mut result = BTreeSet::new();
        for text in self.texts.values() {
            let entry = &text["actors"][id.to_string()];
            if let Some(name) = entry["name"].as_str() {
                result.insert(name.to_owned());
            }
            if let Some(forms) = entry["forms"].as_object() {
                result.extend(forms.values().filter_map(Value::as_str).map(str::to_owned));
            }
            if let Some(aliases) = entry["aliases"].as_array() {
                result.extend(aliases.iter().filter_map(Value::as_str).map(str::to_owned));
            }
        }
        result.into_iter().collect()
    }

    pub fn objective(&self, language: &str) -> String {
        self.texts
            .get(language)
            .and_then(|text| text["objectives"]["multiplayer"].as_str())
            .unwrap_or_default()
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_package_has_complete_locales_and_private_templates() {
        for (id, _, _) in SCENARIOS {
            let content = WorldContent::load(id).unwrap();
            for language in ["en", "pl"] {
                let world = content.localized_blueprint(language).unwrap();
                assert_eq!(world["actors"].as_array().unwrap().len(), 11);
                assert_eq!(world["rooms"].as_array().unwrap().len(), 12);
                assert_eq!(world["doors"].as_array().unwrap().len(), 13);
                assert_eq!(world["actors"][1]["role"], "Dungeon guard");
                assert!(!world.to_string().contains("{actor:"));
                assert!(!content.instructions("npc", language).contains("{actor:"));
                assert!(content.instructions("npc", language).len() > 100);
                assert!(content.objective(language).len() > 10);
                let name = content.name(104, language);
                assert!(content.reserved_name(&name.to_uppercase()));
                assert!(
                    content
                        .entity_text("actors", 1028, "name", language)
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn authored_translation_never_replaces_names_inside_free_text() {
        let content = WorldContent::load("castle").unwrap();
        assert_eq!(
            content.authored("Hello Edric, I am a king.", "pl"),
            "Hello Edric, I am a king."
        );
        assert_eq!(content.authored("Dungeon", "pl"), "Loch");
        assert!(content.localized_blueprint("de").is_err());
    }

    #[test]
    fn builtin_ids_always_select_embedded_content() {
        // A child process owns its cwd; parallel tests keep their own directory.
        if std::env::var_os("BTC_CONTENT_TEST_BUILTIN_CHILD").is_some() {
            for (id, _, _) in SCENARIOS {
                assert_eq!(WorldContent::load(id).unwrap().id, *id);
            }
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        for (id, _, _) in SCENARIOS {
            std::fs::create_dir(scratch.path().join(id)).unwrap();
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "content::tests::builtin_ids_always_select_embedded_content",
            ])
            .env("BTC_CONTENT_TEST_BUILTIN_CHILD", "1")
            .current_dir(scratch.path())
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
    }

    #[test]
    fn invalid_package_and_reference_are_rejected() {
        let mut world: Value =
            serde_json::from_str(include_str!("../content/worlds/castle/world.json")).unwrap();
        world["items"][0]["ownerId"] = 60000.into();
        assert!(validate_blueprint(&world).is_err());
        assert!(render_string("{actor:999:nom}", &serde_json::json!({"actors":{}})).is_err());
        assert!(render_string("{actor:102:bad}", &serde_json::json!({"actors":{}})).is_err());
        assert!(WorldContent::load("../untrusted").is_err());
    }

    #[test]
    fn locales_cannot_change_custody_quantity_or_movement() {
        let original: Value =
            serde_json::from_str(include_str!("../content/worlds/castle/locales/pl.json")).unwrap();
        for (kind, id, field, value) in [
            ("actors", "102", "roomId", serde_json::json!(12)),
            ("actors", "102", "coins", serde_json::json!(60000)),
            ("items", "1001", "ownerId", serde_json::json!(101)),
            ("items", "1001", "quantity", serde_json::json!(60000)),
            ("doors", "201", "flags", serde_json::json!(0)),
        ] {
            let mut locale = original.clone();
            assert!(
                locale[kind][id].is_object(),
                "Missing test fixture {kind}.{id}"
            );
            locale[kind][id][field] = value;
            assert!(
                WorldContent::from_parts(
                    "castle",
                    include_str!("../content/worlds/castle/world.json"),
                    include_str!("../content/worlds/castle/locales/en.json"),
                    &locale.to_string(),
                    [
                        "NPC contract",
                        "Zasady postaci",
                        "Listener contract",
                        "Zasady nasłuchu"
                    ],
                    "{}",
                )
                .is_err(),
                "{kind}.{id}.{field} entered authoritative state through localization"
            );
        }
    }

    #[test]
    fn scenarios_expose_different_physical_opportunities_without_shared_secrets() {
        let siege = WorldContent::load("siege").unwrap();
        let succession = WorldContent::load("succession").unwrap();
        let item = |world: &Value, id: u16| {
            world["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(item(&siege.scenario, 1011)["ownerId"], 103);
        assert_eq!(item(&siege.scenario, 1011)["kind"], "ingredient");
        assert_eq!(item(&siege.scenario, 1021)["roomId"], 5);
        assert_eq!(item(&succession.scenario, 1021)["roomId"], 6);
        assert_eq!(item(&succession.scenario, 1024)["kind"], "lockpick");
        let king = succession.scenario["actors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|actor| actor["id"] == 103)
            .unwrap();
        assert_eq!(king["roomId"], 10);
        for world in [&siege, &succession] {
            for language in ["en", "pl"] {
                let blueprint = world.localized_blueprint(language).unwrap();
                let player = blueprint["actors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|actor| actor["id"] == 101)
                    .unwrap();
                assert_eq!(player["knownRooms"], serde_json::json!([1]));
                assert!(player["knowledge"].as_array().unwrap().len() <= 2);
                assert!(!world.instructions("npc", language).contains("1021"));
                assert!(!world.instructions("listener", language).contains("1021"));
                assert!(
                    !world
                        .entity_text("actors", 104, "role", language)
                        .unwrap()
                        .contains("dowódczyni")
                );
            }
        }
    }

    #[test]
    fn an_incomplete_installed_locale_is_rejected_before_use() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("locales")).unwrap();
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        let content = WorldContent::load("castle").unwrap();
        let mut world = content.scenario.clone();
        world["scenarioId"] = "test-package".into();
        std::fs::write(root.join("world.json"), world.to_string()).unwrap();
        for language in ["en", "pl"] {
            let text: Value = serde_json::from_str(if language == "en" {
                include_str!("../content/worlds/castle/locales/en.json")
            } else {
                include_str!("../content/worlds/castle/locales/pl.json")
            })
            .unwrap();
            std::fs::write(
                root.join(format!("locales/{language}.json")),
                text.to_string(),
            )
            .unwrap();
            for kind in ["npc", "listener"] {
                std::fs::write(
                    root.join(format!("prompts/{kind}.{language}.md")),
                    content.instructions(kind, language),
                )
                .unwrap();
            }
        }
        assert_eq!(
            WorldContent::from_directory(root).unwrap().id,
            "test-package"
        );
        world["scenarioId"] = "castle".into();
        std::fs::write(root.join("world.json"), world.to_string()).unwrap();
        assert!(WorldContent::from_directory(root).is_err());
        world["scenarioId"] = "test-package".into();
        std::fs::write(root.join("world.json"), world.to_string()).unwrap();
        let mut broken: Value =
            serde_json::from_str(include_str!("../content/worlds/castle/locales/pl.json")).unwrap();
        broken["actors"].as_object_mut().unwrap().remove("104");
        std::fs::write(root.join("locales/pl.json"), broken.to_string()).unwrap();
        assert!(WorldContent::from_directory(root).is_err());
    }
}
