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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
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
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::aws::{BedrockCredentials, BedrockSetup, resolve_credentials};
use crate::host_fs::{self, HostFs};
use crate::mcp::{HttpMcpServer, McpServer, StdioMcpServer};
use crate::mcp_recovery;
use crate::oauth::OAuthStore;
use crate::policy::McpPolicy;
use crate::stdio_mcp::{self, PathBridge, StdioHandle};
use crate::task_runner::{self, TaskRunner};

enum McpBackend {
    Http(HttpMcpServer),
    Stdio {
        spec: StdioMcpServer,
        handle: StdioHandle,
        bridge: Option<PathBridge>,
        initialized: bool,
    },
    FailedStdio {
        spec: StdioMcpServer,
        bridge: Option<PathBridge>,
        error: String,
    },
    TaskRunner(Arc<TaskRunner>),
    HostFs(Arc<HostFs>),
}

impl Clone for McpBackend {
    fn clone(&self) -> Self {
        match self {
            Self::Http(h) => Self::Http(h.clone()),
            Self::Stdio {
                spec,
                handle,
                bridge,
                initialized,
            } => Self::Stdio {
                spec: spec.clone(),
                handle: handle.clone(),
                bridge: bridge.clone(),
                initialized: *initialized,
            },
            Self::FailedStdio {
                spec,
                bridge,
                error,
            } => Self::FailedStdio {
                spec: spec.clone(),
                bridge: bridge.clone(),
                error: error.clone(),
            },
            Self::TaskRunner(r) => Self::TaskRunner(r.clone()),
            Self::HostFs(h) => Self::HostFs(h.clone()),
        }
    }
}

struct BrokerState {
    bedrock: Option<(BedrockSetup, Option<String>)>,
    last_error: Mutex<Option<String>>,
    mcp: RwLock<HashMap<String, McpBackend>>,
    notifications: RwLock<HashMap<String, broadcast::Sender<Value>>>,
    policy: RwLock<McpPolicy>,
    annotations: Mutex<HashMap<String, HashMap<String, Option<bool>>>>,
    recovery: Mutex<HashMap<String, String>>,
    http_sessions: Mutex<HashMap<String, HttpSession>>,
    http_initialized: Mutex<HashSet<String>>,
    oauth: Arc<OAuthStore>,
    http_client: reqwest::Client,
}

#[derive(Clone)]
struct HttpSession {
    session_id: String,
    protocol_version: String,
}

