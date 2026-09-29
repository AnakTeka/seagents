//! Agent adapters: everything orchestration needs to know about one specific
//! tool, as data.
//!
//! The core (tags, the launcher, the turn ledger, `wait`, `send`) is written
//! once; how a tool takes a seed prompt, whether it has hooks, what its
//! permission modes are and how it is asked to exit live here, one TOML file
//! per agent. Claude Code is the first adapter, not the design.
//!
//! Keyed by the id the vendored detection engine derives from `argv[0]`
//! (`identify_agent` → `agent_label`), so detection and orchestration always
//! agree on what a session is. Lookup, strongest first:
//!
//! 1. `~/.config/seagents/agents/<agent>.toml` — the user's override, replacing
//!    the bundled file whole (as `agent-detection/<agent>.toml` does for
//!    detection manifests);
//! 2. the bundled file compiled in from `src/adapters/`;
//! 3. [`Adapter::default`] — the generic adapter for any other command: no
//!    seed flag, no hooks, no identity; state from the screen only.
//!
//! `run` reads it to decide whether to inject hooks ([`crate::hook`]);
//! `spawn`, `send` and `stop` are its other consumers.

use std::path::PathBuf;

use serde::Deserialize;

/// How an adapter learns which transcript its agent is writing.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Identity {
    /// Lifecycle hooks report it (Claude's `SessionStart`).
    Hook,
    /// The first file created in `watch_dir` after launch.
    Watch,
    /// Not learned: state and send only.
    #[default]
    None,
}

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

