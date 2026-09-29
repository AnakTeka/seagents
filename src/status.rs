//! Milestone 2: derive each tracked agent's state from its teed output.
//!
//! Classification is herdr's vendored manifest engine (see
//! `vendor/herdr_detect/`): we render the raw tail into a screen via vt100,
//! extract the current OSC title from the stream, and hand both to
//! `detect_agent_with_osc()`. The engine's per-agent TOML rules (bundled, plus
//! self-updating from herdr.dev) decide idle/working/blocked.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::registry::{Files, StateRecord};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Blocked,
    /// Finished (engine says idle) but the user hasn't looked at it yet — an
    /// unread marker, not a detection state. Downgrades to Idle on
    /// acknowledgment (HUD preview/jump, or the agent's window gaining focus).
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
}

impl From<crate::vendor::herdr_detect::AgentState> for State {
    fn from(state: crate::vendor::herdr_detect::AgentState) -> Self {
        use crate::vendor::herdr_detect::AgentState;
        match state {
            AgentState::Blocked => State::Blocked,
            AgentState::Working => State::Working,
            AgentState::Idle => State::Idle,
            AgentState::Unknown => State::Unknown,
        }
    }
}

pub(crate) struct Session {
    /// Stable session id (the wrapper's pid — the cache-file key). External
    /// consumers should treat it as opaque; a future herdr-socket source will
    /// mint ids that are not pids.
    pub(crate) id: u64,
    pub(crate) state: State,
    pub(crate) dir: String,
    /// Full launch cwd (dir is its basename).
    pub(crate) cwd: String,
    pub(crate) cmd: String,
    pub(crate) pid: u64,
    /// Seconds the agent has been in its current state ("idle for" on green
    /// rows, "waiting on you for" on red ones). Tracked via observed state
    /// *transitions* persisted in the `.state` sidecar — NOT output age: an
    /// idle Claude still emits bytes when its window is merely focused (the
    /// terminal's focus-in report triggers a repaint), so a preview from the
    /// HUD would reset an output-mtime clock.
    pub(crate) state_secs: u64,
    /// The conversation's own name, as the agent stamps it into the window
    /// title: Claude writes its generated `aiTitle` there (behind a state
    /// glyph), which is the human-readable line `claude --resume` lists. Glyph
    /// stripped; empty when the agent hasn't named the conversation yet, so a
    /// caller showing it can fall back to the directory.
    pub(crate) title: String,
    /// The agent's name — what every command addresses it by.
    pub(crate) tag: String,
    /// Its spawner's tag, and its spawn generation (0 without a parent).
    pub(crate) parent: Option<String>,
    pub(crate) depth: u32,
    /// What the agent reports about itself (conversation, turn ledger); the
    /// empty record for an agent without hooks.
    pub(crate) agent: crate::registry::AgentRecord,
    /// [`crate::turn::Ledger::in_turn`], as of this read.
    pub(crate) in_turn: bool,
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
/// table and the HUD.
pub(crate) fn collect(sort: Sort) -> Vec<Session> {
    // Sessions whose agent has exited are dropped entirely — their files
    // reaped — rather than shown as corpses.
    crate::registry::reap();
    let mut sessions: Vec<Session> =
        crate::registry::list().iter().map(read_session).collect();
    match sort {
        // Within a rank, longest-in-state first: the blocked agent you've
        // kept waiting longest tops the list.
        Sort::Urgency => sessions.sort_by_key(|s| {
            (
                s.state.rank(),
                std::cmp::Reverse(s.state_secs),
                s.dir.clone(),
                s.pid,
            )
        }),
        Sort::Dir => sessions.sort_by_key(|s| (s.dir.clone(), s.state.rank(), s.pid)),
        Sort::Recent => sessions.sort_by_key(|s| (s.state_secs, s.state.rank(), s.pid)),
    }
    sessions
}

/// Scan `~/.cache/seagents/*.meta`, classify each session, print a table.
pub fn print(sort: Sort) -> i32 {
    let sessions = collect(sort);
    // Cheap piggyback: any `status` invocation also keeps the reboot snapshot
    // fresh, which matters for agents whose wrappers predate it.
    crate::snapshot::maybe_capture();
    if sessions.is_empty() {
        println!("no agents tracked");
        return 0;
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
    if let Some(warning) = crate::detect::manifest_health_warning() {
        eprintln!("⚠ {warning}");
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
                "dir": s.dir,
                "cwd": s.cwd,
                "cmd": s.cmd,
                "pid": s.pid,
                "state_secs": s.state_secs,
                "title": s.title,
                "tag": s.tag,
                "parent": s.parent,
                "depth": s.depth,
                "stops": s.agent.stops,
                "in_turn": s.in_turn,
                "transcript_path": s.agent.transcript_path,
                "session_id": s.agent.session_id,
                "capabilities": crate::adapter::capabilities(&s.cmd),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string(&serde_json::Value::Array(arr)).unwrap_or_else(|_| "[]".into())
    );
    0
}

/// Build a Session from one live session's `.meta` (and its sibling `.raw`).
fn read_session(live: &crate::registry::Session) -> Session {
    let meta = &live.meta;
    let child_pid = live.child_pid();
    let cwd = or_unknown(&meta.cwd);
    let cmd = or_unknown(meta.agent());

    // Prefer the child's *live* pty size (accurate, survives resize and
    // missing meta); fall back to meta, then a sane default.
    let size = crate::procfs::live_pty_size(child_pid).or_else(|| Some((meta.rows?, meta.cols?)));
    let (screen, title) = detect_screen(&live.files.raw, size, &cmd)
        .unwrap_or((Screen { state: State::Unknown, rule: None }, String::new()));

    let now = crate::clock::unix_now();
    let (state, record, agent, in_turn) = settle(&live.files, &cmd, screen, now, false);
    // Present unacknowledged completions as their own state.
    let state = if state == State::Idle && !record.seen {
        State::Done
    } else {
        state
    };

    Session {
        id: meta.wrapper_pid,
        state,
        dir: or_unknown(meta.dir()),
        cwd,
        cmd,
        pid: child_pid,
        state_secs: now.saturating_sub(record.since),
        title,
        tag: meta.tag.clone(),
        parent: meta.parent.clone(),
        depth: meta.depth,
        agent,
        in_turn,
    }
}

/// The detector's verdict on one screen.
struct Screen {
    state: State,
    /// For `blocked`: the id of the manifest rule that matched — which gate
    /// the agent is at (`live_blocked_form`, a permission prompt, …).
    rule: Option<String>,
}

/// Classify the screen the tee's tail renders to, at `size` (rows, cols;
/// 80×24 when unknown): the engine's verdict, and the conversation title the
/// agent stamped into the window title — parsed once, used twice. None when
/// there is no tee to read.
fn detect_screen(raw: &Path, size: Option<(u16, u16)>, cmd: &str) -> Option<(Screen, String)> {
    use crate::vendor::herdr_detect::{self as engine, manifest};
    // 1 MB tail: big enough to contain a full screen repaint for rendering.
    let buf = read_tail(raw, 1024 * 1024)?;
    let (rows, cols) = size.unwrap_or((24, 80));
    let screen = crate::detect::render(&buf, rows, cols);
    let title = current_title(&buf).unwrap_or_default();
    let agent = engine::identify_agent(cmd);
    let state = State::from(engine::detect_agent_with_osc(agent, &screen, &title, "").state);
    // The fast path reports no rule, and a blocked screen is rare, so the
    // gate is named by a second, explaining pass only then.
    let rule = agent.filter(|_| state == State::Blocked).and_then(|agent| {
        let input = manifest::DetectionInput {
            screen: &screen,
            osc_title: &title,
            osc_progress: "",
        };
        manifest::explain_with_input(agent, input).matched_rule.map(|r| r.id)
    });
    Some((Screen { state, rule }, conversation_title(&title)))
}

/// Fold what the screen shows together with the turn ledger, record it in
/// `.state`, and report (the fused state, the record, the ledger, whether
/// the agent is in a turn).
///
/// For an agent whose turns come from hooks the ledger is exact where the
/// screen is not — Claude streams a plain-text answer with no spinner, which
/// the detector reads as idle — so: blocked (screen) wins; else in a turn →
/// working; else the screen's verdict. Recording the fused state (rather
/// than the raw one) is what keeps the done marker and the "finished"
/// notification from firing mid-answer. Screen adapters record the screen,
/// and their turn rule then reads it back.
fn settle(
    files: &Files,
    cmd: &str,
    screen: Screen,
    now: u64,
    mark_unseen: bool,
) -> (State, StateRecord, crate::registry::AgentRecord, bool) {
    let agent = crate::registry::read_agent(files);
    let ledger = crate::turn::Ledger::for_command(cmd);
    if ledger.turns == crate::adapter::Turns::Hook {
        let in_turn = ledger.in_turn(&agent, None);
        let shown = if in_turn && screen.state != State::Blocked {
            Screen { state: State::Working, rule: None }
        } else {
            screen
        };
        let state = shown.state;
        (state, observe_state(files, shown, now, mark_unseen), agent, in_turn)
    } else {
        let state = screen.state;
        let record = observe_state(files, screen, now, mark_unseen);
        let in_turn = ledger.in_turn(&agent, Some(&record));
        (state, record, agent, in_turn)
    }
}

/// A `.meta` field for display, `?` when an older wrapper didn't record it.
fn or_unknown(s: &str) -> String {
    if s.is_empty() { "?" } else { s }.to_string()
}

/// Record `state` into the sidecar if it differs from what's there; returns
/// the record for the current state. Multiple observers (wrapper monitor + an
/// open HUD) may race on a transition — both write since≈now, so the
/// divergence is bounded by one poll interval. Harmless. (Each observer's
/// read-decide-write is one locked registry update, so a race can only ever
/// order two whole decisions, never interleave them.)
///
/// `mark_unseen`: true only for the wrapper's own monitor — a working→idle
/// transition it observes is stamped `seen: false` (the agent finished and
/// nobody has looked yet). Readers (HUD/status fallback writes for
/// pre-monitor sessions) always write `seen: true`, so unmonitored sessions
/// simply never show "done" rather than showing it forever.
fn observe_state(files: &Files, screen: Screen, now: u64, mark_unseen: bool) -> StateRecord {
    let Screen { state, rule } = screen;
    crate::registry::update_state(files, |current| {
        if let Some(p) = current {
            // A transient Unknown sample (a render caught mid-repaint, e.g.
            // the focus-in repaint a HUD preview triggers) must not reset a
            // known state's clock. Only a definite state replaces one.
            if state == State::Unknown && p.state != "unknown" {
                return p.clone();
            }
            if p.state == state.word() {
                // Still blocked, at another gate: name the new one, but the
                // agent has been waiting on a human all along.
                p.rule = rule;
                return p.clone();
            }
        }
        let finished_unwatched = mark_unseen
            && state == State::Idle
            && current.as_ref().is_some_and(|p| p.state == "working");
        let record = StateRecord {
            state: state.word().to_string(),
            since: now,
            seen: !finished_unwatched,
            rule,
        };
        *current = Some(record.clone());
        record
    })
}

/// Mark a finished session as seen (done → idle). Called from the HUD when
/// the user previews or jumps to the agent, and from the wrapper's monitor
/// when the agent's own window gains focus. The state word and the clock pass
/// through unchanged; only the seen bit flips.
pub(crate) fn ack_session_files(files: &Files) {
    crate::registry::update_state(files, |current| {
        if let Some(p) = current {
            p.seen = true;
        }
    });
}

/// Ack by session id (the HUD's handle).
pub(crate) fn ack_session(id: u64) {
    ack_session_files(&Files::of(id));
}

/// One detection pass over our *own* session, for the wrapper's monitor
/// thread: re-classify from the teed tail and stamp the sidecar on
/// transitions (marking a working→idle completion as unseen — the "done"
/// state). This is what keeps "idle for" truthful while no HUD or status
/// invocation is watching. Returns the state (the screen fused with the turn
/// ledger, see `settle`) plus whether the completion has been
/// seen, so the caller can notify and acknowledge.
pub(crate) fn track_session(files: &Files, cmd: &str, child_pid: u64) -> Option<(State, bool)> {
    let (screen, _) = detect_screen(&files.raw, crate::procfs::live_pty_size(child_pid), cmd)?;
    let (state, record, _, _) = settle(files, cmd, screen, crate::clock::unix_now(), true);
    Some((state, record.seen))
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

/// Read at most `max` bytes from the end of a file — enough to hold the most
/// recent title without re-reading a long session every poll.
pub(crate) fn read_tail(path: &Path, max: u64) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > max {
        f.seek(SeekFrom::Start(len - max)).ok()?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// The current window title: the most recent non-empty OSC title in the stream.
/// Skipping empties matters for detection too: Claude clears the title
/// transiently, and the engine's title rules should see the last real one.
/// This is exactly what the terminal emulator mirrors into its X11 window title,
/// so it's the handle seagents uses to locate the agent's i3 window.
pub(crate) fn current_title(data: &[u8]) -> Option<String> {
    all_titles(data)
        .into_iter()
        .rev()
        .find(|t| !t.trim().is_empty())
}

/// Titles an agent shows before it has named the conversation: its own product
/// banner, which says strictly less than the launch directory does. Treated as
/// no title at all.
const PLACEHOLDER_TITLES: &[&str] = &["Claude Code"];

/// The conversation's display name, out of a raw window title.
///
/// Claude stamps the conversation's generated `aiTitle` into the terminal title
/// behind a state glyph, so dropping the glyph leaves exactly the line
/// `claude --resume` lists. Comes back empty when there is nothing to show — a
/// not-yet-named session's banner, or one of the punctuation-only titles Claude
/// sometimes emits — so the caller can fall back to the directory.
pub(crate) fn conversation_title(raw: &str) -> String {
    let title = strip_title_glyph(raw);
    if PLACEHOLDER_TITLES.contains(&title) {
        String::new()
    } else {
        title.to_string()
    }
}

/// Strip the leading state glyph Claude prefixes onto its window title
/// (`✳ `, `◐ `, …) so the remainder can be compared to a transcript's
/// `aiTitle` — or shown as the conversation's name.
pub(crate) fn strip_title_glyph(title: &str) -> &str {
    title
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '"' && c != '\'')
        .trim()
}

/// Collect the text of every OSC 0/1/2 (title) sequence in the stream, in order.
fn all_titles(data: &[u8]) -> Vec<String> {
    let mut titles = Vec::new();
    let n = data.len();
    let mut i = 0;
    while i + 1 < n {
        if data[i] == 0x1b && data[i + 1] == b']' {
            let num_start = i + 2;
            let mut j = num_start;
            while j < n && data[j].is_ascii_digit() {
                j += 1;
            }
            if j > num_start && j < n && data[j] == b';' {
                let num: u32 = std::str::from_utf8(&data[num_start..j])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(u32::MAX);
                let text_start = j + 1;
                // Terminator is BEL (0x07) or ST (ESC \).
                let mut k = text_start;
                let mut end: Option<(usize, usize)> = None;
                while k < n {
                    if data[k] == 0x07 {
                        end = Some((k, k + 1));
                        break;
                    }
                    if data[k] == 0x1b && k + 1 < n && data[k + 1] == b'\\' {
                        end = Some((k, k + 2));
                        break;
                    }
                    k += 1;
                }
                if let Some((text_end, after)) = end {
                    if num <= 2 {
                        if let Ok(s) = std::str::from_utf8(&data[text_start..text_end]) {
                            titles.push(s.to_string());
                        }
                    }
                    i = after;
                    continue;
                }
            }
        }
        i += 1;
    }
    titles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_titles_in_order() {
        let data = b"\x1b]0;first\x07 junk \x1b]2;\xe2\x9c\xb3 Claude Code\x07";
        assert_eq!(all_titles(data), vec!["first", "✳ Claude Code"]);
    }

    #[test]
    fn current_title_skips_trailing_empty() {
        // working spinner, then Claude clears the title → still the spinner title
        let data = b"\x1b]2;\xe2\xa0\x82 Claude Code\x07\x1b]2;\x07";
        assert_eq!(current_title(data).as_deref(), Some("⠂ Claude Code"));
    }

    #[test]
    fn strips_the_state_glyph_off_a_window_title() {
        assert_eq!(strip_title_glyph("\u{2733} Detection update"), "Detection update");
        assert_eq!(strip_title_glyph("\u{25d0} QC-1 Kas & Bank"), "QC-1 Kas & Bank");
        assert_eq!(strip_title_glyph("Detection update"), "Detection update");
        // A title that opens with punctuation keeps it.
        assert_eq!(strip_title_glyph("\u{2733} \"quoted\" thing"), "\"quoted\" thing");
    }

    #[test]
    fn conversation_title_drops_glyphs_placeholders_and_junk() {
        // The real thing: what `claude --resume` shows, behind a state glyph.
        assert_eq!(
            conversation_title("\u{2733} I3wm workspace restoration on reboot"),
            "I3wm workspace restoration on reboot"
        );
        // A session that hasn't named its conversation yet, and the stray
        // punctuation-only titles seen in live tees: nothing to show.
        assert_eq!(conversation_title("\u{2733} Claude Code"), "");
        assert_eq!(conversation_title(";"), "");
        assert_eq!(conversation_title(""), "");
    }

    fn sc(state: State) -> Screen {
        Screen { state, rule: None }
    }

    #[test]
    fn a_blocked_record_names_its_gate_and_a_new_gate_keeps_the_clock() {
        let dir = std::env::temp_dir().join(format!("seagents-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = Files::in_dir(&dir, 1);
        let gate = |rule: &str| Screen { state: State::Blocked, rule: Some(rule.into()) };
        let r = observe_state(&files, gate("live_blocked_form"), 100, true);
        assert_eq!((r.since, r.rule.as_deref()), (100, Some("live_blocked_form")));
        let r = observe_state(&files, gate("permission_prompt"), 150, true);
        assert_eq!((r.since, r.rule.as_deref()), (100, Some("permission_prompt")));
        assert_eq!(crate::registry::read_state(&files).unwrap().rule.as_deref(), Some("permission_prompt"));
        let r = observe_state(&files, sc(State::Working), 200, true);
        assert_eq!((r.since, r.rule), (200, None));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn observe_state_keeps_clock_through_transient_unknown() {
        let dir = std::env::temp_dir().join(format!("seagents-observe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = Files::in_dir(&dir, 1);

        assert_eq!(observe_state(&files, sc(State::Idle), 100, false).since, 100);
        // A mid-repaint Unknown sample must not reset the idle clock…
        assert_eq!(observe_state(&files, sc(State::Unknown), 150, false).since, 100);
        assert_eq!(observe_state(&files, sc(State::Idle), 200, false).since, 100);
        // …but a real transition does.
        assert_eq!(observe_state(&files, sc(State::Blocked), 250, false).since, 250);
        assert_eq!(observe_state(&files, sc(State::Blocked), 300, false).since, 250);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn done_lifecycle_marks_unwatched_completion_and_acks() {
        let dir = std::env::temp_dir().join(format!("seagents-done-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = Files::in_dir(&dir, 1);

        // Wrapper monitor observes: working, then idle → unseen completion.
        assert!(observe_state(&files, sc(State::Working), 100, true).seen);
        let rec = observe_state(&files, sc(State::Idle), 200, true);
        assert!(!rec.seen, "working→idle by the wrapper should be unseen");
        assert_eq!(rec.since, 200);

        // A reader polling the same state keeps the unseen bit and the clock.
        let rec = observe_state(&files, sc(State::Idle), 260, false);
        assert!(!rec.seen);
        assert_eq!(rec.since, 200);

        // Acknowledgment flips seen without touching the clock.
        ack_session_files(&files);
        let rec = observe_state(&files, sc(State::Idle), 300, true);
        assert!(rec.seen);
        assert_eq!(rec.since, 200);

        // A reader-written transition (pre-monitor session) is never unseen.
        assert!(observe_state(&files, sc(State::Working), 400, false).seen);
        assert!(observe_state(&files, sc(State::Idle), 500, false).seen);

        std::fs::remove_dir_all(&dir).ok();
    }
}
