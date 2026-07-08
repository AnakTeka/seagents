//! seagents — observe coding agents running in your own terminal windows.
//!
//! Milestone 1: `seagents run <cmd...>` — a transparent pty-teeing launcher.
//! It spawns <cmd> on an inner PTY, relays stdin/output byte-for-byte between
//! your real terminal and the child, and tees a copy of the output stream to a
//! file so a future daemon can scrape it for agent status. Nothing here parses
//! or rewrites the stream: transparency comes from relaying, not interpreting.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

mod detect;
mod focus;
mod hud;
mod status;
mod vendor;

fn main() {
    let mut args = std::env::args();
    let _bin = args.next();
    match args.next().as_deref() {
        Some("run") => {
            let cmd: Vec<String> = args.collect();
            if cmd.is_empty() {
                eprintln!("usage: seagents run <cmd> [args...]");
                std::process::exit(2);
            }
            std::process::exit(run(cmd));
        }
        // Hidden: the in-PTY job-control anchor `run` wraps around the agent.
        // The PTY session leader must not BE the agent: a lone process group in
        // its own session is "orphaned" and the kernel silently discards the
        // SIGTSTP it raises on Ctrl+Z. The anchor forks the agent into a second
        // process group (de-orphaning it), then relays stops/exits upward.
        Some("__lead") => {
            let cmd: Vec<String> = args.collect();
            if cmd.is_empty() {
                std::process::exit(2);
            }
            std::process::exit(lead(cmd));
        }
        Some("status") => match args.next().as_deref() {
            Some("--json") => std::process::exit(status::print_json()),
            None => std::process::exit(status::print()),
            Some(_) => {
                eprintln!("usage: seagents status [--json]");
                std::process::exit(2);
            }
        },
        Some("focus") => {
            let pid = args.next().and_then(|s| s.parse::<u64>().ok());
            match pid {
                Some(p) => std::process::exit(focus::focus(p)),
                None => {
                    eprintln!("usage: seagents focus <child_pid>");
                    std::process::exit(2);
                }
            }
        }
        // Dev tool: render a .raw file and show the detection engine's verdict
        // (the equivalent of herdr's `agent explain`) for tuning against real
        // captures. Add --json for the full evaluated-rule breakdown.
        Some("debug-screen") => {
            let mut file = String::new();
            let mut dims: Vec<u16> = Vec::new();
            let mut json = false;
            let mut agent_name = "claude".to_string();
            for a in args {
                match a.as_str() {
                    "--json" => json = true,
                    s if s.starts_with("--agent=") => agent_name = s[8..].to_string(),
                    s => match s.parse::<u16>() {
                        Ok(n) => dims.push(n),
                        Err(_) => file = s.to_string(),
                    },
                }
            }
            let cols = dims.first().copied().unwrap_or(80);
            let rows = dims.get(1).copied().unwrap_or(24);
            let data = std::fs::read(&file).unwrap_or_default();
            let screen = detect::render(&data, rows, cols);
            let title = status::current_title(&data).unwrap_or_default();
            println!("{screen}");
            let explain = match vendor::herdr_detect::parse_agent_label(&agent_name) {
                Some(agent) => vendor::herdr_detect::manifest::explain_with_input(
                    agent,
                    vendor::herdr_detect::manifest::DetectionInput {
                        screen: &screen,
                        osc_title: &title,
                        osc_progress: "",
                    },
                ),
                None => {
                    eprintln!("unknown agent {agent_name:?}");
                    std::process::exit(2);
                }
            };
            println!("\n---");
            println!("osc_title = {title:?}");
            println!(
                "state = {} (manifest {} v{})",
                vendor::herdr_detect::manifest::agent_state_label(explain.state),
                explain
                    .source
                    .as_ref()
                    .map(|s| s.kind())
                    .unwrap_or("none"),
                explain.manifest_version.as_deref().unwrap_or("?"),
            );
            match &explain.matched_rule {
                Some(rule) => println!(
                    "matched_rule = {} (priority {}, region {})",
                    rule.id, rule.priority, rule.region
                ),
                None => println!(
                    "matched_rule = none ({})",
                    explain.fallback_reason.as_deref().unwrap_or("no fallback")
                ),
            }
            if let Some(warning) = &explain.warning {
                println!("warning = {warning}");
            }
            if json {
                let value = vendor::herdr_detect::manifest::explain_to_json_value(&explain);
                println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
            }
            std::process::exit(0);
        }
        // Refresh detection manifests from herdr.dev right now (the HUD also
        // does this in the background at most once a day).
        Some("update-manifests") => std::process::exit(update_manifests_now()),
        Some("hud") | None => std::process::exit(hud::run()),
        Some("help") | Some("-h") | Some("--help") => {
            usage();
            std::process::exit(0);
        }
        _ => {
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    eprintln!("seagents {}", env!("CARGO_PKG_VERSION"));
    eprintln!("usage:");
    eprintln!("  seagents [hud]                 the floating agent HUD (default)");
    eprintln!("  seagents run <cmd> [args...]   launch an agent, teeing its output");
    eprintln!("  seagents status [--json]       show every tracked agent's state");
    eprintln!("  seagents focus <child_pid>     focus that agent's i3 window");
    eprintln!("  seagents update-manifests      fetch fresh detection rules from herdr.dev");
}

/// Explicit `update-manifests`: fetch, report per-agent results, reload.
fn update_manifests_now() -> i32 {
    match vendor::herdr_detect::manifest_update::check_and_update() {
        Ok(output) => {
            if !output.updated.is_empty() {
                vendor::herdr_detect::manifest::reload_manifests();
            }
            for (agent, remote) in &output.status.agents {
                let version = remote.cached_version.as_deref().unwrap_or("-");
                match &remote.last_error {
                    Some(err) => println!("{agent:<10} {:<8} {version}  {err}", remote.last_result),
                    None => println!("{agent:<10} {:<8} {version}", remote.last_result),
                }
            }
            0
        }
        Err(err) => {
            eprintln!("manifest update failed: {err}");
            1
        }
    }
}

/// Background refresh for the HUD: at most once per day, silently.
/// `SEAGENTS_NO_UPDATE=1` disables the automatic phone-home entirely
/// (`seagents update-manifests` still works on demand).
pub(crate) fn spawn_manifest_update() {
    if std::env::var_os("SEAGENTS_NO_UPDATE").is_some() {
        return;
    }
    thread::spawn(|| {
        // Throttle on our own attempt stamp, not just the updater's recorded
        // last check: a failed catalog fetch never advances last_check_unix,
        // which would otherwise mean one curl attempt per HUD launch while
        // offline.
        let stamp = vendor::herdr_detect::config::state_dir()
            .join("agent-detection/last-attempt");
        let fresh_attempt = fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|e| e.as_secs() < 24 * 3600);
        if fresh_attempt {
            return;
        }
        if let Some(parent) = stamp.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&stamp, b"");
        if let Ok(output) = vendor::herdr_detect::manifest_update::check_and_update() {
            if !output.updated.is_empty() {
                // The manifest cache is behind an RwLock; the HUD's next poll
                // picks up the new rules.
                vendor::herdr_detect::manifest::reload_manifests();
            }
        }
    });
}

