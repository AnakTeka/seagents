//! Screen reconstruction for state detection.
//!
//! The raw teed byte stream is replayed through a VT emulator to rebuild the
//! visible grid; classification of that grid is herdr's vendored manifest
//! engine (`vendor/herdr_detect/`), not code here. This module used to carry
//! hand-written blocked-detection rules — they were replaced wholesale by the
//! engine, which scopes rules to screen regions with priorities and is kept
//! current upstream.
//!
//! Also here: keeping those rules current (the manifest-update helpers behind
//! `update-manifests`, the HUD's background refresh and the health warning),
//! and `debug-screen`, which shows the engine's verdict on a capture.

use std::fs;
use std::path::Path;
use std::thread;

use crate::vendor;

/// Render the visible screen from a raw PTY byte stream at the given geometry.
/// Processing a tail is fine: Claude repaints the whole screen frequently, so
/// the final grid converges to the true current view.
pub fn render(data: &[u8], rows: u16, cols: u16) -> String {
    let rows = rows.max(1);
    let cols = cols.max(1);
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(data);
    parser.screen().contents()
}

/// What the agent's input box holds, from a raw PTY byte stream rendered at
/// the given geometry: `Some("")` when it is empty (or shows only its
/// placeholder), `Some(text)` when something is typed in it, None when no
/// box is on screen or the cursor is not in it — nothing to go by.
///
/// The box is the last row that starts with `glyph` (Claude's `❯`, Codex's
/// `›`) and the rows under it, up to a blank row or a horizontal rule;
/// its text is what follows the glyph. A placeholder (`Try "…"`, `Ask Codex
/// to do anything`) is drawn dim, which vt100 0.15 does not record, so it is
/// told from typed text by the cursor instead: both tools park the real
/// cursor at the edit point, which for an empty box is where the text would
/// begin (measured, Claude 2.1.284 and Codex 0.152.0). Hence "the cursor is
/// in the box" as the condition for any answer — a placeholder must never
/// read as typed text. (A one-line draft whose cursor was moved to its
/// start reads as empty; the paste then lands in front of it.)
pub(crate) fn input_box(data: &[u8], rows: u16, cols: u16, glyph: &str) -> Option<String> {
    let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), 0);
    parser.process(data);
    let screen = parser.screen();
    let lines: Vec<String> = screen.rows(0, cols.max(1)).collect();
    let top = lines.iter().rposition(|l| l.trim_start().starts_with(glyph))?;
    let rule = |l: &str| l.trim_start().starts_with('─');
    let end = lines[top + 1..]
        .iter()
        .position(|l| l.trim().is_empty() || rule(l))
        .map_or(lines.len(), |n| top + 1 + n);
    let (crow, ccol) = screen.cursor_position();
    let crow = usize::from(crow);
    if !(top..end).contains(&crow) {
        return None;
    }
    // The first cell after the glyph that holds text.
    let cell_text = |col: u16| screen.cell(top as u16, col).map(|c| c.contents()).unwrap_or_default();
    let glyph_col = (0..cols).find(|&c| !cell_text(c).trim().is_empty())?;
    let start = (glyph_col + 1..cols).find(|&c| !cell_text(c).trim().is_empty());
    let first = lines[top].trim_start().strip_prefix(glyph).unwrap_or("").trim();
    let text: Vec<&str> = std::iter::once(first).chain(lines[top + 1..end].iter().map(|l| l.trim())).collect();
    let empty = first.is_empty() || (end == top + 1 && crow == top && Some(ccol) == start);
    Some(if empty { String::new() } else { text.join("\n") })
}

