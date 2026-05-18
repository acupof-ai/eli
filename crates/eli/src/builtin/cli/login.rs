//! Authentication / login flows for all providers.

use std::path::PathBuf;

use super::detect;
use crate::builtin::coding_plan;
use crate::builtin::config::{EliConfig, Profile, default_model_for_provider, normalize_provider};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginTarget {
    OpenAi,
    Claude,
    GitHubCopilot,
    CodingPlan,
    Volcano,
    DeepSeek,
    Local(Option<&'static str>),
}

/// Mask an account ID for display: show first 3 and last 3 chars only.
fn mask_account_id(id: Option<&str>) -> String {
    let s = id.unwrap_or("unknown");
    if s.len() > 6 {
        format!("{}…{}", &s[..3], &s[s.len() - 3..])
    } else {
        s.to_string()
    }
}

/// Login to a provider.
pub(crate) async fn login_command(
    provider: String,
    codex_home: Option<PathBuf>,
    browser: bool,
    manual: bool,
    timeout: f64,
    api_key_mode: bool,
) -> anyhow::Result<()> {
    match parse_login_target(&provider)? {
        LoginTarget::OpenAi => login_openai(codex_home, browser, manual, timeout).await,
        LoginTarget::Claude => login_claude(browser, api_key_mode).await,
        LoginTarget::GitHubCopilot => login_github_copilot(browser, timeout).await,
        LoginTarget::CodingPlan => login_coding_plan().await,
        LoginTarget::Volcano => login_volcano().await,
        LoginTarget::DeepSeek => login_deepseek().await,
        LoginTarget::Local(brand) => login_local(brand).await,
    }
}

fn parse_login_target(provider: &str) -> anyhow::Result<LoginTarget> {
    parse_known_login_target(provider).ok_or_else(|| unsupported_provider(provider))
}

fn parse_known_login_target(provider: &str) -> Option<LoginTarget> {
    let key = provider.trim().to_ascii_lowercase();
    parse_named_login_target(&key).or_else(|| parse_local_login_target(&key))
}

fn parse_named_login_target(provider: &str) -> Option<LoginTarget> {
    match provider {
        "openai" => Some(LoginTarget::OpenAi),
        "claude" | "anthropic" => Some(LoginTarget::Claude),
        "github-copilot" | "copilot" => Some(LoginTarget::GitHubCopilot),
        "coding-plan" | "coding_plan" | "codingplan" => Some(LoginTarget::CodingPlan),
        "volcano" | "volcengine" | "ark" => Some(LoginTarget::Volcano),
        "deepseek" | "deepseek-v4" | "deepseek_v4" | "ds-v4" | "dsv4" => {
            Some(LoginTarget::DeepSeek)
        }
        _ => None,
    }
}

fn parse_local_login_target(provider: &str) -> Option<LoginTarget> {
    match provider {
        "local" => Some(LoginTarget::Local(None)),
        "agent-infer" | "agent_infer" | "agentinfer" => local_brand("agent-infer"),
        "ollama" => local_brand("ollama"),
        "vllm" => local_brand("vllm"),
        "lmstudio" => local_brand("lmstudio"),
        "llama-cpp" | "llamacpp" | "llama.cpp" => local_brand("llama-cpp"),
        _ => None,
    }
}

fn local_brand(brand: &'static str) -> Option<LoginTarget> {
    Some(LoginTarget::Local(Some(brand)))
}

fn unsupported_provider(provider: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Unsupported auth provider: {provider}\n\
         Supported providers: openai, claude, github-copilot, coding-plan, \
         volcano, deepseek, local (or any of: agent-infer, ollama, vllm, lmstudio, llama-cpp)"
    )
}

async fn login_claude(open_browser: bool, api_key_mode: bool) -> anyhow::Result<()> {
    if api_key_mode {
        login_claude_api_key().await
    } else {
        login_claude_oauth(open_browser).await
    }
}

