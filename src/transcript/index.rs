//! The lazy offset index: one fixed-width entry per tree node (a line with a
//! uuid), so the main line can be found without parsing any line's content
//! and only the lines actually shown are read (docs/HANDOFF.md §10, "The
//! offset index"). The largest real transcript here is 53 MB; parsing
//! all of it for `read --tail 20` would take longer than the answer is worth.
//!
//! Cached at `~/.cache/seagents/sessions/<key>.idx`:
//!
//! ```text
//! header  magic "SGIDX\x02\0\0" | indexed_len u64 | prefix_len u64 | prefix_hash u64
//!         | path_len u32 | path (the transcript's canonical path, path_len bytes)
//! entry   offset u64 | len u32 | kind u8 | timestamp_ms u64 | uuid u128 | parent u128
//! ```
//!
//! little-endian. Transcripts are append-only, so a cached index is extended
//! by scanning only the bytes past `indexed_len`; it is thrown away when the
//! file is shorter than that (truncated or replaced) or its first bytes no
//! longer hash the same (a different file at the same path). Only complete
//! lines are indexed: a line still being written is picked up next time.
//! The cache is an accelerator: when it cannot be read or written the index
//! is simply built in memory.
//!
//! Indexes are about conversations, not live agents, so no session's reap
//! takes them; [`cache_path`] sweeps the directory instead, at most once a
//! day, dropping an index whose transcript is gone or that nobody has read
//! for [`UNUSED`] (a read that changes nothing still touches its mtime).
//! That is what the path in the header is for.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// What a node is, as far as the tree needs to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeKind {
    /// In the tree but not conversation: system notes, attachments,
    /// progress records. They carry the parent chain, so they are indexed.
    Other = 0,
    User = 1,
    Assistant = 2,
}

impl NodeKind {
    fn from_byte(b: u8) -> NodeKind {
        match b {
            1 => NodeKind::User,
            2 => NodeKind::Assistant,
            _ => NodeKind::Other,
        }
    }

    /// A turn of the conversation — what the main line's tip must be.
    pub(crate) fn conversation(self) -> bool {
        self != NodeKind::Other
    }
}

/// One tree node: a transcript line with a uuid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) offset: u64,
    /// The line's length, without its newline.
    pub(crate) len: u32,
    pub(crate) kind: NodeKind,
    /// Unix ms; 0 when the line has none.
    pub(crate) ts: u64,
    pub(crate) uuid: u128,
    /// 0 for a root. A parent that is not in the file is kept as is: the
    /// main line needs to tell a root from a line whose parent is missing.
    pub(crate) parent: u128,
}

/// What a format's parser says about one line: None for a line that is not
/// a tree node (no uuid, junk). Given where the line starts and the nodes
/// before it, for a format whose lines carry no uuid and no parent (Codex's
/// rollout, a log with rollback markers): its node is then minted from the
/// offset, and its parent read off the nodes already indexed — which a
/// cached index holds, so extending one gives the same tree as a rebuild.
pub(crate) type HeadFn = fn(line: &[u8], at: u64, prior: &[Node]) -> Option<Node>;

#[derive(Debug, Default)]
pub(crate) struct Index {
    pub(crate) nodes: Vec<Node>,
    /// How many bytes of the file the nodes cover (complete lines only).
    pub(crate) indexed_len: u64,
}

const MAGIC: &[u8; 8] = b"SGIDX\x02\0\0";
/// The fixed part of the header, up to and including `path_len`.
const HEADER: usize = 36;
/// Longer than any path Linux resolves (`PATH_MAX`): a header claiming more
/// is junk.
const PATH_MAX: usize = 4096;
const ENTRY: usize = 8 + 4 + 1 + 8 + 16 + 16;
/// How much of the file's start identifies it.
const PREFIX: u64 = 4096;

