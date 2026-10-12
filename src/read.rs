//! `seagents read` — an agent's conversation, from its own transcript: the
//! main line (root to the newest tip, abandoned branches left out), as
//! collapsed text or as NDJSON in the common record model, ending with a
//! cursor to continue from.
//!
//! The cursor is the transcript's path, how many bytes of it were indexed,
//! and the main line's tip. The tip is what makes continuing right: after a
//! rewind the new records hang off an earlier node, and "everything past the
//! offset" would present them as a continuation of the branch the reader
//! saw. So `--from` walks the *current* main line: if it still runs through
//! the old tip, what follows it is new; if not, `rewound` names the node the
//! two lines share and reading resumes there; a different transcript
//! (`/clear` starts one) or a shorter one is a `reset`, read from the start.
//!
//! One token form serves both modes: `<offset>:<leaf>:<path>` — the text
//! mode's last line (`-- cursor: …`, taken whole or without its prefix) and
//! the JSON cursor's `token`; the JSON cursor object itself is accepted too.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::registry;
use crate::transcript::index::{format_uuid, parse_uuid};
use crate::transcript::mainline::{Resume, Tree};
use crate::transcript::model::{Kind, Record};
use crate::transcript::{render, Format, Transcript};

pub(crate) struct Args {
    pub(crate) target: Option<String>,
    pub(crate) transcript: Option<PathBuf>,
    pub(crate) json: bool,
    pub(crate) from: Option<String>,
    pub(crate) tail: Option<usize>,
    pub(crate) raw: bool,
}

/// Where the last read ended.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Cursor {
    transcript_path: String,
    offset: u64,
    /// The main line's tip; empty when there was no conversation yet.
    leaf: String,
}

const TEXT_PREFIX: &str = "-- cursor: ";

impl Cursor {
    fn token(&self) -> String {
        format!("{}:{}:{}", self.offset, self.leaf, self.transcript_path)
    }

    fn parse(s: &str) -> Result<Cursor, String> {
        let s = s.trim();
        let s = s.strip_prefix(TEXT_PREFIX).unwrap_or(s);
        let bad = || {
            format!("{s:?} is not a cursor (`<offset>:<leaf>:<path>`, or the JSON cursor record)")
        };
        if s.starts_with('{') {
            let v: serde_json::Value = serde_json::from_str(s).map_err(|_| bad())?;
            return serde_json::from_value(v.get("cursor").unwrap_or(&v).clone())
                .map_err(|_| bad());
        }
        let mut parts = s.splitn(3, ':');
        let (Some(offset), Some(leaf), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        let offset = offset.parse().map_err(|_| bad())?;
        if path.is_empty() || !(leaf.is_empty() || parse_uuid(leaf).is_some()) {
            return Err(bad());
        }
        Ok(Cursor {
            transcript_path: path.to_string(),
            offset,
            leaf: leaf.to_string(),
        })
    }
}

/// What precedes the records when the reader's place is gone.
#[derive(Debug, PartialEq)]
enum Notice {
    Reset,
    Rewound(Option<u128>),
}

/// `seagents read`: 0, or 1 with the reason on stderr.
pub(crate) fn run(args: Args) -> i32 {
    match read(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("seagents read: {e}");
            1
        }
    }
}

fn read(args: &Args) -> Result<(), String> {
    let from = args.from.as_deref().map(Cursor::parse).transpose()?;
    let Source {
        path,
        format,
        cwd,
        ended_at,
    } = source(args)?;
    let t = open_settled(&path, format, ended_at)?;
    let tree = Tree::new(&t.index.nodes);
    let main = tree.main_line();
    let (start, notice) = start_at(&tree, &main, &path, t.index.indexed_len, from.as_ref());

    let wanted = &main[start.min(main.len())..];
    let mut records = match args.tail {
        None => wanted
            .iter()
            .flat_map(|&i| t.records(&t.index.nodes[i]))
            .collect(),
        Some(n) => tail(&t, wanted, n),
    };
    let cursor = Cursor {
        transcript_path: path.display().to_string(),
        offset: t.index.indexed_len,
        leaf: main
            .last()
            .map(|&i| format_uuid(t.index.nodes[i].uuid))
            .unwrap_or_default(),
    };

    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    // A closed pipe (`| head`) ends the output; it is not an error.
    let _ = if args.json {
        write_json(&mut out, notice.as_ref(), &records, &cursor)
    } else {
        inline_references(&mut records, cwd.as_deref());
        write_text(&mut out, notice.as_ref(), &records, &cursor, args.raw)
    };
    Ok(())
}

