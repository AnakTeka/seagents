//! Milestone 2b: detect the "blocked" state (agent waiting on the user).
//!
//! Unlike working/idle, blocked has no OSC-title signal — it lives in the
//! rendered screen (a permission or selection prompt). So we replay the raw
//! byte stream through a VT emulator to reconstruct the visible grid, then
//! apply rules ported from herdr's `claude.toml`. We only need "blocked" here;
//! working/idle already come from the title in `status.rs`.

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

/// Does the rendered screen show Claude waiting on the user for a decision?
///
/// Ported from the blocked rules in herdr's `claude.toml`: a decision prompt
/// (permission / selection) plus a selectable option, minus the known
/// non-blocking screens (transcript viewer, model picker) that share phrasing.
pub fn is_blocked(screen: &str) -> bool {
    let lower = screen.to_lowercase();

    // Skip states — screens that look prompt-like but aren't blocking input.
    if lower.contains("showing detailed transcript") {
        return false;
    }
    if lower.contains("select model")
        && lower.contains("enter to set as default")
        && !lower.contains("do you want to proceed?")
    {
        return false;
    }

    // "waiting for permission" is an unambiguous block on its own.
    if lower.contains("waiting for permission") {
        return true;
    }

    // A decision is being requested.
    let asks_decision = lower.contains("do you want to proceed?")
        || lower.contains("run a dynamic workflow?")
        || lower.contains("enter to select")
        || lower.contains("enter to confirm")
        || lower.contains("do you want to")
        || lower.contains("would you like to");

    if !asks_decision {
        return false;
    }

    // ...confirmed by either an explicit cancel/reject footer or a selectable
    // option in the body (guards against incidental prose matches).
    let cancel_footer = lower.contains("esc to cancel") || lower.contains("esc to reject");
    cancel_footer || has_option_marker(screen)
}

/// A line that offers a selectable choice: a `❯` cursor, a numbered yes/no, or
/// a `[ ]`/`[x]` checkbox (the MCP-enable prompt uses these).
fn has_option_marker(screen: &str) -> bool {
    for line in screen.lines() {
        let t = line.trim_start();
        if t.starts_with('❯') || t.starts_with("[ ]") || t.starts_with("[x]") || t.starts_with("[]")
        {
            return true;
        }
        let tl = t.to_lowercase();
        // "1. Yes" / "2. No" style menus, with or without a leading cursor.
        let tl = tl.trim_start_matches(['❯', '>', ' ']);
        if (tl.starts_with("1. ") || tl.starts_with("2. ") || tl.starts_with("3. "))
            && (tl.contains("yes") || tl.contains("no") || tl.contains("proceed"))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_permission_prompt_is_blocked() {
        let screen = "\
Bash command
  rm -rf build/

Do you want to proceed?
❯ 1. Yes
  2. No

esc to cancel";
        assert!(is_blocked(screen));
    }

    #[test]
    fn mcp_enable_prompt_is_blocked() {
        let screen = "\
6 new MCP servers found in this project
Select any you wish to enable.

[ ] telegram-mcp
[ ] baidu-search

Space to select · Enter to confirm · Esc to reject all";
        assert!(is_blocked(screen));
    }

    #[test]
    fn working_screen_is_not_blocked() {
        let screen = "\
● Reading src/main.rs
  Analyzing the pty relay…

⠂ esc to interrupt";
        assert!(!is_blocked(screen));
    }

    #[test]
    fn transcript_viewer_is_not_blocked() {
        let screen = "showing detailed transcript · ctrl+o to toggle\nDo you want to proceed?";
        assert!(!is_blocked(screen));
    }

    #[test]
    fn model_picker_is_not_blocked() {
        let screen =
            "Select model\n❯ 1. Sonnet\n  2. Opus\nenter to set as default · esc to cancel";
        assert!(!is_blocked(screen));
    }
}