impl Index {
    /// The index of `transcript`, from `cache` when that still describes
    /// the file, extended past what it covered, and written back when
    /// changed.
    pub(crate) fn load(transcript: &Path, cache: Option<&Path>, head: HeadFn) -> std::io::Result<Index> {
        let mut file = File::open(transcript)?;
        let len = file.metadata()?.len();
        let cached = cache.and_then(|c| fs::read(c).ok()).and_then(|bytes| decode(&bytes));
        let mut index = match cached {
            Some((index, prefix_len, hash))
                if index.indexed_len <= len && prefix_hash(&mut file, prefix_len)? == hash =>
            {
                index
            }
            _ => Index::default(),
        };
        let before = (index.indexed_len, index.nodes.len());
        index.extend(&mut file, head)?;
        if let Some(cache) = cache {
            if (index.indexed_len, index.nodes.len()) != before || !cache.exists() {
                let prefix_len = index.indexed_len.min(PREFIX);
                let hash = prefix_hash(&mut file, prefix_len)?;
                // Canonical, so the sweep (run from any cwd) finds the file
                // a relative `read --transcript` named.
                let path = fs::canonicalize(transcript).unwrap_or_else(|_| transcript.to_path_buf());
                // Failure is ignored: the cache only saves time.
                let _ = crate::fsutil::write_atomic(cache, &encode(&index, prefix_len, hash, &path));
            } else {
                // Read, not rewritten: mark it used for the sweep.
                let _ = File::options()
                    .write(true)
                    .open(cache)
                    .and_then(|f| f.set_modified(SystemTime::now()));
            }
        }
        Ok(index)
    }

    /// Index the complete lines past `indexed_len`.
    fn extend(&mut self, file: &mut File, head: HeadFn) -> std::io::Result<()> {
        file.seek(SeekFrom::Start(self.indexed_len))?;
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut line = Vec::new();
        let mut at = self.indexed_len;
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 || line.last() != Some(&b'\n') {
                break; // the end, or a line still being written
            }
            let body = &line[..n - 1];
            if let Some(mut node) = head(body, at, &self.nodes) {
                node.offset = at;
                node.len = u32::try_from(body.len()).unwrap_or(u32::MAX);
                self.nodes.push(node);
            }
            at += n as u64;
        }
        self.indexed_len = at;
        Ok(())
    }
}

/// Where `transcript`'s index is cached: `~/.cache/seagents/sessions/`.
/// Every cached read comes through here, so this is also where the
/// directory is swept ([`sweep_daily`]).
pub(crate) fn cache_path(transcript: &Path) -> Option<PathBuf> {
    let dir = crate::registry::sessions_dir()?;
    sweep_daily(&dir, SystemTime::now());
    Some(dir.join(cache_name(transcript)))
}

/// An index nobody has read for this long goes.
const UNUSED: Duration = Duration::from_secs(30 * 24 * 3600);
/// How often the sessions dir is swept, and how old a stray tmp file of an
/// interrupted write must be to go.
const SWEEP_EVERY: Duration = Duration::from_secs(24 * 3600);
/// The sweep's stamp: its mtime is the last sweep.
const STAMP: &str = ".swept";

/// [`sweep`], unless the last one was under [`SWEEP_EVERY`] ago — one
/// `stat` otherwise. The stamp is renewed first, so processes reading at
/// the same moment don't all sweep. A stamp in the future (the clock went
/// back) counts as due, and the renewal repairs it.
fn sweep_daily(dir: &Path, now: SystemTime) {
    let stamp = dir.join(STAMP);
    let last = fs::metadata(&stamp).and_then(|m| m.modified());
    if matches!(last.map(|t| now.duration_since(t)), Ok(Ok(age)) if age < SWEEP_EVERY) {
        return;
    }
    if crate::fsutil::create_private(&stamp, false).is_ok() {
        sweep(dir, now);
    }
}

/// Delete every index whose transcript no longer exists, that has not been
/// used for [`UNUSED`], or that is not one this binary can read (an older
/// format has no path to check, and would be rebuilt anyway); and tmp files
/// an interrupted write left behind.
fn sweep(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let age = |p: &Path| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or_default()
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name();
        let name = name.as_bytes();
        let stale = if name.ends_with(b".idx") {
            age(&path) >= UNUSED || !transcript_of(&path).is_some_and(|t| t.exists())
        } else {
            name.ends_with(b".tmp") && age(&path) >= SWEEP_EVERY
        };
        if stale {
            let _ = fs::remove_file(&path);
        }
    }
}

