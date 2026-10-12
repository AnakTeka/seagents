//! `seagents send` / `stop` — drive an agent through herdr.
//!
//! herdr writes into the agent's pane (`herdr agent prompt`, `pane
//! send-keys`, …); what it does not do, this does, around every write:
//!
//! - **Submit when idle.** herdr sends at once, and a prompt that lands
//!   mid-turn is folded into the running turn (its first answer lost). So by
//!   default a `send` is held (up to its timeout) until the agent is out of
//!   its turn (`turn::Ledger::in_turn`, the one definition), herdr reads it
//!   idle, and no earlier submission of ours is still landing; the reply's
//!   `stops` then pairs it with the turn it opens.
//! - **One at a time.** Every `send` and `stop` holds the session's send lock
//!   for its whole length (`registry::send_lock`), so two never interleave —
//!   from the CLI, the HUD or a seed, in any process.
//! - **The input box.** A paste lands after whatever the agent's input box
//!   holds. A prompt seagents itself submitted that the agent put back (a
//!   turn dropped before its first response) is cleared; anything else is
//!   someone's draft and is refused.
//! - **A blocked agent** — a permission prompt, a trust dialog — is refused:
//!   whatever is typed into one answers it. herdr refuses a prompt there
//!   itself (`agent_blocked`); keys are refused here unless the caller says
//!   it means to answer (`--answer-permission`).
//!
//! The CLI, the HUD's `p`/`x` keys and `run`'s seed delivery all come
//! through [`submit`] and [`stop_session`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::adapter;
use crate::herdr;
use crate::registry::{self, Session};
use crate::turn::Ledger;

/// How long `send` holds out for an idle agent when not told otherwise.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// How often a waiting `send` looks at the agent again.
const POLL: Duration = Duration::from_millis(200);

/// After a submission, how long the next one waits for the agent's prompt
/// hook to confirm it landed. An agent without hooks has none, so for it this
/// is simply a pause long enough for herdr to see the turn start.
const LANDING_GRACE: Duration = Duration::from_secs(5);

/// How long keys typed into a dialog wait for herdr to see it gone before
/// the reply — so a `wait` called the moment this returns doesn't read the
/// dialog that was just answered. A key that doesn't clear it (moving
/// through a menu) just replies this much later.
const ANSWER_SETTLE: Duration = Duration::from_secs(1);

/// `stop`: how long the exit sequence, then closing the pane, each get.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long a `stop` waits for a `send` in progress before going ahead
/// without typing: a send can be held for minutes waiting on a busy agent.
const STOP_LOCK_WAIT: Duration = Duration::from_secs(2);

/// The longest text herdr can be handed: it travels as one argument of the
/// `herdr` command line (`MAX_ARG_STRLEN`).
const MAX_TEXT: usize = 128 * 1024 - 1;

/// The answer to a `send` or `stop`.
#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub(crate) struct Reply {
    pub(crate) ok: bool,
    /// The agent's tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tag: Option<String>,
    /// `send`: the ledger's `stops` at the moment of submission — the
    /// baseline for `wait --after`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stops: Option<u64>,
    /// `send`: whether the prompt went into an idle agent, so the next turn
    /// to end is the one it started. False for `--now` and for keys into a
    /// dialog: `wait` may then return the turn the text was folded into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) paired: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<Refusal>,
    /// For `blocked`: the detection rule naming the dialog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rule: Option<String>,
}

/// Why a request was not carried out.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Refusal {
    /// The agent waits at a dialog (a permission prompt, a trust dialog): a
    /// human answers those. Exit 3.
    Blocked,
    /// It stayed in a turn for the whole timeout. Exit 2.
    BusyTimeout,
    /// The agent exited first. Exit 4.
    Exit,
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
            Refusal::Draft => 6,
            Refusal::BadRequest | Refusal::Failed => 1,
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

/// What a `send` writes.
pub(crate) enum Body {
    /// A prompt: pasted, then Enter.
    Prompt(String),
    /// Text pasted without Enter.
    Text(String),
    /// Key presses, by herdr's names.
    Keys(Vec<String>),
}

