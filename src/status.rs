//! Milestone 2: derive each tracked agent's state from its teed output.
//!
//! Classification is herdr's vendored manifest engine (see
//! `vendor/herdr_detect/`): we render the raw tail into a screen via vt100,
//! extract the current OSC title from the stream, and hand both to
//! `detect_agent_with_osc()`. The engine's per-agent TOML rules (bundled, plus
//! self-updating from herdr.dev) decide idle/working/blocked.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

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
}

/// The three questions the list can be ordered to answer.
#[derive(Clone, Copy, PartialEq, Eq)]
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

    pub(crate) fn parse(s: &str) -> Option<Sort> {
        match s {
            "urgency" => Some(Sort::Urgency),
            "dir" => Some(Sort::Dir),
            "recent" => Some(Sort::Recent),
            _ => None,
        }
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
    let dir = crate::cache_dir();
    let mut sessions: Vec<Session> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("meta") {
                continue;
            }
            if let Some(session) = read_session(&dir, &path) {
                sessions.push(session);
            }
        }
    }
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
    if sessions.is_empty() {
        println!("no agents tracked");
        return 0;
    }

    let dir_w = sessions.iter().map(|s| s.dir.len()).max().unwrap_or(3).max(3);
    let cmd_w = sessions.iter().map(|s| s.cmd.len()).max().unwrap_or(3).max(3);
    for s in &sessions {
        println!(
            "{:<11}  {:<dw$}  {:<cw$}  {:>6}",
            s.state.badge(),
            s.dir,
            s.cmd,
            humanize_secs(s.state_secs),
            dw = dir_w,
            cw = cmd_w,
        );
    }
    if let Some(warning) = crate::manifest_health_warning() {
        eprintln!("⚠ {warning}");
    }
    0
}

/// `seagents status --json`: machine-readable session list, one array on
/// stdout. This is the integration surface for widget frontends (eww etc.):
/// poll this, render rows, and wire clicks to `seagents focus <pid>`.
pub fn print_json(sort: Sort) -> i32 {
    let sessions = collect(sort);
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
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string(&serde_json::Value::Array(arr)).unwrap_or_else(|_| "[]".into())
    );
    0
}

