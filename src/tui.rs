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
//!   The built-in `task-runner` always sits at the top of the tree; its
//!   children are editable `name = command` entries that become MCP
//!   tools for host-side command execution. `i`/`+` adds, `e`/`Enter`
//!   edits, `d` removes.
//!
//! Cross-tab:
//!
//! - `h`/`l` (or ←/→, Tab/Shift+Tab) switch between Proxy and MCP.
//! - `j`/`k` (or ↑/↓) move within the current tab.
//! - `t` toggles the scope target between Global and Workspace (the save
//!   destination). Each scope keeps its own in-memory proxy allow list so
//!   switching back and forth preserves edits.
//! - `s` saves to the currently-active scope.
//! - `q`, `Esc`, or `Ctrl+C` cancels.
//!
//! The alternate screen is entered so the prior terminal contents come
//! back untouched on exit.

use std::collections::{BTreeMap, HashMap};
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
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs};

use crate::policy::McpPolicy;
use crate::settings::Scope;

/// Single-line text buffer with readline-style editing primitives.
///
/// Stores content as a `Vec<char>` so cursor arithmetic is character- (not
/// byte-) based, which Just Works with multi-byte codepoints. Callers use
/// [`value`] to snapshot the current string and [`prefix_width`] to place
/// the terminal caret in the correct display column (unicode-width aware
/// via ratatui's `Span::width`).
///
/// [`value`]: TextField::value
/// [`prefix_width`]: TextField::prefix_width
#[derive(Clone, Default)]
struct TextField {
    chars: Vec<char>,
    cursor: usize,
}

