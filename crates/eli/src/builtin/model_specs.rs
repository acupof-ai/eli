//! Model specification table and inference helpers.
//!
//! Maps model name patterns to context-window and max-output-token limits.

use super::settings::{DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT_TOKENS};

// ---------------------------------------------------------------------------
// Match strategy
// ---------------------------------------------------------------------------

/// Match strategy for a model pattern.
#[derive(Clone, Copy)]
enum Match {
    /// Pattern is a substring of the model ID (default, most entries).
    Contains,
    /// Pattern must appear at the start of the model ID (avoids false positives
    /// for short patterns like "o3").
    Prefix,
}

// ---------------------------------------------------------------------------
// Model spec table
// ---------------------------------------------------------------------------

/// Known model family spec: (pattern, match strategy, context_window, max_output_tokens).
///
/// Order matters — first match wins. More specific patterns must come before
/// generic catch-alls. Matching is case-insensitive against the model_id part
/// (provider prefix stripped).
const MODEL_SPECS: &[(&str, Match, usize, usize)] = &[
    // --- Anthropic Claude ---------------------------------------------------
    ("claude-opus-4-6", Match::Contains, 200_000, 128_000),
    ("claude-sonnet-4-6", Match::Contains, 200_000, 64_000),
    ("claude-sonnet-4-5", Match::Contains, 200_000, 64_000),
    ("claude-haiku-4-5", Match::Contains, 200_000, 16_384),
    ("claude-opus-4", Match::Contains, 200_000, 32_000),
    ("claude-sonnet-4", Match::Contains, 200_000, 64_000),
    ("claude-3-5-sonnet", Match::Contains, 200_000, 8_192),
    ("claude-3-5-haiku", Match::Contains, 200_000, 8_192),
    ("claude-3-opus", Match::Contains, 200_000, 4_096),
    ("claude-3-sonnet", Match::Contains, 200_000, 4_096),
    ("claude-3-haiku", Match::Contains, 200_000, 4_096),
    ("claude", Match::Contains, 200_000, 64_000),
    // --- OpenAI -------------------------------------------------------------
    ("o3", Match::Prefix, 200_000, 100_000),
    ("o4-mini", Match::Prefix, 200_000, 100_000),
    ("o1", Match::Prefix, 200_000, 100_000),
    ("gpt-4.1", Match::Contains, 1_048_576, 32_768),
    ("gpt-4o", Match::Contains, 128_000, 16_384),
    ("gpt-4-turbo", Match::Contains, 128_000, 4_096),
    ("gpt-4", Match::Contains, 8_192, 4_096),
    ("gpt-3.5", Match::Contains, 16_384, 4_096),
    // --- Google Gemini ------------------------------------------------------
    ("gemini-2.5", Match::Contains, 1_048_576, 65_536),
    ("gemini-2.0", Match::Contains, 1_048_576, 8_192),
    ("gemini-1.5", Match::Contains, 1_048_576, 8_192),
    ("gemini", Match::Contains, 128_000, 8_192),
    // --- DeepSeek -----------------------------------------------------------
    ("deepseek-v4", Match::Contains, 1_000_000, 384_000),
    ("deepseek-reasoner", Match::Contains, 164_000, 65_536),
    ("deepseek-r1", Match::Contains, 164_000, 65_536),
    ("deepseek", Match::Contains, 128_000, 8_192),
    // --- Qwen (Alibaba) ----------------------------------------------------
    ("qwen3.5", Match::Contains, 1_000_000, 65_536),
    ("qwen3-coder", Match::Contains, 1_000_000, 65_536),
    ("qwen3", Match::Contains, 1_000_000, 65_536),
    ("qwen", Match::Contains, 128_000, 32_768),
    // --- Kimi (Moonshot AI) -------------------------------------------------
    ("kimi-k2", Match::Contains, 262_144, 65_536),
    ("moonshot", Match::Contains, 128_000, 16_384),
    // --- GLM (Zhipu AI) -----------------------------------------------------
    ("glm-5", Match::Contains, 200_000, 128_000),
    ("glm-4", Match::Contains, 128_000, 4_096),
    ("glm", Match::Contains, 128_000, 4_096),
    // --- MiniMax ------------------------------------------------------------
    ("minimax-m2", Match::Contains, 204_800, 131_072),
    ("minimax-text-01", Match::Contains, 4_000_000, 204_800),
    ("minimax", Match::Contains, 204_800, 65_536),
    // --- Llama (Meta) -------------------------------------------------------
    ("llama-4-scout", Match::Contains, 10_000_000, 16_384),
    ("llama-4", Match::Contains, 1_048_576, 16_384),
    ("llama-3", Match::Contains, 128_000, 8_192),
    ("llama", Match::Contains, 128_000, 8_192),
    // --- Mistral / Codestral ------------------------------------------------
    ("mistral-large", Match::Contains, 128_000, 32_768),
    ("codestral", Match::Contains, 256_000, 32_768),
    ("mistral", Match::Contains, 32_000, 8_192),
];

