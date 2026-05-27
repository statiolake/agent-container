//! Built-in MCP server for controlled host filesystem access.
//!
//! This is intentionally host-side: the container does not gain extra
//! mounts. Every tool call reloads the merged agent-container settings
//! and checks the current `[host_fs].allow` list before touching the
//! requested path.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
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
                        "description": "Read a UTF-8 text file from the host filesystem when its absolute path is allowed by the current [host_fs].allow settings.",
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
                        "description": "List one host directory when its absolute path is allowed by the current [host_fs].allow settings.",
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
                        "description": "Write UTF-8 text to a host file when its absolute path is allowed by the current [host_fs].allow settings.",
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
        self.ensure_allowed(&path)?;
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
        let patterns = self.latest_patterns()?;
        ensure_allowed_by(&path, &patterns)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path)
            .with_context(|| format!("failed to list {}", path.display()))?
        {
            let entry = entry?;
            let entry_path = entry.path();
            let allowed_path =
                std::fs::canonicalize(&entry_path).unwrap_or_else(|_| entry_path.clone());
            if !path_allowed(&allowed_path, &patterns) {
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
        self.ensure_allowed(&path)?;
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
        let patterns = self.latest_patterns()?;
        ensure_allowed_by(&path, &patterns)?;
        let mut results = Vec::new();
        search_recursive(&path, query, max_results, &patterns, &mut results)?;
        serde_json::to_string_pretty(&results).context("failed to encode search results")
    }

    fn ensure_allowed(&self, path: &Path) -> Result<()> {
        let patterns = self.latest_patterns()?;
        ensure_allowed_by(path, &patterns)
    }

    fn latest_patterns(&self) -> Result<Vec<String>> {
        let settings = crate::settings::Settings::load_merged(&self.workspace)
            .context("failed to load latest host-fs allowlist")?;
        Ok(settings.host_fs.allow)
    }
}

fn ensure_allowed_by(path: &Path, patterns: &[String]) -> Result<()> {
    if path_allowed(path, patterns) {
        Ok(())
    } else {
        bail!(
            "{} is not allowed by [host_fs].allow; add an absolute glob such as \"{}\"",
            path.display(),
            path.display()
        )
    }
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

fn path_allowed(path: &Path, patterns: &[String]) -> bool {
    let path = path_to_match_string(path);
    let mut allowed = false;
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        let (deny, body) = pattern
            .strip_prefix('!')
            .map(|p| (true, p))
            .unwrap_or((false, pattern));
        if glob_match_path(body, &path) {
            allowed = !deny;
        }
    }
    allowed
}

fn path_to_match_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn glob_match_path(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_end_matches('/');
    if let Some(base) = pattern.strip_suffix("/**") {
        if path == base {
            return true;
        }
    }
    glob_match(pattern, path)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn rec(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' if p.get(1) == Some(&'*') => rec(&p[2..], t) || (!t.is_empty() && rec(p, &t[1..])),
            '*' => rec(&p[1..], t) || (!t.is_empty() && t[0] != '/' && rec(p, &t[1..])),
            '?' => !t.is_empty() && t[0] != '/' && rec(&p[1..], &t[1..]),
            c => !t.is_empty() && c == t[0] && rec(&p[1..], &t[1..]),
        }
    }
    rec(
        &pattern.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
    )
}

fn search_recursive(
    path: &Path,
    query: &str,
    max_results: usize,
    patterns: &[String],
    results: &mut Vec<Value>,
) -> Result<()> {
    if results.len() >= max_results {
        return Ok(());
    }
    let allowed_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !path_allowed(&allowed_path, patterns) {
        return Ok(());
    }
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            search_recursive(&entry.path(), query, max_results, patterns, results)?;
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
    fn allowlist_defaults_to_denied_and_later_patterns_win() {
        let rules = vec![
            "/tmp/project/**".to_string(),
            "!/tmp/project/secrets/**".to_string(),
        ];
        assert!(path_allowed(Path::new("/tmp/project/README.md"), &rules));
        assert!(path_allowed(Path::new("/tmp/project"), &rules));
        assert!(!path_allowed(Path::new("/tmp/project/secrets/key"), &rules));
        assert!(!path_allowed(Path::new("/tmp/other"), &rules));
    }

    #[test]
    fn glob_star_does_not_cross_path_separator() {
        assert!(glob_match("/tmp/*", "/tmp/a"));
        assert!(!glob_match("/tmp/*", "/tmp/a/b"));
        assert!(glob_match("/tmp/**", "/tmp/a/b"));
    }
}