pub struct RunningServer {
    pub addr: SocketAddr,
    pub handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
pub struct McpReloadConfig {
    pub workspace: PathBuf,
    pub task_runner_enabled: bool,
    pub policy_scope: McpPolicyScope,
}

#[derive(Debug, Clone, Copy)]
pub enum McpPolicyScope {
    ClaudeCode,
    Codex,
}

pub async fn spawn(
    bedrock: Option<(BedrockSetup, Option<String>)>,
    mcp_servers: Vec<McpServer>,
    task_runner: Option<TaskRunner>,
    host_fs: Option<HostFs>,
    policy: McpPolicy,
    oauth: Arc<OAuthStore>,
    stdio_bridge: Option<PathBridge>,
    reload: Option<McpReloadConfig>,
) -> Result<RunningServer> {
    // Loopback bind is load-bearing for the security model: this broker
    // hands out fresh AWS Bedrock credentials and proxies authenticated
    // MCP traffic, and nothing about the protocol authenticates the
    // caller. Binding to `0.0.0.0` would expose those endpoints to the
    // host's LAN — a co-worker on the same Wi-Fi could pull session
    // tokens off the wire. The container reaches this listener via an
    // engine-specific hostname (see `host_kind::HostKind`) that tunnels
    // *only* through the per-engine VM/bridge plumbing into host
    // loopback, so 127.0.0.1 is sufficient for reachability while
    // keeping the LAN out.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind broker listener")?;
    let addr = listener.local_addr()?;

    let http_client = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("failed to build reqwest client")?;

    let mut mcp: HashMap<String, McpBackend> = HashMap::new();
    let mut notifications: HashMap<String, broadcast::Sender<Value>> = HashMap::new();
    for server in mcp_servers {
        let name = server.name().to_string();
        match server {
            McpServer::Http(h) => {
                notifications.insert(name.clone(), new_notification_channel());
                mcp.insert(name, McpBackend::Http(h));
            }
            McpServer::Stdio(s) => match stdio_mcp::spawn_worker(s.clone(), stdio_bridge.clone()) {
                Ok(handle) => {
                    notifications.insert(name.clone(), new_notification_channel());
                    mcp.insert(
                        name,
                        McpBackend::Stdio {
                            spec: s,
                            handle,
                            bridge: stdio_bridge.clone(),
                            initialized: false,
                        },
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[agent-container] failed to start stdio MCP server '{}': {e:#}",
                        s.name
                    );
                    notifications.insert(name.clone(), new_notification_channel());
                    mcp.insert(
                        name,
                        McpBackend::FailedStdio {
                            spec: s,
                            bridge: stdio_bridge.clone(),
                            error: format!("{e:#}"),
                        },
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
        } else {
            notifications.insert(task_runner::NAME.to_string(), new_notification_channel());
            mcp.insert(
                task_runner::NAME.to_string(),
                McpBackend::TaskRunner(Arc::new(runner)),
            );
        }
    }
    if let Some(host_fs) = host_fs {
        if mcp.contains_key(host_fs::NAME) {
            eprintln!(
                "[agent-container] note: a user-declared MCP server named '{}' already exists — skipping the built-in host-fs",
                host_fs::NAME
            );
        } else {
            notifications.insert(host_fs::NAME.to_string(), new_notification_channel());
            mcp.insert(
                host_fs::NAME.to_string(),
                McpBackend::HostFs(Arc::new(host_fs)),
            );
        }
    }

    let state = Arc::new(BrokerState {
        bedrock,
        last_error: Mutex::new(None),
        mcp: RwLock::new(mcp),
        notifications: RwLock::new(notifications),
        policy: RwLock::new(policy),
        annotations: Mutex::new(HashMap::new()),
        recovery: Mutex::new(HashMap::new()),
        http_sessions: Mutex::new(HashMap::new()),
        http_initialized: Mutex::new(HashSet::new()),
        oauth,
        http_client,
    });

    if let Some(config) = reload {
        tokio::spawn(watch_mcp_settings(state.clone(), config));
    }

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

fn new_notification_channel() -> broadcast::Sender<Value> {
    let (tx, _) = broadcast::channel(128);
    tx
}

async fn watch_mcp_settings(state: Arc<BrokerState>, config: McpReloadConfig) {
    let mut last = crate::settings::watched_file_fingerprint(&config.workspace);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let current = crate::settings::watched_file_fingerprint(&config.workspace);
        if current == last {
            continue;
        }
        last = current;
        if let Err(e) = reload_mcp_settings(&state, &config).await {
            tracing::warn!(error = %e, "failed to reload MCP settings");
        }
    }
}

async fn reload_mcp_settings(state: &BrokerState, config: &McpReloadConfig) -> Result<()> {
    let merged = crate::settings::Settings::load_merged(&config.workspace)
        .context("failed to reload merged settings")?;
    {
        let mut policy = state.policy.write().await;
        *policy = match config.policy_scope {
            McpPolicyScope::ClaudeCode => merged.claude_code.mcp,
            McpPolicyScope::Codex => merged.codex.mcp,
        };
    }

    if config.task_runner_enabled {
        let tasks = task_runner::load_specs_from_settings(&config.workspace)?;
        let mut mcp = state.mcp.write().await;
        mcp.insert(
            task_runner::NAME.to_string(),
            McpBackend::TaskRunner(Arc::new(TaskRunner::new(tasks))),
        );
    }

    broadcast_tools_list_changed(state).await;
    Ok(())
}

async fn broadcast_tools_list_changed(state: &BrokerState) {
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/tools/list_changed"
    });
    let senders: Vec<_> = state.notifications.read().await.values().cloned().collect();
    for sender in senders {
        let _ = sender.send(notification.clone());
    }
}

async fn handle_aws(State(state): State<Arc<BrokerState>>) -> Response {
    tracing::info!("aws credentials requested by container");
    let Some((setup, refresh)) = &state.bedrock else {
        tracing::warn!(
            "aws credentials requested but host has no Bedrock configuration — returning 404"
        );
        return (StatusCode::NOT_FOUND, "Bedrock not configured on the host").into_response();
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

async fn forward_mcp(name: &str, rest: &str, state: Arc<BrokerState>, req: Request) -> Response {
    let backend = state.mcp.read().await.get(name).cloned();
    let backend_kind = backend.as_ref().map(|b| match b {
        McpBackend::Http(_) => BackendKind::Http,
        McpBackend::Stdio { .. } | McpBackend::FailedStdio { .. } => BackendKind::Stdio,
        McpBackend::TaskRunner(_) => BackendKind::TaskRunner,
        McpBackend::HostFs(_) => BackendKind::HostFs,
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
        BackendKind::HostFs => forward_host_fs(state, name, req).await,
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
    HostFs,
}

fn local_standard_mcp_response(body: &[u8]) -> Option<Result<Response>> {
    let method = parse_method(body)?;
    let id = parse_jsonrpc_id(body);
    match method.as_str() {
        "initialize" => Some(synthetic_initialize_response(id)),
        m if m.starts_with("notifications/") => Some(
            Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Body::empty())
                .context("building notification response"),
        ),
        "ping" => Some(json_response_value(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        }))),
        "resources/list" => Some(json_response_value(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"resources": []}
        }))),
        "prompts/list" => Some(json_response_value(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"prompts": []}
        }))),
        _ => None,
    }
}

fn parse_jsonrpc_id(body: &[u8]) -> Value {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .unwrap_or(Value::Null)
}

fn is_recovery_tool_call(body: &[u8]) -> Option<Value> {
    let call = parse_tool_call(body)?;
    (call.name == mcp_recovery::TOOL_NAME).then_some(call.id)
}

