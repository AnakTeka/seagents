//! Milestone 4: the floating HUD.
//!
//! A ratatui panel meant to run in an i3 floating window. It polls the same
//! session data as `seagents status`, colour-codes each agent by state, and on
//! Enter or left-click focuses that agent's i3 window (milestone 3). Spawned
//! agents sit under the agent that spawned them, and the selected row can be
//! sent a prompt or stopped through the same control socket `seagents send` /
//! `stop` use.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::send::{Mode, Reply, Request};
use crate::status::{self, Session, State};

const REFRESH: Duration = Duration::from_millis(750);

pub fn run() -> i32 {
    // Long-lived surface → good place to opportunistically refresh detection
    // manifests (daily-throttled, background, silent).
    crate::detect::spawn_manifest_update();
    if enable_raw_mode().is_err() {
        eprintln!("seagents: the HUD needs a terminal (try `seagents status` instead)");
        return 1;
    }
    let mut stdout = io::stdout();
    // Bracketed paste so a prompt pasted into the send line arrives as one
    // event: unbracketed, its first newline would be an Enter and submit the
    // first line alone.
    let _ = execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste);
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(_) => {
            let _ = disable_raw_mode();
            return 1;
        }
    };

    let code = event_loop(&mut terminal);

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = terminal.show_cursor();
    code
}

/// What names a row. The directory says *where* an agent is; the conversation
/// title — what Claude generated for the session, the line `claude --resume`
/// lists — says *what it is doing*. Different questions, so the default answers
/// both and `t` cycles down to one when the window is too narrow to want them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Label {
    /// Directory and title side by side (default).
    Both,
    Title,
    Dir,
}

impl Label {
    fn word(self) -> &'static str {
        match self {
            Label::Both => "both",
            Label::Title => "title",
            Label::Dir => "dir",
        }
    }

    fn parse(s: &str) -> Option<Label> {
        match s {
            "both" => Some(Label::Both),
            "title" => Some(Label::Title),
            "dir" => Some(Label::Dir),
            _ => None,
        }
    }

    /// Cycle order for the `t` key: widest view first, narrowing.
    fn next(self) -> Label {
        match self {
            Label::Both => Label::Title,
            Label::Title => Label::Dir,
            Label::Dir => Label::Both,
        }
    }
}

/// The row's name in its directory column: the agent's tag where somebody
/// chose it (`@reviewer`), else the directory. A tag `run` minted is the
/// directory plus four hex digits of noise — it says nothing the directory
/// doesn't, and every hand-started agent has one — while a chosen tag is the
/// name the agent is addressed by, and a spawned child usually runs in its
/// parent's directory, where the directory would name every row of the fleet
/// alike. `@` because that is how a target is spelled on the command line.
fn name_of(s: &Session) -> String {
    if s.tag.is_empty() || crate::registry::minted_from(&s.tag, Path::new(&s.cwd)) {
        s.dir.clone()
    } else {
        format!("@{}", s.tag)
    }
}

/// How to refer to a row in the footer — its name column, which is short and
/// always there, except in title mode where the title *is* the row's name.
fn label_of(s: &Session, labels: Label) -> String {
    match labels {
        Label::Title if !s.title.is_empty() => s.title.clone(),
        _ => name_of(s),
    }
}

/// How the socket client addresses a row: its tag, or its child pid for a
/// session whose wrapper predates tags (the one resolver takes either).
fn target_of(s: &Session) -> String {
    if s.tag.is_empty() {
        s.pid.to_string()
    } else {
        s.tag.clone()
    }
}

/// One line of the list: a session and how deep it sits in the lineage tree
/// on screen (0 for a top-level row).
struct Row {
    s: Session,
    level: usize,
}

/// A keystroke-owning footer: while one is up, keys edit it instead of
/// driving the list. The target is fixed when it opens — rows move under
/// the urgency and recent sorts, and a prompt typed for one agent must not
/// land in whichever row slid under the cursor meanwhile.
enum Input {
    /// Typing a prompt for `target` (`name` is how the footer shows it).
    Prompt { target: String, name: String, text: String },
    /// Asking before stopping `target`.
    ConfirmStop { target: String, name: String },
}

