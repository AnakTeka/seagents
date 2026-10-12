//! Each tracked agent's state: herdr's verdict on its screen, fused with the
//! turn ledger its hooks keep, and the transitions recorded in `.state`.
//!
//! herdr detects the state — its per-agent rule manifests, kept current
//! upstream, read the screen of the pane it owns — and seagents asks it for
//! all agents in one `herdr agent list`. What herdr does not keep, seagents
//! does: since when the agent has been in its state, whether a finished turn
//! has been seen yet (the "done" marker), and — for a hook agent — whether it
//! is mid-turn while its screen looks idle.

use crate::herdr;
use crate::registry::{Files, StateRecord};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    Blocked,
    /// Finished (idle) but the user hasn't looked at it yet — an unread
    /// marker, not a detection state. Downgrades to Idle on acknowledgment
    /// (HUD preview/jump, or the agent's window gaining focus).
    Done,
    Working,
    Idle,
    Unknown,
}

impl State {
    /// Sort key — most urgent first. Done outranks working: it's actionable.
    pub(crate) fn rank(self) -> u8 {
        match self {
            State::Blocked => 0,
            State::Done => 1,
            State::Working => 2,
            State::Idle => 3,
            State::Unknown => 4,
        }
    }

    pub(crate) fn badge(self) -> &'static str {
        match self {
            State::Blocked => "🔴 blocked",
            State::Done => "🔵 done",
            State::Working => "🟡 working",
            State::Idle => "🟢 idle",
            State::Unknown => "⚪ unknown",
        }
    }

    pub(crate) fn word(self) -> &'static str {
        match self {
            State::Blocked => "blocked",
            State::Done => "done",
            State::Working => "working",
            State::Idle => "idle",
            State::Unknown => "unknown",
        }
    }

    /// herdr's status as a screen verdict. herdr's `done` is idle with its
    /// own unseen marker on it, which seagents does not use: idle.
    fn of(status: herdr::Status) -> State {
        match status {
            herdr::Status::Idle | herdr::Status::Done => State::Idle,
            herdr::Status::Working => State::Working,
            herdr::Status::Blocked => State::Blocked,
            herdr::Status::Unknown => State::Unknown,
        }
    }
}

pub(crate) struct Session {
    /// Stable session id: the key of its cache files (its herdr terminal id).
    pub(crate) id: String,
    pub(crate) state: State,
    pub(crate) dir: String,
    /// Full launch cwd (dir is its basename).
    pub(crate) cwd: String,
    pub(crate) cmd: String,
    /// The agent's pid; 0 while it is still starting.
    pub(crate) pid: u64,
    /// The herdr pane the agent is in now.
    pub(crate) pane_id: String,
    /// herdr's own word for the agent's state, before the turn ledger and the
    /// done marker are folded in; None when herdr does not see the agent.
    pub(crate) herdr_state: Option<&'static str>,
    /// Seconds the agent has been in its current state ("idle for" on green
    /// rows, "waiting on you for" on red ones). Tracked via observed state
    /// *transitions* persisted in the `.state` sidecar — herdr keeps no time.
    pub(crate) state_secs: u64,
    /// The conversation's own name, as the agent stamps it into the window
    /// title: Claude writes its generated `aiTitle` there, which is the
    /// human-readable line `claude --resume` lists. Empty when the agent
    /// hasn't named the conversation yet, so a caller showing it can fall
    /// back to the directory.
    pub(crate) title: String,
    /// The agent's name — what every command addresses it by.
    pub(crate) tag: String,
    /// What the agent reports about itself (conversation, turn ledger); the
    /// empty record for an agent without hooks.
    pub(crate) agent: crate::registry::AgentRecord,
    /// [`crate::turn::Ledger::in_turn`], as of this read.
    pub(crate) in_turn: bool,
    /// [`crate::hook::health`]: None when the agent was given no hooks,
    /// false when they have gone silent.
    pub(crate) hooks_ok: Option<bool>,
}

/// The three questions the list can be ordered to answer. Also the value of
/// `status --sort`, so the variant docs double as its help text.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub(crate) enum Sort {
    /// Triage: who needs me most, longest-waiting first.
    Urgency,
    /// Project view: same cwd grouped alphabetically, urgency within a group.
    Dir,
    /// Activity feed: most recent state change on top.
    Recent,
}

impl Sort {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Sort::Urgency => "urgency",
            Sort::Dir => "dir",
            Sort::Recent => "recent",
        }
    }

    /// The inverse of [`Sort::word`]; the same spelling `--sort` accepts.
    pub(crate) fn parse(s: &str) -> Option<Sort> {
        <Sort as clap::ValueEnum>::from_str(s, false).ok()
    }

    /// Cycle order for the HUD's `s` key.
    pub(crate) fn next(self) -> Sort {
        match self {
            Sort::Urgency => Sort::Dir,
            Sort::Dir => Sort::Recent,
            Sort::Recent => Sort::Urgency,
        }
    }
}