/// Dev tool (`seagents debug-screen`): render a .raw file and show the
/// detection engine's verdict (the equivalent of herdr's `agent explain`) for
/// tuning against real captures. `json` adds the full evaluated-rule breakdown.
pub(crate) fn debug_screen(
    file: &Path,
    cols: u16,
    rows: u16,
    agent_name: &str,
    json: bool,
) -> i32 {
    let data = std::fs::read(file).unwrap_or_default();
    let screen = render(&data, rows, cols);
    let title = crate::status::current_title(&data).unwrap_or_default();
    println!("{screen}");
    let explain = match vendor::herdr_detect::parse_agent_label(agent_name) {
        Some(agent) => vendor::herdr_detect::manifest::explain_with_input(
            agent,
            vendor::herdr_detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &title,
                osc_progress: "",
            },
        ),
        None => {
            eprintln!("unknown agent {agent_name:?}");
            return 2;
        }
    };
    println!("\n---");
    println!("osc_title = {title:?}");
    println!(
        "state = {} (manifest {} v{})",
        vendor::herdr_detect::manifest::agent_state_label(explain.state),
        explain
            .source
            .as_ref()
            .map(|s| s.kind())
            .unwrap_or("none"),
        explain.manifest_version.as_deref().unwrap_or("?"),
    );
    match &explain.matched_rule {
        Some(rule) => println!(
            "matched_rule = {} (priority {}, region {})",
            rule.id, rule.priority, rule.region
        ),
        None => println!(
            "matched_rule = none ({})",
            explain.fallback_reason.as_deref().unwrap_or("no fallback")
        ),
    }
    if let Some(warning) = &explain.warning {
        println!("warning = {warning}");
    }
    let glyph = crate::adapter::for_agent(agent_name).map(|a| a.send.input_glyph).unwrap_or_default();
    if !glyph.is_empty() {
        println!("input_box = {:?}", input_box(&data, rows, cols, &glyph));
    }
    if json {
        let value = vendor::herdr_detect::manifest::explain_to_json_value(&explain);
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
    }
    0
}


/// Explicit `update-manifests`: fetch, report per-agent results, reload.
pub(crate) fn update_manifests_now() -> i32 {
    match vendor::herdr_detect::manifest_update::check_and_update() {
        Ok(output) => {
            reload_updated_manifests(&output);
            for (agent, remote) in &output.status.agents {
                let version = remote.cached_version.as_deref().unwrap_or("-");
                match &remote.last_error {
                    Some(err) => println!("{agent:<10} {:<8} {version}  {err}", remote.last_result),
                    None => println!("{agent:<10} {:<8} {version}", remote.last_result),
                }
            }
            0
        }
        Err(err) => {
            eprintln!("manifest update failed: {err}");
            1
        }
    }
}

/// Refresh the in-process manifest cache for every agent whose rules moved.
/// Not just the ones this run committed: herdr and seagents share one remote
/// cache directory, so a manifest the other tool fetched leaves `updated`
/// empty while the loaded rules are stale.
fn reload_updated_manifests(output: &vendor::herdr_detect::manifest_update::ManifestUpdateOutput) {
    let activated = vendor::herdr_detect::manifest_update::agents_needing_cache_reload(output);
    if !activated.is_empty() {
        vendor::herdr_detect::manifest::reload_manifests_for_agents(&activated);
    }
}

/// Background refresh for the HUD: at most once per day, silently.
/// `SEAGENTS_NO_UPDATE=1` disables the automatic phone-home entirely
/// (`seagents update-manifests` still works on demand).
pub(crate) fn spawn_manifest_update() {
    if std::env::var_os("SEAGENTS_NO_UPDATE").is_some() {
        return;
    }
    thread::spawn(|| {
        // Throttle on our own attempt stamp, not just the updater's recorded
        // last check: a failed catalog fetch never advances last_check_unix,
        // which would otherwise mean one curl attempt per HUD launch while
        // offline.
        let stamp = vendor::herdr_detect::config::state_dir()
            .join("agent-detection/last-attempt");
        let fresh_attempt = fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|e| e.as_secs() < 24 * 3600);
        if fresh_attempt {
            return;
        }
        if let Some(parent) = stamp.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&stamp, b"");
        if let Ok(output) = vendor::herdr_detect::manifest_update::check_and_update() {
            // The manifest cache is behind an RwLock; the HUD's next poll
            // picks up the new rules.
            reload_updated_manifests(&output);
        }
    });
}