/// How a `send` is carried out.
pub(crate) struct Opts {
    /// Write at once instead of waiting for the agent to be idle.
    pub(crate) now: bool,
    /// Keys may go into a dialog — to answer it.
    pub(crate) answer: bool,
    /// How long to wait for the agent to be idle.
    pub(crate) timeout: Duration,
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
/// `reason` (0 ok, 2 busy_timeout, 3 blocked, 4 exit, 6 draft, 1 else).
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
    if text.trim().is_empty() {
        eprintln!("seagents send: the text is empty");
        return 1;
    }
    let body = if args.keys {
        Body::Keys(text.split_whitespace().map(str::to_string).collect())
    } else if args.no_submit {
        Body::Text(text)
    } else {
        Body::Prompt(text)
    };
    let opts = Opts {
        now: args.now,
        answer: args.answer_permission,
        timeout: args.timeout,
    };
    print_reply(
        "send",
        registry::resolve_target(&args.target).map(|s| submit(&s, body, &opts)),
    )
}

/// `seagents stop <target> [--force]`: 0 once the agent is gone, 1 otherwise.
pub(crate) fn stop(target: &str, force: bool) -> i32 {
    print_reply(
        "stop",
        registry::resolve_target(target).map(|s| stop_session(&s, force)),
    )
}

