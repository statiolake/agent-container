//! Built-in MCP server for controlled host filesystem access.
//!
//! This is intentionally mediated on the host side. Every tool call
//! reloads the merged agent-container settings and checks the current
//! `[filesystem]` policy before touching the requested path.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Value, json};

pub const NAME: &str = "host-fs";

const PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_READ_BYTES: u64 = 1024 * 1024;
const RG_MAX_SEARCH_FILE_SIZE: &str = "1M";
const DEFAULT_LIST_DEPTH: usize = 1;
const MAX_LIST_DEPTH: usize = 20;
const DEFAULT_SEARCH_RESULTS: usize = 100;
const MAX_SEARCH_RESULTS: usize = 1000;

#[derive(Debug, Clone)]
pub struct HostFs {
    workspace: PathBuf,
}

impl HostFs {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    pub async fn handle(&self, body: &[u8]) -> Option<Value> {
        let parsed: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return Some(parse_error(format!("invalid JSON: {e}"))),
        };
        let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
        let id = parsed.get("id").cloned();
        let id = id?;

        match method {
            "initialize" => Some(self.initialize(id)),
            "notifications/initialized" => None,
            "ping" => Some(success(id, json!({}))),
            "tools/list" => Some(self.tools_list(id)),
            "tools/call" => Some(self.tools_call(id, &parsed).await),
            "resources/list" | "resources/templates/list" => {
                Some(success(id, json!({ "resources": [] })))
            }
            "prompts/list" => Some(success(id, json!({ "prompts": [] }))),
            other => Some(method_not_found(id, other)),
        }
    }

    fn initialize(&self, id: Value) -> Value {
        success(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": { "listChanged": false }
                },
                "serverInfo": {
                    "name": NAME,
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )
    }

    fn tools_list(&self, id: Value) -> Value {
        success(
            id,
            json!({
                "tools": [
                    {
                        "name": "HostRead",
                        "description": "Read a UTF-8 text file from a mounted host filesystem root when the current [filesystem] filters do not hide it.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Absolute host path to a text file." }
                            },
                            "required": ["path"],
                            "additionalProperties": false
                        },
                        "annotations": { "readOnlyHint": true }
                    },
                    {
                        "name": "HostList",
                        "description": "List one host directory from a mounted host filesystem root, omitting paths hidden by the current [filesystem] filters.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Absolute host path to a directory." },
                                "depth": { "type": "integer", "minimum": 1, "maximum": 20, "description": "How many directory levels to list. Defaults to 1." }
                            },
                            "required": ["path"],
                            "additionalProperties": false
                        },
                        "annotations": { "readOnlyHint": true }
                    },
                    {
                        "name": "HostWrite",
                        "description": "Write UTF-8 text to a host file when its absolute path is under a mounted host filesystem root and not hidden or readonly by the current [filesystem] filters.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Absolute host path to write." },
                                "content": { "type": "string", "description": "UTF-8 file contents." },
                                "createParents": { "type": "boolean", "description": "Create missing parent directories before writing." }
                            },
                            "required": ["path", "content"],
                            "additionalProperties": false
                        },
                        "annotations": { "readOnlyHint": false }
                    },
                    {
                        "name": "HostSearch",
                        "description": "Search files under an allowed host directory using host ripgrep. Results are capped and filtered by the current [filesystem] policy.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Absolute host directory to search." },
                                "query": { "type": "string", "description": "Literal text to find." },
                                "maxResults": { "type": "integer", "minimum": 1, "maximum": 1000 },
                                "hidden": { "type": "boolean", "description": "Pass --hidden to ripgrep so hidden files and directories are searched, subject to [filesystem] filters." },
                                "noIgnore": { "type": "boolean", "description": "Pass --no-ignore to ripgrep." },
                                "noIgnoreVcs": { "type": "boolean", "description": "Pass --no-ignore-vcs to ripgrep." }
                            },
                            "required": ["path", "query"],
                            "additionalProperties": false
                        },
                        "annotations": { "readOnlyHint": true }
                    }
                ]
            }),
        )
    }

    async fn tools_call(&self, id: Value, req: &Value) -> Value {
        let params = req.get("params");
        let name = params.and_then(|p| p.get("name")).and_then(Value::as_str);
        let arguments = params.and_then(|p| p.get("arguments"));
        let Some(name) = name else {
            return invalid_params(id, "tools/call missing `params.name`");
        };

        let result = match name {
            "HostRead" => self.host_read(arguments),
            "HostList" => self.host_list(arguments),
            "HostWrite" => self.host_write(arguments),
            "HostSearch" => self.host_search(arguments),
            other => Err(anyhow::anyhow!("unknown host-fs tool '{other}'")),
        };

        match result {
            Ok(text) => success(
                id,
                json!({
                    "content": [ { "type": "text", "text": text } ],
                    "isError": false,
                }),
            ),
            Err(e) => success(
                id,
                json!({
                    "content": [ { "type": "text", "text": format!("{e:#}") } ],
                    "isError": true,
                }),
            ),
        }
    }

    fn host_read(&self, arguments: Option<&Value>) -> Result<String> {
        let path = required_string(arguments, "path")?;
        let path = resolve_existing_path(path)?;
        self.ensure_readable(&path)?;
        let meta = std::fs::metadata(&path)
            .with_context(|| format!("failed to stat {}", path.display()))?;
        if !meta.is_file() {
            bail!("{} is not a file", path.display());
        }
        if meta.len() > MAX_READ_BYTES {
            bail!(
                "{} is {} bytes; refusing to read more than {} bytes",
                path.display(),
                meta.len(),
                MAX_READ_BYTES
            );
        }
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read UTF-8 text from {}", path.display()))
    }

    fn host_list(&self, arguments: Option<&Value>) -> Result<String> {
        let path = required_string(arguments, "path")?;
        let depth = optional_usize(arguments, "depth")
            .unwrap_or(DEFAULT_LIST_DEPTH)
            .clamp(1, MAX_LIST_DEPTH);
        let path = resolve_existing_path(path)?;
        let policy = self.latest_policy()?;
        let matcher = FilesystemMatcher::new(&self.workspace, &policy)?;
        ensure_listable_by(&matcher, &path)?;
        let mut entries = Vec::new();
        collect_list_entries(&matcher, &path, depth, 0, &mut entries)?;
        serde_json::to_string_pretty(&entries).context("failed to encode directory listing")
    }

    fn host_write(&self, arguments: Option<&Value>) -> Result<String> {
        let path = required_string(arguments, "path")?;
        let content = required_string(arguments, "content")?;
        let create_parents = optional_bool(arguments, "createParents").unwrap_or(false);
        let path = resolve_write_path(path)?;
        self.ensure_writable(&path)?;
        if create_parents && let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        ))
    }

    fn host_search(&self, arguments: Option<&Value>) -> Result<String> {
        let path = required_string(arguments, "path")?;
        let query = required_string(arguments, "query")?;
        if query.is_empty() {
            bail!("query must not be empty");
        }
        let max_results = optional_usize(arguments, "maxResults")
            .unwrap_or(DEFAULT_SEARCH_RESULTS)
            .clamp(1, MAX_SEARCH_RESULTS);
        let options = RipgrepSearchOptions {
            hidden: optional_bool(arguments, "hidden").unwrap_or(false),
            no_ignore: optional_bool(arguments, "noIgnore").unwrap_or(false),
            no_ignore_vcs: optional_bool(arguments, "noIgnoreVcs").unwrap_or(false),
        };
        let path = resolve_existing_path(path)?;
        let policy = self.latest_policy()?;
        let matcher = FilesystemMatcher::new(&self.workspace, &policy)?;
        ensure_readable_by(&matcher, &path)?;
        let results = search_with_ripgrep(&matcher, &path, query, max_results, options)?;
        serde_json::to_string_pretty(&results).context("failed to encode search results")
    }

    fn ensure_readable(&self, path: &Path) -> Result<()> {
        let policy = self.latest_policy()?;
        let matcher = FilesystemMatcher::new(&self.workspace, &policy)?;
        ensure_readable_by(&matcher, path)
    }

    fn ensure_writable(&self, path: &Path) -> Result<()> {
        let policy = self.latest_policy()?;
        match classify_path(&self.workspace, &policy, path)? {
            FilesystemAccess::Readwrite => Ok(()),
            FilesystemAccess::Readonly => {
                bail!("{} is readonly by [filesystem].readonly", path.display())
            }
            FilesystemAccess::Hidden => bail!(
                "{} is hidden by [filesystem].hide or outside mounted roots",
                path.display()
            ),
        }
    }

    fn latest_policy(&self) -> Result<crate::settings::FilesystemPolicy> {
        let settings = crate::settings::Settings::load_merged(&self.workspace)
            .context("failed to load latest filesystem policy")?;
        Ok(settings.filesystem)
    }
}

