//! The agent's own lifecycle events, as `seagents __hook` receives them, and
//! the settings that make the agent run it.
//!
//! For an adapter with hooks (`[hooks] inject`: Claude Code, Codex),
//! `run` hands the agent one flag carrying three hooks — `SessionStart`,
//! `UserPromptSubmit`, `Stop` — each running `<this binary> __hook`, which
//! writes what the event reports into the session's `.agent`
//! ([`crate::registry::AgentRecord`]). That is how seagents learns which
//! conversation a window is writing (exact, and again after every `/clear`),
//! and the turn ledger `wait` blocks on. Both tools send these events in
//! one shape (Codex adopted Claude's: `session_id`, `transcript_path`,
//! `prompt`, `last_assistant_message`, …), so one handler reads both; only
//! how the hooks get onto the command line differs, and that is the
//! adapter's `[hooks] inject` ([`inject`]).
//!
//! **The `__hook` contract**, because a hook that misbehaves changes the
//! agent: it always exits 0 and writes nothing to stdout — a `Stop` hook
//! exiting 2 *prevents Claude from stopping*, and stdout can be read as a hook
//! decision. It runs synchronously on every prompt and every turn end, so it
//! is one directory scan, one small read, one rename: no i3, no herdr, no
//! snapshot. Errors go to `hook.log` in the cache dir, and only under
//! `SEAGENTS_DEBUG`.
//!
//! **Which session.** The agent runs in a herdr pane, and herdr gives
//! everything in it the pane's id and its server's socket
//! (`HERDR_PANE_ID`, `HERDR_SOCKET_PATH`), which the hook inherits through
//! the agent: the session `run` registered for that pane of that server
//! ([`registry::by_pane`]).

use std::io::Read;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::adapter::{Adapter, Inject};
use crate::registry::{self, AgentRecord, Files};

/// The events [`record`] folds into the ledger — what an adapter may hook.
pub(crate) const KNOWN_EVENTS: [&str; 3] = ["SessionStart", "UserPromptSubmit", "Stop"];

/// How long an agent that has been given a prompt has for its hooks to
/// report before they count as silent. Claude fires `SessionStart` at launch
/// and Codex at its first prompt, each within a second of it; 20 s leaves a
/// slow start and a busy machine far behind.
const HOOK_GRACE: u64 = 20;

/// Are the agent's hooks reporting? None when `run` handed it none (an
/// adapter without them, `--bare`, a user `--settings` that would not merge):
/// nothing to expect. Otherwise false exactly when the agent was given a
/// prompt ([`registry::note_engaged`]) at least [`HOOK_GRACE`] ago and its
/// `.agent` still has no `session_id` — which every hook event carries, so
/// one firing is enough to fill it.
///
/// The failure this names is silent otherwise: identity and the turn ledger
/// stay empty, and `wait` would block to its timeout on a turn nothing will
/// ever report — say, a Codex that changes how it hashes a hook's trust
/// (which seagents reproduces), or a Claude that changes what `--settings`
/// means. "Given a prompt" rather than "seen working" because Codex's screen
/// reads `working` while it boots an MCP server, before any prompt, and its
/// first hook only fires at the first prompt (measured 2026-09-29).
pub(crate) fn health(meta: &registry::Meta, agent: &AgentRecord, now: u64) -> Option<bool> {
    if !meta.hooks {
        return None;
    }
    let silent =
        agent.session_id.is_none() && meta.engaged_at.is_some_and(|t| now >= t + HOOK_GRACE);
    Some(!silent)
}

/// What to tell a caller whose agent's hooks are silent ([`health`]).
pub(crate) fn silence(meta: &registry::Meta) -> String {
    format!(
        "{}'s {} hooks have not reported in the {HOOK_GRACE}s since it was given a prompt, so seagents \
         cannot see its turns; rerun it with SEAGENTS_DEBUG=1 set and check {} (a hook that ran and \
         failed logs there; nothing there means the agent never ran them)",
        meta.tag,
        meta.agent(),
        registry::cache_dir().join("hook.log").display()
    )
}

/// `seagents __hook`: read one event from stdin, record it. Always 0.
pub(crate) fn run() -> i32 {
    // A panic would exit 101, and a nonzero Stop hook is shown to the user
    // (2 would even keep the agent from stopping). Nothing here is worth that.
    let outcome = std::panic::catch_unwind(|| -> Result<(), String> {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|e| format!("stdin: {e}"))?;
        let payload: Value = serde_json::from_str(&input).map_err(|e| format!("payload: {e}"))?;
        let session = find_session().ok_or("no seagents session for this herdr pane")?;
        record(&session.files, &payload, crate::clock::unix_now_ms())
    });
    let err = match outcome {
        Ok(Ok(())) => return 0,
        Ok(Err(e)) => e,
        Err(_) => "panicked".to_string(),
    };
    registry::debug_log("hook", &err);
    0
}

