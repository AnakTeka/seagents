//! `seagents send` / `stop` — drive an agent through its wrapper's control
//! socket — and the socket's wire format.
//!
//! Only the wrapper holds the agent's PTY master, so it is the wrapper that
//! writes: it listens on `<runtime dir>/<wrapper_pid>.sock` (`wrapper::control`)
//! and this module is its client. One JSON object per line each way: a
//! [`Request`], then a [`Reply`].
//!
//! Everything that bounds a request is decided **in the wrapper**, from the
//! connecting process's `SO_PEERCRED` pid — never here: a process writing to
//! the socket directly is judged exactly like this CLI. So this side only
//! builds the request, prints the reply and turns its `reason` into an exit
//! code.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::registry::{self, Session};

/// How long `send` holds out for an idle agent when not told otherwise.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Slack on top of the request's own bound before the client stops waiting
/// for a reply: `stop` takes up to 15 s of grace periods, and a wrapper
/// suspended with its agent (Ctrl+Z) answers nothing until `fg`.
const REPLY_SLACK: Duration = Duration::from_secs(30);

/// One request to a wrapper.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(crate) enum Request {
    /// Write `text` into the agent (and by default submit it).
    Send {
        text: String,
        #[serde(default)]
        mode: Mode,
        /// Press Enter after the body.
        #[serde(default = "yes")]
        submit: bool,
        /// Write at once instead of waiting for the agent to be idle.
        #[serde(default)]
        now: bool,
        /// Write even though the agent is blocked at a gate. Humans only.
        #[serde(default)]
        answer_permission: bool,
        /// How long to wait for the agent to be idle (default 5 min).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Ask the agent to exit; signal it if it does not.
    Stop {
        #[serde(default)]
        force: bool,
    },
}

fn yes() -> bool {
    true
}

/// How a `send` body is written.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    /// As a paste: bracketed when the agent has asked for bracketed paste.
    #[default]
    Paste,
    /// Raw key bytes, with `\e`-style escapes decoded (see `send --keys`).
    Keys,
}

/// A wrapper's answer.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub(crate) struct Reply {
    pub(crate) ok: bool,
    /// The agent's tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tag: Option<String>,
    /// `send`: the ledger's `stops` at the moment of submission — the
    /// baseline for `wait --after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stops: Option<u64>,
    /// `send`: whether the prompt went into an idle agent, so the next turn
    /// to end is the one it started. False for `--now` and for an answered
    /// gate: `wait` may then return the turn the text was folded into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) paired: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<Refusal>,
    /// For `blocked`: the detection rule naming the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rule: Option<String>,
}

/// Why a request was not carried out.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Refusal {
    /// The agent waits at a gate (a permission prompt, a trust dialog): a
    /// human answers those. Exit 3.
    Blocked,
    /// It stayed in a turn for the whole timeout. Exit 2.
    BusyTimeout,
    /// The lineage rule: an agent may drive only its own descendants, and
    /// never answer a gate. Exit 5.
    Forbidden,
    /// The agent exited first. Exit 4.
    Exit,
    /// Nothing can tell when this agent is idle (no detector knows it); only
    /// `--now` can send to it. Exit 1.
    Unsupported,
    /// The agent's input box holds text seagents did not send — someone's
    /// draft, which a paste would be appended to. Exit 6.
    Draft,
    /// The request itself was malformed. Exit 1.
    BadRequest,
    /// Anything else went wrong. Exit 1.
    Failed,
}

impl Refusal {
    fn code(self) -> i32 {
        match self {
            Refusal::BusyTimeout => 2,
            Refusal::Blocked => 3,
            Refusal::Exit => 4,
            Refusal::Forbidden => 5,
            Refusal::Draft => 6,
            Refusal::Unsupported | Refusal::BadRequest | Refusal::Failed => 1,
        }
    }
}

impl Reply {
    pub(crate) fn refused(reason: Refusal, error: impl Into<String>) -> Reply {
        Reply {
            ok: false,
            reason: Some(reason),
            error: Some(error.into()),
            ..Reply::default()
        }
    }

    /// The exit code the reply stands for.
    fn code(&self) -> i32 {
        match (self.ok, self.reason) {
            (true, _) => 0,
            (false, Some(r)) => r.code(),
            (false, None) => 1,
        }
    }
}

pub(crate) struct SendArgs {
    pub(crate) target: String,
    pub(crate) now: bool,
    pub(crate) keys: bool,
    pub(crate) no_submit: bool,
    pub(crate) answer_permission: bool,
    pub(crate) timeout: Duration,
    pub(crate) prompt_file: Option<PathBuf>,
    pub(crate) text: Option<String>,
}

/// `seagents send …`: prints the reply JSON; the exit code follows its
/// `reason` (0 ok, 2 busy_timeout, 3 blocked, 4 exit, 5 forbidden, 1 else).
pub(crate) fn send(args: SendArgs) -> i32 {
    let text = match (&args.prompt_file, args.text) {
        (Some(f), _) => match std::fs::read_to_string(f) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("seagents send: {}: {e}", f.display());
                return 1;
            }
        },
        (None, Some(t)) => t,
        (None, None) => {
            eprintln!("seagents send: no text: give TEXT or --prompt-file");
            return 1;
        }
    };
    if !args.keys && text.trim().is_empty() {
        eprintln!("seagents send: the text is empty");
        return 1;
    }
    let request = Request::Send {
        text,
        mode: if args.keys { Mode::Keys } else { Mode::Paste },
        submit: !args.no_submit,
        now: args.now,
        answer_permission: args.answer_permission,
        timeout_ms: Some(args.timeout.as_millis().min(u128::from(u64::MAX)) as u64),
    };
    exchange("send", &args.target, &request, args.timeout)
}

