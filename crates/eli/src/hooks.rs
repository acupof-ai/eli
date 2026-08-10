//! Hook specifications and runtime for the Eli framework.
//!
//! Replaces Python pluggy with a simple Vec-of-trait-objects approach.
//! Hook precedence: implementations are stored in registration order;
//! `call_first` iterates in **reverse** (last registered wins).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;

use crate::types::{Envelope, PromptValue, State};

// ---------------------------------------------------------------------------
// HookError
// ---------------------------------------------------------------------------

/// Identifies which hook point an error originated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPoint {
    ClassifyInbound,
    ResolveSession,
    LoadState,
    BuildUserPrompt,
    BuildSystemPrompt,
    RunModel,
    SaveState,
    RenderOutbound,
    DispatchOutbound,
    RegisterCliCommands,
    OnError,
    WrapTool,
}

impl std::fmt::Display for HookPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::ClassifyInbound => "classify_inbound",
            Self::ResolveSession => "resolve_session",
            Self::LoadState => "load_state",
            Self::BuildUserPrompt => "build_user_prompt",
            Self::BuildSystemPrompt => "build_system_prompt",
            Self::RunModel => "run_model",
            Self::SaveState => "save_state",
            Self::RenderOutbound => "render_outbound",
            Self::DispatchOutbound => "dispatch_outbound",
            Self::RegisterCliCommands => "register_cli_commands",
            Self::OnError => "on_error",
            Self::WrapTool => "wrap_tool",
        };
        f.write_str(name)
    }
}

/// Error returned by hook methods that can fail.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("{hook_point} failed in plugin '{plugin}': {source}")]
    Plugin {
        plugin: String,
        hook_point: HookPoint,
        source: anyhow::Error,
    },
    #[error("hook panicked in plugin '{plugin}': {message}")]
    Panic { plugin: String, message: String },
}

impl HookError {
    /// Wrap a hook error with plugin/hook-point context, extracting the inner source.
    fn wrap(plugin: String, hook_point: HookPoint, e: HookError) -> Self {
        let source = match e {
            HookError::Plugin { source, .. } => source,
            other => anyhow::anyhow!("{other}"),
        };
        HookError::Plugin {
            plugin,
            hook_point,
            source,
        }
    }
}

/// Extract a human-readable message from a `catch_unwind` panic payload.
fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Iterate plugins, call an async method, and swallow any panics.
macro_rules! call_notify_all {
    ($iter:expr, $hook_name:literal, |$p:ident| $call:expr) => {
        for $p in $iter {
            let name = $p.plugin_name();
            let result = std::panic::AssertUnwindSafe($call).catch_unwind().await;
            if let Err(panic_info) = result {
                let msg = panic_payload_message(&panic_info);
                tracing::error!(plugin = %name, panic.message = %msg, concat!("hook.", $hook_name, " panicked"));
            }
        }
    };
}

/// Iterate plugins (sync), call a method, and swallow any panics.
macro_rules! call_sync_all {
    ($iter:expr, $hook_name:literal, |$p:ident| $call:expr) => {
        for $p in $iter {
            let name = $p.plugin_name();
            if let Err(panic_info) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $call)) {
                let msg = panic_payload_message(&panic_info);
                tracing::error!(plugin = %name, panic.message = %msg, concat!("hook.", $hook_name, " panicked"));
            }
        }
    };
}

fn preview_text(text: &str) -> String {
    const LIMIT: usize = 1000;
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(LIMIT).collect();
    let normalized = preview.replace('\n', "\\n");
    if chars.next().is_some() {
        format!("{normalized}...(truncated)")
    } else {
        normalized
    }
}

/// Build a bounded JSON preview without serializing the entire value.
///
/// Writes directly into a size-limited buffer, stopping as soon as the
/// limit is reached. For large envelopes this avoids allocating and then
/// discarding a multi-KB string just to keep the first 1000 characters.
fn preview_json(value: &Envelope) -> String {
    use std::fmt::Write;

    const LIMIT: usize = 1000;

    struct BoundedWriter {
        buf: String,
        limit: usize,
    }

    impl std::fmt::Write for BoundedWriter {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            let remaining = self.limit.saturating_sub(self.buf.len());
            if remaining == 0 {
                return Err(std::fmt::Error); // stop writing
            }
            // Take at most `remaining` bytes, but truncate to a char boundary.
            let end = s.floor_char_boundary(remaining.min(s.len()));
            self.buf.push_str(&s[..end]);
            if end < s.len() || self.buf.len() >= self.limit {
                return Err(std::fmt::Error);
            }
            Ok(())
        }
    }

    let mut w = BoundedWriter {
        buf: String::with_capacity(LIMIT + 64),
        limit: LIMIT,
    };

    // serde_json::to_writer would require io::Write; for fmt::Write we use
    // the Value's Display impl which produces identical JSON output.
    let truncated = write!(w, "{value}").is_err();
    let normalized = w.buf.replace('\n', "\\n");
    if truncated {
        format!("{normalized}...(truncated)")
    } else {
        normalized
    }
}

