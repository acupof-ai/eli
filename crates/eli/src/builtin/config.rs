//! Eli configuration stored in `~/.eli/config.toml` with profile-based provider management.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

/// Return the eli home directory (`~/.eli` or `$ELI_HOME`).
pub fn eli_home() -> PathBuf {
    std::env::var("ELI_HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".eli")
        })
}

/// A single provider profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub provider: String,
    pub model: String,
    /// Optional per-profile API base URL override.
    ///
    /// When set, this overrides the provider's registry default base URL for
    /// requests made under this profile. Enables pointing the same provider
    /// name at a different endpoint (e.g. a local inference server like
    /// agent-infer, Ollama, or vLLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
}

/// Default greeting shown when `enabled = true` but no custom message is set.
const DEFAULT_GREETING: &str = "\
Hey! I'm Eli, your AI assistant.

Here are some things I can help with:
- Answer questions about code, docs, or anything you're curious about
- Help you write, debug, or refactor code
- Brainstorm ideas or talk through a problem
- Summarize articles, translate text, or explain concepts

Just type whatever's on your mind — there are no wrong questions.";

/// Greeting configuration for new sessions / channel joins.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GreetingConfig {
    /// Whether greeting is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Static greeting message text. Falls back to a built-in default when empty.
    #[serde(default)]
    pub message: String,
}

/// Eli user configuration persisted to `~/.eli/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EliConfig {
    /// The name of the currently active profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    /// Named profiles mapping name -> provider+model.
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
    /// Send tool description notices to the channel during the tool loop.
    /// Default: false.
    #[serde(default)]
    pub tool_notices: bool,
    /// Greeting for new sessions / channel joins.
    #[serde(default)]
    pub greeting: GreetingConfig,
}

impl EliConfig {
    /// Return the path to the TOML config file.
    pub fn config_path() -> PathBuf {
        eli_home().join("config.toml")
    }

    /// Return the path to the legacy JSON config file.
    fn legacy_config_path() -> PathBuf {
        eli_home().join("config.json")
    }

    /// Load the config from disk.
    ///
    /// If `config.toml` does not exist but `config.json` does, migrate automatically.
    /// Returns defaults if neither file exists or if parsing fails.
    pub fn load() -> Self {
        let toml_path = Self::config_path();

        if toml_path.exists() {
            let contents = match std::fs::read_to_string(&toml_path) {
                Ok(c) => c,
                Err(_) => return Self::default(),
            };
            return toml::from_str(&contents).unwrap_or_default();
        }

        let legacy_path = Self::legacy_config_path();
        if legacy_path.exists()
            && let Some(migrated) = Self::migrate_from_json(&legacy_path)
        {
            let _ = migrated.save();
            return migrated;
        }

        Self::default()
    }

