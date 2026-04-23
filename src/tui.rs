//! Full-screen ratatui UI for `agent-container config`.
//!
//! The window has two top-level tabs:
//!
//! - **Proxy** — a scope-local list of tinyproxy allow regex patterns
//!   (the ones that will be appended to the bundled base allowlist at
//!   runtime). `i`/`+` appends, `e`/`Enter` edits, `d` removes.
//! - **MCP** — a collapsible tree of servers → tools. `Space` toggles the
//!   highlighted item (collapse on a server row, enable/disable on a
//!   tool row). `a`/`A` bulk-toggles every tool in the focused server.
//!
//! Cross-tab:
//!
//! - `h`/`l` (or ←/→, Tab/Shift+Tab) switch between Proxy and MCP.
//! - `j`/`k` (or ↑/↓) move within the current tab.
//! - `s` saves.
//! - `q`, `Esc`, or `Ctrl+C` cancels.
//!
//! The alternate screen is entered so the prior terminal contents come
//! back untouched on exit.

use std::collections::HashMap;
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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs};

#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub read_only_hint: Option<bool>,
    pub enabled: bool,
}

pub struct TuiInput {
    /// "Global" or "Workspace" — purely decorative, shown in the header.
    pub scope_label: String,
    /// The target-scope's current `proxy.allow` list (not merged — the
    /// editor writes the list verbatim back to that scope).
    pub proxy_allow: Vec<String>,
    /// Merged MCP tool catalogue with effective-enabled state from the
    /// runtime view. Changes compared back to this set decide which
    /// entries land in the target scope at save time.
    pub tool_entries: Vec<ToolEntry>,
}

pub struct TuiOutput {
    pub proxy_allow: Vec<String>,
    pub tool_entries: Vec<ToolEntry>,
}

pub enum Outcome {
    Save(TuiOutput),
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TopTab {
    Proxy,
    Mcp,
}

impl TopTab {
    fn next(self) -> Self {
        match self {
            TopTab::Proxy => TopTab::Mcp,
            TopTab::Mcp => TopTab::Proxy,
        }
    }
    fn prev(self) -> Self {
        self.next()
    }
    fn titles() -> [&'static str; 2] {
        ["Proxy", "MCP"]
    }
    fn index(self) -> usize {
        match self {
            TopTab::Proxy => 0,
            TopTab::Mcp => 1,
        }
    }
}

enum Mode {
    Normal,
    ProxyInput {
        buffer: String,
        editing_idx: Option<usize>,
    },
}

struct ProxyState {
    allow: Vec<String>,
    cursor: usize,
}

impl ProxyState {
    fn new(allow: Vec<String>) -> Self {
        Self { allow, cursor: 0 }
    }

    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_down(&mut self) {
        if self.cursor + 1 < self.allow.len() {
            self.cursor += 1;
        }
    }

    fn current(&self) -> Option<&String> {
        self.allow.get(self.cursor)
    }

    fn remove_current(&mut self) {
        if self.allow.is_empty() {
            return;
        }
        self.allow.remove(self.cursor);
        if self.cursor > 0 && self.cursor >= self.allow.len() {
            self.cursor = self.allow.len().saturating_sub(1);
        }
    }

