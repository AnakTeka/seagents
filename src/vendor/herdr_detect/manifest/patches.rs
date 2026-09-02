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

/// Apply seagents' priority patches to a freshly parsed manifest.
pub(super) fn apply(manifest: &mut AgentManifest) {
    for (manifest_id, rule_id, priority) in PRIORITIES {
        if manifest.id != *manifest_id {
            continue;
        }
        for rule in manifest.rules.iter_mut().filter(|rule| rule.id == *rule_id) {
            rule.priority = *priority;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::herdr_detect::{agent_label, manifest::bundled_manifest, Agent};

    fn rule_priority(manifest: &AgentManifest, id: &str) -> Option<i32> {
        manifest
            .rules
            .iter()
            .find(|rule| rule.id == id)
            .map(|rule| rule.priority)
    }

    #[test]
    fn background_shell_rule_is_demoted_below_the_idle_signals() {
        let mut manifest = bundled_manifest(Agent::Claude).expect("bundled claude manifest");
        let upstream = rule_priority(&manifest, "background_shell_working")
            .expect("upstream still ships background_shell_working");
        let idle_title = rule_priority(&manifest, "osc_title_idle").expect("osc_title_idle");
        assert!(upstream > idle_title, "patch would be pointless");

        apply(&mut manifest);

        let patched = rule_priority(&manifest, "background_shell_working").unwrap();
        assert!(
            patched < idle_title,
            "{patched} should sit below {idle_title}"
        );
        // Everything else is upstream's.
        assert_eq!(rule_priority(&manifest, "osc_title_working"), Some(1100));
        assert_eq!(rule_priority(&manifest, "live_turn_working"), Some(970));
        assert_eq!(
            rule_priority(&manifest, "background_agents_working"),
            Some(965)
        );
    }

    fn priorities(manifest: &AgentManifest) -> Vec<(String, i32)> {
        manifest
            .rules
            .iter()
            .map(|rule| (rule.id.clone(), rule.priority))
            .collect()
    }

    #[test]
    fn patches_only_touch_their_own_manifest() {
        for agent in Agent::SCREEN_MANIFEST_AGENTS {
            let mut manifest = bundled_manifest(agent).expect("bundled manifest");
            let before = priorities(&manifest);
            apply(&mut manifest);
            let patched = priorities(&manifest) != before;
            assert_eq!(
                patched,
                agent_label(agent) == "claude",
                "unexpected patch state for {}",
                agent_label(agent)
            );
        }
    }
}
