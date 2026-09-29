//! The agent's own lifecycle events, as `seagents __hook` receives them, and
//! the settings that make the agent run it.
//!
//! For an adapter with `identity`/`turns = "hook"` (Claude Code), `run` hands
//! the agent one `--settings` carrying three hooks — `SessionStart`,
//! `UserPromptSubmit`, `Stop` — each running `<this binary> __hook`, which
//! writes what the event reports into the session's `.agent`
//! ([`crate::registry::AgentRecord`]). That is how seagents learns which
//! conversation a window is writing (exact, and again after every `/clear`),
//! and the turn ledger `wait` blocks on.
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

use crate::adapter::{Adapter, Identity, Turns};
use crate::registry::{self, AgentRecord, Files};

/// The events injected, in the order Claude documents them.
const EVENTS: [&str; 3] = ["SessionStart", "UserPromptSubmit", "Stop"];

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
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(registry::cache_dir().join("hook.log"))
        {
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
        if let Some(s) = registry::enclosing_session(u64::from(std::process::id())) {
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
    if !EVENTS.contains(&event) {
        return Err(format!("unexpected event {event}"));
    }
    registry::update_agent(files, apply);
    Ok(())
}

// ---------------------------------------------------------------------------
// Injection
// ---------------------------------------------------------------------------

/// The child's argv for the user's `argv`: with one `--settings` carrying our
/// hooks when the adapter takes them, else `argv` unchanged. The second value
/// is a warning for stderr when hooks were wanted but could not be added.
///
/// **Merge, never append** (verified 2026-09-28): repeated `--settings` flags
/// replace each other — only the last flag's hooks run — while hooks from
/// settings *files* and from the flag coexist. So the user's settings files
/// are left alone (their hooks still run; seagents never writes the user's
/// config), and a `--settings X` of the user's own is loaded, has our entries
/// appended to its `hooks.<Event>` arrays, and is passed back as ONE inline
/// flag in its place. If X can't be read or isn't a settings object, the
/// user's flag passes through untouched and the agent runs without hooks:
/// never drop the user's settings to make room for ours.
///
/// Only arguments before a `--` are flags; after it they are the agent's
/// positional arguments (a seed prompt), so ours goes right after `argv[0]`.
pub(crate) fn inject_settings(
    argv: &[String],
    adapter: &Adapter,
    exe: &Path,
    cwd: &Path,
) -> (Vec<String>, Option<String>) {
    let launch = &adapter.launch;
    let wanted = !launch.settings_flag.is_empty()
        && (launch.identity == Identity::Hook || launch.turns == Turns::Hook);
    let flags_end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let flags = argv.get(1..flags_end).unwrap_or_default();
    if !wanted || argv.is_empty() || flags.iter().any(|a| launch.hookless_flags.contains(a)) {
        return (argv.to_vec(), None);
    }
    let flag = launch.settings_flag.as_str();
    let command = format!("{} __hook", crate::launch::shell_quote(&exe.to_string_lossy()));

    // The user's own flag, if any: (index, value, whether it was `--flag=value`).
    let prefix = format!("{flag}=");
    let mut user = None;
    for (i, a) in flags.iter().enumerate() {
        let i = i + 1;
        if a == flag {
            user = Some((i, argv.get(i + 1).cloned(), false));
        } else if let Some(v) = a.strip_prefix(&prefix) {
            user = Some((i, Some(v.to_string()), true));
        }
        // The last one wins in the agent too, so it is the one to merge into.
    }

    let Some((at, value, joined)) = user else {
        let mut out = vec![argv[0].clone(), flag.to_string(), settings_with_hooks(Map::new(), &command)];
        out.extend_from_slice(&argv[1..]);
        return (out, None);
    };
    let merged = value
        .ok_or_else(|| format!("{flag} has no value"))
        .and_then(|v| load_settings(&v, cwd))
        .and_then(|base| {
            let mut base = base;
            add_hooks(&mut base, &command)?;
            Ok(Value::Object(base).to_string())
        });
    match merged {
        Ok(json) => {
            let mut out = argv[..at].to_vec();
            if joined {
                out.push(format!("{flag}={json}"));
                out.extend_from_slice(&argv[at + 1..]);
            } else {
                out.push(flag.to_string());
                out.push(json);
                out.extend_from_slice(&argv[at + 2..]);
            }
            (out, None)
        }
        Err(e) => (
            argv.to_vec(),
            Some(format!(
                "your {flag} could not be merged with seagents' hooks ({e}); \
                 passing it through untouched, without hooks — this agent reports no \
                 conversation id or turns"
            )),
        ),
    }
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

fn settings_with_hooks(mut base: Map<String, Value>, command: &str) -> String {
    // A fresh object always has room for our hooks.
    let _ = add_hooks(&mut base, command);
    Value::Object(base).to_string()
}

/// Append one matcher group running `command` to each of our events' arrays,
/// creating what's missing. A `hooks` that isn't an object, or an event that
/// isn't an array, is a settings file we don't understand: refuse rather than
/// replace it.
fn add_hooks(settings: &mut Map<String, Value>, command: &str) -> Result<(), String> {
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("its \"hooks\" is not an object")?;
    for event in EVENTS {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| format!("its hooks.{event} is not an array"))?;
        groups.push(json!({ "hooks": [{ "type": "command", "command": command }] }));
    }
    Ok(())
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
        inject_settings(&argv(a), &claude(), Path::new(EXE), cwd)
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
        for event in EVENTS {
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
        let (out, warn) = inject(&["claude", "--settings=s.json", "--", "go"], &dir);
        assert!(warn.is_none());
        let merged = out[1].strip_prefix("--settings=").expect("same spelling");
        assert_eq!(commands(merged, "UserPromptSubmit"), ["log", OURS]);
        assert_eq!(&out[2..], ["--", "go"]);
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
        let codex = bundled("codex");
        let a = argv(&["codex", "fix it"]);
        assert_eq!(inject_settings(&a, &codex, Path::new(EXE), Path::new("/")).0, a);
        let a = argv(&["bash"]);
        assert_eq!(inject_settings(&a, &Adapter::default(), Path::new(EXE), Path::new("/")).0, a);

        // A `--settings` or `--bare` after `--` is prompt text, not a flag.
        let (out, _) = inject(&["claude", "--", "--settings", "--bare"], Path::new("/"));
        assert_eq!(out[1], "--settings");
        assert_eq!(commands(&out[2], "Stop"), [OURS]);
        assert_eq!(&out[3..], ["--", "--settings", "--bare"]);
    }
}
