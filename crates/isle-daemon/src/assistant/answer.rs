// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! What every AI answers with, whichever the user picked: a JSON object of what to say, the actions
//! Isle runs, whether it is done or wants to see what they did first, and whether to listen again.

use serde::Deserialize;
use serde_json::Value;

/// What the model must answer with; the extension runs the actions.
pub const SCHEMA: &str = r#"{"type":"object","required":["say","actions"],"properties":{
"say":{"type":"string"},"listen":{"type":"boolean"},"done":{"type":"boolean"},
"actions":{"type":"array","items":{"type":"object","required":["type"],"properties":{
"type":{"type":"string","enum":["open_url","open_app","media","focus","add_task","add_event",
"volume","wifi","bluetooth","dnd","remind","settings","terminal","run","check","close_app","open_folder","brightness","dark_mode","night_light","power_mode","done_task","remove_task","cancel_reminder","remove_event","play_music","window","find_files","list_folder","recent_files","read_text","find_apps","top_processes","chrome_profiles","open_file","reveal_file","chrome_profile","notifications","screenshot","lock","write","arrange"]},
"url":{"type":"string"},"app":{"type":"string"},"command":{"type":"string"},
"text":{"type":"string"},"date":{"type":"string"},"level":{"type":"number"},
"minutes":{"type":"number"},"on":{"type":"boolean"},"panel":{"type":"string"},"path":{"type":"string"},"id":{"type":"number"},"query":{"type":"string"},"uri":{"type":"string"},
"terms":{"type":"array","items":{"type":"string"}},"in":{"type":"string"},"kind":{"type":"string"},
"profile":{"type":"string"},"title":{"type":"string"},"layout":{"type":"string"},
"apps":{"type":"array","items":{"type":"string"}},
"why":{"type":"string"},"detail":{"type":"string"},"root":{"type":"boolean"},"mode":{"type":"string"}}}}}}"#;

/// One parsed answer, plus what isled noted while getting it.
#[derive(Debug, Clone, Deserialize)]
pub struct Answer {
    pub say: String,
    #[serde(default)]
    pub actions: Vec<Value>,
    /// The answer asks the user something, so Isle listens again at once.
    #[serde(default)]
    pub listen: bool,
    /// False while the AI wants its actions run and their results back before it goes on.
    #[serde(default = "yes")]
    pub done: bool,
    /// The tools isled ran for this answer, in order (see agent.rs).
    #[serde(skip)]
    pub tools: Vec<String>,
    /// The checks among them, by their command ("powerprofilesctl get").
    #[serde(skip)]
    pub checks: Vec<String>,
    /// Its actions already ran and worked, but those left for once the island closes (see agent.rs).
    #[serde(skip)]
    pub checked: bool,
    /// The commands it asked to run on the way, and how each went (done, failed, denied, stopped), for the
    /// conversation.
    #[serde(skip)]
    pub ran: Vec<(String, String)>,
}

impl Answer {
    /// Just words: what an AI said without the JSON asked for, or an error.
    pub fn plain(say: impl Into<String>) -> Self {
        Self {
            say: say.into(),
            actions: Vec::new(),
            listen: false,
            done: true,
            tools: Vec::new(),
            checks: Vec::new(),
            checked: false,
            ran: Vec::new(),
        }
    }
}

/// Serde default for `done`: an answer that leaves it out is finished.
fn yes() -> bool {
    true
}

/// The answer in `text`: its JSON, also inside a ```json fence or among words, else the text as said,
/// without actions.
pub fn from_text(text: &str) -> Answer {
    let text = text.trim();
    let inner = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|rest| rest.trim_end().strip_suffix("```"))
        .map_or(text, str::trim);
    let parsed = serde_json::from_str::<Answer>(inner)
        .ok()
        .or_else(|| embedded(inner));
    let mut answer = parsed.unwrap_or_else(|| Answer::plain(text));
    answer.actions.iter_mut().for_each(window_focus);
    answer
}

/// The last JSON object in `inner` that looks like an answer: models sometimes write words around it,
/// or repeat it. Only an object with a `say` or `actions` counts, so prose with braces is left alone.
fn embedded(inner: &str) -> Option<Answer> {
    let mut found = None;
    for (at, _) in inner.match_indices('{') {
        let Ok(value) = serde_json::Deserializer::from_str(&inner[at..])
            .into_iter::<Value>()
            .next()?
        else {
            continue;
        };
        if value.get("say").is_none() && value.get("actions").is_none() {
            continue;
        }
        if let Ok(answer) = serde_json::from_value::<Answer>(value) {
            found = Some(answer);
        }
    }
    found
}

/// A `focus` naming an app or window means switching to it, not the focus timer (Codex mixes the two up).
pub fn window_focus(action: &mut Value) {
    let named = ["app", "title"]
        .iter()
        .any(|key| action[*key].as_str().is_some_and(|name| !name.is_empty()));
    if action["type"] == "focus" && named {
        action["type"] = "window".into();
        action["command"] = "focus".into();
    }
}

