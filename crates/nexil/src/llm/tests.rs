use super::*;
use crate::auth::APIKeyResolver;
use crate::core::results::{StreamEvent, StreamEventKind, ToolExecution};
use crate::tape::entries::{TapeEntry, TapeEntryKind};
use serde_json::json;
use std::collections::HashMap;

// ----- LLM::new -----

#[test]
fn test_llm_new_default_config() {
    let llm = LLM::new(
        Some("openai:gpt-4o"),
        None,
        None,
        None,
        Some("test-key".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    assert_eq!(llm.model(), "gpt-4o");
    assert_eq!(llm.provider(), "openai");
    assert!(llm.fallback_models().is_empty());
}

#[test]
fn test_llm_new_with_provider_prefix() {
    let llm = LLM::new(
        Some("anthropic:claude-3-5-sonnet"),
        None,
        None,
        None,
        Some("key".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    assert_eq!(llm.model(), "claude-3-5-sonnet");
    assert_eq!(llm.provider(), "anthropic");
}

#[test]
fn test_llm_new_with_explicit_provider() {
    let llm = LLM::new(
        Some("gpt-4o"),
        Some("openai"),
        None,
        None,
        Some("key".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    assert_eq!(llm.model(), "gpt-4o");
    assert_eq!(llm.provider(), "openai");
}

#[test]
fn test_llm_new_defaults_to_gpt4o_mini() {
    let llm = LLM::new(
        None,
        None,
        None,
        None,
        Some("key".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    // Default model is "openai:gpt-4o-mini"
    assert_eq!(llm.provider(), "openai");
    assert_eq!(llm.model(), "gpt-4o-mini");
}

#[test]
fn test_llm_new_rejects_invalid_verbose() {
    let result = LLM::new(
        Some("openai:gpt-4o"),
        None,
        None,
        None,
        Some("key".to_string()),
        None,
        None,
        None,
        None,
        Some(5),
        None,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("verbose"));
}

#[test]
fn test_llm_new_rejects_provider_prefix_with_explicit_provider() {
    let result = LLM::new(
        Some("openai:gpt-4o"),
        Some("anthropic"),
        None,
        None,
        Some("key".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    );
    assert!(result.is_err());
}

#[test]
fn test_llm_new_with_fallback_models() {
    let llm = LLM::new(
        Some("openai:gpt-4o"),
        None,
        Some(vec!["openai:gpt-4o-mini".to_string()]),
        None,
        Some("key".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    assert_eq!(llm.fallback_models(), &["openai:gpt-4o-mini"]);
}

// ----- Display / Debug -----

#[test]
fn test_llm_display() {
    let llm = LLM::new(
        Some("openai:gpt-4o"),
        None,
        None,
        None,
        Some("key".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    let display = format!("{}", llm);
    assert_eq!(display, "LLM(openai:gpt-4o)");
}

#[test]
fn test_llm_debug() {
    let llm = LLM::new(
        Some("openai:gpt-4o"),
        None,
        None,
        None,
        Some("key".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    let debug = format!("{:?}", llm);
    assert!(debug.contains("LLM"));
    assert!(debug.contains("openai"));
}

// ----- ApiFormat -----

#[test]
fn test_api_format_as_str() {
    assert_eq!(ApiFormat::Auto.as_str(), "auto");
    assert_eq!(ApiFormat::Completion.as_str(), "completion");
    assert_eq!(ApiFormat::Responses.as_str(), "responses");
    assert_eq!(ApiFormat::Messages.as_str(), "messages");
}

#[test]
fn test_api_format_equality() {
    assert_eq!(ApiFormat::Auto, ApiFormat::Auto);
    assert_eq!(ApiFormat::Completion, ApiFormat::Completion);
    assert_ne!(ApiFormat::Completion, ApiFormat::Responses);
}

// ----- EmbedInput -----

#[test]
fn test_embed_input_from_str() {
    let input: EmbedInput = "hello".into();
    matches!(input, EmbedInput::Single("hello"));
}

#[test]
fn test_embed_input_from_slice() {
    let data = vec!["a".to_string(), "b".to_string()];
    let input: EmbedInput = data.as_slice().into();
    matches!(input, EmbedInput::Multiple(_));
}

// ----- build_messages -----

#[test]
fn test_build_messages_with_prompt_only() {
    let msgs = build_messages(Some("hello"), None, None, None);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "hello");
}

#[test]
fn test_build_messages_with_system_and_prompt() {
    let msgs = build_messages(Some("hello"), None, Some("you are helpful"), None);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "you are helpful");
    assert_eq!(msgs[1]["role"], "user");
}

#[test]
fn test_build_messages_with_existing_messages() {
    let existing = vec![json!({"role": "assistant", "content": "hi"})];
    let msgs = build_messages(Some("follow up"), None, None, Some(&existing));
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "assistant");
    assert_eq!(msgs[1]["role"], "user");
}

#[test]
fn test_build_messages_empty() {
    let msgs = build_messages(None, None, None, None);
    assert!(msgs.is_empty());
}

#[test]
fn test_build_full_context_from_entries_normalizes_responses_tool_calls() {
    let entries = vec![
        TapeEntry::tool_call(
            vec![json!({
                "type": "function_call",
                "call_id": "call_123",
                "name": "tape_info",
                "arguments": "{}"
            })],
            json!({}),
        ),
        TapeEntry::tool_result(
            vec![json!({
                "call_id": "call_123",
                "output": {"count": 1}
            })],
            json!({}),
        ),
    ];

    let messages = build_full_context_from_entries(&entries);

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["tool_calls"][0]["id"], "call_123");
    assert_eq!(
        messages[0]["tool_calls"][0]["function"]["name"],
        "tape_info"
    );
    assert_eq!(messages[1]["tool_call_id"], "call_123");
}

#[test]
fn test_build_full_context_from_entries_preserves_tool_call_content() {
    let entries = vec![TapeEntry::tool_call_with_content(
        vec![json!({
            "type": "function",
            "id": "call_123",
            "function": {
                "name": "tape_info",
                "arguments": "{}"
            }
        })],
        Some("Checking tape state".to_owned()),
        json!({}),
    )];

    let messages = build_full_context_from_entries(&entries);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["content"], "Checking tape state");
    assert_eq!(messages[0]["tool_calls"][0]["id"], "call_123");
}

#[test]
fn test_build_full_context_from_entries_preserves_reasoning_content() {
    let entries = vec![TapeEntry::tool_call_with_assistant_fields(
        vec![json!({
            "id": "call_123",
            "type": "function",
            "function": {"name": "fs_read", "arguments": "{}"}
        })],
        None,
        Some("Need to read the file.".to_owned()),
        json!({}),
    )];

    let messages = build_full_context_from_entries(&entries);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["reasoning_content"], "Need to read the file.");
    assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "fs_read");
}

#[tokio::test]
async fn test_prepare_messages_with_tape_persists_initial_prompt_and_system_prompt() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let initial_round_msgs = build_messages(Some("hello"), None, Some("system"), None);
    llm.persist_initial_messages("test-tape", &initial_round_msgs)
        .await
        .unwrap();

    let messages = llm
        ._prepare_messages(Some("test-tape"), None, &initial_round_msgs)
        .await
        .unwrap();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "hello");

    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape("test-tape"))
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, TapeEntryKind::System);
    assert_eq!(entries[1].kind, TapeEntryKind::Message);
}

#[tokio::test]
async fn test_persist_initial_messages_skips_duplicate_system_prompt() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test-dedup";

    // First call writes system + user.
    let msgs1 = build_messages(Some("hello"), None, Some("system prompt"), None);
    llm.persist_initial_messages(tape, &msgs1).await.unwrap();

    // Second call with same system prompt should NOT duplicate it.
    let msgs2 = build_messages(Some("world"), None, Some("system prompt"), None);
    llm.persist_initial_messages(tape, &msgs2).await.unwrap();

    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();

    let system_count = entries
        .iter()
        .filter(|e| e.kind == TapeEntryKind::System)
        .count();
    assert_eq!(system_count, 1, "system prompt should only appear once");

    let message_count = entries
        .iter()
        .filter(|e| e.kind == TapeEntryKind::Message)
        .count();
    assert_eq!(message_count, 2, "both user messages should be persisted");
}

#[tokio::test]
async fn test_persist_initial_messages_writes_changed_system_prompt() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test-changed";

    let msgs1 = build_messages(Some("hello"), None, Some("prompt v1"), None);
    llm.persist_initial_messages(tape, &msgs1).await.unwrap();

    // Different system prompt should be written.
    let msgs2 = build_messages(Some("world"), None, Some("prompt v2"), None);
    llm.persist_initial_messages(tape, &msgs2).await.unwrap();

    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();

    let system_count = entries
        .iter()
        .filter(|e| e.kind == TapeEntryKind::System)
        .count();
    assert_eq!(system_count, 2, "changed system prompt should be written");
}

#[tokio::test]
async fn test_persist_initial_messages_no_system_in_msgs() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test-no-system";

    // Messages with no system prompt — only user message.
    let msgs = build_messages(Some("hello"), None, None, None);
    llm.persist_initial_messages(tape, &msgs).await.unwrap();

    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();

    assert_eq!(
        entries
            .iter()
            .filter(|e| e.kind == TapeEntryKind::System)
            .count(),
        0
    );
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.kind == TapeEntryKind::Message)
            .count(),
        1
    );
}