struct App {
    rows: Vec<Row>,
    list: ListState,
    sort: status::Sort,
    labels: Label,
    /// The `?` key list is up. It owns the keyboard while open, so a keystroke
    /// aimed at it can't quietly drive the list underneath.
    help: bool,
    input: Option<Input>,
    message: String,
    /// Outcomes of sends/stops, which run off the event loop (a send is held
    /// until its agent is idle — up to minutes) and report here when done.
    done_tx: mpsc::Sender<String>,
    done_rx: mpsc::Receiver<String>,
    /// Screen y of the first agent row, cached each frame for mouse hit-testing.
    rows_top: u16,
    /// con_id of the HUD's own window, so previews can return focus to it.
    /// Captured lazily on the first preview, when the HUD is definitely focused.
    hud_con_id: Option<u64>,
}

impl App {
    fn new(sessions: Vec<Session>, sort: status::Sort, labels: Label, message: String) -> App {
        let (done_tx, done_rx) = mpsc::channel();
        let mut app = App {
            rows: lineage(sessions),
            list: ListState::default(),
            sort,
            labels,
            help: false,
            input: None,
            message,
            done_tx,
            done_rx,
            rows_top: 1,
            hud_con_id: None,
        };
        if !app.rows.is_empty() {
            app.list.select(Some(0));
        }
        app
    }

    fn selected(&self) -> Option<&Session> {
        self.list.selected().and_then(|i| self.rows.get(i)).map(|r| &r.s)
    }
}

/// Order a sorted list as a lineage forest: every spawned agent directly
/// under its parent, indented one level per generation. An agent whose parent
/// is not live (or who has none) is a top-level row.
///
/// The sort still decides, one level at a time: a family is placed where its
/// **highest-sorted member** would be, and siblings likewise. So under
/// urgency a child blocked on a permission prompt lifts its whole family to
/// the top, rather than hiding under an idle parent at the bottom — the HUD's
/// one job is to show who needs you — and the families stay contiguous in
/// every sort.
fn lineage(sorted: Vec<Session>) -> Vec<Row> {
    let n = sorted.len();
    let parent: Vec<Option<usize>> = {
        let at: HashMap<&str, usize> = sorted
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.tag.is_empty())
            .map(|(i, s)| (s.tag.as_str(), i))
            .collect();
        sorted
            .iter()
            .enumerate()
            .map(|(i, s)| {
                s.parent
                    .as_deref()
                    .and_then(|p| at.get(p).copied())
                    .filter(|&p| p != i)
            })
            .collect()
    };
    let mut children = vec![Vec::new(); n];
    for (i, p) in parent.iter().enumerate() {
        if let Some(p) = *p {
            children[p].push(i);
        }
    }

    // Each row is claimed by exactly one tree. Rows no root reaches — only a
    // parent cycle, which tags make near-impossible but a hand-edited `.meta`
    // could make — become roots themselves rather than vanishing.
    struct Forest {
        children: Vec<Vec<usize>>,
        claimed: Vec<bool>,
        /// Tree children, once claimed.
        kids: Vec<Vec<usize>>,
        /// The best sort position anywhere in the subtree.
        key: Vec<usize>,
    }
    impl Forest {
        fn claim(&mut self, i: usize) -> usize {
            self.claimed[i] = true;
            let mut key = i;
            for c in self.children[i].clone() {
                if !self.claimed[c] {
                    self.kids[i].push(c);
                    key = key.min(self.claim(c));
                }
            }
            self.key[i] = key;
            key
        }

        fn emit(&self, i: usize, level: usize, out: &mut Vec<(usize, usize)>) {
            out.push((i, level));
            let mut kids = self.kids[i].clone();
            kids.sort_by_key(|&c| self.key[c]);
            for c in kids {
                self.emit(c, level + 1, out);
            }
        }
    }
    let mut forest = Forest {
        children,
        claimed: vec![false; n],
        kids: vec![Vec::new(); n],
        key: (0..n).collect(),
    };
    let mut roots = Vec::new();
    for i in (0..n).filter(|&i| parent[i].is_none()) {
        roots.push(i);
        forest.claim(i);
    }
    for i in 0..n {
        if !forest.claimed[i] {
            roots.push(i);
            forest.claim(i);
        }
    }
    roots.sort_by_key(|&r| forest.key[r]);
    let mut order = Vec::with_capacity(n);
    for r in roots {
        forest.emit(r, 0, &mut order);
    }

    let mut slots: Vec<Option<Session>> = sorted.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|(i, level)| slots[i].take().map(|s| Row { s, level }))
        .collect()
}

