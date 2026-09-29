//! `seagents run <cmd...>` — the transparent pty-teeing launcher.
//!
//! It spawns <cmd> on an inner PTY, relays stdin/output byte-for-byte between
//! your real terminal and the child, and tees a copy of the output stream to a
//! file so observers can scrape it for agent status. Nothing here parses or
//! rewrites the stream: transparency comes from relaying, not interpreting.
//!
//! This file is the setup and wiring; the threads live in [`pumps`] (output
//! tee, input, resize), [`monitor`] (state detection, notifications, the
//! snapshot offer) and [`control`] (the socket `send`/`stop` drive the agent
//! through, writing with [`inject`]'s framing), and the in-PTY job-control
//! anchor in [`lead`].

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::launch::handle_for;
use crate::registry::{self, Files};
use crate::window;

mod control;
mod inject;
mod lead;
mod monitor;
mod pumps;

pub(crate) use lead::lead;

/// `seagents run [--tag T] [--parent P] [--seed-file F] <cmd…>`. Exit 2 —
/// before anything is spawned — when the tag cannot be had (malformed, or
/// held by a live agent), the parent is not a live agent, or the seed cannot
/// be delivered.
pub(crate) fn run(
    cmd: Vec<String>,
    tag: Option<String>,
    parent: Option<String>,
    seed_file: Option<PathBuf>,
) -> i32 {
    if let Some(t) = tag.as_deref().filter(|t| !registry::valid_tag(t)) {
        eprintln!("seagents: invalid tag {t:?}: want [a-z0-9][a-z0-9-]{{0,31}}, not all digits");
        return 2;
    }
    // Read and deleted first thing, whatever happens next: the prompt must
    // not outlive `run`'s startup on disk.
    let seed = match seed_file.as_deref().map(take_seed).transpose() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("seagents: {e}");
            return 2;
        }
    };
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("seagents"));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let argv = match child_argv(&cmd, &exe, &cwd, seed.as_deref()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("seagents: {e}");
            return 2;
        }
    };
    // A requested tag is also the window's handle: whoever launched us with
    // one (restore) put `handle_for(tag)` on the terminal it opened.
    let (con_id, window_id) = identify_window(tag.as_deref().map(handle_for).as_deref());

    // Held from here until the `.meta` exists, so no other `run` can take the
    // same tag in between. Only the pty and the fork happen under it.
    let claim = match registry::claim_tag(
        tag.as_deref(),
        parent.as_deref(),
        &cwd,
        &crate::snapshot::captured_tags(),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("seagents: {e}");
            return 2;
        }
    };

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
    let mut builder = CommandBuilder::new(&exe);
    builder.arg("__lead");
    for arg in argv {
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
    let writer: inject::SharedWriter = match pair.master.take_writer() {
        Ok(w) => Arc::new(Mutex::new(w)),
        Err(e) => {
            eprintln!("seagents: take writer failed: {e}");
            return 1;
        }
    };
    // Always there on unix; the pump polls it alongside its blocking reads.
    let Some(master_fd) = pair.master.as_raw_fd() else {
        eprintln!("seagents: the pty master has no file descriptor");
        return 1;
    };
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    // Open the tee target and write the identity sidecar. Both best-effort.
    let files = Files::of(u64::from(std::process::id()));
    let tee = File::create(&files.raw).ok();
    write_meta(claim, &files, child.process_id(), &cwd, &cmd, &size, con_id, window_id);
    if std::env::var("SEAGENTS_DEBUG").is_ok() {
        eprintln!("seagents: teeing to {}", files.raw.display());
    }

    // Put the outer terminal into raw mode so keystrokes and control bytes flow
    // through untouched (the inner PTY's line discipline does the cooking).
    let raw = crossterm::terminal::enable_raw_mode().is_ok();

    let modes = Arc::new(inject::PtyModes::default());
    let (reader_handle, drain) =
        pumps::spawn_output(reader, master_fd, tee, files.clone(), Arc::clone(&modes));
    let wake = Arc::new(monitor::Wake::default());
    pumps::spawn_input(Arc::clone(&writer), files.clone(), Arc::clone(&wake));
    pumps::spawn_resize(&master);

    // (Monitor guarded on a known pid: without one, geometry lookups would
    // fail forever and the monitor would misclassify at 80x24 while other
    // observers disagree — better no monitor than a flapping one. The
    // control socket needs it too, to signal the agent on `stop`.)
    let control = child.process_id().map(u64::from).and_then(|anchor_pid| {
        monitor::spawn(files.clone(), anchor_pid, &cmd[0], &cwd, Arc::clone(&wake));
        // After the `.meta`: `reap` sweeps a socket no live session owns.
        control::spawn(control::Wiring {
            files: files.clone(),
            anchor_pid,
            master_fd,
            writer,
            modes,
            wake,
            argv0: cmd[0].clone(),
        })
    });

    let code = wait_forwarding_job_control(&mut child, &master, drain.as_ref(), raw);
    if let Some(control) = control {
        control.finish();
    }

    // Let the output pump drain anything buffered before we tear down.
    let _ = reader_handle.join();
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    code
}