fn ensure_readable_by(matcher: &FilesystemMatcher, path: &Path) -> Result<()> {
    match matcher.classify_existing_or_absolute(path)? {
        FilesystemAccess::Readwrite | FilesystemAccess::Readonly => Ok(()),
        FilesystemAccess::Hidden => bail!(
            "{} is hidden by [filesystem].hide or outside mounted roots",
            path.display()
        ),
    }
}

fn ensure_listable_by(matcher: &FilesystemMatcher, path: &Path) -> Result<()> {
    if list_entry_visible(matcher, path)? {
        return Ok(());
    }
    bail!(
        "{} is hidden by [filesystem].hide or outside mounted roots",
        path.display()
    )
}

fn list_entry_visible(matcher: &FilesystemMatcher, path: &Path) -> Result<bool> {
    let path = if path.exists() {
        std::fs::canonicalize(path)
            .with_context(|| format!("failed to resolve {}", path.display()))?
    } else {
        normalize_absolute(path)?
    };
    Ok(
        !matches!(matcher.classify_resolved(&path)?, FilesystemAccess::Hidden)
            || matcher.has_visible_descendant_root(&path),
    )
}

fn collect_list_entries(
    matcher: &FilesystemMatcher,
    dir: &Path,
    remaining_depth: usize,
    parent_depth: usize,
    out: &mut Vec<Value>,
) -> Result<()> {
    if remaining_depth == 0 {
        return Ok(());
    }

    let mut entries = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to list {}", dir.display()))?
    {
        let entry = entry?;
        let entry_path = entry.path();
        let allowed_path =
            std::fs::canonicalize(&entry_path).unwrap_or_else(|_| entry_path.clone());
        if !list_entry_visible(matcher, &allowed_path)? {
            continue;
        }
        let meta = entry
            .metadata()
            .with_context(|| format!("failed to stat {}", entry_path.display()))?;
        entries.push((entry.file_name(), entry_path, meta));
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path, meta) in entries {
        let depth = parent_depth + 1;
        let is_dir = meta.is_dir();
        out.push(json!({
            "name": name.to_string_lossy(),
            "path": path.display().to_string(),
            "kind": if is_dir { "directory" } else if meta.is_file() { "file" } else { "other" },
            "bytes": if meta.is_file() { Some(meta.len()) } else { None },
            "depth": depth,
        }));

        if is_dir && remaining_depth > 1 {
            collect_list_entries(matcher, &path, remaining_depth - 1, depth, out)?;
        }
    }

    Ok(())
}

