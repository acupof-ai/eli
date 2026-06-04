//! Tool execution helpers for Conduit.

use crate::core::errors::{ConduitError, ErrorKind};
use crate::core::results::{ErrorPayload, ToolExecution};
use crate::core::tool_calls::normalize_tool_calls;
use crate::tools::context::ToolContext;
use crate::tools::schema::{Tool, ToolResult};
use futures::future::join_all;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

/// Default per-tool timeout: 60 seconds wall-clock time.
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Executes tool calls parsed from LLM responses.
#[derive(Debug, Clone)]
pub struct ToolExecutor;

impl ToolExecutor {
    /// Create a new `ToolExecutor`.
    pub fn new() -> Self {
        Self
    }

    /// Execute tool calls synchronously by blocking on the async runtime.
    ///
    /// Prefer `execute_async` instead.
    ///
    /// # Panics
    /// Panics if called from within an async runtime context.
    pub fn execute(
        &self,
        response: ToolCallResponse,
        tools: &[Tool],
        context: Option<&ToolContext>,
    ) -> Result<ToolExecution, ConduitError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                ConduitError::new(ErrorKind::Unknown, format!("Failed to create runtime: {e}"))
            })?;
        rt.block_on(self.execute_async(response, tools, context))
    }

    /// Execute tool calls asynchronously.
    pub async fn execute_async(
        &self,
        response: ToolCallResponse,
        tools: &[Tool],
        context: Option<&ToolContext>,
    ) -> Result<ToolExecution, ConduitError> {
        let tool_calls = self.normalize_response(response)?;
        let tool_map = self.build_tool_map(tools);

        if tool_map.is_empty() {
            if !tool_calls.is_empty() {
                return Err(ConduitError::new(
                    ErrorKind::Tool,
                    "No runnable tools are available.",
                ));
            }
            return Ok(ToolExecution::default());
        }

        // Group tool calls by tool name so same-tool calls run sequentially,
        // while different tools run concurrently.
        let mut groups: HashMap<String, Vec<(usize, &Value)>> = HashMap::new();
        for (idx, call) in tool_calls.iter().enumerate() {
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            groups.entry(name).or_default().push((idx, call));
        }

        // Each group runs its calls sequentially; groups run concurrently.
        let group_futures = groups.into_values().map(|calls| {
            let tool_map = &tool_map;
            async move {
                let mut group_results: Vec<(usize, Result<Value, ConduitError>)> =
                    Vec::with_capacity(calls.len());
                for (idx, call) in calls {
                    let result = self.handle_tool_call(call, tool_map, context).await;
                    group_results.push((idx, result));
                }
                group_results
            }
        });

        let all_group_results = join_all(group_futures).await;

        // Reassemble results in original order.
        let mut indexed_results: Vec<(usize, Result<Value, ConduitError>)> =
            all_group_results.into_iter().flatten().collect();
        indexed_results.sort_by_key(|(idx, _)| *idx);

        let mut results = Vec::with_capacity(tool_calls.len());
        let mut first_error: Option<ErrorPayload> = None;
        for (_idx, result) in indexed_results {
            match result {
                Ok(value) => results.push(value),
                Err(err) => {
                    let payload = ErrorPayload::new(err.kind, &err.message);
                    let mut err_value = serde_json::to_value(&payload).unwrap_or(Value::Null);
                    // MCP-style `is_error`: lets the model and callers tell a tool
                    // failure from an empty-but-successful result without parsing
                    // the message, so in-loop self-correction has a hard signal.
                    if let Value::Object(map) = &mut err_value {
                        map.insert("is_error".to_owned(), Value::Bool(true));
                    }
                    if first_error.is_none() {
                        first_error = Some(payload);
                    }
                    results.push(err_value);
                }
            }
        }

        Ok(ToolExecution {
            tool_calls,
            tool_results: results,
            error: first_error,
        })
    }

    /// Resolve a single tool call to (name, tool, args).
    fn resolve_tool_call<'a>(
        &self,
        call: &'a Value,
        tool_map: &'a HashMap<&'a str, &'a Tool>,
    ) -> Result<(&'a str, &'a Tool, Value), ConduitError> {
        let obj = call.as_object().ok_or_else(|| {
            ConduitError::new(ErrorKind::InvalidInput, "Each tool call must be an object.")
        })?;

        let function = obj
            .get("function")
            .and_then(|v| v.as_object())
            .ok_or_else(|| {
                ConduitError::new(
                    ErrorKind::InvalidInput,
                    "Tool call is missing function object.",
                )
            })?;

        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ConduitError::new(ErrorKind::InvalidInput, "Tool call is missing name.")
            })?;

        let tool = self.resolve_tool(name, tool_map).ok_or_else(|| {
            ConduitError::new(ErrorKind::NotFound, format!("Unknown tool name: {name}."))
        })?;
        let resolved_name = tool.name.as_str();

        let raw_args = function
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let args = self.normalize_tool_args(resolved_name, raw_args)?;

        Ok((resolved_name, tool, args))
    }

    /// Resolve a tool name with dot/underscore alias fallback.
    fn resolve_tool<'a>(
        &self,
        name: &str,
        tool_map: &'a HashMap<&'a str, &'a Tool>,
    ) -> Option<&'a Tool> {
        tool_map
            .get(name)
            .copied()
            .or_else(|| {
                if name.contains('_') {
                    let alt = name.replace('_', ".");
                    tool_map.get(alt.as_str()).copied()
                } else {
                    None
                }
            })
            .or_else(|| {
                if name.contains('.') {
                    let alt = name.replace('.', "_");
                    tool_map.get(alt.as_str()).copied()
                } else {
                    None
                }
            })
    }

    /// Handle a single tool call, invoking the handler.
    async fn handle_tool_call(
        &self,
        call: &Value,
        tool_map: &HashMap<&str, &Tool>,
        context: Option<&ToolContext>,
    ) -> ToolResult {
        let (name, tool, args) = self.resolve_tool_call(call, tool_map)?;

        if tool.context && context.is_none() {
            return Err(ConduitError::new(
                ErrorKind::InvalidInput,
                format!("Tool '{name}' requires context but none was provided."),
            ));
        }

        let ctx = if tool.context { context.cloned() } else { None };
        let timeout = tool.timeout.unwrap_or(DEFAULT_TOOL_TIMEOUT);

        match tokio::time::timeout(timeout, tool.run(args, ctx)).await {
            Ok(result) => result.map_err(|e| {
                ConduitError::new(
                    ErrorKind::Tool,
                    format!("Tool '{name}' execution failed: {e}"),
                )
            }),
            Err(_elapsed) => Err(ConduitError::new(
                ErrorKind::Timeout,
                format!("Tool '{}' timed out after {}s", name, timeout.as_secs()),
            )),
        }
    }

    /// Normalize a response from the model into a list of tool call objects.
    fn normalize_response(&self, response: ToolCallResponse) -> Result<Vec<Value>, ConduitError> {
        match response {
            ToolCallResponse::Json(s) => {
                let parsed: Value = serde_json::from_str(&s).map_err(|e| {
                    ConduitError::new(
                        ErrorKind::InvalidInput,
                        format!("Tool response is not valid JSON: {e}"),
                    )
                })?;
                self.normalize_response(ToolCallResponse::Value(parsed))
            }
            ToolCallResponse::Value(val) => match val {
                Value::Array(arr) => self.normalize_tool_calls(arr),
                Value::Object(obj) => {
                    if let Some(output) = obj.get("output").and_then(|v| v.as_array()) {
                        self.normalize_tool_calls(output.to_vec())
                    } else if let Some(content) = obj.get("content").and_then(|v| v.as_array()) {
                        self.normalize_tool_calls(content.to_vec())
                    } else if let Some(tool_calls) =
                        obj.get("tool_calls").and_then(|v| v.as_array())
                    {
                        self.normalize_tool_calls(tool_calls.to_vec())
                    } else {
                        self.normalize_tool_calls(vec![Value::Object(obj)])
                    }
                }
                _ => Err(ConduitError::new(
                    ErrorKind::InvalidInput,
                    "Tool response must be a list of objects.",
                )),
            },
            ToolCallResponse::List(list) => self.normalize_tool_calls(list),
        }
    }

    fn normalize_tool_calls(&self, raw_calls: Vec<Value>) -> Result<Vec<Value>, ConduitError> {
        let normalized = normalize_tool_calls(&raw_calls);
        if raw_calls.is_empty() || !normalized.is_empty() {
            return Ok(normalized);
        }

        Err(ConduitError::new(
            ErrorKind::InvalidInput,
            "Tool response did not contain any valid tool calls.",
        ))
    }

    fn build_tool_map<'a>(&self, tools: &'a [Tool]) -> HashMap<&'a str, &'a Tool> {
        tools
            .iter()
            .filter(|t| t.is_runnable() && !t.name.is_empty())
            .map(|t| (t.name.as_str(), t))
            .collect()
    }

    /// Parse arguments from string or object form.
    fn normalize_tool_args(&self, tool_name: &str, args: Value) -> Result<Value, ConduitError> {
        match args {
            Value::String(s) => {
                let parsed: Value = serde_json::from_str(&s).map_err(|_| {
                    ConduitError::new(
                        ErrorKind::InvalidInput,
                        format!("Tool '{tool_name}' arguments are not valid JSON."),
                    )
                })?;
                if parsed.is_object() {
                    Ok(parsed)
                } else {
                    Err(ConduitError::new(
                        ErrorKind::InvalidInput,
                        format!("Tool '{tool_name}' arguments must be an object."),
                    ))
                }
            }
            Value::Object(_) => Ok(args),
            _ => Err(ConduitError::new(
                ErrorKind::InvalidInput,
                format!("Tool '{tool_name}' arguments must be an object."),
            )),
        }
    }
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrapper for the various shapes a tool-call response can take.
pub enum ToolCallResponse {
    /// Raw JSON string.
    Json(String),
    /// Already-parsed JSON value.
    Value(Value),
    /// Pre-split list of tool call objects.
    List(Vec<Value>),
}

