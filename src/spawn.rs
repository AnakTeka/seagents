//! `seagents spawn` — open a new terminal running an agent, seeded with a
//! prompt, and hand back its tag.
//!
//! The window is opened by the one launcher `restore` uses
//! ([`launch::spawn_terminal`]) and runs `seagents run --tag <tag>`, which
//! starts the agent in herdr and shows it there. The prompt travels in a
//! 0600 file in the runtime dir that `run --seed-file` reads and deletes, and
//! reaches the agent by reference once it is idle (`run`), so it never lands
//! in `.meta`, an environment or a command line.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::launch;
use crate::registry;

/// How long the agent may take to start: its window opening, the shell in it
/// starting `run`, and `herdr agent start` waiting for the agent (up to
/// 120 s) — normally a few seconds; only a launch that failed outright ever
/// waits it out.
const START_TIMEOUT: Duration = Duration::from_secs(150);

pub(crate) struct Args {
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) tag: Option<String>,
    pub(crate) agent: String,
    pub(crate) permission_mode: Option<String>,
    pub(crate) worktree: bool,
    pub(crate) args: Vec<String>,
    pub(crate) prompt_file: Option<PathBuf>,
    pub(crate) prompt: Option<String>,
}

/// `seagents spawn …`: prints the handle JSON; 1 on any refusal or failure,
/// with the reason on stderr.
pub(crate) fn run(args: Args) -> i32 {
    match spawn(args) {
        Ok(handle) => {
            println!("{handle}");
            0
        }
        Err(e) => {
            eprintln!("seagents spawn: {e}");
            1
        }
    }
}

