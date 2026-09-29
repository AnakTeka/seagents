//! "Is this agent in a turn?" — the one definition `status`, `wait` and
//! `send` share, over the `.agent` turn ledger.
//!
//! **Hook adapters** (Claude, Codex): in a turn = prompted after the last
//! `Stop`, **and** the transcript holds no interrupt record since that
//! prompt, **and** the turn was not dropped before it began (below). An
//! Esc-interrupted turn fires `UserPromptSubmit` and no `Stop` (verified
//! 2026-09-29), so the ledger alone would call it running forever; the
//! agent's own interrupt record is the exact edge it leaves. The screen
//! does not end a turn otherwise: Claude 2.1.284 shows no spinner and an
//! idle title while it streams a plain-text answer, so a screen clause ended
//! turns mid-answer — and missed interrupts the detector never happened to
//! see as `working`.
//!
//! **A turn dropped before it began.** An Esc that lands before Claude's
//! first response puts the prompt back in the input box and leaves no trace
//! at all: no interrupt record, no `Stop`, no hook (the event list has no
//! cancel), and the transcript's records after the prompt — `last-prompt`,
//! `mode`, … — are the ones every turn writes at its start (all measured on
//! 2.1.284). What is left is the absence of everything a live turn does, so
//! that is the rule: the detector's own verdict on the screen (before the
//! ledger is folded in, `.state`'s `screen`) is idle, the tee has been
//! silent for [`DROPPED_AFTER`], and the agent has written nothing of its own
//! — no assistant text, reasoning or tool call — since the prompt. Each
//! clause is something a running turn cannot hide: thinking and a slow first
//! token show a spinner (working, and its timer repaints every second), a
//! streamed answer grows the tee, a tool call or a finished text block is an
//! assistant record, a permission prompt is `blocked`. Codex needs none of
//! this (it records `turn_aborted` even then) and never meets it: its
//! running-turn timer repaints every second.
//!
//! **Screen adapters** have neither hook nor transcript, so the screen is all
//! there is: prompted after the last `Stop`, and not seen `idle` since.

use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::adapter::Turns;
use crate::registry::{AgentRecord, Files, StateRecord};
use crate::transcript::Format;

/// How long the tee must be silent, with the screen idle and nothing
/// answered, before a prompt counts as dropped (ms).
const DROPPED_AFTER: u64 = 10_000;

/// Where one session's turn edges come from — resolved once per read from
/// its command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ledger {
    pub(crate) turns: Turns,
    /// The format of the transcript the agent writes, per its adapter.
    transcript: Option<Format>,
}

/// What [`in_turn_with`] asks the transcript, about the records since a
/// prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    /// Did the agent record that the user interrupted it?
    Interrupted,
    /// Did it write anything of its own (text, reasoning, a tool call)?
    Responded,
}

/// The tee's last write and the time now, Unix ms.
#[derive(Clone, Copy, Debug)]
struct Output {
    last_ms: Option<u64>,
    now_ms: u64,
}

impl Ledger {
    /// For the command whose first word is `argv0`. An adapter override that
    /// does not parse reads as the screen rule: the ledger is only as exact
    /// as the adapter that says where it comes from.
    pub(crate) fn for_command(argv0: &str) -> Ledger {
        let adapter = crate::adapter::for_command(argv0).unwrap_or_default();
        Ledger {
            turns: adapter.launch.turns,
            transcript: adapter.read.transcript,
        }
    }

    /// Is the agent in a turn? `state` is the session's `.state`, `files`
    /// its sidecars (the tee's age is read from them).
    pub(crate) fn in_turn(&self, agent: &AgentRecord, state: Option<&StateRecord>, files: &Files) -> bool {
        let output = Output {
            last_ms: std::fs::metadata(&files.raw)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64),
            now_ms: crate::clock::unix_now_ms(),
        };
        in_turn_with(self.turns, agent, state, output, |ask, path, since| {
            let Some(format) = self.transcript else {
                return false;
            };
            let path = Path::new(path);
            match ask {
                Ask::Interrupted => crate::transcript::interrupted_since(format, path, since),
                Ask::Responded => crate::transcript::responded_since(format, path, since),
            }
        })
    }
}

