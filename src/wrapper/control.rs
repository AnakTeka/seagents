//! The wrapper's control socket: how `seagents send` / `stop` reach the one
//! process that holds the agent's PTY master.
//!
//! `<runtime dir>/<wrapper_pid>.sock` (0600, in a 0700 dir), bound once the
//! `.meta` exists and removed on the way out — by [`Control::finish`] on a
//! normal exit and by a SIGHUP/SIGTERM handler; anything left by a harder
//! death is swept by `registry::reap`. Each connection carries one JSON
//! request line and gets one reply line (`crate::send` has the format).
//!
//! **Every request is authorized here**, from the connecting process's
//! `SO_PEERCRED` pid: its `/proc` ancestry names the calling agent
//! (`registry::enclosing_session`), and an agent may drive only its own
//! descendants — walking this agent's `parent` chain by tag must reach the
//! caller — and may never answer a gate. No wrapper among the caller's
//! ancestors is a human, who may do anything. The CLI's own checks are
//! convenience; this is the gate.
//!
//! A `send` is written into an **idle** agent by default, so the next turn to
//! end is the one it started: the thread waits (up to the request's timeout)
//! until the agent is out of its turn (`turn::Ledger::in_turn`, the one
//! definition) and its `.state` reads idle. A blocked agent is refused at
//! once, with the gate's rule. Submissions are serialized, and one only
//! counts as landed once the agent's own prompt hook has fired for it, so two
//! sends in quick succession cannot both see "idle" and fold into one turn.
//!
//! A body the agent would file as quoted data if pasted (the adapter's
//! `long_prompt`, `adapter::Send::by_reference`) is written to a prompt file
//! in its cwd (`registry::write_prompt`) and replaced by one line telling it
//! to read that file and who sent it; the files go when the agent exits.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::inject::{self, Body, Framing, PtyModes, SharedWriter};
use crate::adapter::{self, Turns};
use crate::registry::{self, Files, StateRecord};
use crate::send::{Mode, Refusal, Reply, Request};
use crate::turn::Ledger;

/// A request line longer than this is refused unread: 16 MiB is far past any
/// prompt, and the line is buffered whole.
const MAX_REQUEST: u64 = 16 * 1024 * 1024;

/// How long a client may take to send its request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a waiting `send` looks at the agent again.
const POLL: Duration = Duration::from_millis(200);

/// After a submission, how long the next one waits for the agent's prompt
/// hook to confirm it landed. Screen adapters have no hook, so for them this
/// is simply a pause long enough for the detector to see the turn start.
const LANDING_GRACE: Duration = Duration::from_secs(5);

/// How long a `send --answer-permission` waits, after writing, for the
/// monitor to see the gate gone before it replies. Long enough for the agent
/// to read the key and repaint; a key that doesn't clear the gate (moving
/// through a menu) just replies this much later.
const ANSWER_SETTLE: Duration = Duration::from_secs(1);

/// `stop`: how long the exit sequence, then SIGTERM, then SIGKILL each get.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long [`Control::finish`] lets in-flight requests reply once the agent
/// has exited (a `stop` is waiting to say so).
const DRAIN: Duration = Duration::from_secs(2);

/// What the control thread needs from the wrapper.
pub(super) struct Wiring {
    pub(super) files: Files,
    /// The `__lead` anchor.
    pub(super) anchor_pid: u64,
    /// The PTY master: its foreground process group is the agent's.
    pub(super) master_fd: RawFd,
    pub(super) writer: SharedWriter,
    pub(super) modes: Arc<PtyModes>,
    /// Hurries the monitor after a write that may have answered a gate.
    pub(super) wake: Arc<super::monitor::Wake>,
    /// The agent's command, first word: which adapter, which ledger.
    pub(super) argv0: String,
    /// The seed prompt it was started on, if any: submitted by seagents,
    /// like a `send`.
    pub(super) seed: Option<String>,
}

struct Shared {
    wiring: Wiring,
    ledger: Ledger,
    life: Mutex<Life>,
    changed: Condvar,
    /// Held across a whole submission (body, delay, Enter), so two never
    /// interleave; records the last one until the agent confirms it.
    last_submit: Mutex<Option<Submitted>>,
    /// The text of the last prompt seagents submitted (a `send`'s paste, or
    /// the seed): what the agent may have put back in its input box.
    ours: Mutex<Option<String>>,
}

#[derive(Default)]
struct Life {
    exited: bool,
    inflight: usize,
}

