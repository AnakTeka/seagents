//! `seagents run <cmd…>` — start an agent in herdr and show it in this
//! window.
//!
//! The agent runs in a pane of a herdr server: herdr owns its terminal,
//! reads its state, and lets any herdr client — this window, or
//! `herdr --remote ssh <this machine>` from another — show and drive it. This
//! window is one such view: `run` stays in it as the parent of a `herdr
//! terminal attach`, so the window shows the agent alone, full-size, and
//! **the window and the agent live and die together**: closing the window
//! closes the agent's pane, and the agent exiting closes the window.
//!
//! In order: find or make the agent's place in herdr (one workspace per
//! launch directory, one tab per agent, labelled with its tag), register the
//! session (`registry`), `herdr agent start` it with our hooks, record which
//! i3 window it is shown in, start the monitor thread, then attach — and
//! re-attach after a detach (`Ctrl+B q` would otherwise leave an empty
//! window over a running agent).
//!
//! Inside a herdr pane (`HERDR_ENV`), `run` is not the launcher: herdr's own
//! `agent start` types the agent's command into the pane's shell, where the
//! user's `claude` → `seagents run claude` alias turns it into this, so it
//! execs the command as it is — herdr must find the agent itself in its pane,
//! not seagents.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::herdr;
use crate::launch::handle_for;
use crate::registry::{self, Files};
use crate::status;
use crate::window;

/// How long `herdr agent start` may wait for the agent to come up.
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// The monitor's pace.
const TICK: Duration = Duration::from_secs(2);

/// Attaches that fail this many times within [`FAILURE_WINDOW`] end the
/// session: something is wrong that retrying will not fix.
const MAX_FAILURES: usize = 3;
const FAILURE_WINDOW: Duration = Duration::from_secs(10);

/// `seagents run [--tag T] [--seed-file F] <cmd…>`. Exit 2 — before anything
/// is started — when the command is not an agent herdr knows, the tag cannot
/// be had (malformed, or held by a live agent), or the seed cannot be read.
pub(crate) fn run(cmd: Vec<String>, tag: Option<String>, seed_file: Option<PathBuf>) -> i32 {
    // Read and deleted first thing, whatever happens next: the prompt must
    // not outlive `run`'s startup on disk.
    let seed = match seed_file.as_deref().map(take_seed).transpose() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("seagents: {e}");
            return 2;
        }
    };
    if in_herdr_pane() {
        if seed.is_some() {
            eprintln!("seagents: inside a herdr pane the agent runs as it is; its seed prompt is dropped");
        }
        let err = std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec();
        eprintln!("seagents: {}: {err}", cmd[0]);
        return 127;
    }
    let Some(kind) = herdr::kind_for(&cmd[0]) else {
        eprintln!(
            "seagents: {} is not an agent herdr can start; seagents runs one of: {}",
            cmd[0],
            herdr::executables()
        );
        return 2;
    };
    match launch(&cmd, kind, tag, seed) {
        Ok(code) => code,
        Err((code, e)) => {
            eprintln!("seagents: {e}");
            code
        }
    }
}

/// Whether this process runs inside a herdr pane.
fn in_herdr_pane() -> bool {
    std::env::var("HERDR_ENV").as_deref() == Ok("1")
        && std::env::var("HERDR_PANE_ID").is_ok_and(|p| !p.is_empty())
}

/// Read a seed prompt file and delete it. The file is written 0600 into the
/// runtime dir by `spawn`, and its one reader is this.
fn take_seed(path: &Path) -> Result<String, String> {
    let text = std::fs::read(path).map_err(|e| format!("seed file {}: {e}", path.display()));
    let _ = std::fs::remove_file(path);
    String::from_utf8(text?).map_err(|_| format!("seed file {}: not UTF-8", path.display()))
}

