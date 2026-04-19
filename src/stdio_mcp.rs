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

    {
        let mut stdin = stdin.lock().await;
        stdin
            .write_all(&compact)
            .await
            .context("write to MCP stdin")?;
        stdin
            .write_all(b"\n")
            .await
            .context("write newline to MCP stdin")?;
        stdin.flush().await.ok();
    }

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
        // Skip server-initiated notifications so the response we hand back
        // is always the one matching the caller's request.
        match serde_json::from_str::<Value>(&line) {
            Ok(v) if v.get("id").is_none() => {
                tracing::debug!(mcp = %server_name, notification = %line);
                continue;
            }
            _ => return Ok(line.into_bytes()),
        }
    }
}
