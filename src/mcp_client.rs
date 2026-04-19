//! Minimal async MCP client used by the `config mcp` TUI to fetch a live
//! tools/list from each upstream server so the operator can see what's
//! available and what the defaults will select.
//!
//! Only enough of the spec is implemented to survive the initialize /
//! initialized / tools/list handshake that most servers require; anything
//! beyond that belongs in the broker or a dedicated MCP library.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::mcp::HttpMcpServer;

const PROTOCOL_VERSION: &str = "2025-03-26";
const CLIENT_NAME: &str = "agent-container";

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub annotations: Option<ToolAnnotations>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolAnnotations {
    #[serde(default, rename = "readOnlyHint")]
    pub read_only_hint: Option<bool>,
    #[serde(default, rename = "destructiveHint")]
    pub destructive_hint: Option<bool>,
}

impl Tool {
    pub fn read_only_hint(&self) -> Option<bool> {
        self.annotations.as_ref().and_then(|a| a.read_only_hint)
    }
}

pub async fn fetch_tools(server: &HttpMcpServer) -> Result<Vec<Tool>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build reqwest client")?;

    let mut session_id: Option<String> = None;

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": CLIENT_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
        }
    });
    let init_resp = post(&client, server, &init_req, session_id.as_deref()).await?;
    if let Some(v) = init_resp.headers.get("mcp-session-id") {
        session_id = Some(v.clone());
    }
    ensure_no_error(&init_resp.body, "initialize")?;

    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    // Notifications have no response body, but some servers still answer
    // with 200 + empty body; fire-and-forget is fine.
    let _ = post(&client, server, &initialized, session_id.as_deref()).await;

    let list_req = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "tools/list"
    });
    let list_resp = post(&client, server, &list_req, session_id.as_deref()).await?;
    ensure_no_error(&list_resp.body, "tools/list")?;
    let tools = list_resp
        .body
        .get("result")
        .and_then(|r| r.get("tools"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("tools/list response missing result.tools"))?;
    let tools: Vec<Tool> =
        serde_json::from_value(tools).context("tools/list response has unexpected shape")?;
    Ok(tools)
}

struct RpcResponse {
    headers: std::collections::BTreeMap<String, String>,
    body: Value,
}

async fn post(
    client: &reqwest::Client,
    server: &HttpMcpServer,
    payload: &Value,
    session_id: Option<&str>,
) -> Result<RpcResponse> {
    let mut req = client
        .post(&server.url)
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .json(payload);
    for (k, v) in &server.headers {
        req = req.header(k, v);
    }
    if let Some(id) = session_id {
        req = req.header("mcp-session-id", id);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("POST {} failed", server.url))?;

    let status = resp.status();
    let headers: std::collections::BTreeMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|vs| (k.as_str().to_string(), vs.to_string())))
        .collect();

    let content_type = headers
        .get("content-type")
        .cloned()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let text = resp.text().await.context("reading response body")?;

    if !status.is_success() {
        bail!("upstream returned HTTP {}: {}", status, text);
    }
    if text.is_empty() {
        return Ok(RpcResponse {
            headers,
            body: Value::Null,
        });
    }

    let body = if content_type.starts_with("text/event-stream") {
        // Pull the first `data:` line that parses as a JSON-RPC message.
        parse_sse_first_message(&text)?
    } else {
        serde_json::from_str(&text).with_context(|| format!("response JSON parse: {text}"))?
    };
    Ok(RpcResponse { headers, body })
}

fn parse_sse_first_message(text: &str) -> Result<Value> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.trim();
            if rest.is_empty() || rest == "[DONE]" {
                continue;
            }
            return serde_json::from_str(rest).context("SSE data JSON parse");
        }
    }
    bail!("SSE stream contained no data line");
}

fn ensure_no_error(body: &Value, context: &'static str) -> Result<()> {
    if let Some(err) = body.get("error") {
        bail!("{context} returned JSON-RPC error: {err}");
    }
    Ok(())
}
