//! The wrapper's self-monitor: re-detects its own agent's state, notifies on
//! transitions worth interrupting for, and keeps the reboot snapshot current.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::registry::{self, Files};
use crate::status;
use crate::window;

/// The monitor's pace when nothing is happening.
const TICK: Duration = Duration::from_secs(2);

/// After input reaches a gated agent: look this often, for this long. The
/// agent repaints once it has read the key, not when it is written, so one
/// immediate look would mostly see the gate still up.
const QUICK_TICK: Duration = Duration::from_millis(100);
const QUICK_FOR: Duration = Duration::from_secs(1);

/// How the rest of the wrapper hurries its monitor.
///
/// `.state` is what `wait` and `send` read the gate from, and this monitor is
/// its writer — so right after a gate is answered it would say `blocked`
/// for up to a tick, and a `wait` called in that instant would return
/// `blocked` again. Input into a gated agent therefore wakes the monitor for
/// a burst of quick looks, and a `send --answer-permission` can wait for the
/// verdict to clear before it replies.
#[derive(Default)]
pub(super) struct Wake {
    flags: Mutex<Flags>,
    changed: Condvar,
}

#[derive(Default)]
struct Flags {
    /// Input arrived since the monitor last slept.
    nudged: bool,
    /// The monitor's last verdict was `blocked`.
    blocked: bool,
}

impl Wake {
    fn flags(&self) -> std::sync::MutexGuard<'_, Flags> {
        self.flags.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Something was written into the agent. Only a gated agent needs a
    /// quick look: anywhere else the next tick is soon enough.
    pub(super) fn input(&self) {
        let mut f = self.flags();
        if f.blocked {
            f.nudged = true;
            self.changed.notify_all();
        }
    }

    /// Wait, up to `within`, for the monitor to see no gate; whether it did.
    pub(super) fn wait_unblocked(&self, within: Duration) -> bool {
        let f = self.flags();
        !self
            .changed
            .wait_timeout_while(f, within, |f| f.blocked)
            .unwrap_or_else(|e| e.into_inner())
            .0
            .blocked
    }

    fn set_blocked(&self, blocked: bool) {
        let mut f = self.flags();
        if f.blocked != blocked {
            f.blocked = blocked;
            self.changed.notify_all();
        }
    }

    /// Sleep `d`, or less when nudged; whether it was.
    fn sleep(&self, d: Duration) -> bool {
        let f = self.flags();
        let mut f = self
            .changed
            .wait_timeout_while(f, d, |f| !f.nudged)
            .unwrap_or_else(|e| e.into_inner())
            .0;
        std::mem::take(&mut f.nudged)
    }
}

/// Self-monitor: re-detect our own agent's state every 2s and stamp the
/// .state sidecar on transitions. The wrapper is the only process
/// guaranteed to live exactly as long as the agent, so this keeps
/// "idle for / waiting for" truthful even when no HUD or status poll is
/// watching — opening the HUD hours later shows the real durations.
pub(super) fn spawn(files: Files, anchor_pid: u64, cmd: &str, cwd: &Path, wake: Arc<Wake>) {
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
        // Nothing new to look at ⇒ no state change (even resizes repaint),
        // so skip the vt100 render entirely while neither input to the
        // state has changed. This is what makes an idle agent's monitor cost
        // ~zero instead of a permanent ~1% of a core re-detecting the same
        // bytes.
        let mut last_seen: Option<Inputs> = None;
        // In-memory previous state for transition notifications — kept
        // separate from the sidecar on purpose: another observer (an open
        // HUD) may stamp the sidecar first, but the notification decision
        // must always be made from this wrapper's own observations.
        let mut prev: Option<status::State> = None;
        let mut quick_until: Option<Instant> = None;
        loop {
            let seen = Inputs::of(&files);
            if seen.raw.is_none() || Some(&seen) != last_seen.as_ref() {
                last_seen = Some(seen);
                if let Some((state, acked)) = status::track_session(&files, &cmd0, anchor_pid) {
                    // Read the con_id fresh: the input pump and focus
                    // fallback heal it in the meta over time, and a stale
                    // copy here would break focused-window suppression.
                    let con_id = registry::read_meta(&files).and_then(|m| m.con_id);
                    if let Some(prev) = prev {
                        notify_transition(prev, state, &dir0, &cmd0, con_id);
                    }
                    prev = Some(state);
                    // After `.state` is written: whoever waits on this reads
                    // it next.
                    wake.set_blocked(state == status::State::Blocked);
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
            let tick = match quick_until {
                Some(t) if Instant::now() < t => QUICK_TICK,
                _ => TICK,
            };
            if wake.sleep(tick) {
                quick_until = Some(Instant::now() + QUICK_FOR);
            }
        }
    });
}

/// What the state is made of, as of one look: the tee (the screen) and the
/// `.agent` turn ledger, each as (length, mtime). A hook agent's turn ends at
/// its `Stop` hook, which need not be followed by a byte of output — watching
/// the tee alone left `.state` at `working` after the turn had ended, with no
/// "finished" notification and no done marker until something repainted.
#[derive(PartialEq, Eq)]
struct Inputs {
    raw: Option<(u64, std::time::SystemTime)>,
    agent: Option<(u64, std::time::SystemTime)>,
}

impl Inputs {
    fn of(files: &Files) -> Inputs {
        let stamp = |p: &Path| fs::metadata(p).ok().and_then(|m| m.modified().ok().map(|t| (m.len(), t)));
        Inputs {
            raw: stamp(&files.raw),
            agent: stamp(&files.agent),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ledger_change_alone_is_something_new_to_look_at() {
        let dir = std::env::temp_dir().join(format!("seagents-inputs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = Files::in_dir(&dir, 1);
        fs::write(&files.raw, b"screen").unwrap();
        let before = Inputs::of(&files);
        assert!(before == Inputs::of(&files), "nothing changed");
        // The Stop hook rewrites `.agent`; the tee is untouched.
        fs::write(&files.agent, br#"{"stops":1}"#).unwrap();
        assert!(before != Inputs::of(&files));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn input_hurries_the_monitor_only_at_a_gate() {
        let wake = Wake::default();
        wake.input();
        let t = Instant::now();
        assert!(!wake.sleep(Duration::from_millis(50)), "not gated: no nudge");
        assert!(t.elapsed() >= Duration::from_millis(50));

        wake.set_blocked(true);
        wake.input();
        let t = Instant::now();
        assert!(wake.sleep(TICK), "gated: woken at once");
        assert!(t.elapsed() < TICK);
        assert!(!wake.sleep(Duration::ZERO), "one nudge, one wake");
    }

    #[test]
    fn an_answer_waits_for_the_monitor_to_see_the_gate_go() {
        let wake = Arc::new(Wake::default());
        assert!(wake.wait_unblocked(Duration::ZERO), "no gate: at once");
        wake.set_blocked(true);
        assert!(!wake.wait_unblocked(Duration::from_millis(20)), "a key that didn't clear it");
        let monitor = Arc::clone(&wake);
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            monitor.set_blocked(false);
        });
        assert!(wake.wait_unblocked(Duration::from_secs(5)));
        t.join().unwrap();
    }
}
