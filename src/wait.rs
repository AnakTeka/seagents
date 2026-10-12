//! `seagents wait` — block until an agent ends a turn, gets stuck, or exits.
//!
//! It waits on the **turn ledger**, not the screen. "Poll until idle" returns
//! at once in exactly the cases that matter — right after `spawn` (the agent
//! paints an idle input box before it takes the seed) and right after a
//! prompt (`.state` is re-stamped every 2 s) — and a turn shorter than a tick is
//! never seen working at all. The ledger's `stops` counter is exact: the
//! caller holds a baseline N (0 from `spawn`, the `stops` `send` returns)
//! and the turn it cares about has ended when `stops > N`. Nothing
//! is consumed, so re-calling with the same N after a timeout is always right
//! — which matters, because the caller is usually an agent whose shell tool
//! kills long commands.
//!
//! Whether the agent is at a dialog is herdr's to say (asked at most once a
//! second); whether it has exited, the registry's.
//!
//! `wait` only reads: it never acknowledges `done` (a human still sees 🔵
//! after an agent has waited on it) and never writes `.state`.

use std::time::{Duration, Instant};

use crate::registry::{self, AgentRecord, Found, StateRecord};
use crate::turn::Ledger;

const POLL: Duration = Duration::from_millis(250);

/// How often herdr is asked whether the agent sits at a dialog.
const HERDR_POLL: Duration = Duration::from_secs(1);

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
    /// herdr reads it as waiting at a dialog.
    blocked: bool,
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
    if seen.blocked {
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

/// The state to report. `.state` is re-stamped by `run`'s monitor every 2 s
/// at most, while the hook ledger `wait` returns on is exact, so at the
/// moment a turn ends `.state` still says `working`. With hooks the
/// ledger decides working-or-not (as `status` fuses it): a dialog still shows
/// as `blocked`, a turn as `working`, and a stale `working` outside a turn as
/// `idle`, which the screen will say too within a tick.
fn state_word(
    hooks: bool,
    state: Option<&StateRecord>,
    blocked: bool,
    in_turn: bool,
) -> Option<&str> {
    if blocked {
        return Some("blocked");
    }
    let word = state.map(|s| s.state.as_str());
    if !hooks {
        // A `blocked` herdr no longer reads is stale: not known yet.
        return word.filter(|w| *w != "blocked");
    }
    match (in_turn, word) {
        (true, _) => Some("working"),
        // Stale: the dialog was answered, or the turn ended, a tick ago.
        (false, Some("working" | "blocked")) => Some("idle"),
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
    // Whether herdr sees the agent at a dialog, and its rule: asked at most
    // every HERDR_POLL.
    let mut gate: (Option<Instant>, Option<String>, bool) = (None, None, false);
    let mut herdr_gate = |s: &registry::Session| -> (bool, Option<String>) {
        if gate.0.is_none_or(|t| t.elapsed() >= HERDR_POLL) {
            let agent = crate::herdr::agent_on(&s.meta.key).ok().flatten();
            let (blocked, rule) = match crate::status::agent_status(agent.as_ref(), s.meta.agent())
            {
                Some((crate::herdr::Status::Blocked, rule)) => (true, rule),
                _ => (false, None),
            };
            gate = (Some(Instant::now()), rule, blocked);
        }
        (gate.2, gate.1.clone())
    };
    // A gone agent has no screen and no turn: its ledger is final, and all
    // that is left to say is how its last turn ended, or that it exited.
    let mut look = || match &found {
        Found::Session(s) if s.alive_now() => {
            let agent = registry::read_agent(&s.files);
            let state = registry::read_state(&s.files);
            let in_turn = ledger.in_turn(&agent, state.as_ref());
            // Read again: `run` and `send` record when the agent was first
            // prompted.
            let meta = registry::read_meta(&s.files).unwrap_or_else(|| s.meta.clone());
            let hooks_ok =
                crate::hook::health(&meta, &agent, crate::clock::unix_now()) != Some(false);
            let (blocked, rule) = herdr_gate(s);
            Look {
                agent,
                state,
                blocked,
                rule,
                alive: true,
                in_turn,
                hooks_ok,
            }
        }
        Found::Session(s) => Look::gone(registry::final_agent(s)),
        Found::Exited(t) => Look::gone(t.ledger()),
    };

    let first = look();
    let after = match (what, after) {
        (For::Turn, Some(n)) if n > first.agent.stops => {
            eprintln!(
                "seagents wait: --after {n} is ahead of {tag}'s ledger (stops = {}); a restored agent \
                 starts again at 0 — wait without --after",
                first.agent.stops
            );
            return 1;
        }
        (For::Turn, None) => first.in_turn.then_some(first.agent.stops),
        (_, after) => after,
    };

    let deadline = Instant::now() + timeout;
    let mut seen = first;
    loop {
        let now = Seen {
            agent: &seen.agent,
            blocked: seen.blocked,
            alive: seen.alive,
            in_turn: seen.in_turn,
            hooks_ok: seen.hooks_ok,
        };
        let reason = decide(what, after, &now)
            .or_else(|| (Instant::now() >= deadline).then_some(Reason::Timeout));
        if let Some(reason) = reason {
            let message = match (&found, reason) {
                (Found::Session(s), Reason::NoHooks) => Some(crate::hook::silence(&s.meta)),
                _ => None,
            };
            return report(&tag, reason, &ledger, &seen, message);
        }
        std::thread::sleep(POLL);
        seen = look();
    }
}

/// Everything one look at the agent found.
struct Look {
    agent: AgentRecord,
    state: Option<StateRecord>,
    blocked: bool,
    rule: Option<String>,
    alive: bool,
    in_turn: bool,
    hooks_ok: bool,
}

impl Look {
    /// An agent that is gone: only its ledger is left.
    fn gone(agent: AgentRecord) -> Look {
        Look {
            agent,
            state: None,
            blocked: false,
            rule: None,
            alive: false,
            in_turn: false,
            hooks_ok: true,
        }
    }
}

fn report(tag: &str, reason: Reason, ledger: &Ledger, seen: &Look, message: Option<String>) -> i32 {
    let agent = &seen.agent;
    // Why, in words, where the reason alone doesn't say what to do (also on
    // stderr, for a human).
    if let Some(m) = &message {
        eprintln!("seagents wait: {m}");
    }
    let mut out = serde_json::json!({
        "tag": tag,
        "reason": reason.word(),
        "stops": agent.stops,
        "state": state_word(ledger.hooks, seen.state.as_ref(), seen.blocked, seen.in_turn),
        "rule": seen.rule.as_deref().filter(|_| seen.blocked),
        "last_message": agent.last_message,
        "transcript_path": agent.transcript_path,
    });
    if let Some(m) = message {
        out["message"] = m.into();
    }
    println!("{out}");
    reason.code()
}
