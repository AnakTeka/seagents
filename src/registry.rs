//! The session cache, `~/.cache/seagents/<wrapper_pid>.*` — its one owner.
//!
//! Every sidecar is named, read and written here and nowhere else:
//!
//! - `.raw` — the tee. Append-only, written by the wrapper's output pump; this
//!   module only names it (and deletes it on reap).
//! - `.meta` — who and where: pids, argv, cwd, window handles, the resolved
//!   conversation id. Written by the wrapper at launch, then updated from any
//!   process: window healing from the wrapper's input pump *and* from the
//!   focus/HUD processes, conversation ids from whichever process runs a
//!   snapshot capture.
//! - `.state` — the last observed screen state, since when, and whether the
//!   user has seen it. Written by the wrapper's monitor and by every observer
//!   whose own detection disagrees (see `status::observe_state`).
//!
//! Because `.meta` and `.state` have writers in several processes, every change
//! is a locked read-modify-write ([`update_meta`], [`update_state`]): an
//! exclusive `flock` on the session's `.lock` sibling, read, mutate the typed
//! record, write a pid-suffixed tmp file, rename it over, unlock. Readers take
//! no lock — the rename means they see the old record or the new one, never a
//! torn one. tmp+rename alone fixes torn reads; only the lock fixes lost
//! updates (a healed con_id and a resolved session id landing at once, one
//! silently dropping the other).
//!
//! Liveness is one rule, [`procfs::alive`] on the child pid, and two
//! operations: [`list`] returns live sessions and never deletes anything;
//! [`reap`] deletes the files of dead ones. Only `status`/the HUD reap.

use std::fs::{self, File};
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
    /// Identity: [`Meta`].
    pub(crate) meta: PathBuf,
    /// Last observed state: [`StateRecord`].
    pub(crate) state: PathBuf,
    /// Guards every read-modify-write of `meta` and `state`.
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
            meta: dir.join(format!("{wrapper_pid}.meta")),
            state: dir.join(format!("{wrapper_pid}.state")),
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
    /// The agent's own conversation id, filled in lazily by the snapshot
    /// capture, and how it was learned (`proc` | `title`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id_source: Option<String>,
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
}

fn seen_by_default() -> bool {
    true
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

fn read_state(files: &Files) -> Option<StateRecord> {
    serde_json::from_str(&fs::read_to_string(&files.state).ok()?).ok()
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
        for path in [&f.meta, &f.raw, &f.state, &f.lock] {
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

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Write a brand-new session's `.meta`. Atomic but unlocked: until this file
/// exists nobody else can find the session, so there is nothing to race.
pub(crate) fn create_meta(files: &Files, meta: &Meta) {
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
/// exclude each other just as two processes do.
struct Lock {
    _held: File,
}

impl Lock {
    fn acquire(path: &Path) -> Option<Lock> {
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
        create_meta(&files, &sample(1, Some(2)));

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
        // No geometry, no window ids, no session id — and a field from some
        // other version of seagents.
        fs::write(
            &files.meta,
            r#"{"wrapper_pid":7,"child_pid":8,"cwd":"/p","argv":["claude"],"started":5,
                "tag":"reviewer","depth":1}"#,
        )
        .unwrap();

        let meta = read_meta(&files).expect("older meta parses");
        assert_eq!(
            (meta.child_pid, meta.cols, meta.con_id),
            (Some(8), None, None)
        );

        update_meta(&files, |m| m.con_id = Some(42));
        let v: Value = serde_json::from_str(&fs::read_to_string(&files.meta).unwrap()).unwrap();
        assert_eq!(v["con_id"].as_u64(), Some(42));
        assert_eq!(v["tag"].as_str(), Some("reviewer"));
        assert_eq!(v["depth"].as_u64(), Some(1));
        assert!(
            v.get("session_id").is_none(),
            "absent optional fields stay absent"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn meta_round_trips_every_field() {
        let dir = scratch("roundtrip");
        let files = Files::in_dir(&dir, 1);
        let mut meta = sample(1, Some(2));
        meta.con_id = Some(3);
        meta.window_id = Some(4);
        meta.session_id = Some("abc".into());
        meta.session_id_source = Some("proc".into());
        create_meta(&files, &meta);
        assert_eq!(read_meta(&files), Some(meta));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writes_leave_no_tmp_files_behind() {
        let dir = scratch("notmp");
        let files = Files::in_dir(&dir, 1);
        create_meta(&files, &sample(1, Some(2)));
        update_meta(&files, |m| m.con_id = Some(9));
        update_state(&files, |r| {
            *r = Some(StateRecord {
                state: "idle".into(),
                since: 1,
                seen: true,
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
        create_meta(&live, &sample(1, Some(me)));
        create_meta(&gone, &sample(2, Some(dead)));
        for f in [&live, &gone] {
            fs::write(&f.raw, b"x").unwrap();
            update_state(f, |r| {
                *r = Some(StateRecord {
                    state: "idle".into(),
                    since: 1,
                    seen: true,
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
}
