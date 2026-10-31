// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! Pi (`pi`, the user's own) in JSON print mode, with its sign-in, its extensions and its own default
//! model used as found: one process per turn, and the conversation a session file resumed every time.
//! Pi's builtin tools stay off (no shell, no files) — Isle runs the actions; what the user's own
//! extensions add (donsetch's `web_search`, `web_fetch`…) is what the brain may use.

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use chrono::{DateTime, Local, NaiveDate};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use super::answer::{Answer, SCHEMA, from_text, partial_say};
use super::child::{self, Events, Group};

/// How many of stderr's last lines are kept to explain a failure.
const STDERR_LINES: usize = 20;

/// Events that can matter, each with its quotes.
const WORDS: &[&str] = &["message_update", "message_end", "agent_settled"];

/// The brain: nothing runs until the first `ask` (Pi has no server to start); every turn is fresh.
pub struct Pi {
    program: PathBuf,
    workspace: PathBuf,
    /// The conversation: a session id, made new, or `start`'s to resume.
    pub conversation: Option<String>,
    instructions: String,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

impl Pi {
    /// Gets ready to answer with Isle's job (`instructions`) in place of Pi's own prompt.
    pub fn start(
        program: &Path,
        _model: &str,
        resume: Option<&str>,
        workspace: &Path,
        instructions: &str,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(workspace)?;
        // Pi has no `--json-schema`, so the shape is spelled out instead: the envelope, the actions
        // flat beside their `type` (never nested under it), and the schema itself.
        let instructions = format!(
            "{instructions}\n\
             Reply with exactly one JSON object and nothing around it — no words before or after it, \
             no code fence, and it only once: like this:\n\
             {{\"say\": \"...\", \"actions\": [], \"listen\": false, \"done\": true}}\n\
             Each action is an object with its \"type\" and its fields at the same level, never nested \
             under a key of its own: {{\"type\": \"open_url\", \"url\": \"https://youtube.com\"}}, \
             {{\"type\": \"media\", \"command\": \"play_pause\"}}. All words to say go in \"say\".\n\
             The fields each may carry, as a JSON Schema:\n{SCHEMA}"
        );
        Ok(Self {
            program: program.to_owned(),
            workspace: workspace.to_owned(),
            conversation: Some(match resume {
                Some(id) if !id.is_empty() => id.to_owned(),
                _ => new_id(),
            }),
            instructions,
            stderr: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    /// One turn; an error carries what Pi printed to stderr meanwhile.
    pub async fn ask(
        &mut self,
        prompt: &str,
        activity: Option<&UnboundedSender<String>>,
        draft: Option<&UnboundedSender<String>>,
    ) -> anyhow::Result<Answer> {
        self.stderr.lock().unwrap().clear();
        self.turn(prompt, activity, draft).await.map_err(|err| {
            let stderr = Vec::from(self.stderr.lock().unwrap().clone()).join(" | ");
            if stderr.is_empty() {
                err
            } else {
                err.context(stderr)
            }
        })
    }

    /// Runs `pi` with `prompt` and reads its events until it settles, passing the steps and the draft
    /// as they come; the process dropped once the answer is in, for only its session is kept.
    async fn turn(
        &mut self,
        prompt: &str,
        activity: Option<&UnboundedSender<String>>,
        draft: Option<&UnboundedSender<String>>,
    ) -> anyhow::Result<Answer> {
        let session = self
            .conversation
            .clone()
            .context("a Pi conversation to resume")?;
        let mut command = tokio::process::Command::new(&self.program);
        // The user's extensions, hooks and model stay as they are (their default provider lives in one
        // of them); only Pi's own skills, prompt templates, themes and context files stay out, and no
        // builtin tool comes back: Isle runs the actions, the brain only answers.
        command
            .args([
                "--mode",
                "json",
                "--print",
                "--no-builtin-tools",
                "--no-skills",
                "--no-prompt-templates",
                "--no-themes",
                "--no-context-files",
                "--no-approve",
                "--session-dir",
            ])
            .arg(&self.workspace)
            .args(["--session-id", &session])
            .arg("--system-prompt")
            .arg(&self.instructions);
        // The user's default, spelled out: a resumed session would otherwise carry the concrete model
        // a router resolved once, which the provider then refuses. `github-copilot-auto/auto` keeps
        // the virtual one, and with it whichever real model the router picks.
        if let Some(model) = default_model() {
            command.arg("--model").arg(model);
        }
        // After `--`: the prompt is never an option, whatever it starts with.
        command.arg("--").arg(prompt);
        command
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child =
            child::spawn(&mut command).context("could not start pi; is Pi installed?")?;
        let mut events = Events::new(child.stdout.take().expect("piped stdout"));
        keep_stderr(&mut child, self.stderr.clone());

        // What it is writing is the draft (`say`, out of the JSON as it comes); the tools it starts
        // are what the island shows; the last assistant message is the answer. Dropped here, the
        // process and its tools end the moment this read does.
        let mut written = String::new();
        let mut shown = String::new();
        let mut finally = String::new();
        while let Some(event) = events.next(WORDS).await? {
            if event["type"] == "message_update" {
                let delta = &event["assistantMessageEvent"];
                if delta["type"] == "text_delta" {
                    written.push_str(delta["delta"].as_str().unwrap_or_default());
                    if let (Some(draft), Some(say)) = (draft, partial_say(written.as_str()))
                        && say != shown
                    {
                        draft.send(say.clone()).ok();
                        shown = say;
                    }
                }
                continue;
            }
            let kind = event["type"].as_str().unwrap_or_default();
            if kind == "agent_settled" {
                break;
            }
            if kind != "message_end" {
                continue;
            }
            let message = &event["message"];
            if message["role"] != "assistant" {
                continue;
            }
            if let Some(activity) = activity {
                steps(message).into_iter().for_each(|step| {
                    activity.send(step).ok();
                });
            }
            let error = message["errorMessage"].as_str().unwrap_or_default();
            if error.is_empty() && message["stopReason"] != "aborted" {
                finally = unsaid(&text_of(message));
            } else if error.is_empty() {
                bail!("pi stopped")
            } else {
                bail!("{error}")
            }
        }
        let _ = child; // dropped here: nothing of Pi lives past the answer
        drop(child);
        if finally.trim().is_empty() {
            bail!("pi stopped")
        }
        Ok(answer(&finally))
    }
}

/// The last lines of `child`'s stderr, kept on `kept` to explain a failure; ends with the child.
fn keep_stderr(child: &mut Group, kept: Arc<Mutex<VecDeque<String>>>) {
    let stderr = child.stderr.take().expect("piped stderr");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut kept = kept.lock().unwrap();
            if kept.len() == STDERR_LINES {
                kept.pop_front();
            }
            kept.push_back(line);
        }
    });
}

/// What Pi got itself into, for the island: its web searches and the pages it reads.
fn steps(message: &Value) -> Vec<String> {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "toolCall")
        .filter_map(|block| {
            let given = block["arguments"].as_object().is_some();
            let input = if given {
                &block["arguments"]
            } else {
                &block["input"]
            };
            match block["name"].as_str()? {
                "web_search" => match input["query"].as_str() {
                    Some(query) if !query.is_empty() => {
                        Some(format!("Searching the web for “{query}”…"))
                    }
                    _ => Some("Searching the web…".into()),
                },
                "web_fetch" => Some(format!(
                    "Reading {}…",
                    input["url"]
                        .as_str()
                        .and_then(|url| url.split("://").nth(1))
                        .and_then(|rest| rest.split('/').next())
                        .unwrap_or("a page")
                )),
                _ => None,
            }
        })
        .collect()
}