/// The wrapper's handle on its control thread.
pub(super) struct Control {
    shared: Arc<Shared>,
    sock: PathBuf,
}

/// Bind the socket and start serving. None (with a note on stderr under
/// `SEAGENTS_DEBUG`) when it cannot be bound: the agent still runs, it just
/// cannot be driven.
pub(super) fn spawn(wiring: Wiring) -> Option<Control> {
    let sock = match wiring.files.sock.clone() {
        Ok(s) => s,
        Err(e) => {
            if std::env::var_os("SEAGENTS_DEBUG").is_some() {
                eprintln!("seagents: no control socket: {e}");
            }
            return None;
        }
    };
    let listener = match bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            if std::env::var_os("SEAGENTS_DEBUG").is_some() {
                eprintln!("seagents: control socket {}: {e}", sock.display());
            }
            return None;
        }
    };
    remove_on_signal(sock.clone());
    let shared = Arc::new(Shared {
        ledger: Ledger::for_command(&wiring.argv0),
        ours: Mutex::new(wiring.seed.clone()),
        wiring,
        life: Mutex::new(Life::default()),
        changed: Condvar::new(),
        last_submit: Mutex::new(None),
    });
    let server = Arc::clone(&shared);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            server.life().inflight += 1;
            let conn = Arc::clone(&server);
            thread::spawn(move || {
                handle(&conn, stream);
                conn.life().inflight -= 1;
                conn.changed.notify_all();
            });
        }
    });
    Some(Control { shared, sock })
}

impl Control {
    /// The agent has exited: wake whatever waits on it, give in-flight
    /// requests a moment to reply (a `stop` is waiting to say it worked),
    /// and remove the socket and the session's prompt files.
    pub(super) fn finish(self) {
        let deadline = Instant::now() + DRAIN;
        let mut life = self.shared.life();
        life.exited = true;
        self.shared.changed.notify_all();
        while life.inflight > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            life = self
                .shared
                .changed
                .wait_timeout(life, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        drop(life);
        let _ = std::fs::remove_file(&self.sock);
        // The agent is gone, so nothing will read them now.
        if let Some(meta) = registry::read_meta(&self.shared.wiring.files) {
            registry::remove_prompts(&meta);
        }
    }
}

impl Shared {
    fn life(&self) -> std::sync::MutexGuard<'_, Life> {
        self.life.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn exited(&self) -> bool {
        self.life().exited
    }

    /// Block until the agent has exited or `within` passes; whether it has.
    fn wait_exit(&self, within: Duration) -> bool {
        let life = self.life();
        self.changed
            .wait_timeout_while(life, within, |l| !l.exited)
            .unwrap_or_else(|e| e.into_inner())
            .0
            .exited
    }
}

/// Bind `sock` owner-only. A leftover file at the path is from a dead
/// wrapper whose pid we now have, so it is ours to replace.
fn bind(sock: &std::path::Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(sock);
    let listener = UnixListener::bind(sock)?;
    // The dir is 0700 already (unless it is the cache-dir fallback); the
    // peer's uid is checked per request besides.
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// On SIGHUP (the terminal closed) or SIGTERM, remove the socket and then
/// die of the signal as before. SIGKILL can't be caught; `reap` sweeps.
fn remove_on_signal(sock: PathBuf) {
    use signal_hook::consts::{SIGHUP, SIGTERM};
    let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGHUP, SIGTERM]) else {
        return;
    };
    thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            let _ = std::fs::remove_file(&sock);
            let _ = signal_hook::low_level::emulate_default_handler(sig);
        }
    });
}

/// Serve one connection: authorize, carry out, reply.
fn handle(shared: &Shared, stream: UnixStream) {
    let reply = match serve(shared, &stream) {
        Ok(r) | Err(r) => r,
    };
    let reply = Reply {
        tag: registry::read_meta(&shared.wiring.files).map(|m| m.tag),
        ..reply
    };
    if let Ok(mut line) = serde_json::to_string(&reply) {
        line.push('\n');
        let _ = (&stream).write_all(line.as_bytes());
    }
}