/// A mouse report torn across a read boundary surfaces as a bare Esc key
/// (seen live during an i3 $mod+drag of the HUD window). A torn sequence's
/// tail arrives as garbage *key* events right behind the Esc; pending
/// mouse-motion events are unrelated and must not veto the Esc (motion
/// streams continuously under mode 1003, so "any pending input" would
/// suppress Esc whenever the pointer is moving). True when the Esc was torn.
fn torn_escape() -> bool {
    let deadline = Instant::now() + Duration::from_millis(50);
    while event::poll(Duration::from_millis(25)).unwrap_or(false) {
        match event::read() {
            Ok(Event::Key(_)) => return true,
            Ok(_) => {} // motion/resize: irrelevant, drop
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
    false
}

fn event_loop<B: Backend>(terminal: &mut Terminal<B>) -> i32 {
    // Debug aid: set SEAGENTS_HUD_LOG=<path> to append every input event the
    // HUD receives (and why it exits) — the terminal vanishes with the process,
    // so this is the only way to see what happened after the fact.
    let mut log = std::env::var_os("SEAGENTS_HUD_LOG").and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    });
    macro_rules! hud_log {
        ($($arg:tt)*) => {
            if let Some(f) = log.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, $($arg)*);
            }
        };
    }
    let sort = load_sort();
    let mut app = App::new(
        status::collect(sort),
        sort,
        load_labels(),
        // Surface detection-rule update problems where they'll be seen; the
        // help line is one keypress away from rediscovery.
        crate::detect::manifest_health_warning()
            .map(|w| format!("⚠ {w}"))
            .unwrap_or_else(help_line),
    );
    let mut last_refresh = Instant::now();

    loop {
        while let Ok(outcome) = app.done_rx.try_recv() {
            app.message = outcome;
        }
        if terminal.draw(|f| draw(f, &mut app)).is_err() {
            return 1;
        }

        let timeout = REFRESH.saturating_sub(last_refresh.elapsed());
        if event::poll(timeout).unwrap_or(false) {
            let ev = event::read();
            // Never log what is typed into the send line: prompt text stays
            // out of every log (HANDOFF §9).
            match (&ev, app.input.is_some()) {
                (Ok(Event::Key(_) | Event::Paste(_)), true) => hud_log!("input-line event"),
                _ => hud_log!("{ev:?}"),
            }
            match ev {
                // The send line / stop confirmation owns the keyboard, like
                // the overlay: `q` there is a letter, not quit.
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press && app.input.is_some() => {
                    input_key(&mut app, k)
                }
                Ok(Event::Paste(text)) => {
                    if let Some(Input::Prompt { text: line, .. }) = app.input.as_mut() {
                        line.push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                    }
                }
                // While the key list is up it owns the keyboard: anything but
                // `q` dismisses it instead of acting, so a keystroke aimed at
                // the overlay can't secretly navigate the list underneath.
                Ok(Event::Key(k))
                    if k.kind == KeyEventKind::Press
                        && app.help
                        && k.code != KeyCode::Char('q') =>
                {
                    app.help = false;
                    app.message = help_line();
                }
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') => {
                        hud_log!("exit: quit key {:?}", k);
                        return 0;
                    }
                    KeyCode::Esc => {
                        if torn_escape() {
                            hud_log!("ignored Esc: torn escape sequence");
                        } else {
                            hud_log!("exit: quit key {:?}", k);
                            return 0;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => move_selection(&mut app, 1),
                    KeyCode::Up | KeyCode::Char('k') => move_selection(&mut app, -1),
                    // vim convention: g = first row, G = last row (with the
                    // same preview-on-move behavior as j/k).
                    KeyCode::Char('g') => jump_selection(&mut app, 0),
                    KeyCode::Char('G') => {
                        let last = app.rows.len().saturating_sub(1);
                        jump_selection(&mut app, last);
                    }
                    KeyCode::Enter => focus_selected(&mut app),
                    KeyCode::Char('p') => open_prompt(&mut app),
                    KeyCode::Char('x') => {
                        if let Some(s) = app.selected() {
                            app.input = Some(Input::ConfirmStop {
                                target: target_of(s),
                                name: name_of(s),
                            });
                        }
                    }
                    KeyCode::Char('s') => {
                        app.sort = app.sort.next();
                        save_sort(app.sort);
                        refresh(&mut app);
                        last_refresh = Instant::now();
                        app.message = format!("sort: {} · s to cycle", app.sort.word());
                    }
                    KeyCode::Char('?') => {
                        app.help = true;
                        app.message = "any key closes · q quits".to_string();
                    }
                    KeyCode::Char('t') => {
                        app.labels = app.labels.next();
                        save_labels(app.labels);
                        app.message = format!("columns: {} · t to cycle", app.labels.word());
                    }
                    KeyCode::Char('r') => {
                        refresh(&mut app);
                        last_refresh = Instant::now();
                    }
                    _ => {}
                },
                // The mouse drives the list only; with a footer open, a click
                // would move the selection away from the row being addressed.
                Ok(Event::Mouse(m)) if app.input.is_none() => match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(idx) = row_at(&app, m.row) {
                            app.list.select(Some(idx));
                            focus_selected(&mut app);
                        }
                    }
                    MouseEventKind::ScrollDown => move_selection(&mut app, 1),
                    MouseEventKind::ScrollUp => move_selection(&mut app, -1),
                    _ => {}
                },
                _ => {}
            }
        }

        if last_refresh.elapsed() >= REFRESH {
            refresh(&mut app);
            // An open HUD keeps the reboot snapshot fresh even for agents
            // whose wrappers predate it (they never call it themselves).
            crate::snapshot::maybe_capture_async();
            last_refresh = Instant::now();
        }
    }
}

