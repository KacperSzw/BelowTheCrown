use crate::model::{Decision, Intent, PlotIntent, Volume};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Utterance {
    text: String,
    target_id: Option<u16>,
    volume: Volume,
    plot: Option<PlotIntent>,
    after_steps: u8,
}

// The native model chooses words independently of physical steps. Saves and
// the executor retain their canonical ordered Intent representation.
pub fn normalize_decision(mut raw: Value) -> Result<Value> {
    let object = raw
        .as_object_mut()
        .context("A decision must be an object.")?;
    ensure!(
        [
            "summary",
            "goals",
            "plan",
            "utterance",
            "memories",
            "relationships",
            "acceptPlots",
            "acceptHeardProposals"
        ]
        .iter()
        .all(|key| object.contains_key(*key)),
        "An NPC wire decision must include all eight fields."
    );
    let words = object
        .remove("utterance")
        .context("Missing utterance; use null for silence.")?;
    if let Some(fields) = words.as_object() {
        ensure!(
            ["text", "targetId", "volume", "plot", "afterSteps"]
                .iter()
                .all(|key| fields.contains_key(*key)),
            "An utterance must include all five fields."
        );
    }
    let utterance: Option<Utterance> = serde_json::from_value(words)?;
    let plan = object
        .get_mut("plan")
        .and_then(Value::as_array_mut)
        .context("A decision must contain a physical plan.")?;
    for step in plan.iter() {
        let intent: Intent = serde_json::from_value(step.clone())?;
        ensure!(
            !matches!(intent, Intent::Speak { .. }),
            "Speech belongs in utterance, not plan."
        );
    }
    if let Some(utterance) = utterance {
        let position = usize::from(utterance.after_steps);
        ensure!(
            position <= plan.len(),
            "utterance.afterSteps exceeds the physical plan length."
        );
        plan.insert(
            position,
            serde_json::to_value(Intent::Speak {
                text: utterance.text,
                target_id: utterance.target_id,
                volume: utterance.volume,
                plot: utterance.plot,
            })?,
        );
    }
    Ok(serde_json::to_value(Decision::parse(raw)?)?)
}