/// The session this hook's agent runs in, by the herdr pane it inherited.
/// `run` writes the `.meta` before it starts the agent, so it is there by
/// the first hook.
fn find_session() -> Option<registry::Session> {
    let pane = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|p| !p.is_empty())?;
    let socket = std::env::var("HERDR_SOCKET_PATH").unwrap_or_default();
    registry::by_pane(&pane, &socket)
}

/// Fold one hook payload into the session's `.agent`.
///
/// Every event carries the current `session_id` and `transcript_path`, so all
/// of them refresh identity, not only `SessionStart`: a `SessionStart` lost to
/// a startup race heals on the next prompt. `now` is Unix
/// milliseconds, the ledger's unit (see [`AgentRecord::prompted_at`]).
fn record(files: &Files, payload: &Value, now: u64) -> Result<(), String> {
    let event = payload["hook_event_name"]
        .as_str()
        .ok_or("payload has no hook_event_name")?;
    let text = |key: &str| payload[key].as_str().map(str::to_string);
    let apply = |r: &mut AgentRecord| {
        if let Some(p) = text("transcript_path") {
            r.transcript_path = Some(p);
        }
        if let Some(id) = text("session_id") {
            r.session_id = Some(id);
        }
        match event {
            "UserPromptSubmit" => r.prompted_at = Some(now),
            "Stop" => {
                r.stops += 1;
                r.stopped_at = Some(now);
                r.last_message = text("last_assistant_message");
            }
            _ => {}
        }
        r.updated = now;
    };
    if !KNOWN_EVENTS.contains(&event) {
        return Err(format!("unexpected event {event}"));
    }
    registry::update_agent(files, apply);
    Ok(())
}

// ---------------------------------------------------------------------------
// Injection
// ---------------------------------------------------------------------------

/// The child's argv for the user's `argv`: with one flag carrying our hooks
/// when the adapter takes them, else `argv` unchanged. The second value is
/// a warning for stderr when hooks were wanted but could not be added.
///
/// **Merge, never append.** Both tools let a later flag replace an earlier
/// one — repeated `--settings` flags replace each other (verified
/// 2026-09-28: only the last flag's hooks run), and a later `-c hooks.Stop=…`
/// replaces what an earlier `-c hooks=…` set — while hooks from the user's
/// own config *files* run beside ours either way. So those files are left
/// alone (seagents never writes the user's config), and every hook flag of
/// the user's is read, has our entries appended to its `hooks.<Event>`
/// arrays, and is passed back as ONE flag right after `argv[0]` (after a
/// `--` would be the agent's positional text). If the user's value can't
/// be read or isn't what the tool takes, their flags pass through untouched
/// and the agent runs without hooks: never drop the user's settings to make
/// room for ours.
///
/// How the one flag is written is the adapter's `[hooks] inject`:
/// [`Inject::Settings`] a whole settings object as JSON (Claude), written to
/// `settings_file` (0600) and passed by its path — herdr starts the agent by
/// typing its command line into the pane's shell, where inline JSON would
/// show and land in the shell's history; [`Inject::Config`] a `hooks=<TOML>`
/// config override that also marks each hook we add as trusted (Codex,
/// [`codex_trust`]).
pub(crate) fn inject(
    argv: &[String],
    adapter: &Adapter,
    exe: &Path,
    cwd: &Path,
    settings_file: &Path,
) -> (Vec<String>, Option<String>) {
    let hooks = &adapter.hooks;
    let flags_end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    if !hooks.enabled()
        || argv.is_empty()
        || argv[1..flags_end]
            .iter()
            .any(|a| hooks.hookless_flags.contains(a))
    {
        return (argv.to_vec(), None);
    }
    let command = format!(
        "{} __hook",
        crate::launch::shell_quote(&exe.to_string_lossy())
    );

    let mine: Vec<Occurrence> = occurrences(&argv[..flags_end], &hooks.flags)
        .into_iter()
        .filter(|o| {
            hooks.inject != Inject::Config || o.value.as_deref().is_none_or(is_hooks_override)
        })
        .collect();
    let merged = match hooks.inject {
        // The last one wins in the agent too, so it is the one merged into.
        Inject::Settings => settings_value(
            mine.last().map(|o| o.value.as_deref()),
            cwd,
            &hooks.events,
            &command,
        )
        .and_then(|json| {
            crate::fsutil::write_atomic(settings_file, json.as_bytes())
                .map(|()| settings_file.to_string_lossy().into_owned())
                .map_err(|e| format!("writing {}: {e}", settings_file.display()))
        }),
        Inject::Config => config_value(&mine, &hooks.events, &command),
        Inject::None => unreachable!("not wanted"),
    };
    match merged {
        Ok(value) => {
            let mut out = vec![argv[0].clone(), hooks.flags[0].clone(), value];
            let mut i = 1;
            for o in &mine {
                out.extend_from_slice(&argv[i..o.at]);
                i = o.at + o.width;
            }
            out.extend_from_slice(&argv[i..]);
            (out, None)
        }
        Err(e) => (
            argv.to_vec(),
            Some(format!(
                "your {} could not be merged with seagents' hooks ({e}); passing it through \
                 untouched, without hooks — this agent reports no conversation id or turns",
                hooks.flags[0]
            )),
        ),
    }
}

