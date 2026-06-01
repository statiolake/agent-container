//! Built-in MCP server for controlled host filesystem access.
//!
//! This is intentionally mediated on the host side. Every tool call
//! reloads the merged agent-container settings and checks the current
//! `[filesystem]` policy before touching the requested path.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Value, json};

pub const NAME: &str = "host-fs";

const PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_READ_BYTES: u64 = 1024 * 1024;
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;
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
        let Some(id) = id else {
            return None;
        };

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
                                "path": { "type": "string", "description": "Absolute host path to a directory." }
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
                        "description": "Search UTF-8 text files under an allowed host directory. Results are capped.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Absolute host directory to search." },
                                "query": { "type": "string", "description": "Literal text to find." },
                                "maxResults": { "type": "integer", "minimum": 1, "maximum": 1000 }
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
        let path = resolve_existing_path(path)?;
        let policy = self.latest_policy()?;
        let matcher = FilesystemMatcher::new(&self.workspace, &policy)?;
        ensure_listable_by(&matcher, &path)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path)
            .with_context(|| format!("failed to list {}", path.display()))?
        {
            let entry = entry?;
            let entry_path = entry.path();
            let allowed_path =
                std::fs::canonicalize(&entry_path).unwrap_or_else(|_| entry_path.clone());
            if !list_entry_visible(&matcher, &allowed_path)? {
                continue;
            }
            let meta = entry.metadata()?;
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "path": entry_path.display().to_string(),
                "kind": if meta.is_dir() { "directory" } else if meta.is_file() { "file" } else { "other" },
                "bytes": if meta.is_file() { Some(meta.len()) } else { None },
            }));
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        serde_json::to_string_pretty(&entries).context("failed to encode directory listing")
    }

    fn host_write(&self, arguments: Option<&Value>) -> Result<String> {
        let path = required_string(arguments, "path")?;
        let content = required_string(arguments, "content")?;
        let create_parents = optional_bool(arguments, "createParents").unwrap_or(false);
        let path = resolve_write_path(path)?;
        self.ensure_writable(&path)?;
        if create_parents {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
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
        let path = resolve_existing_path(path)?;
        let policy = self.latest_policy()?;
        ensure_readable_by(&self.workspace, &path, &policy)?;
        let mut results = Vec::new();
        search_recursive(
            &self.workspace,
            &path,
            query,
            max_results,
            &policy,
            &mut results,
        )?;
        serde_json::to_string_pretty(&results).context("failed to encode search results")
    }

    fn ensure_readable(&self, path: &Path) -> Result<()> {
        let policy = self.latest_policy()?;
        ensure_readable_by(&self.workspace, path, &policy)
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

fn ensure_readable_by(
    workspace: &Path,
    path: &Path,
    policy: &crate::settings::FilesystemPolicy,
) -> Result<()> {
    match classify_path(workspace, policy, path)? {
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
    Ok(!matches!(
        matcher.classify_resolved(path)?,
        FilesystemAccess::Hidden
    ) || matcher.has_visible_descendant_root(path))
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
    roots: Vec<PathBuf>,
    hide: Vec<Regex>,
    readonly: Vec<Regex>,
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

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
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
        for root in &self.roots {
            if !path.starts_with(root) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(Path::new(""))
                .to_string_lossy()
                .replace('\\', "/");
            if matches_any_compiled_regex(&self.hide, &rel) {
                return Ok(FilesystemAccess::Hidden);
            }
            if matches_any_compiled_regex(&self.readonly, &rel) {
                return Ok(FilesystemAccess::Readonly);
            }
            return Ok(FilesystemAccess::Readwrite);
        }
        Ok(FilesystemAccess::Hidden)
    }

    pub fn has_visible_descendant_root(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| root != path && root.starts_with(path))
    }
}

pub fn mounted_roots(
    workspace: &Path,
    policy: &crate::settings::FilesystemPolicy,
) -> Result<Vec<PathBuf>> {
    let mut roots = vec![
        std::fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace {}", workspace.display()))?,
    ];
    for mount in &policy.mounts {
        if mount.trim().is_empty() {
            continue;
        }
        let path = normalize_absolute(Path::new(mount))?;
        let path = std::fs::canonicalize(&path)
            .with_context(|| format!("failed to resolve filesystem mount {}", path.display()))?;
        if !roots.contains(&path) {
            roots.push(path);
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

fn search_recursive(
    workspace: &Path,
    path: &Path,
    query: &str,
    max_results: usize,
    policy: &crate::settings::FilesystemPolicy,
    results: &mut Vec<Value>,
) -> Result<()> {
    if results.len() >= max_results {
        return Ok(());
    }
    let allowed_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if matches!(
        classify_path(workspace, policy, &allowed_path)?,
        FilesystemAccess::Hidden
    ) {
        return Ok(());
    }
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            search_recursive(
                workspace,
                &entry.path(),
                query,
                max_results,
                policy,
                results,
            )?;
            if results.len() >= max_results {
                break;
            }
        }
        return Ok(());
    }
    if !meta.is_file() || meta.len() > MAX_SEARCH_FILE_BYTES {
        return Ok(());
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    for (idx, line) in text.lines().enumerate() {
        if line.contains(query) {
            results.push(json!({
                "path": path.display().to_string(),
                "line": idx + 1,
                "text": line,
            }));
            if results.len() >= max_results {
                break;
            }
        }
    }
    Ok(())
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
}
