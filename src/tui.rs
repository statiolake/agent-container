//! Full-screen ratatui UI for `agent-container config`.
//!
//! The window has two top-level tabs:
//!
//! - **General** — simple runtime defaults such as which agent
//!   `agent-container run` starts when `--agent` is omitted.
//! - **Proxy** — a scope-local list of tinyproxy allow regex patterns.
//! - **Filesystem** — a tree of host path mounts and filter patterns.
//! - **MCP (Claude Code)** and **MCP (Codex)** — collapsible trees of
//!   task-runner commands and servers → tools. `Enter` activates the
//!   highlighted row (expand/collapse, edit, add, or toggle).
//!   The built-in `task-runner` always sits at the top of the tree; its
//!   children are editable `name = command` entries that become MCP
//!   tools for host-side command execution.
//!
//! Cross-tab:
//!
//! - ←/→ or `h`/`l` (or Tab/Shift+Tab) switch tabs.
//! - ↑/↓ or `j`/`k` move within the current tab.
//! - `Enter` activates the highlighted row.
//! - `a` adds an item in the highlighted row's context.
//! - `d` removes the highlighted row when it is owned by the active scope.
//! - `s` saves the active scope and exits.
//! - `t` toggles the scope target between Global and Workspace (the save
//!   destination). Each scope keeps its own in-memory proxy allow list so
//!   switching back and forth preserves edits.
//! - `q` opens the save/discard/continue exit dialog.
//!
//! The alternate screen is entered so the prior terminal contents come
//! back untouched on exit.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
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
use crate::settings::{
    ClaudePolicy, DefaultAgent, FilesystemMount, FilesystemPolicy, GeneralPolicy, Scope,
};

fn plain() -> Style {
    Style::default()
}

fn muted() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn heading() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn selected() -> Style {
    Style::default().bg(Color::DarkGray).fg(Color::White)
}

fn selected_bold() -> Style {
    selected().add_modifier(Modifier::BOLD)
}