#[tokio::test]
async fn test_persist_initial_messages_three_calls_same_prompt() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test-triple";

    for i in 0..3 {
        let msgs = build_messages(Some(&format!("msg {i}")), None, Some("stable system"), None);
        llm.persist_initial_messages(tape, &msgs).await.unwrap();
    }

    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();

    assert_eq!(
        entries
            .iter()
            .filter(|e| e.kind == TapeEntryKind::System)
            .count(),
        1,
        "system prompt should appear exactly once across 3 calls"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.kind == TapeEntryKind::Message)
            .count(),
        3,
        "all 3 user messages should be persisted"
    );
}

#[tokio::test]
async fn test_persist_initial_messages_change_then_revert() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test-revert";

    let msgs_v1 = build_messages(Some("a"), None, Some("v1"), None);
    llm.persist_initial_messages(tape, &msgs_v1).await.unwrap();

    let msgs_v2 = build_messages(Some("b"), None, Some("v2"), None);
    llm.persist_initial_messages(tape, &msgs_v2).await.unwrap();

    // Revert back to v1 — should write again since latest is v2.
    let msgs_v1_again = build_messages(Some("c"), None, Some("v1"), None);
    llm.persist_initial_messages(tape, &msgs_v1_again)
        .await
        .unwrap();

    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();

    assert_eq!(
        entries
            .iter()
            .filter(|e| e.kind == TapeEntryKind::System)
            .count(),
        3,
        "v1 -> v2 -> v1 should produce 3 system entries"
    );
}

#[tokio::test]
async fn test_persist_initial_messages_empty_list() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test-empty";

    llm.persist_initial_messages(tape, &[]).await.unwrap();

    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();
    assert!(entries.is_empty());
}

#[tokio::test]
async fn test_build_full_context_dedup_with_legacy_duplicate_system_entries() {
    // Even if the tape contains duplicate system entries (e.g. from before
    // the dedup fix), build_full_context_from_entries keeps only the last one.
    let meta = json!({});
    let entries = vec![
        TapeEntry::system("prompt v1", meta.clone()),
        TapeEntry::message(json!({"role": "user", "content": "hello"}), meta.clone()),
        TapeEntry::system("prompt v1", meta.clone()), // legacy duplicate
        TapeEntry::message(json!({"role": "user", "content": "world"}), meta),
    ];
    let messages = build_full_context_from_entries(&entries);

    let system_msgs: Vec<_> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
        .collect();
    assert_eq!(
        system_msgs.len(),
        1,
        "build_full_context should keep only the last system"
    );
    assert_eq!(system_msgs[0]["content"], "prompt v1");

    let user_msgs: Vec<_> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .collect();
    assert_eq!(user_msgs.len(), 2, "all user messages should be preserved");
}

#[tokio::test]
async fn test_prepare_messages_no_tape_returns_in_memory() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();

    let in_memory = vec![json!({"role": "user", "content": "hello"})];
    let msgs = llm._prepare_messages(None, None, &in_memory).await.unwrap();

    assert_eq!(msgs, in_memory);
}

// ----- extract_content -----

#[test]
fn test_extract_content_completion_format() {
    let response = json!({
        "choices": [{
            "message": {
                "content": "Hello there!"
            }
        }]
    });
    assert_eq!(extract_content(&response).unwrap(), "Hello there!");
}

#[test]
fn test_extract_content_responses_format() {
    let response = json!({
        "output": [{
            "type": "message",
            "content": [{"text": "Response text"}]
        }]
    });
    assert_eq!(extract_content(&response).unwrap(), "Response text");
}

#[test]
fn test_extract_content_missing() {
    let response = json!({});
    assert!(extract_content(&response).is_err());
}

#[test]
fn test_extract_content_anthropic_empty_content_returns_empty_string() {
    let response = json!({
        "role": "assistant",
        "content": []
    });
    // Empty content array is valid — returns empty string (e.g. tool-use-only response).
    assert_eq!(extract_content(&response).unwrap(), "");
}

// ----- extract_tool_calls -----

#[test]
fn test_extract_tool_calls_completion_format() {
    let response = json!({
        "choices": [{
            "message": {
                "tool_calls": [
                    {"type": "function", "function": {"name": "tool1", "arguments": "{}"}}
                ]
            }
        }]
    });
    let calls = extract_tool_calls(&response).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "tool1");
}

#[test]
fn test_extract_tool_calls_dsml_completion_content() {
    let response = json!({
        "choices": [{
            "message": {
                "content": "<｜DSML｜tool_calls><｜DSML｜invoke name=\"tool1\"></｜DSML｜invoke></｜DSML｜tool_calls>"
            }
        }]
    });

    let calls = extract_tool_calls(&response).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "tool1");
}

#[test]
fn test_extract_tool_calls_responses_format() {
    let response = json!({
        "output": [
            {"type": "function_call", "name": "tool1", "arguments": "{}"},
            {"type": "message", "content": [{"text": "hello"}]},
        ]
    });
    let calls = extract_tool_calls(&response).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "tool1");
}