/// Open the send line for the selected row. A blocked row is turned away
/// here rather than after the prompt is typed: the wrapper would refuse it
/// anyway (only the window answers a gate — Enter jumps there), and losing a
/// typed prompt to a refusal is worse than not starting one. The wrapper
/// still decides; this only spares the typing.
fn open_prompt(app: &mut App) {
    let Some(s) = app.selected() else {
        return;
    };
    let name = name_of(s);
    if s.state == State::Blocked {
        app.message = format!("{name} is blocked at a prompt · enter to answer it there");
        return;
    }
    app.input = Some(Input::Prompt {
        target: target_of(s),
        name,
        text: String::new(),
    });
}

/// A key while the footer is an input: edit the prompt, or answer y/n.
fn input_key(app: &mut App, k: KeyEvent) {
    let Some(input) = app.input.take() else {
        return;
    };
    match input {
        Input::Prompt { target, name, mut text } => match k.code {
            KeyCode::Enter if text.trim().is_empty() => app.message = "nothing to send".into(),
            KeyCode::Enter => {
                app.message = format!("sending to {name} · waits until it is idle");
                dispatch(app, name, target, Request::Send {
                    text,
                    mode: Mode::Paste,
                    submit: true,
                    now: false,
                    answer_permission: false,
                    timeout_ms: Some(crate::send::DEFAULT_TIMEOUT.as_millis() as u64),
                });
            }
            KeyCode::Esc if !torn_escape() => app.message = help_line(),
            KeyCode::Backspace => {
                text.pop();
                app.input = Some(Input::Prompt { target, name, text });
            }
            // Ctrl+U clears the line, as in a shell.
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                app.input = Some(Input::Prompt { target, name, text: String::new() });
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                text.push(c);
                app.input = Some(Input::Prompt { target, name, text });
            }
            _ => app.input = Some(Input::Prompt { target, name, text }),
        },
        Input::ConfirmStop { target, name } => {
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                app.message = format!("stopping {name}…");
                dispatch(app, name, target, Request::Stop { force: false });
            } else {
                app.message = format!("left {name} running");
            }
        }
    }
}

/// Carry a request to a wrapper off the event loop — a send is held until
/// its agent is idle, a stop takes its grace periods — through the same
/// client `seagents send`/`stop` use, and post the outcome to the footer.
fn dispatch(app: &App, name: String, target: String, request: Request) {
    let done = app.done_tx.clone();
    std::thread::spawn(move || {
        let bound = match &request {
            Request::Send { timeout_ms, .. } => Duration::from_millis(timeout_ms.unwrap_or(0)),
            Request::Stop { .. } => Duration::ZERO,
        };
        let reply = crate::send::ask(&target, &request, bound);
        let _ = done.send(outcome(&request, &name, reply));
    });
}

/// The footer line for a finished send/stop: what happened, or the wrapper's
/// own words for why not (they name the gate, the lineage rule, …).
fn outcome(request: &Request, name: &str, reply: Result<Reply, String>) -> String {
    match (request, reply) {
        (Request::Send { .. }, Ok(r)) if r.ok => {
            // The baseline `wait --after` takes: the turn it opened ends at
            // stops + 1.
            format!("sent to {name} · stops {}", r.stops.unwrap_or(0))
        }
        (Request::Stop { .. }, Ok(r)) if r.ok => format!("stopped {name}"),
        (_, Ok(r)) => format!("{name}: {}", r.error.unwrap_or_else(|| "refused".into())),
        (_, Err(e)) => format!("{name}: {e}"),
    }
}

