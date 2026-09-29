//! Launching a terminal window seagents can recognize afterwards.
//!
//! A window seagents opens itself — a restored or a spawned agent — is
//! launched carrying a minted handle,
//! `seagents-<tag>` ([`handle_for`]), so i3 can swallow it into a placeholder
//! and `seagents run --tag` can identify it without a focus race. *Which*
//! X11 property carries the handle is the terminal's business — a role for
//! Terminator, a WM_CLASS instance for most others — so each launch recipe
//! ([`terminal`]) pairs the command line with the [`WindowMatch`] that finds
//! the window it opens.

use std::path::Path;
use std::process::Command;

/// The window handle a terminal seagents launches is given, and the value a
/// restore placeholder swallows on. Unique per agent, so placeholders can never be
/// filled by the wrong window and launch order does not matter (verified: i3
/// matches by criteria, so launching out of order still fills the right slots).
pub(crate) fn handle_for(tag: &str) -> String {
    format!("seagents-{tag}")
}

/// Which X11 property carries that handle. Terminator has no `--class` but does
/// have `--role`; most other terminals are the other way round, so the property
/// travels with the terminal rather than being baked into the stored snapshot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WindowMatch {
    Role,
    Instance,
    Class,
}

impl WindowMatch {
    /// Every property a terminal might use to carry a handle we minted.
    /// Terminator has `--role` and no `--class`; most others are the reverse.
    pub(crate) const ALL: [WindowMatch; 3] =
        [WindowMatch::Role, WindowMatch::Instance, WindowMatch::Class];

    /// The key i3 uses for this property, in both swallow criteria and
    /// `window_properties`.
    pub(crate) fn key(self) -> &'static str {
        match self {
            WindowMatch::Role => "window_role",
            WindowMatch::Instance => "instance",
            WindowMatch::Class => "class",
        }
    }

    fn parse(s: &str) -> Option<WindowMatch> {
        match s {
            "role" | "window_role" => Some(WindowMatch::Role),
            "instance" => Some(WindowMatch::Instance),
            "class" => Some(WindowMatch::Class),
            _ => None,
        }
    }
}

/// How to launch a window seagents can recognize afterwards.
struct Terminal {
    argv: Vec<String>,
    matcher: WindowMatch,
}

/// Resolve the terminal to launch a window with.
///
/// Built-ins are the ones actually verified on this machine. Anything else goes
/// through `SEAGENTS_TERMINAL_CMD` (a template over `{handle}`, `{cwd}`,
/// `{cmd}`) paired with `SEAGENTS_TERMINAL_MATCH`, because a launch flag and
/// the criterion that finds the resulting window are one decision — set the
/// command without saying how to match it and every placeholder silently stays
/// empty.
fn terminal(handle: &str, cwd: &str, inner: &str, class: &str) -> Result<Terminal, String> {
    if let Ok(template) = std::env::var("SEAGENTS_TERMINAL_CMD") {
        let matcher = match std::env::var("SEAGENTS_TERMINAL_MATCH") {
            Ok(m) => WindowMatch::parse(&m).ok_or_else(|| {
                format!("SEAGENTS_TERMINAL_MATCH={m} is not one of role|instance|class")
            })?,
            // Most terminals set WM_CLASS and have no notion of a role.
            Err(_) => WindowMatch::Instance,
        };
        let argv: Vec<String> = template
            .split_whitespace()
            .map(|w| {
                w.replace("{handle}", handle)
                    .replace("{role}", handle)
                    .replace("{cwd}", cwd)
                    .replace("{cmd}", inner)
            })
            .collect();
        if argv.is_empty() {
            return Err("SEAGENTS_TERMINAL_CMD is empty".to_string());
        }
        return Ok(Terminal { argv, matcher });
    }

    // Explicit env wins; otherwise the terminal the agent was actually running
    // in when captured, so a restore reproduces what was there — which is what
    // makes a half-migrated setup (some agents in one terminal, some in
    // another) come back correctly.
    let name = std::env::var("SEAGENTS_TERMINAL")
        .ok()
        .or_else(|| recipe_for_class(class))
        .unwrap_or_else(|| "terminator".to_string());
    match name.as_str() {
        "terminator" => Ok(Terminal {
            argv: vec![
                "terminator".into(),
                // Its own process: a Terminator that delegates over DBus to a
                // running instance does not reliably apply --role, and the
                // handle is the entire swallow criterion.
                "-u".into(),
                "--role".into(),
                handle.into(),
                "--working-directory".into(),
                cwd.into(),
                // -x takes the rest of the line as argv, and must come last.
                "-x".into(),
                "/bin/sh".into(),
                "-c".into(),
                inner.into(),
            ],
            matcher: WindowMatch::Role,
        }),
        "alacritty" => Ok(Terminal {
            argv: vec![
                "alacritty".into(),
                // <general>,<instance>: keep the general class so existing i3
                // rules for Alacritty still apply.
                "--class".into(),
                format!("Alacritty,{handle}"),
                "--working-directory".into(),
                cwd.into(),
                "-e".into(),
                "/bin/sh".into(),
                "-c".into(),
                inner.into(),
            ],
            matcher: WindowMatch::Instance,
        }),
        // One process hosting every window. Cheaper per window, and no longer
        // costs anything in identity: a window created this way still tells its
        // child `ALACRITTY_WINDOW_ID`, so `run` identifies itself exactly even
        // though `_NET_WM_PID` is shared. Needs a daemon already running
        // (`alacritty --daemon`), otherwise `msg` exits with "no socket found".
        "alacritty-daemon" => Ok(Terminal {
            argv: vec![
                "alacritty".into(),
                "msg".into(),
                "create-window".into(),
                "--class".into(),
                format!("Alacritty,{handle}"),
                "--working-directory".into(),
                cwd.into(),
                "-e".into(),
                "/bin/sh".into(),
                "-c".into(),
                inner.into(),
            ],
            matcher: WindowMatch::Instance,
        }),
        other => Err(format!(
            "SEAGENTS_TERMINAL={other} has no built-in launch recipe; set \
             SEAGENTS_TERMINAL_CMD (with {{handle}}, {{cwd}}, {{cmd}}) and \
             SEAGENTS_TERMINAL_MATCH=role|instance|class"
        )),
    }
}

