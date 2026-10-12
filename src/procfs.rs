//! Generic process facts read out of `/proc`: liveness, parentage, and when
//! the machine booted.

/// Is `pid` a running process? What the registry's liveness rule is made
/// of: a process with no `/proc` entry has ended.
pub(crate) fn alive(pid: u64) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// `pid`'s parent, from `/proc/<pid>/stat`.
pub(crate) fn parent_pid(pid: u64) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can hold spaces and parens; ppid is the 2nd field after the last ')'.
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// `pid`'s whole ancestry, nearest ancestor first, up to (not including)
/// init — or None when any link of it cannot be read: `pid` or an ancestor
/// is gone (a parent that exits mid-walk), belongs to a pid namespace we
/// can't see into, or the chain is implausibly deep. For finding the window
/// that hosts a process (`window::window_ids_by_pid`).
pub(crate) fn ancestors(mut pid: u64) -> Option<Vec<u64>> {
    let mut out = Vec::new();
    // Bounded: a /proc loop would otherwise hang the caller. Real chains are
    // a dozen deep.
    for _ in 0..1024 {
        // 1 is init (or a namespace's), 0 a kernel thread's or a process
        // whose parent is outside our pid namespace: the top either way.
        match parent_pid(pid)? {
            0 | 1 => return Some(out),
            parent => {
                out.push(parent);
                pid = parent;
            }
        }
    }
    None
}

/// When this boot began, in Unix seconds: the `btime` line of `/proc/stat`.
/// Anything stamped earlier than this was written by a previous boot.
///
/// The kernel derives it as wall clock minus uptime at read time, so a clock
/// step (NTP correcting a wrong RTC) moves it by the size of the step.
pub(crate) fn boot_time() -> Option<u64> {
    parse_btime(&std::fs::read_to_string("/proc/stat").ok()?)
}

fn parse_btime(stat: &str) -> Option<u64> {
    stat.lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}
