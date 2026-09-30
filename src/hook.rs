//! The agent's own lifecycle events, as `seagents __hook` receives them, and
//! the settings that make the agent run it.
//!
//! For an adapter with `identity`/`turns = "hook"` (Claude Code, Codex),
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
//! is one ancestry walk, one small read, one rename: no i3, no detection, no
//! snapshot. Errors go to `hook.log` in the cache dir, and only under
//! `SEAGENTS_DEBUG`.

use std::io::{Read, Write};
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::adapter::{Adapter, Identity, Inject, Turns};
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
    let silent = agent.session_id.is_none() && meta.engaged_at.is_some_and(|t| now >= t + HOOK_GRACE);
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
        let session = find_session().ok_or("no enclosing seagents session")?;
        record(&session.files, &payload, crate::clock::unix_now_ms())
    });
    let err = match outcome {
        Ok(Ok(())) => return 0,
        Ok(Err(e)) => e,
        Err(_) => "panicked".to_string(),
    };
    if std::env::var_os("SEAGENTS_DEBUG").is_some() {
        if let Ok(mut log) = crate::fsutil::create_private(&registry::cache_dir().join("hook.log"), true) {
            let _ = writeln!(log, "{} pid {}: {err}", crate::clock::unix_now(), std::process::id());
        }
    }
    0
}