fn json_response_value(value: Value) -> Result<Response> {
    let bytes = serde_json::to_vec(&value).context("encoding JSON response")?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::CONTENT_LENGTH, bytes.len())
        .body(Body::from(bytes))?)
}

fn synthetic_initialize_response(id: Value) -> Result<Response> {
    json_response_value(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {
                "name": "agent-container-recovery",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    }))
}

fn recovery_tools_list_response(id: Value, server_name: &str, reason: &str) -> Result<Response> {
    json_response_value(mcp_recovery::tools_list_response(id, server_name, reason))
}

fn recovery_tool_result_response(id: Value, message: String, is_error: bool) -> Result<Response> {
    json_response_value(mcp_recovery::tool_result_response(id, message, is_error))
}

async fn mark_mcp_recovery(state: &BrokerState, server_name: &str, reason: String) {
    state.http_initialized.lock().await.remove(server_name);
    let mut recovery = state.recovery.lock().await;
    let changed = recovery.get(server_name) != Some(&reason);
    recovery.insert(server_name.to_string(), reason);
    drop(recovery);
    if changed {
        broadcast_tools_list_changed(state).await;
    }
}

async fn clear_mcp_recovery(state: &BrokerState, server_name: &str) {
    let mut recovery = state.recovery.lock().await;
    let changed = recovery.remove(server_name).is_some();
    drop(recovery);
    if changed {
        broadcast_tools_list_changed(state).await;
    }
}

fn jsonrpc_error_text(value: &Value) -> Option<String> {
    value.get("error").map(|e| e.to_string())
}

async fn forward_http(
    state: Arc<BrokerState>,
    server_name: &str,
    rest_path: &str,
    req: Request,
) -> Result<Response> {
    if req.method() == axum::http::Method::GET && rest_path.is_empty() {
        return forward_local_notifications_get(state, server_name).await;
    }

    let server = match state.mcp.read().await.get(server_name) {
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

    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .context("failed to buffer request body")?;

    if let Some(id) = is_recovery_tool_call(&body_bytes) {
        return restart_mcp_server(state, server_name, id).await;
    }

    if let Some(response) = local_standard_mcp_response(&body_bytes) {
        return response;
    }

    if let Some(blocked) = enforce_tool_call_policy(&state, server_name, &body_bytes).await {
        return Ok(blocked);
    }

    let method_name = parse_method(&body_bytes);
    let is_tools_list = method_name.as_deref() == Some("tools/list");
    let tool_call = parse_tool_call(&body_bytes);

    if is_tools_list || tool_call.is_some() {
        if let Err(e) = ensure_http_initialized(&state, server_name, &server).await {
            let reason = format!("{e:#}");
            mark_mcp_recovery(&state, server_name, reason.clone()).await;
            if is_tools_list {
                return recovery_tools_list_response(
                    parse_jsonrpc_id(&body_bytes),
                    server_name,
                    &reason,
                );
            }
            if let Some(call) = tool_call.as_ref() {
                return recovery_tool_result_response(
                    call.id.clone(),
                    format!("MCP server '{server_name}' is not initialized: {reason}"),
                    true,
                );
            }
        }
    }

    match state.oauth.access_token(server_name).await {
        Ok(Some(token)) => {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .context("building OAuth Bearer header")?,
            );
        }
        Ok(None) => {}
        Err(e) if is_tools_list => {
            let reason = format!("failed to refresh OAuth token for '{server_name}': {e:#}");
            mark_mcp_recovery(&state, server_name, reason.clone()).await;
            return recovery_tools_list_response(
                parse_jsonrpc_id(&body_bytes),
                server_name,
                &reason,
            );
        }
        Err(e) if tool_call.is_some() => {
            let reason = format!("failed to refresh OAuth token for '{server_name}': {e:#}");
            mark_mcp_recovery(&state, server_name, reason.clone()).await;
            if let Some(call) = tool_call.as_ref() {
                return recovery_tool_result_response(call.id.clone(), reason, true);
            }
        }
        Err(e) => {
            return Err(e).with_context(|| format!("refreshing OAuth token for '{server_name}'"));
        }
    }

    if let Some(session) = state.http_sessions.lock().await.get(server_name).cloned() {
        headers.insert(
            "mcp-session-id",
            reqwest::header::HeaderValue::from_str(&session.session_id)
                .context("building stored MCP session header")?,
        );
        headers.insert(
            "mcp-protocol-version",
            reqwest::header::HeaderValue::from_str(&session.protocol_version)
                .context("building stored MCP protocol header")?,
        );
    }

    let upstream = match state
        .http_client
        .request(method, &upstream_url)
        .headers(headers)
        .body(body_bytes.to_vec())
        .send()
        .await
    {
        Ok(upstream) => upstream,
        Err(e) if is_tools_list => {
            let reason = format!("upstream MCP request failed: {e:#}");
            mark_mcp_recovery(&state, server_name, reason.clone()).await;
            return recovery_tools_list_response(
                parse_jsonrpc_id(&body_bytes),
                server_name,
                &reason,
            );
        }
        Err(e) if tool_call.is_some() => {
            let reason = format!("upstream MCP request failed: {e:#}");
            mark_mcp_recovery(&state, server_name, reason.clone()).await;
            let call = tool_call.as_ref().expect("tool_call checked by guard");
            return recovery_tool_result_response(call.id.clone(), reason, true);
        }
        Err(e) => return Err(e).context("upstream MCP request failed"),
    };

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

    if is_tools_list && !status.is_success() {
        let raw = upstream
            .bytes()
            .await
            .context("failed to buffer failed MCP response body")?;
        let reason = format!(
            "upstream returned HTTP {}: {}",
            status,
            String::from_utf8_lossy(&raw)
        );
        mark_mcp_recovery(&state, server_name, reason.clone()).await;
        return recovery_tools_list_response(parse_jsonrpc_id(&body_bytes), server_name, &reason);
    }

    if is_tools_list && (is_json || is_sse) && status.is_success() {
        let raw = upstream
            .bytes()
            .await
            .context("failed to buffer tools/list response body")?;
        if is_json {
            if let Ok(parsed) = serde_json::from_slice::<Value>(&raw) {
                if let Some(reason) = jsonrpc_error_text(&parsed) {
                    mark_mcp_recovery(&state, server_name, reason.clone()).await;
                    return recovery_tools_list_response(
                        parse_jsonrpc_id(&body_bytes),
                        server_name,
                        &reason,
                    );
                }
            }
        }
        clear_mcp_recovery(&state, server_name).await;
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
        *builder.headers_mut().expect("response builder headers") = out_headers;
        return Ok(builder.body(Body::from(body_bytes))?);
    }

    let mut builder = Response::builder().status(status);
    *builder.headers_mut().expect("response builder headers") = out_headers;
    let stream = upstream.bytes_stream();
    Ok(builder.body(Body::from_stream(stream))?)
}