#[test]
fn test_extract_tool_calls_empty() {
    let response = json!({"choices": [{"message": {"content": "no tools"}}]});
    let calls = extract_tool_calls(&response).unwrap();
    assert!(calls.is_empty());
}

// ----- default_api_base -----

#[test]
fn test_default_api_base_openai() {
    assert_eq!(default_api_base("openai"), "https://api.openai.com/v1");
}

#[test]
fn test_default_api_base_anthropic() {
    assert_eq!(
        default_api_base("anthropic"),
        "https://api.anthropic.com/v1"
    );
}

#[test]
fn test_default_api_base_other() {
    assert_eq!(default_api_base("cohere"), "https://api.cohere.com/v1");
}

#[test]
fn test_default_api_base_deepseek_alias() {
    assert_eq!(
        default_api_base("dsv4"),
        crate::core::provider_policies::DEEPSEEK_OPENAI_BASE
    );
}

// ----- LLMBuilder -----

#[test]
fn test_builder_basic() {
    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test-key")
        .build()
        .unwrap();

    assert_eq!(llm.model(), "gpt-4o");
    assert_eq!(llm.provider(), "openai");
    assert!(llm.fallback_models().is_empty());
    assert!(llm.stream_filter().is_none());
}

#[test]
fn test_builder_with_provider() {
    let llm = LLM::builder()
        .model("gpt-4o")
        .provider("openai")
        .api_key("test-key")
        .build()
        .unwrap();

    assert_eq!(llm.model(), "gpt-4o");
    assert_eq!(llm.provider(), "openai");
}

#[test]
fn test_builder_with_fallback_models() {
    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test-key")
        .fallback_models(vec!["openai:gpt-4o-mini".to_string()])
        .build()
        .unwrap();

    assert_eq!(llm.fallback_models(), &["openai:gpt-4o-mini"]);
}

#[test]
fn test_builder_with_api_format() {
    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test-key")
        .api_format(ApiFormat::Responses)
        .build()
        .unwrap();

    assert_eq!(llm.provider(), "openai");
}

#[test]
fn test_builder_with_verbose() {
    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test-key")
        .verbose(2)
        .build()
        .unwrap();

    assert_eq!(llm.model(), "gpt-4o");
}

#[test]
fn test_builder_rejects_invalid_verbose() {
    let result = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test-key")
        .verbose(5)
        .build();

    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("verbose"));
}

#[test]
fn test_builder_defaults_to_gpt4o_mini() {
    // Default model is "openai:gpt-4o-mini" which includes provider prefix
    let llm = LLM::builder().api_key("test-key").build().unwrap();

    assert_eq!(llm.provider(), "openai");
    assert_eq!(llm.model(), "gpt-4o-mini");
}

#[test]
fn test_builder_with_api_key_resolver() {
    let resolver: APIKeyResolver = Box::new(|provider: &str| {
        if provider == "openai" {
            Some("resolved-key".to_string())
        } else {
            None
        }
    });

    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key_resolver(resolver)
        .build()
        .unwrap();

    assert_eq!(llm.model(), "gpt-4o");
    assert_eq!(llm.provider(), "openai");
}

#[test]
fn test_builder_api_key_resolver_fallback_to_map() {
    // Resolver returns None for "openai", should fall back to map
    let resolver: APIKeyResolver = Box::new(|_provider: &str| None);

    let mut map = HashMap::new();
    map.insert("openai".to_string(), "map-key".to_string());

    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key_resolver(resolver)
        .api_key_map(map)
        .build()
        .unwrap();

    assert_eq!(llm.model(), "gpt-4o");
}

#[test]
fn test_builder_explicit_key_overrides_resolver() {
    let resolver: APIKeyResolver = Box::new(|_provider: &str| Some("resolver-key".to_string()));

    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("explicit-key")
        .api_key_resolver(resolver)
        .build()
        .unwrap();

    // Explicit key takes priority
    assert_eq!(llm.model(), "gpt-4o");
}

#[test]
fn test_builder_with_stream_filter() {
    let filter: StreamEventFilter = Arc::new(|event| Some(event));

    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test-key")
        .stream_filter(filter)
        .build()
        .unwrap();

    assert!(llm.stream_filter().is_some());
}

#[test]
fn test_builder_with_max_retries() {
    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test-key")
        .max_retries(5)
        .build()
        .unwrap();

    assert_eq!(llm.model(), "gpt-4o");
}

#[test]
fn test_builder_with_api_base() {
    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test-key")
        .api_base("https://custom.api.com/v1")
        .build()
        .unwrap();

    assert_eq!(llm.model(), "gpt-4o");
}

#[test]
fn test_builder_default() {
    // LLMBuilder::default() should be equivalent to LLMBuilder::new()
    let _builder = LLMBuilder::default();
}

// ----- ProviderRegistry via LLMBuilder -----

#[test]
fn test_builder_register_provider() {
    use crate::core::provider_registry::ProviderConfig;

    let llm = LLM::builder()
        .model("my-llm:custom-model")
        .api_key("test-key")
        .register_provider(
            "my-llm",
            ProviderConfig::new("https://api.my-llm.example.com/v1", ApiFormat::Completion),
        )
        .build()
        .unwrap();

    assert_eq!(llm.provider(), "my-llm");
    assert_eq!(llm.model(), "custom-model");

    // The registry on the core should resolve the custom provider.
    let reg = llm.core.provider_registry();
    let cfg = reg
        .get("my-llm")
        .expect("custom provider should be registered");
    assert_eq!(cfg.api_base, "https://api.my-llm.example.com/v1");
    assert_eq!(cfg.api_format, ApiFormat::Completion);
}

#[test]
fn test_builder_provider_registry_replaces_defaults() {
    use crate::core::provider_registry::{ProviderConfig, ProviderRegistry};

    let mut custom_reg = ProviderRegistry::new();
    custom_reg.register(
        "special",
        ProviderConfig::new("https://special.example.com/v1", ApiFormat::Responses),
    );

    let llm = LLM::builder()
        .model("special:some-model")
        .api_key("test-key")
        .provider_registry(custom_reg)
        .build()
        .unwrap();

    let reg = llm.core.provider_registry();
    assert!(reg.get("special").is_some());
    // Built-in providers are still present because ProviderRegistry::new() seeds them.
    assert!(reg.get("openai").is_some());
}

#[test]
fn test_builder_register_provider_with_custom_headers() {
    use crate::core::provider_registry::ProviderConfig;

    let llm = LLM::builder()
        .model("custom:my-model")
        .api_key("test-key")
        .register_provider(
            "custom",
            ProviderConfig::new("https://api.custom.example.com/v1", ApiFormat::Completion)
                .with_header("X-Custom-Auth", "token-123"),
        )
        .build()
        .unwrap();

    let cfg = llm
        .core
        .provider_registry()
        .get("custom")
        .expect("custom provider should exist");
    assert_eq!(
        cfg.custom_headers.get("X-Custom-Auth").unwrap(),
        "token-123"
    );
}

// ----- StreamEventFilter -----

#[test]
fn test_stream_filter_drops_events() {
    // Filter that drops all Text events
    let filter: StreamEventFilter = Arc::new(|event| {
        if event.kind == StreamEventKind::Text {
            None
        } else {
            Some(event)
        }
    });

    let text_event = StreamEvent::new(StreamEventKind::Text, json!({"delta": "hello"}));
    let usage_event = StreamEvent::new(StreamEventKind::Usage, json!({"tokens": 42}));

    assert!(filter(text_event).is_none());
    assert!(filter(usage_event).is_some());
}

