//! The one place seagents talks to herdr.
//!
//! A herdr server owns every agent seagents tracks: the agent runs in one of
//! its panes, and the i3 window the user sees runs `herdr terminal attach`
//! (`crate::run`). Everything seagents asks herdr — which agents there are and
//! what state each is in, starting one, typing into one, reading its screen,
//! closing it — goes through the `herdr` CLI from here, and its JSON is parsed
//! here and nowhere else. The CLI rather than the socket API because the CLI
//! is the surface herdr keeps stable across versions (it checks the server's
//! protocol itself); written against herdr 0.9.0.
//!
//! Which server: whatever the environment names, as for any `herdr` command —
//! `HERDR_SOCKET_PATH`, else `HERDR_SESSION`, else the default session.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The variables herdr sets inside one of its panes, naming that pane. A
/// window seagents opens must not inherit them from an agent calling `spawn`
/// (its `run` would take itself for an agent herdr started, and exec), and
/// neither must a server seagents starts. `HERDR_SOCKET_PATH` and
/// `HERDR_SESSION` are not among them: they name the server, which the new
/// window should keep using.
pub(crate) const PANE_ENV: [&str; 4] = ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_TAB_ID", "HERDR_WORKSPACE_ID"];

/// Every agent kind herdr 0.9.0 can start (`agent start --kind`), as
/// (label, executable): the label is the kind herdr reports and seagents keys
/// adapters by; the executable is what herdr types into the pane.
const KINDS: &[(&str, &str)] = &[
    ("pi", "pi"),
    ("claude", "claude"),
    ("codex", "codex"),
    ("gemini", "gemini"),
    ("cursor", "cursor-agent"),
    ("devin", "devin"),
    ("agy", "agy"),
    ("cline", "cline"),
    ("omp", "omp"),
    ("mastracode", "mastracode"),
    ("opencode", "opencode"),
    ("copilot", "copilot"),
    ("kimi", "kimi"),
    ("kiro", "kiro-cli"),
    ("droid", "droid"),
    ("amp", "amp"),
    ("grok", "grok"),
    ("hermes", "hermes"),
    ("kilo", "kilo"),
    ("qodercli", "qodercli"),
    ("qwen", "qwen"),
    ("maki", "maki"),
    ("muse", "muse"),
];

/// The herdr kind of the command whose first word is `argv0` (a path or a
/// bare name), by its executable or its label. None: herdr cannot start it.
pub(crate) fn kind_for(argv0: &str) -> Option<&'static str> {
    let name = Path::new(argv0).file_name().and_then(|n| n.to_str()).unwrap_or(argv0);
    KINDS
        .iter()
        .find(|(label, exe)| name == *exe || name == *label)
        .map(|(label, _)| *label)
}

/// The executables `run` accepts, for its refusal message.
pub(crate) fn executables() -> String {
    KINDS.iter().map(|(_, exe)| *exe).collect::<Vec<_>>().join(", ")
}

/// Why a herdr call failed: herdr's own error code and message when it gave
/// one (`agent_blocked`, `server_not_running`, …), else ours.
#[derive(Debug, Clone)]
pub(crate) struct Error {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl Error {
    fn new(code: &str, message: impl Into<String>) -> Error {
        Error { code: code.to_string(), message: message.into() }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "herdr: {}", self.message)
    }
}

/// An agent's lifecycle state as herdr detects it from its screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    /// Idle, and herdr's own "not yet seen" marker on it — which seagents
    /// does not use (it keeps its own, `.state`'s `seen`): idle to us.
    Done,
    Working,
    Blocked,
    Unknown,
}

impl Status {
    fn parse(s: &str) -> Status {
        match s {
            "idle" => Status::Idle,
            "done" => Status::Done,
            "working" => Status::Working,
            "blocked" => Status::Blocked,
            _ => Status::Unknown,
        }
    }

    /// Ready for input: idle or done.
    pub(crate) fn idle(self) -> bool {
        matches!(self, Status::Idle | Status::Done)
    }
}

/// One agent, as `agent list` / `agent get` report it.
#[derive(Clone, Debug)]
pub(crate) struct Agent {
    /// Its herdr name — the tag of an agent seagents started.
    pub(crate) name: String,
    pub(crate) status: Status,
    pub(crate) terminal_id: String,
    pub(crate) pane_id: String,
    /// The window title the agent set (Claude's conversation name), glyph
    /// stripped by herdr.
    pub(crate) title: String,
}