/// Print a reply (or why there is none), return its exit code.
fn print_reply(cmd: &str, outcome: Result<Result<Reply, Reply>, String>) -> i32 {
    match outcome {
        Ok(Ok(reply) | Err(reply)) => {
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

/// The HUD's `p`: a prompt to the agent tagged `target`, held until it is
/// idle.
pub(crate) fn prompt_target(target: &str, text: String) -> Result<Reply, String> {
    let session = registry::resolve_target(target)?;
    let opts = Opts {
        now: false,
        answer: false,
        timeout: DEFAULT_TIMEOUT,
    };
    Ok(submit(&session, Body::Prompt(text), &opts).unwrap_or_else(|r| r))
}

/// The HUD's `x`: stop the agent tagged `target`.
pub(crate) fn stop_target(target: &str) -> Result<Reply, String> {
    let session = registry::resolve_target(target)?;
    Ok(stop_session(&session, false).unwrap_or_else(|r| r))
}

/// The pane the session's agent is in now (it moves when the pane is moved).
fn pane(session: &Session) -> Result<String, Reply> {
    match herdr::pane_of(&session.meta.key) {
        Ok(Some(p)) => Ok(p.pane_id),
        Ok(None) => Err(Reply::refused(
            Refusal::Exit,
            format!("{}'s herdr pane is gone", session.meta.tag),
        )),
        Err(e) => Err(Reply::refused(Refusal::Failed, e.to_string())),
    }
}

/// Whether a `send` may be written now.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Go { paired: bool },
    Wait,
    Blocked,
}

/// One look at the agent. A dialog refuses everything but keys meant to
/// answer it, and wins over `--now`: whatever is typed into one answers it.
/// Otherwise `--now` goes at once. The default waits for the agent to be out
/// of a turn, with no submission of ours still landing, and herdr reading it
/// idle.
fn gate(
    status: Option<herdr::Status>,
    in_turn: bool,
    landing: bool,
    now: bool,
    answer: bool,
) -> Gate {
    if status == Some(herdr::Status::Blocked) {
        return if answer {
            Gate::Go { paired: false }
        } else {
            Gate::Blocked
        };
    }
    if now {
        return Gate::Go { paired: false };
    }
    if !in_turn && !landing && status.is_some_and(herdr::Status::idle) {
        Gate::Go { paired: true }
    } else {
        Gate::Wait
    }
}

/// Write `body` into `session`'s agent, per `opts`: the reply, or the
/// refusal (also a reply, with `reason`).
pub(crate) fn submit(session: &Session, body: Body, opts: &Opts) -> Result<Reply, Reply> {
    let reply = submit_locked(session, body, opts);
    let tag = Some(session.meta.tag.clone());
    reply
        .map(|r| Reply {
            tag: tag.clone(),
            ..r
        })
        .map_err(|r| Reply { tag, ..r })
}

fn submit_locked(session: &Session, body: Body, opts: &Opts) -> Result<Reply, Reply> {
    let me = &session.meta.tag;
    let files = &session.files;
    let adapter = adapter::for_command(session.meta.agent())
        .map_err(|e| Reply::refused(Refusal::Failed, format!("agent adapter: {e}")))?;
    if let Body::Prompt(t) | Body::Text(t) = &body {
        if t.len() > MAX_TEXT && !(matches!(body, Body::Prompt(_)) && adapter.send.by_reference(t))
        {
            return Err(Reply::refused(
                Refusal::BadRequest,
                format!("the text is over {MAX_TEXT} bytes"),
            ));
        }
    }
    let _serial = registry::send_lock(files, None)
        .ok_or_else(|| Reply::refused(Refusal::Exit, format!("{me} exited")))?;
    let ledger = Ledger::for_command(session.meta.agent());
    let deadline = Instant::now() + opts.timeout;
    let answer = opts.answer && matches!(body, Body::Keys(_));
    loop {
        // On the `.meta` as it is now: the one this session was resolved with
        // may predate the agent's pid. A `stop` that did not wait for this
        // send ends here, with the agent.
        if !session.alive_now() {
            return Err(Reply::refused(Refusal::Exit, format!("{me} exited")));
        }
        let pane = pane(session)?;
        let agent = herdr::agent_on(&session.meta.key)
            .map_err(|e| Reply::refused(Refusal::Failed, e.to_string()))?;
        let (status, rule) = match crate::status::agent_status(agent.as_ref(), session.meta.agent())
        {
            Some((s, rule)) => (Some(s), rule),
            None => (None, None),
        };
        let agent = registry::read_agent(files);
        let in_turn = ledger.in_turn(&agent, registry::read_state(files).as_ref());
        let meta = registry::read_meta(files).unwrap_or_else(|| session.meta.clone());
        let landing = landing(&meta, ledger.hooks, agent.prompted_at);
        match gate(status, in_turn, landing, opts.now, answer) {
            Gate::Go { paired } => {
                return deliver(session, &adapter, &pane, body, paired, status, agent.stops)
            }
            Gate::Blocked => {
                let mut r = Reply::refused(
                    Refusal::Blocked,
                    format!(
                        "{me} is blocked at a dialog ({}); a human answers this",
                        rule.as_deref().unwrap_or("unknown rule")
                    ),
                );
                r.rule = rule;
                return Err(r);
            }
            Gate::Wait if Instant::now() >= deadline => {
                return Err(Reply::refused(
                    Refusal::BusyTimeout,
                    format!(
                        "{me} was still busy after {} s; nothing was sent",
                        opts.timeout.as_secs()
                    ),
                ))
            }
            Gate::Wait => std::thread::sleep(POLL),
        }
    }
}

/// Whether the last prompt seagents submitted is still landing: within the
/// grace, and (with hooks) no prompt event since it was written.
fn landing(meta: &registry::Meta, hooks: bool, prompted_at: Option<u64>) -> bool {
    let Some(at) = meta.submitted_ms else {
        return false;
    };
    let within = crate::clock::unix_now_ms() < at + LANDING_GRACE.as_millis() as u64;
    within && !(hooks && prompted_at.is_some_and(|p| p >= at))
}

/// Write the body, now that the gate is open.
fn deliver(
    session: &Session,
    adapter: &adapter::Adapter,
    pane: &str,
    body: Body,
    paired: bool,
    status: Option<herdr::Status>,
    stops: u64,
) -> Result<Reply, Reply> {
    let files = &session.files;
    let failed = |e: herdr::Error| match e.code.as_str() {
        "agent_blocked" => Reply::refused(
            Refusal::Blocked,
            format!("{} is blocked at a dialog", session.meta.tag),
        ),
        _ => Reply::refused(Refusal::Failed, e.to_string()),
    };
    let ok = Reply {
        ok: true,
        stops: Some(stops),
        paired: Some(paired),
        ..Reply::default()
    };
    match body {
        Body::Keys(keys) => {
            herdr::send_keys(pane, &keys).map_err(failed)?;
            if status == Some(herdr::Status::Blocked) {
                // Into a dialog: reply once it is gone.
                let settle = Instant::now() + ANSWER_SETTLE;
                while Instant::now() < settle && blocked(session) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            Ok(Reply {
                paired: Some(false),
                ..ok
            })
        }
        Body::Text(text) => {
            make_room(session, adapter, pane)?;
            herdr::send_text(pane, &text).map_err(failed)?;
            Ok(Reply {
                paired: Some(false),
                ..ok
            })
        }
        Body::Prompt(text) => {
            make_room(session, adapter, pane)?;
            let text = if adapter.send.by_reference(&text) {
                registry::by_reference(&session.meta.key, &session.meta.cwd, stops, &text).map_err(
                    |e| Reply::refused(Refusal::Failed, format!("writing the prompt file: {e}")),
                )?
            } else {
                text
            };
            let ms = crate::clock::unix_now_ms();
            herdr::prompt(pane, &text).map_err(failed)?;
            registry::update_meta(files, |m| {
                m.last_prompt = Some(text.clone());
                m.submitted_ms = Some(ms);
            });
            // A prompt: from here its hooks owe a report (`hook::health`).
            registry::note_engaged(files);
            if adapter.hooks.enabled() {
                // Reply once the agent has announced it (≤ LANDING_GRACE), so
                // a `wait --after <stops>` called the moment this returns sees
                // this turn open — not the ledger as the last turn left it,
                // which after an Esc reads "prompted, no longer running":
                // `interrupted` (found live on Codex, 2026-09-29).
                if !announced(session, ms) {
                    // Not announced, and the text still sits in the box: its
                    // Enter was lost — an agent still settling right after its
                    // folder-trust dialog closed reads idle yet drops it
                    // (seen live, Claude 2.1.28x). Pressed once more.
                    let glyph = &adapter.send.input_glyph;
                    let unsent = !glyph.is_empty()
                        && input_box(pane, glyph).is_some_and(|t| same_text(&t, &text));
                    if unsent && herdr::send_keys(pane, &["enter".to_string()]).is_ok() {
                        announced(session, ms);
                    }
                }
            }
            Ok(ok)
        }
    }
}

/// Wait up to [`LANDING_GRACE`] for the agent's prompt hook to report a
/// prompt submitted at `ms`; whether it did.
fn announced(session: &Session, ms: u64) -> bool {
    let landed = Instant::now() + LANDING_GRACE;
    loop {
        if registry::read_agent(&session.files)
            .prompted_at
            .is_some_and(|p| p >= ms)
        {
            return true;
        }
        if Instant::now() >= landed || !session.alive_now() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How long, after each clearing key, [`make_room`] lets the agent repaint.
const REPAINT: Duration = Duration::from_millis(150);

/// The same words, however the box wrapped and indented them.
fn same_text(a: &str, b: &str) -> bool {
    a.chars()
        .filter(|c| !c.is_whitespace())
        .eq(b.chars().filter(|c| !c.is_whitespace()))
}

/// What the agent's input box holds now (`screen::input_box`); None: nothing
/// to go by.
fn input_box(pane: &str, glyph: &str) -> Option<String> {
    crate::screen::input_box(&herdr::read(pane, true).ok()?, glyph)
}

/// Make sure a paste starts a prompt of its own: the input box must be
/// empty, or hold exactly the last prompt seagents submitted — one the agent
/// put back after a turn dropped before its first response (Claude, §6) —
/// which is cleared with the adapter's `input_clear`, pressed until the box
/// reads empty. Anything else is someone's draft: refused, `--now` or not —
/// the human is typing. An adapter with no `input_glyph`, or a screen that
/// shows no box, goes ahead.
fn make_room(session: &Session, adapter: &adapter::Adapter, pane: &str) -> Result<(), Reply> {
    let send = &adapter.send;
    if send.input_glyph.is_empty() {
        return Ok(());
    }
    let Some(text) = input_box(pane, &send.input_glyph).filter(|t| !t.is_empty()) else {
        return Ok(());
    };
    let me = &session.meta.tag;
    let ours = registry::read_meta(&session.files).and_then(|m| m.last_prompt);
    if !ours.is_some_and(|o| same_text(&o, &text)) {
        // Not echoed: an agent caller has no business reading a human's
        // unsent words.
        return Err(Reply::refused(
            Refusal::Draft,
            format!(
                "{me}'s input box holds a draft ({} characters) that seagents did not send; \
                 whoever is typing there submits or clears it",
                text.chars().count()
            ),
        ));
    }
    // Two presses per line (its text, then the newline before it), and a
    // few to spare; a box that stops shrinking is given up on.
    let mut left = text;
    let mut stalled = 0;
    for _ in 0..2 * left.lines().count() + 4 {
        herdr::send_keys(pane, &send.input_clear)
            .map_err(|e| Reply::refused(Refusal::Failed, e.to_string()))?;
        std::thread::sleep(REPAINT);
        match input_box(pane, &send.input_glyph) {
            Some(t) if t.is_empty() => return Ok(()),
            Some(t) if t == left => stalled += 1,
            Some(t) => left = t,
            None => stalled += 1,
        }
        if stalled >= 3 {
            break;
        }
    }
    Err(Reply::refused(
        Refusal::Failed,
        format!("{me}'s input box still held a prompt seagents had sent, after clearing it; nothing was sent"),
    ))
}

/// Whether the agent waits at a dialog — herdr's, or one of the adapter's
/// `[screen] dialogs` (`status::agent_status`, which counts a screen it could
/// not read as one).
fn blocked(session: &Session) -> bool {
    let agent = herdr::agent_on(&session.meta.key).ok().flatten();
    crate::status::agent_status(agent.as_ref(), session.meta.agent())
        .is_some_and(|(s, _)| s == herdr::Status::Blocked)
}

/// Make the agent exit: its adapter's exit sequence as a prompt, given
/// [`STOP_GRACE`]; then its pane closed (which hangs it up), given as long;
/// then SIGKILL to its process group. Straight to closing the pane with
/// `force`, with no exit sequence, when the agent is at a dialog — herdr's,
/// or one herdr misses — (the sequence's Enter would pick an answer), when
/// its input box holds someone's draft (the sequence would be submitted as
/// part of it), or when a `send` holds the session for longer than
/// [`STOP_LOCK_WAIT`] (one held waiting on a busy agent; it then fails with
/// `exit` once the agent is gone, rather than type into a dying one).
/// Replies once the agent is gone; `run`, which holds its window, then
/// closes the window too.
pub(crate) fn stop_session(session: &Session, force: bool) -> Result<Reply, Reply> {
    let tag = Some(session.meta.tag.clone());
    let gone = Ok(Reply {
        ok: true,
        tag: tag.clone(),
        ..Reply::default()
    });
    let Some(pid) = session
        .meta
        .pid
        .or_else(|| registry::read_meta(&session.files).and_then(|m| m.pid))
    else {
        return Err(Reply {
            tag,
            ..Reply::refused(Refusal::Failed, "the agent has not started yet")
        });
    };
    if !crate::procfs::alive(pid) {
        return gone;
    }
    let serial = (!force)
        .then(|| registry::send_lock(&session.files, Some(STOP_LOCK_WAIT)))
        .flatten();
    let adapter = adapter::for_command(session.meta.agent()).unwrap_or_default();
    let pane = pane(session).ok();
    if let (Some(_), Some(pane)) = (&serial, pane.as_deref()) {
        if !blocked(session)
            && !adapter.stop.exit_sequence.is_empty()
            && make_room(session, &adapter, pane).is_ok()
            && herdr::prompt(pane, &adapter.stop.exit_sequence).is_ok()
            && exited(pid, STOP_GRACE)
        {
            return gone;
        }
    }
    if let Some(pane) = pane.as_deref() {
        let _ = herdr::close_pane(pane);
        if exited(pid, STOP_GRACE) {
            return gone;
        }
    }
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    if exited(pid, STOP_GRACE) {
        return gone;
    }
    Err(Reply {
        tag,
        ..Reply::refused(Refusal::Failed, "the agent is still running after SIGKILL")
    })
}

/// Wait up to `within` for `pid` to be gone; whether it is.
fn exited(pid: u64, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while crate::procfs::alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}