#[test]
fn test_stream_filter_transforms_events() {
    // Filter that uppercases text deltas
    let filter: StreamEventFilter = Arc::new(|mut event| {
        if event.kind == StreamEventKind::Text {
            if let Some(delta) = event.data.get("delta").and_then(|d| d.as_str()) {
                event.data = json!({"delta": delta.to_uppercase()});
            }
        }
        Some(event)
    });

    let event = StreamEvent::new(StreamEventKind::Text, json!({"delta": "hello"}));
    let result = filter(event).unwrap();
    assert_eq!(result.data["delta"], "HELLO");
}

#[test]
fn test_stream_filter_passthrough() {
    let filter: StreamEventFilter = Arc::new(|event| Some(event));

    let event = StreamEvent::new(StreamEventKind::Final, json!({"ok": true}));
    let result = filter(event);
    assert!(result.is_some());
    let result = result.unwrap();
    assert_eq!(result.kind, StreamEventKind::Final);
}

#[test]
fn test_with_stream_filter_set_and_clear() {
    let mut llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test-key")
        .build()
        .unwrap();

    assert!(llm.stream_filter().is_none());

    let filter: StreamEventFilter = Arc::new(|event| Some(event));
    llm.with_stream_filter(filter);
    assert!(llm.stream_filter().is_some());

    llm.clear_stream_filter();
    assert!(llm.stream_filter().is_none());
}

// ----- LLM::responses URL/body building -----

#[test]
fn test_responses_url_default_provider() {
    // Verify the URL is built correctly from the default provider base
    let base = default_api_base("openai");
    let url = format!("{}/responses", base.trim_end_matches('/'));
    assert_eq!(url, "https://api.openai.com/v1/responses");
}

#[test]
fn test_responses_url_custom_base() {
    let base = "https://custom.api.com/v2/";
    let url = format!("{}/responses", base.trim_end_matches('/'));
    assert_eq!(url, "https://custom.api.com/v2/responses");
}

#[test]
fn test_responses_url_anthropic() {
    let base = default_api_base("anthropic");
    let url = format!("{}/responses", base.trim_end_matches('/'));
    assert_eq!(url, "https://api.anthropic.com/v1/responses");
}

#[test]
fn test_responses_body_structure() {
    // Verify the body JSON structure matches what responses() would build
    let input = json!("Tell me a joke");
    let model = "gpt-4o";
    let body = json!({
        "model": model,
        "input": input,
    });
    assert_eq!(body["model"], "gpt-4o");
    assert_eq!(body["input"], "Tell me a joke");
}

#[test]
fn test_responses_body_with_array_input() {
    let input = json!([
        {"role": "user", "content": "Hello"},
        {"role": "assistant", "content": "Hi there"},
    ]);
    let body = json!({
        "model": "gpt-4o",
        "input": input,
    });
    assert!(body["input"].is_array());
    assert_eq!(body["input"].as_array().unwrap().len(), 2);
}

// ----- Decision functions -----

#[test]
fn test_collect_active_decisions_basic() {
    let meta = json!({});
    let entries = vec![
        TapeEntry::decision("Use PostgreSQL", meta.clone()),
        TapeEntry::decision("API-first design", meta.clone()),
    ];
    let decisions = collect_active_decisions(&entries);
    assert_eq!(decisions, vec!["Use PostgreSQL", "API-first design"]);
}

#[test]
fn test_collect_active_decisions_with_revocation() {
    let meta = json!({});
    let entries = vec![
        TapeEntry::decision("Use PostgreSQL", meta.clone()),
        TapeEntry::decision("API-first design", meta.clone()),
        TapeEntry::decision_revoked("Use PostgreSQL", meta.clone()),
    ];
    let decisions = collect_active_decisions(&entries);
    assert_eq!(decisions, vec!["API-first design"]);
}

#[test]
fn test_collect_active_decisions_empty() {
    let decisions = collect_active_decisions(&[]);
    assert!(decisions.is_empty());
}

#[test]
fn test_collect_active_decisions_skips_empty_text() {
    let meta = json!({});
    let entries = vec![
        TapeEntry::decision("", meta.clone()),
        TapeEntry::decision("Real decision", meta.clone()),
    ];
    let decisions = collect_active_decisions(&entries);
    assert_eq!(decisions, vec!["Real decision"]);
}

#[test]
fn test_collect_active_decisions_duplicate_text() {
    let meta = json!({});
    let entries = vec![
        TapeEntry::decision("Use PostgreSQL", meta.clone()),
        TapeEntry::decision("Use PostgreSQL", meta.clone()),
    ];
    let decisions = collect_active_decisions(&entries);
    // Both kept — dedup is caller's concern
    assert_eq!(decisions.len(), 2);
}

#[test]
fn test_inject_decisions_into_system_prompt() {
    let mut messages = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "Hello"}),
    ];
    inject_decisions_into_system_prompt(
        &mut messages,
        &["Use PostgreSQL".to_string(), "API-first".to_string()],
    );
    let system_content = messages[0]["content"].as_str().unwrap();
    assert!(system_content.contains("Active decisions:"));
    assert!(system_content.contains("1. Use PostgreSQL"));
    assert!(system_content.contains("2. API-first"));
    assert!(system_content.starts_with("You are helpful."));
}

#[test]
fn test_inject_decisions_no_system_message() {
    let mut messages = vec![json!({"role": "user", "content": "Hello"})];
    inject_decisions_into_system_prompt(&mut messages, &["Use PostgreSQL".to_string()]);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("1. Use PostgreSQL")
    );
}

#[test]
fn test_inject_decisions_empty_list() {
    let mut messages = vec![json!({"role": "system", "content": "Original"})];
    inject_decisions_into_system_prompt(&mut messages, &[]);
    assert_eq!(messages[0]["content"], "Original");
}

#[test]
fn test_decisions_survive_full_context_build() {
    // Decisions should NOT appear in build_full_context_from_entries output
    // (they are injected separately via full-tape scan)
    let meta = json!({});
    let entries = vec![
        TapeEntry::decision("Use PostgreSQL", meta.clone()),
        TapeEntry::message(json!({"role": "user", "content": "Hello"}), meta.clone()),
    ];
    let messages = build_full_context_from_entries(&entries);
    // Decision entries are skipped by build_full_context (kind is not message/system/tool_*)
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["content"], "Hello");
}

// ---------------------------------------------------------------------------
// Multimodal / vision: end-to-end through build_messages + normalize
// ---------------------------------------------------------------------------

#[test]
fn test_build_messages_with_user_content_parts() {
    let parts = vec![
        json!({"type": "text", "text": "What animal is this?"}),
        json!({"type": "image_base64", "mime_type": "image/jpeg", "data": "AQID"}),
    ];
    let msgs = build_messages(None, Some(&parts), Some("You are helpful"), None);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[1]["role"], "user");
    let content = msgs[1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_base64");
}

#[test]
fn test_build_messages_user_content_takes_precedence_over_prompt() {
    let parts = vec![json!({"type": "text", "text": "from parts"})];
    let msgs = build_messages(Some("from prompt"), Some(&parts), None, None);
    // user_content should win — only one user message, with content array
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0]["content"].is_array());
    assert_eq!(msgs[0]["content"][0]["text"], "from parts");
}

