//! Host-side broker HTTP server. The container hits it through the
//! forward-proxy sidecar to obtain fresh Bedrock credentials and to reach
//! the host's HTTP/SSE MCP servers without learning their auth headers.
//!
//! The MCP path is not a dumb reverse proxy — it understands enough of
//! JSON-RPC to enforce the operator's per-tool allowlist:
//!
//! - `tools/call` requests are rejected up-front when the named tool is
//!   disallowed, so the upstream never sees the attempt.
//! - `tools/list` responses (when the upstream returns `application/json`)
//!   are parsed, filtered, and re-serialised so Claude Code only learns
//!   about allowed tools. The `annotations.readOnlyHint` on each tool is
//!   cached so `tools/call` can fall back to the same default.
//! - Streaming (SSE) responses are passed through unfiltered for now.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};

use crate::aws::{BedrockCredentials, BedrockSetup, resolve_credentials};
use crate::mcp::{HttpMcpServer, McpServer};
use crate::oauth::OAuthStore;
use crate::policy::McpPolicy;
use crate::stdio_mcp::{self, PathBridge, StdioHandle};
use crate::task_runner::{self, TaskRunner};

enum McpBackend {
    Http(HttpMcpServer),
    Stdio(StdioHandle),
    TaskRunner(Arc<TaskRunner>),
}

struct BrokerState {
    bedrock: Option<(BedrockSetup, Option<String>)>,
    last_error: Mutex<Option<String>>,
    mcp: HashMap<String, McpBackend>,
    policy: RwLock<McpPolicy>,
    annotations: Mutex<HashMap<String, HashMap<String, Option<bool>>>>,
    oauth: Arc<OAuthStore>,
    http_client: reqwest::Client,
}

pub struct RunningServer {
    pub addr: SocketAddr,
    pub handle: tokio::task::JoinHandle<()>,
}

pub async fn spawn(
    bedrock: Option<(BedrockSetup, Option<String>)>,
    mcp_servers: Vec<McpServer>,
    task_runner: Option<TaskRunner>,
    policy: McpPolicy,
    oauth: Arc<OAuthStore>,
    stdio_bridge: Option<PathBridge>,
) -> Result<RunningServer> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind broker listener")?;
    let addr = listener.local_addr()?;

    let http_client = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("failed to build reqwest client")?;

    let mut mcp: HashMap<String, McpBackend> = HashMap::new();
    for server in mcp_servers {
        let name = server.name().to_string();
        match server {
            McpServer::Http(h) => {
                mcp.insert(name, McpBackend::Http(h));
            }
            McpServer::Stdio(s) => match stdio_mcp::spawn_worker(s.clone(), stdio_bridge.clone()) {
                Ok(handle) => {
                    mcp.insert(name, McpBackend::Stdio(handle));
                }
                Err(e) => {
                    eprintln!(
                        "[agent-container] failed to start stdio MCP server '{}': {e:#}",
                        s.name
                    );
                }
            },
        }
    }
    if let Some(runner) = task_runner {
        if mcp.contains_key(task_runner::NAME) {
            eprintln!(
                "[agent-container] note: a user-declared MCP server named '{}' already exists — skipping the built-in task-runner",
                task_runner::NAME
            );
        } else if !runner.is_empty() {
            mcp.insert(
                task_runner::NAME.to_string(),
                McpBackend::TaskRunner(Arc::new(runner)),
            );
        }
    }

    let state = Arc::new(BrokerState {
        bedrock,
        last_error: Mutex::new(None),
        mcp,
        policy: RwLock::new(policy),
        annotations: Mutex::new(HashMap::new()),
        oauth,
        http_client,
    });

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/aws/credentials", get(handle_aws))
        .route("/mcp/:name", any(handle_mcp_root))
        .route("/mcp/:name/*rest", any(handle_mcp_nested))
        .with_state(state);

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "broker server stopped");
        }
    });

    Ok(RunningServer { addr, handle })
}

