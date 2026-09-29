//! The in-PTY job-control anchor (`seagents __lead <cmd...>`) that `run` wraps
//! around the agent.
//!
//! The PTY session leader must not BE the agent: a lone process group in its
//! own session is "orphaned" and the kernel silently discards the SIGTSTP it
//! raises on Ctrl+Z. The anchor forks the agent into a second process group
//! (de-orphaning it), then relays stops/exits upward.

/// The job-control anchor (`seagents __lead <cmd...>`), running as the inner
/// PTY's session leader. It forks the agent into its own foreground process
/// group — giving the agent a same-session parent so its Ctrl+Z SIGTSTP is
/// *not* discarded under the orphaned-process-group rule — then mirrors the
/// agent's lifecycle: agent stops → anchor stops itself (SIGSTOP, which cannot
/// be discarded, so the wrapper's WUNTRACED wait sees it); anchor is resumed →
/// it resumes the agent's group and nudges a repaint with SIGWINCH.
pub(crate) fn lead(cmd: Vec<String>) -> i32 {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    unsafe {
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            // Becoming the foreground group from a background one raises
            // SIGTTOU; ignore it around the handoff.
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::tcsetpgrp(0, libc::getpid());
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            Ok(())
        });
    }
    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("seagents: failed to spawn {}: {e}", cmd[0]);
            return 127;
        }
    };
    let pid = child.id() as libc::pid_t;
    unsafe {
        // Also from the parent side, so the group exists before we wait on it
        // regardless of who wins the fork/exec race.
        libc::setpgid(pid, pid);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::tcsetpgrp(0, pid);
    }

    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return 1;
        }
        if libc::WIFSTOPPED(status) {
            unsafe {
                libc::raise(libc::SIGSTOP);
                // — resumed by the wrapper (the user ran `fg`) —
                libc::kill(-pid, libc::SIGCONT);
                libc::kill(-pid, libc::SIGWINCH);
            }
            continue;
        }
        if libc::WIFEXITED(status) {
            return libc::WEXITSTATUS(status);
        }
        if libc::WIFSIGNALED(status) {
            return 128 + libc::WTERMSIG(status);
        }
    }
}
