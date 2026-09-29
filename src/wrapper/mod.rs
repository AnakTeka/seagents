//! `seagents run <cmd...>` — the transparent pty-teeing launcher.
//!
//! It spawns <cmd> on an inner PTY, relays stdin/output byte-for-byte between
//! your real terminal and the child, and tees a copy of the output stream to a
//! file so observers can scrape it for agent status. Nothing here parses or
//! rewrites the stream: transparency comes from relaying, not interpreting.
//!
//! This file is the setup and wiring; the threads live in [`pumps`] (output
//! tee, input, resize) and [`monitor`] (state detection, notifications, the
//! snapshot offer), and the in-PTY job-control anchor in [`lead`].

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::registry::{self, Files};
use crate::window;

mod lead;
mod monitor;
mod pumps;

pub(crate) use lead::lead;

pub(crate) fn run(cmd: Vec<String>, window: Option<String>) -> i32 {
    let (con_id, window_id) = identify_window(window.as_deref());

    let size = terminal_size();

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(size) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("seagents: openpty failed: {e}");
            return 1;
        }
    };

    // Build the child command, inheriting cwd and environment. The agent runs
    // under the `__lead` anchor (see `lead()`): spawned directly it would be a
    // lone, orphaned process group whose Ctrl+Z SIGTSTP the kernel discards.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("seagents"));
    let mut builder = CommandBuilder::new(exe);
    builder.arg("__lead");
    for arg in &cmd {
        builder.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }

    let mut child = match pair.slave.spawn_command(builder) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("seagents: failed to spawn {}: {e}", cmd[0]);
            return 1;
        }
    };
    // Drop the slave so the master sees EOF once the child exits.
    drop(pair.slave);

    // Split the master into a reader (child output) and a writer (child input)
    // before sharing it for resize handling.
    let reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("seagents: clone reader failed: {e}");
            return 1;
        }
    };
    let writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("seagents: take writer failed: {e}");
            return 1;
        }
    };
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    // Open the tee target and write the identity sidecar. Both best-effort.
    let files = Files::of(u64::from(std::process::id()));
    let tee = File::create(&files.raw).ok();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    write_meta(&files, child.process_id(), &cwd, &cmd, &size, con_id, window_id);
    if std::env::var("SEAGENTS_DEBUG").is_ok() {
        eprintln!("seagents: teeing to {}", files.raw.display());
    }

    // Put the outer terminal into raw mode so keystrokes and control bytes flow
    // through untouched (the inner PTY's line discipline does the cooking).
    let raw = crossterm::terminal::enable_raw_mode().is_ok();

    let reader_handle = pumps::spawn_output(reader, tee, files.clone());
    pumps::spawn_input(writer, files.clone());
    pumps::spawn_resize(&master);

    // (Monitor guarded on a known pid: without one, geometry lookups would
    // fail forever and the monitor would misclassify at 80x24 while other
    // observers disagree — better no monitor than a flapping one.)
    if let Some(anchor_pid) = child.process_id().map(u64::from) {
        monitor::spawn(files.clone(), anchor_pid, &cmd[0], &cwd);
    }

    let code = wait_forwarding_job_control(&mut child, &master, raw);

    // Let the output pump drain anything buffered before we tear down.
    let _ = reader_handle.join();
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    code
}

/// Capture the i3 window this agent lives in, as (con_id, X window id).
///
/// Typed by hand, that's the focused window — the user just typed the
/// command into it. Launched by `restore`, focus is a race we would lose:
/// the terminal is still mapping when its shell reaches us, so the focused
/// window is whatever was focused *before*, and several restored agents
/// would all record the same wrong con_id. The handle is set by the
/// terminal at creation and is unique per agent, so it identifies the
/// window with no focus involved at all.
/// Strongest first:
///   1. the window id the terminal put in our environment — exact, instant,
///      and correct even when one process owns many windows;
///   2. our own /proc ancestry ↔ the window's _NET_WM_PID — also exact, but
///      only on a one-process-per-window terminal (Terminator shares one
///      process across all its windows, so this abstains there);
///   3. the handle `restore` minted and passed as --window;
///   4. the focused terminal, which is right when a human just typed the
///      command into it and wrong for anything spawned programmatically.
fn identify_window(handle: Option<&str>) -> (Option<u64>, Option<u64>) {
    window::window_ids_from_env()
        .or_else(window::window_ids_by_pid)
        .map(|(con, xid)| (Some(con), xid))
        .or_else(|| handle.and_then(window::window_ids_for_handle))
        .or_else(|| window::focused_terminal_ids().map(|(con, xid)| (Some(con), xid)))
        .unwrap_or((None, None))
}

