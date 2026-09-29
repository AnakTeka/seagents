//! Claude Code's transcript: JSONL under `$CLAUDE_CONFIG_DIR/projects/…`,
//! one record per line, appended as the conversation happens.

use std::path::Path;

use serde_json::Value;

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

/// Newest record first, stopping at the first one stamped before `since_ms`:
/// everything older belongs to an earlier turn. The tail's first line may be
/// cut mid-record; it fails to parse and is skipped like any other junk.
fn scan(tail: &[u8], since_ms: u64) -> bool {
    // Nearly every call (a turn in progress, never interrupted) ends here,
    // without parsing a line.
    if !contains(tail, INTERRUPT.as_bytes()) {
        return false;
    }
    for line in tail.rsplit(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(at) = record["timestamp"].as_str().and_then(super::parse_utc_ms) else {
            continue; // summaries, snapshots: no timestamp, not a turn event
        };
        if at < since_ms {
            return false;
        }
        if is_interrupt(&record) {
            return true;
        }
    }
    false
}

/// A `user` record whose text is the interrupt marker. (A tool result that
/// merely quotes it is a `tool_result` block, not a `text` one.)
fn is_interrupt(record: &Value) -> bool {
    if record["type"] != "user" {
        return false;
    }
    let text_starts = |t: &Value| t.as_str().is_some_and(|s| s.starts_with(INTERRUPT));
    match &record["message"]["content"] {
        Value::String(_) => text_starts(&record["message"]["content"]),
        Value::Array(blocks) => blocks
            .iter()
            .any(|b| b["type"] == "text" && text_starts(&b["text"])),
        _ => false,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record as Claude 2.1.284 writes it (trimmed).
    fn rec(kind: &str, ts: &str, text: &str) -> String {
        serde_json::json!({
            "type": kind, "timestamp": ts, "promptId": "p",
            "message": {"role": kind, "content": [{"type": "text", "text": text}]},
        })
        .to_string()
    }

    fn interrupt(ts: &str) -> String {
        let mut v: Value = serde_json::from_str(&rec("user", ts, "[Request interrupted by user]")).unwrap();
        v["interruptedMessageId"] = "msg_1".into();
        v.to_string()
    }

    fn ms(ts: &str) -> u64 {
        super::super::parse_utc_ms(ts).unwrap()
    }

    #[test]
    fn an_interrupt_after_the_prompt_counts_and_one_before_it_does_not() {
        let t = [
            rec("user", "2026-09-29T06:00:00.000Z", "first"),
            interrupt("2026-09-29T06:00:05.000Z"),
            rec("user", "2026-09-29T06:01:00.000Z", "second"),
            rec("assistant", "2026-09-29T06:01:02.000Z", "streaming…"),
        ]
        .join("\n");
        // The second prompt's turn: the old interrupt is before it.
        assert!(!scan(t.as_bytes(), ms("2026-09-29T06:01:00.000Z")));
        // The first prompt's turn was interrupted.
        assert!(scan(t.as_bytes(), ms("2026-09-29T06:00:00.000Z")));

        let t = format!("{t}\n{}\n", interrupt("2026-09-29T06:01:03.500Z"));
        assert!(scan(t.as_bytes(), ms("2026-09-29T06:01:00.000Z")));
    }

    #[test]
    fn tool_use_interrupts_count_and_look_alikes_and_junk_do_not() {
        let tool = rec("user", "2026-09-29T06:00:09.000Z", "[Request interrupted by user for tool use]");
        let since = ms("2026-09-29T06:00:00.000Z");
        assert!(scan(format!("cut-off junk}}\n{tool}\n").as_bytes(), since));

        // The marker quoted inside a tool result, or said by the assistant.
        let quoted = serde_json::json!({"type":"user","timestamp":"2026-09-29T06:00:09.000Z",
            "message":{"content":[{"type":"tool_result","content":"[Request interrupted by user]"}]}})
            .to_string();
        let said = rec("assistant", "2026-09-29T06:00:09.000Z", "[Request interrupted by user]");
        assert!(!scan(format!("{quoted}\n{said}\n").as_bytes(), since));
        assert!(!scan(b"", since));
    }
}
