//! Screen reconstruction for state detection.
//!
//! The raw teed byte stream is replayed through a VT emulator to rebuild the
//! visible grid; classification of that grid is herdr's vendored manifest
//! engine (`vendor/herdr_detect/`), not code here. This module used to carry
//! hand-written blocked-detection rules — they were replaced wholesale by the
//! engine, which scopes rules to screen regions with priorities and is kept
//! current upstream.

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