fn danger() -> Style {
    Style::default().fg(Color::Red)
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAgent {
    Claude,
    Codex,
}

#[derive(Debug, Clone)]
pub struct McpServerEntry {
    pub name: String,
    pub transport: String,
}

#[derive(Debug, Clone)]
pub enum McpCatalogEvent {
    Loading {
        agent: McpAgent,
        server_name: String,
    },
    Loaded {
        agent: McpAgent,
        server_name: String,
        tools: Vec<ToolEntry>,
    },
    Failed {
        agent: McpAgent,
        server_name: String,
        message: String,
        can_auth: bool,
    },
}

#[derive(Debug, Clone)]
pub enum McpCatalogCommand {
    Reload {
        agent: McpAgent,
        server_name: String,
    },
    Auth {
        agent: McpAgent,
        server_name: String,
    },
}

pub struct TuiInput {
    pub workspace: PathBuf,
    /// Scope the editor starts on. `t` flips it to the other scope.
    pub initial_scope: Scope,
    /// Each scope's current `proxy.allow` list as it lives on disk. Both
    /// are loaded up-front so scope-switching doesn't need to re-enter
    /// the TUI.
    pub general_global: GeneralPolicy,
    pub general_workspace: GeneralPolicy,
    pub claude_global: ClaudePolicy,
    pub claude_workspace: ClaudePolicy,
    pub proxy_allow_global: Vec<String>,
    pub proxy_allow_workspace: Vec<String>,
    pub filesystem_global: FilesystemPolicy,
    pub filesystem_workspace: FilesystemPolicy,
    /// Declared MCP servers. Tool catalogs may arrive asynchronously.
    pub claude_servers: Vec<McpServerEntry>,
    pub codex_servers: Vec<McpServerEntry>,
    /// Initial catalogs of every (server, tool) each agent knows about.
    pub claude_tool_catalog: Vec<ToolEntry>,
    pub codex_tool_catalog: Vec<ToolEntry>,
    pub mcp_events: Option<mpsc::Receiver<McpCatalogEvent>>,
    pub mcp_commands: Option<tokio::sync::mpsc::UnboundedSender<McpCatalogCommand>>,
    /// Each scope's MCP policy as it lives on disk. The TUI displays the
    /// effective enabled state (Workspace view = global ∪ workspace at
    /// the tool granularity, Global view = global only) and writes
    /// toggles back into the active scope only.
    pub mcp_global: McpPolicy,
    pub mcp_workspace: McpPolicy,
    pub codex_mcp_global: McpPolicy,
    pub codex_mcp_workspace: McpPolicy,
    /// Each scope's `[task_runner.tasks]` map. Workspace entries shadow
    /// global ones with the same name in the merged display.
    pub tasks_global: BTreeMap<String, String>,
    pub tasks_workspace: BTreeMap<String, String>,
}

pub struct TuiOutput {
    /// Which scope was active when the user hit `s`. The save pass writes
    /// only this scope; the other scope's buffer is discarded.
    pub saved_scope: Scope,
    pub general_global: GeneralPolicy,
    pub general_workspace: GeneralPolicy,
    pub claude_global: ClaudePolicy,
    pub claude_workspace: ClaudePolicy,
    pub proxy_allow_global: Vec<String>,
    pub proxy_allow_workspace: Vec<String>,
    pub filesystem_global: FilesystemPolicy,
    pub filesystem_workspace: FilesystemPolicy,
    pub mcp_global: McpPolicy,
    pub mcp_workspace: McpPolicy,
    pub codex_mcp_global: McpPolicy,
    pub codex_mcp_workspace: McpPolicy,
    pub claude_tool_catalog: Vec<ToolEntry>,
    pub codex_tool_catalog: Vec<ToolEntry>,
    pub tasks_global: BTreeMap<String, String>,
    pub tasks_workspace: BTreeMap<String, String>,
}

pub enum Outcome {
    Save(Box<TuiOutput>),
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TopTab {
    General,
    Proxy,
    HostFs,
    McpClaude,
    McpCodex,
}

impl TopTab {
    fn next(self) -> Self {
        match self {
            TopTab::General => TopTab::Proxy,
            TopTab::Proxy => TopTab::HostFs,
            TopTab::HostFs => TopTab::McpClaude,
            TopTab::McpClaude => TopTab::McpCodex,
            TopTab::McpCodex => TopTab::General,
        }
    }
    fn prev(self) -> Self {
        match self {
            TopTab::General => TopTab::McpCodex,
            TopTab::Proxy => TopTab::General,
            TopTab::HostFs => TopTab::Proxy,
            TopTab::McpClaude => TopTab::HostFs,
            TopTab::McpCodex => TopTab::McpClaude,
        }
    }
    fn titles() -> [&'static str; 5] {
        [
            "General",
            "Proxy",
            "Filesystem",
            "MCP (Claude Code)",
            "MCP (Codex)",
        ]
    }
    fn index(self) -> usize {
        match self {
            TopTab::General => 0,
            TopTab::Proxy => 1,
            TopTab::HostFs => 2,
            TopTab::McpClaude => 3,
            TopTab::McpCodex => 4,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatternTarget {
    Proxy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FilesystemField {
    Mount,
    Hide,
    Readonly,
}

impl FilesystemField {
    fn label(self) -> &'static str {
        match self {
            FilesystemField::Mount => "path",
            FilesystemField::Hide => "hide",
            FilesystemField::Readonly => "readonly",
        }
    }

    fn add_label(self) -> &'static str {
        match self {
            FilesystemField::Mount => "+ Add Path...",
            FilesystemField::Hide => "+ Add Hidden Filter...",
            FilesystemField::Readonly => "+ Add Readonly Filter...",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FilesystemSection {
    Path,
    Filter,
}

enum Mode {
    Normal,
    ProxyInput {
        target: PatternTarget,
        buffer: TextField,
        /// `Some(row)` when editing an existing entry; `None` for adds.
        /// The row carries the origin scope so the commit knows whether
        /// to update or refuse the write.
        editing: Option<ProxyRow>,
    },
    FilesystemInput {
        field: FilesystemField,
        buffer: TextField,
        mount_readonly: bool,
        error: Option<String>,
        editing: Option<FilesystemRow>,
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
    DefaultAgentSelect {
        cursor: usize,
    },
    BedrockRegionInput {
        buffer: TextField,
    },
    BypassWarningSelect {
        cursor: usize,
    },
    ItemAction {
        target: ItemActionTarget,
        cursor: usize,
    },
    McpServerAction {
        agent: McpAgent,
        server_name: String,
        can_auth: bool,
        cursor: usize,
    },
    /// Confirmation prompt before leaving the settings editor. Displayed
    /// when the user hits `q` or ^C.
    ConfirmQuit {
        cursor: usize,
    },
}

#[derive(Clone)]
enum ItemActionTarget {
    Proxy(ProxyRow),
    Filesystem(FilesystemRow),
    Task(String),
}

#[derive(Clone, Copy)]
enum ItemAction {
    Edit,
    Remove,
}

const ITEM_ACTION_CHOICES: [ItemAction; 2] = [ItemAction::Edit, ItemAction::Remove];

#[derive(Clone, Copy)]
enum McpServerAction {
    Reload,
    Reauthenticate,
}

#[derive(Clone, Copy)]
enum QuitAction {
    SaveAndQuit,
    KeepEditing,
    DiscardAndQuit,
}

const QUIT_ACTION_CHOICES: [QuitAction; 3] = [
    QuitAction::SaveAndQuit,
    QuitAction::KeepEditing,
    QuitAction::DiscardAndQuit,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShortcutHint {
    key: &'static str,
    label: &'static str,
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

const DEFAULT_AGENT_CHOICES: [Option<DefaultAgent>; 4] = [
    None,
    Some(DefaultAgent::Claude),
    Some(DefaultAgent::Codex),
    Some(DefaultAgent::Cursor),
];

const BYPASS_WARNING_CHOICES: [Option<bool>; 3] = [None, Some(false), Some(true)];

fn default_agent_index(agent: Option<DefaultAgent>) -> usize {
    DEFAULT_AGENT_CHOICES
        .iter()
        .position(|candidate| *candidate == agent)
        .unwrap_or(0)
}

fn bypass_warning_index(value: Option<bool>) -> usize {
    BYPASS_WARNING_CHOICES
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or(0)
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

#[derive(Clone, Debug)]
enum ProxyViewRow {
    Entry(ProxyRow),
    Add,
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

    fn entry_rows(&self, scope: Scope) -> Vec<ProxyRow> {
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

    fn visible_rows(&self, scope: Scope) -> Vec<ProxyViewRow> {
        let mut rows = Vec::new();
        for row in self.entry_rows(scope) {
            rows.push(ProxyViewRow::Entry(row));
        }
        rows.push(ProxyViewRow::Add);
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

    fn current_row(&self, scope: Scope) -> Option<ProxyViewRow> {
        self.visible_rows(scope).into_iter().nth(self.cursor)
    }

    #[cfg(test)]
    fn current_entry(&self, scope: Scope) -> Option<ProxyRow> {
        match self.current_row(scope) {
            Some(ProxyViewRow::Entry(row)) => Some(row),
            _ => None,
        }
    }

    #[cfg(test)]
    /// Remove the cursor's row, but only if it lives in the active
    /// scope. Global rows shown in the workspace view are inherited and
    /// cannot be deleted from here — the user has to switch to Global
    /// with `t` to remove them.
    fn remove_current(&mut self, scope: Scope) {
        let Some(row) = self.current_entry(scope) else {
            return;
        };
        self.remove_row(scope, row);
    }

    fn remove_row(&mut self, scope: Scope, row: ProxyRow) {
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
    /// foreign-scope row (an inherited global pattern visible while
    /// editing Workspace), do nothing — that case is blocked at the call
    /// site, and treating it as an add would silently fork the entry.
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
                let len = self.entry_rows(scope).len();
                self.cursor = len.saturating_sub(1);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct FilesystemRow {
    origin: ProxyOrigin,
    field: FilesystemField,
    value: String,
    mount_readonly: bool,
    idx_within_scope: usize,
}

#[derive(Clone, Debug)]
enum FilesystemViewRow {
    Section(FilesystemSection),
    Entry(FilesystemRow),
    Add(FilesystemField),
}

struct FilesystemState {
    global: FilesystemPolicy,
    workspace: FilesystemPolicy,
    cursor: usize,
}

impl FilesystemState {
    fn new(global: FilesystemPolicy, workspace: FilesystemPolicy) -> Self {
        Self {
            global,
            workspace,
            cursor: 0,
        }
    }

    fn filter_list(policy: &FilesystemPolicy, field: FilesystemField) -> &Vec<String> {
        match field {
            FilesystemField::Mount => unreachable!("mounts are not regex filter lists"),
            FilesystemField::Hide => &policy.hide,
            FilesystemField::Readonly => &policy.readonly,
        }
    }

    fn filter_list_mut(policy: &mut FilesystemPolicy, field: FilesystemField) -> &mut Vec<String> {
        match field {
            FilesystemField::Mount => unreachable!("mounts are not regex filter lists"),
            FilesystemField::Hide => &mut policy.hide,
            FilesystemField::Readonly => &mut policy.readonly,
        }
    }

    fn policy_mut(&mut self, origin: ProxyOrigin) -> &mut FilesystemPolicy {
        match origin {
            ProxyOrigin::Global => &mut self.global,
            ProxyOrigin::Workspace => &mut self.workspace,
        }
    }

    fn rows_for(policy: &FilesystemPolicy, origin: ProxyOrigin) -> Vec<FilesystemRow> {
        let mut rows = Vec::new();
        for (i, mount) in policy.mounts.iter().enumerate() {
            rows.push(FilesystemRow {
                origin,
                field: FilesystemField::Mount,
                value: mount.path.clone(),
                mount_readonly: mount.readonly,
                idx_within_scope: i,
            });
        }
        for field in [FilesystemField::Hide, FilesystemField::Readonly] {
            for (i, value) in Self::filter_list(policy, field).iter().enumerate() {
                rows.push(FilesystemRow {
                    origin,
                    field,
                    value: value.clone(),
                    mount_readonly: false,
                    idx_within_scope: i,
                });
            }
        }
        rows
    }

    fn entry_rows(&self, scope: Scope) -> Vec<FilesystemRow> {
        let mut rows = Self::rows_for(&self.global, ProxyOrigin::Global);
        if scope == Scope::Workspace {
            rows.extend(Self::rows_for(&self.workspace, ProxyOrigin::Workspace));
        }
        rows
    }

    fn visible_rows(&self, scope: Scope) -> Vec<FilesystemViewRow> {
        let entries = self.entry_rows(scope);
        let mut rows = Vec::new();
        rows.push(FilesystemViewRow::Section(FilesystemSection::Path));
        rows.extend(
            entries
                .iter()
                .filter(|row| row.field == FilesystemField::Mount)
                .cloned()
                .map(FilesystemViewRow::Entry),
        );
        rows.push(FilesystemViewRow::Add(FilesystemField::Mount));
        rows.push(FilesystemViewRow::Section(FilesystemSection::Filter));
        rows.extend(
            entries
                .iter()
                .filter(|row| row.field != FilesystemField::Mount)
                .cloned()
                .map(FilesystemViewRow::Entry),
        );
        rows.push(FilesystemViewRow::Add(FilesystemField::Hide));
        rows.push(FilesystemViewRow::Add(FilesystemField::Readonly));
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

    fn current_row(&self, scope: Scope) -> Option<FilesystemViewRow> {
        self.visible_rows(scope).into_iter().nth(self.cursor)
    }

    fn upsert(
        &mut self,
        scope: Scope,
        field: FilesystemField,
        value: String,
        mount_readonly: bool,
        editing: Option<FilesystemRow>,
    ) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        let active = ProxyOrigin::from_scope(scope);
        let v = trimmed.to_string();
        match editing {
            Some(row) if row.origin == active => {
                let policy = self.policy_mut(active);
                if row.field == FilesystemField::Mount {
                    if row.idx_within_scope < policy.mounts.len() {
                        policy.mounts[row.idx_within_scope] =
                            FilesystemMount::new(v, mount_readonly);
                    }
                } else {
                    let list = Self::filter_list_mut(policy, row.field);
                    if row.idx_within_scope < list.len() {
                        list[row.idx_within_scope] = v;
                    }
                }
            }
            Some(_) => {}
            None => {
                let policy = self.policy_mut(active);
                if field == FilesystemField::Mount {
                    if !policy.mounts.iter().any(|mount| mount.path == v) {
                        policy.mounts.push(FilesystemMount::new(v, mount_readonly));
                    }
                } else {
                    let list = Self::filter_list_mut(policy, field);
                    if !list.contains(&v) {
                        list.push(v);
                    }
                }
                let len = self.entry_rows(scope).len();
                self.cursor = len.saturating_sub(1);
            }
        }
    }

    fn remove_row(&mut self, scope: Scope, row: FilesystemRow) {
        let active = ProxyOrigin::from_scope(scope);
        if row.origin != active {
            return;
        }
        let policy = self.policy_mut(active);
        if row.field == FilesystemField::Mount {
            if row.idx_within_scope < policy.mounts.len() {
                policy.mounts.remove(row.idx_within_scope);
            }
        } else {
            let list = Self::filter_list_mut(policy, row.field);
            if row.idx_within_scope < list.len() {
                list.remove(row.idx_within_scope);
            }
        }
        let len = self.visible_rows(scope).len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GeneralRow {
    DefaultAgent,
    BedrockRegion,
    BypassWarning,
}

struct GeneralState {
    global: GeneralPolicy,
    workspace: GeneralPolicy,
    claude_global: ClaudePolicy,
    claude_workspace: ClaudePolicy,
    cursor: usize,
}

impl GeneralState {
    fn new(
        global: GeneralPolicy,
        workspace: GeneralPolicy,
        claude_global: ClaudePolicy,
        claude_workspace: ClaudePolicy,
    ) -> Self {
        Self {
            global,
            workspace,
            claude_global,
            claude_workspace,
            cursor: 0,
        }
    }

    fn active_policy_mut(&mut self, scope: Scope) -> &mut GeneralPolicy {
        match scope {
            Scope::Global => &mut self.global,
            Scope::Workspace => &mut self.workspace,
        }
    }

    fn effective_agent(&self, scope: Scope) -> DefaultAgent {
        match scope {
            Scope::Global => self.global.default_agent(),
            Scope::Workspace => self
                .workspace
                .default_agent
                .or(self.global.default_agent)
                .unwrap_or_default(),
        }
    }

    fn effective_bedrock_region(&self, scope: Scope) -> &str {
        match scope {
            Scope::Global => self.global.bedrock_region(),
            Scope::Workspace => self
                .workspace
                .bedrock_region
                .as_deref()
                .or(self.global.bedrock_region.as_deref())
                .unwrap_or(GeneralPolicy::DEFAULT_BEDROCK_REGION),
        }
    }

    fn bedrock_region_origin(&self, scope: Scope) -> ProxyOrigin {
        match scope {
            Scope::Global => ProxyOrigin::Global,
            Scope::Workspace if self.workspace.bedrock_region.is_some() => ProxyOrigin::Workspace,
            Scope::Workspace => ProxyOrigin::Global,
        }
    }

    fn origin(&self, scope: Scope) -> ProxyOrigin {
        match scope {
            Scope::Global => ProxyOrigin::Global,
            Scope::Workspace if self.workspace.default_agent.is_some() => ProxyOrigin::Workspace,
            Scope::Workspace => ProxyOrigin::Global,
        }
    }

    fn set_agent(&mut self, scope: Scope, agent: Option<DefaultAgent>) {
        self.active_policy_mut(scope).default_agent = agent;
    }

    fn active_claude_policy_mut(&mut self, scope: Scope) -> &mut ClaudePolicy {
        match scope {
            Scope::Global => &mut self.claude_global,
            Scope::Workspace => &mut self.claude_workspace,
        }
    }

    fn effective_skip_bypass_warning(&self, scope: Scope) -> bool {
        match scope {
            Scope::Global => self.claude_global.skip_bypass_permissions_warning(),
            Scope::Workspace => self
                .claude_workspace
                .skip_bypass_permissions_warning
                .or(self.claude_global.skip_bypass_permissions_warning)
                .unwrap_or(false),
        }
    }

    fn bypass_warning_origin(&self, scope: Scope) -> ProxyOrigin {
        match scope {
            Scope::Global => ProxyOrigin::Global,
            Scope::Workspace
                if self
                    .claude_workspace
                    .skip_bypass_permissions_warning
                    .is_some() =>
            {
                ProxyOrigin::Workspace
            }
            Scope::Workspace => ProxyOrigin::Global,
        }
    }

    fn configured_skip_bypass_warning(&self, scope: Scope) -> Option<bool> {
        match scope {
            Scope::Global => self.claude_global.skip_bypass_permissions_warning,
            Scope::Workspace => self.claude_workspace.skip_bypass_permissions_warning,
        }
    }

    fn set_skip_bypass_warning(&mut self, scope: Scope, value: Option<bool>) {
        self.active_claude_policy_mut(scope)
            .skip_bypass_permissions_warning = value;
    }

    fn configured_agent(&self, scope: Scope) -> Option<DefaultAgent> {
        match scope {
            Scope::Global => self.global.default_agent,
            Scope::Workspace => self.workspace.default_agent,
        }
    }

    fn configured_bedrock_region(&self, scope: Scope) -> Option<&str> {
        match scope {
            Scope::Global => self.global.bedrock_region.as_deref(),
            Scope::Workspace => self.workspace.bedrock_region.as_deref(),
        }
    }

    fn set_bedrock_region(&mut self, scope: Scope, value: Option<String>) {
        self.active_policy_mut(scope).bedrock_region = value;
    }

    fn visible_rows(&self, _scope: Scope) -> Vec<GeneralRow> {
        vec![
            GeneralRow::DefaultAgent,
            GeneralRow::BedrockRegion,
            GeneralRow::BypassWarning,
        ]
    }

    fn current_row(&self, scope: Scope) -> Option<GeneralRow> {
        self.visible_rows(scope).get(self.cursor).copied()
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

    fn clamp_cursor(&mut self, scope: Scope) {
        let len = self.visible_rows(scope).len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
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

#[derive(Clone)]
enum McpServerStatus {
    Loading,
    Ready,
    Failed { message: String, can_auth: bool },
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
    server_transports: HashMap<String, String>,
    server_status: HashMap<String, McpServerStatus>,
    /// Per-server collapse state. The config UI starts with only
    /// top-level MCP rows visible; `Enter` expands the focused server.
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
        servers: Vec<McpServerEntry>,
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

        let mut server_names: Vec<String> = servers.iter().map(|s| s.name.clone()).collect();
        server_names.sort();
        server_names.dedup();
        let server_transports: HashMap<String, String> = servers
            .iter()
            .map(|s| (s.name.clone(), s.transport.clone()))
            .collect();
        let mut server_status: HashMap<String, McpServerStatus> = servers
            .iter()
            .map(|s| (s.name.clone(), McpServerStatus::Loading))
            .collect();
        let mut server_ranges: HashMap<String, (usize, usize)> = HashMap::new();
        for (i, e) in catalog.iter().enumerate() {
            match server_ranges.get_mut(&e.server_name) {
                Some((_, count)) => *count += 1,
                None => {
                    server_ranges.insert(e.server_name.clone(), (i, 1));
                    if !server_names.contains(&e.server_name) {
                        server_names.push(e.server_name.clone());
                    }
                }
            }
            server_status.insert(e.server_name.clone(), McpServerStatus::Ready);
        }
        server_names.sort();

        let expanded = vec![false; server_names.len()];
        Self {
            tasks_global,
            tasks_workspace,
            mcp_global,
            mcp_workspace,
            task_runner_expanded: false,
            server_names,
            server_transports,
            server_status,
            expanded,
            catalog,
            server_ranges,
            cursor: 0,
        }
    }

    fn apply_catalog_event(&mut self, event: McpCatalogEvent) {
        let (server_name, status, tools) = match event {
            McpCatalogEvent::Loading { server_name, .. } => {
                (server_name, McpServerStatus::Loading, None)
            }
            McpCatalogEvent::Loaded {
                server_name, tools, ..
            } => (server_name, McpServerStatus::Ready, Some(tools)),
            McpCatalogEvent::Failed {
                server_name,
                message,
                can_auth,
                ..
            } => (
                server_name,
                McpServerStatus::Failed { message, can_auth },
                Some(Vec::new()),
            ),
        };
        if !self.server_names.contains(&server_name) {
            self.server_names.push(server_name.clone());
            self.server_names.sort();
        }
        self.server_status.insert(server_name.clone(), status);
        if let Some(mut tools) = tools {
            self.catalog
                .retain(|entry| entry.server_name != server_name);
            self.catalog.append(&mut tools);
            self.rebuild_catalog_index();
        }
    }

    fn rebuild_catalog_index(&mut self) {
        let expanded_by_name: HashMap<String, bool> = self
            .server_names
            .iter()
            .cloned()
            .zip(self.expanded.iter().copied())
            .collect();
        self.catalog.sort_by(|a, b| {
            a.server_name
                .cmp(&b.server_name)
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });
        self.server_ranges.clear();
        for (i, e) in self.catalog.iter().enumerate() {
            match self.server_ranges.get_mut(&e.server_name) {
                Some((_, count)) => *count += 1,
                None => {
                    self.server_ranges.insert(e.server_name.clone(), (i, 1));
                }
            }
        }
        self.expanded = self
            .server_names
            .iter()
            .map(|name| expanded_by_name.get(name).copied().unwrap_or(false))
            .collect();
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
            if self.expanded[si]
                && let Some((start, count)) = self.server_ranges.get(name).copied()
            {
                for t in 0..count {
                    rows.push(McpRow::Tool(start + t));
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
                let name = self.server_names[si].clone();
                match self.server_status.get(&name) {
                    Some(McpServerStatus::Failed { can_auth, .. }) => RowAction::McpServerAction {
                        server_name: name,
                        can_auth: *can_auth,
                    },
                    _ => {
                        self.expanded[si] = !self.expanded[si];
                        RowAction::Handled
                    }
                }
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

    /// Delete the task focused by the cursor under `scope`. Workspace
    /// only deletes the workspace-side entry — a global-only task stays
    /// visible (the user has to switch to Global to remove it). Visible
    /// for the user via the `[W]` annotation in the row.
    fn delete_task(&mut self, scope: Scope, name: &str) {
        match scope {
            Scope::Global => {
                self.tasks_global.remove(name);
            }
            Scope::Workspace => {
                self.tasks_workspace.remove(name);
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
    McpServerAction { server_name: String, can_auth: bool },
}

/// Frozen snapshot of every editable buffer at TUI launch. Used to
/// decide whether `q` should pop a confirm-quit dialog.
#[cfg(test)]
#[derive(Clone)]
struct Snapshot {
    general_global: GeneralPolicy,
    general_workspace: GeneralPolicy,
    claude_global: ClaudePolicy,
    claude_workspace: ClaudePolicy,
    proxy_global: Vec<String>,
    proxy_workspace: Vec<String>,
    filesystem_global: FilesystemPolicy,
    filesystem_workspace: FilesystemPolicy,
    mcp_global: McpPolicy,
    mcp_workspace: McpPolicy,
    codex_mcp_global: McpPolicy,
    codex_mcp_workspace: McpPolicy,
    tasks_global: BTreeMap<String, String>,
    tasks_workspace: BTreeMap<String, String>,
}

struct App {
    workspace: PathBuf,
    scope: Scope,
    tab: TopTab,
    general: GeneralState,
    /// Holds both scopes' allow lists; the visible rows are derived
    /// from `scope`. Cursor is on the rendered (merged) view.
    proxy: ProxyState,
    filesystem: FilesystemState,
    mcp_claude: McpState,
    mcp_codex: McpState,
    mcp_events: Option<mpsc::Receiver<McpCatalogEvent>>,
    mcp_commands: Option<tokio::sync::mpsc::UnboundedSender<McpCatalogCommand>>,
    mode: Mode,
    list_state: ListState,
    #[cfg(test)]
    initial: Snapshot,
}

impl App {
    fn new(input: TuiInput) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        #[cfg(test)]
        let initial = Snapshot {
            general_global: input.general_global.clone(),
            general_workspace: input.general_workspace.clone(),
            claude_global: input.claude_global.clone(),
            claude_workspace: input.claude_workspace.clone(),
            proxy_global: input.proxy_allow_global.clone(),
            proxy_workspace: input.proxy_allow_workspace.clone(),
            filesystem_global: input.filesystem_global.clone(),
            filesystem_workspace: input.filesystem_workspace.clone(),
            mcp_global: input.mcp_global.clone(),
            mcp_workspace: input.mcp_workspace.clone(),
            codex_mcp_global: input.codex_mcp_global.clone(),
            codex_mcp_workspace: input.codex_mcp_workspace.clone(),
            tasks_global: input.tasks_global.clone(),
            tasks_workspace: input.tasks_workspace.clone(),
        };
        let tasks_global = input.tasks_global;
        let tasks_workspace = input.tasks_workspace;
        Self {
            workspace: input.workspace,
            scope: input.initial_scope,
            tab: TopTab::General,
            general: GeneralState::new(
                input.general_global,
                input.general_workspace,
                input.claude_global,
                input.claude_workspace,
            ),
            proxy: ProxyState::new(input.proxy_allow_global, input.proxy_allow_workspace),
            filesystem: FilesystemState::new(input.filesystem_global, input.filesystem_workspace),
            mcp_claude: McpState::new(
                input.claude_servers,
                input.claude_tool_catalog,
                input.mcp_global,
                input.mcp_workspace,
                tasks_global.clone(),
                tasks_workspace.clone(),
            ),
            mcp_codex: McpState::new(
                input.codex_servers,
                input.codex_tool_catalog,
                input.codex_mcp_global,
                input.codex_mcp_workspace,
                tasks_global,
                tasks_workspace,
            ),
            mcp_events: input.mcp_events,
            mcp_commands: input.mcp_commands,
            mode: Mode::Normal,
            list_state,
            #[cfg(test)]
            initial,
        }
    }

    #[cfg(test)]
    fn has_unsaved_changes(&self) -> bool {
        self.proxy.global != self.initial.proxy_global
            || self.general.global != self.initial.general_global
            || self.general.workspace != self.initial.general_workspace
            || self.general.claude_global != self.initial.claude_global
            || self.general.claude_workspace != self.initial.claude_workspace
            || self.proxy.workspace != self.initial.proxy_workspace
            || self.filesystem.global != self.initial.filesystem_global
            || self.filesystem.workspace != self.initial.filesystem_workspace
            || self.mcp_claude.mcp_global != self.initial.mcp_global
            || self.mcp_claude.mcp_workspace != self.initial.mcp_workspace
            || self.mcp_codex.mcp_global != self.initial.codex_mcp_global
            || self.mcp_codex.mcp_workspace != self.initial.codex_mcp_workspace
            || self.mcp_claude.tasks_global != self.initial.tasks_global
            || self.mcp_claude.tasks_workspace != self.initial.tasks_workspace
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
        if self.tab == TopTab::General {
            self.general.clamp_cursor(self.scope);
        } else if self.tab == TopTab::Proxy {
            let len = self.proxy.visible_rows(self.scope).len();
            if self.proxy.cursor >= len {
                self.proxy.cursor = len.saturating_sub(1);
            }
        } else if self.tab == TopTab::HostFs {
            let len = self.filesystem.visible_rows(self.scope).len();
            if self.filesystem.cursor >= len {
                self.filesystem.cursor = len.saturating_sub(1);
            }
        }
    }

    fn sync_list_state(&mut self) {
        let cur = match self.tab {
            TopTab::General => self.general.cursor,
            TopTab::Proxy => self.proxy.cursor,
            TopTab::HostFs => self.filesystem.cursor,
            TopTab::McpClaude => self.mcp_claude.cursor,
            TopTab::McpCodex => self.mcp_codex.cursor,
        };
        self.list_state.select(Some(cur));
    }

    fn into_output(self) -> TuiOutput {
        TuiOutput {
            saved_scope: self.scope,
            general_global: self.general.global,
            general_workspace: self.general.workspace,
            claude_global: self.general.claude_global,
            claude_workspace: self.general.claude_workspace,
            proxy_allow_global: self.proxy.global,
            proxy_allow_workspace: self.proxy.workspace,
            filesystem_global: self.filesystem.global,
            filesystem_workspace: self.filesystem.workspace,
            mcp_global: self.mcp_claude.mcp_global,
            mcp_workspace: self.mcp_claude.mcp_workspace,
            codex_mcp_global: self.mcp_codex.mcp_global,
            codex_mcp_workspace: self.mcp_codex.mcp_workspace,
            claude_tool_catalog: self.mcp_claude.catalog,
            codex_tool_catalog: self.mcp_codex.catalog,
            tasks_global: self.mcp_claude.tasks_global,
            tasks_workspace: self.mcp_claude.tasks_workspace,
        }
    }

    fn active_mcp(&self) -> &McpState {
        match self.tab {
            TopTab::McpCodex => &self.mcp_codex,
            _ => &self.mcp_claude,
        }
    }

    fn active_mcp_mut(&mut self) -> &mut McpState {
        match self.tab {
            TopTab::McpCodex => &mut self.mcp_codex,
            _ => &mut self.mcp_claude,
        }
    }

    fn sync_tasks_from_claude(&mut self) {
        self.mcp_codex.tasks_global = self.mcp_claude.tasks_global.clone();
        self.mcp_codex.tasks_workspace = self.mcp_claude.tasks_workspace.clone();
    }

    fn sync_tasks_from_codex(&mut self) {
        self.mcp_claude.tasks_global = self.mcp_codex.tasks_global.clone();
        self.mcp_claude.tasks_workspace = self.mcp_codex.tasks_workspace.clone();
    }

    fn sync_tasks_from_active(&mut self) {
        match self.tab {
            TopTab::McpCodex => self.sync_tasks_from_codex(),
            _ => self.sync_tasks_from_claude(),
        }
    }

    fn drain_mcp_events(&mut self) {
        let Some(rx) = self.mcp_events.take() else {
            return;
        };
        while let Ok(event) = rx.try_recv() {
            match event {
                event @ McpCatalogEvent::Loading {
                    agent: McpAgent::Claude,
                    ..
                }
                | event @ McpCatalogEvent::Loaded {
                    agent: McpAgent::Claude,
                    ..
                }
                | event @ McpCatalogEvent::Failed {
                    agent: McpAgent::Claude,
                    ..
                } => self.mcp_claude.apply_catalog_event(event),
                event @ McpCatalogEvent::Loading {
                    agent: McpAgent::Codex,
                    ..
                }
                | event @ McpCatalogEvent::Loaded {
                    agent: McpAgent::Codex,
                    ..
                }
                | event @ McpCatalogEvent::Failed {
                    agent: McpAgent::Codex,
                    ..
                } => self.mcp_codex.apply_catalog_event(event),
            }
        }
        self.mcp_events = Some(rx);
    }

    fn send_mcp_command(&self, command: McpCatalogCommand) {
        if let Some(tx) = &self.mcp_commands {
            let _ = tx.send(command);
        }
    }
}

fn handle_proxy_input_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    // Pull the current input buffer out first so we can mutate `app.mode`
    // (to Normal on commit/cancel) without aliasing the same borrow.
    let Mode::ProxyInput {
        target,
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
            match target {
                PatternTarget::Proxy => app.proxy.upsert(app.scope, buffer.value(), editing),
            }
            return;
        }
        _ => {
            apply_editing_key(&mut buffer, code, modifiers);
        }
    }

    app.mode = Mode::ProxyInput {
        target,
        buffer,
        editing,
    };
}

fn handle_filesystem_input_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Mode::FilesystemInput {
        field,
        mut buffer,
        mut mount_readonly,
        editing,
        ..
    } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return;
    };

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Esc => return,
        KeyCode::Char('c') if ctrl => return,
        KeyCode::Char(' ') if field == FilesystemField::Mount => {
            mount_readonly = !mount_readonly;
        }
        KeyCode::Enter => {
            let value = if field == FilesystemField::Mount {
                match resolve_existing_mount_path(&app.workspace, &buffer.value()) {
                    Ok(path) => path,
                    Err(e) => {
                        app.mode = Mode::FilesystemInput {
                            field,
                            buffer,
                            mount_readonly,
                            error: Some(e),
                            editing,
                        };
                        return;
                    }
                }
            } else {
                buffer.value()
            };
            app.filesystem
                .upsert(app.scope, field, value, mount_readonly, editing);
            return;
        }
        _ => {
            apply_editing_key(&mut buffer, code, modifiers);
        }
    }

    app.mode = Mode::FilesystemInput {
        field,
        buffer,
        mount_readonly,
        error: None,
        editing,
    };
}

fn resolve_existing_mount_path(workspace: &Path, raw: &str) -> std::result::Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Path is required.".to_string());
    }
    let input = Path::new(trimmed);
    let path = if input.is_absolute() {
        input.to_path_buf()
    } else {
        workspace.join(input)
    };
    let resolved = std::fs::canonicalize(&path)
        .map_err(|e| format!("Path does not exist: {} ({e})", path.display()))?;
    if !resolved.is_dir() {
        return Err(format!("Path is not a directory: {}", resolved.display()));
    }
    Ok(resolved.display().to_string())
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
                if let Some(orig) = &editing
                    && orig != &name_tr
                {
                    match scope {
                        Scope::Global => {
                            app.active_mcp_mut().tasks_global.remove(orig);
                        }
                        Scope::Workspace => {
                            app.active_mcp_mut().tasks_workspace.remove(orig);
                        }
                    }
                }
                app.active_mcp_mut().set_task_for(scope, name_tr, cmd_tr);
                app.sync_tasks_from_active();
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
        .active_mcp()
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

fn start_proxy_add(app: &mut App) {
    app.mode = Mode::ProxyInput {
        target: PatternTarget::Proxy,
        buffer: TextField::default(),
        editing: None,
    };
}

fn start_filesystem_add(app: &mut App, field: FilesystemField) {
    app.mode = Mode::FilesystemInput {
        field,
        buffer: TextField::default(),
        mount_readonly: field == FilesystemField::Mount,
        error: None,
        editing: None,
    };
}

fn filesystem_add_field_for_current(app: &App) -> FilesystemField {
    match app.filesystem.current_row(app.scope) {
        Some(FilesystemViewRow::Section(FilesystemSection::Path)) => FilesystemField::Mount,
        Some(FilesystemViewRow::Section(FilesystemSection::Filter)) => FilesystemField::Hide,
        Some(FilesystemViewRow::Entry(row)) => row.field,
        Some(FilesystemViewRow::Add(field)) => field,
        None => FilesystemField::Mount,
    }
}

fn start_add_for_current_context(app: &mut App) {
    match app.tab {
        TopTab::General => {}
        TopTab::Proxy => start_proxy_add(app),
        TopTab::HostFs => {
            let field = filesystem_add_field_for_current(app);
            start_filesystem_add(app, field);
        }
        TopTab::McpClaude | TopTab::McpCodex => match app.active_mcp().current_row(app.scope) {
            Some(McpRow::TaskRunnerHeader | McpRow::TaskRow(_) | McpRow::TaskAddHint) => {
                start_task_add(app);
            }
            Some(McpRow::Server(_) | McpRow::Tool(_)) | None => {}
        },
    }
}

fn can_add_for_current_context(app: &App) -> bool {
    match app.tab {
        TopTab::General => false,
        TopTab::Proxy | TopTab::HostFs => true,
        TopTab::McpClaude | TopTab::McpCodex => matches!(
            app.active_mcp().current_row(app.scope),
            Some(McpRow::TaskRunnerHeader | McpRow::TaskRow(_) | McpRow::TaskAddHint)
        ),
    }
}

fn removable_item_for_current_context(app: &App) -> Option<ItemActionTarget> {
    let active = ProxyOrigin::from_scope(app.scope);
    match app.tab {
        TopTab::General => None,
        TopTab::Proxy => match app.proxy.current_row(app.scope) {
            Some(ProxyViewRow::Entry(row)) if row.origin == active => {
                Some(ItemActionTarget::Proxy(row))
            }
            _ => None,
        },
        TopTab::HostFs => match app.filesystem.current_row(app.scope) {
            Some(FilesystemViewRow::Entry(row)) if row.origin == active => {
                Some(ItemActionTarget::Filesystem(row))
            }
            _ => None,
        },
        TopTab::McpClaude | TopTab::McpCodex => {
            let mcp = app.active_mcp();
            match mcp.current_row(app.scope) {
                Some(McpRow::TaskRow(name)) => {
                    let owned_by_scope = match app.scope {
                        Scope::Global => mcp.tasks_global.contains_key(&name),
                        Scope::Workspace => mcp.tasks_workspace.contains_key(&name),
                    };
                    owned_by_scope.then_some(ItemActionTarget::Task(name))
                }
                _ => None,
            }
        }
    }
}

fn remove_current_context_item(app: &mut App) -> bool {
    let Some(target) = removable_item_for_current_context(app) else {
        return false;
    };
    remove_item(app, target);
    true
}

fn shortcut_hints(app: &App) -> Vec<ShortcutHint> {
    let mut hints = vec![
        ShortcutHint {
            key: "←/→ h/l",
            label: "tabs",
        },
        ShortcutHint {
            key: "↑/↓ j/k",
            label: "move",
        },
        ShortcutHint {
            key: "Enter",
            label: "select",
        },
    ];
    if can_add_for_current_context(app) {
        hints.push(ShortcutHint {
            key: "a",
            label: "add",
        });
    }
    if removable_item_for_current_context(app).is_some() {
        hints.push(ShortcutHint {
            key: "d",
            label: "remove",
        });
    }
    hints.extend([
        ShortcutHint {
            key: "s",
            label: "save",
        },
        ShortcutHint {
            key: "t",
            label: "scope",
        },
        ShortcutHint {
            key: "q",
            label: "exit",
        },
    ]);
    hints
}

fn handle_default_agent_select_key(app: &mut App, code: KeyCode) {
    let Mode::DefaultAgentSelect { mut cursor } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return;
    };

    match code {
        KeyCode::Esc => return,
        KeyCode::Up | KeyCode::Left | KeyCode::Char('k') => {
            cursor = cursor.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Right | KeyCode::Tab | KeyCode::Char('j')
            if cursor + 1 < DEFAULT_AGENT_CHOICES.len() =>
        {
            cursor += 1;
        }
        KeyCode::Home => {
            cursor = 0;
        }
        KeyCode::End => {
            cursor = DEFAULT_AGENT_CHOICES.len().saturating_sub(1);
        }
        KeyCode::Enter => {
            let agent = DEFAULT_AGENT_CHOICES.get(cursor).copied().unwrap_or(None);
            app.general.set_agent(app.scope, agent);
            return;
        }
        _ => {}
    }

    app.mode = Mode::DefaultAgentSelect { cursor };
}

fn handle_bedrock_region_input_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Mode::BedrockRegionInput { mut buffer } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return;
    };

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Esc => return,
        KeyCode::Char('c') if ctrl => return,
        KeyCode::Enter => {
            let value = buffer.value().trim().to_string();
            app.general
                .set_bedrock_region(app.scope, (!value.is_empty()).then_some(value));
            return;
        }
        _ => {
            apply_editing_key(&mut buffer, code, modifiers);
        }
    }

    app.mode = Mode::BedrockRegionInput { buffer };
}

fn handle_bypass_warning_select_key(app: &mut App, code: KeyCode) {
    let Mode::BypassWarningSelect { mut cursor } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return;
    };

    match code {
        KeyCode::Esc => return,
        KeyCode::Up | KeyCode::Left | KeyCode::Char('k') => {
            cursor = cursor.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Right | KeyCode::Tab | KeyCode::Char('j')
            if cursor + 1 < BYPASS_WARNING_CHOICES.len() =>
        {
            cursor += 1;
        }
        KeyCode::Home => {
            cursor = 0;
        }
        KeyCode::End => {
            cursor = BYPASS_WARNING_CHOICES.len().saturating_sub(1);
        }
        KeyCode::Enter => {
            let value = BYPASS_WARNING_CHOICES.get(cursor).copied().unwrap_or(None);
            app.general.set_skip_bypass_warning(app.scope, value);
            return;
        }
        _ => {}
    }

    app.mode = Mode::BypassWarningSelect { cursor };
}

fn start_item_edit(app: &mut App, target: ItemActionTarget) {
    match target {
        ItemActionTarget::Proxy(row) => {
            app.mode = Mode::ProxyInput {
                target: PatternTarget::Proxy,
                buffer: TextField::from_str(&row.pattern),
                editing: Some(row),
            };
        }
        ItemActionTarget::Filesystem(row) => {
            let mount_readonly = row.field == FilesystemField::Mount && row.mount_readonly;
            app.mode = Mode::FilesystemInput {
                field: row.field,
                buffer: TextField::from_str(&row.value),
                mount_readonly,
                error: None,
                editing: Some(row),
            };
        }
        ItemActionTarget::Task(name) => {
            start_task_edit(app, name);
        }
    }
}

fn remove_item(app: &mut App, target: ItemActionTarget) {
    let scope = app.scope;
    match target {
        ItemActionTarget::Proxy(row) => {
            app.proxy.remove_row(scope, row);
        }
        ItemActionTarget::Filesystem(row) => {
            app.filesystem.remove_row(scope, row);
        }
        ItemActionTarget::Task(name) => {
            app.active_mcp_mut().delete_task(scope, &name);
            app.sync_tasks_from_active();
        }
    }
}

fn handle_item_action_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Mode::ItemAction { target, mut cursor } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return;
    };

    match code {
        KeyCode::Esc => return,
        KeyCode::Char('d') if modifiers.is_empty() => {
            remove_item(app, target);
            return;
        }
        KeyCode::Up | KeyCode::Left | KeyCode::Char('k') => {
            cursor = cursor.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Right | KeyCode::Tab | KeyCode::Char('j')
            if cursor + 1 < ITEM_ACTION_CHOICES.len() =>
        {
            cursor += 1;
        }
        KeyCode::Home => {
            cursor = 0;
        }
        KeyCode::End => {
            cursor = ITEM_ACTION_CHOICES.len().saturating_sub(1);
        }
        KeyCode::Enter => match ITEM_ACTION_CHOICES.get(cursor).copied() {
            Some(ItemAction::Edit) => {
                start_item_edit(app, target);
                return;
            }
            Some(ItemAction::Remove) => {
                remove_item(app, target);
                return;
            }
            None => {}
        },
        _ => {}
    }

    app.mode = Mode::ItemAction { target, cursor };
}

fn mcp_server_action_choices(can_auth: bool) -> Vec<McpServerAction> {
    let mut choices = vec![McpServerAction::Reload];
    if can_auth {
        choices.push(McpServerAction::Reauthenticate);
    }
    choices
}

fn handle_mcp_server_action_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Mode::McpServerAction {
        agent,
        server_name,
        can_auth,
        mut cursor,
    } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return;
    };
    let choices = mcp_server_action_choices(can_auth);
    match code {
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return,
        KeyCode::Esc => return,
        KeyCode::Up | KeyCode::Char('k') => {
            cursor = cursor.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab if cursor + 1 < choices.len() => {
            cursor += 1;
        }
        KeyCode::Home => cursor = 0,
        KeyCode::End => cursor = choices.len().saturating_sub(1),
        KeyCode::Enter => match choices.get(cursor).copied() {
            Some(McpServerAction::Reload) => {
                app.send_mcp_command(McpCatalogCommand::Reload { agent, server_name });
                return;
            }
            Some(McpServerAction::Reauthenticate) => {
                app.send_mcp_command(McpCatalogCommand::Auth { agent, server_name });
                return;
            }
            None => {}
        },
        _ => {}
    }
    app.mode = Mode::McpServerAction {
        agent,
        server_name,
        can_auth,
        cursor,
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
        app.drain_mcp_events();
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
        if matches!(app.mode, Mode::FilesystemInput { .. }) {
            handle_filesystem_input_key(&mut app, key.code, key.modifiers);
            continue;
        }
        if matches!(app.mode, Mode::TaskInput { .. }) {
            handle_task_input_key(&mut app, key.code, key.modifiers);
            continue;
        }
        if matches!(app.mode, Mode::DefaultAgentSelect { .. }) {
            handle_default_agent_select_key(&mut app, key.code);
            continue;
        }
        if matches!(app.mode, Mode::BedrockRegionInput { .. }) {
            handle_bedrock_region_input_key(&mut app, key.code, key.modifiers);
            continue;
        }
        if matches!(app.mode, Mode::BypassWarningSelect { .. }) {
            handle_bypass_warning_select_key(&mut app, key.code);
            continue;
        }
        if matches!(app.mode, Mode::ItemAction { .. }) {
            handle_item_action_key(&mut app, key.code, key.modifiers);
            continue;
        }
        if matches!(app.mode, Mode::McpServerAction { .. }) {
            handle_mcp_server_action_key(&mut app, key.code, key.modifiers);
            continue;
        }
        if let Mode::ConfirmQuit { mut cursor } = std::mem::replace(&mut app.mode, Mode::Normal) {
            match key.code {
                KeyCode::Char('s') if key.modifiers.is_empty() => {
                    break Outcome::Save(Box::new(app.into_output()));
                }
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                    app.mode = Mode::ConfirmQuit { cursor };
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    if cursor + 1 < QUIT_ACTION_CHOICES.len() {
                        cursor += 1;
                    }
                    app.mode = Mode::ConfirmQuit { cursor };
                }
                KeyCode::Home => {
                    app.mode = Mode::ConfirmQuit { cursor: 0 };
                }
                KeyCode::End => {
                    app.mode = Mode::ConfirmQuit {
                        cursor: QUIT_ACTION_CHOICES.len().saturating_sub(1),
                    };
                }
                KeyCode::Enter => match QUIT_ACTION_CHOICES.get(cursor).copied() {
                    Some(QuitAction::SaveAndQuit) => {
                        break Outcome::Save(Box::new(app.into_output()));
                    }
                    Some(QuitAction::KeepEditing) | None => {}
                    Some(QuitAction::DiscardAndQuit) => break Outcome::Cancel,
                },
                _ => {
                    app.mode = Mode::ConfirmQuit { cursor };
                }
            }
            continue;
        }

        if key.code == KeyCode::Char('s') && key.modifiers.is_empty() {
            break Outcome::Save(Box::new(app.into_output()));
        }

        let want_quit = match key.code {
            KeyCode::Char('q') => true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
            _ => false,
        };
        if want_quit {
            app.mode = Mode::ConfirmQuit { cursor: 0 };
            continue;
        }

        let scope = app.scope;
        match key.code {
            KeyCode::Tab => app.tab = app.tab.next(),
            KeyCode::BackTab => app.tab = app.tab.prev(),
            KeyCode::Left | KeyCode::Char('h') => app.tab = app.tab.prev(),
            KeyCode::Right | KeyCode::Char('l') => app.tab = app.tab.next(),
            KeyCode::Char('t') => app.toggle_scope(),
            KeyCode::Char('a') if key.modifiers.is_empty() => {
                start_add_for_current_context(&mut app);
            }
            KeyCode::Char('d') if key.modifiers.is_empty() => {
                remove_current_context_item(&mut app);
            }
            KeyCode::Up | KeyCode::Char('k') => match app.tab {
                TopTab::General => app.general.move_up(),
                TopTab::Proxy => app.proxy.move_up(),
                TopTab::HostFs => app.filesystem.move_up(),
                TopTab::McpClaude | TopTab::McpCodex => app.active_mcp_mut().move_up(),
            },
            KeyCode::Down | KeyCode::Char('j') => match app.tab {
                TopTab::General => app.general.move_down(scope),
                TopTab::Proxy => app.proxy.move_down(scope),
                TopTab::HostFs => app.filesystem.move_down(scope),
                TopTab::McpClaude | TopTab::McpCodex => app.active_mcp_mut().move_down(scope),
            },
            KeyCode::Home => match app.tab {
                TopTab::General => app.general.jump_home(),
                TopTab::Proxy => app.proxy.jump_home(),
                TopTab::HostFs => app.filesystem.jump_home(),
                TopTab::McpClaude | TopTab::McpCodex => app.active_mcp_mut().jump_home(),
            },
            KeyCode::End => match app.tab {
                TopTab::General => app.general.jump_end(scope),
                TopTab::Proxy => app.proxy.jump_end(scope),
                TopTab::HostFs => app.filesystem.jump_end(scope),
                TopTab::McpClaude | TopTab::McpCodex => app.active_mcp_mut().jump_end(scope),
            },
            KeyCode::Enter => match app.tab {
                TopTab::General => match app.general.current_row(scope) {
                    Some(GeneralRow::DefaultAgent) => {
                        app.mode = Mode::DefaultAgentSelect {
                            cursor: default_agent_index(app.general.configured_agent(scope)),
                        };
                    }
                    Some(GeneralRow::BedrockRegion) => {
                        app.mode = Mode::BedrockRegionInput {
                            buffer: TextField::from_str(
                                app.general.configured_bedrock_region(scope).unwrap_or(""),
                            ),
                        };
                    }
                    Some(GeneralRow::BypassWarning) => {
                        app.mode = Mode::BypassWarningSelect {
                            cursor: bypass_warning_index(
                                app.general.configured_skip_bypass_warning(scope),
                            ),
                        };
                    }
                    None => {}
                },
                TopTab::Proxy => {
                    match app.proxy.current_row(scope) {
                        Some(ProxyViewRow::Entry(row))
                            if row.origin == ProxyOrigin::from_scope(scope) =>
                        {
                            // Inherited (global) rows shown in the workspace
                            // view are read-only here — `t` to switch scope
                            // first.
                            app.mode = Mode::ItemAction {
                                target: ItemActionTarget::Proxy(row),
                                cursor: 0,
                            };
                        }
                        Some(ProxyViewRow::Add) => {
                            start_proxy_add(&mut app);
                        }
                        Some(ProxyViewRow::Entry(_)) => {}
                        None => {}
                    }
                }
                TopTab::HostFs => match app.filesystem.current_row(scope) {
                    Some(FilesystemViewRow::Entry(row)) => {
                        if row.origin == ProxyOrigin::from_scope(scope) {
                            app.mode = Mode::ItemAction {
                                target: ItemActionTarget::Filesystem(row),
                                cursor: 0,
                            };
                        }
                    }
                    Some(FilesystemViewRow::Add(field)) => {
                        start_filesystem_add(&mut app, field);
                    }
                    Some(FilesystemViewRow::Section(_)) | None => {}
                },
                TopTab::McpClaude | TopTab::McpCodex => match app.active_mcp_mut().toggle(scope) {
                    RowAction::Handled => app.sync_tasks_from_active(),
                    RowAction::EditTask(name) => {
                        app.mode = Mode::ItemAction {
                            target: ItemActionTarget::Task(name),
                            cursor: 0,
                        };
                    }
                    RowAction::AddTask => start_task_add(&mut app),
                    RowAction::McpServerAction {
                        server_name,
                        can_auth,
                    } => {
                        let agent = if app.tab == TopTab::McpCodex {
                            McpAgent::Codex
                        } else {
                            McpAgent::Claude
                        };
                        app.mode = Mode::McpServerAction {
                            agent,
                            server_name,
                            can_auth,
                            cursor: 0,
                        };
                    }
                },
            },
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
        TopTab::General => render_general(f, chunks[2], app),
        TopTab::Proxy => render_proxy(f, chunks[2], app),
        TopTab::HostFs => render_host_fs(f, chunks[2], app),
        TopTab::McpClaude | TopTab::McpCodex => render_mcp(f, chunks[2], app),
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
        target,
        ref buffer,
        ref editing,
    } = app.mode
    {
        render_proxy_input_modal(f, area, target, buffer, editing.is_some());
    }

    if let Mode::FilesystemInput {
        field,
        ref buffer,
        mount_readonly,
        ref error,
        ref editing,
    } = app.mode
    {
        render_filesystem_input_modal(
            f,
            area,
            field,
            buffer,
            mount_readonly,
            error.as_deref(),
            editing.is_some(),
        );
    }

    if let Mode::DefaultAgentSelect { cursor } = app.mode {
        render_default_agent_select_modal(f, area, cursor);
    }

    if let Mode::BedrockRegionInput { ref buffer } = app.mode {
        render_bedrock_region_input_modal(f, area, buffer);
    }

    if let Mode::BypassWarningSelect { cursor } = app.mode {
        render_bypass_warning_select_modal(f, area, cursor);
    }

    if let Mode::ItemAction { ref target, cursor } = app.mode {
        render_item_action_modal(f, area, target, cursor);
    }

    if let Mode::McpServerAction {
        ref server_name,
        can_auth,
        cursor,
        ..
    } = app.mode
    {
        render_mcp_server_action_modal(f, area, server_name, can_auth, cursor);
    }

    if let Mode::ConfirmQuit { cursor } = app.mode {
        render_confirm_quit_modal(f, area, cursor);
    }
}

fn render_title(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let scope_label = match app.scope {
        Scope::Global => "Global",
        Scope::Workspace => "Workspace",
    };
    let title = Paragraph::new(Line::from(vec![
        Span::styled("agent-container", heading()),
        Span::raw("  settings  "),
        Span::styled(format!("[{scope_label}]"), heading()),
        Span::styled("  (t to switch scope)", muted()),
    ]));
    f.render_widget(title, area);
}

fn render_tabs(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let titles: Vec<Line> = TopTab::titles()
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            if idx == app.tab.index() {
                Line::from(Span::styled(format!(" {s} "), selected_bold()))
            } else {
                Line::from(Span::styled(format!(" {s} "), muted()))
            }
        })
        .collect();
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::BOTTOM))
        .select(app.tab.index())
        .divider("  ")
        .style(muted())
        .highlight_style(selected_bold());
    f.render_widget(tabs, area);
}

fn render_general(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let rows = app.general.visible_rows(app.scope);
    let items: Vec<ListItem> = rows
        .into_iter()
        .map(|row| match row {
            GeneralRow::DefaultAgent => {
                let agent = app.general.effective_agent(app.scope);
                let origin = app.general.origin(app.scope);
                let inherited = app.scope == Scope::Workspace && origin == ProxyOrigin::Global;
                let value_style = if inherited { muted() } else { heading() };
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::raw("Change Default Agent"),
                    Span::raw("  "),
                    Span::styled(agent.label(), value_style.add_modifier(Modifier::BOLD)),
                    Span::styled(if inherited { "  inherited" } else { "" }, muted()),
                ]))
            }
            GeneralRow::BedrockRegion => {
                let region = app.general.effective_bedrock_region(app.scope);
                let origin = app.general.bedrock_region_origin(app.scope);
                let inherited = app.scope == Scope::Workspace && origin == ProxyOrigin::Global;
                let value_style = if inherited { muted() } else { heading() };
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::raw("Change Bedrock Region"),
                    Span::raw("  "),
                    Span::styled(region, value_style.add_modifier(Modifier::BOLD)),
                    Span::styled(if inherited { "  inherited" } else { "" }, muted()),
                ]))
            }
            GeneralRow::BypassWarning => {
                let skip = app.general.effective_skip_bypass_warning(app.scope);
                let origin = app.general.bypass_warning_origin(app.scope);
                let inherited = app.scope == Scope::Workspace && origin == ProxyOrigin::Global;
                let value_style = if inherited { muted() } else { heading() };
                let label = if skip {
                    "Skip warning"
                } else {
                    "Confirm warning"
                };
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::raw("Bypass Permissions Warning"),
                    Span::raw("  "),
                    Span::styled(label, value_style.add_modifier(Modifier::BOLD)),
                    Span::styled(if inherited { "  inherited" } else { "" }, muted()),
                ]))
            }
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(selected_bold())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_proxy(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    render_pattern_list(f, area, app.scope, &app.proxy, &mut app.list_state);
}

fn render_host_fs(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    render_filesystem(f, area, app);
}

fn render_pattern_list(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    scope: Scope,
    state: &ProxyState,
    list_state: &mut ListState,
) {
    let rows = state.visible_rows(scope);
    let active = ProxyOrigin::from_scope(scope);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            ProxyViewRow::Entry(row) => {
                let overlay = scope == Scope::Workspace && row.origin == ProxyOrigin::Workspace;
                let is_inherited = row.origin != active;
                let pattern_style = if is_inherited {
                    // Slightly dim the inherited rows so the scope they
                    // belong to is obvious without an extra marker.
                    muted()
                } else {
                    plain()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(if overlay { "* " } else { "  " }.to_string(), heading()),
                    Span::styled(row.pattern.clone(), pattern_style),
                ]))
            }
            ProxyViewRow::Add => ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled("+ Add Allow Pattern...", muted()),
            ])),
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(selected_bold())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, list_state);
}

