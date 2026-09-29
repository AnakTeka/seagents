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
