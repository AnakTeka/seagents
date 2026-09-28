//! Milestone 5: survive a reboot.
//!
//! `seagents restore` rebuilds the i3 workspaces the agents were living on and
//! relaunches each window on its *own* conversation (`claude --resume <id>`),
//! so a reboot costs you the processes but not the layout or the context.
//!
//! Three problems, three answers:
//!
//! 1. **The layout must outlive the processes.** `~/.cache/seagents/` is
//!    self-reaping — the first `status` after a boot deletes every `.meta`
//!    whose child pid is gone, which is precisely all of them. So the snapshot
//!    lives in `state_dir()/layout.json`, written *continuously* (each
//!    wrapper's monitor thread offers a capture every tick, globally throttled
//!    to one write per [`SNAPSHOT_INTERVAL_SECS`]). An unplanned reboot loses
//!    at most that interval, and there is no command to remember.
//!
//! 2. **A window must be re-fillable.** i3's `append_layout` matches
//!    placeholders against *swallow criteria*, and an ordinary terminal window
//!    offers nothing stable: Terminator windows all share one class, carry no
//!    role, and show a title Claude overwrites on the first repaint. So
//!    restored windows are launched carrying a minted handle,
//!    `seagents-<tag>`, and the placeholder swallows on that alone. (Same
//!    handle ORCHESTRATION-SPEC mints for `spawn`.) *Which* X11 property
//!    carries it is the terminal's business — a role for Terminator, a
//!    WM_CLASS instance for most others — so the stored layout keeps the tag
//!    and the criterion is built at restore time. See [`WindowMatch`].
//!
//! 3. **A window must know which conversation it was.** See
//!    [`resolve_session_ids`] — the hard half, because nothing in a `.meta`
//!    records it today.
//!
//! The layout JSON is emitted here rather than by shelling to `i3-save-tree`,
//! and installing that tool would not change it. It dumps a workspace whole —
//! browsers, editors and all — where we need agents only, with the survivors'
//! percentages renormalized; and it can only propose swallow criteria from a
//! window's *current* properties (class, instance, title), commented out for
//! you to edit by hand. Those are the three that cannot work here: every
//! Terminator window shares a class, carries no role, and shows a title Claude
//! overwrites on its first repaint. The criterion that does work is a handle we
//! mint for a window that does not exist yet, which nothing can introspect.
//! (Its output is also not valid JSON — comments plus concatenated objects —
//! and it would be a subprocess per workspace every 30 s, from a thread that
//! already holds the parsed tree.)

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::i3;

/// One layout write per this many seconds, across all wrappers. Short enough
/// that moving a window between workspaces is captured before you walk away,
/// long enough that twenty monitor threads offering a capture every 2s cost
/// nothing (all but one see a fresh file and return immediately).
const SNAPSHOT_INTERVAL_SECS: u64 = 30;

/// Seconds between window launches during a restore. Resuming a conversation
/// replays its transcript, so twenty at once would hammer a machine that is
/// still finishing its own boot.
const DEFAULT_STAGGER_SECS: u64 = 2;

pub(crate) fn snapshot_path() -> PathBuf {
    let dir = crate::vendor::herdr_detect::config::state_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("layout.json")
}

// ---------------------------------------------------------------------------
// Live sessions
// ---------------------------------------------------------------------------

/// A tracked agent that is still running, as read from its `.meta`.
struct Live {
    wrapper_pid: u64,
    child_pid: u64,
    cwd: String,
    argv: Vec<String>,
    con_id: Option<u64>,
    window_id: Option<u64>,
    started: u64,
    meta_path: PathBuf,
    /// The agent's own conversation id, once resolved (Claude only).
    session_id: Option<String>,
}

impl Live {
    /// The agent binary's basename — `claude`, `codex`, … Session-id
    /// resolution is Claude-shaped and only runs for `claude`.
    fn agent(&self) -> &str {
        self.argv
            .first()
            .map(|a| {
                Path::new(a)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(a.as_str())
            })
            .unwrap_or("")
    }

    fn dir(&self) -> String {
        Path::new(&self.cwd)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.cwd)
            .to_string()
    }
}

/// Every `.meta` whose child process is still alive. (Unlike `status::collect`
/// this deliberately does *not* reap the dead ones: a capture runs from inside
/// a wrapper's monitor thread and has no business deleting other sessions.)
fn live_sessions() -> Vec<Live> {
    let dir = crate::cache_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("meta") {
            continue;
        }
        let Some(v) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        let Some(child_pid) = v["child_pid"].as_u64() else {
            continue;
        };
        if !crate::procfs::alive(child_pid) {
            continue;
        }
        out.push(Live {
            wrapper_pid: v["wrapper_pid"].as_u64().unwrap_or(0),
            child_pid,
            cwd: v["cwd"].as_str().unwrap_or_default().to_string(),
            argv: v["argv"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            con_id: v["con_id"].as_u64(),
            window_id: v["window_id"].as_u64(),
            started: v["started"].as_u64().unwrap_or(0),
            session_id: v["session_id"].as_str().map(str::to_string),
            meta_path: path,
        });
    }
    out.sort_by_key(|s| s.wrapper_pid);
    out
}

/// Persist a resolved conversation id into the session's `.meta`, so later
/// captures skip the (comparatively expensive) resolution entirely.
fn record_session_id(meta_path: &Path, session_id: &str, source: &str) {
    let Some(mut meta) = std::fs::read_to_string(meta_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return;
    };
    if meta["session_id"].as_str() == Some(session_id) {
        return;
    }
    meta["session_id"] = json!(session_id);
    meta["session_id_source"] = json!(source);
    if let Ok(text) = serde_json::to_string_pretty(&meta) {
        let _ = std::fs::write(meta_path, text);
    }
}

// ---------------------------------------------------------------------------
// Which conversation is this window running?
// ---------------------------------------------------------------------------

