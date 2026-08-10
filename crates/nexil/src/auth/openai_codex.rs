//! Authentication helpers for OpenAI Codex OAuth flows.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::auth::APIKeyResolver;

/// Providers handled by the Codex OAuth resolver.
const CODEX_PROVIDERS: &[&str] = &["openai"];

/// Default Codex OAuth client ID (aligned with codex-rs).
pub const DEFAULT_CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Default token endpoint.
pub const DEFAULT_CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Default authorize endpoint.
pub const DEFAULT_CODEX_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// Default OAuth scope.
pub const DEFAULT_CODEX_OAUTH_SCOPE: &str = "openid profile email offline_access";
/// Default originator tag.
pub const DEFAULT_CODEX_OAUTH_ORIGINATOR: &str = "codex_cli_rs";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during Codex OAuth login.
#[derive(Debug, thiserror::Error)]
pub enum CodexOAuthLoginError {
    #[error("OAuth state mismatch")]
    StateMismatch,
    #[error("OAuth redirect did not include authorization code")]
    MissingCode,
    #[error("OAuth token response is malformed: {0}")]
    ResponseError(String),
    #[error("OAuth callback not received: {0}")]
    CallbackError(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// Token struct
// ---------------------------------------------------------------------------

/// Persisted OAuth tokens for the OpenAI Codex flow.
#[derive(Clone, Serialize, Deserialize)]
pub struct OpenAICodexOAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl std::fmt::Debug for OpenAICodexOAuthTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAICodexOAuthTokens")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn resolve_codex_auth_path(codex_home: Option<&Path>) -> PathBuf {
    let base = match codex_home {
        Some(p) => p.to_path_buf(),
        None => {
            if let Ok(val) = std::env::var("CODEX_HOME") {
                PathBuf::from(val)
            } else {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".codex")
            }
        }
    };
    base.join("auth.json")
}

// ---------------------------------------------------------------------------
// Load / Save
// ---------------------------------------------------------------------------

/// Load persisted Codex OAuth tokens from disk.
pub fn load_openai_codex_oauth_tokens(codex_home: Option<&Path>) -> Option<OpenAICodexOAuthTokens> {
    let path = resolve_codex_auth_path(codex_home);
    let contents = fs::read_to_string(&path).ok()?;
    let payload: Value = serde_json::from_str(&contents).ok()?;
    let obj = payload.as_object()?;
    parse_tokens(obj)
}

fn parse_tokens(payload: &serde_json::Map<String, Value>) -> Option<OpenAICodexOAuthTokens> {
    let tokens = payload.get("tokens")?.as_object()?;

    let access_token = tokens.get("access_token")?.as_str()?.trim().to_string();
    let refresh_token = tokens.get("refresh_token")?.as_str()?.trim().to_string();
    if access_token.is_empty() || refresh_token.is_empty() {
        return None;
    }

    let expires_at = tokens
        .get("expires_at")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or_else(|| {
            let last_refresh = payload
                .get("last_refresh")
                .and_then(|v| v.as_i64())
                .unwrap_or_else(|| Utc::now().timestamp());
            last_refresh + 3600
        });

    let account_id = tokens
        .get("account_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    Some(OpenAICodexOAuthTokens {
        access_token,
        refresh_token,
        expires_at,
        account_id,
    })
}