/// Build a Session from one `.meta` file (and its sibling `.raw`).
fn read_session(dir: &Path, meta_path: &Path) -> Option<Session> {
    let text = std::fs::read_to_string(meta_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;

    let wrapper_pid = v["wrapper_pid"].as_u64()?;
    let child_pid = v["child_pid"].as_u64();
    let cwd = v["cwd"].as_str().unwrap_or("?");
    let cmd = v["argv"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|s| s.as_str())
        .map(basename)
        .unwrap_or_else(|| "?".to_string());

    // Liveness: is the child process still around? If not, the session has
    // ended — reap its files and drop it entirely rather than showing a corpse.
    let raw = dir.join(format!("{wrapper_pid}.raw"));
    let alive = child_pid
        .map(|p| Path::new(&format!("/proc/{p}")).exists())
        .unwrap_or(false);
    if !alive {
        let _ = std::fs::remove_file(meta_path);
        let _ = std::fs::remove_file(&raw);
        let _ = std::fs::remove_file(dir.join(format!("{wrapper_pid}.state")));
        return None;
    }

    // 1 MB tail: big enough to contain a full screen repaint for rendering.
    let state = match read_tail(&raw, 1024 * 1024) {
        Some(buf) => {
            // Prefer the child's *live* pty size (accurate, survives resize and
            // missing meta); fall back to meta, then a sane default.
            let (rows, cols) = child_pid
                .and_then(live_pty_size)
                .or_else(|| Some((v["rows"].as_u64()? as u16, v["cols"].as_u64()? as u16)))
                .unwrap_or((24, 80));
            let screen = crate::detect::render(&buf, rows, cols);
            let title = current_title(&buf).unwrap_or_default();
            let agent = crate::vendor::herdr_detect::identify_agent(&cmd);
            let detection = crate::vendor::herdr_detect::detect_agent_with_osc(
                agent, &screen, &title, "",
            );
            State::from(detection.state)
        }
        None => State::Unknown,
    };

    let now = unix_now();
    let record = observe_state(
        &dir.join(format!("{wrapper_pid}.state")),
        state,
        now,
        false,
    );
    // Present unacknowledged completions as their own state.
    let state = if state == State::Idle && !record.seen {
        State::Done
    } else {
        state
    };

    Some(Session {
        id: wrapper_pid,
        state,
        dir: basename(cwd),
        cwd: cwd.to_string(),
        cmd,
        pid: child_pid.unwrap_or(wrapper_pid),
        state_secs: now.saturating_sub(record.since),
    })
}

/// What the `.state` sidecar knows about a session: the last observed engine
/// state, when it started, and whether the user has seen it since finishing
/// (the "done vs idle" distinction).
struct StateRecord {
    since: u64,
    seen: bool,
}

/// Record `state` into the sidecar if it differs from what's there; returns
/// the record for the current state. Multiple observers (wrapper monitor + an
/// open HUD) may race on a transition — both write since≈now, so the
/// divergence is bounded by one poll interval. Harmless.
///
/// `mark_unseen`: true only for the wrapper's own monitor — a working→idle
/// transition it observes is stamped `seen: false` (the agent finished and
/// nobody has looked yet). Readers (HUD/status fallback writes for
/// pre-monitor sessions) always write `seen: true`, so unmonitored sessions
/// simply never show "done" rather than showing it forever.
fn observe_state(path: &Path, state: State, now: u64, mark_unseen: bool) -> StateRecord {
    let prev: Option<serde_json::Value> = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    if let Some(p) = &prev {
        let prev_state = p["state"].as_str();
        let keep = prev_state == Some(state.word())
            // A transient Unknown sample (a render caught mid-repaint, e.g.
            // the focus-in repaint a HUD preview triggers) must not reset a
            // known state's clock. Only a definite state replaces one.
            || (state == State::Unknown && prev_state.is_some_and(|s| s != "unknown"));
        if keep {
            return StateRecord {
                since: p["since"].as_u64().unwrap_or(now),
                seen: p["seen"].as_bool().unwrap_or(true),
            };
        }
    }
    let finished_unwatched = mark_unseen
        && state == State::Idle
        && prev
            .as_ref()
            .and_then(|p| p["state"].as_str())
            .is_some_and(|s| s == "working");
    let record = StateRecord {
        since: now,
        seen: !finished_unwatched,
    };
    write_state_record(path, state, &record);
    record
}

fn write_state_record(path: &Path, state: State, record: &StateRecord) {
    let json = serde_json::json!({
        "state": state.word(),
        "since": record.since,
        "seen": record.seen,
    })
    .to_string();
    // Atomic tmp+rename so a concurrent reader never sees a half-written
    // record; pid-suffixed so the wrapper monitor and a HUD poll can't
    // stomp each other's tmp file.
    let tmp = path.with_extension(format!("state.tmp{}", std::process::id()));
    if std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Mark a finished session as seen (done → idle). Called from the HUD when
/// the user previews or jumps to the agent, and from the wrapper's monitor
/// when the agent's own window gains focus.
pub(crate) fn ack_session_path(path: &Path) {
    let Some(p) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return;
    };
    if p["seen"].as_bool() == Some(false) {
        if let Some(state_word) = p["state"].as_str() {
            let record = StateRecord {
                since: p["since"].as_u64().unwrap_or(unix_now()),
                seen: true,
            };
            // State word passes through unchanged; only the seen bit flips.
            let state = match state_word {
                "blocked" => State::Blocked,
                "working" => State::Working,
                "idle" => State::Idle,
                _ => State::Unknown,
            };
            write_state_record(path, state, &record);
        }
    }
}

/// Ack by session id (the HUD's handle).
pub(crate) fn ack_session(id: u64) {
    ack_session_path(&crate::cache_dir().join(format!("{id}.state")));
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One detection pass over our *own* session, for the wrapper's monitor
/// thread: re-classify from the teed tail and stamp the sidecar on
/// transitions (marking a working→idle completion as unseen — the "done"
/// state). This is what keeps "idle for" truthful while no HUD or status
/// invocation is watching. Returns the detected engine state plus whether
/// the completion has been seen, so the caller can notify and acknowledge.
pub(crate) fn track_session(
    raw: &Path,
    state_path: &Path,
    cmd: &str,
    child_pid: u64,
) -> Option<(State, bool)> {
    let buf = read_tail(raw, 1024 * 1024)?;
    let (rows, cols) = live_pty_size(child_pid).unwrap_or((24, 80));
    let screen = crate::detect::render(&buf, rows, cols);
    let title = current_title(&buf).unwrap_or_default();
    let agent = crate::vendor::herdr_detect::identify_agent(cmd);
    let state = State::from(
        crate::vendor::herdr_detect::detect_agent_with_osc(agent, &screen, &title, "").state,
    );
    let record = observe_state(state_path, state, unix_now(), true);
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

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
        .to_string()
}

/// Read the child's live terminal size via TIOCGWINSZ on its pty (fd 0), found
/// through /proc. Returns (rows, cols). Read-only; `O_NOCTTY` avoids acquiring
/// the pty as our controlling terminal.
fn live_pty_size(child_pid: u64) -> Option<(u16, u16)> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY)
        .open(format!("/proc/{child_pid}/fd/0"))
        .ok()?;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
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
    fn observe_state_keeps_clock_through_transient_unknown() {
        let dir = std::env::temp_dir().join(format!("seagents-observe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1.state");

        assert_eq!(observe_state(&path, State::Idle, 100, false).since, 100);
        // A mid-repaint Unknown sample must not reset the idle clock…
        assert_eq!(observe_state(&path, State::Unknown, 150, false).since, 100);
        assert_eq!(observe_state(&path, State::Idle, 200, false).since, 100);
        // …but a real transition does.
        assert_eq!(observe_state(&path, State::Blocked, 250, false).since, 250);
        assert_eq!(observe_state(&path, State::Blocked, 300, false).since, 250);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn done_lifecycle_marks_unwatched_completion_and_acks() {
        let dir = std::env::temp_dir().join(format!("seagents-done-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1.state");

        // Wrapper monitor observes: working, then idle → unseen completion.
        assert!(observe_state(&path, State::Working, 100, true).seen);
        let rec = observe_state(&path, State::Idle, 200, true);
        assert!(!rec.seen, "working→idle by the wrapper should be unseen");
        assert_eq!(rec.since, 200);

        // A reader polling the same state keeps the unseen bit and the clock.
        let rec = observe_state(&path, State::Idle, 260, false);
        assert!(!rec.seen);
        assert_eq!(rec.since, 200);

        // Acknowledgment flips seen without touching the clock.
        ack_session_path(&path);
        let rec = observe_state(&path, State::Idle, 300, true);
        assert!(rec.seen);
        assert_eq!(rec.since, 200);

        // A reader-written transition (pre-monitor session) is never unseen.
        assert!(observe_state(&path, State::Working, 400, false).seen);
        assert!(observe_state(&path, State::Idle, 500, false).seen);

        std::fs::remove_dir_all(&dir).ok();
    }
}