#[test]
fn test_e2e_image_normalized_to_anthropic() {
    use crate::clients::parsing::TransportKind;
    use crate::core::message_norm::normalize_messages_for_api;

    let parts = vec![
        json!({"type": "text", "text": "describe this photo"}),
        json!({"type": "image_base64", "mime_type": "image/png", "data": "iVBOR"}),
    ];
    let msgs = build_messages(None, Some(&parts), None, None);
    let normalized = normalize_messages_for_api(msgs, TransportKind::Messages);

    let content = normalized[0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(content[1]["source"]["data"], "iVBOR");
}

#[test]
fn test_e2e_image_normalized_to_openai() {
    use crate::clients::parsing::TransportKind;
    use crate::core::message_norm::normalize_messages_for_api;

    let parts = vec![
        json!({"type": "text", "text": "describe"}),
        json!({"type": "image_base64", "mime_type": "image/jpeg", "data": "/9j/4A"}),
    ];
    let msgs = build_messages(None, Some(&parts), None, None);
    let normalized = normalize_messages_for_api(msgs, TransportKind::Completion);

    let content = normalized[0]["content"].as_array().unwrap();
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"],
        "data:image/jpeg;base64,/9j/4A"
    );
}

#[test]
fn test_build_messages_multiple_images() {
    let parts = vec![
        json!({"type": "text", "text": "compare"}),
        json!({"type": "image_base64", "mime_type": "image/png", "data": "A"}),
        json!({"type": "image_base64", "mime_type": "image/jpeg", "data": "B"}),
    ];
    let msgs = build_messages(None, Some(&parts), None, None);
    assert_eq!(msgs.len(), 1);
    let content = msgs[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
}

#[test]
fn test_build_messages_image_only_no_text() {
    let parts = vec![json!({"type": "image_base64", "mime_type": "image/png", "data": "ONLY"})];
    let msgs = build_messages(None, Some(&parts), None, None);
    assert_eq!(msgs.len(), 1);
    let content = msgs[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "image_base64");
}

#[test]
fn test_build_messages_user_content_with_existing_messages() {
    let existing = vec![
        json!({"role": "user", "content": "first message"}),
        json!({"role": "assistant", "content": "ok"}),
    ];
    let parts = vec![
        json!({"type": "text", "text": "follow up with image"}),
        json!({"type": "image_base64", "mime_type": "image/png", "data": "IMG"}),
    ];
    let msgs = build_messages(None, Some(&parts), Some("system"), Some(&existing));
    assert_eq!(msgs.len(), 4); // system + 2 existing + user_content
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[1]["content"], "first message");
    assert_eq!(msgs[2]["content"], "ok");
    assert!(msgs[3]["content"].is_array());
}

#[test]
fn test_build_messages_neither_prompt_nor_user_content() {
    let msgs = build_messages(None, None, Some("system"), None);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"], "system");
}

#[test]
fn test_e2e_multiple_images_anthropic() {
    use crate::clients::parsing::TransportKind;
    use crate::core::message_norm::normalize_messages_for_api;

    let parts = vec![
        json!({"type": "text", "text": "compare these two"}),
        json!({"type": "image_base64", "mime_type": "image/png", "data": "IMG1"}),
        json!({"type": "image_base64", "mime_type": "image/jpeg", "data": "IMG2"}),
    ];
    let msgs = build_messages(None, Some(&parts), None, None);
    let normalized = normalize_messages_for_api(msgs, TransportKind::Messages);

    let content = normalized[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(content[2]["type"], "image");
    assert_eq!(content[2]["source"]["media_type"], "image/jpeg");
}

#[test]
fn test_e2e_image_with_history_and_system() {
    use crate::clients::parsing::TransportKind;
    use crate::core::message_norm::normalize_messages_for_api;

    let history = vec![
        json!({"role": "user", "content": "hello"}),
        json!({"role": "assistant", "content": "hi there"}),
    ];
    let parts = vec![
        json!({"type": "text", "text": "now look at this"}),
        json!({"type": "image_base64", "mime_type": "image/png", "data": "PIC"}),
    ];
    let msgs = build_messages(None, Some(&parts), Some("you are helpful"), Some(&history));
    let normalized = normalize_messages_for_api(msgs, TransportKind::Messages);

    assert_eq!(normalized.len(), 4);
    assert_eq!(normalized[0]["role"], "system");
    assert_eq!(normalized[1]["content"], "hello");
    assert_eq!(normalized[2]["content"], "hi there");
    let user_content = normalized[3]["content"].as_array().unwrap();
    assert_eq!(user_content[1]["type"], "image");
}

// ===== Image stripping for persistence =====

#[test]
fn test_strip_image_blocks_replaces_image_base64() {
    let msg = json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "look at this"},
            {"type": "image_base64", "mime_type": "image/jpeg", "data": "AAAA"},
        ]
    });
    let stripped = strip_image_blocks_for_persistence(&msg);
    let content = stripped["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "look at this");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "[image: image_0.jpeg]");
}

#[test]
fn test_strip_image_blocks_replaces_image_type() {
    let msg = json!({
        "role": "user",
        "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "BBB"}},
        ]
    });
    let stripped = strip_image_blocks_for_persistence(&msg);
    let content = stripped["content"].as_array().unwrap();
    assert_eq!(content[0]["text"], "[image: image_0.png]");
}

#[test]
fn test_strip_image_blocks_replaces_image_url() {
    let msg = json!({
        "role": "user",
        "content": [
            {"type": "image_url", "image_url": {"url": "data:image/gif;base64,AAA"}},
        ]
    });
    let stripped = strip_image_blocks_for_persistence(&msg);
    let content = stripped["content"].as_array().unwrap();
    // No mime_type field → falls back to image/png → .png extension
    assert_eq!(content[0]["text"], "[image: image_0.png]");
}

#[test]
fn test_strip_image_blocks_multiple_images_get_indexed() {
    let msg = json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "two pics:"},
            {"type": "image_base64", "mime_type": "image/jpeg", "data": "A"},
            {"type": "text", "text": "and"},
            {"type": "image_base64", "mime_type": "image/webp", "data": "B"},
        ]
    });
    let stripped = strip_image_blocks_for_persistence(&msg);
    let content = stripped["content"].as_array().unwrap();
    assert_eq!(content.len(), 4);
    assert_eq!(content[0]["text"], "two pics:");
    assert_eq!(content[1]["text"], "[image: image_0.jpeg]");
    assert_eq!(content[2]["text"], "and");
    assert_eq!(content[3]["text"], "[image: image_1.webp]");
}

#[test]
fn test_strip_image_blocks_ignores_assistant_messages() {
    let msg = json!({
        "role": "assistant",
        "content": [
            {"type": "image_base64", "mime_type": "image/png", "data": "X"},
        ]
    });
    let stripped = strip_image_blocks_for_persistence(&msg);
    // Should be identical — assistant messages pass through
    assert_eq!(stripped, msg);
}

#[test]
fn test_strip_image_blocks_passes_through_string_content() {
    let msg = json!({"role": "user", "content": "just text, no images"});
    let stripped = strip_image_blocks_for_persistence(&msg);
    assert_eq!(stripped, msg);
}

