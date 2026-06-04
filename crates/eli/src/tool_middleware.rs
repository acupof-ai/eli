//! Tool middleware chain for wrapping tool execution with cross-cutting concerns.
//!
//! Provides circuit breakers, degradation fallbacks, and metrics collection
//! without modifying the conduit crate.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use futures::future::BoxFuture;
use nexil::Tool;
use nexil::core::errors::{ConduitError, ErrorKind};
use nexil::tools::context::ToolContext;
use nexil::tools::schema::{ToolHandlerFn, ToolResult};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Middleware trait
// ---------------------------------------------------------------------------

/// A function that calls the next layer in the chain.
pub type NextFn =
    Box<dyn Fn(Value, Option<ToolContext>) -> BoxFuture<'static, ToolResult> + Send + Sync>;

/// Trait for tool execution middleware layers.
pub trait ToolMiddleware: Send + Sync {
    /// Wrap a tool call. Call `next` to proceed to the inner handler.
    fn call(
        &self,
        tool_name: &str,
        args: Value,
        ctx: Option<ToolContext>,
        next: &NextFn,
    ) -> BoxFuture<'static, ToolResult>;
}

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

/// Per-tool circuit breaker state.
#[derive(Debug, Clone, Default)]
pub struct CircuitState {
    pub consecutive_failures: u32,
    pub tripped_at: Option<Instant>,
}

/// Circuit breaker middleware: trips after N consecutive failures,
/// auto-resets after a timeout.
pub struct CircuitBreaker {
    max_failures: u32,
    reset_timeout: Duration,
    states: Arc<Mutex<HashMap<String, CircuitState>>>,
}

impl CircuitBreaker {
    pub fn new(max_failures: u32, reset_timeout: Duration) -> Self {
        Self {
            max_failures,
            reset_timeout,
            states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Default: trip after 5 failures, reset after 60s.
    pub fn default_config() -> Self {
        Self::new(5, Duration::from_secs(60))
    }

    fn is_tripped(&self, tool_name: &str) -> bool {
        let states = self.states.lock();
        if let Some(state) = states.get(tool_name)
            && let Some(tripped_at) = state.tripped_at
        {
            if tripped_at.elapsed() >= self.reset_timeout {
                return false; // Will be reset on next call
            }
            return true;
        }
        false
    }
}

impl ToolMiddleware for CircuitBreaker {
    fn call(
        &self,
        tool_name: &str,
        args: Value,
        ctx: Option<ToolContext>,
        next: &NextFn,
    ) -> BoxFuture<'static, ToolResult> {
        if self.is_tripped(tool_name) {
            let msg = format!("Circuit breaker open for tool '{tool_name}'. Try again later.");
            return Box::pin(async move { Err(ConduitError::new(ErrorKind::Tool, msg)) });
        }

        let name = tool_name.to_owned();
        let states = Arc::clone(&self.states);
        let max_failures = self.max_failures;
        let fut = next(args, ctx);
        Box::pin(async move {
            let result = fut.await;
            let mut s = states.lock();
            match &result {
                Ok(_) => {
                    s.insert(name, CircuitState::default());
                }
                Err(_) => {
                    let state = s.entry(name).or_default();
                    state.consecutive_failures += 1;
                    if state.consecutive_failures >= max_failures {
                        state.tripped_at = Some(Instant::now());
                    }
                }
            }
            result
        })
    }
}

// ---------------------------------------------------------------------------
// Metrics collector
// ---------------------------------------------------------------------------

/// Per-tool execution metrics.
#[derive(Debug, Clone, Default)]
pub struct ToolMetrics {
    pub call_count: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub total_duration_ms: u64,
}

/// Metrics collection middleware.
pub struct MetricsCollector {
    pub stats: Arc<Mutex<HashMap<String, ToolMetrics>>>,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolMiddleware for MetricsCollector {
    fn call(
        &self,
        tool_name: &str,
        args: Value,
        ctx: Option<ToolContext>,
        next: &NextFn,
    ) -> BoxFuture<'static, ToolResult> {
        let stats = Arc::clone(&self.stats);
        let name = tool_name.to_owned();
        let fut = next(args, ctx);

        Box::pin(async move {
            let start = Instant::now();
            let result = fut.await;
            let duration_ms = start.elapsed().as_millis() as u64;

            let mut map = stats.lock();
            let metrics = map.entry(name).or_default();
            metrics.call_count += 1;
            metrics.total_duration_ms += duration_ms;
            match &result {
                Ok(_) => metrics.success_count += 1,
                Err(_) => metrics.failure_count += 1,
            }

            result
        })
    }
}

// ---------------------------------------------------------------------------
// Middleware chain
// ---------------------------------------------------------------------------

/// Chain of middleware layers applied around tool execution.
pub struct MiddlewareChain {
    layers: Vec<Arc<dyn ToolMiddleware>>,
}

impl Default for MiddlewareChain {
    fn default() -> Self {
        Self::new()
    }
}

impl MiddlewareChain {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Add a middleware layer. Last-pushed is outermost (executes first).
    pub fn push(&mut self, layer: Arc<dyn ToolMiddleware>) -> &mut Self {
        self.layers.push(layer);
        self
    }

