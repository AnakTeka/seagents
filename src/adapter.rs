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
//! `spawn`, `send` and `stop` are its other consumers, and `[read]` names the
//! transcript format ([`crate::transcript`]) `read` and the turn ledger parse.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

/// How an adapter learns which transcript its agent is writing.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Identity {
    /// Lifecycle hooks report it (Claude's `SessionStart`).
    Hook,
    /// Not learned: state and send only. (The spec's `watch` — the first
    /// file created in a session directory — is not built; an adapter
    /// asking for it is refused by [`parse`].)
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

/// How a seed prompt reaches the agent's command line.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SeedBy {
    /// The prompt itself, in `seed_prompt`'s `{prompt}` — and so in the
    /// agent's `/proc/<pid>/cmdline`, readable by any local user while it
    /// runs, and limited to one argv string (`MAX_ARG_STRLEN`).
    #[default]
    Inline,
    /// Written to a prompt file in the agent's cwd (0600, gone with the
    /// session), with only the one line telling the agent to read it in
    /// `{prompt}` — for a tool that reads a file in its cwd without asking.
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
    /// other.
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
    pub(crate) read: Read,
    pub(crate) restore: Restore,
}

/// The lifecycle hooks `run` adds, each running `<this binary> __hook`.
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
    /// argv appended to the child's (never to the recorded argv), with
    /// `{prompt}` substituted. Empty: the agent cannot be seeded — `spawn`
    /// and `run --seed-file` refuse ([`Launch::seed_args`]); seeding by
    /// paste once idle is designed, not built.
    pub(crate) seed_prompt: Vec<String>,
    /// What goes into `{prompt}`: the prompt, or a line pointing at its file.
    pub(crate) seed_by: SeedBy,
    /// The permission modes, least privileged first — the order a child is
    /// checked against so it never outranks its parent. Named in Claude's
    /// vocabulary whatever the tool, because that is what every bundled
    /// tool's hooks report a parent's mode in. Empty: no modes.
    pub(crate) permission_modes: Vec<String>,
    /// The argv that starts the tool in each of `permission_modes` (every
    /// one, and nothing else): Claude's `--permission-mode <m>`, Codex's
    /// `-a`/`-s` pair.
    pub(crate) permission_argv: BTreeMap<String, Vec<String>>,
    pub(crate) identity: Identity,
    pub(crate) turns: Turns,
    /// Variables a running agent of this kind sets for what it runs — its
    /// own session's identity and messaging channel — which a window
    /// seagents launches must not inherit when the launch comes from inside
    /// such an agent. Names, or a prefix ending in `*`. Never the user's own
    /// configuration (Claude's `CLAUDE_CONFIG_DIR`).
    pub(crate) session_env: Vec<String>,
    /// The flag's spellings that set one configuration key on the command
    /// line, `key=value` (Codex's `-c` / `--config`); what
    /// `privileged.config_keys` is checked against.
    pub(crate) config_flags: Vec<String>,
    /// What an agent calling `spawn` may not pass through `--arg`.
    pub(crate) privileged: Privileged,
    /// When present, a running agent's mode is also read off its own argv
    /// ([`Launch::bound_mode`]), for a tool whose hooks report less than its
    /// mode (Codex's follow its approval policy alone).
    pub(crate) argv_mode: Option<ArgvMode>,
}

/// How a running agent's own command line names its mode, beyond
/// `permission_argv` read in reverse.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ArgvMode {
    /// Other spellings of the flags `permission_argv` uses (`--sandbox` →
    /// `-s`).
    pub(crate) aliases: BTreeMap<String, String>,
    /// Configuration keys (behind `config_flags`) that set what one of those
    /// flags sets (`sandbox_mode` → `-s`).
    pub(crate) config_keys: BTreeMap<String, String>,
    /// Flags that are a whole mode on their own.
    pub(crate) flags: BTreeMap<String, String>,
}