impl From<String> for ToolCallResponse {
    fn from(s: String) -> Self {
        Self::Json(s)
    }
}

impl From<&str> for ToolCallResponse {
    fn from(s: &str) -> Self {
        Self::Json(s.to_string())
    }
}

impl From<Value> for ToolCallResponse {
    fn from(v: Value) -> Self {
        Self::Value(v)
    }
}

impl From<Vec<Value>> for ToolCallResponse {
    fn from(v: Vec<Value>) -> Self {
        Self::List(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::schema::Tool;
    use serde_json::json;

    fn make_echo_tool() -> Tool {
        Tool::new(
            "echo",
            "Echo the input",
            json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
            |args, _ctx| {
                Box::pin(async move {
                    let msg = args.get("msg").and_then(|v| v.as_str()).unwrap_or("(none)");
                    Ok(json!({"echoed": msg}))
                })
            },
        )
    }

    fn make_failing_tool() -> Tool {
        Tool::new(
            "fail",
            "Always fails",
            json!({"type": "object", "properties": {}}),
            |_args, _ctx| {
                Box::pin(
                    async move { Err(ConduitError::new(ErrorKind::Tool, "intentional failure")) },
                )
            },
        )
    }

    fn tool_call_json(name: &str, args: Value) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "arguments": args,
            }
        })
    }

    // ----- ToolExecutor basic creation -----

    #[test]
    fn test_executor_creation() {
        let executor = ToolExecutor::new();
        let default_executor = ToolExecutor::default();
        // Both should be usable; ToolExecutor is a unit struct
        let _ = format!("{:?}", executor);
        let _ = format!("{:?}", default_executor);
    }

    // ----- execute_async with mock tool -----

    #[tokio::test]
    async fn test_execute_async_single_tool_call() {
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();
        let call = tool_call_json("echo", json!({"msg": "hello"}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[echo], None)
            .await
            .unwrap();

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_results.len(), 1);
        assert!(result.error.is_none());
        assert_eq!(result.tool_results[0]["echoed"], "hello");
    }

    #[tokio::test]
    async fn test_execute_async_accepts_responses_function_call_shape() {
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();
        let call = json!({
            "type": "function_call",
            "call_id": "call_123",
            "name": "echo",
            "arguments": "{\"msg\": \"hello\"}"
        });

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[echo], None)
            .await
            .unwrap();

        assert!(result.error.is_none());
        assert_eq!(result.tool_calls[0]["id"], "call_123");
        assert_eq!(result.tool_calls[0]["function"]["name"], "echo");
        assert_eq!(result.tool_results[0]["echoed"], "hello");
    }

    #[tokio::test]
    async fn test_execute_async_multiple_tool_calls() {
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();
        let calls = vec![
            tool_call_json("echo", json!({"msg": "first"})),
            tool_call_json("echo", json!({"msg": "second"})),
        ];

        let result = executor
            .execute_async(ToolCallResponse::List(calls), &[echo], None)
            .await
            .unwrap();

        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.tool_results.len(), 2);
        assert!(result.error.is_none());
        assert_eq!(result.tool_results[0]["echoed"], "first");
        assert_eq!(result.tool_results[1]["echoed"], "second");
    }

    // ----- Unknown tool name -----

    #[tokio::test]
    async fn test_execute_async_unknown_tool_name() {
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();
        let call = tool_call_json("nonexistent", json!({}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[echo], None)
            .await
            .unwrap();

        // Unknown tool is now a specific NotFound (not the catch-all Tool), and
        // the result carries the MCP-style is_error marker.
        assert!(result.error.is_some());
        assert_eq!(result.error.unwrap().kind, ErrorKind::NotFound);
        assert_eq!(result.tool_results[0]["is_error"], true);
        assert_eq!(result.tool_results[0]["kind"], "not_found");
    }

    #[tokio::test]
    async fn test_execute_async_resolves_underscore_alias_for_dotted_tool() {
        let executor = ToolExecutor::new();
        let tool = Tool::new(
            "tape.info",
            "Tape info",
            json!({"type": "object", "properties": {}}),
            |_args, _ctx| Box::pin(async { Ok(json!({"ok": true})) }),
        );
        let call = tool_call_json("tape_info", json!({}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[tool], None)
            .await
            .unwrap();

        assert!(result.error.is_none());
        assert_eq!(result.tool_results[0]["ok"], true);
    }

    #[tokio::test]
    async fn test_execute_async_resolves_dotted_alias_for_underscore_tool() {
        let executor = ToolExecutor::new();
        let tool = Tool::new(
            "tape_info",
            "Tape info",
            json!({"type": "object", "properties": {}}),
            |_args, _ctx| Box::pin(async { Ok(json!({"ok": true})) }),
        );
        let call = tool_call_json("tape.info", json!({}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[tool], None)
            .await
            .unwrap();

        assert!(result.error.is_none());
        assert_eq!(result.tool_results[0]["ok"], true);
    }

    // ----- No tools available -----

    #[tokio::test]
    async fn test_execute_async_no_tools_with_calls_errors() {
        let executor = ToolExecutor::new();
        let call = tool_call_json("echo", json!({}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[], None)
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, ErrorKind::Tool);
    }

    #[tokio::test]
    async fn test_execute_async_no_tools_no_calls_ok() {
        let executor = ToolExecutor::new();

        let result = executor
            .execute_async(ToolCallResponse::List(vec![]), &[], None)
            .await
            .unwrap();

        assert!(result.tool_calls.is_empty());
        assert!(result.tool_results.is_empty());
        assert!(result.error.is_none());
    }

    // ----- Tool that fails -----

    #[tokio::test]
    async fn test_execute_async_tool_failure_captured_in_error() {
        let executor = ToolExecutor::new();
        let fail = make_failing_tool();
        let call = tool_call_json("fail", json!({}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[fail], None)
            .await
            .unwrap();

        assert!(result.error.is_some());
        let err = result.error.unwrap();
        assert_eq!(err.kind, ErrorKind::Tool);
        assert!(err.message.contains("execution failed"));
        // The failing call's result is flagged so the model can self-correct.
        assert_eq!(result.tool_results[0]["is_error"], true);
        assert_eq!(result.tool_results[0]["kind"], "tool");
    }

    #[tokio::test]
    async fn test_successful_tool_result_has_no_is_error_marker() {
        // Success results stay the raw tool output — only failures are flagged.
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();
        let call = tool_call_json("echo", json!({"message": "hi"}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[echo], None)
            .await
            .unwrap();

        assert!(result.error.is_none());
        assert!(result.tool_results[0].get("is_error").is_none());
    }

    // ----- normalize_response -----

    #[tokio::test]
    async fn test_normalize_response_json_string() {
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();
        let json_str =
            serde_json::to_string(&vec![tool_call_json("echo", json!({"msg": "hi"}))]).unwrap();

        let result = executor
            .execute_async(ToolCallResponse::Json(json_str), &[echo], None)
            .await
            .unwrap();

        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0]["echoed"], "hi");
    }

    #[tokio::test]
    async fn test_normalize_response_single_object() {
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();
        let single = tool_call_json("echo", json!({"msg": "single"}));

        let result = executor
            .execute_async(ToolCallResponse::Value(single), &[echo], None)
            .await
            .unwrap();

        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0]["echoed"], "single");
    }

    #[tokio::test]
    async fn test_normalize_response_invalid_json() {
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();

        let result = executor
            .execute_async(
                ToolCallResponse::Json("not-json".to_string()),
                &[echo],
                None,
            )
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn test_normalize_response_rejects_primitive() {
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();

        let result = executor
            .execute_async(ToolCallResponse::Value(json!(42)), &[echo], None)
            .await;

        assert!(result.is_err());
    }

    // ----- String arguments parsing -----

    #[tokio::test]
    async fn test_string_arguments_are_parsed() {
        let executor = ToolExecutor::new();
        let echo = make_echo_tool();
        let call = json!({
            "type": "function",
            "function": {
                "name": "echo",
                "arguments": "{\"msg\": \"from_string\"}"
            }
        });

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[echo], None)
            .await
            .unwrap();

        assert_eq!(result.tool_results[0]["echoed"], "from_string");
    }

    // ----- ToolCallResponse conversions -----

    #[test]
    fn test_tool_call_response_from_string() {
        let resp: ToolCallResponse = String::from("[]").into();
        matches!(resp, ToolCallResponse::Json(_));
    }

    #[test]
    fn test_tool_call_response_from_str() {
        let resp: ToolCallResponse = "[]".into();
        matches!(resp, ToolCallResponse::Json(_));
    }

    #[test]
    fn test_tool_call_response_from_value() {
        let resp: ToolCallResponse = json!([]).into();
        matches!(resp, ToolCallResponse::Value(_));
    }

    #[test]
    fn test_tool_call_response_from_vec() {
        let resp: ToolCallResponse = vec![json!({})].into();
        matches!(resp, ToolCallResponse::List(_));
    }

    // ----- Context-requiring tool without context -----

    #[tokio::test]
    async fn test_context_tool_without_context_errors() {
        let tool = Tool::with_context(
            "ctx_tool",
            "Needs context",
            json!({"type": "object", "properties": {}}),
            |_args, _ctx| Box::pin(async { Ok(json!("ok")) }),
        );
        let executor = ToolExecutor::new();
        let call = tool_call_json("ctx_tool", json!({}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[tool], None)
            .await
            .unwrap();

        assert!(result.error.is_some());
        assert!(result.error.unwrap().message.contains("requires context"));
    }

    // ----- Context-requiring tool with context -----

    #[tokio::test]
    async fn test_context_tool_with_context_succeeds() {
        let tool = Tool::with_context(
            "ctx_tool",
            "Needs context",
            json!({"type": "object", "properties": {}}),
            |_args, ctx| {
                Box::pin(async move {
                    let run_id = ctx.map(|c| c.run_id).unwrap_or_default();
                    Ok(json!({"run_id": run_id}))
                })
            },
        );
        let executor = ToolExecutor::new();
        let call = tool_call_json("ctx_tool", json!({}));
        let context = ToolContext::new("test-run-123");

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[tool], Some(&context))
            .await
            .unwrap();

        assert!(result.error.is_none());
        assert_eq!(result.tool_results[0]["run_id"], "test-run-123");
    }

    // ----- Per-tool timeout -----

    fn make_slow_tool(sleep_ms: u64) -> Tool {
        Tool::new(
            "slow",
            "Sleeps for a while",
            json!({"type": "object", "properties": {}}),
            move |_args, _ctx| {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                    Ok(json!({"done": true}))
                })
            },
        )
    }

    #[tokio::test]
    async fn test_tool_timeout_fires() {
        let tool = make_slow_tool(200).with_timeout(std::time::Duration::from_millis(50));
        let executor = ToolExecutor::new();
        let call = tool_call_json("slow", json!({}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[tool], None)
            .await
            .unwrap();

        assert!(result.error.is_some());
        let err = result.error.unwrap();
        assert!(err.message.contains("timed out"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn test_tool_completes_before_timeout() {
        let tool = make_slow_tool(10).with_timeout(std::time::Duration::from_secs(1));
        let executor = ToolExecutor::new();
        let call = tool_call_json("slow", json!({}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[tool], None)
            .await
            .unwrap();

        assert!(result.error.is_none());
        assert_eq!(result.tool_results[0]["done"], true);
    }

    #[tokio::test]
    async fn test_tool_custom_timeout_used() {
        let tool = make_slow_tool(150).with_timeout(std::time::Duration::from_millis(100));
        assert_eq!(tool.timeout, Some(std::time::Duration::from_millis(100)));
        let executor = ToolExecutor::new();
        let call = tool_call_json("slow", json!({}));

        let result = executor
            .execute_async(ToolCallResponse::List(vec![call]), &[tool], None)
            .await
            .unwrap();

        // Should timeout because sleep(150ms) > timeout(100ms)
        assert!(result.error.is_some());
        assert!(result.error.unwrap().message.contains("timed out"));
    }
}