// ---------------------------------------------------------------------------
// Inference helpers
// ---------------------------------------------------------------------------

/// Strip the `"provider:"` prefix from a model string, returning just the
/// model ID portion. If no colon is present, returns the full string.
fn strip_provider(model: &str) -> &str {
    model.split_once(':').map_or(model, |(_, id)| id)
}

/// Look up model specs from `MODEL_SPECS` table. Returns
/// `(context_window, max_output_tokens)` or `None` if no match.
fn lookup_model_spec(model: &str) -> Option<(usize, usize)> {
    let id = strip_provider(model).to_lowercase();
    // For OpenRouter paths like "qwen/qwen3-coder-next", also try the part
    // after the last slash.
    let slug = id.rsplit('/').next().unwrap_or(&id);

    for &(pattern, strategy, ctx, out) in MODEL_SPECS {
        let matched = match strategy {
            Match::Contains => id.contains(pattern) || slug.contains(pattern),
            Match::Prefix => id.starts_with(pattern) || slug.starts_with(pattern),
        };
        if matched {
            return Some((ctx, out));
        }
    }
    None
}

/// Infer context window size (in tokens) from a model name string.
pub(super) fn infer_context_window(model: &str) -> usize {
    lookup_model_spec(model)
        .map(|(ctx, _)| ctx)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

/// Infer the maximum output tokens for a model from its name.
///
/// Returns the largest output-token limit the model supports so that tool
/// calls with large payloads (e.g. document creation) are not silently
/// truncated.
pub(super) fn infer_max_output_tokens(model: &str) -> usize {
    lookup_model_spec(model)
        .map(|(_, out)| out)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- strip_provider -------------------------------------------------------

    #[test]
    fn test_strip_provider() {
        assert_eq!(
            strip_provider("anthropic:claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            strip_provider("openrouter:qwen/qwen3-coder-next"),
            "qwen/qwen3-coder-next"
        );
        assert_eq!(strip_provider("claude-sonnet-4-6"), "claude-sonnet-4-6");
    }

    // -- infer_context_window ------------------------------------------------

    #[test]
    fn test_context_window_anthropic() {
        assert_eq!(infer_context_window("anthropic:claude-opus-4-6"), 200_000);
        assert_eq!(infer_context_window("anthropic:claude-sonnet-4-6"), 200_000);
        assert_eq!(
            infer_context_window("anthropic:claude-sonnet-4-20250514"),
            200_000
        );
        assert_eq!(
            infer_context_window("anthropic:claude-3-5-sonnet-20241022"),
            200_000
        );
        assert_eq!(
            infer_context_window("anthropic:claude-3-haiku-20240307"),
            200_000
        );
    }

    #[test]
    fn test_context_window_openai() {
        assert_eq!(infer_context_window("openai:gpt-4o-2024-08-06"), 128_000);
        assert_eq!(infer_context_window("openai:gpt-4.1-2025-04-14"), 1_048_576);
        assert_eq!(infer_context_window("openai:o3-2025-04-16"), 200_000);
        assert_eq!(infer_context_window("openai:o4-mini"), 200_000);
        assert_eq!(infer_context_window("openai:o1-2024-12-17"), 200_000);
        assert_eq!(infer_context_window("openai:gpt-3.5-turbo"), 16_384);
    }

    #[test]
    fn test_context_window_gemini() {
        assert_eq!(infer_context_window("google:gemini-2.5-pro"), 1_048_576);
        assert_eq!(infer_context_window("google:gemini-2.5-flash"), 1_048_576);
        assert_eq!(infer_context_window("google:gemini-1.5-flash"), 1_048_576);
    }

    #[test]
    fn test_context_window_chinese_models() {
        assert_eq!(infer_context_window("dashscope:qwen3.5-plus"), 1_000_000);
        assert_eq!(
            infer_context_window("openrouter:qwen/qwen3-coder-next"),
            1_000_000
        );
        assert_eq!(infer_context_window("moonshot:kimi-k2.5"), 262_144);
        assert_eq!(infer_context_window("zhipu:glm-5"), 200_000);
        assert_eq!(infer_context_window("minimax:minimax-text-01"), 4_000_000);
        assert_eq!(infer_context_window("minimax:minimax-m2.7"), 204_800);
    }

    #[test]
    fn test_context_window_others() {
        assert_eq!(infer_context_window("deepseek:deepseek-chat"), 128_000);
        assert_eq!(infer_context_window("deepseek:deepseek-v4-pro"), 1_000_000);
        assert_eq!(infer_context_window("deepseek:deepseek-reasoner"), 164_000);
        assert_eq!(infer_context_window("meta:llama-4-maverick"), 1_048_576);
        assert_eq!(infer_context_window("meta:llama-4-scout"), 10_000_000);
        assert_eq!(infer_context_window("meta:llama-3.1-70b"), 128_000);
        assert_eq!(
            infer_context_window("mistral:mistral-large-latest"),
            128_000
        );
        assert_eq!(infer_context_window("mistral:codestral-latest"), 256_000);
    }

    #[test]
    fn test_context_window_unknown_fallback() {
        assert_eq!(
            infer_context_window("unknown:some-model"),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    // -- infer_max_output_tokens ---------------------------------------------

    #[test]
    fn test_max_output_anthropic() {
        assert_eq!(
            infer_max_output_tokens("anthropic:claude-opus-4-6"),
            128_000
        );
        assert_eq!(
            infer_max_output_tokens("anthropic:claude-sonnet-4-6"),
            64_000
        );
        assert_eq!(
            infer_max_output_tokens("anthropic:claude-opus-4-20250514"),
            32_000
        );
        assert_eq!(
            infer_max_output_tokens("anthropic:claude-sonnet-4-20250514"),
            64_000
        );
        assert_eq!(
            infer_max_output_tokens("anthropic:claude-3-5-sonnet-20241022"),
            8_192
        );
        assert_eq!(
            infer_max_output_tokens("anthropic:claude-3-haiku-20240307"),
            4_096
        );
    }

    #[test]
    fn test_max_output_openai() {
        assert_eq!(infer_max_output_tokens("openai:gpt-4o"), 16_384);
        assert_eq!(infer_max_output_tokens("openai:gpt-4.1"), 32_768);
        assert_eq!(infer_max_output_tokens("openai:o3"), 100_000);
        assert_eq!(infer_max_output_tokens("openai:o4-mini"), 100_000);
        assert_eq!(infer_max_output_tokens("openai:o1"), 100_000);
    }

    #[test]
    fn test_max_output_gemini() {
        assert_eq!(infer_max_output_tokens("google:gemini-2.5-pro"), 65_536);
        assert_eq!(infer_max_output_tokens("google:gemini-2.0-flash"), 8_192);
    }

    #[test]
    fn test_max_output_chinese_models() {
        assert_eq!(infer_max_output_tokens("dashscope:qwen3.5-plus"), 65_536);
        assert_eq!(
            infer_max_output_tokens("openrouter:qwen/qwen3-coder-next"),
            65_536
        );
        assert_eq!(infer_max_output_tokens("moonshot:kimi-k2.5"), 65_536);
        assert_eq!(infer_max_output_tokens("zhipu:glm-5"), 128_000);
        assert_eq!(infer_max_output_tokens("minimax:minimax-m2.7"), 131_072);
    }

    #[test]
    fn test_max_output_others() {
        assert_eq!(infer_max_output_tokens("deepseek:deepseek-chat"), 8_192);
        assert_eq!(
            infer_max_output_tokens("deepseek:deepseek-v4-flash"),
            384_000
        );
        assert_eq!(
            infer_max_output_tokens("deepseek:deepseek-reasoner"),
            65_536
        );
        assert_eq!(infer_max_output_tokens("meta:llama-4-maverick"), 16_384);
        assert_eq!(
            infer_max_output_tokens("mistral:mistral-large-latest"),
            32_768
        );
    }

    #[test]
    fn test_max_output_unknown_fallback() {
        assert_eq!(
            infer_max_output_tokens("unknown:some-model"),
            DEFAULT_MAX_OUTPUT_TOKENS
        );
    }

    // -- OpenRouter-style paths ----------------------------------------------

    #[test]
    fn test_openrouter_model_paths() {
        assert_eq!(
            infer_context_window("openrouter:anthropic/claude-sonnet-4-6"),
            200_000
        );
        assert_eq!(
            infer_max_output_tokens("openrouter:anthropic/claude-sonnet-4-6"),
            64_000
        );
        assert_eq!(
            infer_context_window("openrouter:google/gemini-2.5-pro"),
            1_048_576
        );
        assert_eq!(
            infer_max_output_tokens("openrouter:google/gemini-2.5-pro"),
            65_536
        );
        assert_eq!(
            infer_context_window("openrouter:deepseek/deepseek-r1"),
            164_000
        );
        assert_eq!(
            infer_context_window("openrouter:deepseek/deepseek-v4-flash"),
            1_000_000
        );
    }
}