fn serve(shared: &Shared, stream: &UnixStream) -> Result<Reply, Reply> {
    let (pid, uid) = peer(stream).ok_or_else(|| Reply::refused(Refusal::Failed, "cannot identify the caller"))?;
    if uid != unsafe { libc::getuid() } {
        return Err(Reply::refused(Refusal::Forbidden, "another user's process"));
    }
    let request = read_request(stream)?;

    let me = registry::read_meta(&shared.wiring.files)
        .map(|m| m.tag)
        .ok_or_else(|| Reply::refused(Refusal::Failed, "this agent's .meta is gone"))?;
    let caller = registry::enclosing_session(pid).map(|s| s.meta.tag);
    let answer = matches!(request, Request::Send { answer_permission: true, .. });
    let parents: std::collections::HashMap<String, Option<String>> = registry::list()
        .into_iter()
        .map(|s| (s.meta.tag, s.meta.parent))
        .collect();
    authorize(caller.as_deref(), &me, answer, |t| parents.get(t).cloned().flatten())
        .map_err(|e| Reply::refused(Refusal::Forbidden, e))?;

    match request {
        Request::Send {
            text,
            mode,
            submit,
            now,
            answer_permission,
            timeout_ms,
        } => {
            let body = match mode {
                Mode::Paste => Paste(text),
                Mode::Keys => Keys(
                    inject::decode_keys(&text).map_err(|e| Reply::refused(Refusal::BadRequest, e))?,
                ),
            };
            let timeout = timeout_ms.map_or(crate::send::DEFAULT_TIMEOUT, Duration::from_millis);
            let who = caller.as_deref().map_or_else(|| "the user".to_string(), |t| format!("@{t}"));
            send(shared, &me, &who, body, submit, now, answer_permission, timeout)
        }
        Request::Stop { force } => stop(shared, force),
    }
}

/// The body of a `send`, owned.
enum Owned {
    Paste(String),
    Keys(Vec<u8>),
}
use Owned::{Keys, Paste};

impl Owned {
    fn body(&self) -> Body<'_> {
        match self {
            Paste(t) => Body::Paste(t),
            Keys(k) => Body::Keys(k),
        }
    }
}

/// `SO_PEERCRED`: the connecting process's pid and uid, as of `connect`.
fn peer(stream: &UnixStream) -> Option<(u64, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0 && cred.pid > 0).then_some((cred.pid as u64, cred.uid))
}

fn read_request(stream: &UnixStream) -> Result<Request, Reply> {
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST))
        .read_line(&mut line)
        .map_err(|e| Reply::refused(Refusal::BadRequest, format!("reading the request: {e}")))?;
    if !line.ends_with('\n') {
        return Err(Reply::refused(Refusal::BadRequest, "the request is not one complete line"));
    }
    serde_json::from_str(&line).map_err(|e| Reply::refused(Refusal::BadRequest, format!("bad request: {e}")))
}

/// The lineage rule. A human (`caller` None) may do anything. An agent may
/// act on `target` only when walking `target`'s parent chain reaches it — so
/// never on itself, its parent, a sibling or a stranger, which is also what
/// makes an A→B→A ping-pong impossible — and may never answer a gate.
fn authorize(
    caller: Option<&str>,
    target: &str,
    answer_permission: bool,
    parent_of: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let Some(caller) = caller else {
        return Ok(());
    };
    if answer_permission {
        return Err(format!(
            "only a human answers a permission prompt; the caller is agent {caller}"
        ));
    }
    let mut at = parent_of(target);
    // Bounded: a cycle of tags (a hand-edited `.meta`) must not hang us.
    for _ in 0..64 {
        match at {
            Some(p) if p == caller => return Ok(()),
            Some(p) => at = parent_of(&p),
            None => break,
        }
    }
    Err(format!(
        "agent {caller} may drive only its own descendants, and {target} is not one"
    ))
}

/// A submission the agent has not yet confirmed.
struct Submitted {
    at: Instant,
    /// Unix ms, comparable with the hook ledger's `prompted_at`.
    ms: u64,
}

impl Submitted {
    /// Still in flight: within the grace, and (with hooks) no prompt event
    /// since it was written.
    fn pending(&self, turns: Turns, prompted_at: Option<u64>) -> bool {
        self.at.elapsed() < LANDING_GRACE && !(turns == Turns::Hook && prompted_at.is_some_and(|p| p >= self.ms))
    }
}

/// Whether a `send` may be written now.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Go { paired: bool },
    Wait,
    Blocked,
    Unsupported,
}