/// Block until the child exits, forwarding job control along the way.
/// Claude (and other TUIs) implement Ctrl+Z by stopping themselves on the
/// inner PTY — a plain wait() never sees that, so the wrapper would sit
/// running while the child is frozen and the user's shell never gets its
/// prompt back. Waiting with WUNTRACED mirrors the stop: restore the outer
/// terminal, suspend ourselves (now the shell shows the job as suspended);
/// when the user resumes us, re-enter raw mode, push the possibly-changed
/// size, and continue the child.
fn wait_forwarding_job_control(
    child: &mut Box<dyn Child + Send + Sync>,
    master: &Arc<Mutex<Box<dyn MasterPty + Send>>>,
    raw: bool,
) -> i32 {
    match child.process_id() {
        Some(pid) => {
            let pid = pid as libc::pid_t;
            loop {
                let mut status: libc::c_int = 0;
                let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
                if r < 0 {
                    if std::io::Error::last_os_error().kind()
                        == std::io::ErrorKind::Interrupted
                    {
                        continue;
                    }
                    break 1; // ECHILD: nothing left to wait for
                }
                if libc::WIFSTOPPED(status) {
                    if raw {
                        let _ = crossterm::terminal::disable_raw_mode();
                    }
                    unsafe { libc::raise(libc::SIGTSTP) };
                    // — stopped here until the user resumes us —
                    // `bg` resumes us in a *background* pgrp: re-rawing from
                    // there raises SIGTTOU (stopping us again) and a bg
                    // fullscreen TUI is useless anyway. Treat bg as a deferred
                    // fg: hold the agent stopped and wait until we own the
                    // terminal again before unwinding.
                    loop {
                        let fg = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
                        if fg < 0 || fg == unsafe { libc::getpgrp() } {
                            break; // foreground again (or no tty to wait on)
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    if raw {
                        let _ = crossterm::terminal::enable_raw_mode();
                    }
                    let size = terminal_size();
                    if let Ok(m) = master.lock() {
                        let _ = m.resize(size);
                    }
                    // Continue the anchor; it in turn continues the agent's
                    // process group (see `lead()`).
                    unsafe {
                        if libc::kill(-pid, libc::SIGCONT) != 0 {
                            let _ = libc::kill(pid, libc::SIGCONT);
                        }
                    }
                    continue;
                }
                if libc::WIFEXITED(status) {
                    break libc::WEXITSTATUS(status);
                }
                if libc::WIFSIGNALED(status) {
                    break 128 + libc::WTERMSIG(status);
                }
            }
        }
        // No pid (shouldn't happen post-spawn): fall back to a plain wait.
        None => match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => 1,
        },
    }
}

/// Query the *outer* terminal's real size, including pixel dimensions, so the
/// inner PTY matches it exactly (apps like image/graphics protocols care).
fn terminal_size() -> PtySize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        // Best-effort: on failure we fall back to a sane 80x24.
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col != 0 {
            PtySize {
                rows: ws.ws_row,
                cols: ws.ws_col,
                pixel_width: ws.ws_xpixel,
                pixel_height: ws.ws_ypixel,
            }
        } else {
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }
        }
    }
}

/// Record who this agent is so the daemon can label it and locate its process /
/// i3 window. cwd here is the launch cwd; the daemon can also read the live cwd
/// from /proc/<child_pid>/cwd.
fn write_meta(
    files: &Files,
    child_pid: Option<u32>,
    cwd: &Path,
    argv: &[String],
    size: &PtySize,
    con_id: Option<u64>,
    window_id: Option<u64>,
) {
    let meta = registry::Meta {
        wrapper_pid: u64::from(std::process::id()),
        child_pid: child_pid.map(u64::from),
        cwd: cwd.to_string_lossy().into_owned(),
        argv: argv.to_vec(),
        started: crate::clock::unix_now(),
        // Geometry so the daemon can re-render the raw stream at the right size.
        cols: Some(size.cols),
        rows: Some(size.rows),
        // The i3 window this agent runs in, captured at launch.
        con_id,
        window_id,
        ..registry::Meta::default()
    };
    registry::create_meta(files, &meta);
}