/// Everything after the checks: start the agent, show it, end with it.
fn launch(cmd: &[String], kind: &str, tag: Option<String>, seed: Option<String>) -> Result<i32, (i32, String)> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("seagents"));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let session_env = crate::adapter::own_session_env();
    herdr::ensure_server(&session_env).map_err(|e| (1, e))?;
    let adapter = match crate::adapter::for_command(&cmd[0]) {
        Ok(a) => Some(a),
        // A seed has nowhere to go without the adapter.
        Err(e) if seed.is_some() => return Err((2, format!("agent adapter: {e}; cannot seed the prompt"))),
        Err(e) => {
            // The agent still starts — a broken override must not take every
            // `claude` launch down with it — just without our hooks.
            eprintln!("seagents: agent adapter: {e}; launching without hooks");
            None
        }
    };

    // The window closing (SIGHUP; SIGTERM alike) ends the session at any
    // point; Ctrl+C (SIGINT) only cancels the start — while the agent is
    // shown the attach has the terminal raw, and in a window that only waits
    // (taken over) a Ctrl+C must not end an agent another client is using.
    let hup = Arc::new(AtomicBool::new(false));
    let int = Arc::new(AtomicBool::new(false));
    for sig in [libc::SIGHUP, libc::SIGTERM] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&hup));
    }
    let _ = signal_hook::flag::register(libc::SIGINT, Arc::clone(&int));
    let cancelled = || hup.load(Ordering::Relaxed) || int.load(Ordering::Relaxed);
    // A requested tag is also the window's handle: whoever launched us with
    // one (spawn, restore) put `handle_for(tag)` on the terminal.
    let handle = tag.as_deref().map(handle_for);
    // Held from here until the `.meta` exists, so no other `run` can take
    // the same tag in between.
    let claim = registry::claim_tag(tag.as_deref(), &cwd, &crate::snapshot::captured_tags()).map_err(|e| (2, e))?;
    let env = pane_env(adapter.as_ref().map_or(&[][..], |a| &a.launch.pass_env));
    let pane = place(&cwd, &claim.tag, &env).map_err(|e| (1, format!("making the agent's herdr tab: {e}")))?;
    let key = pane.terminal_id.clone();
    let meta = registry::Meta {
        key: key.clone(),
        pane_id: pane.pane_id.clone(),
        socket: herdr::server_socket().unwrap_or_default(),
        run_pid: u64::from(std::process::id()),
        cwd: cwd.to_string_lossy().into_owned(),
        argv: cmd.to_vec(),
        started: crate::clock::unix_now(),
        ..registry::Meta::default()
    };
    // Before the agent starts: its first hook fires before `agent start`
    // returns, and finds the session by this. And before anything else of
    // the session's is written (its hook settings), so `finish` — here, or a
    // reap if this `run` dies — finds all of it.
    let tag = claim.tag.clone();
    let files = claim.create_meta(meta);
    // The window is named after the agent from here on, not after the
    // command line the shell put in its title.
    set_title(&tag);
    let session = || registry::by_key(&key);
    let finish = || {
        if let Some(s) = session() {
            registry::finish(&s);
        }
    };
    let fail = |e: String| {
        let _ = herdr::close_pane(&pane.pane_id);
        finish();
        (1, e)
    };

    let args = match &adapter {
        Some(a) => {
            let settings =
                registry::hooks_file(&key).map_err(|e| fail(format!("no runtime dir for the hook settings: {e}")))?;
            let (argv, warning) = crate::hook::inject(cmd, a, &exe, &cwd, &settings);
            if let Some(w) = warning {
                eprintln!("seagents: {w}");
            }
            argv
        }
        None => cmd.to_vec(),
    };
    // `inject` hands back the user's argv untouched exactly when it added
    // nothing (no hook support, a hookless flag, settings that won't merge).
    if args != cmd {
        registry::update_meta(&files, |m| m.hooks = true);
    }

    // Its window is looked for while the agent starts, on a thread: the
    // handle path polls i3 for up to 5 s (all of it where there is no i3),
    // and the focus path is right only while focus is still where the user
    // typed the command.
    {
        let files = files.clone();
        std::thread::spawn(move || {
            if let Some((con_id, xid)) = identify_window(handle.as_deref()) {
                window::record_window(&files, con_id, xid);
            }
        });
    }
    println!("starting {tag}…");
    clean_shell(&pane.pane_id);
    match start(&tag, kind, &pane.pane_id, &args[1..], &cancelled) {
        Ok(()) => {}
        // Up, and stopped at a dialog on the way (a folder-trust prompt):
        // the window is where it is answered.
        Err(e) if e.code == "agent_not_ready" => {}
        Err(e) if e.code == "cancelled" => {
            let _ = fail(String::new());
            return Err((130, "cancelled".to_string()));
        }
        Err(e) => return Err(fail(format!("starting {tag}: {}", e.message))),
    }
    record_pid(&files, &pane.pane_id);

    let gone = Arc::new(AtomicBool::new(false));
    monitor(files.clone(), key.clone(), tag.clone(), cmd[0].clone(), Arc::clone(&gone));
    if let Some(seed) = seed {
        deliver_seed(key.clone(), seed);
    }

    let outcome = attach_loop(&key, &tag, &hup, &gone);
    // Window closed or agent gone: the agent's pane goes, either way.
    if let Ok(Some(p)) = herdr::pane_of(&key) {
        let _ = herdr::close_pane(&p.pane_id);
    }
    finish();
    match outcome {
        Ended::Failed(e) => Err((1, e)),
        _ => Ok(0),
    }
}

