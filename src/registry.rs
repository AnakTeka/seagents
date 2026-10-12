//! The session cache, `~/.cache/seagents/<key>.*` — its one owner.
//!
//! A session is one agent `seagents run` started in a herdr pane, keyed by
//! the herdr **terminal id** of that pane (`term_65d9…`): herdr's own name for
//! it, fixed for its life (a pane moved to another workspace gets a new pane
//! id, its terminal keeps its id) and unique across herdr servers.
//!
//! Every sidecar is named, read and written here and nowhere else:
//!
//! - `.meta` — who and where: the herdr pane and server, the agent's pid,
//!   argv, cwd, window handles, tag. Written by `run` before it starts the
//!   agent, then updated from any process: window healing from the focus/HUD
//!   processes, the agent's pid from `run`'s monitor, the last prompt from
//!   `send`.
//! - `.state` — the last observed state, since when, and whether the user has
//!   seen it. Written by `run`'s monitor and by every observer whose own look
//!   disagrees (see `status::observe_state`).
//! - `.agent` — what the agent itself reports: which conversation it is
//!   writing, and the turn ledger ([`AgentRecord`]). Written by `__hook`,
//!   which the agent runs at its own lifecycle events. Kept apart from
//!   `.meta` because it churns on every prompt and turn end.
//!
//! Because every sidecar has writers in several processes, every change is a
//! locked read-modify-write ([`update_meta`], [`update_state`],
//! [`update_agent`]): an exclusive `flock` on the session's `.lock` sibling,
//! read, mutate the typed record, write a pid-suffixed tmp file, rename it
//! over, unlock. Readers take no lock — the rename means they see the old
//! record or the new one, never a torn one. A `send` or `stop` holds a
//! second lock, `.send.lock`, for its whole length ([`send_lock`]), so two of
//! them never interleave — a separate file because a `send` waits for a hook
//! that takes `.lock`.
//!
//! A session's **prompt files** (`<cwd>/.seagents/prompts/<key>-…`,
//! [`write_prompt`]) live outside the cache, in the agent's working
//! directory, and its **hook settings** (`<runtime dir>/hooks-<key>.json`,
//! [`hooks_file`]) in the runtime dir, but are the session's all the same:
//! named here, deleted with it.
//!
//! Liveness is one rule ([`Session::alive`]): the `run` holding the window is
//! alive, and so is the agent — or it is still starting. [`list`] returns
//! live sessions and never deletes anything; [`reap`] deletes the files of
//! dead ones. Only `status`/the HUD reap.
//!
//! Every session has a **tag** — its one name, the address every command
//! resolves ([`resolve_target`]), the window handle `restore` swallows on, and
//! its herdr agent name. Tags are claimed under a cache-wide lock
//! ([`claim_tag`]) held from the uniqueness check through the `.meta`'s
//! creation, which is the only way to create one; two `run`s racing for a
//! name cannot both get it.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::fsutil::write_atomic;
use crate::procfs;

/// Directory holding this machine's seagents session files:
/// `~/.cache/seagents`, private (`fsutil::private_dir`: 0700, a loosened mode
/// on ours tightened). Under `HOME` a symlink or a directory that isn't ours
/// is the user's own arrangement and is used as it is. With no `HOME` it is
/// `/tmp/seagents-<uid>/cache` — the checked directory the runtime dir falls
/// back to — and if `/tmp` holds someone else's directory or a symlink by
/// that name, a path nothing can be written under (`/dev/null/…`): the
/// agent still runs, untracked, rather than keep its records in a directory
/// another user controls.
pub(crate) fn cache_dir() -> PathBuf {
    match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        Some(home) => {
            let dir = PathBuf::from(home).join(".cache/seagents");
            let _ = crate::fsutil::private_dir(&dir);
            dir
        }
        None => crate::fsutil::tmp_dir_in(Path::new("/tmp"))
            .map(|d| d.join("cache"))
            .and_then(|d| crate::fsutil::private_dir(&d).map(|()| d))
            .unwrap_or_else(|_| PathBuf::from("/dev/null/seagents-cache")),
    }
}

/// `~/.cache/seagents/sessions/`, created on demand (0700): per-transcript
/// caches (`transcript::index`'s `.idx` files) — about conversations, not
/// live agents, so nothing here is reaped with a session. None when it cannot
/// be created.
pub(crate) fn sessions_dir() -> Option<PathBuf> {
    let dir = cache_dir().join("sessions");
    crate::fsutil::private_dir(&dir).ok().map(|_| dir)
}

/// A session's sidecar files, by its key.
#[derive(Clone, Debug)]
pub(crate) struct Files {
    /// Identity: [`Meta`].
    pub(crate) meta: PathBuf,
    /// Last observed state: [`StateRecord`].
    pub(crate) state: PathBuf,
    /// Identity and the turn ledger: [`AgentRecord`].
    pub(crate) agent: PathBuf,
    /// Guards every read-modify-write of `meta`, `state` and `agent`.
    lock: PathBuf,
    /// Held across one whole `send` or `stop` ([`send_lock`]).
    send_lock: PathBuf,
}

impl Files {
    /// The files of session `key` on this machine.
    pub(crate) fn of(key: &str) -> Files {
        Files::in_dir(&cache_dir(), key)
    }

    fn in_dir(dir: &Path, key: &str) -> Files {
        Files {
            meta: dir.join(format!("{key}.meta")),
            state: dir.join(format!("{key}.state")),
            agent: dir.join(format!("{key}.agent")),
            lock: dir.join(format!("{key}.lock")),
            send_lock: dir.join(format!("{key}.send.lock")),
        }
    }
}