/// One line of manifest-update health for the HUD footer / status output —
/// the vendored engine only improves while updates apply, and a persistent
/// failure (worst case: manifests requiring a newer engine after an upstream
/// bump) would otherwise rot silently behind SEAGENTS_DEBUG.
pub(crate) fn manifest_health_warning() -> Option<String> {
    let status = vendor::herdr_detect::manifest_update::load_status();
    let failing: Vec<&String> = status
        .agents
        .iter()
        .filter(|(_, s)| s.last_result == "failed")
        .map(|(agent, _)| agent)
        .collect();
    if failing.is_empty() {
        return None;
    }
    let engine_drift = status.agents.values().any(|s| {
        s.last_error
            .as_deref()
            .is_some_and(|e| e.contains("requires engine"))
    });
    Some(if engine_drift {
        "detection rules need a newer engine — re-sync src/vendor/herdr_detect (see VENDORED.md)"
            .to_string()
    } else {
        format!(
            "manifest update failing for {} agent(s) — try `seagents update-manifests`",
            failing.len()
        )
    })
}

/// The job-control anchor (`seagents __lead <cmd...>`), running as the inner
/// PTY's session leader. It forks the agent into its own foreground process
/// group — giving the agent a same-session parent so its Ctrl+Z SIGTSTP is
/// *not* discarded under the orphaned-process-group rule — then mirrors the
/// agent's lifecycle: agent stops → anchor stops itself (SIGSTOP, which cannot
/// be discarded, so the wrapper's WUNTRACED wait sees it); anchor is resumed →
/// it resumes the agent's group and nudges a repaint with SIGWINCH.
fn lead(cmd: Vec<String>) -> i32 {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    unsafe {
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            // Becoming the foreground group from a background one raises
            // SIGTTOU; ignore it around the handoff.
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::tcsetpgrp(0, libc::getpid());
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            Ok(())
        });
    }
    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("seagents: failed to spawn {}: {e}", cmd[0]);
            return 127;
        }
    };
    let pid = child.id() as libc::pid_t;
    unsafe {
        // Also from the parent side, so the group exists before we wait on it
        // regardless of who wins the fork/exec race.
        libc::setpgid(pid, pid);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::tcsetpgrp(0, pid);
    }

    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return 1;
        }
        if libc::WIFSTOPPED(status) {
            unsafe {
                libc::raise(libc::SIGSTOP);
                // — resumed by the wrapper (the user ran `fg`) —
                libc::kill(-pid, libc::SIGCONT);
                libc::kill(-pid, libc::SIGWINCH);
            }
            continue;
        }
        if libc::WIFEXITED(status) {
            return libc::WEXITSTATUS(status);
        }
        if libc::WIFSIGNALED(status) {
            return 128 + libc::WTERMSIG(status);
        }
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

