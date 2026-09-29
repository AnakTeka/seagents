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
/// first string field, else nothing. The field names are the common
/// vocabulary of coding agents' tools, not one agent's.
fn short_arg(input: &Value) -> String {
    const TELLING: &[&str] = &["command", "file_path", "path", "notebook_path", "pattern", "url", "query", "description", "skill"];
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    let value = TELLING
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .or_else(|| obj.values().find_map(Value::as_str))
        .unwrap_or("");
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

#[cfg(test)]
mod tests {
    use super::super::model::Subagent;
    use super::*;

    fn rec(kind: Kind) -> Record {
        Record { uuid: "u".into(), parent_uuid: None, timestamp: None, kind }
    }

    fn fixture() -> Vec<Record> {
        vec![
            rec(Kind::User { text: "fix the tests\n\nplease".into() }),
            rec(Kind::Thinking { text: "the parser is wrong here".into() }),
            rec(Kind::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"description": "run", "command": "cargo test\n--all"}),
                subagent: None,
            }),
            rec(Kind::ToolResult { tool_use_id: "t1".into(), text: "\nok: 3 passed\nmore".into(), bytes: 19, is_error: false }),
            rec(Kind::ToolUse {
                id: "t2".into(),
                name: "Agent".into(),
                input: serde_json::json!({"description": "Scan"}),
                subagent: Some(Subagent { agent_type: "Explore".into(), description: "Scan".into(), transcript: None }),
            }),
            rec(Kind::ToolResult { tool_use_id: "t2".into(), text: String::new(), bytes: 0, is_error: true }),
            rec(Kind::Assistant { text: "Fixed.\nAll green.".into() }),
            rec(Kind::User { text: "thanks".into() }),
        ]
    }

    #[test]
    fn each_kind_collapses_to_its_line() {
        assert_eq!(render(&fixture(), false), [
            "│ fix the tests",
            "│",
            "│ please",
            "",
            "⋯ thinking (5 words)",
            "● Bash(cargo test …)",
            "  ⎿ ok: 3 passed (19 bytes)",
            "▸ subagent: Explore — Scan",
            "  ⎿ ✗ (no output) (0 bytes)",
            "Fixed.",
            "All green.",
            "",
            "│ thanks",
        ]);
    }

    #[test]
    fn raw_shows_everything() {
        let out = render(&fixture(), true);
        assert!(out.contains(&"⋯ the parser is wrong here".to_string()));
        assert!(out.contains(&"      \"command\": \"cargo test\\n--all\",".to_string()), "{out:#?}");
        assert!(out.contains(&"  ⎿ ".to_string()) && out.contains(&"    ok: 3 passed".to_string()) && out.contains(&"    more".to_string()));
    }

    #[test]
    fn a_tool_argument_is_its_telling_field_shortened() {
        assert_eq!(short_arg(&serde_json::json!({"file_path": "/a/b.rs", "old_string": "x"})), "/a/b.rs");
        assert_eq!(short_arg(&serde_json::json!({"n": 3, "foo": "bar"})), "bar");
        assert_eq!(short_arg(&serde_json::json!("str")), "");
        let long = short_arg(&serde_json::json!({"command": "x".repeat(300)}));
        assert_eq!(long.chars().count(), SHORT);
        assert!(long.ends_with('…'));
    }
}
