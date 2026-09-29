//! Launching a terminal window seagents can recognize afterwards.
//!
//! A window seagents opens itself — a restored agent today, a spawned one once
//! orchestration lands — is launched carrying a minted handle,
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

/// Launch one terminal window running `argv` under `seagents run --tag <tag>`
/// (and `--parent`, when given), carrying the handle minted from that tag.
///
/// The agent runs under `seagents run` so the session is teed and
/// tracked from its first byte, and the shell survives it — closing the agent
/// leaves the window sitting at a prompt, exactly as it does when you start one
/// by hand.
///
/// `SEAGENTS_TERMINAL_CMD` overrides the whole invocation for terminals without
/// a built-in recipe; `{handle}` (alias `{role}`), `{cwd}` and `{cmd}` are
/// substituted.
pub(crate) fn spawn_terminal(
    tag: &str,
    parent: Option<&str>,
    cwd: &str,
    argv: &[String],
    class: &str,
) -> Result<(), String> {
    let handle = handle_for(tag);
    // This very binary, by absolute path: the new window's shell has whatever
    // PATH its rc files give it, and the one it finds need not be us.
    let exe = std::env::current_exe().map_err(|e| format!("locating seagents: {e}"))?;
    // `--tag` so the wrapper identifies its own window by `handle_for(tag)`
    // rather than by winning a focus race it cannot win.
    let mut inner = format!(
        "{} run --tag {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(tag)
    );
    if let Some(parent) = parent {
        inner.push_str(&format!(" --parent {}", shell_quote(parent)));
    }
    inner.push_str(&format!(
        " -- {}; exec \"$SHELL\"",
        argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
    ));
    let term = terminal(&handle, cwd, &inner, class)?;
    let Some((program, rest)) = term.argv.split_first() else {
        return Err("terminal command is empty".to_string());
    };

    Command::new(program)
        .args(rest)
        .current_dir(if Path::new(cwd).is_dir() { cwd } else { "/" })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("launching {program}: {e}"))
}

/// Single-quote for `sh -c`.
fn shell_quote(s: &str) -> String {
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
    fn shell_quoting_survives_a_prompt_full_of_quotes() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_quote("plain"), "'plain'");
    }
}