fn refresh(app: &mut App) {
    // Follow the selected *session*, not its row index — under the urgency and
    // recent sorts, rows move as states change.
    let selected_id = app.selected().map(|s| s.id);
    app.rows = lineage(status::collect(app.sort));
    let followed = selected_id.and_then(|id| app.rows.iter().position(|r| r.s.id == id));
    match followed {
        Some(i) => app.list.select(Some(i)),
        None if app.rows.is_empty() => app.list.select(None),
        // Selected session ended: clamp the old index into range.
        None => {
            let i = app.list.selected().unwrap_or(0).min(app.rows.len() - 1);
            app.list.select(Some(i));
        }
    }
}

/// The footer hint. Deliberately short: a 47-column HUD window clips this
/// line, so the full list lives behind `?` and this only has to advertise it.
fn help_line() -> String {
    "? keys · j/k preview · enter jump".to_string()
}

/// Every binding, for the `?` overlay — the one place the HUD can state them
/// in full, since the footer can't.
fn help_rows() -> &'static [(&'static str, &'static str)] {
    &[
        ("j/k ↑↓", "navigate + preview"),
        ("g/G", "first / last row"),
        ("enter", "jump to the agent"),
        ("click", "jump to that row"),
        ("p", "send the agent a prompt"),
        ("x", "stop the agent (asks y/n)"),
        ("s", "sort: urgency/dir/recent"),
        ("t", "columns: both/title/dir"),
        ("r", "refresh now"),
        ("?", "this list"),
        ("q / esc", "quit"),
    ]
}

/// Width of the key column, sized to the widest key so the descriptions line
/// up. Counted in chars: the arrow glyphs are multi-byte.
fn help_key_width() -> usize {
    help_rows()
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0)
}

/// The `?` overlay. `height` is the area it gets: a HUD window too short for
/// every binding would otherwise drop the last ones — `q` included — with no
/// sign that it had, so a clipped panel says so in its bottom border, the same
/// idiom the agent list uses when it scrolls.
fn help_panel(height: u16) -> Paragraph<'static> {
    use ratatui::widgets::block::{Position, Title};
    let key_w = help_key_width();
    let lines: Vec<Line> = help_rows()
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(
                    format!("{key:<key_w$}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {what}"), Style::default().fg(Color::Gray)),
            ])
        })
        .collect();
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(" keys ")
        .title_style(Style::default().fg(Color::DarkGray));
    let hidden = help_rows()
        .len()
        .saturating_sub(height.saturating_sub(2) as usize);
    if hidden > 0 {
        block = block.title(
            Title::from(Span::styled(
                format!(" ▼ {hidden} more "),
                Style::default().fg(Color::DarkGray),
            ))
            .alignment(Alignment::Right)
            .position(Position::Bottom),
        );
    }
    Paragraph::new(lines).block(block)
}