/// The built-in recipe matching a captured window's WM_CLASS, if there is one.
fn recipe_for_class(class: &str) -> Option<String> {
    match class.to_ascii_lowercase().as_str() {
        "terminator" => Some("terminator".to_string()),
        "alacritty" => Some("alacritty".to_string()),
        _ => None,
    }
}

/// The swallow criterion for one captured agent — i.e. which property the
/// terminal that will relaunch it is going to set.
pub(crate) fn matcher_for(class: &str) -> WindowMatch {
    terminal("", "", "", class)
        .map(|t| t.matcher)
        .unwrap_or(WindowMatch::Instance)
}

/// What a window seagents opens runs: `argv` under `seagents run --tag <tag>`
/// (with `--parent` and `--seed-file` when given).
pub(crate) struct Run<'a> {
    pub(crate) tag: &'a str,
    pub(crate) parent: Option<&'a str>,
    /// A prompt file `run` delivers to the agent and deletes (`spawn`).
    pub(crate) seed_file: Option<&'a Path>,
    pub(crate) argv: &'a [String],
    /// Leave the window at a shell when the agent exits — `restore`, which
    /// reproduces windows you opened by hand, where closing the agent drops
    /// you back to your prompt. `spawn` doesn't: a fan-out would otherwise
    /// leave a dead shell per child.
    pub(crate) keep_shell: bool,
}

/// Launch one terminal window running `run`, carrying the handle minted from
/// its tag. The one launcher `restore` and `spawn` share.
///
/// The agent runs under `seagents run` so the session is teed and tracked
/// from its first byte.
///
/// `SEAGENTS_TERMINAL_CMD` overrides the whole invocation for terminals without
/// a built-in recipe; `{handle}` (alias `{role}`), `{cwd}` and `{cmd}` are
/// substituted.
///
/// The window starts as a user's shell would, not as a child of whoever
/// calls this: an agent running `spawn` carries its own session's variables
/// (the adapters' `session_env` — Claude's session id, messaging socket and
/// token, …), and a Claude that inherits them never writes its transcript
/// outside tmux. They are dropped twice over — from the terminal's own
/// environment, and by an `unset` at the head of the line it runs, which
/// covers a window hosted by a terminal daemon started with them.
pub(crate) fn spawn_terminal(run: &Run, cwd: &str, class: &str) -> Result<(), String> {
    let handle = handle_for(run.tag);
    // This very binary, by absolute path: the new window's shell has whatever
    // PATH its rc files give it, and the one it finds need not be us.
    let exe = std::env::current_exe().map_err(|e| format!("locating seagents: {e}"))?;
    let vars: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();
    let strip = crate::adapter::session_env(vars.iter().map(String::as_str));
    let inner = inner_command(&exe, run, &strip);
    let term = terminal(&handle, cwd, &inner, class)?;
    let Some((program, rest)) = term.argv.split_first() else {
        return Err("terminal command is empty".to_string());
    };

    let mut cmd = Command::new(program);
    for name in &strip {
        cmd.env_remove(name);
    }
    cmd.args(rest)
        .current_dir(if Path::new(cwd).is_dir() { cwd } else { "/" })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("launching {program}: {e}"))
}

