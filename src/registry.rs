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

use crate::procfs;

/// Directory holding this machine's seagents session files.
pub(crate) fn cache_dir() -> PathBuf {
    let base = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".cache/seagents"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/seagents"));
    let _ = fs::create_dir_all(&base);
    base
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
}

impl Files {
    /// The files of session `wrapper_pid` in the cache directory.
    pub(crate) fn of(wrapper_pid: u64) -> Files {
        Files::in_dir(&cache_dir(), wrapper_pid)
    }

    pub(crate) fn in_dir(dir: &Path, wrapper_pid: u64) -> Files {
        Files {
            raw: dir.join(format!("{wrapper_pid}.raw")),
            raw_tmp: dir.join(format!("{wrapper_pid}.raw.tmp")),
            meta: dir.join(format!("{wrapper_pid}.meta")),
            state: dir.join(format!("{wrapper_pid}.state")),
            agent: dir.join(format!("{wrapper_pid}.agent")),
            lock: dir.join(format!("{wrapper_pid}.lock")),
        }
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
fn all_in(dir: &Path) -> Vec<Session> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Session> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("meta"))
        .filter_map(|e| {
            let text = fs::read_to_string(e.path()).ok()?;
            let meta: Meta = serde_json::from_str(&text).ok()?;
            let files = Files::in_dir(dir, meta.wrapper_pid);
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
    list_in(&cache_dir())
}

fn list_in(dir: &Path) -> Vec<Session> {
    all_in(dir)
        .into_iter()
        .filter(|s| is_alive(&s.meta))
        .collect()
}

/// Delete every file of every session whose agent has exited. A fully exited
/// agent isn't interesting — "idle" already means finished-and-waiting — so
/// the HUD and `status` only ever show live agents. A `.meta` that does not
/// parse is left alone: it is not known to be dead.
pub(crate) fn reap() {
    reap_in(&cache_dir())
}

fn reap_in(dir: &Path) {
    for s in all_in(dir).into_iter().filter(|s| !is_alive(&s.meta)) {
        let f = &s.files;
        for path in [&f.meta, &f.raw, &f.raw_tmp, &f.state, &f.agent, &f.lock] {
            let _ = fs::remove_file(path);
        }
    }
}

/// The session whose anchor is `child_pid` — the handle `focus` and the HUD
/// address agents by. Not filtered on liveness: a lookup is not the place to
/// decide a session has ended.
pub(crate) fn by_child_pid(child_pid: u64) -> Option<Session> {
    all_in(&cache_dir())
        .into_iter()
        .find(|s| s.meta.child_pid == Some(child_pid))
}

/// The live session named by `target`, as every command that addresses an
/// agent takes it: `@tag`, a bare `tag`, or an all-digits `child_pid` (the
/// handle `status --json` has always reported as `pid`). Tags are never all
/// digits, so the bare forms cannot be confused. The error is the message to
/// show the user.
pub(crate) fn resolve_target(target: &str) -> Result<Session, String> {
    resolve_target_in(&cache_dir(), target)
}

fn resolve_target_in(dir: &Path, target: &str) -> Result<Session, String> {
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

fn live_by_tag(dir: &Path, tag: &str) -> Option<Session> {
    list_in(dir).into_iter().find(|s| s.meta.tag == tag)
}

/// The live session process `pid` runs inside: the first of its `/proc`
/// ancestors that is a wrapper with a live `.meta` (for a hook: sh → the agent
/// → `__lead` → the wrapper). This is "who is calling" for every command that
/// cares — a hook finding its ledger, `spawn` deriving lineage, and (Phase 2)
/// the control socket judging an `SO_PEERCRED` pid. None is a human caller.
///
/// Derived from ancestry rather than an environment variable, which
/// everything the agent runs inherits and can rewrite: unforgeable by
/// accident, though an agent that deliberately escapes its tree (`setsid`,
/// `systemd-run`) will look human — the line the threat model draws.
pub(crate) fn enclosing_session(pid: u64) -> Option<Session> {
    enclosing_session_in(&cache_dir(), &procfs::ancestors(pid))
}

fn enclosing_session_in(dir: &Path, ancestors: &[u64]) -> Option<Session> {
    ancestors.iter().find_map(|&pid| {
        let files = Files::in_dir(dir, pid);
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
        // for us has nothing left to hold.
        let _ = fs::remove_file(&self.pending);
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
pub(crate) fn claim_tag(
    requested: Option<&str>,
    parent: Option<&str>,
    cwd: &Path,
    reserved: &HashSet<String>,
) -> Result<TagClaim, String> {
    claim_tag_in(&cache_dir(), requested, parent, cwd, reserved)
}

fn claim_tag_in(
    dir: &Path,
    requested: Option<&str>,
    parent: Option<&str>,
    cwd: &Path,
    reserved: &HashSet<String>,
) -> Result<TagClaim, String> {
    check_requested(requested)?;
    // Unlike a session lock, this one must hold: without it two runs can both
    // pass the check. Failing to take it is failing to claim.
    let lock = Lock::acquire(&dir.join("tags.lock"))
        .ok_or("could not lock the tag registry")?;
    let live = list_in(dir);

    let (parent, depth) = match parent {
        None => (None, 0),
        Some(p) => {
            let p = p.strip_prefix('@').unwrap_or(p);
            let Some(ps) = live.iter().find(|s| s.meta.tag == p) else {
                return Err(format!("parent {p}: no live agent has that tag"));
            };
            (Some(p.to_string()), ps.meta.depth + 1)
        }
    };

    let pending: HashSet<String> = live_pending(dir).into_iter().map(|p| p.tag).collect();
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Caps {
    /// Deepest generation a spawn may create.
    pub(crate) depth: u32,
    /// Live children one agent may have.
    pub(crate) children: usize,
    /// Live agent-spawned agents, machine-wide.
    pub(crate) spawned: usize,
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

fn pending_path(dir: &Path, tag: &str) -> PathBuf {
    dir.join(format!("{tag}.pending"))
}

/// Every reservation whose spawner is still running. Only called under the
/// tags lock, which is also what makes deleting the dead ones here safe.
fn live_pending(dir: &Path) -> Vec<Pending> {
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
    caps: &Caps,
) -> Result<SpawnClaim, String> {
    claim_spawn_in(&cache_dir(), requested, caller, cwd, reserved, caps, std::process::id())
}

fn claim_spawn_in(
    dir: &Path,
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
            let mut lineage: Vec<(String, Option<String>)> = live
                .iter()
                .map(|s| (s.meta.tag.clone(), s.meta.parent.clone()))
                .collect();
            lineage.extend(pending.iter().map(|p| (p.tag.clone(), p.parent.clone())));
            check_caps(caps, &me.meta.tag, me.meta.depth, &lineage)?;
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
    fs::write(&file, text).map_err(|e| format!("reserving tag {tag}: {e}"))?;
    Ok(SpawnClaim { tag, parent, file })
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
             (SEAGENTS_MAX_DEPTH)",
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
            "{parent} already has {} live {} ({}), the cap is {} (SEAGENTS_MAX_CHILDREN); \
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
            "{} agent-spawned agents are live ({}), the cap is {} (SEAGENTS_MAX_SPAWNED)",
            spawned.len(),
            spawned.join(", "),
            caps.spawned
        ));
    }
    Ok(())
}

/// Where per-session runtime files go — spawn's seed prompts today, control
/// sockets in Phase 2: `$XDG_RUNTIME_DIR/seagents` (tmpfs, private, gone at
/// logout), created `0700`; the cache dir when there is no runtime dir.
pub(crate) fn runtime_dir() -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(base) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(base).join("seagents");
        if fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).is_ok() {
            return dir;
        }
    }
    cache_dir()
}

/// Give every live session that has no tag one — sessions whose wrapper
/// predates tags. Called by the snapshot capture, which cannot record an
/// agent without a name for its placeholder; the tag lands in the `.meta`, so
/// the agent is also addressable and a later restore sees it as running.
pub(crate) fn backfill_tags(sessions: &mut [Session], reserved: &HashSet<String>) {
    if sessions.iter().all(|s| !s.meta.tag.is_empty()) {
        return;
    }
    backfill_tags_in(&cache_dir(), sessions, reserved)
}

fn backfill_tags_in(dir: &Path, sessions: &mut [Session], reserved: &HashSet<String>) {
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
        write_atomic(&files.meta, text.as_bytes());
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
            write_atomic(&files.meta, text.as_bytes());
        }
    }
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
            write_atomic(&files.agent, text.as_bytes());
        }
    }
}

