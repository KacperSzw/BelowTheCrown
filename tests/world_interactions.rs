use below_the_crown::{
    engine::Engine,
    model::*,
    room_context::{ContextBudget, room_context},
    store::{Store, assert_world, migrate_world},
    ui::{Operation, parse_input, suggestions},
};
use serde_json::{Value, json};

fn fixture(scenario: &str, language: &str) -> Engine {
    Engine::memory_with_tuning(
        scenario,
        language,
        41,
        "solo",
        Tuning {
            hunger_per_hour: 0.,
            thirst_per_hour: 0.,
            fatigue_per_hour: 0.,
            action_seconds: 0.03,
            travel_seconds: 0.07,
            combat_round_seconds: 0.11,
            ..Tuning::default()
        },
    )
    .unwrap()
}
fn speech(text: &str) -> Value {
    json!({"kind":"speak","text":text,"targetId":null,"volume":"normal","plot":null})
}
fn dispatch(engine: &mut Engine, actor: u16, intent: Value) -> Value {
    engine.dispatch(actor, intent, vec![], None).unwrap()
}
fn finish(engine: &mut Engine, actor: u16, intent: Value) {
    let result = dispatch(engine, actor, intent);
    assert_eq!(result["ok"], true, "{result}");
    if let Some(activity) = engine.actor(actor).unwrap().activity.clone() {
        let seconds = ((activity.ends - engine.world.time) / CLOCK_RATE)
            .max(
                activity
                    .interaction_ends
                    .unwrap_or(engine.world.interaction_time)
                    - engine.world.interaction_time,
            )
            .max(0.);
        engine.advance(seconds + 1e-8).unwrap();
    }
}
fn own(engine: &mut Engine, item: u16, actor: u16) {
    for person in &mut engine.world.actors {
        person.equipment.retain(|id| *id != item);
    }
    let item = engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == item)
        .unwrap();
    item.owner_id = Some(actor);
    item.room_id = None;
    item.equipped = false;
}
fn decision(plan: Value) -> Value {
    json!({"summary":"A controlled choice.","goals":[],"plan":plan,"memories":[],"relationships":[],"acceptPlots":[]})
}
fn apply(engine: &mut Engine, actor: u16, plan: Value) -> Value {
    let observation = engine.observation(actor, "Controlled perception.").unwrap();
    let evidence: Vec<u64> =
        serde_json::from_value(observation["context"]["evidenceIds"].clone()).unwrap();
    engine
        .apply_decision(
            actor,
            decision(plan),
            observation["context"]["version"].as_u64().unwrap(),
            &evidence,
        )
        .unwrap()
}
fn action(engine: &Engine, actor: u16, kind: &str, item: u16) -> Value {
    engine
        .actions(actor)
        .unwrap()
        .into_iter()
        .find(|action| action["intent"]["kind"] == kind && action["intent"]["itemId"] == item)
        .unwrap()
}

#[test]
fn execution_feedback_is_private_control_data_not_a_perceived_memory() {
    let mut engine = fixture("castle", "pl");
    engine.actor_mut(102).unwrap().room_id = 3;
    let before = engine.store.memories(102, 100).unwrap();
    assert_eq!(
        dispatch(&mut engine, 102, json!({"kind":"wait","seconds":60}))["ok"],
        true
    );
    assert_eq!(engine.store.memories(102, 100).unwrap().len(), before.len());
    let event = engine.store.events(Some(102), 1).unwrap().pop().unwrap();
    assert!(matches!(event.details, Some(EventDetails::ActivityStarted)));
    engine.actor_mut(102).unwrap().clear_plan();
    engine.actor_mut(102).unwrap().activity = None;
    assert_eq!(
        apply(
            &mut engine,
            102,
            json!([
                {"kind":"wait","seconds":5},
                {"kind":"take","itemId":1002,"quantity":1},
                {"kind":"speak","text":"This must never be heard.","targetId":null,"volume":"normal","plot":null}
            ])
        )["ok"],
        true
    );
    engine.advance(5. / CLOCK_RATE + 1e-8).unwrap();
    let observation = engine.observation(102, "Reconsider.").unwrap()["input"].clone();
    assert!(observation["world"]["self"].get("health").is_none());
    assert!(observation["execution"]["self"]["health"].is_number());
    assert!(
        observation["world"]["people"]
            .as_array()
            .unwrap()
            .iter()
            .all(|person| person.get("health").is_none())
    );
    assert!(observation["execution"]["people"].is_array());
    assert_eq!(
        observation["execution"]["self"]["lastPlanFailure"]["intent"]["kind"],
        "take"
    );
    assert!(
        observation["world"]["self"]
            .get("lastPlanFailure")
            .is_none()
    );
    assert!(
        engine
            .store
            .events(Some(102), 20)
            .unwrap()
            .iter()
            .any(|e| matches!(e.details, Some(EventDetails::ExecutionFeedback)))
    );
    let memories = engine.store.memories(102, 100).unwrap();
    assert_eq!(memories.len(), before.len());
    assert!(
        engine
            .store
            .events(None, 100)
            .unwrap()
            .iter()
            .all(|e| !e.text.contains("This must never be heard."))
    );
}