/// The transcript an index file was built from, read from its header alone
/// (an index can run to megabytes).
fn transcript_of(idx: &Path) -> Option<PathBuf> {
    let mut f = File::open(idx).ok()?;
    let mut head = [0u8; HEADER];
    f.read_exact(&mut head).ok()?;
    if &head[..8] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(head[32..36].try_into().unwrap()) as usize;
    if len == 0 || len > PATH_MAX {
        return None;
    }
    let mut path = vec![0; len];
    f.read_exact(&mut path).ok()?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&path)))
}

/// The cache file's name: the session id when the transcript is named by
/// one (`<uuid>.jsonl`); otherwise (a subagent's `agent-<id>.jsonl`, which
/// is only unique within its session) a hash of the whole path.
fn cache_name(transcript: &Path) -> String {
    let stem = transcript.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if parse_uuid(stem).is_some() {
        format!("{stem}.idx")
    } else {
        format!("{:016x}.idx", fnv1a(transcript.as_os_str().as_encoded_bytes()))
    }
}

fn prefix_hash(file: &mut File, len: u64) -> std::io::Result<u64> {
    let mut buf = Vec::with_capacity(len as usize);
    file.seek(SeekFrom::Start(0))?;
    file.take(len).read_to_end(&mut buf)?;
    Ok(if buf.len() as u64 == len { fnv1a(&buf) } else { !fnv1a(&buf) })
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3))
}

fn encode(index: &Index, prefix_len: u64, hash: u64, transcript: &Path) -> Vec<u8> {
    let path = transcript.as_os_str().as_bytes();
    let mut out = Vec::with_capacity(HEADER + path.len() + index.nodes.len() * ENTRY);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&index.indexed_len.to_le_bytes());
    out.extend_from_slice(&prefix_len.to_le_bytes());
    out.extend_from_slice(&hash.to_le_bytes());
    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
    out.extend_from_slice(path);
    for n in &index.nodes {
        out.extend_from_slice(&n.offset.to_le_bytes());
        out.extend_from_slice(&n.len.to_le_bytes());
        out.push(n.kind as u8);
        out.extend_from_slice(&n.ts.to_le_bytes());
        out.extend_from_slice(&n.uuid.to_le_bytes());
        out.extend_from_slice(&n.parent.to_le_bytes());
    }
    out
}

/// The index, its prefix length and hash; None for anything malformed.
fn decode(bytes: &[u8]) -> Option<(Index, u64, u64)> {
    if bytes.len() < HEADER || &bytes[..8] != MAGIC {
        return None;
    }
    let path_len = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
    let entries = HEADER + path_len;
    if path_len > PATH_MAX || bytes.len() < entries || (bytes.len() - entries) % ENTRY != 0 {
        return None;
    }
    let u64_at = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    let u128_at = |b: &[u8], at: usize| u128::from_le_bytes(b[at..at + 16].try_into().unwrap());
    let nodes = bytes[entries..]
        .chunks_exact(ENTRY)
        .map(|e| Node {
            offset: u64_at(e, 0),
            len: u32::from_le_bytes(e[8..12].try_into().unwrap()),
            kind: NodeKind::from_byte(e[12]),
            ts: u64_at(e, 13),
            uuid: u128_at(e, 21),
            parent: u128_at(e, 37),
        })
        .collect();
    let index = Index { nodes, indexed_len: u64_at(bytes, 8) };
    Some((index, u64_at(bytes, 16), u64_at(bytes, 24)))
}

/// `8-4-4-4-12` hex, either case → its 128 bits.
pub(crate) fn parse_uuid(s: &str) -> Option<u128> {
    let b = s.as_bytes();
    if b.len() != 36 || [8, 13, 18, 23].iter().any(|&i| b[i] != b'-') {
        return None;
    }
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok()
}

/// The canonical lowercase spelling of a uuid.
pub(crate) fn format_uuid(u: u128) -> String {
    let h = format!("{u:032x}");
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}
