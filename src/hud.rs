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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

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

struct App {
    sessions: Vec<Session>,
    list: ListState,
    sort: status::Sort,
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

fn help_line() -> String {
    "j/k preview · g/G ends · enter jump · s sort · r refresh · q quit".to_string()
}

fn sort_pref_path() -> std::path::PathBuf {
    crate::vendor::herdr_detect::config::state_dir().join("hud-sort")
}

/// The HUD remembers its sort mode across launches — one word in a state file.
fn load_sort() -> status::Sort {
    std::fs::read_to_string(sort_pref_path())
        .ok()
        .and_then(|s| status::Sort::parse(s.trim()))
        .unwrap_or(status::Sort::Urgency)
}

fn save_sort(sort: status::Sort) {
    let path = sort_pref_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, sort.word());
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
    match crate::focus::preview_window(s.pid, app.hud_con_id) {
        Ok(()) => app.message = format!("previewing {} · enter to jump", s.dir),
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
    match crate::focus::focus_window(s.pid) {
        Ok(()) => app.message = format!("focused {} ({})", s.dir, s.cmd),
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
            .block(agent_block(app.sort, 0, 0))
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
        let time_w = duration_width(&app.sessions);
        let items: Vec<ListItem> = app
            .sessions
            .iter()
            .map(|s| session_row(s, row_width, time_w))
            .collect();
        let list = List::new(items)
            .block(agent_block(app.sort, above, below))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol(HIGHLIGHT_SYMBOL);
        f.render_stateful_widget(list, body, &mut app.list);
    }

    let footer = Paragraph::new(app.message.as_str()).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[1]);
}

fn agent_block(sort: status::Sort, above: usize, below: usize) -> Block<'static> {
    use ratatui::widgets::block::{Position, Title};
    let dim = |text: String| Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)));
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(" seagents ")
        // Persistent sort indicator, top-right — the footer flash on `s` is
        // gone by the next message, this isn't.
        .title(Title::from(dim(format!(" {} ", sort.word()))).alignment(Alignment::Right));
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

/// The duration column's width: the widest value on screen, so every row
/// allocates the same space and the columns to its left line up. Sizing each
/// row to its own duration instead makes the directory column ragged.
fn duration_width(sessions: &[Session]) -> usize {
    sessions
        .iter()
        .map(|s| crate::status::humanize_secs(s.state_secs).chars().count())
        .max()
        .unwrap_or(3)
}

fn session_row(s: &Session, width: usize, time_w: usize) -> ListItem<'static> {
    ListItem::new(session_line(s, width, time_w))
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
/// agent has been waiting on you is the question the HUD exists to answer),
/// and optional columns drop as the window narrows: the command first (it is
/// `claude` on nearly every row), then the state word (the coloured dot already
/// carries the state). The directory takes whatever is left.
fn session_line(s: &Session, width: usize, time_w: usize) -> Line<'static> {
    let color = state_color(s.state);
    let time = crate::status::humanize_secs(s.state_secs);

    const DOT: usize = 2;
    const GAP: usize = 2;
    const CMD: usize = 6;
    const DIR_MIN: usize = 8;
    let time_w = time_w.max(time.chars().count());
    let state_w = 7;

    let with_cmd = DOT + state_w + GAP + DIR_MIN + 1 + CMD + GAP + time_w;
    let with_state = DOT + state_w + GAP + DIR_MIN + GAP + time_w;
    let (show_state, show_cmd) = if width >= with_cmd {
        (true, true)
    } else if width >= with_state {
        (true, false)
    } else {
        (false, false)
    };

    let fixed = DOT
        + if show_state { state_w + GAP } else { 0 }
        + if show_cmd { CMD + 1 } else { 0 }
        + GAP
        + time_w;
    let dir_w = width.saturating_sub(fixed).max(1);

    let mut spans = vec![Span::styled("● ", Style::default().fg(color))];
    if show_state {
        spans.push(Span::styled(
            format!("{:<state_w$}", s.state.word()),
            Style::default().fg(color),
        ));
        spans.push(Span::raw(" ".repeat(GAP)));
    }
    spans.push(Span::styled(
        format!("{:<dir_w$}", truncate(&s.dir, dir_w)),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if show_cmd {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{:<CMD$}", truncate(&s.cmd, CMD)),
            Style::default().fg(Color::Gray),
        ));
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
        }
    }

    /// Lay a row out and flatten it the way the terminal would.
    fn rendered(s: &Session, width: usize, time_w: usize) -> String {
        session_line(s, width, time_w)
            .spans
            .iter()
            .map(|sp| sp.content.as_ref())
            .collect()
    }

    #[test]
    fn a_row_never_overflows_the_width_it_is_given() {
        // The List clips silently, so an overflowing row is a wrong row.
        for width in 20..=120 {
            let row = rendered(&session("20260612_doc_parser", 56_100), width, 6);
            assert!(
                row.chars().count() <= width,
                "width {width}: row is {} columns: {row:?}",
                row.chars().count()
            );
        }
    }

    #[test]
    fn the_duration_is_never_truncated() {
        // The regression this guards: in the 47-column HUD window only 43
        // columns reached the row, and the old fixed layout rendered 15h35m as
        // 15h3 and 52m as 5.
        for secs in [45, 3_120, 56_100, 72_300, 300_000] {
            let want = crate::status::humanize_secs(secs);
            for width in 20..=120 {
                let row = rendered(&session("20260612_doc_parser", secs), width, 6);
                assert!(
                    row.ends_with(&want),
                    "width {width}, {want}: row ended {row:?}"
                );
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
        let time_w = duration_width(&sessions);
        assert_eq!(time_w, 6, "widest is 15h35m");
        let cmd_at: Vec<_> = sessions
            .iter()
            .map(|s| rendered(s, 45, time_w).find("claude"))
            .collect();
        assert!(cmd_at.iter().all(|c| c.is_some()), "{cmd_at:?}");
        assert!(
            cmd_at.windows(2).all(|w| w[0] == w[1]),
            "command column is ragged: {cmd_at:?}"
        );
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