    fn upsert(&mut self, value: String, at: Option<usize>) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        let v = trimmed.to_string();
        match at {
            Some(i) if i < self.allow.len() => {
                self.allow[i] = v;
            }
            _ => {
                self.allow.push(v);
                self.cursor = self.allow.len() - 1;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum McpRow {
    Server(usize),
    Tool(usize),
}

struct McpState {
    server_names: Vec<String>,
    /// Per-server collapse state. Initially expanded when a server has
    /// any overrides visible so the user can immediately see them.
    expanded: Vec<bool>,
    entries: Vec<ToolEntry>,
    /// Precomputed map from `server_name -> first-tool-index, tool-count`
    /// so expand/collapse doesn't have to scan the full list each frame.
    server_ranges: HashMap<String, (usize, usize)>,
    cursor: usize,
}

impl McpState {
    fn new(mut entries: Vec<ToolEntry>) -> Self {
        entries.sort_by(|a, b| {
            a.server_name
                .cmp(&b.server_name)
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });

        let mut server_names: Vec<String> = Vec::new();
        let mut server_ranges: HashMap<String, (usize, usize)> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            match server_ranges.get_mut(&e.server_name) {
                Some((_, count)) => *count += 1,
                None => {
                    server_ranges.insert(e.server_name.clone(), (i, 1));
                    server_names.push(e.server_name.clone());
                }
            }
        }

        let expanded = vec![true; server_names.len()];
        Self {
            server_names,
            expanded,
            entries,
            server_ranges,
            cursor: 0,
        }
    }

    /// Flat list of currently-visible rows (respecting expanded state).
    fn visible_rows(&self) -> Vec<McpRow> {
        let mut rows = Vec::new();
        for (si, name) in self.server_names.iter().enumerate() {
            rows.push(McpRow::Server(si));
            if self.expanded[si] {
                if let Some((start, count)) = self.server_ranges.get(name).copied() {
                    for t in 0..count {
                        rows.push(McpRow::Tool(start + t));
                    }
                }
            }
        }
        rows
    }

    fn current_row(&self) -> Option<McpRow> {
        self.visible_rows().get(self.cursor).copied()
    }

    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_down(&mut self) {
        let max = self.visible_rows().len();
        if self.cursor + 1 < max {
            self.cursor += 1;
        }
    }

    fn jump_home(&mut self) {
        self.cursor = 0;
    }

    fn jump_end(&mut self) {
        let len = self.visible_rows().len();
        self.cursor = len.saturating_sub(1);
    }

    fn toggle(&mut self) {
        match self.current_row() {
            Some(McpRow::Server(si)) => {
                self.expanded[si] = !self.expanded[si];
            }
            Some(McpRow::Tool(ti)) => {
                self.entries[ti].enabled = !self.entries[ti].enabled;
            }
            None => {}
        }
    }

    fn toggle_all_in_focused_server(&mut self, enable: bool) {
        let server_idx = match self.current_row() {
            Some(McpRow::Server(si)) => si,
            Some(McpRow::Tool(ti)) => {
                // find which server owns entries[ti]
                self.server_names
                    .iter()
                    .position(|n| n == &self.entries[ti].server_name)
                    .unwrap_or(0)
            }
            None => return,
        };
        let Some(name) = self.server_names.get(server_idx) else {
            return;
        };
        if let Some((start, count)) = self.server_ranges.get(name).copied() {
            for i in start..(start + count) {
                self.entries[i].enabled = enable;
            }
        }
    }

    fn enabled_count_for(&self, server_idx: usize) -> (usize, usize) {
        let Some(name) = self.server_names.get(server_idx) else {
            return (0, 0);
        };
        let Some((start, count)) = self.server_ranges.get(name).copied() else {
            return (0, 0);
        };
        let enabled = self.entries[start..start + count]
            .iter()
            .filter(|e| e.enabled)
            .count();
        (enabled, count)
    }
}

struct App {
    scope_label: String,
    tab: TopTab,
    proxy: ProxyState,
    mcp: McpState,
    mode: Mode,
    list_state: ListState,
}

impl App {
    fn new(input: TuiInput) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            scope_label: input.scope_label,
            tab: TopTab::Proxy,
            proxy: ProxyState::new(input.proxy_allow),
            mcp: McpState::new(input.tool_entries),
            mode: Mode::Normal,
            list_state,
        }
    }

    fn sync_list_state(&mut self) {
        let cur = match self.tab {
            TopTab::Proxy => self.proxy.cursor,
            TopTab::Mcp => self.mcp.cursor,
        };
        self.list_state.select(Some(cur));
    }

    fn into_output(self) -> TuiOutput {
        TuiOutput {
            proxy_allow: self.proxy.allow,
            tool_entries: self.mcp.entries,
        }
    }
}

fn handle_proxy_input_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    // Pull the current input buffer out first so we can mutate `app.mode`
    // (to Normal on commit/cancel) without aliasing the same borrow.
    let Mode::ProxyInput {
        mut buffer,
        editing_idx,
    } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return;
    };

    match code {
        KeyCode::Esc => {
            // mode is already Normal; nothing else to do
        }
        KeyCode::Enter => {
            app.proxy.upsert(buffer, editing_idx);
        }
        KeyCode::Backspace => {
            buffer.pop();
            app.mode = Mode::ProxyInput {
                buffer,
                editing_idx,
            };
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            // cancel — mode already reset to Normal
        }
        KeyCode::Char(c) => {
            buffer.push(c);
            app.mode = Mode::ProxyInput {
                buffer,
                editing_idx,
            };
        }
        _ => {
            app.mode = Mode::ProxyInput {
                buffer,
                editing_idx,
            };
        }
    }
}