/// Arguments that would give a child more than the permission mode `spawn`
/// chose for it — refused from an agent caller, whose child's mode is
/// capped at its own ([`Launch::privileged_arg`]); a human may pass anything,
/// as they could type it into a terminal themselves.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Privileged {
    /// Flags refused whatever their value: `--flag`, `--flag=value`, and for
    /// a one-letter `-f` also `-fVALUE` and a cluster holding it (`-cp`).
    pub(crate) flags: Vec<String>,
    /// Subcommands, refused as a bare word anywhere in the arguments.
    pub(crate) commands: Vec<String>,
    /// Configuration keys refused after one of `config_flags`: the key
    /// itself, or anything under it (`sandbox_workspace_write` covers
    /// `sandbox_workspace_write.network_access`).
    pub(crate) config_keys: Vec<String>,
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
    /// The glyph the input box's first row starts with, so a `send` can see
    /// what is already typed there before pasting after it
    /// ([`crate::detect::input_box`]). Empty: the box is not looked at.
    pub(crate) input_glyph: String,
    /// Keys, in `send --keys` syntax, that clear one line of the input box;
    /// pressed until it reads empty.
    pub(crate) input_clear: String,
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
            input_glyph: String::new(),
            input_clear: String::new(),
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

/// Whether `arg` is `flag` as a command line may spell it, and the value
/// glued to it if any: `--flag` (None) or `--flag=v`; for a one-letter `-f`,
/// `-f` or `-fv`.
fn spelled<'a>(arg: &'a str, flag: &str) -> Option<Option<&'a str>> {
    if !flag.starts_with("--") && arg.starts_with("--") {
        return None;
    }
    let rest = arg.strip_prefix(flag)?;
    if rest.is_empty() {
        return Some(None);
    }
    if flag.starts_with("--") {
        return rest.strip_prefix('=').map(Some);
    }
    (flag.len() == 2).then_some(Some(rest))
}

/// The key of a `key=value` configuration override, as the tool reads it:
/// surrounding blanks and quotes dropped.
fn config_key(kv: &str) -> &str {
    kv.split('=').next().unwrap_or("").trim().trim_matches(|c| c == '"' || c == '\'')
}

/// A value as a command line or a TOML override writes it: blanks and
/// quotes dropped.
fn unquoted(v: &str) -> &str {
    v.trim().trim_matches(|c| c == '"' || c == '\'')
}

impl Launch {
    /// The mode a running agent of this kind may pass on: the one its hooks
    /// `reported`, unless its own command line (`argv`, as `.meta` records
    /// it) says it has less — the lesser of the two. Only for an adapter with
    /// `argv_mode`; the others' hooks report their mode whole. None: no mode
    /// is known (the child then gets the least privileged).
    pub(crate) fn bound_mode(&self, reported: Option<String>, argv: &[String]) -> Option<String> {
        let reported = reported?;
        let Some(ceiling) = self.argv_mode.as_ref().and_then(|am| self.argv_rank(am, argv)) else {
            return Some(reported);
        };
        match self.permission_modes.iter().position(|m| *m == reported) {
            Some(r) if r > ceiling => Some(self.permission_modes[ceiling].clone()),
            _ => Some(reported),
        }
    }

    /// The highest mode `argv` (a whole command line, the tool's name first)
    /// allows, by its permission flags; None when it has none. A flag's value
    /// ranks as the most privileged mode whose `permission_argv` uses it —
    /// the value grants no more than that mode does (Codex's `-s read-only`
    /// is `default`, since `plan` and `default` share it) — and a value no
    /// mode uses as the least privileged. The last of a repeated flag
    /// counts; every flag present bounds.
    fn argv_rank(&self, am: &ArgvMode, argv: &[String]) -> Option<usize> {
        // (flag, value) → the highest rank using it; flags that take a value.
        let mut value_rank: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        let mut valued: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (rank, mode) in self.permission_modes.iter().enumerate() {
            let words = &self.permission_argv[mode];
            for (i, w) in words.iter().enumerate() {
                if let Some(v) = words.get(i + 1).filter(|_| w.starts_with('-')).filter(|v| !v.starts_with('-')) {
                    valued.insert(w);
                    let r = value_rank.entry((w, v)).or_insert(rank);
                    *r = (*r).max(rank);
                }
            }
        }
        let canonical = |f: &str| -> Option<String> {
            let f = am.aliases.get(f).map_or(f, String::as_str);
            valued.contains(f).then(|| f.to_string())
        };
        let spellings: Vec<&str> = valued.iter().copied().chain(am.aliases.keys().map(String::as_str)).collect();
        let mut last: BTreeMap<String, String> = BTreeMap::new();
        let mut whole: Vec<usize> = Vec::new();
        let mut it = argv.iter().skip(1);
        while let Some(arg) = it.next() {
            if arg == "--" {
                break;
            }
            if let Some(mode) = am.flags.get(arg.as_str()) {
                whole.push(self.permission_modes.iter().position(|m| m == mode).unwrap_or(0));
                continue;
            }
            if let Some(glued) = self.config_flags.iter().find_map(|cf| spelled(arg, cf)) {
                let kv = match glued {
                    Some(kv) => kv.to_string(),
                    None => it.next().cloned().unwrap_or_default(),
                };
                let key = config_key(&kv);
                if let Some(flag) = am.config_keys.get(key).and_then(|f| canonical(f)) {
                    let value = kv.split_once('=').map_or("", |(_, v)| unquoted(v));
                    last.insert(flag, value.to_string());
                }
                continue;
            }
            if let Some((flag, glued)) = spellings.iter().find_map(|f| spelled(arg, f).map(|g| (*f, g))) {
                let value = match glued {
                    Some(v) => v.to_string(),
                    None => it.next().cloned().unwrap_or_default(),
                };
                if let Some(flag) = canonical(flag) {
                    last.insert(flag, unquoted(&value).to_string());
                }
            }
        }
        last.iter()
            .map(|(f, v)| value_rank.get(&(f.as_str(), v.as_str())).copied().unwrap_or(0))
            .chain(whole)
            .min()
    }