    /// Save the config to disk as TOML.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let toml_str = toml::to_string_pretty(self)? + "\n";
        std::fs::write(&path, &toml_str)?;
        Ok(())
    }

    /// Get the currently active profile.
    pub fn active_profile(&self) -> Option<&Profile> {
        let name = self.active_profile.as_deref()?;
        self.profiles.get(name)
    }

    /// Resolve the model string from the active profile.
    ///
    /// Always returns the model in `provider:model` format. If the stored model
    /// does not contain a `:`, the provider from the profile is prepended.
    pub fn resolve_model(&self) -> Option<String> {
        let name = self.active_profile.as_deref()?;
        let p = self.profiles.get(name)?;
        if p.model.contains(':') {
            Some(p.model.clone())
        } else {
            Some(format!("{}:{}", p.provider, p.model))
        }
    }

    /// Resolve the provider string from the active profile.
    pub fn resolve_provider(&self) -> Option<String> {
        self.active_profile().map(|p| p.provider.clone())
    }

    /// Resolve the API base URL override from the active profile, if any.
    ///
    /// Returns `None` when no profile is active or when the active profile
    /// does not carry an `api_base` override (use the provider default).
    pub fn resolve_api_base(&self) -> Option<String> {
        self.active_profile()
            .and_then(|p| p.api_base.clone())
            .filter(|s| !s.trim().is_empty())
    }

    /// Return every profile's `(provider, api_base)` for profiles that set
    /// a non-empty `api_base`. Used so a fallback to a non-active provider
    /// (e.g. anthropic primary → deepseek fallback) still hits the right
    /// endpoint without requiring `ELI_<PROVIDER>_API_BASE` env vars.
    pub fn profile_api_bases(&self) -> Vec<(String, String)> {
        self.profiles
            .values()
            .filter_map(|p| {
                p.api_base
                    .as_ref()
                    .map(|b| b.trim())
                    .filter(|b| !b.is_empty())
                    .map(|b| (p.provider.clone(), b.to_owned()))
            })
            .collect()
    }

    /// Build a default fallback-model list from the configured profiles, in
    /// declaration order, excluding the active profile and any duplicate of
    /// the primary model string. Each entry is `"provider:model"`.
    ///
    /// Lets the gateway recover from one provider's quota / outage without
    /// the operator pre-setting `ELI_FALLBACK_MODELS` — if there's a second
    /// profile configured, it's already the obvious next try.
    pub fn default_fallback_models(&self) -> Vec<String> {
        let primary = self.resolve_model();
        let primary_str = primary.as_deref();
        let active_name = self.active_profile.as_deref();
        let mut out: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (name, p) in &self.profiles {
            if active_name == Some(name.as_str()) {
                continue;
            }
            let id = if p.model.contains(':') {
                p.model.clone()
            } else {
                format!("{}:{}", p.provider, p.model)
            };
            if Some(id.as_str()) == primary_str {
                continue;
            }
            if seen.insert(id.clone()) {
                out.push(id);
            }
        }
        out
    }

    /// Switch the active profile. Returns `true` if the profile exists.
    pub fn set_active(&mut self, name: &str) -> bool {
        if self.profiles.contains_key(name) {
            self.active_profile = Some(name.to_string());
            true
        } else {
            false
        }
    }

    /// Add or update a profile.
    pub fn add_profile(&mut self, name: &str, profile: Profile) {
        self.profiles.insert(name.to_string(), profile);
    }

    /// Return the greeting message if enabled, checking env override first.
    ///
    /// Priority: `ELI_GREETING_MESSAGE` env > `greeting.message` config > built-in default.
    pub fn greeting_message(&self) -> Option<String> {
        // Env override takes precedence.
        if let Ok(env_msg) = std::env::var("ELI_GREETING_MESSAGE")
            && !env_msg.is_empty()
        {
            return Some(env_msg);
        }
        if !self.greeting.enabled {
            return None;
        }
        if self.greeting.message.is_empty() {
            Some(DEFAULT_GREETING.to_owned())
        } else {
            Some(self.greeting.message.clone())
        }
    }

    /// Migrate from the legacy `config.json` format.
    fn migrate_from_json(path: &std::path::Path) -> Option<Self> {
        let contents = std::fs::read_to_string(path).ok()?;
        let legacy: LegacyConfig = serde_json::from_str(&contents).ok()?;

        let mut config = EliConfig::default();

        if let (Some(provider), Some(model)) = (legacy.default_provider, legacy.default_model) {
            config.add_profile(
                &provider,
                Profile {
                    provider: provider.clone(),
                    model,
                    api_base: None,
                },
            );
            config.active_profile = Some(provider);
        }

        Some(config)
    }
}

/// Legacy config.json schema for migration.
#[derive(Debug, Deserialize)]
struct LegacyConfig {
    #[serde(default)]
    default_provider: Option<String>,
    #[serde(default)]
    default_model: Option<String>,
}

/// Load the Anthropic credential stored by `eli login claude`.
///
/// Returns an OAuth access token (refreshing if expired) or a legacy API key.
pub fn load_anthropic_api_key() -> Option<String> {
    let auth_path = eli_home().join("auth.json");
    let contents = std::fs::read_to_string(&auth_path).ok()?;
    let payload: Value = serde_json::from_str(&contents).ok()?;
    let anthropic = payload.get("anthropic")?;

    resolve_oauth_token(anthropic).or_else(|| resolve_api_key(anthropic))
}