/// Gather every tracked session in the given order. Shared by the `status`
/// table and the HUD. One `herdr agent list` for all of them; with no herdr
/// server (or none reachable) every agent reads unknown.
pub(crate) fn collect(sort: Sort) -> Vec<Session> {
    // Sessions whose agent has exited are dropped entirely — their files
    // reaped — rather than shown as corpses.
    crate::registry::reap();
    let agents = herdr::agents().unwrap_or_default();
    let now = crate::clock::unix_now();
    let mut sessions: Vec<Session> = crate::registry::list()
        .iter()
        .map(|s| read_session(s, agents.iter().find(|a| a.terminal_id == s.meta.key), now))
        .collect();
    match sort {
        // Within a rank, longest-in-state first: the blocked agent you've
        // kept waiting longest tops the list.
        Sort::Urgency => sessions.sort_by(|a, b| {
            (a.state.rank(), std::cmp::Reverse(a.state_secs), &a.dir, &a.id)
                .cmp(&(b.state.rank(), std::cmp::Reverse(b.state_secs), &b.dir, &b.id))
        }),
        Sort::Dir => sessions.sort_by(|a, b| (&a.dir, a.state.rank(), &a.id).cmp(&(&b.dir, b.state.rank(), &b.id))),
        Sort::Recent => {
            sessions.sort_by(|a, b| (a.state_secs, a.state.rank(), &a.id).cmp(&(b.state_secs, b.state.rank(), &b.id)))
        }
    }
    sessions
}

/// Every tracked session, one per line.
pub fn print(sort: Sort) -> i32 {
    let sessions = collect(sort);
    // Cheap piggyback: any `status` invocation also keeps the reboot snapshot
    // fresh.
    crate::snapshot::maybe_capture();
    if sessions.is_empty() {
        println!("no agents tracked");
    }
    let tag_w = sessions.iter().map(|s| s.tag.len()).max().unwrap_or(3).max(3);
    let dir_w = sessions.iter().map(|s| s.dir.len()).max().unwrap_or(3).max(3);
    let cmd_w = sessions.iter().map(|s| s.cmd.len()).max().unwrap_or(3).max(3);
    for s in &sessions {
        println!(
            "{:<11}  {:<tw$}  {:<dw$}  {:<cw$}  {:>6}",
            s.state.badge(),
            s.tag,
            s.dir,
            s.cmd,
            humanize_secs(s.state_secs),
            tw = tag_w,
            dw = dir_w,
            cw = cmd_w,
        );
    }
    if !sessions.is_empty() && !herdr::server_running() {
        eprintln!("⚠ no herdr server is running: states are unknown");
    }
    0
}

/// `seagents status --json`: machine-readable session list, one array on
/// stdout. This is the integration surface for widget frontends (eww etc.):
/// poll this, render rows, and wire clicks to `seagents focus <tag>`.
pub fn print_json(sort: Sort) -> i32 {
    let sessions = collect(sort);
    crate::snapshot::maybe_capture();
    let arr: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "state": s.state.word(),
                "herdr_state": s.herdr_state,
                "dir": s.dir,
                "cwd": s.cwd,
                "cmd": s.cmd,
                "pid": s.pid,
                "pane_id": s.pane_id,
                "state_secs": s.state_secs,
                "title": s.title,
                "tag": s.tag,
                "stops": s.agent.stops,
                "in_turn": s.in_turn,
                "hooks_ok": s.hooks_ok,
                "transcript_path": s.agent.transcript_path,
                "session_id": s.agent.session_id,
                "capabilities": crate::adapter::capabilities(&s.cmd),
            })
        })
        .collect();
    println!("{}", serde_json::to_string(&serde_json::Value::Array(arr)).unwrap_or_else(|_| "[]".into()));
    0
}

/// Build a Session from one live session's `.meta` and what herdr reports
/// of its agent (None: herdr does not see it — not started yet, or
/// suspended).
fn read_session(live: &crate::registry::Session, herdr_agent: Option<&herdr::Agent>, now: u64) -> Session {
    let meta = &live.meta;
    let cmd = or_unknown(meta.agent());
    let screen = screen_of(herdr_agent, &cmd);
    let herdr_state = herdr_agent.map(|a| State::of(a.status).word());
    let (state, record, agent, in_turn) = settle(&live.files, &cmd, screen, now);
    // Present unacknowledged completions as their own state.
    let state = if state == State::Idle && !record.seen { State::Done } else { state };
    Session {
        id: meta.key.clone(),
        state,
        dir: or_unknown(meta.dir()),
        cwd: or_unknown(&meta.cwd),
        cmd,
        pid: live.pid(),
        pane_id: herdr_agent.map_or_else(|| meta.pane_id.clone(), |a| a.pane_id.clone()),
        herdr_state,
        state_secs: now.saturating_sub(record.since),
        title: herdr_agent.map(|a| conversation_title(&a.title, meta.agent())).unwrap_or_default(),
        tag: meta.tag.clone(),
        hooks_ok: crate::hook::health(meta, &agent, now),
        agent,
        in_turn,
    }
}

