//! Window identity: which i3 window an agent lives in, and focusing it.
//!
//! (Milestone 3.) The `seagents focus` command and the HUD's preview/jump live
//! here, next to the identity-capture strategies they depend on.
//!
//! X11 offers no query for "the window hosting this pty" (Terminator shares
//! one process across all its windows, so `_NET_WM_PID` is useless). So
//! `seagents run` captures it at launch, strongest strategy first (the
//! terminal's own window-id variable, `/proc` ancestry, the handle a window
//! seagents opened carries, and last the focused terminal — which is the
//! agent's when the user just typed the command into it), and records its
//! identity (i3 con_id + X window id) in the session meta. Focus then uses
//! exact handles only: con_id first (fast, dies on i3 restart), X window id
//! second (survives restarts), healing the con_id when used.

use std::process::Command;

use crate::i3;
use crate::launch::WindowMatch;
use crate::registry::Files;

/// `seagents focus <target>` — bring the agent's terminal window to the
/// front. `target` is anything [`crate::registry::resolve_target`] takes. CLI
/// wrapper: prints any error and maps to an exit code.
pub fn focus(target: &str) -> i32 {
    let result = crate::registry::resolve_target(target).and_then(|s| focus_window(&s.meta.key));
    match result {
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
/// at launch, by exact means wherever the terminal allows.
pub(crate) fn focus_window(key: &str) -> Result<(), String> {
    let session = crate::registry::by_key(key).ok_or_else(|| format!("no tracked session {key}"))?;

    if let Some(con_id) = session.meta.con_id {
        if i3::focus(con_id) {
            return Ok(());
        }
        // Recorded con_id is gone — likely an i3 restart. Fall through.
    }

    if let Some(xid) = session.meta.window_id {
        let windows =
            i3::windows().ok_or("could not read the i3 tree (is i3-msg on PATH?)")?;
        if let Some(win) = windows.iter().find(|w| w.xid == Some(xid)) {
            if i3::focus(win.con_id) {
                record_window(&session.files, win.con_id, Some(xid));
                return Ok(());
            }
        }
    }

    Err(format!(
        "{}: window not identified (focus was elsewhere at launch, or the window is gone)",
        session.meta.tag
    ))
}

/// Persist the agent's window identity into its meta — called at launch, and
/// from the X-id focus path to heal a post-restart con_id. Skips the write
/// when nothing changed.
pub(crate) fn record_window(files: &Files, con_id: u64, xid: Option<u64>) {
    crate::registry::update_meta(files, |meta| {
        meta.con_id = Some(con_id);
        if xid.is_some() {
            meta.window_id = xid;
        }
    });
}

/// Reveal the agent's window on its monitor, then hand focus straight back to
/// `return_to` (the HUD) so the user keeps navigating. `i3-msg focus` switches
/// the target output to the agent's workspace; refocusing the HUD leaves that
/// output showing the agent while input stays on the HUD.
pub(crate) fn preview_window(key: &str, return_to: Option<u64>) -> Result<(), String> {
    focus_window(key)?;
    if let Some(id) = return_to {
        i3::focus(id);
    }
    Ok(())
}

/// The con_id of the currently focused window (used to remember the HUD so we
/// can return focus to it after a preview).
pub(crate) fn focused_con_id() -> Option<u64> {
    i3::focused_node().and_then(|n| n["id"].as_u64())
}

/// The focused window's (con_id, X window id), but only if it's a
/// terminal-class window. `seagents run` calls this at launch to record which
/// window an agent lives in; the terminal guard filters out dialogs/browsers
/// that may momentarily hold focus, so we never record the wrong window. The
/// X id is the durable half: con_ids die on an i3 restart, X ids don't.
pub(crate) fn focused_terminal_ids() -> Option<(u64, Option<u64>)> {
    let node = i3::focused_node()?;
    let xid = node.get("window").and_then(|w| w.as_u64());
    let class = node["window_properties"]["class"].as_str().unwrap_or("");
    // Never record the HUD's own window as an agent's: it's terminal-class
    // too, and preview flicker can leave it focused at capture time. Check
    // both properties — the HUD is launched with `--role` under Terminator and
    // with a WM_CLASS instance under terminals that have no roles.
    let is_hud = ["window_role", "instance"]
        .iter()
        .any(|p| node["window_properties"][*p].as_str() == Some("seagents-hud"));
    if xid.is_some() && looks_like_terminal(class) && !is_hud {
        node["id"].as_u64().map(|con_id| (con_id, xid))
    } else {
        None
    }
}

/// Environment variables through which a terminal tells its child which window
/// it is running in. `WINDOWID` is the old xterm convention, still set by a
/// number of terminals; alacritty sets its own.
const WINDOW_ID_VARS: [&str; 2] = ["ALACRITTY_WINDOW_ID", "WINDOWID"];

/// Every variable through which a terminal names the window its child runs
/// in — the ones [`window_ids_from_env`] reads, and Terminator's own. A
/// process that outlives or leaves that window must not carry them on: a
/// herdr server would hand them to every pane, and a terminal seagents opens
/// that sets none of its own (Terminator sets no window id) would hand the
/// caller's to its `run`, which would record the caller's window as its own.
/// Removed from the environment of the process started, never `unset` inside
/// a new terminal (where they are already that terminal's own).
pub(crate) const WINDOW_ENV: [&str; 3] = ["ALACRITTY_WINDOW_ID", "WINDOWID", "TERMINATOR_UUID"];

/// The (con_id, X window id) of our window, straight from the environment the
/// terminal handed us.
///
/// This beats every other strategy when it is available: no focus, no handle,
/// no `/proc` walk, no polling, and — unlike `_NET_WM_PID` — it stays exact on
/// a terminal that runs many windows from one process. Measured on alacritty:
/// the value equals i3's window id for every window, in daemon mode too, where
/// `_NET_WM_PID` is shared and useless. Terminator sets neither variable, which
/// is why the fallbacks below still exist.
///
/// The id is validated against the i3 tree, so a value inherited from somewhere
/// that no longer applies (a detached multiplexer, a window since closed) is
/// declined rather than believed.
pub(crate) fn window_ids_from_env() -> Option<(u64, Option<u64>)> {
    let xid: u64 = WINDOW_ID_VARS
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find_map(|v| v.trim().parse().ok())?;
    i3::windows()?
        .into_iter()
        .find(|w| w.xid == Some(xid))
        .map(|w| (w.con_id, w.xid))
}

/// The (con_id, X window id) of the window hosting **this process**, found by
/// matching our own `/proc` ancestry against windows' `_NET_WM_PID`.
///
/// This is the identity capture with no correlation in it at all: no focus, no
/// minted handle, nothing to race. `seagents run` is a descendant of the
/// terminal that owns the window, so walking up from our own pid and asking X
/// which window each ancestor owns answers the question directly.
///
/// It only works on a terminal that runs **one process per window**. Terminator
/// shares a single process across every window it owns (verified: fourteen
/// windows, all `_NET_WM_PID = 3965`), so the lookup finds many windows for one
/// ancestor and gives up rather than guessing — which is exactly why the
/// handle and focus paths still exist behind it.
///
/// Needs `xdotool`; without it this simply returns None and the caller falls
/// back. Ancestors are tried nearest-first, so a shell between us and the
/// terminal costs one extra lookup rather than a wrong answer.
pub(crate) fn window_ids_by_pid() -> Option<(u64, Option<u64>)> {
    let windows = i3::windows()?;
    let known: std::collections::HashSet<u64> =
        windows.iter().filter_map(|w| w.xid).collect();

    for pid in crate::procfs::ancestors(std::process::id() as u64).unwrap_or_default() {
        let Ok(out) = Command::new("xdotool")
            .args(["search", "--pid", &pid.to_string()])
            .output()
        else {
            return None; // xdotool missing — nothing to fall through to here.
        };
        if !out.status.success() {
            continue;
        }
        let hits: Vec<u64> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u64>().ok())
            // X clients own windows i3 never manages (icon/group leaders);
            // only the ones in the tree are real candidates.
            .filter(|x| known.contains(x))
            .collect();
        match hits.len() {
            0 => continue,
            1 => {
                let xid = hits[0];
                return windows
                    .iter()
                    .find(|w| w.xid == Some(xid))
                    .map(|w| (w.con_id, w.xid));
            }
            // One process, many windows: a shared-process terminal. Ambiguous.
            _ => return None,
        }
    }
    None
}

/// The (con_id, X window id) of the window carrying `value` in any of
/// [`WindowMatch::ALL`] — the handle-based identity capture, for windows seagents
/// launched itself on a terminal where [`window_ids_by_pid`] cannot help.
///
/// All three properties are searched rather than the one this terminal is
/// configured to set, because the handle is a minted unique string: whichever
/// property it turns up in, it identifies the same window, and searching
/// broadly means `run` needs no knowledge of how it was launched.
///
/// A terminal launched a moment ago may not have reached the i3 tree yet (the
/// shell inside it can start before the window is mapped), so this polls
/// briefly rather than answering "not there" on the first look. Returns None if
/// the handle never appears, or if two windows carry it — an ambiguous handle
/// is worse than none.
pub(crate) fn window_ids_for_handle(value: &str) -> Option<(u64, Option<u64>)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mut hits = Vec::new();
        if let Some(tree) = i3::tree() {
            collect_handle(&tree, value, &mut hits);
        }
        hits.sort();
        hits.dedup();
        match hits.len() {
            1 => return Some(hits[0]),
            0 if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            _ => return None,
        }
    }
}

