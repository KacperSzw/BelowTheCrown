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
fn plain_text_says_to_room_and_shout_is_explicit() {
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
    let Operation::Request { method, params } = parse_input("/SHOUT Pomocy!", &view).unwrap()
    else {
        panic!()
    };
    assert_eq!(method, "act");
    assert_eq!(params["intent"]["volume"], "shout");
    assert_eq!(params["intent"]["text"], "Pomocy!");
    assert_eq!(params["context"]["roomVisitId"], "visit-1");
    for command in [
        "/do attack Edric",
        "/whisper Edric hello",
        "/speed 5",
        "/pause",
        "/save",
        "/new",
        "/shout",
        &"x".repeat(1201),
    ] {
        assert!(parse_input(command, &view).is_err(), "{command}");
    }
    let menu = suggestions("/", &view);
    assert!(!menu.iter().any(|item| {
        ["/whisper", "/speed", "/pause", "/save", "/do"]
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
            params: json!({"intent":{"kind":"inspect","targetId":102}})
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