impl Agent {
    fn from(v: &Value) -> Agent {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        Agent {
            name: s("name"),
            status: Status::parse(v["agent_status"].as_str().unwrap_or("unknown")),
            terminal_id: s("terminal_id"),
            pane_id: s("pane_id"),
            title: s("terminal_title_stripped"),
        }
    }
}

/// One pane, as `pane list` and the create calls report it.
#[derive(Clone, Debug)]
pub(crate) struct Pane {
    pub(crate) pane_id: String,
    pub(crate) tab_id: String,
    pub(crate) workspace_id: String,
    pub(crate) terminal_id: String,
    pub(crate) cwd: String,
}

impl Pane {
    fn from(v: &Value) -> Pane {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        Pane {
            pane_id: s("pane_id"),
            tab_id: s("tab_id"),
            workspace_id: s("workspace_id"),
            terminal_id: s("terminal_id"),
            cwd: s("cwd"),
        }
    }
}

/// One tab, as `tab list` reports it.
#[derive(Clone, Debug)]
pub(crate) struct Tab {
    pub(crate) tab_id: String,
    pub(crate) label: String,
    pub(crate) pane_count: u64,
}

/// What runs in a pane (`pane process-info`).
#[derive(Clone, Debug)]
pub(crate) struct Procs {
    pub(crate) shell_pid: u64,
    /// The pane's foreground process group: the agent's, once it runs.
    pub(crate) fg_pgid: u64,
}

/// The `herdr` binary: the one herdr itself names inside its panes
/// (`HERDR_BIN_PATH`), else the first on `PATH`, else `~/.local/bin/herdr` —
/// where herdr installs itself, and which a window i3 opens through `sh -c`
/// may not have on its `PATH`.
fn bin() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_BIN_PATH").filter(|p| !p.is_empty()).map(PathBuf::from) {
        if p.is_file() {
            return p;
        }
    }
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).map(|d| d.join("herdr")).find(|p| p.is_file())
    });
    if let Some(p) = on_path {
        return p;
    }
    let local = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/herdr"));
    local.filter(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("herdr"))
}

fn command(args: &[&str]) -> Command {
    let mut cmd = Command::new(bin());
    cmd.args(args).stdin(Stdio::null());
    cmd
}

/// What a finished `herdr` call printed: Ok(stdout) on success, else the
/// error from herdr's JSON on stderr (or what there is of one).
fn outcome(out: Output) -> Result<String, Error> {
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if let Some(e) = stderr.lines().find_map(|l| serde_json::from_str::<Value>(l).ok()) {
        if let Some(code) = e["error"]["code"].as_str() {
            return Err(Error::new(code, e["error"]["message"].as_str().unwrap_or(code)));
        }
    }
    let code = if out.status.code() == Some(2) { "usage" } else { "failed" };
    let message = stderr.trim();
    Err(Error::new(code, if message.is_empty() { "the command failed" } else { message }))
}

fn spawn_error(e: std::io::Error) -> Error {
    Error::new("no_herdr", format!("running {}: {e} (is herdr installed?)", bin().display()))
}

/// Run `herdr <args>` and return its stdout.
fn text(args: &[&str]) -> Result<String, Error> {
    outcome(command(args).output().map_err(spawn_error)?)
}

/// Run `herdr <args>` and return the `result` of the JSON it prints — null
/// for the commands that print nothing when they succeed (`pane send-keys`,
/// `send-text`, `run`).
fn call(args: &[&str]) -> Result<Value, Error> {
    let out = text(args)?;
    if out.trim().is_empty() {
        return Ok(Value::Null);
    }
    let v: Value = serde_json::from_str(out.trim())
        .map_err(|e| Error::new("bad_reply", format!("herdr {}: unreadable reply: {e}", args.join(" "))))?;
    Ok(v["result"].clone())
}

/// The API socket of the server this environment names — what an agent's
/// hooks see as `HERDR_SOCKET_PATH`. None when no server is running.
pub(crate) fn server_socket() -> Option<String> {
    let v: Value = serde_json::from_str(&text(&["status", "--json"]).ok()?).ok()?;
    (v["server"]["running"].as_bool() == Some(true)).then(|| v["server"]["socket"].as_str().map(str::to_string))?
}