/// Where the turn ledger's edges come from.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Turns {
    /// Exact, from prompt-submit / stop hooks.
    Hook,
    /// Approximate, from the detector seeing the agent go idle.
    #[default]
    Screen,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Adapter {
    pub(crate) launch: Launch,
    pub(crate) send: Send,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Launch {
    /// argv appended to the child's (never to the recorded argv), with
    /// `{prompt}` substituted. Empty: the prompt is pasted instead, once the
    /// agent first reaches idle.
    pub(crate) seed_prompt: Vec<String>,
    /// The flag that takes a settings JSON carrying our hooks; empty when the
    /// tool has no hook support.
    pub(crate) settings_flag: String,
    /// Flags under which the tool skips hooks altogether (Claude's `--bare`):
    /// nothing is injected, and `.agent` stays empty.
    pub(crate) hookless_flags: Vec<String>,
    /// The flag that sets a permission mode; empty when there is none.
    pub(crate) permission_flag: String,
    /// The modes that flag takes, least privileged first — the order a child
    /// is checked against so it never outranks its parent.
    pub(crate) permission_modes: Vec<String>,
    pub(crate) identity: Identity,
    pub(crate) turns: Turns,
    /// For `identity = "watch"`.
    pub(crate) watch_dir: String,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Send {
    /// Body, then this long, then the submit key: the TUI has to consume a
    /// paste before Enter arrives (herdr's value).
    pub(crate) submit_delay_ms: u64,
    /// A nudge written before the body (e.g. `focus_gained` for Copilot);
    /// empty for none.
    pub(crate) pre_send: String,
    /// Sent as a prompt to ask the agent to exit; empty goes straight to
    /// SIGTERM.
    pub(crate) exit_sequence: String,
    /// What happens to a body over `inline_max_chars` characters or
    /// `inline_max_lines` lines. A tool that files a large paste as quoted
    /// data rather than as the user's words (Claude) takes it by reference.
    pub(crate) long_prompt: LongPrompt,
    pub(crate) inline_max_chars: usize,
    pub(crate) inline_max_lines: usize,
}

impl Default for Send {
    fn default() -> Send {
        Send {
            submit_delay_ms: 300,
            pre_send: String::new(),
            exit_sequence: String::new(),
            long_prompt: LongPrompt::Paste,
            inline_max_chars: 0,
            inline_max_lines: 0,
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

/// The longest single argv string the kernel accepts (`MAX_ARG_STRLEN`, 32
/// pages), terminating NUL included.
const MAX_ARG_STRLEN: usize = 128 * 1024;

impl Launch {
    /// The argv a seed prompt adds to the agent's command line, per
    /// `seed_prompt`. Refused — the error is for the user — when the adapter
    /// seeds by pasting (an empty template) or the prompt cannot be one argv
    /// string: over `MAX_ARG_STRLEN`, or holding a NUL. Pasting such a seed
    /// through the wrapper's own PTY writer is the spec's answer to both; it
    /// is not built yet, as no bundled adapter needs it.
    pub(crate) fn seed_args(&self, prompt: &str) -> Result<Vec<String>, String> {
        if self.seed_prompt.is_empty() {
            return Err("this agent's adapter has no launch.seed_prompt; seeding by paste is not \
                        implemented yet"
                .to_string());
        }
        if prompt.contains('\0') {
            return Err("the prompt contains a NUL byte".to_string());
        }
        let args: Vec<String> = self
            .seed_prompt
            .iter()
            .map(|a| a.replace("{prompt}", prompt))
            .collect();
        if let Some(long) = args.iter().find(|a| a.len() >= MAX_ARG_STRLEN) {
            return Err(format!(
                "the prompt is too long to pass on the command line ({} bytes; the limit is {}); \
                 seeding by paste is not implemented yet",
                long.len(),
                MAX_ARG_STRLEN - 1
            ));
        }
        Ok(args)
    }
}

/// The adapters compiled into the binary, by agent id.
const BUNDLED: &[(&str, &str)] = &[
    ("claude", include_str!("adapters/claude.toml")),
    ("codex", include_str!("adapters/codex.toml")),
];

/// Parse one adapter file. Refuses a seed template with nowhere to put the
/// prompt, which would otherwise launch the agent with the prompt dropped.
fn parse(text: &str) -> Result<Adapter, String> {
    let adapter: Adapter = toml::from_str(text).map_err(|e| e.to_string())?;
    let seed = &adapter.launch.seed_prompt;
    if !seed.is_empty() && !seed.iter().any(|a| a.contains("{prompt}")) {
        return Err("launch.seed_prompt has no {prompt}".to_string());
    }
    let send = &adapter.send;
    if send.long_prompt == LongPrompt::File && (send.inline_max_chars == 0 || send.inline_max_lines == 0) {
        return Err("send.long_prompt = \"file\" needs inline_max_chars and inline_max_lines".to_string());
    }
    Ok(adapter)
}

/// `~/.config/seagents/agents/<agent>.toml`.
fn override_path(agent: &str) -> PathBuf {
    crate::vendor::herdr_detect::config::config_dir()
        .join("agents")
        .join(format!("{agent}.toml"))
}

/// The adapter for the command whose first word is `argv0` (a path or a bare
/// name). An override that exists but does not parse is an error rather than
/// a silent fallback: the user wrote it to change how the agent is driven,
/// and driving it the default way instead is the surprise to avoid.
pub(crate) fn for_command(argv0: &str) -> Result<Adapter, String> {
    match agent_id(argv0) {
        Some(agent) => for_agent(agent),
        None => Ok(Adapter::default()),
    }
}

/// What seagents can do with an agent started as `argv0`, as `status --json`
/// reports it so a caller driving a mixed fleet knows — derived from the
/// adapter, never stored: `state` (the detector knows the agent), `wait`
/// (always: exit and gates need nothing from the agent), `turns` (an exact
/// turn ledger, from hooks), `spawn` (a seed prompt can go on its command
/// line), `send` (the default submit-when-idle needs a detector to say idle;
/// any agent takes `send --now`), `stop` (always: signals need nothing from
/// the agent). `read` joins in Phase 3.
pub(crate) fn capabilities(argv0: &str) -> Vec<&'static str> {
    let Ok(adapter) = for_command(argv0) else {
        return vec!["wait", "stop"];
    };
    let mut caps = Vec::new();
    if agent_id(argv0).is_some() {
        caps.push("state");
    }
    caps.push("wait");
    if adapter.launch.turns == Turns::Hook && !adapter.launch.settings_flag.is_empty() {
        caps.push("turns");
    }
    if !adapter.launch.seed_prompt.is_empty() {
        caps.push("spawn");
    }
    if agent_id(argv0).is_some() {
        caps.push("send");
    }
    caps.push("stop");
    caps
}

/// The agent id for the command whose first word is `argv0` — the key of its
/// adapter, detection manifest and transcript parser. None for a command no
/// detection manifest knows.
pub(crate) fn agent_id(argv0: &str) -> Option<&'static str> {
    crate::vendor::herdr_detect::identify_agent(argv0).map(crate::vendor::herdr_detect::agent_label)
}

/// The adapter for agent id `agent` (as `agent_label` spells it).
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
        Some((_, text)) => parse(text).expect("bundled adapters parse (pinned by a test)"),
        None => Adapter::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_adapters_parse_and_say_what_the_spec_says() {
        for (id, text) in BUNDLED {
            parse(text).unwrap_or_else(|e| panic!("{id}.toml: {e}"));
        }
        let claude = parse(BUNDLED[0].1).unwrap();
        assert_eq!(claude.launch.seed_prompt, ["--", "{prompt}"]);
        assert_eq!(claude.launch.settings_flag, "--settings");
        assert_eq!(claude.launch.hookless_flags, ["--bare"]);
        assert_eq!(claude.launch.permission_flag, "--permission-mode");
        assert_eq!(
            claude.launch.permission_modes,
            ["plan", "default", "acceptEdits", "bypassPermissions"]
        );
        assert_eq!((claude.launch.identity, claude.launch.turns), (Identity::Hook, Turns::Hook));
        assert_eq!(claude.send.submit_delay_ms, 300);
        assert_eq!(claude.send.exit_sequence, "/exit");
        assert_eq!(
            (claude.send.long_prompt, claude.send.inline_max_chars, claude.send.inline_max_lines),
            (LongPrompt::File, 700, 2)
        );

        let codex = parse(BUNDLED[1].1).unwrap();
        assert_eq!(codex.launch.seed_prompt, ["{prompt}"]);
        assert_eq!(codex.launch.settings_flag, "");
        assert_eq!((codex.launch.identity, codex.launch.turns), (Identity::None, Turns::Screen));
        assert_eq!(codex.send.long_prompt, LongPrompt::Paste);
    }

    #[test]
    fn a_body_past_either_measured_limit_goes_by_reference() {
        let claude = parse(BUNDLED[0].1).unwrap().send;
        assert!(!claude.by_reference("Reply with exactly: ONE"));
        assert!(!claude.by_reference(&"x".repeat(700)));
        assert!(claude.by_reference(&"x".repeat(701)));
        assert!(!claude.by_reference(&"é".repeat(700)), "characters, not bytes");
        assert!(!claude.by_reference("one\ntwo"));
        assert!(claude.by_reference("one\ntwo\nthree"));
        assert!(claude.by_reference("one\ntwo\n"), "the trailing newline is pasted too");
        // Every other adapter pastes whatever it is given.
        let codex = parse(BUNDLED[1].1).unwrap().send;
        assert!(!codex.by_reference(&"line\n".repeat(500)));
        assert!(!Send::default().by_reference(&"x".repeat(10_000)));
        assert!(parse("[send]\nlong_prompt = \"file\"\n").unwrap_err().contains("inline_max"));
    }

    #[test]
    fn capabilities_follow_the_adapter() {
        let _env = crate::vendor::herdr_detect::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(capabilities("claude"), ["state", "wait", "turns", "spawn", "send", "stop"]);
        assert_eq!(capabilities("/usr/bin/codex"), ["state", "wait", "spawn", "send", "stop"]);
        assert_eq!(capabilities("gemini"), ["state", "wait", "send", "stop"]);
        assert_eq!(capabilities("sleep"), ["wait", "stop"]);
    }

    #[test]
    fn a_seed_becomes_the_templates_argv_or_a_clear_refusal() {
        let claude = parse(BUNDLED[0].1).unwrap();
        assert_eq!(claude.launch.seed_args("-rf? it's fine").unwrap(), ["--", "-rf? it's fine"]);
        let codex = parse(BUNDLED[1].1).unwrap();
        assert_eq!(codex.launch.seed_args("go").unwrap(), ["go"]);

        assert!(Adapter::default().launch.seed_args("go").unwrap_err().contains("seed_prompt"));
        assert!(claude.launch.seed_args("a\0b").unwrap_err().contains("NUL"));
        let limit = MAX_ARG_STRLEN - 1;
        assert!(claude.launch.seed_args(&"x".repeat(limit)).is_ok());
        assert!(claude.launch.seed_args(&"x".repeat(limit + 1)).unwrap_err().contains("too long"));
        // The template's own text counts toward the one string.
        let wrapped = parse("[launch]\nseed_prompt = [\"--prompt={prompt}\"]\n").unwrap();
        assert!(wrapped.launch.seed_args(&"x".repeat(limit - 5)).is_err());
    }

    #[test]
    fn a_partial_file_takes_generic_defaults_and_bad_files_are_refused() {
        let a = parse("[send]\nexit_sequence = \"/quit\"\n").unwrap();
        assert_eq!(a.send.exit_sequence, "/quit");
        assert_eq!(a.send.submit_delay_ms, 300);
        assert_eq!(a.launch, Launch::default());
        assert!(parse("[launch]\nidentity = \"telepathy\"\n").is_err());
        assert!(parse("[launch]\nseed_flag = \"-p\"\n").is_err(), "typos are errors");
        assert!(parse("[launch]\nseed_prompt = [\"-p\"]\n").unwrap_err().contains("{prompt}"));
    }

    #[test]
    fn overrides_replace_the_bundled_file_and_unknown_agents_get_the_generic_one() {
        let _env = crate::vendor::herdr_detect::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("seagents-adapter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("seagents/agents")).unwrap();
        let old = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        // No override: bundled, keyed by the detection id of argv[0].
        assert_eq!(for_command("/usr/local/bin/claude").unwrap().launch.identity, Identity::Hook);
        assert_eq!(for_command("claude-code").unwrap().send.exit_sequence, "/exit");
        // Recognised by detection but no bundled adapter, or not an agent at
        // all: the generic, state-only adapter.
        assert_eq!(for_command("gemini").unwrap(), Adapter::default());
        assert_eq!(for_command("bash").unwrap(), Adapter::default());

        // An override replaces the bundled file whole.
        std::fs::write(
            dir.join("seagents/agents/claude.toml"),
            "[send]\nsubmit_delay_ms = 50\n",
        )
        .unwrap();
        let claude = for_agent("claude").unwrap();
        assert_eq!(claude.send.submit_delay_ms, 50);
        assert_eq!(claude.launch.identity, Identity::None, "whole-file override");

        // A broken override is an error naming the file, not a silent default.
        std::fs::write(dir.join("seagents/agents/claude.toml"), "[launch\n").unwrap();
        assert!(for_agent("claude").unwrap_err().contains("claude.toml"));

        match old {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