/// One use of the hook flag on the user's command line.
struct Occurrence {
    /// Where it starts in argv, and how many words it takes (2 for
    /// `--flag value`, 1 for `--flag=value` or `-fvalue`).
    at: usize,
    width: usize,
    /// None when the flag is the last word, with no value.
    value: Option<String>,
}

/// Every use of any spelling in `spellings` among `words` (an argv up to its
/// `--`, `words[0]` the command), as a parser takes them: `--long value`,
/// `--long=value`, `-s value`, `-svalue`, `-s=value`.
fn occurrences(words: &[String], spellings: &[String]) -> Vec<Occurrence> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        let a = &words[i];
        let joined = spellings.iter().find_map(|s| {
            let rest = a.strip_prefix(s.as_str())?;
            if s.starts_with("--") {
                rest.strip_prefix('=')
            } else {
                (!rest.is_empty()).then(|| rest.strip_prefix('=').unwrap_or(rest))
            }
        });
        if spellings.contains(a) {
            out.push(Occurrence {
                at: i,
                width: 2,
                value: words.get(i + 1).cloned(),
            });
            i += 2;
        } else if let Some(v) = joined {
            out.push(Occurrence {
                at: i,
                width: 1,
                value: Some(v.to_string()),
            });
            i += 1;
        } else {
            i += 1;
        }
    }
    // A trailing flag with no value takes one word, not two.
    if let Some(last) = out.last_mut().filter(|o| o.value.is_none()) {
        last.width = 1;
    }
    out
}

/// Whether a config override (`key=value`) sets `hooks` or something in it.
fn is_hooks_override(value: &str) -> bool {
    let key = value.split('=').next().unwrap_or("").trim();
    key == "hooks" || key.starts_with("hooks.")
}

/// [`Inject::Settings`]: the user's settings (`user`: their last
/// `--settings`, if any — Some(None) when it had no value) with our hooks
/// added, as one JSON object.
fn settings_value(
    user: Option<Option<&str>>,
    cwd: &Path,
    events: &[String],
    command: &str,
) -> Result<String, String> {
    let mut settings = match user {
        None => Map::new(),
        Some(None) => return Err("it has no value".to_string()),
        Some(Some(v)) => load_settings(v, cwd)?,
    };
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("its \"hooks\" is not an object")?;
    add_hooks(hooks, events, command)?;
    Ok(Value::Object(settings).to_string())
}

/// A `--settings` value as the agent reads it: inline JSON when it looks like
/// an object, else a path (relative to the agent's cwd, which is ours).
fn load_settings(value: &str, cwd: &Path) -> Result<Map<String, Value>, String> {
    let text = if value.trim_start().starts_with('{') {
        value.to_string()
    } else {
        std::fs::read_to_string(cwd.join(value)).map_err(|e| format!("{value}: {e}"))?
    };
    match serde_json::from_str(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("not a JSON object".to_string()),
        Err(e) => Err(format!("not valid JSON: {e}")),
    }
}

/// Codex keys a hook's trust by where it was configured; a `-c` override is
/// its "session flags" layer, which it names this.
const CODEX_SESSION_FLAGS: &str = "/<session-flags>/config.toml";