struct Source {
    path: PathBuf,
    format: Format,
    /// The agent's working directory, which a prompt sent by reference is
    /// relative to.
    cwd: Option<PathBuf>,
    /// When the agent's last turn ended (its `Stop`, Unix ms), if it is not
    /// in one now.
    ended_at: Option<u64>,
}

fn source(args: &Args) -> Result<Source, String> {
    if let Some(p) = &args.transcript {
        // No agent to ask which tool wrote it: the content says.
        let format = Format::detect(p).map_err(|e| format!("{}: {e}", p.display()))?;
        return Ok(Source {
            path: p.clone(),
            format,
            cwd: None,
            ended_at: None,
        });
    }
    let target = args.target.as_deref().unwrap_or_default();
    // An exited agent's conversation is still on disk: its files say where
    // while they last, then the tombstone its reap left.
    let (tag, command, cwd, agent, ended) = match registry::resolve_exited(target)? {
        registry::Found::Session(s) => {
            let alive = s.alive();
            let agent = if alive {
                registry::read_agent(&s.files)
            } else {
                registry::final_agent(&s)
            };
            (
                s.meta.tag.clone(),
                s.meta.agent().to_string(),
                s.meta.cwd.clone(),
                agent,
                !alive,
            )
        }
        registry::Found::Exited(t) => {
            let agent = t.ledger();
            (t.tag, t.agent, t.cwd, agent, true)
        }
    };
    let adapter = crate::adapter::for_command(&command)?;
    let format = adapter
        .read
        .transcript
        .ok_or_else(|| format!("{tag} runs {command:?}, whose transcript seagents cannot read"))?;
    let path = agent.transcript_path.ok_or_else(|| match ended {
        true => format!("{tag} exited without reporting a transcript (no hook fired)"),
        false => format!("{tag} has not reported its transcript yet (no hook has fired)"),
    })?;
    // Nothing more will be written by an agent that has exited.
    let ended_at = agent
        .stopped_at
        .filter(|&s| !ended && agent.prompted_at.is_none_or(|p| p <= s));
    Ok(Source {
        path: PathBuf::from(path),
        format,
        cwd: Some(PathBuf::from(cwd)),
        ended_at,
    })
}

/// How long a read right after a turn waits for the turn's last records.
const SETTLE: Duration = Duration::from_secs(1);

