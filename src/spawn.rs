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
use crate::registry::{self, Caps};

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
    adapter.launch.seed_args(&prompt)?;
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

    let caller = registry::enclosing_session(u64::from(std::process::id()));
    let who = match &caller {
        None => Caller::Human,
        Some(s) => Caller::Agent {
            mode: registry::read_agent(&s.files).permission_mode,
        },
    };
    let permission = permission_args(&adapter.launch, &who, args.permission_mode.as_deref())?;

    let claim = registry::claim_spawn(
        args.tag.as_deref(),
        caller.as_ref().map(|s| s.meta.wrapper_pid),
        &cwd,
        &crate::snapshot::captured_tags(),
        &caps_from_env()?,
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
    /// An agent, with its mode as its hooks last reported it (None: no hook
    /// has reported one — not started yet, or `--bare`).
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

/// Caps from `SEAGENTS_MAX_DEPTH` / `_MAX_CHILDREN` / `_MAX_SPAWNED`, else
/// the defaults (3 deep, 4 wide, 12 in all). A value that isn't a number is
/// an error, not silently the default.
fn caps_from_env() -> Result<Caps, String> {
    caps_from(|k| std::env::var(k).ok())
}

fn caps_from(var: impl Fn(&str) -> Option<String>) -> Result<Caps, String> {
    let d = Caps::default();
    let get = |name: &str, default: usize| -> Result<usize, String> {
        match var(name) {
            None => Ok(default),
            Some(v) => v
                .trim()
                .parse()
                .map_err(|_| format!("{name}={v:?} is not a number")),
        }
    };
    Ok(Caps {
        depth: get("SEAGENTS_MAX_DEPTH", d.depth as usize)? as u32,
        children: get("SEAGENTS_MAX_CHILDREN", d.children)?,
        spawned: get("SEAGENTS_MAX_SPAWNED", d.spawned)?,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> Launch {
        let _env = crate::vendor::herdr_detect::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::adapter::for_agent("claude").unwrap().launch
    }

    fn agent(mode: Option<&str>) -> Caller {
        Caller::Agent {
            mode: mode.map(str::to_string),
        }
    }

    #[test]
    fn a_human_gets_what_they_ask_for_and_the_tools_default_otherwise() {
        let l = claude();
        assert_eq!(permission_args(&l, &Caller::Human, None).unwrap(), Vec::<String>::new());
        assert_eq!(
            permission_args(&l, &Caller::Human, Some("bypassPermissions")).unwrap(),
            ["--permission-mode", "bypassPermissions"]
        );
        assert!(permission_args(&l, &Caller::Human, Some("yolo")).unwrap_err().contains("one of"));
    }

    #[test]
    fn an_agents_child_never_outranks_it_and_always_gets_an_explicit_mode() {
        let l = claude();
        let mode = |caller: &Caller, req: Option<&str>| permission_args(&l, caller, req).map(|a| a[1].clone());
        // Default: the parent's own mode, passed explicitly.
        assert_eq!(mode(&agent(Some("acceptEdits")), None).unwrap(), "acceptEdits");
        assert_eq!(mode(&agent(Some("bypassPermissions")), None).unwrap(), "bypassPermissions");
        // At or below the parent: fine. Above: refused.
        assert_eq!(mode(&agent(Some("acceptEdits")), Some("plan")).unwrap(), "plan");
        assert_eq!(mode(&agent(Some("acceptEdits")), Some("acceptEdits")).unwrap(), "acceptEdits");
        assert!(mode(&agent(Some("acceptEdits")), Some("bypassPermissions")).unwrap_err().contains("outranks"));
        // Parent's mode unknown, or one the adapter doesn't list: least privileged.
        assert_eq!(mode(&agent(None), None).unwrap(), "plan");
        assert_eq!(mode(&agent(Some("someNewMode")), None).unwrap(), "plan");
        assert!(mode(&agent(None), Some("default")).is_err());
        assert!(mode(&agent(Some("plan")), Some("bogus")).unwrap_err().contains("unknown"));
    }

    #[test]
    fn an_adapter_without_modes_takes_none() {
        let none = Launch::default();
        assert_eq!(permission_args(&none, &agent(Some("x")), None).unwrap(), Vec::<String>::new());
        assert!(permission_args(&none, &Caller::Human, Some("plan")).is_err());
    }

    #[test]
    fn a_codex_child_gets_the_approval_and_sandbox_pair_for_its_mode() {
        let l = {
            let _env = crate::vendor::herdr_detect::config::test_config_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            crate::adapter::for_agent("codex").unwrap().launch
        };
        let args = |caller: &Caller, req: Option<&str>| permission_args(&l, caller, req).map(|a| a.join(" "));
        const READ_ONLY: &str = "-a on-request -s read-only";
        // A Codex parent reports "default" (on-request) or "bypassPermissions"
        // (never), as a Claude parent reports any of its four.
        assert_eq!(args(&agent(Some("default")), None).unwrap(), READ_ONLY);
        assert_eq!(args(&agent(Some("plan")), None).unwrap(), READ_ONLY);
        assert_eq!(args(&agent(None), None).unwrap(), READ_ONLY);
        assert_eq!(args(&agent(Some("acceptEdits")), None).unwrap(), "-a on-request -s workspace-write");
        assert_eq!(args(&agent(Some("bypassPermissions")), None).unwrap(), "-a never -s danger-full-access");
        assert!(args(&agent(Some("default")), Some("acceptEdits")).unwrap_err().contains("outranks"));
        assert_eq!(args(&agent(Some("bypassPermissions")), Some("plan")).unwrap(), READ_ONLY);
        // A human who asks for nothing gets Codex's own configured default.
        assert_eq!(args(&Caller::Human, None).unwrap(), "");
    }

    #[test]
    fn caps_default_and_env_overrides_must_be_numbers() {
        assert_eq!(caps_from(|_| None).unwrap(), Caps::default());
        let c = caps_from(|k| (k == "SEAGENTS_MAX_CHILDREN").then(|| " 9 ".to_string())).unwrap();
        assert_eq!((c.depth, c.children, c.spawned), (3, 9, 12));
        assert!(caps_from(|k| (k == "SEAGENTS_MAX_DEPTH").then(|| "lots".to_string()))
            .unwrap_err()
            .contains("SEAGENTS_MAX_DEPTH"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seagents-spawn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_worktree_lands_inside_the_repo_and_is_never_reused() {
        let repo = scratch("wt");
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/f"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        let got = add_worktree(&repo.join("sub"), "kid").unwrap();
        assert_eq!(got, repo.join(".seagents/worktrees/kid/sub"));
        assert!(got.join("f").exists());
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == ".seagents/").count(), 1);
        // The worktree is not an untracked change in the parent's tree.
        let status = Command::new("git").arg("-C").arg(&repo).args(["status", "--porcelain"]).output().unwrap();
        assert!(status.stdout.is_empty(), "{}", String::from_utf8_lossy(&status.stdout));

        assert!(add_worktree(&repo, "kid").unwrap_err().contains("already exists"));
        std::fs::remove_dir_all(repo.join(".seagents/worktrees/kid")).unwrap();
        assert!(add_worktree(&repo, "kid").unwrap_err().contains("branch seagents/kid"));
        assert!(add_worktree(Path::new("/"), "x").unwrap_err().contains("--worktree"));
        std::fs::remove_dir_all(&repo).ok();
    }
}