async fn forward_task_runner(
    state: Arc<BrokerState>,
    server_name: &str,
    req: Request,
) -> Result<Response> {
    let runner = match state.mcp.read().await.get(server_name) {
        Some(McpBackend::TaskRunner(r)) => r.clone(),
        _ => bail!("internal: expected TaskRunner backend for '{server_name}'"),
    };

    if req.method() == axum::http::Method::GET {
        return forward_local_notifications_get(state, server_name).await;
    }

    // Only POST has meaning for task execution — everything else would
    // just be Claude Code probing for optional protocol bits that we
    // don't need. Answer the common ones cleanly.
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

async fn forward_host_fs(
    state: Arc<BrokerState>,
    server_name: &str,
    req: Request,
) -> Result<Response> {
    let host_fs = match state.mcp.read().await.get(server_name) {
        Some(McpBackend::HostFs(h)) => h.clone(),
        _ => bail!("internal: expected HostFs backend for '{server_name}'"),
    };

    if req.method() == axum::http::Method::GET {
        return forward_local_notifications_get(state, server_name).await;
    }

    if req.method() != axum::http::Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(axum::http::header::ALLOW, "POST")
            .body(Body::from("host-fs accepts POST only"))?);
    }

    let (_parts, body) = req.into_parts();
    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .context("failed to buffer request body")?;

    match host_fs.handle(&body_bytes).await {
        Some(value) => {
            let bytes = serde_json::to_vec(&value).context("encoding host-fs response")?;
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
    let backend = match state.mcp.read().await.get(server_name) {
        Some(McpBackend::Stdio { handle, .. }) => Ok(handle.clone()),
        Some(McpBackend::FailedStdio { error, .. }) => Err(error.clone()),
        _ => bail!("internal: expected stdio backend for '{server_name}'"),
    };

    let method = req.method().clone();
    match method.as_str() {
        "POST" => match backend {
            Ok(handle) => forward_stdio_post(state, server_name, handle, req).await,
            Err(error) => forward_failed_stdio_post(state, server_name, error, req).await,
        },
        "GET" => match backend {
            Ok(handle) => forward_stdio_get(state, server_name, handle).await,
            Err(_) => forward_local_notifications_get(state, server_name).await,
        },
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

async fn forward_failed_stdio_post(
    state: Arc<BrokerState>,
    server_name: &str,
    error: String,
    req: Request,
) -> Result<Response> {
    let (_parts, body) = req.into_parts();
    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .context("failed to buffer request body")?;
    let method_name = parse_method(&body_bytes);

    if let Some(id) = is_recovery_tool_call(&body_bytes) {
        return restart_mcp_server(state, server_name, id).await;
    }
    if let Some(response) = local_standard_mcp_response(&body_bytes) {
        mark_mcp_recovery(&state, server_name, error).await;
        return response;
    }
    if method_name.as_deref() == Some("tools/list") {
        mark_mcp_recovery(&state, server_name, error.clone()).await;
        return recovery_tools_list_response(parse_jsonrpc_id(&body_bytes), server_name, &error);
    }

    Ok(jsonrpc_error_response(
        parse_jsonrpc_id(&body_bytes),
        -32002,
        format!("MCP server '{server_name}' is not running: {error}"),
    ))
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

    if let Some(id) = is_recovery_tool_call(&body_bytes) {
        return restart_mcp_server(state, server_name, id).await;
    }

    if let Some(response) = local_standard_mcp_response(&body_bytes) {
        return response;
    }

    if let Some(blocked) = enforce_tool_call_policy(&state, server_name, &body_bytes).await {
        return Ok(blocked);
    }

    let method_name = parse_method(&body_bytes);
    let is_tools_list = method_name.as_deref() == Some("tools/list");
    let tool_call = parse_tool_call(&body_bytes);

    if is_tools_list || tool_call.is_some() {
        if let Err(e) = ensure_stdio_initialized(&state, server_name, &handle).await {
            let reason = format!("{e:#}");
            mark_mcp_recovery(&state, server_name, reason.clone()).await;
            if is_tools_list {
                return recovery_tools_list_response(
                    parse_jsonrpc_id(&body_bytes),
                    server_name,
                    &reason,
                );
            }
            if let Some(call) = tool_call.as_ref() {
                return recovery_tool_result_response(
                    call.id.clone(),
                    format!("MCP server '{server_name}' is not initialized: {reason}"),
                    true,
                );
            }
        }
    }

    tracing::debug!(
        server = %server_name,
        method = method_name.as_deref().unwrap_or("<unparsed>"),
        body_len = body_bytes.len(),
        "forwarding POST to stdio MCP",
    );

    let outcome = match handle.submit_post(body_bytes.to_vec()).await {
        Ok(outcome) => outcome,
        Err(e) if is_tools_list => {
            let reason = format!("stdio MCP submit failed: {e:#}");
            mark_mcp_recovery(&state, server_name, reason.clone()).await;
            return recovery_tools_list_response(
                parse_jsonrpc_id(&body_bytes),
                server_name,
                &reason,
            );
        }
        Err(e) if tool_call.is_some() => {
            let reason = format!("stdio MCP submit failed: {e:#}");
            mark_mcp_recovery(&state, server_name, reason.clone()).await;
            let call = tool_call.as_ref().expect("tool_call checked by guard");
            return recovery_tool_result_response(call.id.clone(), reason, true);
        }
        Err(e) => return Err(e).context("stdio MCP submit failed"),
    };

    // Notifications / responses to server-initiated requests: nothing to
    // wait on, confirm receipt to the HTTP caller.
    let Some(response_rx) = outcome.response else {
        return Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())?);
    };

    let response_value = match response_rx.await {
        Ok(value) => value,
        Err(_) if is_tools_list => {
            let reason = "stdio MCP dropped the tools/list response channel before answering";
            mark_mcp_recovery(&state, server_name, reason.to_string()).await;
            return recovery_tools_list_response(
                parse_jsonrpc_id(&body_bytes),
                server_name,
                reason,
            );
        }
        Err(_) if tool_call.is_some() => {
            let reason = "stdio MCP dropped the tools/call response channel before answering";
            mark_mcp_recovery(&state, server_name, reason.to_string()).await;
            let call = tool_call.as_ref().expect("tool_call checked by guard");
            return recovery_tool_result_response(call.id.clone(), reason.to_string(), true);
        }
        Err(_) => {
            return Err(anyhow::anyhow!(
                "stdio MCP dropped the response channel before answering"
            ));
        }
    };
    if is_tools_list {
        if let Some(reason) = jsonrpc_error_text(&response_value) {
            mark_mcp_recovery(&state, server_name, reason.clone()).await;
            return recovery_tools_list_response(
                parse_jsonrpc_id(&body_bytes),
                server_name,
                &reason,
            );
        }
        clear_mcp_recovery(&state, server_name).await;
    }
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

async fn forward_stdio_get(
    state: Arc<BrokerState>,
    server_name: &str,
    handle: StdioHandle,
) -> Result<Response> {
    use tokio_stream::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    tracing::debug!(
        server = %server_name,
        "opening SSE channel for server-initiated messages",
    );

    let upstream_rx = handle.subscribe();
    let local_rx = local_notification_receiver(&state, server_name).await?;
    let sn = server_name.to_string();
    let stream = futures::stream::select(
        BroadcastStream::new(upstream_rx),
        BroadcastStream::new(local_rx),
    )
    .filter_map(move |item| sse_notification_frame(item, &sn));

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(stream))?)
}

