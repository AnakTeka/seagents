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

// The seed, permission and send facts have no reader until spawn/send land.
#![cfg_attr(not(test), allow(dead_code))]

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
}

impl Default for Send {
    fn default() -> Send {
        Send {
            submit_delay_ms: 300,
            pre_send: String::new(),
            exit_sequence: String::new(),
        }
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
    match crate::vendor::herdr_detect::identify_agent(argv0) {
        Some(agent) => for_agent(crate::vendor::herdr_detect::agent_label(agent)),
        None => Ok(Adapter::default()),
    }
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

        let codex = parse(BUNDLED[1].1).unwrap();
        assert_eq!(codex.launch.seed_prompt, ["{prompt}"]);
        assert_eq!(codex.launch.settings_flag, "");
        assert_eq!((codex.launch.identity, codex.launch.turns), (Identity::None, Turns::Screen));
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
