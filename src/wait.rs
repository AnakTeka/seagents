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
use crate::registry::{self, AgentRecord, StateRecord};
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
    /// No baseline given and the agent is not in a turn: there is nothing
    /// to wait for, and the last turn's result is returned. Exit 0.
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
        }
    }

    fn code(self) -> i32 {
        match self {
            Reason::Turn | Reason::Idle => 0,
            Reason::Timeout => 2,
            Reason::Blocked => 3,
            Reason::Exit => 4,
            Reason::Interrupted => 5,
        }
    }
}

/// One look at the agent.
struct Seen<'a> {
    agent: &'a AgentRecord,
    state: Option<&'a StateRecord>,
    alive: bool,
    in_turn: bool,
}

/// The whole decision, given one look: Some(reason) to return now.
///
/// Order matters. A finished turn wins over everything (its result is what
/// was asked for, even if the agent has since exited or hit a gate); then
/// exit (a dead agent's last `.state` may still say blocked); then a gate;
/// then an interrupt — a turn opened since the last `Stop` (so the one after
/// N, as `stops` is still N) that is no longer running.
fn decide(what: For, after: u64, seen: &Seen) -> Option<Reason> {
    if what == For::Exit {
        return (!seen.alive).then_some(Reason::Exit);
    }
    if seen.agent.stops > after {
        return Some(Reason::Turn);
    }
    if !seen.alive {
        return Some(Reason::Exit);
    }
    if seen.state.is_some_and(|s| s.state == "blocked") {
        return Some(Reason::Blocked);
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
    let session = match registry::resolve_target(target) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("seagents wait: {e}");
            return 1;
        }
    };
    let ledger = Ledger::for_command(session.meta.agent());
    let child = session.child_pid();
    let look = || {
        let agent = registry::read_agent(&session.files);
        let state = registry::read_state(&session.files);
        let in_turn = ledger.in_turn(&agent, state.as_ref());
        (agent, state, crate::procfs::alive(child), in_turn)
    };

    let (agent, state, alive, in_turn) = look();
    let baseline = match (what, after) {
        (For::Exit, _) => 0,
        (For::Turn, Some(n)) if n > agent.stops => {
            eprintln!(
                "seagents wait: --after {n} is ahead of {}'s ledger (stops = {}); a restored agent \
                 starts again at 0 — wait without --after",
                session.meta.tag, agent.stops
            );
            return 1;
        }
        (For::Turn, Some(n)) => n,
        (For::Turn, None) if in_turn => agent.stops,
        (For::Turn, None) => {
            return report(&session.meta.tag, Reason::Idle, &ledger, &agent, state.as_ref(), in_turn);
        }
    };

    let deadline = Instant::now() + timeout;
    let mut seen = (agent, state, alive, in_turn);
    loop {
        let (agent, state, alive, in_turn) = &seen;
        let now = Seen {
            agent,
            state: state.as_ref(),
            alive: *alive,
            in_turn: *in_turn,
        };
        let reason = decide(what, baseline, &now)
            .or_else(|| (Instant::now() >= deadline).then_some(Reason::Timeout));
        if let Some(reason) = reason {
            return report(&session.meta.tag, reason, &ledger, agent, state.as_ref(), *in_turn);
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
) -> i32 {
    let out = serde_json::json!({
        "tag": tag,
        "reason": reason.word(),
        "stops": agent.stops,
        "state": state_word(ledger.turns, state, in_turn),
        "rule": state.filter(|s| s.state == "blocked").and_then(|s| s.rule.as_deref()),
        "last_message": agent.last_message,
        "transcript_path": agent.transcript_path,
    });
    println!("{out}");
    reason.code()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(stops: u64, prompted: Option<u64>, stopped: Option<u64>) -> AgentRecord {
        AgentRecord {
            stops,
            prompted_at: prompted,
            stopped_at: stopped,
            ..AgentRecord::default()
        }
    }

    fn state(word: &str) -> StateRecord {
        StateRecord {
            state: word.into(),
            since: 0,
            seen: true,
            rule: None,
        }
    }

    fn decide_on(what: For, after: u64, a: &AgentRecord, s: Option<&str>, alive: bool, in_turn: bool) -> Option<Reason> {
        let st = s.map(state);
        decide(what, after, &Seen { agent: a, state: st.as_ref(), alive, in_turn })
    }

    #[test]
    fn a_turn_ends_when_stops_passes_the_baseline_and_not_before() {
        // Just spawned: no prompt yet, the screen idle — keep waiting.
        let fresh = ledger(0, None, None);
        assert_eq!(decide_on(For::Turn, 0, &fresh, Some("idle"), true, false), None);
        // The seed submitted and running.
        let running = ledger(0, Some(10), None);
        assert_eq!(decide_on(For::Turn, 0, &running, Some("working"), true, true), None);
        // Stopped: the turn after 0 ended — even if it has exited since.
        let done = ledger(1, Some(10), Some(20));
        assert_eq!(decide_on(For::Turn, 0, &done, Some("idle"), true, false), Some(Reason::Turn));
        assert_eq!(decide_on(For::Turn, 0, &done, None, false, false), Some(Reason::Turn));
        // The same ledger against its own count as baseline: nothing yet.
        assert_eq!(decide_on(For::Turn, 1, &done, Some("idle"), true, false), None);
    }

    #[test]
    fn gates_exits_and_interrupts_end_a_wait_early() {
        let running = ledger(2, Some(30), Some(20));
        assert_eq!(decide_on(For::Turn, 2, &running, Some("blocked"), true, true), Some(Reason::Blocked));
        // A dead agent's stale gate is not the news.
        assert_eq!(decide_on(For::Turn, 2, &running, Some("blocked"), false, true), Some(Reason::Exit));
        // Opened after the last Stop and no longer running: Esc.
        assert_eq!(decide_on(For::Turn, 2, &running, Some("idle"), true, false), Some(Reason::Interrupted));
        // Not opened (idle since its last Stop): nothing to report yet.
        let idle = ledger(2, Some(10), Some(20));
        assert_eq!(decide_on(For::Turn, 2, &idle, Some("idle"), true, false), None);
    }

    #[test]
    fn waiting_for_exit_ignores_everything_else() {
        let done = ledger(5, Some(10), Some(20));
        assert_eq!(decide_on(For::Exit, 0, &done, Some("blocked"), true, false), None);
        assert_eq!(decide_on(For::Exit, 0, &done, Some("idle"), false, false), Some(Reason::Exit));
    }

    #[test]
    fn the_reported_state_follows_the_ledger_for_hook_agents() {
        let w = |turns, word: &str, in_turn| state_word(turns, Some(&state(word)), in_turn).map(str::to_string);
        assert_eq!(w(Turns::Hook, "working", false).as_deref(), Some("idle"), "stale by a tick");
        assert_eq!(w(Turns::Hook, "idle", true).as_deref(), Some("working"));
        assert_eq!(w(Turns::Hook, "blocked", true).as_deref(), Some("blocked"));
        assert_eq!(w(Turns::Hook, "unknown", false).as_deref(), Some("unknown"));
        assert_eq!(w(Turns::Screen, "working", false).as_deref(), Some("working"));
        assert_eq!(state_word(Turns::Hook, None, true), Some("working"));
        assert_eq!(state_word(Turns::Hook, None, false), None);
    }

    #[test]
    fn exit_codes_follow_the_spec() {
        let codes: Vec<(&str, i32)> = [
            Reason::Turn,
            Reason::Idle,
            Reason::Timeout,
            Reason::Blocked,
            Reason::Exit,
            Reason::Interrupted,
        ]
        .iter()
        .map(|r| (r.word(), r.code()))
        .collect();
        assert_eq!(
            codes,
            [("turn", 0), ("idle", 0), ("timeout", 2), ("blocked", 3), ("exit", 4), ("interrupted", 5)]
        );
    }

    #[test]
    fn durations_take_a_unit_or_mean_seconds() {
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_duration("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration(" 0 "), Ok(Duration::ZERO));
        for bad in ["", "m", "5 m", "1.5m", "5d", "-3", "soon"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }
}
