//! Full-screen ratatui UI for `agent-container config mcp`.
//!
//! One tab per MCP server. `h`/`l` (or ←/→) switch tabs, `j`/`k` (or ↑/↓)
//! move within the active tab's tool list, space toggles the highlighted
//! tool, `a`/`A` enable/disable every tool on the current tab in one go,
//! `s` or Enter saves, `q` or Esc cancels. The alternate screen is used
//! so the prior terminal contents reappear untouched after exit.

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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs};

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
    server_names: Vec<String>,
    tab: usize,
    /// Cursor position per tab, kept across tab switches.
    per_tab_cursor: Vec<usize>,
    state: ListState,
}

impl App {
    fn new(mut entries: Vec<ToolEntry>) -> Self {
        entries.sort_by(|a, b| {
            a.server_name
                .cmp(&b.server_name)
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });
        let mut server_names: Vec<String> =
            entries.iter().map(|e| e.server_name.clone()).collect();
        server_names.dedup();
        let per_tab_cursor = vec![0usize; server_names.len()];
        let mut state = ListState::default();
        if !entries.is_empty() {
            state.select(Some(0));
        }
        Self {
            entries,
            server_names,
            tab: 0,
            per_tab_cursor,
            state,
        }
    }

    fn tab_indices(&self) -> Vec<usize> {
        let Some(server) = self.server_names.get(self.tab) else {
            return Vec::new();
        };
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| &e.server_name == server)
            .map(|(i, _)| i)
            .collect()
    }

    fn current_entry_index(&self) -> Option<usize> {
        self.tab_indices()
            .get(self.per_tab_cursor.get(self.tab).copied().unwrap_or(0))
            .copied()
    }

    fn sync_state(&mut self) {
        self.state
            .select(self.per_tab_cursor.get(self.tab).copied());
    }

    fn next_tab(&mut self) {
        if self.server_names.is_empty() {
            return;
        }
        self.tab = (self.tab + 1) % self.server_names.len();
        self.sync_state();
    }

    fn prev_tab(&mut self) {
        if self.server_names.is_empty() {
            return;
        }
        self.tab = if self.tab == 0 {
            self.server_names.len() - 1
        } else {
            self.tab - 1
        };
        self.sync_state();
    }

    fn move_up(&mut self) {
        let cur = self.per_tab_cursor.get_mut(self.tab);
        if let Some(cur) = cur {
            *cur = cur.saturating_sub(1);
        }
        self.sync_state();
    }

    fn move_down(&mut self) {
        let tab_len = self.tab_indices().len();
        if let Some(cur) = self.per_tab_cursor.get_mut(self.tab) {
            if *cur + 1 < tab_len {
                *cur += 1;
            }
        }
        self.sync_state();
    }

    fn jump_home(&mut self) {
        if let Some(cur) = self.per_tab_cursor.get_mut(self.tab) {
            *cur = 0;
        }
        self.sync_state();
    }

    fn jump_end(&mut self) {
        let tab_len = self.tab_indices().len();
        if tab_len == 0 {
            return;
        }
        if let Some(cur) = self.per_tab_cursor.get_mut(self.tab) {
            *cur = tab_len - 1;
        }
        self.sync_state();
    }

    fn toggle(&mut self) {
        if let Some(idx) = self.current_entry_index() {
            self.entries[idx].enabled = !self.entries[idx].enabled;
        }
    }

    fn toggle_all_in_tab(&mut self, enable: bool) {
        let Some(server) = self.server_names.get(self.tab).cloned() else {
            return;
        };
        for e in &mut self.entries {
            if e.server_name == server {
                e.enabled = enable;
            }
        }
    }
}

pub fn run_selection(initial: Vec<ToolEntry>) -> Result<Outcome> {
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
                    KeyCode::Left | KeyCode::Char('h') => app.prev_tab(),
                    KeyCode::Right | KeyCode::Char('l') => app.next_tab(),
                    KeyCode::Tab => app.next_tab(),
                    KeyCode::BackTab => app.prev_tab(),
                    KeyCode::Home | KeyCode::Char('g') => app.jump_home(),
                    KeyCode::End | KeyCode::Char('G') => app.jump_end(),
                    KeyCode::Char(' ') => app.toggle(),
                    KeyCode::Char('a') => app.toggle_all_in_tab(true),
                    KeyCode::Char('A') => app.toggle_all_in_tab(false),
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
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);

    render_title(f, chunks[0]);
    render_tabs(f, chunks[1], app);
    render_list(f, chunks[2], app);
    render_footer(f, chunks[3], app);
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

fn render_tabs(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let titles: Vec<Line> = app
        .server_names
        .iter()
        .map(|s| Line::from(Span::raw(s.as_str())))
        .collect();
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::BOTTOM))
        .select(app.tab)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, area);
}

fn render_list(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let Some(server) = app.server_names.get(app.tab) else {
        return;
    };
    let items: Vec<ListItem> = app
        .entries
        .iter()
        .filter(|e| &e.server_name == server)
        .map(render_row)
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut app.state);
}

fn render_row(entry: &ToolEntry) -> ListItem<'static> {
    let cb = if entry.enabled { "[x]" } else { "[ ]" };
    let first_line = entry.description.lines().next().unwrap_or("").trim();
    let desc = if first_line.len() > 64 {
        format!("{}…", &first_line[..64])
    } else {
        first_line.to_string()
    };

    // Small, out-of-the-way annotation tag after the description.
    let annotation: Option<Span<'static>> = match entry.read_only_hint {
        Some(true) => Some(Span::styled(" [RO]", Style::default().fg(Color::Green))),
        Some(false) => Some(Span::styled(" [W]", Style::default().fg(Color::Yellow))),
        None => None,
    };

    let mut spans = vec![
        Span::raw(format!("{cb} ")),
        Span::raw(format!("{:<32}", entry.tool_name)),
        Span::styled(desc, Style::default().fg(Color::DarkGray)),
    ];
    if let Some(tag) = annotation {
        spans.push(tag);
    }
    ListItem::new(Line::from(spans))
}

fn render_footer(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let help = Line::from(vec![
        Span::styled(
            "h/l",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" switch MCP · "),
        Span::styled(
            "j/k",
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
        Span::raw(" tab on/off · "),
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
    let (tab_enabled, tab_total) = app
        .server_names
        .get(app.tab)
        .map(|server| {
            let total = app
                .entries
                .iter()
                .filter(|e| &e.server_name == server)
                .count();
            let enabled = app
                .entries
                .iter()
                .filter(|e| &e.server_name == server && e.enabled)
                .count();
            (enabled, total)
        })
        .unwrap_or((0, 0));
    let total_enabled = app.entries.iter().filter(|e| e.enabled).count();
    let count = Line::from(vec![
        Span::styled(
            format!(
                "{}: {}/{} enabled",
                app.server_names
                    .get(app.tab)
                    .map(String::as_str)
                    .unwrap_or("-"),
                tab_enabled,
                tab_total
            ),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("   "),
        Span::styled(
            format!("(overall {}/{})", total_enabled, app.entries.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let para = Paragraph::new(vec![help, count]);
    f.render_widget(para, area);
}
