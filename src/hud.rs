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
    message: String,
    /// Screen y of the first agent row, cached each frame for mouse hit-testing.
    rows_top: u16,
    /// con_id of the HUD's own window, so previews can return focus to it.
    /// Captured lazily on the first preview, when the HUD is definitely focused.
    hud_con_id: Option<u64>,
}

fn event_loop<B: Backend>(terminal: &mut Terminal<B>) -> i32 {
    let mut app = App {
        sessions: status::collect(),
        list: ListState::default(),
        message: "j/k preview · enter jump · r refresh · q quit".to_string(),
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
            match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return 0,
                    KeyCode::Down | KeyCode::Char('j') => move_selection(&mut app, 1),
                    KeyCode::Up | KeyCode::Char('k') => move_selection(&mut app, -1),
                    KeyCode::Enter => focus_selected(&mut app),
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
            last_refresh = Instant::now();
        }
    }
}

fn refresh(app: &mut App) {
    app.sessions = status::collect();
    // Keep the selection in range as sessions come and go.
    match app.list.selected() {
        Some(i) if i >= app.sessions.len() => {
            app.list
                .select(app.sessions.len().checked_sub(1));
        }
        None if !app.sessions.is_empty() => app.list.select(Some(0)),
        _ => {}
    }
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
    match crate::focus::focus_window(s.pid) {
        Ok(()) => app.message = format!("focused {} ({})", s.dir, s.cmd),
        Err(e) => app.message = e,
    }
}

/// Map a screen row to a session index, accounting for the border/header above
/// the first agent row.
fn row_at(app: &App, row: u16) -> Option<usize> {
    let idx = row.checked_sub(app.rows_top)? as usize;
    (idx < app.sessions.len()).then_some(idx)
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
    let body = chunks[0];
    // Rows start one line below the block's top border.
    app.rows_top = body.y + 1;

    if app.sessions.is_empty() {
        let empty = Paragraph::new("no agents tracked — launch one with `seagents run claude`")
            .block(agent_block())
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, body);
    } else {
        let items: Vec<ListItem> = app.sessions.iter().map(session_row).collect();
        let list = List::new(items)
            .block(agent_block())
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, body, &mut app.list);
    }

    let footer = Paragraph::new(app.message.as_str()).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[1]);
}

fn agent_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(" seagents ")
}

fn session_row(s: &Session) -> ListItem<'static> {
    let color = state_color(s.state);
    let line = Line::from(vec![
        Span::styled("● ", Style::default().fg(color)),
        Span::styled(format!("{:<8}", state_word(s.state)), Style::default().fg(color)),
        Span::raw("  "),
        Span::styled(
            format!("{:<16}", truncate(&s.dir, 16)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:<8}", truncate(&s.cmd, 8)),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(format!("  {}", s.pid), Style::default().fg(Color::DarkGray)),
    ]);
    ListItem::new(line)
}

fn state_color(state: State) -> Color {
    match state {
        State::Blocked => Color::Red,
        State::Working => Color::Yellow,
        State::Idle => Color::Green,
        State::Unknown => Color::Gray,
    }
}

fn state_word(state: State) -> &'static str {
    // Reuse the badge text without its emoji prefix.
    match state {
        State::Blocked => "blocked",
        State::Working => "working",
        State::Idle => "idle",
        State::Unknown => "unknown",
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