/// One look at the agent. A gate refuses everything but a human's
/// `--answer-permission`, and wins over `--now`: whatever is typed into a
/// permission menu answers it. Otherwise `--now` goes at once. The default
/// waits for the agent to be out of a turn, with no submission of ours still
/// landing, and the screen idle.
fn gate(state: Option<&StateRecord>, in_turn: bool, pending: bool, detectable: bool, now: bool, answer: bool) -> Gate {
    let word = state.map(|s| s.state.as_str());
    if word == Some("blocked") {
        return if answer { Gate::Go { paired: false } } else { Gate::Blocked };
    }
    if now {
        return Gate::Go { paired: false };
    }
    if !detectable {
        return Gate::Unsupported;
    }
    if !in_turn && !pending && word == Some("idle") {
        Gate::Go { paired: true }
    } else {
        Gate::Wait
    }
}

fn framing(adapter: &adapter::Adapter) -> Framing<'_> {
    Framing {
        pre_send: &adapter.send.pre_send,
        submit_delay: Duration::from_millis(adapter.send.submit_delay_ms),
    }
}

/// The one line that stands in for a body sent by reference: where the
/// prompt file is — relative when it is under the agent's working directory,
/// as it is written to be — and who sent it, so the agent can tell its
/// parent's instructions from text it merely found.
fn reference_line(file: &std::path::Path, cwd: &str, who: &str) -> String {
    let at = match file.strip_prefix(cwd) {
        Ok(rel) if !cwd.is_empty() => format!("{} (relative to your working directory)", rel.display()),
        _ => file.display().to_string(),
    };
    format!(
        "Read the file {at} and carry out the instructions in it. They were sent to you through \
         seagents by {who}."
    )
}

/// The inverse of [`reference_line`]: the prompt file a stand-in line
/// names, as written there (relative to the agent's working directory, or
/// absolute) — for `read`, which can show the words the agent was only told
/// where to find.
pub(crate) fn referenced_file(text: &str) -> Option<&str> {
    let at = text
        .strip_prefix("Read the file ")?
        .split_once(" and carry out the instructions in it. They were sent to you through seagents by ")?
        .0;
    Some(at.strip_suffix(" (relative to your working directory)").unwrap_or(at))
}

#[allow(clippy::too_many_arguments)]
fn send(
    shared: &Shared,
    me: &str,
    who: &str,
    body: Owned,
    submit: bool,
    now: bool,
    answer: bool,
    timeout: Duration,
) -> Result<Reply, Reply> {
    let w = &shared.wiring;
    let adapter = adapter::for_command(&w.argv0).map_err(|e| Reply::refused(Refusal::Failed, format!("agent adapter: {e}")))?;
    let detectable = adapter::agent_id(&w.argv0).is_some();
    let deadline = Instant::now() + timeout;
    loop {
        if shared.exited() {
            return Err(Reply::refused(Refusal::Exit, format!("{me} exited")));
        }
        let mut last = shared.last_submit.lock().unwrap_or_else(|e| e.into_inner());
        let agent = registry::read_agent(&w.files);
        let state = registry::read_state(&w.files);
        let in_turn = shared.ledger.in_turn(&agent, state.as_ref(), &w.files);
        let pending = last.as_ref().is_some_and(|s| s.pending(shared.ledger.turns, agent.prompted_at));
        match gate(state.as_ref(), in_turn, pending, detectable, now, answer) {
            Gate::Go { paired } => {
                let gated = state.as_ref().is_some_and(|s| s.state == "blocked");
                // A paste lands after whatever is in the input box. Keys
                // are the caller's own business (a menu, an Esc).
                if !gated && matches!(body, Paste(_)) {
                    make_room(shared, &adapter.send, me)?;
                }
                let stops = agent.stops;
                let line = match &body {
                    Paste(text) if adapter.send.by_reference(text) => {
                        let meta = registry::read_meta(&w.files)
                            .ok_or_else(|| Reply::refused(Refusal::Failed, "this agent's .meta is gone"))?;
                        let file = registry::write_prompt(&meta, stops, text)
                            .map_err(|e| Reply::refused(Refusal::Failed, format!("writing the prompt file: {e}")))?;
                        Some(reference_line(&file, &meta.cwd, who))
                    }
                    _ => None,
                };
                let body = line.as_deref().map_or_else(|| body.body(), Body::Paste);
                let ms = crate::clock::unix_now_ms();
                inject::deliver(&w.writer, &w.modes, body, submit, framing(&adapter))
                    .map_err(|e| Reply::refused(Refusal::Failed, e))?;
                if let (Body::Paste(text), true) = (body, submit && !gated) {
                    *shared.ours.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.to_string());
                }
                // Into a gate: reply once `.state` has caught up, so a
                // `wait` called the moment this returns doesn't read the
                // gate that was just answered.
                w.wake.input();
                if submit && !gated {
                    // A prompt: from here its hooks owe a report (`hook::health`).
                    registry::note_engaged(&w.files);
                }
                if gated {
                    w.wake.wait_unblocked(ANSWER_SETTLE);
                } else if submit && shared.ledger.turns == Turns::Hook {
                    // And a prompt: reply once the agent has announced it
                    // (≤ LANDING_GRACE), so a `wait --after <stops>` called
                    // the moment this returns sees this turn open — not the
                    // ledger as the last turn left it, which after an Esc
                    // reads "prompted, no longer running": `interrupted`
                    // (found live on Codex, 2026-09-29).
                    let landed = Instant::now() + LANDING_GRACE;
                    while registry::read_agent(&w.files).prompted_at.is_none_or(|p| p < ms)
                        && Instant::now() < landed
                        && !shared.exited()
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                if submit {
                    *last = Some(Submitted { at: Instant::now(), ms });
                }
                return Ok(Reply {
                    ok: true,
                    stops: Some(stops),
                    paired: Some(paired),
                    ..Reply::default()
                });
            }
            Gate::Blocked => {
                let rule = state.and_then(|s| s.rule);
                let mut r = Reply::refused(
                    Refusal::Blocked,
                    format!(
                        "{me} is blocked at a gate ({}); a human answers this",
                        rule.as_deref().unwrap_or("unknown rule")
                    ),
                );
                r.rule = rule;
                return Err(r);
            }
            Gate::Unsupported => {
                return Err(Reply::refused(
                    Refusal::Unsupported,
                    format!("nothing can tell when {me} ({}) is idle; send --now", w.argv0),
                ))
            }
            Gate::Wait if Instant::now() >= deadline => {
                return Err(Reply::refused(
                    Refusal::BusyTimeout,
                    format!("{me} was still busy after {} s; nothing was sent", timeout.as_secs()),
                ))
            }
            Gate::Wait => {}
        }
        drop(last);
        thread::sleep(POLL);
    }
}