async fn forward_local_notifications_get(
    state: Arc<BrokerState>,
    server_name: &str,
) -> Result<Response> {
    use tokio_stream::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    tracing::debug!(
        server = %server_name,
        "opening local MCP notification SSE channel",
    );

    let rx = local_notification_receiver(&state, server_name).await?;
    let sn = server_name.to_string();
    let stream = BroadcastStream::new(rx).filter_map(move |item| sse_notification_frame(item, &sn));

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(stream))?)
}

async fn restart_mcp_server(
    state: Arc<BrokerState>,
    server_name: &str,
    id: Value,
) -> Result<Response> {
    let backend = state.mcp.read().await.get(server_name).cloned();
    let Some(backend) = backend else {
        return recovery_tool_result_response(
            id,
            format!("No MCP server named '{server_name}' is registered."),
            true,
        );
    };

    match backend {
        McpBackend::Http(server) => {
            match initialize_http_backend(&state, server_name, &server).await {
                Ok(tool_count) => {
                    clear_mcp_recovery(&state, server_name).await;
                    broadcast_tools_list_changed(&state).await;
                    recovery_tool_result_response(
                        id,
                        format!(
                            "Refreshed credentials and reinitialized MCP server '{server_name}'. Discovered {} tool(s).",
                            tool_count
                        ),
                        false,
                    )
                }
                Err(e) => {
                    let reason = format!("{e:#}");
                    mark_mcp_recovery(&state, server_name, reason.clone()).await;
                    recovery_tool_result_response(
                        id,
                        format!("Failed to reinitialize MCP server '{server_name}': {reason}"),
                        true,
                    )
                }
            }
        }
        McpBackend::Stdio { spec, bridge, .. } | McpBackend::FailedStdio { spec, bridge, .. } => {
            let handle = match stdio_mcp::spawn_worker(spec.clone(), bridge.clone()) {
                Ok(handle) => handle,
                Err(e) => {
                    let reason = format!("{e:#}");
                    state.mcp.write().await.insert(
                        server_name.to_string(),
                        McpBackend::FailedStdio {
                            spec,
                            bridge,
                            error: reason.clone(),
                        },
                    );
                    mark_mcp_recovery(&state, server_name, reason.clone()).await;
                    return recovery_tool_result_response(
                        id,
                        format!("Failed to restart MCP server '{server_name}': {reason}"),
                        true,
                    );
                }
            };

            match initialize_stdio_handle(&handle).await {
                Ok(tool_count) => {
                    state.mcp.write().await.insert(
                        server_name.to_string(),
                        McpBackend::Stdio {
                            spec,
                            handle,
                            bridge,
                            initialized: true,
                        },
                    );
                    clear_mcp_recovery(&state, server_name).await;
                    broadcast_tools_list_changed(&state).await;
                    recovery_tool_result_response(
                        id,
                        format!(
                            "Restarted and reinitialized MCP server '{server_name}'. Discovered {tool_count} tool(s)."
                        ),
                        false,
                    )
                }
                Err(e) => {
                    let reason = format!("{e:#}");
                    state.mcp.write().await.insert(
                        server_name.to_string(),
                        McpBackend::FailedStdio {
                            spec,
                            bridge,
                            error: reason.clone(),
                        },
                    );
                    mark_mcp_recovery(&state, server_name, reason.clone()).await;
                    recovery_tool_result_response(
                        id,
                        format!(
                            "Restarted MCP server '{server_name}', but initialization failed: {reason}"
                        ),
                        true,
                    )
                }
            }
        }
        McpBackend::TaskRunner(_) | McpBackend::HostFs(_) => recovery_tool_result_response(
            id,
            format!("MCP server '{server_name}' is built in and does not support restart."),
            true,
        ),
    }
}

