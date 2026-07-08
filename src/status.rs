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
    Working,
    Idle,
    Unknown,
}

impl State {
    /// Sort key — most urgent first.
    pub(crate) fn rank(self) -> u8 {
        match self {
            State::Blocked => 0,
            State::Working => 1,
            State::Idle => 2,
            State::Unknown => 3,
        }
    }

    pub(crate) fn badge(self) -> &'static str {
        match self {
            State::Blocked => "🔴 blocked",
            State::Working => "🟡 working",
            State::Idle => "🟢 idle",
            State::Unknown => "⚪ unknown",
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
    pub(crate) state: State,
    pub(crate) dir: String,
    pub(crate) cmd: String,
    pub(crate) pid: u64,
    /// Seconds since the agent last produced output (the `.raw` tee mtime).
    /// An idle agent emits nothing, so this is "idle for"; for a blocked agent
    /// it's how long it has been waiting on the user.
    pub(crate) quiet_secs: u64,
}

/// Gather every tracked session, sorted most-urgent first. Shared by the
/// `status` table and the HUD.
pub(crate) fn collect() -> Vec<Session> {
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
    // Alphabetical by directory so agents sharing a cwd sit together; urgency
    // (blocked > working > idle) orders within a group, colors still flag
    // blocked rows at a glance.
    sessions.sort_by_key(|s| (s.dir.clone(), s.state.rank(), s.pid));
    sessions
}

/// Scan `~/.cache/seagents/*.meta`, classify each session, print a table.
pub fn print() -> i32 {
    let sessions = collect();
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
            humanize_secs(s.quiet_secs),
            dw = dir_w,
            cw = cmd_w,
        );
    }
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

    // Last-output age from the tee file's mtime — cheap and already accurate:
    // the pump flushes on every chunk, and quiet agents write nothing.
    let quiet_secs = std::fs::metadata(&raw)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Some(Session {
        state,
        dir: basename(cwd),
        cmd,
        pid: child_pid.unwrap_or(wrapper_pid),
        quiet_secs,
    })
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
}
