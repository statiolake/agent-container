//! Claude Code's Keychain-managed MCP OAuth tokens.
//!
//! For HTTP MCP servers that use OAuth 2.1 (like the built-in Notion,
//! Google Calendar, Google Drive, Gmail integrations) Claude Code stores
//! the full OAuth state — access token, refresh token, expiry, client id,
//! authorization server URL — inside the `mcpOAuth` field of the
//! `Claude Code-credentials` Keychain item. This module reads that map
//! and exposes it per-server so the broker and the TUI can inject a
//! fresh `Authorization: Bearer <access_token>` before forwarding.
//!
//! A shared `OAuthStore` owns the live entries behind per-server
//! tokio mutexes. `access_token()` hands back a valid bearer, refreshing
//! via `grant_type=refresh_token` when the cached copy is within 30s of
//! its expiry. Refreshed tokens stay in memory — we never write back to
//! the Keychain so Claude Code's own bookkeeping keeps owning the
//! canonical copy.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct McpOAuthEntry {
    pub server_name: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub client_id: Option<String>,
    pub authorization_server_url: Option<String>,
    pub scope: Option<String>,
}

impl McpOAuthEntry {
    /// Consider a token about-to-expire as expired. Gives the refresh
    /// path some slack so we don't race with an in-flight request.
    pub fn is_expiring_soon(&self) -> bool {
        let Some(exp) = self.expires_at_ms else {
            return false;
        };
        now_ms() + 30_000 >= exp
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default, rename = "serverName")]
    server_name: Option<String>,
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
    #[serde(default, rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(default, rename = "clientId")]
    client_id: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default, rename = "discoveryState")]
    discovery_state: Option<DiscoveryState>,
}

#[derive(Deserialize)]
struct DiscoveryState {
    #[serde(default, rename = "authorizationServerUrl")]
    authorization_server_url: Option<String>,
}

/// Load every `mcpOAuth` entry from the host Keychain and key them by
/// their MCP server name (the part of the Keychain key before `|`).
///
/// Returns an empty map when the Keychain entry or the field is absent,
/// so callers can treat "no OAuth managed servers" and "no keychain" the
/// same way.
pub fn load_from_keychain() -> Result<HashMap<String, McpOAuthEntry>> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-w", "-s", "Claude Code-credentials"])
            .output()
            .context("failed to invoke `security` command")?;
        if !output.status.success() {
            return Ok(HashMap::new());
        }
        let raw = String::from_utf8(output.stdout)
            .context("keychain entry was not valid UTF-8")?;
        parse_raw_credentials(raw.trim())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(HashMap::new())
    }
}

fn parse_raw_credentials(raw: &str) -> Result<HashMap<String, McpOAuthEntry>> {
    let cfg: Value = serde_json::from_str(raw).context("keychain JSON parse")?;
    let Some(map) = cfg.get("mcpOAuth").and_then(Value::as_object) else {
        return Ok(HashMap::new());
    };
    let mut out = HashMap::new();
    for (key, value) in map {
        match parse_entry(key, value) {
            Ok(entry) => {
                out.insert(entry.server_name.clone(), entry);
            }
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "skipping malformed mcpOAuth entry");
            }
        }
    }
    Ok(out)
}

fn parse_entry(keychain_key: &str, value: &Value) -> Result<McpOAuthEntry> {
    let raw: RawEntry =
        serde_json::from_value(value.clone()).context("entry is not a valid OAuth record")?;

    // `serverName` inside the entry is authoritative; fall back to the
    // prefix before `|` in the keychain key.
    let server_name = raw
        .server_name
        .clone()
        .or_else(|| keychain_key.split_once('|').map(|(n, _)| n.to_string()))
        .unwrap_or_else(|| keychain_key.to_string());

    let Some(access_token) = raw.access_token else {
        bail!("mcpOAuth entry '{keychain_key}' has no accessToken");
    };

    Ok(McpOAuthEntry {
        server_name,
        access_token,
        refresh_token: raw.refresh_token,
        expires_at_ms: raw.expires_at,
        client_id: raw.client_id,
        authorization_server_url: raw
            .discovery_state
            .and_then(|d| d.authorization_server_url),
        scope: raw.scope,
    })
}

/// Live-refresh wrapper over the Keychain snapshot.
pub struct OAuthStore {
    entries: HashMap<String, Arc<Mutex<McpOAuthEntry>>>,
    http: reqwest::Client,
}

impl OAuthStore {
    pub fn new(entries: HashMap<String, McpOAuthEntry>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("build reqwest client");
        let entries = entries
            .into_iter()
            .map(|(k, v)| (k, Arc::new(Mutex::new(v))))
            .collect();
        Self { entries, http }
    }

    /// Return a currently-valid access token for an MCP server, refreshing
    /// silently when it's about to expire. `Ok(None)` means the server
    /// has no OAuth record and the caller should fall back to static
    /// headers from `.claude.json`.
    pub async fn access_token(&self, server: &str) -> Result<Option<String>> {
        let Some(slot) = self.entries.get(server) else {
            return Ok(None);
        };
        let mut guard = slot.lock().await;
        if guard.is_expiring_soon() {
            refresh_entry(&mut guard, &self.http)
                .await
                .with_context(|| format!("failed to refresh OAuth token for '{server}'"))?;
        }
        Ok(Some(guard.access_token.clone()))
    }
}

