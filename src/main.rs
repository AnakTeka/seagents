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
mod snapshot;
mod status;
mod vendor;

fn main() {
    let mut args = std::env::args();
    let _bin = args.next();
    match args.next().as_deref() {
        Some("run") => {
            // `--window <handle>` (before the agent's own argv) names the i3
            // window this agent will live in. `restore` passes it because a
            // window it spawned itself may not be focused yet when we look —
            // see `window_ids_for_handle`.
            let mut window: Option<String> = None;
            let mut cmd: Vec<String> = Vec::new();
            let mut rest = args.peekable();
            while let Some(arg) = rest.peek() {
                match arg.as_str() {
                    "--window" => {
                        rest.next();
                        match rest.next() {
                            Some(r) => window = Some(r),
                            None => {
                                eprintln!("seagents run: --window needs a value");
                                std::process::exit(2);
                            }
                        }
                    }
                    "--" => {
                        rest.next();
                        break;
                    }
                    _ => break,
                }
            }
            cmd.extend(rest);
            if cmd.is_empty() {
                eprintln!("usage: seagents run [--window <handle>] <cmd> [args...]");
                std::process::exit(2);
            }
            std::process::exit(run(cmd, window));
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
        Some("status") => {
            let mut json = false;
            let mut sort = status::Sort::Urgency;
            for a in args {
                match a.as_str() {
                    "--json" => json = true,
                    s if s.starts_with("--sort=") => match status::Sort::parse(&s[7..]) {
                        Some(parsed) => sort = parsed,
                        None => {
                            eprintln!("usage: seagents status [--json] [--sort=urgency|dir|recent]");
                            std::process::exit(2);
                        }
                    },
                    _ => {
                        eprintln!("usage: seagents status [--json] [--sort=urgency|dir|recent]");
                        std::process::exit(2);
                    }
                }
            }
            std::process::exit(if json {
                status::print_json(sort)
            } else {
                status::print(sort)
            });
        }
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
        // Reboot survival: the layout snapshot is written continuously by
        // every wrapper, so these two are only ever "now" and "put it back".
        Some("snapshot") => {
            let rest: Vec<String> = args.collect();
            std::process::exit(snapshot::snapshot_cmd(&rest));
        }
        Some("restore") => {
            let rest: Vec<String> = args.collect();
            std::process::exit(snapshot::restore_cmd(&rest));
        }
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
    eprintln!("  seagents status [--json] [--sort=urgency|dir|recent]");
    eprintln!("                                 show every tracked agent's state");
    eprintln!("  seagents focus <child_pid>     focus that agent's i3 window");
    eprintln!("  seagents snapshot [--refresh] [--print]");
    eprintln!("                                 capture the agent layout now (also automatic)");
    eprintln!("  seagents restore [--dry-run] [--stagger=N] [--workspace=NAME]");
    eprintln!("                                 rebuild that layout, resuming each session");
    eprintln!("  seagents update-manifests      fetch fresh detection rules from herdr.dev");
}

/// Explicit `update-manifests`: fetch, report per-agent results, reload.
fn update_manifests_now() -> i32 {
    match vendor::herdr_detect::manifest_update::check_and_update() {
        Ok(output) => {
            reload_updated_manifests(&output);
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

/// Refresh the in-process manifest cache for every agent whose rules moved.
/// Not just the ones this run committed: herdr and seagents share one remote
/// cache directory, so a manifest the other tool fetched leaves `updated`
/// empty while the loaded rules are stale.
fn reload_updated_manifests(output: &vendor::herdr_detect::manifest_update::ManifestUpdateOutput) {
    let activated = vendor::herdr_detect::manifest_update::agents_needing_cache_reload(output);
    if !activated.is_empty() {
        vendor::herdr_detect::manifest::reload_manifests_for_agents(&activated);
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
            // The manifest cache is behind an RwLock; the HUD's next poll
            // picks up the new rules.
            reload_updated_manifests(&output);
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
        "detection rules need a newer engine: re-sync src/vendor/herdr_detect (see VENDORED.md)"
            .to_string()
    } else {
        format!(
            "manifest update failing for {} agent(s), try `seagents update-manifests`",
            failing.len()
        )
    })
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

/// The session's current window handle, read fresh from its meta (the input
/// pump and focus paths heal it over time; launch-time copies go stale).
fn meta_con_id(meta_path: &std::path::Path) -> Option<u64> {
    let text = fs::read_to_string(meta_path).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&text).ok()?;
    meta["con_id"].as_u64()
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
    if con_id.is_some() && focus::focused_con_id() == con_id {
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
    window_id: Option<u64>,
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
        // handle for focus (titles can collide between idle agents). con_id is
        // the fast handle but dies on an i3 restart; window_id is the X11 id,
        // which survives one.
        "con_id": con_id,
        "window_id": window_id,
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
    fn input_capture_eligibility_rules() {
        use super::input_capture_eligible;
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

fn run(cmd: Vec<String>, window: Option<String>) -> i32 {
    // Capture the i3 window this agent lives in.
    //
    // Typed by hand, that's the focused window — the user just typed the
    // command into it. Launched by `restore`, focus is a race we would lose:
    // the terminal is still mapping when its shell reaches us, so the focused
    // window is whatever was focused *before*, and several restored agents
    // would all record the same wrong con_id. The handle is set by the
    // terminal at creation and is unique per agent, so it identifies the
    // window with no focus involved at all.
    // Strongest first:
    //   1. our own /proc ancestry ↔ the window's _NET_WM_PID — exact, and free
    //      of focus and of handles, but only on a one-process-per-window
    //      terminal (Terminator shares one process across all its windows, so
    //      this abstains there);
    //   2. the handle `restore` minted and passed as --window;
    //   3. the focused terminal, which is right when a human just typed the
    //      command into it and wrong for anything spawned programmatically.
    let (con_id, window_id) = focus::window_ids_by_pid()
        .map(|(con, xid)| (Some(con), xid))
        .or_else(|| window.as_ref().and_then(|h| focus::window_ids_for_handle(h)))
        .or_else(|| focus::focused_terminal_ids().map(|(con, xid)| (Some(con), xid)))
        .unwrap_or((None, None));

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
    write_meta(&meta_path, child.process_id(), &cwd, &cmd, &size, con_id, window_id);
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
    // Doubles as the window-identity healer: input can only arrive while our
    // terminal is the focused X window, so whenever the user types we know
    // exactly which window hosts this agent. Re-capture (throttled) keeps the
    // meta's con_id/window_id correct through i3 restarts and launch-time
    // capture misses — this is what lets focus use exact handles only.
    {
        let meta_path = meta_path.clone();
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
                            if let Some((con_id, xid)) = focus::focused_terminal_ids() {
                                focus::record_window(&meta_path, con_id, xid);
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
    }

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
    // (Guarded on a known pid: without one, geometry lookups would fail
    // forever and the monitor would misclassify at 80x24 while other
    // observers disagree — better no monitor than a flapping one.)
    if let Some(anchor_pid) = child.process_id().map(u64::from) {
        let raw_path = raw_path.clone();
        let state_path = raw_path.with_extension("state");
        let mon_meta_path = meta_path.clone();
        let cmd0 = std::path::Path::new(&cmd[0])
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&cmd[0])
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
                let seen = fs::metadata(&raw_path)
                    .ok()
                    .and_then(|m| m.modified().ok().map(|t| (m.len(), t)));
                if seen.is_none() || seen != last_seen {
                    last_seen = seen;
                    if let Some((state, acked)) =
                        status::track_session(&raw_path, &state_path, &cmd0, anchor_pid)
                    {
                        // Read the con_id fresh: the input pump and focus
                        // fallback heal it in the meta over time, and a stale
                        // copy here would break focused-window suppression.
                        let con_id = meta_con_id(&mon_meta_path);
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
                            && focus::focused_con_id() == con_id
                        {
                            status::ack_session_path(&state_path);
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