/// herdr's verdict on one agent's screen.
pub(crate) struct Screen {
    pub(crate) state: State,
    /// For `blocked`: the id of the detection rule that matched — which dialog
    /// the agent is at (`live_blocked_form`, a permission prompt, …).
    pub(crate) rule: Option<String>,
}

/// The screen verdict for an agent of command `cmd` as herdr reports it,
/// with the dialogs herdr misses ([`agent_status`]).
pub(crate) fn screen_of(agent: Option<&herdr::Agent>, cmd: &str) -> Screen {
    match agent_status(agent, cmd) {
        Some((status, rule)) => Screen { state: State::of(status), rule },
        None => Screen { state: State::Unknown, rule: None },
    }
}

/// The rule a dialog from the adapter's `[screen] dialogs` is reported
/// under.
pub(crate) const DIALOG_RULE: &str = "seagents_dialog";

/// herdr's status for an agent of command `cmd`, and for a blocked one the
/// rule naming its dialog — corrected for the dialogs herdr does not read as
/// one (the adapter's `[screen] dialogs`), which count as blocked under
/// [`DIALOG_RULE`] — and so does a screen that could not be read, for an
/// agent whose adapter lists dialogs: what cannot be checked is not typed
/// into. The one place `status`, `wait`, `send`, `stop` and the seed ask
/// whether an agent waits at a dialog. None: herdr does not see the agent.
/// Costs a herdr call more for a blocked agent (its rule), and one for an
/// agent whose adapter lists dialogs (its screen).
pub(crate) fn agent_status(agent: Option<&herdr::Agent>, cmd: &str) -> Option<(herdr::Status, Option<String>)> {
    let a = agent?;
    if a.status == herdr::Status::Blocked {
        return Some((a.status, herdr::blocked_rule(&a.pane_id)));
    }
    let dialogs = crate::adapter::for_command(cmd).map(|ad| ad.screen.dialogs).unwrap_or_default();
    if !dialogs.is_empty() && herdr::read(&a.pane_id, false).ok().is_none_or(|s| crate::screen::shows_any(&s, &dialogs)) {
        return Some((herdr::Status::Blocked, Some(DIALOG_RULE.to_string())));
    }
    Some((a.status, None))
}

/// Fold what the screen shows together with the turn ledger, record it in
/// `.state`, and report (the fused state, the record, the ledger, whether
/// the agent is in a turn).
///
/// For an agent whose turns come from hooks the ledger is exact where the
/// screen is not, so: blocked (screen) wins; else in a turn → working; else
/// the screen's verdict. Recording the fused state (rather than the raw one)
/// is what keeps the done marker and the "finished" notification from firing
/// mid-answer. An agent without hooks has no ledger: the screen is its state.
pub(crate) fn settle(
    files: &Files,
    cmd: &str,
    screen: Screen,
    now: u64,
) -> (State, StateRecord, crate::registry::AgentRecord, bool) {
    let agent = crate::registry::read_agent(files);
    let ledger = crate::turn::Ledger::for_command(cmd);
    if ledger.hooks {
        // The ledger's rule reads the screen only for a turn dropped before
        // it began, and this pass's verdict is the freshest there is; the
        // screen's clock is the one `.state` keeps.
        let current = crate::registry::read_state(files);
        let raw = StateRecord {
            state: screen.state.word().to_string(),
            since: now,
            seen: true,
            rule: None,
            screen: None,
            screen_changed_ms: current.and_then(|c| c.screen_changed_ms),
        };
        let in_turn = ledger.in_turn(&agent, Some(&raw));
        let seen = screen.state;
        let shown = if in_turn && screen.state != State::Blocked {
            Screen { state: State::Working, rule: None }
        } else {
            screen
        };
        let state = shown.state;
        (state, observe_state(files, shown, seen, now), agent, in_turn)
    } else {
        let state = screen.state;
        (state, observe_state(files, screen, state, now), agent, false)
    }
}

/// A `.meta` field for display, `?` when it wasn't recorded.
fn or_unknown(s: &str) -> String {
    if s.is_empty() { "?" } else { s }.to_string()
}

