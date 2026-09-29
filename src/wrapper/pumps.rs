//! The byte pumps between the user's terminal and the inner PTY: output (with
//! the tee), input (with window-identity healing), and resize.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use portable_pty::MasterPty;

use crate::registry::Files;
use crate::window;

/// Output pump: master -> stdout (+ tee). This owns the "T-junction".
pub(super) fn spawn_output(
    mut reader: Box<dyn Read + Send>,
    tee: Option<File>,
    files: Files,
) -> JoinHandle<()> {
    thread::spawn(move || {
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
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = &buf[..n];
                    if out.write_all(data).is_err() {
                        break;
                    }
                    let _ = out.flush();
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
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

/// Input pump: stdin -> master. Detached; the process exit reaps it.
/// Doubles as the window-identity healer: input can only arrive while our
/// terminal is the focused X window, so whenever the user types we know
/// exactly which window hosts this agent. Re-capture (throttled) keeps the
/// meta's con_id/window_id correct through i3 restarts and launch-time
/// capture misses — this is what lets focus use exact handles only.
pub(super) fn spawn_input(mut writer: Box<dyn Write + Send>, files: Files) {
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 8192];
        let mut last_capture: Option<std::time::Instant> = None;
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = writer.flush();
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
    fs::write(&files.raw_tmp, &tail).ok()?;
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
