//! Surviving a reboot.
//!
//! `seagents restore` rebuilds the i3 workspaces the agents were living on and
//! relaunches each window on its *own* conversation (`claude --resume <id>`),
//! so a reboot costs you the processes but not the layout or the context.
//!
//! Three problems, three answers:
//!
//! 1. **The layout must outlive the processes.** `~/.cache/seagents/` is
//!    self-reaping — the first `status` after a boot deletes every session
//!    whose `run` is gone, which is precisely all of them. So the snapshot
//!    lives in `state_dir()/layout.json`, written *continuously* (each
//!    `run`'s monitor thread offers a capture every tick, globally throttled
//!    to one write per [`SNAPSHOT_INTERVAL_SECS`]). An unplanned reboot loses
//!    at most that interval, and there is no command to remember. The first
//!    capture of the next boot sets the old boot's layout aside before
//!    overwriting it, and `restore` reads that one first ([`write_layout`],
//!    [`restore_source`]).
//!
//! 2. **A window must be re-fillable.** i3's `append_layout` matches
//!    placeholders against *swallow criteria*, and an ordinary terminal window
//!    offers nothing stable: Terminator windows all share one class, carry no
//!    role, and show a title Claude overwrites on the first repaint. So
//!    restored windows are launched carrying a minted handle,
//!    `seagents-<tag>`, and the placeholder swallows on that alone. (Same
//!    handle `spawn` opens its windows with.) *Which* X11 property
//!    carries it is the terminal's business — a role for Terminator, a
//!    WM_CLASS instance for most others — so the stored layout keeps the tag
//!    and the criterion is built at restore time. See [`WindowMatch`].
//!
//! 3. **A window must know which conversation it was.** The agent says so
//!    itself: the `SessionStart` hook `run` injects records the conversation
//!    id in the session's `.agent` (again after every `/clear`), and the
//!    capture copies it into the layout for `restore` to resume.
//!
//! The agents themselves live in a herdr server, which keeps its workspaces
//! and tabs across its own restart but not the processes in them: after a
//! reboot every agent is gone, and `restore` relaunches each through
//! `seagents run --tag <its tag>`, which takes back the tab herdr kept for it
//! (`run::place`).
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

use serde_json::{json, Value};

use crate::i3;
use crate::launch::{self, WindowMatch};
use crate::registry::Session;

/// One layout write per this many seconds, across all `run`s. Short enough
/// that moving a window between workspaces is captured before you walk away,
/// long enough that twenty monitor threads offering a capture every 2s cost
/// nothing (all but one see a fresh file and return immediately).
const SNAPSHOT_INTERVAL_SECS: u64 = 30;

/// Seconds between window launches during a restore. Resuming a conversation
/// replays its transcript, so twenty at once would hammer a machine that is
/// still finishing its own boot.
pub(crate) const DEFAULT_STAGGER_SECS: u64 = 2;

/// The live snapshot: rewritten every [`SNAPSHOT_INTERVAL_SECS`] by whoever
/// offers first.
const LAYOUT: &str = "layout.json";
/// The last layout of a previous boot, set aside by the first capture of this
/// one so that capture can carry on overwriting [`LAYOUT`]. A reboot leaves
/// nothing running, so without this the first `status`, HUD open or new agent
/// of the new boot would replace the layout `restore` exists to bring back
/// with an empty one.
const PREV_BOOT: &str = "layout.prev-boot.json";
/// [`PREV_BOOT`] once a restore has used it. Renamed rather than deleted: it
/// costs nothing, and it is the only record of what a botched restore was
/// working from (`mv` it back to retry).
const RESTORED: &str = "layout.restored.json";

/// The state dir, private (`fsutil::private_dir`): the layout names every
/// agent's directory and conversation.
fn layout_dir() -> PathBuf {
    let dir = crate::fsutil::state_dir();
    let _ = crate::fsutil::private_dir(&dir);
    dir
}

/// When this boot began. `SEAGENTS_TEST_BTIME` is **test-only**: it stands in
/// for `/proc/stat`'s btime so `tools/test-restore.sh` can play a reboot
/// without one. Nothing else should set it.
fn current_boot() -> u64 {
    std::env::var("SEAGENTS_TEST_BTIME")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(crate::procfs::boot_time)
        .unwrap_or(0)
}

