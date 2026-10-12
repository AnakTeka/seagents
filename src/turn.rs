//! "Is this agent in a turn?" — the one definition `status`, `wait` and
//! `send` share, over the `.agent` turn ledger.
//!
//! Only an agent with hooks (Claude, Codex) has a ledger: in a turn = prompted after the last
//! `Stop`, **and** the transcript holds no interrupt record since that
//! prompt, **and** the turn was not dropped before it began (below). An
//! Esc-interrupted turn fires `UserPromptSubmit` and no `Stop` (verified
//! 2026-09-29), so the ledger alone would call it running forever; the
//! agent's own interrupt record is the exact edge it leaves. The screen
//! does not end a turn otherwise: Claude 2.1.284 shows no spinner and an
//! idle title while it streams a plain-text answer, so a screen clause ended
//! turns mid-answer — and missed interrupts the screen never happened to
//! read as `working`.
//!
//! **A turn dropped before it began.** An Esc that lands before Claude's
//! first response puts the prompt back in the input box and leaves no trace
//! at all: no interrupt record, no `Stop`, no hook (the event list has no
//! cancel), and the transcript's records after the prompt — `last-prompt`,
//! `mode`, … — are the ones every turn writes at its start (all measured on
//! 2.1.284). What is left is the absence of everything a live turn does, so
//! that is the rule: herdr's own verdict on the screen (before the ledger is
//! folded in, `.state`'s `screen`) is idle, the screen has not changed for
//! [`DROPPED_AFTER`] (`.state`'s `screen_changed_ms`, which `run`'s monitor
//! keeps while the agent sits idle in a turn), and the agent has written
//! nothing of its own — no assistant text, reasoning or tool call — since the
//! prompt. Each clause is something a running turn cannot hide: thinking and
//! a slow first token show a spinner (working, and its timer repaints every
//! second), a streamed answer changes the screen, a tool call or a finished
//! text block is an assistant record, a permission prompt is `blocked`. Codex needs none of
//! this (it records `turn_aborted` even then) and never meets it: its
//! running-turn timer repaints every second.
//!
//! An agent without hooks is never in a turn here: nothing reports its
//! prompts, so herdr's reading of its screen is all there is of its state.

use std::path::Path;

use crate::registry::{AgentRecord, StateRecord};
use crate::transcript::Format;

/// How long the screen must stay unchanged, idle and with nothing answered,
/// before a prompt counts as dropped (ms).
const DROPPED_AFTER: u64 = 10_000;

/// Where one session's turn edges come from — resolved once per read from
/// its command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ledger {
    /// The agent's adapter gives it hooks: an exact ledger.
    pub(crate) hooks: bool,
    /// The format of the transcript the agent writes, per its adapter.
    transcript: Option<Format>,
}

impl Ledger {
    /// For the command whose first word is `argv0`. An adapter override that
    /// does not parse reads as one without hooks: the ledger is only as
    /// exact as the adapter that says where it comes from.
    pub(crate) fn for_command(argv0: &str) -> Ledger {
        let adapter = crate::adapter::for_command(argv0).unwrap_or_default();
        Ledger {
            hooks: adapter.hooks.enabled(),
            transcript: adapter.read.transcript,
        }
    }

    /// Is the agent in a turn? `state` is the session's `.state`.
    pub(crate) fn in_turn(&self, agent: &AgentRecord, state: Option<&StateRecord>) -> bool {
        if !self.hooks {
            return false;
        }
        let Some(prompted) = agent.prompted_at else {
            return false;
        };
        if prompted <= agent.stopped_at.unwrap_or(0) {
            return false;
        }
        // No transcript known yet (no hook has reported one): nothing can
        // say the prompt was interrupted, so it stands.
        let Some(path) = agent.transcript_path.as_deref() else {
            return true;
        };
        let path = Path::new(path);
        // What the transcript says since the prompt — nothing, when this
        // adapter names no format to parse it with.
        let ask = |f: fn(Format, &Path, u64) -> bool| {
            self.transcript
                .is_some_and(|format| f(format, path, prompted))
        };
        if ask(crate::transcript::interrupted_since) {
            return false;
        }
        // Cheapest clauses first: the transcript is read only for a turn
        // that already looks dropped.
        let last_change = state.and_then(|s| s.screen_changed_ms).unwrap_or(0);
        let dropped = state.is_some_and(|s| s.screen() == "idle")
            && crate::clock::unix_now_ms() >= last_change.max(prompted) + DROPPED_AFTER
            && !ask(crate::transcript::responded_since);
        !dropped
    }
}