pub fn run_selection(input: TuiInput) -> Result<Outcome> {
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

    let mut app = App::new(input);
    let outcome = loop {
        app.sync_list_state();
        terminal.draw(|f| render(f, &mut app))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Input mode handling short-circuits every other binding.
        if matches!(app.mode, Mode::ProxyInput { .. }) {
            handle_proxy_input_key(&mut app, key.code, key.modifiers);
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break Outcome::Cancel,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                break Outcome::Cancel;
            }
            KeyCode::Char('s') => break Outcome::Save(app.into_output()),
            KeyCode::Tab => app.tab = app.tab.next(),
            KeyCode::BackTab => app.tab = app.tab.prev(),
            KeyCode::Left | KeyCode::Char('h') => app.tab = app.tab.prev(),
            KeyCode::Right | KeyCode::Char('l') => app.tab = app.tab.next(),
            KeyCode::Up | KeyCode::Char('k') => match app.tab {
                TopTab::Proxy => app.proxy.move_up(),
                TopTab::Mcp => app.mcp.move_up(),
            },
            KeyCode::Down | KeyCode::Char('j') => match app.tab {
                TopTab::Proxy => app.proxy.move_down(),
                TopTab::Mcp => app.mcp.move_down(),
            },
            KeyCode::Home | KeyCode::Char('g') => match app.tab {
                TopTab::Proxy => app.proxy.cursor = 0,
                TopTab::Mcp => app.mcp.jump_home(),
            },
            KeyCode::End | KeyCode::Char('G') => match app.tab {
                TopTab::Proxy => {
                    app.proxy.cursor = app.proxy.allow.len().saturating_sub(1);
                }
                TopTab::Mcp => app.mcp.jump_end(),
            },
            KeyCode::Char(' ') | KeyCode::Enter => match app.tab {
                TopTab::Proxy => {
                    if let Some(cur) = app.proxy.current().cloned() {
                        app.mode = Mode::ProxyInput {
                            buffer: cur,
                            editing_idx: Some(app.proxy.cursor),
                        };
                    }
                }
                TopTab::Mcp => app.mcp.toggle(),
            },
            KeyCode::Char('i') | KeyCode::Char('+') if app.tab == TopTab::Proxy => {
                app.mode = Mode::ProxyInput {
                    buffer: String::new(),
                    editing_idx: None,
                };
            }
            KeyCode::Char('e') if app.tab == TopTab::Proxy => {
                if let Some(cur) = app.proxy.current().cloned() {
                    app.mode = Mode::ProxyInput {
                        buffer: cur,
                        editing_idx: Some(app.proxy.cursor),
                    };
                }
            }
            KeyCode::Char('d') if app.tab == TopTab::Proxy => {
                app.proxy.remove_current();
            }
            KeyCode::Char('a') if app.tab == TopTab::Mcp => {
                app.mcp.toggle_all_in_focused_server(true);
            }
            KeyCode::Char('A') if app.tab == TopTab::Mcp => {
                app.mcp.toggle_all_in_focused_server(false);
            }
            _ => {}
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

    render_title(f, chunks[0], app);
    render_tabs(f, chunks[1], app);
    match app.tab {
        TopTab::Proxy => render_proxy(f, chunks[2], app),
        TopTab::Mcp => render_mcp(f, chunks[2], app),
    }
    render_footer(f, chunks[3], app);

    // Overlay modal for proxy input.
    if let Mode::ProxyInput {
        ref buffer,
        editing_idx,
    } = app.mode
    {
        render_proxy_input_modal(f, area, buffer, editing_idx);
    }
}

fn render_title(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " agent-container ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  settings ({})", app.scope_label)),
    ]));
    f.render_widget(title, area);
}

fn render_tabs(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let titles: Vec<Line> = TopTab::titles()
        .iter()
        .map(|s| Line::from(Span::raw(*s)))
        .collect();
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::BOTTOM))
        .select(app.tab.index())
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, area);
}