    /// The first of `args` an agent caller may not pass (`privileged`), as
    /// written — with its value, for a configuration override. Matching errs
    /// toward refusing: a one-letter flag is found inside any cluster of
    /// letters (`-cp` holds `-p`), and a refused word is refused even where
    /// it is some other flag's value.
    pub(crate) fn privileged_arg(&self, args: &[String]) -> Option<String> {
        let p = &self.privileged;
        let short_in_cluster = |arg: &str, flag: &str| {
            let letters = arg.strip_prefix('-').filter(|l| !l.starts_with('-') && l.len() > 1);
            flag.len() == 2
                && !flag.starts_with("--")
                && letters.is_some_and(|l| l.chars().all(|c| c.is_ascii_alphabetic()) && l.contains(&flag[1..]))
        };
        let mut it = args.iter().peekable();
        while let Some(arg) = it.next() {
            for cf in &self.config_flags {
                let Some(glued) = spelled(arg, cf) else { continue };
                let kv = match glued {
                    Some(kv) => kv.to_string(),
                    None => it.peek().map(|v| v.to_string()).unwrap_or_default(),
                };
                let key = config_key(&kv);
                if p.config_keys.iter().any(|k| key == k || key.strip_prefix(k.as_str()).is_some_and(|r| r.starts_with('.'))) {
                    return Some(match glued {
                        Some(_) => arg.clone(),
                        None => format!("{arg} {kv}"),
                    });
                }
            }
            if p.flags.iter().any(|f| spelled(arg, f).is_some() || short_in_cluster(arg, f))
                || p.commands.iter().any(|c| c == arg)
            {
                return Some(arg.clone());
            }
        }
        None
    }

    /// Whether `prompt` can seed this agent, as `spawn` checks before a
    /// window opens: a template to carry it and, sent inline, one argv
    /// string's worth ([`Launch::seed_args`]). By file, only the short line
    /// pointing at it goes on the command line.
    pub(crate) fn check_seed(&self, prompt: &str) -> Result<(), String> {
        match self.seed_by {
            SeedBy::File if !self.seed_prompt.is_empty() => Ok(()),
            _ => self.seed_args(prompt).map(drop),
        }
    }