/// How often, and at most how long, [`make_room`] watches the tee for the
/// agent's repaint after a key.
const REPAINT_POLL: Duration = Duration::from_millis(20);
const REPAINT_WAIT: Duration = Duration::from_secs(1);

/// What the agent's input box holds now (`detect::input_box`), rendered
/// from the tee at the terminal's live size; None: nothing to go by.
fn input_box(w: &Wiring, glyph: &str) -> Option<String> {
    let size = crate::procfs::live_pty_size(w.anchor_pid)
        .or_else(|| registry::read_meta(&w.files).and_then(|m| Some((m.rows?, m.cols?))));
    let (rows, cols) = size.unwrap_or((24, 80));
    let data = crate::status::read_tail(&w.files.raw, 1024 * 1024)?;
    crate::detect::input_box(&data, rows, cols, glyph)
}

/// The same words, however the box wrapped and indented them.
fn same_text(a: &str, b: &str) -> bool {
    a.chars().filter(|c| !c.is_whitespace()).eq(b.chars().filter(|c| !c.is_whitespace()))
}

/// Make sure a paste starts a prompt of its own: the input box must be
/// empty, or hold exactly the last prompt seagents submitted — one the agent
/// put back after a turn dropped before its first response (Claude, §6) —
/// which is cleared with the adapter's `input_clear`, pressed until the box
/// reads empty. Anything else is someone's draft: refused, `--now` or not —
/// the human is typing. An adapter with no `input_glyph`, or a screen that
/// shows no box with the cursor in it, goes ahead as before.
fn make_room(shared: &Shared, send: &adapter::Send, me: &str) -> Result<(), Reply> {
    if send.input_glyph.is_empty() {
        return Ok(());
    }
    let w = &shared.wiring;
    let Some(text) = input_box(w, &send.input_glyph).filter(|t| !t.is_empty()) else {
        return Ok(());
    };
    let ours = shared.ours.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
    let key = inject::decode_keys(&send.input_clear).map_err(|e| Reply::refused(Refusal::Failed, e))?;
    let plain = Framing { pre_send: "", submit_delay: Duration::ZERO };
    // Two presses per line (its text, then the newline before it), and a
    // few to spare; a box that stops shrinking is given up on.
    let mut left = text;
    let mut stalled = 0;
    for _ in 0..2 * left.lines().count() + 4 {
        let before = std::fs::metadata(&w.files.raw).map(|m| m.len()).ok();
        inject::deliver(&w.writer, &w.modes, Body::Keys(&key), false, plain).map_err(|e| Reply::refused(Refusal::Failed, e))?;
        let deadline = Instant::now() + REPAINT_WAIT;
        while std::fs::metadata(&w.files.raw).map(|m| m.len()).ok() == before && Instant::now() < deadline {
            thread::sleep(REPAINT_POLL);
        }
        // The repaint may come in more than one write.
        thread::sleep(REPAINT_POLL * 3);
        match input_box(w, &send.input_glyph) {
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

/// Ask the agent to exit with its adapter's exit sequence (as a `--now`
/// prompt), then SIGTERM its process group, then SIGKILL, a grace period
/// apart — straight to the signals with `force`, with no exit sequence, or
/// when the agent is blocked: typed into a permission menu, the sequence's
/// Enter would pick an answer. Replies once the agent is gone.
fn stop(shared: &Shared, force: bool) -> Result<Reply, Reply> {
    let w = &shared.wiring;
    let gone = Ok(Reply { ok: true, ..Reply::default() });
    if shared.exited() {
        return gone;
    }
    let adapter = adapter::for_command(&w.argv0).unwrap_or_default();
    let blocked = registry::read_state(&w.files).is_some_and(|s| s.state == "blocked");
    if !force && !blocked && !adapter.send.exit_sequence.is_empty() {
        let _serial = shared.last_submit.lock().unwrap_or_else(|e| e.into_inner());
        // Typed after a human's draft the sequence would be submitted as
        // part of it — a prompt, not an exit: straight to the signals then.
        let me = registry::read_meta(&w.files).map(|m| m.tag).unwrap_or_default();
        let body = Body::Paste(&adapter.send.exit_sequence);
        if make_room(shared, &adapter.send, &me).is_ok()
            && inject::deliver(&w.writer, &w.modes, body, true, framing(&adapter)).is_ok()
            && shared.wait_exit(STOP_GRACE)
        {
            return gone;
        }
    }
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        signal_agent(w, sig);
        if shared.wait_exit(STOP_GRACE) {
            return gone;
        }
    }
    Err(Reply::refused(Refusal::Failed, "the agent is still running after SIGKILL"))
}

/// Signal the agent's process group — the PTY's foreground group, which
/// `__lead` hands the agent — continuing it too, in case it is stopped.
/// SIGKILL also takes the anchor, which is what the wrapper waits on.
fn signal_agent(w: &Wiring, sig: libc::c_int) {
    let fg = unsafe { libc::tcgetpgrp(w.master_fd) };
    let anchor = w.anchor_pid as libc::pid_t;
    let group = if fg > 0 { fg } else { anchor };
    unsafe {
        libc::kill(-group, sig);
        libc::kill(-group, libc::SIGCONT);
        if sig == libc::SIGKILL {
            libc::kill(anchor, sig);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// planner → reviewer → fixer; planner → tester; stranger.
    fn tree() -> HashMap<&'static str, Option<&'static str>> {
        [
            ("planner", None),
            ("reviewer", Some("planner")),
            ("fixer", Some("reviewer")),
            ("tester", Some("planner")),
            ("stranger", None),
        ]
        .into()
    }

    fn may(caller: Option<&str>, target: &str, answer: bool) -> bool {
        let t = tree();
        authorize(caller, target, answer, |x| t.get(x).copied().flatten().map(str::to_string)).is_ok()
    }

    #[test]
    fn the_lineage_rule_truth_table() {
        // A human drives anyone, and answers gates.
        assert!(may(None, "fixer", false));
        assert!(may(None, "planner", true));
        // An agent drives its child and its grandchild…
        assert!(may(Some("planner"), "reviewer", false));
        assert!(may(Some("planner"), "fixer", false));
        // …but not itself, its parent, a sibling or a stranger.
        assert!(!may(Some("reviewer"), "reviewer", false));
        assert!(!may(Some("fixer"), "reviewer", false));
        assert!(!may(Some("fixer"), "planner", false));
        assert!(!may(Some("tester"), "reviewer", false));
        assert!(!may(Some("reviewer"), "tester", false));
        assert!(!may(Some("planner"), "stranger", false));
        assert!(!may(Some("stranger"), "planner", false));
        // Never answers a gate, even its own child's.
        let e = authorize(Some("planner"), "reviewer", true, |_| Some("planner".into())).unwrap_err();
        assert!(e.contains("only a human"), "{e}");
        // A parent cycle ends the walk instead of hanging it.
        assert!(authorize(Some("x"), "a", false, |t| Some(if t == "a" { "b" } else { "a" }.into())).is_err());
    }

    fn st(word: &str) -> StateRecord {
        StateRecord { state: word.into(), since: 0, seen: true, rule: None, screen: None }
    }

    #[test]
    fn a_send_waits_for_an_idle_agent_and_refuses_a_gate() {
        let g = |s: Option<&str>, in_turn, pending, detectable, now, answer| {
            gate(s.map(st).as_ref(), in_turn, pending, detectable, now, answer)
        };
        assert_eq!(g(Some("idle"), false, false, true, false, false), Gate::Go { paired: true });
        assert_eq!(g(Some("idle"), true, false, true, false, false), Gate::Wait, "in a turn");
        assert_eq!(g(Some("idle"), false, true, true, false, false), Gate::Wait, "ours still landing");
        assert_eq!(g(Some("working"), false, false, true, false, false), Gate::Wait);
        assert_eq!(g(None, false, false, true, false, false), Gate::Wait, "not seen yet");
        assert_eq!(g(Some("working"), true, false, true, true, false), Gate::Go { paired: false }, "--now");
        assert_eq!(g(Some("blocked"), true, false, true, false, false), Gate::Blocked);
        assert_eq!(g(Some("blocked"), true, false, true, true, false), Gate::Blocked, "--now doesn't pass a gate");
        assert_eq!(g(Some("blocked"), true, false, true, false, true), Gate::Go { paired: false });
        assert_eq!(g(Some("unknown"), false, false, false, false, false), Gate::Unsupported);
        assert_eq!(g(Some("unknown"), false, false, false, true, false), Gate::Go { paired: false });
    }

    #[test]
    fn a_body_by_reference_is_one_line_naming_the_file_and_the_sender() {
        let line = reference_line(
            std::path::Path::new("/w/proj/.seagents/prompts/7-2-beef.md"),
            "/w/proj",
            "@planner",
        );
        assert_eq!(
            line,
            "Read the file .seagents/prompts/7-2-beef.md (relative to your working directory) and carry \
             out the instructions in it. They were sent to you through seagents by @planner."
        );
        assert!(!line.contains('\n'));
        let abs = reference_line(std::path::Path::new("/elsewhere/p.md"), "/w/proj", "the user");
        assert!(abs.starts_with("Read the file /elsewhere/p.md and") && abs.ends_with("by the user."), "{abs}");
        assert_eq!(referenced_file(&line), Some(".seagents/prompts/7-2-beef.md"));
        assert_eq!(referenced_file(&abs), Some("/elsewhere/p.md"));
        assert_eq!(referenced_file("Read the file x.md please"), None);
    }

    #[test]
    fn a_prompt_put_back_is_ours_however_the_box_wrapped_it() {
        let line = "Read the file .seagents/prompts/7-2-beef.md (relative to your working directory) and carry out";
        assert!(same_text(line, "Read the file .seagents/prompts/7-2-beef.md (relative to your working\ndirectory) and carry out"));
        assert!(same_text(line, "Read the file .seagents/prompts/7-2-beef.md (relative to your wor\nking directory) and carry out"));
        assert!(same_text("one\ntwo", "one\ntwo"));
        assert!(!same_text("Reply with exactly: A", "Reply with exactly: AB"));
        assert!(!same_text("Reply with exactly: A", "my own draft"));
    }

    #[test]
    fn a_submission_lands_when_the_prompt_hook_fires_or_the_grace_ends() {
        let s = Submitted { at: Instant::now(), ms: 1_000 };
        assert!(s.pending(Turns::Hook, None));
        assert!(s.pending(Turns::Hook, Some(999)), "an older prompt");
        assert!(!s.pending(Turns::Hook, Some(1_000)));
        assert!(s.pending(Turns::Screen, Some(5_000)), "no hook to confirm it");
        let old = Submitted { at: Instant::now() - LANDING_GRACE, ms: 1_000 };
        assert!(!old.pending(Turns::Hook, None));
        assert!(!old.pending(Turns::Screen, None));
    }
}