#[test]
fn test_strip_image_blocks_preserves_other_fields() {
    let msg = json!({
        "role": "user",
        "name": "alice",
        "content": [
            {"type": "text", "text": "hi"},
            {"type": "image_base64", "mime_type": "image/jpeg", "data": "X"},
        ]
    });
    let stripped = strip_image_blocks_for_persistence(&msg);
    assert_eq!(stripped["role"], "user");
    assert_eq!(stripped["name"], "alice");
}

// ===== Spill reversibility =====

#[test]
fn test_spill_file_contains_exact_original_content() {
    use crate::tape::spill::{self, DEFAULT_SPILL};
    let dir = tempfile::tempdir().unwrap();
    // Build content that's larger than threshold (500 chars)
    let original: String = (0..100)
        .map(|i| format!("line {i}: some data here"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(original.len() > DEFAULT_SPILL.threshold_chars);

    let truncated = spill::spill_if_needed(&original, "call_42", dir.path(), &DEFAULT_SPILL)
        .unwrap()
        .expect("should spill");

    // Truncated version is shorter
    assert!(truncated.len() < original.len());

    // File on disk contains the EXACT original — byte-for-byte reversible
    let recovered = std::fs::read_to_string(dir.path().join("call_42.txt")).unwrap();
    assert_eq!(recovered, original);
}

#[test]
fn test_spill_reversibility_with_unicode() {
    use crate::tape::spill::{self, SpillConfig};
    let dir = tempfile::tempdir().unwrap();
    let config = SpillConfig {
        threshold_chars: 10,
        head_lines: 2,
        tail_lines: 1,
    };
    let original = "你好世界\n日本語テスト\nрусский\n한국어\n🎉🎊🎈";
    let _ = spill::spill_if_needed(original, "unicode", dir.path(), &config)
        .unwrap()
        .expect("should spill");

    let recovered = std::fs::read_to_string(dir.path().join("unicode.txt")).unwrap();
    assert_eq!(recovered, original);
}

#[test]
fn test_spill_at_exact_threshold_does_not_spill() {
    use crate::tape::spill::{self, SpillConfig};
    let dir = tempfile::tempdir().unwrap();
    let config = SpillConfig {
        threshold_chars: 10,
        head_lines: 5,
        tail_lines: 2,
    };
    let content = "0123456789"; // exactly 10 chars
    let result = spill::spill_if_needed(content, "exact", dir.path(), &config).unwrap();
    assert!(result.is_none(), "should not spill at exact threshold");
}

#[test]
fn test_spill_one_over_threshold_does_spill() {
    use crate::tape::spill::{self, SpillConfig};
    let dir = tempfile::tempdir().unwrap();
    let config = SpillConfig {
        threshold_chars: 10,
        head_lines: 5,
        tail_lines: 2,
    };
    let content = "01234567890"; // 11 chars
    let result = spill::spill_if_needed(content, "over", dir.path(), &config).unwrap();
    assert!(result.is_some(), "should spill one over threshold");

    let recovered = std::fs::read_to_string(dir.path().join("over.txt")).unwrap();
    assert_eq!(recovered, content);
}

// ===== LLM spill integration =====

fn make_spill_llm(spill_dir: std::path::PathBuf) -> LLM {
    LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test")
        .spill_dir(spill_dir)
        .build()
        .unwrap()
}

#[test]
fn test_maybe_spill_result_small_passes_through() {
    let dir = tempfile::tempdir().unwrap();
    let llm = make_spill_llm(dir.path().to_path_buf());
    let result = json!("small output");
    let spilled = llm.maybe_spill_result(&result, "tape__session", "call_1");
    assert_eq!(spilled, result);
}

#[test]
fn test_maybe_spill_result_large_truncates_and_saves() {
    let dir = tempfile::tempdir().unwrap();
    let llm = make_spill_llm(dir.path().to_path_buf());
    let large: String = (0..100)
        .map(|i| format!("result line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let result = json!(large);
    let spilled = llm.maybe_spill_result(&result, "tape__session", "call_big");

    // Truncated
    let spilled_str = spilled.as_str().unwrap();
    assert!(spilled_str.len() < large.len());
    assert!(spilled_str.contains("omitted"));
    assert!(spilled_str.contains("call_big.txt"));

    // Reversible: file on disk has exact original
    let spill_file = dir.path().join("tape__session.d").join("call_big.txt");
    assert!(spill_file.exists());
    assert_eq!(std::fs::read_to_string(&spill_file).unwrap(), large);
}

#[test]
fn test_maybe_spill_result_non_string_passes_through() {
    let dir = tempfile::tempdir().unwrap();
    let llm = make_spill_llm(dir.path().to_path_buf());
    let result = json!({"key": "value", "num": 42});
    let spilled = llm.maybe_spill_result(&result, "tape__sess", "call_obj");
    assert_eq!(spilled, result);
}

#[test]
fn test_maybe_spill_no_spill_dir_passes_through() {
    let llm = LLM::builder()
        .model("openai:gpt-4o")
        .api_key("test")
        .build()
        .unwrap();
    let large: String = (0..100)
        .map(|i| format!("L{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let result = json!(large);
    let spilled = llm.maybe_spill_result(&result, "tape", "call");
    assert_eq!(spilled, result, "no spill_dir → pass through unchanged");
}

#[test]
fn test_spill_tool_call_args_large() {
    let dir = tempfile::tempdir().unwrap();
    let llm = make_spill_llm(dir.path().to_path_buf());

    let large_args: String = (0..100)
        .map(|i| format!("arg line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let call = json!({
        "id": "call_write",
        "type": "function",
        "function": {
            "name": "fs.write",
            "arguments": large_args,
        }
    });
    let spilled = llm.maybe_spill_tool_call(&call, "tape__sess");

    // Structure preserved
    assert_eq!(spilled["id"], "call_write");
    assert_eq!(spilled["function"]["name"], "fs.write");

    // Arguments truncated
    let spilled_args = spilled["function"]["arguments"].as_str().unwrap();
    assert!(spilled_args.len() < large_args.len());
    assert!(spilled_args.contains("omitted"));

    // Reversible
    let args_file = dir.path().join("tape__sess.d").join("call_write.args.txt");
    assert!(args_file.exists());
    assert_eq!(std::fs::read_to_string(&args_file).unwrap(), large_args);
}

#[test]
fn test_spill_tool_call_args_small_passes_through() {
    let dir = tempfile::tempdir().unwrap();
    let llm = make_spill_llm(dir.path().to_path_buf());

    let call = json!({
        "id": "call_small",
        "type": "function",
        "function": {
            "name": "tape.info",
            "arguments": "{\"tape\": \"main\"}",
        }
    });
    let spilled = llm.maybe_spill_tool_call(&call, "tape__sess");
    assert_eq!(spilled, call, "small args should not be spilled");
}

#[test]
fn test_spill_both_args_and_result_coexist() {
    let dir = tempfile::tempdir().unwrap();
    let llm = make_spill_llm(dir.path().to_path_buf());
    let tape = "test__both";

    let large_args: String = (0..80)
        .map(|i| format!("arg {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let large_result: String = (0..80)
        .map(|i| format!("result {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    let call = json!({
        "id": "call_x",
        "type": "function",
        "function": {"name": "test", "arguments": large_args}
    });
    let _ = llm.maybe_spill_tool_call(&call, tape);
    let _ = llm.maybe_spill_result(&json!(large_result), tape, "call_x");

    // Both files exist in the same .d directory
    let spill_dir = dir.path().join("test__both.d");
    let args_file = spill_dir.join("call_x.args.txt");
    let result_file = spill_dir.join("call_x.txt");
    assert!(args_file.exists());
    assert!(result_file.exists());

    // Both are fully reversible
    assert_eq!(std::fs::read_to_string(&args_file).unwrap(), large_args);
    assert_eq!(std::fs::read_to_string(&result_file).unwrap(), large_result);
}

#[test]
fn test_spill_dir_named_after_tape() {
    use crate::tape::spill::spill_dir_for_tape;
    let base = std::path::Path::new("/home/user/.eli/tapes");
    let dir = spill_dir_for_tape(base, "workspace___session");
    assert_eq!(
        dir,
        std::path::PathBuf::from("/home/user/.eli/tapes/workspace___session.d")
    );
}

#[test]
fn test_spill_truncated_output_has_head_and_tail() {
    use crate::tape::spill::{self, SpillConfig};
    let dir = tempfile::tempdir().unwrap();
    let config = SpillConfig {
        threshold_chars: 10,
        head_lines: 3,
        tail_lines: 2,
    };
    let content = "H1\nH2\nH3\nM1\nM2\nM3\nT1\nT2";
    let truncated = spill::spill_if_needed(content, "c", dir.path(), &config)
        .unwrap()
        .unwrap();

    // Head lines present
    assert!(truncated.contains("H1"));
    assert!(truncated.contains("H2"));
    assert!(truncated.contains("H3"));
    // Middle omitted
    assert!(!truncated.contains("M1"));
    assert!(!truncated.contains("M2"));
    assert!(!truncated.contains("M3"));
    // Tail lines present
    assert!(truncated.contains("T1"));
    assert!(truncated.contains("T2"));
    // Omission notice
    assert!(truncated.contains("3 lines,"));
    assert!(truncated.contains("chars omitted"));
    // File path reference
    assert!(truncated.contains("c.txt"));
}

// ===== E2E: image persistence round-trip =====

#[tokio::test]
async fn test_e2e_image_stripped_in_tape_but_full_in_memory() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test__img_roundtrip";

    // Multimodal user message with image
    let user_content = vec![
        json!({"type": "text", "text": "what is this?"}),
        json!({"type": "image_base64", "mime_type": "image/png", "data": "iVBORw0KGgo="}),
    ];
    let initial = build_messages(None, Some(&user_content), Some("you are helpful"), None);
    llm.persist_initial_messages(tape, &initial).await.unwrap();

    // Tape should have placeholder, not base64
    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();
    let user_entry = entries
        .iter()
        .find(|e| {
            e.kind == TapeEntryKind::Message
                && e.payload.get("role").and_then(|r| r.as_str()) == Some("user")
        })
        .unwrap();
    let tape_content = user_entry.payload["content"].as_array().unwrap();
    // Image block replaced with placeholder text
    assert_eq!(tape_content[1]["type"], "text");
    assert!(
        tape_content[1]["text"]
            .as_str()
            .unwrap()
            .contains("[image:")
    );
    // No base64 data in tape
    let tape_json = serde_json::to_string(&user_entry.payload).unwrap();
    assert!(!tape_json.contains("iVBORw0KGgo="));

    // But in_memory_msgs (from run_tools init) should have full image.
    // Simulate what run_tools does: build_tape_messages + prepend
    let tape_msgs = llm.build_tape_messages(tape, None).await;
    let mut in_memory = initial.clone();
    prepend_tape_history(&mut in_memory, tape_msgs);
    // The last user message should have the original multimodal content
    let last_user = in_memory
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .unwrap();
    let content = last_user["content"].as_array().unwrap();
    assert_eq!(content[1]["type"], "image_base64");
    assert_eq!(content[1]["data"], "iVBORw0KGgo=");
}

#[tokio::test]
async fn test_e2e_next_turn_sees_placeholder_not_image() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test__img_next_turn";

    // Turn 1: user sends image
    let user_content = vec![
        json!({"type": "text", "text": "describe this"}),
        json!({"type": "image_base64", "mime_type": "image/jpeg", "data": "BIGBASE64DATA"}),
    ];
    let initial1 = build_messages(None, Some(&user_content), Some("system"), None);
    llm.persist_initial_messages(tape, &initial1).await.unwrap();

    // Simulate assistant response
    let meta = json!({"run_id": "r1"});
    let assistant = json!({"role": "assistant", "content": "I see a cat"});
    llm.async_tape
        .append_entry(tape, &TapeEntry::message(assistant, meta))
        .await
        .unwrap();

    // Turn 2: user sends text only
    let initial2 = build_messages(Some("what color is it?"), None, Some("system"), None);
    llm.persist_initial_messages(tape, &initial2).await.unwrap();

    // Build context for turn 2 — should see placeholder, not image
    let tape_msgs = llm.build_tape_messages(tape, None).await;
    let mut context = initial2.clone();
    prepend_tape_history(&mut context, tape_msgs);

    // Find the old user message (turn 1)
    let user_msgs: Vec<_> = context
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .collect();
    assert!(user_msgs.len() >= 2);
    // First user message should have placeholder, not base64
    let first_user_content = &user_msgs[0]["content"];
    let full_json = serde_json::to_string(first_user_content).unwrap();
    assert!(
        !full_json.contains("BIGBASE64DATA"),
        "old image should not be in context"
    );
    assert!(full_json.contains("[image:"), "should have placeholder");
}

// ===== E2E: tool result spill round-trip =====

#[tokio::test]
async fn test_e2e_spill_tool_result_in_tape_full_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .spill_dir(dir.path())
        .build()
        .unwrap();
    let tape = "test__spill_roundtrip";

    let large_result: String = (0..200)
        .map(|i| format!("line {i}: data here"))
        .collect::<Vec<_>>()
        .join("\n");

    let execution = ToolExecution {
        tool_calls: vec![json!({
            "id": "call_42",
            "type": "function",
            "function": {"name": "fs.read", "arguments": "{\"path\": \"big.txt\"}"}
        })],
        tool_results: vec![json!(large_result)],
        error: None,
    };
    let response = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_42",
                    "type": "function",
                    "function": {"name": "fs.read", "arguments": "{\"path\": \"big.txt\"}"}
                }]
            }
        }]
    });

    let mut in_memory = vec![
        json!({"role": "system", "content": "system"}),
        json!({"role": "user", "content": "read big.txt"}),
    ];
    llm._persist_round(Some(tape), &response, &execution, &mut in_memory)
        .await
        .unwrap();

    // in_memory should have FULL result
    let tool_msg = in_memory
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .unwrap();
    assert_eq!(tool_msg["content"].as_str().unwrap(), large_result);

    // Tape should have SPILLED (truncated) result
    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();
    let result_entry = entries
        .iter()
        .find(|e| e.kind == TapeEntryKind::ToolResult)
        .unwrap();
    let tape_output = result_entry.payload["results"][0]["output"]
        .as_str()
        .unwrap();
    assert!(tape_output.len() < large_result.len());
    assert!(tape_output.contains("chars omitted"));

    // Spill file should have full content
    let spill_file = dir.path().join("test__spill_roundtrip.d/call_42.txt");
    assert!(spill_file.exists());
    assert_eq!(std::fs::read_to_string(&spill_file).unwrap(), large_result);
}