/// An event, task or reminder whose line came split (`title`, and its time in `detail` or `time`) gets it
/// whole in `text`, as Isle reads it: some AIs split it, and nothing would be added.
pub fn line_in_text(action: &mut Value) {
    let kind = action["type"].as_str().unwrap_or_default();
    let has_text = action["text"]
        .as_str()
        .is_some_and(|text| !text.trim().is_empty());
    if has_text || !matches!(kind, "add_event" | "add_task" | "remind") {
        return;
    }
    let part = |key: &str| {
        action[key]
            .as_str()
            .map(str::trim)
            .filter(|part| !part.is_empty())
    };
    let Some(title) = part("title") else { return };
    let when = (kind == "add_event")
        .then(|| part("time").or_else(|| part("detail")))
        .flatten();
    let line = when.map_or_else(|| title.to_owned(), |when| format!("{title} {when}"));
    action["text"] = line.into();
}

/// What `say` holds so far in an answer still being written (`{"say": "Hel`), for the island to show as it
/// comes; `None` until its value has begun.
pub fn partial_say(json: &str) -> Option<String> {
    let at = json.find("\"say\"")?;
    let rest = json[at + 5..].trim_start().strip_prefix(':')?.trim_start();
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut say = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => say.push('\n'),
                Some('t') => say.push('\t'),
                Some('u') => {
                    let code: String = chars.by_ref().take(4).collect();
                    // Half a \u escape so far, or half a surrogate pair: shown once it is whole.
                    match u32::from_str_radix(&code, 16).ok().and_then(char::from_u32) {
                        Some(c) if code.len() == 4 => say.push(c),
                        _ => break,
                    }
                }
                Some(other) => say.push(other),
                None => break,
            },
            c => say.push(c),
        }
    }
    Some(say)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_say_as_it_is_written() {
        assert_eq!(partial_say(r#"{"actions":[],"#), None);
        assert_eq!(partial_say(r#"{"say": "Hel"#).as_deref(), Some("Hel"));
        assert_eq!(
            partial_say(r#"{"say":"a \"b\"\nc","listen":false}"#).as_deref(),
            Some("a \"b\"\nc")
        );
        assert_eq!(
            partial_say(r#"{"say":"caf\u00e9 \u00"#).as_deref(),
            Some("café ")
        );
        assert_eq!(partial_say(r#"{"say":"end\"#).as_deref(), Some("end"));
    }

    #[test]
    fn reads_an_answer_however_it_is_wrapped() {
        let json = r#"{"say": "Opened", "actions": [{"type": "open_app", "app": "Files"}]}"#;
        assert_eq!(from_text(json).actions.len(), 1);
        assert_eq!(from_text(&format!("```json\n{json}\n```")).actions.len(), 1);
        assert_eq!(from_text(&format!("```\n{json}\n```")).say, "Opened");
        let switch =
            from_text(r#"{"say": "Ok", "actions": [{"type": "focus", "app": "Google Chrome"}]}"#);
        assert_eq!(switch.actions[0]["type"], "window");
        assert_eq!(switch.actions[0]["command"], "focus");
        let timer =
            from_text(r#"{"say": "Ok", "actions": [{"type": "focus", "command": "start"}]}"#);
        assert_eq!(timer.actions[0]["type"], "focus");
        let mut split = serde_json::json!({"type": "add_event", "date": "2026-10-29", "title": "Meeting", "detail": "09:00"});
        line_in_text(&mut split);
        assert_eq!(split["text"], "Meeting 09:00");
        let mut whole =
            serde_json::json!({"type": "add_event", "text": "Lunch 12:00", "title": "Other"});
        line_in_text(&mut whole);
        assert_eq!(whole["text"], "Lunch 12:00");
        let mut task =
            serde_json::json!({"type": "add_task", "title": "Buy milk", "detail": "the oat one"});
        line_in_text(&mut task);
        assert_eq!(task["text"], "Buy milk");
        let plain = from_text("  Sure, it is 3 pm.  ");
        assert_eq!(plain.say, "Sure, it is 3 pm.");
        assert!(plain.actions.is_empty() && plain.done);
    }

    #[test]
    fn reads_an_answer_written_among_words() {
        // What Pi wrote once: the envelope, a sentence, and the envelope again.
        let json = r#"{"say":"Opening Brave now.","actions":[{"type":"open_app","app":"Brave Origin"}],"listen":false,"done":true}"#;
        let answer = from_text(&format!("{json}\nI’m going to open Brave.\n{json}"));
        assert_eq!(answer.say, "Opening Brave now.");
        assert_eq!(answer.actions.len(), 1);
        assert_eq!(answer.actions[0]["app"], "Brave Origin");
        // Braces in prose are not mistaken for an answer.
        let prose = from_text("The set {a, b} is fine, and {c} too.");
        assert_eq!(prose.say, "The set {a, b} is fine, and {c} too.");
        assert!(prose.actions.is_empty());
    }
}
