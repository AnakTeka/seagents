//! The session cache, `~/.cache/seagents/<wrapper_pid>.*` — its one owner.
//!
//! Every sidecar is named, read and written here and nowhere else:
//!
//! - `.raw` — the tee. Append-only, written by the wrapper's output pump; this
//!   module only names it (and deletes it on reap).
//! - `.meta` — who and where: pids, argv, cwd, window handles, tag and
//!   lineage. Written by the wrapper at launch, then updated from any process:
//!   window healing from the wrapper's input pump *and* from the focus/HUD
//!   processes, tag backfill from whichever process runs a snapshot capture.
//! - `.state` — the last observed screen state, since when, and whether the
//!   user has seen it. Written by the wrapper's monitor and by every observer
//!   whose own detection disagrees (see `status::observe_state`).
//! - `.agent` — what the agent itself reports: which conversation it is
//!   writing, and the turn ledger ([`AgentRecord`]). Written by `__hook`,
//!   which the agent runs at its own lifecycle events. Kept apart from
//!   `.meta` because it churns on every prompt and turn end while `.meta`
//!   almost never changes.
//!
//! Because every sidecar has writers in several processes, every change is a
//! locked read-modify-write ([`update_meta`], [`update_state`],
//! [`update_agent`]): an
//! exclusive `flock` on the session's `.lock` sibling, read, mutate the typed
//! record, write a pid-suffixed tmp file, rename it over, unlock. Readers take
//! no lock — the rename means they see the old record or the new one, never a
//! torn one. tmp+rename alone fixes torn reads; only the lock fixes lost
//! updates (a healed con_id and a backfilled tag landing at once, one
//! silently dropping the other).
//!
//! A session's **prompt files** (`<cwd>/.seagents/prompts/<wrapper_pid>-…`,
//! [`write_prompt`]) live outside the cache, in the agent's working
//! directory, but are the session's all the same: named here, deleted with it.
//!
//! Liveness is one rule, [`procfs::alive`] on the child pid, and two
//! operations: [`list`] returns live sessions and never deletes anything;
//! [`reap`] deletes the files of dead ones. Only `status`/the HUD reap.
//!
//! Every session has a **tag** — its one name, the address every command
//! resolves ([`resolve_target`]), the window handle `restore` swallows on, the
//! lineage key. Tags are claimed under a cache-wide lock ([`claim_tag`]) held
//! from the uniqueness check through the `.meta`'s creation, which is the only
//! way to create one; two `run`s racing for a name cannot both get it.

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
/// agent still runs, untracked, rather than tee its screen into a directory
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

/// Where sessions live: the cache (every sidecar) and the runtime dir (their
/// control sockets, [`runtime_dir`]). Two roots because a socket belongs on
/// tmpfs and gone at logout, while `.meta` & co. must outlive a logout long
/// enough to be reaped into tombstones. Both are private (0700). A test puts
/// both in one scratch directory.
#[derive(Clone, Debug)]
pub(crate) struct Dirs {
    cache: PathBuf,
    /// Err: why there is no usable runtime dir (and so no sockets).
    runtime: Result<PathBuf, String>,
}

impl Dirs {
    /// This machine's: `~/.cache/seagents` and [`runtime_dir`].
    pub(crate) fn live() -> Dirs {
        Dirs {
            cache: cache_dir(),
            runtime: runtime_dir(),
        }
    }

    /// Everything in one directory.
    #[cfg(test)]
    pub(crate) fn single(dir: &Path) -> Dirs {
        Dirs {
            cache: dir.to_path_buf(),
            runtime: Ok(dir.to_path_buf()),
        }
    }

    /// A path in the cache.
    fn join(&self, name: &str) -> PathBuf {
        self.cache.join(name)
    }
}

impl AsRef<Path> for Dirs {
    /// The cache directory.
    fn as_ref(&self) -> &Path {
        &self.cache
    }
}

/// A session is keyed by the wrapper's pid; these are its sidecar files.
#[derive(Clone, Debug)]
pub(crate) struct Files {
    /// The teed output stream.
    pub(crate) raw: PathBuf,
    /// Where a rotation writes the tee's tail before renaming it over `raw`.
    pub(crate) raw_tmp: PathBuf,
    /// Identity: [`Meta`].
    pub(crate) meta: PathBuf,
    /// Last observed state: [`StateRecord`].
    pub(crate) state: PathBuf,
    /// Identity and the turn ledger: [`AgentRecord`].
    pub(crate) agent: PathBuf,
    /// Guards every read-modify-write of `meta`, `state` and `agent`.
    lock: PathBuf,
    /// The wrapper's control socket (`send`/`stop`), in the runtime dir; Err
    /// says why there can be none.
    pub(crate) sock: Result<PathBuf, String>,
}

impl Files {
    /// The files of session `wrapper_pid` on this machine.
    pub(crate) fn of(wrapper_pid: u64) -> Files {
        Files::in_dirs(&Dirs::live(), wrapper_pid)
    }

    pub(crate) fn in_dirs(dirs: &Dirs, wrapper_pid: u64) -> Files {
        let dir = &dirs.cache;
        Files {
            raw: dir.join(format!("{wrapper_pid}.raw")),
            raw_tmp: dir.join(format!("{wrapper_pid}.raw.tmp")),
            meta: dir.join(format!("{wrapper_pid}.meta")),
            state: dir.join(format!("{wrapper_pid}.state")),
            agent: dir.join(format!("{wrapper_pid}.agent")),
            lock: dir.join(format!("{wrapper_pid}.lock")),
            sock: dirs.runtime.as_ref().map(|d| d.join(format!("{wrapper_pid}.sock"))).map_err(Clone::clone),
        }
    }

    /// Every file of the session in one directory (tests).
    #[cfg(test)]
    pub(crate) fn in_dir(dir: impl AsRef<Path>, wrapper_pid: u64) -> Files {
        Files::in_dirs(&Dirs::single(dir.as_ref()), wrapper_pid)
    }
}

/// A session's identity, as its `.meta` records it.
///
/// Live wrappers are long-running processes that survive a reinstall, so a
/// `.meta` in the cache may have been written by an older or newer binary than
/// this one. Everything but `wrapper_pid` therefore defaults when missing, and
/// fields this binary does not know are carried in `extra` and written back
/// untouched, so an update from here never strips what another version added.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct Meta {
    pub(crate) wrapper_pid: u64,
    /// The `__lead` anchor: the agent's parent, same lifetime, same tty.
    #[serde(default)]
    pub(crate) child_pid: Option<u64>,
    /// Launch cwd — the grouping key.
    #[serde(default)]
    pub(crate) cwd: String,
    #[serde(default)]
    pub(crate) argv: Vec<String>,
    #[serde(default)]
    pub(crate) started: u64,
    /// Launch geometry, so the raw stream can be re-rendered at the right size
    /// when the live pty size is unavailable.
    #[serde(default)]
    pub(crate) cols: Option<u16>,
    #[serde(default)]
    pub(crate) rows: Option<u16>,
    /// The i3 window this agent runs in — the reliable handle for focus
    /// (titles can collide between idle agents). con_id is the fast handle but
    /// dies on an i3 restart; window_id is the X11 id, which survives one.
    #[serde(default)]
    pub(crate) con_id: Option<u64>,
    #[serde(default)]
    pub(crate) window_id: Option<u64>,
    /// The agent's name ([`claim_tag`]). Empty only in a `.meta` written by a
    /// binary from before tags, until a snapshot capture backfills one.
    #[serde(default)]
    pub(crate) tag: String,
    /// The spawner's tag; None when not started under another agent (or when
    /// a restore could not bring the lineage back).
    #[serde(default)]
    pub(crate) parent: Option<String>,
    /// Spawn generation: 0 without a parent, the parent's depth + 1 with one.
    #[serde(default)]
    pub(crate) depth: u32,
    /// `run` handed the agent our lifecycle hooks, so its `.agent` should
    /// fill in once it is prompted ([`crate::hook::health`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) hooks: bool,
    /// When the agent was first given a prompt (Unix s): its seed, once it
    /// got past any gate to take one, or a `send`. Set once
    /// ([`note_engaged`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) engaged_at: Option<u64>,
    /// Fields written by another version of seagents, preserved verbatim.
    #[serde(flatten)]
    pub(crate) extra: Map<String, Value>,
}

impl Meta {
    /// The agent binary's basename — `claude`, `codex`, … Empty when argv is.
    pub(crate) fn agent(&self) -> &str {
        self.argv.first().map(|a| basename(a)).unwrap_or("")
    }

    /// The launch directory's basename.
    pub(crate) fn dir(&self) -> &str {
        basename(&self.cwd)
    }
}

fn basename(p: &str) -> &str {
    Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
}

/// What the `.state` sidecar records: the last observed engine state word, when
/// it started, and whether the user has seen it since finishing (the "done vs
/// idle" distinction). The semantics live in `status`; this is only the file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct StateRecord {
    pub(crate) state: String,
    pub(crate) since: u64,
    #[serde(default = "seen_by_default")]
    pub(crate) seen: bool,
    /// For `blocked`: the detection rule that matched, which says which gate
    /// the agent waits at — what `wait` reports and `send` will refuse with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rule: Option<String>,
    /// The detector's own verdict on the screen, when it differs from
    /// `state` — which for a hook agent is the screen with the turn ledger
    /// folded in (a turn reads `working` while the screen may say idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) screen: Option<String>,
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
/// stays comparable for the wrapper's whole life.
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

impl Session {
    /// The agent's anchor pid. Every session the registry hands out has one:
    /// a `.meta` without it can be neither alive nor looked up by it.
    pub(crate) fn child_pid(&self) -> u64 {
        self.meta.child_pid.unwrap_or_default()
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

/// Every parseable `.meta` in `dir`, alive or not.
fn all_in(dir: &Dirs) -> Vec<Session> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Session> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("meta"))
        .filter_map(|e| {
            let text = fs::read_to_string(e.path()).ok()?;
            let meta: Meta = serde_json::from_str(&text).ok()?;
            let files = Files::in_dirs(dir, meta.wrapper_pid);
            Some(Session { files, meta })
        })
        .collect();
    out.sort_by_key(|s| s.meta.wrapper_pid);
    out
}

