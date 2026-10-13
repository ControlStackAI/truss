//! Codex subscription credentials — read (and refresh) `~/.codex/auth.json`.
//!
//! This is the auth source for the in-process Codex *subscription* provider
//! ([`crate::openai_codex`], ADR 0003 Tier 1). The `codex` CLI stores its
//! ChatGPT login here after "Sign in with ChatGPT"; we read the OAuth
//! `access_token` + `account_id` and call the ChatGPT backend as that user.
//!
//! Field names and the refresh request are taken verbatim from `openai/codex`
//! (`codex-rs/login/src/{token_data.rs,auth/manager.rs,auth/storage.rs}`):
//! `tokens.{access_token, refresh_token, account_id}`; refresh is
//! `POST https://auth.openai.com/oauth/token` with
//! `{ client_id, grant_type: "refresh_token", refresh_token }`.
//!
//! We hold the **whole file** as a JSON value and update only the token fields on
//! refresh, so unknown fields the CLI cares about are preserved byte-for-byte.

use std::path::PathBuf;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use truss_core::ctx::Ctx;
use truss_core::error::ProviderError;

/// `openai/codex` login client id (`codex-rs/login/src/auth/manager.rs`).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Refresh endpoint (`REFRESH_TOKEN_URL`, same file).
const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";

struct Inner {
    /// The entire auth.json, preserved so refresh writes don't drop CLI fields.
    raw: Value,
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

/// A handle to the Codex login, refreshable in place.
pub struct CodexAuth {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
    client: reqwest::Client,
}

impl CodexAuth {
    /// Resolve `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) and load it.
    pub fn load() -> Result<Self, String> {
        let path = Self::auth_path()?;
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "no Codex login at {} ({e}). Run `codex` and sign in with ChatGPT.",
                path.display()
            )
        })?;
        let raw: Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
        let inner = Self::extract(raw, &path.display().to_string())?;
        Ok(Self {
            path: Some(path),
            inner: Mutex::new(inner),
            client: reqwest::Client::new(),
        })
    }

    /// `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`.
    fn auth_path() -> Result<PathBuf, String> {
        if let Ok(home) = std::env::var("CODEX_HOME") {
            return Ok(PathBuf::from(home).join("auth.json"));
        }
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home).join(".codex").join("auth.json"))
    }

    fn extract(raw: Value, src: &str) -> Result<Inner, String> {
        let tokens = &raw["tokens"];
        let access_token = tokens["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{src}: tokens.access_token missing (not a ChatGPT login?)"))?
            .to_string();
        let refresh_token = tokens["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let account_id = tokens["account_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Ok(Inner {
            raw,
            access_token,
            refresh_token,
            account_id,
        })
    }

    /// Current bearer token + account id (a snapshot; cheap to call per request).
    pub async fn current(&self) -> (String, Option<String>) {
        let g = self.inner.lock().await;
        (g.access_token.clone(), g.account_id.clone())
    }

    /// Exchange the refresh token for a fresh access token and persist it. Called
    /// reactively on a 401. No-op error if there's no refresh token.
    pub async fn refresh(&self, ctx: &Ctx) -> Result<(), ProviderError> {
        let refresh_token = {
            let g = self.inner.lock().await;
            g.refresh_token.clone()
        };
        let refresh_token = refresh_token.ok_or_else(|| ProviderError::Status {
            status: 401,
            body: "Codex access token rejected and no refresh_token to renew it".into(),
        })?;

        let body = json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        });
        let rb = self
            .client
            .post(REFRESH_URL)
            .header("content-type", "application/json")
            .json(&body);
        let resp = super::send(ctx, rb).await?;
        let v = super::read_json(resp).await?;

        let mut g = self.inner.lock().await;
        if let Some(at) = v["access_token"].as_str() {
            g.access_token = at.to_string();
            g.raw["tokens"]["access_token"] = json!(at);
        }
        if let Some(rt) = v["refresh_token"].as_str() {
            g.refresh_token = Some(rt.to_string());
            g.raw["tokens"]["refresh_token"] = json!(rt);
        }
        if let Some(it) = v["id_token"].as_str() {
            g.raw["tokens"]["id_token"] = json!(it);
        }
        // Best-effort persist so the CLI and future runs see the rotated token.
        if let Some(path) = &self.path {
            if let Ok(text) = serde_json::to_string_pretty(&g.raw) {
                let _ = std::fs::write(path, text);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl CodexAuth {
    /// Build from an in-memory auth.json value (no disk). For tests.
    pub(crate) fn from_value(raw: Value) -> Result<Self, String> {
        let inner = Self::extract(raw, "<test>")?;
        Ok(Self {
            path: None,
            inner: Mutex::new(inner),
            client: reqwest::Client::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "eyJ.fake.jwt",
                "access_token": "sk-access-abc",
                "refresh_token": "rt-xyz",
                "account_id": "acct-123"
            },
            "last_refresh": "2026-06-16T00:00:00Z"
        })
    }

    #[tokio::test]
    async fn parses_tokens_and_account() {
        let auth = CodexAuth::from_value(fixture()).unwrap();
        let (token, account) = auth.current().await;
        assert_eq!(token, "sk-access-abc");
        assert_eq!(account.as_deref(), Some("acct-123"));
    }

    #[test]
    fn missing_access_token_is_an_error() {
        let mut v = fixture();
        v["tokens"]["access_token"] = json!("");
        // CodexAuth isn't Debug (holds tokens), so match rather than unwrap_err.
        let err = match CodexAuth::from_value(v) {
            Ok(_) => panic!("expected an error for an empty access_token"),
            Err(e) => e,
        };
        assert!(err.contains("access_token"), "got: {err}");
    }

    #[test]
    fn account_id_is_optional() {
        let mut v = fixture();
        v["tokens"]["account_id"] = Value::Null;
        // Should still load (account header is just omitted downstream).
        assert!(CodexAuth::from_value(v).is_ok());
    }
}
