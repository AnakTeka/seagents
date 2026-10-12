//! Agent adapters: everything orchestration needs to know about one specific
//! tool, as data.
//!
//! The core (tags, the launcher, the turn ledger, `wait`, `send`) is written
//! once; whether a tool has hooks, how its permission modes are spelled, how
//! its input box looks and how it is asked to exit live here, one TOML file
//! per agent. Claude Code is the first adapter, not the design.
//!
//! Keyed by the herdr agent kind of `argv[0]` (`herdr::kind_for`), so herdr's
//! detection and seagents' orchestration always agree on what a session is.
//! Lookup, strongest first:
//!
//! 1. `~/.config/seagents/agents/<agent>.toml` — the user's override, replacing
//!    the bundled file whole;
//! 2. the bundled file compiled in from `src/adapters/`;
//! 3. [`Adapter::default`] — the generic adapter for any other herdr kind: no
//!    hooks, so no conversation identity and no turn ledger; its state is
//!    herdr's reading of the screen alone.
//!
//! `run` reads it to decide whether to inject hooks ([`crate::hook`]);
//! `spawn`, `send` and `stop` are its other consumers, and `[read]` names the
//! transcript format ([`crate::transcript`]) `read` and the turn ledger parse.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

/// How a `send` body too long to pass as the user's own words is delivered.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LongPrompt {
    /// Pasted like any other: the tool takes every paste as typed text.
    #[default]
    Paste,
    /// Written to a prompt file in the agent's cwd, and a one-line
    /// instruction to read it is pasted instead.
    File,
}

/// How a tool takes the hooks seagents adds to its command line — the one
/// thing about hook injection that differs between tools; finding the
/// user's own hook configuration, merging ours into it and passing ONE flag
/// back is the same for all ([`crate::hook::inject`]).
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Inject {
    /// No hook support: nothing is injected.
    #[default]
    None,
    /// A settings JSON object whose `hooks` holds the event arrays, inline
    /// or as a path (Claude's `--settings`). Repeated flags replace each
    /// other. seagents passes it as a path, to a file of its own.
    Settings,
    /// A config override `hooks=<TOML>` (Codex's `-c`): every override of
    /// `hooks` or `hooks.*` contributes, and each hook we add is marked
    /// trusted in the same table, since Codex runs no hook it has not been
    /// told to trust.
    Config,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Adapter {
    pub(crate) launch: Launch,
    pub(crate) hooks: Hooks,
    pub(crate) send: Send,
    pub(crate) stop: Stop,
    pub(crate) screen: Screen,
    pub(crate) read: Read,
    pub(crate) restore: Restore,
}

/// The lifecycle hooks `run` adds, each running `<this binary> __hook`. An
/// adapter with hooks (`inject` other than `none`) is one whose agent reports
/// which conversation it is writing and keeps the exact turn ledger
/// ([`Hooks::enabled`]); one without has neither.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Hooks {
    pub(crate) inject: Inject,
    /// The flag's spellings, the first being the one passed; the others are
    /// how a user's own may be written (`-c` / `--config`).
    pub(crate) flags: Vec<String>,
    /// The events hooked, by the tool's names. `__hook` understands
    /// `SessionStart`, `UserPromptSubmit` and `Stop` payloads in the shape
    /// both Claude Code and Codex send.
    pub(crate) events: Vec<String>,
    /// Flags under which the tool skips hooks altogether (Claude's `--bare`):
    /// nothing is injected, and `.agent` stays empty.
    pub(crate) hookless_flags: Vec<String>,
}

impl Hooks {
    /// Whether this adapter's agents get our hooks — and so report their
    /// conversation and keep the exact turn ledger.
    pub(crate) fn enabled(&self) -> bool {
        self.inject != Inject::None
    }
}