impl TextField {
    fn from_str(s: &str) -> Self {
        let chars: Vec<char> = s.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    fn value(&self) -> String {
        self.chars.iter().collect()
    }

    fn insert_char(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete_forward(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.chars.len();
    }

    fn kill_to_end(&mut self) {
        self.chars.truncate(self.cursor);
    }

    fn kill_to_home(&mut self) {
        self.chars.drain(0..self.cursor);
        self.cursor = 0;
    }

    fn kill_word_backward(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.chars.drain(i..self.cursor);
        self.cursor = i;
    }

    fn kill_word_forward(&mut self) {
        let mut i = self.cursor;
        while i < self.chars.len() && self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < self.chars.len() && !self.chars[i].is_whitespace() {
            i += 1;
        }
        self.chars.drain(self.cursor..i);
    }

    fn move_word_left(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = i;
    }

    fn move_word_right(&mut self) {
        let mut i = self.cursor;
        while i < self.chars.len() && self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < self.chars.len() && !self.chars[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
    }

    /// Terminal cells occupied by the substring before the cursor — use
    /// this to offset the caret from the field's starting column.
    fn prefix_width(&self) -> u16 {
        let prefix: String = self.chars[..self.cursor].iter().collect();
        Span::raw(prefix).width() as u16
    }
}

/// Apply one readline-style editing key to `field`. Returns `true` if the
/// key was consumed (so the caller knows not to fall through to mode-level
/// handling such as Enter/Esc/Tab).
fn apply_editing_key(field: &mut TextField, code: KeyCode, modifiers: KeyModifiers) -> bool {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    match code {
        KeyCode::Left if alt => field.move_word_left(),
        KeyCode::Right if alt => field.move_word_right(),
        KeyCode::Left => field.move_left(),
        KeyCode::Right => field.move_right(),
        KeyCode::Home => field.move_home(),
        KeyCode::End => field.move_end(),
        KeyCode::Delete => field.delete_forward(),
        KeyCode::Backspace if ctrl || alt => field.kill_word_backward(),
        KeyCode::Backspace => field.backspace(),
        KeyCode::Char('a') if ctrl => field.move_home(),
        KeyCode::Char('e') if ctrl => field.move_end(),
        KeyCode::Char('b') if ctrl => field.move_left(),
        KeyCode::Char('f') if ctrl => field.move_right(),
        KeyCode::Char('b') if alt => field.move_word_left(),
        KeyCode::Char('f') if alt => field.move_word_right(),
        KeyCode::Char('d') if ctrl => field.delete_forward(),
        KeyCode::Char('d') if alt => field.kill_word_forward(),
        KeyCode::Char('h') if ctrl => field.backspace(),
        KeyCode::Char('k') if ctrl => field.kill_to_end(),
        KeyCode::Char('u') if ctrl => field.kill_to_home(),
        KeyCode::Char('w') if ctrl => field.kill_word_backward(),
        KeyCode::Char(c) if !ctrl && !alt => field.insert_char(c),
        _ => return false,
    }
    true
}

/// Catalog row for the MCP tab — describes a tool's identity and
/// upstream-declared safety hint. The effective enabled state is *not*
/// stored here; it is computed on the fly from the active scope's
/// [`McpPolicy`] (see [`McpState::effective_tool_allowed`]).
#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub read_only_hint: Option<bool>,
}

pub struct TuiInput {
    /// Scope the editor starts on. `t` flips it to the other scope.
    pub initial_scope: Scope,
    /// Each scope's current `proxy.allow` list as it lives on disk. Both
    /// are loaded up-front so scope-switching doesn't need to re-enter
    /// the TUI.
    pub proxy_allow_global: Vec<String>,
    pub proxy_allow_workspace: Vec<String>,
    /// Static catalog of every (server, tool) the merged settings know
    /// about — used to render the MCP tab regardless of scope.
    pub tool_catalog: Vec<ToolEntry>,
    /// Each scope's MCP policy as it lives on disk. The TUI displays the
    /// effective enabled state (Workspace view = global ∪ workspace at
    /// the tool granularity, Global view = global only) and writes
    /// toggles back into the active scope only.
    pub mcp_global: McpPolicy,
    pub mcp_workspace: McpPolicy,
    /// Each scope's `[task_runner.tasks]` map. Workspace entries shadow
    /// global ones with the same name in the merged display.
    pub tasks_global: BTreeMap<String, String>,
    pub tasks_workspace: BTreeMap<String, String>,
}

pub struct TuiOutput {
    /// Which scope was active when the user hit `s`. The save pass writes
    /// only this scope; the other scope's buffer is discarded.
    pub saved_scope: Scope,
    pub proxy_allow_global: Vec<String>,
    pub proxy_allow_workspace: Vec<String>,
    pub mcp_global: McpPolicy,
    pub mcp_workspace: McpPolicy,
    pub tasks_global: BTreeMap<String, String>,
    pub tasks_workspace: BTreeMap<String, String>,
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
        buffer: TextField,
        /// `Some(row)` when editing an existing entry; `None` for adds.
        /// The row carries the origin scope so the commit knows whether
        /// to update or refuse the write.
        editing: Option<ProxyRow>,
    },
    TaskInput {
        name: TextField,
        command: TextField,
        focus: TaskField,
        /// Original name of the task being edited, or None for a fresh
        /// add. Used on commit to delete the old key when a rename
        /// happens.
        editing: Option<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskField {
    Name,
    Command,
}

impl TaskField {
    fn toggle(self) -> Self {
        match self {
            TaskField::Name => TaskField::Command,
            TaskField::Command => TaskField::Name,
        }
    }
}

/// Origin scope of a proxy row in the merged display.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProxyOrigin {
    Global,
    Workspace,
}

impl ProxyOrigin {
    fn from_scope(scope: Scope) -> Self {
        match scope {
            Scope::Global => ProxyOrigin::Global,
            Scope::Workspace => ProxyOrigin::Workspace,
        }
    }
}

/// One row in the rendered proxy list. `idx_within_scope` points back
/// into the origin scope's `Vec<String>` so edits / deletes know where
/// to write — the merged display index isn't usable directly.
#[derive(Clone, Debug)]
struct ProxyRow {
    origin: ProxyOrigin,
    pattern: String,
    idx_within_scope: usize,
}

struct ProxyState {
    /// Each scope's allow patterns. Workspace view shows a tool-level
    /// (here, pattern-level) merge: every global pattern, then any
    /// workspace patterns that don't already appear in global. Global
    /// view shows only `global`.
    global: Vec<String>,
    workspace: Vec<String>,
    cursor: usize,
}

impl ProxyState {
    fn new(global: Vec<String>, workspace: Vec<String>) -> Self {
        Self {
            global,
            workspace,
            cursor: 0,
        }
    }

    fn list_mut(&mut self, origin: ProxyOrigin) -> &mut Vec<String> {
        match origin {
            ProxyOrigin::Global => &mut self.global,
            ProxyOrigin::Workspace => &mut self.workspace,
        }
    }

    fn visible_rows(&self, scope: Scope) -> Vec<ProxyRow> {
        let mut rows: Vec<ProxyRow> = self
            .global
            .iter()
            .enumerate()
            .map(|(i, p)| ProxyRow {
                origin: ProxyOrigin::Global,
                pattern: p.clone(),
                idx_within_scope: i,
            })
            .collect();
        if scope == Scope::Workspace {
            for (i, p) in self.workspace.iter().enumerate() {
                if !self.global.contains(p) {
                    rows.push(ProxyRow {
                        origin: ProxyOrigin::Workspace,
                        pattern: p.clone(),
                        idx_within_scope: i,
                    });
                }
            }
        }
        rows
    }

    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_down(&mut self, scope: Scope) {
        let len = self.visible_rows(scope).len();
        if self.cursor + 1 < len {
            self.cursor += 1;
        }
    }

    fn jump_home(&mut self) {
        self.cursor = 0;
    }

    fn jump_end(&mut self, scope: Scope) {
        let len = self.visible_rows(scope).len();
        self.cursor = len.saturating_sub(1);
    }

    fn current_row(&self, scope: Scope) -> Option<ProxyRow> {
        self.visible_rows(scope).into_iter().nth(self.cursor)
    }

    /// Remove the cursor's row, but only if it lives in the active
    /// scope. Global rows shown in the workspace view are inherited and
    /// cannot be deleted from here — the user has to switch to Global
    /// with `t` to remove them.
    fn remove_current(&mut self, scope: Scope) {
        let Some(row) = self.current_row(scope) else {
            return;
        };
        if row.origin != ProxyOrigin::from_scope(scope) {
            return;
        }
        let list = self.list_mut(row.origin);
        if row.idx_within_scope < list.len() {
            list.remove(row.idx_within_scope);
        }
        let len = self.visible_rows(scope).len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
    }

    /// Apply an upsert at the active scope. When `editing` points at a
    /// row owned by the active scope, replace it. When it points at a
    /// foreign-scope row (the user pressed `e` on an inherited global
    /// pattern while editing Workspace), do nothing — that case is
    /// blocked at the call site, and treating it as an add would
    /// silently fork the entry.
    fn upsert(&mut self, scope: Scope, value: String, editing: Option<ProxyRow>) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        let v = trimmed.to_string();
        let active = ProxyOrigin::from_scope(scope);
        match editing {
            Some(row) if row.origin == active => {
                let list = self.list_mut(active);
                if row.idx_within_scope < list.len() {
                    list[row.idx_within_scope] = v;
                }
            }
            Some(_) => {
                // Editing target is in the other scope: ignore.
            }
            None => {
                let list = self.list_mut(active);
                if !list.contains(&v) {
                    list.push(v);
                }
                // Move cursor onto the freshly-appended row.
                let len = self.visible_rows(scope).len();
                self.cursor = len.saturating_sub(1);
            }
        }
    }
}

#[derive(Clone)]
enum McpRow {
    TaskRunnerHeader,
    TaskRow(String),
    TaskAddHint,
    Server(usize),
    Tool(usize),
}

struct McpState {
    /// Per-scope tasks. The visible list is derived: for `Workspace` we
    /// merge `tasks_global` ∪ `tasks_workspace` (workspace wins); for
    /// `Global` we show only `tasks_global`.
    tasks_global: BTreeMap<String, String>,
    tasks_workspace: BTreeMap<String, String>,
    /// Per-scope MCP policies. Edits go into the active scope's policy
    /// only; the other one is kept untouched until the next `t` switch
    /// or save-and-quit.
    mcp_global: McpPolicy,
    mcp_workspace: McpPolicy,
    task_runner_expanded: bool,
    server_names: Vec<String>,
    /// Per-server collapse state. Initially expanded when a server has
    /// any overrides visible so the user can immediately see them.
    expanded: Vec<bool>,
    /// Static catalog of (server, tool, hint, description) tuples — the
    /// tool inventory itself doesn't change between scopes.
    catalog: Vec<ToolEntry>,
    /// Precomputed map from `server_name -> first-tool-index, tool-count`
    /// so expand/collapse doesn't have to scan the full list each frame.
    server_ranges: HashMap<String, (usize, usize)>,
    cursor: usize,
}

impl McpState {
    fn new(
        mut catalog: Vec<ToolEntry>,
        mcp_global: McpPolicy,
        mcp_workspace: McpPolicy,
        tasks_global: BTreeMap<String, String>,
        tasks_workspace: BTreeMap<String, String>,
    ) -> Self {
        catalog.sort_by(|a, b| {
            a.server_name
                .cmp(&b.server_name)
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });

        let mut server_names: Vec<String> = Vec::new();
        let mut server_ranges: HashMap<String, (usize, usize)> = HashMap::new();
        for (i, e) in catalog.iter().enumerate() {
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
            tasks_global,
            tasks_workspace,
            mcp_global,
            mcp_workspace,
            task_runner_expanded: true,
            server_names,
            expanded,
            catalog,
            server_ranges,
            cursor: 0,
        }
    }