/// Directory holding this machine's seagents session files.
fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache/seagents"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/seagents"));
    let _ = fs::create_dir_all(&base);
    base
}

/// A session is keyed by the wrapper's pid; `.raw` holds the teed output and
/// `.meta` holds the identity the daemon needs (cwd, argv, child pid).
fn session_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let base = cache_dir();
    let id = std::process::id();
    (
        base.join(format!("{id}.raw")),
        base.join(format!("{id}.meta")),
    )
}

/// Record who this agent is so the daemon can label it and locate its process /
/// i3 window. cwd here is the launch cwd; the daemon can also read the live cwd
/// from /proc/<child_pid>/cwd.
fn write_meta(
    path: &std::path::Path,
    child_pid: Option<u32>,
    cwd: &std::path::Path,
    argv: &[String],
    size: &PtySize,
    con_id: Option<u64>,
) {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let meta = serde_json::json!({
        "wrapper_pid": std::process::id(),
        "child_pid": child_pid,
        "cwd": cwd.to_string_lossy(),
        "argv": argv,
        "started": started,
        // Geometry so the daemon can re-render the raw stream at the right size.
        "cols": size.cols,
        "rows": size.rows,
        // The i3 window this agent runs in, captured at launch — the reliable
        // handle for focus (titles can collide between idle agents).
        "con_id": con_id,
    });
    if let Ok(s) = serde_json::to_string_pretty(&meta) {
        let _ = fs::write(path, s);
    }
}

