//! `seagents spawn` — open a new terminal running an agent, seeded with a
//! prompt, and hand back its tag.
//!
//! The primary caller is another agent, so everything that bounds it is
//! decided here from facts the caller cannot casually change: lineage from
//! `/proc` ancestry ([`registry::enclosing_session`]), fan-out caps and the
//! tag under the tags lock ([`registry::claim_spawn`]), the permission mode
//! from the parent's own hook-reported mode. The window is opened by the one
//! launcher `restore` uses ([`launch::spawn_terminal`]); the prompt travels
//! in a 0600 file in the runtime dir that `run --seed-file` reads and deletes,
//! so it never lands in `.meta` or a process's environment.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::adapter::Launch;
use crate::launch;
use crate::registry;

/// How long the child's `.meta` may take to appear. `run` writes it right
/// after forking the agent, before it looks for its window, so this covers
/// the terminal opening and the shell in it starting `run` — normally well
/// under a second; the margin is for a loaded machine or a cold terminal, and
/// only a launch that failed outright ever waits it out.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(15);

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
    let adapter = crate::adapter::for_command(&args.agent)?;
    let prompt = match (&args.prompt_file, args.prompt) {
        (Some(f), _) => std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?,
        (None, Some(p)) => p,
        (None, None) => return Err("no prompt: give PROMPT or --prompt-file".to_string()),
    };
    if prompt.trim().is_empty() {
        return Err("the prompt is empty".to_string());
    }
    // Everything that can refuse the prompt is checked before a window opens.
    adapter.launch.check_seed(&prompt)?;
    if args.args.iter().any(|a| a == "--") {
        return Err("--arg --: the agent's arguments end where the seed prompt begins".to_string());
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
    // The seed's file is written there by `run`, in the new window, where a
    // failure would only show as a window that closed at once. (A worktree
    // is made below, inside the repo, writable by construction.)
    if adapter.launch.seed_by == crate::adapter::SeedBy::File && !args.worktree && !writable(&cwd) {
        return Err(format!(
            "{} is not writable, and this agent's seed prompt goes to a file there (.seagents/prompts/)",
            cwd.display()
        ));
    }

    // Unknown is not human: a human's spawn is uncapped and keeps its
    // `--arg`s, so it is never the fallback for an unreadable ancestry.
    let caller = registry::enclosing_session(u64::from(std::process::id()), None)
        .map_err(|e| format!("cannot tell who is calling ({e}); nothing was spawned"))?;
    let who = match &caller {
        None => Caller::Human,
        // The mode as the parent's own adapter reads it: its hooks' word,
        // bounded by its command line where they report less than the
        // mode (Codex).
        Some(s) => Caller::Agent {
            mode: crate::adapter::for_command(s.meta.argv.first().map_or("", String::as_str))
                .unwrap_or_default()
                .launch
                .bound_mode(registry::read_agent(&s.files).permission_mode, &s.meta.argv),
        },
    };
    let permission = permission_args(&adapter.launch, &who, args.permission_mode.as_deref())?;
    if matches!(who, Caller::Agent { .. }) {
        privileged_check(&adapter.launch, &args.args)?;
    }

    let claim = registry::claim_spawn(
        args.tag.as_deref(),
        caller.as_ref().map(|s| s.meta.wrapper_pid),
        &cwd,
        &crate::snapshot::captured_tags(),
    )?;

    let cwd = if args.worktree {
        add_worktree(&cwd, &claim.tag)?
    } else {
        cwd
    };

    let mut argv = vec![args.agent.clone()];
    argv.extend(permission);
    argv.extend(args.args);

    let seed = write_seed(&claim.tag, &prompt)?;
    let cwd_str = cwd.to_string_lossy().into_owned();
    let run = launch::Run {
        tag: &claim.tag,
        parent: claim.parent.as_deref(),
        seed_file: Some(&seed),
        argv: &argv,
        keep_shell: false,
    };
    // The terminal the caller itself is in, so an agent's children open
    // where it runs; `SEAGENTS_TERMINAL` overrides, as for `restore`.
    let class = crate::window::terminal_class(caller.as_ref().and_then(|s| s.meta.con_id));
    if let Err(e) = launch::spawn_terminal(&run, &cwd_str, &class) {
        let _ = std::fs::remove_file(&seed);
        return Err(e);
    }

    let deadline = Instant::now() + REGISTER_TIMEOUT;
    let child = loop {
        if let Some(s) = registry::list().into_iter().find(|s| s.meta.tag == claim.tag) {
            break s;
        }
        if Instant::now() >= deadline {
            // `run` never started (or died before its `.meta`), so nobody
            // else will delete the prompt.
            let _ = std::fs::remove_file(&seed);
            return Err(format!(
                "no agent registered as {} within {} s — the terminal did not start `seagents run` \
                 (is SEAGENTS_TERMINAL right, and is the display reachable?)",
                claim.tag,
                REGISTER_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    Ok(serde_json::json!({
        "tag": child.meta.tag,
        "child_pid": child.meta.child_pid,
        "wrapper_pid": child.meta.wrapper_pid,
        "cwd": child.meta.cwd,
        "parent": child.meta.parent,
        "depth": child.meta.depth,
        "stops": 0,
    }))
}

/// Who is spawning, as far as permissions go.
#[derive(Debug)]
enum Caller {
    /// No seagents wrapper among the caller's ancestors.
    Human,
    /// An agent, with its mode as its hooks last reported it, bounded by its
    /// own command line for an adapter whose hooks report less than the mode
    /// (`Launch::bound_mode`). None: no hook has reported one — not started
    /// yet, or `--bare`.
    Agent { mode: Option<String> },
}

/// The argv that starts the child in its permission mode, per the adapter's
/// `permission_argv`.
///
/// A human gets exactly what they asked for, and the tool's own configured
/// default when they asked for nothing — the same as typing the command in a
/// terminal. An agent's child always gets an explicit mode, because the
/// tool's default on this machine is `bypassPermissions`: the parent's own
/// mode when none is asked for, never one ranked above it in the adapter's
/// `permission_modes` (least privileged first), and the least privileged
/// one when the parent's mode is unknown. The parent's mode is a word from
/// its own hooks and the child's adapter ranks it, which works across tools
/// because every bundled adapter names its modes in the one vocabulary
/// (Claude's) that every bundled tool's hooks report in.
fn permission_args(launch: &Launch, caller: &Caller, requested: Option<&str>) -> Result<Vec<String>, String> {
    let modes = &launch.permission_modes;
    if modes.is_empty() {
        return match requested {
            Some(_) => Err("this agent has no permission modes".to_string()),
            None => Ok(Vec::new()),
        };
    }
    let rank = |m: &str| modes.iter().position(|x| x == m);
    if let Some(m) = requested.filter(|m| rank(m).is_none()) {
        return Err(format!("unknown permission mode {m:?}; one of: {}", modes.join(", ")));
    }
    let chosen = match caller {
        Caller::Human => match requested {
            Some(m) => m.to_string(),
            None => return Ok(Vec::new()),
        },
        Caller::Agent { mode } => {
            let ceiling = mode.as_deref().and_then(rank).unwrap_or(0);
            match requested {
                Some(m) if rank(m).unwrap_or(usize::MAX) > ceiling => {
                    return Err(format!(
                        "permission mode {m} is above your own ({}); a child never outranks its parent",
                        mode.as_deref().unwrap_or("unknown, so the least privileged")
                    ))
                }
                Some(m) => m.to_string(),
                None => modes[ceiling].clone(),
            }
        }
    };
    // `parse` guarantees argv for every listed mode.
    Ok(launch.permission_argv.get(&chosen).cloned().unwrap_or_default())
}

/// An agent's `--arg`s may not reach past the mode [`permission_args`]
/// capped: the adapter's `privileged` flags, subcommands and configuration
/// keys are refused (a human's are not checked — they could type the same
/// command into a terminal).
fn privileged_check(launch: &Launch, args: &[String]) -> Result<(), String> {
    match launch.privileged_arg(args) {
        None => Ok(()),
        Some(arg) => Err(format!(
            "--arg {arg}: an agent may not pass arguments that set permissions or sandboxing; \
             use --permission-mode (capped at your own)"
        )),
    }
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
/// returning the directory the child should run in: the worktree's
/// counterpart of `cwd` (the same subdirectory, when it exists there).
///
/// Inside the repo, not beside it: Claude's folder-trust dialog appears in
/// any never-trusted directory under every permission mode, before any hook
/// (verified 2026-09-29), while a subdirectory of a trusted one inherits the
/// trust — a sibling worktree would start every child blocked on a human.
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
            return Err(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
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
