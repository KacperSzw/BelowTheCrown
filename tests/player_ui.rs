use anyhow::anyhow;
use below_the_crown::ui::{
    self, Draft, Operation, PlayerUi, PortraitMode, parse_input, suggestions,
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use termwiz::input::{InputEvent, KeyCode, KeyEvent, Modifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn view() -> Value {
    json!({
        "language":"en","branch":"branch-a","roomVisitId":"visit-1","clock":"Day 1 · 18:00","paused":[],
        "room":{"id":1,"name":"Dungeon","description":"Water beads on the stone."},
        "self":{"id":1028,"name":"Wren","role":"Prisoner","health":100,"hunger":35,"thirst":25,"fatigue":20,"activity":"watching","coins":8,"restrained":false},
        "actors":[{"id":102,"name":"Edric","role":"Dungeon guard","activity":"watching","condition":"apparently well"}],
        "listener":{"enabled":false},"events":[],"doors":[],"inventory":[],"map":[],"journal":[],
        "actions":[{"command":"/inspect Edric","label":"Inspect Edric","intent":{"kind":"inspect","targetId":102}}],"combat":null
    })
}
fn key(key: KeyCode) -> InputEvent {
    InputEvent::Key(KeyEvent {
        key,
        modifiers: Modifiers::NONE,
    })
}
fn ctrl(ch: char) -> InputEvent {
    InputEvent::Key(KeyEvent {
        key: KeyCode::Char(ch),
        modifiers: Modifiers::CTRL,
    })
}
fn screen(ui: &mut PlayerUi, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn editor_moves_and_deletes_whole_graphemes() {
    let text = "Aé e\u{301}👨‍👩‍👧‍👦🇵🇱";
    assert_eq!(text.graphemes(true).count(), 6);
    let mut draft = Draft {
        text: text.into(),
        cursor: 5,
    };
    draft.delete(false);
    assert_eq!(
        draft,
        Draft {
            text: "Aé e\u{301}🇵🇱".into(),
            cursor: 4
        }
    );
    let mut draft = Draft {
        text: "e".into(),
        cursor: 1,
    };
    draft.insert("\u{301}");
    assert_eq!(draft.cursor, 1);
    draft.delete(false);
    assert_eq!(draft.text, "");
    let mut draft = Draft {
        text: "first\nsecond\nthird".into(),
        cursor: 9,
    };
    draft.boundary(false);
    assert_eq!(draft.cursor, 6);
    draft.boundary(true);
    assert_eq!(draft.cursor, 12);
}
#[test]
fn paste_is_sanitized_without_losing_lines_and_obeys_limit() {
    assert_eq!(
        ui::clean("\x1b[31mred\x1b[0m\r\nnext\u{202e}line", true),
        "red\nnext line"
    );
    assert_eq!(
        ui::clean("before\x1b]52;c;payload\x07after", false),
        "beforeafter"
    );
    let mut draft = Draft {
        text: "AB".into(),
        cursor: 1,
    };
    draft.insert("first\r\nsecond");
    assert_eq!(
        draft,
        Draft {
            text: "Afirst\nsecondB".into(),
            cursor: 13
        }
    );
    let mut draft = Draft {
        text: "x".repeat(4096),
        cursor: 4096,
    };
    let original = draft.clone();
    draft.insert("y");
    assert_eq!(draft, original);
}
#[test]
fn wrapped_drafts_have_one_cursor_and_respect_cell_widths() {
    for width in [4, 7, 19] {
        for text in [
            "",
            "hello world",
            "one\ntwo",
            "界界🙂🙂",
            "👨‍👩‍👧‍👦 e\u{301} okay",
        ] {
            for cursor in 0..=text.graphemes(true).count() {
                let draft = Draft {
                    text: text.into(),
                    cursor,
                };
                let rows = draft.rows(width);
                assert_eq!(rows.iter().filter(|row| row.cursor.is_some()).count(), 1);
                assert!(rows.iter().all(|row| {
                    UnicodeWidthStr::width(
                        format!(
                            "{}{}{}",
                            row.before,
                            row.cursor.as_deref().unwrap_or(""),
                            row.after
                        )
                        .as_str(),
                    ) <= width
                }));
            }
        }
    }
}
#[test]
fn emphasized_text_uses_plain_input_and_shout_command_is_retired() {
    let mut view = view();
    view["listener"]["enabled"] = json!(true);
    assert_eq!(
        parse_input("  Kill the king!  ", &view).unwrap(),
        Operation::Request {
            method: "input".into(),
            params: json!({"text":"Kill the king!","context":{"roomVisitId":"visit-1"}})
        }
    );
    assert!(suggestions("Kill the king!", &view).is_empty());
    let Operation::Request { method, params } = parse_input("POMOCY!!", &view).unwrap() else {
        panic!()
    };
    assert_eq!(method, "input");
    assert_eq!(params["text"], "POMOCY!!");
    assert_eq!(params["context"]["roomVisitId"], "visit-1");
    for command in [
        "/do attack Edric",
        "/whisper Edric hello",
        "/speed 5",
        "/pause",
        "/save",
        "/new",
        "/shout",
        "/SHOUT Pomocy!",
        &"x".repeat(1201),
    ] {
        assert!(parse_input(command, &view).is_err(), "{command}");
    }
    let menu = suggestions("/", &view);
    assert!(!menu.iter().any(|item| {
        ["/whisper", "/speed", "/pause", "/save", "/do", "/shout"]
            .iter()
            .any(|prefix| item.command.starts_with(prefix))
    }));
}
#[test]
fn physical_choices_and_auto_use_public_view_only() {
    let mut view = view();
    assert_eq!(
        parse_input("/INSPECT  EDRIC", &view).unwrap(),
        Operation::Request {
            method: "act".into(),
            params: json!({"intent":{"kind":"inspect","targetId":102},"context":{"roomVisitId":"visit-1"}})
        }
    );
    for enabled in [false, true] {
        view["listener"]["enabled"] = json!(enabled);
        assert_eq!(
            parse_input("/auto", &view).unwrap(),
            Operation::Request {
                method: "listener".into(),
                params: json!({"enabled":!enabled})
            }
        );
    }
    for enabled in [false, true] {
        let mode = if enabled { "ON" } else { "OFF" };
        assert_eq!(
            parse_input(&format!("/auto {mode}"), &view).unwrap(),
            Operation::Request {
                method: "listener".into(),
                params: json!({"enabled":enabled})
            }
        );
    }
    assert_eq!(suggestions("/inspect", &view).len(), 1);
    assert!(parse_input("/auto maybe", &view).is_err());
}

#[test]
fn chat_marks_own_and_unrecognized_shouts_without_command_echo() {
    for utf8 in [false, true] {
        let mut ui = PlayerUi::new(utf8);
        let mut view = view();
        view["events"] = json!([
            {"id":1,"kind":"activity","actorId":1028,"text":"Beginning an activity in any locale.","details":{"type":"activity_started"}},
            {"id":2,"kind":"speech","actorId":1028,"text":"Wren shouts: “POMOCY!!”","details":{"type":"speech","speakerName":"Wren","volume":"shout"}},
            {"id":3,"kind":"speech","actorId":null,"text":"A distant voice shouts: “H…LP”","details":{"type":"speech","speakerName":null,"volume":"shout"}},
            {"id":4,"kind":"action","actorId":102,"text":"Edric hits Wren: −7 HP."}
        ]);
        ui.set_view(view);
        ui.draft.set("/inspect Edric".into());
        let submission = ui.input(key(KeyCode::Enter)).unwrap();
        ui.finish(&submission, Ok(json!({"ok":true})));
        let text = screen(&mut ui, 100, 35);
        assert!(
            text.contains(&format!(
                "{} You: “POMOCY!!”",
                if utf8 { "»" } else { ">>" }
            )),
            "{text}"
        );
        assert!(text.contains("A distant voice: “H…LP”"), "{text}");
        assert!(text.contains("· Edric hits Wren: −7 HP."));
        assert!(!text.contains("Beginning an activity"));
        assert!(!text.contains("/inspect Edric"));
        ui.input(key(KeyCode::UpArrow));
        assert_eq!(ui.draft.text, "/inspect Edric");
        ui.draft.set("ŻÓŁĆ!!".into());
        assert!(screen(&mut ui, 100, 35).contains("Shout · Enter sends"));
    }
}
#[test]
fn submissions_capture_context_preserve_failed_drafts_and_keep_new_typing() {
    let mut ui = PlayerUi::new(true);
    ui.set_view(view());
    ui.draft.set("Open the door".into());
    let submission = ui.input(key(KeyCode::Enter)).unwrap();
    let Operation::Request { params, .. } = &submission.operation else {
        panic!()
    };
    assert_eq!(params["context"]["roomVisitId"], "visit-1");
    let mut next = view();
    next["roomVisitId"] = json!("visit-2");
    ui.set_view(next);
    ui.finish(&submission, Err(anyhow!("Visit changed")));
    assert_eq!(ui.draft.text, "Open the door");
    let submission = ui.input(key(KeyCode::Enter)).unwrap();
    ui.input(key(KeyCode::Char('!')));
    ui.finish(&submission, Ok(json!({"ok":true})));
    assert_eq!(ui.draft.text, "Open the door!");
    let submission = ui.input(key(KeyCode::Enter)).unwrap();
    ui.finish(&submission, Ok(json!({"ok":true})));
    assert_eq!(ui.draft.text, "");
    ui.input(key(KeyCode::UpArrow));
    assert_eq!(ui.draft.text, "Open the door!");
}
#[test]
fn chat_scroll_keeps_room_and_people_pinned_and_new_visits_clear_transcript() {
    let mut ui = PlayerUi::new(false);
    let mut first = view();
    first["events"] = json!(
        (1..=40)
            .map(
                |n| json!({"id":n,"kind":"speech","actorId":102,"text":format!("Edric: line {n}")})
            )
            .collect::<Vec<_>>()
    );
    ui.set_view(first.clone());
    let live = screen(&mut ui, 80, 24);
    assert!(live.contains("Dungeon"));
    assert!(live.contains("People in the room · 2"));
    assert!(live.contains("Wren"));
    assert!(live.contains("Edric"));
    assert!(live.contains("line 40"));
    ui.scroll(-1);
    let browsed = screen(&mut ui, 80, 24);
    assert_eq!(
        live.lines().take(4).collect::<Vec<_>>(),
        browsed.lines().take(4).collect::<Vec<_>>()
    );
    assert!(!browsed.contains("line 40"));
    first["events"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":41,"kind":"speech","actorId":102,"text":"Edric: fresh words"}));
    ui.set_view(first);
    assert_eq!(ui.unread, 1);
    ui.live();
    assert!(screen(&mut ui, 80, 24).contains("fresh words"));
    let mut next = view();
    next["roomVisitId"] = json!("visit-2");
    ui.set_view(next);
    let next_screen = screen(&mut ui, 80, 24);
    assert!(!next_screen.contains("line 40"));
    assert!(!next_screen.contains("fresh words"));
}
#[test]
fn roster_deduplicates_self_and_small_terminals_keep_editor() {
    let mut ui = PlayerUi::new(true);
    let mut view = view();
    let own = view["self"].clone();
    view["actors"].as_array_mut().unwrap().push(own);
    ui.set_view(view);
    ui.draft.set("draft survives".into());
    for (width, height) in [(20, 8), (40, 12), (80, 24), (140, 40)] {
        let screen = screen(&mut ui, width, height);
        assert!(screen.contains("Dungeon"));
        assert!(screen.contains("People in the room") || screen.contains("People in the roo"));
        assert!(screen.contains("draft survives"));
    }
    let text = screen(&mut ui, 100, 32);
    assert!(text.contains("People in the room · 2"));
    ui.portraits = PortraitMode::Off;
    assert!(
        !screen(&mut ui, 100, 32)
            .chars()
            .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
    );
}
#[test]
fn combat_does_not_swallow_existing_draft_or_submit_wrong_round() {
    let mut ui = PlayerUi::new(false);
    ui.set_view(view());
    ui.draft.set("Do not hurt me".into());
    let mut fighting = view();
    fighting["combat"] = json!({"encounterId":"fight-a","round":3,"remainingSeconds":15,"choices":[{"label":"Defend","intent":{"kind":"defend"}}]});
    ui.set_view(fighting.clone());
    assert_eq!(ui.draft.text, "Do not hurt me");
    let speech = ui.input(key(KeyCode::Enter)).unwrap();
    let Operation::Request { params, .. } = &speech.operation else {
        panic!()
    };
    assert_eq!(
        params["context"]["combat"],
        json!({"encounterId":"fight-a","round":3})
    );
    ui.finish(&speech, Err(anyhow!("Try again")));
    ui.input(key(KeyCode::Tab));
    let choice = ui.input(key(KeyCode::Enter)).unwrap();
    let Operation::Request { params, .. } = choice.operation else {
        panic!()
    };
    assert_eq!(params["combat"], json!({"encounterId":"fight-a","round":3}));
    assert_eq!(params["intent"]["kind"], "defend");
    assert_eq!(ui.draft.text, "Do not hurt me");
    fighting["combat"]["round"] = json!(4);
    ui.set_view(fighting);
    assert_eq!(ui.draft.text, "Do not hurt me");
}
#[test]
fn pasted_lines_wait_for_explicit_enter_and_ctrl_c_quits() {
    let mut ui = PlayerUi::new(true);
    ui.set_view(view());
    assert!(
        ui.input(InputEvent::Paste("First\nsecond\x1b[31m".into()))
            .is_none()
    );
    assert_eq!(ui.draft.text, "First\nsecond");
    ui.input(ctrl('j'));
    assert_eq!(ui.draft.text, "First\nsecond\n");
    assert_eq!(ui.input(ctrl('c')).unwrap().operation, Operation::Quit);
}

#[test]
fn command_picker_wraps_upward_and_downward() {
    let view = view();
    let choices = suggestions("/", &view);
    assert!(choices.len() > 2);
    let mut ui = PlayerUi::new(false);
    ui.set_view(view.clone());
    ui.draft.set("/".into());
    ui.input(key(KeyCode::UpArrow));
    ui.input(key(KeyCode::Tab));
    assert_eq!(
        ui.draft.text,
        choices.last().unwrap().command,
        "Up from the first choice reaches the last"
    );
    let mut ui = PlayerUi::new(false);
    ui.set_view(view);
    ui.draft.set("/".into());
    for _ in 0..choices.len() {
        ui.input(key(KeyCode::DownArrow));
    }
    ui.input(key(KeyCode::Tab));
    assert_eq!(
        ui.draft.text, choices[0].command,
        "Down from the last reaches the first"
    );
}

#[test]
fn dotted_title_fits_cell_bounds() {
    let full = ui::title(120, 24, true);
    assert!(
        full.iter()
            .flat_map(|row| row.chars())
            .any(|ch| ('\u{2801}'..='\u{28ff}').contains(&ch))
    );
    assert!(
        full.iter()
            .flat_map(|row| row.chars())
            .all(|ch| ch == ' ' || ('\u{2800}'..='\u{28ff}').contains(&ch))
    );
    for width in [0, 1, 4, 9, 20, 24, 28, 40, 68, 80, 120] {
        for rows in [0, 1, 2, 3, 4, 8, 12, 24] {
            for braille in [false, true] {
                let title = ui::title(width, rows, braille);
                assert!(title.len() <= rows);
                assert!(
                    title
                        .iter()
                        .all(|row| UnicodeWidthStr::width(row.as_str()) <= width)
                );
                if !braille {
                    assert!(title.iter().all(|row| row.is_ascii()));
                }
            }
        }
    }
    for (width, rows, braille) in [(80, 3, true), (80, 20, false), (9, 2, false)] {
        assert_eq!(
            ui::title(width, rows, braille)
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["BELOW", "THE", "CROWN"]
        );
    }
    assert!(
        ui::title(28, 20, true).len() > full.len(),
        "Narrow artwork stacks complete words"
    );
}

#[test]
fn joining_keeps_name_and_error_visible_with_title_and_resize() {
    let draft = Draft {
        text: "Wren".into(),
        cursor: 4,
    };
    for braille in [false, true] {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        for (width, height) in [(120, 40), (40, 18), (20, 8), (80, 24)] {
            terminal.backend_mut().resize(width, height);
            terminal
                .resize(ratatui::layout::Rect::new(0, 0, width, height))
                .unwrap();
            terminal
                .draw(|frame| ui::render_join(frame, &draft, "en", Some("Test error"), braille))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            assert!(row(height - 1).contains("Wren"));
            assert!(row(height - 2).contains("Your name"));
            assert!(row(height - 3).contains("Test error"));
            if !braille {
                assert!(
                    !(0..height)
                        .flat_map(|y| row(y).chars().collect::<Vec<_>>())
                        .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
                );
            }
        }
    }
}

fn door_menu_view() -> Value {
    let mut view = view();
    view["actions"] = json!([
        {"command":"/go Corridor","label":"go Corridor","intent":{"kind":"move","doorId":201},"available":false,"status":"locked","hint":"locked","reason":"The door is locked. Unlock it first."},
        {"command":"/go Kitchen","label":"go Kitchen","intent":{"kind":"move","doorId":202},"available":false,"status":"closed","hint":"closed"},
        {"command":"/go Courtyard","label":"go Courtyard","intent":{"kind":"move","doorId":203},"available":true,"status":"open","hint":"open"}
    ]);
    view["doors"] = json!([
        {"id":201,"name":"Iron door","destination":"Corridor","open":false,"locked":true},
        {"id":202,"name":"Wooden door","destination":"Kitchen","open":false,"locked":false},
        {"id":203,"name":"Archway","destination":"Courtyard","open":true,"locked":false}
    ]);
    view
}

#[test]
fn blocked_exits_are_discoverable_and_completion_submits_only_the_command() {
    let view = door_menu_view();
    let choices = suggestions("/go ", &view);
    assert_eq!(choices.len(), 3);
    assert_eq!(choices[0].status, "locked");
    let mut ui = PlayerUi::new(false);
    ui.set_view(view);
    ui.draft.set("/go ".into());
    ui.input(key(KeyCode::Tab));
    assert_eq!(ui.draft.text, "/go Corridor");
    let submission = ui.input(key(KeyCode::Enter)).unwrap();
    let Operation::Request { method, params } = &submission.operation else {
        panic!()
    };
    assert_eq!(method, "act");
    assert_eq!(params["intent"], json!({"kind":"move","doorId":201}));
    assert_eq!(params["context"], json!({"roomVisitId":"visit-1"}));
    ui.finish(
        &submission,
        Ok(json!({"ok":false,"message":"The door is locked. Unlock it first."})),
    );
    assert_eq!(ui.draft.text, "/go Corridor");
    assert!(screen(&mut ui, 80, 24).contains("Unlock it first."));
}

#[test]
fn door_states_use_distinct_colors_in_suggestions_and_look() {
    for look in [false, true] {
        let mut ui = PlayerUi::new(false);
        ui.set_view(door_menu_view());
        ui.draft.set(if look { "/look" } else { "/go " }.into());
        if look {
            ui.input(key(KeyCode::Enter));
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        for (destination, status, color) in [
            ("Corridor", "locked", ui::BLOOD),
            ("Kitchen", "closed", ui::AMBER),
            ("Courtyard", "open", ui::SAGE),
        ] {
            let (y, line) = (0..30)
                .map(|y| {
                    (
                        y,
                        (0..100)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>(),
                    )
                })
                .find(|(_, line)| line.contains(destination) && line.contains(status))
                .unwrap();
            let x = UnicodeWidthStr::width(&line[..line.rfind(status).unwrap()]) as u16;
            assert_eq!(buffer[(x, y)].fg, color);
            if !look {
                let x = UnicodeWidthStr::width(&line[..line.find("/go").unwrap()]) as u16;
                assert_eq!(buffer[(x, y)].fg, ui::BLUE);
            }
        }
    }
}

#[test]
fn refreshed_door_state_changes_the_hint_without_losing_the_draft() {
    let mut ui = PlayerUi::new(false);
    let mut view = door_menu_view();
    ui.set_view(view.clone());
    ui.draft.set("/go ".into());
    view["actions"][0]["available"] = json!(true);
    view["actions"][0]["status"] = json!("open");
    view["actions"][0]["hint"] = json!("open");
    ui.set_view(view.clone());
    assert_eq!(ui.draft.text, "/go ");
    assert_eq!(suggestions("/go ", &view)[0].status, "open");
}

#[test]
fn selected_item_is_not_silently_retargeted_after_a_view_refresh() {
    let mut initial = view();
    initial["actions"] = json!([{"command":"/take Bread","completionCommand":"/take Bread","arguments":[{"value":"Bread","prefix":"","entityId":903}],"intent":{"kind":"take","itemId":903,"quantity":1},"quantityAvailable":3,"forecast":{"durationRealSeconds":0.3}}]);
    let mut ui = PlayerUi::new(false);
    ui.set_view(initial.clone());
    ui.draft.set("/take ".into());
    ui.input(key(KeyCode::Tab));
    assert_eq!(ui.draft.text, "/take Bread");
    initial["actions"][0]["intent"]["itemId"] = json!(1030);
    initial["actions"][0]["arguments"][0]["entityId"] = json!(1030);
    ui.set_view(initial);
    assert!(ui.input(key(KeyCode::Enter)).is_none());
    assert!(!ui.sending);
    assert!(screen(&mut ui, 100, 30).contains("selected object is no longer available"));
}
#[test]
fn room_hp_is_visible_with_and_without_portraits_and_look_lists_overflow() {
    let mut value = view();
    value["actors"][0]["health"] = json!(73.5);
    let mut portrait_ui = PlayerUi::new(true);
    portrait_ui.portraits = PortraitMode::On;
    portrait_ui.set_view(value.clone());
    let portrait_rendered = screen(&mut portrait_ui, 160, 48);
    assert!(portrait_rendered.contains("HP 73.5"), "{portrait_rendered}");
    value["actors"].as_array_mut().unwrap().extend((200..212).map(|id|json!({"id":id,"name":format!("Guest{id}"),"role":"Visitor","health":id as f64/10.,"activity":"watching"})));
    for mode in [PortraitMode::On, PortraitMode::Off] {
        let mut ui = PlayerUi::new(true);
        ui.portraits = mode;
        ui.set_view(value.clone());
        ui.draft.set("/look".into());
        ui.input(key(KeyCode::Enter));
        let rendered = screen(&mut ui, 160, 48);
        assert!(rendered.contains("HP 73.5"), "{rendered}");
        assert!(rendered.contains("Guest211"), "{rendered}");
    }
    value["actors"] =
        json!([{"id":102,"name":"Edric","role":"Guard","health":0.005,"activity":"watching"}]);
    let mut ui = PlayerUi::new(false);
    ui.set_view(value);
    assert!(screen(&mut ui, 80, 24).contains("HP <0.01"));
}
#[test]
fn enter_completes_required_arguments_and_default_quantity_then_submits() {
    let mut value = view();
    value["actions"] = json!([{"command":"/take Bread","completionCommand":"/take Bread","arguments":[{"value":"Bread","prefix":"","entityId":903}],"intent":{"kind":"take","itemId":903,"quantity":1},"quantityAvailable":3}]);
    let mut ui = PlayerUi::new(false);
    ui.set_view(value);
    ui.draft.set("/ta".into());
    assert!(ui.input(key(KeyCode::Enter)).is_none());
    assert_eq!(ui.draft.text, "/take ");
    assert!(ui.input(key(KeyCode::Enter)).is_none());
    assert_eq!(ui.draft.text, "/take Bread");
    ui.input(key(KeyCode::Char(' ')));
    assert!(ui.input(key(KeyCode::Enter)).is_none());
    assert_eq!(ui.draft.text, "/take Bread 1");
    let submission = ui.input(key(KeyCode::Enter)).unwrap();
    let Operation::Request { params, .. } = submission.operation else {
        panic!()
    };
    assert_eq!(params["intent"]["quantity"], 1);
}