/// `~/.claude/projects/<slug>` for a working directory. Claude Code slugs a
/// path by replacing every non-alphanumeric character with `-`, so
/// `/home/me/.config/i3` becomes `-home-me--config-i3`.
fn claude_project_dir(cwd: &str) -> Option<PathBuf> {
    let slug: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".claude/projects").join(slug))
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Pull conversation ids out of a running Claude's open file descriptors.
///
/// Claude keeps a directory fd on its per-session scratch area,
/// `/tmp/claude-<uid>/<project-slug>/<session-uuid>/…`, so the id is readable
/// straight off `/proc`. **Exact when present, but not always present** — the
/// fd only exists once a session has used the scratch area (background tasks,
/// subagents), which on the dev machine covered 14 of 20 live sessions.
fn session_id_from_proc(child_pid: u64, project_dir: &Path) -> Option<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|p| p.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|p| p.parse::<u64>().ok())
        else {
            continue;
        };
        // The agent is a child of the `__lead` anchor we recorded.
        if crate::procfs::parent_pid(pid) != Some(child_pid) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            for part in target.iter().filter_map(|p| p.to_str()) {
                // Validated against the session's own project dir: a bare
                // uuid-shaped path component proves nothing on its own.
                if is_uuid(part) && project_dir.join(format!("{part}.jsonl")).exists() {
                    found.push(part.to_string());
                }
            }
        }
    }
    found.sort();
    found.dedup();
    // Two different conversations behind one pid would mean we guessed wrong
    // about what the fd is; refuse rather than pick.
    if found.len() == 1 {
        found.pop()
    } else {
        None
    }
}