fn trace_hook_call(plugin: &str, session_id: &str, hook: &str, input: &str) {
    tracing::info!(target: "eli_trace", plugin = %plugin, session_id = %session_id, input = %input, "hook.{hook}.call");
}

fn trace_hook_return(plugin: &str, session_id: &str, hook: &str, output: &str) {
    tracing::info!(target: "eli_trace", plugin = %plugin, session_id = %session_id, output = %output, "hook.{hook}.return");
}

fn trace_hook_none(plugin: &str, session_id: &str, hook: &str) {
    tracing::info!(target: "eli_trace", plugin = %plugin, session_id = %session_id, "hook.{hook}.none");
}

// ---------------------------------------------------------------------------
// EliHookSpec trait
// ---------------------------------------------------------------------------

/// Hook contract for Eli framework extensions.
///
/// All methods have default implementations that return `None` / empty so that
/// plugins only need to override the hooks they care about.
///
/// # Panic Safety
///
/// Hooks are classified into two categories:
/// - **Chain-aborting** (`resolve_session`, `load_state`, `run_model`): a panic
///   causes `HookError::Panic` to propagate to the caller. These hooks are
///   critical to the turn pipeline and cannot be skipped.
/// - **Best-effort** (all others): a panic is caught and logged, then execution
///   continues to the next plugin.
#[async_trait]
#[allow(unused_variables)]
pub trait EliHookSpec: Send + Sync {
    /// Human-readable name for this plugin (used in diagnostics).
    fn plugin_name(&self) -> &str {
        "unnamed"
    }

    /// Classify an inbound message to determine its processing route.
    /// Returns `None` to defer to the next plugin, or `Some(reply)` to
    /// short-circuit with a greeting reply.
    fn classify_inbound(&self, message: &Envelope) -> Option<String> {
        None
    }

    /// Resolve session id for one inbound message.
    async fn resolve_session(&self, message: &Envelope) -> Result<Option<String>, HookError> {
        Ok(None)
    }

    /// Load state snapshot for one session.
    async fn load_state(
        &self,
        message: &Envelope,
        session_id: &str,
    ) -> Result<Option<State>, HookError> {
        Ok(None)
    }

    /// Build model prompt for this turn.
    /// Returns either plain text or a list of content parts (multimodal).
    async fn build_user_prompt(
        &self,
        message: &Envelope,
        session_id: &str,
        state: &State,
    ) -> Option<PromptValue> {
        None
    }

    /// Run model for one turn and return plain text output.
    async fn run_model(
        &self,
        prompt: &PromptValue,
        session_id: &str,
        state: &State,
    ) -> Result<Option<String>, HookError> {
        Ok(None)
    }

    /// Persist state updates after one model turn.
    async fn save_state(
        &self,
        session_id: &str,
        state: &State,
        message: &Envelope,
        model_output: &str,
    ) {
    }

    /// Render outbound messages from model output.
    /// Each implementation may return zero or more envelopes.
    async fn render_outbound(
        &self,
        message: &Envelope,
        session_id: &str,
        state: &State,
        model_output: &str,
    ) -> Option<Vec<Envelope>> {
        None
    }

    /// Dispatch one outbound message to external channel(s).
    async fn dispatch_outbound(&self, message: &Envelope) -> Option<bool> {
        None
    }

    /// Register CLI commands (synchronous hook).
    fn register_cli_commands(&self, app: &mut clap::Command) {}

    /// Observe framework errors from any stage.
    async fn on_error(&self, stage: &str, error: &anyhow::Error, message: Option<&Envelope>) {}

    /// Build the full system prompt for the agent loop.
    /// Returns `None` to defer to the next plugin, or `Some(String)` with the assembled prompt.
    fn build_system_prompt(&self, prompt_text: &str, state: &State) -> Option<String> {
        None
    }

    /// Wrap a tool before execution. Returns a `ToolAction` to keep, remove, or replace
    /// the tool. Plugins are called in forward order (first-registered first) so that
    /// safety plugins registered early can remove tools before later plugins see them.
    fn wrap_tool(&self, tool: &nexil::Tool) -> nexil::ToolAction {
        nexil::ToolAction::Keep
    }
}

// ---------------------------------------------------------------------------
// HookRuntime
// ---------------------------------------------------------------------------