/// Save Codex OAuth tokens to disk.
pub fn save_openai_codex_oauth_tokens(
    tokens: &OpenAICodexOAuthTokens,
    codex_home: Option<&Path>,
) -> Result<PathBuf, CodexOAuthLoginError> {
    let path = resolve_codex_auth_path(codex_home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut payload: serde_json::Map<String, Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .and_then(|v: Value| v.as_object().cloned())
        .unwrap_or_default();

    let mut tokens_node = payload
        .get("tokens")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    tokens_node.insert(
        "access_token".to_string(),
        Value::String(tokens.access_token.clone()),
    );
    tokens_node.insert(
        "refresh_token".to_string(),
        Value::String(tokens.refresh_token.clone()),
    );
    tokens_node.insert(
        "expires_at".to_string(),
        Value::Number(serde_json::Number::from(tokens.expires_at)),
    );
    if let Some(ref account_id) = tokens.account_id {
        tokens_node.insert("account_id".to_string(), Value::String(account_id.clone()));
    }

    payload.insert("tokens".to_string(), Value::Object(tokens_node));
    payload.insert(
        "last_refresh".to_string(),
        Value::Number(serde_json::Number::from(Utc::now().timestamp())),
    );

    let json_str = serde_json::to_string_pretty(&payload)? + "\n";
    fs::write(&path, &json_str)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }

    Ok(path)
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

/// Refresh Codex OAuth tokens using the refresh token.
pub async fn refresh_openai_codex_oauth_tokens(
    refresh_token: &str,
    client_id: &str,
    token_url: &str,
    timeout_seconds: f64,
) -> Result<OpenAICodexOAuthTokens, CodexOAuthLoginError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(timeout_seconds))
        .build()?;

    let resp = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?
        .error_for_status()?;

    let payload: Value = resp.json().await?;
    tokens_from_token_payload(&payload, None)
}

// ---------------------------------------------------------------------------
// JWT account extraction
// ---------------------------------------------------------------------------

/// Extract the OpenAI account ID from the JWT access token.
pub fn extract_openai_codex_account_id(access_token: &str) -> Option<String> {
    let parts: Vec<&str> = access_token.splitn(4, '.').collect();
    if parts.len() != 3 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let payload: Value = serde_json::from_slice(&decoded).ok()?;
    payload
        .get("https://api.openai.com/auth")?
        .as_object()?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Login (PKCE OAuth flow)
// ---------------------------------------------------------------------------

/// Run the OpenAI Codex OAuth login flow.
///
/// This opens a browser for the user to authorize, starts a local HTTP server to
/// receive the callback, exchanges the authorization code for tokens, and persists them.
#[allow(clippy::too_many_arguments)]
pub async fn login_openai_codex_oauth(
    codex_home: Option<&Path>,
    redirect_uri: &str,
    timeout_seconds: f64,
    client_id: &str,
    authorize_url: &str,
    token_url: &str,
    scope: &str,
    originator: &str,
    open_browser: bool,
) -> Result<OpenAICodexOAuthTokens, CodexOAuthLoginError> {
    use rand::Rng;

    // Build PKCE verifier
    let mut rng = rand::thread_rng();
    let verifier_bytes: Vec<u8> = (0..32).map(|_| rng.r#gen::<u8>()).collect();
    let verifier = URL_SAFE_NO_PAD.encode(&verifier_bytes);

    // Build state
    let state_bytes: Vec<u8> = (0..16).map(|_| rng.r#gen::<u8>()).collect();
    let state = hex::encode(&state_bytes);

    // Compute S256 code challenge
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    // Build authorize URL
    let params = [
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", scope),
        ("state", &state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", originator),
    ];
    let oauth_url = reqwest::Url::parse_with_params(authorize_url, &params)
        .map_err(|e| CodexOAuthLoginError::ResponseError(format!("Failed to build URL: {e}")))?;

    if open_browser {
        let _ = open::that(oauth_url.as_str());
    }

    // Wait for local callback
    let (code, returned_state) =
        wait_for_local_oauth_callback(redirect_uri, timeout_seconds).await?;

    if let Some(ref rs) = returned_state
        && rs != &state
    {
        return Err(CodexOAuthLoginError::StateMismatch);
    }

    let code = code.ok_or(CodexOAuthLoginError::MissingCode)?;
    if code.trim().is_empty() {
        return Err(CodexOAuthLoginError::MissingCode);
    }

    // Exchange code for tokens
    let tokens = exchange_authorization_code(
        code.trim(),
        &verifier,
        redirect_uri,
        timeout_seconds,
        client_id,
        token_url,
    )
    .await?;

    save_openai_codex_oauth_tokens(&tokens, codex_home)?;
    Ok(tokens)
}

async fn exchange_authorization_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    timeout_seconds: f64,
    client_id: &str,
    token_url: &str,
) -> Result<OpenAICodexOAuthTokens, CodexOAuthLoginError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(timeout_seconds))
        .build()?;

    let resp = client
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?
        .error_for_status()?;

    let payload: Value = resp.json().await?;
    let access = payload
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let account_id = extract_openai_codex_account_id(access);
    tokens_from_token_payload(&payload, account_id.as_deref())
}

async fn wait_for_local_oauth_callback(
    redirect_uri: &str,
    timeout_seconds: f64,
) -> Result<(Option<String>, Option<String>), CodexOAuthLoginError> {
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let parsed = url::Url::parse(redirect_uri)
        .map_err(|e| CodexOAuthLoginError::CallbackError(format!("Invalid redirect_uri: {e}")))?;
    let host = parsed.host_str().unwrap_or("127.0.0.1");
    let port = parsed
        .port()
        .ok_or_else(|| CodexOAuthLoginError::CallbackError("redirect_uri has no port".into()))?;
    let expected_path = parsed.path().to_string();

    let listener = TcpListener::bind(format!("{host}:{port}"))
        .await
        .map_err(|e| {
            CodexOAuthLoginError::CallbackError(format!("Cannot bind to {host}:{port}: {e}"))
        })?;

    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CodexOAuthLoginError::CallbackError(format!(
                "Did not receive OAuth callback within {timeout_seconds}s"
            )));
        }

        let accept = tokio::time::timeout(remaining, listener.accept()).await;
        let (mut stream, _) = match accept {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                return Err(CodexOAuthLoginError::CallbackError(format!(
                    "Accept error: {e}"
                )));
            }
            Err(_) => {
                return Err(CodexOAuthLoginError::CallbackError(format!(
                    "Did not receive OAuth callback within {timeout_seconds}s"
                )));
            }
        };

        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);

        // Parse GET line
        let first_line = request.lines().next().unwrap_or("");
        let path_and_query = first_line.split_whitespace().nth(1).unwrap_or("/");

        let req_url =
            url::Url::parse(&format!("http://localhost{path_and_query}")).unwrap_or_else(|_| {
                url::Url::parse("http://localhost/")
                    .expect("SAFETY: static literal URL is always valid")
            });

        if req_url.path() != expected_path {
            let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(resp).await;
            continue;
        }

        let code = req_url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string());
        let state = req_url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.to_string());

        let body = b"<!doctype html><html><body><p>Authentication successful. Return to your terminal.</p></body></html>";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.write_all(body).await;

        return Ok((code, state));
    }
}