async fn ensure_http_initialized(
    state: &BrokerState,
    server_name: &str,
    server: &HttpMcpServer,
) -> Result<()> {
    if state.http_initialized.lock().await.contains(server_name) {
        return Ok(());
    }
    initialize_http_backend(state, server_name, server)
        .await
        .map(|_| ())?;
    clear_mcp_recovery(state, server_name).await;
    broadcast_tools_list_changed(state).await;
    Ok(())
}

async fn ensure_stdio_initialized(
    state: &BrokerState,
    server_name: &str,
    handle: &StdioHandle,
) -> Result<()> {
    let already_initialized = {
        let mcp = state.mcp.read().await;
        matches!(
            mcp.get(server_name),
            Some(McpBackend::Stdio {
                initialized: true,
                ..
            })
        )
    };
    if already_initialized {
        return Ok(());
    }

    initialize_stdio_handle(handle).await?;

    let mut mcp = state.mcp.write().await;
    if let Some(McpBackend::Stdio { initialized, .. }) = mcp.get_mut(server_name) {
        *initialized = true;
    }
    drop(mcp);
    clear_mcp_recovery(state, server_name).await;
    broadcast_tools_list_changed(state).await;
    Ok(())
}

async fn initialize_stdio_handle(handle: &StdioHandle) -> Result<usize> {
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "agent-container", "version": env!("CARGO_PKG_VERSION")},
        }
    });
    let init_outcome = handle.submit_post(serde_json::to_vec(&init)?).await?;
    if let Some(rx) = init_outcome.response {
        let response = rx
            .await
            .map_err(|_| anyhow::anyhow!("stdio MCP dropped initialize response"))?;
        if let Some(err) = jsonrpc_error_text(&response) {
            bail!("initialize returned JSON-RPC error: {err}");
        }
    }

    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let _ = handle.submit_post(serde_json::to_vec(&initialized)?).await;

    let list = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let list_outcome = handle.submit_post(serde_json::to_vec(&list)?).await?;
    let rx = list_outcome
        .response
        .ok_or_else(|| anyhow::anyhow!("tools/list did not register a response waiter"))?;
    let response = rx
        .await
        .map_err(|_| anyhow::anyhow!("stdio MCP dropped tools/list response"))?;
    if let Some(err) = jsonrpc_error_text(&response) {
        bail!("tools/list returned JSON-RPC error: {err}");
    }
    let count = response
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .context("tools/list response missing result.tools")?;
    Ok(count)
}