/// The agent's place in herdr: a tab of its own, labelled with its tag, in
/// the workspace of its launch directory — made when there is none, its
/// shell given `env`. A tab already labelled with the tag, holding one pane
/// and no agent, is taken instead (its shell keeps the environment it has):
/// herdr brings tabs back after a server restart, and `restore` relaunches an
/// agent under its old tag into the one it left.
fn place(cwd: &Path, tag: &str, env: &[(String, String)]) -> Result<herdr::Pane, herdr::Error> {
    let cwd_str = cwd.to_string_lossy();
    let panes = herdr::panes()?;
    let agents = herdr::agents()?;
    let here: Vec<&herdr::Pane> = panes.iter().filter(|p| p.cwd == cwd_str).collect();
    let mut workspaces: Vec<&str> = here.iter().map(|p| p.workspace_id.as_str()).collect();
    workspaces.dedup();
    for ws in &workspaces {
        for tab in herdr::tabs(ws)?.into_iter().filter(|t| t.label == tag && t.pane_count == 1) {
            let free = here
                .iter()
                .find(|p| p.tab_id == tab.tab_id && !agents.iter().any(|a| a.terminal_id == p.terminal_id));
            if let Some(p) = free {
                return Ok((*p).clone());
            }
        }
    }
    match workspaces.first() {
        Some(ws) => herdr::create_tab(ws, cwd, tag, env),
        None => {
            let label = cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".to_string());
                let pane = herdr::create_workspace(cwd, &label, env)?;
            if let Err(e) = herdr::rename_tab(&pane.tab_id, tag) {
                let _ = herdr::close_pane(&pane.pane_id);
                return Err(e);
            }
            Ok(pane)
        }
    }
}