/// A session's identity, as its `.meta` records it.
///
/// `run` is a long-running process that survives a reinstall, so a `.meta`
/// in the cache may have been written by an older or newer binary than this
/// one. Everything defaults when missing, and fields this binary does not
/// know are carried in `extra` and written back untouched, so an update from
/// here never strips what another version added. A `.meta` with no `key` is
/// not one of ours (a pty-wrapper session from before herdr) and is ignored.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct Meta {
    /// The herdr terminal the agent runs in — the session's key.
    #[serde(default)]
    pub(crate) key: String,
    /// The herdr pane the agent was started in: what its hooks see as
    /// `HERDR_PANE_ID`. A pane moved since has a new id; this one stays.
    #[serde(default)]
    pub(crate) pane_id: String,
    /// The herdr server's API socket (its hooks' `HERDR_SOCKET_PATH`), which
    /// tells pane ids of two servers apart.
    #[serde(default)]
    pub(crate) socket: String,
    /// The `run` holding the window: alive exactly as long as the window is.
    #[serde(default)]
    pub(crate) run_pid: u64,
    /// The agent's process (its pane's foreground process group leader);
    /// None until it has started.
    #[serde(default)]
    pub(crate) pid: Option<u64>,
    /// The herdr agent kind (`claude`, `codex`): which adapter.
    #[serde(default)]
    pub(crate) kind: String,
    /// Launch cwd — the grouping key.
    #[serde(default)]
    pub(crate) cwd: String,
    /// The user's command line — never what `run` added to it — which
    /// `restore` relaunches from.
    #[serde(default)]
    pub(crate) argv: Vec<String>,
    #[serde(default)]
    pub(crate) started: u64,
    /// The i3 window the agent is shown in — the reliable handle for focus
    /// (titles can collide between idle agents). con_id is the fast handle but
    /// dies on an i3 restart; window_id is the X11 id, which survives one.
    #[serde(default)]
    pub(crate) con_id: Option<u64>,
    #[serde(default)]
    pub(crate) window_id: Option<u64>,
    /// The agent's name ([`claim_tag`]), and its herdr agent name.
    #[serde(default)]
    pub(crate) tag: String,
    /// `run` handed the agent our lifecycle hooks, so its `.agent` should
    /// fill in once it is prompted ([`crate::hook::health`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) hooks: bool,
    /// When the agent was first given a prompt (Unix s): its seed, or a
    /// `send`. Set once ([`note_engaged`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) engaged_at: Option<u64>,
    /// The text of the last prompt seagents submitted (a `send`'s paste, or
    /// the seed): what the agent may have put back in its input box, which a
    /// `send` may clear — anything else there is someone's draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_prompt: Option<String>,
    /// When it was submitted (Unix ms): until the agent's prompt hook has
    /// fired since, it is still landing, and the next `send` waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) submitted_ms: Option<u64>,
    /// Fields written by another version of seagents, preserved verbatim.
    #[serde(flatten)]
    pub(crate) extra: Map<String, Value>,
}

impl Meta {
    /// The agent's command, first word's basename — `claude`, `codex`, …
    pub(crate) fn agent(&self) -> &str {
        self.argv.first().map(|a| basename(a)).unwrap_or("")
    }

    /// The launch directory's basename.
    pub(crate) fn dir(&self) -> &str {
        basename(&self.cwd)
    }
}

fn basename(p: &str) -> &str {
    Path::new(p).file_name().and_then(|s| s.to_str()).unwrap_or(p)
}

/// What the `.state` sidecar records: the last observed state word, when it
/// started, and whether the user has seen it since finishing (the "done vs
/// idle" distinction). The semantics live in `status`; this is only the file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct StateRecord {
    pub(crate) state: String,
    pub(crate) since: u64,
    #[serde(default = "seen_by_default")]
    pub(crate) seen: bool,
    /// For `blocked`: the detection rule that matched, which says which
    /// dialog the agent waits at — what `wait` reports and `send` refuses with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rule: Option<String>,
    /// herdr's own verdict on the screen, when it differs from `state` —
    /// which for a hook agent is the screen with the turn ledger folded in
    /// (a turn reads `working` while the screen may say idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) screen: Option<String>,
    /// When the agent's screen last changed while it sat idle in a turn
    /// (Unix ms), as `run`'s monitor saw it: the clock the dropped-turn rule
    /// runs on (`turn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) screen_changed_ms: Option<u64>,
}

impl StateRecord {
    /// What the screen itself shows, ledger aside.
    pub(crate) fn screen(&self) -> &str {
        self.screen.as_deref().unwrap_or(&self.state)
    }
}

fn seen_by_default() -> bool {
    true
}

/// What the agent reports about itself, as its `.agent` records it: which
/// conversation it is writing (identity) and the turn ledger. Written by
/// `__hook` from the agent's own lifecycle events; absent for agents without
/// hooks, and every field defaults, so a missing or older file reads as "not
/// known yet".
///
/// The ledger counts `Stop`s; it does not pair prompts with turns. A message
/// typed mid-turn fires a prompt event reusing the running turn's id and is
/// answered by a second `Stop` nobody announced, and an Esc-interrupted turn
/// fires the prompt event and no `Stop` at all — so prompt counts drift and
/// prompt ids don't pair, and `stops` is the one monotonic edge. It never
/// resets (not on `/clear`, not on a resume), so a baseline a caller holds
/// stays comparable for the session's whole life.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub(crate) struct AgentRecord {
    /// The conversation file the agent is writing now; changes on `/clear`.
    pub(crate) transcript_path: Option<String>,
    /// The conversation id — what `restore` resumes with.
    pub(crate) session_id: Option<String>,
    /// Why the current conversation began: startup | resume | clear | compact.
    pub(crate) source: Option<String>,
    /// The agent's permission mode, as last reported.
    pub(crate) permission_mode: Option<String>,
    /// Turn ends seen, +1 at every `Stop`.
    pub(crate) stops: u64,
    /// When the last prompt was submitted, and the last turn ended — Unix
    /// **milliseconds**: a prompt in the same second as the last `Stop` is
    /// common, and seconds would read it as not after it.
    pub(crate) prompted_at: Option<u64>,
    pub(crate) stopped_at: Option<u64>,
    /// The final assistant text of the last turn.
    pub(crate) last_message: Option<String>,
    /// When any of the above last changed (Unix ms).
    pub(crate) updated: u64,
    /// Fields written by another version of seagents, preserved verbatim.
    #[serde(flatten)]
    pub(crate) extra: Map<String, Value>,
}