fn resolve_oauth_token(anthropic: &Value) -> Option<String> {
    let access_token = anthropic
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;

    if is_token_expired_with_buffer(anthropic, 300)
        && let Some(refresh_token) = anthropic.get("refresh_token").and_then(|v| v.as_str())
        && let Some(new_token) = refresh_anthropic_token_sync(refresh_token)
    {
        return Some(new_token);
    }
    Some(access_token.trim().to_string())
}

fn resolve_api_key(anthropic: &Value) -> Option<String> {
    anthropic
        .get("api_key")
        .and_then(|k| k.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_token_expired_with_buffer(val: &Value, buffer_seconds: i64) -> bool {
    val.get("expires_at")
        .and_then(|v| v.as_i64())
        .map(|exp| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            now >= exp - buffer_seconds
        })
        .unwrap_or(false)
}

/// Synchronously refresh an Anthropic OAuth token using the refresh_token.
fn refresh_anthropic_token_sync(refresh_token: &str) -> Option<String> {
    let refresh_token = refresh_token.to_owned();

    let run_refresh = move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        rt.block_on(async { refresh_anthropic_token(&refresh_token).await })
    };

    // This helper can be reached from async request handling. Running a nested
    // Tokio runtime on the current thread would panic, so hop to a dedicated
    // OS thread when we're already inside a runtime.
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(run_refresh).join().ok().flatten()
    } else {
        run_refresh()
    }
}

/// Refresh an Anthropic OAuth token and save the new tokens.
pub async fn refresh_anthropic_token(refresh_token: &str) -> Option<String> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
    });

    let resp = client
        .post("https://console.anthropic.com/v1/oauth/token")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let token_resp: Value = resp.json().await.ok()?;
    let access_token = token_resp.get("access_token")?.as_str()?.to_string();
    let refresh_token_new = token_resp
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or(refresh_token)
        .to_string();
    let expires_in = token_resp
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(28800);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let expires_at = now + expires_in;

    let _ = save_anthropic_oauth_tokens(&access_token, &refresh_token_new, expires_at);

    Some(access_token)
}

/// Save Anthropic OAuth tokens to ~/.eli/auth.json.
pub fn save_anthropic_oauth_tokens(
    access_token: &str,
    refresh_token: &str,
    expires_at: i64,
) -> anyhow::Result<()> {
    let entry = serde_json::json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "expires_at": expires_at
    });
    save_auth_entry("anthropic", entry)
}

fn save_auth_entry(provider: &str, value: Value) -> anyhow::Result<()> {
    let home = eli_home();
    std::fs::create_dir_all(&home)?;

    let auth_path = home.join("auth.json");
    let mut auth_data = load_auth_data(&auth_path);
    auth_data.insert(provider.to_string(), value);

    let json_str = serde_json::to_string_pretty(&Value::Object(auth_data))? + "\n";
    std::fs::write(&auth_path, &json_str)?;
    set_file_private(&auth_path);
    Ok(())
}

fn load_auth_data(path: &std::path::Path) -> serde_json::Map<String, Value> {
    if !path.exists() {
        return serde_json::Map::new();
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn set_file_private(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Save an Anthropic API key to ~/.eli/auth.json.
pub fn save_anthropic_api_key_entry(api_key: &str) -> anyhow::Result<()> {
    save_api_key_entry("anthropic", api_key)
}

pub fn save_api_key_entry(provider: &str, api_key: &str) -> anyhow::Result<()> {
    let provider = normalize_provider(provider);
    save_auth_entry(&provider, serde_json::json!({ "api_key": api_key }))
}

pub fn load_api_key_entry(provider: &str) -> Option<String> {
    let provider = normalize_provider(provider);
    let auth_path = eli_home().join("auth.json");
    let contents = std::fs::read_to_string(&auth_path).ok()?;
    let payload: Value = serde_json::from_str(&contents).ok()?;
    payload.get(&provider).and_then(resolve_api_key)
}

/// Load all stored auth entries from `~/.eli/auth.json`.
/// Returns a map of provider -> redacted credential info.
pub fn load_auth_status() -> HashMap<String, String> {
    let mut result = load_eli_auth_entries();
    add_external_auth_entries(&mut result);
    result
}

fn load_eli_auth_entries() -> HashMap<String, String> {
    let auth_path = eli_home().join("auth.json");
    let Ok(contents) = std::fs::read_to_string(&auth_path) else {
        return HashMap::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&contents) else {
        return HashMap::new();
    };
    let Some(obj) = payload.as_object() else {
        return HashMap::new();
    };
    obj.iter()
        .map(|(provider, val)| (provider.clone(), describe_credential(val)))
        .collect()
}

fn describe_credential(val: &Value) -> String {
    if let Some(access_token) = val.get("access_token").and_then(|k| k.as_str()) {
        let expired = is_token_expired(val);
        let status = if expired {
            "(oauth token, expired)"
        } else {
            "(oauth token)"
        };
        format!("{} {}", redact_key(access_token), status)
    } else if let Some(key) = val.get("api_key").and_then(|k| k.as_str()) {
        redact_key(key)
    } else if val.get("token").is_some() {
        "(oauth token)".to_string()
    } else {
        "(configured)".to_string()
    }
}

fn is_token_expired(val: &Value) -> bool {
    is_token_expired_with_buffer(val, 0)
}

fn add_external_auth_entries(result: &mut HashMap<String, String>) {
    let codex_home = std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".codex")
        });
    if codex_home.join("auth.json").exists() && !result.contains_key("openai") {
        result.insert("openai".to_string(), "(codex oauth token)".to_string());
    }

    let gh_home = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("github-copilot");
    if gh_home.exists() && !result.contains_key("github-copilot") {
        result.insert(
            "github-copilot".to_string(),
            "(copilot oauth token)".to_string(),
        );
    }
}