/// `message`'s words: its text blocks, whole. What a model could only write out — its tool calls as
/// words (`<tool_code>…`, `<function_calls>…`) — is left out, for none of it ran.
fn text_of(message: &Value) -> String {
    message["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// `text` as an answer; one action on its own (`{"media": {...}}`, its type the only key) is taken as
/// that action with nothing said, since Isle only needs its `actions`.
fn answer(text: &str) -> Answer {
    let text = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(text)
        && value.get("say").is_none()
        && let Some(fields) = value.as_object().filter(|object| object.len() == 1)
        && let Some((kind, parts)) = fields.iter().next()
        && let Some(mut action) = parts.as_object().cloned()
    {
        action.insert("type".into(), kind.clone().into());
        return from_text(&serde_json::json!({"say": "", "actions": [action]}).to_string());
    }
    from_text(text)
}

fn unsaid(text: &str) -> String {
    let mut text = text.to_owned();
    for (start, end) in [
        ("<tool_code>", "</tool_code>"),
        ("<function_calls>", "</function_calls>"),
    ] {
        loop {
            match (text.find(start), text.find(end)) {
                // Started and ended: it never ran.
                (Some(at), Some(to)) if at < to => {
                    text = format!("{}{}", &text[..at], &text[to + end.len()..]);
                }
                // Started but never finished: the turn stopped there; it never ran. Its tag is
                // stripped, what may follow it kept.
                (Some(at), _) => {
                    text = format!("{}{}", &text[..at], &text[at + start.len()..]);
                    break;
                }
                _ => break,
            }
        }
    }
    text
}

/// `$HOME/.pi/agent`, as Pi's files and its web package's path are put together from.
fn home() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".pi").join("agent"))
}