/// [`Ledger::in_turn`] with the clock, the tee's age and the transcript
/// passed in.
fn in_turn_with(
    turns: Turns,
    agent: &AgentRecord,
    state: Option<&StateRecord>,
    output: Output,
    ask: impl Fn(Ask, &str, u64) -> bool,
) -> bool {
    let Some(prompted) = agent.prompted_at else {
        return false;
    };
    if prompted <= agent.stopped_at.unwrap_or(0) {
        return false;
    }
    match turns {
        Turns::Hook => {
            // No transcript known yet (no hook has reported one): nothing
            // can say the prompt was interrupted, so it stands.
            let Some(path) = agent.transcript_path.as_deref() else {
                return true;
            };
            if ask(Ask::Interrupted, path, prompted) {
                return false;
            }
            // Cheapest clauses first: the transcript is read only for a
            // turn that already looks dropped.
            let dropped = state.is_some_and(|s| s.screen() == "idle")
                && output.now_ms >= output.last_ms.unwrap_or(0).max(prompted) + DROPPED_AFTER
                && !ask(Ask::Responded, path, prompted);
            !dropped
        }
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
            screen: None,
        }
    }

    /// Output just now: nothing has been silent long enough to matter.
    const LIVE: Output = Output { last_ms: Some(1_000_000), now_ms: 1_000_000 };

    #[test]
    fn hook_turns_come_from_the_ledger_and_the_transcript() {
        let hook = |a: &AgentRecord, s: Option<&StateRecord>, interrupted: bool| {
            in_turn_with(Turns::Hook, a, s, LIVE, |ask, _, _| ask == Ask::Interrupted && interrupted)
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
        let asked = std::cell::RefCell::new(Vec::new());
        in_turn_with(Turns::Hook, &agent(Some(123_456), None), None, LIVE, |ask, p, since| {
            asked.borrow_mut().push((ask, p.to_string(), since));
            false
        });
        assert_eq!(asked.into_inner(), [(Ask::Interrupted, "/t.jsonl".to_string(), 123_456)]);
        // No transcript yet: the prompt stands.
        let mut a = agent(Some(100_000), None);
        a.transcript_path = None;
        assert!(in_turn_with(Turns::Hook, &a, None, LIVE, |_, _, _| true));
    }

    #[test]
    fn a_prompt_dropped_before_its_first_response_ends_the_turn() {
        // Claude, Esc'd within a second of the prompt at t = 100 s: the prompt
        // is back in the input box, the screen idle, the tee silent since.
        let prompted = agent(Some(100_000), Some(90_000));
        let turn = |screen: &str, last_out: u64, now: u64, responded: bool| {
            let st = StateRecord { screen: Some(screen.into()), ..state("working", 100) };
            let out = Output { last_ms: Some(last_out), now_ms: now };
            in_turn_with(Turns::Hook, &prompted, Some(&st), out, |ask, _, _| ask == Ask::Responded && responded)
        };
        assert!(!turn("idle", 100_600, 110_600, false), "silent for DROPPED_AFTER, nothing answered");
        // Each clause alone keeps the turn: not yet silent long enough …
        assert!(turn("idle", 100_600, 110_599, false));
        // … the tee still growing (a streamed answer) …
        assert!(turn("idle", 110_000, 115_000, false));
        // … the screen working (a spinner) or at a gate …
        assert!(turn("working", 100_600, 200_000, false));
        assert!(turn("blocked", 100_600, 200_000, false));
        // … or the agent wrote something (a tool call now running quietly).
        assert!(turn("idle", 100_600, 200_000, true));
        // Silence counts from the prompt, not from older output.
        assert!(turn("idle", 50_000, 109_999, false));
        assert!(!turn("idle", 50_000, 110_000, false));
        // No record at all: nothing says the screen is idle.
        assert!(in_turn_with(Turns::Hook, &prompted, None, Output { last_ms: None, now_ms: 900_000 }, |_, _, _| false));
    }

    #[test]
    fn screen_turns_end_when_the_screen_goes_idle_after_the_prompt() {
        let screen = |a: &AgentRecord, s: Option<&StateRecord>| {
            in_turn_with(Turns::Screen, a, s, LIVE, |_, _, _| panic!("screen adapters have no transcript"))
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
