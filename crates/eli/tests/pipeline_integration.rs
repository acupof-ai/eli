//! Integration test: inbound message → EliFramework → hook chain → outbound.
//!
//! Exercises the full `process_inbound` pipeline with mock hooks (no real LLM).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use eli::framework::EliFramework;
use eli::hooks::{EliHookSpec, HookError};
use eli::types::{Envelope, PromptValue, State};

// ---------------------------------------------------------------------------
// Mock plugins
// ---------------------------------------------------------------------------

/// Mock model plugin that returns a canned response without any LLM call.
struct MockModelPlugin {
    canned_response: String,
}

#[async_trait]
impl EliHookSpec for MockModelPlugin {
    fn plugin_name(&self) -> &str {
        "mock-model"
    }

    async fn run_model(
        &self,
        prompt: &PromptValue,
        _session_id: &str,
        _state: &State,
    ) -> Result<Option<String>, HookError> {
        Ok(Some(format!(
            "{}: {}",
            self.canned_response,
            prompt.as_text()
        )))
    }
}

/// Mock render/dispatch plugin that captures outbound envelopes.
struct CapturePlugin {
    captured: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl EliHookSpec for CapturePlugin {
    fn plugin_name(&self) -> &str {
        "capture"
    }

    async fn dispatch_outbound(&self, message: &Envelope) -> Option<bool> {
        self.captured.lock().unwrap().push(message.clone());
        Some(true)
    }
}

/// Mock session plugin that resolves session from the envelope.
struct MockSessionPlugin;

#[async_trait]
impl EliHookSpec for MockSessionPlugin {
    fn plugin_name(&self) -> &str {
        "mock-session"
    }

