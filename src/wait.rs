//! `seagents wait` — block until an agent ends a turn, gets stuck, or exits.
//!
//! It waits on the **turn ledger**, not the screen. "Poll until idle" returns
//! at once in exactly the cases that matter — right after `spawn` (the agent
//! paints an idle input box before it takes the seed) and right after a
//! prompt (the detector ticks every 2 s) — and a turn shorter than a tick is
//! never seen working at all. The ledger's `stops` counter is exact: the
//! caller holds a baseline N (0 from `spawn`, the `stops` `send` returns)
//! and the turn it cares about has ended when `stops > N`. Nothing
//! is consumed, so re-calling with the same N after a timeout is always right
//! — which matters, because the caller is usually an agent whose shell tool
//! kills long commands.
//!
//! `wait` only reads: it never acknowledges `done` (a human still sees 🔵
//! after an agent has waited on it) and never writes `.state`.

use std::time::{Duration, Instant};

use crate::adapter::Turns;
use crate::registry::{self, AgentRecord, Found, StateRecord};
use crate::turn::Ledger;

const POLL: Duration = Duration::from_millis(250);

/// What to wait for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum For {
    /// The turn after N ends — or the agent is blocked, interrupted or gone.
    Turn,
    /// The agent exits; nothing else returns early.
    Exit,
}

/// Why `wait` returned: the JSON `reason`, and the exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    /// `stops` passed the baseline. Exit 0.
    Turn,
    /// No baseline given, and the agent is not in a turn, at a gate or
    /// gone: there is nothing to wait for, and the last turn's result is
    /// returned. Exit 0.
    Idle,
    /// Waiting on a human (a permission prompt, the folder-trust dialog, …).
    /// Exit 3.
    Blocked,
    /// The agent is gone. Exit 4.
    Exit,
    /// `--timeout` passed; call again with the same baseline. Exit 2.
    Timeout,
    /// The turn after N was cut short (Esc) — it ended without a `Stop`, so
    /// there is no final message for it. Exit 5, its own code, so a caller
    /// chaining on success doesn't read an abandoned turn as a finished one.
    Interrupted,
    /// The agent was given a prompt long enough ago, and its hooks have
    /// still not reported (`hook::health`): no turn will ever be seen to
    /// end, so waiting would only time out. Exit 6.
    NoHooks,
}

impl Reason {
    fn word(self) -> &'static str {
        match self {
            Reason::Turn => "turn",
            Reason::Idle => "idle",
            Reason::Blocked => "blocked",
            Reason::Exit => "exit",
            Reason::Timeout => "timeout",
            Reason::Interrupted => "interrupted",
            Reason::NoHooks => "no_hooks",
        }
    }

    fn code(self) -> i32 {
        match self {
            Reason::Turn | Reason::Idle => 0,
            Reason::Timeout => 2,
            Reason::Blocked => 3,
            Reason::Exit => 4,
            Reason::Interrupted => 5,
            Reason::NoHooks => 6,
        }
    }
}

/// One look at the agent.
struct Seen<'a> {
    agent: &'a AgentRecord,
    state: Option<&'a StateRecord>,
    alive: bool,
    in_turn: bool,
    /// `hook::health`: false when the agent's hooks are silent.
    hooks_ok: bool,
}

/// The whole decision, given one look: Some(reason) to return now.
///
/// `after` is the turn waited on: `stops` must pass it. None when there is
/// no turn to wait for (no `--after`, and the agent was not in a turn) —
/// then the agent's condition is the answer at once, by the same rules.
///
/// Order matters. A finished turn wins over everything (its result is what
/// was asked for, even if the agent has since exited or hit a gate); then
/// exit (a dead agent's last `.state` may still say blocked); then a gate;
/// then silent hooks (the ledger below is empty, so nothing past here can
/// be told); then, with nothing to wait for, idle; then an interrupt — a turn opened
/// since the last `Stop` (so the one after N, as `stops` is still N) that is
/// no longer running.
fn decide(what: For, after: Option<u64>, seen: &Seen) -> Option<Reason> {
    if what == For::Exit {
        return (!seen.alive).then_some(Reason::Exit);
    }
    if after.is_some_and(|n| seen.agent.stops > n) {
        return Some(Reason::Turn);
    }
    if !seen.alive {
        return Some(Reason::Exit);
    }
    if seen.state.is_some_and(|s| s.state == "blocked") {
        return Some(Reason::Blocked);
    }
    if !seen.hooks_ok {
        return Some(Reason::NoHooks);
    }
    if after.is_none() {
        return Some(Reason::Idle);
    }
    let opened = seen.agent.prompted_at > seen.agent.stopped_at;
    (opened && !seen.in_turn).then_some(Reason::Interrupted)
}

