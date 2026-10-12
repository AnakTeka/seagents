//! Codex's transcript: the rollout JSONL under
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`, whose
//! path Codex's hooks report as `transcript_path`. The one parser of it —
//! the offset index, `read` and the turn ledger's interrupt check all go
//! through here.
//!
//! Shapes (codex-cli 0.152; the `{timestamp, type, payload}` envelope is
//! what every rollout here since late 2025 has; `ordinal` is new in 0.152
//! and not relied on):
//!
//! - `response_item` lines are the conversation: `message` (role `user`,
//!   `assistant`, `developer`), `reasoning`, `function_call` /
//!   `custom_tool_call` / `local_shell_call` / `web_search_call` and the
//!   calls' `*_output`s. `event_msg` lines narrate the same thing for UIs
//!   (`item_completed`, `token_count`, `task_complete`, `turn_aborted`, …);
//!   `session_meta`, `turn_context`, `world_state` are context.
//! - A `user` message is often not the user: Codex files its environment
//!   context, AGENTS.md, skill bodies and the `<turn_aborted>` note as user
//!   messages too. 0.152 tags each content item's origin
//!   (`internal_chat_message_metadata_passthrough.content_item_kinds`,
//!   `user.text` for what was typed); older rollouts are told apart by the
//!   fragments' tags, as Codex itself does.
//! - No uuids and no parent links: the file is a log. It branches only by
//!   rollback — `thread_rolled_back {num_turns}` drops the last N user turns
//!   and the conversation goes on from before them — so the tree is the log
//!   chained line to line, with a rollback hung off the node before the
//!   turns it dropped (what Codex's own `user_message_positions_in_rollout`
//!   does to the list). The dropped turns are then a side branch, and the
//!   main line ([`super::mainline`]) runs past them. A node's uuid is its
//!   byte offset + 1: stable, since the file only grows.
//! - Esc writes `turn_aborted {reason: "interrupted"}` (verified: also when
//!   pressed before the first token), and no `Stop` hook fires.

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::index::{format_uuid, Node, NodeKind};
use super::model::{Kind, Record};

/// Whether `line` is a rollout line: the envelope, with a payload.
pub(super) fn sniff(line: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Envelope<'a> {
        #[serde(borrow, rename = "type")]
        kind: std::borrow::Cow<'a, str>,
        #[allow(dead_code)] // required: a line without one is not a rollout's
        payload: serde::de::IgnoredAny,
    }
    serde_json::from_slice::<Envelope>(line).is_ok_and(|e| !e.kind.is_empty())
}

/// What the tree needs from a line, borrowed where the JSON allows; a
/// `reasoning` item's encrypted blob and a tool's output are skipped unparsed.
#[derive(Deserialize)]
struct Head<'a> {
    #[serde(borrow, default)]
    timestamp: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default, rename = "type")]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default)]
    payload: Option<Payload<'a>>,
}

#[derive(Deserialize)]
struct Payload<'a> {
    #[serde(borrow, default, rename = "type")]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default)]
    role: Option<std::borrow::Cow<'a, str>>,
    #[serde(default)]
    num_turns: Option<u64>,
    #[serde(borrow, default)]
    content: Option<Vec<Content<'a>>>,
    #[serde(borrow, default)]
    internal_chat_message_metadata_passthrough: Option<Metadata<'a>>,
}

#[derive(Deserialize)]
struct Content<'a> {
    #[serde(borrow, default)]
    text: Option<std::borrow::Cow<'a, str>>,
}

#[derive(Deserialize)]
struct Metadata<'a> {
    #[serde(borrow, default)]
    content_item_kinds: Option<Vec<std::borrow::Cow<'a, str>>>,
}

/// The tags Codex opens a fragment of injected context with, when it files
/// one as a user message (codex-rs `CONTEXTUAL_USER_FRAGMENT_MATCHERS`) —
/// for rollouts that predate `content_item_kinds`.
const CONTEXT_TAGS: &[&str] = &[
    "<environment_context",
    "<user_instructions",
    "# AGENTS.md instructions",
    "<turn_aborted",
    "<user_shell_command",
    "<subagent_notification",
    "<codex_internal_context",
    "<goal_context",
    "<skill",
    "<additional_context",
    "<recommended_plugins",
    "<model_switch",
];

/// Whether a `user` message is the user's own words — a turn — rather
/// than context Codex injected under the user role.
fn user_turn(p: &Payload) -> bool {
    if let Some(kinds) = p.internal_chat_message_metadata_passthrough.as_ref().and_then(|m| m.content_item_kinds.as_ref()) {
        return kinds.iter().any(|k| k.starts_with("user."));
    }
    let contextual = p.content.iter().flatten().any(|c| {
        c.text.as_deref().is_some_and(|t| CONTEXT_TAGS.iter().any(|tag| t.trim_start().starts_with(tag)))
    });
    !contextual
}

/// The tree node a line is: every line with a timestamp, chained to the one
/// before, a rollback hung off the node before the turns it drops.
pub(super) fn head(line: &[u8], at: u64, prior: &[Node]) -> Option<Node> {
    let h: Head = serde_json::from_slice(line).ok()?;
    let ts = super::parse_utc_ms(h.timestamp.as_deref()?)?;
    let p = h.payload.as_ref();
    let item = p.and_then(|p| p.kind.as_deref()).unwrap_or("");
    let kind = match h.kind.as_deref() {
        Some("response_item") => match (item, p.and_then(|p| p.role.as_deref())) {
            ("message", Some("user")) if p.is_some_and(user_turn) => NodeKind::User,
            ("message", Some("assistant")) => NodeKind::Assistant,
            ("message", _) => NodeKind::Other,
            // A call, its output, reasoning: the assistant's side of a turn.
            _ => NodeKind::Assistant,
        },
        _ => NodeKind::Other,
    };
    let mut parent = prior.last().map_or(0, |n| n.uuid);
    if h.kind.as_deref() == Some("event_msg") && item == "thread_rolled_back" {
        let turns = p.and_then(|p| p.num_turns).unwrap_or(0);
        parent = before_turns(prior, turns);
    }
    Some(Node { offset: 0, len: 0, kind, ts, uuid: u128::from(at) + 1, parent })
}

/// The node the conversation stood on before its last `turns` user turns:
/// the parent of the `turns`-th user node, walking back along the chain
/// from the newest node. 0 (a new root) when that is all of them.
fn before_turns(prior: &[Node], turns: u64) -> u128 {
    let Some(last) = prior.last() else {
        return 0;
    };
    if turns == 0 {
        return last.uuid;
    }
    let by_uuid = |u: u128| {
        // uuid = offset + 1, and nodes are in offset order.
        let offset = u64::try_from(u.checked_sub(1)?).ok()?;
        prior.binary_search_by_key(&offset, |n| n.offset).ok().map(|i| &prior[i])
    };
    let mut seen = 0;
    let mut at = Some(last);
    while let Some(n) = at {
        if n.kind == NodeKind::User {
            seen += 1;
            if seen == turns {
                return n.parent;
            }
        }
        at = (n.parent != 0).then(|| by_uuid(n.parent)).flatten();
    }
    0
}

/// The records of one line: what the user typed, the assistant's text, its
/// reasoning summary, tool calls and their output.
pub(super) fn records(line: &[u8], node: &Node) -> Vec<Record> {
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return Vec::new();
    };
    if v["type"] != "response_item" {
        return Vec::new();
    }
    let p = &v["payload"];
    let record = |kind| Record {
        uuid: format_uuid(node.uuid),
        parent_uuid: (node.parent != 0).then(|| format_uuid(node.parent)),
        timestamp: v["timestamp"].as_str().map(str::to_string),
        kind,
    };
    let text = |s: &str| s.to_string();
    let kind = match p["type"].as_str().unwrap_or("") {
        "message" => match p["role"].as_str() {
            Some("user") if node.kind == NodeKind::User => Kind::User { text: flatten(&p["content"]) },
            Some("assistant") => Kind::Assistant { text: flatten(&p["content"]) },
            _ => return Vec::new(),
        },
        "reasoning" => {
            let parts: Vec<&str> = ["summary", "content"]
                .iter()
                .flat_map(|k| p[*k].as_array().into_iter().flatten())
                .filter_map(|s| s["text"].as_str())
                .collect();
            if parts.is_empty() {
                return Vec::new(); // encrypted only
            }
            Kind::Thinking { text: parts.join("\n\n") }
        }
        "function_call" | "custom_tool_call" => {
            let raw = p["arguments"].as_str().or(p["input"].as_str()).unwrap_or("");
            Kind::ToolUse {
                id: p["call_id"].as_str().map_or_else(String::new, text),
                name: p["name"].as_str().map_or_else(String::new, text),
                // `arguments` is JSON in a string; a custom tool's `input`
                // is free text (0.152's `exec` takes a script).
                input: serde_json::from_str::<Value>(raw).ok().filter(Value::is_object).unwrap_or_else(|| Value::String(raw.to_string())),
                subagent: None,
            }
        }
        "local_shell_call" => Kind::ToolUse {
            id: p["call_id"].as_str().or(p["id"].as_str()).map_or_else(String::new, text),
            name: "shell".to_string(),
            input: p["action"].clone(),
            subagent: None,
        },
        "web_search_call" => Kind::ToolUse {
            id: p["id"].as_str().map_or_else(String::new, text),
            name: "web_search".to_string(),
            input: p["action"].clone(),
            subagent: None,
        },
        "function_call_output" | "custom_tool_call_output" => {
            let text = flatten(&p["output"]);
            Kind::ToolResult {
                tool_use_id: p["call_id"].as_str().map_or_else(String::new, |s| s.to_string()),
                bytes: text.len(),
                text,
                is_error: false,
            }
        }
        _ => return Vec::new(),
    };
    vec![record(kind)]
}

/// Content as text: a string, or its items' `text` with anything else (an
/// image) as a `[type]` placeholder; an output object's `content`/`output`.
fn flatten(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| match i["text"].as_str() {
                Some(t) => t.to_string(),
                None => format!("[{}]", i["type"].as_str().unwrap_or("item")),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(o) => o.get("content").or(o.get("output")).map(flatten).unwrap_or_default(),
        _ => String::new(),
    }
}

/// How much of the rollout's end is read for an interrupt.
const TAIL: u64 = 256 * 1024;

/// Did Codex record an Esc — `turn_aborted` — stamped at or after
/// `since_ms`? Newest line first, stopping at the first one older than
/// that: everything before belongs to an earlier turn.
pub(super) fn interrupted_since(transcript: &Path, since_ms: u64) -> bool {
    crate::status::read_tail(transcript, TAIL).is_some_and(|tail| scan(&tail, since_ms))
}

fn scan(tail: &[u8], since_ms: u64) -> bool {
    const MARK: &[u8] = b"\"turn_aborted\"";
    if !tail.windows(MARK.len()).any(|w| w == MARK) {
        return false;
    }
    for line in tail.rsplit(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let Ok(h) = serde_json::from_slice::<Head>(line) else {
            continue; // the tail's first line, cut mid-record
        };
        let Some(ts) = h.timestamp.as_deref().and_then(super::parse_utc_ms) else {
            continue;
        };
        if ts < since_ms {
            return false;
        }
        if h.kind.as_deref() == Some("event_msg") && h.payload.as_ref().and_then(|p| p.kind.as_deref()) == Some("turn_aborted") {
            return true;
        }
    }
    false
}