fn is_alive(meta: &Meta) -> bool {
    meta.child_pid.is_some_and(procfs::alive)
}

/// Every session whose agent is still running, by wrapper pid. Never deletes
/// anything, so it is safe from any process — a snapshot capture runs inside a
/// wrapper's monitor thread and has no business deleting other sessions.
pub(crate) fn list() -> Vec<Session> {
    list_in(&Dirs::live())
}

fn list_in(dir: &Dirs) -> Vec<Session> {
    all_in(dir)
        .into_iter()
        .filter(|s| is_alive(&s.meta))
        .collect()
}

/// Delete every file of every session whose agent has exited — its prompt
/// files in its cwd included ([`remove_prompts`]). A fully exited
/// agent isn't interesting — "idle" already means finished-and-waiting — so
/// the HUD and `status` only ever show live agents. A `.meta` that does not
/// parse is left alone: it is not known to be dead.
///
/// Also sweeps control sockets left by dead wrappers. A wrapper removes its
/// own on the way out, but one killed outright (SIGKILL, a crash) cannot, and
/// its `.meta` may already have been reaped by the time anyone notices.
/// The runtime dir is not this cache's alone — a seagents run with another
/// `HOME` (a test's scratch one, `sudo -E`, a second profile) shares it — so
/// "no session here owns it" is not enough: a socket goes only when its pid
/// is dead or provably not seagents ([`sweepable`]).
pub(crate) fn reap() {
    reap_in(&Dirs::live())
}

fn reap_in(dir: &Dirs) {
    let all = all_in(dir);
    let (live, dead): (Vec<&Session>, Vec<&Session>) = all.iter().partition(|s| is_alive(&s.meta));
    let live_tags: HashSet<&str> = live.iter().map(|s| s.meta.tag.as_str()).collect();
    let now = crate::clock::unix_now();
    for s in dead {
        // Written before anything goes, so a reader that finds the `.agent`
        // gone finds this ([`final_agent`]). A live agent holding the tag
        // again has superseded it.
        if !s.meta.tag.is_empty() && !live_tags.contains(s.meta.tag.as_str()) {
            let stone = Tombstone::of(s, now);
            if let Ok(text) = serde_json::to_vec(&stone) {
                let _ = write_atomic(&tombstone_path(dir, &s.meta.tag), &text);
            }
        }
        let f = &s.files;
        for path in [&f.meta, &f.raw, &f.raw_tmp, &f.state, &f.agent, &f.lock] {
            let _ = fs::remove_file(path);
        }
        if let (Ok(sock), true) = (&f.sock, sweepable(s.meta.wrapper_pid)) {
            let _ = fs::remove_file(sock);
        }
        remove_prompts(&s.meta);
    }
    prune_tombstones(dir, now);
    sweep_orphans(dir);
    let owned: HashSet<u64> = live.iter().map(|s| s.meta.wrapper_pid).collect();
    let Some(Ok(entries)) = dir.runtime.as_ref().ok().map(fs::read_dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let pid = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".sock"))
            .and_then(|p| p.parse::<u64>().ok());
        if pid.is_some_and(|p| !owned.contains(&p) && sweepable(p)) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Delete the sidecars no `.meta` claims — `<pid>.state`, `.lock`, `.agent`,
/// `.raw`, `.raw.tmp` — once that pid is dead. `reap` finds sessions by
/// their `.meta`, so these were never its to find: left by a writer that
/// raced a reap (before every update checked for the `.meta`), or by a
/// wrapper from before `.meta`s existed. A live pid keeps them: a wrapper
/// opens its tee before it writes its `.meta`.
fn sweep_orphans(dir: &Dirs) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some((pid, ext)) = name.to_str().and_then(|n| n.split_once('.')) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u64>() else {
            continue;
        };
        if matches!(ext, "state" | "lock" | "agent" | "raw" | "raw.tmp")
            && !dir.join(&format!("{pid}.meta")).exists()
            && !procfs::alive(pid)
        {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Whether `<wrapper_pid>.sock` may be swept: its wrapper is dead, or the pid
/// now runs something that is not seagents (reused). A live seagents process
/// may be a wrapper registered in another cache, and one that can't be read
/// isn't provably anything; both keep their socket — at worst a stale file
/// stays until the pid is gone.
fn sweepable(wrapper_pid: u64) -> bool {
    !crate::procfs::alive(wrapper_pid) || crate::procfs::runs_seagents(wrapper_pid) == Some(false)
}

/// The session whose anchor is `child_pid` — the handle `focus` and the HUD
/// address agents by. Not filtered on liveness: a lookup is not the place to
/// decide a session has ended.
pub(crate) fn by_child_pid(child_pid: u64) -> Option<Session> {
    all_in(&Dirs::live())
        .into_iter()
        .find(|s| s.meta.child_pid == Some(child_pid))
}

/// The live session named by `target`, as every command that addresses an
/// agent takes it: `@tag`, a bare `tag`, or an all-digits `child_pid` (the
/// handle `status --json` has always reported as `pid`). Tags are never all
/// digits, so the bare forms cannot be confused. The error is the message to
/// show the user.
pub(crate) fn resolve_target(target: &str) -> Result<Session, String> {
    resolve_target_in(&Dirs::live(), target)
}

fn resolve_target_in(dir: &Dirs, target: &str) -> Result<Session, String> {
    if !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()) {
        let pid: u64 = target
            .parse()
            .map_err(|_| format!("{target} is not a pid"))?;
        return all_in(dir)
            .into_iter()
            .find(|s| s.meta.child_pid == Some(pid))
            .ok_or_else(|| format!("no tracked agent with pid {pid}"));
    }
    let tag = target.strip_prefix('@').unwrap_or(target);
    if !valid_tag(tag) {
        return Err(format!(
            "{target:?} is neither a tag ([a-z0-9][a-z0-9-]{{0,31}}, not all digits) nor a pid"
        ));
    }
    live_by_tag(dir, tag).ok_or_else(|| format!("no live agent tagged {tag}"))
}

fn live_by_tag(dir: &Dirs, tag: &str) -> Option<Session> {
    list_in(dir).into_iter().find(|s| s.meta.tag == tag)
}

// ---------------------------------------------------------------------------
// Exited agents
// ---------------------------------------------------------------------------

/// Tombstones older than this are pruned by [`reap`].
const TOMBSTONE_TTL: u64 = 24 * 3600;

/// What is left of an agent once its session is reaped: `<tag>.exited` in
/// the cache, so a caller that waits on a child which has since exited — and
/// been cleaned up by whoever looked first — still gets its last result
/// rather than "no such agent". Written by [`reap`] (which deletes the
/// session), removed when a new agent claims the tag
/// ([`TagClaim::create_meta`]) and by [`reap`] after a day. Only `wait` and
/// `read` look at it ([`resolve_exited`]); everything that drives an agent
/// needs a live one.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct Tombstone {
    pub(crate) tag: String,
    pub(crate) wrapper_pid: u64,
    /// When the session was found gone (Unix s) — within one `status`/HUD
    /// poll of its exit, as they are what reap.
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
            wrapper_pid: s.meta.wrapper_pid,
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

fn tombstone_path(dir: &Dirs, tag: &str) -> PathBuf {
    dir.join(&format!("{tag}.exited"))
}

fn read_tombstone(dir: &Dirs, tag: &str) -> Option<Tombstone> {
    serde_json::from_str(&fs::read_to_string(tombstone_path(dir, tag)).ok()?).ok()
}

/// Remove tombstones past [`TOMBSTONE_TTL`], and any that do not parse.
fn prune_tombstones(dir: &Dirs, now: u64) {
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
/// of one that has been reaped.
#[derive(Debug)]
pub(crate) enum Found {
    Session(Session),
    Exited(Tombstone),
}

/// [`resolve_target`], then — for a tag no live agent holds — the newest
/// dead session holding it, then its tombstone. For `wait` and `read` only:
/// a finished child's result stays readable after it exits.
pub(crate) fn resolve_exited(target: &str) -> Result<Found, String> {
    resolve_exited_in(&Dirs::live(), target)
}

fn resolve_exited_in(dir: &Dirs, target: &str) -> Result<Found, String> {
    let err = match resolve_target_in(dir, target) {
        Ok(s) => return Ok(Found::Session(s)),
        Err(e) => e,
    };
    let tag = target.strip_prefix('@').unwrap_or(target);
    if !valid_tag(tag) {
        return Err(err);
    }
    let dead = all_in(dir)
        .into_iter()
        .filter(|s| s.meta.tag == tag)
        .max_by_key(|s| (s.meta.started, s.meta.wrapper_pid));
    if let Some(s) = dead {
        return Ok(Found::Session(s));
    }
    read_tombstone(dir, tag).map(Found::Exited).ok_or(err)
}

/// A session's ledger when it may have ended: its `.agent`, or — once a
/// reap has deleted that — what its tombstone kept. `reap` writes the
/// tombstone before it deletes anything, so one of the two is always there.
pub(crate) fn final_agent(session: &Session) -> AgentRecord {
    final_agent_in(&Dirs::live(), session)
}

fn final_agent_in(dir: &Dirs, session: &Session) -> AgentRecord {
    match fs::read_to_string(&session.files.agent) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => read_tombstone(dir, &session.meta.tag)
            .filter(|t| t.wrapper_pid == session.meta.wrapper_pid)
            .map(|t| t.ledger())
            .unwrap_or_default(),
    }
}