/// One tracked session: its files and its identity.
#[derive(Clone, Debug)]
pub(crate) struct Session {
    pub(crate) files: Files,
    pub(crate) meta: Meta,
}

/// How long a session may go without an agent pid before it counts as a
/// failed start: `run` writes the `.meta` before `herdr agent start`, which
/// waits for the agent for up to 120 s.
const START_GRACE: u64 = 180;

impl Session {
    /// The one liveness rule: the `run` holding the window is alive, and so is
    /// the agent — or it has not started yet, within [`START_GRACE`]. Judged
    /// on the `.meta` this was read with.
    pub(crate) fn alive(&self) -> bool {
        is_alive(&self.meta)
    }

    /// [`Session::alive`] on the `.meta` as it is now — for a caller that
    /// holds a session across time (`wait`, `send`): one read before the
    /// agent had a pid would otherwise judge it by the start grace alone.
    /// False once the `.meta` is gone.
    pub(crate) fn alive_now(&self) -> bool {
        read_meta(&self.files).is_some_and(|m| is_alive(&m))
    }

    /// The agent's pid, 0 while it has none — `status --json`'s `pid`.
    pub(crate) fn pid(&self) -> u64 {
        self.meta.pid.unwrap_or_default()
    }
}

fn is_alive(meta: &Meta) -> bool {
    procfs::alive(meta.run_pid)
        && match meta.pid {
            Some(pid) => procfs::alive(pid),
            None => crate::clock::unix_now() < meta.started + START_GRACE,
        }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Parse a `.meta`. None when missing or unreadable.
pub(crate) fn read_meta(files: &Files) -> Option<Meta> {
    serde_json::from_str(&fs::read_to_string(&files.meta).ok()?).ok()
}

pub(crate) fn read_state(files: &Files) -> Option<StateRecord> {
    serde_json::from_str(&fs::read_to_string(&files.state).ok()?).ok()
}

/// Parse a `.agent`; the empty record when there is none (no hook has fired
/// yet, or the agent has none) or it does not parse.
pub(crate) fn read_agent(files: &Files) -> AgentRecord {
    fs::read_to_string(&files.agent)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Every parseable session `.meta` in `dir`, alive or not.
fn all_in(dir: &Path) -> Vec<Session> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Session> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("meta"))
        .filter_map(|e| {
            let meta: Meta = serde_json::from_str(&fs::read_to_string(e.path()).ok()?).ok()?;
            // The file must be the key's own: a `.meta` named otherwise
            // (a pty-wrapper session's `<pid>.meta`) is not a session here.
            (!meta.key.is_empty() && e.file_name().to_str() == Some(&format!("{}.meta", meta.key)))
                .then(|| Session { files: Files::in_dir(dir, &meta.key), meta })
        })
        .collect();
    out.sort_by(|a, b| (a.meta.started, &a.meta.key).cmp(&(b.meta.started, &b.meta.key)));
    out
}

/// Every session whose agent is still running. Never deletes anything, so
/// it is safe from any process.
pub(crate) fn list() -> Vec<Session> {
    list_in(&cache_dir())
}

fn list_in(dir: &Path) -> Vec<Session> {
    all_in(dir).into_iter().filter(Session::alive).collect()
}

/// Delete every file of every session that has ended — its prompt files in
/// its cwd and its hook settings included ([`finish`]). A fully exited agent
/// isn't interesting — "idle" already means finished-and-waiting — so the HUD
/// and `status` only ever show live agents. A `.meta` that does not parse is
/// left alone: it is not known to be dead.
///
/// A session whose `run` is alive is never touched, ended agent or not: `run`
/// owns its own ending (it closes the pane, the window, then `finish()`es).
/// A session whose window is gone (its `run` died without cleaning up — a
/// SIGKILL, a crash) while its agent still runs in herdr has its herdr pane
/// closed first: a window and its agent live and die together.
pub(crate) fn reap() {
    let dir = cache_dir();
    let all = all_in(&dir);
    let live_tags: HashSet<&str> = all.iter().filter(|s| s.alive()).map(|s| s.meta.tag.as_str()).collect();
    for s in all.iter().filter(|s| !procfs::alive(s.meta.run_pid)) {
        if s.meta.pid.is_some_and(procfs::alive) {
            if let Ok(Some(pane)) = crate::herdr::pane_of(&s.meta.key) {
                let _ = crate::herdr::close_pane(&pane.pane_id);
            }
        }
        // A live agent holding the tag again has superseded the dead one.
        finish_in(&dir, s, !live_tags.contains(s.meta.tag.as_str()));
    }
    prune_tombstones(&dir, crate::clock::unix_now());
    sweep_orphans(&dir);
}

/// End session `s`: leave its tombstone, then delete its files, its prompt
/// files and its hook settings. Called by `run` as it exits, and by [`reap`]
/// for a session whose `run` could not.
pub(crate) fn finish(s: &Session) {
    let dir = cache_dir();
    let superseded = list_in(&dir).iter().any(|o| o.meta.tag == s.meta.tag && o.meta.key != s.meta.key);
    finish_in(&dir, s, !superseded);
}