fn required_string<'a>(arguments: Option<&'a Value>, key: &str) -> Result<&'a str> {
    arguments
        .and_then(Value::as_object)
        .and_then(|obj| obj.get(key))
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument `{key}`"))
}

fn optional_bool(arguments: Option<&Value>, key: &str) -> Option<bool> {
    arguments
        .and_then(Value::as_object)
        .and_then(|obj| obj.get(key))
        .and_then(Value::as_bool)
}

fn optional_usize(arguments: Option<&Value>, key: &str) -> Option<usize> {
    arguments
        .and_then(Value::as_object)
        .and_then(|obj| obj.get(key))
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

fn resolve_existing_path(raw: &str) -> Result<PathBuf> {
    let path = normalize_absolute(Path::new(raw))?;
    std::fs::canonicalize(&path).with_context(|| format!("failed to resolve {}", path.display()))
}

fn resolve_write_path(raw: &str) -> Result<PathBuf> {
    let path = normalize_absolute(Path::new(raw))?;
    if path.exists() {
        return std::fs::canonicalize(&path)
            .with_context(|| format!("failed to resolve {}", path.display()));
    }
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let parent = if parent.exists() {
        std::fs::canonicalize(parent)
            .with_context(|| format!("failed to resolve {}", parent.display()))?
    } else {
        normalize_absolute(parent)?
    };
    let file_name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    Ok(parent.join(file_name))
}

fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("host-fs paths must be absolute: {}", path.display());
    }
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            Component::Prefix(_) => bail!("unsupported path prefix in {}", path.display()),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemAccess {
    Hidden,
    Readonly,
    Readwrite,
}

