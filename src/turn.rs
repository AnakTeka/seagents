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
