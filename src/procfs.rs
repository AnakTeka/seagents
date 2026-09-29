//! Generic process facts read out of `/proc`: liveness, parentage, the size
//! of a process's terminal, and when the machine booted.

/// Is `pid` a running process? The one liveness rule every caller shares: a
/// session whose child pid has no `/proc` entry has ended.
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

/// `pid`'s ancestry, nearest ancestor first, stopping at init.
pub(crate) fn ancestors(mut pid: u64) -> Vec<u64> {
    let mut out = Vec::new();
    // Bounded: a /proc loop would otherwise hang the launch path.
    for _ in 0..32 {
        let Some(parent) = parent_pid(pid) else { break };
        if parent <= 1 {
            break;
        }
        out.push(parent);
        pid = parent;
    }
    out
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

/// Read the child's live terminal size via TIOCGWINSZ on its pty (fd 0), found
/// through /proc. Returns (rows, cols). Read-only; `O_NOCTTY` avoids acquiring
/// the pty as our controlling terminal.
pub(crate) fn live_pty_size(child_pid: u64) -> Option<(u16, u16)> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY)
        .open(format!("/proc/{child_pid}/fd/0"))
        .ok()?;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_process_is_alive_and_has_a_parent() {
        let me = std::process::id() as u64;
        assert!(alive(me));
        assert!(!alive(u32::MAX as u64));
        let parent = parent_pid(me).expect("our own stat is readable");
        assert_eq!(
            ancestors(me).first().copied(),
            Some(parent).filter(|p| *p > 1)
        );
    }

    #[test]
    fn boot_time_is_the_btime_line() {
        let stat = "cpu  1 2 3\nintr 5 6\nctxt 99\nbtime 1759132800\nprocesses 7\n";
        assert_eq!(parse_btime(stat), Some(1759132800));
        assert_eq!(parse_btime("cpu 1 2 3\n"), None);
        // This boot began before now, and after 2020.
        let now = crate::clock::unix_now();
        assert!((1_577_836_800..=now).contains(&boot_time().expect("/proc/stat has btime")));
    }
}