fn finish_in(dir: &Path, s: &Session, tombstone: bool) {
    // Written before anything goes, so a reader that finds the `.agent` gone
    // finds this ([`final_agent`]).
    if tombstone && !s.meta.tag.is_empty() {
        let stone = Tombstone::of(s, crate::clock::unix_now());
        if let Ok(text) = serde_json::to_vec(&stone) {
            let _ = write_atomic(&tombstone_path(dir, &s.meta.tag), &text);
        }
    }
    let f = &s.files;
    for path in [&f.meta, &f.state, &f.agent, &f.lock, &f.send_lock] {
        let _ = fs::remove_file(path);
    }
    remove_prompts(&s.meta.key, &s.meta.cwd);
    if let Ok(hooks) = hooks_file(&s.meta.key) {
        let _ = fs::remove_file(hooks);
    }
}

/// Delete sidecars no `.meta` claims: a session's `.state`, `.agent`,
/// `.lock` or `.send.lock` left by a writer that raced its end, and the
/// files of pty-wrapper sessions (`<pid>.*`, from before herdr) once their
/// pid is gone.
fn sweep_orphans(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some((stem, ext)) = name.to_str().and_then(|n| n.split_once('.')) else {
            continue;
        };
        if ext.ends_with(".tmp") {
            continue;
        }
        let gone = match stem.parse::<u64>() {
            // A pty-wrapper session's (`.raw`, `.meta`, … too): its wrapper
            // pid is the stem.
            Ok(pid) => !procfs::alive(pid),
            Err(_) => {
                matches!(ext, "state" | "agent" | "lock" | "send.lock")
                    && stem.starts_with("term_")
                    && !dir.join(format!("{stem}.meta")).exists()
            }
        };
        if gone {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// The session whose key is `key`, live or not (a lookup is not the place to
/// decide a session has ended).
pub(crate) fn by_key(key: &str) -> Option<Session> {
    let files = Files::of(key);
    let meta = read_meta(&files).filter(|m| m.key == key)?;
    Some(Session { files, meta })
}

/// The live session started in herdr pane `pane_id` of the server at
/// `socket` — how a hook finds its agent's session from the environment herdr
/// gives the agent (`HERDR_PANE_ID`, `HERDR_SOCKET_PATH`). Not filtered on
/// the agent's pid: a hook fires before `run` has recorded it.
pub(crate) fn by_pane(pane_id: &str, socket: &str) -> Option<Session> {
    all_in(&cache_dir())
        .into_iter()
        .filter(|s| procfs::alive(s.meta.run_pid))
        .find(|s| s.meta.pane_id == pane_id && (socket.is_empty() || s.meta.socket.is_empty() || s.meta.socket == socket))
}

/// The live session named by `target`, as every command that addresses an
/// agent takes it: `@tag`, a bare `tag`, or an all-digits agent `pid` (the
/// handle `status --json` reports). Tags start with a letter, so the bare
/// forms cannot be confused. The error is the message to show the user.
pub(crate) fn resolve_target(target: &str) -> Result<Session, String> {
    resolve_target_in(&cache_dir(), target)
}

fn resolve_target_in(dir: &Path, target: &str) -> Result<Session, String> {
    if !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()) {
        let pid: u64 = target.parse().map_err(|_| format!("{target} is not a pid"))?;
        return all_in(dir)
            .into_iter()
            .find(|s| s.meta.pid == Some(pid))
            .ok_or_else(|| format!("no tracked agent with pid {pid}"));
    }
    let tag = target.strip_prefix('@').unwrap_or(target);
    if !valid_tag(tag) {
        return Err(format!("{target:?} is neither a tag ({TAG_RULE}) nor a pid"));
    }
    list_in(dir)
        .into_iter()
        .find(|s| s.meta.tag == tag)
        .ok_or_else(|| format!("no live agent tagged {tag}"))
}

// ---------------------------------------------------------------------------
// Exited agents
// ---------------------------------------------------------------------------

/// Tombstones older than this are pruned by [`reap`].
const TOMBSTONE_TTL: u64 = 24 * 3600;

/// What is left of an agent once its session has ended: `<tag>.exited` in
/// the cache, so a caller that waits on an agent which has since exited — and
/// been cleaned up — still gets its last result rather than "no such agent".
/// Written by [`finish`], removed when a new agent claims the tag
/// ([`TagClaim::create_meta`]) and by [`reap`] after a day. Only `wait` and
/// `read` look at it ([`resolve_exited`]); everything that drives an agent
/// needs a live one.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct Tombstone {
    pub(crate) tag: String,
    /// The session's key.
    #[serde(default)]
    pub(crate) key: String,
    /// When the session ended (Unix s).
    pub(crate) exited_at: u64,
    /// The agent's command (`claude`, `codex`): which adapter reads its
    /// transcript.
    #[serde(default)]
    pub(crate) agent: String,
    /// Its working directory, which a prompt sent by reference is relative to.
    #[serde(default)]
    pub(crate) cwd: String,
    /// The ledger as it ended.
    #[serde(default)]
    pub(crate) stops: u64,
    #[serde(default)]
    pub(crate) last_message: Option<String>,
    #[serde(default)]
    pub(crate) transcript_path: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
}

impl Tombstone {
    fn of(s: &Session, exited_at: u64) -> Tombstone {
        let a = read_agent(&s.files);
        Tombstone {
            tag: s.meta.tag.clone(),
            key: s.meta.key.clone(),
            exited_at,
            agent: s.meta.agent().to_string(),
            cwd: s.meta.cwd.clone(),
            stops: a.stops,
            last_message: a.last_message,
            transcript_path: a.transcript_path,
            session_id: a.session_id,
        }
    }

