//! Custom tool example — register a tool and process a message through the framework.
//!
//! This example shows how to register a custom tool in the global REGISTRY
//! and wire up a minimal EliFramework with a model plugin that echoes input.
//!
//! ```bash
//! cargo run --example custom_tool -p eli
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use eli::{EliFramework, EliHookSpec, HookError, PromptValue, State};
use nexil::Tool;
use serde_json::json;

/// A minimal model plugin that echoes the user prompt.
struct EchoModel;

#[async_trait]
impl EliHookSpec for EchoModel {
    fn plugin_name(&self) -> &str {
        "echo-model"
    }

    async fn run_model(
        &self,
        prompt: &PromptValue,
        _session_id: &str,
        _state: &State,
    ) -> Result<Option<String>, HookError> {
        Ok(Some(format!("echo: {}", prompt.as_text())))
    }
}

#[tokio::main]
async fn main() {
    // Register a custom tool in the global registry.
    let tool = Tool::new(
        "math.add",
        "Add two numbers",
        json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}),
        |args, _ctx| {
            Box::pin(async move {
                let a = args.get("a").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let b = args.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0);
                Ok(json!({"result": a + b}))
            })
        },
    );
    {
        let mut reg = eli::tools::REGISTRY.lock();
        reg.insert(tool.name.clone(), tool);
    }

    // Build framework with the echo model and process a message.
    let fw = EliFramework::new();
    fw.register_plugin(Arc::new(EchoModel)).await;

    let msg = json!({"content": "What is 2 + 3?", "channel": "cli", "chat_id": "demo"});
    let result = fw
        .process_inbound(msg)
        .await
        .expect("process_inbound failed");

    println!("done:   processed 1 turn");
    println!("output: {}", result.model_output);
}
