use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
use crate::shared_cred::{HostSync, SharedCredFile, shared_dir};

#[cfg(not(target_os = "macos"))]
const DOMAIN: &str = "cursor";
#[cfg(target_os = "macos")]
const ACCOUNT: &str = "cursor-user";
#[cfg(target_os = "macos")]
const ACCESS_TOKEN_SERVICE: &str = "cursor-access-token";
#[cfg(target_os = "macos")]
const REFRESH_TOKEN_SERVICE: &str = "cursor-refresh-token";
#[cfg(target_os = "macos")]
const API_KEY_SERVICE: &str = "cursor-api-key";

#[derive(Debug, Clone)]
pub struct CursorStateDir {
    pub path: PathBuf,
}

pub struct CursorAuthFile {
    pub path: PathBuf,
    pub host_auth_found: bool,
    #[cfg(target_os = "macos")]
    _shared: Option<SharedCredFile>,
}

pub struct CursorFiles {
    pub state_dir: CursorStateDir,
    pub auth_file: CursorAuthFile,
}

pub fn prepare(host_home: &Path) -> Result<CursorFiles> {
    Ok(CursorFiles {
        state_dir: prepare_state(host_home)?,
        auth_file: prepare_auth(host_home)?,
    })
}

pub fn prepare_state(host_home: &Path) -> Result<CursorStateDir> {
    let path = host_home.join(".cursor");
    std::fs::create_dir_all(&path)
        .with_context(|| format!("failed to prepare Cursor state dir {}", path.display()))?;
    Ok(CursorStateDir { path })
}

pub fn prepare_auth(_host_home: &Path) -> Result<CursorAuthFile> {
    #[cfg(target_os = "macos")]
    {
        let shared_path = shared_dir()?.join("cursor-auth.json");
        let (shared, raw) = SharedCredFile::open(
            shared_path,
            HostSync::CursorKeychain {
                account: ACCOUNT.to_string(),
                access_token_service: ACCESS_TOKEN_SERVICE.to_string(),
                refresh_token_service: REFRESH_TOKEN_SERVICE.to_string(),
                api_key_service: API_KEY_SERVICE.to_string(),
            },
            read_keychain_auth_json,
        )?;
        let host_auth_found = auth_json_has_secret(&raw);
        Ok(CursorAuthFile {
            path: shared.path.clone(),
            host_auth_found,
            _shared: Some(shared),
        })
    }

    #[cfg(not(target_os = "macos"))]
    {
        let path = file_auth_path(_host_home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if !path.exists() {
            std::fs::write(&path, "{}")
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let path = std::fs::canonicalize(&path)
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        Ok(CursorAuthFile {
            path,
            host_auth_found: auth_json_has_secret(&raw),
        })
    }
}

pub fn has_host_cli_config(host_home: &Path) -> bool {
    host_home.join(".cursor/cli-config.json").is_file()
}

pub fn has_portable_auth_env() -> bool {
    std::env::var_os("CURSOR_API_KEY").is_some() || std::env::var_os("CURSOR_AUTH_TOKEN").is_some()
}

#[cfg(target_os = "macos")]
fn read_keychain_auth_json() -> Result<String> {
    let auth = CursorAuthJson {
        access_token: read_cursor_secret(ACCESS_TOKEN_SERVICE)?,
        refresh_token: read_cursor_secret(REFRESH_TOKEN_SERVICE)?,
        api_key: read_cursor_secret(API_KEY_SERVICE)?,
    };
    serde_json::to_string_pretty(&auth).context("failed to serialise Cursor auth JSON")
}

#[cfg(target_os = "macos")]
fn read_cursor_secret(service: &str) -> Result<Option<String>> {
    crate::keychain::read_generic_password_for_account(service, Some(ACCOUNT))
}

#[cfg(target_os = "macos")]
pub fn write_keychain_auth(
    account: &str,
    access_token_service: &str,
    refresh_token_service: &str,
    api_key_service: &str,
    raw: &str,
) -> Result<()> {
    let auth: CursorAuthJson =
        serde_json::from_str(raw.trim()).context("failed to parse Cursor auth JSON")?;
    if let Some(access_token) = auth.access_token.as_deref() {
        crate::keychain::write_generic_password(access_token_service, Some(account), access_token)?;
    }
    if let Some(refresh_token) = auth.refresh_token.as_deref() {
        crate::keychain::write_generic_password(
            refresh_token_service,
            Some(account),
            refresh_token,
        )?;
    }
    if let Some(api_key) = auth.api_key.as_deref() {
        crate::keychain::write_generic_password(api_key_service, Some(account), api_key)?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn file_auth_path(host_home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| host_home.join(".config"))
        .join(DOMAIN)
        .join("auth.json")
}

fn auth_json_has_secret(raw: &str) -> bool {
    serde_json::from_str::<CursorAuthJson>(raw.trim())
        .map(|auth| {
            auth.access_token.as_deref().is_some_and(|s| !s.is_empty())
                || auth.refresh_token.as_deref().is_some_and(|s| !s.is_empty())
                || auth.api_key.as_deref().is_some_and(|s| !s.is_empty())
        })
        .unwrap_or(false)
}

#[derive(Default, Deserialize, Serialize)]
struct CursorAuthJson {
    #[serde(
        default,
        rename = "accessToken",
        skip_serializing_if = "Option::is_none"
    )]
    access_token: Option<String>,
    #[serde(
        default,
        rename = "refreshToken",
        skip_serializing_if = "Option::is_none"
    )]
    refresh_token: Option<String>,
    #[serde(default, rename = "apiKey", skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_state_creates_cursor_dir() {
        let home = tempfile::tempdir().unwrap();
        let state = prepare_state(home.path()).unwrap();
        assert_eq!(state.path, home.path().join(".cursor"));
        assert!(state.path.is_dir());
    }

    #[test]
    fn host_cli_config_detects_cli_config() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".cursor")).unwrap();
        std::fs::write(home.path().join(".cursor/cli-config.json"), "{}").unwrap();
        assert!(has_host_cli_config(home.path()));
    }

    #[test]
    fn auth_json_detects_any_cursor_secret() {
        assert!(!auth_json_has_secret("{}"));
        assert!(auth_json_has_secret(r#"{"accessToken":"token"}"#));
        assert!(auth_json_has_secret(r#"{"refreshToken":"token"}"#));
        assert!(auth_json_has_secret(r#"{"apiKey":"key"}"#));
    }
}