/// Locked read-modify-write of a session's `.state`. `f` sees the current
/// record (None when there is none, or it does not parse) and may replace it;
/// the file is rewritten only if it did. Returns what `f` returns.
pub(crate) fn update_state<R>(files: &Files, f: impl FnOnce(&mut Option<StateRecord>) -> R) -> R {
    // A lock we could not take still leaves the tmp+rename write atomic; the
    // caller's observation is better recorded unlocked than dropped.
    let _lock = Lock::acquire(&files.lock);
    let before = read_state(files);
    let mut record = before.clone();
    let out = f(&mut record);
    if record != before {
        if let Some(text) = record.as_ref().and_then(|r| serde_json::to_string(r).ok()) {
            write_atomic(&files.state, text.as_bytes());
        }
    }
    out
}

/// Atomic tmp+rename so a concurrent reader never sees a half-written record;
/// pid-suffixed so writers in different processes can't stomp each other's
/// tmp file (writers within one process are serialized by the lock).
fn write_atomic(path: &Path, bytes: &[u8]) {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let tmp = path.with_extension(format!("{ext}.tmp{}", std::process::id()));
    if fs::write(&tmp, bytes).is_err() || fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
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
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seagents-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
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
        let gone = Files::in_dir(&dir, 2);
        write_new_meta(&live, &sample(1, Some(me)));
        write_new_meta(&gone, &sample(2, Some(dead)));
        for f in [&live, &gone] {
            fs::write(&f.raw, b"x").unwrap();
            update_state(f, |r| {
                *r = Some(StateRecord {
                    state: "idle".into(),
                    since: 1,
                    seen: true,
                    rule: None,
                })
            });
        }
        // Unparseable: not known to be dead, so never reaped.
        fs::write(dir.join("3.meta"), "{torn").unwrap();

        let listed: Vec<u64> = list_in(&dir).iter().map(|s| s.meta.wrapper_pid).collect();
        assert_eq!(listed, [1]);
        assert!(gone.meta.exists(), "list must not reap");

        reap_in(&dir);
        for path in [&gone.meta, &gone.raw, &gone.state, &gone.lock] {
            assert!(!path.exists(), "{} should be reaped", path.display());
        }
        for path in [&live.meta, &live.raw, &live.state] {
            assert!(path.exists(), "{} should survive", path.display());
        }
        assert!(dir.join("3.meta").exists());
        fs::remove_dir_all(&dir).ok();
    }

    fn claim(dir: &Path, requested: Option<&str>, parent: Option<&str>) -> Result<TagClaim, String> {
        claim_tag_in(dir, requested, parent, Path::new("/home/u/My Project!"), &HashSet::new())
    }

    /// Claim and create a live session (child = this test process).
    fn create(dir: &Path, wrapper_pid: u64, requested: Option<&str>, parent: Option<&str>) -> Result<Meta, String> {
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
        let c = claim_tag_in(&dir, None, None, Path::new("/p"), &reserved).unwrap();
        assert_eq!(c.tag, "p-002a");
        drop(c);
        assert!(claim_tag_in(&dir, Some("p-0001"), None, Path::new("/p"), &reserved).is_ok());
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
    fn the_enclosing_session_is_the_nearest_ancestor_with_a_live_meta() {
        let dir = scratch("enclosing");
        let me = std::process::id() as u64;
        let dead = u32::MAX as u64;
        write_new_meta(&Files::in_dir(&dir, 10), &sample(10, Some(me)));
        write_new_meta(&Files::in_dir(&dir, 20), &sample(20, Some(me)));
        write_new_meta(&Files::in_dir(&dir, 30), &sample(30, Some(dead)));
        let found = |chain: &[u64]| enclosing_session_in(&dir, chain).map(|s| s.meta.wrapper_pid);
        // sh → claude → __lead → wrapper 10 → … → wrapper 20: the nearest wins
        // (an agent running inside another agent's session belongs to its own).
        assert_eq!(found(&[500, 501, 502, 10, 600, 20]), Some(10));
        // A dead session is skipped, not matched.
        assert_eq!(found(&[500, 30, 20]), Some(20));
        assert_eq!(found(&[500, 501]), None);
        fs::remove_dir_all(&dir).ok();
    }

    fn spawn_claim(
        dir: &Path,
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
        assert!(e.contains("depth 3") && e.contains("SEAGENTS_MAX_DEPTH"), "{e}");
        // Global: b and c are spawned, one reservation makes three.
        let _d = spawn_claim(&dir, Some("d"), Some(1), &none, &caps).unwrap();
        let e = spawn_claim(&dir, None, Some(2), &none, &caps).unwrap_err();
        assert!(e.contains("3 agent-spawned") && e.contains("SEAGENTS_MAX_SPAWNED"), "{e}");
        // A human is not capped, and has no parent.
        let h = spawn_claim(&dir, None, None, &none, &caps).unwrap();
        assert_eq!(h.parent, None);
        // A caller whose session is gone is refused rather than made human.
        assert!(spawn_claim(&dir, None, Some(99), &none, &caps).unwrap_err().contains("gone"));
        fs::remove_dir_all(&dir).ok();
    }
}
