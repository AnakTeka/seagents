//! The wrapper's self-monitor: re-detects its own agent's state, notifies on
//! transitions worth interrupting for, and keeps the reboot snapshot current.

use std::fs;
use std::path::Path;
use std::thread;

use crate::registry::{self, Files};
use crate::status;
use crate::window;

/// Self-monitor: re-detect our own agent's state every 2s and stamp the
/// .state sidecar on transitions. The wrapper is the only process
/// guaranteed to live exactly as long as the agent, so this keeps
/// "idle for / waiting for" truthful even when no HUD or status poll is
/// watching — opening the HUD hours later shows the real durations.
pub(super) fn spawn(files: Files, anchor_pid: u64, cmd: &str, cwd: &Path) {
    let cmd0 = Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd)
        .to_string();
    let dir0 = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    thread::spawn(move || {
        // No new output ⇒ no state change (even resizes repaint), so skip
        // the vt100 render entirely while the tee is unchanged. This is
        // what makes an idle agent's monitor cost ~zero instead of a
        // permanent ~1% of a core re-detecting the same bytes.
        let mut last_seen: Option<(u64, std::time::SystemTime)> = None;
        // In-memory previous state for transition notifications — kept
        // separate from the sidecar on purpose: another observer (an open
        // HUD) may stamp the sidecar first, but the notification decision
        // must always be made from this wrapper's own observations.
        let mut prev: Option<status::State> = None;
        loop {
            let seen = fs::metadata(&files.raw)
                .ok()
                .and_then(|m| m.modified().ok().map(|t| (m.len(), t)));
            if seen.is_none() || seen != last_seen {
                last_seen = seen;
                if let Some((state, acked)) = status::track_session(&files, &cmd0, anchor_pid) {
                    // Read the con_id fresh: the input pump and focus
                    // fallback heal it in the meta over time, and a stale
                    // copy here would break focused-window suppression.
                    let con_id = registry::read_meta(&files).and_then(|m| m.con_id);
                    if let Some(prev) = prev {
                        notify_transition(prev, state, &dir0, &cmd0, con_id);
                    }
                    prev = Some(state);
                    // Unacknowledged completion + the agent's window is
                    // focused → the user is looking at it: done → idle.
                    // (Focusing triggers a repaint, which is exactly what
                    // woke this pass — so the check happens when needed.)
                    if state == status::State::Idle
                        && !acked
                        && con_id.is_some()
                        && window::focused_con_id() == con_id
                    {
                        status::ack_session_files(&files);
                    }
                }
            }
            // Keep the reboot snapshot current. Globally throttled, so
            // all but one wrapper per interval pay a single stat() here —
            // and it lives outside the output-changed guard on purpose:
            // moving a window to another workspace changes the layout
            // without producing a byte of agent output.
            crate::snapshot::maybe_capture();
            thread::sleep(std::time::Duration::from_secs(2));
        }
    });
}

/// Desktop notification for a state transition worth interrupting the user
/// for: the agent finished (working → idle) or is waiting on input
/// (→ blocked). Suppressed while the agent's own window is focused — the
/// user is already looking at it (same idea as herdr's focus-aware
/// notifications; mechanism differs since Terminator supports neither OSC 9
/// nor OSC 99, so we shell out to notify-send). SEAGENTS_NO_NOTIFY=1 disables.
fn notify_transition(
    prev: status::State,
    state: status::State,
    dir: &str,
    cmd: &str,
    con_id: Option<u64>,
) {
    use status::State;
    if std::env::var_os("SEAGENTS_NO_NOTIFY").is_some() {
        return;
    }
    let (summary, body, urgency) = match (prev, state) {
        (State::Working, State::Idle) => (
            format!("{cmd} · {dir}: done"),
            "finished and waiting at the prompt".to_string(),
            "normal",
        ),
        (p, State::Blocked) if p != State::Blocked && p != State::Unknown => (
            format!("{cmd} · {dir}: needs you"),
            "waiting for your input".to_string(),
            "critical",
        ),
        _ => return,
    };
    if con_id.is_some() && window::focused_con_id() == con_id {
        return;
    }
    // Reap in a detached thread: a dropped Child is never wait()ed, so each
    // finished notify-send would otherwise linger as a zombie for the life of
    // this wrapper process.
    if let Ok(mut child) = std::process::Command::new("notify-send")
        .args(["-a", "seagents", "-u", urgency, "-t", "8000", &summary, &body])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
