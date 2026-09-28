//! Every conversation seagents has with i3, in one place: reading the tree,
//! listing its windows, focusing one, and sending commands. Nothing else in
//! the crate runs `i3-msg`; what callers *do* with the tree (identity capture,
//! layout snapshots) stays with them.

use std::process::Command;

use serde_json::Value;

/// The whole i3 tree, via `i3-msg -t get_tree`. None when i3-msg is missing or
/// fails (no i3 running, not on X).
pub(crate) fn tree() -> Option<Value> {
    let out = Command::new("i3-msg")
        .args(["-t", "get_tree"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// The currently focused node of the tree.
pub(crate) fn focused_node() -> Option<Value> {
    find_focused(&tree()?).cloned()
}

fn find_focused(node: &Value) -> Option<&Value> {
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

pub(crate) struct Win {
    pub(crate) con_id: u64,
    /// The X11 window id (i3's "window" field) — stable across i3 restarts.
    pub(crate) xid: Option<u64>,
}

/// Read every leaf window from the i3 tree via `i3-msg -t get_tree`.
pub(crate) fn windows() -> Option<Vec<Win>> {
    let mut windows = Vec::new();
    collect(&tree()?, &mut windows);
    Some(windows)
}

fn collect(node: &Value, out: &mut Vec<Win>) {
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
///
/// Stricter than [`msg`] on purpose: success has to be *affirmed*, not merely
/// not denied, because a false "focused" here stops the caller falling through
/// from a dead con_id to the X window id.
pub(crate) fn focus(con_id: u64) -> bool {
    Command::new("i3-msg")
        .arg(format!("[con_id={con_id}] focus"))
        .output()
        .map(|o| o.status.success() && affirms_success(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(false)
}

/// Run an i3 command, failing only when i3 reports that it failed.
pub(crate) fn msg(command: &str) -> Result<(), String> {
    let out = Command::new("i3-msg")
        .arg(command)
        .output()
        .map_err(|e| format!("i3-msg: {e}"))?;
    let body = String::from_utf8_lossy(&out.stdout);
    if out.status.success()
        && !body.contains("\"success\": false")
        && !body.contains("\"success\":false")
    {
        return Ok(());
    }
    Err(format!("i3-msg {command} failed: {}", body.trim()))
}

/// Whether an i3 reply says `"success": true`, spaced or not.
fn affirms_success(body: &str) -> bool {
    body.contains("\"success\":true") || body.contains("\"success\": true")
}

/// Names of the outputs (monitors) that are currently active.
pub(crate) fn active_outputs() -> Vec<String> {
    let Ok(out) = Command::new("i3-msg").args(["-t", "get_outputs"]).output() else {
        return Vec::new();
    };
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.iter()
                .filter(|o| o["active"].as_bool() == Some(true))
                .filter_map(|o| o["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// i3's command parser takes double-quoted strings; workspace and output names
/// can contain spaces.
pub(crate) fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_must_be_affirmed_in_either_spacing() {
        assert!(affirms_success(r#"[{"success":true}]"#));
        assert!(affirms_success(r#"[ { "success": true } ]"#));
        assert!(!affirms_success(
            r#"[{"success":false,"error":"No such con"}]"#
        ));
        assert!(!affirms_success("[]"));
    }

    #[test]
    fn collects_tiled_and_floating_windows_but_not_containers() {
        let tree = serde_json::json!({
            "id": 1, "nodes": [
                {"id": 2, "window": 20, "nodes": []},
                {"id": 3, "nodes": [{"id": 4, "window": 40}]}
            ],
            "floating_nodes": [{"id": 5, "window": 50}]
        });
        let mut wins = Vec::new();
        collect(&tree, &mut wins);
        let ids: Vec<(u64, Option<u64>)> = wins.iter().map(|w| (w.con_id, w.xid)).collect();
        assert_eq!(ids, vec![(2, Some(20)), (4, Some(40)), (5, Some(50))]);
    }

    #[test]
    fn quoting_escapes_backslashes_and_quotes() {
        assert_eq!(quote(r#"1: "work" \ ws"#), r#""1: \"work\" \\ ws""#);
    }
}