fn collect_handle(node: &serde_json::Value, value: &str, out: &mut Vec<(u64, Option<u64>)>) {
    let carries = WindowMatch::ALL
        .iter()
        .any(|m| node["window_properties"][m.key()].as_str() == Some(value));
    if carries {
        if let Some(con_id) = node["id"].as_u64() {
            out.push((con_id, node["window"].as_u64()));
        }
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            for child in children {
                collect_handle(child, value, out);
            }
        }
    }
}

/// The WM_CLASS of the terminal window `con_id` (else of the focused
/// window), when it is a terminal — what a window seagents opens on the
/// caller's behalf should match: an agent's children open in the terminal the
/// agent runs in, a human's in the one they typed `spawn` into. Empty when
/// neither is a terminal, which leaves the launcher to its default.
pub(crate) fn terminal_class(con_id: Option<u64>) -> String {
    let Some(tree) = i3::tree() else {
        return String::new();
    };
    fn find(node: &serde_json::Value, pred: &dyn Fn(&serde_json::Value) -> bool) -> Option<String> {
        if pred(node) {
            return node["window_properties"]["class"].as_str().map(str::to_string);
        }
        ["nodes", "floating_nodes"]
            .iter()
            .filter_map(|k| node[*k].as_array())
            .flatten()
            .find_map(|c| find(c, pred))
    }
    con_id
        .and_then(|id| find(&tree, &|n| n["id"].as_u64() == Some(id)))
        .or_else(|| find(&tree, &|n| n["focused"].as_bool() == Some(true)))
        .filter(|c| looks_like_terminal(c))
        .unwrap_or_default()
}

fn looks_like_terminal(class: &str) -> bool {
    let c = class.to_lowercase();
    c.contains("term") || c.contains("kitty") || c.contains("alacritty") || c.contains("ghostty")
}
