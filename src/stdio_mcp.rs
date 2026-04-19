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

/// Handle shared by every HTTP request targeting a given stdio MCP server.
#[derive(Clone)]
pub struct StdioHandle {
    writer: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    server_broadcast: broadcast::Sender<Value>,
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
        let parsed: Value = serde_json::from_slice(&body).context("request is not valid JSON")?;
        let compact = serde_json::to_vec(&parsed).context("re-serialising request")?;

        let has_method = parsed.get("method").is_some();
        let id = parsed.get("id").cloned();

        let response = if has_method {
            // Client → server request: register a waiter keyed by id.
            if let Some(id_value) = id {
                let key = id_key(&id_value);
                let (tx, rx) = oneshot::channel();
                self.pending.lock().await.insert(key, tx);
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
pub fn spawn_worker(spec: StdioMcpServer) -> Result<StdioHandle> {
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

async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    server_broadcast: broadcast::Sender<Value>,
    server_name: String,
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
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(mcp = %server_name, line = %line, error = %e, "non-JSON line on stdout");
                continue;
            }
        };

        if value.get("method").is_some() {
            // Server-initiated request or notification → broadcast.
            tracing::debug!(
                mcp = %server_name,
                message = %line,
                "← stdout (server-initiated)",
            );
            let _ = server_broadcast.send(value);
            continue;
        }

        if let Some(id) = value.get("id") {
            tracing::debug!(
                mcp = %server_name,
                response = %line,
                "← stdout (response)",
            );
            let key = id_key(id);
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