/// Redact an API key, showing first 7 and last 4 characters.
fn redact_key(key: &str) -> String {
    if key.len() <= 12 {
        return "****".to_string();
    }
    format!("{}...{}", &key[..7], &key[key.len() - 4..])
}

/// Default model for a given provider.
pub fn default_model_for_provider(provider: &str) -> &str {
    nexil::core::provider_policies::default_model_for_provider(provider)
}

/// Canonical provider name normalization.
pub fn normalize_provider(provider: &str) -> String {
    nexil::core::provider_policies::normalized_provider_name(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_config_toml_round_trip() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mut config = EliConfig::default();
        config.add_profile(
            "openai",
            Profile {
                provider: "openai".to_string(),
                model: "openai:gpt-5-codex-mini".to_string(),
                api_base: None,
            },
        );
        config.active_profile = Some("openai".to_string());

        let toml_str = toml::to_string_pretty(&config).unwrap() + "\n";
        std::fs::write(&config_path, &toml_str).unwrap();

        let contents = std::fs::read_to_string(&config_path).unwrap();
        let loaded: EliConfig = toml::from_str(&contents).unwrap();

        assert_eq!(loaded.active_profile.as_deref(), Some("openai"));
        let profile = loaded.profiles.get("openai").unwrap();
        assert_eq!(profile.provider, "openai");
        assert_eq!(profile.model, "openai:gpt-5-codex-mini");
    }

    #[test]
    fn test_config_default_is_empty() {
        let config = EliConfig::default();
        assert!(config.active_profile.is_none());
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn test_resolve_model_from_active_profile() {
        let mut config = EliConfig::default();
        config.add_profile(
            "anthropic",
            Profile {
                provider: "anthropic".to_string(),
                model: "anthropic:claude-sonnet-4-20250514".to_string(),
                api_base: None,
            },
        );
        config.active_profile = Some("anthropic".to_string());

        assert_eq!(
            config.resolve_model().as_deref(),
            Some("anthropic:claude-sonnet-4-20250514")
        );
        assert_eq!(config.resolve_provider().as_deref(), Some("anthropic"));
    }

    #[test]
    fn test_resolve_model_none_when_no_active() {
        let config = EliConfig::default();
        assert!(config.resolve_model().is_none());
        assert!(config.resolve_provider().is_none());
    }

    #[test]
    fn test_set_active_returns_false_for_missing_profile() {
        let mut config = EliConfig::default();
        assert!(!config.set_active("nonexistent"));
        assert!(config.active_profile.is_none());
    }

    #[test]
    fn test_set_active_returns_true_for_existing_profile() {
        let mut config = EliConfig::default();
        config.add_profile(
            "openai",
            Profile {
                provider: "openai".to_string(),
                model: "openai:gpt-5-codex-mini".to_string(),
                api_base: None,
            },
        );
        assert!(config.set_active("openai"));
        assert_eq!(config.active_profile.as_deref(), Some("openai"));
    }

    #[test]
    fn test_migrate_from_json() {
        let tmp = TempDir::new().unwrap();
        let json_path = tmp.path().join("config.json");
        let content = serde_json::json!({
            "default_provider": "openai",
            "default_model": "openai:gpt-5-codex-mini"
        });
        std::fs::write(&json_path, serde_json::to_string(&content).unwrap()).unwrap();

        let migrated = EliConfig::migrate_from_json(&json_path).unwrap();
        assert_eq!(migrated.active_profile.as_deref(), Some("openai"));
        let profile = migrated.profiles.get("openai").unwrap();
        assert_eq!(profile.provider, "openai");
        assert_eq!(profile.model, "openai:gpt-5-codex-mini");
    }

    #[test]
    fn test_redact_key() {
        assert_eq!(redact_key("sk-ant-api03-abcdefghij"), "sk-ant-...ghij");
        assert_eq!(redact_key("short"), "****");
    }

    #[test]
    fn test_generic_api_key_entry_uses_normalized_provider() {
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("ELI_HOME", tmp.path());
        }

        save_api_key_entry("ark", "volcano-key").unwrap();

        assert_eq!(
            load_api_key_entry("volcengine").as_deref(),
            Some("volcano-key")
        );
        unsafe {
            std::env::remove_var("ELI_HOME");
        }
    }

    #[test]
    fn test_default_model_for_provider() {
        assert_eq!(default_model_for_provider("openai"), "openai:gpt-5.4-mini");
        assert_eq!(
            default_model_for_provider("anthropic"),
            "anthropic:claude-sonnet-4-6"
        );
        assert_eq!(
            default_model_for_provider("github-copilot"),
            "github-copilot:gpt-5.4-mini"
        );
        assert_eq!(
            default_model_for_provider("dsv4"),
            "deepseek:deepseek-v4-pro"
        );
    }

    #[test]
    fn test_normalize_provider() {
        assert_eq!(normalize_provider("claude"), "anthropic");
        assert_eq!(normalize_provider("copilot"), "github-copilot");
        assert_eq!(normalize_provider("openai"), "openai");
        assert_eq!(normalize_provider("deepseek-v4"), "deepseek");
    }

    #[test]
    fn test_load_anthropic_key_from_json() {
        let tmp = TempDir::new().unwrap();
        let auth_path = tmp.path().join("auth.json");
        let content = serde_json::json!({
            "anthropic": {
                "api_key": "sk-ant-test123"
            }
        });
        std::fs::write(&auth_path, serde_json::to_string(&content).unwrap()).unwrap();

        // Verify the JSON parsing logic directly.
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
        let key = payload
            .get("anthropic")
            .and_then(|a| a.get("api_key"))
            .and_then(|k| k.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        assert_eq!(key.as_deref(), Some("sk-ant-test123"));
    }

    #[test]
    fn test_profile_api_base_round_trip() {
        let mut config = EliConfig::default();
        config.add_profile(
            "agent-infer",
            Profile {
                provider: "agent-infer".to_string(),
                model: "agent-infer:Qwen3-4B".to_string(),
                api_base: Some("http://127.0.0.1:8000/v1".to_string()),
            },
        );
        config.active_profile = Some("agent-infer".to_string());

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let loaded: EliConfig = toml::from_str(&toml_str).unwrap();

        let profile = loaded.profiles.get("agent-infer").unwrap();
        assert_eq!(
            profile.api_base.as_deref(),
            Some("http://127.0.0.1:8000/v1")
        );
        assert_eq!(
            loaded.resolve_api_base().as_deref(),
            Some("http://127.0.0.1:8000/v1")
        );
    }

    #[test]
    fn test_profile_api_base_skipped_when_none() {
        let mut config = EliConfig::default();
        config.add_profile(
            "openai",
            Profile {
                provider: "openai".to_string(),
                model: "openai:gpt-5.4-mini".to_string(),
                api_base: None,
            },
        );
        let toml_str = toml::to_string_pretty(&config).unwrap();
        // Missing field should not appear in serialized TOML when None.
        assert!(!toml_str.contains("api_base"));
    }

    #[test]
    fn test_profile_api_base_deserializes_when_missing() {
        let toml_str = r#"
active_profile = "openai"

[profiles.openai]
provider = "openai"
model = "openai:gpt-5.4-mini"
"#;
        let loaded: EliConfig = toml::from_str(toml_str).unwrap();
        assert!(loaded.profiles.get("openai").unwrap().api_base.is_none());
    }

    #[test]
    fn test_multiple_profiles() {
        let mut config = EliConfig::default();
        config.add_profile(
            "openai",
            Profile {
                provider: "openai".to_string(),
                model: "openai:gpt-5-codex-mini".to_string(),
                api_base: None,
            },
        );
        config.add_profile(
            "anthropic",
            Profile {
                provider: "anthropic".to_string(),
                model: "anthropic:claude-sonnet-4-20250514".to_string(),
                api_base: None,
            },
        );
        config.add_profile(
            "copilot",
            Profile {
                provider: "github-copilot".to_string(),
                model: "github-copilot:gpt-4o".to_string(),
                api_base: None,
            },
        );
        config.active_profile = Some("anthropic".to_string());

        assert_eq!(config.profiles.len(), 3);
        assert_eq!(
            config.resolve_model().as_deref(),
            Some("anthropic:claude-sonnet-4-20250514")
        );

        assert!(config.set_active("openai"));
        assert_eq!(
            config.resolve_model().as_deref(),
            Some("openai:gpt-5-codex-mini")
        );
    }

    #[test]
    fn test_default_fallback_models_excludes_active_and_dups() {
        let mut config = EliConfig::default();
        config.add_profile(
            "openai",
            Profile {
                provider: "openai".to_string(),
                model: "openai:gpt-5.4".to_string(),
                api_base: None,
            },
        );
        config.add_profile(
            "anthropic",
            Profile {
                provider: "anthropic".to_string(),
                model: "anthropic:claude-sonnet-4-6".to_string(),
                api_base: None,
            },
        );
        config.add_profile(
            "deepseek",
            Profile {
                provider: "deepseek".to_string(),
                model: "deepseek:deepseek-v4-pro".to_string(),
                api_base: Some("https://api.deepseek.com/beta".to_string()),
            },
        );
        config.active_profile = Some("anthropic".to_string());

        let fb = config.default_fallback_models();
        assert!(!fb.iter().any(|m| m == "anthropic:claude-sonnet-4-6"));
        assert!(fb.iter().any(|m| m == "openai:gpt-5.4"));
        assert!(fb.iter().any(|m| m == "deepseek:deepseek-v4-pro"));
        // Each entry appears once
        for entry in &fb {
            assert_eq!(fb.iter().filter(|m| m == &entry).count(), 1);
        }
    }

    #[test]
    fn test_default_fallback_models_empty_when_no_others() {
        let mut config = EliConfig::default();
        config.add_profile(
            "anthropic",
            Profile {
                provider: "anthropic".to_string(),
                model: "anthropic:claude-sonnet-4-6".to_string(),
                api_base: None,
            },
        );
        config.active_profile = Some("anthropic".to_string());
        assert!(config.default_fallback_models().is_empty());
    }

    #[test]
    fn test_profile_api_bases_only_non_empty() {
        let mut config = EliConfig::default();
        config.add_profile(
            "anthropic",
            Profile {
                provider: "anthropic".to_string(),
                model: "anthropic:claude-sonnet-4-6".to_string(),
                api_base: None,
            },
        );
        config.add_profile(
            "deepseek",
            Profile {
                provider: "deepseek".to_string(),
                model: "deepseek:deepseek-v4-pro".to_string(),
                api_base: Some("https://api.deepseek.com/beta".to_string()),
            },
        );
        config.add_profile(
            "blanks",
            Profile {
                provider: "blanks".to_string(),
                model: "blanks:noop".to_string(),
                api_base: Some("   ".to_string()),
            },
        );

        let bases = config.profile_api_bases();
        assert_eq!(bases.len(), 1);
        assert_eq!(bases[0].0, "deepseek");
        assert_eq!(bases[0].1, "https://api.deepseek.com/beta");
    }
}