fn read_layout(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Does this snapshot record anything a restore could launch?
fn has_agents(snapshot: &Value) -> bool {
    snapshot["workspaces"].as_array().is_some_and(|ws| {
        ws.iter()
            .any(|w| w["agents"].as_array().is_some_and(|a| !a.is_empty()))
    })
}

/// Write `snapshot` as `dir`'s live layout, first setting aside a live layout
/// captured before `boot` began (a previous boot's) as [`PREV_BOOT`].
///
/// Serialized on `layout.lock`: after a reboot several callers can offer the
/// first capture at once, and the decision has to be re-made under the lock
/// against what is on disk *now* — otherwise a second capturer that read the
/// old file too would set aside the first one's fresh, near-empty layout over
/// the one worth keeping.
///
/// A previous boot's layout that holds no agents is not set aside, just
/// overwritten: it can restore nothing, and setting it aside would replace a
/// still-unrestored one from the boot before (reboot, a brief boot that
/// captures an empty layout, reboot again).
fn write_layout(dir: &Path, snapshot: &Value, boot: u64) -> Result<(), String> {
    let _lock = crate::registry::Lock::acquire(&dir.join("layout.lock"));
    let path = dir.join(LAYOUT);
    if let Some(old) = read_layout(&path) {
        let previous_boot = old["captured"].as_u64().is_some_and(|t| t < boot);
        if previous_boot && has_agents(&old) {
            std::fs::rename(&path, dir.join(PREV_BOOT))
                .map_err(|e| format!("setting aside {}: {e}", path.display()))?;
        }
    }
    let text = serde_json::to_string_pretty(snapshot).map_err(|e| e.to_string())?;
    crate::fsutil::write_atomic(&path, text.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// The layout `restore` works from: a previous boot's that no restore has used
/// yet, else the live one — the same-boot case, where terminals closed or
/// crashed without a reboot. `true` when it is the previous boot's.
fn restore_source(dir: &Path) -> (PathBuf, bool) {
    let prev = dir.join(PREV_BOOT);
    if prev.exists() {
        (prev, true)
    } else {
        (dir.join(LAYOUT), false)
    }
}

/// Mark the previous boot's layout used, so the next restore goes back to the
/// live one.
fn consume_prev_boot(dir: &Path) -> Result<(), String> {
    let _lock = crate::registry::Lock::acquire(&dir.join("layout.lock"));
    let prev = dir.join(PREV_BOOT);
    std::fs::rename(&prev, dir.join(RESTORED)).map_err(|e| format!("{}: {e}", prev.display()))
}

// ---------------------------------------------------------------------------
// The i3 tree
// ---------------------------------------------------------------------------

/// Where a window sits: the workspace it is on and the output that workspace
/// is displayed on.
#[derive(Clone)]
struct Place {
    workspace: String,
    output: String,
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
        let children = node["nodes"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
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
                let matcher = matchers.get(&tag).copied().unwrap_or(WindowMatch::Instance);
                out.as_object_mut().map(|o| o.remove("tag"));
                out["swallows"] = json!([{
                    matcher.key(): format!("^{}$", launch::handle_for(&tag))
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

/// Every agent tag the snapshots on disk record — the live one and a previous
/// boot's still awaiting restore: the names a pending restore will relaunch
/// under, which a newly minted tag must not take.
pub(crate) fn captured_tags() -> HashSet<String> {
    let dir = layout_dir();
    [LAYOUT, PREV_BOOT]
        .iter()
        .filter_map(|name| read_layout(&dir.join(name)))
        .flat_map(|snapshot| {
            snapshot["workspaces"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|ws| ws["agents"].as_array())
                .flatten()
                .filter_map(|a| a["tag"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// True when the snapshot on disk has gone stale.
fn capture_due() -> bool {
    !std::fs::metadata(layout_dir().join(LAYOUT))
        .and_then(|m| m.modified())
        .map(|t| {
            t.elapsed()
                .map(|e| e.as_secs() < SNAPSHOT_INTERVAL_SECS)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Offered by every `run`'s monitor thread, every tick, and by `status`.
/// Globally throttled by the snapshot file's own mtime, so exactly one caller
/// pays for a capture per interval and the rest return after a single `stat`.
pub(crate) fn maybe_capture() {
    if capture_due() {
        let _ = capture();
    }
}

/// Same, off the calling thread — for the HUD, whose event loop must not stall
/// for the length of an `i3-msg` round trip.
pub(crate) fn maybe_capture_async() {
    if capture_due() {
        std::thread::spawn(|| {
            let _ = capture();
        });
    }
}

/// Build the snapshot and write it atomically. Returns it for callers that
/// want to report on what was captured.
fn capture() -> Result<Value, String> {
    let sessions = crate::registry::list();
    let tree = i3::tree().ok_or("could not read the i3 tree (is i3-msg on PATH?)")?;
    let (places, by_xid) = index_tree(&tree);

    // Locate every agent on screen.
    //
    // Two sessions can name the same window — an agent launched while its own
    // terminal was still mapping records whatever was focused instead, and
    // `--role` only covers windows seagents launched. At most one of them can
    // be right and nothing here can say which, so the earliest launch keeps
    // the window (the later one is the one that read stale focus) and the rest
    // are reported unplaced.
    let mut located: HashMap<String, u64> = HashMap::new();
    let mut owner: HashMap<u64, (u64, String)> = HashMap::new(); // con_id -> (started, key)
    for s in &sessions {
        // Recorded con_id first; the X window id survives an i3 restart and
        // heals it, which is the same order `focus` resolves in.
        let Some(con_id) = s
            .meta
            .con_id
            .filter(|id| places.contains_key(id))
            .or_else(|| s.meta.window_id.and_then(|x| by_xid.get(&x).copied()))
        else {
            continue;
        };
        match owner.get(&con_id) {
            Some((started, _)) if *started <= s.meta.started => continue,
            Some((_, prev)) => {
                located.remove(prev);
            }
            None => {}
        }
        owner.insert(con_id, (s.meta.started, s.meta.key.clone()));
        located.insert(s.meta.key.clone(), con_id);
    }
    // Which conversation each agent is running, as its own SessionStart hook
    // reported it. None for agents without hooks: those restore fresh,
    // without `--resume`.
    let session_ids: HashMap<&str, Option<String>> = sessions
        .iter()
        .map(|s| {
            (
                s.meta.key.as_str(),
                crate::registry::read_agent(&s.files).session_id,
            )
        })
        .collect();

    // con_id → tag, for the agents we can actually locate on screen.
    let mut tags: HashMap<u64, String> = HashMap::new();
    let mut placed: HashMap<String, Vec<&Session>> = HashMap::new();
    let mut outputs: HashMap<String, String> = HashMap::new();
    let mut unplaced: Vec<Value> = Vec::new();

    for s in &sessions {
        let Some(con_id) = located.get(&s.meta.key).copied() else {
            unplaced.push(json!({ "tag": s.meta.tag, "dir": s.meta.dir() }));
            continue;
        };
        let place = &places[&con_id];
        tags.insert(con_id, s.meta.tag.clone());
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
                    "tag": s.meta.tag,
                    "cwd": s.meta.cwd,
                    "dir": s.meta.dir(),
                    "argv": s.meta.argv,
                    "session_id": session_ids[s.meta.key.as_str()],
                    "terminal_class": located.get(&s.meta.key).map(|c| places[c].class.clone()),
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
        "captured": crate::clock::unix_now(),
        "focused_workspace": focused_workspace(&tree),
        "workspaces": workspaces,
        "unplaced": unplaced,
    });

    write_layout(&layout_dir(), &snapshot, current_boot())?;
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

/// `seagents snapshot [--print]` — force a capture now.
pub fn snapshot_cmd(print_json: bool) -> i32 {
    match capture() {
        Ok(snapshot) => {
            let dir = layout_dir();
            // The capture just made is not what the next `restore` reads while
            // a previous boot's layout is pending; say so (on stderr under
            // `--print`, which keeps stdout the JSON alone).
            let (source, prev_boot) = restore_source(&dir);
            let pending = prev_boot.then(|| {
                format!(
                    "note: the previous boot's layout is still unrestored at {} — \
                     `restore` will use that, not this one",
                    source.display()
                )
            });
            if print_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&snapshot).unwrap_or_default()
                );
                if let Some(note) = pending {
                    eprintln!("{note}");
                }
                return 0;
            }
            summarize(&snapshot);
            println!("\nsaved to {}", dir.join(LAYOUT).display());
            if let Some(note) = pending {
                println!("{note}");
            }
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
            "\n{} agent{} not captured (window never identified):",
            unplaced.len(),
            if unplaced.len() == 1 { "" } else { "s" }
        );
        for a in unplaced {
            println!("  {}", a["dir"].as_str().unwrap_or("?"));
        }
    }
}

/// `seagents restore [--dry-run] [--stagger=N] [--workspace=NAME]`
pub fn restore_cmd(dry_run: bool, stagger: u64, only: Option<String>) -> i32 {
    restore_in(&layout_dir(), dry_run, stagger, only)
}

fn restore_in(dir: &Path, dry_run: bool, stagger: u64, only: Option<String>) -> i32 {
    let (path, prev_boot) = restore_source(dir);
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
    if prev_boot {
        println!("from the previous boot's layout ({})", path.display());
    }

    // Never restore something that is already on screen: `restore` has to be
    // safe to run twice, and safe to run when only half the agents died. Every
    // agent has a tag and a restored one comes back under its captured tag, so
    // a captured tag that is live *is* that agent, already running.
    let live: HashSet<String> = crate::registry::list()
        .into_iter()
        .map(|s| s.meta.tag)
        .collect();
    // An agent herdr runs under one of the captured tags without a seagents
    // window — herdr resumed it itself after its server restarted (its own
    // Codex resume, or its Claude integration) — holds the name, and `run
    // --tag` would be refused after its window had opened. Left to herdr,
    // and said so.
    let in_herdr: HashSet<String> = crate::herdr::agents()
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.name)
        .collect();

    let empty = Vec::new();
    let mut skipped = 0usize;
    let mut plan: Vec<(String, &Value, Vec<&Value>)> = Vec::new();
    for ws in snapshot["workspaces"].as_array().unwrap_or(&empty) {
        let name = ws["name"].as_str().unwrap_or_default().to_string();
        if only.as_ref().is_some_and(|only| *only != name) {
            continue;
        }
        let mut todo: Vec<&Value> = Vec::new();
        for a in ws["agents"].as_array().unwrap_or(&empty) {
            let tag = a["tag"].as_str().unwrap_or_default();
            if live.contains(tag) {
                skipped += 1;
            } else if in_herdr.contains(tag) {
                eprintln!(
                    "seagents: {}: herdr already runs an agent named {tag} (resumed by herdr itself?); \
                     not restored — find it in herdr, or stop it there and restore again",
                    a["dir"].as_str().unwrap_or("?")
                );
            } else {
                todo.push(a);
            }
        }
        if !todo.is_empty() {
            plan.push((name, ws, todo));
        }
    }
    let mut launched = 0usize;
    for (name, ws, todo) in &plan {
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
                    launch::matcher_for(a["terminal_class"].as_str().unwrap_or_default()),
                ))
            })
            .collect();

        println!(
            "workspace {name}: {} window{}",
            todo.len(),
            if todo.len() == 1 { "" } else { "s" }
        );
        for a in todo {
            let cmd = resume_argv(a);
            println!(
                "  {:<24}  {}",
                a["dir"].as_str().unwrap_or("?"),
                cmd.join(" ")
            );
        }
        if dry_run {
            launched += todo.len();
            continue;
        }

        if let Err(e) = place_workspace(
            name,
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
            let argv = resume_argv(a);
            let run = launch::Run {
                tag,
                seed_file: None,
                argv: &argv,
                keep_shell: true,
            };
            if let Err(e) =
                launch::spawn_terminal(&run, cwd, a["terminal_class"].as_str().unwrap_or_default())
            {
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
        // Used up once a whole restore has run from it, failed launches
        // included — otherwise it would shadow the live layout for good, and a
        // later same-boot crash would bring back the old boot instead. A
        // `--workspace` restore leaves it pending for the rest.
        if prev_boot && only.is_none() {
            if let Err(e) = consume_prev_boot(dir) {
                eprintln!("seagents: {e}");
            }
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
/// stale conversation selection dropped and the captured conversation
/// resumed, both per the agent's adapter (`[restore]`: Claude's
/// `--resume <id>`, Codex's `resume <id>` subcommand). Model flags and
/// anything else the user launched with are preserved. The resume goes
/// right after the command: before any `--` (after it everything is the
/// agent's positional text) and in a subcommand's place. An agent whose
/// adapter can't resume — or with no captured id — comes back on a fresh
/// conversation.
fn resume_argv(agent: &Value) -> Vec<String> {
    let argv: Vec<String> = serde_json::from_value(agent["argv"].clone()).unwrap_or_default();
    let Some(cmd) = argv.first() else {
        return argv;
    };
    let restore = crate::adapter::for_command(cmd).unwrap_or_default().restore;
    let mut out = argv_without_resume(&argv, &restore.drop);
    if let Some(id) = agent["session_id"]
        .as_str()
        .filter(|_| !restore.resume.is_empty())
    {
        let resume = restore.resume.iter().map(|a| a.replace("{session}", id));
        out.splice(1..1, resume);
    }
    out
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// An argv with its session-selection words (`drop`, each with the id or
/// `--last` after it) removed, so a restored window can be pointed at the
/// conversation we captured instead of whichever one the agent was
/// originally launched on.
fn argv_without_resume(argv: &[String], drop: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut it = argv.iter().peekable();
    while let Some(arg) = it.next() {
        if arg == "--" {
            // Positional text from here on: a `-c` in it is words, not a flag.
            out.push(arg.clone());
            out.extend(it.cloned());
            break;
        }
        if drop.contains(arg) {
            // Drop the word, and its value when it carries one.
            if it.peek().is_some_and(|v| is_uuid(v) || *v == "--last") {
                it.next();
            }
            continue;
        }
        out.push(arg.clone());
    }
    out
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
