use serde_json::{Map, Value, json};

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
        "seconds" => json!({"type":"integer","minimum":5,"maximum":1800}),
        "recipe" => choice(&["meal", "lockpick", "medicine"]),
        "style" => choice(&["lethal", "subdue"]),
        "mode" => choice(&[
            "heal",
            "poison",
            "pick_lock",
            "disguise",
            "trap",
            "cut_bonds",
        ]),
        "volume" => choice(&["normal", "whisper", "shout"]),
        "text" => text(1200),
        _ => id(),
    }
}
pub fn intents(speech: bool) -> Value {
    let variants: [(&str, &[&str]); 30] = [
        ("move", &["doorId"]),
        ("flee", &["doorId"]),
        ("inspect", &["targetId"]),
        ("take", &["itemId"]),
        ("steal", &["itemId"]),
        ("drop", &["itemId"]),
        ("equip", &["itemId"]),
        ("give", &["itemId", "targetId"]),
        ("offer", &["itemId", "targetId"]),
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
        ("guard", &["seconds"]),
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
                } else {
                    field(name)
                };
                properties.insert((*name).into(), value);
            }
            object(Value::Object(properties))
        })
        .collect::<Vec<_>>();
    json!({"anyOf":alternatives})
}
pub fn decision_schema() -> Value {
    let evidence =
        json!({"type":"array","items":{"type":"integer","minimum":1},"minItems":1,"maxItems":8});
    let delta = json!({"type":"integer","minimum":-15,"maximum":15});
    object(
        json!({"summary":text(300),"goals":array(text(240),5),"plan":{"type":"array","items":intents(true),"minItems":1,"maxItems":6},
        "memories":array(object(json!({"text":text(600),"kind":choice(&["belief","reflection"]),"evidenceIds":evidence,"importance":{"type":"integer","minimum":1,"maximum":10}})),4),
        "relationships":array(object(json!({"targetId":id(),"trust":delta,"affection":delta,"fear":delta,"respect":delta,"evidenceIds":evidence})),4),"acceptPlots":array(id(),3)}),
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
        assert_eq!(intents(true)["anyOf"].as_array().unwrap().len(), 30);
        assert!(
            intents(false)["anyOf"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["properties"]["kind"]["enum"][0] != "speak")
        );
    }
}