async fn refresh_entry(
    entry: &mut McpOAuthEntry,
    http: &reqwest::Client,
) -> Result<()> {
    let refresh_token = entry
        .refresh_token
        .clone()
        .context("entry has no refresh_token; re-run Claude Code's OAuth flow")?;
    let as_url = entry
        .authorization_server_url
        .clone()
        .context("entry has no authorizationServerUrl")?;
    let client_id = entry
        .client_id
        .clone()
        .context("entry has no clientId")?;

    let token_endpoint = discover_token_endpoint(http, &as_url).await?;

    let form: [(&str, &str); 3] = [
        ("grant_type", "refresh_token"),
        ("refresh_token", &refresh_token),
        ("client_id", &client_id),
    ];
    let resp = http
        .post(&token_endpoint)
        .form(&form)
        .send()
        .await
        .with_context(|| format!("POST {token_endpoint} failed"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("token refresh failed ({status}): {body}");
    }
    let parsed: RefreshResponse = resp
        .json()
        .await
        .context("parsing token refresh response")?;

    entry.access_token = parsed.access_token;
    if let Some(new_refresh) = parsed.refresh_token {
        entry.refresh_token = Some(new_refresh);
    }
    if let Some(expires_in) = parsed.expires_in {
        entry.expires_at_ms = Some(now_ms() + expires_in * 1000);
    }
    if let Some(scope) = parsed.scope {
        entry.scope = Some(scope);
    }
    Ok(())
}

async fn discover_token_endpoint(http: &reqwest::Client, as_url: &str) -> Result<String> {
    let base = as_url.trim_end_matches('/');

    for suffix in &[
        "/.well-known/oauth-authorization-server",
        "/.well-known/openid-configuration",
    ] {
        let url = format!("{base}{suffix}");
        let Ok(resp) = http.get(&url).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(meta) = resp.json::<Value>().await else {
            continue;
        };
        if let Some(ep) = meta.get("token_endpoint").and_then(Value::as_str) {
            return Ok(ep.to_string());
        }
    }

    // Conservative fall-back: some issuers advertise their token endpoint
    // only via an un-documented path, so `/token` is the least surprising
    // guess once discovery has failed.
    Ok(format!("{base}/token"))
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mcp_oauth_map_and_keys_by_server_name() {
        let raw = r#"{
          "claudeAiOauth": {"accessToken": "ignored"},
          "mcpOAuth": {
            "notion|abcd1234": {
              "serverName": "notion",
              "serverUrl": "https://mcp.notion.com/mcp",
              "accessToken": "ntn-XYZ",
              "refreshToken": "ntn-REFRESH",
              "expiresAt": 1776600000000,
              "clientId": "client-123",
              "scope": "read write",
              "discoveryState": {
                "authorizationServerUrl": "https://api.notion.com/v1/oauth",
                "oauthMetadataFound": true
              }
            },
            "gdrive|99": {
              "serverName": "google-drive",
              "accessToken": "gdrive-token"
            }
          }
        }"#;
        let out = parse_raw_credentials(raw).unwrap();
        let notion = out.get("notion").unwrap();
        assert_eq!(notion.access_token, "ntn-XYZ");
        assert_eq!(notion.refresh_token.as_deref(), Some("ntn-REFRESH"));
        assert_eq!(notion.expires_at_ms, Some(1776600000000));
        assert_eq!(notion.client_id.as_deref(), Some("client-123"));
        assert_eq!(
            notion.authorization_server_url.as_deref(),
            Some("https://api.notion.com/v1/oauth")
        );
        assert!(out.contains_key("google-drive"));
    }

    #[test]
    fn empty_when_no_mcp_oauth_field() {
        let raw = r#"{"claudeAiOauth": {"accessToken": "x"}}"#;
        assert!(parse_raw_credentials(raw).unwrap().is_empty());
    }

    #[test]
    fn server_name_falls_back_to_key_prefix() {
        let raw = r#"{
          "mcpOAuth": {
            "weather|beef": {"accessToken": "t"}
          }
        }"#;
        let out = parse_raw_credentials(raw).unwrap();
        assert!(out.contains_key("weather"));
        assert_eq!(out["weather"].access_token, "t");
    }

    #[test]
    fn entries_without_access_token_are_skipped() {
        let raw = r#"{
          "mcpOAuth": {
            "broken|0": {"serverName": "broken"}
          }
        }"#;
        let out = parse_raw_credentials(raw).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn is_expiring_soon_behaves_near_deadline() {
        let mut e = McpOAuthEntry {
            server_name: "n".into(),
            access_token: "a".into(),
            refresh_token: None,
            expires_at_ms: Some(now_ms() + 1_000_000),
            client_id: None,
            authorization_server_url: None,
            scope: None,
        };
        assert!(!e.is_expiring_soon());

        e.expires_at_ms = Some(now_ms() - 1);
        assert!(e.is_expiring_soon());

        e.expires_at_ms = Some(now_ms() + 5_000);
        assert!(e.is_expiring_soon());

        e.expires_at_ms = None;
        assert!(!e.is_expiring_soon());
    }
}