async fn handle_aws(State(state): State<Arc<BrokerState>>) -> Response {
    tracing::info!("aws credentials requested by container");
    let Some((setup, refresh)) = &state.bedrock else {
        tracing::warn!(
            "aws credentials requested but host has no Bedrock configuration — returning 404"
        );
        return (StatusCode::NOT_FOUND, "Bedrock not configured on the host")
            .into_response();
    };
    match resolve_credentials(setup, refresh.as_deref()) {
        Ok(creds) => {
            *state.last_error.lock().await = None;
            tracing::info!(profile = %setup.profile, "aws credentials resolved and returned");
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                render_awscredentialexport_json(&creds),
            )
                .into_response()
        }
        Err(e) => {
            let msg = format!("{e:#}");
            tracing::error!(error = %msg, "aws credentials resolution failed");
            *state.last_error.lock().await = Some(msg.clone());
            (StatusCode::BAD_GATEWAY, msg).into_response()
        }
    }
}

async fn handle_mcp_root(
    AxumPath(name): AxumPath<String>,
    State(state): State<Arc<BrokerState>>,
    req: Request,
) -> Response {
    forward_mcp(&name, "", state, req).await
}

async fn handle_mcp_nested(
    AxumPath((name, rest)): AxumPath<(String, String)>,
    State(state): State<Arc<BrokerState>>,
    req: Request,
) -> Response {
    forward_mcp(&name, &rest, state, req).await
}

async fn forward_mcp(
    name: &str,
    rest: &str,
    state: Arc<BrokerState>,
    req: Request,
) -> Response {
    let backend_kind = state.mcp.get(name).map(|b| match b {
        McpBackend::Http(_) => BackendKind::Http,
        McpBackend::Stdio(_) => BackendKind::Stdio,
        McpBackend::TaskRunner(_) => BackendKind::TaskRunner,
    });
    let Some(kind) = backend_kind else {
        return (
            StatusCode::NOT_FOUND,
            format!("no MCP server named '{name}' on host"),
        )
            .into_response();
    };

    let result = match kind {
        BackendKind::Http => forward_http(state, name, rest, req).await,
        BackendKind::Stdio => forward_stdio(state, name, req).await,
        BackendKind::TaskRunner => forward_task_runner(state, name, req).await,
    };
    match result {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(name = %name, error = %e, "MCP forward failed");
            (StatusCode::BAD_GATEWAY, format!("mcp proxy error: {e:#}")).into_response()
        }
    }
}

enum BackendKind {
    Http,
    Stdio,
    TaskRunner,
}