fn render_filesystem(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let rows = app.filesystem.visible_rows(app.scope);
    let active = ProxyOrigin::from_scope(app.scope);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            FilesystemViewRow::Section(section) => {
                let title = match section {
                    FilesystemSection::Path => "▾ Path",
                    FilesystemSection::Filter => "▾ Filter",
                };
                ListItem::new(Line::from(Span::styled(title, heading())))
            }
            FilesystemViewRow::Entry(row) => {
                let is_inherited = row.origin != active;
                let style = if is_inherited { muted() } else { plain() };
                let overlay = app.scope == Scope::Workspace && row.origin == ProxyOrigin::Workspace;
                let readonly = if row.field == FilesystemField::Mount {
                    if row.mount_readonly { "[x] " } else { "[ ] " }
                } else {
                    ""
                };
                ListItem::new(Line::from(vec![
                    Span::styled(if overlay { "* " } else { "  " }.to_string(), heading()),
                    Span::styled(format!("{:<8}", row.field.label()), muted()),
                    Span::raw(" "),
                    Span::styled(readonly, muted()),
                    Span::styled(row.value.clone(), style),
                ]))
            }
            FilesystemViewRow::Add(field) => ListItem::new(Line::from(vec![
                Span::raw("    "),
                Span::styled(field.add_label(), muted()),
            ])),
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(selected_bold())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_mcp(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let scope = app.scope;
    let rows = app.active_mcp().visible_rows(scope);
    let visible_tasks = app.active_mcp().effective_tasks(scope);
    let items: Vec<ListItem> = rows
        .into_iter()
        .map(|row| match row {
            McpRow::TaskRunnerHeader => {
                let marker = if app.active_mcp().task_runner_expanded {
                    "▾"
                } else {
                    "▸"
                };
                let count = visible_tasks.len();
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{marker} task-runner"), heading()),
                    Span::styled(
                        format!("  ({count} task{})", if count == 1 { "" } else { "s" }),
                        muted(),
                    ),
                    Span::styled("  host commands exposed as MCP tools", muted()),
                ]))
            }
            McpRow::TaskRow(name) => {
                let command = visible_tasks.get(&name).cloned().unwrap_or_default();
                let overlay =
                    scope == Scope::Workspace && app.active_mcp().task_is_workspace_override(&name);
                ListItem::new(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(if overlay { "* " } else { "  " }.to_string(), heading()),
                    Span::styled(name, heading()),
                    Span::raw(" = "),
                    Span::raw(command),
                ]))
            }
            McpRow::TaskAddHint => ListItem::new(Line::from(vec![
                Span::raw("      "),
                Span::styled("+ Add Task...", muted()),
            ])),
            McpRow::Server(si) => {
                let mcp = app.active_mcp();
                let name = &mcp.server_names[si];
                let (enabled, total) = mcp.enabled_count_for(scope, si);
                let marker = if mcp.expanded[si] { "▾" } else { "▸" };
                let transport = mcp
                    .server_transports
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or("mcp");
                let status = match mcp.server_status.get(name) {
                    Some(McpServerStatus::Loading) => Span::styled("  loading...", muted()),
                    Some(McpServerStatus::Failed { message, .. }) => Span::styled(
                        format!("  failed: {}", message.lines().next().unwrap_or("error")),
                        danger(),
                    ),
                    _ => Span::styled(format!("  ({enabled}/{total} enabled)"), muted()),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{marker} {name}"), heading()),
                    Span::styled(format!("  {transport}"), muted()),
                    status,
                ]))
            }
            McpRow::Tool(ti) => render_tool_row(
                &app.active_mcp().catalog[ti],
                app.active_mcp().effective_tool_allowed(scope, ti),
                scope == Scope::Workspace && app.active_mcp().tool_is_workspace_override(ti),
            ),
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(selected_bold())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.list_state);
}

