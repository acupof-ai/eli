//! Tool schema definitions and normalization for Conduit.

use crate::core::errors::{ConduitError, ErrorKind};
use crate::tools::context::ToolContext;
use futures::future::BoxFuture;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;

/// Result alias used throughout the tools module.
pub type ToolResult = Result<Value, ConduitError>;

/// A type-erased async tool handler.
///
/// Accepts a JSON `Value` of arguments and returns a future resolving to a JSON `Value`.
pub type ToolHandlerFn =
    dyn Fn(Value, Option<ToolContext>) -> BoxFuture<'static, ToolResult> + Send + Sync;

/// A Tool is a callable unit the model can invoke.
pub struct Tool {
    /// The tool name, used to dispatch calls.
    pub name: String,
    /// Human-readable description for the model.
    pub description: String,
    /// JSON Schema describing the tool parameters.
    pub parameters: Value,
    /// The async handler function, if this tool is runnable.
    pub handler: Option<Arc<ToolHandlerFn>>,
    /// Whether this tool expects a `ToolContext` argument.
    pub context: bool,
    /// Per-tool wall-clock timeout. `None` means use the executor default (60s).
    pub timeout: Option<std::time::Duration>,
    /// MCP-style behavior hint: the tool only reads state and never mutates the
    /// workspace/session. Lets callers auto-approve safe reads and gate mutating
    /// tools (e.g. a read-only "plan" mode). Defaults to `false` (assume unsafe).
    pub read_only: bool,
}

impl fmt::Debug for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .field("handler", &self.handler.is_some())
            .field("context", &self.context)
            .field("timeout", &self.timeout)
            .field("read_only", &self.read_only)
            .finish()
    }
}

/// Action returned by `wrap_tool` hooks to control tool visibility.
#[derive(Debug)]
pub enum ToolAction {
    /// Leave the tool unchanged.
    Keep,
    /// Remove the tool from the set (model will not see it).
    Remove,
    /// Replace the tool with a modified version.
    Replace(Tool),
}

impl Clone for Tool {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            handler: self.handler.clone(),
            context: self.context,
            timeout: self.timeout,
            read_only: self.read_only,
        }
    }
}

impl Tool {
    /// Create a new schema-only tool (no handler).
    pub fn schema_only(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            handler: None,
            context: false,
            timeout: None,
            read_only: false,
        }
    }

    /// Create a new runnable tool with a handler.
    pub fn new<F>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(Value, Option<ToolContext>) -> BoxFuture<'static, ToolResult> + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            handler: Some(Arc::new(handler)),
            context: false,
            timeout: None,
            read_only: false,
        }
    }

    /// Create a new runnable tool that receives a `ToolContext`.
    pub fn with_context<F>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(Value, Option<ToolContext>) -> BoxFuture<'static, ToolResult> + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            handler: Some(Arc::new(handler)),
            context: true,
            timeout: None,
            read_only: false,
        }
    }

    /// Mark this tool as read-only (never mutates workspace/session state).
    /// Used by callers to auto-approve safe reads or gate mutating tools.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Produce the OpenAI-compatible tool schema.
    pub fn schema(&self) -> Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }

    /// Return the schema as either a JSON string or a `Value`.
    pub fn as_tool(&self, json_mode: bool) -> Value {
        let schema = self.schema();
        if json_mode {
            Value::String(serde_json::to_string_pretty(&schema).unwrap_or_default())
        } else {
            schema
        }
    }

    /// Invoke the handler with the given arguments.
    ///
    /// Returns an error if the tool is schema-only.
    pub async fn run(&self, args: Value, context: Option<ToolContext>) -> ToolResult {
        match &self.handler {
            Some(handler) => handler(args, context).await,
            None => Err(ConduitError::new(
                ErrorKind::Tool,
                format!(
                    "Tool '{}' is schema-only and cannot be executed.",
                    self.name
                ),
            )),
        }
    }

    /// Returns true if this tool has a handler.
    pub fn is_runnable(&self) -> bool {
        self.handler.is_some()
    }

    /// Set a custom wall-clock timeout for this tool.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Normalized collection of tools with schema payload and runnable implementations.
#[derive(Debug, Clone)]
pub struct ToolSet {
    /// All tool schemas (for sending to the model).
    pub schemas: Vec<Value>,
    /// Only the tools that have handlers.
    pub runnable: Vec<Tool>,
}

impl ToolSet {
    /// Create an empty `ToolSet`.
    pub fn empty() -> Self {
        Self {
            schemas: Vec::new(),
            runnable: Vec::new(),
        }
    }

