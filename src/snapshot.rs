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
//!    handle ORCHESTRATION-SPEC mints for `spawn`.) *Which* X11 property
//!    carries it is the terminal's business — a role for Terminator, a
//!    WM_CLASS instance for most others — so the stored layout keeps the tag
//!    and the criterion is built at restore time. See [`WindowMatch`].
//!
//! 3. **A window must know which conversation it was.** The agent says so
//!    itself: the `SessionStart` hook `run` injects records the conversation
//!    id in the session's `.agent` (again after every `/clear`), and the
//!    capture copies it into the layout for `restore` to resume.
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

/// One layout write per this many seconds, across all wrappers. Short enough
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

fn layout_dir() -> PathBuf {
    let dir = crate::vendor::herdr_detect::config::state_dir();
    let _ = std::fs::create_dir_all(&dir);
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
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(snapshot).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("writing {}: {e}", path.display()))
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
    captured_tags_in(&layout_dir())
}

fn captured_tags_in(dir: &Path) -> HashSet<String> {
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

/// Offered by every wrapper's monitor thread, every tick, and by `status`.
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
    let mut sessions = crate::registry::list();
    let tree = i3::tree().ok_or("could not read the i3 tree (is i3-msg on PATH?)")?;
    let (places, by_xid) = index_tree(&tree);

    // Locate every agent on screen.
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
            .meta
            .con_id
            .filter(|id| places.contains_key(id))
            .or_else(|| s.meta.window_id.and_then(|x| by_xid.get(&x).copied()))
        else {
            continue;
        };
        match owner.get(&con_id) {
            Some((started, prev)) if *started <= s.meta.started => {
                let _ = prev;
                continue;
            }
            Some((_, prev)) => {
                located.remove(prev);
            }
            None => {}
        }
        owner.insert(con_id, (s.meta.started, s.meta.wrapper_pid));
        located.insert(s.meta.wrapper_pid, con_id);
    }
    // Which conversation each agent is running, as its own SessionStart hook
    // reported it. None for agents without hooks and for wrappers started by
    // a pre-hook binary: those restore fresh, without `--resume`.
    let session_ids: HashMap<u64, Option<String>> = sessions
        .iter()
        .map(|s| (s.meta.wrapper_pid, crate::registry::read_agent(&s.files).session_id))
        .collect();
    // A placeholder needs a name; sessions from a pre-tag wrapper get theirs
    // now, in their `.meta`, so the restore that reads it back knows them.
    crate::registry::backfill_tags(&mut sessions, &captured_tags());

    // con_id → tag, for the agents we can actually locate on screen.
    let mut tags: HashMap<u64, String> = HashMap::new();
    let mut placed: HashMap<String, Vec<&Session>> = HashMap::new();
    let mut outputs: HashMap<String, String> = HashMap::new();
    let mut unplaced: Vec<Value> = Vec::new();

    for s in &sessions {
        // (An untagged session is one whose `.meta` vanished mid-backfill.)
        let Some(con_id) = located
            .get(&s.meta.wrapper_pid)
            .copied()
            .filter(|_| !s.meta.tag.is_empty())
        else {
            unplaced.push(json!({
                "tag": s.meta.tag,
                "cwd": s.meta.cwd,
                "dir": s.meta.dir(),
                "argv": s.meta.argv,
                "session_id": session_ids[&s.meta.wrapper_pid],
            }));
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
                    "parent": s.meta.parent,
                    "depth": s.meta.depth,
                    "cwd": s.meta.cwd,
                    "dir": s.meta.dir(),
                    "argv": s.meta.argv,
                    "session_id": session_ids[&s.meta.wrapper_pid],
                    "terminal_class": located
                        .get(&s.meta.wrapper_pid)
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
        .filter(|t| !t.is_empty())
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
            } else if !crate::registry::valid_tag(tag) {
                // Only a layout captured by a pre-tag binary can hold one: its
                // tags were conversation-id prefixes, and `run` would refuse
                // an all-digits one after the window had already opened.
                eprintln!(
                    "seagents: {}: captured tag {tag:?} is not a valid tag; not restored",
                    a["dir"].as_str().unwrap_or("?")
                );
            } else {
                todo.push(a);
            }
        }
        // Parents before children, so a child's `--parent` can find its parent
        // live. Launch order within a workspace is free: i3 swallows by
        // criteria, not by order.
        todo.sort_by_key(|a| a["depth"].as_u64().unwrap_or(0));
        if !todo.is_empty() {
            plan.push((name, ws, todo));
        }
    }
    let restoring: HashSet<&str> = plan
        .iter()
        .flat_map(|(_, _, todo)| todo.iter().filter_map(|a| a["tag"].as_str()))
        .collect();

    let mut launched = 0usize;
    let mut started: HashSet<&str> = HashSet::new();
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
            let lineage = match a["parent"].as_str() {
                Some(p) if live.contains(p) || restoring.contains(p) => format!("  (under {p})"),
                Some(p) => format!("  ({p} is not running: restored without a parent)"),
                None => String::new(),
            };
            println!("  {:<24}  {}{lineage}", a["dir"].as_str().unwrap_or("?"), cmd.join(" "));
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
            let parent = a["parent"]
                .as_str()
                .filter(|p| parent_is_up(p, &live, &started));
            let argv = resume_argv(a);
            let run = launch::Run {
                tag,
                parent,
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
            started.insert(tag);
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
    let argv = argv_of(agent);
    let restore = crate::adapter::for_command(&argv[0]).unwrap_or_default().restore;
    let mut out = argv_without_resume(&argv, &restore.drop);
    if let Some(id) = agent["session_id"].as_str().filter(|_| !restore.resume.is_empty()) {
        let resume = restore.resume.iter().map(|a| a.replace("{session}", id));
        out.splice(1..1, resume);
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

/// Whether a restored agent can be launched under `parent`: `run --parent`
/// refuses a parent that is not live, so it must be running already or have
/// been launched earlier in this restore — in which case its terminal may
/// still be starting, and it is given a moment to register. Otherwise the
/// lineage is dropped rather than failing the child's launch. (A parent that
/// restores *after* its child, on a later workspace, is the case this loses.)
fn parent_is_up(parent: &str, live: &HashSet<String>, started: &HashSet<&str>) -> bool {
    if live.contains(parent) {
        return true;
    }
    if !started.contains(parent) {
        return false;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if crate::registry::list().iter().any(|s| s.meta.tag == parent) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_session_ids() {
        assert!(is_uuid("5c603939-bf27-47c8-8b0f-13731d2cc6eb"));
        assert!(!is_uuid("5c603939-bf27-47c8-8b0f-13731d2cc6e"));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("5c603939zbf27-47c8-8b0f-13731d2cc6eb"));
    }

    /// `resume_argv` under the lock the adapter tests hold while they point
    /// the config dir at a scratch override.
    fn resume_argv(agent: &Value) -> Vec<String> {
        let _env = crate::vendor::herdr_detect::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        super::resume_argv(agent)
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
                "--resume",
                "0885fa15-47cf-4903-9d97-db59762dc210",
                "--model",
                "claude-opus-4-8",
            ]
        );
    }

    #[test]
    fn resume_argv_drops_a_valueless_resume_and_restores_fresh_without_an_id() {
        let agent = json!({ "argv": ["claude", "--resume"], "session_id": null });
        assert_eq!(resume_argv(&agent), vec!["claude"]);
    }

    #[test]
    fn resume_goes_before_the_separator_and_positional_text_is_left_alone() {
        let id = "5c603939-bf27-47c8-8b0f-13731d2cc6eb";
        let agent = json!({ "argv": ["claude", "-c", "--model", "haiku", "--", "say", "-c", "--resume"],
                            "session_id": id });
        assert_eq!(
            resume_argv(&agent),
            ["claude", "--resume", id, "--model", "haiku", "--", "say", "-c", "--resume"]
        );
    }

    #[test]
    fn codex_resumes_by_its_subcommand_and_keeps_its_config_flags() {
        let id = "01a0ec42-c264-7013-b70f-9acd600b4571";
        // `-c` is Codex's config override, not Claude's --continue.
        let agent = json!({ "argv": ["codex", "-c", "model=\"x\"", "resume", "--last", "-m", "m"], "session_id": id });
        assert_eq!(resume_argv(&agent), ["codex", "resume", id, "-c", "model=\"x\"", "-m", "m"]);
        let forked = json!({ "argv": ["codex", "fork", "5c603939-bf27-47c8-8b0f-13731d2cc6eb"], "session_id": id });
        assert_eq!(resume_argv(&forked), ["codex", "resume", id]);
        // Nothing captured: a fresh conversation, the stale pick still gone.
        let fresh = json!({ "argv": ["codex", "resume", "--last"], "session_id": null });
        assert_eq!(resume_argv(&fresh), ["codex"]);
        // An agent no adapter can resume comes back as it was launched.
        let other = json!({ "argv": ["gemini", "-c", "x"], "session_id": id });
        assert_eq!(resume_argv(&other), ["gemini", "-c", "x"]);
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seagents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A one-agent snapshot as `capture` writes it, stamped `captured`.
    fn layout(captured: u64, tag: &str) -> Value {
        json!({
            "version": 1,
            "captured": captured,
            "focused_workspace": "1",
            "workspaces": [{
                "name": "1",
                "output": "",
                "agents": [{"tag": tag, "cwd": "/tmp", "dir": "tmp", "argv": ["sleep", "1"],
                            "session_id": null, "terminal_class": "Alacritty"}],
                "layout": [],
            }],
            "unplaced": [],
        })
    }

    fn tag_of(path: &Path) -> String {
        read_layout(path).unwrap()["workspaces"][0]["agents"][0]["tag"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn capture_over_a_previous_boots_layout_sets_it_aside() {
        let dir = scratch("prevboot");
        let boot = 1000;
        write_layout(&dir, &layout(900, "old"), 500).unwrap();
        assert!(
            !dir.join(PREV_BOOT).exists(),
            "a same-boot rewrite sets nothing aside"
        );

        // First capture of the new boot: the old boot's layout moves aside.
        write_layout(&dir, &layout(1010, "new"), boot).unwrap();
        assert_eq!(tag_of(&dir.join(PREV_BOOT)), "old");
        assert_eq!(tag_of(&dir.join(LAYOUT)), "new");

        // Later captures in the same boot leave it alone.
        write_layout(&dir, &layout(1040, "newer"), boot).unwrap();
        assert_eq!(tag_of(&dir.join(PREV_BOOT)), "old");
        assert_eq!(tag_of(&dir.join(LAYOUT)), "newer");

        // Both boots' tags stay reserved while the old one awaits restore.
        let tags = captured_tags_in(&dir);
        assert!(tags.contains("old") && tags.contains("newer"));

        // A newer boot's layout replaces an older pending one.
        write_layout(&dir, &layout(2010, "third"), 2000).unwrap();
        assert_eq!(tag_of(&dir.join(PREV_BOOT)), "newer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_previous_boot_layout_is_overwritten_not_set_aside() {
        let dir = scratch("prevboot-empty");
        write_layout(&dir, &layout(900, "old"), 500).unwrap();
        write_layout(&dir, &json!({"captured": 1010, "workspaces": []}), 1000).unwrap();
        // Reboot again before restoring: the brief boot's empty layout must not
        // bury the one that still has agents in it.
        write_layout(&dir, &layout(2010, "new"), 2000).unwrap();
        assert_eq!(tag_of(&dir.join(PREV_BOOT)), "old");
        assert_eq!(tag_of(&dir.join(LAYOUT)), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_prefers_an_unrestored_previous_boot_then_falls_back_to_live() {
        let dir = scratch("restore-source");
        assert_eq!(restore_source(&dir), (dir.join(LAYOUT), false));
        write_layout(&dir, &layout(900, "old"), 500).unwrap();
        assert_eq!(restore_source(&dir), (dir.join(LAYOUT), false));
        write_layout(&dir, &layout(1010, "new"), 1000).unwrap();
        assert_eq!(restore_source(&dir), (dir.join(PREV_BOOT), true));

        consume_prev_boot(&dir).unwrap();
        assert_eq!(restore_source(&dir), (dir.join(LAYOUT), false));
        assert_eq!(tag_of(&dir.join(RESTORED)), "old");
        assert!(!captured_tags_in(&dir).contains("old"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_restore_does_not_consume_the_previous_boot() {
        let dir = scratch("restore-dry");
        // Tags no live agent carries, so the plan is to launch it.
        write_layout(&dir, &layout(900, "sg-unit-old"), 500).unwrap();
        write_layout(&dir, &layout(1010, "sg-unit-new"), 1000).unwrap();
        assert_eq!(restore_in(&dir, true, 0, None), 0);
        assert_eq!(restore_source(&dir), (dir.join(PREV_BOOT), true));
        assert!(!dir.join(RESTORED).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspaces_sort_numerically_then_by_name() {
        let mut names = vec!["10", "2", "scratch", "9"];
        names.sort_by_key(|n| workspace_sort_key(n));
        assert_eq!(names, vec!["2", "9", "10", "scratch"]);
    }
}