/// Save a profile after login and print a summary.
fn post_login_save_profile(provider_raw: &str) -> anyhow::Result<()> {
    let provider = normalize_provider(provider_raw);
    let model = default_model_for_provider(&provider);
    save_profile_with_overrides(&provider, None, model, None, false)
}

/// Persist a profile with explicit model and optional api_base override.
///
/// Used by login flows that discover the model/endpoint at runtime
/// (e.g. `eli login local` probing `/v1/models`). `profile_name_override`
/// lets brand-named entries (`agent-infer`, `ollama`, …) save under a
/// distinct user-facing label while sharing one underlying provider type.
fn save_profile_with_overrides(
    provider_raw: &str,
    profile_name_override: Option<&str>,
    model: &str,
    api_base: Option<String>,
    set_active: bool,
) -> anyhow::Result<()> {
    let provider = normalize_provider(provider_raw);
    let profile_name = profile_name(&provider, profile_name_override);
    let mut config = EliConfig::load();
    let had_active = config.active_profile.is_some();

    add_profile(
        &mut config,
        &profile_name,
        &provider,
        model,
        api_base.clone(),
    );
    let became_active = maybe_activate_profile(&mut config, &profile_name, set_active, had_active);
    config.save()?;
    print_profile_summary(&profile_name, &provider, model, api_base);
    print_active_summary(&config, &profile_name, had_active, became_active);
    Ok(())
}

fn profile_name(provider: &str, override_name: Option<&str>) -> String {
    override_name
        .map(str::to_owned)
        .unwrap_or_else(|| provider.to_owned())
}

fn add_profile(
    config: &mut EliConfig,
    profile_name: &str,
    provider: &str,
    model: &str,
    api_base: Option<String>,
) {
    config.add_profile(
        profile_name,
        Profile {
            provider: provider.to_owned(),
            model: model.to_owned(),
            api_base,
        },
    );
}

fn maybe_activate_profile(
    config: &mut EliConfig,
    profile_name: &str,
    set_active: bool,
    had_active: bool,
) -> bool {
    if set_active || !had_active {
        config.active_profile = Some(profile_name.to_owned());
        return true;
    }
    false
}

fn print_profile_summary(
    profile_name: &str,
    provider: &str,
    model: &str,
    api_base: Option<String>,
) {
    println!();
    println!("  Profile:  {profile_name}");
    println!("  Provider: {provider}");
    println!("  Model:    {model}");
    if let Some(base) = api_base {
        println!("  Endpoint: {base}");
    }
}

fn print_active_summary(
    config: &EliConfig,
    profile_name: &str,
    had_active: bool,
    became_active: bool,
) {
    if became_active {
        print_new_active_summary(had_active);
    } else {
        print_inactive_summary(config, profile_name);
    }
}

fn print_new_active_summary(had_active: bool) {
    if had_active {
        println!("  Active:   yes");
    } else {
        println!("  Active:   yes (auto-selected as first profile)");
    }
}

fn print_inactive_summary(config: &EliConfig, profile_name: &str) {
    let current = config.active_profile.as_deref().unwrap_or("(none)");
    if current == profile_name {
        return;
    }
    println!();
    println!("  Tip: run `eli use {profile_name}` to switch to this profile");
    println!("  (current active profile: {current})");
}

async fn login_coding_plan() -> anyhow::Result<()> {
    match pick_coding_plan_provider()?.as_str() {
        coding_plan::VOLCANO_PROVIDER => login_volcano().await,
        provider => anyhow::bail!("Unsupported Coding Plan provider: {provider}"),
    }
}

fn pick_coding_plan_provider() -> anyhow::Result<String> {
    println!("Select Coding Plan provider:");
    println!("  [1] Volcano");
    let answer = read_optional_line("Enter number or name (default 1): ")?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "volcano" | "volcengine" | "ark" => Ok(coding_plan::VOLCANO_PROVIDER.into()),
        other => anyhow::bail!("Unsupported Coding Plan provider: {other}"),
    }
}