fn tokens_from_token_payload(
    payload: &Value,
    account_id: Option<&str>,
) -> Result<OpenAICodexOAuthTokens, CodexOAuthLoginError> {
    let access_token = payload
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CodexOAuthLoginError::ResponseError("missing access_token".into()))?
        .trim()
        .to_string();

    let refresh_token = payload
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CodexOAuthLoginError::ResponseError("missing refresh_token".into()))?
        .trim()
        .to_string();

    let expires_in = payload
        .get("expires_in")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| CodexOAuthLoginError::ResponseError("missing expires_in".into()))?;

    let expires_at = Utc::now().timestamp() + expires_in as i64;

    let resolved_account_id = account_id
        .map(|s| s.to_string())
        .or_else(|| extract_openai_codex_account_id(&access_token));

    Ok(OpenAICodexOAuthTokens {
        access_token,
        refresh_token,
        expires_at,
        account_id: resolved_account_id,
    })
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Build a simple resolver that reads the Codex CLI OAuth token from disk.
///
/// Only returns a token for the `"openai"` provider.
pub fn codex_cli_api_key_resolver(codex_home: Option<PathBuf>) -> APIKeyResolver {
    Box::new(move |provider: &str| -> Option<String> {
        if !CODEX_PROVIDERS.contains(&provider) {
            return None;
        }
        let tokens = load_openai_codex_oauth_tokens(codex_home.as_deref())?;
        let token = tokens.access_token.trim().to_string();
        if token.is_empty() { None } else { Some(token) }
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn make_tokens(
        access: &str,
        refresh: &str,
        expires_at: i64,
        account_id: Option<&str>,
    ) -> OpenAICodexOAuthTokens {
        OpenAICodexOAuthTokens {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_at,
            account_id: account_id.map(|s| s.to_string()),
        }
    }

    // ----- Token save/load round-trip -----

    #[test]
    fn test_save_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let tokens = make_tokens("access123", "refresh456", 9999999999, Some("acct_abc"));

        let path = save_openai_codex_oauth_tokens(&tokens, Some(tmp.path())).unwrap();
        assert!(path.exists());

        let loaded = load_openai_codex_oauth_tokens(Some(tmp.path())).unwrap();
        assert_eq!(loaded.access_token, "access123");
        assert_eq!(loaded.refresh_token, "refresh456");
        assert_eq!(loaded.expires_at, 9999999999);
        assert_eq!(loaded.account_id.as_deref(), Some("acct_abc"));
    }

    #[test]
    fn test_save_load_round_trip_without_account_id() {
        let tmp = TempDir::new().unwrap();
        let tokens = make_tokens("at", "rt", 1000, None);

        save_openai_codex_oauth_tokens(&tokens, Some(tmp.path())).unwrap();
        let loaded = load_openai_codex_oauth_tokens(Some(tmp.path())).unwrap();

        assert_eq!(loaded.access_token, "at");
        assert_eq!(loaded.refresh_token, "rt");
        assert_eq!(loaded.account_id, None);
    }

    #[test]
    fn test_load_returns_none_when_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(load_openai_codex_oauth_tokens(Some(tmp.path())).is_none());
    }

    #[test]
    fn test_load_returns_none_for_empty_tokens() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("auth.json");
        let content = json!({
            "tokens": {
                "access_token": "",
                "refresh_token": "rt",
                "expires_at": 1000,
            }
        });
        fs::write(&path, serde_json::to_string(&content).unwrap()).unwrap();

        assert!(load_openai_codex_oauth_tokens(Some(tmp.path())).is_none());
    }

    // ----- parse_tokens edge cases -----

    #[test]
    fn test_parse_tokens_falls_back_to_last_refresh_plus_1h() {
        let payload: serde_json::Map<String, serde_json::Value> = serde_json::from_value(json!({
            "tokens": {
                "access_token": "at",
                "refresh_token": "rt",
            },
            "last_refresh": 5000
        }))
        .unwrap();

        let result = parse_tokens(&payload).unwrap();
        assert_eq!(result.expires_at, 5000 + 3600);
    }

    #[test]
    fn test_parse_tokens_expires_at_as_float() {
        let payload: serde_json::Map<String, serde_json::Value> = serde_json::from_value(json!({
            "tokens": {
                "access_token": "at",
                "refresh_token": "rt",
                "expires_at": 1234.5,
            }
        }))
        .unwrap();

        let result = parse_tokens(&payload).unwrap();
        assert_eq!(result.expires_at, 1234);
    }

    // ----- PKCE S256 challenge -----

    #[test]
    fn test_pkce_s256_challenge_generation() {
        use sha2::{Digest, Sha256};

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

        // Verify the challenge is URL-safe base64 without padding
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.is_empty());
    }

    // ----- extract_openai_codex_account_id -----

    fn build_jwt_with_account_id(account_id: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{}");
        let payload = json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
            }
        });
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let sig = URL_SAFE_NO_PAD.encode(b"signature");
        format!("{header}.{payload_b64}.{sig}")
    }

    #[test]
    fn test_extract_account_id_valid() {
        let token = build_jwt_with_account_id("acct_123");
        assert_eq!(
            extract_openai_codex_account_id(&token),
            Some("acct_123".to_string())
        );
    }

    #[test]
    fn test_extract_account_id_empty_string() {
        let token = build_jwt_with_account_id("");
        assert_eq!(extract_openai_codex_account_id(&token), None);
    }

    #[test]
    fn test_extract_account_id_whitespace_only() {
        let token = build_jwt_with_account_id("   ");
        assert_eq!(extract_openai_codex_account_id(&token), None);
    }

    #[test]
    fn test_extract_account_id_not_a_jwt() {
        assert_eq!(extract_openai_codex_account_id("not-a-jwt"), None);
    }

    #[test]
    fn test_extract_account_id_missing_auth_claim() {
        let header = URL_SAFE_NO_PAD.encode(b"{}");
        let payload = URL_SAFE_NO_PAD.encode(b"{}");
        let sig = URL_SAFE_NO_PAD.encode(b"sig");
        let token = format!("{header}.{payload}.{sig}");
        assert_eq!(extract_openai_codex_account_id(&token), None);
    }

    // ----- codex_cli_api_key_resolver -----

    #[test]
    fn test_codex_cli_api_key_resolver_returns_token_for_openai() {
        let tmp = TempDir::new().unwrap();
        let tokens = make_tokens("my-api-key", "rt", 9999999999, None);
        save_openai_codex_oauth_tokens(&tokens, Some(tmp.path())).unwrap();

        let resolver = codex_cli_api_key_resolver(Some(tmp.path().to_path_buf()));
        assert_eq!(resolver("openai"), Some("my-api-key".to_string()));
    }

    #[test]
    fn test_codex_cli_api_key_resolver_returns_none_for_non_openai() {
        let tmp = TempDir::new().unwrap();
        let tokens = make_tokens("key", "rt", 9999999999, None);
        save_openai_codex_oauth_tokens(&tokens, Some(tmp.path())).unwrap();

        let resolver = codex_cli_api_key_resolver(Some(tmp.path().to_path_buf()));
        assert_eq!(resolver("anthropic"), None);
        assert_eq!(resolver("github-copilot"), None);
    }

    #[test]
    fn test_codex_cli_api_key_resolver_returns_none_when_no_file() {
        let tmp = TempDir::new().unwrap();
        let resolver = codex_cli_api_key_resolver(Some(tmp.path().to_path_buf()));
        assert_eq!(resolver("openai"), None);
    }

    // ----- tokens_from_token_payload -----

    #[test]
    fn test_tokens_from_token_payload_success() {
        let payload = json!({
            "access_token": "at123",
            "refresh_token": "rt456",
            "expires_in": 3600.0,
        });
        let result = tokens_from_token_payload(&payload, Some("acct_x")).unwrap();
        assert_eq!(result.access_token, "at123");
        assert_eq!(result.refresh_token, "rt456");
        assert_eq!(result.account_id.as_deref(), Some("acct_x"));
        // expires_at should be approximately now + 3600
        let now = Utc::now().timestamp();
        assert!((result.expires_at - now - 3600).abs() <= 2);
    }

    #[test]
    fn test_tokens_from_token_payload_missing_access_token() {
        let payload = json!({
            "refresh_token": "rt",
            "expires_in": 3600.0,
        });
        assert!(tokens_from_token_payload(&payload, None).is_err());
    }

    #[test]
    fn test_tokens_from_token_payload_missing_refresh_token() {
        let payload = json!({
            "access_token": "at",
            "expires_in": 3600.0,
        });
        assert!(tokens_from_token_payload(&payload, None).is_err());
    }

    // ----- save overwrites existing file -----

    #[test]
    fn test_save_preserves_extra_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("auth.json");

        // Write initial data with an extra field
        let initial = json!({
            "tokens": {
                "access_token": "old",
                "refresh_token": "old_rt",
                "expires_at": 1000,
            },
            "custom_field": "preserved"
        });
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(&path, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

        // Save new tokens
        let tokens = make_tokens("new", "new_rt", 2000, None);
        save_openai_codex_oauth_tokens(&tokens, Some(tmp.path())).unwrap();

        // Verify custom field is preserved
        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(content["custom_field"].as_str(), Some("preserved"));
        assert_eq!(content["tokens"]["access_token"].as_str(), Some("new"));
    }

    // ----- resolve_codex_auth_path -----

    #[test]
    fn test_resolve_codex_auth_path_with_explicit_path() {
        let result = resolve_codex_auth_path(Some(Path::new("/custom/home")));
        assert_eq!(result, PathBuf::from("/custom/home/auth.json"));
    }
}
