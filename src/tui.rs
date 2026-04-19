//! Full-screen ratatui UI for `agent-container config mcp`.
//!
//! Every tool advertised by every configured MCP server (HTTP/SSE/stdio)
//! appears in a single scrollable checklist. Arrow keys move the cursor,
//! space toggles the current tool, `s` or Enter saves, `q` or Esc cancels.
//! The alternate screen is used so the prior terminal contents reappear
//! untouched after exit.

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub read_only_hint: Option<bool>,
    pub enabled: bool,
}

pub enum Outcome {
    Save(Vec<ToolEntry>),
    Cancel,
}

struct App {
    entries: Vec<ToolEntry>,
    selected: usize,
    state: ListState,
    message: Option<String>,
}

impl App {
    fn new(entries: Vec<ToolEntry>) -> Self {
        let mut state = ListState::default();
        if !entries.is_empty() {
            state.select(Some(0));
        }
        Self {
            entries,
            selected: 0,
            state,
            message: None,
        }
    }

    fn move_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
        self.state.select(Some(self.selected));
    }

    fn move_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
            self.state.select(Some(self.selected));
        }
    }

    fn jump_home(&mut self) {
        if !self.entries.is_empty() {
            self.selected = 0;
            self.state.select(Some(0));
        }
    }

    fn jump_end(&mut self) {
        if !self.entries.is_empty() {
            self.selected = self.entries.len() - 1;
            self.state.select(Some(self.selected));
        }
    }

    fn toggle(&mut self) {
        if let Some(entry) = self.entries.get_mut(self.selected) {
            entry.enabled = !entry.enabled;
        }
    }

    fn toggle_server_all(&mut self, enable: bool) {
        let Some(current) = self.entries.get(self.selected) else {
            return;
        };
        let server = current.server_name.clone();
        for e in &mut self.entries {
            if e.server_name == server {
                e.enabled = enable;
            }
        }
    }
}

pub fn run_selection(initial: Vec<ToolEntry>) -> Result<Outcome> {
    // Enter alt-screen + raw mode; wire a panic hook to restore on crash.
    enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide).context("entering alt screen")?;

    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
        orig_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("creating terminal")?;

    let mut app = App::new(initial);
    let outcome = loop {
        terminal.draw(|f| render(f, &mut app))?;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Up | KeyCode::Char('k') => app.move_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.move_down(),
                    KeyCode::Home | KeyCode::Char('g') => app.jump_home(),
                    KeyCode::End | KeyCode::Char('G') => app.jump_end(),
                    KeyCode::Char(' ') => app.toggle(),
                    KeyCode::Char('a') => app.toggle_server_all(true),
                    KeyCode::Char('A') => app.toggle_server_all(false),
                    KeyCode::Char('s') | KeyCode::Enter => break Outcome::Save(app.entries),
                    KeyCode::Char('q') | KeyCode::Esc => break Outcome::Cancel,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break Outcome::Cancel;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    };

    // Restore terminal.
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show).ok();
    Ok(outcome)
}

fn render(f: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);

    render_title(f, chunks[0]);
    render_list(f, chunks[1], app);
    render_footer(f, chunks[2], app);
}

fn render_title(f: &mut ratatui::Frame<'_>, area: Rect) {
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " agent-container ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" MCP tool allowlist"),
    ]));
    f.render_widget(title, area);
}

fn render_list(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let mut items: Vec<ListItem> = Vec::with_capacity(app.entries.len());
    let mut last_server: Option<&str> = None;
    for entry in &app.entries {
        let server_changed = last_server != Some(entry.server_name.as_str());
        last_server = Some(&entry.server_name);

        let cb = if entry.enabled { "[x]" } else { "[ ]" };
        let ann = match entry.read_only_hint {
            Some(true) => Span::styled(" RO    ", Style::default().fg(Color::Green)),
            Some(false) => Span::styled(" WRITE ", Style::default().fg(Color::Yellow)),
            None => Span::styled("  ?    ", Style::default().fg(Color::DarkGray)),
        };
        let server = if server_changed {
            Span::styled(
                format!("{:<12}", entry.server_name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw(format!("{:<12}", ""))
        };
        let tool = Span::raw(format!("{:<32}", entry.tool_name));
        let first_line = entry
            .description
            .lines()
            .next()
            .unwrap_or("")
            .trim();
        let desc = Span::styled(
            if first_line.len() > 60 {
                format!("{}…", &first_line[..60])
            } else {
                first_line.to_string()
            },
            Style::default().fg(Color::DarkGray),
        );

        items.push(ListItem::new(Line::from(vec![
            Span::raw(format!(" {cb} ")),
            ann,
            Span::raw("  "),
            server,
            tool,
            desc,
        ])));
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶");

    f.render_stateful_widget(list, area, &mut app.state);
}

fn render_footer(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let enabled_count = app.entries.iter().filter(|e| e.enabled).count();
    let total = app.entries.len();
    let help = Line::from(vec![
        Span::styled(
            "↑/↓",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" move · "),
        Span::styled(
            "space",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" toggle · "),
        Span::styled(
            "a/A",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" server enable/disable all · "),
        Span::styled(
            "s",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" save · "),
        Span::styled(
            "q",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" quit"),
    ]);
    let count = Line::from(Span::styled(
        format!(
            "{}/{} enabled{}",
            enabled_count,
            total,
            app.message
                .as_ref()
                .map(|m| format!(" — {m}"))
                .unwrap_or_default()
        ),
        Style::default().fg(Color::DarkGray),
    ));
    let para = Paragraph::new(vec![help, count]);
    f.render_widget(para, area);
}