pub struct FilesystemMatcher {
    roots: Vec<MountedRoot>,
    hide: Vec<Regex>,
    readonly: Vec<Regex>,
}

#[derive(Debug, Clone)]
struct MountedRoot {
    path: PathBuf,
    readonly: bool,
}

impl FilesystemMatcher {
    pub fn new(workspace: &Path, policy: &crate::settings::FilesystemPolicy) -> Result<Self> {
        let roots = mounted_roots(workspace, policy)?;
        let hide = compile_regexes(&policy.hide)?;
        let readonly = compile_regexes(&policy.readonly)?;
        Ok(Self {
            roots,
            hide,
            readonly,
        })
    }

    pub fn root_paths(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|root| root.path.as_path())
    }

    pub fn root_readonly(&self, path: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| root.path == path && root.readonly)
    }

    pub fn classify_existing_or_absolute(&self, path: &Path) -> Result<FilesystemAccess> {
        let path = if path.exists() {
            std::fs::canonicalize(path)
                .with_context(|| format!("failed to resolve {}", path.display()))?
        } else {
            normalize_absolute(path)?
        };
        self.classify_resolved(&path)
    }

    pub fn classify_resolved(&self, path: &Path) -> Result<FilesystemAccess> {
        self.classify_resolved_inner(path, true)
    }

    pub fn classify_resolved_for_shadow(&self, path: &Path) -> Result<FilesystemAccess> {
        self.classify_resolved_inner(path, false)
    }

    fn classify_resolved_inner(
        &self,
        path: &Path,
        include_mount_readonly: bool,
    ) -> Result<FilesystemAccess> {
        for root in &self.roots {
            if !path.starts_with(&root.path) {
                continue;
            }
            let rel = path
                .strip_prefix(&root.path)
                .unwrap_or(Path::new(""))
                .to_string_lossy()
                .replace('\\', "/");
            if matches_any_compiled_regex(&self.hide, &rel) {
                return Ok(FilesystemAccess::Hidden);
            }
            if (include_mount_readonly && root.readonly)
                || matches_any_compiled_regex(&self.readonly, &rel)
            {
                return Ok(FilesystemAccess::Readonly);
            }
            return Ok(FilesystemAccess::Readwrite);
        }
        Ok(FilesystemAccess::Hidden)
    }

    pub fn has_visible_descendant_root(&self, path: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| root.path != path && root.path.starts_with(path))
    }
}

fn mounted_roots(
    workspace: &Path,
    policy: &crate::settings::FilesystemPolicy,
) -> Result<Vec<MountedRoot>> {
    let mut roots = vec![MountedRoot {
        path: std::fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace {}", workspace.display()))?,
        readonly: false,
    }];
    for mount in &policy.mounts {
        if mount.path.trim().is_empty() {
            continue;
        }
        let path = normalize_absolute(Path::new(&mount.path))?;
        let path = std::fs::canonicalize(&path)
            .with_context(|| format!("failed to resolve filesystem mount {}", path.display()))?;
        if !roots.iter().any(|root| root.path == path) {
            roots.push(MountedRoot {
                path,
                readonly: mount.readonly,
            });
        }
    }
    Ok(roots)
}

pub fn classify_path(
    workspace: &Path,
    policy: &crate::settings::FilesystemPolicy,
    path: &Path,
) -> Result<FilesystemAccess> {
    FilesystemMatcher::new(workspace, policy)?.classify_existing_or_absolute(path)
}

