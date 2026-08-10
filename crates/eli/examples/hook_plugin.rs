//! Hook plugin example — implement EliHookSpec to customize framework behavior.
//!
//! Demonstrates how to create a plugin that injects a custom system prompt
//! and a model plugin that uses it. Plugins are registered in order;
//! last-registered wins for hooks that return the first `Some`.
//!
//! ```bash
//! cargo run --example hook_plugin -p eli
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use eli::{EliFramework, EliHookSpec, HookError, PromptValue, State};
use serde_json::json;

/// Plugin that injects a custom system prompt.
struct SystemPromptPlugin {
    instruction: String,
}

#[async_trait]
impl EliHookSpec for SystemPromptPlugin {
    fn plugin_name(&self) -> &str {
        "system-prompt"
    }

    fn build_system_prompt(&self, _prompt_text: &str, _state: &State) -> Option<String> {
        Some(self.instruction.clone())
    }
}

/// Minimal model plugin that prefixes output with the session id.
struct SimpleModel;

#[async_trait]
impl EliHookSpec for SimpleModel {
    fn plugin_name(&self) -> &str {
        "simple-model"
    }

    async fn run_model(
        &self,
        prompt: &PromptValue,
        session_id: &str,
        _state: &State,
    ) -> Result<Option<String>, HookError> {
        Ok(Some(format!(
            "[{session_id}] reply to: {}",
            prompt.as_text()
        )))
    }
}

#[tokio::main]
async fn main() {
    let fw = EliFramework::new();

    // Register system prompt plugin first, then model plugin.
    let prompt_plugin = SystemPromptPlugin {
        instruction: "You are a helpful assistant that speaks like a pirate.".into(),
    };
    fw.register_plugin(Arc::new(prompt_plugin))
        .await;
    fw.register_plugin(Arc::new(SimpleModel))
        .await;

    // Verify the system prompt is wired up.
    let state = State::new();
    let system = fw
        .get_system_prompt(&PromptValue::Text("hello".into()), &state)
        .await;
    println!("system prompt: {system}");

    // Process a message through the full pipeline.
    let msg = json!({"content": "Ahoy!", "channel": "cli", "chat_id": "pirate"});
    let result = fw
        .process_inbound(msg)
        .await
        .expect("process_inbound failed");

    println!("done:   processed 1 turn");
    println!("output: {}", result.model_output);
}