/// One line of manifest-update health for the HUD footer / status output —
/// the vendored engine only improves while updates apply, and a persistent
/// failure (worst case: manifests requiring a newer engine after an upstream
/// bump) would otherwise rot silently behind SEAGENTS_DEBUG.
pub(crate) fn manifest_health_warning() -> Option<String> {
    let status = vendor::herdr_detect::manifest_update::load_status();
    let failing: Vec<&String> = status
        .agents
        .iter()
        .filter(|(_, s)| s.last_result == "failed")
        .map(|(agent, _)| agent)
        .collect();
    if failing.is_empty() {
        return None;
    }
    let engine_drift = status.agents.values().any(|s| {
        s.last_error
            .as_deref()
            .is_some_and(|e| e.contains("requires engine"))
    });
    Some(if engine_drift {
        "detection rules need a newer engine: re-sync src/vendor/herdr_detect (see VENDORED.md)"
            .to_string()
    } else {
        format!(
            "manifest update failing for {} agent(s), try `seagents update-manifests`",
            failing.len()
        )
    })
}

#[cfg(test)]
mod tests {
    //! Regression scenarios from live captures, now asserted against the
    //! vendored engine + bundled claude manifest instead of local rules.

    use crate::vendor::herdr_detect::{detect_agent_with_osc, Agent, AgentState};

    /// A screen drawn the way Claude Code 2.1.284 draws its prompt box
    /// (rules, `❯` + NBSP, the placeholder dim), with the cursor parked at
    /// `cursor` (1-based row;col).
    fn claude_box(box_rows: &[&str], cursor: &str) -> Vec<u8> {
        let rule = "─".repeat(40);
        let mut s = format!("\x1b[H\x1b[2J❯ earlier prompt\r\n  answer\r\n\r\n{rule}\r\n");
        for r in box_rows {
            s.push_str(r);
            s.push_str("\r\n");
        }
        s.push_str(&format!("{rule}\r\n\r\n  ⏵⏵ bypass permissions on\x1b[{cursor}H"));
        s.into_bytes()
    }

    #[test]
    fn the_input_box_is_empty_under_a_placeholder_and_holds_what_was_typed() {
        let read = |data: &[u8]| super::input_box(data, 12, 40, "❯");
        // Empty: the placeholder, with the cursor where text would begin.
        assert_eq!(read(&claude_box(&["❯\u{a0}\x1b[2mTry \"refactor x\"\x1b[22m"], "5;3")).as_deref(), Some(""));
        assert_eq!(read(&claude_box(&["❯\u{a0}"], "5;3")).as_deref(), Some(""));
        // Typed: the cursor after the text.
        assert_eq!(read(&claude_box(&["❯\u{a0}hello draft"], "5;14")).as_deref(), Some("hello draft"));
        assert_eq!(
            read(&claude_box(&["❯ line one", "  line two"], "6;11")).as_deref(),
            Some("line one\nline two")
        );
        // Several lines are text wherever the cursor is in them.
        assert_eq!(read(&claude_box(&["❯ one", "  two"], "5;3")).as_deref(), Some("one\ntwo"));
        // The cursor outside the box: nothing to go by (a placeholder must
        // never read as a draft).
        assert_eq!(read(&claude_box(&["❯\u{a0}Try \"x\""], "1;1")), None);
        // No box at all.
        assert_eq!(read(b"$ plain shell\r\n"), None);
    }