/// The model Pi's own settings pick (`defaultProvider`/`defaultModel`), when they say.
fn default_model() -> Option<String> {
    let settings = std::fs::read_to_string(home()?.join("settings.json")).ok()?;
    let settings: Value = serde_json::from_str(&settings).ok()?;
    let provider = settings["defaultProvider"]
        .as_str()
        .filter(|p| !p.is_empty())?;
    let model = settings["defaultModel"]
        .as_str()
        .filter(|m| !m.is_empty())?;
    Some(format!("{provider}/{model}"))
}

/// Whether the user signed in to Pi: its credentials file.
pub fn signed_in() -> bool {
    home().is_some_and(|home| home.join("auth.json").exists())
}

/// Whether `name` holds one of Pi's sessions (`<timestamp>_<id>.jsonl`), which are Isle's own.
fn a_session(name: &str) -> bool {
    name.ends_with(".jsonl")
        && name
            .strip_suffix(".jsonl")
            .and_then(|name| name.split_once('_'))
            .is_some_and(|(_, id)| {
                id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
            })
}

/// Deletes Isle's Pi conversation `id` (a session id as `--session-id` takes; never a path).
pub fn forget(id: &str) {
    if id.is_empty() {
        return;
    }
    let dir = crate::store::data_dir().join("assistant");
    let Some(entries) = std::fs::read_dir(dir).ok() else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if a_session(&name) && name.ends_with(&format!("_{id}.jsonl")) {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

/// Deletes Isle's Pi conversations last changed before `today`.
pub fn prune(today: NaiveDate) {
    let Some(entries) = std::fs::read_dir(crate::store::data_dir().join("assistant")).ok() else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !a_session(&name) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|time| DateTime::<Local>::from(time).date_naive() < today);
        if old {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

/// A session id with the shape of a UUID, from the system random.
fn new_id() -> String {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut here| here.read_exact(&mut bytes))
        .ok();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_the_shape_of_uuids() {
        let id = new_id();
        assert_eq!(
            id.split('-').map(str::len).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert!(a_session(&format!("2026-10-30T15-09-42Z_{id}.jsonl")));
        assert!(!a_session(&format!("{id}.jsonl"))); // not Pi's name for one
        assert!(!a_session("anything-else.jsonl"));
        // The id passes through: sessions are only ever deleted by their whole name.
        let _ = forget(&id);
    }

    #[test]
    fn written_out_tool_calls_are_left_out() {
        assert_eq!(
            unsaid("x<tool_code><tool_name>web_search</tool_name></tool_code>done"),
            "xdone"
        );
        assert_eq!(
            unsaid("a<function_calls>x</function_calls>b<function_calls>y</function_calls>"),
            "ab"
        );
        // Not ended: what is beside it keeps the words anyway.
        assert_eq!(unsaid("a<tool_code>b"), "ab");
    }

    #[test]
    fn one_action_on_its_own_is_taken_as_that_action() {
        let media = answer(r#"{"media":{"command":"play_pause"}}"#);
        assert_eq!(media.say, "");
        assert_eq!(
            media.actions,
            [serde_json::json!({"command": "play_pause", "type": "media"})]
        );
        let opened = answer(r#"{"open_url":{"url":"https://youtube.com"}}"#);
        assert_eq!(opened.actions[0]["type"], "open_url");
        assert_eq!(opened.actions[0]["url"], "https://youtube.com");
        // An answer proper is left as it is; so is prose.
        let proper = answer(r#"{"say":"hi","actions":[]}"#);
        assert_eq!(proper.say, "hi");
        assert_eq!(answer("just words").say, "just words");
    }

    #[test]
    fn reads_its_steps() {
        let search = serde_json::json!({"role": "assistant", "content": [
            {"type": "text", "text": "x"},
            {"type": "toolCall", "name": "web_search", "arguments": {"query": "cuaca jakarta"}},
        ]});
        assert_eq!(steps(&search), ["Searching the web for “cuaca jakarta”…"]);
    }
}