    /// Create a default chain with metrics + circuit breaker.
    pub fn with_defaults() -> Self {
        let mut chain = Self::new();
        chain.push(Arc::new(CircuitBreaker::default_config()));
        chain.push(Arc::new(MetricsCollector::new()));
        chain
    }

    /// Wrap a tool's handler through the middleware chain, returning a new handler.
    pub fn wrap_handler(&self, tool_name: &str, tool: &Tool) -> Option<Arc<ToolHandlerFn>> {
        let handler = tool.handler.as_ref()?.clone();

        if self.layers.is_empty() {
            return Some(handler);
        }

        let layers: Vec<Arc<dyn ToolMiddleware>> = self.layers.clone();
        let name = tool_name.to_owned();

        let wrapped: Arc<ToolHandlerFn> = Arc::new(move |args: Value, ctx: Option<ToolContext>| {
            let layers = layers.clone();
            let name = name.clone();
            let handler = handler.clone();

            Box::pin(async move {
                let inner: NextFn = Box::new(move |a, c| handler(a, c));

                let mut current = inner;
                for layer in layers.iter() {
                    let layer = Arc::clone(layer);
                    let prev = current;
                    let n = name.clone();
                    current = Box::new(move |a, c| {
                        let layer = Arc::clone(&layer);
                        let prev = &prev;
                        let n = n.clone();
                        layer.call(&n, a, c, prev)
                    });
                }

                current(args, ctx).await
            }) as BoxFuture<'static, ToolResult>
        });

        Some(wrapped)
    }

    /// Wrap all runnable tools in a Vec, replacing handlers with chain-wrapped versions.
    pub fn wrap_tools(&self, tools: &[Tool]) -> Vec<Tool> {
        if self.layers.is_empty() {
            return tools.to_vec();
        }

        tools
            .iter()
            .map(|tool| {
                let wrapped_handler = self.wrap_handler(&tool.name, tool);
                Tool {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                    handler: wrapped_handler.or_else(|| tool.handler.clone()),
                    context: tool.context,
                    timeout: tool.timeout,
                    read_only: tool.read_only,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_tool() -> Tool {
        Tool::new(
            "test_ok",
            "always succeeds",
            json!({"type": "object"}),
            |_args, _ctx| Box::pin(async { Ok(json!({"status": "ok"})) }),
        )
    }

    fn fail_tool() -> Tool {
        Tool::new(
            "test_fail",
            "always fails",
            json!({"type": "object"}),
            |_args, _ctx| Box::pin(async { Err(ConduitError::new(ErrorKind::Tool, "boom")) }),
        )
    }

    #[tokio::test]
    async fn test_metrics_collector_counts() {
        let metrics = Arc::new(MetricsCollector::new());
        let mut chain = MiddlewareChain::new();
        chain.push(Arc::clone(&metrics) as Arc<dyn ToolMiddleware>);

        let tools = chain.wrap_tools(&[ok_tool()]);
        let tool = &tools[0];
        let handler = tool.handler.as_ref().unwrap();

        let _ = handler(json!({}), None).await;
        let _ = handler(json!({}), None).await;

        let stats = metrics.stats.lock();
        let m = stats.get("test_ok").unwrap();
        assert_eq!(m.call_count, 2);
        assert_eq!(m.success_count, 2);
    }

    #[tokio::test]
    async fn test_circuit_breaker_trips() {
        let cb = Arc::new(CircuitBreaker::new(2, Duration::from_secs(60)));
        let mut chain = MiddlewareChain::new();
        chain.push(cb.clone() as Arc<dyn ToolMiddleware>);

        let tools = chain.wrap_tools(&[fail_tool()]);
        let tool = &tools[0];
        let handler = tool.handler.as_ref().unwrap();

        // First two failures trigger the breaker.
        let _ = handler(json!({}), None).await;
        let _ = handler(json!({}), None).await;

        // Third call should be rejected by the breaker.
        let result = handler(json!({}), None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Circuit breaker open"));
    }
}