async fn forward_http(
    state: Arc<BrokerState>,
    server_name: &str,
    rest_path: &str,
    req: Request,
) -> Result<Response> {
    let server = match state.mcp.get(server_name) {
        Some(McpBackend::Http(s)) => s.clone(),
        _ => bail!("internal: expected HTTP backend for '{server_name}'"),
    };
    let (parts, body) = req.into_parts();
    let upstream_url = build_upstream_url(&server.url, rest_path, parts.uri.query())?;
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .context("invalid HTTP method")?;

    let mut headers = reqwest::header::HeaderMap::new();
    copy_request_headers(&parts.headers, &mut headers);
    apply_server_auth(&server.headers, &mut headers)?;
    if let Some(token) = state
        .oauth
        .access_token(server_name)
        .await
        .with_context(|| format!("refreshing OAuth token for '{server_name}'"))?
    {
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .context("building OAuth Bearer header")?,
        );
    }

    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .context("failed to buffer request body")?;

    if let Some(blocked) = enforce_tool_call_policy(&state, server_name, &body_bytes).await {
        return Ok(blocked);
    }

    let is_tools_list = parse_method(&body_bytes).as_deref() == Some("tools/list");

    let upstream = state
        .http_client
        .request(method, &upstream_url)
        .headers(headers)
        .body(body_bytes.to_vec())
        .send()
        .await
        .context("upstream MCP request failed")?;

    let status = StatusCode::from_u16(upstream.status().as_u16())?;
    let upstream_content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase());
    let is_json =
        matches!(upstream_content_type.as_deref(), Some(t) if t.starts_with("application/json"));
    let is_sse = matches!(
        upstream_content_type.as_deref(),
        Some(t) if t.starts_with("text/event-stream")
    );

    let mut out_headers = HeaderMap::new();
    for (n, v) in upstream.headers() {
        if is_hop_by_hop(n.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(n.as_ref()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out_headers.append(name, val);
        }
    }

    if is_tools_list && (is_json || is_sse) && status.is_success() {
        let raw = upstream
            .bytes()
            .await
            .context("failed to buffer tools/list response body")?;
        let filter_result = if is_json {
            filter_tools_list_body(&raw, server_name, &state.policy, &state.annotations).await
        } else {
            filter_tools_list_sse(&raw, server_name, &state.policy, &state.annotations).await
        };
        let body_bytes = match filter_result {
            Ok(bytes) => {
                // Content-Length now reflects the filtered body.
                out_headers.remove(reqwest::header::CONTENT_LENGTH.as_str());
                bytes
            }
            Err(e) => {
                tracing::warn!(server = %server_name, error = %e, "tools/list filter failed; passing through");
                raw.to_vec()
            }
        };
        let mut builder = Response::builder().status(status);
        *builder
            .headers_mut()
            .expect("response builder headers") = out_headers;
        return Ok(builder.body(Body::from(body_bytes))?);
    }

    let mut builder = Response::builder().status(status);
    *builder
        .headers_mut()
        .expect("response builder headers") = out_headers;
    let stream = upstream.bytes_stream();
    Ok(builder.body(Body::from_stream(stream))?)
}

async fn forward_task_runner(
    state: Arc<BrokerState>,
    server_name: &str,
    req: Request,
) -> Result<Response> {
    let runner = match state.mcp.get(server_name) {
        Some(McpBackend::TaskRunner(r)) => r.clone(),
        _ => bail!("internal: expected TaskRunner backend for '{server_name}'"),
    };

    // Only POST has meaning for the task-runner — everything else would
    // just be Claude Code probing for SSE / optional protocol bits that
    // we don't need. Answer the common ones cleanly.
    if req.method() != axum::http::Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(axum::http::header::ALLOW, "POST")
            .body(Body::from("task-runner accepts POST only"))?);
    }

    let (_parts, body) = req.into_parts();
    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .context("failed to buffer request body")?;

    tracing::debug!(
        server = %server_name,
        body_len = body_bytes.len(),
        "task-runner incoming request",
    );

    match runner.handle(&body_bytes).await {
        Some(value) => {
            let bytes = serde_json::to_vec(&value).context("encoding task-runner response")?;
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(bytes))?)
        }
        None => Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())?),
    }
}

async fn forward_stdio(
    state: Arc<BrokerState>,
    server_name: &str,
    req: Request,
) -> Result<Response> {
    let handle = match state.mcp.get(server_name) {
        Some(McpBackend::Stdio(h)) => h.clone(),
        _ => bail!("internal: expected stdio backend for '{server_name}'"),
    };

    let method = req.method().clone();
    match method.as_str() {
        "POST" => forward_stdio_post(state, server_name, handle, req).await,
        "GET" => forward_stdio_get(server_name, handle).await,
        _ => {
            tracing::debug!(
                server = %server_name,
                method = %method,
                "unsupported method on stdio MCP endpoint; responding 405",
            );
            Ok(Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .header(axum::http::header::ALLOW, "GET, POST")
                .body(Body::from("stdio MCP backend accepts GET or POST"))?)
        }
    }
}

