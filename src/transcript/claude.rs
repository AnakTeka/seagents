//! Claude Code's transcript: JSONL under `$CLAUDE_CONFIG_DIR/projects/…`,
//! one record per line, appended as the conversation happens. The one parser
//! of it — the offset index, `read` and the turn ledger's interrupt check all
//! go through here.
//!
//! Shapes (measured over this machine's 462 transcripts, Claude Code up to
//! 2.1.284):
//!
//! - Tree nodes are the lines with a `uuid`: `user`, `assistant`, and also
//!   `system`, `attachment` and (older versions) `progress` — which sit in
//!   the parent chain (a prompt's parent is often the previous turn's
//!   `system` `turn_duration` note), so they are indexed though never shown.
//!   Everything else (`file-history-snapshot`, `mode`, `permission-mode`,
//!   `ai-title`, `last-prompt`, `queue-operation`, …) has no uuid and is
//!   not part of the conversation.
//! - The parent is `parentUuid`; a compaction's `compact_boundary` has none
//!   and names the pre-compaction tip in `logicalParentUuid` instead, which
//!   is taken as its parent (in 5 of 15 boundaries that tip is not in the
//!   file; [`super::mainline`] bridges that gap).
//! - One content block per line nearly always (streamed blocks each get
//!   their own line and uuid); `user` content is a string or blocks (`text`,
//!   `image`, `document`, `tool_result`), a tool result's content a string
//!   or blocks too (`text`, `image`, `tool_reference`).
//! - `isMeta` user lines are injected context (image notes, a skill's body),
//!   not the user's words: in the tree, never records.
//! - About 34 uuids repeat within their file (resumes copying a line); the
//!   first occurrence is the node.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::index::{parse_uuid, Node, NodeKind};
use super::model::{Kind, Record, Subagent};

/// How much of the transcript's end is read. An interrupt that matters is
/// after the current turn's prompt, so it is near the end; a turn whose own
/// records outgrow this (a huge tool result) would hide one, and reads as
/// still in its turn until the next prompt or `Stop` — the safe direction.
const TAIL: u64 = 256 * 1024;

/// The text Claude writes as a `user` record when Esc cuts a turn short:
/// `[Request interrupted by user]`, or `… for tool use]` mid tool call
/// (verified 2026-09-29 on 2.1.284: such records also carry
/// `interruptedMessageId` and the turn's `promptId`, but versions before
/// ~2.1.150 wrote the text alone, so the text is what is matched).
const INTERRUPT: &str = "[Request interrupted by user";

pub(super) fn interrupted_since(transcript: &Path, since_ms: u64) -> bool {
    crate::status::read_tail(transcript, TAIL).is_some_and(|tail| scan(&tail, since_ms))
}

/// Newest line first, stopping at the first one stamped before `since_ms`:
/// everything older belongs to an earlier turn. The tail's first line may be
/// cut mid-record; it fails to parse and is skipped like any other junk.
fn scan(tail: &[u8], since_ms: u64) -> bool {
    // Nearly every call (a turn in progress, never interrupted) ends here,
    // without parsing a line.
    if !contains(tail, INTERRUPT.as_bytes()) {
        return false;
    }
    for line in tail.rsplit(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let Some(node) = head(line, 0, &[]).filter(|n| n.ts != 0) else {
            continue; // snapshots, titles: no timestamp, not a turn event
        };
        if node.ts < since_ms {
            return false;
        }
        // A tool result quoting the marker is a `tool_result` record, and the
        // assistant saying it an `assistant` one: only the user's own counts.
        let interrupt = node.kind == NodeKind::User
            && records(line, &node)
                .iter()
                .any(|r| matches!(&r.kind, Kind::User { text } if text.starts_with(INTERRUPT)));
        if interrupt {
            return true;
        }
    }
    false
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The fields of a line the tree needs, borrowed where the JSON allows;
/// everything else in the line is skipped unparsed, which is what keeps
/// indexing a 50 MB transcript quick.
#[derive(Deserialize)]
struct Head<'a> {
    #[serde(borrow, default)]
    uuid: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default, rename = "parentUuid")]
    parent: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default, rename = "logicalParentUuid")]
    logical_parent: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default, rename = "type")]
    kind: Option<std::borrow::Cow<'a, str>>,
}

/// The tree node a line is, if any (offset and length are the index's).
pub(super) fn head(line: &[u8], _at: u64, _prior: &[Node]) -> Option<Node> {
    let h: Head = serde_json::from_slice(line).ok()?;
    let uuid = parse_uuid(h.uuid.as_deref()?)?;
    let parent = h
        .parent
        .as_deref()
        .or(h.logical_parent.as_deref())
        .and_then(parse_uuid)
        .unwrap_or(0);
    let kind = match h.kind.as_deref() {
        Some("user") => NodeKind::User,
        Some("assistant") => NodeKind::Assistant,
        _ => NodeKind::Other,
    };
    let ts = h.timestamp.as_deref().and_then(super::parse_utc_ms).unwrap_or(0);
    Some(Node { offset: 0, len: 0, kind, ts, uuid, parent })
}

