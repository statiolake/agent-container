//! Host-side broker HTTP server. The container hits it through the
//! forward-proxy sidecar to obtain fresh Bedrock credentials and to reach
//! the host's HTTP/SSE MCP servers without learning their auth headers.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::aws::{BedrockCredentials, BedrockSetup, resolve_credentials};
use crate::mcp::HttpMcpServer;

struct BrokerState {
    bedrock: Option<(BedrockSetup, Option<String>)>,
    last_error: Mutex<Option<String>>,
    mcp: HashMap<String, HttpMcpServer>,
    http_client: reqwest::Client,
}

pub struct RunningServer {
    pub addr: SocketAddr,
    pub handle: tokio::task::JoinHandle<()>,
}

pub async fn spawn(
    bedrock: Option<(BedrockSetup, Option<String>)>,
    mcp_servers: Vec<HttpMcpServer>,
) -> Result<RunningServer> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind broker listener")?;
    let addr = listener.local_addr()?;

    let http_client = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("failed to build reqwest client")?;

    let mcp: HashMap<String, HttpMcpServer> = mcp_servers
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();

    let state = Arc::new(BrokerState {
        bedrock,
        last_error: Mutex::new(None),
        mcp,
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
    let Some((setup, refresh)) = &state.bedrock else {
        return (StatusCode::NOT_FOUND, "Bedrock not configured on the host")
            .into_response();
    };
    match resolve_credentials(setup, refresh.as_deref()) {
        Ok(creds) => {
            *state.last_error.lock().await = None;
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                render_credentials_file(&creds),
            )
                .into_response()
        }
        Err(e) => {
            let msg = format!("{e:#}");
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
    let Some(server) = state.mcp.get(name).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            format!("no MCP server named '{name}' on host"),
        )
            .into_response();
    };

    match forward_inner(&state.http_client, server, rest, req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(name = %name, error = %e, "MCP forward failed");
            (StatusCode::BAD_GATEWAY, format!("mcp proxy error: {e:#}")).into_response()
        }
    }
}

async fn forward_inner(
    client: &reqwest::Client,
    server: HttpMcpServer,
    rest_path: &str,
    req: Request,
) -> Result<Response> {
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

    let upstream = client
        .request(method, &upstream_url)
        .headers(headers)
        .body(body_bytes.to_vec())
        .send()
        .await
        .context("upstream MCP request failed")?;

    let status = StatusCode::from_u16(upstream.status().as_u16())?;
    let mut builder = Response::builder().status(status);
    if let Some(out_headers) = builder.headers_mut() {
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
    }
    let stream = upstream.bytes_stream();
    Ok(builder.body(Body::from_stream(stream))?)
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

fn render_credentials_file(creds: &BedrockCredentials) -> String {
    let mut out = String::new();
    out.push_str("[bedrock]\n");
    out.push_str(&format!("aws_access_key_id = {}\n", creds.access_key_id));
    out.push_str(&format!(
        "aws_secret_access_key = {}\n",
        creds.secret_access_key
    ));
    if let Some(token) = &creds.session_token {
        out.push_str(&format!("aws_session_token = {}\n", token));
    }
    if let Some(region) = &creds.region {
        out.push_str(&format!("region = {}\n", region));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creds_file_renders_minimal_profile() {
        let c = BedrockCredentials {
            access_key_id: "AKIA".into(),
            secret_access_key: "SECRET".into(),
            session_token: None,
            region: None,
        };
        let out = render_credentials_file(&c);
        assert!(out.contains("[bedrock]"));
        assert!(out.contains("aws_access_key_id = AKIA"));
        assert!(out.contains("aws_secret_access_key = SECRET"));
        assert!(!out.contains("aws_session_token"));
        assert!(!out.contains("region"));
    }

    #[test]
    fn creds_file_includes_optional_fields() {
        let c = BedrockCredentials {
            access_key_id: "AKIA".into(),
            secret_access_key: "SECRET".into(),
            session_token: Some("TOKEN".into()),
            region: Some("us-west-2".into()),
        };
        let out = render_credentials_file(&c);
        assert!(out.contains("aws_session_token = TOKEN"));
        assert!(out.contains("region = us-west-2"));
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
}
