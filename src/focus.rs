//! Milestone 3: focus the i3 window running a given agent.
//!
//! X11 offers no query for "the window hosting this pty" (Terminator shares
//! one process across all its windows, so `_NET_WM_PID` is useless). The only
//! honest strategy is *correlation*: whenever we know the user is interacting
//! with the agent — at `seagents run` launch, and on every input burst
//! thereafter — the focused terminal window IS the agent's window, and we
//! record its identity (i3 con_id + X window id) in the session meta. Focus
//! then uses exact handles only: con_id first (fast, dies on i3 restart),
//! X window id second (survives restarts), healing the con_id when used.

use std::path::PathBuf;
use std::process::Command;

/// `seagents focus <child_pid>` — bring the agent's terminal window to the
/// front. CLI wrapper: prints any error and maps to an exit code.
pub fn focus(child_pid: u64) -> i32 {
    match focus_window(child_pid) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("seagents: {e}");
            1
        }
    }
}

/// Locate and focus the agent's i3 window, returning a human-readable error
/// instead of printing — so callers like the HUD stay in control of the screen.
///
/// Exact handles only: recorded con_id → recorded X window id. con_ids die on
/// an i3 *restart* (the tree is rebuilt); X window ids survive it. When the
/// X-id path is used, the meta's con_id is healed so the fast path works
/// again. There is deliberately no fuzzy fallback: window identity is captured
/// at launch and re-captured on every input burst (typing proves the agent's
/// terminal is focused), so a session the user has ever typed into has
/// accurate handles.
pub(crate) fn focus_window(child_pid: u64) -> Result<(), String> {
    let (meta, meta_path) = session_for_child(child_pid)
        .ok_or_else(|| format!("no tracked session with pid {child_pid}"))?;

    if let Some(con_id) = meta["con_id"].as_u64() {
        if i3_focus(con_id) {
            return Ok(());
        }
        // Recorded con_id is gone — likely an i3 restart. Fall through.
    }

    if let Some(xid) = meta["window_id"].as_u64() {
        let windows =
            i3_windows().ok_or("could not read the i3 tree (is i3-msg on PATH?)")?;
        if let Some(win) = windows.iter().find(|w| w.xid == Some(xid)) {
            if i3_focus(win.con_id) {
                record_window(&meta_path, win.con_id, Some(xid));
                return Ok(());
            }
        }
    }

    Err(format!(
        "session {child_pid}: window not identified (focus was elsewhere at launch, \
         or the window is gone); type into the agent once and retry"
    ))
}

/// Persist the agent's window identity into its meta — called at launch, from
/// the input pump whenever the user types (focused terminal = agent's window),
/// and from the X-id focus path to heal a post-restart con_id. Skips the write
/// when nothing changed, so the once-a-minute input capture is usually free.
pub(crate) fn record_window(meta_path: &std::path::Path, con_id: u64, xid: Option<u64>) {
    let Some(mut meta) = std::fs::read_to_string(meta_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return;
    };
    let unchanged = meta["con_id"].as_u64() == Some(con_id)
        && (xid.is_none() || meta["window_id"].as_u64() == xid);
    if unchanged {
        return;
    }
    meta["con_id"] = serde_json::json!(con_id);
    if let Some(xid) = xid {
        meta["window_id"] = serde_json::json!(xid);
    }
    if let Ok(text) = serde_json::to_string_pretty(&meta) {
        let _ = std::fs::write(meta_path, text);
    }
}

/// Reveal the agent's window on its monitor, then hand focus straight back to
/// `return_to` (the HUD) so the user keeps navigating. `i3-msg focus` switches
/// the target output to the agent's workspace; refocusing the HUD leaves that
/// output showing the agent while input stays on the HUD.
pub(crate) fn preview_window(child_pid: u64, return_to: Option<u64>) -> Result<(), String> {
    focus_window(child_pid)?;
    if let Some(id) = return_to {
        i3_focus(id);
    }
    Ok(())
}