fn spawn(args: Args) -> Result<serde_json::Value, String> {
    if crate::herdr::kind_for(&args.agent).is_none() {
        return Err(format!("{} is not an agent herdr can start; one of: {}", args.agent, crate::herdr::executables()));
    }
    let adapter = crate::adapter::for_command(&args.agent)?;
    let prompt = match (&args.prompt_file, args.prompt) {
        (Some(f), _) => std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?,
        (None, Some(p)) => p,
        (None, None) => return Err("no prompt: give PROMPT or --prompt-file".to_string()),
    };
    if prompt.trim().is_empty() {
        return Err("the prompt is empty".to_string());
    }
    let cwd = args
        .cwd
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .and_then(|d| d.canonicalize())
        .map_err(|e| format!("working directory: {e}"))?;
    if !cwd.is_dir() {
        return Err(format!("{} is not a directory", cwd.display()));
    }
    // The seed reaches the agent through a prompt file there, written in the
    // new window, where a failure would only show as a seed that never
    // arrived. (A worktree is made below, inside the repo, writable by
    // construction.)
    if !args.worktree && !writable(&cwd) {
        return Err(format!(
            "{} is not writable, and the seed prompt goes to a file there (.seagents/prompts/)",
            cwd.display()
        ));
    }
    let permission = permission_args(&adapter.launch, args.permission_mode.as_deref())?;
    let tag = registry::pick_tag(args.tag.as_deref(), &cwd, &crate::snapshot::captured_tags())?;

    let cwd = if args.worktree { add_worktree(&cwd, &tag)? } else { cwd };
    let mut argv = vec![args.agent.clone()];
    argv.extend(permission);
    argv.extend(args.args);

    let seed = write_seed(&tag, &prompt)?;
    let cwd_str = cwd.to_string_lossy().into_owned();
    let run = launch::Run { tag: &tag, seed_file: Some(&seed), argv: &argv, keep_shell: false };
    if let Err(e) = launch::spawn_terminal(&run, &cwd_str, &crate::window::terminal_class(caller_window())) {
        let _ = std::fs::remove_file(&seed);
        return Err(e);
    }

    // Registered (its `.meta`) once `run` has its herdr tab; started (a pid)
    // once herdr has the agent up. A `.meta` that appears and goes again is a
    // start that failed — the window says why.
    let deadline = Instant::now() + START_TIMEOUT;
    let mut registered = false;
    loop {
        match registry::list().into_iter().find(|s| s.meta.tag == tag) {
            Some(s) if s.meta.pid.is_some() => {
                return Ok(serde_json::json!({
                    "tag": s.meta.tag,
                    "pid": s.meta.pid,
                    "pane_id": s.meta.pane_id,
                    "cwd": s.meta.cwd,
                    "stops": 0,
                }));
            }
            Some(_) => registered = true,
            None if registered => {
                return Err(format!("{tag} registered but did not start; its window says why (if it is still open)"));
            }
            None => {}
        }
        if Instant::now() >= deadline {
            // `run` never started (or died before reading it), so nobody
            // else will delete the prompt.
            let _ = std::fs::remove_file(&seed);
            return Err(if registered {
                format!("{tag} did not start within {} s", START_TIMEOUT.as_secs())
            } else {
                format!(
                    "no agent registered as {tag} within {} s — the terminal did not start `seagents run` \
                     (is SEAGENTS_TERMINAL right, and is the display reachable?)",
                    START_TIMEOUT.as_secs()
                )
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The i3 window of the agent calling `spawn`, if it is one seagents
/// started — so its new agent opens in the same terminal. From the herdr pane
/// the caller runs in.
fn caller_window() -> Option<u64> {
    let pane = std::env::var("HERDR_PANE_ID").ok()?;
    let socket = std::env::var("HERDR_SOCKET_PATH").unwrap_or_default();
    registry::by_pane(&pane, &socket)?.meta.con_id
}

/// The argv that starts the agent in permission mode `requested`, per the
/// adapter's `permission_argv`; none asked for, none passed — the tool's own
/// configured default, the same as typing the command in a terminal.
fn permission_args(launch: &crate::adapter::Launch, requested: Option<&str>) -> Result<Vec<String>, String> {
    let Some(mode) = requested else {
        return Ok(Vec::new());
    };
    let modes = &launch.permission_argv;
    if modes.is_empty() {
        return Err("this agent has no permission modes".to_string());
    }
    modes.get(mode).cloned().ok_or_else(|| {
        let known: Vec<&str> = modes.keys().map(String::as_str).collect();
        format!("unknown permission mode {mode:?}; one of: {}", known.join(", "))
    })
}

fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// `$runtime/seed-<tag>`, 0600, holding the prompt for `run --seed-file`.
fn write_seed(tag: &str, prompt: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = registry::runtime_dir()
        .map_err(|e| format!("no runtime dir for the seed prompt: {e}"))?
        .join(format!("seed-{tag}"));
    // A leftover from a spawn of this tag that died before `run` read it.
    let _ = std::fs::remove_file(&path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(prompt.as_bytes()))
        .map_err(|e| format!("writing the seed prompt to {}: {e}", path.display()))?;
    Ok(path)
}

/// `git worktree add <repo>/.seagents/worktrees/<tag> -b seagents/<tag>`,
/// returning the directory the agent should run in: the worktree's
/// counterpart of `cwd` (the same subdirectory, when it exists there).
///
/// Inside the repo, not beside it: Claude's folder-trust dialog appears in
/// any never-trusted directory under every permission mode, before any hook
/// (verified 2026-09-29), while a subdirectory of a trusted one inherits the
/// trust — a sibling worktree would start every agent blocked on a human.
/// Refuses an existing path or branch rather than reusing a stale one.
/// `.seagents/` goes into the repo's local `info/exclude`, never a tracked
/// file.
fn add_worktree(cwd: &Path, tag: &str) -> Result<PathBuf, String> {
    let git = |args: &[&str]| -> Result<String, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .map_err(|e| format!("running git: {e}"))?;
        if !out.status.success() {
            return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let root = PathBuf::from(git(&["rev-parse", "--show-toplevel"]).map_err(|e| format!("--worktree: {e}"))?);
    let path = root.join(".seagents/worktrees").join(tag);
    let branch = format!("seagents/{tag}");
    if path.exists() {
        return Err(format!("--worktree: {} already exists", path.display()));
    }
    if git(&["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
        return Err(format!("--worktree: branch {branch} already exists"));
    }
    registry::exclude_seagents(&root)?;
    git(&["worktree", "add", &path.to_string_lossy(), "-b", &branch])?;
    let within = cwd.strip_prefix(&root).unwrap_or(Path::new(""));
    Ok(Some(path.join(within)).filter(|p| p.is_dir()).unwrap_or(path))
}