/// The HUD remembers its view across launches — one word per preference in a
/// state file, so a setting survives the window being closed and reopened.
fn read_pref(name: &str) -> Option<String> {
    let path = crate::vendor::herdr_detect::config::state_dir().join(name);
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn write_pref(name: &str, value: &str) {
    let dir = crate::vendor::herdr_detect::config::state_dir();
    let _ = crate::fsutil::private_dir(&dir);
    let _ = crate::fsutil::write_atomic(&dir.join(name), value.as_bytes());
}

fn load_sort() -> status::Sort {
    read_pref("hud-sort")
        .and_then(|s| status::Sort::parse(&s))
        .unwrap_or(status::Sort::Urgency)
}

fn save_sort(sort: status::Sort) {
    write_pref("hud-sort", sort.word());
}

fn load_labels() -> Label {
    read_pref("hud-label")
        .and_then(|s| Label::parse(&s))
        .unwrap_or(Label::Both)
}

fn save_labels(labels: Label) {
    write_pref("hud-label", labels.word());
}

/// Select an absolute row (g/G), previewing like relative navigation does.
fn jump_selection(app: &mut App, idx: usize) {
    if app.rows.is_empty() {
        return;
    }
    app.list.select(Some(idx.min(app.rows.len() - 1)));
    preview_selected(app);
}

fn move_selection(app: &mut App, delta: isize) {
    let n = app.rows.len();
    if n == 0 {
        return;
    }
    let cur = app.list.selected().unwrap_or(0) as isize;
    let next = (cur + delta).rem_euclid(n as isize) as usize;
    app.list.select(Some(next));
    preview_selected(app);
}

/// Reveal the highlighted agent's window on its monitor while keeping keyboard
/// focus on the HUD, so navigation doubles as a live preview.
fn preview_selected(app: &mut App) {
    // Capture the HUD's window the first time — right now it's focused, since
    // the user is driving the HUD.
    if app.hud_con_id.is_none() {
        app.hud_con_id = crate::window::focused_con_id();
    }
    let Some(s) = app.selected() else {
        return;
    };
    // Looking at it counts as seeing it: done → idle.
    status::ack_session(s.id);
    let name = label_of(s, app.labels);
    match crate::window::preview_window(s.pid, app.hud_con_id) {
        Ok(()) => app.message = format!("previewing {name} · enter to jump"),
        Err(e) => app.message = e,
    }
}

fn focus_selected(app: &mut App) {
    let Some(s) = app.selected() else {
        return;
    };
    status::ack_session(s.id);
    let name = label_of(s, app.labels);
    match crate::window::focus_window(s.pid) {
        Ok(()) => app.message = format!("focused {name} ({})", s.cmd),
        Err(e) => app.message = e,
    }
}

/// Map a screen row to a session index, accounting for the border above the
/// first agent row and the list's scroll offset (a scrolled list's first
/// visible row is not session 0).
fn row_at(app: &App, row: u16) -> Option<usize> {
    let idx = row.checked_sub(app.rows_top)? as usize + app.list.offset();
    (idx < app.rows.len()).then_some(idx)
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
    let body = chunks[0];
    // Rows start one line below the block's top border.
    app.rows_top = body.y + 1;

    if app.rows.is_empty() {
        let empty = Paragraph::new("no agents tracked; launch one with `seagents run claude`")
            .block(agent_block(app.sort, app.labels, 0, 0))
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, body);
    } else {
        // Pre-apply the same scroll clamp the List does during render, so the
        // clipped-row indicators describe *this* frame, not the previous one.
        let visible = body.height.saturating_sub(2) as usize; // minus borders
        let total = app.rows.len();
        let mut offset = app.list.offset();
        if let Some(sel) = app.list.selected() {
            if visible > 0 && sel >= offset + visible {
                offset = sel + 1 - visible;
            }
            if sel < offset {
                offset = sel;
            }
        }
        offset = offset.min(total.saturating_sub(visible.min(total)));
        *app.list.offset_mut() = offset;
        let above = offset;
        let below = total.saturating_sub(offset + visible);

        // Width a row actually gets: the block's borders take two columns, and
        // `highlight_symbol` reserves its own width on *every* row, not just
        // the selected one.
        let row_width = body
            .width
            .saturating_sub(2)
            .saturating_sub(HIGHLIGHT_SYMBOL.chars().count() as u16)
            as usize;
        let cols = Cols::measure(app.rows.iter().map(|r| (&r.s, r.level)));
        let items: Vec<ListItem> = app
            .rows
            .iter()
            .map(|r| ListItem::new(session_line(&r.s, r.level, row_width, &cols, app.labels)))
            .collect();
        let list = List::new(items)
            .block(agent_block(app.sort, app.labels, above, below))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol(HIGHLIGHT_SYMBOL);
        f.render_stateful_widget(list, body, &mut app.list);
    }

    // Drawn over the list rather than beside it: the HUD is a small floating
    // window, and there is no room for both.
    if app.help {
        f.render_widget(Clear, body);
        f.render_widget(help_panel(body.height), body);
    }

    let footer = chunks[1];
    match &app.input {
        None => f.render_widget(
            Paragraph::new(app.message.as_str()).style(Style::default().fg(Color::DarkGray)),
            footer,
        ),
        Some(input) => {
            let (line, cursor) = input_line(input, footer.width as usize);
            f.render_widget(Paragraph::new(line), footer);
            if let Some(x) = cursor {
                f.set_cursor_position((footer.x + x as u16, footer.y));
            }
        }
    }
}

/// The footer while it is an input, fitted to `width`, and the cursor column
/// for a prompt. A prompt longer than the line shows its **end** — the part
/// being typed — behind a `…`; newlines from a paste show as `↵` (they are
/// sent as typed: the line is one row, the prompt need not be).
fn input_line(input: &Input, width: usize) -> (Line<'static>, Option<usize>) {
    match input {
        Input::Prompt { name, text, .. } => {
            let head = format!("send {name} › ");
            let shown: String = text.chars().map(|c| if c == '\n' { '↵' } else { c }).collect();
            // One column is kept for the cursor after the last character.
            let room = width.saturating_sub(head.chars().count() + 1);
            let n = shown.chars().count();
            let tail = if n > room {
                let keep = room.saturating_sub(1);
                format!("…{}", shown.chars().skip(n - keep).collect::<String>())
            } else {
                shown
            };
            let cursor = head.chars().count() + tail.chars().count();
            (
                Line::from(vec![
                    Span::styled(head, Style::default().fg(Color::Cyan)),
                    Span::raw(tail),
                ]),
                Some(cursor.min(width.saturating_sub(1))),
            )
        }
        Input::ConfirmStop { name, .. } => (
            Line::from(Span::styled(
                format!("stop {name}? y/n"),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )),
            None,
        ),
    }
}