    /// Tasks visible for `scope`. Workspace shows the merged view
    /// (workspace overlay wins on collisions); Global shows only its
    /// own map.
    fn effective_tasks(&self, scope: Scope) -> BTreeMap<String, String> {
        match scope {
            Scope::Global => self.tasks_global.clone(),
            Scope::Workspace => {
                let mut merged = self.tasks_global.clone();
                for (k, v) in &self.tasks_workspace {
                    merged.insert(k.clone(), v.clone());
                }
                merged
            }
        }
    }

    /// Whether a task at the given key is currently overridden in the
    /// active scope (used for the `[W]` annotation while editing
    /// Workspace).
    fn task_is_workspace_override(&self, name: &str) -> bool {
        self.tasks_workspace.contains_key(name)
    }

    /// Effective enabled state for the indexed catalog entry under the
    /// active scope. Workspace mode does a *tool-level* merge (workspace
    /// override wins, otherwise fall through to global) so the user sees
    /// "what would actually be enabled if I saved right now".
    fn effective_tool_allowed(&self, scope: Scope, idx: usize) -> bool {
        let entry = &self.catalog[idx];
        match scope {
            Scope::Global => self.mcp_global.tool_allowed(
                &entry.server_name,
                &entry.tool_name,
                entry.read_only_hint,
            ),
            Scope::Workspace => {
                if let Some(ws_server) = self.mcp_workspace.servers.get(&entry.server_name) {
                    if !ws_server.enabled {
                        return false;
                    }
                    if let Some(t) = ws_server.tools.get(&entry.tool_name) {
                        return *t;
                    }
                }
                self.mcp_global.tool_allowed(
                    &entry.server_name,
                    &entry.tool_name,
                    entry.read_only_hint,
                )
            }
        }
    }

    /// Whether the (server, tool) at `idx` carries an explicit
    /// per-tool entry in the workspace policy. Used for the `[W]`
    /// annotation that distinguishes "inherited from global" from
    /// "overridden here".
    fn tool_is_workspace_override(&self, idx: usize) -> bool {
        let entry = &self.catalog[idx];
        self.mcp_workspace
            .servers
            .get(&entry.server_name)
            .and_then(|sp| sp.tools.get(&entry.tool_name))
            .is_some()
    }

    /// Apply a desired enabled state to the indexed catalog entry under
    /// `scope`. Writes through the policy's `set_tool` so the change is
    /// always representable; the save pass minimises redundant entries
    /// against the inheritance base afterwards.
    fn set_tool_for(&mut self, scope: Scope, idx: usize, enabled: bool) {
        let entry = &self.catalog[idx];
        let policy = match scope {
            Scope::Global => &mut self.mcp_global,
            Scope::Workspace => &mut self.mcp_workspace,
        };
        policy.set_tool(&entry.server_name, &entry.tool_name, enabled);
    }