    /// The ledger it keeps, as the record a live agent's `.agent` would be.
    pub(crate) fn ledger(&self) -> AgentRecord {
        AgentRecord {
            stops: self.stops,
            last_message: self.last_message.clone(),
            transcript_path: self.transcript_path.clone(),
            session_id: self.session_id.clone(),
            ..AgentRecord::default()
        }
    }
}

fn tombstone_path(dir: &Path, tag: &str) -> PathBuf {
    dir.join(format!("{tag}.exited"))
}

fn read_tombstone(dir: &Path, tag: &str) -> Option<Tombstone> {
    serde_json::from_str(&fs::read_to_string(tombstone_path(dir, tag)).ok()?).ok()
}

/// Remove tombstones past [`TOMBSTONE_TTL`], and any that do not parse.
fn prune_tombstones(dir: &Path, now: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("exited") {
            continue;
        }
        let stale = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Tombstone>(&t).ok())
            .is_none_or(|t| now.saturating_sub(t.exited_at) > TOMBSTONE_TTL);
        if stale {
            let _ = fs::remove_file(&path);
        }
    }
}

/// What a target names when it may have exited: a session (live, or dead
/// and not yet reaped — its files still say how it ended), or the tombstone
/// of one that has been cleaned up.
#[derive(Debug)]
pub(crate) enum Found {
    Session(Session),
    Exited(Tombstone),
}

/// [`resolve_target`], then — for a tag no live agent holds — the newest
/// dead session holding it, then its tombstone. For `wait` and `read` only:
/// a finished agent's result stays readable after it exits.
pub(crate) fn resolve_exited(target: &str) -> Result<Found, String> {
    let dir = cache_dir();
    let err = match resolve_target_in(&dir, target) {
        Ok(s) => return Ok(Found::Session(s)),
        Err(e) => e,
    };
    let tag = target.strip_prefix('@').unwrap_or(target);
    if !valid_tag(tag) {
        return Err(err);
    }
    let dead = all_in(&dir).into_iter().filter(|s| s.meta.tag == tag).max_by_key(|s| s.meta.started);
    if let Some(s) = dead {
        return Ok(Found::Session(s));
    }
    read_tombstone(&dir, tag).map(Found::Exited).ok_or(err)
}

/// A session's ledger when it may have ended: its `.agent`, or — once that
/// is deleted — what its tombstone kept. [`finish`] writes the tombstone
/// before it deletes anything, so one of the two is always there.
pub(crate) fn final_agent(session: &Session) -> AgentRecord {
    match fs::read_to_string(&session.files.agent) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => read_tombstone(&cache_dir(), &session.meta.tag)
            .filter(|t| t.key == session.meta.key)
            .map(|t| t.ledger())
            .unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

/// Longest tag. It lands in an X11 property, a branch name, a filename, and
/// herdr's agent name.
const TAG_MAX: usize = 32;

/// The tag charset, as the messages that refuse one spell it.
const TAG_RULE: &str = "[a-z][a-z0-9_-]{0,31}";

/// `[a-z][a-z0-9_-]{0,31}` — herdr's rule for agent names, which a tag is.
/// A leading letter also keeps a bare target from reading as a pid.
pub(crate) fn valid_tag(tag: &str) -> bool {
    let b = tag.as_bytes();
    !b.is_empty()
        && b.len() <= TAG_MAX
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

/// A tag from before tags had to start with a letter (`20260612-sme-3f2a`),
/// as it is today: a `d` in front, the noise suffix kept when shortening.
/// `restore` reads every captured tag through this, for the placeholder and
/// the launch alike. A valid tag is returned as it is.
pub(crate) fn fold_tag(tag: &str) -> String {
    if valid_tag(tag) || tag.is_empty() {
        return tag.to_string();
    }
    let folded: String = format!("d{tag}")
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '-' })
        .collect();
    if folded.len() <= TAG_MAX {
        return folded;
    }
    match folded.rsplit_once('-').filter(|(_, hex)| hex.len() == 4) {
        Some((base, hex)) => format!("{}-{hex}", &base[..TAG_MAX - 5].trim_end_matches('-')),
        None => folded[..TAG_MAX].trim_end_matches('-').to_string(),
    }
}

/// The name part of a minted tag: the cwd's basename folded into the tag
/// charset (runs of anything else become one `-`), a `d` in front when it
/// would not start with a letter, short enough to leave room for the `-xxxx`
/// suffix. Never empty.
fn tag_base(cwd: &Path) -> String {
    let name = cwd.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut base = String::new();
    for c in name.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if base.is_empty() && !c.is_ascii_lowercase() {
                base.push('d');
            }
            base.push(c);
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    base.truncate(TAG_MAX - 5);
    let base = base.trim_end_matches('-');
    if base.is_empty() { "agent" } else { base }.to_string()
}

/// Four hex digits of noise. /dev/urandom when it reads, else the clock mixed
/// with our pid — uniqueness is enforced by the caller's retry, not by this.
fn tag_suffix() -> String {
    let mut buf = [0u8; 2];
    let n = File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map(|_| u16::from_le_bytes(buf))
        .unwrap_or_else(|_| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            (nanos ^ (std::process::id() << 7)) as u16
        });
    format!("{n:04x}")
}

/// A tag nobody holds: `<cwd basename>-<4 hex>`, avoiding every tag in
/// `taken`.
fn mint_tag(cwd: &Path, taken: &HashSet<String>) -> String {
    let base = tag_base(cwd);
    loop {
        let tag = format!("{base}-{}", tag_suffix());
        if !taken.contains(&tag) {
            return tag;
        }
    }
}