fn object(properties: Value) -> Value {
    let required = properties
        .as_object()
        .expect("schema properties")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn id() -> Value {
    json!({"type":"integer","minimum":1,"maximum":65535})
}
fn text(max: usize) -> Value {
    json!({"type":"string","minLength":1,"maxLength":max})
}
fn array(items: Value, max: usize) -> Value {
    json!({"type":"array","items":items,"maxItems":max})
}
fn choice(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
fn field(name: &str) -> Value {
    match name {
        "amount" => json!({"type":"integer","minimum":1,"maximum":1000}),
        "quantity" => {
            json!({"type":"integer","minimum":0,"maximum":255,"description":"Number of units; normally 1. Zero means all available units."})
        }
        "itemId" => {
            json!({"type":"integer","minimum":0,"maximum":65535,"description":"Copy the actual visible item ID. If you already hold the item, use its real ID, never zero. Zero is only the last portion acquired by take, steal, or accept_offer earlier in this same plan; it is not an inventory slot or a newly crafted item's ID."})
        }
        "seconds" => json!({"type":"integer","minimum":5,"maximum":1800}),
        "recipe" => choice(&["meal", "lockpick", "medicine"]),
        "style" => choice(&["lethal", "subdue"]),
        "mode" => choice(&["heal", "poison", "pick_lock", "trap"]),
        "volume" => choice(&["normal", "whisper", "shout"]),
        "text" => text(1200),
        _ => id(),
    }
}
pub fn intents(speech: bool) -> Value {
    let variants: [(&str, &[&str]); 29] = [
        ("move", &["doorId"]),
        ("flee", &["doorId"]),
        ("inspect", &["targetId"]),
        ("take", &["itemId", "quantity"]),
        ("steal", &["itemId", "quantity"]),
        ("drop", &["itemId", "quantity"]),
        ("equip", &["itemId"]),
        ("give", &["itemId", "targetId", "quantity"]),
        ("offer", &["itemId", "targetId", "quantity"]),
        ("pay", &["targetId", "amount"]),
        ("accept_offer", &["offerId"]),
        ("decline_offer", &["offerId"]),
        ("unlock", &["doorId"]),
        ("lock", &["doorId"]),
        ("open", &["doorId"]),
        ("close", &["doorId"]),
        ("eat", &["itemId"]),
        ("drink", &["itemId"]),
        ("rest", &["seconds"]),
        ("wait", &["seconds"]),
        ("defend", &["seconds"]),
        ("hide", &["seconds"]),
        ("work", &["recipe"]),
        ("attack", &["targetId", "style"]),
        ("bind", &["targetId"]),
        ("release", &["targetId"]),
        ("escort", &["targetId", "doorId"]),
        ("use", &["itemId", "targetId", "mode"]),
        ("speak", &["text", "targetId", "volume", "plot"]),
    ];
    let alternatives = variants
        .into_iter()
        .filter(|(kind, _)| speech || *kind != "speak")
        .map(|(kind, fields)| {
            let mut properties = Map::new();
            properties.insert("kind".into(), choice(&[kind]));
            for name in fields {
                let value = if kind == "speak" && *name == "targetId" {
                    json!({"anyOf":[id(),{"type":"null"}]})
                } else if *name == "plot" {
                    json!({"anyOf":[object(json!({"victimId":id()})),{"type":"null"}]})
                } else if kind == "rest" && *name == "seconds" {
                    json!({"type":"integer","minimum":5,"maximum":8640})
                } else {
                    field(name)
                };
                properties.insert((*name).into(), value);
            }
            let mut schema = object(Value::Object(properties));
            let description = match kind {
                "speak" => Some("Your complete utterance: this must be the only speak entry anywhere in plan, including after inspect or another action. Combine everything you want to say here. Speech cannot transfer an item or complete an action."),
                "inspect" => Some("Observe this target once. Inspection does not secure, take, move or alter it."),
                "take" => Some("Actually acquire this many units of an unowned item. If you intend to consume or use the portion now, include drink, eat or use with itemId=0 later in this same plan; saying you will take it or inspecting it does not acquire it."),
                "eat" | "drink" => Some("Actually consume one owned unit. After take, steal or accept_offer in this same plan, itemId=0 uses the acquired portion. Do not replace consumption with a spoken promise or repeated inspection."),
                "use" => Some("Actually use an owned item for this mode. Healing with an unowned medical kit requires take first, then use with itemId=0, mode=heal and the visible patient's targetId."),
                "give" => Some("Actually transfer this owned item to the recipient. Use this action, rather than speech or accept_offer, when giving something."),
                "accept_offer" => Some("Accept an existing offer addressed to you, using an offerId from offers. If offers is empty, this action is unavailable."),
                "wait" | "rest" | "hide" => Some("Start one activity for this duration. Do not repeat identical steps. Omit this action when you mean to continue currentActivity."),
                _ => None,
            };
            if let Some(description) = description {
                schema["description"] = json!(description);
            }
            schema
        })
        .collect::<Vec<_>>();
    json!({"anyOf":alternatives})
}
pub fn decision_schema() -> Value {
    let evidence =
        json!({"type":"array","items":{"type":"integer","minimum":1},"minItems":1,"maxItems":8});
    let delta = json!({"type":"integer","minimum":-15,"maximum":15});
    object(
        json!({"summary":text(300),"goals":array(text(240),5),"plan":{"type":"array","items":intents(false),"maxItems":6,"description":"Physical steps only. Empty can mean continue your activity or quietly observe. Words are optional and belong in utterance. At most six steps total including a non-null utterance. Later steps must satisfy their preconditions when executed."},
        "utterance":{"anyOf":[object(json!({"text":text(1200),"targetId":{"anyOf":[id(),{"type":"null"}]},"volume":field("volume"),"plot":{"anyOf":[object(json!({"victimId":id()})),{"type":"null"}]},"afterSteps":{"type":"integer","minimum":0,"maximum":6}})),{"type":"null"}],"description":"Words you actually want someone to hear, or null for silence. Not a report of your plan, private thoughts, or execution metadata. Genuine coordination, promises and explanations can be spoken when socially motivated. afterSteps is the number of physical steps to finish before speaking, from zero to plan length."},
        "memories":array(object(json!({"text":text(600),"kind":choice(&["belief","reflection"]),"evidenceIds":evidence,"importance":{"type":"integer","minimum":1,"maximum":10}})),4),
        "relationships":array(object(json!({"targetId":id(),"trust":delta,"affection":delta,"fear":delta,"respect":delta,"evidenceIds":evidence})),4),"acceptPlots":array(id(),3),
        "acceptHeardProposals":array(object(json!({"memoryId":{"type":"integer","minimum":1},"victimId":id()})),3)}),
    )
}
pub fn interpretation_schema() -> Value {
    object(json!({"summary":text(300),"plan":array(intents(false),3)}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn strict(schema: &Value) {
        if schema["type"] == "object" {
            assert_eq!(schema["additionalProperties"], false);
            let properties = schema["properties"].as_object().unwrap();
            assert_eq!(
                schema["required"].as_array().unwrap().len(),
                properties.len()
            );
            for value in properties.values() {
                strict(value);
            }
        }
        if let Some(variants) = schema["anyOf"].as_array() {
            for item in variants {
                strict(item);
            }
        }
        if schema["type"] == "array" {
            strict(&schema["items"]);
        }
        assert!(schema.get("oneOf").is_none());
    }
    #[test]
    fn schemas_are_strict_and_listener_cannot_speak() {
        strict(&decision_schema());
        strict(&interpretation_schema());
        assert_eq!(intents(true)["anyOf"].as_array().unwrap().len(), 29);
        assert!(
            intents(false)["anyOf"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["properties"]["kind"]["enum"][0] != "speak")
        );
        assert_eq!(
            decision_schema()["properties"]["plan"]["items"],
            intents(false)
        );
    }

    fn wire(plan: Value, utterance: Value) -> Value {
        json!({"summary":"A private thought.","goals":[],"plan":plan,"utterance":utterance,"memories":[],"relationships":[],"acceptPlots":[],"acceptHeardProposals":[]})
    }
    fn words(position: u8) -> Value {
        json!({"text":"Here.","targetId":null,"volume":"normal","plot":null,"afterSteps":position})
    }
    #[test]
    fn utterance_is_optional_and_preserves_every_execution_position() {
        let physical =
            json!([{"kind":"take","itemId":12,"quantity":1},{"kind":"drink","itemId":0}]);
        assert_eq!(
            normalize_decision(wire(physical.clone(), Value::Null)).unwrap()["plan"],
            physical
        );
        for position in 0..=2 {
            let result = normalize_decision(wire(physical.clone(), words(position))).unwrap();
            let plan = result["plan"].as_array().unwrap();
            assert_eq!(plan[usize::from(position)]["kind"], "speak");
            let remaining: Vec<_> = plan
                .iter()
                .filter(|step| step["kind"] != "speak")
                .cloned()
                .collect();
            assert_eq!(json!(remaining), physical);
            assert!(result.get("utterance").is_none());
        }
        assert_eq!(
            normalize_decision(wire(json!([]), words(0))).unwrap()["plan"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            normalize_decision(wire(json!([]), Value::Null)).unwrap()["plan"],
            json!([])
        );
    }
    #[test]
    fn wire_rejects_bad_positions_embedded_speech_and_combined_limit() {
        assert!(normalize_decision(wire(json!([]), words(1))).is_err());
        assert!(
            normalize_decision(wire(
                json!([{"kind":"speak","text":"Hi","targetId":null,"volume":"normal","plot":null}]),
                Value::Null
            ))
            .is_err()
        );
        let plan = json!(vec![json!({"kind":"inspect","targetId":11}); 6]);
        assert!(normalize_decision(wire(plan.clone(), Value::Null)).is_ok());
        assert!(normalize_decision(wire(plan, words(0))).is_err());
        let mut missing = wire(json!([]), Value::Null);
        missing.as_object_mut().unwrap().remove("utterance");
        assert!(normalize_decision(missing).is_err());
        let mut missing = words(0);
        missing.as_object_mut().unwrap().remove("plot");
        assert!(normalize_decision(wire(json!([]), missing)).is_err());
    }
}