/// Every `aiTitle` recorded in a transcript. Claude writes the conversation's
/// generated title into the transcript *and* into the terminal's OSC title, so
/// this is what lets us match a live window to its conversation.
///
/// Only the tail is read: the title is re-stamped as the conversation grows
/// (a 4.8 MB transcript here carries one every ~35 KB), so the current one is
/// always near the end, and transcripts run to tens of megabytes.
fn transcript_titles(path: &Path) -> Vec<String> {
    let Some(buf) = crate::status::read_tail(path, 2 * 1024 * 1024) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in buf.split(|b| *b == b'\n') {
        // Cheap reject before paying for a JSON parse.
        if !line.windows(7).any(|w| w == b"aiTitle") {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<Value>(line) {
            if let Some(t) = v["aiTitle"].as_str() {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// Work out which conversation each live Claude window is running.
///
/// Two sources, strongest first — and the result is cached into the `.meta`,
/// so this whole pass is a no-op on every capture after the first.
///
/// 1. **`/proc` file descriptors** — exact, but only present for sessions that
///    have touched their scratch area.
/// 2. **Window title ↔ transcript `aiTitle`** — the window is showing the
///    conversation's own generated title, so a transcript in the same project
///    that carries that title is almost certainly the one. Accepted **only**
///    when it identifies exactly one *unclaimed* transcript: two windows in a
///    project can show the same title (a resumed session and its parent), and
///    the wrong conversation is worse than none. Candidates are limited to
///    transcripts appended to since the wrapper started, which is what a live
///    session must have been doing.
///
///    The title comes from the **i3 tree**, not from re-deriving the last OSC
///    title out of the teed stream: the tree holds what the terminal is
///    actually displaying, it costs nothing extra (the capture reads the tree
///    anyway), and the stream's last title sequence is not reliably the real
///    one — two live sessions here have a stray `;` title later in the tee
///    than their real one.
///
/// Measured on the dev machine's 20 live sessions: 14 exact from `/proc`, the
/// remaining 6 title-matched, with the two genuinely ambiguous pairs correctly
/// refused. (Matching on file mtime instead was tried and was wrong 3 times
/// out of 3 — it is not a fallback, it is a coin flip.)
///
/// Neither source survives `/clear`, which starts a new transcript under a new
/// id. The durable fix is Claude's `SessionStart` hook (ORCHESTRATION-SPEC),
/// which reports the id — including after `/clear` — for sessions launched
/// once that lands. `--refresh` re-resolves everything in the meantime.
fn resolve_session_ids(
    sessions: &mut [Live],
    titles: &HashMap<u64, String>,
    refresh: bool,
) {
    if refresh {
        for s in sessions.iter_mut() {
            s.session_id = None;
        }
    }

    // Project dir per session, and a set of ids already spoken for so the
    // title pass can never hand one conversation to two windows.
    let mut claimed: HashSet<String> = HashSet::new();
    let mut project: HashMap<u64, PathBuf> = HashMap::new();
    for s in sessions.iter() {
        if let Some(p) = claude_project_dir(&s.cwd) {
            project.insert(s.wrapper_pid, p);
        }
        if let Some(id) = &s.session_id {
            claimed.insert(id.clone());
        }
    }

    // Pass 1: exact, from /proc.
    for s in sessions.iter_mut() {
        if s.session_id.is_some() || s.agent() != "claude" {
            continue;
        }
        let Some(dir) = project.get(&s.wrapper_pid) else {
            continue;
        };
        if let Some(id) = session_id_from_proc(s.child_pid, dir) {
            if claimed.insert(id.clone()) {
                record_session_id(&s.meta_path, &id, "proc");
                s.session_id = Some(id);
            }
        }
    }

    // Pass 2: title match, over what pass 1 left unclaimed.
    for s in sessions.iter_mut() {
        if s.session_id.is_some() || s.agent() != "claude" {
            continue;
        }
        let Some(dir) = project.get(&s.wrapper_pid) else {
            continue;
        };
        let Some(title) = titles.get(&s.wrapper_pid) else {
            continue;
        };
        let title = crate::status::strip_title_glyph(title).to_string();
        if title.is_empty() {
            continue;
        }
        let mut hits: Vec<String> = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if claimed.contains(id) {
                continue;
            }
            // A live session has been appending since its wrapper started.
            let recent = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() >= s.started)
                .unwrap_or(false);
            if !recent {
                continue;
            }
            if transcript_titles(&path).iter().any(|t| *t == title) {
                hits.push(id.to_string());
            }
        }
        if hits.len() == 1 {
            let id = hits.pop().unwrap();
            claimed.insert(id.clone());
            record_session_id(&s.meta_path, &id, "title");
            s.session_id = Some(id);
        }
    }
}

// ---------------------------------------------------------------------------
// The i3 tree
// ---------------------------------------------------------------------------

/// Where a window sits: the workspace it is on, the output that workspace is
/// displayed on, and the title it is currently showing.
#[derive(Clone)]
struct Place {
    workspace: String,
    output: String,
    title: String,
    /// WM_CLASS general class — which terminal this agent is actually running
    /// in, so `restore` can put it back in the same one.
    class: String,
}

/// Index the tree: con_id → placement, and X window id → con_id (the durable
/// handle, for sessions whose recorded con_id died in an i3 restart).
fn index_tree(tree: &Value) -> (HashMap<u64, Place>, HashMap<u64, u64>) {
    let mut places = HashMap::new();
    let mut by_xid = HashMap::new();
    walk_index(tree, None, None, &mut places, &mut by_xid);
    (places, by_xid)
}

fn walk_index(
    node: &Value,
    output: Option<&str>,
    workspace: Option<&str>,
    places: &mut HashMap<u64, Place>,
    by_xid: &mut HashMap<u64, u64>,
) {
    let node_type = node["type"].as_str().unwrap_or("");
    let name = node["name"].as_str().unwrap_or("");
    let mut output = output;
    let mut workspace = workspace;
    match node_type {
        "output" => output = Some(name),
        // i3's scratchpad lives on a pseudo-workspace; it is not a place.
        "workspace" if name != "__i3_scratch" => workspace = Some(name),
        _ => {}
    }

    if let (Some(con_id), Some(ws)) = (node["id"].as_u64(), workspace) {
        if node.get("window").and_then(|w| w.as_u64()).is_some() {
            places.insert(
                con_id,
                Place {
                    workspace: ws.to_string(),
                    output: output.unwrap_or_default().to_string(),
                    // window_properties.title is what the terminal reports to
                    // X11 — i.e. Claude's own OSC title, already parsed for us.
                    title: node["window_properties"]["title"]
                        .as_str()
                        .unwrap_or(name)
                        .to_string(),
                    class: node["window_properties"]["class"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                },
            );
            if let Some(xid) = node["window"].as_u64() {
                by_xid.insert(xid, con_id);
            }
        }
    }

    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            for child in children {
                walk_index(child, output, workspace, places, by_xid);
            }
        }
    }
}

/// Find a workspace node by name.
fn workspace_node<'a>(tree: &'a Value, name: &str) -> Option<&'a Value> {
    if tree["type"].as_str() == Some("workspace") && tree["name"].as_str() == Some(name) {
        return Some(tree);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = tree[key].as_array() {
            for child in children {
                if let Some(found) = workspace_node(child, name) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn focused_workspace(tree: &Value) -> Option<String> {
    fn walk<'a>(node: &'a Value, ws: Option<&'a str>) -> Option<String> {
        let ws = if node["type"].as_str() == Some("workspace") {
            node["name"].as_str()
        } else {
            ws
        };
        if node["focused"].as_bool() == Some(true) {
            return ws.map(str::to_string);
        }
        for key in ["nodes", "floating_nodes"] {
            if let Some(children) = node[key].as_array() {
                for child in children {
                    if let Some(found) = walk(child, ws) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }
    walk(tree, None)
}

// ---------------------------------------------------------------------------
// Layout emission
// ---------------------------------------------------------------------------

/// The window handle a restored terminal is launched with, and the value its
/// placeholder swallows on. Unique per agent, so placeholders can never be
/// filled by the wrong window and launch order does not matter (verified: i3
/// matches by criteria, so launching out of order still fills the right slots).
pub(crate) fn handle_for(tag: &str) -> String {
    format!("seagents-{tag}")
}

/// Which X11 property carries that handle. Terminator has no `--class` but does
/// have `--role`; most other terminals are the other way round, so the property
/// travels with the terminal rather than being baked into the stored snapshot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WindowMatch {
    Role,
    Instance,
    Class,
}

impl WindowMatch {
    /// The key i3 uses for this property, in both swallow criteria and
    /// `window_properties`.
    pub(crate) fn key(self) -> &'static str {
        match self {
            WindowMatch::Role => "window_role",
            WindowMatch::Instance => "instance",
            WindowMatch::Class => "class",
        }
    }

    fn parse(s: &str) -> Option<WindowMatch> {
        match s {
            "role" | "window_role" => Some(WindowMatch::Role),
            "instance" => Some(WindowMatch::Instance),
            "class" => Some(WindowMatch::Class),
            _ => None,
        }
    }
}

/// How to launch a window seagents can recognize afterwards.
struct Terminal {
    argv: Vec<String>,
    matcher: WindowMatch,
}

/// Resolve the terminal to launch restored windows with.
///
/// Built-ins are the ones actually verified on this machine. Anything else goes
/// through `SEAGENTS_TERMINAL_CMD` (a template over `{handle}`, `{cwd}`,
/// `{cmd}`) paired with `SEAGENTS_TERMINAL_MATCH`, because a launch flag and
/// the criterion that finds the resulting window are one decision — set the
/// command without saying how to match it and every placeholder silently stays
/// empty.
fn terminal(handle: &str, cwd: &str, inner: &str, class: &str) -> Result<Terminal, String> {
    if let Ok(template) = std::env::var("SEAGENTS_TERMINAL_CMD") {
        let matcher = match std::env::var("SEAGENTS_TERMINAL_MATCH") {
            Ok(m) => WindowMatch::parse(&m).ok_or_else(|| {
                format!("SEAGENTS_TERMINAL_MATCH={m} is not one of role|instance|class")
            })?,
            // Most terminals set WM_CLASS and have no notion of a role.
            Err(_) => WindowMatch::Instance,
        };
        let argv: Vec<String> = template
            .split_whitespace()
            .map(|w| {
                w.replace("{handle}", handle)
                    .replace("{role}", handle)
                    .replace("{cwd}", cwd)
                    .replace("{cmd}", inner)
            })
            .collect();
        if argv.is_empty() {
            return Err("SEAGENTS_TERMINAL_CMD is empty".to_string());
        }
        return Ok(Terminal { argv, matcher });
    }

    // Explicit env wins; otherwise the terminal the agent was actually running
    // in when captured, so a restore reproduces what was there — which is what
    // makes a half-migrated setup (some agents in one terminal, some in
    // another) come back correctly.
    let name = std::env::var("SEAGENTS_TERMINAL")
        .ok()
        .or_else(|| recipe_for_class(class))
        .unwrap_or_else(|| "terminator".to_string());
    match name.as_str() {
        "terminator" => Ok(Terminal {
            argv: vec![
                "terminator".into(),
                // Its own process: a Terminator that delegates over DBus to a
                // running instance does not reliably apply --role, and the
                // handle is the entire swallow criterion.
                "-u".into(),
                "--role".into(),
                handle.into(),
                "--working-directory".into(),
                cwd.into(),
                // -x takes the rest of the line as argv, and must come last.
                "-x".into(),
                "/bin/sh".into(),
                "-c".into(),
                inner.into(),
            ],
            matcher: WindowMatch::Role,
        }),
        "alacritty" => Ok(Terminal {
            argv: vec![
                "alacritty".into(),
                // <general>,<instance>: keep the general class so existing i3
                // rules for Alacritty still apply.
                "--class".into(),
                format!("Alacritty,{handle}"),
                "--working-directory".into(),
                cwd.into(),
                "-e".into(),
                "/bin/sh".into(),
                "-c".into(),
                inner.into(),
            ],
            matcher: WindowMatch::Instance,
        }),
        // One process hosting every window. Cheaper per window, and no longer
        // costs anything in identity: a window created this way still tells its
        // child `ALACRITTY_WINDOW_ID`, so `run` identifies itself exactly even
        // though `_NET_WM_PID` is shared. Needs a daemon already running
        // (`alacritty --daemon`), otherwise `msg` exits with "no socket found".
        "alacritty-daemon" => Ok(Terminal {
            argv: vec![
                "alacritty".into(),
                "msg".into(),
                "create-window".into(),
                "--class".into(),
                format!("Alacritty,{handle}"),
                "--working-directory".into(),
                cwd.into(),
                "-e".into(),
                "/bin/sh".into(),
                "-c".into(),
                inner.into(),
            ],
            matcher: WindowMatch::Instance,
        }),
        other => Err(format!(
            "SEAGENTS_TERMINAL={other} has no built-in launch recipe; set \
             SEAGENTS_TERMINAL_CMD (with {{handle}}, {{cwd}}, {{cmd}}) and \
             SEAGENTS_TERMINAL_MATCH=role|instance|class"
        )),
    }
}

/// The built-in recipe matching a captured window's WM_CLASS, if there is one.
fn recipe_for_class(class: &str) -> Option<String> {
    match class.to_ascii_lowercase().as_str() {
        "terminator" => Some("terminator".to_string()),
        "alacritty" => Some("alacritty".to_string()),
        _ => None,
    }
}

/// The swallow criterion for one captured agent — i.e. which property the
/// terminal that will relaunch it is going to set.
fn matcher_for(class: &str) -> WindowMatch {
    terminal("", "", "", class)
        .map(|t| t.matcher)
        .unwrap_or(WindowMatch::Instance)
}

/// Mint a stable tag for an agent: the head of its conversation id when we
/// know it (stable across the reboot we are preparing for), else its wrapper
/// pid (unique among live sessions, which is all a single restore needs).
fn tag_for(s: &Live) -> String {
    match &s.session_id {
        Some(id) => id.chars().take(8).collect(),
        None => format!("p{}", s.wrapper_pid),
    }
}

/// Reduce a workspace's subtree to just the agent windows, keeping the split
/// structure that connects them.
///
/// Containers holding no agent are dropped, and a container left with a single
/// child is collapsed into it — otherwise pruning a 3-way split down to one
/// agent would restore a pointless chain of nested splits. Surviving siblings
/// have their `percent` renormalized to fill the space the dropped ones left.
///
/// Floating agents are folded into the tiled layout: i3's placeholder matching
/// is per-container, and a restored agent that lands tiled is a far smaller
/// surprise than one that does not come back at all.
fn prune_tree(node: &Value, tags: &HashMap<u64, String>) -> Option<Value> {
    let con_id = node["id"].as_u64();

    // A leaf window: keep it only if it is a tracked agent.
    if node.get("window").and_then(|w| w.as_u64()).is_some() {
        let tag = con_id.and_then(|id| tags.get(&id))?;
        return Some(json!({
            "type": "con",
            "name": node["name"].as_str().unwrap_or("agent"),
            "border": node["border"].as_str().unwrap_or("pixel"),
            "current_border_width": node["current_border_width"].as_i64().unwrap_or(1),
            "percent": node["percent"].as_f64().unwrap_or(1.0),
            // The agent's tag, not a finished swallow criterion: which X11
            // property carries it depends on the terminal used at *restore*
            // time, which capture time cannot know. `to_i3_nodes` turns this
            // into `swallows` when the layout is handed to i3.
            "tag": tag,
        }));
    }

    let mut kids: Vec<Value> = Vec::new();
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            for child in children {
                if let Some(k) = prune_tree(child, tags) {
                    kids.push(k);
                }
            }
        }
    }
    if kids.is_empty() {
        return None;
    }

    let percent = node["percent"].as_f64().unwrap_or(1.0);
    if kids.len() == 1 && node["type"].as_str() != Some("workspace") {
        // Collapse: the lone survivor takes this container's share.
        let mut only = kids.pop().unwrap();
        only["percent"] = json!(percent);
        return Some(only);
    }
    renormalize(&mut kids);

    if node["type"].as_str() == Some("workspace") {
        // The workspace itself is not emitted — append_layout appends its
        // children to whichever workspace is focused.
        return Some(json!({ "nodes": kids }));
    }
    Some(json!({
        "type": "con",
        "layout": node["layout"].as_str().unwrap_or("splith"),
        "percent": percent,
        "nodes": kids,
    }))
}

/// Scale a set of siblings' `percent` back up to sum to 1.0 after pruning.
fn renormalize(kids: &mut [Value]) {
    let total: f64 = kids
        .iter()
        .map(|k| k["percent"].as_f64().unwrap_or(0.0))
        .sum();
    if total <= 0.0 {
        let share = 1.0 / kids.len() as f64;
        for k in kids.iter_mut() {
            k["percent"] = json!(share);
        }
        return;
    }
    for k in kids.iter_mut() {
        let p = k["percent"].as_f64().unwrap_or(0.0) / total;
        k["percent"] = json!(p);
    }
}

/// Re-prune a *stored* layout, dropping the placeholders whose agents are not
/// being restored (already running, or filtered out by `--workspace`). Mirrors
/// [`prune_tree`]'s collapse and renormalize rules so a partial restore has the
/// same shape a full one would.
fn prune_layout(nodes: &[Value], keep: &HashSet<String>) -> Vec<Value> {
    let mut out = Vec::new();
    for node in nodes {
        if let Some(tag) = leaf_tag(node) {
            if keep.contains(&tag) {
                out.push(node.clone());
            }
            continue;
        }
        let children = node["nodes"].as_array().map(|v| v.as_slice()).unwrap_or(&[]);
        let mut kids = prune_layout(children, keep);
        if kids.is_empty() {
            continue;
        }
        if kids.len() == 1 {
            let mut only = kids.pop().unwrap();
            only["percent"] = node["percent"].clone();
            out.push(only);
            continue;
        }
        renormalize(&mut kids);
        let mut container = node.clone();
        container["nodes"] = json!(kids);
        out.push(container);
    }
    renormalize(&mut out);
    out
}

/// The agent a placeholder is holding a slot for.
fn leaf_tag(node: &Value) -> Option<String> {
    node["tag"].as_str().map(str::to_string)
}

/// Turn the stored layout into what i3's `append_layout` expects: every leaf's
/// `tag` becomes a swallow criterion on whichever property this terminal
/// actually sets.
fn to_i3_nodes(nodes: &[Value], matchers: &HashMap<String, WindowMatch>) -> Vec<Value> {
    nodes
        .iter()
        .map(|node| {
            let mut out = node.clone();
            if let Some(tag) = leaf_tag(node) {
                let matcher = matchers
                    .get(&tag)
                    .copied()
                    .unwrap_or(WindowMatch::Instance);
                out.as_object_mut().map(|o| o.remove("tag"));
                out["swallows"] = json!([{
                    matcher.key(): format!("^{}$", handle_for(&tag))
                }]);
            } else if let Some(kids) = node["nodes"].as_array() {
                out["nodes"] = json!(to_i3_nodes(kids, matchers));
            }
            out
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// True when the snapshot on disk has gone stale.
fn capture_due() -> bool {
    !std::fs::metadata(snapshot_path())
        .and_then(|m| m.modified())
        .map(|t| {
            t.elapsed()
                .map(|e| e.as_secs() < SNAPSHOT_INTERVAL_SECS)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Offered by every wrapper's monitor thread, every tick, and by `status`.
/// Globally throttled by the snapshot file's own mtime, so exactly one caller
/// pays for a capture per interval and the rest return after a single `stat`.
pub(crate) fn maybe_capture() {
    if capture_due() {
        let _ = capture(false);
    }
}

/// Same, off the calling thread — for the HUD, whose event loop must not stall
/// for the length of an `i3-msg` round trip.
pub(crate) fn maybe_capture_async() {
    if capture_due() {
        std::thread::spawn(|| {
            let _ = capture(false);
        });
    }
}

/// Build the snapshot and write it atomically. Returns it for callers that
/// want to report on what was captured.
fn capture(refresh: bool) -> Result<Value, String> {
    let mut sessions = live_sessions();
    let tree = i3::tree().ok_or("could not read the i3 tree (is i3-msg on PATH?)")?;
    let (places, by_xid) = index_tree(&tree);

    // Locate every agent on screen first: the window it lives in supplies the
    // title that identifies its conversation.
    //
    // Two sessions can name the same window — an agent launched while its own
    // terminal was still mapping records whatever was focused instead, and
    // `--role` only covers windows seagents launched. At most one of them can
    // be right and nothing here can say which, so the earliest launch keeps
    // the window (the later one is the one that read stale focus) and the rest
    // are reported unplaced. Typing into them re-captures and heals it.
    let mut located: HashMap<u64, u64> = HashMap::new();
    let mut owner: HashMap<u64, (u64, u64)> = HashMap::new(); // con_id -> (started, wrapper)
    for s in &sessions {
        // Recorded con_id first; the X window id survives an i3 restart and
        // heals it, which is the same order `focus` resolves in.
        let Some(con_id) = s
            .con_id
            .filter(|id| places.contains_key(id))
            .or_else(|| s.window_id.and_then(|x| by_xid.get(&x).copied()))
        else {
            continue;
        };
        match owner.get(&con_id) {
            Some((started, prev)) if *started <= s.started => {
                let _ = prev;
                continue;
            }
            Some((_, prev)) => {
                located.remove(prev);
            }
            None => {}
        }
        owner.insert(con_id, (s.started, s.wrapper_pid));
        located.insert(s.wrapper_pid, con_id);
    }
    let titles: HashMap<u64, String> = located
        .iter()
        .map(|(wrapper, con_id)| (*wrapper, places[con_id].title.clone()))
        .collect();

    resolve_session_ids(&mut sessions, &titles, refresh);

    // con_id → tag, for the agents we can actually locate on screen.
    let mut tags: HashMap<u64, String> = HashMap::new();
    let mut placed: HashMap<String, Vec<&Live>> = HashMap::new();
    let mut outputs: HashMap<String, String> = HashMap::new();
    let mut unplaced: Vec<Value> = Vec::new();

    for s in &sessions {
        let Some(con_id) = located.get(&s.wrapper_pid).copied() else {
            unplaced.push(json!({
                "cwd": s.cwd,
                "dir": s.dir(),
                "argv": s.argv,
                "session_id": s.session_id,
            }));
            continue;
        };
        let place = &places[&con_id];
        tags.insert(con_id, tag_for(s));
        outputs.insert(place.workspace.clone(), place.output.clone());
        placed.entry(place.workspace.clone()).or_default().push(s);
    }

    let mut workspaces = Vec::new();
    let mut names: Vec<&String> = placed.keys().collect();
    names.sort_by_key(|n| workspace_sort_key(n));
    for name in names {
        let Some(ws_node) = workspace_node(&tree, name) else {
            continue;
        };
        let Some(pruned) = prune_tree(ws_node, &tags) else {
            continue;
        };
        let layout = pruned["nodes"].as_array().cloned().unwrap_or_default();
        let agents: Vec<Value> = placed[name]
            .iter()
            .map(|s| {
                json!({
                    "tag": tag_for(s),
                    "cwd": s.cwd,
                    "dir": s.dir(),
                    "argv": s.argv,
                    "session_id": s.session_id,
                    "terminal_class": located
                        .get(&s.wrapper_pid)
                        .map(|c| places[c].class.clone()),
                })
            })
            .collect();
        workspaces.push(json!({
            "name": name,
            "output": outputs.get(name).cloned().unwrap_or_default(),
            "agents": agents,
            "layout": layout,
        }));
    }

    let snapshot = json!({
        "version": 1,
        "captured": crate::clock::unix_now(),
        "focused_workspace": focused_workspace(&tree),
        "workspaces": workspaces,
        "unplaced": unplaced,
    });

    let path = snapshot_path();
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(snapshot)
}

/// Sort workspaces the way i3 orders them: numbered ones numerically first,
/// named ones alphabetically after.
fn workspace_sort_key(name: &str) -> (u8, u64, String) {
    let num: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    match num.parse::<u64>() {
        Ok(n) => (0, n, name.to_string()),
        Err(_) => (1, 0, name.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `seagents snapshot [--refresh] [--print]` — force a capture now.
pub fn snapshot_cmd(args: &[String]) -> i32 {
    let mut refresh = false;
    let mut print_json = false;
    for a in args {
        match a.as_str() {
            "--refresh" => refresh = true,
            "--print" => print_json = true,
            other => {
                eprintln!("seagents snapshot: unknown option {other}");
                return 2;
            }
        }
    }
    match capture(refresh) {
        Ok(snapshot) => {
            if print_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&snapshot).unwrap_or_default()
                );
                return 0;
            }
            summarize(&snapshot);
            println!("\nsaved to {}", snapshot_path().display());
            0
        }
        Err(e) => {
            eprintln!("seagents: {e}");
            1
        }
    }
}

fn summarize(snapshot: &Value) {
    let empty = Vec::new();
    let workspaces = snapshot["workspaces"].as_array().unwrap_or(&empty);
    if workspaces.is_empty() {
        println!("no agent windows to snapshot");
    }
    for ws in workspaces {
        let agents = ws["agents"].as_array().unwrap_or(&empty);
        println!(
            "workspace {} ({}) — {} agent{}",
            ws["name"].as_str().unwrap_or("?"),
            ws["output"].as_str().unwrap_or("?"),
            agents.len(),
            if agents.len() == 1 { "" } else { "s" }
        );
        for a in agents {
            let id = a["session_id"].as_str();
            println!(
                "  {:<24}  {:<28}  {}",
                a["dir"].as_str().unwrap_or("?"),
                a["argv"]
                    .as_array()
                    .map(|v| v
                        .iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" "))
                    .unwrap_or_default(),
                match id {
                    Some(id) => format!("resume {}", &id[..8.min(id.len())]),
                    None => "no session id — restores fresh".to_string(),
                }
            );
        }
    }
    let unplaced = snapshot["unplaced"].as_array().unwrap_or(&empty);
    if !unplaced.is_empty() {
        println!(
            "\n{} agent{} not captured (window never identified — type into it once):",
            unplaced.len(),
            if unplaced.len() == 1 { "" } else { "s" }
        );
        for a in unplaced {
            println!("  {}", a["dir"].as_str().unwrap_or("?"));
        }
    }
}

/// `seagents restore [--dry-run] [--stagger=N] [--workspace=NAME]`
pub fn restore_cmd(args: &[String]) -> i32 {
    let mut dry_run = false;
    let mut stagger = DEFAULT_STAGGER_SECS;
    let mut only: Option<String> = None;
    for a in args {
        match a.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            s if s.starts_with("--stagger=") => match s[10..].parse::<u64>() {
                Ok(n) => stagger = n,
                Err(_) => {
                    eprintln!("seagents restore: --stagger wants seconds, got {}", &s[10..]);
                    return 2;
                }
            },
            s if s.starts_with("--workspace=") => only = Some(s[12..].to_string()),
            other => {
                eprintln!("seagents restore: unknown option {other}");
                return 2;
            }
        }
    }

    let path = snapshot_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!(
            "seagents: no snapshot at {} — none is written until an agent has run \
             under `seagents run`",
            path.display()
        );
        return 1;
    };
    let Ok(snapshot) = serde_json::from_str::<Value>(&text) else {
        eprintln!("seagents: {} is not readable as a snapshot", path.display());
        return 1;
    };

    // Never restore something that is already on screen: `restore` has to be
    // safe to run twice, and safe to run when only half the agents died.
    //
    // A conversation id settles it outright. Agents without one (a
    // non-Claude tool, or a session whose id we could not pin down) are
    // matched *by count* per launch signature instead: four identical
    // `claude` windows in one project are indistinguishable, so if three are
    // running and four were captured, exactly one is missing and exactly one
    // should come back. Signatures ignore `--resume`, so an agent restored
    // earlier still counts against its own captured slot before its id is
    // known.
    let live = live_sessions();
    let live_ids: HashSet<String> = live.iter().filter_map(|s| s.session_id.clone()).collect();
    let captured_ids: HashSet<String> = snapshot["workspaces"]
        .as_array()
        .map(|wss| {
            wss.iter()
                .filter_map(|ws| ws["agents"].as_array())
                .flatten()
                .filter_map(|a| a["session_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut live_counts: HashMap<String, usize> = HashMap::new();
    for s in &live {
        // Sessions already accounted for by the id match must not also
        // consume a by-signature slot.
        if s.session_id
            .as_ref()
            .map(|id| captured_ids.contains(id))
            .unwrap_or(false)
        {
            continue;
        }
        *live_counts
            .entry(launch_signature(&s.cwd, &s.argv))
            .or_insert(0) += 1;
    }

    let empty = Vec::new();
    let mut launched = 0usize;
    let mut skipped = 0usize;
    for ws in snapshot["workspaces"].as_array().unwrap_or(&empty) {
        let name = ws["name"].as_str().unwrap_or_default().to_string();
        if let Some(only) = &only {
            if *only != name {
                continue;
            }
        }
        let agents = ws["agents"].as_array().unwrap_or(&empty);
        let mut todo: Vec<&Value> = Vec::new();
        for a in agents {
            if let Some(id) = a["session_id"].as_str() {
                if live_ids.contains(id) {
                    skipped += 1;
                    continue;
                }
            }
            let key = launch_signature(
                a["cwd"].as_str().unwrap_or_default(),
                &argv_of(a),
            );
            if let Some(n) = live_counts.get_mut(&key).filter(|n| **n > 0) {
                *n -= 1;
                skipped += 1;
                continue;
            }
            todo.push(a);
        }
        if todo.is_empty() {
            continue;
        }

        let keep: HashSet<String> = todo
            .iter()
            .filter_map(|a| a["tag"].as_str().map(str::to_string))
            .collect();
        let layout = prune_layout(ws["layout"].as_array().unwrap_or(&empty), &keep);
        // Per agent, not per restore: mid-migration a workspace can hold
        // windows from two different terminals, which swallow on different
        // properties.
        let matchers: HashMap<String, WindowMatch> = todo
            .iter()
            .filter_map(|a| {
                let tag = a["tag"].as_str()?;
                Some((
                    tag.to_string(),
                    matcher_for(a["terminal_class"].as_str().unwrap_or_default()),
                ))
            })
            .collect();

        println!(
            "workspace {name}: {} window{}",
            todo.len(),
            if todo.len() == 1 { "" } else { "s" }
        );
        for a in &todo {
            let cmd = resume_argv(a);
            println!("  {:<24}  {}", a["dir"].as_str().unwrap_or("?"), cmd.join(" "));
        }
        if dry_run {
            launched += todo.len();
            continue;
        }

        if let Err(e) = place_workspace(
            &name,
            ws["output"].as_str().unwrap_or_default(),
            &layout,
            &matchers,
        ) {
            eprintln!("seagents: workspace {name}: {e}");
            continue;
        }
        for (i, a) in todo.iter().enumerate() {
            if i > 0 && stagger > 0 {
                std::thread::sleep(std::time::Duration::from_secs(stagger));
            }
            let tag = a["tag"].as_str().unwrap_or_default();
            let cwd = a["cwd"].as_str().unwrap_or_default();
            if let Err(e) = spawn_terminal(
                tag,
                cwd,
                &resume_argv(a),
                a["terminal_class"].as_str().unwrap_or_default(),
            ) {
                eprintln!("seagents: {}: {e}", a["dir"].as_str().unwrap_or("?"));
                continue;
            }
            launched += 1;
        }
    }

    if !dry_run {
        // Leave the user looking where they were looking, not on whichever
        // workspace happened to be restored last.
        if let Some(ws) = snapshot["focused_workspace"].as_str() {
            let _ = i3::msg(&format!(
                "workspace --no-auto-back-and-forth {}",
                i3::quote(ws)
            ));
        }
    }

    let unplaced = snapshot["unplaced"].as_array().unwrap_or(&empty);
    if !unplaced.is_empty() {
        println!(
            "\nnot restored — window was never identified when captured: {}",
            unplaced
                .iter()
                .filter_map(|a| a["dir"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if skipped > 0 {
        println!("{skipped} already running, left alone");
        if launched > 0 && !dry_run {
            // i3 appends a layout to a *workspace*; there is no way to drop a
            // placeholder into a container that already holds live windows. A
            // restore after a reboot — nothing running — is exact; this one
            // could only put the survivors' missing neighbours alongside them.
            println!(
                "note: restoring alongside live windows places them at workspace level, \
                 not back inside their original split"
            );
        }
    }
    println!(
        "{} {} window{}",
        if dry_run { "would restore" } else { "restored" },
        launched,
        if launched == 1 { "" } else { "s" }
    );
    0
}

/// The command a restored window runs: the agent's original argv with any
/// stale `--resume`/`-r` replaced by the conversation we captured. Model flags
/// and anything else the user launched with are preserved.
fn resume_argv(agent: &Value) -> Vec<String> {
    let mut out = argv_without_resume(&argv_of(agent));
    if let Some(id) = agent["session_id"].as_str() {
        out.push("--resume".to_string());
        out.push(id.to_string());
    }
    out
}

fn argv_of(agent: &Value) -> Vec<String> {
    agent["argv"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| vec!["claude".to_string()])
}

/// An argv with any session-selection flag removed, so "the same agent
/// launched again" compares equal whether or not it was resumed.
fn argv_without_resume(argv: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut it = argv.iter().peekable();
    while let Some(arg) = it.next() {
        if arg == "--resume" || arg == "-r" || arg == "--continue" || arg == "-c" {
            // Drop the flag, and its value when it carries one.
            if it.peek().map(|v| is_uuid(v)).unwrap_or(false) {
                it.next();
            }
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// What makes two agent launches interchangeable: same project, same command.
fn launch_signature(cwd: &str, argv: &[String]) -> String {
    let mut key = String::from(cwd);
    for arg in argv_without_resume(argv) {
        key.push('\u{1}');
        key.push_str(&arg);
    }
    key
}

/// Focus the workspace, put it on the output it came from, and append the
/// placeholder layout to it.
fn place_workspace(
    name: &str,
    output: &str,
    layout: &[Value],
    matchers: &HashMap<String, WindowMatch>,
) -> Result<(), String> {
    i3::msg(&format!(
        "workspace --no-auto-back-and-forth {}",
        i3::quote(name)
    ))?;
    if !output.is_empty() && i3::active_outputs().len() > 1 {
        // Best-effort: a monitor that isn't plugged in now is not an error.
        let _ = i3::msg(&format!("move workspace to output {}", i3::quote(output)));
        i3::msg(&format!(
            "workspace --no-auto-back-and-forth {}",
            i3::quote(name)
        ))?;
    }
    if layout.is_empty() {
        return Ok(());
    }

    // i3 reads the layout from a file, and wants each of the workspace's
    // children as its own top-level object (i3-save-tree's shape) — not an
    // array.
    let file = std::env::temp_dir().join(format!("seagents-restore-{}.json", std::process::id()));
    let mut text = String::new();
    for node in to_i3_nodes(layout, matchers) {
        text.push_str(&serde_json::to_string_pretty(&node).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    std::fs::write(&file, text).map_err(|e| format!("writing {}: {e}", file.display()))?;
    let result = i3::msg(&format!("append_layout {}", file.display()));
    let _ = std::fs::remove_file(&file);
    result
}

/// Launch one restored window.
///
/// The agent runs under `seagents run` so the restored session is teed and
/// tracked from its first byte, and the shell survives it — closing the agent
/// leaves the window sitting at a prompt, exactly as it does when you start one
/// by hand.
///
/// `SEAGENTS_TERMINAL_CMD` overrides the whole invocation for terminals other
/// than Terminator; `{role}`, `{cwd}` and `{cmd}` are substituted.
fn spawn_terminal(tag: &str, cwd: &str, argv: &[String], class: &str) -> Result<(), String> {
    let handle = handle_for(tag);
    // `--window` so the wrapper identifies its own window by handle rather than
    // by winning a focus race it cannot win.
    let inner = format!(
        "seagents run --window {} -- {}; exec \"$SHELL\"",
        shell_quote(&handle),
        argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
    );
    let term = terminal(&handle, cwd, &inner, class)?;
    let Some((program, rest)) = term.argv.split_first() else {
        return Err("terminal command is empty".to_string());
    };

    Command::new(program)
        .args(rest)
        .current_dir(if Path::new(cwd).is_dir() { cwd } else { "/" })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("launching {program}: {e}"))
}

/// Single-quote for `sh -c`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_a_working_directory_the_way_claude_does() {
        let dir = claude_project_dir("/home/me/.config/i3").unwrap();
        assert_eq!(dir.file_name().unwrap(), "-home-me--config-i3");
        let dir = claude_project_dir("/home/me/projects/x_y").unwrap();
        assert_eq!(dir.file_name().unwrap(), "-home-me-projects-x-y");
    }

    #[test]
    fn recognizes_session_ids() {
        assert!(is_uuid("5c603939-bf27-47c8-8b0f-13731d2cc6eb"));
        assert!(!is_uuid("5c603939-bf27-47c8-8b0f-13731d2cc6e"));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("5c603939zbf27-47c8-8b0f-13731d2cc6eb"));
    }

    #[test]
    fn resume_argv_replaces_a_stale_resume_and_keeps_other_flags() {
        let agent = json!({
            "argv": ["claude", "--resume", "cc5834d8-7172-443e-a34f-b7eca4047cf5", "--model", "claude-opus-4-8"],
            "session_id": "0885fa15-47cf-4903-9d97-db59762dc210",
        });
        assert_eq!(
            resume_argv(&agent),
            vec![
                "claude",
                "--model",
                "claude-opus-4-8",
                "--resume",
                "0885fa15-47cf-4903-9d97-db59762dc210"
            ]
        );
    }

    #[test]
    fn resume_argv_drops_a_valueless_resume_and_restores_fresh_without_an_id() {
        let agent = json!({ "argv": ["claude", "--resume"], "session_id": null });
        assert_eq!(resume_argv(&agent), vec!["claude"]);
    }

    #[test]
    fn pruning_keeps_only_agent_windows_and_collapses_what_is_left() {
        // A workspace split in two: left is a browser, right is a v-split of
        // one agent and one editor. Only the agent survives, so the whole
        // workspace should collapse to a single full-width placeholder.
        let tags = HashMap::from([(3u64, "abc".to_string())]);
        let tree = json!({
            "type": "workspace", "name": "9", "layout": "splith",
            "nodes": [
                { "type": "con", "id": 1, "window": 100, "percent": 0.5, "name": "chrome" },
                { "type": "con", "id": 2, "layout": "splitv", "percent": 0.5, "nodes": [
                    { "type": "con", "id": 3, "window": 300, "percent": 0.6, "name": "✳ agent" },
                    { "type": "con", "id": 4, "window": 400, "percent": 0.4, "name": "code" },
                ]},
            ],
        });
        let pruned = prune_tree(&tree, &tags).unwrap();
        let nodes = pruned["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["tag"].as_str(), Some("abc"));
        // The criterion is chosen when the layout is handed to i3, not stored,
        // and it is per agent — a half-migrated workspace holds windows from
        // two terminals that swallow on different properties.
        let role = to_i3_nodes(
            nodes,
            &HashMap::from([("abc".to_string(), WindowMatch::Role)]),
        );
        assert_eq!(
            role[0]["swallows"][0]["window_role"].as_str(),
            Some("^seagents-abc$")
        );
        assert!(role[0].get("tag").is_none());
        let instance = to_i3_nodes(
            nodes,
            &HashMap::from([("abc".to_string(), WindowMatch::Instance)]),
        );
        assert_eq!(
            instance[0]["swallows"][0]["instance"].as_str(),
            Some("^seagents-abc$")
        );
        // Sole survivor fills the workspace.
        assert_eq!(nodes[0]["percent"].as_f64(), Some(1.0));
    }

    #[test]
    fn pruning_renormalizes_surviving_siblings() {
        let tags = HashMap::from([(1u64, "a".to_string()), (2u64, "b".to_string())]);
        let tree = json!({
            "type": "workspace", "name": "2", "layout": "splith",
            "nodes": [
                { "type": "con", "id": 1, "window": 1, "percent": 0.25, "name": "a" },
                { "type": "con", "id": 2, "window": 2, "percent": 0.25, "name": "b" },
                { "type": "con", "id": 3, "window": 3, "percent": 0.5, "name": "browser" },
            ],
        });
        let pruned = prune_tree(&tree, &tags).unwrap();
        let nodes = pruned["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["percent"].as_f64(), Some(0.5));
        assert_eq!(nodes[1]["percent"].as_f64(), Some(0.5));
    }

    #[test]
    fn a_workspace_with_no_agents_produces_no_layout() {
        let tags = HashMap::new();
        let tree = json!({
            "type": "workspace", "name": "1", "layout": "splith",
            "nodes": [ { "type": "con", "id": 1, "window": 1, "percent": 1.0, "name": "chrome" } ],
        });
        assert!(prune_tree(&tree, &tags).is_none());
    }

    #[test]
    fn partial_restore_reprunes_the_stored_layout_by_tag() {
        let tags = HashMap::from([(1u64, "a".to_string()), (2u64, "b".to_string())]);
        let tree = json!({
            "type": "workspace", "name": "2", "layout": "splith",
            "nodes": [
                { "type": "con", "id": 1, "window": 1, "percent": 0.5, "name": "a" },
                { "type": "con", "id": 2, "layout": "splitv", "percent": 0.5, "nodes": [
                    { "type": "con", "id": 2, "window": 2, "percent": 1.0, "name": "b" },
                ]},
            ],
        });
        let layout = prune_tree(&tree, &tags).unwrap()["nodes"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(layout.len(), 2);

        // Agent "a" is already running: only b's placeholder should be laid out.
        let keep = HashSet::from(["b".to_string()]);
        let kept = prune_layout(&layout, &keep);
        assert_eq!(kept.len(), 1);
        assert_eq!(leaf_tag(&kept[0]).as_deref(), Some("b"));
        assert_eq!(kept[0]["percent"].as_f64(), Some(1.0));
    }

    #[test]
    fn the_swallow_property_follows_the_terminal_the_agent_was_running_in() {
        // No SEAGENTS_TERMINAL override in the test environment, so the
        // captured WM_CLASS decides.
        if std::env::var_os("SEAGENTS_TERMINAL").is_none()
            && std::env::var_os("SEAGENTS_TERMINAL_CMD").is_none()
        {
            assert_eq!(matcher_for("Terminator"), WindowMatch::Role);
            assert_eq!(matcher_for("Alacritty"), WindowMatch::Instance);
            // Unknown terminal: fall back to the Terminator recipe, whose
            // property is a role.
            assert_eq!(matcher_for("Xterm"), WindowMatch::Role);
        }
        assert_eq!(recipe_for_class("alacritty").as_deref(), Some("alacritty"));
        assert_eq!(recipe_for_class("Konsole"), None);
    }

    #[test]
    fn launch_signatures_ignore_how_a_session_was_selected() {
        let fresh = launch_signature("/p", &["claude".into(), "--model".into(), "opus".into()]);
        let resumed = launch_signature(
            "/p",
            &[
                "claude".into(),
                "--model".into(),
                "opus".into(),
                "--resume".into(),
                "5c603939-bf27-47c8-8b0f-13731d2cc6eb".into(),
            ],
        );
        assert_eq!(fresh, resumed);
        // Different project, or different flags, are different agents.
        assert_ne!(fresh, launch_signature("/q", &["claude".into()]));
        assert_ne!(fresh, launch_signature("/p", &["claude".into()]));
    }

    #[test]
    fn workspaces_sort_numerically_then_by_name() {
        let mut names = vec!["10", "2", "scratch", "9"];
        names.sort_by_key(|n| workspace_sort_key(n));
        assert_eq!(names, vec!["2", "9", "10", "scratch"]);
    }

    #[test]
    fn shell_quoting_survives_a_prompt_full_of_quotes() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_quote("plain"), "'plain'");
    }
}