/// Whether `tag` is one [`mint_tag`] would have made for `cwd` — the
/// directory's name plus four hex digits of noise. Such a tag says nothing the
/// directory doesn't, so a display that has room for one name shows the
/// directory; a tag somebody chose (`spawn --tag reviewer`) names the agent
/// better than where it runs.
pub(crate) fn minted_from(tag: &str, cwd: &Path) -> bool {
    tag.strip_prefix(tag_base(cwd).as_str())
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|hex| hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Every name a new agent may not take: live sessions' tags, the agent names
/// herdr already has (an agent somebody started in herdr directly), and
/// `reserved` (the snapshot's — a pending restore will want those back).
fn taken_tags(dir: &Path, reserved: &HashSet<String>) -> HashSet<String> {
    let herdr = crate::herdr::agent_names().unwrap_or_default();
    list_in(dir).into_iter().map(|s| s.meta.tag).chain(herdr).chain(reserved.iter().cloned()).collect()
}

/// The name a new session will be created under, with the cache-wide tags lock
/// still held. Only [`TagClaim::create_meta`] turns it into a session, and it
/// releases the lock when it has — so from the uniqueness check to the `.meta`
/// that makes the tag visible to the next claimant, nobody else can claim.
#[derive(Debug)]
pub(crate) struct TagClaim {
    pub(crate) tag: String,
    /// Where a previous holder's tombstone would be; superseded on create.
    exited: PathBuf,
    _lock: Lock,
}

impl TagClaim {
    /// Write the new session's `.meta` under the claimed tag, and release the
    /// claim. Atomic but not per-session locked: until this file exists
    /// nobody else can find the session, so there is nothing to race. A dead
    /// session that held the same key (herdr reused a terminal id) is
    /// finished first.
    pub(crate) fn create_meta(self, mut meta: Meta) -> Files {
        let files = Files::of(&meta.key);
        if let Some(old) = by_key(&meta.key) {
            finish(&old);
        }
        meta.tag = self.tag;
        if let Ok(text) = serde_json::to_string_pretty(&meta) {
            let _ = write_atomic(&files.meta, text.as_bytes());
        }
        // An agent that held the tag before is no longer what the name means.
        let _ = fs::remove_file(&self.exited);
        files
    }
}

/// Claim a tag for a session about to be created: `requested` (validated,
/// and refused while a live session or a herdr agent holds it), or a freshly
/// minted one. The error is the message for the user.
///
/// A requested tag may be one the snapshot recorded (`reserved`): that is
/// precisely how `restore` relaunches an agent under its old name. Minting
/// avoids them, so a new agent never takes a placeholder a pending restore
/// will want back.
pub(crate) fn claim_tag(requested: Option<&str>, cwd: &Path, reserved: &HashSet<String>) -> Result<TagClaim, String> {
    let dir = cache_dir();
    check_requested(requested)?;
    // Without this lock two runs can both pass the check. Failing to take it
    // is failing to claim.
    let lock = Lock::acquire(&dir.join("tags.lock")).ok_or("could not lock the tag registry")?;
    let tag = match requested {
        Some(t) => {
            if taken_tags(&dir, &HashSet::new()).contains(t) {
                return Err(format!("tag {t} is already held by a live agent"));
            }
            t.to_string()
        }
        None => mint_tag(cwd, &taken_tags(&dir, reserved)),
    };
    Ok(TagClaim { exited: tombstone_path(&dir, &tag), tag, _lock: lock })
}

/// A tag for an agent `spawn` is about to open a window for: `requested`,
/// which must be free — of live agents, herdr's agent names and the
/// snapshot's tags (`reserved`: a spawn must not take a placeholder a pending
/// restore will swallow, which `run --tag` alone allows) — or one minted
/// avoiding all three. Nothing is held: the window's `run` claims it
/// ([`claim_tag`]), and refuses it there if another took it meanwhile.
pub(crate) fn pick_tag(requested: Option<&str>, cwd: &Path, reserved: &HashSet<String>) -> Result<String, String> {
    check_requested(requested)?;
    let dir = cache_dir();
    let _lock = Lock::acquire(&dir.join("tags.lock")).ok_or("could not lock the tag registry")?;
    let taken = taken_tags(&dir, &HashSet::new());
    match requested {
        Some(t) if taken.contains(t) => Err(format!("tag {t} is already held by a live agent")),
        Some(t) if reserved.contains(t) => Err(format!(
            "tag {t} belongs to an agent in the saved layout that `restore` would bring back; pick another"
        )),
        Some(t) => Ok(t.to_string()),
        None => Ok(mint_tag(cwd, &taken.union(reserved).cloned().collect())),
    }
}

fn check_requested(requested: Option<&str>) -> Result<(), String> {
    match requested.filter(|t| !valid_tag(t)) {
        Some(tag) => Err(format!("invalid tag {tag:?}: want {TAG_RULE}")),
        None => Ok(()),
    }
}

/// Where per-session runtime files go — spawn's seed prompts and the hook
/// settings `run` hands Claude: `$XDG_RUNTIME_DIR/seagents` (tmpfs, private,
/// gone at logout), created `0700`; else `/tmp/seagents-<uid>`. Err when
/// neither can be had, with why.
pub(crate) fn runtime_dir() -> Result<PathBuf, String> {
    if let Some(base) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(base).join("seagents");
        if crate::fsutil::private_dir(&dir).is_ok() {
            return Ok(dir);
        }
    }
    // `/tmp` is shared: anyone may have made this name first. Only a real
    // directory of ours will do — a symlink or another user's directory
    // would hand them our seed prompts.
    crate::fsutil::tmp_dir_in(Path::new("/tmp"))
}