const HOOK_NAMES: &[&str] = &[
    "classify_inbound",
    "resolve_session",
    "load_state",
    "build_user_prompt",
    "build_system_prompt",
    "run_model",
    "save_state",
    "render_outbound",
    "dispatch_outbound",
    "register_cli_commands",
    "on_error",
    "wrap_tool",
];

/// Executes hooks with fault isolation and precedence semantics.
///
/// # Panic Policy
///
/// - **Upgraded hooks** (`resolve_session`, `load_state`, `run_model`):
///   A panic aborts the hook chain and returns `Err(HookError::Panic)`.
///   No further plugins are consulted.
///
/// - **Non-upgraded hooks** (`build_user_prompt`, `save_state`, `render_outbound`,
///   `dispatch_outbound`, `on_error`, `register_cli_commands`):
///   A panic is caught and logged; execution continues to the next plugin.
#[derive(Clone)]
pub struct HookRuntime {
    plugins: Vec<Arc<dyn EliHookSpec>>,
}

impl HookRuntime {
    /// Create a new runtime from a list of plugins (in registration order).
    pub fn new(plugins: Vec<Arc<dyn EliHookSpec>>) -> Self {
        Self { plugins }
    }

    /// Add a plugin at the end of the registration list.
    pub fn register(&mut self, plugin: Arc<dyn EliHookSpec>) {
        self.plugins.push(plugin);
    }

    /// Return a closure that applies all plugins' `wrap_tool` hooks.
    /// Captures a shared reference to this runtime instead of cloning the plugin list.
    pub fn wrap_tools_fn(self: &Arc<Self>) -> crate::control_plane::WrapToolsFn {
        let rt = Arc::clone(self);
        Arc::new(move |tools| rt.call_wrap_tools(tools))
    }

    /// Return an iterator over plugins in **reverse** registration order (last-registered first).
    fn reversed(&self) -> impl Iterator<Item = &Arc<dyn EliHookSpec>> {
        self.plugins.iter().rev()
    }

    // -- classify inbound (sync, first-result) --------------------------------