    async fn resolve_session(&self, message: &Envelope) -> Result<Option<String>, HookError> {
        Ok(message
            .as_object()
            .and_then(|o| o.get("session_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned()))
    }
}

/// Mock state plugin that injects a custom key into the turn state.
struct MockStatePlugin;

#[async_trait]
impl EliHookSpec for MockStatePlugin {
    fn plugin_name(&self) -> &str {
        "mock-state"
    }

    async fn load_state(
        &self,
        _message: &Envelope,
        _session_id: &str,
    ) -> Result<Option<State>, HookError> {
        let mut s = State::new();
        s.insert("test_key".into(), Value::String("test_value".into()));
        Ok(Some(s))
    }
}

/// Mock model plugin that reads the system prompt from state and returns it.
struct StateEchoModelPlugin;

#[async_trait]
impl EliHookSpec for StateEchoModelPlugin {
    fn plugin_name(&self) -> &str {
        "state-echo-model"
    }

    fn build_system_prompt(&self, _prompt_text: &str, _state: &State) -> Option<String> {
        Some("system-prompt-from-hook".into())
    }

    async fn run_model(
        &self,
        _prompt: &PromptValue,
        _session_id: &str,
        state: &State,
    ) -> Result<Option<String>, HookError> {
        // Return the system prompt and test_key from state to prove state flows correctly.
        let sys = state
            .get("_runtime_system_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("no-system-prompt");
        let test_key = state
            .get("test_key")
            .and_then(|v| v.as_str())
            .unwrap_or("no-test-key");
        Ok(Some(format!("sys={sys}|key={test_key}")))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full pipeline: inbound → session → model → outbound dispatch.
#[tokio::test]
async fn test_full_pipeline_inbound_to_outbound() {
    let captured = Arc::new(Mutex::new(Vec::new()));

    let fw = EliFramework::with_workspace("/tmp/test-pipeline".into());
    fw.register_plugin(
        Arc::new(MockSessionPlugin) as Arc<dyn EliHookSpec>,
    )
    .await;
    fw.register_plugin(
        Arc::new(MockModelPlugin {
            canned_response: "REPLY".into(),
        }) as Arc<dyn EliHookSpec>,
    )
    .await;
    fw.register_plugin(
        Arc::new(CapturePlugin {
            captured: captured.clone(),
        }) as Arc<dyn EliHookSpec>,
    )
    .await;

    let inbound = json!({
        "content": "hello world",
        "channel": "test",
        "chat_id": "42",
        "session_id": "sess-001",
    });

    let result = fw.process_inbound(inbound).await.unwrap();

    // Session resolved correctly
    assert_eq!(result.session_id, "sess-001");

    // Model output contains our canned prefix
    assert!(
        result.model_output.contains("REPLY"),
        "expected model output to contain 'REPLY', got: {}",
        result.model_output
    );

    // Model received the user prompt text
    assert!(
        result.model_output.contains("hello world"),
        "expected model output to contain original prompt, got: {}",
        result.model_output
    );

    // Outbound envelopes produced
    assert!(
        !result.outbounds.is_empty(),
        "expected at least one outbound envelope"
    );

    // Outbound dispatched to capture plugin
    let dispatched = captured.lock().unwrap();
    assert!(
        !dispatched.is_empty(),
        "expected capture plugin to receive dispatched outbounds"
    );

    // Dispatched envelope has the model output
    let first = &dispatched[0];
    let content = first.get("content").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        content.contains("REPLY"),
        "dispatched outbound should contain model response, got: {content}"
    );
}

/// Pipeline with default session ID (no session_id field).
#[tokio::test]
async fn test_pipeline_default_session_id() {
    let fw = EliFramework::with_workspace("/tmp/test-pipeline-default".into());
    fw.register_plugin(
        Arc::new(MockModelPlugin {
            canned_response: "OK".into(),
        }) as Arc<dyn EliHookSpec>,
    )
    .await;

    let inbound = json!({
        "content": "ping",
        "channel": "cli",
        "chat_id": "99",
    });

    let result = fw.process_inbound(inbound).await.unwrap();

    // Default session id = "channel:chat_id"
    assert_eq!(result.session_id, "cli:99");
    assert!(result.model_output.contains("OK"));
}

/// Pipeline with state plugin — verifies state flows through to run_model.
#[tokio::test]
async fn test_pipeline_state_flows_to_model() {
    let fw = EliFramework::with_workspace("/tmp/test-pipeline-state".into());
    fw.register_plugin(Arc::new(MockStatePlugin) as Arc<dyn EliHookSpec>)
        .await;
    fw.register_plugin(
        Arc::new(StateEchoModelPlugin) as Arc<dyn EliHookSpec>,
    )
    .await;

    let inbound = json!({
        "content": "test",
        "session_id": "state-test",
    });

    let result = fw.process_inbound(inbound).await.unwrap();

    // Model output should contain the system prompt from hook
    assert!(
        result.model_output.contains("sys=system-prompt-from-hook"),
        "expected system prompt in model output, got: {}",
        result.model_output
    );

    // Model output should contain the state key from load_state
    assert!(
        result.model_output.contains("key=test_value"),
        "expected test_key in model output, got: {}",
        result.model_output
    );
}

/// Pipeline without a model plugin returns error message.
#[tokio::test]
async fn test_pipeline_no_model_returns_error() {
    let fw = EliFramework::with_workspace("/tmp/test-pipeline-no-model".into());

    let inbound = json!({
        "content": "hello",
        "session_id": "no-model",
    });

    let result = fw.process_inbound(inbound).await.unwrap();

    // Without a model plugin, framework returns an error string
    assert!(
        result.model_output.contains("Error"),
        "expected error indicator in model output, got: {}",
        result.model_output
    );
}

/// Multiple inbound messages maintain independent sessions.
#[tokio::test]
async fn test_pipeline_independent_sessions() {
    let fw = EliFramework::with_workspace("/tmp/test-pipeline-sessions".into());
    fw.register_plugin(
        Arc::new(MockSessionPlugin) as Arc<dyn EliHookSpec>,
    )
    .await;
    fw.register_plugin(
        Arc::new(MockModelPlugin {
            canned_response: "ECHO".into(),
        }) as Arc<dyn EliHookSpec>,
    )
    .await;

    let msg_a = json!({
        "content": "alpha",
        "session_id": "sess-a",
        "channel": "test",
        "chat_id": "1",
    });
    let msg_b = json!({
        "content": "beta",
        "session_id": "sess-b",
        "channel": "test",
        "chat_id": "2",
    });

    let result_a = fw.process_inbound(msg_a).await.unwrap();
    let result_b = fw.process_inbound(msg_b).await.unwrap();

    assert_eq!(result_a.session_id, "sess-a");
    assert_eq!(result_b.session_id, "sess-b");
    assert!(result_a.model_output.contains("alpha"));
    assert!(result_b.model_output.contains("beta"));
}