    #[test]
    fn codexs_composer_ends_at_the_blank_row_above_its_footer() {
        let screen = |composer: &str, cursor: &str| {
            format!(
                "\x1b[H\x1b[2J› Write an essay\r\n■ Conversation interrupted\r\n\r\n{composer}\r\n\r\n  gpt-5.6-luna low · /w\x1b[{cursor}H"
            )
            .into_bytes()
        };
        let read = |data: &[u8]| super::input_box(data, 10, 40, "›");
        assert_eq!(read(&screen("\x1b[1m›\x1b[22m \x1b[2mAsk Codex to do anything\x1b[22m", "4;3")).as_deref(), Some(""));
        assert_eq!(read(&screen("› cx one\r\n  cx two", "5;9")).as_deref(), Some("cx one\ncx two"));
    }

    fn claude_state(screen: &str, title: &str) -> AgentState {
        detect_agent_with_osc(Some(Agent::Claude), screen, title, "").state
    }

    #[test]
    fn bash_permission_prompt_is_blocked() {
        let screen = "\
Bash command
  rm -rf build/

Do you want to proceed?
❯ 1. Yes
  2. No

esc to cancel";
        assert_eq!(claude_state(screen, ""), AgentState::Blocked);
    }

    #[test]
    fn chat_reply_with_question_is_not_blocked() {
        // Claude's own reply text plus the always-present `❯` input prompt used
        // to trip our old blocked detector (seen live: an idle agent in
        // .dotfiles). The engine's prompt-box idle rule outranks loose blockers.
        let screen = "\
● Hi! I'm here and ready to help in your dotfiles repo. What would you like to work on?

──────────────────────────────────────────────
❯
──────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(claude_state(screen, ""), AgentState::Idle);
    }

    #[test]
    fn working_title_wins() {
        // The braille-spinner OSC title is the highest-priority working signal.
        let screen = "● Reading src/main.rs\n  Analyzing the pty relay…";
        assert_eq!(claude_state(screen, "⠂ Claude Code"), AgentState::Working);
    }

    #[test]
    fn idle_title_classifies_idle() {
        assert_eq!(claude_state("", "✳ Claude Code"), AgentState::Idle);
    }

    /// The screen that prompted `manifest/patches.rs`: the turn ended hours
    /// ago (`done 11:25 AM`) but a background dev server keeps the footer's
    /// `· 1 shell ·` chip lit, which upstream reads as the agent working.
    const BACKGROUND_SHELL_AT_PROMPT: &str = "\
● Committed (505b77b) and the app is back up with the judgment-free verdicts.

✻ Cooked for 2m 38s · done 11:25 AM · 1 shell still running
──────────────────────────────────────────────
❯
──────────────────────────────────────────────
  personal | Opus 4.8 [high] | ~/projects/acme/adhoc/20260612_doc_parser
  ⏵⏵ bypass permissions on · 1 shell · ← for agents";

    #[test]
    fn background_shell_at_the_prompt_is_idle() {
        assert_eq!(
            claude_state(
                BACKGROUND_SHELL_AT_PROMPT,
                "✳ Find BSC tagging configuration"
            ),
            AgentState::Idle
        );
    }

    #[test]
    fn background_shell_during_a_live_turn_stays_working() {
        // Same footer chip, but the turn is live — the demoted rule is not
        // what decides this screen.
        let screen = "\
● Reading src/main.rs

✻ Cooking… (12s · ↑ 1.2k tokens · esc to interrupt)
──────────────────────────────────────────────
❯
──────────────────────────────────────────────
  ⏵⏵ bypass permissions on · 1 shell · ← for agents";
        assert_eq!(claude_state(screen, ""), AgentState::Working);
        assert_eq!(claude_state(screen, "⠂ Claude Code"), AgentState::Working);
    }

    #[test]
    fn background_shell_does_not_mask_a_permission_prompt() {
        let screen = "\
● Bash(rm -rf build/)

Do you want to proceed?
❯ 1. Yes
  2. No

esc to cancel
  ⏵⏵ bypass permissions on · 1 shell · ← for agents";
        assert_eq!(claude_state(screen, "✳ Claude Code"), AgentState::Blocked);
    }
}