/// Shrink an oversized tee file to its last ~1 MB (all detection ever reads),
/// atomically: write the tail to a sibling temp file, rename it over the
/// original — readers see the old or the new file, never a torn one — and
/// return a fresh append handle. vt100 replay tolerates starting mid-stream:
/// the next full repaint converges the grid.
fn rotate_tee(path: &std::path::Path) -> Option<File> {
    let tail = status::read_tail(path, 1024 * 1024)?;
    let tmp = path.with_extension("raw.tmp");
    fs::write(&tmp, &tail).ok()?;
    fs::rename(&tmp, path).ok()?;
    fs::OpenOptions::new().append(true).open(path).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn rotate_tee_keeps_the_tail() {
        let dir = std::env::temp_dir().join(format!("seagents-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.raw");
        // 3 MB of 'a' then a recognizable 16-byte tail marker.
        let mut data = vec![b'a'; 3 * 1024 * 1024];
        data.extend_from_slice(b"TAIL-MARKER-HERE");
        std::fs::write(&path, &data).unwrap();

        let handle = super::rotate_tee(&path).expect("rotation should succeed");
        drop(handle);

        let rotated = std::fs::read(&path).unwrap();
        assert_eq!(rotated.len(), 1024 * 1024);
        assert!(rotated.ends_with(b"TAIL-MARKER-HERE"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

fn run(cmd: Vec<String>) -> i32 {
    // Capture the launching terminal's i3 window up front, while it's still the
    // focused window (the user just typed the command into it).
    let con_id = focus::focused_terminal_con_id();

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
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("seagents"));
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
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("seagents: clone reader failed: {e}");
            return 1;
        }
    };
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("seagents: take writer failed: {e}");
            return 1;
        }
    };
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    // Open the tee target and write the identity sidecar. Both best-effort.
    let (raw_path, meta_path) = session_paths();
    let tee = File::create(&raw_path).ok();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    write_meta(&meta_path, child.process_id(), &cwd, &cmd, &size, con_id);
    if std::env::var("SEAGENTS_DEBUG").is_ok() {
        eprintln!("seagents: teeing to {}", raw_path.display());
    }

    // Put the outer terminal into raw mode so keystrokes and control bytes flow
    // through untouched (the inner PTY's line discipline does the cooking).
    let raw = crossterm::terminal::enable_raw_mode().is_ok();

    // Output pump: master -> stdout (+ tee). This owns the "T-junction".
    let tee_path = raw_path.clone();
    let reader_handle = thread::spawn(move || {
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
                                if let Some(rotated) = rotate_tee(&tee_path) {
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
    });

    // Input pump: stdin -> master. Detached; the process exit reaps it.
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // Resize pump: on SIGWINCH, re-read the outer size and push it to the inner
    // PTY so the child's TUI reflows.
    {
        let master = Arc::clone(&master);
        if let Ok(mut signals) =
            signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
        {
            thread::spawn(move || {
                for _ in &mut signals {
                    let size = terminal_size();
                    if let Ok(m) = master.lock() {
                        let _ = m.resize(size);
                    }
                }
            });
        }
    }

    // Self-monitor: re-detect our own agent's state every 2s and stamp the
    // .state sidecar on transitions. The wrapper is the only process
    // guaranteed to live exactly as long as the agent, so this keeps
    // "idle for / waiting for" truthful even when no HUD or status poll is
    // watching — opening the HUD hours later shows the real durations.
    {
        let raw_path = raw_path.clone();
        let state_path = raw_path.with_extension("state");
        let cmd0 = std::path::Path::new(&cmd[0])
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&cmd[0])
            .to_string();
        let anchor_pid = child.process_id().map(u64::from).unwrap_or(0);
        thread::spawn(move || loop {
            status::track_session(&raw_path, &state_path, &cmd0, anchor_pid);
            thread::sleep(std::time::Duration::from_secs(2));
        });
    }

    // Block until the child exits, forwarding job control along the way.
    // Claude (and other TUIs) implement Ctrl+Z by stopping themselves on the
    // inner PTY — a plain wait() never sees that, so the wrapper would sit
    // running while the child is frozen and the user's shell never gets its
    // prompt back. Waiting with WUNTRACED mirrors the stop: restore the outer
    // terminal, suspend ourselves (now the shell shows the job as suspended);
    // when the user resumes us, re-enter raw mode, push the possibly-changed
    // size, and continue the child.
    let code = match child.process_id() {
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
    };

    // Let the output pump drain anything buffered before we tear down.
    let _ = reader_handle.join();
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    code
}