async fn forward_stdio_post(
    state: Arc<BrokerState>,
    server_name: &str,
    handle: StdioHandle,
    req: Request,
) -> Result<Response> {
    let (_parts, body) = req.into_parts();
    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .context("failed to buffer request body")?;

    if let Some(blocked) = enforce_tool_call_policy(&state, server_name, &body_bytes).await {
        return Ok(blocked);
    }

    let is_tools_list = parse_method(&body_bytes).as_deref() == Some("tools/list");
    tracing::debug!(
        server = %server_name,
        method = parse_method(&body_bytes).as_deref().unwrap_or("<unparsed>"),
        body_len = body_bytes.len(),
        "forwarding POST to stdio MCP",
    );

    let outcome = handle
        .submit_post(body_bytes.to_vec())
        .await
        .context("stdio MCP submit failed")?;

    // Notifications / responses to server-initiated requests: nothing to
    // wait on, confirm receipt to the HTTP caller.
    let Some(response_rx) = outcome.response else {
        return Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())?);
    };

    let response_value = response_rx
        .await
        .map_err(|_| anyhow::anyhow!("stdio MCP dropped the response channel before answering"))?;
    let response_bytes = serde_json::to_vec(&response_value)?;
    tracing::debug!(
        server = %server_name,
        bytes = response_bytes.len(),
        "stdio MCP response ready",
    );

    let body_bytes = if is_tools_list {
        match filter_tools_list_body(
            &response_bytes,
            server_name,
            &state.policy,
            &state.annotations,
        )
        .await
        {
            Ok(filtered) => filtered,
            Err(e) => {
                tracing::warn!(server = %server_name, error = %e, "tools/list filter failed; passing stdio response through");
                response_bytes
            }
        }
    } else {
        response_bytes
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body_bytes))?)
}

async fn forward_stdio_get(server_name: &str, handle: StdioHandle) -> Result<Response> {
    use tokio_stream::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    tracing::debug!(
        server = %server_name,
        "opening SSE channel for server-initiated messages",
    );

    let rx = handle.subscribe();
    let sn = server_name.to_string();
    let stream = BroadcastStream::new(rx).filter_map(move |item| match item {
        Ok(value) => {
            tracing::debug!(
                server = %sn,
                message = %serde_json::to_string(&value).unwrap_or_default(),
                "→ SSE (server → client)",
            );
            let payload = serde_json::to_string(&value).unwrap_or_default();
            let frame = format!("data: {payload}\n\n");
            Some(Ok::<_, std::io::Error>(Bytes::from(frame)))
        }
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            tracing::warn!(
                server = %sn,
                skipped = n,
                "SSE subscriber lagged; some server-initiated messages were dropped",
            );
            None
        }
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(stream))?)
}

/// Run the tool-call allowlist gate. Returns a pre-built JSON-RPC error
/// response when the request should be blocked, or None when it should
/// be forwarded as-is.
async fn enforce_tool_call_policy(
    state: &BrokerState,
    server_name: &str,
    body_bytes: &[u8],
) -> Option<Response> {
    let call = parse_tool_call(body_bytes)?;
    let read_only = {
        let cache = state.annotations.lock().await;
        cache
            .get(server_name)
            .and_then(|m| m.get(&call.name))
            .copied()
            .unwrap_or(None)
    };
    let allowed = {
        let policy = state.policy.read().await;
        policy.tool_allowed(server_name, &call.name, read_only)
    };
    if allowed {
        return None;
    }
    Some(jsonrpc_error_response(
        call.id,
        -32601,
        format!(
            "tool '{}' is blocked by agent-container allowlist",
            call.name
        ),
    ))
}

/// Parse just enough of the request body to extract the JSON-RPC method,
/// if any. Returns None for batches or unparseable bodies.
fn parse_method(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("method")?.as_str().map(|s| s.to_string())
}

struct ParsedToolCall {
    id: Value,
    name: String,
}

fn parse_tool_call(body: &[u8]) -> Option<ParsedToolCall> {
    let v: Value = serde_json::from_slice(body).ok()?;
    if v.get("method")?.as_str()? != "tools/call" {
        return None;
    }
    let name = v.get("params")?.get("name")?.as_str()?.to_string();
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    Some(ParsedToolCall { id, name })
}

