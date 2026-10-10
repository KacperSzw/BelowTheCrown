//! Player terminal state. Rendering is independent of stdin and terminal size.
//! One instance belongs to one SSH PTY; world state arrives only as public views.
use anyhow::{Result, anyhow};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use termwiz::input::{InputEvent, KeyCode, Modifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const ASH: Color = Color::Rgb(160, 156, 147);
pub const AMBER: Color = Color::Rgb(217, 183, 121);
pub const PALE: Color = Color::Rgb(222, 215, 201);
pub const BLOOD: Color = Color::Rgb(215, 145, 137);
pub const BLUE: Color = Color::Rgb(169, 203, 213);
pub const SAGE: Color = Color::Rgb(181, 197, 154);
pub const LILAC: Color = Color::Rgb(181, 167, 201);
pub const IVORY: Color = Color::Rgb(244, 236, 220);
const SELECTED: Color = Color::Rgb(48, 41, 31);
const VOICES: [Color; 16] = [
    BLUE,
    Color::Rgb(196, 183, 219),
    Color::Rgb(184, 205, 169),
    Color::Rgb(228, 184, 200),
    Color::Rgb(184, 198, 228),
    Color::Rgb(198, 201, 151),
    Color::Rgb(220, 193, 167),
    Color::Rgb(169, 211, 199),
    Color::Rgb(216, 188, 221),
    Color::Rgb(195, 212, 208),
    Color::Rgb(221, 197, 174),
    Color::Rgb(195, 197, 222),
    Color::Rgb(193, 217, 185),
    Color::Rgb(226, 188, 177),
    Color::Rgb(180, 212, 222),
    Color::Rgb(216, 202, 180),
];

pub fn actor_color(id: u16, self_id: u16) -> Color {
    if id == self_id {
        IVORY
    } else if id == 103 {
        AMBER
    } else {
        VOICES[(usize::from(id) * 5) % VOICES.len()]
    }
}

/// Remove escape sequences, control characters and bidirectional overrides.
/// Preserve explicit newlines only in drafts, never inside a rendered line.
pub fn clean(text: &str, newlines: bool) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if ch == '\r' && newlines {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else if ch == '\n' && newlines {
            out.push(ch);
        } else if ch.is_control()
            || ('\u{202a}'..='\u{202e}').contains(&ch)
            || ('\u{2066}'..='\u{2069}').contains(&ch)
        {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    clean(text, false)
        .graphemes(true)
        .take_while(|g| {
            let cells = UnicodeWidthStr::width(*g);
            if used + cells <= width {
                used += cells;
                true
            } else {
                false
            }
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    pub cursor: usize,
}
impl Draft {
    pub fn set(&mut self, text: String) {
        self.cursor = text.graphemes(true).count();
        self.text = text;
    }
    pub fn insert(&mut self, text: &str) {
        let units: Vec<_> = self.text.graphemes(true).collect();
        let offset = units
            .iter()
            .take(self.cursor)
            .map(|g| g.len())
            .sum::<usize>();
        let added = clean(text, true);
        let mut next = self.text.clone();
        next.insert_str(offset, &added);
        if next.chars().count() > 4096 {
            return;
        }
        self.cursor = next[..offset + added.len()].graphemes(true).count();
        self.text = next;
        self.cursor = self.cursor.min(self.text.graphemes(true).count());
    }
    pub fn delete(&mut self, forward: bool) {
        let mut units: Vec<_> = self.text.graphemes(true).map(str::to_owned).collect();
        let Some(index) = (if forward {
            Some(self.cursor)
        } else {
            self.cursor.checked_sub(1)
        }) else {
            return;
        };
        if index >= units.len() {
            return;
        }
        units.remove(index);
        self.text = units.concat();
        if !forward {
            self.cursor -= 1;
        }
        self.cursor = self.cursor.min(self.text.graphemes(true).count());
    }
    pub fn boundary(&mut self, end: bool) {
        let units: Vec<_> = self.text.graphemes(true).collect();
        if end {
            while self.cursor < units.len() && units[self.cursor] != "\n" {
                self.cursor += 1;
            }
        } else {
            while self.cursor > 0 && units[self.cursor - 1] != "\n" {
                self.cursor -= 1;
            }
        }
    }
    pub fn rows(&self, width: usize) -> Vec<DraftRow> {
        let units: Vec<_> = self.text.graphemes(true).collect();
        let mut rows = Vec::new();
        let mut row = DraftRow::default();
        let mut cells = 0;
        for index in 0..=units.len() {
            let cursor = index == self.cursor;
            if index == units.len() && !cursor {
                break;
            }
            let unit = units.get(index).copied().unwrap_or(" ");
            if unit == "\n" {
                if cursor {
                    row.cursor = Some(" ".into());
                }
                rows.push(row);
                row = DraftRow::default();
                cells = 0;
                continue;
            }
            let size = UnicodeWidthStr::width(unit);
            if cells + size > width && cells > 0 {
                rows.push(row);
                row = DraftRow::default();
                cells = 0;
            }
            if cursor {
                row.cursor = Some(unit.to_owned());
            } else if row.cursor.is_some() {
                row.after.push_str(unit);
            } else {
                row.before.push_str(unit);
            }
            cells += size;
        }
        rows.push(row);
        rows
    }
}
#[derive(Debug, Default)]
pub struct DraftRow {
    pub before: String,
    pub cursor: Option<String>,
    pub after: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PortraitMode {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Operation {
    Request { method: String, params: Value },
    Local(String),
    Portraits(PortraitMode),
    Quit,
}
#[derive(Clone, Debug)]
pub struct Submission {
    pub operation: Operation,
    pub text: String,
}
#[derive(Clone, Debug)]
pub struct Suggestion {
    pub command: String,
    pub description: String,
    pub status: String,
}

fn door_color(status: &str) -> Color {
    match status {
        "open" | "ready" => SAGE,
        "closed" | "close_first" => AMBER,
        "locked" | "needs_key" | "blocked" => BLOOD,
        _ => ASH,
    }
}

pub fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn number(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}
fn id(value: &Value) -> u16 {
    number(value, "id") as u16
}
fn translated(view: &Value, source: &str) -> String {
    crate::localization::tr(
        if field(view, "language") == "pl" {
            "pl"
        } else {
            "en"
        },
        source,
        &[],
    )
}
fn combat_context(view: &Value) -> Option<Value> {
    view.get("combat")
        .filter(|c| c.is_object())
        .map(|c| json!({"encounterId":c["encounterId"],"round":c["round"]}))
}
fn context(view: &Value) -> Value {
    let mut context = json!({"roomVisitId":view["roomVisitId"]});
    if let Some(combat) = combat_context(view) {
        context["combat"] = combat;
    }
    context
}

pub fn parse_input(input: &str, view: &Value) -> Result<Operation> {
    let text = input.trim();
    if text.is_empty() {
        return Err(anyhow!(translated(
            view,
            "Write a message or choose a command."
        )));
    }
    if !text.starts_with('/') {
        if text.chars().count() > 1200 {
            return Err(anyhow!(translated(
                view,
                "Messages can contain at most 1200 characters."
            )));
        }
        return Ok(Operation::Request {
            method: "input".into(),
            params: json!({"text":text,"context":context(view)}),
        });
    }
    let (command, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let command = command.to_lowercase();
    let args = args.trim();
    if ["/pause", "/resume", "/save", "/load", "/new", "/speed"].contains(&command.as_str()) {
        return Err(anyhow!(translated(
            view,
            "That control belongs to the host manager."
        )));
    }
    if command == "/whisper" {
        return Err(anyhow!(translated(view, "Speech stays in this room.")));
    }
    for action in list(view, "actions") {
        if normalized(field(action, "command")) == normalized(text) {
            let mut params = json!({"intent":action["intent"],"context":context(view)});
            if matches!(
                field(&action["intent"], "kind"),
                "attack" | "defend" | "guard" | "flee" | "move"
            ) && let Some(combat) = combat_context(view)
            {
                params["combat"] = combat;
            }
            return Ok(Operation::Request {
                method: "act".into(),
                params,
            });
        }
    }
    match command.as_str() {
        "/say" => {
            if args.is_empty() || args.chars().count() > 1200 {
                return Err(anyhow!(translated(view, "Write a message.")));
            }
            Ok(Operation::Request {
                method: "input".into(),
                params: json!({"text":args,"context":context(view)}),
            })
        }
        "/shout" => {
            if args.is_empty() || args.chars().count() > 1200 {
                return Err(anyhow!(translated(view, "Write a message.")));
            }
            Ok(Operation::Request {
                method: "act".into(),
                params: json!({"intent":{"kind":"speak","text":args,"targetId":null,"volume":"shout","plot":null},"context":context(view)}),
            })
        }
        "/look" | "/inventory" | "/map" | "/journal" | "/help" if args.is_empty() => {
            Ok(Operation::Local(command[1..].into()))
        }
        "/quit" if args.is_empty() => Ok(Operation::Quit),
        "/cancel" if args.is_empty() => Ok(Operation::Request {
            method: "cancel".into(),
            params: json!({}),
        }),
        "/auto"
            if args.is_empty()
                || args.eq_ignore_ascii_case("on")
                || args.eq_ignore_ascii_case("off") =>
        {
            let enabled = if args.is_empty() {
                !view["listener"]["enabled"].as_bool().unwrap_or(false)
            } else {
                args.eq_ignore_ascii_case("on")
            };
            Ok(Operation::Request {
                method: "listener".into(),
                params: json!({"enabled":enabled}),
            })
        }
        "/experimental"
            if args.eq_ignore_ascii_case("listener on")
                || args.eq_ignore_ascii_case("listener off") =>
        {
            Ok(Operation::Request {
                method: "listener".into(),
                params: json!({"enabled":args.to_lowercase().ends_with("on")}),
            })
        }
        "/portraits" if ["auto", "on", "off"].contains(&args.to_lowercase().as_str()) => {
            Ok(Operation::Portraits(match args.to_lowercase().as_str() {
                "on" => PortraitMode::On,
                "off" => PortraitMode::Off,
                _ => PortraitMode::Auto,
            }))
        }
        _ => Err(anyhow!(translated(
            view,
            "That command is incomplete or unavailable. Use /help or Tab for suggestions."
        ))),
    }
}

pub fn suggestions(input: &str, view: &Value) -> Vec<Suggestion> {
    if !input.trim_start().starts_with('/') {
        return Vec::new();
    }
    let prefix = normalized(input);
    let mut choices = vec![
        ("/say ", "Speak to the room"),
        ("/shout ", "Shout to adjoining rooms"),
        ("/look", "Your surroundings"),
        ("/inventory", "Your belongings"),
        ("/map", "Known rooms"),
        ("/journal", "Your memories"),
        ("/help", "Commands and controls"),
        (
            "/cancel",
            "Cancel queued interpretations and remaining steps",
        ),
        ("/auto", "Toggle actions inferred from your speech"),
        ("/auto on", "Turn speech actions on"),
        ("/auto off", "Turn speech actions off"),
        ("/portraits auto", "Detect portrait support"),
        ("/portraits on", "Show portraits"),
        ("/portraits off", "Hide portraits"),
        ("/quit", "Leave the castle"),
    ]
    .into_iter()
    .map(|(command, description)| Suggestion {
        command: command.into(),
        description: translated(view, description),
        status: String::new(),
    })
    .collect::<Vec<_>>();
    for action in list(view, "actions") {
        choices.push(Suggestion {
            command: field(action, "command").into(),
            status: field(action, "status").into(),
            description: field(action, "hint").to_owned().chars().next().map_or_else(
                || field(action, "label").into(),
                |_| field(action, "hint").into(),
            ),
        });
    }
    let mut seen = std::collections::HashSet::new();
    choices.retain(|choice| {
        let normalized = normalized(&choice.command);
        normalized.starts_with(&prefix) && normalized != prefix && seen.insert(normalized)
    });
    choices
}

#[derive(Clone)]
struct Block {
    id: u64,
    text: String,
    color: Color,
}
#[derive(Clone)]
struct ChatRow {
    block: u64,
    offset: usize,
    text: String,
    color: Color,
}
#[derive(Clone, Copy)]
struct Anchor {
    block: u64,
    offset: usize,
}

pub struct PlayerUi {
    pub view: Value,
    pub draft: Draft,
    pub portraits: PortraitMode,
    pub sending: bool,
    pub unread: usize,
    pub error: Option<String>,
    pub utf8: bool,
    blocks: VecDeque<Block>,
    next_block: u64,
    branch: String,
    visit: String,
    event_cursor: u64,
    anchor: Option<Anchor>,
    viewport_width: usize,
    viewport_height: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Draft,
    selection: usize,
    dismissed: bool,
    combat_focus: bool,
    combat_selection: usize,
    combat_round: String,
}
impl PlayerUi {
    pub fn new(utf8: bool) -> Self {
        Self {
            view: Value::Null,
            draft: Draft::default(),
            portraits: PortraitMode::Auto,
            sending: false,
            unread: 0,
            error: None,
            utf8,
            blocks: VecDeque::new(),
            next_block: 0,
            branch: String::new(),
            visit: String::new(),
            event_cursor: 0,
            anchor: None,
            viewport_width: 76,
            viewport_height: 12,
            history: Vec::new(),
            history_index: None,
            history_draft: Draft::default(),
            selection: 0,
            dismissed: false,
            combat_focus: false,
            combat_selection: 0,
            combat_round: String::new(),
        }
    }
    pub fn set_view(&mut self, view: Value) {
        let branch = field(&view, "branch").to_owned();
        let visit = field(&view, "roomVisitId").to_owned();
        if branch != self.branch || visit != self.visit {
            self.blocks.clear();
            self.anchor = None;
            self.unread = 0;
            if branch != self.branch {
                self.event_cursor = 0;
            }
        }
        self.branch = branch;
        self.visit = visit;
        let round = view
            .get("combat")
            .filter(|c| c.is_object())
            .map(|c| format!("{}:{}", field(c, "encounterId"), c["round"]))
            .unwrap_or_default();
        if round != self.combat_round {
            self.combat_focus = !round.is_empty() && self.draft.text.trim().is_empty();
            self.combat_selection = 0;
            self.combat_round = round;
        }
        let self_id = id(&view["self"]);
        for event in list(&view, "events") {
            let event_id = event["id"].as_u64().unwrap_or(0);
            if event_id <= self.event_cursor {
                continue;
            }
            self.event_cursor = event_id;
            let text = field(event, "text");
            if text.starts_with("You begin to ")
                || text.starts_with("Zaczynasz ")
                || text.starts_with("Damp stone. A guttering torch.")
                || text.starts_with("Wilgotny kamień. Dogasająca pochodnia.")
            {
                continue;
            }
            let actor = number(event, "actorId") as u16;
            let color = match field(event, "kind") {
                "speech" => actor_color(actor, self_id),
                "action" => AMBER,
                "injury" | "death" => BLOOD,
                "notice" => SAGE,
                _ => ASH,
            };
            let text = if field(event, "kind") == "speech" && actor == self_id {
                text.split_once(": ").map_or_else(
                    || text.to_owned(),
                    |(_, speech)| format!("{}: {speech}", translated(&view, "You")),
                )
            } else {
                text.into()
            };
            self.write(&text, color);
        }
        self.view = view;
    }
    pub fn write(&mut self, text: &str, color: Color) {
        for line in text.split('\n') {
            self.next_block += 1;
            self.blocks.push_back(Block {
                id: self.next_block,
                text: clean(line, false),
                color,
            });
        }
        while self.blocks.len() > 2000 {
            self.blocks.pop_front();
        }
        if self.anchor.is_some() {
            self.unread += 1;
        }
    }
    fn rows(&self, width: usize) -> Vec<ChatRow> {
        let mut rows = Vec::new();
        let width = width.max(1);
        for block in &self.blocks {
            let units: Vec<_> = block.text.graphemes(true).collect();
            let mut start = 0;
            if units.is_empty() {
                rows.push(ChatRow {
                    block: block.id,
                    offset: 0,
                    text: String::new(),
                    color: block.color,
                });
            }
            while start < units.len() {
                let mut end = start;
                let mut cells = 0;
                let mut break_at = None;
                while end < units.len() {
                    let size = UnicodeWidthStr::width(units[end]);
                    if cells + size > width && end > start {
                        break;
                    }
                    cells += size;
                    if units[end].chars().all(char::is_whitespace) {
                        break_at = Some(end);
                    }
                    end += 1;
                }
                if end < units.len()
                    && let Some(space) = break_at.filter(|space| *space > start)
                {
                    end = space + 1;
                }
                rows.push(ChatRow {
                    block: block.id,
                    offset: start,
                    text: units[start..end].concat().trim_end().into(),
                    color: block.color,
                });
                start = end;
            }
        }
        rows
    }
    fn start(&self, rows: &[ChatRow], height: usize) -> usize {
        self.anchor
            .and_then(|a| {
                rows.iter()
                    .rposition(|r| r.block == a.block && r.offset <= a.offset)
            })
            .unwrap_or_else(|| {
                if self.anchor.is_some() {
                    0
                } else {
                    rows.len().saturating_sub(height)
                }
            })
    }
    pub fn scroll(&mut self, direction: i32) {
        let rows = self.rows(self.viewport_width);
        let last = rows.len().saturating_sub(self.viewport_height);
        let current = self.start(&rows, self.viewport_height);
        let next = current
            .saturating_add_signed(
                direction as isize * self.viewport_height.saturating_sub(1).max(1) as isize,
            )
            .min(last);
        self.anchor = if next < last {
            rows.get(next).map(|r| Anchor {
                block: r.block,
                offset: r.offset,
            })
        } else {
            None
        };
        if self.anchor.is_none() {
            self.unread = 0;
        }
    }
    pub fn live(&mut self) {
        self.anchor = None;
        self.unread = 0;
    }
    fn change(&mut self) {
        self.selection = 0;
        self.dismissed = false;
        self.history_index = None;
        self.combat_focus = false;
    }
    pub fn input(&mut self, event: InputEvent) -> Option<Submission> {
        if let InputEvent::Paste(text) = event {
            self.draft.insert(&text);
            self.change();
            return None;
        }
        let InputEvent::Key(key) = event else {
            return None;
        };
        let ctrl = key.modifiers.contains(Modifiers::CTRL);
        let shift = key.modifiers.contains(Modifiers::SHIFT);
        if ctrl && matches!(key.key, KeyCode::Char('c') | KeyCode::Char('C')) {
            return Some(Submission {
                operation: Operation::Quit,
                text: String::new(),
            });
        }
        match key.key {
            KeyCode::PageUp => {
                self.scroll(-1);
                return None;
            }
            KeyCode::PageDown => {
                self.scroll(1);
                return None;
            }
            KeyCode::End if ctrl => {
                self.live();
                return None;
            }
            KeyCode::Enter if shift => {
                self.draft.insert("\n");
                self.change();
                return None;
            }
            KeyCode::Char('j') if ctrl => {
                self.draft.insert("\n");
                self.change();
                return None;
            }
            KeyCode::Char('u') if ctrl => {
                self.draft = Draft::default();
                self.change();
                return None;
            }
            _ => {}
        }
        let combat_choices = list(&self.view["combat"], "choices").to_vec();
        if !combat_choices.is_empty() && key.key == KeyCode::Tab {
            self.combat_focus = !self.combat_focus;
            self.dismissed = true;
            return None;
        }
        if self.combat_focus && !combat_choices.is_empty() {
            match key.key {
                KeyCode::UpArrow => {
                    self.combat_selection =
                        (self.combat_selection + combat_choices.len() - 1) % combat_choices.len();
                    return None;
                }
                KeyCode::DownArrow => {
                    self.combat_selection = (self.combat_selection + 1) % combat_choices.len();
                    return None;
                }
                KeyCode::Escape => {
                    self.combat_focus = false;
                    return None;
                }
                KeyCode::Enter if !self.sending => {
                    let choice =
                        &combat_choices[self.combat_selection.min(combat_choices.len() - 1)];
                    self.sending = true;
                    return Some(Submission {
                        operation: Operation::Request {
                            method: "act".into(),
                            params: json!({"intent":choice["intent"],"combat":combat_context(&self.view)}),
                        },
                        text: String::new(),
                    });
                }
                KeyCode::Char(_) if !ctrl => self.combat_focus = false,
                _ => return None,
            }
        }
        let menu = if self.dismissed {
            Vec::new()
        } else {
            suggestions(&self.draft.text, &self.view)
        };
        match key.key {
            KeyCode::Escape => {
                self.dismissed = true;
                self.error = None;
            }
            KeyCode::LeftArrow => self.draft.cursor = self.draft.cursor.saturating_sub(1),
            KeyCode::RightArrow => {
                self.draft.cursor =
                    (self.draft.cursor + 1).min(self.draft.text.graphemes(true).count())
            }
            KeyCode::Home => self.draft.boundary(false),
            KeyCode::End => self.draft.boundary(true),
            KeyCode::Char('a') if ctrl => self.draft.boundary(false),
            KeyCode::Char('e') if ctrl => self.draft.boundary(true),
            KeyCode::Backspace => {
                self.draft.delete(false);
                self.change();
            }
            KeyCode::Delete => {
                self.draft.delete(true);
                self.change();
            }
            KeyCode::UpArrow | KeyCode::DownArrow if !menu.is_empty() => {
                self.selection = if key.key == KeyCode::UpArrow {
                    (self.selection + menu.len() - 1) % menu.len()
                } else {
                    (self.selection + 1) % menu.len()
                };
            }
            KeyCode::Tab if !menu.is_empty() => {
                self.draft
                    .set(menu[self.selection.min(menu.len() - 1)].command.clone());
                self.change();
            }
            KeyCode::Enter if !self.sending => {
                if self.draft.text.trim().is_empty() {
                    return None;
                }
                match parse_input(&self.draft.text, &self.view) {
                    Ok(operation) => {
                        let text = self.draft.text.clone();
                        match &operation {
                            Operation::Local(name) => {
                                self.show(name);
                                self.acknowledge(&text);
                                return None;
                            }
                            Operation::Portraits(mode) => {
                                self.portraits = *mode;
                                self.acknowledge(&text);
                                return None;
                            }
                            Operation::Quit => {}
                            Operation::Request { .. } => self.sending = true,
                        }
                        return Some(Submission { operation, text });
                    }
                    Err(error) => {
                        if let Some(candidate) = menu.get(self.selection) {
                            self.draft.set(candidate.command.clone());
                            self.change();
                        } else {
                            self.write(&error.to_string(), BLOOD);
                        }
                    }
                }
            }
            KeyCode::UpArrow | KeyCode::DownArrow
                if !self.history.is_empty()
                    && (!self.draft.text.contains('\n') || self.history_index.is_some()) =>
            {
                if self.history_index.is_none() {
                    self.history_draft = self.draft.clone();
                }
                let next = if key.key == KeyCode::UpArrow {
                    Some(
                        self.history_index
                            .map_or(0, |n| (n + 1).min(self.history.len() - 1)),
                    )
                } else {
                    self.history_index.and_then(|n| n.checked_sub(1))
                };
                self.history_index = next;
                self.dismissed = true;
                if let Some(index) = next {
                    self.draft
                        .set(self.history[self.history.len() - 1 - index].clone());
                } else {
                    self.draft = self.history_draft.clone();
                }
            }
            KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(Modifiers::ALT) => {
                self.draft.insert(&ch.to_string());
                self.change();
            }
            _ => {}
        }
        None
    }
    fn acknowledge(&mut self, text: &str) {
        if !text.is_empty() {
            self.history.retain(|line| line != text);
            self.history.push(text.into());
            if self.history.len() > 100 {
                self.history.remove(0);
            }
        }
        if self.draft.text == text {
            self.draft = Draft::default();
        }
        self.history_index = None;
        self.selection = 0;
        self.dismissed = false;
    }
    pub fn finish(&mut self, submission: &Submission, result: Result<Value>) {
        self.sending = false;
        match result {
            Ok(value) if value.get("ok").and_then(Value::as_bool) != Some(false) => {
                if let Operation::Request { method, .. } = &submission.operation
                    && method == "act"
                    && !submission.text.is_empty()
                {
                    self.write(&format!("› {}", submission.text.trim()), AMBER);
                }
                self.acknowledge(&submission.text);
                self.combat_focus = false;
            }
            Ok(value) => self.write(field(&value, "message"), BLOOD),
            Err(error) => self.write(&error.to_string(), BLOOD),
        }
    }
    fn show(&mut self, name: &str) {
        self.write(&format!("› /{name}"), IVORY);
        let view = self.view.clone();
        match name {
            "look" => {
                self.write(field(&view["room"], "name"), AMBER);
                self.write(field(&view["room"], "description"), PALE);
                for actor in list(&view, "actors") {
                    self.write(
                        &format!(
                            "{} · {} · {}",
                            field(actor, "name"),
                            field(actor, "role"),
                            field(actor, "activity")
                        ),
                        actor_color(id(actor), id(&view["self"])),
                    );
                }
                for door in list(&view, "doors") {
                    let status = if door["locked"].as_bool().unwrap_or(false) {
                        "locked"
                    } else if door["open"].as_bool().unwrap_or(false) {
                        "open"
                    } else {
                        "closed"
                    };
                    self.write(
                        &format!(
                            "{} → {} · {}",
                            field(door, "name"),
                            field(door, "destination"),
                            translated(&view, status)
                        ),
                        door_color(status),
                    );
                }
            }
            "inventory" => {
                self.write(
                    &format!(
                        "{} · {} {}",
                        translated(&view, "Inventory"),
                        number(&view["self"], "coins"),
                        translated(&view, "coins")
                    ),
                    AMBER,
                );
                for item in list(&view, "inventory") {
                    self.write(
                        &format!("{} ×{}", field(item, "name"), number(item, "quantity")),
                        AMBER,
                    );
                    self.write(field(item, "description"), ASH);
                }
                if list(&view, "inventory").is_empty() {
                    self.write(&translated(&view, "Nothing carried."), ASH);
                }
            }
            "map" => {
                for room in list(&view, "map") {
                    self.write(
                        &format!(
                            "{}{}",
                            if id(room) == id(&view["room"]) {
                                "› "
                            } else {
                                "  "
                            },
                            field(room, "name")
                        ),
                        SAGE,
                    );
                    let connections = list(room, "connections")
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" · ");
                    if !connections.is_empty() {
                        self.write(&format!("    ↔ {connections}"), ASH);
                    }
                }
            }
            "journal" => {
                for memory in list(&view, "journal") {
                    self.write(
                        &format!(
                            "[{}] {}",
                            translated(&view, field(memory, "kind")),
                            field(memory, "text")
                        ),
                        if matches!(field(memory, "kind"), "belief" | "reflection") {
                            LILAC
                        } else {
                            ASH
                        },
                    );
                }
            }
            "help" => {
                for help in [
                    "Plain text · say it to the room",
                    "/shout <message> · adjoining rooms",
                    "/ · available commands and physical actions; Tab completes",
                    "/look · surroundings    /inventory · belongings",
                    "/map · known rooms    /journal · memories",
                    "/auto [on|off] · toggle actions inferred from speech    /cancel",
                    "/portraits auto|on|off · portrait preference",
                    "/quit · leave the castle",
                    "Enter sends · Shift+Enter / Ctrl+J newline · Ctrl+U clears",
                    "↑↓ history / suggestions · Esc closes suggestions",
                    "Page Up/Down scroll chat · Ctrl+End returns to live chat",
                    "Currently available physical actions:",
                ] {
                    self.write(&translated(&view, help), PALE);
                }
                for action in list(&view, "actions") {
                    self.write(
                        &format!("{} · {}", field(action, "command"), field(action, "label")),
                        AMBER,
                    );
                }
            }
            _ => {}
        }
    }
    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.width < 4 || area.height < 4 {
            return;
        }
        let width = usize::from(area.width.saturating_sub(2));
        let height = usize::from(area.height);
        if self.view.is_null() {
            draw(
                frame,
                Rect::new(area.x + 1, area.y, area.width - 2, 1),
                Line::styled("A torch catches in the dark…", Style::default().fg(ASH)),
            );
            return;
        }
        let menu = if self.dismissed {
            Vec::new()
        } else {
            suggestions(&self.draft.text, &self.view)
        };
        let compact = height < 16;
        let stat_rows = if compact || width >= 65 {
            1
        } else if width >= 31 {
            2
        } else {
            4
        };
        let draft_rows = self.draft.rows(width.saturating_sub(2).max(1));
        let cursor_row = draft_rows
            .iter()
            .position(|r| r.cursor.is_some())
            .unwrap_or(0);
        let draft_height = draft_rows.len().min(if compact { 1 } else { 4 }).max(1);
        let combat = self.view.get("combat").filter(|c| c.is_object());
        let combat_rows = if combat.is_some() {
            if compact { 1 } else { 3 }
        } else {
            0
        };
        let menu_rows = if combat_rows > 0 {
            0
        } else {
            menu.len().min(if height < 24 { 2 } else { 5 })
        };
        let status_rows = usize::from(!compact);
        let hint_rows = usize::from(height >= 12);
        let mut footer = stat_rows
            + draft_height
            + combat_rows
            + menu_rows
            + status_rows
            + hint_rows
            + usize::from(height >= 12);
        footer = footer.min(height.saturating_sub(4));
        let header_budget = (height / 2)
            .min(14)
            .min(height.saturating_sub(footer + 1))
            .max(3);
        let header_rows = self.render_header(
            frame,
            Rect::new(area.x + 1, area.y, area.width - 2, header_budget as u16),
            header_budget,
        );
        let chat_height = height.saturating_sub(header_rows + footer).max(1);
        self.viewport_width = width;
        self.viewport_height = chat_height;
        let rows = self.rows(width);
        let start = self.start(&rows, chat_height);
        for (offset, row) in rows.iter().skip(start).take(chat_height).enumerate() {
            draw(
                frame,
                Rect::new(
                    area.x + 1,
                    area.y + (header_rows + offset) as u16,
                    area.width - 2,
                    1,
                ),
                Line::styled(row.text.clone(), Style::default().fg(row.color)),
            );
        }
        let mut y = area.y + area.height - footer as u16;
        let bottom = area.y + area.height;
        if status_rows > 0 && y < bottom {
            let mut status = field(&self.view, "clock").to_owned();
            let self_view = &self.view["self"];
            if number(self_view, "health") <= 0.0 {
                status = translated(&self.view, "Dead");
            } else if self_view["unconscious"].as_bool().unwrap_or(false) {
                status = format!(
                    "{} · {}s",
                    translated(&self.view, "Unconscious"),
                    number(self_view, "recoverySeconds").ceil()
                );
            } else if self_view["travel"].is_object() {
                status = format!(
                    "{} · {} → {} · {}s",
                    translated(&self.view, "Travelling"),
                    field(&self_view["travel"], "origin"),
                    field(&self_view["travel"], "destination"),
                    number(&self_view["travel"], "remainingSeconds").ceil()
                );
            } else if self_view["restrained"].as_bool().unwrap_or(false) {
                status.push_str(&format!(" · {}", translated(&self.view, "Restrained")));
            }
            for paused in list(&self.view, "paused").iter().filter_map(Value::as_str) {
                status.push_str(&format!(" · {}", translated(&self.view, paused)));
            }
            status.push_str(&format!(
                " · auto {}{}",
                translated(
                    &self.view,
                    if self.view["listener"]["enabled"].as_bool().unwrap_or(false) {
                        "on"
                    } else {
                        "off"
                    }
                ),
                if self.sending { " …" } else { "" }
            ));
            draw(
                frame,
                Rect::new(area.x + 1, y, area.width - 2, 1),
                Line::styled(clip(&status, width), Style::default().fg(ASH)),
            );
            y += 1;
        }
        for line in self.needs(width, stat_rows, compact) {
            if y >= bottom {
                break;
            }
            draw(frame, Rect::new(area.x + 1, y, area.width - 2, 1), line);
            y += 1;
        }
        if let Some(combat) = combat {
            if y < bottom {
                let line = format!(
                    "{} {} · {}s{}",
                    translated(&self.view, "Round"),
                    number(combat, "round"),
                    number(combat, "remainingSeconds").ceil(),
                    if self.combat_focus {
                        " · ↑↓ Enter"
                    } else {
                        " · Tab"
                    }
                );
                draw(
                    frame,
                    Rect::new(area.x + 1, y, area.width - 2, 1),
                    Line::styled(clip(&line, width), Style::default().fg(AMBER)),
                );
                y += 1;
            }
            let choices = list(combat, "choices");
            let from = self.combat_selection.saturating_sub(1);
            for (index, choice) in choices
                .iter()
                .enumerate()
                .skip(from)
                .take(combat_rows.saturating_sub(1))
            {
                if y >= bottom {
                    break;
                }
                let selected = self.combat_focus && index == self.combat_selection;
                let style = Style::default()
                    .fg(if selected { IVORY } else { PALE })
                    .bg(if selected { SELECTED } else { Color::Reset });
                draw(
                    frame,
                    Rect::new(area.x + 1, y, area.width - 2, 1),
                    Line::styled(
                        clip(
                            &format!(
                                "{}{}",
                                if selected { "› " } else { "  " },
                                field(choice, "label")
                            ),
                            width,
                        ),
                        style,
                    ),
                );
                y += 1;
            }
        }
        let menu_start = self
            .selection
            .saturating_sub(menu_rows / 2)
            .min(menu.len().saturating_sub(menu_rows));
        for (index, choice) in menu.iter().enumerate().skip(menu_start).take(menu_rows) {
            if y >= bottom {
                break;
            }
            let selected = index == self.selection;
            let command = format!("{}{}", if selected { "› " } else { "  " }, choice.command);
            let description = clip(
                &format!(" · {}", choice.description),
                width.saturating_sub(UnicodeWidthStr::width(command.as_str())),
            );
            let line = Line::from(vec![
                Span::styled(clip(&command, width), Style::default().fg(BLUE)),
                Span::styled(description, Style::default().fg(door_color(&choice.status))),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(if selected {
                    SELECTED
                } else {
                    Color::Reset
                })),
                Rect::new(area.x + 1, y, area.width - 2, 1),
            );
            y += 1;
        }
        let editor_start = bottom.saturating_sub((draft_height + hint_rows) as u16);
        if y < editor_start {
            y = editor_start;
        }
        let from = cursor_row.saturating_sub(draft_height - 1);
        for (index, row) in draft_rows.iter().enumerate().skip(from).take(draft_height) {
            if y >= bottom {
                break;
            }
            let mut spans = vec![
                Span::styled(
                    if index == 0 { "› " } else { "  " },
                    Style::default().fg(BLUE),
                ),
                Span::styled(row.before.clone(), Style::default().fg(IVORY)),
            ];
            if let Some(cursor) = &row.cursor {
                spans.push(Span::styled(
                    cursor.clone(),
                    Style::default().fg(IVORY).add_modifier(Modifier::REVERSED),
                ));
            }
            spans.push(Span::styled(row.after.clone(), Style::default().fg(IVORY)));
            if self.draft.text.is_empty() {
                spans.push(Span::styled(
                    clip(
                        &translated(&self.view, "Say something…  / commands"),
                        width.saturating_sub(3),
                    ),
                    Style::default().fg(ASH),
                ));
            }
            draw(
                frame,
                Rect::new(area.x + 1, y, area.width - 2, 1),
                Line::from(spans),
            );
            y += 1;
        }
        if hint_rows > 0 && y < bottom {
            let hint = if self.anchor.is_some() {
                format!(
                    "{} {} · PgUp/PgDn · Ctrl+End",
                    self.unread,
                    translated(&self.view, "new")
                )
            } else {
                translated(&self.view, "/ commands · Enter speak · Ctrl+J newline")
            };
            draw(
                frame,
                Rect::new(area.x + 1, y, area.width - 2, 1),
                Line::styled(clip(&hint, width), Style::default().fg(ASH)),
            );
        }
    }
    fn needs(&self, width: usize, rows: usize, compact: bool) -> Vec<Line<'static>> {
        let specs = [
            ("health", "health", SAGE),
            ("hunger", "hunger", AMBER),
            ("thirst", "thirst", BLUE),
            ("fatigue", "tired", LILAC),
        ];
        if compact {
            return vec![Line::from(
                specs
                    .iter()
                    .enumerate()
                    .map(|(i, (key, label, color))| {
                        Span::styled(
                            format!(
                                "{}{}{}",
                                if i > 0 { " " } else { "" },
                                translated(&self.view, label)
                                    .chars()
                                    .take(if width < 31 { 1 } else { 2 })
                                    .collect::<String>(),
                                number(&self.view["self"], key).clamp(0.0, 100.0).round()
                            ),
                            Style::default().fg(*color),
                        )
                    })
                    .collect::<Vec<_>>(),
            )];
        }
        let columns = 4 / rows;
        let column_width = (width.saturating_sub((columns - 1) * 3)) / columns;
        let cells = column_width.saturating_sub(11).clamp(3, 5);
        specs
            .chunks(columns)
            .map(|chunk| {
                let mut spans = Vec::new();
                for (i, (key, label, color)) in chunk.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::styled(" · ", Style::default().fg(ASH)));
                    }
                    let value = number(&self.view["self"], key).clamp(0.0, 100.0);
                    let filled = (value * cells as f64 / 100.0).round() as usize;
                    spans.push(Span::styled(
                        format!("{:<6} ", translated(&self.view, label)),
                        Style::default().fg(*color),
                    ));
                    spans.push(Span::styled(
                        "━".repeat(filled),
                        Style::default().fg(*color),
                    ));
                    spans.push(Span::styled(
                        "─".repeat(cells - filled),
                        Style::default().fg(Color::Rgb(81, 77, 70)),
                    ));
                    spans.push(Span::styled(
                        format!(" {:>3}", value.round()),
                        Style::default().fg(IVORY),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }
    fn render_header(&self, frame: &mut Frame<'_>, area: Rect, budget: usize) -> usize {
        let width = usize::from(area.width);
        let self_view = &self.view["self"];
        let mut people = vec![self_view];
        people.extend(
            list(&self.view, "actors")
                .iter()
                .filter(|a| id(a) != id(self_view)),
        );
        let description = budget >= 5;
        let gap = budget >= 7;
        let base = 2 + usize::from(description) + usize::from(gap) + 1;
        let mut portrait = None;
        if self.portraits != PortraitMode::Off
            && (self.utf8 || self.portraits == PortraitMode::On)
            && !self.view["combat"].is_object()
        {
            for (pw, ph) in [(21usize, 9usize), (15, 7), (11, 5)] {
                let columns = width / (pw + 14);
                if columns > 0 && base + people.len().div_ceil(columns) * ph <= budget {
                    portrait = Some((pw, ph, columns));
                    break;
                }
            }
        }
        let mut y = area.y;
        draw(
            frame,
            Rect::new(area.x, y, area.width, 1),
            Line::styled(
                clip(field(&self.view["room"], "name"), width),
                Style::default().fg(AMBER),
            ),
        );
        y += 1;
        if description {
            draw(
                frame,
                Rect::new(area.x, y, area.width, 1),
                Line::styled(
                    clip(field(&self.view["room"], "description"), width),
                    Style::default().fg(PALE),
                ),
            );
            y += 1;
        }
        if gap {
            y += 1;
        }
        let label = format!(
            "{} · {}",
            translated(&self.view, "People in the room"),
            people.len()
        );
        draw(
            frame,
            Rect::new(area.x, y, area.width, 1),
            Line::styled(clip(&label, width), Style::default().fg(ASH)),
        );
        y += 1;
        if let Some((pw, ph, columns)) = portrait {
            let card_width = width / columns;
            for (index, person) in people.iter().enumerate() {
                let key = if field(person, "portraitKey").is_empty() {
                    crate::portraits::key_for(id(person))
                } else {
                    field(person, "portraitKey").into()
                };
                let face = crate::portraits::render(&key, pw as u16, ph as u16);
                let x = area.x + (index % columns * card_width) as u16;
                let row_y = y + (index / columns * ph) as u16;
                let state = if matches!(field(person, "condition"), "" | "apparently well") {
                    field(person, "activity")
                } else {
                    field(person, "condition")
                };
                for line in 0..ph {
                    let mut spans = vec![Span::styled(
                        face.get(line).cloned().unwrap_or_else(|| " ".repeat(pw)),
                        Style::default().fg(PALE),
                    )];
                    if let Some(text) =
                        [field(person, "name"), field(person, "role"), state].get(line)
                    {
                        spans.push(Span::styled(
                            format!("  {}", clip(text, card_width.saturating_sub(pw + 2))),
                            Style::default().fg(if line == 0 {
                                actor_color(id(person), id(self_view))
                            } else {
                                ASH
                            }),
                        ));
                    }
                    draw(
                        frame,
                        Rect::new(x, row_y + line as u16, card_width as u16, 1),
                        Line::from(spans),
                    );
                }
            }
            y += (people.len().div_ceil(columns) * ph) as u16;
        } else {
            let available = budget.saturating_sub(base).max(1);
            let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
            let mut used = 0;
            let mut hidden = 0;
            for person in &people {
                let name = clip(&format!(". {} ", field(person, "name")), width);
                let cells = UnicodeWidthStr::width(name.as_str());
                if used + cells > width && used > 0 {
                    if rows.len() >= available {
                        hidden += 1;
                        continue;
                    }
                    rows.push(Vec::new());
                    used = 0;
                }
                rows.last_mut().unwrap().push(Span::styled(
                    name,
                    Style::default().fg(actor_color(id(person), id(self_view))),
                ));
                used += cells;
            }
            if hidden > 0
                && let Some(last) = rows.last_mut()
            {
                last.push(Span::styled(
                    format!(" +{hidden} /look"),
                    Style::default().fg(ASH),
                ));
            }
            for row in rows {
                draw(frame, Rect::new(area.x, y, area.width, 1), Line::from(row));
                y += 1;
            }
        }
        if usize::from(y - area.y) < budget {
            y += 1;
        }
        usize::from(y - area.y)
    }
}
fn draw(frame: &mut Frame<'_>, area: Rect, line: Line<'_>) {
    if area.height > 0 && area.y < frame.area().bottom() {
        frame.render_widget(Paragraph::new(line), area);
    }
}

const TITLE_WORDS: [[&str; 4]; 3] = [
    [
        "⣿⠛⠛⡄ ⣿⠛⠛⠃ ⣿    ⣤⠛⠛⡄ ⣿  ⡇",
        "⣿⣤⣤⠃ ⣿⣤⣤  ⣿    ⣿  ⡇ ⣿⢠⡄⡇",
        "⣿  ⡇ ⣿    ⣿    ⣿  ⡇ ⣿⢸⡇⡇",
        "⠛⠛⠛  ⠛⠛⠛⠃ ⠛⠛⠛⠃  ⠛⠛   ⠃⠘ ",
    ],
    [
        "⠛⢻⡟⠃ ⣿  ⡇ ⣿⠛⠛⠃",
        " ⢸⡇  ⣿⣤⣤⡇ ⣿⣤⣤ ",
        " ⢸⡇  ⣿  ⡇ ⣿   ",
        " ⠘⠃  ⠛  ⠃ ⠛⠛⠛⠃",
    ],
    [
        "⣤⠛⠛⠃ ⣿⠛⠛⡄ ⣤⠛⠛⡄ ⣿  ⡇ ⣿⡄ ⡇",
        "⣿    ⣿⣤⣤⠃ ⣿  ⡇ ⣿⢠⡄⡇ ⣿⢣⡄⡇",
        "⣿    ⣿⠘⢣  ⣿  ⡇ ⣿⢸⡇⡇ ⣿ ⢸⡇",
        " ⠛⠛⠃ ⠛  ⠃  ⠛⠛   ⠃⠘  ⠛  ⠃",
    ],
];

/// Whole-word title layout measured in terminal cells, with an ASCII fallback.
pub fn title(width: usize, max_rows: usize, braille: bool) -> Vec<String> {
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }
    let names = ["BELOW", "THE", "CROWN"];
    let group = |widths: [usize; 3], gap: usize| {
        let mut groups = vec![Vec::new()];
        let mut used = 0;
        for (word, word_width) in widths.into_iter().enumerate() {
            let spacing = if used == 0 { 0 } else { gap };
            if used > 0 && used + spacing + word_width > width {
                groups.push(Vec::new());
                used = 0;
            }
            if used > 0 {
                used += gap;
            }
            used += word_width;
            groups.last_mut().unwrap().push(word);
        }
        groups
    };
    if braille {
        let widths = TITLE_WORDS.map(|rows| {
            rows.iter()
                .map(|row| UnicodeWidthStr::width(*row))
                .max()
                .unwrap()
        });
        let groups = group(widths, 3);
        if widths.iter().all(|w| *w <= width) && groups.len() * 4 <= max_rows {
            return groups
                .into_iter()
                .flat_map(|words| {
                    (0..4).map(move |row| {
                        words
                            .iter()
                            .map(|word| TITLE_WORDS[*word][row])
                            .collect::<Vec<_>>()
                            .join("   ")
                            .trim_end()
                            .to_owned()
                    })
                })
                .collect();
        }
    }
    group(names.map(str::len), 1)
        .into_iter()
        .take(max_rows)
        .map(|words| {
            clip(
                &words
                    .iter()
                    .map(|word| names[*word])
                    .collect::<Vec<_>>()
                    .join(" "),
                width,
            )
        })
        .collect()
}

pub fn local_braille_support() -> bool {
    let term = std::env::var("TERM").unwrap_or_default();
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
    crate::ssh::supports_braille(&term, locale.as_deref())
}

pub fn render_join(
    frame: &mut Frame<'_>,
    draft: &Draft,
    language: &str,
    error: Option<&str>,
    braille: bool,
) {
    let area = frame.area();
    if area.height < 3 || area.width < 4 {
        return;
    }
    let width = usize::from(area.width - 2);
    for (index, line) in title(width, usize::from(area.height - 3), braille)
        .into_iter()
        .take(usize::from(area.height - 3))
        .enumerate()
    {
        draw(
            frame,
            Rect::new(area.x + 1, area.y + index as u16, area.width - 2, 1),
            Line::styled(line, Style::default().fg(AMBER)),
        );
    }
    let label = crate::localization::tr(language, "Your name · enter join", &[]);
    if let Some(error) = error {
        draw(
            frame,
            Rect::new(area.x + 1, area.bottom() - 3, area.width - 2, 1),
            Line::styled(clip(error, width), Style::default().fg(BLOOD)),
        );
    }
    draw(
        frame,
        Rect::new(area.x + 1, area.bottom() - 2, area.width - 2, 1),
        Line::styled(clip(&label, width), Style::default().fg(ASH)),
    );
    let name = clip(&draft.text, width.saturating_sub(3));
    draw(
        frame,
        Rect::new(area.x + 1, area.bottom() - 1, area.width - 2, 1),
        Line::from(vec![
            Span::styled("› ", Style::default().fg(BLUE)),
            Span::styled(name, Style::default().fg(IVORY)),
            Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)),
        ]),
    );
}