/// `mark_overlay` paints a small `*` in front of the tool name when the
/// active scope owns an explicit per-tool entry (so the user can see
/// which checkbox states are inherited from global vs. overridden in
/// workspace).
fn render_tool_row(entry: &ToolEntry, enabled: bool, mark_overlay: bool) -> ListItem<'static> {
    let cb = if enabled { "[x]" } else { "[ ]" };
    let desc = entry
        .description
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    let annotation: Option<Span<'static>> = match entry.read_only_hint {
        Some(true) => Some(Span::styled(" [RO]", muted())),
        Some(false) => Some(Span::styled(" [W]", muted())),
        None => None,
    };

    let mut spans: Vec<Span<'static>> = vec![
        Span::raw("    "),
        Span::raw(format!("{cb} ")),
        Span::styled(
            if mark_overlay { "* " } else { "  " }.to_string(),
            heading(),
        ),
        Span::raw(entry.tool_name.clone()),
    ];
    if let Some(tag) = annotation {
        spans.push(tag);
    }
    if !desc.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(desc, muted()));
    }
    ListItem::new(Line::from(spans))
}

fn render_footer(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let key = |s: &str| Span::styled(s.to_string(), heading());

    let hints = shortcut_hints(app);
    let mut help_spans = Vec::new();
    for (idx, hint) in hints.iter().enumerate() {
        if idx > 0 {
            help_spans.push(Span::raw(" · "));
        }
        help_spans.push(key(hint.key));
        help_spans.push(Span::raw(format!(" {}", hint.label)));
    }
    let help = Line::from(help_spans);

    let status = match app.tab {
        TopTab::General => Line::from(vec![Span::styled(
            format!(
                "Global: {} · Workspace: {}",
                app.general.global.default_agent().label(),
                app.general
                    .workspace
                    .default_agent
                    .map(DefaultAgent::label)
                    .unwrap_or("inherited"),
            ),
            muted(),
        )]),
        TopTab::Proxy => Line::from(vec![Span::styled(
            format!(
                "Global: {} · Workspace: {} allow pattern(s)",
                app.proxy.global.len(),
                app.proxy.workspace.len(),
            ),
            muted(),
        )]),
        TopTab::HostFs => Line::from(vec![Span::styled(
            format!(
                "Global: {} mount, {} hide, {} readonly · Workspace: {} mount, {} hide, {} readonly",
                app.filesystem.global.mounts.len(),
                app.filesystem.global.hide.len(),
                app.filesystem.global.readonly.len(),
                app.filesystem.workspace.mounts.len(),
                app.filesystem.workspace.hide.len(),
                app.filesystem.workspace.readonly.len(),
            ),
            muted(),
        )]),
        TopTab::McpClaude | TopTab::McpCodex => {
            let mcp = app.active_mcp();
            let total = mcp.catalog.len();
            let enabled = (0..total)
                .filter(|i| mcp.effective_tool_allowed(app.scope, *i))
                .count();
            let task_count = mcp.effective_tasks(app.scope).len();
            Line::from(vec![Span::styled(
                format!(
                    "{task_count} task(s) · {enabled}/{total} tool(s) enabled across {} server(s)",
                    mcp.server_names.len()
                ),
                muted(),
            )])
        }
    };

    let para = Paragraph::new(vec![help, status]);
    f.render_widget(para, area);
}