fn compile_regexes(patterns: &[String]) -> Result<Vec<Regex>> {
    let mut regexes = Vec::new();
    for pattern in patterns {
        if pattern.trim().is_empty() {
            continue;
        }
        regexes.push(
            Regex::new(pattern)
                .with_context(|| format!("invalid filesystem filter regex `{pattern}`"))?,
        );
    }
    Ok(regexes)
}

fn matches_any_compiled_regex(regexes: &[Regex], value: &str) -> bool {
    let value = if value.is_empty() { "." } else { value };
    regexes.iter().any(|re| re.is_match(value))
}

#[derive(Debug, Clone, Copy, Default)]
struct RipgrepSearchOptions {
    hidden: bool,
    no_ignore: bool,
    no_ignore_vcs: bool,
}

fn search_with_ripgrep(
    matcher: &FilesystemMatcher,
    path: &Path,
    query: &str,
    max_results: usize,
    options: RipgrepSearchOptions,
) -> Result<Vec<Value>> {
    let mut cmd = Command::new("rg");
    cmd.args([
        "--json",
        "--fixed-strings",
        "--color",
        "never",
        "--max-filesize",
        RG_MAX_SEARCH_FILE_SIZE,
    ]);
    if options.hidden {
        cmd.arg("--hidden");
    }
    if options.no_ignore {
        cmd.arg("--no-ignore");
    }
    if options.no_ignore_vcs {
        cmd.arg("--no-ignore-vcs");
    }
    cmd.arg("--").arg(query).arg(path);
    cmd.stdin(Stdio::null());

    let output = cmd
        .output()
        .context("failed to run `rg`; install ripgrep on the host to use HostSearch")?;
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("rg failed with status {}: {}", output.status, stderr.trim());
    }

    let mut results = Vec::new();
    for line in output.stdout.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_slice(line).context("failed to parse rg JSON")?;
        if event.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        let Some(match_path) = data
            .get("path")
            .and_then(|p| p.get("text"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let match_path = PathBuf::from(match_path);
        let allowed_path = std::fs::canonicalize(&match_path).unwrap_or(match_path.clone());
        if matches!(
            matcher.classify_resolved(&allowed_path)?,
            FilesystemAccess::Hidden
        ) {
            continue;
        }
        let line_number = data.get("line_number").and_then(Value::as_u64).unwrap_or(0);
        let text = data
            .get("lines")
            .and_then(|p| p.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_end_matches(['\r', '\n']);
        results.push(json!({
            "path": match_path.display().to_string(),
            "line": line_number,
            "text": text,
        }));
        if results.len() >= max_results {
            break;
        }
    }

    Ok(results)
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn parse_error(message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": Value::Null, "error": { "code": -32700, "message": message } })
}

fn invalid_params(id: Value, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": message } })
}

fn method_not_found(id: Value, method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("method not found: {method}") } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_policy_classifies_workspace_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("work");
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        std::fs::write(workspace.join(".env"), "secret").unwrap();
        std::fs::write(workspace.join("README.md"), "ok").unwrap();
        let policy = crate::settings::FilesystemPolicy {
            mounts: Vec::new(),
            hide: crate::settings::default_filesystem_hide(),
            readonly: crate::settings::default_filesystem_readonly(),
        };

        assert_eq!(
            classify_path(&workspace, &policy, &workspace.join(".env")).unwrap(),
            FilesystemAccess::Hidden
        );
        assert_eq!(
            classify_path(&workspace, &policy, &workspace.join(".claude")).unwrap(),
            FilesystemAccess::Readonly
        );
        assert_eq!(
            classify_path(&workspace, &policy, &workspace.join("README.md")).unwrap(),
            FilesystemAccess::Readwrite
        );
        assert_eq!(
            classify_path(&workspace, &policy, dir.path()).unwrap(),
            FilesystemAccess::Hidden
        );
    }

    #[test]
    fn host_list_can_walk_to_visible_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("foo");
        let workspace = parent.join("work");
        let sibling = dir.path().join("bar");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let policy = crate::settings::FilesystemPolicy::default();
        let matcher = FilesystemMatcher::new(&workspace, &policy).unwrap();

        assert!(list_entry_visible(&matcher, dir.path()).unwrap());
        assert!(list_entry_visible(&matcher, &parent).unwrap());
        assert!(list_entry_visible(&matcher, &workspace).unwrap());
        assert!(!list_entry_visible(&matcher, &sibling).unwrap());
        ensure_listable_by(&matcher, dir.path()).unwrap();
        ensure_listable_by(&matcher, &parent).unwrap();
    }

    #[test]
    fn host_list_depth_exposes_visible_ancestor_one_level_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let root = dir.path().join("a");
        let mounted = root.join("b/c");
        let hidden_sibling = root.join("x");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&mounted).unwrap();
        std::fs::create_dir_all(&hidden_sibling).unwrap();

        let policy = crate::settings::FilesystemPolicy {
            mounts: vec![crate::settings::FilesystemMount::new(
                mounted.display().to_string(),
                false,
            )],
            hide: Vec::new(),
            readonly: Vec::new(),
        };
        let matcher = FilesystemMatcher::new(&workspace, &policy).unwrap();

        let mut depth_one = Vec::new();
        collect_list_entries(&matcher, &root, 1, 0, &mut depth_one).unwrap();
        assert_eq!(entry_names(&depth_one), vec!["b"]);
        assert_eq!(entry_depths(&depth_one), vec![1]);

        let mut depth_two = Vec::new();
        collect_list_entries(&matcher, &root, 2, 0, &mut depth_two).unwrap();
        assert_eq!(entry_names(&depth_two), vec!["b", "c"]);
        assert_eq!(entry_depths(&depth_two), vec![1, 2]);
    }

    #[test]
    fn readonly_mount_classifies_descendants_readonly() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let mounted = dir.path().join("notes");
        let file = mounted.join("note.md");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&mounted).unwrap();
        std::fs::write(&file, "note").unwrap();

        let policy = crate::settings::FilesystemPolicy {
            mounts: vec![crate::settings::FilesystemMount::new(
                mounted.display().to_string(),
                true,
            )],
            hide: Vec::new(),
            readonly: Vec::new(),
        };

        assert_eq!(
            classify_path(&workspace, &policy, &file).unwrap(),
            FilesystemAccess::Readonly
        );
    }

    #[test]
    fn host_search_uses_ripgrep_hidden_option() {
        if std::process::Command::new("rg")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let hidden_dir = workspace.join(".hidden");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        std::fs::write(workspace.join("visible.txt"), "needle\n").unwrap();
        std::fs::write(hidden_dir.join("secret.txt"), "needle\n").unwrap();

        let policy = crate::settings::FilesystemPolicy {
            mounts: Vec::new(),
            hide: Vec::new(),
            readonly: Vec::new(),
        };
        let matcher = FilesystemMatcher::new(&workspace, &policy).unwrap();

        let default_results = search_with_ripgrep(
            &matcher,
            &workspace,
            "needle",
            10,
            RipgrepSearchOptions::default(),
        )
        .unwrap();
        assert_eq!(entry_paths(&default_results).len(), 1);
        assert!(
            entry_paths(&default_results)[0].ends_with("visible.txt"),
            "{default_results:?}"
        );

        let hidden_results = search_with_ripgrep(
            &matcher,
            &workspace,
            "needle",
            10,
            RipgrepSearchOptions {
                hidden: true,
                ..RipgrepSearchOptions::default()
            },
        )
        .unwrap();
        assert_eq!(entry_paths(&hidden_results).len(), 2);
        assert!(
            entry_paths(&hidden_results)
                .iter()
                .any(|path| path.ends_with(".hidden/secret.txt")),
            "{hidden_results:?}"
        );
    }

    fn entry_names(entries: &[Value]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect()
    }

    fn entry_depths(entries: &[Value]) -> Vec<usize> {
        entries
            .iter()
            .map(|entry| entry["depth"].as_u64().unwrap() as usize)
            .collect()
    }

    fn entry_paths(entries: &[Value]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect()
    }
}