async fn initialize_http_backend(
    state: &BrokerState,
    server_name: &str,
    server: &HttpMcpServer,
) -> Result<usize> {
    let token = state
        .oauth
        .refresh_or_reload(server_name)
        .await
        .with_context(|| format!("refreshing OAuth credentials for '{server_name}'"))?;

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "agent-container", "version": env!("CARGO_PKG_VERSION")},
        }
    });
    let init_resp = post_http_recovery_json(
        state,
        server,
        &init,
        None,
        Some("2025-06-18"),
        token.as_deref(),
    )
    .await
    .context("initialize failed")?;
    if let Some(err) = jsonrpc_error_text(&init_resp.body) {
        bail!("initialize returned JSON-RPC error: {err}");
    }
    let protocol_version = init_resp
        .body
        .pointer("/result/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("2025-06-18")
        .to_string();
    if let Some(session_id) = init_resp.headers.get("mcp-session-id").cloned() {
        state.http_sessions.lock().await.insert(
            server_name.to_string(),
            HttpSession {
                session_id,
                protocol_version: protocol_version.clone(),
            },
        );
    }

    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let session_id = init_resp.headers.get("mcp-session-id").map(String::as_str);
    let _ = post_http_recovery_json(
        state,
        server,
        &initialized,
        session_id,
        Some(&protocol_version),
        token.as_deref(),
    )
    .await;

    let list = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let list_resp = post_http_recovery_json(
        state,
        server,
        &list,
        session_id,
        Some(&protocol_version),
        token.as_deref(),
    )
    .await
    .context("tools/list failed")?;
    if let Some(err) = jsonrpc_error_text(&list_resp.body) {
        bail!("tools/list returned JSON-RPC error: {err}");
    }
    let count = list_resp
        .body
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .context("tools/list response missing result.tools")?;
    state
        .http_initialized
        .lock()
        .await
        .insert(server_name.to_string());
    Ok(count)
}

struct HttpRecoveryResponse {
    headers: BTreeMap<String, String>,
    body: Value,
}

async fn post_http_recovery_json(
    state: &BrokerState,
    server: &HttpMcpServer,
    payload: &Value,
    session_id: Option<&str>,
    protocol_version: Option<&str>,
    bearer: Option<&str>,
) -> Result<HttpRecoveryResponse> {
    let mut headers = reqwest::header::HeaderMap::new();
    apply_server_auth(&server.headers, &mut headers)?;
    if let Some(token) = bearer {
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .context("building OAuth Bearer header")?,
        );
    }
    if let Some(session_id) = session_id {
        headers.insert(
            "mcp-session-id",
            reqwest::header::HeaderValue::from_str(session_id)
                .context("building MCP session header")?,
        );
    }
    if let Some(protocol_version) = protocol_version {
        headers.insert(
            "mcp-protocol-version",
            reqwest::header::HeaderValue::from_str(protocol_version)
                .context("building MCP protocol header")?,
        );
    }

    let resp = state
        .http_client
        .post(&server.url)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .headers(headers)
        .json(payload)
        .send()
        .await
        .with_context(|| format!("POST {} failed", server.url))?;
    let status = resp.status();
    let headers: BTreeMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|vs| (k.as_str().to_string(), vs.to_string()))
        })
        .collect();
    let content_type = headers
        .get("content-type")
        .cloned()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let text = resp
        .text()
        .await
        .context("reading recovery response body")?;
    if !status.is_success() {
        bail!("upstream returned HTTP {status}: {text}");
    }
    let body = if content_type.starts_with("text/event-stream") {
        parse_sse_first_json(&text)?
    } else if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).with_context(|| format!("response JSON parse: {text}"))?
    };
    Ok(HttpRecoveryResponse { headers, body })
}