fn render_proxy_input_modal(
    f: &mut ratatui::Frame<'_>,
    parent: Rect,
    target: PatternTarget,
    buffer: &TextField,
    is_edit: bool,
) {
    // Centered 60-char-wide 5-line modal.
    let w = parent.width.clamp(40, 72);
    let h: u16 = 5;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let title = match (target, is_edit) {
        (PatternTarget::Proxy, true) => " Edit proxy allow pattern ",
        (PatternTarget::Proxy, false) => " Add proxy allow pattern ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(muted())
        .title_style(heading());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let hint = Line::from(vec![Span::styled(
        "POSIX extended regex. Enter commit · Esc cancel · readline keys (^A/^E/^W/M-b/M-f…)",
        muted(),
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

fn render_filesystem_input_modal(
    f: &mut ratatui::Frame<'_>,
    parent: Rect,
    field: FilesystemField,
    buffer: &TextField,
    mount_readonly: bool,
    error: Option<&str>,
    is_edit: bool,
) {
    let w = parent.width.clamp(46, 84);
    let h: u16 = if field == FilesystemField::Mount {
        8
    } else {
        5
    };
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let action = if is_edit { "Edit" } else { "Add" };
    let title = format!(" {action} filesystem {} ", field.label());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(muted())
        .title_style(heading());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let hint_text = match field {
        FilesystemField::Mount => {
            "Host directory path. Relative paths are resolved from the current workspace."
        }
        FilesystemField::Hide => {
            "Regex matched against paths relative to each mounted root; matching paths are hidden."
        }
        FilesystemField::Readonly => {
            "Regex matched against paths relative to each mounted root; matching paths are mounted read-only."
        }
    };
    let hint = Line::from(vec![Span::styled(hint_text, muted())]);
    let body = Line::from(vec![Span::raw("> "), Span::raw(buffer.value())]);
    let mut lines = vec![hint, Line::from(""), body];
    if field == FilesystemField::Mount {
        let checkbox = if mount_readonly { "[x]" } else { "[ ]" };
        lines.push(Line::from(vec![
            Span::styled(checkbox, heading()),
            Span::raw(" Readonly"),
            Span::styled("  Space toggle", muted()),
        ]));
    }
    if let Some(error) = error {
        lines.push(Line::from(Span::styled(error.to_string(), danger())));
    }
    let para = Paragraph::new(lines);
    f.render_widget(para, inner);

    let cursor_x = inner.x + 2 + buffer.prefix_width();
    let cursor_y = inner.y + 2;
    f.set_cursor_position(Position::new(cursor_x, cursor_y));
}

fn render_confirm_quit_modal(f: &mut ratatui::Frame<'_>, parent: Rect, cursor: usize) {
    let w = parent.width.clamp(40, 56);
    let h: u16 = 9;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Exit settings? ")
        .border_style(muted())
        .title_style(heading());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(Span::styled(
            "Choose how to leave the settings editor.",
            plain(),
        )),
        Line::from(""),
    ];
    for (idx, action) in QUIT_ACTION_CHOICES.iter().copied().enumerate() {
        let selected = idx == cursor;
        let marker = if selected { ">" } else { " " };
        let (label, style) = match action {
            QuitAction::SaveAndQuit => ("Save and Quit", plain()),
            QuitAction::KeepEditing => ("Keep Editing", plain()),
            QuitAction::DiscardAndQuit => ("Discard and Quit", danger()),
        };
        let style = if selected {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::raw(" "),
            Span::styled(label, style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ j/k move · Enter select · Esc cancel",
        muted(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_default_agent_select_modal(f: &mut ratatui::Frame<'_>, parent: Rect, cursor: usize) {
    let w = parent.width.clamp(36, 48);
    let h: u16 = 9;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Select default agent ")
        .border_style(muted())
        .title_style(heading());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![Line::from(Span::styled(
        "Choose the agent for this scope.",
        muted(),
    ))];
    for (idx, agent) in DEFAULT_AGENT_CHOICES.iter().copied().enumerate() {
        let selected = idx == cursor;
        let marker = if selected { ">" } else { " " };
        let style = if selected { heading() } else { plain() };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::raw(" "),
            Span::styled(agent.map(DefaultAgent::label).unwrap_or("(unset)"), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ j/k move · Enter select · Esc cancel",
        muted(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_bedrock_region_input_modal(f: &mut ratatui::Frame<'_>, parent: Rect, buffer: &TextField) {
    let w = parent.width.clamp(44, 66);
    let h: u16 = 6;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Bedrock region ")
        .border_style(muted())
        .title_style(heading());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let hint = Line::from(vec![Span::styled(
        "AWS region. Empty value inherits the lower scope/default.",
        muted(),
    )]);
    let body = Line::from(vec![Span::raw("> "), Span::raw(buffer.value())]);
    let help = Line::from(Span::styled(
        "Enter commit · Esc cancel · readline keys (^A/^E/^W/M-b/M-f…)",
        muted(),
    ));
    let para = Paragraph::new(vec![hint, Line::from(""), body, help]);
    f.render_widget(para, inner);

    let cursor_x = inner.x + 2 + buffer.prefix_width();
    let cursor_y = inner.y + 2;
    f.set_cursor_position(Position::new(cursor_x, cursor_y));
}

fn render_bypass_warning_select_modal(f: &mut ratatui::Frame<'_>, parent: Rect, cursor: usize) {
    let w = parent.width.clamp(44, 62);
    let h: u16 = 9;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Bypass permissions warning ")
        .border_style(muted())
        .title_style(heading());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![Line::from(Span::styled(
        "Choose whether Claude Code asks before bypass mode.",
        muted(),
    ))];
    for (idx, choice) in BYPASS_WARNING_CHOICES.iter().copied().enumerate() {
        let selected = idx == cursor;
        let marker = if selected { ">" } else { " " };
        let label = match choice {
            None => "(unset)",
            Some(false) => "Confirm warning",
            Some(true) => "Skip warning",
        };
        let style = if selected { heading() } else { plain() };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::raw(" "),
            Span::styled(label, style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ j/k move · Enter select · Esc cancel",
        muted(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn item_action_title(target: &ItemActionTarget) -> &'static str {
    match target {
        ItemActionTarget::Proxy(_) => " Proxy allow pattern ",
        ItemActionTarget::Filesystem(row) => match row.field {
            FilesystemField::Mount => " Filesystem path ",
            FilesystemField::Hide => " Hidden filter ",
            FilesystemField::Readonly => " Readonly filter ",
        },
        ItemActionTarget::Task(_) => " Task runner command ",
    }
}

fn item_action_value(target: &ItemActionTarget) -> String {
    match target {
        ItemActionTarget::Proxy(row) => row.pattern.clone(),
        ItemActionTarget::Filesystem(row) => row.value.clone(),
        ItemActionTarget::Task(name) => name.clone(),
    }
}

fn render_item_action_modal(
    f: &mut ratatui::Frame<'_>,
    parent: Rect,
    target: &ItemActionTarget,
    cursor: usize,
) {
    let w = parent.width.clamp(40, 56);
    let h: u16 = 8;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(item_action_title(target))
        .border_style(muted())
        .title_style(heading());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(Span::styled(item_action_value(target), plain())),
        Line::from(""),
    ];
    for (idx, action) in ITEM_ACTION_CHOICES.iter().copied().enumerate() {
        let selected = idx == cursor;
        let marker = if selected { ">" } else { " " };
        let (label, style) = match action {
            ItemAction::Edit => ("Edit", plain()),
            ItemAction::Remove => ("Remove", danger()),
        };
        let style = if selected {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::raw(" "),
            Span::styled(label, style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ j/k move · Enter select · Esc cancel",
        muted(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_mcp_server_action_modal(
    f: &mut ratatui::Frame<'_>,
    parent: Rect,
    server_name: &str,
    can_auth: bool,
    cursor: usize,
) {
    let w = parent.width.clamp(44, 64);
    let h: u16 = if can_auth { 8 } else { 7 };
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" MCP server ")
        .border_style(muted())
        .title_style(heading());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(Span::styled(server_name.to_string(), plain())),
        Line::from(""),
    ];
    let choices = mcp_server_action_choices(can_auth);
    for (idx, action) in choices.iter().copied().enumerate() {
        let selected = idx == cursor;
        let marker = if selected { ">" } else { " " };
        let label = match action {
            McpServerAction::Reload => "Reload tools",
            McpServerAction::Reauthenticate => "Re-authenticate",
        };
        let style = if selected { heading() } else { plain() };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::raw(" "),
            Span::styled(label, style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ j/k move · Enter select · Esc cancel",
        muted(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_task_input_modal(
    f: &mut ratatui::Frame<'_>,
    parent: Rect,
    name: &TextField,
    command: &TextField,
    focus: TaskField,
    is_edit: bool,
) {
    let w = parent.width.clamp(50, 80);
    let h: u16 = 8;
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect::new(x, y, w, h);

    f.render_widget(Clear, area);
    let title = if is_edit { " Edit task " } else { " Add task " };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(muted())
        .title_style(heading());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let focus_style = |f: TaskField, row: TaskField| {
        if f == row { selected_bold() } else { muted() }
    };

    let hint = Line::from(vec![Span::styled(
        "Tab/↑↓ switch · Enter commit · Esc cancel · readline keys (^A/^E/^W/M-b/M-f…)",
        muted(),
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
    let para = Paragraph::new(vec![
        hint,
        Line::from(""),
        name_line,
        Line::from(""),
        cmd_line,
    ]);
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
            servers_from_catalog(&catalog),
            catalog,
            mcp_global,
            mcp_workspace,
            tasks_global,
            tasks_workspace,
        )
    }

    fn servers_from_catalog(catalog: &[ToolEntry]) -> Vec<McpServerEntry> {
        let mut names: Vec<String> = catalog
            .iter()
            .map(|entry| entry.server_name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| McpServerEntry {
                name,
                transport: "stdio".into(),
            })
            .collect()
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
    fn mcp_rows_start_collapsed() {
        let mut tasks_global = BTreeMap::new();
        tasks_global.insert("build".to_string(), "cargo build".to_string());
        let state = make_state(
            vec![entry("server", "tool", Some(true))],
            McpPolicy::default(),
            McpPolicy::default(),
            tasks_global,
            BTreeMap::new(),
        );

        assert!(!state.task_runner_expanded);
        assert_eq!(state.expanded, vec![false]);
        let rows = state.visible_rows(Scope::Global);
        assert!(matches!(
            rows.as_slice(),
            [McpRow::TaskRunnerHeader, McpRow::Server(0)]
        ));
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
        let state = make_state(vec![], McpPolicy::default(), McpPolicy::default(), tg, tw);
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
        let state = make_state(vec![], McpPolicy::default(), McpPolicy::default(), tg, tw);
        assert_eq!(
            state.effective_tasks(Scope::Workspace).get("k").unwrap(),
            "workspace",
        );
    }

    fn fresh_input() -> TuiInput {
        TuiInput {
            workspace: PathBuf::from("/tmp"),
            initial_scope: Scope::Workspace,
            general_global: GeneralPolicy {
                default_agent: Some(DefaultAgent::Claude),
                ..Default::default()
            },
            general_workspace: GeneralPolicy::default(),
            claude_global: ClaudePolicy::default(),
            claude_workspace: ClaudePolicy::default(),
            proxy_allow_global: vec!["g".into()],
            proxy_allow_workspace: vec!["w".into()],
            filesystem_global: FilesystemPolicy {
                mounts: vec![FilesystemMount::new("/tmp/shared".into(), false)],
                hide: vec![r"(^|/)\.env$".into()],
                readonly: vec![r"(^|/)\.claude(/|$)".into()],
            },
            filesystem_workspace: FilesystemPolicy {
                mounts: Vec::new(),
                hide: vec![r"^secrets(/|$)".into()],
                readonly: Vec::new(),
            },
            claude_servers: vec![McpServerEntry {
                name: "s".into(),
                transport: "stdio".into(),
            }],
            codex_servers: vec![McpServerEntry {
                name: "local-tools".into(),
                transport: "stdio".into(),
            }],
            claude_tool_catalog: vec![entry("s", "t", Some(true))],
            codex_tool_catalog: vec![entry("local-tools", "search", Some(true))],
            mcp_events: None,
            mcp_commands: None,
            mcp_global: McpPolicy::default(),
            mcp_workspace: McpPolicy::default(),
            codex_mcp_global: McpPolicy::default(),
            codex_mcp_workspace: McpPolicy::default(),
            tasks_global: BTreeMap::new(),
            tasks_workspace: BTreeMap::new(),
        }
    }

    #[test]
    fn has_unsaved_changes_reports_no_diff_at_launch() {
        let app = App::new(fresh_input());
        assert!(!app.has_unsaved_changes());
    }

    #[test]
    fn has_unsaved_changes_detects_proxy_edit() {
        let mut app = App::new(fresh_input());
        app.proxy.workspace.push("w2".into());
        assert!(app.has_unsaved_changes());
    }

    #[test]
    fn has_unsaved_changes_detects_default_agent_edit() {
        let mut app = App::new(fresh_input());
        app.general
            .set_agent(Scope::Workspace, Some(DefaultAgent::Codex));
        assert!(app.has_unsaved_changes());
    }

    #[test]
    fn workspace_default_agent_inherits_until_overridden() {
        let mut general = GeneralState::new(
            GeneralPolicy {
                default_agent: Some(DefaultAgent::Codex),
                ..Default::default()
            },
            GeneralPolicy::default(),
            ClaudePolicy::default(),
            ClaudePolicy::default(),
        );
        assert_eq!(
            general.effective_agent(Scope::Workspace),
            DefaultAgent::Codex
        );
        assert_eq!(general.origin(Scope::Workspace), ProxyOrigin::Global);

        general.set_agent(Scope::Workspace, Some(DefaultAgent::Claude));
        assert_eq!(
            general.effective_agent(Scope::Workspace),
            DefaultAgent::Claude
        );
        assert_eq!(general.origin(Scope::Workspace), ProxyOrigin::Workspace);
    }

    #[test]
    fn general_rows_include_default_agent_and_bypass_warning() {
        let mut general = GeneralState::new(
            GeneralPolicy {
                default_agent: Some(DefaultAgent::Codex),
                ..Default::default()
            },
            GeneralPolicy::default(),
            ClaudePolicy::default(),
            ClaudePolicy::default(),
        );
        assert_eq!(
            general.visible_rows(Scope::Workspace),
            vec![
                GeneralRow::DefaultAgent,
                GeneralRow::BedrockRegion,
                GeneralRow::BypassWarning,
            ]
        );

        general.set_agent(Scope::Workspace, Some(DefaultAgent::Claude));
        assert_eq!(
            general.visible_rows(Scope::Workspace),
            vec![
                GeneralRow::DefaultAgent,
                GeneralRow::BedrockRegion,
                GeneralRow::BypassWarning,
            ]
        );
    }

    #[test]
    fn bedrock_region_inherits_until_overridden() {
        let mut general = GeneralState::new(
            GeneralPolicy {
                bedrock_region: Some("us-west-2".into()),
                ..Default::default()
            },
            GeneralPolicy::default(),
            ClaudePolicy::default(),
            ClaudePolicy::default(),
        );
        assert_eq!(
            general.effective_bedrock_region(Scope::Workspace),
            "us-west-2"
        );
        assert_eq!(
            general.bedrock_region_origin(Scope::Workspace),
            ProxyOrigin::Global
        );

        general.set_bedrock_region(Scope::Workspace, Some("eu-central-1".into()));
        assert_eq!(
            general.effective_bedrock_region(Scope::Workspace),
            "eu-central-1"
        );
        assert_eq!(
            general.bedrock_region_origin(Scope::Workspace),
            ProxyOrigin::Workspace
        );
    }

    #[test]
    fn bedrock_region_input_commits_trimmed_value() {
        let mut app = App::new(fresh_input());
        app.mode = Mode::BedrockRegionInput {
            buffer: TextField::from_str(" us-west-2 "),
        };

        handle_bedrock_region_input_key(&mut app, KeyCode::Enter, KeyModifiers::empty());

        assert_eq!(
            app.general.workspace.bedrock_region.as_deref(),
            Some("us-west-2")
        );
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn default_agent_select_modal_commits_explicit_choice() {
        let mut app = App::new(fresh_input());
        app.mode = Mode::DefaultAgentSelect {
            cursor: default_agent_index(Some(DefaultAgent::Codex)),
        };

        handle_default_agent_select_key(&mut app, KeyCode::Enter);

        assert_eq!(
            app.general.workspace.default_agent,
            Some(DefaultAgent::Codex)
        );
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn default_agent_select_modal_can_unset_scope_value() {
        let mut app = App::new(fresh_input());
        app.general
            .set_agent(Scope::Workspace, Some(DefaultAgent::Codex));
        app.mode = Mode::DefaultAgentSelect {
            cursor: default_agent_index(None),
        };

        handle_default_agent_select_key(&mut app, KeyCode::Enter);

        assert_eq!(app.general.workspace.default_agent, None);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn bypass_warning_select_modal_commits_explicit_choice() {
        let mut app = App::new(fresh_input());
        app.mode = Mode::BypassWarningSelect {
            cursor: bypass_warning_index(Some(true)),
        };

        handle_bypass_warning_select_key(&mut app, KeyCode::Enter);

        assert_eq!(
            app.general.claude_workspace.skip_bypass_permissions_warning,
            Some(true)
        );
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn bypass_warning_defaults_to_confirm_until_overridden() {
        let mut app = App::new(fresh_input());
        assert!(!app.general.effective_skip_bypass_warning(Scope::Workspace));

        app.general
            .set_skip_bypass_warning(Scope::Global, Some(true));
        assert!(app.general.effective_skip_bypass_warning(Scope::Workspace));
        assert_eq!(
            app.general.bypass_warning_origin(Scope::Workspace),
            ProxyOrigin::Global
        );

        app.general
            .set_skip_bypass_warning(Scope::Workspace, Some(false));
        assert!(!app.general.effective_skip_bypass_warning(Scope::Workspace));
        assert_eq!(
            app.general.bypass_warning_origin(Scope::Workspace),
            ProxyOrigin::Workspace
        );
    }

    #[test]
    fn quit_menu_choices_are_menu_ordered() {
        assert!(matches!(
            QUIT_ACTION_CHOICES,
            [
                QuitAction::SaveAndQuit,
                QuitAction::KeepEditing,
                QuitAction::DiscardAndQuit
            ]
        ));
    }

    #[test]
    fn has_unsaved_changes_detects_filesystem_edit() {
        let mut app = App::new(fresh_input());
        app.filesystem
            .workspace
            .mounts
            .push(FilesystemMount::new("/tmp/other".into(), false));
        assert!(app.has_unsaved_changes());
    }

    #[test]
    fn has_unsaved_changes_detects_mcp_toggle() {
        let mut app = App::new(fresh_input());
        // Catalog has one (s, t) pair; flipping it writes through to mcp_workspace.
        let cur = app.mcp_claude.effective_tool_allowed(Scope::Workspace, 0);
        app.mcp_claude.set_tool_for(Scope::Workspace, 0, !cur);
        assert!(app.has_unsaved_changes());
    }

    #[test]
    fn has_unsaved_changes_detects_task_edit() {
        let mut app = App::new(fresh_input());
        app.mcp_claude
            .set_task_for(Scope::Global, "new".into(), "echo new".into());
        assert!(app.has_unsaved_changes());
    }

    #[test]
    fn proxy_visible_rows_global_view_shows_only_global() {
        let p = ProxyState::new(vec!["g1".into(), "g2".into()], vec!["w1".into()]);
        let rows = p.entry_rows(Scope::Global);
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
        let rows = p.entry_rows(Scope::Workspace);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].origin, ProxyOrigin::Global);
        assert_eq!(rows[0].pattern, "g1");
        assert_eq!(rows[1].origin, ProxyOrigin::Global);
        assert_eq!(rows[1].pattern, "g2");
        assert_eq!(rows[2].origin, ProxyOrigin::Workspace);
        assert_eq!(rows[2].pattern, "w1");
    }

    #[test]
    fn proxy_visible_rows_include_row_actions() {
        let p = ProxyState::new(vec!["g1".into()], vec!["w1".into()]);
        let rows = p.visible_rows(Scope::Workspace);
        assert!(matches!(rows[0], ProxyViewRow::Entry(_)));
        assert!(matches!(rows[1], ProxyViewRow::Entry(_)));
        assert!(matches!(rows[2], ProxyViewRow::Add));
    }

    #[test]
    fn proxy_remove_current_workspace_view_skips_global_rows() {
        let mut p = ProxyState::new(vec!["g1".into()], vec!["w1".into()]);
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
        let mut p = ProxyState::new(vec!["g1".into()], vec!["w1".into()]);
        // Edit the workspace row in workspace view.
        p.cursor = 1;
        let row = p.current_entry(Scope::Workspace).unwrap();
        p.upsert(Scope::Workspace, "w1-renamed".to_string(), Some(row));
        assert_eq!(p.global, vec!["g1".to_string()]);
        assert_eq!(p.workspace, vec!["w1-renamed".to_string()]);

        // Add a new workspace entry; global stays untouched.
        p.upsert(Scope::Workspace, "w2".to_string(), None);
        assert_eq!(p.global, vec!["g1".to_string()]);
        assert_eq!(
            p.workspace,
            vec!["w1-renamed".to_string(), "w2".to_string()]
        );
    }

    #[test]
    fn proxy_upsert_dedupes_within_active_scope() {
        let mut p = ProxyState::new(vec![], vec!["w1".into()]);
        p.upsert(Scope::Workspace, "w1".to_string(), None);
        // Already present, must not be re-appended.
        assert_eq!(p.workspace, vec!["w1".to_string()]);
    }

    #[test]
    fn filesystem_visible_rows_group_paths_and_filters() {
        let state = FilesystemState::new(
            FilesystemPolicy {
                mounts: vec![FilesystemMount::new("/tmp/shared".into(), true)],
                hide: vec!["secret".into()],
                readonly: vec!["readonly".into()],
            },
            FilesystemPolicy::default(),
        );
        let rows = state.visible_rows(Scope::Global);
        assert!(matches!(
            rows[0],
            FilesystemViewRow::Section(FilesystemSection::Path)
        ));
        assert!(matches!(rows[1], FilesystemViewRow::Entry(_)));
        assert!(matches!(
            rows[2],
            FilesystemViewRow::Add(FilesystemField::Mount)
        ));
        assert!(matches!(
            rows[3],
            FilesystemViewRow::Section(FilesystemSection::Filter)
        ));
        assert!(matches!(rows[4], FilesystemViewRow::Entry(_)));
        assert!(matches!(rows[5], FilesystemViewRow::Entry(_)));
        assert!(matches!(
            rows[6],
            FilesystemViewRow::Add(FilesystemField::Hide)
        ));
        assert!(matches!(
            rows[7],
            FilesystemViewRow::Add(FilesystemField::Readonly)
        ));
    }

    #[test]
    fn add_shortcut_opens_proxy_add_input() {
        let mut app = App::new(fresh_input());
        app.tab = TopTab::Proxy;

        start_add_for_current_context(&mut app);

        assert!(matches!(app.mode, Mode::ProxyInput { editing: None, .. }));
    }

    #[test]
    fn add_shortcut_uses_current_filesystem_context() {
        let mut app = App::new(fresh_input());
        app.tab = TopTab::HostFs;
        app.filesystem.cursor = 3; // Filter section.

        start_add_for_current_context(&mut app);

        assert!(matches!(
            app.mode,
            Mode::FilesystemInput {
                field: FilesystemField::Hide,
                editing: None,
                ..
            }
        ));
    }

    #[test]
    fn add_shortcut_opens_task_input_from_task_runner_rows() {
        let mut app = App::new(fresh_input());
        app.tab = TopTab::McpClaude;
        app.mcp_claude.cursor = 0; // task-runner header.

        start_add_for_current_context(&mut app);

        assert!(matches!(app.mode, Mode::TaskInput { editing: None, .. }));
    }

    #[test]
    fn item_action_modal_removes_existing_filesystem_item() {
        let mut app = App::new(fresh_input());
        app.scope = Scope::Workspace;
        app.filesystem.cursor = 6;
        let Some(FilesystemViewRow::Entry(row)) = app.filesystem.current_row(Scope::Workspace)
        else {
            panic!("expected workspace filesystem entry");
        };
        app.mode = Mode::ItemAction {
            target: ItemActionTarget::Filesystem(row),
            cursor: 1,
        };

        handle_item_action_key(&mut app, KeyCode::Enter, KeyModifiers::empty());

        assert!(app.filesystem.workspace.hide.is_empty());
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn shortcut_hints_include_save_and_contextual_remove() {
        let mut app = App::new(fresh_input());
        app.tab = TopTab::Proxy;
        app.scope = Scope::Workspace;
        app.proxy.cursor = 0; // inherited global row

        let inherited = shortcut_hints(&app);
        assert!(
            inherited
                .iter()
                .any(|hint| hint.key == "s" && hint.label == "save")
        );
        assert!(
            inherited
                .iter()
                .any(|hint| hint.key == "a" && hint.label == "add")
        );
        assert!(!inherited.iter().any(|hint| hint.key == "d"));

        app.proxy.cursor = 1; // workspace-owned row
        let owned = shortcut_hints(&app);
        assert!(
            owned
                .iter()
                .any(|hint| hint.key == "d" && hint.label == "remove")
        );
    }

    #[test]
    fn delete_shortcut_removes_only_active_scope_item() {
        let mut app = App::new(fresh_input());
        app.tab = TopTab::Proxy;
        app.scope = Scope::Workspace;
        app.proxy.cursor = 0; // inherited global row

        assert!(!remove_current_context_item(&mut app));
        assert_eq!(app.proxy.global, vec!["g".to_string()]);

        app.proxy.cursor = 1; // workspace row
        assert!(remove_current_context_item(&mut app));
        assert!(app.proxy.workspace.is_empty());
    }

    #[test]
    fn shortcut_hints_do_not_duplicate_meanings() {
        let mut app = App::new(fresh_input());
        app.tab = TopTab::HostFs;
        app.scope = Scope::Workspace;
        app.filesystem.cursor = 6; // workspace-owned hide filter

        let hints = shortcut_hints(&app);
        let mut labels = std::collections::BTreeSet::new();
        for hint in hints {
            assert!(
                labels.insert(hint.label),
                "duplicate shortcut meaning: {}",
                hint.label
            );
        }
    }

    #[test]
    fn filesystem_mount_input_resolves_relative_path_and_defaults_readonly() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let notes = workspace.join("notes");
        std::fs::create_dir_all(&notes).unwrap();

        let mut input = fresh_input();
        input.workspace = workspace.clone();
        let mut app = App::new(input);
        app.mode = Mode::FilesystemInput {
            field: FilesystemField::Mount,
            buffer: TextField::from_str("notes"),
            mount_readonly: true,
            error: None,
            editing: None,
        };

        handle_filesystem_input_key(&mut app, KeyCode::Enter, KeyModifiers::empty());

        assert_eq!(
            app.filesystem.workspace.mounts,
            vec![FilesystemMount::new(
                std::fs::canonicalize(notes).unwrap().display().to_string(),
                true,
            )]
        );
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn filesystem_mount_input_rejects_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let mut input = fresh_input();
        input.workspace = workspace;
        let mut app = App::new(input);
        app.mode = Mode::FilesystemInput {
            field: FilesystemField::Mount,
            buffer: TextField::from_str("missing"),
            mount_readonly: true,
            error: None,
            editing: None,
        };

        handle_filesystem_input_key(&mut app, KeyCode::Enter, KeyModifiers::empty());

        assert!(app.filesystem.workspace.mounts.is_empty());
        assert!(matches!(
            app.mode,
            Mode::FilesystemInput { error: Some(_), .. }
        ));
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

    #[test]
    fn render_tool_row_accepts_multibyte_descriptions() {
        let item = render_tool_row(
            &ToolEntry {
                server_name: "moneyforward".to_string(),
                tool_name: "create_transaction".to_string(),
                description:
                    "新しい入出金を MoneyForward に登録します。amount は正=収入 / 負=支出。"
                        .to_string(),
                read_only_hint: Some(false),
            },
            true,
            false,
        );
        assert_eq!(item.height(), 1);
    }
}