/// The state to report. `.state` is re-stamped by the wrapper's monitor
/// every 2 s at most, while the hook ledger `wait` returns on is exact, so at
/// the moment a turn ends `.state` still says `working`. For hook adapters
/// the ledger decides working-or-not (as `status` fuses it): a gate still
/// shows as `blocked`, a turn as `working`, and a stale `working` outside a
/// turn as `idle`, which the screen will say too within a tick.
fn state_word(turns: Turns, state: Option<&StateRecord>, in_turn: bool) -> Option<&str> {
    let word = state.map(|s| s.state.as_str());
    if turns != Turns::Hook || word == Some("blocked") {
        return word;
    }
    match (in_turn, word) {
        (true, _) => Some("working"),
        (false, Some("working")) => Some("idle"),
        (false, w) => w,
    }
}

/// `30s`, `5m`, `1h`, or plain seconds.
pub(crate) fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("{s:?} is not a duration (30s, 5m, 1h, or seconds)"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(format!("{s:?}: unknown unit {unit:?} (s, m or h)")),
    };
    Ok(Duration::from_secs(secs))
}

/// `seagents wait <target> [--after N] [--for turn|exit] [--timeout DUR]`:
/// prints the outcome JSON; the exit code says the same as its `reason`
/// (1 for an error, with the message on stderr).
pub(crate) fn run(target: &str, after: Option<u64>, what: For, timeout: Duration) -> i32 {
    // An agent that has exited is still answered for — from its files while
    // they last, then from the tombstone its reap left — so a caller whose
    // child finished and exited between two waits gets its result, not
    // "no such agent".
    let found = match registry::resolve_exited(target) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("seagents wait: {e}");
            return 1;
        }
    };
    let (tag, command) = match &found {
        Found::Session(s) => (s.meta.tag.clone(), s.meta.agent().to_string()),
        Found::Exited(t) => (t.tag.clone(), t.agent.clone()),
    };
    let ledger = Ledger::for_command(&command);
    // A gone agent has no screen and no turn: its ledger is final, and all
    // that is left to say is how its last turn ended, or that it exited.
    let look = || match &found {
        Found::Session(s) if crate::procfs::alive(s.child_pid()) => {
            let agent = registry::read_agent(&s.files);
            let state = registry::read_state(&s.files);
            let in_turn = ledger.in_turn(&agent, state.as_ref(), &s.files);
            // Read again: the monitor and the control thread record when the
            // agent was first prompted.
            let meta = registry::read_meta(&s.files).unwrap_or_else(|| s.meta.clone());
            let hooks_ok = crate::hook::health(&meta, &agent, crate::clock::unix_now()) != Some(false);
            (agent, state, true, in_turn, hooks_ok)
        }
        Found::Session(s) => (registry::final_agent(s), None, false, false, true),
        Found::Exited(t) => (t.ledger(), None, false, false, true),
    };

    let (agent, state, alive, in_turn, hooks_ok) = look();
    let after = match (what, after) {
        (For::Turn, Some(n)) if n > agent.stops => {
            eprintln!(
                "seagents wait: --after {n} is ahead of {tag}'s ledger (stops = {}); a restored agent \
                 starts again at 0 — wait without --after",
                agent.stops
            );
            return 1;
        }
        (For::Turn, None) => in_turn.then_some(agent.stops),
        (_, after) => after,
    };

    let deadline = Instant::now() + timeout;
    let mut seen = (agent, state, alive, in_turn, hooks_ok);
    loop {
        let (agent, state, alive, in_turn, hooks_ok) = &seen;
        let now = Seen {
            agent,
            state: state.as_ref(),
            alive: *alive,
            in_turn: *in_turn,
            hooks_ok: *hooks_ok,
        };
        let reason = decide(what, after, &now)
            .or_else(|| (Instant::now() >= deadline).then_some(Reason::Timeout));
        if let Some(reason) = reason {
            let message = match (&found, reason) {
                (Found::Session(s), Reason::NoHooks) => Some(crate::hook::silence(&s.meta)),
                _ => None,
            };
            return report(&tag, reason, &ledger, agent, state.as_ref(), *in_turn, message);
        }
        std::thread::sleep(POLL);
        seen = look();
    }
}

fn report(
    tag: &str,
    reason: Reason,
    ledger: &Ledger,
    agent: &AgentRecord,
    state: Option<&StateRecord>,
    in_turn: bool,
    message: Option<String>,
) -> i32 {
    // Why, in words, where the reason alone doesn't say what to do (also on
    // stderr, for a human).
    if let Some(m) = &message {
        eprintln!("seagents wait: {m}");
    }
    let mut out = serde_json::json!({
        "tag": tag,
        "reason": reason.word(),
        "stops": agent.stops,
        "state": state_word(ledger.turns, state, in_turn),
        "rule": state.filter(|s| s.state == "blocked").and_then(|s| s.rule.as_deref()),
        "last_message": agent.last_message,
        "transcript_path": agent.transcript_path,
    });
    if let Some(m) = message {
        out["message"] = m.into();
    }
    println!("{out}");
    reason.code()
}
