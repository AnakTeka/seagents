//! An agent's own conversation record, as it writes it to disk — the source
//! for content (`read`, and one day the sessions browser) and for the one
//! turn fact the hooks cannot give: an interrupt.
//!
//! Layers, each written once and shared by every format:
//!
//! - [`model`] — the common record model (user / assistant / thinking /
//!   tool_use / tool_result, with uuid, parent and timestamp).
//! - one parser per format ([`claude`], [`codex`]) — the only per-agent
//!   code: a cheap per-line *head* (uuid, parent, timestamp, kind) for the
//!   index, the full records of one line when it is actually read, and the
//!   interrupt record the turn ledger looks for.
//! - [`index`] — the lazy per-transcript offset index, cached and extended
//!   incrementally, so a 50 MB transcript is scanned once, not per read.
//! - [`mainline`] — the tree those heads form, and the main line through it.
//! - [`render`] — the collapse rules, records → text lines.
//!
//! [`Transcript`] ties them together for a reader: open (index), walk the
//! main line, read the records of the nodes wanted.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Deserialize;

mod claude;
mod codex;
pub(crate) mod index;
pub(crate) mod mainline;
pub(crate) mod model;
pub(crate) mod render;

use index::{Index, Node};
use model::{Kind, Record, Subagent};

/// A transcript format — named by an adapter's `[read] transcript`.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Format {
    /// Claude Code's JSONL.
    Claude,
    /// Codex's rollout JSONL.
    Codex,
}

impl Format {
    /// The format of a transcript named by path alone (`read --transcript`),
    /// from its first complete lines: a Codex rollout wraps every line in
    /// `{timestamp, type, payload}`, a Claude transcript never does. Claude
    /// when nothing tells (an empty file).
    pub(crate) fn detect(path: &Path) -> std::io::Result<Format> {
        let mut head = Vec::new();
        File::open(path)?.take(64 * 1024).read_to_end(&mut head)?;
        let lines = head
            .split(|b| *b == b'\n')
            .filter(|l| l.first() == Some(&b'{'));
        for line in lines {
            if codex::sniff(line) {
                return Ok(Format::Codex);
            }
            if serde_json::from_slice::<serde_json::Value>(line).is_ok() {
                return Ok(Format::Claude);
            }
        }
        Ok(Format::Claude)
    }
}

/// Did the agent write an interrupt record — its turn cut short by the user —
/// stamped at or after `since_ms`?
pub(crate) fn interrupted_since(format: Format, transcript: &Path, since_ms: u64) -> bool {
    match format {
        Format::Claude => claude::interrupted_since(transcript, since_ms),
        Format::Codex => codex::interrupted_since(transcript, since_ms),
    }
}

/// Did the agent write anything of its own — assistant text, reasoning, a
/// tool call — stamped at or after `since_ms`? Read from the transcript's
/// last 256 KiB, newest line first, stopping at the first one older than
/// `since_ms`; what a turn writes at its start (the prompt, attachments,
/// mode records) is not the agent's.
pub(crate) fn responded_since(format: Format, transcript: &Path, since_ms: u64) -> bool {
    let head = match format {
        Format::Claude => claude::head,
        Format::Codex => codex::head,
    };
    let Some(tail) = crate::fsutil::read_tail(transcript, 256 * 1024) else {
        return false;
    };
    for line in tail.rsplit(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        // Untimed lines (Claude's mode and title records) and the tail's
        // first line, cut mid-record, say nothing either way.
        let Some(node) = head(line, 0, &[]).filter(|n| n.ts != 0) else {
            continue;
        };
        if node.ts < since_ms {
            return false;
        }
        if node.kind == index::NodeKind::Assistant {
            return true;
        }
    }
    false
}

/// One transcript opened for reading: its index (built or extended on open)
/// and the file, read by offset only for the lines asked for.
pub(crate) struct Transcript {
    path: PathBuf,
    format: Format,
    file: File,
    pub(crate) index: Index,
    /// Subagents by the id of the tool call that spawned them, looked up
    /// the first time a line holds such a call.
    subagents: OnceCell<HashMap<String, Subagent>>,
}

impl Transcript {
    /// Open `path`, its index cached in the sessions dir.
    pub(crate) fn open(path: &Path, format: Format) -> std::io::Result<Transcript> {
        let cache = index::cache_path(path);
        Transcript::open_cached(path, format, cache.as_deref())
    }

    /// Open `path` with its index cached at `cache` (None: in memory only).
    pub(crate) fn open_cached(
        path: &Path,
        format: Format,
        cache: Option<&Path>,
    ) -> std::io::Result<Transcript> {
        let head = match format {
            Format::Claude => claude::head,
            Format::Codex => codex::head,
        };
        let index = Index::load(path, cache, head)?;
        Ok(Transcript {
            path: path.to_path_buf(),
            format,
            file: File::open(path)?,
            index,
            subagents: OnceCell::new(),
        })
    }

    /// The records of one indexed line — none for a line that is not
    /// conversation (a system note, an attachment) or no longer parses.
    pub(crate) fn records(&self, node: &Node) -> Vec<Record> {
        let mut buf = vec![0; node.len as usize];
        let mut f = &self.file;
        if f.seek(SeekFrom::Start(node.offset)).is_err() || f.read_exact(&mut buf).is_err() {
            return Vec::new();
        }
        let mut records = match self.format {
            Format::Claude => claude::records(&buf, node),
            Format::Codex => codex::records(&buf, node),
        };
        for r in &mut records {
            if let Kind::ToolUse {
                id,
                subagent: Some(sub),
                ..
            } = &mut r.kind
            {
                let known = self.subagents.get_or_init(|| match self.format {
                    Format::Claude => claude::subagents(&self.path),
                    // Codex's subagents are threads of their own, not
                    // anchored in the parent's rollout by a tool call.
                    Format::Codex => HashMap::new(),
                });
                if let Some(found) = known.get(id.as_str()) {
                    *sub = found.clone();
                }
            }
        }
        records
    }
}

/// An RFC 3339 UTC timestamp as transcripts write it
/// (`2026-09-29T06:01:13.951Z`), in Unix milliseconds. Fractional digits
/// past milliseconds are dropped; offsets other than `Z` are refused rather
/// than misread — every format parsed here writes UTC.
fn parse_utc_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.splitn(3, ':').map(|p| p.parse::<u64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let ms: u64 = match frac.len() {
        0 => 0,
        _ if !frac.bytes().all(|b| b.is_ascii_digit()) => return None,
        n => {
            let head = &frac[..n.min(3)];
            head.parse::<u64>().ok()? * 10u64.pow(3 - head.len() as u32)
        }
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days).ok()? * 86_400 + h * 3600 + mi * 60 + se;
    Some(secs * 1000 + ms)
}