/// Whether a herdr server is up for this environment.
pub(crate) fn server_running() -> bool {
    server_socket().is_some()
}

/// Make sure a herdr server is running, starting one if not: `herdr server`,
/// detached into its own session, without the variables that would leak into
/// every pane it opens — a calling agent's own session (`strip`, the adapters'
/// `session_env`; a Claude that inherits them never writes its transcript),
/// the pane that called us ([`PANE_ENV`]) and the window that did
/// (`window::WINDOW_ENV`). herdr's CLI does not start a server
/// itself (only bare `herdr`, its TUI, does).
pub(crate) fn ensure_server(strip: &[String]) -> Result<(), String> {
    if server_running() {
        return Ok(());
    }
    let mut cmd = Command::new(bin());
    cmd.arg("server").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    for name in strip.iter().map(String::as_str).chain(PANE_ENV).chain(crate::window::WINDOW_ENV) {
        cmd.env_remove(name);
    }
    if let Some(home) = std::env::var_os("HOME") {
        cmd.current_dir(home);
    }
    // Its own session: not hung up with the window that started it.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| spawn_error(e).to_string())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if server_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("started `herdr server`, but it did not come up within 5 s".to_string())
}

/// Every agent herdr knows.
pub(crate) fn agents() -> Result<Vec<Agent>, Error> {
    let v = call(&["agent", "list"])?;
    Ok(v["agents"].as_array().map(|a| a.iter().map(Agent::from).collect()).unwrap_or_default())
}

/// The agent hosted by terminal `terminal_id`, if herdr sees one there now
/// (it does not while the agent is suspended, or before it has started).
/// By terminal rather than pane id: a pane moved to another workspace gets a
/// new id, and its terminal keeps its own.
pub(crate) fn agent_on(terminal_id: &str) -> Result<Option<Agent>, Error> {
    Ok(agents()?.into_iter().find(|a| a.terminal_id == terminal_id))
}

/// Every pane.
pub(crate) fn panes() -> Result<Vec<Pane>, Error> {
    let v = call(&["pane", "list"])?;
    Ok(v["panes"].as_array().map(|a| a.iter().map(Pane::from).collect()).unwrap_or_default())
}

/// The pane terminal `terminal_id` is in now; None once it is closed.
pub(crate) fn pane_of(terminal_id: &str) -> Result<Option<Pane>, Error> {
    Ok(panes()?.into_iter().find(|p| p.terminal_id == terminal_id))
}