/// The live session process `pid` runs inside: the first of its `/proc`
/// ancestors that is a wrapper with a live `.meta` (for a hook: sh → the agent
/// → `__lead` → the wrapper). This is "who is calling" for every command that
/// cares — a hook finding its ledger, `spawn` deriving lineage, and
/// the control socket judging an `SO_PEERCRED` pid. `Ok(None)` is a human
/// caller; Err says why the caller cannot be told at all.
///
/// Human is a finding, never a default: it takes the whole ancestry, read up
/// to init, with no wrapper in it. A link that can't be read (the caller or
/// an ancestor already gone) is Err, and so is a caller that is no longer
/// the process that was walked — `pinned`, when given, must still be alive
/// once the walk is done (a pid read by number may have been reused), and a
/// second walk must find the same chain (an ancestor that died mid-walk
/// reparents its children, and its number could have been reused too). A
/// caller that connected, forked and exited used to read as a human here,
/// with a human's rights on the control socket.
///
/// Derived from ancestry rather than an environment variable, which
/// everything the agent runs inherits and can rewrite: unforgeable by
/// accident, though an agent that deliberately escapes its tree (double
/// fork, `systemd-run`) will look human — the line the threat model draws.
pub(crate) fn enclosing_session(pid: u64, pinned: Option<&procfs::Pinned>) -> Result<Option<Session>, String> {
    enclosing_session_in(&Dirs::live(), pid, || pinned.is_none_or(procfs::Pinned::alive))
}

fn enclosing_session_in(dir: &Dirs, pid: u64, still_there: impl Fn() -> bool) -> Result<Option<Session>, String> {
    let chain = procfs::ancestors(pid).ok_or_else(|| format!("the ancestry of pid {pid} cannot be read"))?;
    let found = nearest_session_in(dir, &chain);
    if !still_there() || procfs::ancestors(pid).as_ref() != Some(&chain) {
        return Err(format!("pid {pid} exited or changed parents while it was being identified"));
    }
    Ok(found)
}