    /// Classify inbound message: return the first non-None result.
    pub fn call_classify_inbound(&self, message: &Envelope) -> Option<String> {
        for plugin in self.reversed() {
            let name = plugin.plugin_name();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                plugin.classify_inbound(message)
            })) {
                Ok(Some(decision)) => {
                    tracing::info!(
                        target: "eli_trace",
                        plugin = %name,
                        decision = ?decision,
                        "hook.classify_inbound"
                    );
                    return Some(decision);
                }
                Ok(None) => {}
                Err(panic_info) => {
                    let msg = panic_payload_message(&panic_info);
                    tracing::error!(plugin = %name, panic.message = %msg, "hook.classify_inbound panicked");
                }
            }
        }
        None
    }

    // -- build system prompt (sync, first-result) -----------------------------

    /// Build system prompt: return the first non-None result.
    pub fn call_build_system_prompt(&self, prompt_text: &str, state: &State) -> Option<String> {
        for plugin in self.reversed() {
            let name = plugin.plugin_name();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                plugin.build_system_prompt(prompt_text, state)
            })) {
                Ok(Some(prompt)) => return Some(prompt),
                Ok(None) => {}
                Err(panic_info) => {
                    let msg = panic_payload_message(&panic_info);
                    tracing::error!(plugin = %name, panic.message = %msg, "hook.build_system_prompt panicked");
                }
            }
        }
        None
    }

    // -- wrap tool (sync, all plugins) ----------------------------------------

    /// Wrap tools through all plugins. Each plugin can modify/wrap a tool.
    pub fn call_wrap_tools(&self, tools: Vec<nexil::Tool>) -> Vec<nexil::Tool> {
        let mut result = tools;
        for plugin in self.plugins.iter() {
            let name = plugin.plugin_name();
            result = result
                .into_iter()
                .filter_map(|tool| {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        plugin.wrap_tool(&tool)
                    })) {
                        Ok(nexil::ToolAction::Keep) => Some(tool),
                        Ok(nexil::ToolAction::Remove) => {
                            tracing::info!(
                                plugin = %name,
                                tool = %tool.name,
                                "hook.wrap_tool removed tool"
                            );
                            None
                        }
                        Ok(nexil::ToolAction::Replace(wrapped)) => Some(wrapped),
                        Err(panic_info) => {
                            let msg = panic_payload_message(&panic_info);
                            tracing::error!(plugin = %name, panic.message = %msg, "hook.wrap_tool panicked");
                            Some(tool)
                        }
                    }
                })
                .collect();
        }
        result
    }

    // -- firstresult hooks (async) ------------------------------------------

    /// Resolve session: return the first non-None result.
    pub async fn call_resolve_session(
        &self,
        message: &Envelope,
    ) -> Result<Option<String>, HookError> {
        let session_id = "<resolving>";
        for p in self.reversed() {
            let name = p.plugin_name();
            trace_hook_call(name, session_id, "resolve_session", &preview_json(message));
            let result = std::panic::AssertUnwindSafe(p.resolve_session(message))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(Some(val))) => {
                    trace_hook_return(name, &val, "resolve_session", &preview_text(&val));
                    return Ok(Some(val));
                }
                Ok(Ok(None)) => {
                    trace_hook_none(name, session_id, "resolve_session");
                    continue;
                }
                Ok(Err(e)) => {
                    tracing::warn!(plugin = %name, error = %e, "hook.resolve_session failed");
                    return Err(HookError::wrap(
                        name.to_owned(),
                        HookPoint::ResolveSession,
                        e,
                    ));
                }
                Err(panic_info) => {
                    let msg = panic_payload_message(&panic_info);
                    tracing::warn!(plugin = %name, panic.message = %msg, "hook.resolve_session panicked");
                    return Err(HookError::Panic {
                        plugin: name.to_owned(),
                        message: msg,
                    });
                }
            }
        }
        Ok(None)
    }

    /// Load state: collect all results and merge.
    pub async fn call_load_state(
        &self,
        message: &Envelope,
        session_id: &str,
    ) -> Result<Vec<Option<State>>, HookError> {
        let mut results = Vec::new();
        for p in self.plugins.iter() {
            let name = p.plugin_name();
            trace_hook_call(name, session_id, "load_state", &preview_json(message));
            let result = std::panic::AssertUnwindSafe(p.load_state(message, session_id))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(val)) => {
                    let preview = format!("{} keys", val.as_ref().map_or(0, |s| s.len()));
                    trace_hook_return(name, session_id, "load_state", &preview);
                    results.push(val);
                }
                Ok(Err(e)) => {
                    tracing::warn!(plugin = %name, error = %e, "hook.load_state failed");
                    return Err(HookError::wrap(name.to_owned(), HookPoint::LoadState, e));
                }
                Err(panic_info) => {
                    let msg = panic_payload_message(&panic_info);
                    tracing::warn!(plugin = %name, panic.message = %msg, "hook.load_state panicked");
                    return Err(HookError::Panic {
                        plugin: name.to_owned(),
                        message: msg,
                    });
                }
            }
        }
        Ok(results)
    }

    /// Build prompt: return the first non-None result (non-upgraded — panics skip).
    pub async fn call_build_user_prompt(
        &self,
        message: &Envelope,
        session_id: &str,
        state: &State,
    ) -> Option<PromptValue> {
        for plugin in self.reversed() {
            let name = plugin.plugin_name();
            trace_hook_call(
                name,
                session_id,
                "build_user_prompt",
                &preview_json(message),
            );
            let result =
                std::panic::AssertUnwindSafe(plugin.build_user_prompt(message, session_id, state))
                    .catch_unwind()
                    .await;
            match result {
                Ok(Some(val)) => {
                    trace_hook_return(
                        name,
                        session_id,
                        "build_user_prompt",
                        &preview_text(&val.as_text()),
                    );
                    return Some(val);
                }
                Ok(None) => {
                    trace_hook_none(name, session_id, "build_user_prompt");
                    continue;
                }
                Err(panic_info) => {
                    let msg = panic_payload_message(&panic_info);
                    tracing::error!(plugin = %name, session_id = %session_id, panic.message = %msg, "hook.build_user_prompt panicked");
                    continue;
                }
            }
        }
        None
    }

    /// Run model: return the first non-None result (upgraded — errors propagate).
    pub async fn call_run_model(
        &self,
        prompt: &PromptValue,
        session_id: &str,
        state: &State,
    ) -> Result<Option<String>, HookError> {
        for plugin in self.reversed() {
            let name = plugin.plugin_name();
            trace_hook_call(
                name,
                session_id,
                "run_model",
                &preview_text(&prompt.as_text()),
            );
            let result = std::panic::AssertUnwindSafe(plugin.run_model(prompt, session_id, state))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(Some(val))) => {
                    trace_hook_return(name, session_id, "run_model", &preview_text(&val));
                    return Ok(Some(val));
                }
                Ok(Ok(None)) => {
                    trace_hook_none(name, session_id, "run_model");
                    continue;
                }
                Ok(Err(e)) => {
                    tracing::warn!(plugin = %name, error = %e, "hook.run_model failed");
                    return Err(HookError::wrap(name.to_owned(), HookPoint::RunModel, e));
                }
                Err(panic_info) => {
                    let msg = panic_payload_message(&panic_info);
                    tracing::warn!(plugin = %name, panic.message = %msg, "hook.run_model panicked");
                    return Err(HookError::Panic {
                        plugin: name.to_owned(),
                        message: msg,
                    });
                }
            }
        }
        Ok(None)
    }

    /// Save state: call all implementations (notify pattern).
    pub async fn call_save_state(
        &self,
        session_id: &str,
        state: &State,
        message: &Envelope,
        model_output: &str,
    ) {
        for p in self.plugins.iter() {
            let name = p.plugin_name();
            trace_hook_call(name, session_id, "save_state", &preview_text(model_output));
            let result = std::panic::AssertUnwindSafe(p.save_state(
                session_id,
                state,
                message,
                model_output,
            ))
            .catch_unwind()
            .await;
            match result {
                Ok(()) => trace_hook_return(name, session_id, "save_state", "ok"),
                Err(panic_info) => {
                    let msg = panic_payload_message(&panic_info);
                    tracing::error!(plugin = %name, panic.message = %msg, "hook.save_state panicked");
                }
            }
        }
    }

    /// Render outbound: collect results from all implementations.
    pub async fn call_render_outbound(
        &self,
        message: &Envelope,
        session_id: &str,
        state: &State,
        model_output: &str,
    ) -> Vec<Vec<Envelope>> {
        let mut results = Vec::new();
        for plugin in self.plugins.iter() {
            let name = plugin.plugin_name();
            trace_hook_call(
                name,
                session_id,
                "render_outbound",
                &preview_text(model_output),
            );
            let result = std::panic::AssertUnwindSafe(plugin.render_outbound(
                message,
                session_id,
                state,
                model_output,
            ))
            .catch_unwind()
            .await;
            match result {
                Ok(Some(batch)) => {
                    let preview = batch.first().map(preview_json).unwrap_or_default();
                    trace_hook_return(name, session_id, "render_outbound", &preview);
                    results.push(batch);
                }
                Ok(None) => trace_hook_none(name, session_id, "render_outbound"),
                Err(panic_info) => {
                    let msg = panic_payload_message(&panic_info);
                    tracing::error!(plugin = %name, panic.message = %msg, "hook.render_outbound panicked");
                }
            }
        }
        results
    }

    /// Dispatch outbound: call all implementations.
    pub async fn call_dispatch_outbound(&self, message: &Envelope) {
        let session_id = message
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        for p in self.plugins.iter() {
            let name = p.plugin_name();
            trace_hook_call(
                name,
                session_id,
                "dispatch_outbound",
                &preview_json(message),
            );
            let result = std::panic::AssertUnwindSafe(p.dispatch_outbound(message))
                .catch_unwind()
                .await;
            match result {
                Ok(Some(delivered)) => {
                    trace_hook_return(
                        name,
                        session_id,
                        "dispatch_outbound",
                        if delivered {
                            "delivered"
                        } else {
                            "not_delivered"
                        },
                    );
                }
                Ok(None) => trace_hook_none(name, session_id, "dispatch_outbound"),
                Err(panic_info) => {
                    let msg = panic_payload_message(&panic_info);
                    tracing::error!(plugin = %name, panic.message = %msg, "hook.dispatch_outbound panicked");
                }
            }
        }
    }

    /// Register CLI commands on all plugins (synchronous).
    pub fn call_register_cli_commands(&self, app: &mut clap::Command) {
        call_sync_all!(self.plugins.iter(), "register_cli_commands", |p| p
            .register_cli_commands(app));
    }

    /// Notify all error observers, swallowing any panics/errors from the observers.
    pub async fn notify_error(
        &self,
        stage: &str,
        error: &anyhow::Error,
        message: Option<&Envelope>,
    ) {
        call_notify_all!(self.plugins.iter(), "on_error", |p| p
            .on_error(stage, error, message));
    }

    /// Build a hook-name to adapter-names mapping for diagnostics.
    pub fn hook_report(&self) -> HashMap<String, Vec<String>> {
        let names: Vec<String> = self
            .plugins
            .iter()
            .map(|p| p.plugin_name().to_string())
            .collect();

        if names.is_empty() {
            return HashMap::new();
        }

        HOOK_NAMES
            .iter()
            .map(|&hook| (hook.to_string(), names.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    // -- Test plugins ---------------------------------------------------------

    struct HighPriorityPlugin;

    #[async_trait]
    impl EliHookSpec for HighPriorityPlugin {
        fn plugin_name(&self) -> &str {
            "high"
        }

        async fn resolve_session(&self, _message: &Envelope) -> Result<Option<String>, HookError> {
            Ok(Some("high-session".into()))
        }

        fn build_system_prompt(&self, _prompt_text: &str, _state: &State) -> Option<String> {
            Some("high-prompt".into())
        }

        async fn render_outbound(
            &self,
            _message: &Envelope,
            _session_id: &str,
            _state: &State,
            _model_output: &str,
        ) -> Option<Vec<Envelope>> {
            Some(vec![json!({"content": "high-out"})])
        }
    }

    struct LowPriorityPlugin;

    #[async_trait]
    impl EliHookSpec for LowPriorityPlugin {
        fn plugin_name(&self) -> &str {
            "low"
        }

        async fn resolve_session(&self, _message: &Envelope) -> Result<Option<String>, HookError> {
            Ok(Some("low-session".into()))
        }

        fn build_system_prompt(&self, _prompt_text: &str, _state: &State) -> Option<String> {
            Some("low-prompt".into())
        }
    }

    struct ReturnsNonePlugin;

    #[async_trait]
    impl EliHookSpec for ReturnsNonePlugin {
        fn plugin_name(&self) -> &str {
            "none-plugin"
        }
    }

    struct ErrorObserver {
        observed: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl EliHookSpec for ErrorObserver {
        fn plugin_name(&self) -> &str {
            "error-observer"
        }

        async fn on_error(&self, stage: &str, _error: &anyhow::Error, _message: Option<&Envelope>) {
            self.observed
                .lock()
                .expect("lock poisoned")
                .push(stage.to_owned());
        }
    }

    struct FailingErrorObserver;

    #[async_trait]
    impl EliHookSpec for FailingErrorObserver {
        fn plugin_name(&self) -> &str {
            "failing-observer"
        }

        async fn on_error(
            &self,
            _stage: &str,
            _error: &anyhow::Error,
            _message: Option<&Envelope>,
        ) {
            panic!("observer panic");
        }
    }

    struct PanicSessionPlugin;

    #[async_trait]
    impl EliHookSpec for PanicSessionPlugin {
        fn plugin_name(&self) -> &str {
            "panic-session"
        }

        async fn resolve_session(&self, _message: &Envelope) -> Result<Option<String>, HookError> {
            panic!("resolve_session panic");
        }
    }

    // -- call_resolve_session (call_first semantics) -------------------------

    #[tokio::test]
    async fn test_call_first_returns_last_registered_non_none() {
        // Low registered first, High registered last.
        // reversed() iterates High first, which returns Some -> wins.
        let rt = HookRuntime::new(vec![
            Arc::new(LowPriorityPlugin) as Arc<dyn EliHookSpec>,
            Arc::new(HighPriorityPlugin),
        ]);
        let msg = json!({"content": "hello"});
        let result = rt.call_resolve_session(&msg).await.unwrap();
        assert_eq!(result, Some("high-session".into()));
    }

    #[tokio::test]
    async fn test_call_first_skips_none_and_returns_next() {
        // ReturnsNone registered last -> reversed first, returns None.
        // LowPriority next -> returns Some("low-session").
        let rt = HookRuntime::new(vec![
            Arc::new(LowPriorityPlugin) as Arc<dyn EliHookSpec>,
            Arc::new(ReturnsNonePlugin),
        ]);
        let msg = json!({"content": "hello"});
        let result = rt.call_resolve_session(&msg).await.unwrap();
        assert_eq!(result, Some("low-session".into()));
    }

    #[tokio::test]
    async fn test_call_first_returns_none_when_all_return_none() {
        let rt = HookRuntime::new(vec![Arc::new(ReturnsNonePlugin) as Arc<dyn EliHookSpec>]);
        let msg = json!({"content": "hello"});
        let result = rt.call_resolve_session(&msg).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_call_first_propagates_panic_as_error() {
        let rt = HookRuntime::new(vec![
            Arc::new(LowPriorityPlugin) as Arc<dyn EliHookSpec>,
            Arc::new(PanicSessionPlugin),
        ]);
        let msg = json!({"content": "hello"});
        let result = rt.call_resolve_session(&msg).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HookError::Panic { ref plugin, .. } if plugin == "panic-session"));
    }

    // -- call_build_system_prompt (first-result sync) -------------------------

    #[tokio::test]
    async fn test_call_build_system_prompt_returns_first_result() {
        let rt = HookRuntime::new(vec![
            Arc::new(LowPriorityPlugin) as Arc<dyn EliHookSpec>,
            Arc::new(HighPriorityPlugin),
        ]);
        let state = State::new();
        let result = rt.call_build_system_prompt("hello", &state);
        // Last-registered (High) wins
        assert_eq!(result, Some("high-prompt".into()));
    }

    #[tokio::test]
    async fn test_call_build_system_prompt_skips_none_results() {
        let rt = HookRuntime::new(vec![
            Arc::new(LowPriorityPlugin) as Arc<dyn EliHookSpec>,
            Arc::new(ReturnsNonePlugin),
        ]);
        let state = State::new();
        let result = rt.call_build_system_prompt("hello", &state);
        assert_eq!(result, Some("low-prompt".into()));
    }

    // -- call_render_outbound (call_many async) ------------------------------

    #[tokio::test]
    async fn test_call_render_outbound_collects_all() {
        let rt = HookRuntime::new(vec![
            Arc::new(HighPriorityPlugin) as Arc<dyn EliHookSpec>,
            Arc::new(ReturnsNonePlugin),
        ]);
        let msg = json!({"content": "hello"});
        let state = State::new();
        let result = rt.call_render_outbound(&msg, "s1", &state, "output").await;
        // Only HighPriorityPlugin returns Some
        assert_eq!(result.len(), 1);
        assert_eq!(result[0][0], json!({"content": "high-out"}));
    }

    // -- notify_error swallows observer failures -----------------------------

    #[tokio::test]
    async fn test_notify_error_calls_all_observers() {
        let observer = Arc::new(ErrorObserver {
            observed: std::sync::Mutex::new(Vec::new()),
        });
        let rt = HookRuntime::new(vec![
            Arc::new(FailingErrorObserver) as Arc<dyn EliHookSpec>,
            observer.clone() as Arc<dyn EliHookSpec>,
        ]);
        let err = anyhow::anyhow!("test error");
        rt.notify_error("turn", &err, None).await;
        let observed = observer.observed.lock().expect("lock poisoned");
        assert_eq!(*observed, vec!["turn"]);
    }

    #[tokio::test]
    async fn test_notify_error_with_message() {
        let observer = Arc::new(ErrorObserver {
            observed: std::sync::Mutex::new(Vec::new()),
        });
        let rt = HookRuntime::new(vec![observer.clone() as Arc<dyn EliHookSpec>]);
        let err = anyhow::anyhow!("test error");
        let msg = json!({"content": "hello"});
        rt.notify_error("pipeline", &err, Some(&msg)).await;
        let observed = observer.observed.lock().expect("lock poisoned");
        assert_eq!(*observed, vec!["pipeline"]);
    }

    // -- hook_report ---------------------------------------------------------

    #[test]
    fn test_hook_report_lists_all_registered_plugins() {
        let rt = HookRuntime::new(vec![
            Arc::new(LowPriorityPlugin) as Arc<dyn EliHookSpec>,
            Arc::new(HighPriorityPlugin),
        ]);
        let report = rt.hook_report();
        assert!(report.contains_key("resolve_session"));
        assert_eq!(report["resolve_session"], vec!["low", "high"]);
        assert!(report.contains_key("build_system_prompt"));
    }

    #[test]
    fn test_hook_report_empty_when_no_plugins() {
        let rt = HookRuntime::new(vec![]);
        let report = rt.hook_report();
        assert!(report.is_empty());
    }

    // -- register ------------------------------------------------------------

    #[test]
    fn test_register_adds_plugin() {
        let mut rt = HookRuntime::new(vec![]);
        assert!(rt.hook_report().is_empty());
        rt.register(Arc::new(LowPriorityPlugin));
        let report = rt.hook_report();
        assert_eq!(report["resolve_session"], vec!["low"]);
    }

    // -- call_load_state error/panic handling ---------------------------------

    struct PanicLoadStatePlugin;

    #[async_trait]
    impl EliHookSpec for PanicLoadStatePlugin {
        fn plugin_name(&self) -> &str {
            "panic-load-state"
        }

        async fn load_state(
            &self,
            _message: &Envelope,
            _session_id: &str,
        ) -> Result<Option<State>, HookError> {
            panic!("load_state panic");
        }
    }

    struct ErrorLoadStatePlugin;

    #[async_trait]
    impl EliHookSpec for ErrorLoadStatePlugin {
        fn plugin_name(&self) -> &str {
            "error-load-state"
        }

        async fn load_state(
            &self,
            _message: &Envelope,
            _session_id: &str,
        ) -> Result<Option<State>, HookError> {
            Err(HookError::Plugin {
                plugin: "error-load-state".into(),
                hook_point: HookPoint::LoadState,
                source: anyhow::anyhow!("state unavailable"),
            })
        }
    }

    #[tokio::test]
    async fn test_call_load_state_propagates_panic_as_error() {
        let rt = HookRuntime::new(vec![Arc::new(PanicLoadStatePlugin) as Arc<dyn EliHookSpec>]);
        let msg = json!({"content": "hello"});
        let result = rt.call_load_state(&msg, "s1").await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), HookError::Panic { ref plugin, .. } if plugin == "panic-load-state")
        );
    }

    #[tokio::test]
    async fn test_call_load_state_propagates_plugin_error() {
        let rt = HookRuntime::new(vec![Arc::new(ErrorLoadStatePlugin) as Arc<dyn EliHookSpec>]);
        let msg = json!({"content": "hello"});
        let result = rt.call_load_state(&msg, "s1").await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), HookError::Plugin { hook_point, .. } if hook_point == HookPoint::LoadState)
        );
    }

    // -- call_run_model error/panic handling ----------------------------------

    struct PanicRunModelPlugin;

    #[async_trait]
    impl EliHookSpec for PanicRunModelPlugin {
        fn plugin_name(&self) -> &str {
            "panic-run-model"
        }

        async fn run_model(
            &self,
            _prompt: &PromptValue,
            _session_id: &str,
            _state: &State,
        ) -> Result<Option<String>, HookError> {
            panic!("run_model panic");
        }
    }

    struct ErrorRunModelPlugin;

    #[async_trait]
    impl EliHookSpec for ErrorRunModelPlugin {
        fn plugin_name(&self) -> &str {
            "error-run-model"
        }

        async fn run_model(
            &self,
            _prompt: &PromptValue,
            _session_id: &str,
            _state: &State,
        ) -> Result<Option<String>, HookError> {
            Err(HookError::Plugin {
                plugin: "error-run-model".into(),
                hook_point: HookPoint::RunModel,
                source: anyhow::anyhow!("model unavailable"),
            })
        }
    }

    #[tokio::test]
    async fn test_call_run_model_propagates_panic_as_error() {
        let rt = HookRuntime::new(vec![Arc::new(PanicRunModelPlugin) as Arc<dyn EliHookSpec>]);
        let prompt = PromptValue::Text("hello".into());
        let state = State::new();
        let result = rt.call_run_model(&prompt, "s1", &state).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), HookError::Panic { ref plugin, .. } if plugin == "panic-run-model")
        );
    }

    #[tokio::test]
    async fn test_call_run_model_propagates_plugin_error() {
        let rt = HookRuntime::new(vec![Arc::new(ErrorRunModelPlugin) as Arc<dyn EliHookSpec>]);
        let prompt = PromptValue::Text("hello".into());
        let state = State::new();
        let result = rt.call_run_model(&prompt, "s1", &state).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), HookError::Plugin { hook_point, .. } if hook_point == HookPoint::RunModel)
        );
    }

    #[test]
    fn test_preview_json_truncates_multibyte_content_on_char_boundary() {
        let message = json!({
            "content": "你".repeat(400),
            "channel": "test",
        });

        let preview = preview_json(&message);

        assert!(preview.ends_with("...(truncated)"));
        assert!(!preview.contains('\u{fffd}'));
        assert!(preview.contains("你"));
    }

    // -- call_build_user_prompt panic skipping ------------------------------------

    struct PanicBuildPromptPlugin;

    #[async_trait]
    impl EliHookSpec for PanicBuildPromptPlugin {
        fn plugin_name(&self) -> &str {
            "panic-build-prompt"
        }

        async fn build_user_prompt(
            &self,
            _message: &Envelope,
            _session_id: &str,
            _state: &State,
        ) -> Option<PromptValue> {
            panic!("build_user_prompt panic");
        }
    }

    struct BuildPromptFallbackPlugin;

    #[async_trait]
    impl EliHookSpec for BuildPromptFallbackPlugin {
        fn plugin_name(&self) -> &str {
            "build-prompt-fallback"
        }

        async fn build_user_prompt(
            &self,
            _message: &Envelope,
            _session_id: &str,
            _state: &State,
        ) -> Option<PromptValue> {
            Some(PromptValue::Text("fallback-prompt".into()))
        }
    }

    #[tokio::test]
    async fn test_call_build_user_prompt_skips_panicking_plugin() {
        // PanicBuildPromptPlugin registered last (highest priority, tried first in reversed).
        // It panics → skipped. BuildPromptFallbackPlugin tried next → returns Some.
        let rt = HookRuntime::new(vec![
            Arc::new(BuildPromptFallbackPlugin) as Arc<dyn EliHookSpec>,
            Arc::new(PanicBuildPromptPlugin),
        ]);
        let msg = json!({"content": "hello"});
        let state = State::new();
        let result = rt.call_build_user_prompt(&msg, "s1", &state).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().as_text(), "fallback-prompt");
    }
}