fn agent_block(
    sort: status::Sort,
    labels: Label,
    above: usize,
    below: usize,
) -> Block<'static> {
    use ratatui::widgets::block::{Position, Title};
    let dim = |text: String| Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)));
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(" seagents ")
        // Both view modes, top-right: the footer flash on `s`/`t` is gone by
        // the next message, this isn't — and in a narrow window the columns
        // mode is not always obvious from the row itself (a `both` row with no
        // room for the title looks exactly like a `dir` row).
        .title(
            Title::from(dim(format!(" {} · {} ", labels.word(), sort.word())))
                .alignment(Alignment::Right),
        );
    // Clipped-row indicators in the bottom border when the list scrolls.
    if above > 0 {
        block = block.title(
            Title::from(dim(format!(" ▲ {above} more ")))
                .alignment(Alignment::Left)
                .position(Position::Bottom),
        );
    }
    if below > 0 {
        block = block.title(
            Title::from(dim(format!(" ▼ {below} more ")))
                .alignment(Alignment::Right)
                .position(Position::Bottom),
        );
    }
    block
}

/// Rendered on every row, selected or not — ratatui reserves its width across
/// the whole list, so it comes out of the space each row has to lay out in.
const HIGHLIGHT_SYMBOL: &str = "▶ ";

/// The lineage marker in front of a row's name: nothing at the top level,
/// `└ ` under a parent, two more columns per further generation. In the name
/// column rather than at the row's start, so the state and time columns stay
/// aligned down the whole list.
fn tree_prefix(level: usize) -> String {
    match level {
        0 => String::new(),
        n => format!("{}└ ", "  ".repeat(n - 1)),
    }
}

/// Column measurements taken once per frame and shared by every row, so the
/// columns line up: sizing each row to its own contents makes the list ragged.
struct Cols {
    /// Widest duration on screen. Sized from the data because it is the one
    /// column that is never allowed to truncate.
    time_w: usize,
    /// Width of the name column when a title column sits beside it: the
    /// longest name on screen (tree prefix included), capped so one deep
    /// project name (they run to `20260612_doc_parser` here) can't eat the
    /// title's space.
    dir_w: usize,
    /// Whether any agent on screen has named its conversation. When none has,
    /// `both` draws no title column rather than a frame of blanks.
    any_title: bool,
}

impl Cols {
    const DIR_CAP: usize = 18;

    fn measure<'a>(rows: impl IntoIterator<Item = (&'a Session, usize)> + Clone) -> Cols {
        Cols {
            time_w: rows
                .clone()
                .into_iter()
                .map(|(s, _)| crate::status::humanize_secs(s.state_secs).chars().count())
                .max()
                .unwrap_or(3),
            dir_w: rows
                .clone()
                .into_iter()
                .map(|(s, level)| tree_prefix(level).chars().count() + name_of(s).chars().count())
                .max()
                .unwrap_or(4)
                .min(Cols::DIR_CAP),
            any_title: rows.into_iter().any(|(s, _)| !s.title.is_empty()),
        }
    }
}