/// How `restore` puts a window back on the conversation it was showing.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Restore {
    /// argv inserted right after the command, `{session}` the conversation
    /// id the hooks reported. Empty: the window is relaunched on a fresh
    /// conversation.
    pub(crate) resume: Vec<String>,
    /// Words in the recorded argv that picked a conversation at launch
    /// (each dropped with the id or `--last` after it), so the captured one
    /// replaces them rather than fighting them.
    pub(crate) drop: Vec<String>,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Read {
    /// The format of the transcript the agent writes — which parser reads
    /// it. None: seagents cannot read this agent's conversation (and a hook
    /// turn ledger has no interrupt record to go by).
    pub(crate) transcript: Option<crate::transcript::Format>,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Launch {
    /// The permission modes `spawn --permission-mode` takes, in one
    /// vocabulary (Claude's) whatever the tool, and the argv that starts the
    /// tool in each: Claude's `--permission-mode <m>`, Codex's `-a`/`-s`
    /// pair. Empty: no modes.
    pub(crate) permission_argv: BTreeMap<String, Vec<String>>,
    /// Variables a running agent of this kind sets for what it runs — its
    /// own session's identity and messaging channel — which a window
    /// seagents launches, and a herdr server it starts, must not inherit when
    /// the launch comes from inside such an agent. Names, or a prefix ending
    /// in `*`. Never the user's own configuration (Claude's
    /// `CLAUDE_CONFIG_DIR`).
    pub(crate) session_env: Vec<String>,
    /// Variables of `run`'s own environment the agent's pane is given, when
    /// set: where the tool keeps its configuration (`CLAUDE_CONFIG_DIR`,
    /// `CODEX_HOME`). The agent runs under the herdr server's environment,
    /// not `run`'s, so without this `CODEX_HOME=… seagents run codex` would
    /// not reach Codex. Paths only, never secrets: they travel on herdr's
    /// command line, which `/proc` shows every local user.
    pub(crate) pass_env: Vec<String>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Send {
    /// What happens to a body over `inline_max_chars` characters or
    /// `inline_max_lines` lines. A tool that files a large paste as quoted
    /// data rather than as the user's words (Claude) takes it by reference.
    pub(crate) long_prompt: LongPrompt,
    pub(crate) inline_max_chars: usize,
    pub(crate) inline_max_lines: usize,
    /// The glyph the input box's first row starts with, so a `send` can see
    /// what is already typed there before pasting after it
    /// ([`crate::screen::input_box`]). Empty: the box is not looked at.
    pub(crate) input_glyph: String,
    /// Keys, by herdr's names, that clear one line of the input box;
    /// pressed until it reads empty.
    pub(crate) input_clear: Vec<String>,
}

impl Default for Send {
    fn default() -> Send {
        Send {
            long_prompt: LongPrompt::Paste,
            inline_max_chars: 0,
            inline_max_lines: 0,
            input_glyph: String::new(),
            input_clear: Vec::new(),
        }
    }
}

impl Send {
    /// Whether `body` goes by reference rather than pasted: only for a
    /// `long_prompt = "file"` adapter, and only past either limit. Lines are
    /// counted as the paste carries them, so a trailing newline is one more.
    pub(crate) fn by_reference(&self, body: &str) -> bool {
        self.long_prompt == LongPrompt::File
            && (body.chars().count() > self.inline_max_chars
                || body.split('\n').count() > self.inline_max_lines)
    }
}

/// How `stop` asks the agent to go.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Stop {
    /// Submitted as a prompt to ask the agent to exit; empty goes straight
    /// to closing its pane.
    pub(crate) exit_sequence: String,
}

/// What seagents reads off the agent's screen itself.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Screen {
    /// Screen text that means the agent waits at a dialog herdr does not
    /// read as one (Codex's folder-trust question drawn over an update
    /// notice). While one shows the agent is blocked, to seagents, whatever
    /// herdr says: nothing is pasted or typed into it — its Enter would
    /// answer it (and trust the folder) — `wait` returns `blocked`, and
    /// `stop` closes the pane rather than type its exit sequence.
    pub(crate) dialogs: Vec<String>,
}

/// The adapters compiled into the binary, by agent kind.
const BUNDLED: &[(&str, &str)] = &[
    ("claude", include_str!("adapters/claude.toml")),
    ("codex", include_str!("adapters/codex.toml")),
];

/// Parse one adapter file, refusing what could only half work.
fn parse(text: &str) -> Result<Adapter, String> {
    let adapter: Adapter = toml::from_str(text).map_err(|e| e.to_string())?;
    let launch = &adapter.launch;
    if let Some((mode, _)) = launch
        .permission_argv
        .iter()
        .find(|(_, a)| !a.first().is_some_and(|f| f.starts_with('-')))
    {
        return Err(format!(
            "launch.permission_argv.{mode} does not start with a flag"
        ));
    }
    if let Some(bad) = adapter
        .launch
        .session_env
        .iter()
        .find(|n| !valid_env_pattern(n))
    {
        return Err(format!(
            "launch.session_env: {bad:?} is not a variable name (or a prefix and *)"
        ));
    }
    if let Some(bad) = adapter
        .launch
        .pass_env
        .iter()
        .find(|n| n.ends_with('*') || !valid_env_pattern(n))
    {
        return Err(format!("launch.pass_env: {bad:?} is not a variable name"));
    }
    let hooks = &adapter.hooks;
    if hooks.enabled() && (hooks.flags.is_empty() || hooks.events.is_empty()) {
        return Err("hooks.inject needs flags and events".to_string());
    }
    if let Some(bad) = hooks
        .flags
        .iter()
        .find(|f| !f.starts_with('-') || f.contains('='))
    {
        return Err(format!("hooks.flags: {bad:?} is not a flag"));
    }
    if let Some(bad) = hooks
        .events
        .iter()
        .find(|e| !crate::hook::KNOWN_EVENTS.contains(&e.as_str()))
    {
        return Err(format!(
            "hooks.events: seagents records {}, not {bad:?}",
            crate::hook::KNOWN_EVENTS.join(", ")
        ));
    }
    let resume = &adapter.restore.resume;
    if !resume.is_empty() && !resume.iter().any(|a| a.contains("{session}")) {
        return Err("restore.resume has no {session}".to_string());
    }
    let send = &adapter.send;
    if send.long_prompt == LongPrompt::File
        && (send.inline_max_chars == 0 || send.inline_max_lines == 0)
    {
        return Err(
            "send.long_prompt = \"file\" needs inline_max_chars and inline_max_lines".to_string(),
        );
    }
    if !send.input_glyph.is_empty() && send.input_clear.is_empty() {
        return Err("send.input_glyph needs input_clear".to_string());
    }
    Ok(adapter)
}

