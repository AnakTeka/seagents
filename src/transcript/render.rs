//! Records → text lines, by the collapse rules (SESSIONS-SPEC "Collapse
//! rules"): what makes a thousand-turn session skimmable. The user's words
//! and the assistant's text in full; the rest one line each.
//!
//! ```text
//! │ what the user said, every line behind a bar
//!
//! the assistant's text
//! ⋯ thinking (412 words)
//! ● Bash(cargo test)
//!   ⎿ test result: ok. 12 passed (1843 bytes)
//! ▸ subagent: Explore — Analyze the parser
//! ```
//!
//! `raw` turns collapsing off: thinking, tool input and tool output in full.
//! No wrapping — the terminal does that, and a pipe wants the lines whole.

use serde_json::Value;

use super::model::{Kind, Record};

/// How much of a tool's argument or a result's first line a collapsed line
/// keeps, in characters.
const SHORT: usize = 100;

pub(crate) fn render(records: &[Record], raw: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut after_user = false;
    for r in records {
        let is_user = matches!(r.kind, Kind::User { .. });
        // A blank line around each user message: the turns are what the
        // eye scans for.
        if (is_user || after_user) && !out.is_empty() {
            out.push(String::new());
        }
        after_user = is_user;
        match &r.kind {
            Kind::User { text } => {
                out.extend(text.lines().map(|l| if l.is_empty() { "│".to_string() } else { format!("│ {l}") }))
            }
            Kind::Assistant { text } => out.extend(text.lines().map(str::to_string)),
            Kind::Thinking { text } if raw => out.extend(text.lines().map(|l| format!("⋯ {l}"))),
            Kind::Thinking { text } => out.push(format!("⋯ thinking ({} words)", text.split_whitespace().count())),
            Kind::ToolUse { name, input, subagent, .. } => {
                out.push(match subagent {
                    Some(s) => format!("▸ subagent: {} — {}", s.agent_type, s.description),
                    None => format!("● {name}({})", short_arg(input)),
                });
                if raw {
                    let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
                    out.extend(pretty.lines().map(|l| format!("    {l}")));
                }
            }
            Kind::ToolResult { text, bytes, is_error, .. } => {
                let mark = if *is_error { "✗ " } else { "" };
                if raw {
                    let mut lines = text.lines();
                    out.push(format!("  ⎿ {mark}{}", lines.next().unwrap_or("")));
                    out.extend(lines.map(|l| format!("    {l}")));
                } else {
                    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                    let first = if first.is_empty() { "(no output)".to_string() } else { truncate(first.trim(), SHORT) };
                    out.push(format!("  ⎿ {mark}{first} ({bytes} bytes)"));
                }
            }
        }
    }
    out
}

/// A tool call's argument in a few words: the first of the fields that
/// name what a call is about (a command, a path, a pattern, …), else its
/// first string field, else nothing; an argv (Codex's `command`) joined
/// with spaces; a free-text input (a Codex custom tool's script) itself.
/// The field names are the common vocabulary of coding agents' tools, not
/// one agent's.
fn short_arg(input: &Value) -> String {
    const TELLING: &[&str] = &["command", "cmd", "file_path", "path", "notebook_path", "pattern", "url", "query", "description", "skill"];
    let text = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) if !a.is_empty() && a.iter().all(Value::is_string) => {
            Some(a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
        }
        _ => None,
    };
    let value = match input {
        Value::String(s) => s.clone(),
        Value::Object(obj) => TELLING
            .iter()
            .find_map(|k| obj.get(*k).and_then(text))
            .or_else(|| obj.values().find_map(text))
            .unwrap_or_default(),
        _ => String::new(),
    };
    let value = value.as_str();
    let first = value.lines().next().unwrap_or("");
    let more = value.lines().nth(1).is_some();
    let short = truncate(first, SHORT);
    if more && !short.ends_with('…') {
        format!("{short} …")
    } else {
        short
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max - 1).collect();
    t.push('…');
    t
}
