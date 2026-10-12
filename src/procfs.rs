//! Generic process facts read out of `/proc`: liveness, parentage, the size
//! of a process's terminal, and when the machine booted.

/// Is `pid` a running process? The one liveness rule every caller shares: a
/// session whose child pid has no `/proc` entry has ended.
pub(crate) fn alive(pid: u64) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Whether `pid` runs a seagents binary: the file name of its executable,
/// or of its argv[0], starts with `seagents` (a binary replaced by a
/// reinstall reads `seagents (deleted)`). None when neither can be read —
/// gone, another user's, or a kernel thread — so a caller can tell "not
/// ours" from "can't say".
pub(crate) fn runs_seagents(pid: u64) -> Option<bool> {
    let named = |path: &std::path::Path| {
        path.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("seagents"))
    };
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let argv0 = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|c| c.split(|b| *b == 0).next().map(|a| String::from_utf8_lossy(a).into_owned()))
        .filter(|a| !a.is_empty());
    if exe.is_none() && argv0.is_none() {
        return None;
    }
    Some(exe.as_deref().is_some_and(named) || argv0.is_some_and(|a| named(std::path::Path::new(&a))))
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
/// can't see into, or the chain is implausibly deep. Complete or nothing,
/// because "no wrapper among the ancestors" is what makes a caller a human
/// (`registry::enclosing_session`): a walk that stopped at an unreadable
/// link would call anything above it human for lack of data.
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

/// A process pinned by a pidfd: unlike its pid number, which the kernel
/// hands to the next process once this one is reaped, the descriptor goes
/// on naming this process alone. `SO_PEERCRED`'s pid is a number fixed at
/// `connect`; the control socket pins it so that a peer which exits (and
/// whose pid is reused) is noticed rather than mistaken for another process.
#[derive(Debug)]
pub(crate) struct Pinned {
    fd: std::os::fd::OwnedFd,
}

impl Pinned {
    /// Pin `pid` now (`pidfd_open`). Already gone: None. Racy on its own —
    /// if `pid` died and was reused before this call it pins the newcomer —
    /// so prefer [`Pinned::from_fd`] with a pidfd the kernel took earlier.
    pub(crate) fn open(pid: u64) -> Option<Pinned> {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        (fd >= 0).then(|| Pinned {
            fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) },
        })
    }

    /// A pidfd the kernel handed us (`SO_PEERPIDFD`).
    pub(crate) fn from_fd(fd: std::os::fd::OwnedFd) -> Pinned {
        Pinned { fd }
    }

    /// Whether the pinned process still exists (signal 0 through the pidfd:
    /// no signal sent, only the check). A zombie still holds its pid, so it
    /// counts — its `/proc` entry and parentage are still its own.
    pub(crate) fn alive(&self) -> bool {
        use std::os::fd::AsRawFd;
        let r = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd.as_raw_fd(),
                0,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        r == 0
    }
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