/// The con_id of the currently focused window (used to remember the HUD so we
/// can return focus to it after a preview).
pub(crate) fn focused_con_id() -> Option<u64> {
    focused_node().and_then(|n| n["id"].as_u64())
}

/// The focused window's (con_id, X window id), but only if it's a
/// terminal-class window. `seagents run` calls this at launch to record which
/// window an agent lives in; the terminal guard filters out dialogs/browsers
/// that may momentarily hold focus, so we never record the wrong window. The
/// X id is the durable half: con_ids die on an i3 restart, X ids don't.
pub(crate) fn focused_terminal_ids() -> Option<(u64, Option<u64>)> {
    let node = focused_node()?;
    let xid = node.get("window").and_then(|w| w.as_u64());
    let class = node["window_properties"]["class"].as_str().unwrap_or("");
    // Never record the HUD's own window as an agent's: it's terminal-class
    // too, and preview flicker can leave it focused at capture time.
    let role = node["window_properties"]["window_role"]
        .as_str()
        .unwrap_or("");
    if xid.is_some() && looks_like_terminal(class) && role != "seagents-hud" {
        node["id"].as_u64().map(|con_id| (con_id, xid))
    } else {
        None
    }
}

fn focused_node() -> Option<serde_json::Value> {
    let out = Command::new("i3-msg").args(["-t", "get_tree"]).output().ok()?;
    let tree: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    find_focused(&tree).cloned()
}

fn find_focused(node: &serde_json::Value) -> Option<&serde_json::Value> {
    if node["focused"].as_bool() == Some(true) {
        return Some(node);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            for child in children {
                if let Some(found) = find_focused(child) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// Look up a session's meta (and its path) by child pid.
fn session_for_child(child_pid: u64) -> Option<(serde_json::Value, PathBuf)> {
    let dir = crate::cache_dir();
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("meta") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if v["child_pid"].as_u64() == Some(child_pid) {
            return Some((v, path));
        }
    }
    None
}

fn looks_like_terminal(class: &str) -> bool {
    let c = class.to_lowercase();
    c.contains("term") || c.contains("kitty") || c.contains("alacritty") || c.contains("ghostty")
}

struct Win {
    con_id: u64,
    /// The X11 window id (i3's "window" field) — stable across i3 restarts.
    xid: Option<u64>,
}

/// Read every leaf window from the i3 tree via `i3-msg -t get_tree`.
fn i3_windows() -> Option<Vec<Win>> {
    let out = Command::new("i3-msg").args(["-t", "get_tree"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let tree: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut windows = Vec::new();
    collect(&tree, &mut windows);
    Some(windows)
}

fn collect(node: &serde_json::Value, out: &mut Vec<Win>) {
    if node.get("window").and_then(|w| w.as_u64()).is_some() {
        out.push(Win {
            con_id: node["id"].as_u64().unwrap_or(0),
            xid: node["window"].as_u64(),
        });
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            for child in children {
                collect(child, out);
            }
        }
    }
}

/// Focus a window by con_id. Returns true on i3 success.
fn i3_focus(con_id: u64) -> bool {
    Command::new("i3-msg")
        .arg(format!("[con_id={con_id}] focus"))
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains("\"success\":true")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_window_updates_ids_and_skips_when_unchanged() {
        let dir = std::env::temp_dir().join(format!("seagents-record-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1.meta");
        std::fs::write(&path, r#"{"wrapper_pid":1,"con_id":10,"window_id":100}"#).unwrap();

        // Changed con_id: rewritten, xid preserved when not supplied.
        record_window(&path, 20, None);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["con_id"].as_u64(), Some(20));
        assert_eq!(v["window_id"].as_u64(), Some(100));

        // Unchanged: file untouched (mtime-free check via content identity).
        let before = std::fs::read_to_string(&path).unwrap();
        record_window(&path, 20, Some(100));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // New window entirely (agent moved / restarted terminal).
        record_window(&path, 30, Some(300));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["con_id"].as_u64(), Some(30));
        assert_eq!(v["window_id"].as_u64(), Some(300));

        std::fs::remove_dir_all(&dir).ok();
    }
}