#[tokio::test]
async fn test_e2e_spill_tool_args_in_tape_full_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .spill_dir(dir.path())
        .build()
        .unwrap();
    let tape = "test__spill_args";

    let large_args: String = (0..200)
        .map(|i| format!("content line {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    let execution = ToolExecution {
        tool_calls: vec![json!({
            "id": "call_w1",
            "type": "function",
            "function": {"name": "fs.write", "arguments": large_args}
        })],
        tool_results: vec![json!("ok")],
        error: None,
    };
    let response = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_w1",
                    "type": "function",
                    "function": {"name": "fs.write", "arguments": large_args}
                }]
            }
        }]
    });

    let mut in_memory = vec![json!({"role": "user", "content": "write file"})];
    llm._persist_round(Some(tape), &response, &execution, &mut in_memory)
        .await
        .unwrap();

    // in_memory should have FULL args in assistant message
    let assistant = in_memory
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        .unwrap();
    let mem_args = assistant["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(mem_args, large_args);

    // Tape should have SPILLED args
    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();
    let call_entry = entries
        .iter()
        .find(|e| e.kind == TapeEntryKind::ToolCall)
        .unwrap();
    let tape_args = call_entry.payload["calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert!(tape_args.len() < large_args.len());
    assert!(tape_args.contains("chars omitted"));

    // Spill file has full args
    let spill_file = dir.path().join("test__spill_args.d/call_w1.args.txt");
    assert!(spill_file.exists());
    assert_eq!(std::fs::read_to_string(&spill_file).unwrap(), large_args);
}

// ===== Edge cases =====

#[tokio::test]
async fn test_e2e_small_tool_result_not_spilled() {
    let dir = tempfile::tempdir().unwrap();
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .spill_dir(dir.path())
        .build()
        .unwrap();
    let tape = "test__small_result";

    let execution = ToolExecution {
        tool_calls: vec![json!({
            "id": "call_s",
            "type": "function",
            "function": {"name": "tape.info", "arguments": "{}"}
        })],
        tool_results: vec![json!("small output")],
        error: None,
    };
    let response = json!({
        "choices": [{"message": {"role": "assistant", "tool_calls": [{
            "id": "call_s", "type": "function",
            "function": {"name": "tape.info", "arguments": "{}"}
        }]}}]
    });

    let mut in_memory = vec![];
    llm._persist_round(Some(tape), &response, &execution, &mut in_memory)
        .await
        .unwrap();

    // No spill file created
    let spill_dir = dir.path().join("test__small_result.d");
    assert!(!spill_dir.exists());

    // Tape has exact same content as in-memory
    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();
    let result_entry = entries
        .iter()
        .find(|e| e.kind == TapeEntryKind::ToolResult)
        .unwrap();
    let tape_output = result_entry.payload["results"][0]["output"]
        .as_str()
        .unwrap();
    assert_eq!(tape_output, "small output");
}

#[tokio::test]
async fn test_e2e_no_spill_dir_passes_through() {
    // LLM without spill_dir — everything stored as-is
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test__no_spill";

    let large: String = (0..200)
        .map(|i| format!("L{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let execution = ToolExecution {
        tool_calls: vec![json!({
            "id": "call_ns",
            "type": "function",
            "function": {"name": "test", "arguments": "{}"}
        })],
        tool_results: vec![json!(large)],
        error: None,
    };
    let response = json!({
        "choices": [{"message": {"role": "assistant", "tool_calls": [{
            "id": "call_ns", "type": "function",
            "function": {"name": "test", "arguments": "{}"}
        }]}}]
    });

    let mut in_memory = vec![];
    llm._persist_round(Some(tape), &response, &execution, &mut in_memory)
        .await
        .unwrap();

    // Without spill_dir, tape stores FULL content
    let entries = llm
        .async_tape
        .fetch_entries(&llm.async_tape.query_tape(tape))
        .await
        .unwrap();
    let result_entry = entries
        .iter()
        .find(|e| e.kind == TapeEntryKind::ToolResult)
        .unwrap();
    let tape_output = result_entry.payload["results"][0]["output"]
        .as_str()
        .unwrap();
    assert_eq!(
        tape_output, large,
        "without spill_dir, tape has full content"
    );
}

#[test]
fn test_strip_images_text_only_message_unchanged() {
    // Edge: user message with only text blocks (no images)
    let msg = json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "hello"},
            {"type": "text", "text": "world"},
        ]
    });
    let stripped = strip_image_blocks_for_persistence(&msg);
    assert_eq!(stripped, msg, "text-only multimodal should be unchanged");
}