/// [`Inject::Config`]: the user's `hooks` overrides applied in order, as
/// Codex applies them (a dotted key into the table, the value TOML), our
/// hooks added and marked trusted, as one `hooks=<inline TOML>` override.
fn config_value(user: &[Occurrence], events: &[String], command: &str) -> Result<String, String> {
    let mut root = toml::Table::new();
    for o in user {
        let v = o.value.as_deref().ok_or("it has no value")?;
        let (key, raw) = v
            .split_once('=')
            .ok_or_else(|| format!("{v:?} has no '='"))?;
        let value = toml::from_str::<toml::Table>(&format!("v = {}", raw.trim()))
            .ok()
            .and_then(|mut t| t.remove("v"))
            .ok_or_else(|| format!("{key} is not a TOML value"))?;
        set_path(&mut root, key.trim(), value)?;
    }
    let hooks = match root.remove("hooks") {
        None => Map::new(),
        Some(toml::Value::Table(t)) => match serde_json::to_value(t) {
            Ok(Value::Object(m)) => m,
            _ => return Err("its hooks table has values JSON can't hold".to_string()),
        },
        Some(_) => return Err("its \"hooks\" is not a table".to_string()),
    };
    let mut hooks = hooks;
    let ours = add_hooks(&mut hooks, events, command)?;
    let state = hooks
        .entry("state")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("its hooks.state is not a table")?;
    for (event, group) in ours {
        let (key, hash) = codex_trust(&event, group, command);
        state.insert(key, json!({ "trusted_hash": hash }));
    }
    Ok(format!("hooks={}", toml_inline(&Value::Object(hooks))?))
}

/// Set `value` at the dotted `key` (split on every `.`, as Codex does).
fn set_path(root: &mut toml::Table, key: &str, value: toml::Value) -> Result<(), String> {
    let mut parts = key.split('.').peekable();
    let mut table = root;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            table.insert(part.to_string(), value);
            return Ok(());
        }
        table = table
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| format!("{key}: {part} is not a table"))?;
    }
    Err("an empty key".to_string())
}

/// A JSON value as a TOML inline value — every key quoted, strings in the
/// escapes JSON and TOML share, plus DEL, which TOML wants escaped and JSON
/// does not.
fn toml_inline(v: &Value) -> Result<String, String> {
    let string = |s: &str| {
        serde_json::to_string(s)
            .unwrap_or_default()
            .replace('\u{7f}', "\\u007F")
    };
    Ok(match v {
        Value::Null => return Err("a null has no TOML form".to_string()),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => string(s),
        Value::Array(items) => {
            let items: Result<Vec<String>, String> = items.iter().map(toml_inline).collect();
            format!("[{}]", items?.join(","))
        }
        Value::Object(map) => {
            let fields: Result<Vec<String>, String> = map
                .iter()
                .map(|(k, v)| Ok(format!("{}={}", string(k), toml_inline(v)?)))
                .collect();
            format!("{{{}}}", fields?.join(","))
        }
    })
}

/// The `hooks.state` entry that makes Codex run our handler for `event`,
/// the only one in matcher group `group` of the session-flags layer: its
/// key, and the hash Codex computes for it (codex-rs `hook_hash` →
/// `version_for_toml`): sha256 over the canonical (sorted, compact) JSON of
/// the hook's normalized identity — the event's snake-case name and the
/// group, its handler with defaults filled in (`async = false`, a 600 s
/// timeout for every event [`KNOWN_EVENTS`] allows). Measured: the hash
/// Codex wrote on trusting such a hook in `/hooks` is exactly this.
fn codex_trust(event: &str, group: usize, command: &str) -> (String, String) {
    use sha2::{Digest, Sha256};
    let mut snake = String::new();
    for (i, c) in event.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            snake.push('_');
        }
        snake.push(c.to_ascii_lowercase());
    }
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let identity = format!(
        r#"{{"event_name":{},"hooks":[{{"async":false,"command":{},"timeout":600,"type":"command"}}]}}"#,
        q(&snake),
        q(command)
    );
    let hex: String = Sha256::digest(identity.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    (
        format!("{CODEX_SESSION_FLAGS}:{snake}:{group}:0"),
        format!("sha256:{hex}"),
    )
}

/// Append one matcher group running `command` to each event's array,
/// creating what's missing; returns each event with the index its group
/// landed at. A `hooks` event that isn't an array is a configuration we
/// don't understand: refuse rather than replace it.
fn add_hooks(
    hooks: &mut Map<String, Value>,
    events: &[String],
    command: &str,
) -> Result<Vec<(String, usize)>, String> {
    let mut placed = Vec::new();
    for event in events {
        let groups = hooks
            .entry(event.as_str())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| format!("its hooks.{event} is not an array"))?;
        groups.push(json!({ "hooks": [{ "type": "command", "command": command }] }));
        placed.push((event.clone(), groups.len() - 1));
    }
    Ok(placed)
}