/// `seagents stop <target> [--force]`: 0 once the agent is gone, 5 when the
/// lineage rule forbids it, 1 otherwise.
pub(crate) fn stop(target: &str, force: bool) -> i32 {
    exchange("stop", target, &Request::Stop { force }, Duration::ZERO)
}

/// Send `request` to `target`'s wrapper, print the reply, return its code.
fn exchange(cmd: &str, target: &str, request: &Request, bound: Duration) -> i32 {
    match ask(target, request, bound) {
        Ok(reply) => {
            println!("{}", serde_json::to_string(&reply).unwrap_or_default());
            if let Some(e) = &reply.error {
                eprintln!("seagents {cmd}: {e}");
            }
            reply.code()
        }
        Err(e) => {
            eprintln!("seagents {cmd}: {e}");
            1
        }
    }
}

/// Resolve `target` and carry `request` to its wrapper: the reply, or why
/// there is none. `bound` is how long the request itself may take (a `send`'s
/// idle wait; zero for `stop`, whose grace periods the slack covers). The one
/// client: the CLI and the HUD's keys both come through here.
pub(crate) fn ask(target: &str, request: &Request, bound: Duration) -> Result<Reply, String> {
    let session = registry::resolve_target(target)?;
    call(&session, request, bound + REPLY_SLACK)
}

/// One request/reply round trip with `session`'s wrapper.
fn call(session: &Session, request: &Request, reply_within: Duration) -> Result<Reply, String> {
    let tag = &session.meta.tag;
    let sock = session
        .files
        .sock
        .as_ref()
        .map_err(|e| format!("{tag} can have no control socket: {e}"))?;
    let mut stream = UnixStream::connect(sock).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "{tag} has no control socket at {}: its wrapper predates `send`, or could not bind \
             one (SEAGENTS_DEBUG=1 on `run` says why); restart the agent to drive it",
            sock.display()
        ),
        std::io::ErrorKind::ConnectionRefused => {
            // Nobody listens: the wrapper died without removing it.
            let _ = std::fs::remove_file(sock);
            format!("{tag}'s control socket was stale (its wrapper is gone); removed it")
        }
        _ => format!("connecting to {tag}'s wrapper at {}: {e}", sock.display()),
    })?;
    let _ = stream.set_read_timeout(Some(reply_within));
    let mut line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("writing to {tag}'s wrapper: {e}"))?;
    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(|e| format!("no reply from {tag}'s wrapper: {e}"))?;
    if reply.is_empty() {
        return Err(format!("{tag}'s wrapper closed the connection without a reply"));
    }
    serde_json::from_str(&reply).map_err(|e| format!("{tag}'s wrapper replied with nonsense: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_with_defaults_and_round_trip() {
        let r: Request = serde_json::from_str(r#"{"op":"send","text":"hi"}"#).unwrap();
        assert_eq!(
            r,
            Request::Send {
                text: "hi".into(),
                mode: Mode::Paste,
                submit: true,
                now: false,
                answer_permission: false,
                timeout_ms: None
            }
        );
        let r: Request = serde_json::from_str(
            r#"{"op":"send","text":"1","mode":"keys","submit":false,"now":true,"answer_permission":true,"timeout_ms":5}"#,
        )
        .unwrap();
        let back: Request = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert_eq!(serde_json::from_str::<Request>(r#"{"op":"stop"}"#).unwrap(), Request::Stop { force: false });
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"stop","force":true}"#).unwrap(),
            Request::Stop { force: true }
        );
        for bad in [r#"{"op":"frob"}"#, r#"{"op":"send"}"#, r#"{"op":"send","text":"x","mode":"typed"}"#, "nope"] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn replies_carry_only_what_applies_and_map_to_exit_codes() {
        let ok = Reply { ok: true, tag: Some("b".into()), stops: Some(1), paired: Some(true), ..Reply::default() };
        assert_eq!(serde_json::to_string(&ok).unwrap(), r#"{"ok":true,"tag":"b","stops":1,"paired":true}"#);
        let mut blocked = Reply::refused(Refusal::Blocked, "b is blocked");
        blocked.rule = Some("permission_prompt".into());
        assert_eq!(
            serde_json::to_string(&blocked).unwrap(),
            r#"{"ok":false,"error":"b is blocked","reason":"blocked","rule":"permission_prompt"}"#
        );
        let codes: Vec<(String, i32)> = [
            Refusal::Blocked,
            Refusal::BusyTimeout,
            Refusal::Forbidden,
            Refusal::Exit,
            Refusal::Unsupported,
            Refusal::BadRequest,
            Refusal::Failed,
        ]
        .into_iter()
        .map(|r| (serde_json::to_string(&r).unwrap(), Reply::refused(r, "").code()))
        .collect();
        let want: Vec<(String, i32)> = [
            ("\"blocked\"", 3),
            ("\"busy_timeout\"", 2),
            ("\"forbidden\"", 5),
            ("\"exit\"", 4),
            ("\"unsupported\"", 1),
            ("\"bad_request\"", 1),
            ("\"failed\"", 1),
        ]
        .into_iter()
        .map(|(s, c)| (s.to_string(), c))
        .collect();
        assert_eq!(codes, want);
        assert_eq!(ok.code(), 0);
    }
}