fn parse_sse_first_json(text: &str) -> Result<Value> {
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

async fn local_notification_receiver(
    state: &BrokerState,
    server_name: &str,
) -> Result<broadcast::Receiver<Value>> {
    let notifications = state.notifications.read().await;
    let Some(tx) = notifications.get(server_name) else {
        bail!("internal: missing notification channel for '{server_name}'");
    };
    Ok(tx.subscribe())
}

fn sse_notification_frame(
    item: Result<Value, tokio_stream::wrappers::errors::BroadcastStreamRecvError>,
    server_name: &str,
) -> Option<Result<Bytes, std::io::Error>> {
    match item {
        Ok(value) => {
            tracing::debug!(
                server = %server_name,
                message = %serde_json::to_string(&value).unwrap_or_default(),
                "→ SSE (server → client)",
            );
            let payload = serde_json::to_string(&value).unwrap_or_default();
            let frame = format!("data: {payload}\n\n");
            Some(Ok(Bytes::from(frame)))
        }
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            tracing::warn!(
                server = %server_name,
                skipped = n,
                "SSE subscriber lagged; some server-initiated messages were dropped",
            );
            None
        }
    }
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

    let mut kept = Vec::with_capacity(tools.len() + 1);
    for tool in tools.drain(..) {
        let name = tool.get("name").and_then(Value::as_str).map(String::from);
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
        if n == mcp_recovery::TOOL_NAME {
            continue;
        }
        if policy_snapshot.tool_allowed(server_name, &n, read_only) {
            kept.push(tool);
        }
    }
    server_cache.insert(mcp_recovery::TOOL_NAME.to_string(), Some(false));
    kept.push(mcp_recovery::tool_json(server_name, None));
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
        let ann: Mutex<HashMap<String, HashMap<String, Option<bool>>>> = Mutex::new(HashMap::new());

        let out = filter_tools_list_body(raw, "srv", &policy, &ann)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let names: Vec<_> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["read_file", mcp_recovery::TOOL_NAME]);
        // annotations cache populated.
        let cache = ann.lock().await;
        assert_eq!(
            cache["srv"].get("delete_file").copied().flatten(),
            Some(false)
        );
        assert_eq!(
            cache["srv"].get(mcp_recovery::TOOL_NAME).copied().flatten(),
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

        let out = filter_tools_list_body(raw, "srv", &policy, &ann)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let names: Vec<_> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["read_file", "delete_file", mcp_recovery::TOOL_NAME]
        );
    }

    #[tokio::test]
    async fn sse_filter_drops_non_readonly_tools() {
        let raw = b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"read_file\",\"annotations\":{\"readOnlyHint\":true}},{\"name\":\"delete_file\",\"annotations\":{\"readOnlyHint\":false}}]}}\n\n";
        let policy = RwLock::new(McpPolicy::default());
        let ann = Mutex::new(HashMap::new());
        let filtered = filter_tools_list_sse(raw, "srv", &policy, &ann)
            .await
            .unwrap();
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
        assert_eq!(names, vec!["read_file", mcp_recovery::TOOL_NAME]);
        // Cache populated by the SSE path, so tools/call can decide.
        let cache = ann.lock().await;
        assert_eq!(
            cache["srv"].get("delete_file").copied().flatten(),
            Some(false)
        );
    }

    #[tokio::test]
    async fn sse_filter_preserves_non_tools_list_events() {
        let raw =
            b"event: ping\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        let policy = RwLock::new(McpPolicy::default());
        let ann = Mutex::new(HashMap::new());
        let filtered = filter_tools_list_sse(raw, "srv", &policy, &ann)
            .await
            .unwrap();
        let text = String::from_utf8(filtered).unwrap();
        assert!(text.contains("event: ping"));
        assert!(text.contains("notifications/progress"));
    }

    #[test]
    fn sse_notification_frame_serializes_jsonrpc_notification() {
        let frame = sse_notification_frame(
            Ok(json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed"
            })),
            "srv",
        )
        .unwrap()
        .unwrap();
        let text = String::from_utf8(frame.to_vec()).unwrap();
        assert!(text.starts_with("data: "));
        assert!(text.contains("notifications/tools/list_changed"));
        assert!(text.ends_with("\n\n"));
    }

    #[tokio::test]
    async fn synthetic_initialize_advertises_tools_list_changed() {
        let resp = synthetic_initialize_response(Value::from(9)).unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(v["id"], 9);
        assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], true);
        assert_eq!(
            v["result"]["serverInfo"]["name"],
            "agent-container-recovery"
        );
    }

    #[tokio::test]
    async fn local_standard_response_handles_handshake_without_upstream() {
        let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let init_resp = local_standard_mcp_response(init).unwrap().unwrap();
        let init_body = axum::body::to_bytes(init_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let init_json: Value = serde_json::from_slice(&init_body).unwrap();
        assert_eq!(
            init_json["result"]["capabilities"]["tools"]["listChanged"],
            true
        );

        let ping = br#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        let ping_resp = local_standard_mcp_response(ping).unwrap().unwrap();
        let ping_body = axum::body::to_bytes(ping_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let ping_json: Value = serde_json::from_slice(&ping_body).unwrap();
        assert_eq!(ping_json["result"], json!({}));

        let initialized = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let initialized_resp = local_standard_mcp_response(initialized).unwrap().unwrap();
        assert_eq!(initialized_resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn recovery_tools_list_exposes_restart_tool() {
        let resp = recovery_tools_list_response(Value::from(3), "notion", "HTTP 401").unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], mcp_recovery::TOOL_NAME);
        assert!(
            tools[0]["description"]
                .as_str()
                .unwrap()
                .contains("HTTP 401")
        );
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
        let out = filter_tools_list_body(raw, "srv", &policy, &ann)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let arr = v["result"]["tools"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], mcp_recovery::TOOL_NAME);
    }
}