/// Open the transcript — once the turn that ended at `ended_at` is in it.
/// Claude writes its transcript behind its hooks: measured on 2.1.284, a
/// `read` issued the moment `wait` returned a turn (the `Stop` hook fired)
/// saw the prompt but not yet the answer, which landed ~60 ms later. The
/// last line a turn writes, its `stop_hook_summary`, is stamped after the
/// hooks ran, so the turn is all there once a line is stamped at or after
/// the `Stop` — waited for up to [`SETTLE`], then read as it is.
fn open_settled(path: &Path, format: Format, ended_at: Option<u64>) -> Result<Transcript, String> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let t = Transcript::open(path, format).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                format!(
                    "{}: not written yet (the agent writes it at its first prompt)",
                    path.display()
                )
            }
            _ => format!("{}: {e}", path.display()),
        })?;
        let settled = ended_at.is_none_or(|end| t.index.nodes.iter().rev().any(|n| n.ts >= end));
        if settled || Instant::now() >= deadline {
            return Ok(t);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The main-line position to start at, and what to say first.
fn start_at(
    tree: &Tree,
    main: &[usize],
    path: &Path,
    indexed: u64,
    from: Option<&Cursor>,
) -> (usize, Option<Notice>) {
    let Some(c) = from else {
        return (0, None);
    };
    if !same_file(Path::new(&c.transcript_path), path) || indexed < c.offset {
        return (0, Some(Notice::Reset));
    }
    if c.leaf.is_empty() {
        return (0, None);
    }
    match parse_uuid(&c.leaf).map(|leaf| tree.resume(main, leaf)) {
        Some(Resume::After(p)) => (p, None),
        Some(Resume::Rewound { fork, from }) => (from, Some(Notice::Rewound(fork))),
        None => (0, Some(Notice::Rewound(None))),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// The last `n` records of `nodes`, reading lines from the end until there
/// are enough — the rest of the transcript is never read.
fn tail(t: &Transcript, nodes: &[usize], n: usize) -> Vec<Record> {
    let mut chunks = Vec::new();
    let mut have = 0;
    for &i in nodes.iter().rev() {
        if have >= n {
            break;
        }
        let r = t.records(&t.index.nodes[i]);
        have += r.len();
        chunks.push(r);
    }
    let all: Vec<Record> = chunks.into_iter().rev().flatten().collect();
    let skip = all.len().saturating_sub(n);
    all.into_iter().skip(skip).collect()
}

/// A user message that only points at a prompt file (`send` by reference)
/// gets the file's words under it, while the file still exists.
fn inline_references(records: &mut [Record], cwd: Option<&Path>) {
    for r in records {
        let Kind::User { text } = &mut r.kind else {
            continue;
        };
        let Some(named) = crate::registry::referenced_file(text) else {
            continue;
        };
        let file = match (Path::new(named).is_absolute(), cwd) {
            (true, _) => PathBuf::from(named),
            (false, Some(cwd)) => cwd.join(named),
            (false, None) => continue,
        };
        if let Ok(body) = std::fs::read_to_string(&file) {
            text.push_str(&format!("\n┄ {}\n{}", file.display(), body.trim_end()));
        }
    }
}

fn write_json(
    out: &mut impl Write,
    notice: Option<&Notice>,
    records: &[Record],
    c: &Cursor,
) -> std::io::Result<()> {
    match notice {
        Some(Notice::Reset) => writeln!(
            out,
            "{}",
            json!({"reset": {"transcript_path": c.transcript_path}})
        )?,
        Some(Notice::Rewound(fork)) => writeln!(
            out,
            "{}",
            json!({"rewound": {"fork": fork.map(format_uuid)}})
        )?,
        None => {}
    }
    for r in records {
        writeln!(out, "{}", serde_json::to_string(r).unwrap_or_default())?;
    }
    let cursor = json!({"cursor": {
        "transcript_path": c.transcript_path, "offset": c.offset, "leaf": c.leaf, "token": c.token(),
    }});
    writeln!(out, "{cursor}")?;
    out.flush()
}

fn write_text(
    out: &mut impl Write,
    notice: Option<&Notice>,
    records: &[Record],
    c: &Cursor,
    raw: bool,
) -> std::io::Result<()> {
    match notice {
        Some(Notice::Reset) => writeln!(
            out,
            "-- reset: a new transcript, {}, read from its start",
            c.transcript_path
        )?,
        Some(Notice::Rewound(Some(fork))) => writeln!(
            out,
            "-- rewound: the conversation went back to {}; continuing from there",
            format_uuid(*fork)
        )?,
        Some(Notice::Rewound(None)) => writeln!(
            out,
            "-- rewound: the cursor's place is not in this transcript; from its start"
        )?,
        None => {}
    }
    for line in render::render(records, raw) {
        writeln!(out, "{line}")?;
    }
    writeln!(out, "{TEXT_PREFIX}{}", c.token())?;
    out.flush()
}
