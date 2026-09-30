//! The byte pumps between the user's terminal and the inner PTY: output (with
//! the tee), input (with window-identity healing), and resize.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use portable_pty::MasterPty;

use super::inject::{ModeScanner, PtyModes, SharedWriter};
use crate::registry::Files;
use crate::window;

/// Output pump: master -> stdout (+ tee). This owns the "T-junction".
///
/// It also watches the stream (without touching it) for the input modes the
/// agent switches on — bracketed paste, the kitty keyboard protocol — which
/// is how injection knows how to frame a paste and encode Enter
/// ([`super::inject`]).
///
/// It waits in `poll` rather than a blocking `read` so that it can also be
/// woken by [`OutputDrain::wait`]: the wrapper stops its *whole process* when
/// the agent suspends, pump thread included, and whatever the agent wrote just
/// before stopping — Claude's "run `fg` to resume" line — would otherwise sit
/// unread on the master until `fg`.
pub(super) fn spawn_output(
    mut reader: Box<dyn Read + Send>,
    master_fd: RawFd,
    tee: Option<File>,
    files: Files,
    modes: Arc<PtyModes>,
) -> (JoinHandle<()>, Option<OutputDrain>) {
    // Without a pipe (fd exhaustion) the pump just relays; suspends then work
    // as they did before draining existed.
    let (drain, wake) = match OutputDrain::new() {
        Some((drain, wake, done)) => (Some(drain), Some((wake, done))),
        None => (None, None),
    };
    let handle = thread::spawn(move || {
        let mut out = std::io::stdout();
        let mut tee = tee;
        let mut buf = [0u8; 8192];
        // Rotation bookkeeping: the tee is the only unbounded file in the
        // system (Claude repaints constantly; multi-day sessions reach
        // hundreds of MB). Detection only ever reads the last ~1 MB, so once
        // the file passes the cap we rewrite it keeping just that tail.
        // Checked every CHECK_EVERY bytes written, not per chunk.
        const ROTATE_AT: u64 = 16 * 1024 * 1024;
        const CHECK_EVERY: u64 = 4 * 1024 * 1024;
        let mut since_check: u64 = 0;
        let mut scanner = ModeScanner::default();
        // One read + relay. False once the pty is gone (child exited: EIO) or
        // stdout is.
        let mut relay = |reader: &mut Box<dyn Read + Send>| -> bool {
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => return false,
                    Ok(n) => {
                        let data = &buf[..n];
                        if out.write_all(data).is_err() {
                            return false;
                        }
                        let _ = out.flush();
                        scanner.feed(data, &modes);
                        if let Some(f) = tee.as_mut() {
                            let _ = f.write_all(data);
                            let _ = f.flush();
                            since_check += n as u64;
                            if since_check >= CHECK_EVERY {
                                since_check = 0;
                                let oversize =
                                    f.metadata().map(|m| m.len() > ROTATE_AT).unwrap_or(false);
                                if oversize {
                                    if let Some(rotated) = rotate_tee(&files) {
                                        tee = Some(rotated);
                                    }
                                }
                            }
                        }
                        return true;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return false,
                }
            }
        };
        loop {
            let wake_fd = wake.as_ref().map_or(-1, |(w, _)| w.as_raw_fd());
            let (output, drain_asked) = match poll_output(master_fd, wake_fd, true) {
                Some(ready) => ready,
                None => break,
            };
            if output && !relay(&mut reader) {
                break;
            }
            if let Some((wake, done)) = wake.as_ref().filter(|_| drain_asked) {
                let _ = (&*wake).read(&mut [0u8; 64]);
                // Relay until the master has nothing left. The agent is stopped
                // by now, so nothing new can arrive behind this.
                while poll_output(master_fd, -1, false) == Some((true, false)) {
                    if !relay(&mut reader) {
                        return;
                    }
                }
                let _ = done.send(());
            }
        }
    });
    (handle, drain)
}