async fn login_volcano() -> anyhow::Result<()> {
    println!("Volcano Coding Plan login");
    println!("Endpoint: {}", coding_plan::VOLCANO_OPENAI_BASE);
    let api_key = read_api_key("Enter your Volcano API key: ")?;
    let model = pick_volcano_model()?;
    save_volcano_profile(&api_key, &model)?;
    print_volcano_done(&model);
    Ok(())
}

fn pick_volcano_model() -> anyhow::Result<String> {
    println!();
    println!("Select model:");
    print_volcano_models();
    let answer = read_optional_line("Enter number or model name (default 1): ")?;
    resolve_volcano_model_choice(answer.trim())
}

fn print_volcano_models() {
    for (idx, model) in coding_plan::volcano_models().iter().enumerate() {
        println!("  [{}] {}", idx + 1, model);
    }
}

fn resolve_volcano_model_choice(choice: &str) -> anyhow::Result<String> {
    if choice.is_empty() {
        return Ok(coding_plan::volcano_model_at(1)
            .unwrap_or("ark-code-latest")
            .into());
    }
    if let Ok(index) = choice.parse::<usize>() {
        return coding_plan::volcano_model_at(index)
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("model selection out of range: {index}"));
    }
    Ok(choice.to_owned())
}

fn save_volcano_profile(api_key: &str, model: &str) -> anyhow::Result<()> {
    crate::builtin::config::save_api_key_entry(coding_plan::VOLCANO_PROVIDER, api_key)?;
    let model = format!("{}:{model}", coding_plan::VOLCANO_PROVIDER);
    save_profile_with_overrides(
        coding_plan::VOLCANO_PROVIDER,
        Some(coding_plan::VOLCANO_PROFILE),
        &model,
        Some(coding_plan::VOLCANO_OPENAI_BASE.to_owned()),
        true,
    )
}

fn print_volcano_done(model: &str) {
    println!();
    println!("Done. Active profile: volcano");
    println!("  Model:    {model}");
    println!("  Endpoint: {}", coding_plan::VOLCANO_OPENAI_BASE);
    println!();
    println!("Try: eli chat");
}

async fn login_deepseek() -> anyhow::Result<()> {
    println!("DeepSeek V4 login");
    println!(
        "Endpoint: {}",
        nexil::core::provider_policies::DEEPSEEK_OPENAI_BASE
    );
    let api_key = read_api_key("Enter your DeepSeek API key: ")?;
    let model = pick_deepseek_model()?;
    save_deepseek_profile(&api_key, &model)?;
    print_deepseek_done(&model);
    Ok(())
}

fn pick_deepseek_model() -> anyhow::Result<String> {
    println!();
    println!("Select model:");
    print_deepseek_models();
    let answer = read_optional_line("Enter number or model name (default 1): ")?;
    resolve_deepseek_model_choice(answer.trim())
}

fn print_deepseek_models() {
    for (idx, model) in deepseek_models().iter().enumerate() {
        println!("  [{}] {}", idx + 1, model);
    }
}

fn deepseek_models() -> &'static [&'static str] {
    &["deepseek-v4-pro", "deepseek-v4-flash"]
}

fn resolve_deepseek_model_choice(choice: &str) -> anyhow::Result<String> {
    if choice.is_empty() {
        return Ok(deepseek_models()[0].to_owned());
    }
    if let Ok(index) = choice.parse::<usize>() {
        return index
            .checked_sub(1)
            .and_then(|idx| deepseek_models().get(idx))
            .map(|model| (*model).to_owned())
            .ok_or_else(|| anyhow::anyhow!("model selection out of range: {index}"));
    }
    Ok(choice.to_owned())
}