    /// The argv a seed adds to the agent's command line, per `seed_prompt`:
    /// `prompt` is what goes into `{prompt}` — the prompt, or under
    /// `seed_by = "file"` the line pointing at its file. Refused — the error
    /// is for the user — when the adapter has no template, or the text cannot
    /// be one argv string: over `MAX_ARG_STRLEN`, or holding a NUL. Pasting such a seed
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
    // Options the spec designed and nothing implements: said so, rather
    // than a bare "unknown variant" — or, as when they parsed, silently
    // doing nothing.
    if let Ok(raw) = toml::from_str::<toml::Table>(text) {
        let launch = raw.get("launch").and_then(|l| l.as_table());
        if launch.and_then(|l| l.get("identity")).and_then(|i| i.as_str()) == Some("watch")
            || launch.is_some_and(|l| l.contains_key("watch_dir"))
        {
            return Err("launch.identity = \"watch\" (and watch_dir) is not built: no bundled agent needs \
                        it; use \"hook\" or \"none\""
                .to_string());
        }
    }
    let adapter: Adapter = toml::from_str(text).map_err(|e| e.to_string())?;
    let seed = &adapter.launch.seed_prompt;
    if !seed.is_empty() && !seed.iter().any(|a| a.contains("{prompt}")) {
        return Err("launch.seed_prompt has no {prompt}".to_string());
    }
    // A mode with no argv could not be launched, and argv for an unlisted
    // mode has no rank to bound a child by.
    let launch = &adapter.launch;
    let listed: std::collections::BTreeSet<&String> = launch.permission_modes.iter().collect();
    if listed.len() != launch.permission_modes.len() {
        return Err("launch.permission_modes lists a mode twice".to_string());
    }
    if !listed.iter().copied().eq(launch.permission_argv.keys()) {
        return Err("launch.permission_argv must give argv for each of permission_modes, and only those".to_string());
    }
    if let Some((mode, _)) = launch.permission_argv.iter().find(|(_, a)| !a.first().is_some_and(|f| f.starts_with('-'))) {
        return Err(format!("launch.permission_argv.{mode} does not start with a flag"));
    }
    let privileged = &launch.privileged;
    if let Some(bad) = launch.config_flags.iter().chain(&privileged.flags).find(|f| !f.starts_with('-') || f.contains('=')) {
        return Err(format!("launch: {bad:?} is not a flag"));
    }
    if !privileged.config_keys.is_empty() && launch.config_flags.is_empty() {
        return Err("launch.privileged.config_keys needs launch.config_flags".to_string());
    }
    if let Some(bad) = privileged.commands.iter().find(|c| c.is_empty() || c.starts_with('-')) {
        return Err(format!("launch.privileged.commands: {bad:?} is not a word"));
    }
    if let Some(am) = &launch.argv_mode {
        if let Some(bad) = am.flags.values().find(|m| !listed.contains(m)) {
            return Err(format!("launch.argv_mode.flags: {bad:?} is not one of permission_modes"));
        }
        if !am.config_keys.is_empty() && launch.config_flags.is_empty() {
            return Err("launch.argv_mode.config_keys needs launch.config_flags".to_string());
        }
        if let Some(bad) = am.aliases.iter().chain(&am.flags).map(|(f, _)| f).chain(am.aliases.values()).find(|f| !f.starts_with('-')) {
            return Err(format!("launch.argv_mode: {bad:?} is not a flag"));
        }
    }
    if let Some(bad) = adapter.launch.session_env.iter().find(|n| !valid_env_pattern(n)) {
        return Err(format!("launch.session_env: {bad:?} is not a variable name (or a prefix and *)"));
    }
    let hooks = &adapter.hooks;
    if hooks.inject != Inject::None && (hooks.flags.is_empty() || hooks.events.is_empty()) {
        return Err("hooks.inject needs flags and events".to_string());
    }
    if let Some(bad) = hooks.flags.iter().find(|f| !f.starts_with('-') || f.contains('=')) {
        return Err(format!("hooks.flags: {bad:?} is not a flag"));
    }
    if let Some(bad) = hooks.events.iter().find(|e| !crate::hook::KNOWN_EVENTS.contains(&e.as_str())) {
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
    if send.long_prompt == LongPrompt::File && (send.inline_max_chars == 0 || send.inline_max_lines == 0) {
        return Err("send.long_prompt = \"file\" needs inline_max_chars and inline_max_lines".to_string());
    }
    if !send.input_glyph.is_empty() {
        match crate::wrapper::inject::decode_keys(&send.input_clear) {
            Ok(keys) if !keys.is_empty() => {}
            Ok(_) => return Err("send.input_glyph needs input_clear".to_string()),
            Err(e) => return Err(format!("send.input_clear: {e}")),
        }
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
/// `session_env` names — what a window seagents launches must not inherit.
/// Every bundled adapter, not the one being launched: an agent of one kind
/// may spawn an agent of another, and it is the caller's session that
/// leaks. The bundled files, not overrides: an override written before a
/// variable was listed would otherwise quietly let it through.
pub(crate) fn session_env<'a>(env: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let patterns: Vec<String> = BUNDLED
        .iter()
        .flat_map(|(_, text)| parse(text).expect("bundled adapters parse (pinned by a test)").launch.session_env)
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
/// the agent), `read` (a transcript parser, and a way to learn which file the
/// agent writes).
pub(crate) fn capabilities(argv0: &str) -> Vec<&'static str> {
    let Ok(adapter) = for_command(argv0) else {
        return vec!["wait", "stop"];
    };
    let mut caps = Vec::new();
    if agent_id(argv0).is_some() {
        caps.push("state");
    }
    caps.push("wait");
    if adapter.launch.turns == Turns::Hook && adapter.hooks.inject != Inject::None {
        caps.push("turns");
    }
    if !adapter.launch.seed_prompt.is_empty() {
        caps.push("spawn");
    }
    if agent_id(argv0).is_some() {
        caps.push("send");
    }
    caps.push("stop");
    if adapter.read.transcript.is_some() && adapter.launch.identity != Identity::None {
        caps.push("read");
    }
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
        assert_eq!(claude.hooks.inject, Inject::Settings);
        assert_eq!(claude.hooks.flags, ["--settings"]);
        assert_eq!(claude.hooks.events, crate::hook::KNOWN_EVENTS);
        assert_eq!(claude.hooks.hookless_flags, ["--bare"]);
        assert_eq!(claude.restore.resume, ["--resume", "{session}"]);
        assert_eq!(
            claude.launch.permission_modes,
            ["plan", "default", "acceptEdits", "bypassPermissions"]
        );
        for m in &claude.launch.permission_modes {
            assert_eq!(claude.launch.permission_argv[m], ["--permission-mode", m.as_str()]);
        }
        assert_eq!((claude.launch.identity, claude.launch.turns), (Identity::Hook, Turns::Hook));
        assert_eq!(claude.read.transcript, Some(crate::transcript::Format::Claude));
        assert_eq!(claude.send.submit_delay_ms, 300);
        assert_eq!(claude.send.exit_sequence, "/exit");
        assert_eq!(
            (claude.send.long_prompt, claude.send.inline_max_chars, claude.send.inline_max_lines),
            (LongPrompt::File, 700, 2)
        );

        let codex = parse(BUNDLED[1].1).unwrap();
        assert_eq!(codex.launch.seed_prompt, ["--", "{prompt}"]);
        assert_eq!((codex.hooks.inject, &codex.hooks.flags[..]), (Inject::Config, &["-c".to_string(), "--config".to_string()][..]));
        assert_eq!(codex.hooks.events, crate::hook::KNOWN_EVENTS);
        assert_eq!((codex.launch.identity, codex.launch.turns), (Identity::Hook, Turns::Hook));
        assert_eq!(codex.send.long_prompt, LongPrompt::Paste);
        assert_eq!(codex.send.exit_sequence, "/exit");
        assert_eq!((claude.send.input_glyph.as_str(), claude.send.input_clear.as_str()), ("❯", "\\x15"));
        assert_eq!((codex.send.input_glyph.as_str(), codex.send.input_clear.as_str()), ("›", "\\x15"));
        assert_eq!(codex.read.transcript, Some(crate::transcript::Format::Codex));
        assert_eq!(codex.restore.resume, ["resume", "{session}"]);
        // Same vocabulary as Claude's, since its hooks report a parent's mode
        // in it; never `-a never` below bypass, since Codex reports that as
        // bypassPermissions whatever the sandbox.
        assert_eq!(codex.launch.permission_modes, claude.launch.permission_modes);
        let argv = |m: &str| codex.launch.permission_argv[m].join(" ");
        assert_eq!(argv("plan"), "-a on-request -s read-only");
        assert_eq!(argv("default"), "-a on-request -s read-only");
        assert_eq!(argv("acceptEdits"), "-a on-request -s workspace-write");
        assert_eq!(argv("bypassPermissions"), "-a never -s danger-full-access");
    }

    #[test]
    fn a_codex_parents_mode_is_the_lesser_of_its_hooks_and_its_command_line() {
        let codex = parse(BUNDLED[1].1).unwrap().launch;
        let bound = |reported: Option<&str>, argv: &str| {
            let argv: Vec<String> = argv.split_whitespace().map(str::to_string).collect();
            codex.bound_mode(reported.map(str::to_string), &argv)
        };
        let bypass = Some("bypassPermissions");
        // A human's `-a never -s read-only` reads as bypass to the hooks, but
        // writes nothing.
        assert_eq!(bound(bypass, "codex -a never -s read-only").as_deref(), Some("default"));
        assert_eq!(bound(bypass, "codex -s read-only -a never").as_deref(), Some("default"));
        assert_eq!(bound(bypass, "codex --sandbox=workspace-write --ask-for-approval never").as_deref(), Some("acceptEdits"));
        assert_eq!(bound(bypass, "codex -anever -sread-only").as_deref(), Some("default"));
        assert_eq!(bound(bypass, "codex -c sandbox_mode=\"read-only\"").as_deref(), Some("default"));
        assert_eq!(bound(bypass, "codex --config=sandbox_mode='workspace-write'").as_deref(), Some("acceptEdits"));
        assert_eq!(bound(bypass, "codex -c approval_policy=on-request").as_deref(), Some("acceptEdits"));
        assert_eq!(bound(bypass, "codex --full-auto").as_deref(), Some("acceptEdits"));
        // Every flag present bounds; the last of a repeated one counts.
        assert_eq!(bound(bypass, "codex -s read-only -s danger-full-access").as_deref(), Some("bypassPermissions"));
        assert_eq!(bound(bypass, "codex --dangerously-bypass-approvals-and-sandbox -s read-only").as_deref(), Some("default"));
        // What seagents itself launches a child with reads back as that mode.
        for mode in ["default", "acceptEdits", "bypassPermissions"] {
            let argv = format!("codex {}", codex.permission_argv[mode].join(" "));
            assert_eq!(bound(bypass, &argv).as_deref(), Some(mode), "{argv}");
        }
        // The command line never raises the hooks' word, nor invents one.
        assert_eq!(bound(Some("default"), "codex -a never -s danger-full-access").as_deref(), Some("default"));
        assert_eq!(bound(None, "codex -a never -s danger-full-access"), None);
        // No permission flags: the hooks' word stands. A value no mode uses
        // (an older `-a untrusted`) is the least privileged.
        assert_eq!(bound(bypass, "codex -m o3 -c model=x").as_deref(), bypass);
        assert_eq!(bound(bypass, "codex -a untrusted").as_deref(), Some("plan"));
        // After `--` is the prompt.
        assert_eq!(bound(bypass, "codex -- -s read-only").as_deref(), bypass);
        // Claude's hooks report its mode whole (and current), so its argv is
        // not read.
        let claude = parse(BUNDLED[0].1).unwrap().launch;
        let argv: Vec<String> = ["claude", "--permission-mode", "plan"].map(String::from).to_vec();
        assert_eq!(claude.bound_mode(Some("acceptEdits".into()), &argv).as_deref(), Some("acceptEdits"));
    }

    #[test]
    fn argv_mode_names_listed_modes_and_flags() {
        let base = "[launch]\npermission_modes = [\"a\"]\n[launch.permission_argv]\na = [\"-m\", \"a\"]\n[launch.argv_mode]\n";
        assert!(parse(&format!("{base}flags = {{ \"--all\" = \"a\" }}\n")).is_ok());
        assert!(parse(&format!("{base}flags = {{ \"--all\" = \"z\" }}\n")).unwrap_err().contains("permission_modes"));
        assert!(parse(&format!("{base}aliases = {{ \"--mode\" = \"m\" }}\n")).unwrap_err().contains("not a flag"));
        assert!(parse(&format!("{base}config_keys = {{ mode = \"-m\" }}\n")).unwrap_err().contains("config_flags"));
    }

    #[test]
    fn permission_argv_must_cover_exactly_the_modes() {
        let with = |launch: &str| parse(&format!("[launch]\n{launch}"));
        assert!(with("permission_modes = [\"a\", \"b\"]\n[launch.permission_argv]\na = [\"-m\", \"a\"]\nb = [\"-m\", \"b\"]").is_ok());
        assert!(with("permission_modes = [\"a\", \"b\"]\n[launch.permission_argv]\na = [\"-m\", \"a\"]")
            .unwrap_err()
            .contains("each of"));
        assert!(with("permission_modes = [\"a\"]\n[launch.permission_argv]\na = [\"-m\"]\nz = [\"-z\"]").is_err());
        assert!(with("[launch.permission_argv]\na = [\"-m\"]").is_err());
        assert!(with("permission_modes = [\"a\", \"a\"]\n[launch.permission_argv]\na = [\"-m\"]").unwrap_err().contains("twice"));
        assert!(with("permission_modes = [\"a\"]\n[launch.permission_argv]\na = [\"a\"]").unwrap_err().contains("flag"));
        assert!(with("permission_modes = [\"a\"]\n[launch.permission_argv]\na = []").is_err());
    }

    #[test]
    fn a_launch_strips_the_calling_sessions_variables_and_nothing_else() {
        let env = [
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ATTENDED",
            "CLAUDE_PID",
            "CLAUDE_EFFORT",
            "AI_AGENT",
            // The user's configuration stays.
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "HOME",
            "PATH",
        ];
        let mut want = env[..11].to_vec();
        want.sort();
        assert_eq!(session_env(env), want);
        assert!(session_env(["HOME", "TERM"]).is_empty());
    }

    #[test]
    fn session_env_takes_names_or_a_prefix_glob() {
        assert!(parse("[launch]\nsession_env = [\"FOO_BAR\", \"X_*\", \"_Y1\"]\n").is_ok());
        for bad in ["", "*", "1X", "A-B", "A B", "A*B", "$(rm)"] {
            let text = format!("[launch]\nsession_env = [{bad:?}]\n");
            assert!(parse(&text).unwrap_err().contains("session_env"), "{bad}");
        }
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
        assert_eq!(capabilities("claude"), ["state", "wait", "turns", "spawn", "send", "stop", "read"]);
        assert_eq!(capabilities("/usr/bin/codex"), ["state", "wait", "turns", "spawn", "send", "stop", "read"]);
        assert_eq!(capabilities("gemini"), ["state", "wait", "send", "stop"]);
        assert_eq!(capabilities("sleep"), ["wait", "stop"]);
    }

    #[test]
    fn a_seed_becomes_the_templates_argv_or_a_clear_refusal() {
        let claude = parse(BUNDLED[0].1).unwrap();
        assert_eq!(claude.launch.seed_args("-rf? it's fine").unwrap(), ["--", "-rf? it's fine"]);
        let codex = parse(BUNDLED[1].1).unwrap();
        assert_eq!(codex.launch.seed_args("-go").unwrap(), ["--", "-go"]);

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
        assert_eq!(a.read, Read::default());
        assert!(parse("[read]\ntranscript = \"hieroglyphs\"\n").is_err());
        assert!(parse("[launch]\nidentity = \"telepathy\"\n").is_err());
        assert!(parse("[launch]\nseed_flag = \"-p\"\n").is_err(), "typos are errors");
        assert!(parse("[launch]\nseed_prompt = [\"-p\"]\n").unwrap_err().contains("{prompt}"));
        assert!(parse("[hooks]\ninject = \"settings\"\n").unwrap_err().contains("flags"));
        let hooks = "[hooks]\ninject = \"config\"\nflags = [\"-c\"]\n";
        assert!(parse(&format!("{hooks}events = [\"Stop\"]\n")).is_ok());
        assert!(parse(&format!("{hooks}events = [\"Interrupt\"]\n")).unwrap_err().contains("Interrupt"));
        assert!(parse("[hooks]\nflags = [\"c\"]\n").unwrap_err().contains("not a flag"));
        assert!(parse("[restore]\nresume = [\"--resume\"]\n").unwrap_err().contains("{session}"));
        assert!(parse("[send]\ninput_glyph = \">\"\n").unwrap_err().contains("input_clear"));
        assert!(parse("[send]\ninput_glyph = \">\"\ninput_clear = \"\\\\q\"\n").unwrap_err().contains("input_clear"));
        assert!(parse("[send]\ninput_glyph = \">\"\ninput_clear = \"\\\\x15\"\n").is_ok());
        assert!(parse("[launch.privileged]\nflags = [\"x\"]\n").unwrap_err().contains("not a flag"));
        assert!(parse("[launch.privileged]\ncommands = [\"-x\"]\n").unwrap_err().contains("not a word"));
        assert!(parse("[launch.privileged]\nconfig_keys = [\"k\"]\n").unwrap_err().contains("config_flags"));
        // The designed-but-unbuilt watch identity is refused by name, not
        // accepted to do nothing.
        for text in ["[launch]\nidentity = \"watch\"\n", "[launch]\nwatch_dir = \"/x\"\n", "[launch]\nwatch_dir = \"\"\n"] {
            assert!(parse(text).unwrap_err().contains("not built"), "{text}");
        }
        assert!(parse("[launch]\nconfig_flags = [\"-c\"]\n[launch.privileged]\nconfig_keys = [\"k\"]\n").is_ok());
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