    /// Flat list of currently-visible rows (respecting expanded state)
    /// for the active scope.
    fn visible_rows(&self, scope: Scope) -> Vec<McpRow> {
        let mut rows = Vec::new();
        rows.push(McpRow::TaskRunnerHeader);
        if self.task_runner_expanded {
            for name in self.effective_tasks(scope).keys() {
                rows.push(McpRow::TaskRow(name.clone()));
            }
            rows.push(McpRow::TaskAddHint);
        }
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

    fn current_row(&self, scope: Scope) -> Option<McpRow> {
        self.visible_rows(scope).get(self.cursor).cloned()
    }

    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_down(&mut self, scope: Scope) {
        let max = self.visible_rows(scope).len();
        if self.cursor + 1 < max {
            self.cursor += 1;
        }
    }

    fn jump_home(&mut self) {
        self.cursor = 0;
    }

    fn jump_end(&mut self, scope: Scope) {
        let len = self.visible_rows(scope).len();
        self.cursor = len.saturating_sub(1);
    }

    /// Toggle the currently-focused row. Returns a [`RowAction`] when the
    /// row can't handle the toggle locally (e.g. a task row needs the
    /// outer event loop to spawn an input modal).
    fn toggle(&mut self, scope: Scope) -> RowAction {
        match self.current_row(scope) {
            Some(McpRow::TaskRunnerHeader) => {
                self.task_runner_expanded = !self.task_runner_expanded;
                RowAction::Handled
            }
            Some(McpRow::Server(si)) => {
                self.expanded[si] = !self.expanded[si];
                RowAction::Handled
            }
            Some(McpRow::Tool(ti)) => {
                let cur = self.effective_tool_allowed(scope, ti);
                self.set_tool_for(scope, ti, !cur);
                RowAction::Handled
            }
            Some(McpRow::TaskRow(name)) => RowAction::EditTask(name),
            Some(McpRow::TaskAddHint) => RowAction::AddTask,
            None => RowAction::Handled,
        }
    }

    fn toggle_all_in_focused_server(&mut self, scope: Scope, enable: bool) {
        let server_idx = match self.current_row(scope) {
            Some(McpRow::Server(si)) => si,
            Some(McpRow::Tool(ti)) => self
                .server_names
                .iter()
                .position(|n| n == &self.catalog[ti].server_name)
                .unwrap_or(0),
            _ => return,
        };
        let Some(name) = self.server_names.get(server_idx).cloned() else {
            return;
        };
        if let Some((start, count)) = self.server_ranges.get(&name).copied() {
            for i in start..(start + count) {
                self.set_tool_for(scope, i, enable);
            }
        }
    }

    /// Delete the task focused by the cursor under `scope`. Workspace
    /// only deletes the workspace-side entry — a global-only task stays
    /// visible (the user has to switch to Global to remove it). Visible
    /// for the user via the `[W]` annotation in the row.
    fn delete_task_at_cursor(&mut self, scope: Scope) {
        let Some(McpRow::TaskRow(name)) = self.current_row(scope) else {
            return;
        };
        match scope {
            Scope::Global => {
                self.tasks_global.remove(&name);
            }
            Scope::Workspace => {
                self.tasks_workspace.remove(&name);
            }
        };
        let len = self.visible_rows(scope).len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
    }

    fn set_task_for(&mut self, scope: Scope, name: String, command: String) {
        match scope {
            Scope::Global => self.tasks_global.insert(name, command),
            Scope::Workspace => self.tasks_workspace.insert(name, command),
        };
    }

    fn task_command_for(&self, scope: Scope, name: &str) -> Option<String> {
        // For Workspace the editor preloads the merged value (so editing
        // a global-only task starts from its global definition), so the
        // user sees the same value the merged display showed them.
        match scope {
            Scope::Global => self.tasks_global.get(name).cloned(),
            Scope::Workspace => self
                .tasks_workspace
                .get(name)
                .or_else(|| self.tasks_global.get(name))
                .cloned(),
        }
    }

    fn enabled_count_for(&self, scope: Scope, server_idx: usize) -> (usize, usize) {
        let Some(name) = self.server_names.get(server_idx) else {
            return (0, 0);
        };
        let Some((start, count)) = self.server_ranges.get(name).copied() else {
            return (0, 0);
        };
        let enabled = (start..start + count)
            .filter(|i| self.effective_tool_allowed(scope, *i))
            .count();
        (enabled, count)
    }
}

/// Outcome of invoking the toggle action on an MCP row. Task rows need
/// the outer event loop to spawn an input modal (can't be done inside
/// `&mut self` without borrowing the App).
enum RowAction {
    Handled,
    EditTask(String),
    AddTask,
}

struct App {
    scope: Scope,
    tab: TopTab,
    /// Holds both scopes' allow lists; the visible rows are derived
    /// from `scope`. Cursor is on the rendered (merged) view.
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
            scope: input.initial_scope,
            tab: TopTab::Proxy,
            proxy: ProxyState::new(input.proxy_allow_global, input.proxy_allow_workspace),
            mcp: McpState::new(
                input.tool_catalog,
                input.mcp_global,
                input.mcp_workspace,
                input.tasks_global,
                input.tasks_workspace,
            ),
            mode: Mode::Normal,
            list_state,
        }
    }

    fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            Scope::Global => Scope::Workspace,
            Scope::Workspace => Scope::Global,
        };
        // Keep the cursor inside the new visible-row count for whichever
        // panel happens to be active. The MCP cursor is naturally bounded
        // by visible_rows(); for the proxy panel we re-clamp here so an
        // out-of-range cursor doesn't render off-list.
        if self.tab == TopTab::Proxy {
            let len = self.proxy.visible_rows(self.scope).len();
            if self.proxy.cursor >= len {
                self.proxy.cursor = len.saturating_sub(1);
            }
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
            saved_scope: self.scope,
            proxy_allow_global: self.proxy.global,
            proxy_allow_workspace: self.proxy.workspace,
            mcp_global: self.mcp.mcp_global,
            mcp_workspace: self.mcp.mcp_workspace,
            tasks_global: self.mcp.tasks_global,
            tasks_workspace: self.mcp.tasks_workspace,
        }
    }
}

fn handle_proxy_input_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    // Pull the current input buffer out first so we can mutate `app.mode`
    // (to Normal on commit/cancel) without aliasing the same borrow.
    let Mode::ProxyInput {
        mut buffer,
        editing,
    } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return;
    };

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Esc => return,
        KeyCode::Char('c') if ctrl => return,
        KeyCode::Enter => {
            app.proxy.upsert(app.scope, buffer.value(), editing);
            return;
        }
        _ => {
            apply_editing_key(&mut buffer, code, modifiers);
        }
    }

    app.mode = Mode::ProxyInput { buffer, editing };
}

fn handle_task_input_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Mode::TaskInput {
        mut name,
        mut command,
        mut focus,
        editing,
    } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return;
    };

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Esc => return,
        KeyCode::Char('c') if ctrl => return,
        KeyCode::Enter => {
            let name_tr = name.value().trim().to_string();
            let cmd_tr = command.value().trim().to_string();
            if name_tr.is_empty() || cmd_tr.is_empty() {
                // Nudge focus back to the empty field and stay in input mode.
                focus = if name_tr.is_empty() {
                    TaskField::Name
                } else {
                    TaskField::Command
                };
            } else {
                let scope = app.scope;
                // Rename clears the old key in the active scope's map.
                // (A workspace rename never touches the global map — the
                // global definition stays put as the inheritance fallback.)
                if let Some(orig) = &editing {
                    if orig != &name_tr {
                        match scope {
                            Scope::Global => {
                                app.mcp.tasks_global.remove(orig);
                            }
                            Scope::Workspace => {
                                app.mcp.tasks_workspace.remove(orig);
                            }
                        }
                    }
                }
                app.mcp.set_task_for(scope, name_tr, cmd_tr);
                return;
            }
        }
        // Tab / Up / Down switch focus between the two fields. Up/Down
        // have no in-line meaning on a single-line field, so we repurpose
        // them for field navigation — matching most form-style TUIs.
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
            focus = focus.toggle();
        }
        _ => {
            let target = match focus {
                TaskField::Name => &mut name,
                TaskField::Command => &mut command,
            };
            apply_editing_key(target, code, modifiers);
        }
    }

    app.mode = Mode::TaskInput {
        name,
        command,
        focus,
        editing,
    };
}