    /// Return schemas for the API payload, or `None` if empty.
    pub fn payload(&self) -> Option<&[Value]> {
        if self.schemas.is_empty() {
            None
        } else {
            Some(&self.schemas)
        }
    }

    /// Error if there are schemas but no runnable tools.
    pub fn require_runnable(&self) -> Result<(), ConduitError> {
        if !self.schemas.is_empty() && self.runnable.is_empty() {
            return Err(ConduitError::new(
                ErrorKind::Tool,
                "Schema-only tools cannot be executed.",
            ));
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_echo_tool(name: &str, desc: &str) -> Tool {
        Tool::new(
            name,
            desc,
            json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
            |args, _ctx| Box::pin(async move { Ok(args) }),
        )
    }

    #[test]
    fn read_only_defaults_false_and_builder_sets_it() {
        let tool = make_echo_tool("x", "x");
        assert!(!tool.read_only, "tools default to mutating (safe default)");
        assert!(tool.read_only().read_only, "builder marks read-only");
    }

    fn make_schema_only_tool(name: &str) -> Tool {
        Tool::schema_only(
            name,
            "A schema-only tool",
            json!({"type": "object", "properties": {}}),
        )
    }

    // ----- Tool creation -----

    #[test]
    fn test_tool_creation_with_handler() {
        let tool = make_echo_tool("echo", "Echo args back");
        assert_eq!(tool.name, "echo");
        assert_eq!(tool.description, "Echo args back");
        assert!(tool.is_runnable());
        assert!(!tool.context);
    }

    #[test]
    fn test_tool_creation_schema_only() {
        let tool = make_schema_only_tool("readonly");
        assert_eq!(tool.name, "readonly");
        assert!(!tool.is_runnable());
    }

    #[test]
    fn test_tool_with_context_flag() {
        let tool = Tool::with_context(
            "ctx_tool",
            "Needs context",
            json!({"type": "object", "properties": {}}),
            |_args, _ctx| Box::pin(async { Ok(json!(null)) }),
        );
        assert!(tool.context);
        assert!(tool.is_runnable());
    }

    // ----- Tool::schema() -----

    #[test]
    fn test_tool_schema_format() {
        let tool = make_echo_tool("my_tool", "Does stuff");
        let schema = tool.schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "my_tool");
        assert_eq!(schema["function"]["description"], "Does stuff");
        assert!(schema["function"]["parameters"].is_object());
    }

    #[test]
    fn test_tool_as_tool_json_mode() {
        let tool = make_echo_tool("t", "d");
        let result = tool.as_tool(true);
        assert!(result.is_string());
        let parsed: Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(parsed["type"], "function");
    }

    #[test]
    fn test_tool_as_tool_value_mode() {
        let tool = make_echo_tool("t", "d");
        let result = tool.as_tool(false);
        assert!(result.is_object());
        assert_eq!(result["type"], "function");
    }

    // ----- Tool::run() -----

    #[tokio::test]
    async fn test_tool_run_returns_args() {
        let tool = make_echo_tool("echo", "Echo");
        let args = json!({"msg": "hello"});
        let result = tool.run(args.clone(), None).await.unwrap();
        assert_eq!(result, args);
    }

    #[tokio::test]
    async fn test_schema_only_tool_run_errors() {
        let tool = make_schema_only_tool("readonly");
        let result = tool.run(json!({}), None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Tool);
        assert!(err.message.contains("schema-only"));
    }

    // ----- ToolSet -----

    #[test]
    fn test_toolset_empty() {
        let ts = ToolSet::empty();
        assert!(ts.schemas.is_empty());
        assert!(ts.runnable.is_empty());
        assert!(ts.payload().is_none());
    }

    #[test]
    fn test_toolset_require_runnable_ok_when_empty() {
        let ts = ToolSet::empty();
        assert!(ts.require_runnable().is_ok());
    }

    #[test]
    fn test_toolset_require_runnable_fails_when_schema_only() {
        let ts = ToolSet {
            schemas: vec![json!({"type": "function", "function": {"name": "x", "parameters": {}}})],
            runnable: vec![],
        };
        assert!(ts.require_runnable().is_err());
    }

    // ----- Tool clone and debug -----

    #[test]
    fn test_tool_clone() {
        let tool = make_echo_tool("orig", "Original");
        let cloned = tool.clone();
        assert_eq!(cloned.name, "orig");
        assert_eq!(cloned.description, "Original");
        assert!(cloned.is_runnable());
    }

    #[test]
    fn test_tool_debug() {
        let tool = make_echo_tool("dbg", "Debug test");
        let debug_str = format!("{:?}", tool);
        assert!(debug_str.contains("dbg"));
        assert!(debug_str.contains("handler: true"));
    }
}
