//! "Is this agent in a turn?" — the one definition `status`, `wait` and (in
//! Phase 2) `send` share, over the `.agent` turn ledger.
//!
//! **Hook adapters** (Claude): in a turn = prompted after the last `Stop`
//! **and** the transcript holds no interrupt record since that prompt. An
//! Esc-interrupted turn fires `UserPromptSubmit` and no `Stop` (verified
//! 2026-09-29), so the ledger alone would call it running forever; the agent's
//! own interrupt record is the exact edge it leaves. The screen is not
//! consulted: Claude 2.1.284 shows no spinner and an idle title while it
//! streams a plain-text answer, so a screen clause ended turns mid-answer —
//! and missed interrupts the detector never happened to see as `working`.
//!
//! **Screen adapters** have neither hook nor transcript, so the screen is all
//! there is: prompted after the last `Stop`, and not seen `idle` since.

use std::path::Path;

use crate::adapter::Turns;
use crate::registry::{AgentRecord, StateRecord};

/// Where one session's turn edges come from — resolved once per read from
/// its command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ledger {
    pub(crate) turns: Turns,
    /// Adapter id, which also names the transcript format.
    agent: Option<&'static str>,
}

impl Ledger {
    /// For the command whose first word is `argv0`. An adapter override that
    /// does not parse reads as the screen rule: the ledger is only as exact
    /// as the adapter that says where it comes from.
    pub(crate) fn for_command(argv0: &str) -> Ledger {
        Ledger {
            turns: crate::adapter::for_command(argv0)
                .map(|a| a.launch.turns)
                .unwrap_or_default(),
            agent: crate::adapter::agent_id(argv0),
        }
    }

    /// Is the agent in a turn? `state` is only read by screen adapters.
    pub(crate) fn in_turn(&self, agent: &AgentRecord, state: Option<&StateRecord>) -> bool {
        in_turn_with(self.turns, agent, state, |path, since| {
            self.agent
                .is_some_and(|id| crate::transcript::interrupted_since(id, Path::new(path), since))
        })
    }
}

/// [`Ledger::in_turn`] with the transcript lookup passed in.
fn in_turn_with(
    turns: Turns,
    agent: &AgentRecord,
    state: Option<&StateRecord>,
    interrupted_since: impl FnOnce(&str, u64) -> bool,
) -> bool {
    let Some(prompted) = agent.prompted_at else {
        return false;
    };
    if prompted <= agent.stopped_at.unwrap_or(0) {
        return false;
    }
    match turns {
        // No transcript known yet (no hook has reported one): nothing can
        // say the prompt was interrupted, so it stands.
        Turns::Hook => !agent
            .transcript_path
            .as_deref()
            .is_some_and(|p| interrupted_since(p, prompted)),
        // `.state` is stamped in seconds: an idle observation in the same
        // second as the prompt counts as after it. Blocked is still in a turn
        // — a permission prompt interrupts a turn, it does not end one.
        Turns::Screen => !state.is_some_and(|s| s.state == "idle" && s.since >= prompted / 1000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(prompted: Option<u64>, stopped: Option<u64>) -> AgentRecord {
        AgentRecord {
            prompted_at: prompted,
            stopped_at: stopped,
            transcript_path: Some("/t.jsonl".into()),
            ..AgentRecord::default()
        }
    }

    fn state(word: &str, since: u64) -> StateRecord {
        StateRecord {
            state: word.into(),
            since,
            seen: true,
            rule: None,
        }
    }

    #[test]
    fn hook_turns_come_from_the_ledger_and_the_transcript_never_the_screen() {
        let hook = |a: &AgentRecord, s: Option<&StateRecord>, interrupted: bool| {
            in_turn_with(Turns::Hook, a, s, |_, _| interrupted)
        };
        let idle = state("idle", 200);
        // Never prompted.
        assert!(!hook(&agent(None, None), None, false));
        // Prompted, no Stop: in a turn — even while the screen reads idle
        // (a streamed plain-text answer shows no spinner).
        assert!(hook(&agent(Some(100_000), None), Some(&idle), false));
        // Same-second prompt after a Stop is still after it, in ms.
        assert!(hook(&agent(Some(100_400), Some(100_100)), None, false));
        // Stopped since the prompt, or at the same instant: over.
        assert!(!hook(&agent(Some(100_000), Some(100_050)), None, false));
        assert!(!hook(&agent(Some(100_000), Some(100_000)), None, false));
        // Esc: an interrupt record since the prompt ends it with no Stop.
        assert!(!hook(&agent(Some(100_000), Some(90_000)), None, true));

        // The lookup is asked about this prompt, in this transcript.
        let mut asked = None;
        in_turn_with(Turns::Hook, &agent(Some(123_456), None), None, |p, since| {
            asked = Some((p.to_string(), since));
            false
        });
        assert_eq!(asked, Some(("/t.jsonl".to_string(), 123_456)));
        // No transcript yet: the prompt stands.
        let mut a = agent(Some(100_000), None);
        a.transcript_path = None;
        assert!(in_turn_with(Turns::Hook, &a, None, |_, _| true));
    }

    #[test]
    fn screen_turns_end_when_the_screen_goes_idle_after_the_prompt() {
        let screen = |a: &AgentRecord, s: Option<&StateRecord>| {
            in_turn_with(Turns::Screen, a, s, |_, _| panic!("screen adapters have no transcript"))
        };
        let p = agent(Some(100_500), Some(90_000));
        assert!(screen(&p, None));
        assert!(screen(&p, Some(&state("idle", 50))), "idle from before the prompt");
        assert!(screen(&p, Some(&state("working", 101))));
        assert!(screen(&p, Some(&state("blocked", 102))), "a gate is mid-turn");
        assert!(!screen(&p, Some(&state("idle", 110))));
        assert!(!screen(&p, Some(&state("idle", 100))), "same second counts as after");
        assert!(!screen(&agent(Some(100_000), Some(105_000)), Some(&state("working", 101))));
    }
}