/// The `sh -c` line the terminal runs: `unset` of `strip` (names the adapter
/// validated, safe unquoted), then `run`. `--tag` so the wrapper identifies
/// its own window by `handle_for(tag)` rather than by winning a focus race it
/// cannot win.
fn inner_command(exe: &Path, run: &Run, strip: &[String]) -> String {
    let unset = if strip.is_empty() {
        String::new()
    } else {
        format!("unset {}; ", strip.join(" "))
    };
    let mut inner = format!(
        "{} run --tag {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(run.tag)
    );
    if let Some(parent) = run.parent {
        inner.push_str(&format!(" --parent {}", shell_quote(parent)));
    }
    if let Some(seed) = run.seed_file {
        inner.push_str(&format!(" --seed-file {}", shell_quote(&seed.to_string_lossy())));
    }
    let argv = run.argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    if run.keep_shell {
        format!("{unset}{inner} -- {argv}; exec \"$SHELL\"")
    } else {
        format!("{unset}exec {inner} -- {argv}")
    }
}

/// Single-quote for `sh -c`.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_swallow_property_follows_the_terminal_the_agent_was_running_in() {
        // No SEAGENTS_TERMINAL override in the test environment, so the
        // captured WM_CLASS decides.
        if std::env::var_os("SEAGENTS_TERMINAL").is_none()
            && std::env::var_os("SEAGENTS_TERMINAL_CMD").is_none()
        {
            assert_eq!(matcher_for("Terminator"), WindowMatch::Role);
            assert_eq!(matcher_for("Alacritty"), WindowMatch::Instance);
            // Unknown terminal: fall back to the Terminator recipe, whose
            // property is a role.
            assert_eq!(matcher_for("Xterm"), WindowMatch::Role);
        }
        assert_eq!(recipe_for_class("alacritty").as_deref(), Some("alacritty"));
        assert_eq!(recipe_for_class("Konsole"), None);
    }

    #[test]
    fn the_inner_command_runs_this_binary_and_keeps_a_shell_only_when_asked() {
        let argv: Vec<String> = ["claude", "--resume", "it's"].iter().map(|s| s.to_string()).collect();
        let mut run = Run {
            tag: "t1",
            parent: None,
            seed_file: None,
            argv: &argv,
            keep_shell: true,
        };
        let exe = Path::new("/opt/my bin/seagents");
        assert_eq!(
            inner_command(exe, &run, &[]),
            r#"'/opt/my bin/seagents' run --tag 't1' -- 'claude' '--resume' 'it'\''s'; exec "$SHELL""#
        );
        run.parent = Some("boss");
        run.seed_file = Some(Path::new("/run/user/1/seagents/seed-t1"));
        run.keep_shell = false;
        assert_eq!(
            inner_command(exe, &run, &[]),
            "exec '/opt/my bin/seagents' run --tag 't1' --parent 'boss' \
             --seed-file '/run/user/1/seagents/seed-t1' -- 'claude' '--resume' 'it'\\''s'"
        );
        // The calling session's variables are gone before `run` starts, and
        // stay gone for the shell a restored window drops back to.
        let strip = ["CLAUDECODE".to_string(), "CLAUDE_PID".to_string()];
        assert!(inner_command(exe, &run, &strip).starts_with("unset CLAUDECODE CLAUDE_PID; exec '/opt/my bin/seagents' run"));
        run.keep_shell = true;
        assert!(inner_command(exe, &run, &strip).starts_with("unset CLAUDECODE CLAUDE_PID; '/opt/my bin/seagents' run"));
    }

    #[test]
    fn shell_quoting_survives_a_prompt_full_of_quotes() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_quote("plain"), "'plain'");
    }
}