/// Read a seed prompt file and delete it. The file is written 0600 into the
/// runtime dir by `spawn`, and its one reader is this.
fn take_seed(path: &Path) -> Result<String, String> {
    let text = std::fs::read(path).map_err(|e| format!("seed file {}: {e}", path.display()));
    let _ = std::fs::remove_file(path);
    String::from_utf8(text?).map_err(|_| format!("seed file {}: not UTF-8", path.display()))
}

/// What the agent is actually started with: the user's `cmd`, plus our hooks
/// when its adapter takes them, plus the seed prompt per the adapter's
/// `seed_prompt`. Only the child sees this; `.meta` records `cmd` itself,
/// because `restore` relaunches from the recorded argv — it would otherwise
/// stack a second `--settings`, and re-submit the seed into the resumed
/// conversation, on every restore (and the prompt stays out of `.meta`).
fn child_argv(cmd: &[String], exe: &Path, cwd: &Path, seed: Option<&str>) -> Result<Vec<String>, String> {
    let adapter = match crate::adapter::for_command(&cmd[0]) {
        Ok(a) => a,
        // The agent still starts — a broken override must not take every
        // `claude` launch down with it — just without anything the adapter
        // would have added. A seed has nowhere to go without it, though.
        Err(e) if seed.is_some() => return Err(format!("agent adapter: {e}; cannot seed the prompt")),
        Err(e) => {
            eprintln!("seagents: agent adapter: {e}; launching without hooks");
            return Ok(cmd.to_vec());
        }
    };
    let (mut argv, warning) = crate::hook::inject_settings(cmd, &adapter, exe, cwd);
    if let Some(w) = warning {
        eprintln!("seagents: {w}");
    }
    if let Some(prompt) = seed {
        argv.extend(adapter.launch.seed_args(prompt)?);
    }
    Ok(argv)
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
///   3. the handle `restore` put on the terminal, `handle_for(--tag)`;
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
/// prompt back. Waiting with WUNTRACED mirrors the stop: let the output pump
/// relay what the agent wrote before stopping, restore the outer terminal,
/// suspend ourselves (now the shell shows the job as suspended); when the
/// user resumes us, re-enter raw mode, push the possibly-changed size, and
/// continue the child.
fn wait_forwarding_job_control(
    child: &mut Box<dyn Child + Send + Sync>,
    master: &Arc<Mutex<Box<dyn MasterPty + Send>>>,
    drain: Option<&pumps::OutputDrain>,
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
                    // Stopping ourselves freezes the output pump with us, so
                    // first let it relay what the agent printed on its way
                    // down (Claude's "run `fg`" line). Nothing new can come:
                    // the agent and the anchor are both stopped by now.
                    if let Some(drain) = drain {
                        drain.wait();
                    }
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
/// i3 window, under the tag `claim` holds (and releasing it). cwd here is the
/// launch cwd; the daemon can also read the live cwd from /proc/<child_pid>/cwd.
#[allow(clippy::too_many_arguments)]
fn write_meta(
    claim: registry::TagClaim,
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
    claim.create_meta(files, meta);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_child_gets_the_hooks_and_the_recorded_argv_stays_the_users() {
        let _env = crate::vendor::herdr_detect::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cmd: Vec<String> = ["claude", "--model", "haiku", "--", "say hi"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let before = cmd.clone();
        let child = child_argv(&cmd, Path::new("/x/seagents"), Path::new("/"), None).unwrap();
        assert_eq!(child[1], "--settings");
        assert!(child[2].contains("'/x/seagents' __hook"));
        assert_eq!(&child[3..], &cmd[1..]);
        // `cmd` is what `write_meta` records; building the child's argv must
        // not have touched it.
        assert_eq!(cmd, before);
        // A tool with no hook support is started exactly as typed.
        let bash = vec!["bash".to_string()];
        assert_eq!(child_argv(&bash, Path::new("/x/seagents"), Path::new("/"), None).unwrap(), bash);
    }

    #[test]
    fn a_seed_goes_after_the_separator_in_the_childs_argv_only() {
        let _env = crate::vendor::herdr_detect::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cmd: Vec<String> = ["claude", "--permission-mode", "plan", "--model", "haiku"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let child = child_argv(&cmd, Path::new("/x/seagents"), Path::new("/"), Some("-v: do it")).unwrap();
        assert_eq!((child[1].as_str(), child.len()), ("--settings", 9));
        assert_eq!(&child[3..], ["--permission-mode", "plan", "--model", "haiku", "--", "-v: do it"]);
        // A command with no way to take one is refused, not launched unseeded.
        let bash = vec!["bash".to_string()];
        assert!(child_argv(&bash, Path::new("/x"), Path::new("/"), Some("hi")).is_err());
    }

    #[test]
    fn a_seed_file_is_gone_once_read() {
        let dir = std::env::temp_dir().join(format!("seagents-seed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("seed-x");
        std::fs::write(&f, "hello\nworld").unwrap();
        assert_eq!(take_seed(&f).unwrap(), "hello\nworld");
        assert!(!f.exists());
        assert!(take_seed(&f).unwrap_err().contains("seed-x"));
        std::fs::write(&f, [0xff, 0xfe]).unwrap();
        assert!(take_seed(&f).is_err());
        assert!(!f.exists(), "deleted even when unusable");
        std::fs::remove_dir_all(&dir).ok();
    }
}
