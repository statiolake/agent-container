//! Bridge stdio-transport MCP servers over MCP Streamable-HTTP.
//!
//! A single per-server subprocess runs for the lifetime of the broker.
//! Three background tasks manage it:
//!
//! - **writer**: drains a shared mpsc and writes each payload to the
//!   child's stdin (LF-framed, re-serialised compact so embedded newlines
//!   in pretty-printed bodies never desynchronise the stream).
//! - **reader**: continuously reads stdout lines, classifies each line
//!   as either a *response* (carries `id`, no `method`) or a
//!   *server-initiated message* (carries `method`). Responses are routed
//!   to the waiting oneshot by id; server-initiated messages are
//!   broadcast to every open `GET` subscriber.
//! - **stderr drain**: pipes stderr into tracing so the child can't
//!   deadlock on an unread error pipe.
//!
//! From HTTP side, `POST /mcp/<name>` uses `submit_post` which buffers a
//! client request, writes it to stdin, and hands back either the matching
//! response (for requests) or nothing (for notifications / responses to
//! a server-initiated request). `GET /mcp/<name>` uses `subscribe` which
//! returns a broadcast receiver that yields every server-initiated
//! JSON-RPC message in order; the broker wraps that receiver into an
//! SSE response.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::mcp::StdioMcpServer;

/// Translate between the container's view of the workspace path and the
/// host filesystem path where the stdio MCP server actually runs. Every
/// string value in client→server messages has `container_root` rewritten
/// to `host_root`; every string value in server→client messages gets the
/// reverse treatment. This keeps `roots/list`, screenshot paths, trace
/// directories, etc. consistent on both sides of the bridge.
#[derive(Clone, Debug)]
pub struct PathBridge {
    pub container_root: String,
    pub host_root: String,
}

/// Handle shared by every HTTP request targeting a given stdio MCP server.
#[derive(Clone)]
pub struct StdioHandle {
    writer: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    server_broadcast: broadcast::Sender<Value>,
    bridge: Option<PathBridge>,
    /// Method for each outbound client request awaiting a response.
    /// Needed so that when the server responds, we know which spec-defined
    /// URI fields to translate.
    client_req_methods: Arc<Mutex<HashMap<String, String>>>,
    /// Method for each inbound server request awaiting a client response.
    /// Needed so that when the client POSTs its response, we know which
    /// URI fields in the result to translate.
    server_req_methods: Arc<Mutex<HashMap<String, String>>>,
}

pub struct PostOutcome {
    /// `None` means the caller's body was a notification or a reply to a
    /// server-initiated request; the HTTP side should answer 202 Accepted.
    /// `Some(rx)` will resolve with the JSON-RPC response whose id matches
    /// the caller's request.
    pub response: Option<oneshot::Receiver<Value>>,
}

impl StdioHandle {
    /// Submit a client-originated JSON-RPC payload. Returns a handle that
    /// the HTTP side awaits for the response, if one is expected.
    pub async fn submit_post(&self, body: Vec<u8>) -> Result<PostOutcome> {
        let mut parsed: Value =
            serde_json::from_slice(&body).context("request is not valid JSON")?;

        let method = parsed
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = parsed.get("id").cloned();

        // Translate container→host paths in the outgoing payload. For
        // client-originated requests we dispatch on the body's own method;
        // for client responses (no method, only id) we look up the
        // matching server-originated request so we know which fields
        // to translate in the result.
        if let Some(bridge) = &self.bridge {
            if let Some(m) = &method {
                rewrite_known_uris(&mut parsed, m, Direction::ClientToServer, bridge);
            } else if let Some(id_value) = &id {
                let key = id_key(id_value);
                let server_method = self.server_req_methods.lock().await.remove(&key);
                if let Some(server_method) = server_method {
                    rewrite_known_uris(
                        &mut parsed,
                        &server_method,
                        Direction::ClientToServer,
                        bridge,
                    );
                }
            }
        }

        let compact = serde_json::to_vec(&parsed).context("re-serialising request")?;

        let has_method = method.is_some();

        let response = if has_method {
            // Client → server request: register a waiter keyed by id.
            if let Some(id_value) = id {
                let key = id_key(&id_value);
                let (tx, rx) = oneshot::channel();
                self.pending.lock().await.insert(key.clone(), tx);
                // Remember the method so reader_loop can translate paths
                // in the response we're about to wait for.
                if let Some(m) = method {
                    self.client_req_methods.lock().await.insert(key, m);
                }
                Some(rx)
            } else {
                // Client → server notification (no id): nothing to wait on.
                None
            }
        } else {
            // Client → server response (has id but no method): the stdio
            // server is consuming it; we have nothing to return.
            None
        };

        tracing::debug!(
            request = %String::from_utf8_lossy(&compact),
            "→ stdin (client POST)",
        );
        self.writer
            .send(compact)
            .map_err(|_| anyhow::anyhow!("stdio writer has stopped"))?;

        Ok(PostOutcome { response })
    }

