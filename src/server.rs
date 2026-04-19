//! Host-side broker HTTP server. The container hits it through the
//! forward-proxy sidecar and gets back fresh AWS credentials (or, in
//! future, MCP traffic) on demand, so long-running sessions keep working
//! after the host's SSO session would otherwise expire.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::aws::{BedrockCredentials, BedrockSetup, resolve_credentials};

struct BrokerState {
    bedrock: Option<(BedrockSetup, Option<String>)>,
    last_error: Mutex<Option<String>>,
}

pub struct RunningServer {
    pub addr: SocketAddr,
    pub handle: tokio::task::JoinHandle<()>,
}

pub async fn spawn(
    bedrock: Option<(BedrockSetup, Option<String>)>,
) -> Result<RunningServer> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind broker listener")?;
    let addr = listener.local_addr()?;

    let state = Arc::new(BrokerState {
        bedrock,
        last_error: Mutex::new(None),
    });

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/aws/credentials", get(handle_aws))
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
        return (
            StatusCode::NOT_FOUND,
            "Bedrock not configured on the host",
        )
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
}