fn start_task_edit(app: &mut App, name: String) {
    let command = app
        .mcp
        .task_command_for(app.scope, &name)
        .unwrap_or_default();
    app.mode = Mode::TaskInput {
        name: TextField::from_str(&name),
        command: TextField::from_str(&command),
        focus: TaskField::Command,
        editing: Some(name),
    };
}

fn start_task_add(app: &mut App) {
    app.mode = Mode::TaskInput {
        name: TextField::default(),
        command: TextField::default(),
        focus: TaskField::Name,
        editing: None,
    };
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
        if matches!(app.mode, Mode::TaskInput { .. }) {
            handle_task_input_key(&mut app, key.code, key.modifiers);
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
            KeyCode::Char('t') => app.toggle_scope(),
            KeyCode::Up | KeyCode::Char('k') => match app.tab {
                TopTab::Proxy => app.proxy.move_up(),
                TopTab::Mcp => app.mcp.move_up(),
            },
            KeyCode::Down | KeyCode::Char('j') => match app.tab {
                TopTab::Proxy => app.proxy.move_down(app.scope),
                TopTab::Mcp => app.mcp.move_down(app.scope),
            },
            KeyCode::Home | KeyCode::Char('g') => match app.tab {
                TopTab::Proxy => app.proxy.jump_home(),
                TopTab::Mcp => app.mcp.jump_home(),
            },
            KeyCode::End | KeyCode::Char('G') => match app.tab {
                TopTab::Proxy => app.proxy.jump_end(app.scope),
                TopTab::Mcp => app.mcp.jump_end(app.scope),
            },
            KeyCode::Char(' ') | KeyCode::Enter => match app.tab {
                TopTab::Proxy => {
                    if let Some(row) = app.proxy.current_row(app.scope) {
                        // Inherited (global) rows shown in the workspace
                        // view are read-only here — `t` to switch scope
                        // first.
                        if row.origin == ProxyOrigin::from_scope(app.scope) {
                            app.mode = Mode::ProxyInput {
                                buffer: TextField::from_str(&row.pattern),
                                editing: Some(row),
                            };
                        }
                    }
                }
                TopTab::Mcp => match app.mcp.toggle(app.scope) {
                    RowAction::Handled => {}
                    RowAction::EditTask(name) => start_task_edit(&mut app, name),
                    RowAction::AddTask => start_task_add(&mut app),
                },
            },
            KeyCode::Char('i') | KeyCode::Char('+') if app.tab == TopTab::Proxy => {
                app.mode = Mode::ProxyInput {
                    buffer: TextField::default(),
                    editing: None,
                };
            }
            KeyCode::Char('i') | KeyCode::Char('+') if app.tab == TopTab::Mcp => {
                start_task_add(&mut app);
            }
            KeyCode::Char('e') if app.tab == TopTab::Proxy => {
                if let Some(row) = app.proxy.current_row(app.scope) {
                    if row.origin == ProxyOrigin::from_scope(app.scope) {
                        app.mode = Mode::ProxyInput {
                            buffer: TextField::from_str(&row.pattern),
                            editing: Some(row),
                        };
                    }
                }
            }
            KeyCode::Char('e') if app.tab == TopTab::Mcp => {
                if let Some(McpRow::TaskRow(name)) = app.mcp.current_row(app.scope) {
                    start_task_edit(&mut app, name);
                }
            }
            KeyCode::Char('d') if app.tab == TopTab::Proxy => {
                app.proxy.remove_current(app.scope);
            }
            KeyCode::Char('d') if app.tab == TopTab::Mcp => {
                app.mcp.delete_task_at_cursor(app.scope);
            }
            KeyCode::Char('a') if app.tab == TopTab::Mcp => {
                app.mcp.toggle_all_in_focused_server(app.scope, true);
            }
            KeyCode::Char('A') if app.tab == TopTab::Mcp => {
                app.mcp.toggle_all_in_focused_server(app.scope, false);
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

    if let Mode::TaskInput {
        ref name,
        ref command,
        focus,
        ref editing,
    } = app.mode
    {
        render_task_input_modal(f, area, name, command, focus, editing.is_some());
    }

    // Overlay modal for proxy input.
    if let Mode::ProxyInput {
        ref buffer,
        ref editing,
    } = app.mode
    {
        render_proxy_input_modal(f, area, buffer, editing.is_some());
    }
}

fn render_title(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let scope_label = match app.scope {
        Scope::Global => "Global",
        Scope::Workspace => "Workspace",
    };
    // Brand tag uses a deep blue so it doesn't collide with the active-tab
    // highlight below (cyan was ambiguous with the old tab style).
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " agent-container ",
            Style::default()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  settings  "),
        Span::styled(
            format!("[{scope_label}]"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  (t to switch scope)",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    f.render_widget(title, area);
}

fn render_tabs(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // Pad labels so the highlight background feels like a chip instead of a
    // bare word, then drop the default "|" divider — the coloured background
    // on the active tab is already enough of a separator.
    let titles: Vec<Line> = TopTab::titles()
        .iter()
        .map(|s| Line::from(Span::raw(format!(" {s} "))))
        .collect();
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::BOTTOM))
        .select(app.tab.index())
        .divider("")
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
    let scope = app.scope;
    let rows = app.proxy.visible_rows(scope);
    let active = ProxyOrigin::from_scope(scope);
    let items: Vec<ListItem> = if rows.is_empty() {
        let hint = match scope {
            Scope::Global => "  (no global allow patterns; press `i` to add)",
            Scope::Workspace => "  (no allow patterns inherited or set here; press `i` to add)",
        };
        vec![ListItem::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        rows.iter()
            .map(|row| {
                let overlay = scope == Scope::Workspace && row.origin == ProxyOrigin::Workspace;
                let is_inherited = row.origin != active;
                let pattern_style = if is_inherited {
                    // Slightly dim the inherited rows so the scope they
                    // belong to is obvious without an extra marker.
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if overlay { "* " } else { "  " }.to_string(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(row.pattern.clone(), pattern_style),
                ]))
            })
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
    let scope = app.scope;
    let rows = app.mcp.visible_rows(scope);
    let visible_tasks = app.mcp.effective_tasks(scope);
    let items: Vec<ListItem> = rows
        .into_iter()
        .map(|row| match row {
            McpRow::TaskRunnerHeader => {
                let marker = if app.mcp.task_runner_expanded {
                    "▾"
                } else {
                    "▸"
                };
                let count = visible_tasks.len();
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{marker} task-runner"),
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  ({count} task{})", if count == 1 { "" } else { "s" }),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        "  host commands exposed as MCP tools",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            }
            McpRow::TaskRow(name) => {
                let command = visible_tasks.get(&name).cloned().unwrap_or_default();
                let overlay = scope == Scope::Workspace
                    && app.mcp.task_is_workspace_override(&name);
                ListItem::new(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        if overlay { "* " } else { "  " }.to_string(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(name, Style::default().fg(Color::Cyan)),
                    Span::raw(" = "),
                    Span::styled(command, Style::default().fg(Color::White)),
                ]))
            }
            McpRow::TaskAddHint => ListItem::new(Line::from(vec![
                Span::raw("      "),
                Span::styled(
                    "+ add task (i)",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ])),
            McpRow::Server(si) => {
                let name = &app.mcp.server_names[si];
                let (enabled, total) = app.mcp.enabled_count_for(scope, si);
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
            McpRow::Tool(ti) => render_tool_row(
                &app.mcp.catalog[ti],
                app.mcp.effective_tool_allowed(scope, ti),
                scope == Scope::Workspace && app.mcp.tool_is_workspace_override(ti),
            ),
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

/// `mark_overlay` paints a small `*` in front of the tool name when the
/// active scope owns an explicit per-tool entry (so the user can see
/// which checkbox states are inherited from global vs. overridden in
/// workspace).
fn render_tool_row(entry: &ToolEntry, enabled: bool, mark_overlay: bool) -> ListItem<'static> {
    let cb = if enabled { "[x]" } else { "[ ]" };
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
        Span::styled(
            if mark_overlay { "* " } else { "  " }.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
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
            key("t", Color::Yellow),
            Span::raw(" scope · "),
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
            Span::raw(" toggle · "),
            key("i/e/d", Color::Cyan),
            Span::raw(" task add/edit/del · "),
            key("a/A", Color::Cyan),
            Span::raw(" bulk · "),
            key("t", Color::Yellow),
            Span::raw(" scope · "),
            key("s", Color::Green),
            Span::raw(" save · "),
            key("q", Color::Red),
            Span::raw(" cancel"),
        ]),
    };

    let status = match app.tab {
        TopTab::Proxy => Line::from(vec![Span::styled(
            format!(
                "Global: {} · Workspace: {} allow pattern(s)",
                app.proxy.global.len(),
                app.proxy.workspace.len(),
            ),
            Style::default().fg(Color::DarkGray),
        )]),
        TopTab::Mcp => {
            let total = app.mcp.catalog.len();
            let enabled = (0..total)
                .filter(|i| app.mcp.effective_tool_allowed(app.scope, *i))
                .count();
            let task_count = app.mcp.effective_tasks(app.scope).len();
            Line::from(vec![Span::styled(
                format!(
                    "{task_count} task(s) · {enabled}/{total} tool(s) enabled across {} server(s)",
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
    buffer: &TextField,
    is_edit: bool,
) {
    // Centered 60-char-wide 5-line modal.
    let w = parent.width.min(72).max(40);
    let h: u16 = 5;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let title = if is_edit {
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
        "POSIX extended regex. Enter commit · Esc cancel · readline keys (^A/^E/^W/M-b/M-f…)",
        Style::default().fg(Color::DarkGray),
    )]);
    let body = Line::from(vec![Span::raw("> "), Span::raw(buffer.value())]);
    let para = Paragraph::new(vec![hint, Line::from(""), body]);
    f.render_widget(para, inner);

    // Place the terminal caret after the "> " prefix plus whatever the
    // buffer has already consumed up to the logical cursor.
    let cursor_x = inner.x + 2 + buffer.prefix_width();
    let cursor_y = inner.y + 2;
    f.set_cursor_position(Position::new(cursor_x, cursor_y));
}

fn render_task_input_modal(
    f: &mut ratatui::Frame<'_>,
    parent: Rect,
    name: &TextField,
    command: &TextField,
    focus: TaskField,
    is_edit: bool,
) {
    let w = parent.width.min(80).max(50);
    let h: u16 = 8;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let title = if is_edit {
        " Edit task "
    } else {
        " Add task "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(Color::Magenta));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let focus_style = |f: TaskField, row: TaskField| {
        if f == row {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    };

    let hint = Line::from(vec![Span::styled(
        "Tab/↑↓ switch · Enter commit · Esc cancel · readline keys (^A/^E/^W/M-b/M-f…)",
        Style::default().fg(Color::DarkGray),
    )]);
    let name_line = Line::from(vec![
        Span::styled(" name    ", focus_style(focus, TaskField::Name)),
        Span::raw("  "),
        Span::raw(name.value()),
    ]);
    let cmd_line = Line::from(vec![
        Span::styled(" command ", focus_style(focus, TaskField::Command)),
        Span::raw("  "),
        Span::raw(command.value()),
    ]);
    let para = Paragraph::new(vec![hint, Line::from(""), name_line, Line::from(""), cmd_line]);
    f.render_widget(para, inner);

    // Field text starts 11 cells in from the modal's inner-left: 9-char
    // label (" name    " / " command ") + 2-space separator. The hint sits
    // on row 0, a blank row on 1, so the fields are at rows 2 and 4.
    let (active_field, row) = match focus {
        TaskField::Name => (name, 2),
        TaskField::Command => (command, 4),
    };
    let cursor_x = inner.x + 11 + active_field.prefix_width();
    let cursor_y = inner.y + row;
    f.set_cursor_position(Position::new(cursor_x, cursor_y));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_field_insert_backspace_and_cursor_track() {
        let mut f = TextField::default();
        f.insert_char('a');
        f.insert_char('b');
        f.insert_char('c');
        assert_eq!(f.value(), "abc");
        assert_eq!(f.cursor, 3);
        f.backspace();
        assert_eq!(f.value(), "ab");
        f.move_left();
        f.backspace();
        assert_eq!(f.value(), "b");
        assert_eq!(f.cursor, 0);
    }

    #[test]
    fn text_field_from_str_puts_cursor_at_end() {
        let f = TextField::from_str("hello");
        assert_eq!(f.cursor, 5);
        assert_eq!(f.value(), "hello");
    }

    #[test]
    fn text_field_home_end_and_delete_forward() {
        let mut f = TextField::from_str("hello");
        f.move_home();
        assert_eq!(f.cursor, 0);
        f.delete_forward();
        assert_eq!(f.value(), "ello");
        f.move_end();
        assert_eq!(f.cursor, 4);
        f.delete_forward(); // past-end should be a no-op
        assert_eq!(f.value(), "ello");
    }

    #[test]
    fn text_field_kill_to_end_and_home() {
        let mut f = TextField::from_str("hello world");
        for _ in 0..5 {
            f.move_left();
        }
        f.kill_to_end();
        assert_eq!(f.value(), "hello ");

        let mut f = TextField::from_str("hello world");
        for _ in 0..5 {
            f.move_left();
        }
        f.kill_to_home();
        assert_eq!(f.value(), "world");
        assert_eq!(f.cursor, 0);
    }

    #[test]
    fn text_field_word_navigation_hops_whitespace() {
        let mut f = TextField::from_str("foo bar  baz");
        f.move_word_left();
        assert_eq!(f.cursor, 9); // start of "baz"
        f.move_word_left();
        assert_eq!(f.cursor, 4); // start of "bar"
        f.move_word_right();
        assert_eq!(f.cursor, 7); // end of "bar"
    }

    #[test]
    fn text_field_kill_word_backward_and_forward() {
        let mut f = TextField::from_str("foo bar baz");
        f.kill_word_backward();
        assert_eq!(f.value(), "foo bar ");
        f.kill_word_backward();
        assert_eq!(f.value(), "foo ");

        let mut f = TextField::from_str("foo bar baz");
        f.move_home();
        f.kill_word_forward();
        assert_eq!(f.value(), " bar baz");
        f.kill_word_forward();
        assert_eq!(f.value(), " baz");
    }

    #[test]
    fn text_field_edits_multibyte_per_char_not_per_byte() {
        let mut f = TextField::from_str("日本語");
        assert_eq!(f.cursor, 3);
        f.backspace();
        assert_eq!(f.value(), "日本");
        f.move_home();
        f.delete_forward();
        assert_eq!(f.value(), "本");
    }

    #[test]
    fn apply_editing_key_dispatches_common_readline_bindings() {
        let mut f = TextField::from_str("hello");
        assert!(apply_editing_key(
            &mut f,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL
        ));
        assert_eq!(f.cursor, 0);
        assert!(apply_editing_key(
            &mut f,
            KeyCode::Char('e'),
            KeyModifiers::CONTROL
        ));
        assert_eq!(f.cursor, 5);
        assert!(apply_editing_key(
            &mut f,
            KeyCode::Char('k'),
            KeyModifiers::CONTROL
        ));
        // At end-of-buffer, kill-to-end is a no-op.
        assert_eq!(f.value(), "hello");
        apply_editing_key(&mut f, KeyCode::Char('a'), KeyModifiers::CONTROL);
        apply_editing_key(&mut f, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(f.value(), "");

        // Plain 'a' (no modifiers) inserts.
        let mut f = TextField::default();
        assert!(apply_editing_key(
            &mut f,
            KeyCode::Char('a'),
            KeyModifiers::NONE
        ));
        assert_eq!(f.value(), "a");
    }

    #[test]
    fn apply_editing_key_ignores_unmapped_ctrl_combos() {
        // Ctrl+Z isn't bound — must return false so the outer event loop
        // can fall through without the field silently absorbing a 'z'.
        let mut f = TextField::from_str("x");
        assert!(!apply_editing_key(
            &mut f,
            KeyCode::Char('z'),
            KeyModifiers::CONTROL
        ));
        assert_eq!(f.value(), "x");
    }

    fn make_state(
        catalog: Vec<ToolEntry>,
        mcp_global: McpPolicy,
        mcp_workspace: McpPolicy,
        tasks_global: BTreeMap<String, String>,
        tasks_workspace: BTreeMap<String, String>,
    ) -> McpState {
        McpState::new(
            catalog,
            mcp_global,
            mcp_workspace,
            tasks_global,
            tasks_workspace,
        )
    }

    fn entry(server: &str, tool: &str, ro: Option<bool>) -> ToolEntry {
        ToolEntry {
            server_name: server.to_string(),
            tool_name: tool.to_string(),
            description: String::new(),
            read_only_hint: ro,
        }
    }

    #[test]
    fn effective_tool_allowed_workspace_falls_through_to_global() {
        // Global has an explicit override that flips the read_only_hint
        // default (a writable tool turned on). Workspace has no entry —
        // workspace mode should still report it enabled.
        let mut g = McpPolicy::default();
        g.set_tool("github", "create_issue", true);
        let state = make_state(
            vec![entry("github", "create_issue", Some(false))],
            g,
            McpPolicy::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert!(state.effective_tool_allowed(Scope::Global, 0));
        assert!(state.effective_tool_allowed(Scope::Workspace, 0));
    }

    #[test]
    fn effective_tool_allowed_workspace_override_wins() {
        // Global says enabled; workspace explicitly turns it off.
        let mut g = McpPolicy::default();
        g.set_tool("s", "t", true);
        let mut w = McpPolicy::default();
        w.set_tool("s", "t", false);
        let state = make_state(
            vec![entry("s", "t", Some(true))],
            g,
            w,
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert!(state.effective_tool_allowed(Scope::Global, 0));
        assert!(!state.effective_tool_allowed(Scope::Workspace, 0));
    }

    #[test]
    fn set_tool_for_targets_only_active_scope() {
        let state_seed = || {
            make_state(
                vec![entry("s", "t", Some(false))],
                McpPolicy::default(),
                McpPolicy::default(),
                BTreeMap::new(),
                BTreeMap::new(),
            )
        };
        // Global toggle writes to mcp_global, leaves mcp_workspace empty.
        let mut s = state_seed();
        s.set_tool_for(Scope::Global, 0, true);
        assert!(s.mcp_global.servers.get("s").is_some());
        assert!(s.mcp_workspace.servers.get("s").is_none());

        // Workspace toggle writes to mcp_workspace, leaves mcp_global empty.
        let mut s = state_seed();
        s.set_tool_for(Scope::Workspace, 0, true);
        assert!(s.mcp_global.servers.get("s").is_none());
        assert!(s.mcp_workspace.servers.get("s").is_some());
    }

    #[test]
    fn effective_tasks_show_global_only_for_global_scope() {
        let mut tg = BTreeMap::new();
        tg.insert("a".to_string(), "echo a".to_string());
        let mut tw = BTreeMap::new();
        tw.insert("b".to_string(), "echo b".to_string());
        let state = make_state(
            vec![],
            McpPolicy::default(),
            McpPolicy::default(),
            tg,
            tw,
        );
        let g = state.effective_tasks(Scope::Global);
        assert_eq!(g.len(), 1);
        assert!(g.contains_key("a"));
        assert!(!g.contains_key("b"));

        let w = state.effective_tasks(Scope::Workspace);
        assert_eq!(w.len(), 2);
        assert_eq!(w.get("a").unwrap(), "echo a");
        assert_eq!(w.get("b").unwrap(), "echo b");
    }

    #[test]
    fn effective_tasks_workspace_overrides_global_on_collision() {
        let mut tg = BTreeMap::new();
        tg.insert("k".to_string(), "global".to_string());
        let mut tw = BTreeMap::new();
        tw.insert("k".to_string(), "workspace".to_string());
        let state = make_state(
            vec![],
            McpPolicy::default(),
            McpPolicy::default(),
            tg,
            tw,
        );
        assert_eq!(
            state.effective_tasks(Scope::Workspace).get("k").unwrap(),
            "workspace",
        );
    }

    #[test]
    fn proxy_visible_rows_global_view_shows_only_global() {
        let p = ProxyState::new(
            vec!["g1".into(), "g2".into()],
            vec!["w1".into()],
        );
        let rows = p.visible_rows(Scope::Global);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.origin == ProxyOrigin::Global));
        let patterns: Vec<&str> = rows.iter().map(|r| r.pattern.as_str()).collect();
        assert_eq!(patterns, ["g1", "g2"]);
    }

    #[test]
    fn proxy_visible_rows_workspace_view_appends_workspace_only() {
        // workspace contains one duplicate of global ("g1") and one
        // workspace-only entry ("w1"). The merge dedupes the duplicate.
        let p = ProxyState::new(
            vec!["g1".into(), "g2".into()],
            vec!["g1".into(), "w1".into()],
        );
        let rows = p.visible_rows(Scope::Workspace);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].origin, ProxyOrigin::Global);
        assert_eq!(rows[0].pattern, "g1");
        assert_eq!(rows[1].origin, ProxyOrigin::Global);
        assert_eq!(rows[1].pattern, "g2");
        assert_eq!(rows[2].origin, ProxyOrigin::Workspace);
        assert_eq!(rows[2].pattern, "w1");
    }

    #[test]
    fn proxy_remove_current_workspace_view_skips_global_rows() {
        let mut p = ProxyState::new(
            vec!["g1".into()],
            vec!["w1".into()],
        );
        // cursor on the global row (index 0): delete should be a no-op.
        p.cursor = 0;
        p.remove_current(Scope::Workspace);
        assert_eq!(p.global, vec!["g1".to_string()]);
        assert_eq!(p.workspace, vec!["w1".to_string()]);

        // cursor on the workspace row (index 1): deletes from workspace.
        p.cursor = 1;
        p.remove_current(Scope::Workspace);
        assert_eq!(p.global, vec!["g1".to_string()]);
        assert!(p.workspace.is_empty());
    }

    #[test]
    fn proxy_upsert_workspace_does_not_touch_global() {
        let mut p = ProxyState::new(
            vec!["g1".into()],
            vec!["w1".into()],
        );
        // Edit the workspace row in workspace view.
        p.cursor = 1;
        let row = p.current_row(Scope::Workspace).unwrap();
        p.upsert(Scope::Workspace, "w1-renamed".to_string(), Some(row));
        assert_eq!(p.global, vec!["g1".to_string()]);
        assert_eq!(p.workspace, vec!["w1-renamed".to_string()]);

        // Add a new workspace entry; global stays untouched.
        p.upsert(Scope::Workspace, "w2".to_string(), None);
        assert_eq!(p.global, vec!["g1".to_string()]);
        assert_eq!(p.workspace, vec!["w1-renamed".to_string(), "w2".to_string()]);
    }

    #[test]
    fn proxy_upsert_dedupes_within_active_scope() {
        let mut p = ProxyState::new(vec![], vec!["w1".into()]);
        p.upsert(Scope::Workspace, "w1".to_string(), None);
        // Already present, must not be re-appended.
        assert_eq!(p.workspace, vec!["w1".to_string()]);
    }

    #[test]
    fn tool_is_workspace_override_only_true_when_workspace_has_explicit_entry() {
        let mut g = McpPolicy::default();
        g.set_tool("s", "t", true);
        let state = make_state(
            vec![entry("s", "t", Some(true))],
            g,
            McpPolicy::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert!(!state.tool_is_workspace_override(0));

        let mut w = McpPolicy::default();
        w.set_tool("s", "t", false);
        let state = make_state(
            vec![entry("s", "t", Some(true))],
            McpPolicy::default(),
            w,
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert!(state.tool_is_workspace_override(0));
    }
}