    /// Subscribe to server-initiated messages. Used to back a `GET`
    /// SSE stream: every `Ok(Value)` yielded should be wrapped as
    /// `data: <json>\n\n` on the wire.
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.server_broadcast.subscribe()
    }
}

/// Spawn the subprocess and its attendant reader/writer tasks. Failures
/// here surface immediately so misconfigured servers don't cause late
/// mysterious hangs.
pub fn spawn_worker(
    spec: StdioMcpServer,
    bridge: Option<PathBridge>,
) -> Result<StdioHandle> {
    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args)
        .envs(spec.env.iter())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn stdio MCP server '{}'", spec.name))?;

    let stdin = child.stdin.take().context("child has no stdin")?;
    let stdout = child.stdout.take().context("child has no stdout")?;
    let stderr = child.stderr.take();

    // Shared state.
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let client_req_methods: Arc<Mutex<HashMap<String, String>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let server_req_methods: Arc<Mutex<HashMap<String, String>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (server_broadcast, _) = broadcast::channel::<Value>(128);
    let (writer_tx, writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Keep a handle to the child so we can reap it on shutdown.
    let child = Arc::new(Mutex::new(Some(child)));

    // Writer task.
    let writer_child = child.clone();
    tokio::spawn(writer_loop(
        stdin,
        writer_rx,
        spec.name.clone(),
        writer_child,
    ));

    // Reader task.
    tokio::spawn(reader_loop(
        stdout,
        pending.clone(),
        server_broadcast.clone(),
        spec.name.clone(),
        bridge.clone(),
        client_req_methods.clone(),
        server_req_methods.clone(),
    ));

    // Stderr drain.
    if let Some(stderr) = stderr {
        let server_name = spec.name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(mcp = %server_name, stderr = %line);
            }
        });
    }

    Ok(StdioHandle {
        writer: writer_tx,
        pending,
        server_broadcast,
        bridge,
        client_req_methods,
        server_req_methods,
    })
}

async fn writer_loop(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    server_name: String,
    child: Arc<Mutex<Option<tokio::process::Child>>>,
) {
    while let Some(bytes) = rx.recv().await {
        if let Err(e) = stdin.write_all(&bytes).await {
            tracing::error!(mcp = %server_name, error = %e, "stdin write failed");
            break;
        }
        if let Err(e) = stdin.write_all(b"\n").await {
            tracing::error!(mcp = %server_name, error = %e, "stdin write failed");
            break;
        }
        if let Err(e) = stdin.flush().await {
            tracing::debug!(mcp = %server_name, error = %e, "stdin flush failed");
        }
    }
    // Reap the child when the writer drains (which means the handle was
    // dropped everywhere).
    if let Some(mut c) = child.lock().await.take() {
        let _ = c.kill().await;
        let _ = c.wait().await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    server_broadcast: broadcast::Sender<Value>,
    server_name: String,
    bridge: Option<PathBridge>,
    client_req_methods: Arc<Mutex<HashMap<String, String>>>,
    server_req_methods: Arc<Mutex<HashMap<String, String>>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                tracing::warn!(mcp = %server_name, "stdio MCP closed its stdout");
                return;
            }
            Err(e) => {
                tracing::error!(mcp = %server_name, error = %e, "reading stdio stdout failed");
                return;
            }
        };
        if line.is_empty() {
            continue;
        }
        let mut value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(mcp = %server_name, line = %line, error = %e, "non-JSON line on stdout");
                continue;
            }
        };

        let method = value.get("method").and_then(Value::as_str).map(str::to_string);
        let id = value.get("id").cloned();

        // Apply targeted host→container URI rewriting based on the
        // message's role and method.
        if let Some(bridge) = &bridge {
            if let Some(m) = &method {
                rewrite_known_uris(&mut value, m, Direction::ServerToClient, bridge);
            } else if let Some(id_value) = &id {
                let key = id_key(id_value);
                let client_method = client_req_methods.lock().await.remove(&key);
                if let Some(client_method) = client_method {
                    rewrite_known_uris(
                        &mut value,
                        &client_method,
                        Direction::ServerToClient,
                        bridge,
                    );
                }
            }
        }

        if let Some(m) = method {
            // Server-initiated request or notification. Requests carry an
            // id so we can correlate the client's later response back to
            // the server's method for path translation.
            if let Some(id_value) = &id {
                server_req_methods
                    .lock()
                    .await
                    .insert(id_key(id_value), m.clone());
            }
            tracing::debug!(
                mcp = %server_name,
                message = %line,
                translated = %serde_json::to_string(&value).unwrap_or_default(),
                "← stdout (server-initiated)",
            );
            let _ = server_broadcast.send(value);
            continue;
        }

        if let Some(id) = id {
            tracing::debug!(
                mcp = %server_name,
                response = %line,
                translated = %serde_json::to_string(&value).unwrap_or_default(),
                "← stdout (response)",
            );
            let key = id_key(&id);
            let mut guard = pending.lock().await;
            if let Some(tx) = guard.remove(&key) {
                let _ = tx.send(value);
            } else {
                tracing::warn!(
                    mcp = %server_name,
                    id = %key,
                    "response for unknown id — dropping",
                );
            }
        } else {
            tracing::warn!(mcp = %server_name, line = %line, "stdout line with neither method nor id");
        }
    }
}

