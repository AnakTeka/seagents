//! seagents-local policy patches over herdr's shipped rules — NOT vendored.
//!
//! Manifests self-update from herdr.dev, so a rule that reads the screen
//! correctly but answers a different question than seagents asks cannot be
//! fixed by editing the TOML in `manifests/` — the next fetch brings it back.
//! The other lever, a local override at
//! `~/.config/seagents/agent-detection/<agent>.toml`, freezes the entire file
//! and forfeits every future upstream fix. These patches instead adjust
//! individual rules by id in whichever manifest loads (bundled or remote),
//! leaving the rest of the file upstream's. A user's explicit override is left
//! literal — hot-tuning has to mean what it says.
//!
//! Keep this list short, and re-check each entry on a re-sync: an upstream
//! rule that changes shape may no longer need (or may no longer match) its
//! patch. `seagents debug-screen` prints the patched priority, so the effect
//! is visible where the verdict is explained.

use super::AgentManifest;

/// `(manifest id, rule id, seagents priority)`.
const PRIORITIES: &[(&str, &str, i32)] = &[
    // herdr counts a live background shell as the agent working:
    // `background_shell_working` matches Claude's footer chip
    // (`⏵⏵ bypass permissions on · 1 shell · ← for agents`) at 965, which
    // outranks every idle signal there is. That chip stays up for as long as
    // the shell lives, so an agent that started a dev server and went back to
    // its prompt reads `working` for as long as the server runs (2h07m and
    // counting, in the session that prompted this), and the finished-turn
    // notification never fires.
    //
    // seagents asks "does this agent need me?", not "is anything it started
    // still alive?", so the chip drops below the idle signals (osc_title_idle
    // and osc_progress_idle, both 250) rather than being deleted: it still
    // breaks a tie on a screen where nothing else matched. Live turns are
    // unaffected — osc_title_working (1100) and live_turn_working (970,
    // "esc to interrupt") outrank it either way, and the two sibling rules
    // that do mean the agent is stuck waiting (`background_agents_working`,
    // `background_mcp_task_working`) are left alone.
    ("claude", "background_shell_working", 200),
];

/// `(manifest id, rules)`: rules seagents adds, in the manifest's own TOML.
/// An addition is for a screen upstream reads wrong in a way no re-weighting
/// fixes; its id says it is seagents', so a re-sync can tell whether
/// upstream has caught up.
const ADDED: &[(&str, &str)] = &[
    // Codex's folder-trust dialog (0.152, a never-trusted directory) is a
    // gate: whatever is typed into it picks an answer — `stop`'s `/exit` +
    // Enter picked "1. Yes, continue" and wrote the directory's trust into
    // the user's config.toml (measured in a scratch CODEX_HOME). Upstream's
    // `trust_directory` wants `> You are in …` as the screen's first line,
    // but when an update notice is up the dialog is drawn over its box and
    // the row reads `>─You are in …──╮`, so that rule misses and the screen
    // falls through to idle. The dialog's own last lines are unmistakable.
    (
        "codex",
        r#"
[[rules]]
id = "seagents_trust_directory"
state = "blocked"
priority = 950
region = "bottom_non_empty_lines(8)"
visible_blocker = true
all = [
  { regex = ['(?s)Do\s+you\s+trust\s+the\s+contents\s+of\s+this\s+directory\?'] },
  { contains = ["1. Yes, continue"] },
  { contains = ["2. No, quit"] },
]
"#,
    ),
];

/// Apply seagents' patches to a freshly parsed manifest.
pub(super) fn apply(manifest: &mut AgentManifest) {
    for (manifest_id, rule_id, priority) in PRIORITIES {
        if manifest.id != *manifest_id {
            continue;
        }
        for rule in manifest.rules.iter_mut().filter(|rule| rule.id == *rule_id) {
            rule.priority = *priority;
        }
    }
    #[derive(serde::Deserialize)]
    struct Rules {
        rules: Vec<super::ManifestRule>,
    }
    for (manifest_id, text) in ADDED {
        if manifest.id == *manifest_id {
            let added: Rules = toml::from_str(text).expect("seagents' added rules parse");
            manifest.rules.extend(added.rules);
        }
    }
}