/// What a new pane is given of `run`'s own environment. The agent runs in
/// the pane's shell, under the herdr server's environment, not `run`'s; what
/// is carried over is where the tool keeps its configuration (the adapter's
/// `pass_env`), and `SEAGENTS_DEBUG`, so a hook that fails says why
/// (`hook.log`) for an agent started with it.
fn pane_env(pass: &[String]) -> Vec<(String, String)> {
    pass.iter()
        .map(String::as_str)
        .chain(["SEAGENTS_DEBUG"])
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// A herdr server started from inside an agent gives every pane that
/// agent's session variables, and a Claude that inherits them never writes
/// its transcript (bisected 2026-09-29). seagents starts its own servers
/// without them (`herdr::ensure_server`), but one already running may have
/// them: if the pane's shell holds any, they are unset there first.
fn clean_shell(pane: &str) {
    let Ok(procs) = herdr::process_info(pane) else {
        return;
    };
    let names: Vec<String> = std::fs::read(format!("/proc/{}/environ", procs.shell_pid))
        .map(|env| {
            env.split(|b| *b == 0)
                .filter_map(|kv| std::str::from_utf8(kv).ok()?.split_once('=').map(|(k, _)| k.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let leaked = crate::adapter::session_env(names.iter().map(String::as_str));
    if !leaked.is_empty() {
        let _ = herdr::run_line(pane, &format!("unset {}", leaked.join(" ")));
    }
}

/// How long the pane's shell may take to be ready for `agent start`: herdr
/// refuses a pane whose foreground is not its shell (`agent_pane_busy`), and
/// a shell still reading its rc files, or still running the `unset` above,
/// is not yet.
const SHELL_READY: Duration = Duration::from_secs(15);

/// `herdr agent start`, once the pane's shell is in the foreground — and
/// again while herdr still finds the pane busy, within [`SHELL_READY`].
fn start(tag: &str, kind: &str, pane: &str, args: &[String], cancelled: &dyn Fn() -> bool) -> Result<(), herdr::Error> {
    let deadline = Instant::now() + SHELL_READY;
    loop {
        // The pane's foreground process group is its shell's own.
        while Instant::now() < deadline
            && !cancelled()
            && herdr::process_info(pane).is_ok_and(|p| p.fg_pgid != 0 && p.fg_pgid != p.shell_pid)
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        match herdr::start_agent(tag, kind, pane, args, START_TIMEOUT, cancelled) {
            Err(e) if e.code == "agent_pane_busy" && Instant::now() < deadline && !cancelled() => {
                std::thread::sleep(Duration::from_millis(250));
            }
            other => return other,
        }
    }
}

/// Record the agent's pid: its pane's foreground process group, once that
/// is no longer the shell.
fn record_pid(files: &Files, pane: &str) -> Option<u64> {
    let procs = herdr::process_info(pane).ok()?;
    let pid = Some(procs.fg_pgid).filter(|&p| p != 0 && p != procs.shell_pid)?;
    registry::update_meta(files, |m| m.pid = Some(pid));
    Some(pid)
}

/// Capture the i3 window this agent is shown in, as (con_id, X window id).
///
/// Typed by hand, that's the focused window — the user just typed the
/// command into it. Launched by `restore` or `spawn`, focus is a race we would
/// lose: the terminal is still mapping when its shell reaches us, so the
/// focused window is whatever was focused *before*. The handle is set by the
/// terminal at creation and is unique per agent, so it identifies the window
/// with no focus involved at all.
/// Strongest first:
///   1. the window id the terminal put in our environment — exact, instant,
///      and correct even when one process owns many windows;
///   2. our own /proc ancestry ↔ the window's _NET_WM_PID — also exact, but
///      only on a one-process-per-window terminal (Terminator shares one
///      process across all its windows, so this abstains there);
///   3. the handle `restore`/`spawn` put on the terminal, `handle_for(--tag)`;
///   4. the focused terminal, which is right when a human just typed the
///      command into it and wrong for anything spawned programmatically.
fn identify_window(handle: Option<&str>) -> Option<(u64, Option<u64>)> {
    window::window_ids_from_env()
        .or_else(window::window_ids_by_pid)
        .or_else(|| handle.and_then(window::window_ids_for_handle))
        .or_else(window::focused_terminal_ids)
}

/// How the window's time with the agent ended.
enum Ended {
    /// The window is closing (hung up, or `run` was told to stop).
    Window,
    /// The agent exited.
    Agent,
    /// Attaching kept failing; why.
    Failed(String),
}

/// Show the agent in this window until the window or the agent goes.
///
/// A detach (`Ctrl+B q`) is undone: the attach is started again at once, so
/// the window never sits empty over a running agent. Another client taking
/// the terminal over (`herdr terminal attach --takeover`) is respected: this
/// window says so and waits — it never takes the agent back — until it is
/// closed (which still ends the agent) or the agent exits.
fn attach_loop(key: &str, tag: &str, end: &AtomicBool, gone: &AtomicBool) -> Ended {
    // (`end` is the window closing — SIGHUP or SIGTERM — not Ctrl+C: while
    // the agent is shown the attach has the terminal raw, and while this
    // window only waits a Ctrl+C must not end an agent another client uses.)
    let mut failures: Vec<Instant> = Vec::new();
    loop {
        if end.load(Ordering::Relaxed) || hung_up() {
            return Ended::Window;
        }
        if gone.load(Ordering::Relaxed) {
            return Ended::Agent;
        }
        let child = herdr::attach(key).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::piped()).spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return Ended::Failed(format!("running herdr: {e}")),
        };
        // Its stderr carries one line, after it has given the terminal back:
        // why it ended, when not a detach.
        let stderr = child.stderr.take();
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut e) = stderr {
                use std::io::Read;
                let _ = e.read_to_string(&mut text);
            }
            text
        });
        let status = loop {
            if end.load(Ordering::Relaxed) || hung_up() {
                let _ = child.kill();
                let _ = child.wait();
                return Ended::Window;
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Ended::Failed(e.to_string()),
            }
        };
        let said = reader.join().unwrap_or_default();
        if end.load(Ordering::Relaxed) || hung_up() {
            return Ended::Window;
        }
        if gone.load(Ordering::Relaxed) || !agent_alive(key) {
            return Ended::Agent;
        }
        if status.success() {
            // A detach: show it again.
            continue;
        }
        if said.contains("taken over") || said.contains("already has an attached client") {
            eprint!("{said}");
            println!("\n{tag} is shown by another herdr client now; this window waits. Close it to stop {tag}.");
            return wait_out(end, gone);
        }
        eprint!("{said}");
        let now = Instant::now();
        failures.retain(|t| now.duration_since(*t) < FAILURE_WINDOW);
        failures.push(now);
        if failures.len() >= MAX_FAILURES {
            return Ended::Failed(format!("could not show {tag} in this window: {}", said.trim()));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Sit until the window closes or the agent exits.
fn wait_out(end: &AtomicBool, gone: &AtomicBool) -> Ended {
    loop {
        if end.load(Ordering::Relaxed) || hung_up() {
            return Ended::Window;
        }
        if gone.load(Ordering::Relaxed) {
            return Ended::Agent;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Whether this window's terminal is gone: its side of the tty reports a
/// hang-up. Beside SIGHUP, which the shell between us and the terminal may
/// not pass on.
fn hung_up() -> bool {
    let mut fd = libc::pollfd { fd: libc::STDIN_FILENO, events: 0, revents: 0 };
    let n = unsafe { libc::poll(&mut fd, 1, 0) };
    n > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

/// Whether session `key`'s agent still runs (or is still starting).
fn agent_alive(key: &str) -> bool {
    registry::by_key(key).is_some_and(|s| s.alive())
}

/// The monitor: every [`TICK`], fold herdr's verdict and the ledger into
/// `.state`, notify on transitions worth interrupting for, mark a finished
/// turn seen when its window is focused, keep the screen clock the
/// dropped-turn rule needs, and keep the reboot snapshot current. `run` is
/// the one process that lives exactly as long as the agent, so this keeps
/// "idle for / waiting for" truthful even when no HUD or status poll is
/// watching. When the agent exits it closes its pane — the attach ends, and
/// with it the window — and says so through `gone`.
fn monitor(files: Files, key: String, tag: String, cmd: String, gone: Arc<AtomicBool>) {
    let cmd0 = Path::new(&cmd).file_name().and_then(|s| s.to_str()).unwrap_or(&cmd).to_string();
    std::thread::spawn(move || {
        // In-memory previous state for transition notifications — kept apart
        // from `.state` on purpose: another observer (an open HUD) may stamp
        // it first, but the decision must be made from this monitor's own
        // observations.
        let mut prev: Option<status::State> = None;
        let mut screen_hash: Option<u64> = None;
        let mut title = tag.clone();
        loop {
            // Only this `run` finishes its session while it lives, so the
            // `.meta` is there.
            let meta = registry::read_meta(&files).unwrap_or_default();
            let pid = match meta.pid {
                Some(pid) => Some(pid),
                None => herdr::pane_of(&key).ok().flatten().and_then(|p| record_pid(&files, &p.pane_id)),
            };
            if pid.is_some_and(|p| !crate::procfs::alive(p)) {
                if let Ok(Some(p)) = herdr::pane_of(&key) {
                    let _ = herdr::close_pane(&p.pane_id);
                }
                gone.store(true, Ordering::Relaxed);
                return;
            }
            let agent = herdr::agent_on(&key).ok().flatten();
            // The agent's own title (Claude's conversation name), which
            // herdr's attach does not pass on to the window; the tag until
            // it has one.
            if let Some(a) = &agent {
                let want = if a.title.is_empty() { &tag } else { &a.title };
                if *want != title {
                    title = want.clone();
                    set_title(&title);
                }
            }
            let (state, seen, in_turn) = status::track(&files, &cmd0, agent.as_ref());
            if let Some(prev) = prev {
                notify_transition(prev, state, meta.dir(), &cmd0, meta.con_id);
            }
            prev = Some(state);
            // Unacknowledged completion + the agent's window is focused → the
            // user is looking at it: done → idle.
            if state == status::State::Idle && !seen && meta.con_id.is_some() && window::focused_con_id() == meta.con_id {
                status::ack_session_files(&files);
            }
            // The dropped-turn clock: while the agent sits idle in a turn,
            // when its screen last changed.
            match agent.as_ref().filter(|a| in_turn && a.status.idle()) {
                Some(a) => {
                    let hash = herdr::read(&a.pane_id, false).ok().map(|s| {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        s.hash(&mut h);
                        h.finish()
                    });
                    if hash.is_some() && hash != screen_hash {
                        screen_hash = hash;
                        let now = crate::clock::unix_now_ms();
                        registry::update_state(&files, |s| {
                            if let Some(s) = s {
                                s.screen_changed_ms = Some(now);
                            }
                        });
                    }
                }
                None => screen_hash = None,
            }
            // Throttled across every caller, so all but one per interval pay
            // a single stat() here.
            crate::snapshot::maybe_capture();
            std::thread::sleep(TICK);
        }
    });
}

/// Set this window's title: `ESC ] 2 ; <title> BEL` to its terminal, in one
/// `write`, so it cannot land inside a sequence the attach is drawing.
/// herdr's `terminal attach` does not forward the agent's own title (only
/// its full UI shows it), so `run` does. Control characters are dropped from
/// `title` — an ESC, BEL or C1 ST in it would end the sequence early.
fn set_title(title: &str) {
    use std::io::Write;
    let clean: String = title.chars().filter(|c| !c.is_control()).collect();
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write(format!("\x1b]2;{clean}\x07").as_bytes());
    }
}

/// How long the agent must have read idle before its seed goes in: right
/// after a folder-trust dialog closes it reads idle while it is still
/// settling, and drops an Enter.
const SEED_SETTLE: Duration = Duration::from_millis(1500);

/// Give the agent its seed prompt once it is ready for one: past any dialog
/// it opened on the way up (a folder-trust prompt is answered in the window
/// first), settled idle, through the one `send` path — by reference, so the
/// prompt never travels on a command line.
fn deliver_seed(key: String, seed: String) {
    let mut idle_since: Option<Instant> = None;
    // The line standing in for the prompt, its file written once.
    let mut line: Option<String> = None;
    std::thread::spawn(move || loop {
        let Some(session) = registry::by_key(&key).filter(registry::Session::alive) else {
            return;
        };
        let agent = herdr::agent_on(&key).ok().flatten();
        let idle = status::agent_status(agent.as_ref(), session.meta.agent()).is_some_and(|(s, _)| s.idle());
        idle_since = if idle { idle_since.or(Some(Instant::now())) } else { None };
        if idle_since.is_some_and(|t| t.elapsed() >= SEED_SETTLE) {
            if line.is_none() {
                match registry::by_reference(&key, &session.meta.cwd, 0, &seed) {
                    Ok(l) => line = Some(l),
                    Err(e) => {
                        // Not printed: this window's terminal is the agent's
                        // screen now.
                        registry::debug_log("run", &format!("{key}: writing the seed prompt's file: {e}"));
                        return;
                    }
                }
            }
            let body = crate::send::Body::Prompt(line.clone().unwrap_or_default());
            let opts = crate::send::Opts { now: false, answer: false, timeout: Duration::from_secs(10) };
            match crate::send::submit(&session, body, &opts) {
                Ok(_) => return,
                Err(r) if r.reason == Some(crate::send::Refusal::Exit) => return,
                // Anything else — busy, blocked, someone's draft in the box —
                // is waited out: the seed is not dropped.
                Err(r) => registry::debug_log("run", &format!("{key}: seed held: {}", r.error.unwrap_or_default())),
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    });
}

/// Desktop notification for a state transition worth interrupting the user
/// for: the agent finished (working → idle) or is waiting on input
/// (→ blocked). Suppressed while the agent's own window is focused — the
/// user is already looking at it. SEAGENTS_NO_NOTIFY=1 disables.
fn notify_transition(prev: status::State, state: status::State, dir: &str, cmd: &str, con_id: Option<u64>) {
    use status::State;
    if std::env::var_os("SEAGENTS_NO_NOTIFY").is_some() {
        return;
    }
    let (summary, body, urgency) = match (prev, state) {
        (State::Working, State::Idle) => (format!("{cmd} · {dir}: done"), "finished and waiting at the prompt".to_string(), "normal"),
        (p, State::Blocked) if p != State::Blocked && p != State::Unknown => {
            (format!("{cmd} · {dir}: needs you"), "waiting for your input".to_string(), "critical")
        }
        _ => return,
    };
    if con_id.is_some() && window::focused_con_id() == con_id {
        return;
    }
    // Reap in a detached thread: a dropped Child is never wait()ed, so each
    // finished notify-send would otherwise linger as a zombie.
    if let Ok(mut child) = std::process::Command::new("notify-send")
        .args(["-a", "seagents", "-u", urgency, "-t", "8000", &summary, &body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