fn id_key(v: &Value) -> String {
    // JSON-RPC ids are either numbers or strings; the canonical JSON
    // repr is unambiguous for either case and works as a HashMap key.
    serde_json::to_string(v).unwrap_or_default()
}

#[derive(Clone, Copy)]
enum Direction {
    ClientToServer,
    ServerToClient,
}

/// Apply MCP-spec-aware URI translation. Only touches the specific fields
/// that the specification says carry `uri` / `uriTemplate`, never a blind
/// substring replace over the whole message.
fn rewrite_known_uris(v: &mut Value, method: &str, dir: Direction, bridge: &PathBridge) {
    // Figure out the transform: outgoing client→server maps the
    // container root onto the host root; the reverse direction undoes it.
    let container_root = bridge.container_root.clone();
    let host_root = bridge.host_root.clone();
    let map_owned: Box<dyn Fn(&str) -> Option<String>> = match dir {
        Direction::ClientToServer => Box::new(move |s: &str| rewrite_root(s, &container_root, &host_root)),
        Direction::ServerToClient => Box::new(move |s: &str| rewrite_root(s, &host_root, &container_root)),
    };

    match (method, dir) {
        // Client replies to `roots/list` carry an array of {uri,name} in
        // `result.roots`; server → client direction never produces these.
        ("roots/list", Direction::ClientToServer) => {
            map_each_uri(v.pointer_mut("/result/roots"), "uri", &map_owned);
        }
        // Resource URIs travel in both directions.
        ("resources/read", Direction::ClientToServer)
        | ("resources/subscribe", Direction::ClientToServer)
        | ("resources/unsubscribe", Direction::ClientToServer)
        | ("notifications/resources/updated", Direction::ClientToServer) => {
            map_single_uri(v.pointer_mut("/params/uri"), &map_owned);
        }
        ("resources/list", Direction::ServerToClient) => {
            map_each_uri(v.pointer_mut("/result/resources"), "uri", &map_owned);
        }
        ("resources/templates/list", Direction::ServerToClient) => {
            map_each_uri(
                v.pointer_mut("/result/resourceTemplates"),
                "uriTemplate",
                &map_owned,
            );
        }
        ("resources/read", Direction::ServerToClient) => {
            map_each_uri(v.pointer_mut("/result/contents"), "uri", &map_owned);
        }
        ("notifications/resources/updated", Direction::ServerToClient) => {
            map_single_uri(v.pointer_mut("/params/uri"), &map_owned);
        }
        _ => {}
    }
}

fn map_single_uri(slot: Option<&mut Value>, map: &dyn Fn(&str) -> Option<String>) {
    if let Some(Value::String(s)) = slot {
        if let Some(replaced) = map(s) {
            *s = replaced;
        }
    }
}