fn jsonrpc_error_response(id: Value, code: i32, message: String) -> Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    });
    let bytes = serde_json::to_vec(&body).expect("json encode");
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    builder = builder.header(axum::http::header::CONTENT_LENGTH, bytes.len());
    builder.body(Body::from(bytes)).expect("build error body")
}

pub(crate) async fn filter_tools_list_body(
    raw: &[u8],
    server_name: &str,
    policy: &RwLock<McpPolicy>,
    annotations: &Mutex<HashMap<String, HashMap<String, Option<bool>>>>,
) -> Result<Vec<u8>> {
    let mut parsed: Value = serde_json::from_slice(raw).context("response is not JSON")?;
    let changed = filter_tools_list_value(&mut parsed, server_name, policy, annotations).await;
    if !changed {
        return Ok(raw.to_vec());
    }
    serde_json::to_vec(&parsed).context("re-serialising filtered tools/list")
}

/// Filter `tools/list` responses delivered via Server-Sent Events. MCP
/// streamable-HTTP servers often reply that way: each `data:` line in an
/// event carries a JSON-RPC message. Parse each event, filter any
/// `result.tools` arrays in place, then re-emit the stream with the
/// filtered payload. Non-tools/list events pass through untouched.
pub(crate) async fn filter_tools_list_sse(
    raw: &[u8],
    server_name: &str,
    policy: &RwLock<McpPolicy>,
    annotations: &Mutex<HashMap<String, HashMap<String, Option<bool>>>>,
) -> Result<Vec<u8>> {
    let text_raw = std::str::from_utf8(raw).context("SSE response was not valid UTF-8")?;
    // Normalise line endings so the \n\n split below works regardless.
    let text = text_raw.replace("\r\n", "\n");
    let mut out = String::with_capacity(text.len());

    // Event boundary is a blank line; re-emit each event independently.
    for event in text.split("\n\n") {
        if event.is_empty() {
            continue;
        }
        let mut data = String::new();
        let mut other_lines: Vec<String> = Vec::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            } else if line.is_empty() {
                // intra-event empty line shouldn't occur (event boundary
                // was already the split above), ignore defensively.
            } else {
                other_lines.push(line.to_string());
            }
        }

        let replacement_data = if data.is_empty() {
            None
        } else if let Ok(mut parsed) = serde_json::from_str::<Value>(&data) {
            let changed =
                filter_tools_list_value(&mut parsed, server_name, policy, annotations).await;
            if changed {
                Some(serde_json::to_string(&parsed).unwrap_or(data.clone()))
            } else {
                None
            }
        } else {
            None
        };
        let emit_data = replacement_data.unwrap_or(data);

        for ol in &other_lines {
            out.push_str(ol);
            out.push('\n');
        }
        if !emit_data.is_empty() {
            for line in emit_data.split('\n') {
                out.push_str("data: ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    Ok(out.into_bytes())
}

async fn filter_tools_list_value(
    parsed: &mut Value,
    server_name: &str,
    policy: &RwLock<McpPolicy>,
    annotations: &Mutex<HashMap<String, HashMap<String, Option<bool>>>>,
) -> bool {
    let Some(obj) = parsed.as_object_mut() else {
        return false;
    };
    let Some(result) = obj.get_mut("result").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) else {
        return false;
    };

    let policy_snapshot = policy.read().await.clone();
    let mut cache = annotations.lock().await;
    let server_cache = cache.entry(server_name.to_string()).or_default();

    let mut kept = Vec::with_capacity(tools.len());
    for tool in tools.drain(..) {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .map(String::from);
        let read_only = tool
            .get("annotations")
            .and_then(|a| a.get("readOnlyHint"))
            .and_then(Value::as_bool);
        if let Some(n) = &name {
            server_cache.insert(n.clone(), read_only);
        }
        let Some(n) = name else {
            continue;
        };
        if policy_snapshot.tool_allowed(server_name, &n, read_only) {
            kept.push(tool);
        }
    }
    *tools = kept;
    true
}

fn build_upstream_url(base: &str, rest: &str, query: Option<&str>) -> Result<String> {
    let mut url = base.trim_end_matches('/').to_string();
    if !rest.is_empty() {
        url.push('/');
        url.push_str(rest.trim_start_matches('/'));
    }
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    Ok(url)
}

fn copy_request_headers(src: &HeaderMap, dst: &mut reqwest::header::HeaderMap) {
    for (name, value) in src.iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        // Container clients should not be supplying auth — strip any that
        // leaked through so only the host's configured headers reach the
        // upstream server.
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "authorization" || lower == "x-api-key" || lower == "cookie" {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_ref()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            dst.append(n, v);
        }
    }
}