fn render_proxy(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = if app.proxy.allow.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  (no scope-local allow patterns; press `i` to add)",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        app.proxy
            .allow
            .iter()
            .map(|p| ListItem::new(Line::from(Span::raw(p.clone()))))
            .collect()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_mcp(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let rows = app.mcp.visible_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match *row {
            McpRow::Server(si) => {
                let name = &app.mcp.server_names[si];
                let (enabled, total) = app.mcp.enabled_count_for(si);
                let marker = if app.mcp.expanded[si] { "▾" } else { "▸" };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{marker} {name}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  ({enabled}/{total} enabled)"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            }
            McpRow::Tool(ti) => render_tool_row(&app.mcp.entries[ti]),
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_tool_row(entry: &ToolEntry) -> ListItem<'static> {
    let cb = if entry.enabled { "[x]" } else { "[ ]" };
    let first_line = entry.description.lines().next().unwrap_or("").trim();
    let desc = if first_line.len() > 64 {
        format!("{}…", &first_line[..64])
    } else {
        first_line.to_string()
    };

    let annotation: Option<Span<'static>> = match entry.read_only_hint {
        Some(true) => Some(Span::styled(" [RO]", Style::default().fg(Color::Green))),
        Some(false) => Some(Span::styled(" [W]", Style::default().fg(Color::Yellow))),
        None => None,
    };

    let mut spans: Vec<Span<'static>> = vec![
        Span::raw("    "),
        Span::raw(format!("{cb} ")),
        Span::raw(entry.tool_name.clone()),
    ];
    if let Some(tag) = annotation {
        spans.push(tag);
    }
    if !desc.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(desc, Style::default().fg(Color::DarkGray)));
    }
    ListItem::new(Line::from(spans))
}

fn render_footer(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let key = |s: &str, color: Color| {
        Span::styled(
            s.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    };

    let help = match app.tab {
        TopTab::Proxy => Line::from(vec![
            key("h/l", Color::Cyan),
            Span::raw(" tabs · "),
            key("j/k", Color::Cyan),
            Span::raw(" move · "),
            key("i", Color::Cyan),
            Span::raw(" add · "),
            key("e/Enter", Color::Cyan),
            Span::raw(" edit · "),
            key("d", Color::Cyan),
            Span::raw(" delete · "),
            key("s", Color::Green),
            Span::raw(" save · "),
            key("q", Color::Red),
            Span::raw(" cancel"),
        ]),
        TopTab::Mcp => Line::from(vec![
            key("h/l", Color::Cyan),
            Span::raw(" tabs · "),
            key("j/k", Color::Cyan),
            Span::raw(" move · "),
            key("space", Color::Cyan),
            Span::raw(" toggle/collapse · "),
            key("a/A", Color::Cyan),
            Span::raw(" bulk on/off · "),
            key("s", Color::Green),
            Span::raw(" save · "),
            key("q", Color::Red),
            Span::raw(" cancel"),
        ]),
    };

    let status = match app.tab {
        TopTab::Proxy => Line::from(vec![Span::styled(
            format!(
                "{} allow pattern(s) in this scope",
                app.proxy.allow.len()
            ),
            Style::default().fg(Color::DarkGray),
        )]),
        TopTab::Mcp => {
            let total = app.mcp.entries.len();
            let enabled = app.mcp.entries.iter().filter(|e| e.enabled).count();
            Line::from(vec![Span::styled(
                format!(
                    "{enabled}/{total} tool(s) enabled across {} server(s)",
                    app.mcp.server_names.len()
                ),
                Style::default().fg(Color::DarkGray),
            )])
        }
    };

    let para = Paragraph::new(vec![help, status]);
    f.render_widget(para, area);
}

fn render_proxy_input_modal(
    f: &mut ratatui::Frame<'_>,
    parent: Rect,
    buffer: &str,
    editing_idx: Option<usize>,
) {
    // Centered 60-char-wide 5-line modal.
    let w = parent.width.min(72).max(40);
    let h: u16 = 5;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let title = if editing_idx.is_some() {
        " Edit proxy allow pattern "
    } else {
        " Add proxy allow pattern "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let hint = Line::from(vec![Span::styled(
        "POSIX extended regex matched against the request host. Enter to commit, Esc to cancel.",
        Style::default().fg(Color::DarkGray),
    )]);
    let body = Line::from(vec![Span::raw("> "), Span::raw(buffer.to_string())]);
    let para = Paragraph::new(vec![hint, Line::from(""), body]);
    f.render_widget(para, inner);
}