/// Record `state` into the sidecar if it differs from what's there; returns
/// the record for the current state. Multiple observers (`run`'s monitor and
/// an open HUD) may race on a transition — both write since≈now, so the
/// divergence is bounded by one poll interval. Harmless. (Each observer's
/// read-decide-write is one locked registry update, so a race can only ever
/// order two whole decisions, never interleave them.)
///
/// A working→idle transition is stamped `seen: false` — the agent finished
/// and nobody has looked yet (the "done" state) — whoever observes it. For a
/// hook agent that edge is the ledger's (its `Stop`), which lands between
/// two screen changes: the monitor and an open HUD (every 750 ms) race to see
/// it, and when only the monitor's sighting counted, a HUD that saw it first
/// wrote plain idle and the done marker was lost.
///
/// `seen` is herdr's own verdict, which `shown` may have overridden with the
/// ledger; it is kept beside the state when the two differ
/// ([`StateRecord::screen`]) — what `wait` and `send` read to tell a turn
/// dropped before it began (`turn`).
fn observe_state(files: &Files, shown: Screen, seen: State, now: u64) -> StateRecord {
    let Screen { state, rule } = shown;
    let screen = (seen != state).then(|| seen.word().to_string());
    crate::registry::update_state(files, |current| {
        if let Some(p) = current {
            // A transient Unknown sample (herdr not seeing the agent for a
            // moment) must not reset a known state's clock. Only a definite
            // state replaces one.
            if state == State::Unknown && p.state != "unknown" {
                return p.clone();
            }
            if p.state == state.word() {
                // Still blocked, at another dialog: name the new one, but the
                // agent has been waiting on a human all along.
                p.rule = rule;
                // Likewise a transient Unknown under a known screen.
                if seen != State::Unknown || screen.is_none() {
                    p.screen = screen;
                }
                return p.clone();
            }
        }
        let finished_unwatched = state == State::Idle && current.as_ref().is_some_and(|p| p.state == "working");
        let record = StateRecord {
            state: state.word().to_string(),
            since: now,
            seen: !finished_unwatched,
            rule,
            screen,
            screen_changed_ms: current.as_ref().and_then(|p| p.screen_changed_ms),
        };
        *current = Some(record.clone());
        record
    })
}

/// Mark a finished session as seen (done → idle). Called from the HUD when
/// the user previews or jumps to the agent, and from `run`'s monitor when the
/// agent's own window gains focus. The state word and the clock pass through
/// unchanged; only the seen bit flips.
pub(crate) fn ack_session_files(files: &Files) {
    crate::registry::update_state(files, |current| {
        if let Some(p) = current {
            p.seen = true;
        }
    });
}

/// Ack by session id (the HUD's handle).
pub(crate) fn ack_session(id: &str) {
    ack_session_files(&Files::of(id));
}

/// One pass over a session for `run`'s monitor: herdr's verdict fused with
/// the ledger and stamped into `.state` (marking a working→idle completion as
/// unseen — the "done" state). This is what keeps "idle for" truthful while
/// no HUD or status invocation is watching. Returns the fused state, whether
/// the completion has been seen, and whether the agent is in a turn.
pub(crate) fn track(files: &Files, cmd: &str, agent: Option<&herdr::Agent>) -> (State, bool, bool) {
    let (state, record, _, in_turn) = settle(files, cmd, screen_of(agent, cmd), crate::clock::unix_now());
    (state, record.seen, in_turn)
}

/// Compact human duration for table/HUD cells: 42s, 7m, 1h05m, 3d.
pub(crate) fn humanize_secs(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => {
            let m = (secs % 3600) / 60;
            if m == 0 {
                format!("{}h", secs / 3600)
            } else {
                format!("{}h{m:02}m", secs / 3600)
            }
        }
        _ => format!("{}d", secs / 86_400),
    }
}

/// Titles an agent shows before it has named the conversation: its own product
/// banner, which says strictly less than the launch directory does. Treated as
/// no title at all.
const PLACEHOLDER_TITLES: &[&str] = &["Claude Code"];

/// The conversation's display name, out of the window title herdr reports
/// for an agent started as `cmd` (its command's first word).
///
/// Claude stamps the conversation's generated `aiTitle` into the terminal
/// title behind a state glyph, so dropping the glyph leaves exactly the line
/// `claude --resume` lists. Comes back empty when there is nothing to show —
/// a not-yet-named session's banner, the command line the shell put there
/// before the agent set its own (`claude --model haiku`), or one of the
/// punctuation-only titles Claude sometimes emits — so the caller can fall
/// back to the directory.
pub(crate) fn conversation_title(raw: &str, cmd: &str) -> String {
    let title = raw
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '"' && c != '\'')
        .trim();
    let command_line = !cmd.is_empty() && (title == cmd || title.starts_with(&format!("{cmd} ")));
    if PLACEHOLDER_TITLES.contains(&title) || command_line {
        String::new()
    } else {
        title.to_string()
    }
}
