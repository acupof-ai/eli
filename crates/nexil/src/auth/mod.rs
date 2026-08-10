//! OAuth and API key authentication for LLM providers.

pub mod github_copilot;
pub mod openai_codex;

pub use github_copilot::{
    GitHubCopilotOAuthLoginError, GitHubCopilotOAuthTokens, github_copilot_oauth_resolver,
    load_github_cli_oauth_token, load_github_cli_oauth_token_via_command,
    load_github_copilot_oauth_tokens, login_github_copilot_oauth, save_github_copilot_oauth_tokens,
};
pub use openai_codex::{
    CodexOAuthLoginError, OpenAICodexOAuthTokens, codex_cli_api_key_resolver,
    extract_openai_codex_account_id, load_openai_codex_oauth_tokens, login_openai_codex_oauth,
    refresh_openai_codex_oauth_tokens, save_openai_codex_oauth_tokens,
};

/// A function that resolves an API key for a given provider name.
pub type APIKeyResolver = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Chain multiple resolvers; the first to return `Some` wins.
pub fn multi_api_key_resolver(resolvers: Vec<APIKeyResolver>) -> APIKeyResolver {
    Box::new(move |provider: &str| -> Option<String> {
        resolvers.iter().find_map(|resolver| resolver(provider))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_api_key_resolver_first_wins() {
        let r1: APIKeyResolver = Box::new(|provider: &str| {
            if provider == "openai" {
                Some("key-from-r1".to_string())
            } else {
                None
            }
        });
        let r2: APIKeyResolver = Box::new(|provider: &str| {
            if provider == "openai" {
                Some("key-from-r2".to_string())
            } else {
                None
            }
        });

        let chained = multi_api_key_resolver(vec![r1, r2]);
        assert_eq!(chained("openai"), Some("key-from-r1".to_string()));
    }

    #[test]
    fn test_multi_api_key_resolver_falls_through() {
        let r1: APIKeyResolver = Box::new(|_provider: &str| None);
        let r2: APIKeyResolver = Box::new(|provider: &str| {
            if provider == "openai" {
                Some("key-from-r2".to_string())
            } else {
                None
            }
        });

        let chained = multi_api_key_resolver(vec![r1, r2]);
        assert_eq!(chained("openai"), Some("key-from-r2".to_string()));
    }

    #[test]
    fn test_multi_api_key_resolver_none_when_all_fail() {
        let r1: APIKeyResolver = Box::new(|_: &str| None);
        let r2: APIKeyResolver = Box::new(|_: &str| None);

        let chained = multi_api_key_resolver(vec![r1, r2]);
        assert_eq!(chained("openai"), None);
    }

    #[test]
    fn test_multi_api_key_resolver_empty_chain() {
        let chained = multi_api_key_resolver(vec![]);
        assert_eq!(chained("openai"), None);
    }

    #[test]
    fn test_multi_api_key_resolver_different_providers() {
        let r1: APIKeyResolver = Box::new(|provider: &str| {
            if provider == "openai" {
                Some("openai-key".to_string())
            } else {
                None
            }
        });
        let r2: APIKeyResolver = Box::new(|provider: &str| {
            if provider == "anthropic" {
                Some("anthropic-key".to_string())
            } else {
                None
            }
        });

        let chained = multi_api_key_resolver(vec![r1, r2]);
        assert_eq!(chained("openai"), Some("openai-key".to_string()));
        assert_eq!(chained("anthropic"), Some("anthropic-key".to_string()));
        assert_eq!(chained("other"), None);
    }
}
