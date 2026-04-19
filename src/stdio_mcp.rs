//! Bridge stdio-transport MCP servers over HTTP so the container can
//! address them the same way it addresses HTTP MCP servers.
//!
//! For each stdio server declared on the host, we spawn the subprocess
//! once and keep it alive for the lifetime of the broker. An mpsc channel
//! queues one request at a time; the worker writes the JSON-RPC payload
//! to the child's stdin (LF-framed per the MCP spec) and reads the next
//! response line from stdout. Server-initiated notifications (messages
//! without an `id`) are logged and skipped so they don't desynchronise
//! the request/response pairing — routing them back to the HTTP client
//! would require a streaming response and is out of scope for this first
//! pass.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::mcp::StdioMcpServer;

/// Thin handle posted into the broker's router. Clones share one worker.
#[derive(Clone)]
pub struct StdioHandle {
    tx: mpsc::Sender<Call>,
}

impl StdioHandle {
    /// Send a JSON-RPC request (or notification) to the stdio server and
    /// await the matching response line. Notifications resolve with an
    /// empty body — HTTP 204-equivalent.
    pub async fn call(&self, body: Vec<u8>) -> Result<Vec<u8>> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx
            .send(Call {
                body,
                response: resp_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("stdio MCP worker has stopped"))?;
        resp_rx
            .await
            .map_err(|_| anyhow::anyhow!("stdio MCP worker dropped response channel"))?
    }
}

struct Call {
    body: Vec<u8>,
    response: oneshot::Sender<Result<Vec<u8>>>,
}

/// Spawn a worker task for a stdio server. The child process starts
/// eagerly here so any misconfiguration (missing binary, wrong args)
/// surfaces at broker startup rather than on the first request.
pub fn spawn_worker(spec: StdioMcpServer) -> Result<StdioHandle> {
    let (tx, rx) = mpsc::channel::<Call>(16);

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

    let stdin = child
        .stdin
        .take()
        .context("child has no stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("child has no stdout")?;
    let stderr = child.stderr.take();

    // Drain stderr into tracing so a misbehaving server surfaces warnings
    // instead of filling up an unread pipe and deadlocking the child.
    if let Some(stderr) = stderr {
        let server_name = spec.name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(mcp = %server_name, stderr = %line);
            }
        });
    }

    let name = spec.name.clone();
    tokio::spawn(async move {
        let stdin = Arc::new(Mutex::new(stdin));
        let mut stdout_lines = BufReader::new(stdout).lines();
        if let Err(e) = run(rx, stdin, &mut stdout_lines, &name).await {
            tracing::error!(mcp = %name, error = %e, "stdio MCP worker terminating");
        }
        // Reaping the child guarantees we don't leak zombies.
        let _ = child.kill().await;
        let _ = child.wait().await;
    });

    Ok(StdioHandle { tx })
}

async fn run(
    mut rx: mpsc::Receiver<Call>,
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    stdout_lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    server_name: &str,
) -> Result<()> {
    while let Some(call) = rx.recv().await {
        let outcome = handle_one(&call.body, &stdin, stdout_lines, server_name).await;
        let _ = call.response.send(outcome);
    }
    Ok(())
}

async fn handle_one(
    body: &[u8],
    stdin: &Arc<Mutex<tokio::process::ChildStdin>>,
    stdout_lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    server_name: &str,
) -> Result<Vec<u8>> {
    // Flatten the body to a single line — MCP stdio framing is LF-delimited
    // JSON, so any embedded newline in a pretty-printed request would
    // desynchronise the server. serde_json re-serialises compact on round-
    // trip, which is the standard way to normalise.
    let parsed: Value = serde_json::from_slice(body).context("request is not valid JSON")?;
    let compact = serde_json::to_vec(&parsed).context("re-serialising request")?;
    tracing::debug!(
        mcp = %server_name,
        request = %String::from_utf8_lossy(&compact),
        "→ stdin",
    );
    write_line(stdin, &compact).await?;

    // Notifications (no `id`) get no response — tell the caller immediately
    // so the HTTP handler can return 204-equivalent.
    if parsed.get("id").is_none() {
        return Ok(Vec::new());
    }

    loop {
        let Some(line) = stdout_lines
            .next_line()
            .await
            .context("reading MCP stdout line")?
        else {
            bail!("stdio MCP '{server_name}' closed its stdout");
        };
        if line.is_empty() {
            continue;
        }

        let parsed_line = serde_json::from_str::<Value>(&line).ok();

        // Anything with a `method` field is a server→client request or
        // notification. Those are NOT the response we're waiting for; drop
        // them from the response pipeline and, when they are requests
        // (i.e. they carry an id), auto-reply so the server doesn't stall
        // waiting for something we have no way to surface back through
        // the HTTP bridge.
        if let Some(v) = &parsed_line {
            if let Some(method) = v.get("method").and_then(Value::as_str) {
                let id = v.get("id").cloned();
                tracing::debug!(
                    mcp = %server_name,
                    method = %method,
                    has_id = id.is_some(),
                    raw = %line,
                    "stdio MCP server-initiated message; handling locally",
                );
                if let Some(id) = id {
                    let reply = synthesise_server_reply(method, id);
                    let bytes = serde_json::to_vec(&reply)?;
                    tracing::debug!(
                        mcp = %server_name,
                        method = %method,
                        response = %String::from_utf8_lossy(&bytes),
                        "→ stdin (auto-reply to server request)",
                    );
                    write_line(stdin, &bytes).await?;
                }
                continue;
            }
        }

        tracing::debug!(
            mcp = %server_name,
            response = %String::from_utf8_lossy(line.as_bytes()),
            "← stdout",
        );
        return Ok(line.into_bytes());
    }
}

async fn write_line(
    stdin: &Arc<Mutex<tokio::process::ChildStdin>>,
    bytes: &[u8],
) -> Result<()> {
    let mut stdin = stdin.lock().await;
    stdin.write_all(bytes).await.context("write to MCP stdin")?;
    stdin
        .write_all(b"\n")
        .await
        .context("write newline to MCP stdin")?;
    stdin.flush().await.ok();
    Ok(())
}

/// Build a canned JSON-RPC reply for server-initiated requests the broker
/// cannot round-trip to the real client. For `roots/list` we know the
/// container's workspace layout and can serve an accurate answer locally;
/// anything else turns into a `-32601 method not found` error so the
/// server proceeds gracefully instead of waiting for a reply.
fn synthesise_server_reply(method: &str, id: Value) -> Value {
    match method {
        "roots/list" => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "roots": [
                    {"uri": "file:///workspace", "name": "workspace"}
                ]
            }
        }),
        _ => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!(
                    "agent-container does not forward server-initiated MCP request '{method}' to the client"
                )
            }
        }),
    }
}