/// `NAME` or `PREFIX*`, `[A-Za-z_][A-Za-z0-9_]*` — safe to write unquoted
/// into a shell's `unset`.
fn valid_env_pattern(p: &str) -> bool {
    let name = p.strip_suffix('*').unwrap_or(p);
    let b = name.as_bytes();
    !b.is_empty()
        && !b[0].is_ascii_digit()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// Of the variables `env` holds, those every bundled adapter's
/// `session_env` names — what a window seagents launches, or a herdr server
/// it starts, must not inherit. Every bundled adapter, not the one being
/// launched: an agent of one kind may spawn an agent of another, and it is
/// the caller's session that leaks. The bundled files, not overrides: an
/// override written before a variable was listed would otherwise quietly let
/// it through.
pub(crate) fn session_env<'a>(env: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let patterns: Vec<String> = BUNDLED
        .iter()
        .flat_map(|(_, text)| {
            parse(text)
                .expect("bundled adapters parse")
                .launch
                .session_env
        })
        .collect();
    let mut names: Vec<String> = env
        .into_iter()
        .filter(|name| {
            patterns.iter().any(|p| match p.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == p,
            })
        })
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// [`session_env`] of this process's own environment.
pub(crate) fn own_session_env() -> Vec<String> {
    let vars: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();
    session_env(vars.iter().map(String::as_str))
}

/// `~/.config/seagents/agents/<agent>.toml`.
fn override_path(agent: &str) -> PathBuf {
    crate::fsutil::config_dir()
        .join("agents")
        .join(format!("{agent}.toml"))
}

/// The adapter for the command whose first word is `argv0` (a path or a bare
/// name). An override that exists but does not parse is an error rather than
/// a silent fallback: the user wrote it to change how the agent is driven,
/// and driving it the default way instead is the surprise to avoid.
pub(crate) fn for_command(argv0: &str) -> Result<Adapter, String> {
    match crate::herdr::kind_for(argv0) {
        Some(agent) => for_agent(agent),
        None => Ok(Adapter::default()),
    }
}

/// What seagents can do with an agent started as `argv0`, as `status --json`
/// reports it so a caller driving a mixed fleet knows — derived from the
/// adapter, never stored: `state`, `send`, `spawn` and `stop` (herdr starts,
/// reads and drives every kind it knows), `wait` (always: exit and dialogs
/// need nothing from the agent), `turns` (an exact turn ledger, from hooks),
/// `read` (a transcript parser, and a way to learn which file the agent
/// writes).
pub(crate) fn capabilities(argv0: &str) -> Vec<&'static str> {
    let known = crate::herdr::kind_for(argv0).is_some();
    let Ok(adapter) = for_command(argv0) else {
        return vec!["wait", "stop"];
    };
    let mut caps = Vec::new();
    if known {
        caps.push("state");
    }
    caps.push("wait");
    if adapter.hooks.enabled() {
        caps.push("turns");
    }
    if known {
        caps.extend(["spawn", "send"]);
    }
    caps.push("stop");
    if adapter.read.transcript.is_some() && adapter.hooks.enabled() {
        caps.push("read");
    }
    caps
}

/// The adapter for herdr agent kind `agent`.
pub(crate) fn for_agent(agent: &str) -> Result<Adapter, String> {
    let path = override_path(agent);
    match std::fs::read_to_string(&path) {
        Ok(text) => return parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("{}: {e}", path.display()));
        }
        Err(_) => {}
    }
    Ok(match BUNDLED.iter().find(|(id, _)| *id == agent) {
        Some((_, text)) => parse(text).expect("bundled adapters parse"),
        None => Adapter::default(),
    })
}