/// Session `key`'s hook settings file, `<runtime dir>/hooks-<key>.json`:
/// the merged `--settings` `run` passes Claude by path, so the JSON never
/// shows on the line herdr types into the pane (or in the shell's history).
pub(crate) fn hooks_file(key: &str) -> Result<PathBuf, String> {
    Ok(runtime_dir()?.join(format!("hooks-{key}.json")))
}

// ---------------------------------------------------------------------------
// Prompt files
// ---------------------------------------------------------------------------

/// Where a session's prompt files go: `<cwd>/.seagents/prompts`. Inside the
/// agent's own working directory because that is where it may read without
/// asking — a Read there needs no permission in any mode, where one in the
/// runtime or cache dir would stop a `default`-mode agent at a prompt.
fn prompts_dir(cwd: &Path) -> PathBuf {
    cwd.join(".seagents/prompts")
}

/// Deliver `body` to session `key` (running in `cwd`) by reference: write
/// it to a new prompt file and return the one line that stands in for it,
/// telling the agent to read that file — for a `send` body the tool would
/// file as quoted data if pasted (`adapter::Send::by_reference`, `stops` the
/// ledger's count as it is sent), and for a seed prompt, which is always sent
/// this way: the text a prompt is submitted with travels on a herdr command
/// line, which every local user can read in `/proc` while it runs.
pub(crate) fn by_reference(key: &str, cwd: &str, stops: u64, body: &str) -> Result<String, String> {
    let file = write_prompt(key, cwd, stops, body)?;
    Ok(reference_line(&file, cwd))
}