/// One agent row, laid out to fit `width` columns.
///
/// Computed rather than fixed, because the HUD is meant to live in a small
/// floating window: the previous fixed 45-column row silently lost its last two
/// columns in a 47-column window (borders take 2, the highlight symbol 2), and
/// what got clipped was the duration — `15h35m` rendered as `15h3` and, far
/// worse, `52m` rendered as `5`, a plausible wrong number rather than an
/// obviously cut one.
///
/// So the duration is allocated **first** and never sacrificed (how long an
/// agent has been waiting on you is the question the HUD exists to answer), and
/// the rest of the row is chosen from a ladder of layouts — the first one that
/// fits `width` wins. Optional columns therefore drop widest-first as the
/// window narrows: the command (it is `claude` on nearly every row), then the
/// state word (the coloured dot already carries the state).
///
/// `labels` picks which ladder:
///
/// - **both** — name *and* title. The title yields **last**, not first: a
///   47-column HUD cannot hold four columns, and a `both` mode that dropped the
///   title there would silently be `dir` mode, which is the thing it exists not
///   to be. So the command and the state word are spent on the title before the
///   row gives up and falls back to the name alone.
/// - **title** — the title as the row's whole name (the name column's when the
///   agent has not named its conversation yet). Titles are sentences where
///   directories are words, so it asks for a wider minimum.
/// - **dir** — the original row.
///
/// The name is [`name_of`] — the directory, or a chosen `@tag` — behind the
/// row's [`tree_prefix`] at `level`, whichever column it lands in.
fn session_line(s: &Session, level: usize, width: usize, cols: &Cols, labels: Label) -> Line<'static> {
    let color = state_color(s.state);
    let time = crate::status::humanize_secs(s.state_secs);
    let prefix = tree_prefix(level);
    let name = format!("{prefix}{}", name_of(s));

    const DOT: usize = 2;
    const GAP: usize = 2;
    const CMD: usize = 6;
    /// Narrowest useful directory when it is the only name column.
    const DIR_MIN: usize = 8;
    /// Narrowest title worth drawing beside a directory. Below this the row
    /// falls back to the directory alone rather than showing three words.
    const TITLE_MIN: usize = 14;
    /// Narrowest title when it is the row's whole name.
    const TITLE_ALONE_MIN: usize = 20;
    let time_w = cols.time_w.max(time.chars().count());
    let state_w = 7;

    // The directory column is capped against the *window*, not just against
    // `DIR_CAP`: a 47-column HUD that let one long project name take 18
    // columns had no room left for a title, so `both` mode silently became
    // `dir` mode there — the one outcome it exists to avoid.
    let dir_w = cols.dir_w.min((width / 3).max(DIR_MIN));

    // The single flexible column, when no separate title column is drawn.
    let solo = match labels {
        Label::Title if !s.title.is_empty() => format!("{prefix}{}", s.title),
        _ => name.clone(),
    };
    let solo_min = match labels {
        Label::Title => TITLE_ALONE_MIN,
        _ => DIR_MIN,
    };

    // What each candidate layout costs. With a title column the directory is
    // fixed at the measured width and the title flexes; without one, the name
    // column flexes.
    let need = |state: bool, cmd: bool, title: bool| {
        DOT + if state { state_w + GAP } else { 0 }
            + if title { dir_w + GAP + TITLE_MIN } else { solo_min }
            + if cmd { 1 + CMD } else { 0 }
            + GAP
            + time_w
    };

    let mut ladder: Vec<(bool, bool, bool)> = Vec::new();
    if labels == Label::Both && cols.any_title {
        ladder.extend([(true, true, true), (true, false, true), (false, false, true)]);
    }
    ladder.extend([(true, true, false), (true, false, false), (false, false, false)]);
    let (show_state, show_cmd, show_title) = ladder
        .into_iter()
        .find(|&(state, cmd, title)| width >= need(state, cmd, title))
        .unwrap_or((false, false, false));

    let fixed = DOT
        + if show_state { state_w + GAP } else { 0 }
        + if show_title { dir_w + GAP } else { 0 }
        + if show_cmd { 1 + CMD } else { 0 }
        + GAP
        + time_w;
    let flex_w = width.saturating_sub(fixed).max(1);

    let mut spans = vec![Span::styled("● ", Style::default().fg(color))];
    if show_state {
        spans.push(Span::styled(
            format!("{:<state_w$}", s.state.word()),
            Style::default().fg(color),
        ));
        spans.push(Span::raw(" ".repeat(GAP)));
    }
    let bold = Style::default().add_modifier(Modifier::BOLD);
    if show_title {
        spans.push(Span::styled(
            format!("{:<dir_w$}", truncate(&name, dir_w)),
            bold,
        ));
    } else {
        spans.push(Span::styled(
            format!("{:<flex_w$}", truncate(&solo, flex_w)),
            bold,
        ));
    }
    if show_cmd {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{:<CMD$}", truncate(&s.cmd, CMD)),
            Style::default().fg(Color::Gray),
        ));
    }
    if show_title {
        spans.push(Span::raw(" ".repeat(GAP)));
        // Blank for an agent that hasn't named its conversation: the column
        // stays where the other rows put it.
        spans.push(Span::raw(format!(
            "{:<flex_w$}",
            truncate(&s.title, flex_w)
        )));
    }
    // Time in current state: idle-for on green rows, waiting-on-you-for on red
    // ones. Right-aligned against the edge, and always rendered whole.
    spans.push(Span::styled(
        format!("{:>pad$}", time, pad = GAP + time_w),
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

fn state_color(state: State) -> Color {
    match state {
        State::Blocked => Color::Red,
        State::Done => Color::Cyan,
        State::Working => Color::Yellow,
        State::Idle => Color::Green,
        State::Unknown => Color::Gray,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