/// Workspace `workspace`'s tabs.
pub(crate) fn tabs(workspace: &str) -> Result<Vec<Tab>, Error> {
    let v = call(&["tab", "list", "--workspace", workspace])?;
    Ok(v["tabs"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| Tab {
                    tab_id: t["tab_id"].as_str().unwrap_or_default().to_string(),
                    label: t["label"].as_str().unwrap_or_default().to_string(),
                    pane_count: t["pane_count"].as_u64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// A new workspace at `cwd`, without moving anyone's focus, its shell given
/// `env` besides the server's: its first tab's pane.
pub(crate) fn create_workspace(cwd: &Path, label: &str, env: &[(String, String)]) -> Result<Pane, Error> {
    let cwd = cwd.to_string_lossy();
    let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut args = vec!["workspace", "create", "--cwd", &cwd, "--label", label, "--no-focus"];
    args.extend(env.iter().flat_map(|e| ["--env", e.as_str()]));
    let v = call(&args)?;
    Ok(Pane::from(&v["root_pane"]))
}

/// A new tab in `workspace` at `cwd`, without moving anyone's focus, its
/// shell given `env` besides the server's: its pane.
pub(crate) fn create_tab(workspace: &str, cwd: &Path, label: &str, env: &[(String, String)]) -> Result<Pane, Error> {
    let cwd = cwd.to_string_lossy();
    let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut args = vec!["tab", "create", "--workspace", workspace, "--cwd", &cwd, "--label", label, "--no-focus"];
    args.extend(env.iter().flat_map(|e| ["--env", e.as_str()]));
    let v = call(&args)?;
    Ok(Pane::from(&v["root_pane"]))
}

pub(crate) fn rename_tab(tab: &str, label: &str) -> Result<(), Error> {
    call(&["tab", "rename", tab, label]).map(drop)
}

/// `herdr agent start <name> --kind <kind> --pane <pane> -- <args>`: herdr
/// types the kind's executable and `args` into the pane's shell and returns
/// once the agent is detected there and ready for input. An agent stopped at
/// a dialog on its way up (a folder-trust prompt) is `agent_not_ready`.
/// Abandoned — the herdr call killed — once `cancelled` says so.
pub(crate) fn start_agent(
    name: &str,
    kind: &str,
    pane: &str,
    args: &[String],
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), Error> {
    let timeout_ms = timeout.as_millis().to_string();
    let mut argv = vec!["agent", "start", name, "--kind", kind, "--pane", pane, "--timeout", &timeout_ms, "--"];
    argv.extend(args.iter().map(String::as_str));
    let mut child = command(&argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    loop {
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::new("cancelled", "cancelled"));
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(Error::new("failed", e.to_string())),
        }
    }
    outcome(child.wait_with_output().map_err(spawn_error)?).map(drop)
}

/// What runs in `pane`.
pub(crate) fn process_info(pane: &str) -> Result<Procs, Error> {
    let v = call(&["pane", "process-info", "--pane", pane])?;
    let p = &v["process_info"];
    Ok(Procs {
        shell_pid: p["shell_pid"].as_u64().unwrap_or(0),
        fg_pgid: p["foreground_process_group_id"].as_u64().unwrap_or(0),
    })
}

/// `herdr terminal attach <terminal_id>`, to be run in the caller's own
/// terminal: shows that one terminal full-window until it detaches (`Ctrl+B
/// q`), is taken over, or the terminal goes. Never `--takeover`: a window does
/// not steal its agent back from another client that took it.
pub(crate) fn attach(terminal_id: &str) -> Command {
    let mut cmd = Command::new(bin());
    cmd.args(["terminal", "attach", terminal_id]);
    cmd
}

/// Submit `text` to the agent in `pane` as a prompt: herdr pastes it the way
/// the agent takes a paste, then Enter. Refused with `agent_blocked` when the
/// agent waits at a dialog — whatever is typed into one answers it.
pub(crate) fn prompt(pane: &str, text: &str) -> Result<(), Error> {
    call(&["agent", "prompt", pane, text]).map(drop)
}

/// Run `line` at the shell prompt in `pane` (the text, then Enter).
pub(crate) fn run_line(pane: &str, line: &str) -> Result<(), Error> {
    call(&["pane", "run", pane, line]).map(drop)
}

/// Type `text` into `pane` as a paste, without Enter.
pub(crate) fn send_text(pane: &str, text: &str) -> Result<(), Error> {
    call(&["pane", "send-text", pane, text]).map(drop)
}

/// Press `keys` in `pane`, by herdr's key names (`enter`, `esc`, `down`,
/// `ctrl+u`, `1`, …). herdr checks every name before it sends any.
pub(crate) fn send_keys(pane: &str, keys: &[String]) -> Result<(), Error> {
    let mut argv = vec!["pane", "send-keys", pane];
    argv.extend(keys.iter().map(String::as_str));
    call(&argv).map(drop)
}

/// What `pane` shows now; with `ansi`, with its styling (which is how a dim
/// placeholder is told from typed text, `crate::screen`).
pub(crate) fn read(pane: &str, ansi: bool) -> Result<String, Error> {
    let format = if ansi { "ansi" } else { "text" };
    text(&["pane", "read", pane, "--source", "visible", "--format", format])
}

/// For an agent herdr reads as blocked: the id of the detection rule that
/// matched, which names the dialog it waits at.
pub(crate) fn blocked_rule(pane: &str) -> Option<String> {
    let out = text(&["agent", "explain", pane, "--json"]).ok()?;
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    v["matched_rule"]["id"].as_str().map(str::to_string)
}

/// Close `pane` — its tab with it when it was the only one, and its
/// workspace when that was the last tab. What runs in it is hung up.
pub(crate) fn close_pane(pane: &str) -> Result<(), Error> {
    call(&["pane", "close", pane]).map(drop)
}