/// The records of one line, in block order; none for a line that is not
/// conversation or does not parse.
pub(super) fn records(line: &[u8], _node: &Node) -> Vec<Record> {
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return Vec::new();
    };
    let role = v["type"].as_str().unwrap_or("");
    let Some(uuid) = v["uuid"].as_str() else {
        return Vec::new();
    };
    if !matches!(role, "user" | "assistant") || v["isMeta"] == true {
        return Vec::new();
    }
    let record = |kind| Record {
        uuid: uuid.to_string(),
        parent_uuid: v["parentUuid"]
            .as_str()
            .or(v["logicalParentUuid"].as_str())
            .map(str::to_string),
        timestamp: v["timestamp"].as_str().map(str::to_string),
        kind,
    };
    let content = &v["message"]["content"];
    if let Some(text) = content.as_str() {
        return vec![record(if role == "user" {
            Kind::User { text: text.to_string() }
        } else {
            Kind::Assistant { text: text.to_string() }
        })];
    }
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // A user's text, images and documents are one message: runs of them
    // become one record, split only by a tool result between them.
    let mut said: Vec<String> = Vec::new();
    let flush = |said: &mut Vec<String>, out: &mut Vec<Record>| {
        if !said.is_empty() {
            out.push(record(Kind::User { text: said.join("\n") }));
            said.clear();
        }
    };
    for b in blocks {
        let t = b["type"].as_str().unwrap_or("");
        match (role, t) {
            ("user", "tool_result") => {
                flush(&mut said, &mut out);
                let text = flatten(&b["content"]);
                out.push(record(Kind::ToolResult {
                    tool_use_id: b["tool_use_id"].as_str().unwrap_or("").to_string(),
                    bytes: text.len(),
                    text,
                    is_error: b["is_error"] == true,
                }));
            }
            ("user", "text") => said.push(b["text"].as_str().unwrap_or("").to_string()),
            ("user", other) => said.push(format!("[{other}]")),
            ("assistant", "text") => {
                out.push(record(Kind::Assistant { text: b["text"].as_str().unwrap_or("").to_string() }))
            }
            ("assistant", "thinking") => {
                out.push(record(Kind::Thinking { text: b["thinking"].as_str().unwrap_or("").to_string() }))
            }
            ("assistant", "tool_use" | "server_tool_use") => {
                let name = b["name"].as_str().unwrap_or("").to_string();
                let input = b["input"].clone();
                out.push(record(Kind::ToolUse {
                    id: b["id"].as_str().unwrap_or("").to_string(),
                    subagent: subagent_call(&name, &input),
                    name,
                    input,
                }))
            }
            // redacted_thinking, fallback markers, whatever comes next.
            _ => {}
        }
    }
    flush(&mut said, &mut out);
    out
}

/// A tool result's content as text: a string, or its blocks' text with
/// anything else (images, tool references) as a `[type]` placeholder.
fn flatten(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or("").to_string(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A call that spawns a real subagent — `Agent` (`Task` before ~2.1.100)
/// — labelled from its own input; [`subagents`] refines it where a sidecar
/// survives.
fn subagent_call(name: &str, input: &Value) -> Option<Subagent> {
    if !matches!(name, "Agent" | "Task") {
        return None;
    }
    Some(Subagent {
        // Claude Code's own default when the call names no type.
        agent_type: input["subagent_type"].as_str().unwrap_or("general-purpose").to_string(),
        description: input["description"].as_str().unwrap_or("").to_string(),
        transcript: None,
    })
}

/// The real subagents of `transcript`, by the id of the tool call that
/// spawned each: `<session>/subagents/agent-<id>.meta.json` sidecars carry
/// `toolUseId`, `agentType` and `description` (the call's own, verbatim),
/// beside the subagent's `agent-<id>.jsonl`. Built-in helpers
/// (`agent-aprompt_suggestion-…`, `agent-acompact-…`,
/// `agent-aside_question-…`) are not subagents — `aside_question` re-records
/// the parent conversation nearly whole — and are skipped. About half the
/// real ones have no sidecar; their call's own input labels them.
pub(super) fn subagents(transcript: &Path) -> HashMap<String, Subagent> {
    let dir = transcript.with_extension("").join("subagents");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".meta.json")) else {
            continue;
        };
        if is_helper(stem) {
            continue;
        }
        let Some(meta) = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
            continue;
        };
        let Some(id) = meta["toolUseId"].as_str() else {
            continue;
        };
        let jsonl = dir.join(format!("{stem}.jsonl"));
        out.insert(
            id.to_string(),
            Subagent {
                agent_type: meta["agentType"].as_str().unwrap_or("general-purpose").to_string(),
                description: meta["description"].as_str().unwrap_or("").to_string(),
                transcript: jsonl.exists().then(|| jsonl.display().to_string()),
            },
        );
    }
    out
}

/// `agent-a<word>-…`: a helper. A real subagent is `agent-a<hex>`, no
/// hyphen after the prefix.
fn is_helper(stem: &str) -> bool {
    stem.strip_prefix("agent-").is_none_or(|rest| rest.contains('-'))
}