#[test]
fn test_strip_images_empty_content_array() {
    let msg = json!({"role": "user", "content": []});
    let stripped = strip_image_blocks_for_persistence(&msg);
    assert_eq!(stripped["content"].as_array().unwrap().len(), 0);
}

#[test]
fn test_strip_images_system_message_untouched() {
    let msg = json!({
        "role": "system",
        "content": [
            {"type": "image_base64", "mime_type": "image/png", "data": "X"},
        ]
    });
    let stripped = strip_image_blocks_for_persistence(&msg);
    assert_eq!(stripped, msg, "system messages should not be stripped");
}

#[tokio::test]
async fn test_e2e_multiple_tool_rounds_in_memory_accumulates() {
    let llm = LLM::builder()
        .tape_store(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))
        .build()
        .unwrap();
    let tape = "test__multi_round";

    // Simulate two tool rounds
    let mut in_memory = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "do things"}),
    ];

    for i in 0..2 {
        let call_id = format!("call_{i}");
        let execution = ToolExecution {
            tool_calls: vec![json!({
                "id": call_id,
                "type": "function",
                "function": {"name": "test", "arguments": "{}"}
            })],
            tool_results: vec![json!(format!("result_{i}"))],
            error: None,
        };
        let response = json!({
            "choices": [{"message": {"role": "assistant", "tool_calls": [{
                "id": call_id, "type": "function",
                "function": {"name": "test", "arguments": "{}"}
            }]}}]
        });
        llm._persist_round(Some(tape), &response, &execution, &mut in_memory)
            .await
            .unwrap();
    }

    // in_memory should have: system, user, assistant+tool_calls, tool_result, assistant+tool_calls, tool_result
    assert_eq!(in_memory.len(), 6);
    assert_eq!(in_memory[0]["role"], "system");
    assert_eq!(in_memory[1]["role"], "user");
    assert_eq!(in_memory[2]["role"], "assistant");
    assert_eq!(in_memory[3]["role"], "tool");
    assert_eq!(in_memory[3]["content"], "result_0");
    assert_eq!(in_memory[4]["role"], "assistant");
    assert_eq!(in_memory[5]["role"], "tool");
    assert_eq!(in_memory[5]["content"], "result_1");
}

#[test]
fn test_chat_request_default_has_no_session_id() {
    let req: ChatRequest<'_> = ChatRequest::default();
    assert!(req.session_id.is_none());
}