fn apply_server_auth(
    auth: &BTreeMap<String, String>,
    dst: &mut reqwest::header::HeaderMap,
) -> Result<()> {
    for (k, v) in auth {
        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid MCP header name {k}"))?;
        let value = reqwest::header::HeaderValue::from_str(v)
            .with_context(|| format!("invalid MCP header value for {k}"))?;
        dst.insert(name, value);
    }
    Ok(())
}

fn is_hop_by_hop(name: &str) -> bool {
    const HOP: &[&str] = &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
    ];
    HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// Emit the JSON shape Claude Code's `awsCredentialExport` command
/// interface expects:
///
/// ```json
/// {
///   "Credentials": {
///     "AccessKeyId": "...",
///     "SecretAccessKey": "...",
///     "SessionToken": "..."
///   }
/// }
/// ```
///
/// The container-side `awsCredentialExport` command is a `curl` against
/// this endpoint, so the container never has to touch its own
/// `~/.aws/credentials` — Claude Code keeps the creds in memory.
fn render_awscredentialexport_json(creds: &BedrockCredentials) -> String {
    let mut credentials = serde_json::Map::new();
    credentials.insert(
        "AccessKeyId".to_string(),
        serde_json::Value::String(creds.access_key_id.clone()),
    );
    credentials.insert(
        "SecretAccessKey".to_string(),
        serde_json::Value::String(creds.secret_access_key.clone()),
    );
    if let Some(token) = &creds.session_token {
        credentials.insert(
            "SessionToken".to_string(),
            serde_json::Value::String(token.clone()),
        );
    }
    let body = serde_json::json!({ "Credentials": serde_json::Value::Object(credentials) });
    serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_json_emits_credentials_shape_without_session_token() {
        let c = BedrockCredentials {
            access_key_id: "AKIA".into(),
            secret_access_key: "SECRET".into(),
            session_token: None,
        };
        let out = render_awscredentialexport_json(&c);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["Credentials"]["AccessKeyId"], "AKIA");
        assert_eq!(v["Credentials"]["SecretAccessKey"], "SECRET");
        assert!(v["Credentials"].get("SessionToken").is_none());
    }

    #[test]
    fn aws_json_includes_session_token_when_present() {
        let c = BedrockCredentials {
            access_key_id: "AKIA".into(),
            secret_access_key: "SECRET".into(),
            session_token: Some("TOKEN".into()),
        };
        let out = render_awscredentialexport_json(&c);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["Credentials"]["SessionToken"], "TOKEN");
    }

    #[test]
    fn upstream_url_joins_paths_and_query() {
        assert_eq!(
            build_upstream_url("https://example.com/mcp", "", Some("k=v")).unwrap(),
            "https://example.com/mcp?k=v"
        );
        assert_eq!(
            build_upstream_url("https://example.com/mcp/", "messages", None).unwrap(),
            "https://example.com/mcp/messages"
        );
        assert_eq!(
            build_upstream_url("https://example.com/", "/foo/bar", Some("x=1")).unwrap(),
            "https://example.com/foo/bar?x=1"
        );
    }

    #[test]
    fn parse_method_extracts_jsonrpc_method_name() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        assert_eq!(parse_method(body).as_deref(), Some("tools/list"));
        assert!(parse_method(b"not json").is_none());
        assert!(parse_method(br#"[{"method":"x"}]"#).is_none());
    }

    #[test]
    fn parse_tool_call_extracts_name_and_id() {
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"evil"}}"#;
        let call = parse_tool_call(body).unwrap();
        assert_eq!(call.name, "evil");
        assert_eq!(call.id, Value::from(7));
        // A `tools/list` is not a tool call.
        assert!(parse_tool_call(br#"{"method":"tools/list"}"#).is_none());
    }

    #[tokio::test]
    async fn tools_list_filter_drops_non_readonly_by_default() {
        let raw = br#"{
          "jsonrpc":"2.0","id":1,
          "result":{"tools":[
            {"name":"read_file","annotations":{"readOnlyHint":true}},
            {"name":"delete_file","annotations":{"readOnlyHint":false}},
            {"name":"unknown"}
          ]}
        }"#;
        let policy = RwLock::new(McpPolicy::default());
        let ann: Mutex<HashMap<String, HashMap<String, Option<bool>>>> =
            Mutex::new(HashMap::new());

        let out = filter_tools_list_body(raw, "srv", &policy, &ann).await.unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let names: Vec<_> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["read_file"]);
        // annotations cache populated.
        let cache = ann.lock().await;
        assert_eq!(
            cache["srv"].get("delete_file").copied().flatten(),
            Some(false)
        );
    }

    #[tokio::test]
    async fn tools_list_filter_respects_explicit_enables() {
        let raw = br#"{
          "jsonrpc":"2.0",
          "result":{"tools":[
            {"name":"read_file","annotations":{"readOnlyHint":true}},
            {"name":"delete_file","annotations":{"readOnlyHint":false}}
          ]}
        }"#;
        let mut policy = McpPolicy::default();
        policy.set_tool("srv", "delete_file", true);
        let policy = RwLock::new(policy);
        let ann = Mutex::new(HashMap::new());

        let out = filter_tools_list_body(raw, "srv", &policy, &ann).await.unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let names: Vec<_> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["read_file", "delete_file"]);
    }

    #[tokio::test]
    async fn sse_filter_drops_non_readonly_tools() {
        let raw = b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"read_file\",\"annotations\":{\"readOnlyHint\":true}},{\"name\":\"delete_file\",\"annotations\":{\"readOnlyHint\":false}}]}}\n\n";
        let policy = RwLock::new(McpPolicy::default());
        let ann = Mutex::new(HashMap::new());
        let filtered = filter_tools_list_sse(raw, "srv", &policy, &ann).await.unwrap();
        let text = String::from_utf8(filtered).unwrap();
        // Pull the data: line back out of the re-emitted SSE and parse it.
        let data = text
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .expect("data line in filtered SSE");
        let v: Value = serde_json::from_str(data).unwrap();
        let names: Vec<_> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["read_file"]);
        // Cache populated by the SSE path, so tools/call can decide.
        let cache = ann.lock().await;
        assert_eq!(
            cache["srv"].get("delete_file").copied().flatten(),
            Some(false)
        );
    }

    #[tokio::test]
    async fn sse_filter_preserves_non_tools_list_events() {
        let raw = b"event: ping\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        let policy = RwLock::new(McpPolicy::default());
        let ann = Mutex::new(HashMap::new());
        let filtered = filter_tools_list_sse(raw, "srv", &policy, &ann).await.unwrap();
        let text = String::from_utf8(filtered).unwrap();
        assert!(text.contains("event: ping"));
        assert!(text.contains("notifications/progress"));
    }

    #[tokio::test]
    async fn tools_list_filter_hides_everything_for_disabled_server() {
        let raw = br#"{
          "result":{"tools":[
            {"name":"read_file","annotations":{"readOnlyHint":true}}
          ]}
        }"#;
        let mut policy = McpPolicy::default();
        policy.set_server_enabled("srv", false);
        let policy = RwLock::new(policy);
        let ann = Mutex::new(HashMap::new());
        let out = filter_tools_list_body(raw, "srv", &policy, &ann).await.unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let arr = v["result"]["tools"].as_array().unwrap();
        assert!(arr.is_empty());
    }
}