fn map_each_uri(array: Option<&mut Value>, field: &str, map: &dyn Fn(&str) -> Option<String>) {
    let Some(Value::Array(items)) = array else {
        return;
    };
    for item in items.iter_mut() {
        if let Value::Object(obj) = item {
            if let Some(Value::String(s)) = obj.get_mut(field) {
                if let Some(replaced) = map(s) {
                    *s = replaced;
                }
            }
        }
    }
}

/// Replace the filesystem root in a path or `file://` URI. Returns `None`
/// when the string does not start with the expected root (so unrelated
/// strings pass through unchanged).
fn rewrite_root(s: &str, from: &str, to: &str) -> Option<String> {
    if from.is_empty() {
        return None;
    }
    // Bare path form.
    if s == from {
        return Some(to.to_string());
    }
    if let Some(rest) = s.strip_prefix(from) {
        if rest.starts_with('/') {
            return Some(format!("{to}{rest}"));
        }
    }
    // file:// URI form.
    let file_prefix = format!("file://{from}");
    if s == file_prefix {
        return Some(format!("file://{to}"));
    }
    if let Some(rest) = s.strip_prefix(&file_prefix) {
        if rest.starts_with('/') {
            return Some(format!("file://{to}{rest}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bridge() -> PathBridge {
        PathBridge {
            container_root: "/workspace".to_string(),
            host_root: "/Users/test/project".to_string(),
        }
    }

    #[test]
    fn rewrite_root_handles_exact_file_and_subpath_forms() {
        assert_eq!(
            rewrite_root("/workspace", "/workspace", "/host"),
            Some("/host".to_string())
        );
        assert_eq!(
            rewrite_root("/workspace/foo", "/workspace", "/host"),
            Some("/host/foo".to_string())
        );
        assert_eq!(
            rewrite_root("file:///workspace", "/workspace", "/host"),
            Some("file:///host".to_string())
        );
        assert_eq!(
            rewrite_root("file:///workspace/a/b", "/workspace", "/host"),
            Some("file:///host/a/b".to_string())
        );
        // Boundary: /workspaces is NOT under /workspace.
        assert_eq!(rewrite_root("/workspaces/x", "/workspace", "/host"), None);
    }

    #[test]
    fn rewrites_roots_list_response_uri_client_to_server() {
        let b = bridge();
        let mut v = json!({
            "id": 0,
            "jsonrpc": "2.0",
            "result": {
                "roots": [
                    {"uri": "file:///workspace", "name": "workspace"},
                    {"uri": "file:///workspace/nested", "name": "nested"}
                ]
            }
        });
        rewrite_known_uris(&mut v, "roots/list", Direction::ClientToServer, &b);
        let roots = v["result"]["roots"].as_array().unwrap();
        assert_eq!(
            roots[0]["uri"].as_str(),
            Some("file:///Users/test/project")
        );
        assert_eq!(
            roots[1]["uri"].as_str(),
            Some("file:///Users/test/project/nested")
        );
    }

    #[test]
    fn rewrites_resource_read_request_uri_client_to_server() {
        let b = bridge();
        let mut v = json!({
            "id": 1,
            "jsonrpc": "2.0",
            "method": "resources/read",
            "params": {"uri": "file:///workspace/readme.md"}
        });
        rewrite_known_uris(&mut v, "resources/read", Direction::ClientToServer, &b);
        assert_eq!(
            v["params"]["uri"].as_str(),
            Some("file:///Users/test/project/readme.md")
        );
    }

    #[test]
    fn rewrites_resource_list_response_server_to_client() {
        let b = bridge();
        let mut v = json!({
            "id": 1,
            "jsonrpc": "2.0",
            "result": {
                "resources": [
                    {"uri": "file:///Users/test/project/a.md", "name": "a"},
                    {"uri": "https://example.com/x", "name": "external"}
                ]
            }
        });
        rewrite_known_uris(&mut v, "resources/list", Direction::ServerToClient, &b);
        let items = v["result"]["resources"].as_array().unwrap();
        assert_eq!(items[0]["uri"].as_str(), Some("file:///workspace/a.md"));
        // External URIs outside the bridge root are left untouched.
        assert_eq!(items[1]["uri"].as_str(), Some("https://example.com/x"));
    }

    #[test]
    fn unknown_methods_are_a_no_op() {
        let b = bridge();
        let mut v = json!({
            "method": "tools/call",
            "params": {"name": "foo", "arguments": {"path": "/workspace/x"}}
        });
        let before = v.clone();
        rewrite_known_uris(&mut v, "tools/call", Direction::ClientToServer, &b);
        assert_eq!(v, before);
    }
}