/// The first session of `ancestors` (nearest first) with a live `.meta`.
fn nearest_session_in(dir: &Dirs, ancestors: &[u64]) -> Option<Session> {
    ancestors.iter().find_map(|&pid| {
        let files = Files::in_dirs(dir, pid);
        let meta = read_meta(&files).filter(|m| m.wrapper_pid == pid && is_alive(m))?;
        Some(Session { files, meta })
    })
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

/// Longest tag. It lands in an X11 property, a branch name and a filename.
const TAG_MAX: usize = 32;

/// `[a-z0-9][a-z0-9-]{0,31}`, and not all digits — so a bare target is never
/// ambiguous against a `child_pid`.
pub(crate) fn valid_tag(tag: &str) -> bool {
    let b = tag.as_bytes();
    !b.is_empty()
        && b.len() <= TAG_MAX
        && b[0] != b'-'
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !b.iter().all(|c| c.is_ascii_digit())
}

/// The name part of a minted tag: the cwd's basename folded into the tag
/// charset (runs of anything else become one `-`), short enough to leave room
/// for the `-xxxx` suffix. Never empty, never starts with `-`.
fn tag_base(cwd: &Path) -> String {
    let name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut base = String::new();
    for c in name.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
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

/// A tag nobody holds: `<cwd basename>-<4 hex>`, avoiding every live tag and
/// every tag in `reserved` (the snapshot's — a pending restore will want those
/// back). Only called under the tags lock.
fn mint_tag(cwd: &Path, taken: &HashSet<String>, reserved: &HashSet<String>) -> String {
    let base = tag_base(cwd);
    loop {
        let tag = format!("{base}-{}", tag_suffix());
        if !taken.contains(&tag) && !reserved.contains(&tag) {
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

/// The name a new session will be created under, with the cache-wide tags lock
/// still held. Only [`TagClaim::create_meta`] turns it into a session, and it
/// releases the lock when it has — so from the uniqueness check to the `.meta`
/// that makes the tag visible to the next claimant, nobody else can claim.
#[derive(Debug)]
pub(crate) struct TagClaim {
    pub(crate) tag: String,
    pub(crate) parent: Option<String>,
    pub(crate) depth: u32,
    /// Where a `spawn` reservation of this tag would be; consumed on create.
    pending: PathBuf,
    /// Where a previous holder's tombstone would be; superseded on create.
    exited: PathBuf,
    _lock: Lock,
}

impl TagClaim {
    /// Write the new session's `.meta` under the claimed tag and lineage, and
    /// release the claim. Atomic but not per-session locked: until this file
    /// exists nobody else can find the session, so there is nothing to race.
    pub(crate) fn create_meta(self, files: &Files, mut meta: Meta) {
        meta.tag = self.tag;
        meta.parent = self.parent;
        meta.depth = self.depth;
        write_new_meta(files, &meta);
        // The session now holds the tag itself; a `spawn` that reserved it
        // for us has nothing left to hold, and an agent that held it before
        // is no longer what the name means.
        let _ = fs::remove_file(&self.pending);
        let _ = fs::remove_file(&self.exited);
    }
}

/// Claim a tag for a session about to be created: `requested` (validated, and
/// refused while a live session holds it), or a freshly minted one. `parent`,
/// when given, must be a live session's tag; the child sits one generation
/// below it. The error is the message for the user.
///
/// A requested tag may be one the snapshot recorded (`reserved`): that is
/// precisely how `restore` relaunches an agent under its old name. Minting
/// avoids them, so a new agent never takes a placeholder a pending restore
/// will want back. It may also be one a `spawn` reserved ([`claim_spawn`]):
/// that is the spawned `run` taking the name it was launched with.
///
/// A child (`parent` given) is held to the fan-out caps ([`admit_child`]),
/// whoever runs it: `spawn`'s own `run`, `restore`, or a `run --parent`
/// typed by hand — which is otherwise a way round `spawn`'s caps.
pub(crate) fn claim_tag(
    requested: Option<&str>,
    parent: Option<&str>,
    cwd: &Path,
    reserved: &HashSet<String>,
) -> Result<TagClaim, String> {
    let caps = match parent {
        Some(_) => Caps::configured()?,
        None => Caps::default(),
    };
    claim_tag_in(&Dirs::live(), requested, parent, cwd, reserved, &caps)
}

fn claim_tag_in(
    dir: &Dirs,
    requested: Option<&str>,
    parent: Option<&str>,
    cwd: &Path,
    reserved: &HashSet<String>,
    caps: &Caps,
) -> Result<TagClaim, String> {
    check_requested(requested)?;
    // Unlike a session lock, this one must hold: without it two runs can both
    // pass the check. Failing to take it is failing to claim.
    let lock = Lock::acquire(&dir.join("tags.lock"))
        .ok_or("could not lock the tag registry")?;
    let live = list_in(dir);
    let pending = live_pending(dir);

    let (parent, depth) = match parent {
        None => (None, 0),
        Some(p) => {
            let p = p.strip_prefix('@').unwrap_or(p);
            let Some(ps) = live.iter().find(|s| s.meta.tag == p) else {
                return Err(format!("parent {p}: no live agent has that tag"));
            };
            admit_child(caps, &ps.meta, &live, &pending, requested)?;
            (Some(p.to_string()), ps.meta.depth + 1)
        }
    };

    let pending: HashSet<String> = pending.into_iter().map(|p| p.tag).collect();
    let live: HashSet<String> = live.into_iter().map(|s| s.meta.tag).collect();
    let tag = match requested {
        Some(t) if live.contains(t) => {
            return Err(format!("tag {t} is already held by a live agent"));
        }
        Some(t) => t.to_string(),
        None => mint_tag(cwd, &live, &reserved.union(&pending).cloned().collect()),
    };
    Ok(TagClaim {
        pending: pending_path(dir, &tag),
        exited: tombstone_path(dir, &tag),
        tag,
        parent,
        depth,
        _lock: lock,
    })
}

fn check_requested(requested: Option<&str>) -> Result<(), String> {
    match requested.filter(|t| !valid_tag(t)) {
        Some(tag) => Err(format!(
            "invalid tag {tag:?}: want [a-z0-9][a-z0-9-]{{0,31}}, not all digits"
        )),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Spawn reservations and fan-out caps
// ---------------------------------------------------------------------------

/// Fan-out limits for agent-spawned agents (ORCHESTRATION-SPEC "Fan-out
/// caps"). Depth alone would not stop an agent opening fifty windows at
/// depth 1, hence breadth and a global count too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Caps {
    /// Deepest generation a spawn may create.
    #[serde(rename = "max_depth")]
    pub(crate) depth: u32,
    /// Live children one agent may have.
    #[serde(rename = "max_children")]
    pub(crate) children: usize,
    /// Live agent-spawned agents, machine-wide.
    #[serde(rename = "max_spawned")]
    pub(crate) spawned: usize,
}

impl Caps {
    /// The user's caps: `[spawn] max_depth / max_children / max_spawned` in
    /// `~/.config/seagents/config.toml` (beside the adapter overrides), each
    /// defaulting when absent; the compiled defaults when there is no file.
    ///
    /// Never from the environment: the process asking for a child is the
    /// agent the caps bound, and everything it runs inherits — and can set —
    /// its environment (`SEAGENTS_MAX_SPAWNED=999 seagents spawn …` once lifted
    /// every cap). A file that does not parse, or names a key this binary
    /// doesn't know, is an error rather than silently the defaults: the user
    /// wrote it to change a limit.
    pub(crate) fn configured() -> Result<Caps, String> {
        let path = crate::vendor::herdr_detect::config::config_dir().join("config.toml");
        match fs::read_to_string(&path) {
            Ok(text) => Caps::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Caps::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    fn parse(text: &str) -> Result<Caps, String> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Config {
            spawn: Caps,
        }
        toml::from_str::<Config>(text).map(|c| c.spawn).map_err(|e| e.message().to_string())
    }
}

impl Default for Caps {
    fn default() -> Caps {
        Caps {
            depth: 3,
            children: 4,
            spawned: 12,
        }
    }
}

/// A tag `spawn` holds for a child whose `run` has not yet created its
/// `.meta`, recorded as `<tag>.pending` in the cache. `spawn` and the `run` it
/// launches are separate processes with a terminal between them, so the tags
/// lock cannot be held from the check to the `.meta`; the reservation is what
/// makes the tag — and the child's place under the caps — taken in between.
/// It lives as long as its spawner (a crashed `spawn` leaves nothing held).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Pending {
    tag: String,
    parent: Option<String>,
    spawner: u64,
}

fn pending_path(dir: &Dirs, tag: &str) -> PathBuf {
    dir.join(&format!("{tag}.pending"))
}

/// Every reservation whose spawner is still running. Only called under the
/// tags lock, which is also what makes deleting the dead ones here safe.
fn live_pending(dir: &Dirs) -> Vec<Pending> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("pending") {
            continue;
        }
        match fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Pending>(&t).ok()) {
            Some(p) if procfs::alive(p.spawner) => out.push(p),
            _ => {
                let _ = fs::remove_file(&path);
            }
        }
    }
    out
}

/// A `spawn`'s claim on its child's tag and lineage: the reservation file,
/// removed when this is dropped (the child's `run` removes it first when it
/// creates the `.meta`).
#[derive(Debug)]
pub(crate) struct SpawnClaim {
    pub(crate) tag: String,
    pub(crate) parent: Option<String>,
    file: PathBuf,
}

impl Drop for SpawnClaim {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
    }
}

/// Claim a tag for an agent `spawn` is about to launch, under the tags lock:
/// its lineage from the calling session (`caller`, a wrapper pid; None for a
/// human), the fan-out caps (agent callers only — they bound runaway agents,
/// and a human is not one), and the tag — `requested`, which must be free of
/// live agents, other spawns' reservations and the snapshot's tags
/// (`reserved`: a spawn must not take a placeholder a pending restore will
/// swallow, which `run --tag` alone allows), or one minted avoiding all three.
pub(crate) fn claim_spawn(
    requested: Option<&str>,
    caller: Option<u64>,
    cwd: &Path,
    reserved: &HashSet<String>,
) -> Result<SpawnClaim, String> {
    let caps = match caller {
        Some(_) => Caps::configured()?,
        None => Caps::default(),
    };
    claim_spawn_in(&Dirs::live(), requested, caller, cwd, reserved, &caps, std::process::id())
}

fn claim_spawn_in(
    dir: &Dirs,
    requested: Option<&str>,
    caller: Option<u64>,
    cwd: &Path,
    reserved: &HashSet<String>,
    caps: &Caps,
    spawner: u32,
) -> Result<SpawnClaim, String> {
    check_requested(requested)?;
    let _lock = Lock::acquire(&dir.join("tags.lock")).ok_or("could not lock the tag registry")?;
    let live = list_in(dir);
    let pending = live_pending(dir);

    let parent = match caller {
        None => None,
        Some(pid) => {
            let Some(me) = live.iter().find(|s| s.meta.wrapper_pid == pid) else {
                return Err("the calling agent's session is gone".to_string());
            };
            admit_child(caps, &me.meta, &live, &pending, None)?;
            Some(me.meta.tag.clone())
        }
    };

    let taken: HashSet<String> = live
        .into_iter()
        .map(|s| s.meta.tag)
        .chain(pending.into_iter().map(|p| p.tag))
        .collect();
    let tag = match requested {
        Some(t) if taken.contains(t) => return Err(format!("tag {t} is already held by a live agent")),
        Some(t) if reserved.contains(t) => {
            return Err(format!(
                "tag {t} belongs to an agent in the saved layout that `restore` would bring back; pick another"
            ))
        }
        Some(t) => t.to_string(),
        None => mint_tag(cwd, &taken, reserved),
    };
    let file = pending_path(dir, &tag);
    let record = Pending {
        tag: tag.clone(),
        parent: parent.clone(),
        spawner: u64::from(spawner),
    };
    let text = serde_json::to_string(&record).map_err(|e| e.to_string())?;
    write_atomic(&file, text.as_bytes()).map_err(|e| format!("reserving tag {tag}: {e}"))?;
    Ok(SpawnClaim { tag, parent, file })
}

/// Whether agent `parent` may have one more child: the one cap check, made
/// under the tags lock by a `spawn` reserving a child ([`claim_spawn`]) and
/// by any `run --parent` registering one ([`claim_tag`]). Counts every live
/// agent and every reservation, except a reservation of `claiming` itself —
/// that is this very child, the `run` a `spawn` launched taking the tag it
/// reserved, and the spawn already counted it.
fn admit_child(
    caps: &Caps,
    parent: &Meta,
    live: &[Session],
    pending: &[Pending],
    claiming: Option<&str>,
) -> Result<(), String> {
    let lineage: Vec<(String, Option<String>)> = live
        .iter()
        .map(|s| (s.meta.tag.clone(), s.meta.parent.clone()))
        .chain(
            pending
                .iter()
                .filter(|p| Some(p.tag.as_str()) != claiming)
                .map(|p| (p.tag.clone(), p.parent.clone())),
        )
        .collect();
    check_caps(caps, &parent.tag, parent.depth, &lineage)
}

/// Whether agent `parent` (at `depth`) may have one more child, given every
/// live or reserved agent's (tag, parent). The error names what is in the way,
/// so the caller can `wait` on a child instead of piling on.
fn check_caps(
    caps: &Caps,
    parent: &str,
    depth: u32,
    lineage: &[(String, Option<String>)],
) -> Result<(), String> {
    if depth + 1 > caps.depth {
        return Err(format!(
            "{parent} is at depth {depth}: a child would be depth {}, past the cap of {} \
             (the user's max_depth)",
            depth + 1,
            caps.depth
        ));
    }
    let children: Vec<&str> = lineage
        .iter()
        .filter(|(_, p)| p.as_deref() == Some(parent))
        .map(|(t, _)| t.as_str())
        .collect();
    if children.len() >= caps.children {
        return Err(format!(
            "{parent} already has {} live {} ({}), the cap is {} (the user's max_children); \
             wait on one of them first",
            children.len(),
            if children.len() == 1 { "child" } else { "children" },
            children.join(", "),
            caps.children
        ));
    }
    let spawned: Vec<&str> = lineage
        .iter()
        .filter(|(_, p)| p.is_some())
        .map(|(t, _)| t.as_str())
        .collect();
    if spawned.len() >= caps.spawned {
        return Err(format!(
            "{} agent-spawned agents are live ({}), the cap is {} (the user's max_spawned)",
            spawned.len(),
            spawned.join(", "),
            caps.spawned
        ));
    }
    Ok(())
}

/// Where per-session runtime files go — spawn's seed prompts and the
/// wrappers' control sockets — decided here alone, so the wrapper binding a
/// socket and every client connecting to it agree:
/// `$XDG_RUNTIME_DIR/seagents` (tmpfs, private, gone at logout), created
/// `0700`; else `/tmp/seagents-<uid>`. Err when neither can be had, with why.
pub(crate) fn runtime_dir() -> Result<PathBuf, String> {
    runtime_dir_in(std::env::var_os("XDG_RUNTIME_DIR"), Path::new("/tmp"))
}

/// The longest socket name a runtime dir must leave room for: a pid is at
/// most a `u32` (`pid_max` tops out at 2^22 today), plus `.sock`.
const SOCK_NAME_MAX: usize = "4294967295.sock".len();

/// A `sockaddr_un`'s `sun_path`: 108 bytes, the terminating NUL included.
const SUN_PATH: usize = 108;

fn runtime_dir_in(xdg: Option<std::ffi::OsString>, tmp: &Path) -> Result<PathBuf, String> {
    // A socket path that does not fit `sun_path` cannot be bound at all, so
    // an XDG_RUNTIME_DIR that deep (a nested test desktop, a container's
    // bind mount) is as unusable as none.
    let fits = |dir: &Path| dir.as_os_str().len() + 1 + SOCK_NAME_MAX < SUN_PATH;
    if let Some(base) = xdg.filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(base).join("seagents");
        if fits(&dir) && crate::fsutil::private_dir(&dir).is_ok() {
            return Ok(dir);
        }
    }
    // `/tmp` is shared: anyone may have made this name first. Only a real
    // directory of ours will do — a symlink or another user's directory
    // would hand them our sockets and seed prompts.
    let dir = crate::fsutil::tmp_dir_in(tmp)?;
    if !fits(&dir) {
        return Err(format!("{} is too long for a socket path", dir.display()));
    }
    Ok(dir)
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

/// Write `body` to a new prompt file of session `meta`,
/// `<wrapper_pid>-<stops>-<4 hex>.md` (0600, dirs 0700), and return its
/// path. A body some tools would file as quoted data if pasted (see
/// `adapter::Send::by_reference`) travels this way instead, and the agent is
/// told to read it. In a git work tree `.seagents/` is excluded first, so
/// the file never shows up as an untracked change another agent might
/// commit; a work tree whose exclude cannot be written refuses the prompt.
pub(crate) fn write_prompt(meta: &Meta, stops: u64, body: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let cwd = Path::new(&meta.cwd);
    if meta.cwd.is_empty() || !cwd.is_dir() {
        return Err(format!("the agent's working directory {:?} is gone", meta.cwd));
    }
    exclude_seagents(cwd)?;
    let dir = prompts_dir(cwd);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    // Two sends at one `stops` are serialized by the wrapper, but the suffix
    // is random; retry the rare collision rather than overwrite.
    for _ in 0..16 {
        let path = dir.join(format!("{}-{stops}-{}.md", meta.wrapper_pid, tag_suffix()));
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

/// Delete session `meta`'s prompt files — those named
/// `<its wrapper_pid>-<stops>-<4 hex>.md`, nothing else — and the prompts
/// dir and `.seagents/` when that leaves them empty. Called when the
/// wrapper's agent exits and by [`reap`] for a wrapper that could not.
pub(crate) fn remove_prompts(meta: &Meta) {
    if meta.cwd.is_empty() {
        return;
    }
    let cwd = Path::new(&meta.cwd);
    let dir = prompts_dir(cwd);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_str().is_some_and(|n| is_prompt_of(n, meta.wrapper_pid)) {
            let _ = fs::remove_file(e.path());
        }
    }
    // Only ever removes an empty directory.
    let _ = fs::remove_dir(&dir);
    let _ = fs::remove_dir(cwd.join(".seagents"));
}

/// Whether `name` is one of wrapper `pid`'s prompt files.
fn is_prompt_of(name: &str, pid: u64) -> bool {
    let Some(rest) = name
        .strip_prefix(&format!("{pid}-"))
        .and_then(|r| r.strip_suffix(".md"))
    else {
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

/// Give every live session that has no tag one — sessions whose wrapper
/// predates tags. Called by the snapshot capture, which cannot record an
/// agent without a name for its placeholder; the tag lands in the `.meta`, so
/// the agent is also addressable and a later restore sees it as running.
pub(crate) fn backfill_tags(sessions: &mut [Session], reserved: &HashSet<String>) {
    if sessions.iter().all(|s| !s.meta.tag.is_empty()) {
        return;
    }
    backfill_tags_in(&Dirs::live(), sessions, reserved)
}

fn backfill_tags_in(dir: &Dirs, sessions: &mut [Session], reserved: &HashSet<String>) {
    let Some(_lock) = Lock::acquire(&dir.join("tags.lock")) else {
        return;
    };
    let mut taken: HashSet<String> = list_in(dir).into_iter().map(|s| s.meta.tag).collect();
    for s in sessions.iter_mut().filter(|s| s.meta.tag.is_empty()) {
        let tag = mint_tag(Path::new(&s.meta.cwd), &taken, reserved);
        let mut kept = None;
        update_meta(&s.files, |m| {
            // Another capture may have got here first; its tag stands.
            if m.tag.is_empty() {
                m.tag = tag.clone();
            }
            kept = Some(m.tag.clone());
        });
        if let Some(tag) = kept {
            taken.insert(tag.clone());
            s.meta.tag = tag;
        }
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn write_new_meta(files: &Files, meta: &Meta) {
    if let Ok(text) = serde_json::to_string_pretty(meta) {
        let _ = write_atomic(&files.meta, text.as_bytes());
    }
}

/// Locked read-modify-write of a session's `.meta`. Does nothing when the
/// session has no (parseable) `.meta` — an update never creates one — and
/// skips the write when `f` changed nothing.
pub(crate) fn update_meta(files: &Files, f: impl FnOnce(&mut Meta)) {
    // Checked before taking the lock, so an update racing a reap does not
    // leave a stray `.lock` behind for a session that is gone.
    if !files.meta.exists() {
        return;
    }
    // As in `update_state`: a lock we could not take still leaves the write
    // atomic, and one unlocked update beats a dropped window heal.
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
/// its agent is reaped must not leave a stray `.agent` behind.
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
/// caller's), but nothing is written: an observer that detected the agent
/// just before a reap would otherwise recreate a `.state` and `.lock` that
/// no reap removes (it goes by `.meta`s), and that a later wrapper given the
/// same pid would inherit as its own.
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

/// An exclusive `flock` on a session's `.lock` file, released on drop (closing
/// the descriptor releases it, which also covers a crash mid-update). `flock`
/// locks belong to the open file description, so two threads of one process
/// exclude each other just as two processes do. Also what serializes the
/// snapshot's writers (`snapshot::write_layout`).
#[derive(Debug)]
pub(crate) struct Lock {
    _held: File,
}

impl Lock {
    pub(crate) fn acquire(path: &Path) -> Option<Lock> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)
            .ok()?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Some(Lock { _held: file });
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Dirs {
        let dir = std::env::temp_dir().join(format!("seagents-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Dirs::single(&dir)
    }

    fn sample(wrapper_pid: u64, child_pid: Option<u64>) -> Meta {
        Meta {
            wrapper_pid,
            child_pid,
            cwd: "/home/u/proj".into(),
            argv: vec!["/usr/bin/claude".into(), "--model".into(), "x".into()],
            started: 100,
            cols: Some(80),
            rows: Some(24),
            ..Meta::default()
        }
    }

    #[test]
    fn concurrent_updates_lose_nothing() {
        let dir = scratch("concurrent");
        let files = Files::in_dir(&dir, 1);
        write_new_meta(&files, &sample(1, Some(2)));

        const THREADS: u64 = 16;
        const ROUNDS: u64 = 25;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let files = files.clone();
                std::thread::spawn(move || {
                    for _ in 0..ROUNDS {
                        update_meta(&files, |m| {
                            let n = m.extra.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                            m.extra.insert("n".into(), (n + 1).into());
                            m.extra.insert(format!("t{t}"), true.into());
                        });
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let meta = read_meta(&files).unwrap();
        assert_eq!(meta.extra["n"].as_u64(), Some(THREADS * ROUNDS));
        for t in 0..THREADS {
            assert_eq!(meta.extra[&format!("t{t}")], Value::Bool(true));
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_older_meta_parses_and_keeps_fields_this_binary_does_not_know() {
        let dir = scratch("oldmeta");
        let files = Files::in_dir(&dir, 7);
        // No geometry, no window ids, no tag (a pre-tag wrapper) — and the
        // conversation id a pre-hook binary cached here, which this one no
        // longer reads from `.meta` but must not strip.
        fs::write(
            &files.meta,
            r#"{"wrapper_pid":7,"child_pid":8,"cwd":"/p","argv":["claude"],"started":5,
                "session_id":"abc","session_id_source":"proc"}"#,
        )
        .unwrap();

        let meta = read_meta(&files).expect("older meta parses");
        assert_eq!(
            (meta.child_pid, meta.cols, meta.con_id),
            (Some(8), None, None)
        );
        assert_eq!((meta.tag.as_str(), meta.parent.as_deref(), meta.depth), ("", None, 0));

        update_meta(&files, |m| m.con_id = Some(42));
        let v: Value = serde_json::from_str(&fs::read_to_string(&files.meta).unwrap()).unwrap();
        assert_eq!(v["con_id"].as_u64(), Some(42));
        assert_eq!(v["session_id"].as_str(), Some("abc"));
        assert_eq!(v["session_id_source"].as_str(), Some("proc"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn meta_round_trips_every_field() {
        let dir = scratch("roundtrip");
        let files = Files::in_dir(&dir, 1);
        let mut meta = sample(1, Some(2));
        meta.con_id = Some(3);
        meta.window_id = Some(4);
        meta.tag = "reviewer".into();
        meta.parent = Some("planner".into());
        meta.depth = 1;
        write_new_meta(&files, &meta);
        assert_eq!(read_meta(&files), Some(meta));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writes_leave_no_tmp_files_behind() {
        let dir = scratch("notmp");
        let files = Files::in_dir(&dir, 1);
        write_new_meta(&files, &sample(1, Some(2)));
        update_meta(&files, |m| m.con_id = Some(9));
        update_state(&files, |r| {
            *r = Some(StateRecord {
                state: "idle".into(),
                since: 1,
                seen: true,
                rule: None,
                screen: None,
            })
        });
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["1.lock", "1.meta", "1.state"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_update_never_creates_a_meta() {
        let dir = scratch("nocreate");
        let files = Files::in_dir(&dir, 1);
        update_meta(&files, |m| m.con_id = Some(1));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_never_deletes_and_reap_removes_only_the_dead() {
        let dir = scratch("reap");
        let me = std::process::id() as u64;
        let dead = u32::MAX as u64; // no such pid
        let live = Files::in_dir(&dir, 1);
        // Its wrapper is dead too, as a dead session's usually is.
        let gone_wrapper = u32::MAX as u64 - 2;
        let gone = Files::in_dir(&dir, gone_wrapper);
        write_new_meta(&live, &sample(1, Some(me)));
        write_new_meta(&gone, &sample(gone_wrapper, Some(dead)));
        for f in [&live, &gone] {
            fs::write(&f.raw, b"x").unwrap();
            update_state(f, |r| {
                *r = Some(StateRecord {
                    state: "idle".into(),
                    since: 1,
                    seen: true,
                    rule: None,
                    screen: None,
                })
            });
        }
        // Unparseable: not known to be dead, so never reaped.
        fs::write(dir.join("3.meta"), "{torn").unwrap();
        // Sockets: the live session's stays; the dead one's goes, and so does one whose session left no
        // `.meta` at all (a SIGKILLed wrapper: a dead pid). A socket of a
        // live seagents process registered in no session here — a wrapper
        // run with another HOME — stays, as does one whose pid can't be read.
        // A live process that is not seagents holds a reused pid: swept.
        let (live_sock, gone_sock) = (live.sock.clone().unwrap(), gone.sock.clone().unwrap());
        for f in [&live_sock, &gone_sock] {
            fs::write(f, b"").unwrap();
        }
        let orphan = dir.join(&format!("{}.sock", u32::MAX - 1));
        let foreign = dir.join(&format!("{me}.sock"));
        let mut sleep = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let reused = dir.join(&format!("{}.sock", sleep.id()));
        for f in [&orphan, &foreign, &reused] {
            fs::write(f, b"").unwrap();
        }

        let listed: Vec<u64> = list_in(&dir).iter().map(|s| s.meta.wrapper_pid).collect();
        assert_eq!(listed, [1]);
        assert!(gone.meta.exists(), "list must not reap");

        reap_in(&dir);
        sleep.kill().ok();
        sleep.wait().ok();
        for path in [&gone.meta, &gone.raw, &gone.state, &gone.lock, &gone_sock, &orphan, &reused] {
            assert!(!path.exists(), "{} should be reaped", path.display());
        }
        for path in [&live.meta, &live.raw, &live.state, &live_sock, &foreign] {
            assert!(path.exists(), "{} should survive", path.display());
        }
        assert!(dir.join("3.meta").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_runtime_dir_falls_back_to_a_private_tmp_dir_when_sockets_would_not_fit() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let base = std::env::temp_dir().join(format!("sg-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (xdg, tmp) = (base.join("xdg"), base.join("tmp"));
        fs::create_dir_all(&xdg).unwrap();
        fs::create_dir_all(&tmp).unwrap();
        let ours = tmp.join(format!("seagents-{}", unsafe { libc::getuid() }));

        // A short runtime dir is used as ever.
        let got = runtime_dir_in(Some(xdg.clone().into()), &tmp).unwrap();
        assert_eq!(got, xdg.join("seagents"));
        assert_eq!(fs::metadata(&got).unwrap().mode() & 0o777, 0o700);

        // One whose sockets would overflow `sun_path` is passed over, as is
        // an unset or empty one.
        let long = xdg.join("d".repeat(SUN_PATH));
        for v in [Some(long.clone().into_os_string()), Some("".into()), None] {
            assert_eq!(runtime_dir_in(v, &tmp).unwrap(), ours);
        }
        assert!(!long.exists(), "a too-long runtime dir is not even created");
        let sock = ours.join("4294967295.sock");
        assert!(sock.as_os_str().len() < SUN_PATH);
        assert_eq!(fs::metadata(&ours).unwrap().mode() & 0o777, 0o700);

        // A loosened mode on our own directory is tightened again.
        fs::set_permissions(&ours, fs::Permissions::from_mode(0o755)).unwrap();
        runtime_dir_in(None, &tmp).unwrap();
        assert_eq!(fs::metadata(&ours).unwrap().mode() & 0o777, 0o700);

        // A symlink planted in its place is refused, not followed.
        fs::remove_dir(&ours).unwrap();
        std::os::unix::fs::symlink(&xdg, &ours).unwrap();
        assert!(runtime_dir_in(None, &tmp).unwrap_err().contains("refusing"));
        fs::remove_file(&ours).unwrap();
        // So is a plain file.
        fs::write(&ours, b"").unwrap();
        assert!(runtime_dir_in(None, &tmp).unwrap_err().contains("refusing"));

        fs::remove_dir_all(&base).ok();
    }

    fn claim(dir: &Dirs, requested: Option<&str>, parent: Option<&str>) -> Result<TagClaim, String> {
        claim_tag_in(dir, requested, parent, Path::new("/home/u/My Project!"), &HashSet::new(), &Caps::default())
    }

    /// Claim and create a live session (child = this test process).
    fn create(dir: &Dirs, wrapper_pid: u64, requested: Option<&str>, parent: Option<&str>) -> Result<Meta, String> {
        let c = claim(dir, requested, parent)?;
        let files = Files::in_dir(dir, wrapper_pid);
        c.create_meta(&files, sample(wrapper_pid, Some(std::process::id() as u64)));
        Ok(read_meta(&files).unwrap())
    }

    #[test]
    fn tag_validation() {
        for ok in ["a", "reviewer", "x-1", "0a", "1-2", "seagents-9f3c", &"a".repeat(32)] {
            assert!(valid_tag(ok), "{ok}");
        }
        for bad in ["", "-a", "A", "a_b", "a b", "123", "0", "é", "@a", &"a".repeat(33)] {
            assert!(!valid_tag(bad), "{bad}");
        }
    }

    #[test]
    fn minted_tags_are_the_cwd_basename_plus_four_hex_and_always_valid() {
        assert_eq!(tag_base(Path::new("/home/u/My Project!")), "my-project");
        assert_eq!(tag_base(Path::new("/srv/__x__y")), "x-y");
        assert_eq!(tag_base(Path::new("/")), "agent");
        assert_eq!(tag_base(Path::new("/p/日本")), "agent");
        assert_eq!(tag_base(Path::new("/p/2024")), "2024");
        assert_eq!(tag_base(Path::new(&format!("/p/{}", "a".repeat(40)))).len(), 27);
        for cwd in ["/home/u/seagents", "/", "/p/2024", "/p/-x-", &format!("/p/{}", "b-".repeat(30))] {
            let tag = mint_tag(Path::new(cwd), &HashSet::new(), &HashSet::new());
            assert!(valid_tag(&tag), "{cwd} -> {tag}");
            let (_, hex) = tag.rsplit_once('-').unwrap();
            assert!(hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit()), "{tag}");
        }
    }

    #[test]
    fn a_minted_tag_is_recognised_as_minted_and_a_chosen_one_is_not() {
        let cwd = Path::new("/home/u/My Project!");
        let tag = mint_tag(cwd, &HashSet::new(), &HashSet::new());
        assert!(minted_from(&tag, cwd), "{tag}");
        assert!(minted_from("my-project-00af", cwd));
        for chosen in ["reviewer", "my-project", "my-project-x", "my-project-00afx", "my-project-zzzz", "other-00af"] {
            assert!(!minted_from(chosen, cwd), "{chosen}");
        }
    }

    #[test]
    fn claims_refuse_bad_and_live_tags_and_derive_depth_from_the_parent() {
        let dir = scratch("claim");
        let a = create(&dir, 1, Some("planner"), None).unwrap();
        assert_eq!((a.tag.as_str(), a.parent, a.depth), ("planner", None, 0));

        assert!(claim(&dir, Some("planner"), None).unwrap_err().contains("already held"));
        assert!(claim(&dir, Some("123"), None).unwrap_err().contains("invalid tag"));
        assert!(claim(&dir, None, Some("nobody")).unwrap_err().contains("no live agent"));

        let b = create(&dir, 2, Some("reviewer"), Some("@planner")).unwrap();
        assert_eq!((b.parent.as_deref(), b.depth), (Some("planner"), 1));
        let c = create(&dir, 3, None, Some("reviewer")).unwrap();
        assert_eq!((c.parent.as_deref(), c.depth), (Some("reviewer"), 2));
        assert!(c.tag.starts_with("my-project-"), "{}", c.tag);

        // A dead session's tag is free again.
        let dead = Files::in_dir(&dir, 4);
        let mut m = sample(4, Some(u32::MAX as u64));
        m.tag = "gone".into();
        write_new_meta(&dead, &m);
        assert!(create(&dir, 5, Some("gone"), None).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn minting_avoids_snapshot_tags_but_a_restore_may_claim_one() {
        let dir = scratch("reserved");
        let reserved: HashSet<String> = (0..=0xffffu32)
            .filter(|n| *n != 0x2a)
            .map(|n| format!("p-{n:04x}"))
            .collect();
        let c = claim_tag_in(&dir, None, None, Path::new("/p"), &reserved, &Caps::default()).unwrap();
        assert_eq!(c.tag, "p-002a");
        drop(c);
        assert!(claim_tag_in(&dir, Some("p-0001"), None, Path::new("/p"), &reserved, &Caps::default()).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_claims_of_one_tag_never_both_succeed() {
        let dir = scratch("claimrace");
        const THREADS: u64 = 16;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let one = create(&dir, 100 + t, Some("same"), None).is_ok();
                    let minted = create(&dir, 200 + t, None, None).unwrap().tag;
                    (one, minted)
                })
            })
            .collect();
        let results: Vec<(bool, String)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|(ok, _)| *ok).count(), 1);
        let minted: HashSet<&String> = results.iter().map(|(_, t)| t).collect();
        assert_eq!(minted.len(), THREADS as usize);
        let tags: Vec<String> = list_in(&dir).into_iter().map(|s| s.meta.tag).collect();
        assert_eq!(tags.iter().filter(|t| *t == "same").count(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backfill_names_only_the_untagged_and_keeps_names_unique() {
        let dir = scratch("backfill");
        let me = std::process::id() as u64;
        let mut old = sample(1, Some(me));
        old.cwd = "/p/proj".into();
        write_new_meta(&Files::in_dir(&dir, 1), &old);
        create(&dir, 2, Some("kept"), None).unwrap();
        let mut sessions = list_in(&dir);
        backfill_tags_in(&dir, &mut sessions, &HashSet::new());
        assert!(sessions[0].meta.tag.starts_with("proj-"), "{}", sessions[0].meta.tag);
        assert_eq!(sessions[1].meta.tag, "kept");
        let on_disk = read_meta(&Files::in_dir(&dir, 1)).unwrap().tag;
        assert_eq!(on_disk, sessions[0].meta.tag);
        // Idempotent: a second pass leaves the name alone.
        let mut again = list_in(&dir);
        backfill_tags_in(&dir, &mut again, &HashSet::new());
        assert_eq!(again[0].meta.tag, on_disk);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn targets_resolve_by_tag_at_tag_or_child_pid() {
        let dir = scratch("resolve");
        let me = std::process::id() as u64;
        create(&dir, 1, Some("reviewer"), None).unwrap();
        let mut dead = sample(2, Some(u32::MAX as u64));
        dead.tag = "ghost".into();
        write_new_meta(&Files::in_dir(&dir, 2), &dead);

        for t in ["reviewer", "@reviewer", &me.to_string()] {
            let s = resolve_target_in(&dir, t).unwrap_or_else(|e| panic!("{t}: {e}"));
            assert_eq!(s.meta.wrapper_pid, 1, "{t}");
        }
        // Tags resolve over live sessions only.
        assert!(resolve_target_in(&dir, "ghost").unwrap_err().contains("no live agent"));
        assert!(resolve_target_in(&dir, "nobody").unwrap_err().contains("no live agent"));
        assert!(resolve_target_in(&dir, "999999999").unwrap_err().contains("pid"));
        assert!(resolve_target_in(&dir, "Bad Tag").unwrap_err().contains("neither"));
        assert!(resolve_target_in(&dir, "@").is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agent_updates_need_a_live_meta_and_keep_unknown_fields() {
        let dir = scratch("agent");
        let files = Files::in_dir(&dir, 1);
        update_agent(&files, |r| r.stops = 1);
        assert!(!files.agent.exists(), "no .meta, no .agent");
        assert_eq!(read_agent(&files), AgentRecord::default());

        write_new_meta(&files, &sample(1, Some(2)));
        update_agent(&files, |r| r.stops += 1);
        update_agent(&files, |r| r.stops += 1);
        assert_eq!(read_agent(&files).stops, 2);

        // A field from another version survives an update.
        let mut v: Value = serde_json::from_str(&fs::read_to_string(&files.agent).unwrap()).unwrap();
        v["turn"] = 9.into();
        fs::write(&files.agent, v.to_string()).unwrap();
        update_agent(&files, |r| r.session_id = Some("s".into()));
        let v: Value = serde_json::from_str(&fs::read_to_string(&files.agent).unwrap()).unwrap();
        assert_eq!((v["turn"].as_u64(), v["stops"].as_u64()), (Some(9), Some(2)));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reap_takes_the_agent_ledger_with_the_session() {
        let dir = scratch("reapagent");
        let gone = Files::in_dir(&dir, 2);
        write_new_meta(&gone, &sample(2, Some(u32::MAX as u64)));
        update_agent(&gone, |r| r.stops = 1);
        assert!(gone.agent.exists());
        reap_in(&dir);
        assert!(!gone.agent.exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_recreates_a_reaped_sessions_state_and_reap_sweeps_orphans() {
        let dir = scratch("orphans");
        let dead = u32::MAX as u64;
        let gone = Files::in_dir(&dir, dead);
        write_new_meta(&gone, &sample(dead, Some(dead)));
        reap_in(&dir);
        // An observer that detected the agent a moment before the reap:
        // it gets its answer, and writes nothing.
        let seen = update_state(&gone, |r| {
            *r = Some(StateRecord { state: "idle".into(), since: 1, seen: true, rule: None, screen: None });
            7
        });
        assert_eq!(seen, 7);
        update_agent(&gone, |r| r.stops = 1);
        update_meta(&gone, |m| m.depth = 1);
        for p in [&gone.state, &gone.lock, &gone.agent, &gone.meta] {
            assert!(!p.exists(), "{} was recreated", p.display());
        }
        // Left over by an older binary: swept once their pid is dead. A live
        // pid's (a wrapper that opened its tee and has not written its
        // `.meta` yet) and names that are no pid's stay.
        let me = std::process::id();
        let stale = ["state", "lock", "agent", "raw", "raw.tmp"].map(|x| dir.join(&format!("{}.{x}", u32::MAX - 1)));
        let kept = [dir.join(&format!("{me}.raw")), dir.join("tags.lock"), dir.join("x.pending")];
        for p in stale.iter().chain(&kept) {
            fs::write(p, b"").unwrap();
        }
        reap_in(&dir);
        for p in &stale {
            assert!(!p.exists(), "{} should be swept", p.display());
        }
        for p in &kept {
            assert!(p.exists(), "{} should stay", p.display());
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_reaped_agent_leaves_a_tombstone_that_wait_and_read_resolve() {
        let dir = scratch("tombstone");
        let gone = Files::in_dir(&dir, 2);
        let mut meta = sample(2, Some(u32::MAX as u64));
        meta.tag = "worker".into();
        write_new_meta(&gone, &meta);
        update_agent(&gone, |r| {
            r.stops = 3;
            r.last_message = Some("done: 4 files".into());
            r.transcript_path = Some("/t.jsonl".into());
            r.session_id = Some("s1".into());
        });
        let session = || Session { files: gone.clone(), meta: meta.clone() };

        // Dead but not reaped: the session itself, its ledger from its files.
        assert!(resolve_target_in(&dir, "worker").is_err(), "not a live agent");
        assert!(matches!(resolve_exited_in(&dir, "@worker"), Ok(Found::Session(s)) if s.meta.wrapper_pid == 2));
        assert_eq!(final_agent_in(&dir, &session()).stops, 3);

        reap_in(&dir);
        assert!(!gone.agent.exists());
        let Ok(Found::Exited(t)) = resolve_exited_in(&dir, "worker") else {
            panic!("no tombstone");
        };
        assert_eq!((t.wrapper_pid, t.agent.as_str(), t.cwd.as_str()), (2, "claude", "/home/u/proj"));
        let ledger = t.ledger();
        assert_eq!((ledger.stops, ledger.last_message.as_deref()), (3, Some("done: 4 files")));
        assert_eq!((ledger.transcript_path.as_deref(), ledger.session_id.as_deref()), (Some("/t.jsonl"), Some("s1")));
        // A waiter that had the session in hand when the reap came reads the
        // same ledger from the tombstone.
        assert_eq!(final_agent_in(&dir, &session()), ledger);
        // Unknown tags and live-only resolution are unchanged.
        assert!(resolve_exited_in(&dir, "nobody").unwrap_err().contains("no live agent"));
        assert!(resolve_target_in(&dir, "worker").is_err());

        // A new agent claiming the name supersedes it.
        create(&dir, 5, Some("worker"), None).unwrap();
        assert!(!tombstone_path(&dir, "worker").exists());
        assert!(matches!(resolve_exited_in(&dir, "worker"), Ok(Found::Session(s)) if s.meta.wrapper_pid == 5));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_tombstone_while_a_live_agent_holds_the_tag_and_old_ones_are_pruned() {
        let dir = scratch("tombprune");
        create(&dir, 1, Some("busy"), None).unwrap();
        let mut dead = sample(2, Some(u32::MAX as u64));
        dead.tag = "busy".into();
        write_new_meta(&Files::in_dir(&dir, 2), &dead);
        let old = Tombstone {
            tag: "old".into(),
            wrapper_pid: 9,
            exited_at: crate::clock::unix_now() - TOMBSTONE_TTL - 1,
            agent: "claude".into(),
            cwd: String::new(),
            stops: 1,
            last_message: None,
            transcript_path: None,
            session_id: None,
        };
        fs::write(tombstone_path(&dir, "old"), serde_json::to_vec(&old).unwrap()).unwrap();
        fs::write(tombstone_path(&dir, "torn"), "{").unwrap();
        let fresh = Tombstone { tag: "fresh".into(), exited_at: crate::clock::unix_now(), ..old.clone() };
        fs::write(tombstone_path(&dir, "fresh"), serde_json::to_vec(&fresh).unwrap()).unwrap();

        reap_in(&dir);
        assert!(!tombstone_path(&dir, "busy").exists(), "the live holder is what `busy` means");
        assert!(!tombstone_path(&dir, "old").exists());
        assert!(!tombstone_path(&dir, "torn").exists());
        assert!(tombstone_path(&dir, "fresh").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_exclude_line_is_added_once() {
        let dir = scratch("exclude");
        let ex = dir.as_ref().join("info/exclude");
        add_exclude_line(&ex).unwrap();
        assert_eq!(fs::read_to_string(&ex).unwrap(), ".seagents/\n");
        fs::write(&ex, "# git ls-files --others\n*.o").unwrap();
        add_exclude_line(&ex).unwrap();
        add_exclude_line(&ex).unwrap();
        assert_eq!(fs::read_to_string(&ex).unwrap(), "# git ls-files --others\n*.o\n.seagents/\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_files_are_private_excluded_from_git_and_go_with_their_session_only() {
        let dir = scratch("prompts");
        let root = dir.as_ref().to_path_buf();
        let repo = root.join("repo");
        let cwd = repo.join("sub");
        fs::create_dir_all(&cwd).unwrap();
        let git_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .is_ok_and(|s| s.success());
        let meta = |pid: u64, child: Option<u64>| Meta {
            cwd: cwd.to_string_lossy().into_owned(),
            ..sample(pid, child)
        };

        let p = write_prompt(&meta(42, None), 3, "do\nthe\nthing").unwrap();
        assert_eq!(p.parent().unwrap(), cwd.join(".seagents/prompts"));
        let name = p.file_name().unwrap().to_str().unwrap().to_string();
        assert!(is_prompt_of(&name, 42) && name.starts_with("42-3-"), "{name}");
        assert_eq!(fs::read_to_string(&p).unwrap(), "do\nthe\nthing");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_ne!(write_prompt(&meta(42, None), 3, "again").unwrap(), p, "never overwritten");
        if git_ok {
            let ex = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
            assert_eq!(ex.lines().filter(|l| *l == ".seagents/").count(), 1);
            let st = std::process::Command::new("git").arg("-C").arg(&repo).args(["status", "--porcelain"]).output().unwrap();
            assert!(st.stdout.is_empty(), "{}", String::from_utf8_lossy(&st.stdout));
        }

        // Another session's files, and anything not named like a prompt
        // file, survive a removal.
        let other = write_prompt(&meta(420, None), 0, "theirs").unwrap();
        let stray = cwd.join(".seagents/prompts/42-notes.md");
        fs::write(&stray, "mine").unwrap();
        for bad in ["42-3-ABCD.md", "42--abcd.md", "42-3-abcde.md", "42-3-abcd.txt", "142-3-abcd.md", "4-3-abcd.md"] {
            assert!(!is_prompt_of(bad, 42), "{bad}");
        }
        remove_prompts(&meta(42, None));
        assert!(!p.exists());
        assert!(other.exists() && stray.exists());

        // Reap removes a dead session's; the directories go once empty.
        fs::remove_file(&stray).unwrap();
        write_new_meta(&Files::in_dirs(&dir, 420), &meta(420, Some(u32::MAX as u64)));
        reap_in(&dir);
        assert!(!other.exists());
        assert!(!cwd.join(".seagents").exists());
        // Nowhere to write: an error, not a panic.
        let mut gone = meta(7, None);
        gone.cwd = root.join("missing").to_string_lossy().into_owned();
        assert!(write_prompt(&gone, 0, "x").unwrap_err().contains("gone"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_enclosing_session_is_the_nearest_ancestor_with_a_live_meta() {
        let dir = scratch("enclosing");
        let me = std::process::id() as u64;
        let dead = u32::MAX as u64;
        write_new_meta(&Files::in_dir(&dir, 10), &sample(10, Some(me)));
        write_new_meta(&Files::in_dir(&dir, 20), &sample(20, Some(me)));
        write_new_meta(&Files::in_dir(&dir, 30), &sample(30, Some(dead)));
        let found = |chain: &[u64]| nearest_session_in(&dir, chain).map(|s| s.meta.wrapper_pid);
        // sh → claude → __lead → wrapper 10 → … → wrapper 20: the nearest wins
        // (an agent running inside another agent's session belongs to its own).
        assert_eq!(found(&[500, 501, 502, 10, 600, 20]), Some(10));
        // A dead session is skipped, not matched.
        assert_eq!(found(&[500, 30, 20]), Some(20));
        assert_eq!(found(&[500, 501]), None);
        // Our own ancestry holds no wrapper of this scratch cache: a human,
        // positively. A pid with no ancestry to read, or a caller that is
        // gone by the end of the walk, is nobody in particular.
        assert!(matches!(enclosing_session_in(&dir, me, || true), Ok(None)));
        assert!(enclosing_session_in(&dir, dead, || true).unwrap_err().contains("cannot be read"));
        assert!(enclosing_session_in(&dir, me, || false).unwrap_err().contains("exited"));
        fs::remove_dir_all(&dir).ok();
    }

    fn spawn_claim(
        dir: &Dirs,
        requested: Option<&str>,
        caller: Option<u64>,
        reserved: &HashSet<String>,
        caps: &Caps,
    ) -> Result<SpawnClaim, String> {
        claim_spawn_in(dir, requested, caller, Path::new("/p/proj"), reserved, caps, std::process::id())
    }

    #[test]
    fn a_spawn_reservation_holds_its_tag_until_the_spawned_run_takes_it() {
        let dir = scratch("spawnres");
        let caps = Caps::default();
        let layout: HashSet<String> = ["old".to_string()].into();
        // A spawn may not take a tag the saved layout will restore (run may).
        assert!(spawn_claim(&dir, Some("old"), None, &layout, &caps).unwrap_err().contains("restore"));
        assert!(spawn_claim(&dir, Some("Bad"), None, &layout, &caps).unwrap_err().contains("invalid"));

        let a = spawn_claim(&dir, Some("kid"), None, &layout, &caps).unwrap();
        assert_eq!((a.tag.as_str(), a.parent.as_deref()), ("kid", None));
        assert!(dir.join("kid.pending").exists());
        // Another spawn cannot have it meanwhile, nor mint over it…
        assert!(spawn_claim(&dir, Some("kid"), None, &layout, &caps).unwrap_err().contains("held"));
        // …but the `run` it launches can, and consumes the reservation.
        let c = claim(&dir, Some("kid"), None).unwrap();
        c.create_meta(&Files::in_dir(&dir, 1), sample(1, Some(std::process::id() as u64)));
        assert!(!dir.join("kid.pending").exists());
        drop(a);
        assert!(spawn_claim(&dir, Some("kid"), None, &layout, &caps).unwrap_err().contains("held"));

        // A reservation dies with its spawner (and is swept), and is removed
        // when the claim is dropped.
        let dead = Pending { tag: "ghost".into(), parent: None, spawner: u32::MAX as u64 };
        fs::write(dir.join("ghost.pending"), serde_json::to_string(&dead).unwrap()).unwrap();
        let g = spawn_claim(&dir, Some("ghost"), None, &layout, &caps).unwrap();
        drop(g);
        assert!(!dir.join("ghost.pending").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_agent_callers_spawn_is_its_child_within_the_caps() {
        let dir = scratch("spawncaps");
        let none = HashSet::new();
        let caps = Caps { depth: 2, children: 2, spawned: 3 };
        create(&dir, 1, Some("root"), None).unwrap();
        let a = spawn_claim(&dir, None, Some(1), &none, &caps).unwrap();
        assert_eq!(a.parent.as_deref(), Some("root"));
        assert!(a.tag.starts_with("proj-"), "{}", a.tag);
        let b = spawn_claim(&dir, Some("b"), Some(1), &none, &caps).unwrap();
        // Breadth: two reserved children already; the error names them.
        let e = spawn_claim(&dir, None, Some(1), &none, &caps).unwrap_err();
        assert!(e.contains("2 live children") && e.contains(&a.tag) && e.contains("b"), "{e}");
        drop(a);
        // b's run registers it; it then spawns at depth 2, the cap.
        let c = claim(&dir, Some("b"), Some("root")).unwrap();
        c.create_meta(&Files::in_dir(&dir, 2), sample(2, Some(std::process::id() as u64)));
        drop(b);
        let c = claim(&dir, Some("c"), Some("b")).unwrap();
        c.create_meta(&Files::in_dir(&dir, 3), sample(3, Some(std::process::id() as u64)));
        assert_eq!(read_meta(&Files::in_dir(&dir, 3)).unwrap().depth, 2);
        let e = spawn_claim(&dir, None, Some(3), &none, &caps).unwrap_err();
        assert!(e.contains("depth 3") && e.contains("max_depth"), "{e}");
        // Global: b and c are spawned, one reservation makes three.
        let _d = spawn_claim(&dir, Some("d"), Some(1), &none, &caps).unwrap();
        let e = spawn_claim(&dir, None, Some(2), &none, &caps).unwrap_err();
        assert!(e.contains("3 agent-spawned") && e.contains("max_spawned"), "{e}");
        // A human is not capped, and has no parent.
        let h = spawn_claim(&dir, None, None, &none, &caps).unwrap();
        assert_eq!(h.parent, None);
        // A caller whose session is gone is refused rather than made human.
        assert!(spawn_claim(&dir, None, Some(99), &none, &caps).unwrap_err().contains("gone"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_run_with_a_parent_is_held_to_the_caps_its_spawn_already_counted_once() {
        let dir = scratch("runcaps");
        let none = HashSet::new();
        let caps = Caps { depth: 3, children: 2, spawned: 12 };
        let run = |tag: &str, parent: &str| {
            claim_tag_in(&dir, Some(tag), Some(parent), Path::new("/p"), &none, &caps)
        };
        create(&dir, 1, Some("root"), None).unwrap();
        // A spawn reserves root's second child…
        run("a", "root").unwrap().create_meta(&Files::in_dir(&dir, 2), sample(2, Some(std::process::id() as u64)));
        let b = spawn_claim(&dir, Some("b"), Some(1), &none, &caps).unwrap();
        // …a `run --parent root` typed by hand is a third: refused, naming them.
        let e = run("x", "root").unwrap_err();
        assert!(e.contains("2 live children") && e.contains("a, b") && e.contains("max_children"), "{e}");
        // The spawn's own `run` takes the reservation it was counted as.
        run("b", "root").unwrap().create_meta(&Files::in_dir(&dir, 3), sample(3, Some(std::process::id() as u64)));
        drop(b);
        assert!(run("x", "root").is_err(), "a and b are live now");
        // Depth binds a run too.
        let deep = Caps { depth: 1, ..caps };
        let e = claim_tag_in(&dir, Some("y"), Some("a"), Path::new("/p"), &none, &deep).unwrap_err();
        assert!(e.contains("max_depth"), "{e}");
        // A top-level run has no parent to count against.
        assert!(claim_tag_in(&dir, Some("z"), None, Path::new("/p"), &none, &Caps { children: 0, ..caps }).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn caps_come_from_the_users_config_file_and_nothing_else() {
        assert_eq!(Caps::parse("").unwrap(), Caps::default());
        assert_eq!(Caps::parse("[spawn]\n").unwrap(), Caps::default());
        let c = Caps::parse("[spawn]\nmax_children = 9\n").unwrap();
        assert_eq!((c.depth, c.children, c.spawned), (3, 9, 12));
        let c = Caps::parse("[spawn]\nmax_depth = 1\nmax_children = 2\nmax_spawned = 5\n").unwrap();
        assert_eq!((c.depth, c.children, c.spawned), (1, 2, 5));
        // A typo or a non-number is an error, not quietly the defaults.
        assert!(Caps::parse("[spawn]\nmax_childs = 9\n").unwrap_err().contains("max_childs"));
        assert!(Caps::parse("[spawn]\nmax_depth = \"lots\"\n").is_err());
        assert!(Caps::parse("[spwan]\nmax_depth = 1\n").is_err());

        let _env = crate::vendor::herdr_detect::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("seagents-caps-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("seagents")).unwrap();
        let old = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &root);
        // The environment variables that once set them do nothing.
        std::env::set_var("SEAGENTS_MAX_CHILDREN", "999");
        std::env::set_var("SEAGENTS_MAX_SPAWNED", "999");
        std::env::set_var("SEAGENTS_MAX_DEPTH", "999");
        assert_eq!(Caps::configured().unwrap(), Caps::default(), "no file: the defaults");
        fs::write(root.join("seagents/config.toml"), "[spawn]\nmax_children = 1\n").unwrap();
        assert_eq!(Caps::configured().unwrap().children, 1);
        fs::write(root.join("seagents/config.toml"), "[spawn\n").unwrap();
        assert!(Caps::configured().unwrap_err().contains("config.toml"));
        for v in ["SEAGENTS_MAX_CHILDREN", "SEAGENTS_MAX_SPAWNED", "SEAGENTS_MAX_DEPTH"] {
            std::env::remove_var(v);
        }
        match old {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        fs::remove_dir_all(&root).ok();
    }
}