#[test]
fn royal_presence_cannot_block_either_direction_but_physical_locks_do() {
    for language in ["pl", "en"] {
        for start in [10, 11] {
            let mut engine = fixture("castle", language);
            let destination = if start == 10 { 11 } else { 10 };
            engine.actor_mut(PLAYER).unwrap().room_id = start;
            engine.actor_mut(103).unwrap().room_id = destination;
            let door = engine.world.doors.iter_mut().find(|d| d.id == 210).unwrap();
            door.restricted = true; // Legacy saves keep the field; it confers no invisible guard.
            door.flags = OPEN;
            assert!(
                engine.player_view(PLAYER).unwrap()["doors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|d| d.get("restricted").is_none())
            );
            finish(&mut engine, PLAYER, json!({"kind":"move","doorId":210}));
            assert_eq!(engine.actor(PLAYER).unwrap().room_id, destination);
            engine
                .world
                .doors
                .iter_mut()
                .find(|d| d.id == 210)
                .unwrap()
                .flags = LOCKED;
            assert_eq!(
                dispatch(&mut engine, PLAYER, json!({"kind":"move","doorId":210}))["ok"],
                false
            );
            assert_eq!(engine.actor(PLAYER).unwrap().room_id, destination);
        }
    }
}

#[test]
fn partial_transfers_preserve_source_and_copy_physical_and_text_provenance() {
    for count in [1, 2, 254] {
        let mut engine = fixture("castle", "en");
        let source = engine
            .world
            .items
            .iter_mut()
            .find(|i| i.id == 1023)
            .unwrap();
        source.quantity = 255;
        source.poisoned_by = Some(108);
        source.causes = vec![];
        source.key = Some("test-key".into());
        source.power = 17.25;
        source.description = "My literal label".into();
        let before = source.clone();
        let allocated = engine.world.next_item_id as u16;
        finish(
            &mut engine,
            PLAYER,
            json!({"kind":"take","itemId":1023,"quantity":count}),
        );
        assert_eq!(engine.item(1023).unwrap().quantity, 255 - count);
        assert_eq!(engine.item(1023).unwrap().owner_id, before.owner_id);
        let part = engine.item(allocated).unwrap();
        assert_eq!(part.quantity, count);
        assert_eq!(part.owner_id, Some(PLAYER));
        assert!(!part.equipped);
        assert_eq!(part.poisoned_by, before.poisoned_by);
        assert_eq!(part.power, before.power);
        assert_eq!(part.key, before.key);
        assert_eq!(part.text_origin, before.text_origin);
        assert_eq!(part.description, before.description);
        assert_world(&engine.world).unwrap();
    }
}
#[test]
fn partial_equipped_clothes_keep_remainder_equipped_until_the_whole_stack_moves() {
    let mut engine = fixture("castle", "pl");
    own(&mut engine, 1014, PLAYER);
    engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == 1014)
        .unwrap()
        .quantity = 3;
    finish(&mut engine, PLAYER, json!({"kind":"equip","itemId":1014}));
    let part = engine.world.next_item_id as u16;
    finish(
        &mut engine,
        PLAYER,
        json!({"kind":"drop","itemId":1014,"quantity":1}),
    );
    assert_eq!(engine.item(1014).unwrap().quantity, 2);
    assert!(engine.item(1014).unwrap().equipped);
    assert_eq!(engine.actor(PLAYER).unwrap().flags & DISGUISED, DISGUISED);
    assert!(engine.actor(PLAYER).unwrap().equipment.contains(&1014));
    assert!(!engine.item(part).unwrap().equipped);
    assert!(engine.item(part).unwrap().visible);
    finish(
        &mut engine,
        PLAYER,
        json!({"kind":"drop","itemId":1014,"quantity":0}),
    );
    assert_eq!(engine.item(1014).unwrap().owner_id, None);
    assert!(!engine.item(1014).unwrap().equipped);
    assert_eq!(engine.actor(PLAYER).unwrap().flags & DISGUISED, 0);
    assert_world(&engine.world).unwrap();
}
#[test]
fn exhausted_split_ids_do_not_consume_rng_or_mutate_custody_but_whole_transfer_works() {
    let mut engine = fixture("castle", "en");
    engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == 1007)
        .unwrap()
        .quantity = 3;
    engine.world.next_item_id = 65536;
    let before = json!(engine.world);
    let rng = engine.world.rng;
    assert_eq!(
        dispatch(
            &mut engine,
            PLAYER,
            json!({"kind":"steal","itemId":1007,"quantity":1})
        )["ok"],
        false
    );
    assert_eq!(json!(engine.world), before);
    assert_eq!(engine.world.rng, rng);
    own(&mut engine, 1007, PLAYER);
    finish(
        &mut engine,
        PLAYER,
        json!({"kind":"drop","itemId":1007,"quantity":0}),
    );
    assert_eq!(engine.item(1007).unwrap().quantity, 3);
    assert_world(&engine.world).unwrap();
}
#[test]
fn quantity_boundaries_and_legacy_whole_stack_semantics_are_distinct() {
    for count in [0, 1, 255] {
        let mut engine = fixture("castle", "en");
        engine
            .world
            .items
            .iter_mut()
            .find(|i| i.id == 1023)
            .unwrap()
            .quantity = 255;
        finish(
            &mut engine,
            PLAYER,
            json!({"kind":"take","itemId":1023,"quantity":count}),
        );
        let owned = engine
            .inventory(PLAYER)
            .iter()
            .filter(|i| i.kind == ItemKind::Water)
            .map(|i| u16::from(i.quantity))
            .sum::<u16>();
        assert_eq!(owned, if count == 0 { 255 } else { count });
    }
    let parsed = Intent::parse(json!({"kind":"take","itemId":1023})).unwrap();
    assert_eq!(parsed.quantity(), Some(0));
    for value in [-1, 256, 1_000] {
        assert!(Intent::parse(json!({"kind":"take","itemId":1023,"quantity":value})).is_err());
    }
    let mut engine = fixture("castle", "en");
    let available = engine.item(1023).unwrap().quantity;
    assert_eq!(
        dispatch(
            &mut engine,
            PLAYER,
            json!({"kind":"take","itemId":1023,"quantity":available+1})
        )["ok"],
        false
    );
}
#[test]
fn offers_fix_their_quantity_without_reserving_stock_and_recheck_on_acceptance() {
    let mut engine = fixture("castle", "en");
    own(&mut engine, 1023, PLAYER);
    engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == 1023)
        .unwrap()
        .quantity = 3;
    engine.actor_mut(104).unwrap().room_id = 1;
    finish(
        &mut engine,
        PLAYER,
        json!({"kind":"offer","itemId":1023,"targetId":102,"quantity":2}),
    );
    let first = engine.world.offers.last().unwrap().id;
    finish(
        &mut engine,
        PLAYER,
        json!({"kind":"offer","itemId":1023,"targetId":104,"quantity":2}),
    );
    let second = engine.world.offers.last().unwrap().id;
    assert_eq!(engine.item(1023).unwrap().quantity, 3);
    finish(
        &mut engine,
        102,
        json!({"kind":"accept_offer","offerId":first}),
    );
    assert_eq!(engine.item(1023).unwrap().quantity, 1);
    assert_eq!(
        dispatch(
            &mut engine,
            104,
            json!({"kind":"accept_offer","offerId":second})
        )["ok"],
        false
    );
    assert_eq!(
        engine
            .world
            .offers
            .iter()
            .find(|o| o.id == second)
            .unwrap()
            .status,
        "open"
    );
    assert_world(&engine.world).unwrap();
}
#[test]
fn npc_acquired_reference_survives_save_load_and_drinks_the_split_portion() {
    for language in ["en", "pl"] {
        let mut engine = fixture("castle", language);
        engine.actor_mut(102).unwrap().thirst = 63.;
        engine
            .world
            .items
            .iter_mut()
            .find(|i| i.id == 1023)
            .unwrap()
            .quantity = 4;
        let power = engine.item(1023).unwrap().power;
        let split = engine.world.next_item_id as u16;
        assert_eq!(
            apply(
                &mut engine,
                102,
                json!([{"kind":"take","itemId":1023,"quantity":1},{"kind":"drink","itemId":0}])
            )["ok"],
            true
        );
        engine.advance(engine.tuning.action_seconds + 1e-8).unwrap();
        assert_eq!(engine.actor(102).unwrap().last_acquired_item_id, split);
        assert_eq!(
            engine
                .actor(102)
                .unwrap()
                .activity
                .as_ref()
                .unwrap()
                .intent
                .item_id(),
            Some(split)
        );
        assert_eq!(engine.item(1023).unwrap().quantity, 3);
        engine.save("split").unwrap();
        engine.load("split", language).unwrap();
        engine.world.manual_pause = false;
        engine.advance(engine.tuning.action_seconds + 1e-8).unwrap();
        assert!(engine.item(split).is_none());
        assert_eq!(engine.item(1023).unwrap().quantity, 3);
        assert!((engine.actor(102).unwrap().thirst - (63. - power).max(0.)).abs() < 1e-8);
        assert_eq!(engine.actor(102).unwrap().last_acquired_item_id, 0);
    }
}
#[test]
fn listener_acquired_reference_uses_actual_transfer_result_and_rejects_unbound_zero() {
    let mut engine = fixture("castle", "en");
    engine.actor_mut(PLAYER).unwrap().thirst = 72.;
    engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == 1023)
        .unwrap()
        .quantity = 4;
    let source = dispatch(
        &mut engine,
        PLAYER,
        speech("Take one water portion and drink it."),
    )["eventId"]
        .as_u64()
        .unwrap();
    assert_eq!(engine.start_player_plan(PLAYER,json!({"summary":"Drink one actual portion.","plan":[{"kind":"take","itemId":1023,"quantity":1},{"kind":"drink","itemId":0}]}),source,None).unwrap()["ok"],true);
    engine
        .advance(engine.tuning.action_seconds * 2. + 1e-7)
        .unwrap();
    assert_eq!(engine.item(1023).unwrap().quantity, 3);
    assert!(
        engine
            .inventory(PLAYER)
            .iter()
            .all(|i| i.kind != ItemKind::Water)
    );
    assert_eq!(
        dispatch(&mut engine, PLAYER, json!({"kind":"drink","itemId":0}))["ok"],
        false
    );
    assert_eq!(
        apply(&mut engine, 102, json!([{"kind":"drink","itemId":0}]))["ok"],
        false
    );
    assert!(
        Interpretation::parse(
            json!({"summary":"Unbound reference.","plan":[{"kind":"drink","itemId":0}]})
        )
        .is_err()
    );
}
#[test]
fn speaking_during_a_listener_plan_preserves_its_acquired_portion() {
    let mut engine = fixture("castle", "en");
    engine.actor_mut(PLAYER).unwrap().thirst = 72.;
    engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == 1023)
        .unwrap()
        .quantity = 4;
    let power = engine.item(1023).unwrap().power;
    let source = dispatch(
        &mut engine,
        PLAYER,
        speech("Take one portion, wait, then drink it."),
    )["eventId"]
        .as_u64()
        .unwrap();
    assert_eq!(engine.start_player_plan(PLAYER,json!({"summary":"Use one portion after waiting.","plan":[{"kind":"take","itemId":1023,"quantity":1},{"kind":"wait","seconds":60},{"kind":"drink","itemId":0}]}),source,None).unwrap()["ok"],true);
    engine.advance(engine.tuning.action_seconds + 1e-8).unwrap();
    let acquired = engine.actor(PLAYER).unwrap().last_acquired_item_id;
    assert_ne!(acquired, 0);
    assert_eq!(engine.item(acquired).unwrap().owner_id, Some(PLAYER));
    assert_eq!(
        dispatch(
            &mut engine,
            PLAYER,
            speech("I can still talk while waiting.")
        )["ok"],
        true
    );
    assert_eq!(
        engine.actor(PLAYER).unwrap().last_acquired_item_id,
        acquired
    );
    assert_eq!(engine.actor(PLAYER).unwrap().plan.len(), 1);
    engine
        .advance(60. / CLOCK_RATE + engine.tuning.action_seconds + 1e-7)
        .unwrap();
    assert!(engine.item(acquired).is_none());
    assert_eq!(engine.item(1023).unwrap().quantity, 3);
    assert!((engine.actor(PLAYER).unwrap().thirst - (72. - power)).abs() < 1e-8);
    assert_eq!(engine.actor(PLAYER).unwrap().last_acquired_item_id, 0);
    assert!(engine.actor(PLAYER).unwrap().last_plan_failure.is_none());
}
#[test]
fn failed_or_cancelled_acquisition_does_not_leave_a_reference_for_another_plan() {
    let mut engine = fixture("castle", "en");
    assert_eq!(
        apply(
            &mut engine,
            102,
            json!([{"kind":"take","itemId":1023,"quantity":1},{"kind":"drink","itemId":0}])
        )["ok"],
        true
    );
    // Another actor gets the source while the scheduled acquisition is pending.
    own(&mut engine, 1023, PLAYER);
    engine.advance(engine.tuning.action_seconds + 1e-8).unwrap();
    assert_eq!(engine.actor(102).unwrap().last_acquired_item_id, 0);
    assert!(engine.actor(102).unwrap().plan.is_empty());
    assert_eq!(
        engine
            .actor(102)
            .unwrap()
            .last_action_outcome
            .as_ref()
            .unwrap()
            .stage,
        OutcomeStage::Failed
    );
    assert_eq!(
        apply(&mut engine, 102, json!([{"kind":"drink","itemId":0}]))["ok"],
        false
    );
}
#[test]
fn repeating_speech_is_grouped_before_limits_and_uses_owned_memory_evidence() {
    let mut engine = fixture("castle", "pl");
    for id in [PLAYER, 102, KING] {
        engine.actor_mut(id).unwrap().room_id = 10;
    }
    dispatch(&mut engine, PLAYER, speech("Królu, wysłuchasz mnie?"));
    dispatch(&mut engine, KING, speech("Mów. Słucham."));
    for _ in 0..70 {
        dispatch(&mut engine, 102, speech("Jestem głodny!"));
    }
    let observation = engine.observation(KING, "Rozmowa.").unwrap();
    let entries = observation["input"]["world"]["roomContext"]["entries"]
        .as_array()
        .unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2]["count"], 70);
    assert!(entries[0]["text"].as_str().unwrap().contains("wysłuchasz"));
    for id in entries[2]["evidenceIds"].as_array().unwrap() {
        assert_eq!(
            engine
                .store
                .owned_memory(KING, id.as_u64().unwrap())
                .unwrap()
                .unwrap()
                .source_id,
            Some(102)
        );
    }
    assert!(entries[2]["evidenceIds"].as_array().unwrap().len() <= 4);
    assert!(
        observation["input"]["world"]["memories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| !m["text"].as_str().unwrap().contains("Jestem głodny!"))
    );
}
#[test]
fn private_beliefs_between_repeated_speech_remain_in_automatic_recall() {
    let mut engine = fixture("castle", "en");
    for id in [PLAYER, KING] {
        engine.actor_mut(id).unwrap().room_id = 10;
    }
    dispatch(&mut engine, PLAYER, speech("Listen to my petition."));
    let belief = engine
        .store
        .remember(Memory {
            id: 0,
            actor_id: KING,
            time: engine.world.time,
            text: "The petitioner may be concealing a motive.".into(),
            kind: "belief".into(),
            importance: 10,
            source_id: Some(KING),
            event_id: None,
            evidence_ids: vec![],
            perceived: None,
        })
        .unwrap();
    dispatch(&mut engine, PLAYER, speech("Listen to my petition."));
    let observation = engine.observation(KING, "A repeated petition.").unwrap();
    let entries = observation["input"]["world"]["roomContext"]["entries"]
        .as_array()
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry["kind"] == "heard")
            .count(),
        2
    );
    assert!(entries.iter().all(|entry| entry["count"] == 1));
    assert!(
        observation["input"]["world"]["memories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == belief.id)
    );
    assert!(
        observation["context"]["evidenceIds"]
            .as_array()
            .unwrap()
            .contains(&json!(belief.id))
    );
}
#[test]
fn context_never_merges_across_public_movement_and_is_scoped_to_the_new_visit() {
    let mut engine = fixture("castle", "en");
    for id in [PLAYER, 102, KING] {
        engine.actor_mut(id).unwrap().room_id = 10;
    }
    let door = engine
        .exits(10)
        .into_iter()
        .find(|d| d.flags & LOCKED == 0)
        .unwrap()
        .id;
    finish(&mut engine, 102, json!({"kind":"open","doorId":door}));
    dispatch(&mut engine, PLAYER, speech("Listen."));
    finish(&mut engine, 102, json!({"kind":"move","doorId":door}));
    dispatch(&mut engine, PLAYER, speech("Listen."));
    let context = room_context(
        &engine.store,
        engine.actor(KING).unwrap(),
        engine.world.time,
        "en",
        ContextBudget::default(),
    )
    .unwrap();
    assert_eq!(
        context.entries.iter().filter(|e| e.kind == "heard").count(),
        2
    );
    assert!(context.entries.iter().any(|e| matches!(
        e.perceived.as_ref().map(|p| &p.details),
        Some(EventDetails::Left)
    )));
    finish(&mut engine, 102, json!({"kind":"move","doorId":door}));
    let returned = engine.observation(102, "Returned.").unwrap();
    assert!(
        returned["input"]["world"]["roomContext"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["eventId"].as_u64().unwrap()
                >= engine.actor(102).unwrap().entered_event_id)
    );
    assert!(
        returned["input"]["world"]["roomContext"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| !entry["text"].as_str().unwrap().contains("Listen."))
    );
}
#[test]
fn chronology_projects_unknown_shouts_without_recipient_identity_or_private_whispers() {
    let mut engine = fixture("castle", "en");
    engine.actor_mut(104).unwrap().room_id = 2;
    engine
        .actor_mut(104)
        .unwrap()
        .known_actors
        .retain(|id| *id != PLAYER);
    dispatch(
        &mut engine,
        PLAYER,
        json!({"kind":"speak","text":"private-test-secret","targetId":102,"volume":"whisper","plot":null}),
    );
    let id=dispatch(&mut engine,PLAYER,json!({"kind":"speak","text":"Listen to the king carefully!","targetId":102,"volume":"shout","plot":null}))["eventId"].as_u64().unwrap();
    let projected = engine.store.observed_event(104, id).unwrap().unwrap();
    assert_eq!(projected.room_id, Some(2));
    assert_eq!(projected.actor_id, None);
    match projected.details.unwrap() {
        EventDetails::Speech {
            speaker_name,
            recipient_id,
            recipient_name,
            recipient_known,
            ..
        } => {
            assert_eq!(speaker_name, None);
            assert_eq!(recipient_id, None);
            assert_eq!(recipient_name, None);
            assert!(!recipient_known);
        }
        _ => panic!("Expected projected speech"),
    }
    let input = engine.observation(104, "Heard a voice.").unwrap()["input"].clone();
    assert!(!input.to_string().contains("private-test-secret"));
    assert!(
        !input["world"]["roomContext"]
            .to_string()
            .contains("Listen to the king carefully!")
    );
}
#[test]
fn chronology_byte_and_entry_limits_are_read_only_and_mark_omissions() {
    let mut engine = fixture("castle", "en");
    for index in 0..12 {
        dispatch(
            &mut engine,
            PLAYER,
            speech(&format!("A distinct question {index}: {}", "ż".repeat(70))),
        );
    }
    let before = json!(engine.world);
    let journal = json!(engine.store.events(None, 1000).unwrap());
    for bytes in [700, 1600] {
        let context = room_context(
            &engine.store,
            engine.actor(102).unwrap(),
            engine.world.time,
            "pl",
            ContextBudget {
                speech: 3,
                events: 2,
                bytes,
            },
        )
        .unwrap();
        assert!(serde_json::to_vec(&context).unwrap().len() <= bytes);
        assert!(context.entries.len() <= 3);
        assert!(context.omitted_older_entries);
    }
    assert_eq!(json!(engine.world), before);
    assert_eq!(json!(engine.store.events(None, 1000).unwrap()), journal);
}
#[test]
fn forecasts_do_not_mutate_state_or_disclose_opponent_skill_or_move() {
    let mut engine = fixture("castle", "en");
    let before = json!(engine.world);
    let events = json!(engine.store.events(None, 1000).unwrap());
    let find = |engine: &Engine| {
        engine
            .actions(PLAYER)
            .unwrap()
            .into_iter()
            .find(|a| a["intent"]["kind"] == "attack" && a["intent"]["targetId"] == 102)
            .unwrap()["forecast"]
            .clone()
    };
    let forecast = find(&engine);
    assert_eq!(json!(engine.world), before);
    assert_eq!(json!(engine.store.events(None, 1000).unwrap()), events);
    for skill in [0., 37., 100.] {
        engine.actor_mut(102).unwrap().skills.combat = skill;
        assert_eq!(find(&engine), forecast);
    }
    assert!(
        forecast["effects"][0]["loss"]["maximum"].as_f64().unwrap()
            >= forecast["effects"][0]["loss"]["minimum"].as_f64().unwrap()
    );
    assert!(!forecast.to_string().contains("successChance"));
    dispatch(
        &mut engine,
        PLAYER,
        json!({"kind":"attack","targetId":102,"style":"lethal"}),
    );
    let before_move = find(&engine);
    engine.world.encounters[0]
        .participants
        .iter_mut()
        .find(|p| p.actor_id == 102)
        .unwrap()
        .choice = Some(Intent::Defend { seconds: 30 });
    assert_eq!(find(&engine), before_move);
}
#[test]
fn combat_time_preview_accounts_for_joining_and_flee_travel_as_an_upper_bound() {
    let mut engine = fixture("castle", "en");
    engine.actor_mut(KING).unwrap().room_id = 1;
    engine
        .world
        .doors
        .iter_mut()
        .find(|d| d.id == 201)
        .unwrap()
        .flags = 0;
    assert_eq!(
        dispatch(
            &mut engine,
            102,
            json!({"kind":"attack","targetId":KING,"style":"lethal"})
        )["ok"],
        true
    );
    let preview = engine
        .actions(PLAYER)
        .unwrap()
        .into_iter()
        .find(|a| a["intent"]["kind"] == "attack")
        .unwrap()["forecast"]
        .clone();
    assert_eq!(preview["durationKind"], "maximum");
    assert!(
        (preview["durationRealSeconds"].as_f64().unwrap()
            - 2. * engine.tuning.combat_round_seconds)
            .abs()
            < 1e-8
    );
    assert_eq!(
        dispatch(
            &mut engine,
            PLAYER,
            json!({"kind":"attack","targetId":102,"style":"lethal"})
        )["ok"],
        true
    );
    let escape = engine
        .actions(PLAYER)
        .unwrap()
        .into_iter()
        .find(|a| a["intent"]["kind"] == "flee")
        .unwrap()["forecast"]
        .clone();
    assert_eq!(escape["durationKind"], "maximum");
    assert!(
        (escape["durationRealSeconds"].as_f64().unwrap()
            - (2. * engine.tuning.combat_round_seconds + engine.tuning.travel_seconds))
            .abs()
            < 1e-8
    );
}
#[test]
fn combat_reports_resolved_fractional_damage_and_subdue_never_heals_low_hp() {
    for health in [3., 8.5, 60.] {
        let mut engine = fixture("castle", "en");
        engine.actor_mut(PLAYER).unwrap().skills.combat = 25.;
        engine.actor_mut(102).unwrap().skills.combat = 10.;
        engine.actor_mut(PLAYER).unwrap().fatigue = 0.;
        engine.actor_mut(102).unwrap().health = health;
        assert_eq!(
            dispatch(
                &mut engine,
                PLAYER,
                json!({"kind":"attack","targetId":102,"style":"subdue"})
            )["ok"],
            true
        );
        assert_eq!(
            engine
                .actor(PLAYER)
                .unwrap()
                .last_action_outcome
                .as_ref()
                .unwrap()
                .stage,
            OutcomeStage::Accepted
        );
        dispatch(&mut engine, 102, json!({"kind":"defend","seconds":30}));
        let target = engine.actor(102).unwrap();
        assert!(target.health <= health);
        let expected = if health < 10. { health } else { 53.5 };
        assert!((target.health - expected).abs() < 1e-8);
        let outcome = engine
            .actor(PLAYER)
            .unwrap()
            .last_action_outcome
            .as_ref()
            .unwrap();
        assert_eq!(outcome.stage, OutcomeStage::Resolved);
        assert!(
            outcome
                .reason
                .contains(if health < 10. { "−0 HP" } else { "−6.5 HP" })
        );
        assert!(
            engine
                .store
                .events(Some(PLAYER), 100)
                .unwrap()
                .iter()
                .any(|e| e.kind == "injury" && e.text.contains("HP"))
        );
    }
}
#[test]
fn eating_and_medicine_report_actual_clamped_effects_without_public_need_or_poison_leaks() {
    let mut engine = fixture("castle", "en");
    engine.actor_mut(PLAYER).unwrap().hunger = 4.5;
    engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == 1005)
        .unwrap()
        .power = 9.;
    let preview = action(&engine, PLAYER, "eat", 1005);
    assert_eq!(preview["forecast"]["effects"][0]["delta"], -4.5);
    let hp = engine.actor(PLAYER).unwrap().health;
    finish(&mut engine, PLAYER, json!({"kind":"eat","itemId":1005}));
    assert_eq!(engine.actor(PLAYER).unwrap().health, hp);
    assert!(
        engine
            .actor(PLAYER)
            .unwrap()
            .last_action_outcome
            .as_ref()
            .unwrap()
            .reason
            .contains("−4.5")
    );
    assert!(
        engine
            .store
            .events(Some(102), 100)
            .unwrap()
            .iter()
            .all(|e| !e.text.contains("Hunger"))
    );
    let medicine = engine
        .world
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Medicine)
        .unwrap()
        .id;
    own(&mut engine, medicine, PLAYER);
    engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == medicine)
        .unwrap()
        .power = 8.25;
    engine.actor_mut(102).unwrap().health = 96.5;
    engine.world.poison.push(PoisonEffect {
        actor_id: 102,
        source_id: 108,
        damage: 1.,
        next: engine.world.time + 1000.,
        remaining: 1,
        causes: vec![],
        interaction_next: Some(engine.world.interaction_time + 1000.),
    });
    engine.actor_mut(104).unwrap().room_id = 1;
    finish(
        &mut engine,
        PLAYER,
        json!({"kind":"use","itemId":medicine,"targetId":102,"mode":"heal"}),
    );
    assert_eq!(engine.actor(102).unwrap().health, 100.);
    assert!(engine.world.poison.is_empty());
    assert!(
        engine
            .store
            .events(Some(104), 100)
            .unwrap()
            .iter()
            .all(|e| !e.text.contains("removed your poisoning"))
    );
    assert!(
        engine
            .actor(PLAYER)
            .unwrap()
            .last_action_outcome
            .as_ref()
            .unwrap()
            .reason
            .contains("+3.5 HP")
    );
}
#[test]
fn authored_objects_and_translated_splits_exist_only_in_new_worlds() {
    for scenario in ["castle", "siege", "succession"] {
        for language in ["en", "pl"] {
            let mut engine = fixture(scenario, language);
            assert_eq!(
                engine
                    .world
                    .items
                    .iter()
                    .filter(|i| (900..=905).contains(&i.id))
                    .count(),
                6
            );
            engine.actor_mut(PLAYER).unwrap().room_id = 10;
            assert!(
                engine.player_view(PLAYER).unwrap()["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|i| i["id"] == 903)
            );
            let name = engine.name(903);
            finish(
                &mut engine,
                PLAYER,
                json!({"kind":"take","itemId":903,"quantity":1}),
            );
            assert!(
                engine.player_view(PLAYER).unwrap()["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|i| i["id"] != 903)
            );
            assert!(
                !engine.player_view(PLAYER).unwrap()["room"]["description"]
                    .as_str()
                    .unwrap()
                    .contains(&name)
            );
            let mut legacy = json!(engine.world);
            legacy["format"] = json!(4);
            legacy["items"]
                .as_array_mut()
                .unwrap()
                .retain(|item| !(900..=905).contains(&item["id"].as_u64().unwrap()));
            let migrated = migrate_world(legacy).unwrap();
            assert_eq!(migrated.format, 6);
            assert!(migrated.items.iter().all(|i| !(900..=905).contains(&i.id)));
        }
    }
}
#[test]
fn saved_instances_never_localize_by_new_template_id_and_custom_fields_stay_literal() {
    let mut engine = fixture("castle", "en");
    let item = engine.world.items.iter_mut().find(|i| i.id == 900).unwrap();
    item.text_origin = None;
    item.name = "My old object".into();
    item.description = "Literal custom description".into();
    let clothes = engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == 1014)
        .unwrap();
    clothes.name = "My clothes".into();
    engine.save("custom").unwrap();
    engine.load("custom", "pl").unwrap();
    assert_eq!(engine.name(900), "My old object");
    assert_eq!(engine.name(1014), "My clothes");
    own(&mut engine, 1023, PLAYER);
    engine
        .world
        .items
        .iter_mut()
        .find(|i| i.id == 1023)
        .unwrap()
        .quantity = 3;
    let split = engine.world.next_item_id as u16;
    engine.world.manual_pause = false;
    finish(
        &mut engine,
        PLAYER,
        json!({"kind":"drop","itemId":1023,"quantity":1}),
    );
    assert_eq!(engine.name(split), engine.name(1023));
    engine.save("localized").unwrap();
    engine.load("localized", "en").unwrap();
    assert_eq!(engine.name(split), engine.name(1023));
}
#[test]
fn retired_actions_are_absent_from_new_decisions_but_saved_guard_duration_migrates() {
    let mut engine = fixture("castle", "en");
    assert_eq!(
        dispatch(&mut engine, PLAYER, json!({"kind":"guard","seconds":20}))["ok"],
        false
    );
    let actions = engine.actions(PLAYER).unwrap();
    assert!(
        actions.iter().all(|a| !["guard", "disguise", "cut-bonds"]
            .iter()
            .any(|name| a["command"]
                .as_str()
                .unwrap()
                .starts_with(&format!("/{name}"))))
    );
    let mut raw = json!(engine.world);
    raw["format"] = json!(4);
    raw["actors"][0]["activity"] = json!({"intent":{"kind":"guard","seconds":91},"started":engine.world.time,"ends":engine.world.time+91.,"causes":[]});
    let migrated = migrate_world(raw).unwrap();
    assert!(matches!(
        migrated.actors[0].activity.as_ref().unwrap().intent,
        Intent::Wait { seconds: 91 }
    ));
    let wrapper = json!({"format":4,"world":migrated,"memories":[],"events":[]});
    assert_eq!(
        Store::parse_snapshot(&wrapper.to_string()).unwrap().format,
        6
    );
}
#[test]
fn saved_disguise_and_cut_bonds_finish_through_canonical_actions() {
    let mut engine = fixture("castle", "en");
    own(&mut engine, 1014, PLAYER);
    own(&mut engine, 1007, PLAYER);
    for (item, target, mode) in [
        (1014, PLAYER, UseMode::Disguise),
        (1007, 102, UseMode::CutBonds),
    ] {
        if mode == UseMode::CutBonds {
            engine.actor_mut(target).unwrap().flags |= RESTRAINED;
        }
        let intent = Intent::Use {
            item_id: item,
            target_id: target,
            mode,
        };
        assert_eq!(dispatch(&mut engine, PLAYER, json!(intent))["ok"], false);
        engine.actor_mut(PLAYER).unwrap().activity = Some(Activity {
            intent,
            started: engine.world.time,
            ends: engine.world.time + engine.tuning.action_seconds * CLOCK_RATE,
            interaction_ends: Some(engine.world.interaction_time + engine.tuning.action_seconds),
            causes: vec![],
            started_vitals: None,
        });
        engine.save("legacy-use").unwrap();
        engine.load("legacy-use", "en").unwrap();
        engine.world.manual_pause = false;
        engine.advance(engine.tuning.action_seconds + 1e-8).unwrap();
        assert_eq!(
            engine
                .actor(PLAYER)
                .unwrap()
                .last_action_outcome
                .as_ref()
                .unwrap()
                .stage,
            OutcomeStage::Resolved
        );
        if mode == UseMode::Disguise {
            assert!(engine.item(item).unwrap().equipped);
            assert!(engine.actor(PLAYER).unwrap().flags & DISGUISED != 0);
        } else {
            assert_eq!(engine.actor(target).unwrap().flags & RESTRAINED, 0);
            assert!(engine.item(item).is_some());
        }
    }
}
#[test]
fn completion_is_staged_and_quantity_is_a_parameter_not_a_list_of_full_actions() {
    let mut engine = fixture("castle", "pl");
    engine.actor_mut(PLAYER).unwrap().room_id = 10;
    own(&mut engine, 903, PLAYER);
    let view = engine.player_view(PLAYER).unwrap();
    let verbs = suggestions("/g", &view);
    assert_eq!(
        verbs.iter().map(|s| s.command.as_str()).collect::<Vec<_>>(),
        vec!["/go ", "/give "]
    );
    let destinations = suggestions("/go ", &view);
    assert!(destinations.iter().all(|s| s.command.starts_with("/go ")));
    assert!(destinations.iter().any(|s| s.command.contains(' ')));
    let item = engine.name(903);
    let item_choice = suggestions("/give ", &view)
        .into_iter()
        .find(|s| s.command == format!("/give {item} "))
        .unwrap();
    assert_eq!(item_choice.entity_ids, vec![903]);
    assert!(
        suggestions(&item_choice.command, &view)
            .iter()
            .all(|s| s.command.starts_with(&format!("/give {item} to ")))
    );
    let drop = action(&engine, PLAYER, "drop", 903)["command"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(suggestions(&format!("{drop} "), &view).len(), 2);
    for quantity in ["1", "all", "7"] {
        let Operation::Request { params, .. } =
            parse_input(&format!("{drop} {quantity}"), &view).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            params["intent"]["quantity"],
            if quantity == "all" {
                0
            } else {
                quantity.parse::<u8>().unwrap()
            }
        );
    }
    for quantity in ["0", "256", "-1"] {
        assert!(parse_input(&format!("{drop} {quantity}"), &view).is_err());
    }
    assert!(parse_input("/say hello", &view).is_err());
    assert!(parse_input("/experimental listener on", &view).is_err());
    let Operation::Request { params, .. } = parse_input("//go literally spoken", &view).unwrap()
    else {
        panic!()
    };
    assert_eq!(params["text"], "/go literally spoken");
    assert!(
        view["actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["intent"]["kind"] != "eat" || a["intent"]["itemId"] != 904)
    );
}