/// Write `body` to a new prompt file of session `key`,
/// `<cwd>/.seagents/prompts/<key>-<stops>-<4 hex>.md` (0600, dirs 0700), and
/// return its path. In a git work tree `.seagents/` is excluded first, so the
/// file never shows up as an untracked change another agent might commit; a
/// work tree whose exclude cannot be written refuses the prompt.
fn write_prompt(key: &str, cwd: &str, stops: u64, body: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = Path::new(cwd);
    if cwd.is_empty() || !dir.is_dir() {
        return Err(format!("the agent's working directory {cwd:?} is gone"));
    }
    exclude_seagents(dir)?;
    let dir = prompts_dir(dir);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    // Two sends at one `stops` are serialized by the send lock, but the suffix
    // is random; retry the rare collision rather than overwrite.
    for _ in 0..16 {
        let path = dir.join(format!("{key}-{stops}-{}.md", tag_suffix()));
        match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut f) => {
                return f
                    .write_all(body.as_bytes())
                    .map(|_| path.clone())
                    .map_err(|e| format!("{}: {e}", path.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Err(format!("{}: no free prompt file name", dir.display()))
}

/// The one line that stands in for a body sent by reference: where the
/// prompt file is — relative when it is under the agent's working directory,
/// as it is written to be — and that it came through seagents, so the agent
/// can tell instructions it was sent from text it merely found.
fn reference_line(file: &Path, cwd: &str) -> String {
    let at = match file.strip_prefix(cwd) {
        Ok(rel) if !cwd.is_empty() => format!("{} (relative to your working directory)", rel.display()),
        _ => file.display().to_string(),
    };
    format!("Read the file {at} and carry out the instructions in it. They were sent to you through seagents.")
}

/// The inverse of [`reference_line`]: the prompt file a stand-in line
/// names, as written there (relative to the agent's working directory, or
/// absolute) — for `read`, which can show the words the agent was only told
/// where to find.
pub(crate) fn referenced_file(text: &str) -> Option<&str> {
    let at = text
        .strip_prefix("Read the file ")?
        .split_once(" and carry out the instructions in it. They were sent to you through seagents")?
        .0;
    Some(at.strip_suffix(" (relative to your working directory)").unwrap_or(at))
}

/// Delete session `key`'s prompt files in `cwd` — those named
/// `<key>-<stops>-<4 hex>.md`, nothing else — and the prompts dir and
/// `.seagents/` when that leaves them empty.
pub(crate) fn remove_prompts(key: &str, cwd: &str) {
    if cwd.is_empty() || key.is_empty() {
        return;
    }
    let cwd = Path::new(cwd);
    let dir = prompts_dir(cwd);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_str().is_some_and(|n| is_prompt_of(n, key)) {
            let _ = fs::remove_file(e.path());
        }
    }
    // Only ever removes an empty directory.
    let _ = fs::remove_dir(&dir);
    let _ = fs::remove_dir(cwd.join(".seagents"));
}

/// Whether `name` is one of session `key`'s prompt files.
fn is_prompt_of(name: &str, key: &str) -> bool {
    let Some(rest) = name.strip_prefix(&format!("{key}-")).and_then(|r| r.strip_suffix(".md")) else {
        return false;
    };
    let Some((stops, hex)) = rest.split_once('-') else {
        return false;
    };
    !stops.is_empty()
        && stops.bytes().all(|b| b.is_ascii_digit())
        && hex.len() == 4
        && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Keep `.seagents/` (worktrees, prompt files) out of the git work tree
/// `dir` is in, through the repository's local `info/exclude` — never a
/// tracked file. Nothing to do outside a work tree.
pub(crate) fn exclude_seagents(dir: &Path) -> Result<(), String> {
    let common = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stderr(std::process::Stdio::null())
        .output();
    match common {
        Ok(out) if out.status.success() => {
            let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            add_exclude_line(&common.join("info/exclude"))
        }
        _ => Ok(()),
    }
}

/// Add `.seagents/` to a git `info/exclude`, once.
fn add_exclude_line(exclude: &Path) -> Result<(), String> {
    const LINE: &str = ".seagents/";
    let text = fs::read_to_string(exclude).unwrap_or_default();
    if text.lines().any(|l| l.trim() == LINE) {
        return Ok(());
    }
    if let Some(dir) = exclude.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let sep = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
    fs::write(exclude, format!("{text}{sep}{LINE}\n")).map_err(|e| format!("{}: {e}", exclude.display()))
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Locked read-modify-write of a session's `.meta`. Does nothing when the
/// session has no (parseable) `.meta` — an update never creates one — and
/// skips the write when `f` changed nothing.
pub(crate) fn update_meta(files: &Files, f: impl FnOnce(&mut Meta)) {
    // Checked before taking the lock, so an update racing the session's end
    // does not leave a stray `.lock` behind.
    if !files.meta.exists() {
        return;
    }
    // A lock we could not take still leaves the write atomic, and one
    // unlocked update beats a dropped window heal.
    let _lock = Lock::acquire(&files.lock);
    let Some(before) = read_meta(files) else {
        return;
    };
    let mut meta = before.clone();
    f(&mut meta);
    if meta != before {
        if let Ok(text) = serde_json::to_string_pretty(&meta) {
            let _ = write_atomic(&files.meta, text.as_bytes());
        }
    }
}

/// Record that the agent has been given a prompt, if nothing has yet: the
/// start of the window in which its hooks must have reported
/// ([`crate::hook::health`]).
pub(crate) fn note_engaged(files: &Files) {
    if read_meta(files).is_some_and(|m| m.engaged_at.is_some()) {
        return;
    }
    let now = crate::clock::unix_now();
    update_meta(files, |m| {
        m.engaged_at.get_or_insert(now);
    });
}

/// Locked read-modify-write of a session's `.agent`, creating it on the first
/// update. Does nothing once the session's `.meta` is gone: a hook firing as
/// its agent ends must not leave a stray `.agent` behind.
pub(crate) fn update_agent(files: &Files, f: impl FnOnce(&mut AgentRecord)) {
    if !files.meta.exists() {
        return;
    }
    // As in `update_state`: better an unlocked atomic write than a lost turn.
    let _lock = Lock::acquire(&files.lock);
    let before = read_agent(files);
    let mut record = before.clone();
    f(&mut record);
    if record != before {
        if let Ok(text) = serde_json::to_string_pretty(&record) {
            let _ = write_atomic(&files.agent, text.as_bytes());
        }
    }
}

/// Locked read-modify-write of a session's `.state`. `f` sees the current
/// record (None when there is none, or it does not parse) and may replace it;
/// the file is rewritten only if it did. Returns what `f` returns.
///
/// Once the session's `.meta` is gone `f` still decides (its answer is the
/// caller's), but nothing is written: an observer that looked at the agent
/// just before its session ended would otherwise recreate a `.state` and
/// `.lock` nobody removes.
pub(crate) fn update_state<R>(files: &Files, f: impl FnOnce(&mut Option<StateRecord>) -> R) -> R {
    if !files.meta.exists() {
        return f(&mut read_state(files));
    }
    // A lock we could not take still leaves the tmp+rename write atomic; the
    // caller's observation is better recorded unlocked than dropped.
    let _lock = Lock::acquire(&files.lock);
    let before = read_state(files);
    let mut record = before.clone();
    let out = f(&mut record);
    if record != before {
        if let Some(text) = record.as_ref().and_then(|r| serde_json::to_string(r).ok()) {
            let _ = write_atomic(&files.state, text.as_bytes());
        }
    }
    out
}

/// Session `files`' send lock, held for one whole `send` or `stop` — the wait
/// for an idle agent, the input-box check, the submission and its
/// confirmation — so two never interleave, from any processes. None when it
/// cannot be taken (the session's files are gone), or not within `within`
/// when given.
pub(crate) fn send_lock(files: &Files, within: Option<std::time::Duration>) -> Option<Lock> {
    if !files.meta.exists() {
        return None;
    }
    match within {
        Some(d) => Lock::acquire_within(&files.send_lock, d),
        None => Lock::acquire(&files.send_lock),
    }
}

/// Append `message` to `<cache>/<log>.log` (0600) — only under
/// `SEAGENTS_DEBUG`. For what runs where nothing can be printed: a hook (its
/// stdout is the agent's to read), and `run`'s background threads (stderr is
/// the attached agent's screen).
pub(crate) fn debug_log(log: &str, message: &str) {
    use std::io::Write;
    if std::env::var_os("SEAGENTS_DEBUG").is_none() {
        return;
    }
    if let Ok(mut f) = crate::fsutil::create_private(&cache_dir().join(format!("{log}.log")), true) {
        let _ = writeln!(f, "{} pid {}: {message}", crate::clock::unix_now(), std::process::id());
    }
}

/// An exclusive `flock` on a lock file, released on drop (closing the
/// descriptor releases it, which also covers a crash mid-update). `flock`
/// locks belong to the open file description, so two threads of one process
/// exclude each other just as two processes do. Also what serializes the
/// snapshot's writers (`snapshot::write_layout`).
#[derive(Debug)]
pub(crate) struct Lock {
    _held: File,
}

impl Lock {
    pub(crate) fn acquire(path: &Path) -> Option<Lock> {
        let file = Lock::open(path)?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Some(Lock { _held: file });
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return None;
            }
        }
    }

    /// [`Lock::acquire`], giving up after `within`.
    pub(crate) fn acquire_within(path: &Path, within: std::time::Duration) -> Option<Lock> {
        let file = Lock::open(path)?;
        let deadline = std::time::Instant::now() + within;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Some(Lock { _held: file });
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn open(path: &Path) -> Option<File> {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(path).ok()
    }
}