/// The session this hook's agent runs in. The wrapper forks the agent before
/// it writes the `.meta` (the child pid is part of it), so an agent that fires
/// `SessionStart` very early could beat it; a miss is retried briefly. A hook
/// only ever runs under a wrapper that injected it, so a miss is rare, and
/// the common path pays nothing.
fn find_session() -> Option<registry::Session> {
    for attempt in 0..10 {
        if let Ok(Some(s)) = registry::enclosing_session(u64::from(std::process::id()), None) {
            return Some(s);
        }
        if attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    None
}

/// Fold one hook payload into the session's `.agent`.
///
/// Every event carries the current `session_id` and `transcript_path`, so all
/// of them refresh identity, not only `SessionStart`: a `SessionStart` lost to
/// a startup race (or to a wrapper that was not yet recording) heals on the
/// next prompt. `source` only comes with `SessionStart`. `now` is Unix
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
            "SessionStart" => r.source = text("source").or(r.source.take()),
            "UserPromptSubmit" => {
                r.prompted_at = Some(now);
                r.permission_mode = text("permission_mode").or(r.permission_mode.take());
            }
            "Stop" => {
                r.stops += 1;
                r.stopped_at = Some(now);
                r.last_message = text("last_assistant_message");
                r.permission_mode = text("permission_mode").or(r.permission_mode.take());
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
/// [`Inject::Settings`] a whole settings object as JSON (Claude),
/// [`Inject::Config`] a `hooks=<TOML>` config override that also marks each
/// hook we add as trusted (Codex, [`codex_trust`]).
pub(crate) fn inject(argv: &[String], adapter: &Adapter, exe: &Path, cwd: &Path) -> (Vec<String>, Option<String>) {
    let hooks = &adapter.hooks;
    let launch = &adapter.launch;
    let wanted = hooks.inject != Inject::None
        && !hooks.flags.is_empty()
        && (launch.identity == Identity::Hook || launch.turns == Turns::Hook);
    let flags_end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    if !wanted || argv.is_empty() || argv[1..flags_end].iter().any(|a| hooks.hookless_flags.contains(a)) {
        return (argv.to_vec(), None);
    }
    let command = format!("{} __hook", crate::launch::shell_quote(&exe.to_string_lossy()));

    let mine: Vec<Occurrence> = occurrences(&argv[..flags_end], &hooks.flags)
        .into_iter()
        .filter(|o| hooks.inject != Inject::Config || o.value.as_deref().is_none_or(is_hooks_override))
        .collect();
    let merged = match hooks.inject {
        // The last one wins in the agent too, so it is the one merged into.
        Inject::Settings => settings_value(mine.last().map(|o| o.value.as_deref()), cwd, &hooks.events, &command),
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
            out.push(Occurrence { at: i, width: 2, value: words.get(i + 1).cloned() });
            i += 2;
        } else if let Some(v) = joined {
            out.push(Occurrence { at: i, width: 1, value: Some(v.to_string()) });
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
fn settings_value(user: Option<Option<&str>>, cwd: &Path, events: &[String], command: &str) -> Result<String, String> {
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
        let (key, raw) = v.split_once('=').ok_or_else(|| format!("{v:?} has no '='"))?;
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
    let string = |s: &str| serde_json::to_string(s).unwrap_or_default().replace('\u{7f}', "\\u007F");
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
            let fields: Result<Vec<String>, String> =
                map.iter().map(|(k, v)| Ok(format!("{}={}", string(k), toml_inline(v)?))).collect();
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
    let hex: String = Sha256::digest(identity.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    (format!("{CODEX_SESSION_FLAGS}:{snake}:{group}:0"), format!("sha256:{hex}"))
}

/// Append one matcher group running `command` to each event's array,
/// creating what's missing; returns each event with the index its group
/// landed at. A `hooks` event that isn't an array is a configuration we
/// don't understand: refuse rather than replace it.
fn add_hooks(hooks: &mut Map<String, Value>, events: &[String], command: &str) -> Result<Vec<(String, usize)>, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seagents-hook-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A session in `dir` whose `.meta` exists, so `.agent` updates land.
    fn session(dir: &Path) -> Files {
        let files = Files::in_dir(dir, 1);
        std::fs::write(&files.meta, r#"{"wrapper_pid":1,"child_pid":2}"#).unwrap();
        files
    }

    fn feed(files: &Files, payload: Value, now: u64) {
        record(files, &payload, now).unwrap();
    }

    #[test]
    fn hooks_are_silent_only_when_a_prompt_went_unreported_for_the_grace() {
        let meta = |hooks, engaged_at| registry::Meta { hooks, engaged_at, started: 100, ..Default::default() };
        let quiet = AgentRecord::default();
        let heard = AgentRecord { session_id: Some("s".into()), ..AgentRecord::default() };
        // No hooks handed over: nothing to expect.
        assert_eq!(health(&meta(false, Some(100)), &quiet, 1000), None);
        // Never prompted: an agent left at its input box owes no report
        // (Codex's first hook is its first prompt), however long it idles.
        assert_eq!(health(&meta(true, None), &quiet, 10_000), Some(true));
        // Prompted: HOOK_GRACE seconds to hear from it.
        assert_eq!(health(&meta(true, Some(150)), &quiet, 150 + HOOK_GRACE - 1), Some(true));
        assert_eq!(health(&meta(true, Some(150)), &quiet, 150 + HOOK_GRACE), Some(false));
        // One event is enough.
        assert_eq!(health(&meta(true, Some(150)), &heard, 10_000), Some(true));
    }

    #[test]
    fn each_event_updates_its_part_of_the_ledger() {
        let dir = scratch("events");
        let files = session(&dir);
        feed(&files, json!({"hook_event_name":"SessionStart","source":"startup",
            "session_id":"s1","transcript_path":"/c/projects/p/s1.jsonl","cwd":"/p"}), 10);
        let r = registry::read_agent(&files);
        assert_eq!(r.session_id.as_deref(), Some("s1"));
        assert_eq!(r.transcript_path.as_deref(), Some("/c/projects/p/s1.jsonl"));
        assert_eq!(r.source.as_deref(), Some("startup"));
        assert_eq!((r.stops, r.prompted_at, r.stopped_at, r.updated), (0, None, None, 10));

        feed(&files, json!({"hook_event_name":"UserPromptSubmit","session_id":"s1",
            "transcript_path":"/c/projects/p/s1.jsonl","prompt_id":"x","permission_mode":"plan",
            "prompt":"hi"}), 20);
        let r = registry::read_agent(&files);
        assert_eq!((r.prompted_at, r.permission_mode.as_deref()), (Some(20), Some("plan")));
        assert_eq!(r.stops, 0);

        feed(&files, json!({"hook_event_name":"Stop","session_id":"s1",
            "transcript_path":"/c/projects/p/s1.jsonl","prompt_id":"x",
            "permission_mode":"acceptEdits","stop_hook_active":false,
            "last_assistant_message":"hello"}), 25);
        let r = registry::read_agent(&files);
        assert_eq!((r.stops, r.stopped_at), (1, Some(25)));
        assert_eq!(r.last_message.as_deref(), Some("hello"));
        assert_eq!(r.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(r.prompted_at, Some(20), "a Stop leaves the prompt stamp alone");

        // /clear: a new conversation; the ledger carries on counting.
        feed(&files, json!({"hook_event_name":"SessionStart","source":"clear",
            "session_id":"s2","transcript_path":"/c/projects/p/s2.jsonl"}), 30);
        let r = registry::read_agent(&files);
        assert_eq!((r.session_id.as_deref(), r.source.as_deref()), (Some("s2"), Some("clear")));
        assert_eq!(r.transcript_path.as_deref(), Some("/c/projects/p/s2.jsonl"));
        assert_eq!((r.stops, r.last_message.as_deref()), (1, Some("hello")));

        // Interrupted turn: a prompt and no Stop — nothing counted.
        feed(&files, json!({"hook_event_name":"UserPromptSubmit","session_id":"s2"}), 40);
        feed(&files, json!({"hook_event_name":"Stop","session_id":"s2",
            "last_assistant_message":"second"}), 50);
        let r = registry::read_agent(&files);
        assert_eq!((r.stops, r.prompted_at, r.stopped_at), (2, Some(40), Some(50)));
        assert_eq!(r.permission_mode.as_deref(), Some("acceptEdits"), "kept when not reported");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_first_event_heals_identity_and_junk_is_refused_without_writing() {
        let dir = scratch("heal");
        let files = session(&dir);
        // No SessionStart seen: the prompt's own ids are enough.
        feed(&files, json!({"hook_event_name":"UserPromptSubmit","session_id":"s9",
            "transcript_path":"/t/s9.jsonl"}), 5);
        let r = registry::read_agent(&files);
        assert_eq!((r.session_id.as_deref(), r.source), (Some("s9"), None));

        assert!(record(&files, &json!({"session_id":"zz"}), 6).is_err());
        assert!(record(&files, &json!({"hook_event_name":"PreToolUse","session_id":"zz"}), 6).is_err());
        assert_eq!(registry::read_agent(&files).session_id.as_deref(), Some("s9"));

        // A reaped session (no .meta) gets no .agent back.
        let gone = Files::in_dir(&dir, 7);
        feed(&gone, json!({"hook_event_name":"Stop"}), 7);
        assert!(!gone.agent.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A bundled adapter, read under the lock the adapter tests hold while
    /// they point the config dir at a scratch override.
    fn bundled(agent: &str) -> Adapter {
        let _env = crate::vendor::herdr_detect::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::adapter::for_agent(agent).unwrap()
    }

    fn claude() -> Adapter {
        bundled("claude")
    }

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    const EXE: &str = "/opt/my bin/seagents";

    fn inject(a: &[&str], cwd: &Path) -> (Vec<String>, Option<String>) {
        super::inject(&argv(a), &claude(), Path::new(EXE), cwd)
    }

    fn inject_codex(a: &[&str]) -> (Vec<String>, Option<String>) {
        super::inject(&argv(a), &bundled("codex"), Path::new(EXE), Path::new("/"))
    }

    /// Every hook command in a settings value, per event.
    fn commands(settings: &str, event: &str) -> Vec<String> {
        let v: Value = serde_json::from_str(settings).unwrap();
        v["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().clone())
            .map(|h| h["command"].as_str().unwrap().to_string())
            .collect()
    }

    const OURS: &str = "'/opt/my bin/seagents' __hook";

    #[test]
    fn no_user_settings_gets_one_flag_right_after_the_command() {
        let (out, warn) = inject(&["claude", "--model", "haiku", "--", "say hi"], Path::new("/"));
        assert!(warn.is_none());
        assert_eq!(out.len(), 7);
        assert_eq!((&out[0][..], &out[1][..]), ("claude", "--settings"));
        assert_eq!(&out[3..], ["--model", "haiku", "--", "say hi"]);
        for event in KNOWN_EVENTS {
            assert_eq!(commands(&out[2], event), [OURS], "{event}");
        }
    }

    #[test]
    fn inline_user_settings_are_merged_keeping_their_hooks_and_other_keys() {
        let theirs = r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"theirs"}]}],
            "PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"guard"}]}]}}"#;
        let (out, warn) = inject(&["claude", "--settings", theirs, "-c"], Path::new("/"));
        assert!(warn.is_none());
        assert_eq!(out.iter().filter(|a| *a == "--settings").count(), 1, "one flag, not two");
        assert_eq!((&out[0][..], &out[1][..], &out[3][..]), ("claude", "--settings", "-c"));
        assert_eq!(commands(&out[2], "Stop"), ["theirs", OURS]);
        assert_eq!(commands(&out[2], "SessionStart"), [OURS]);
        assert_eq!(commands(&out[2], "PreToolUse"), ["guard"]);
        let v: Value = serde_json::from_str(&out[2]).unwrap();
        assert_eq!(v["model"], "opus");
    }

    #[test]
    fn a_settings_file_is_loaded_relative_to_the_cwd_and_passed_inline() {
        let dir = scratch("file");
        std::fs::write(
            dir.join("s.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"log"}]}]}}"#,
        )
        .unwrap();
        let (out, warn) = inject(&["claude", "--model", "x", "--settings=s.json", "--", "go"], &dir);
        assert!(warn.is_none());
        assert_eq!(out[1], "--settings");
        assert_eq!(commands(&out[2], "UserPromptSubmit"), ["log", OURS]);
        assert_eq!(&out[3..], ["--model", "x", "--", "go"]);

        // Only the last --settings counts in Claude, so only it is merged;
        // the dead earlier one goes too, leaving one flag.
        let (out, _) = inject(&["claude", "--settings", r#"{"model":"a"}"#, "--settings=s.json"], &dir);
        assert_eq!(out.len(), 3);
        assert_eq!(commands(&out[2], "UserPromptSubmit"), ["log", OURS]);
        assert!(!out[2].contains("model"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unusable_user_settings_pass_through_untouched_with_a_warning() {
        for a in [
            &["claude", "--settings", "{not json", "-c"][..],
            &["claude", "--settings", "/no/such/file.json"][..],
            &["claude", "--settings", "[1,2]"][..],
            &["claude", "--settings", r#"{"hooks":{"Stop":{}}}"#][..],
            &["claude", "--settings"][..],
        ] {
            let (out, warn) = inject(a, Path::new("/"));
            assert_eq!(out, argv(a), "{a:?}");
            assert!(warn.unwrap().contains("--settings"), "{a:?}");
        }
    }

    #[test]
    fn no_injection_for_bare_hookless_adapters_or_flags_after_the_separator() {
        let (out, warn) = inject(&["claude", "--bare", "-p", "x"], Path::new("/"));
        assert_eq!((out, warn), (argv(&["claude", "--bare", "-p", "x"]), None));
        let a = argv(&["bash"]);
        assert_eq!(super::inject(&a, &Adapter::default(), Path::new(EXE), Path::new("/")).0, a);

        // A `--settings` or `--bare` after `--` is prompt text, not a flag.
        let (out, _) = inject(&["claude", "--", "--settings", "--bare"], Path::new("/"));
        assert_eq!(out[1], "--settings");
        assert_eq!(commands(&out[2], "Stop"), [OURS]);
        assert_eq!(&out[3..], ["--", "--settings", "--bare"]);
    }

    /// The `hooks` table a `-c hooks=…` value carries, as JSON.
    fn codex_hooks(flag: &str) -> Value {
        let raw = flag.strip_prefix("hooks=").expect("a hooks override");
        let t: toml::Table = toml::from_str(&format!("v = {raw}")).expect("valid TOML");
        serde_json::to_value(&t["v"]).unwrap()
    }

    #[test]
    fn codex_trust_is_the_hash_codex_itself_wrote() {
        // What codex 0.152 wrote to config.toml when this hook, passed as
        // `-c hooks.Stop=[…]`, was trusted in its /hooks screen.
        let cmd = concat!(
            "/tmp/claude-1000/-home-me-projects-seagents/",
            "834e239b-6ca3-481b-8b2f-64eb3c55e00b/scratchpad/hk/log.sh"
        );
        assert_eq!(
            codex_trust("Stop", 0, cmd),
            (
                "/<session-flags>/config.toml:stop:0:0".to_string(),
                "sha256:be18b9969a8018dc51251280acc5147119c5083adb4c31c8508e1bc16c155c04".to_string()
            )
        );
        assert_eq!(codex_trust("UserPromptSubmit", 2, cmd).0, "/<session-flags>/config.toml:user_prompt_submit:2:0");
    }

    #[test]
    fn codex_gets_one_trusted_hooks_override_right_after_the_command() {
        let (out, warn) = inject_codex(&["codex", "-m", "gpt", "-c", "model_reasoning_effort=\"low\"", "--", "go"]);
        assert!(warn.is_none());
        assert_eq!(&out[..2], ["codex", "-c"]);
        assert_eq!(&out[3..], ["-m", "gpt", "-c", "model_reasoning_effort=\"low\"", "--", "go"]);
        let hooks = codex_hooks(&out[2]);
        for event in KNOWN_EVENTS {
            assert_eq!(hooks[event], json!([{"hooks": [{"type": "command", "command": OURS}]}]), "{event}");
            let (key, hash) = codex_trust(event, 0, OURS);
            assert_eq!(hooks["state"][key]["trusted_hash"], hash, "{event}");
        }
    }

    #[test]
    fn a_users_codex_hook_overrides_are_merged_in_order_and_ours_go_after_them() {
        let theirs = r#"hooks={Stop=[{hooks=[{type="command",command="a"}]}],state={"k"={enabled=false}}}"#;
        let (out, warn) = inject_codex(&[
            "codex",
            "-c",
            theirs,
            "--config=hooks.Stop=[{hooks=[{type=\"command\",command=\"b\"}]}]",
            "-chooks.PreToolUse=[{matcher=\"x\",hooks=[{type=\"command\",command=\"g\"}]}]",
            "-c=model=\"m\"",
        ]);
        assert!(warn.is_none());
        assert_eq!(out.len(), 4, "{out:?}");
        assert_eq!(out[3], "-c=model=\"m\"", "other overrides stay");
        let hooks = codex_hooks(&out[2]);
        // The later hooks.Stop replaced the earlier one, as in Codex; ours
        // is appended after it, and trusted at its index.
        assert_eq!(commands(&json!({"hooks": hooks}).to_string(), "Stop"), ["b", OURS]);
        assert_eq!(commands(&json!({"hooks": hooks}).to_string(), "PreToolUse"), ["g"]);
        assert_eq!(hooks["state"]["k"], json!({"enabled": false}), "their state stays");
        let (key, hash) = codex_trust("Stop", 1, OURS);
        assert_eq!(hooks["state"][key]["trusted_hash"], hash);
    }

    #[test]
    fn unusable_codex_hook_overrides_pass_through_untouched_with_a_warning() {
        for a in [
            &["codex", "-c", "hooks=[1,2]"][..],
            &["codex", "-c", "hooks.Stop={}"][..],
            &["codex", "-c", "hooks.Stop=not toml"][..],
            &["codex", "-c", "hooks={state=3}"][..],
            &["codex", "-c"][..],
        ] {
            let (out, warn) = inject_codex(a);
            assert_eq!(out, argv(a), "{a:?}");
            assert!(warn.unwrap().contains("-c"), "{a:?}");
        }
    }

    #[test]
    fn toml_inline_writes_what_toml_reads_back() {
        let v = json!({"a b": ["x\"y\\z\n\u{7f}\u{1}", 1, 2.5, true], "t": {"k.dot": {}}});
        let text = toml_inline(&v).unwrap();
        let t: toml::Table = toml::from_str(&format!("v = {text}")).unwrap();
        assert_eq!(serde_json::to_value(&t["v"]).unwrap(), v);
        assert!(toml_inline(&json!({"n": null})).is_err());
    }
}