/// `poll` the master for output and the drain-request pipe `wake` (-1: none);
/// blocking, or returning at once. `(output ready, drain asked)`, or `None`
/// when polling itself failed.
///
/// A hangup counts as output ready: the read that follows is what reports the
/// pty gone. The zero-timeout form is exact, not a guess: before answering
/// "nothing to read", the tty layer's `poll` flushes the work queue that moves
/// bytes the slave side wrote across to the master (`tty_buffer_flush_work`),
/// so a write that returned before the agent stopped is visible here.
fn poll_output(master_fd: RawFd, wake: RawFd, block: bool) -> Option<(bool, bool)> {
    // A negative fd is skipped by `poll` and reports no events.
    let mut fds = [
        libc::pollfd { fd: master_fd, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: wake, events: libc::POLLIN, revents: 0 },
    ];
    let timeout = if block { -1 } else { 0 };
    loop {
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if r >= 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
    let ready = |p: &libc::pollfd| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
    Some((ready(&fds[0]), ready(&fds[1])))
}

/// The wrapper's handle on the output pump for a suspend: see
/// [`OutputDrain::wait`].
pub(super) struct OutputDrain {
    wake: File,
    done: mpsc::Receiver<()>,
}

impl OutputDrain {
    /// A pipe to wake the pump's `poll` (the pump keeps its read end) and a
    /// channel for its answer.
    fn new() -> Option<(OutputDrain, File, mpsc::Sender<()>)> {
        let mut fds = [-1 as libc::c_int; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        let (read, write) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        Some((OutputDrain { wake: write, done: rx }, read, tx))
    }

    /// Block until everything already on the master has reached stdout (and
    /// the tee) — called once the agent has stopped, before the wrapper stops
    /// itself. Bounded, so a terminal that won't take output can delay a
    /// suspend but never swallow it; returns at once if the pump has ended.
    pub(super) fn wait(&self) {
        while self.done.try_recv().is_ok() {} // a stale answer from a timed-out wait
        if (&self.wake).write_all(b"d").is_ok() {
            let _ = self.done.recv_timeout(std::time::Duration::from_secs(1));
        }
    }
}

/// Input pump: stdin -> master. Detached; the process exit reaps it.
/// Doubles as the window-identity healer: input can only arrive while our
/// terminal is the focused X window, so whenever the user types we know
/// exactly which window hosts this agent. Re-capture (throttled) keeps the
/// meta's con_id/window_id correct through i3 restarts and launch-time
/// capture misses — this is what lets focus use exact handles only.
///
/// The writer is shared with the control thread and locked per chunk, so a
/// `send` body (written under one lock) is never split by a keystroke.
pub(super) fn spawn_input(writer: SharedWriter, files: Files, wake: Arc<super::monitor::Wake>) {
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 8192];
        let mut last_capture: Option<std::time::Instant> = None;
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    {
                        let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
                        if w.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = w.flush();
                    }
                    // A human answering a gate at the window.
                    wake.input();
                    let due = last_capture
                        .map_or(true, |t| t.elapsed().as_secs() >= 60);
                    if due && input_capture_eligible(&buf[..n]) {
                        last_capture = Some(std::time::Instant::now());
                        if let Some((con_id, xid)) = window::focused_terminal_ids() {
                            window::record_window(&files, con_id, xid);
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

/// Resize pump: on SIGWINCH, re-read the outer size and push it to the inner
/// PTY so the child's TUI reflows.
pub(super) fn spawn_resize(master: &Arc<Mutex<Box<dyn MasterPty + Send>>>) {
    let master = Arc::clone(master);
    if let Ok(mut signals) =
        signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
    {
        thread::spawn(move || {
            for _ in &mut signals {
                let size = super::terminal_size();
                if let Ok(m) = master.lock() {
                    let _ = m.resize(size);
                }
            }
        });
    }
}

/// Does this stdin chunk prove a *user* is interacting with our terminal?
/// Not every stdin byte is typed: with focus reporting enabled, the terminal
/// itself injects ESC[I / ESC[O (focus in/out). Capturing on those is actively
/// wrong — ESC[O arrives exactly when the user switches AWAY, so the "focused
/// window" query would record whichever window they switched to (seen live:
/// two agents in one dir ending up with the same recorded window, or an
/// agent recording the HUD after a preview flicker).
///
/// Rules: any focus-out in the chunk vetoes capture entirely (focus is in
/// flux); focus-in alone doesn't count as user input (preview flicker sends
/// it, then refocuses the HUD within milliseconds); anything else — real
/// keys, mouse reports — counts.
fn input_capture_eligible(chunk: &[u8]) -> bool {
    let mut has_user_bytes = false;
    let mut i = 0;
    while i < chunk.len() {
        if chunk[i] == 0x1b && i + 2 < chunk.len() && chunk[i + 1] == b'[' {
            match chunk[i + 2] {
                b'O' => return false,
                b'I' => {
                    i += 3;
                    continue;
                }
                _ => {}
            }
        }
        has_user_bytes = true;
        i += 1;
    }
    has_user_bytes
}

/// Shrink an oversized tee file to its last ~1 MB (all detection ever reads),
/// atomically: write the tail to a sibling temp file, rename it over the
/// original — readers see the old or the new file, never a torn one — and
/// return a fresh append handle. vt100 replay tolerates starting mid-stream:
/// the next full repaint converges the grid.
fn rotate_tee(files: &Files) -> Option<File> {
    let tail = crate::status::read_tail(&files.raw, 1024 * 1024)?;
    crate::fsutil::create_private(&files.raw_tmp, false).and_then(|mut f| f.write_all(&tail)).ok()?;
    fs::rename(&files.raw_tmp, &files.raw).ok()?;
    fs::OpenOptions::new().append(true).open(&files.raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_capture_eligibility_rules() {
        // Real typing: eligible.
        assert!(input_capture_eligible(b"hello"));
        assert!(input_capture_eligible(b"\x1b[A")); // arrow key
        // Terminal focus reports alone: not user input.
        assert!(!input_capture_eligible(b"\x1b[I"));
        // Focus-out vetoes the whole chunk — focus is in flux right now.
        assert!(!input_capture_eligible(b"\x1b[O"));
        assert!(!input_capture_eligible(b"x\x1b[O"));
        assert!(!input_capture_eligible(b"\x1b[Ozzz"));
        // Focus-in followed by real keys: the keys count.
        assert!(input_capture_eligible(b"\x1b[Ihello"));
        // Empty chunk: nothing to conclude.
        assert!(!input_capture_eligible(b""));
    }

    #[test]
    fn rotate_tee_keeps_the_tail() {
        let dir = std::env::temp_dir().join(format!("seagents-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = Files::in_dir(&dir, 1);
        // 3 MB of 'a' then a recognizable 16-byte tail marker.
        let mut data = vec![b'a'; 3 * 1024 * 1024];
        data.extend_from_slice(b"TAIL-MARKER-HERE");
        std::fs::write(&files.raw, &data).unwrap();

        let handle = rotate_tee(&files).expect("rotation should succeed");
        drop(handle);

        let rotated = std::fs::read(&files.raw).unwrap();
        assert_eq!(rotated.len(), 1024 * 1024);
        assert!(rotated.ends_with(b"TAIL-MARKER-HERE"));
        assert!(!files.raw_tmp.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
