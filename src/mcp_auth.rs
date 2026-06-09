use anyhow::{Context, Result, bail};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::mcp::HttpMcpServer;
use crate::oauth::McpOAuthEntry;

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    authorization_servers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClientRegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

pub async fn authenticate(server: &HttpMcpServer) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let prm = discover_protected_resource(&http, &server.url).await?;
    let as_url = prm
        .authorization_servers
        .first()
        .cloned()
        .context("protected resource metadata has no authorization_servers")?;
    let metadata = discover_authorization_server(&http, &as_url).await?;

    let callback = CallbackListener::bind().await?;
    let redirect_uri = callback.redirect_uri();
    let client = register_client(&http, &metadata, &redirect_uri).await?;
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = random_urlsafe(32);

    let auth_url = authorization_url(
        &metadata.authorization_endpoint,
        &client.client_id,
        &redirect_uri,
        &challenge,
        &state,
        &server.url,
    )?;

    eprintln!(
        "[agent-container] Open this URL to authenticate MCP server '{}':",
        server.name
    );
    eprintln!("{auth_url}");
    open_browser(&auth_url);

    let code = callback.wait_for_code(&state).await?;
    let token = exchange_code(
        &http,
        &metadata.token_endpoint,
        &client,
        &redirect_uri,
        &verifier,
        &code,
        &server.url,
    )
    .await?;

    let entry = McpOAuthEntry {
        server_name: server.name.clone(),
        server_url: Some(server.url.clone()),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at_ms: token
            .expires_in
            .map(|seconds| crate::oauth::now_ms() + seconds * 1000),
        client_id: Some(client.client_id),
        authorization_server_url: Some(as_url),
        scope: token.scope,
    };
    crate::oauth::save_to_keychain(&entry).context("failed to save MCP OAuth token")?;
    eprintln!(
        "[agent-container] MCP OAuth token saved for server '{}'.",
        server.name
    );
    Ok(())
}

async fn discover_protected_resource(
    http: &reqwest::Client,
    server_url: &str,
) -> Result<ProtectedResourceMetadata> {
    let metadata_url = protected_resource_metadata_url(server_url)?;
    let resp = http
        .get(metadata_url.clone())
        .send()
        .await
        .with_context(|| format!("GET {metadata_url} failed"))?;
    if !resp.status().is_success() {
        bail!(
            "protected resource metadata request failed for {metadata_url}: {}",
            resp.status()
        );
    }
    resp.json()
        .await
        .with_context(|| format!("failed to parse protected resource metadata from {metadata_url}"))
}

fn protected_resource_metadata_url(server_url: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(server_url).context("MCP server URL is invalid")?;
    let mut path = url.path().trim_end_matches('/').to_string();
    path.push_str("/.well-known/oauth-protected-resource");
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

async fn discover_authorization_server(
    http: &reqwest::Client,
    authorization_server: &str,
) -> Result<AuthorizationServerMetadata> {
    let mut url =
        reqwest::Url::parse(authorization_server).context("authorization server URL is invalid")?;
    let mut path = url.path().trim_end_matches('/').to_string();
    path.push_str("/.well-known/oauth-authorization-server");
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);

    let resp = http
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?;
    if !resp.status().is_success() {
        bail!(
            "authorization server metadata request failed for {url}: {}",
            resp.status()
        );
    }
    resp.json()
        .await
        .with_context(|| format!("failed to parse authorization server metadata from {url}"))
}

async fn register_client(
    http: &reqwest::Client,
    metadata: &AuthorizationServerMetadata,
    redirect_uri: &str,
) -> Result<ClientRegistrationResponse> {
    let Some(endpoint) = &metadata.registration_endpoint else {
        bail!("authorization server does not advertise dynamic client registration");
    };
    let resp = http
        .post(endpoint)
        .json(&serde_json::json!({
            "client_name": "agent-container",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .await
        .with_context(|| format!("POST {endpoint} failed"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("dynamic client registration failed ({status}): {body}");
    }
    resp.json()
        .await
        .context("failed to parse dynamic client registration response")
}

fn authorization_url(
    endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
    resource: &str,
) -> Result<String> {
    let mut url = reqwest::Url::parse(endpoint).context("authorization endpoint is invalid")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("resource", resource);
    Ok(url.to_string())
}

async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    client: &ClientRegistrationResponse,
    redirect_uri: &str,
    verifier: &str,
    code: &str,
    resource: &str,
) -> Result<TokenResponse> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client.client_id.as_str()),
        ("code_verifier", verifier),
        ("resource", resource),
    ];
    if let Some(secret) = &client.client_secret {
        form.push(("client_secret", secret));
    }
    let resp = http
        .post(token_endpoint)
        .form(&form)
        .send()
        .await
        .with_context(|| format!("POST {token_endpoint} failed"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("token exchange failed ({status}): {body}");
    }
    resp.json().await.context("failed to parse token response")
}

struct CallbackListener {
    listener: tokio::net::TcpListener,
    redirect_uri: String,
}

impl CallbackListener {
    async fn bind() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("failed to bind OAuth callback listener")?;
        let addr = listener
            .local_addr()
            .context("failed to read callback addr")?;
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{}/callback", addr.port()),
        })
    }

    fn redirect_uri(&self) -> String {
        self.redirect_uri.clone()
    }

    async fn wait_for_code(self, expected_state: &str) -> Result<String> {
        let (mut stream, _) = self
            .listener
            .accept()
            .await
            .context("failed to accept OAuth callback")?;
        let mut buf = vec![0; 8192];
        let n = stream
            .read(&mut buf)
            .await
            .context("failed to read OAuth callback")?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let first_line = request.lines().next().context("empty OAuth callback")?;
        let target = first_line
            .split_whitespace()
            .nth(1)
            .context("malformed OAuth callback request")?;
        let parsed = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
            .context("failed to parse OAuth callback URL")?;
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                _ => {}
            }
        }

        let (status, body) = if error.is_some() {
            (
                "400 Bad Request",
                "MCP authentication failed. Return to the terminal.",
            )
        } else {
            (
                "200 OK",
                "MCP authentication complete. Return to the terminal.",
            )
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;

        if let Some(error) = error {
            bail!("authorization server returned error: {error}");
        }
        if state.as_deref() != Some(expected_state) {
            bail!("OAuth callback state did not match");
        }
        code.context("OAuth callback did not include code")
    }
}

fn pkce_verifier() -> String {
    random_urlsafe(64)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn random_urlsafe(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut raw);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

fn open_browser(url: &str) {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    if status.as_ref().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("[agent-container] Could not open a browser automatically.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_resource_metadata_preserves_mcp_path() {
        assert_eq!(
            protected_resource_metadata_url("https://mcp.notion.com/mcp").unwrap(),
            "https://mcp.notion.com/mcp/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn auth_url_includes_pkce_and_resource() {
        let url = authorization_url(
            "https://auth.example/authorize",
            "client",
            "http://127.0.0.1:123/callback",
            "challenge",
            "state",
            "https://mcp.example/mcp",
        )
        .unwrap();
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("resource=https%3A%2F%2Fmcp.example%2Fmcp"));
    }
}
