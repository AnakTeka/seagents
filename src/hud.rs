//! Milestone 4: the floating HUD.
//!
//! A ratatui panel meant to run in an i3 floating window. It polls the same
//! session data as `seagents status`, colour-codes each agent by state, and on
//! Enter or left-click focuses that agent's i3 window (milestone 3). Everything
//! underneath is already proven — this is assembly.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::status::{self, Session, State};

const REFRESH: Duration = Duration::from_millis(750);

pub fn run() -> i32 {
    // Long-lived surface → good place to opportunistically refresh detection
    // manifests (daily-throttled, background, silent).
    crate::spawn_manifest_update();
    if enable_raw_mode().is_err() {
        eprintln!("seagents: the HUD needs a terminal (try `seagents status` instead)");
        return 1;
    }
    let mut stdout = io::stdout();
    let _ = execute!(stdout, EnterAlternateScreen, EnableMouseCapture);
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

/// How to refer to a row in the footer — the directory, which is short and
/// always there, except in title mode where the title *is* the row's name.
fn label_of(s: &Session, labels: Label) -> &str {
    match labels {
        Label::Title if !s.title.is_empty() => &s.title,
        _ => &s.dir,
    }
}

struct App {
    sessions: Vec<Session>,
    list: ListState,
    sort: status::Sort,
    labels: Label,
    /// The `?` key list is up. It owns the keyboard while open, so a keystroke
    /// aimed at it can't quietly drive the list underneath.
    help: bool,
    message: String,
    /// Screen y of the first agent row, cached each frame for mouse hit-testing.
    rows_top: u16,
    /// con_id of the HUD's own window, so previews can return focus to it.
    /// Captured lazily on the first preview, when the HUD is definitely focused.
    hud_con_id: Option<u64>,
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
    let mut app = App {
        sessions: status::collect(sort),
        list: ListState::default(),
        sort,
        labels: load_labels(),
        help: false,
        // Surface detection-rule update problems where they'll be seen; the
        // help line is one keypress away from rediscovery.
        message: crate::manifest_health_warning()
            .map(|w| format!("⚠ {w}"))
            .unwrap_or_else(help_line),
        rows_top: 1,
        hud_con_id: None,
    };
    if !app.sessions.is_empty() {
        app.list.select(Some(0));
    }
    let mut last_refresh = Instant::now();

    loop {
        if terminal.draw(|f| draw(f, &mut app)).is_err() {
            return 1;
        }

        let timeout = REFRESH.saturating_sub(last_refresh.elapsed());
        if event::poll(timeout).unwrap_or(false) {
            let ev = event::read();
            hud_log!("{ev:?}");
            match ev {
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
                    // A mouse report torn across a read boundary surfaces as a
                    // bare Esc key (seen live during an i3 $mod+drag of the HUD
                    // window). A torn sequence's tail arrives as garbage *key*
                    // events right behind the Esc; pending mouse-motion events
                    // are unrelated and must not veto the quit (motion streams
                    // continuously under mode 1003, so "any pending input"
                    // would suppress Esc whenever the pointer is moving).
                    KeyCode::Esc => {
                        let deadline = Instant::now() + Duration::from_millis(50);
                        let mut torn = false;
                        while event::poll(Duration::from_millis(25)).unwrap_or(false) {
                            match event::read() {
                                Ok(Event::Key(_)) => {
                                    torn = true;
                                    break;
                                }
                                Ok(_) => {} // motion/resize: irrelevant, drop
                                Err(_) => break,
                            }
                            if Instant::now() >= deadline {
                                break;
                            }
                        }
                        if torn {
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
                        let last = app.sessions.len().saturating_sub(1);
                        jump_selection(&mut app, last);
                    }
                    KeyCode::Enter => focus_selected(&mut app),
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
                Ok(Event::Mouse(m)) => match m.kind {
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

fn refresh(app: &mut App) {
    // Follow the selected *session*, not its row index — under the urgency and
    // recent sorts, rows move as states change.
    let selected_id = app
        .list
        .selected()
        .and_then(|i| app.sessions.get(i))
        .map(|s| s.id);
    app.sessions = status::collect(app.sort);
    let followed = selected_id.and_then(|id| app.sessions.iter().position(|s| s.id == id));
    match followed {
        Some(i) => app.list.select(Some(i)),
        None if app.sessions.is_empty() => app.list.select(None),
        // Selected session ended: clamp the old index into range.
        None => {
            let i = app
                .list
                .selected()
                .unwrap_or(0)
                .min(app.sessions.len() - 1);
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
    let path = crate::vendor::herdr_detect::config::state_dir().join(name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, value);
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
    if app.sessions.is_empty() {
        return;
    }
    app.list.select(Some(idx.min(app.sessions.len() - 1)));
    preview_selected(app);
}

fn move_selection(app: &mut App, delta: isize) {
    let n = app.sessions.len();
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
        app.hud_con_id = crate::focus::focused_con_id();
    }
    let Some(i) = app.list.selected() else {
        return;
    };
    let Some(s) = app.sessions.get(i) else {
        return;
    };
    // Looking at it counts as seeing it: done → idle.
    status::ack_session(s.id);
    let name = label_of(s, app.labels).to_string();
    match crate::focus::preview_window(s.pid, app.hud_con_id) {
        Ok(()) => app.message = format!("previewing {name} · enter to jump"),
        Err(e) => app.message = e,
    }
}

fn focus_selected(app: &mut App) {
    let Some(i) = app.list.selected() else {
        return;
    };
    let Some(s) = app.sessions.get(i) else {
        return;
    };
    status::ack_session(s.id);
    let name = label_of(s, app.labels).to_string();
    match crate::focus::focus_window(s.pid) {
        Ok(()) => app.message = format!("focused {name} ({})", s.cmd),
        Err(e) => app.message = e,
    }
}

/// Map a screen row to a session index, accounting for the border above the
/// first agent row and the list's scroll offset (a scrolled list's first
/// visible row is not session 0).
fn row_at(app: &App, row: u16) -> Option<usize> {
    let idx = row.checked_sub(app.rows_top)? as usize + app.list.offset();
    (idx < app.sessions.len()).then_some(idx)
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
    let body = chunks[0];
    // Rows start one line below the block's top border.
    app.rows_top = body.y + 1;

    if app.sessions.is_empty() {
        let empty = Paragraph::new("no agents tracked; launch one with `seagents run claude`")
            .block(agent_block(app.sort, app.labels, 0, 0))
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, body);
    } else {
        // Pre-apply the same scroll clamp the List does during render, so the
        // clipped-row indicators describe *this* frame, not the previous one.
        let visible = body.height.saturating_sub(2) as usize; // minus borders
        let total = app.sessions.len();
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
        let cols = Cols::measure(&app.sessions);
        let items: Vec<ListItem> = app
            .sessions
            .iter()
            .map(|s| session_row(s, row_width, &cols, app.labels))
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

    let footer = Paragraph::new(app.message.as_str()).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[1]);
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

/// Column measurements taken once per frame and shared by every row, so the
/// columns line up: sizing each row to its own contents makes the list ragged.
struct Cols {
    /// Widest duration on screen. Sized from the data because it is the one
    /// column that is never allowed to truncate.
    time_w: usize,
    /// Width of the directory column when a title column sits beside it: the
    /// longest name on screen, capped so one deep project name (they run to
    /// `20260612_doc_parser` here) can't eat the title's space.
    dir_w: usize,
    /// Whether any agent on screen has named its conversation. When none has,
    /// `both` draws no title column rather than a frame of blanks.
    any_title: bool,
}

impl Cols {
    const DIR_CAP: usize = 18;

    fn measure(sessions: &[Session]) -> Cols {
        Cols {
            time_w: sessions
                .iter()
                .map(|s| crate::status::humanize_secs(s.state_secs).chars().count())
                .max()
                .unwrap_or(3),
            dir_w: sessions
                .iter()
                .map(|s| s.dir.chars().count())
                .max()
                .unwrap_or(4)
                .min(Cols::DIR_CAP),
            any_title: sessions.iter().any(|s| !s.title.is_empty()),
        }
    }
}

fn session_row(s: &Session, width: usize, cols: &Cols, labels: Label) -> ListItem<'static> {
    ListItem::new(session_line(s, width, cols, labels))
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
/// - **both** — directory *and* title. The title yields **last**, not first: a
///   47-column HUD cannot hold four columns, and a `both` mode that dropped the
///   title there would silently be `dir` mode, which is the thing it exists not
///   to be. So the command and the state word are spent on the title before the
///   row gives up and falls back to the directory alone.
/// - **title** — the title as the row's whole name (directory when the agent
///   has not named its conversation yet). Titles are sentences where
///   directories are words, so it asks for a wider minimum.
/// - **dir** — the original row.
fn session_line(s: &Session, width: usize, cols: &Cols, labels: Label) -> Line<'static> {
    let color = state_color(s.state);
    let time = crate::status::humanize_secs(s.state_secs);

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
    let solo = label_of(s, labels);
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
            format!("{:<dir_w$}", truncate(&s.dir, dir_w)),
            bold,
        ));
    } else {
        spans.push(Span::styled(
            format!("{:<flex_w$}", truncate(solo, flex_w)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Session, State};

    fn session(dir: &str, secs: u64) -> Session {
        Session {
            id: 1,
            state: State::Blocked,
            dir: dir.to_string(),
            cwd: format!("/home/me/{dir}"),
            cmd: "claude".to_string(),
            pid: 2,
            state_secs: secs,
            title: String::new(),
        }
    }

    fn titled(dir: &str, secs: u64, title: &str) -> Session {
        Session {
            title: title.to_string(),
            ..session(dir, secs)
        }
    }

    /// Lay a row out and flatten it the way the terminal would.
    fn rendered(s: &Session, width: usize, time_w: usize) -> String {
        rendered_as(s, width, time_w, Label::Dir)
    }

    fn rendered_as(s: &Session, width: usize, time_w: usize, labels: Label) -> String {
        let mut cols = Cols::measure(std::slice::from_ref(s));
        cols.time_w = time_w;
        flatten(session_line(s, width, &cols, labels))
    }

    /// Lay a whole list out together, so the shared column measurements are
    /// the ones a real frame would use.
    fn rendered_all(sessions: &[Session], width: usize, labels: Label) -> Vec<String> {
        let cols = Cols::measure(sessions);
        sessions
            .iter()
            .map(|s| flatten(session_line(s, width, &cols, labels)))
            .collect()
    }

    fn flatten(line: Line<'static>) -> String {
        line.spans
            .iter()
            .map(|sp| sp.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn a_row_never_overflows_the_width_it_is_given() {
        // The List clips silently, so an overflowing row is a wrong row.
        let s = titled(
            "20260612_doc_parser",
            56_100,
            "Kredimall search ranking and recommendation logic",
        );
        for labels in [Label::Both, Label::Title, Label::Dir] {
            for width in 20..=120 {
                let row = rendered_as(&s, width, 6, labels);
                assert!(
                    row.chars().count() <= width,
                    "{} width {width}: row is {} columns: {row:?}",
                    labels.word(),
                    row.chars().count()
                );
            }
        }
    }

    #[test]
    fn the_duration_is_never_truncated() {
        // The regression this guards: in the 47-column HUD window only 43
        // columns reached the row, and the old fixed layout rendered 15h35m as
        // 15h3 and 52m as 5.
        for secs in [45, 3_120, 56_100, 72_300, 300_000] {
            let want = crate::status::humanize_secs(secs);
            let s = titled("20260612_doc_parser", secs, "Validate issues in the parser");
            for labels in [Label::Both, Label::Title, Label::Dir] {
                for width in 20..=120 {
                    let row = rendered_as(&s, width, 6, labels);
                    assert!(
                        row.ends_with(&want),
                        "{} width {width}, {want}: row ended {row:?}",
                        labels.word()
                    );
                }
            }
        }
    }

    #[test]
    fn columns_line_up_across_rows_whatever_the_durations_are() {
        let sessions = vec![
            session("seagents", 52 * 60),
            session("20260612_doc_parser", 56_100),
            session("mode-agent", 45),
        ];
        let cols = Cols::measure(&sessions);
        assert_eq!(cols.time_w, 6, "widest is 15h35m");
        let cmd_at: Vec<_> = sessions
            .iter()
            .map(|s| flatten(session_line(s, 45, &cols, Label::Dir)).find("claude"))
            .collect();
        assert!(cmd_at.iter().all(|c| c.is_some()), "{cmd_at:?}");
        assert!(
            cmd_at.windows(2).all(|w| w[0] == w[1]),
            "command column is ragged: {cmd_at:?}"
        );
    }

    #[test]
    fn both_mode_puts_where_and_what_on_the_same_row() {
        let sessions = vec![
            titled("seagents", 45, "Detection update"),
            titled("tortoise", 7_200, "Check how brand search with txt_ai works"),
        ];
        let rows = rendered_all(&sessions, 70, Label::Both);
        assert!(rows[0].contains("seagents") && rows[0].contains("Detection update"), "{rows:?}");
        assert!(rows[1].contains("tortoise") && rows[1].contains("Check how brand"), "{rows:?}");
        // The title column starts in the same place on every row.
        let at: Vec<_> = rows
            .iter()
            .zip(["Detection", "Check"])
            .map(|(row, title)| row.find(title))
            .collect();
        assert!(at.iter().all(|c| c.is_some()), "{at:?}");
        assert!(at[0] == at[1], "title column is ragged: {at:?}");
    }

    #[test]
    fn both_mode_spends_the_command_and_state_word_on_the_title() {
        // The real 47-column window (43 columns reach the row) cannot hold four
        // columns. What has to give is the command, then the state word — never
        // the title, which is the whole point of the mode.
        let sessions = vec![titled("seagents", 56_100, "Detection update")];
        let row = &rendered_all(&sessions, 43, Label::Both)[0];
        assert!(row.contains("seagents"), "{row:?}");
        assert!(row.contains("Detection"), "{row:?}");
        assert!(!row.contains("claude"), "the command should have gone first: {row:?}");
        assert!(row.ends_with("15h35m"), "{row:?}");
    }

    #[test]
    fn a_long_project_name_cannot_squeeze_the_title_out_of_a_narrow_window() {
        // The regression: the directory column was capped at 18 columns
        // absolute, so one `20260612_doc_parser` on screen left a 43-column row
        // no room for a title and `both` silently rendered as `dir` — in
        // exactly the window size the HUD is configured for.
        let sessions = vec![
            titled("20260612_doc_parser", 7_200, "Validate issues"),
            titled("me", 45, "Autorandr suspend on laptop lid close"),
        ];
        let rows = rendered_all(&sessions, 43, Label::Both);
        assert!(rows[0].contains("Validate"), "{rows:?}");
        assert!(rows[1].contains("Autorandr"), "{rows:?}");
        // …and the directory is still identifiable, not cut to nothing.
        assert!(rows[0].contains("20260612_sme"), "{rows:?}");
    }

    #[test]
    fn both_mode_gives_up_the_title_before_it_squeezes_the_directory() {
        // Narrower than any two-name layout: rather than three cramped words,
        // the row falls back to the directory alone.
        let sessions = vec![titled("20260612_doc_parser", 56_100, "Validate issues")];
        let row = &rendered_all(&sessions, 26, Label::Both)[0];
        assert!(!row.contains("Validate"), "{row:?}");
        assert!(row.contains("20260612"), "{row:?}");
        assert!(row.ends_with("15h35m"), "{row:?}");
    }

    #[test]
    fn both_mode_draws_no_title_column_when_no_agent_has_named_one() {
        // A frame of blank space would be worse than the plain row.
        let sessions = vec![session("seagents", 45), session("mode-agent", 90)];
        let both = rendered_all(&sessions, 70, Label::Both);
        let dir = rendered_all(&sessions, 70, Label::Dir);
        assert_eq!(both, dir, "an untitled frame should lay out as dir mode");
    }

    #[test]
    fn title_mode_names_the_row_by_the_conversation() {
        let s = titled("seagents", 45, "HUD toggle for Claude Code title display");
        let by_dir = rendered_as(&s, 60, 3, Label::Dir);
        assert!(by_dir.contains("seagents"), "{by_dir:?}");
        assert!(!by_dir.contains("HUD toggle"), "{by_dir:?}");
        let by_title = rendered_as(&s, 60, 3, Label::Title);
        assert!(by_title.contains("HUD toggle"), "{by_title:?}");
        assert!(!by_title.contains("seagents"), "{by_title:?}");
    }

    #[test]
    fn title_mode_falls_back_to_the_directory_before_a_title_exists() {
        // A fresh session shows Claude's own banner, which status strips to
        // nothing — the directory still tells you where the agent is.
        let s = session("mode-agent", 45);
        let row = rendered_as(&s, 60, 3, Label::Title);
        assert!(row.contains("mode-agent"), "{row:?}");
    }

    #[test]
    fn title_mode_buys_the_name_column_more_room() {
        // The real 47-column HUD window: 43 columns reach the row. Titles are
        // sentences, so the mode trades the command column away for width.
        let s = titled("seagents", 56_100, "HUD toggle for Claude Code title display");
        let dir_row = rendered_as(&s, 43, 6, Label::Dir);
        let title_row = rendered_as(&s, 43, 6, Label::Title);
        assert!(dir_row.contains("claude"), "{dir_row:?}");
        assert!(!title_row.contains("claude"), "{title_row:?}");
        // A title too long for the window is the column that gives, and it
        // says so with an ellipsis rather than stopping mid-word.
        assert!(title_row.contains("HUD toggle"), "{title_row:?}");
        assert!(title_row.contains('…'), "{title_row:?}");
        assert!(title_row.ends_with("15h35m"), "{title_row:?}");
    }

    /// Draw a real frame through ratatui's test backend and flatten it to
    /// lines — the closest we get to the HUD without a terminal (the event
    /// loop still has to be driven by hand).
    fn frame(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn app_with(sessions: Vec<Session>, labels: Label) -> App {
        let mut list = ListState::default();
        list.select(Some(0));
        App {
            sessions,
            list,
            sort: status::Sort::Urgency,
            labels,
            help: false,
            message: help_line(),
            rows_top: 1,
            hud_con_id: None,
        }
    }

    #[test]
    fn a_drawn_frame_shows_the_labels_the_mode_asks_for() {
        let sessions = || {
            vec![
                titled("seagents", 45, "Detection update"),
                session("mode-agent", 90),
            ]
        };

        let mut app = app_with(sessions(), Label::Dir);
        let dir = frame(&mut app, 47, 8);
        let dir_rows = dir[1..].join("\n");
        assert!(dir_rows.contains("seagents"), "{dir_rows}");
        assert!(!dir_rows.contains("Detection"), "{dir_rows}");
        // Default mode: the border names the sort only. (Checked on the border
        // line itself — the footer's own key hints mention both modes.)
        assert!(dir[0].contains("urgency"), "{}", dir[0]);
        assert!(!dir[0].contains("title"), "{}", dir[0]);

        let mut app = app_with(sessions(), Label::Title);
        let title = frame(&mut app, 47, 8);
        let title_rows = title[1..].join("\n");
        assert!(title_rows.contains("Detection update"), "{title_rows}");
        // The untitled session keeps its directory, in the same column.
        assert!(title_rows.contains("mode-agent"), "{title_rows}");
        // …and the mode is stated persistently, not just in the footer flash.
        assert!(title[0].contains("title \u{b7} urgency"), "{}", title[0]);
    }

    #[test]
    fn the_key_list_fits_the_real_hud_window() {
        // 47 columns is the configured floating width; the panel's own borders
        // take two. A clipped binding is a binding nobody can read.
        let key_w = help_key_width();
        for (key, what) in help_rows() {
            let line = format!("{key:<key_w$}  {what}");
            assert!(
                line.chars().count() <= 45,
                "{line:?} is {} columns",
                line.chars().count()
            );
        }
    }

    #[test]
    fn the_overlay_covers_the_list_and_states_every_binding() {
        let mut app = app_with(vec![titled("seagents", 45, "Detection update")], Label::Title);
        app.help = true;
        let shown = frame(&mut app, 47, 14).join("\n");
        // The list underneath is hidden, not drawn around.
        assert!(!shown.contains("Detection update"), "{shown}");
        assert!(shown.contains("keys"), "{shown}");
        for (key, what) in help_rows() {
            assert!(shown.contains(what), "missing {key} ({what}):\n{shown}");
        }
    }

    #[test]
    fn a_window_too_short_for_every_binding_says_so() {
        let mut app = app_with(vec![session("seagents", 45)], Label::Dir);
        app.help = true;
        // Tall enough: no indicator, and quit is visible.
        let tall = frame(&mut app, 47, 13).join("\n");
        assert!(tall.contains("quit"), "{tall}");
        assert!(!tall.contains("more"), "{tall}");
        // Too short: the last bindings are gone, and the border admits it.
        let short = frame(&mut app, 47, 9).join("\n");
        assert!(!short.contains("quit"), "{short}");
        assert!(short.contains("▼ 3 more"), "{short}");
    }

    #[test]
    fn the_label_mode_round_trips_through_its_preference_word() {
        for labels in [Label::Both, Label::Title, Label::Dir] {
            assert_eq!(Label::parse(labels.word()), Some(labels));
            // Three modes, so the cycle has to come home in three steps.
            assert_eq!(labels.next().next().next(), labels);
        }
        assert_eq!(Label::parse("nonsense"), None);
    }

    #[test]
    fn columns_drop_widest_first_as_the_window_narrows() {
        let s = session("seagents", 56_100);
        let wide = rendered(&s, 60, 6);
        assert!(wide.contains("blocked") && wide.contains("claude"), "{wide:?}");
        // The real 47-column HUD: everything still fits.
        let mid = rendered(&s, 45, 6);
        assert!(mid.contains("blocked") && mid.contains("claude"), "{mid:?}");
        assert!(mid.ends_with("15h35m"), "{mid:?}");
        // Narrower: command goes first.
        let no_cmd = rendered(&s, 30, 6);
        assert!(no_cmd.contains("blocked") && !no_cmd.contains("claude"), "{no_cmd:?}");
        // Narrowest: only the dot, the directory and the duration.
        let narrow = rendered(&s, 22, 6);
        assert!(!narrow.contains("blocked"), "{narrow:?}");
        assert!(narrow.ends_with("15h35m"), "{narrow:?}");
    }
}
