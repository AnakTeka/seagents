//! Milestone 3: focus the i3 window running a given agent.
//!
//! i3 on X11 doesn't expose a usable per-window PID (Terminator, for one, shares
//! a single process across all its windows), so we can't map by PID. Instead we
//! use the agent's terminal *title*, which the emulator mirrors into the X11
//! window title. We read the agent's current title from its teed stream, find
//! the i3 window whose title matches, and focus it by con_id.

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
pub(crate) fn focus_window(child_pid: u64) -> Result<(), String> {
    let (meta, raw) = session_for_child(child_pid)
        .ok_or_else(|| format!("no tracked session with pid {child_pid}"))?;

    // Preferred: the exact i3 window recorded at launch. Unambiguous even when
    // several agents share an identical title (e.g. idle "✳ Claude Code").
    if let Some(con_id) = meta["con_id"].as_u64() {
        if i3_focus(con_id) {
            return Ok(());
        }
        // Recorded window is gone (or moved handlers) — fall through to title.
    }

    // Fallback for sessions launched before con_id was recorded: match by title.
    let title = crate::status::read_tail(&raw, 256 * 1024)
        .as_deref()
        .and_then(crate::status::current_title)
        .ok_or_else(|| format!("session {child_pid} has no title yet"))?;
    let want = core_title(&title);

    let windows = i3_windows().ok_or("could not read the i3 tree (is i3-msg on PATH?)")?;
    let mut matches: Vec<&Win> = windows
        .iter()
        .filter(|w| core_title(&w.title) == want)
        .collect();
    matches.sort_by_key(|w| !looks_like_terminal(&w.class));

    let target = matches
        .first()
        .ok_or_else(|| format!("no i3 window titled {title:?}"))?;

    if i3_focus(target.con_id) {
        Ok(())
    } else {
        Err(format!("i3-msg focus failed for con_id {}", target.con_id))
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

/// The focused window's con_id, but only if it's a terminal-class window.
/// `seagents run` calls this at launch to record which window an agent lives in;
/// the terminal guard filters out dialogs/browsers that may momentarily hold
/// focus, so we never record the wrong window.
pub(crate) fn focused_terminal_con_id() -> Option<u64> {
    let node = focused_node()?;
    let is_window = node.get("window").and_then(|w| w.as_u64()).is_some();
    let class = node["window_properties"]["class"].as_str().unwrap_or("");
    (is_window && looks_like_terminal(class)).then(|| node["id"].as_u64())?
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

/// Look up a session's meta and `.raw` path by child pid.
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
            let wrapper_pid = v["wrapper_pid"].as_u64()?;
            return Some((v, dir.join(format!("{wrapper_pid}.raw"))));
        }
    }
    None
}

/// Strip a leading state glyph (braille spinner, ✳, ✻, …) plus its space so a
/// changing spinner doesn't defeat the title match.
fn core_title(title: &str) -> String {
    let t = title.trim();
    let mut chars = t.chars();
    if let (Some(a), Some(b)) = (chars.next(), chars.next()) {
        let cp = a as u32;
        let glyph = (0x2700..=0x27BF).contains(&cp)   // dingbats: ✳ ✻ ✽
            || (0x2800..=0x28FF).contains(&cp)          // braille spinner
            || (!a.is_ascii() && !a.is_alphanumeric()); // other leading symbol
        if glyph && b == ' ' {
            return t.chars().skip(2).collect::<String>().trim().to_string();
        }
    }
    t.to_string()
}

fn looks_like_terminal(class: &str) -> bool {
    let c = class.to_lowercase();
    c.contains("term") || c.contains("kitty") || c.contains("alacritty") || c.contains("ghostty")
}

struct Win {
    con_id: u64,
    title: String,
    class: String,
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
        let props = &node["window_properties"];
        out.push(Win {
            con_id: node["id"].as_u64().unwrap_or(0),
            title: props["title"].as_str().unwrap_or("").to_string(),
            class: props["class"].as_str().unwrap_or("").to_string(),
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
    fn strips_state_glyphs() {
        assert_eq!(core_title("✳ Understand folder purpose"), "Understand folder purpose");
        assert_eq!(core_title("⠂ Build custom agent monitor"), "Build custom agent monitor");
        assert_eq!(core_title("plain shell title"), "plain shell title");
    }
}