fn save_deepseek_profile(api_key: &str, model: &str) -> anyhow::Result<()> {
    let provider = "deepseek";
    crate::builtin::config::save_api_key_entry(provider, api_key)?;
    let model = format!("{provider}:{model}");
    save_profile_with_overrides(
        provider,
        None,
        &model,
        Some(nexil::core::provider_policies::DEEPSEEK_OPENAI_BASE.to_owned()),
        true,
    )
}

fn print_deepseek_done(model: &str) {
    println!();
    println!("Done. Active profile: deepseek");
    println!("  Model:    {model}");
    println!(
        "  Endpoint: {}",
        nexil::core::provider_policies::DEEPSEEK_OPENAI_BASE
    );
    println!();
    println!("Try: eli chat");
}

fn read_required_line(prompt: &str) -> anyhow::Result<String> {
    let value = read_optional_line(prompt)?;
    if value.is_empty() {
        anyhow::bail!("value cannot be empty");
    }
    Ok(value)
}

fn read_api_key(prompt: &str) -> anyhow::Result<String> {
    let value = read_required_line(prompt)?;
    if value.chars().all(|ch| ch == '*') {
        anyhow::bail!("masked API key cannot be used");
    }
    Ok(value)
}

fn read_optional_line(prompt: &str) -> anyhow::Result<String> {
    use std::io::{self, Write};

    print!("{prompt}");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
}

/// OpenAI Codex OAuth login flow.
async fn login_openai(
    codex_home: Option<PathBuf>,
    open_browser: bool,
    _manual: bool,
    timeout: f64,
) -> anyhow::Result<()> {
    use nexil::auth::openai_codex::{
        self, DEFAULT_CODEX_OAUTH_AUTHORIZE_URL, DEFAULT_CODEX_OAUTH_CLIENT_ID,
        DEFAULT_CODEX_OAUTH_ORIGINATOR, DEFAULT_CODEX_OAUTH_SCOPE, DEFAULT_CODEX_OAUTH_TOKEN_URL,
    };

    let home = codex_home.unwrap_or_else(|| {
        std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".codex")
            })
    });

    let redirect_uri = "http://localhost:1455/auth/callback";

    println!("Starting OpenAI Codex OAuth login...");
    if !open_browser {
        println!("(browser auto-open disabled; copy the URL from above)");
    }

    let tokens = openai_codex::login_openai_codex_oauth(
        Some(home.as_path()),
        redirect_uri,
        timeout,
        DEFAULT_CODEX_OAUTH_CLIENT_ID,
        DEFAULT_CODEX_OAUTH_AUTHORIZE_URL,
        DEFAULT_CODEX_OAUTH_TOKEN_URL,
        DEFAULT_CODEX_OAUTH_SCOPE,
        DEFAULT_CODEX_OAUTH_ORIGINATOR,
        open_browser,
    )
    .await
    .map_err(|e| anyhow::anyhow!("OpenAI OAuth login failed: {e}"))?;

    println!(
        "Login successful! Account: {}",
        mask_account_id(tokens.account_id.as_deref())
    );
    println!("Tokens saved to: {}", home.join("auth.json").display());

    post_login_save_profile("openai")?;

    Ok(())
}

/// Claude / Anthropic OAuth login — user runs `claude setup-token` separately and pastes the token.
async fn login_claude_oauth(_open_browser: bool) -> anyhow::Result<()> {
    use std::io::{self, Write};

    println!("Claude subscription login");
    println!();
    println!("  1. Run this in another terminal:  claude setup-token");
    println!("  2. Complete the browser login");
    println!("  3. Copy the token (starts with sk-ant-) and paste it below");
    println!();
    print!("Paste your token: ");
    io::stdout().flush()?;

    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    if token.is_empty() {
        anyhow::bail!("Token cannot be empty");
    }

    if !token.starts_with("sk-ant-") {
        anyhow::bail!(
            "Invalid token format (expected sk-ant-...).\n\
             Make sure you copied the full token from `claude setup-token`."
        );
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let expires_at = now + 365 * 24 * 3600;

    crate::builtin::config::save_anthropic_oauth_tokens(&token, "", expires_at)?;

    let auth_path = crate::builtin::config::eli_home().join("auth.json");
    println!("Token saved to: {}", auth_path.display());

    post_login_save_profile("anthropic")?;

    Ok(())
}

/// Claude / Anthropic API key login flow (legacy, via --api-key flag).
async fn login_claude_api_key() -> anyhow::Result<()> {
    use std::io::{self, Write};

    println!("Anthropic API key login");
    println!("Get your API key from: https://console.anthropic.com/settings/keys");
    print!("Enter your Anthropic API key: ");
    io::stdout().flush()?;

    let mut api_key = String::new();
    io::stdin().read_line(&mut api_key)?;
    let api_key = api_key.trim().to_string();

    if api_key.is_empty() {
        anyhow::bail!("API key cannot be empty");
    }

    crate::builtin::config::save_anthropic_api_key_entry(&api_key)?;

    let auth_path = crate::builtin::config::eli_home().join("auth.json");
    println!("API key saved to: {}", auth_path.display());

    post_login_save_profile("anthropic")?;

    Ok(())
}

/// Auto-detect a running local OpenAI-compatible server and persist a profile.
///
/// Probes candidate endpoints (see `detect::local_candidates`: `ELI_LOCAL_URL`
/// override, then default ports for agent-infer/vllm/llama.cpp/ollama/lmstudio,
/// plus any `ELI_LOCAL_PORTS` extras). Reads the served model from
/// `/v1/models`, prompts the user to confirm, then writes a profile with both
/// the discovered model and the `api_base` override.
///
/// `brand_hint` is purely cosmetic: it picks the saved profile name and the
/// "how to start" message shown when no server responds. `None` saves under
/// the generic name `local`. The profile's `provider` field is always `local`
/// so all brands share one runtime path.
async fn login_local(brand_hint: Option<&str>) -> anyhow::Result<()> {
    use std::io::{self, Write};

    let label = brand_hint.unwrap_or("local");
    println!("🔍 Probing {label}...");

    let hit = match detect::detect_local().await {
        Some(h) => h,
        None => {
            eprintln!();
            eprintln!("No local server responded on any candidate endpoint.");
            eprintln!();
            eprintln!("Tried:");
            for candidate in detect::local_candidates() {
                eprintln!("  - {candidate}");
            }
            eprintln!();
            print_start_hint(brand_hint);
            eprintln!();
            eprintln!("Or override the endpoint:");
            eprintln!("  ELI_LOCAL_URL=http://host:port eli login {label}");
            eprintln!("  ELI_LOCAL_PORTS=8090,9000 eli login {label}   # extra ports");
            anyhow::bail!("{label} not reachable");
        }
    };

    println!("  Endpoint: {}", hit.api_base);
    println!("  Model:    {}", hit.model_id);
    println!();
    print!("Save as profile '{label}' and set active? [y/N] ");
    io::stdout().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer != "y" && answer != "yes" {
        println!("Aborted.");
        return Ok(());
    }

    // Always prefix with `local:` unless the model id already starts with a
    // known provider prefix. Ollama-style tags like `llama3.2:3b` contain a
    // colon but the prefix is the model name, not a provider — so plain
    // `contains(':')` would mis-strip the prefix and break runtime lookup.
    let model_id = match hit.model_id.split_once(':') {
        Some((prefix, _)) if nexil::core::provider_policies::is_known_provider(prefix) => {
            hit.model_id.clone()
        }
        _ => format!("local:{}", hit.model_id),
    };

    save_profile_with_overrides("local", Some(label), &model_id, Some(hit.api_base), true)?;

    println!();
    println!("Done. Try: eli chat");
    Ok(())
}

/// Print a brand-specific "how to start the server" hint on the miss path.
fn print_start_hint(brand: Option<&str>) {
    eprintln!("Start a local server first:");
    match brand {
        Some("agent-infer") => {
            eprintln!("  cd ~/code/agent-infer && ./scripts/start_infer.sh");
            eprintln!("    # optional: ./scripts/start_infer.sh models/Qwen3-4B 8000");
            eprintln!("  # Docker (Linux + CUDA)");
            eprintln!(
                "  docker run --gpus all -v /path/to/model:/model \\\n    \
                 ghcr.io/cklxx/agent-infer:latest --model-path /model --port 8000"
            );
        }
        Some("ollama") => {
            eprintln!("  ollama serve                           # default port 11434");
            eprintln!("  ollama pull llama3.2                   # in another shell");
        }
        Some("vllm") => {
            eprintln!("  python -m vllm.entrypoints.openai.api_server \\");
            eprintln!("    --model <hf-id-or-path> --port 8000");
        }
        Some("lmstudio") => {
            eprintln!("  Open LM Studio → Local Server tab → Start Server (default :1234)");
        }
        Some("llama-cpp") => {
            eprintln!("  llama-server -m <model.gguf> --port 8080");
        }
        _ => {
            eprintln!("  Any OpenAI-compatible server on one of the probed ports above.");
            eprintln!("  Common defaults: agent-infer/vllm:8000, llama.cpp:8080,");
            eprintln!("                   ollama:11434, lmstudio:1234.");
        }
    }
}

/// GitHub Copilot device-flow login.
async fn login_github_copilot(open_browser: bool, timeout: f64) -> anyhow::Result<()> {
    use nexil::auth::github_copilot::{
        self, DEFAULT_GITHUB_COPILOT_ACCESS_TOKEN_URL, DEFAULT_GITHUB_COPILOT_DEVICE_CODE_URL,
        DEFAULT_GITHUB_COPILOT_OAUTH_CLIENT_ID, DEFAULT_GITHUB_COPILOT_SCOPE,
    };

    println!("Starting GitHub Copilot device-flow login...");

    let tokens = github_copilot::login_github_copilot_oauth(
        None, // config_home (uses default)
        open_browser,
        DEFAULT_GITHUB_COPILOT_SCOPE,
        timeout,
        DEFAULT_GITHUB_COPILOT_OAUTH_CLIENT_ID,
        DEFAULT_GITHUB_COPILOT_DEVICE_CODE_URL,
        DEFAULT_GITHUB_COPILOT_ACCESS_TOKEN_URL,
        None, // device_code_notifier (uses default eprintln)
    )
    .await
    .map_err(|e| anyhow::anyhow!("GitHub Copilot login failed: {e}"))?;

    let login_info = tokens.login.as_deref().unwrap_or("(unknown user)");
    println!("Login successful! GitHub user: {login_info}");

    post_login_save_profile("github-copilot")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_coding_plan_aliases() {
        assert_eq!(
            parse_login_target("coding-plan").unwrap(),
            LoginTarget::CodingPlan
        );
        assert_eq!(parse_login_target("ark").unwrap(), LoginTarget::Volcano);
        assert_eq!(
            parse_login_target("volcengine").unwrap(),
            LoginTarget::Volcano
        );
        assert_eq!(parse_login_target("dsv4").unwrap(), LoginTarget::DeepSeek);
    }

    #[test]
    fn resolves_volcano_model_selection() {
        assert_eq!(
            resolve_volcano_model_choice("2").unwrap(),
            "doubao-seed-2.0-code"
        );
        assert_eq!(resolve_volcano_model_choice("").unwrap(), "ark-code-latest");
        assert!(resolve_volcano_model_choice("99").is_err());
    }

    #[test]
    fn resolves_deepseek_model_selection() {
        assert_eq!(
            resolve_deepseek_model_choice("2").unwrap(),
            "deepseek-v4-flash"
        );
        assert_eq!(
            resolve_deepseek_model_choice("").unwrap(),
            "deepseek-v4-pro"
        );
        assert!(resolve_deepseek_model_choice("0").is_err());
    }
}
