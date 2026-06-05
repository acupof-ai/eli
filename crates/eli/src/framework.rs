//! Hook-first Eli framework runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::Value;
use tokio::sync::RwLock;

use nexil::CancellationToken;

use crate::builtin::config::EliConfig;
use crate::control_plane::{BudgetLedger, DispatchFn, TurnContext, turn_usage, with_turn_context};
use crate::envelope::{ValueExt, unpack_batch_vec};
use crate::evolution::{
    AutoEvolutionLoop, AutoEvolutionPolicy, EvolutionStore, load_runtime_policy_for_workspace,
};
use crate::hooks::{ChannelHook, EliHookSpec, HookRuntime, TapeStoreKind};
use crate::types::{
    Envelope, MessageHandler, PromptValue, RUNTIME_SYSTEM_PROMPT_KEY, RUNTIME_TAPES_DIR_KEY,
    RUNTIME_WORKSPACE_KEY, State, TurnResult, TurnUsageInfo,
};

// ---------------------------------------------------------------------------
// PluginStatus
// ---------------------------------------------------------------------------

/// Records whether a plugin loaded successfully.
#[derive(Debug, Clone)]
pub struct PluginStatus {
    pub is_success: bool,
    pub detail: Option<String>,
}

// ---------------------------------------------------------------------------
// EliFramework
// ---------------------------------------------------------------------------

/// Minimal framework core. Everything grows from hook plugins.
pub struct EliFramework {
    /// Working directory / project root.
    pub workspace: RwLock<PathBuf>,
    /// The hook runtime that dispatches to registered plugins.
    /// Uses `ArcSwap` for lock-free snapshot reads on the hot path.
    hook_runtime: ArcSwap<HookRuntime>,
    /// Status of each loaded plugin.
    plugin_status: RwLock<HashMap<String, PluginStatus>>,
    /// Token budget ledger (control plane infrastructure).
    pub budget: BudgetLedger,
}

impl EliFramework {
    /// Create a new framework instance rooted at the current working directory.
    pub fn new() -> Self {
        Self {
            workspace: RwLock::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
            hook_runtime: ArcSwap::from_pointee(HookRuntime::new(Vec::new())),
            plugin_status: RwLock::new(HashMap::new()),

            budget: BudgetLedger::new(),
        }
    }

    /// Create a framework with a given workspace path.
    pub fn with_workspace(workspace: PathBuf) -> Self {
        Self {
            workspace: RwLock::new(workspace),
            hook_runtime: ArcSwap::from_pointee(HookRuntime::new(Vec::new())),
            plugin_status: RwLock::new(HashMap::new()),

            budget: BudgetLedger::new(),
        }
    }

    // -- Plugin registration ------------------------------------------------

    /// Register a plugin with the framework.
    pub async fn register_plugin(&self, name: &str, plugin: Arc<dyn EliHookSpec>) {
        self.hook_runtime.rcu(|current| {
            let mut new_rt = (**current).clone();
            new_rt.register(plugin.clone());
            new_rt
        });
        let mut status = self.plugin_status.write().await;
        status.insert(
            name.to_string(),
            PluginStatus {
                is_success: true,
                detail: None,
            },
        );
    }

    /// Record a plugin load failure.
    pub async fn record_plugin_failure(&self, name: &str, detail: String) {
        let mut status = self.plugin_status.write().await;
        status.insert(
            name.to_string(),
            PluginStatus {
                is_success: false,
                detail: Some(detail),
            },
        );
    }

    /// Load hooks from a list of plugins. This is the Rust equivalent of
    /// the Python `load_hooks` which discovers plugins via entry points.
    /// In Rust, callers explicitly provide the plugin list.
    pub async fn load_hooks(&self, plugins: Vec<(String, Arc<dyn EliHookSpec>)>) {
        for (name, plugin) in plugins {
            self.register_plugin(&name, plugin).await;
        }
    }

    // -- Main orchestration loop --------------------------------------------

    /// Run one inbound message through all hooks and return the turn result.
    pub async fn process_inbound(&self, mut inbound: Envelope) -> anyhow::Result<TurnResult> {
        use std::time::Instant;

        let turn_start = Instant::now();
        Self::strip_internal_context(&mut inbound);

        let t0 = Instant::now();
        let rt = self.hook_runtime_snapshot();
        let workspace = self.workspace.read().await.clone();
        let snapshot_ms = t0.elapsed().as_micros();

        if let Some(result) = Self::try_greet_shortcircuit(&rt, &inbound) {
            for outbound in &result.outbounds {
                rt.call_dispatch_outbound(outbound).await;
            }
            return Ok(result);
        }

        // Build turn context: cancellation token + tool wrapping from hooks.
        let rt_clone = rt.clone();
        let dispatch: DispatchFn = Arc::new(move |envelope| {
            let rt = rt_clone.clone();
            Box::pin(async move {
                rt.call_dispatch_outbound(&envelope).await;
            })
        });
        let turn_ctx = TurnContext {
            cancellation: CancellationToken::new(),
            wrap_tools: Some(rt.wrap_tools_fn()),
            usage: Default::default(),
            save_events: Default::default(),
            dispatch: Some(dispatch),
            outbound_media: Default::default(),
        };

        with_turn_context(turn_ctx, async {
            let t0 = Instant::now();
            let session_id = self.resolve_session_id(&rt, &inbound).await;
            let resolve_us = t0.elapsed().as_micros();
            Self::inject_session_id(&mut inbound, &session_id);

            let t0 = Instant::now();
            let state = self
                .build_state(&rt, &inbound, &session_id, &workspace)
                .await;
            let load_state_us = t0.elapsed().as_micros();

            // --- Greeting on join / new session ---
            let is_join = inbound.get("kind").and_then(|v| v.as_str()) == Some("join");
            let is_new_session = state
                .get("_is_new_session")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if is_join || is_new_session {
                if let Some(greeting) = EliConfig::load().greeting_message() {
                    let outbound = Self::build_greeting_outbound(&inbound, &session_id, &greeting);
                    rt.call_dispatch_outbound(&outbound).await;
                }

                if is_join {
                    return Ok(TurnResult {
                        session_id,
                        prompt: PromptValue::Text(String::new()),
                        model_output: String::new(),
                        outbounds: Vec::new(),
                        usage: TurnUsageInfo::default(),
                    });
                }
            }

            let t0 = Instant::now();
            let prompt = Self::build_prompt(&rt, &inbound, &session_id, &state).await;
            let build_prompt_us = t0.elapsed().as_micros();

            let t0 = Instant::now();
            let state = Self::state_with_system_prompt(&rt, &prompt, state);
            let sys_prompt_us = t0.elapsed().as_micros();

            let t0 = Instant::now();
            let model_output = Self::run_model(&rt, &prompt, &session_id, &state, &inbound).await;
            let run_model_us = t0.elapsed().as_micros();

            let preview_len = model_output.floor_char_boundary(120);
            tracing::info!(
                session_id = %session_id,
                output_len = model_output.len(),
                output_preview = %&model_output[..preview_len],
                "run_model completed"
            );

            let t0 = Instant::now();
            rt.call_save_state(&session_id, &state, &inbound, &model_output)
                .await;
            self.spawn_auto_evolution(&session_id, &state);
            let save_state_us = t0.elapsed().as_micros();

            let t0 = Instant::now();
            let outbounds = self
                .collect_outbounds(&rt, &inbound, &session_id, &state, &model_output)
                .await;
            let render_us = t0.elapsed().as_micros();

            let t0 = Instant::now();
            for outbound in &outbounds {
                rt.call_dispatch_outbound(outbound).await;
            }
            let dispatch_us = t0.elapsed().as_micros();

            let total_us = turn_start.elapsed().as_micros();

            tracing::info!(
                session_id = %session_id,
                total_ms = total_us / 1000,
                snapshot_us = snapshot_ms,
                resolve_us,
                load_state_us,
                build_prompt_us,
                sys_prompt_us,
                run_model_ms = run_model_us / 1000,
                save_state_us,
                render_us,
                dispatch_us,
                "turn.timing"
            );

            let usage = turn_usage()
                .map(|u| TurnUsageInfo {
                    input_tokens: u.input_tokens(),
                    output_tokens: u.output_tokens(),
                    total_tokens: u.total_tokens(),
                })
                .unwrap_or_default();

            Ok(TurnResult {
                session_id,
                prompt,
                model_output,
                outbounds,
                usage,
            })
        })
        .await
    }

    fn spawn_auto_evolution(&self, session_id: &str, state: &State) {
        let Some(ctx) = auto_evolution_context(session_id, state) else {
            return;
        };
        tokio::spawn(async move {
            let result = AutoEvolutionLoop::new(ctx.policy.clone()).run(
                &EvolutionStore::new(&ctx.workspace),
                &ctx.tapes_dir,
                &ctx.tape_name,
            );
            log_auto_evolution_result(&ctx, result);
        });
    }

    /// Load a snapshot of the hook runtime. Lock-free via `ArcSwap`.
    fn hook_runtime_snapshot(&self) -> Arc<HookRuntime> {
        self.hook_runtime.load_full()
    }

    fn strip_internal_context(inbound: &mut Envelope) {
        if let Some(ctx) = inbound.get_mut("context").and_then(|v| v.as_object_mut()) {
            ctx.remove("_eli_cleanup_only");
        }
    }

    fn try_greet_shortcircuit(rt: &HookRuntime, inbound: &Envelope) -> Option<TurnResult> {
        let crate::smart_router::RouteDecision::Greet(reply) = rt.call_classify_inbound(inbound)?;

        let session_id = Self::default_session_id(inbound);
        let channel = inbound.field_str("channel", "");
        let chat_id = inbound.field_str("chat_id", "");
        let output_channel = inbound
            .get("output_channel")
            .and_then(|v| v.as_str())
            .unwrap_or(&channel)
            .to_owned();

        let outbound = serde_json::json!({
            "content": reply,
            "session_id": session_id,
            "channel": channel,
            "chat_id": chat_id,
            "output_channel": output_channel,
        });
        Some(TurnResult {
            session_id,
            prompt: PromptValue::Text(String::new()),
            model_output: reply.clone(),
            outbounds: vec![outbound],
            usage: TurnUsageInfo::default(),
        })
    }

    /// Build an outbound envelope for a greeting message.
    fn build_greeting_outbound(inbound: &Envelope, session_id: &str, greeting: &str) -> Envelope {
        let channel = inbound.field_str("channel", "");
        let chat_id = inbound.field_str("chat_id", "");
        let output_channel = inbound
            .get("output_channel")
            .and_then(|v| v.as_str())
            .unwrap_or(&channel)
            .to_owned();

        serde_json::json!({
            "content": greeting,
            "session_id": session_id,
            "channel": channel,
            "chat_id": chat_id,
            "output_channel": output_channel,
            // Greetings dispatch before the LLM runs and are followed by the
            // final reply on the same session. Channel plugins (feishu-cli)
            // that consume per-turn state on final replies must skip this
            // one or they pop the inflight batch belonging to the user's
            // actual question.
            "context": { "_eli_mid_turn": true },
        })
    }

    async fn resolve_session_id(&self, rt: &HookRuntime, inbound: &Envelope) -> String {
        match rt.call_resolve_session(inbound).await {
            Ok(Some(id)) => id,
            Ok(None) => Self::default_session_id(inbound),
            Err(e) => {
                tracing::warn!(error = %e, "resolve_session failed, using default session id");
                Self::default_session_id(inbound)
            }
        }
    }

    fn inject_session_id(inbound: &mut Envelope, session_id: &str) {
        if let Some(obj) = inbound.as_object_mut() {
            obj.entry("session_id")
                .or_insert_with(|| Value::String(session_id.to_owned()));
        }
    }

    /// Build the merged state for one turn.
    ///
    /// **State merge precedence**: `call_load_state` collects results in
    /// forward registration order, then this method iterates them in
    /// *reverse* and uses `or_insert` — so **last-registered plugin's
    /// value wins** for duplicate keys, consistent with first-result
    /// hooks (e.g. `resolve_session`, `build_system_prompt`).
    async fn build_state(
        &self,
        rt: &HookRuntime,
        inbound: &Envelope,
        session_id: &str,
        workspace: &std::path::Path,
    ) -> State {
        let mut state: State = HashMap::new();
        state.insert(
            RUNTIME_WORKSPACE_KEY.to_string(),
            Value::String(workspace.to_string_lossy().to_string()),
        );

        let hook_states = match rt.call_load_state(inbound, session_id).await {
            Ok(states) => states,
            Err(e) => {
                tracing::warn!(error = %e, session_id = %session_id, "load_state failed, using empty state");
                Vec::new()
            }
        };
        for hs in hook_states.into_iter().rev().flatten() {
            for (k, v) in hs {
                state.entry(k).or_insert(v);
            }
        }
        state
    }

    async fn build_prompt(
        rt: &HookRuntime,
        inbound: &Envelope,
        session_id: &str,
        state: &State,
    ) -> PromptValue {
        rt.call_build_user_prompt(inbound, session_id, state)
            .await
            .unwrap_or_else(|| PromptValue::Text(inbound.content_text()))
    }

    fn state_with_system_prompt(rt: &HookRuntime, prompt: &PromptValue, mut state: State) -> State {
        let system_prompt = rt.call_build_system_prompt(&prompt.as_text(), &state);
        if let Some(system_prompt) = system_prompt.filter(|text| !text.is_empty()) {
            state.insert(
                RUNTIME_SYSTEM_PROMPT_KEY.to_owned(),
                Value::String(system_prompt),
            );
        }
        state
    }

    async fn run_model(
        rt: &HookRuntime,
        prompt: &PromptValue,
        session_id: &str,
        state: &State,
        inbound: &Envelope,
    ) -> String {
        match rt.call_run_model(prompt, session_id, state).await {
            Ok(Some(ref output)) if output.is_empty() => {
                tracing::warn!(
                    session_id,
                    "run_model returned empty output — possible content filter"
                );
                "(model returned empty response)".to_owned()
            }
            Ok(Some(output)) => output,
            Ok(None) => {
                let err = anyhow::anyhow!("no model skill returned output");
                rt.notify_error("run_model:fallback", &err, Some(inbound))
                    .await;
                "[Error: no model plugin available]".to_owned()
            }
            Err(e) => {
                let err_msg = format!("{e}");
                let err = anyhow::anyhow!("run_model hook failed: {}", err_msg);
                rt.notify_error("run_model", &err, Some(inbound)).await;
                format!("[Error: {err_msg}]")
            }
        }
    }

    // -- Diagnostics --------------------------------------------------------

    /// Return hook implementation summary for diagnostics.
    pub async fn hook_report(&self) -> HashMap<String, Vec<String>> {
        let rt = self.hook_runtime.load();
        rt.hook_report()
    }

    /// Return the plugin status map.
    pub async fn plugin_status(&self) -> HashMap<String, PluginStatus> {
        self.plugin_status.read().await.clone()
    }

    // -- Channel and tape store accessors -----------------------------------

    /// Collect channels from all plugins, deduplicating by name.
    pub async fn get_channels(
        &self,
        message_handler: MessageHandler,
    ) -> HashMap<String, Box<dyn ChannelHook>> {
        let rt = self.hook_runtime.load();
        let all_channels = rt.call_provide_channels(message_handler);
        let mut map: HashMap<String, Box<dyn ChannelHook>> = HashMap::new();
        for ch in all_channels {
            let name = ch.name().to_string();
            map.entry(name).or_insert(ch);
        }
        map
    }

    /// Get the first provided tape store.
    pub async fn get_tape_store(&self) -> Option<TapeStoreKind> {
        let rt = self.hook_runtime.load();
        rt.call_provide_tape_store()
    }

    /// Build the system prompt via the hook chain.
    pub async fn get_system_prompt(&self, prompt: &PromptValue, state: &State) -> String {
        let rt = self.hook_runtime.load();
        rt.call_build_system_prompt(&prompt.as_text(), state)
            .unwrap_or_default()
    }

    // -- Internal helpers ---------------------------------------------------

    /// Compute a default session ID from the envelope fields.
    fn default_session_id(message: &Envelope) -> String {
        // Try session_id field first
        if let Some(obj) = message.as_object()
            && let Some(Value::String(sid)) = obj.get("session_id")
        {
            return sid.clone();
        }
        let channel = message.field_str("channel", "default");
        let chat_id = message.field_str("chat_id", "default");
        format!("{channel}:{chat_id}")
    }

    /// Collect outbound envelopes from render hooks, falling back to a default
    /// envelope containing the model output.
    async fn collect_outbounds(
        &self,
        rt: &HookRuntime,
        message: &Envelope,
        session_id: &str,
        state: &State,
        model_output: &str,
    ) -> Vec<Envelope> {
        let batches = rt
            .call_render_outbound(message, session_id, state, model_output)
            .await;
        if !batches.is_empty() {
            return unpack_batch_vec(batches);
        }

        // Fallback: build a default outbound envelope
        let mut fallback = serde_json::Map::new();
        fallback.insert(
            "content".to_string(),
            Value::String(model_output.to_string()),
        );
        fallback.insert(
            "session_id".to_string(),
            Value::String(session_id.to_string()),
        );

        if let Some(channel) = message.field("channel", None) {
            fallback.insert("channel".to_string(), channel);
        }
        if let Some(chat_id) = message.field("chat_id", None) {
            fallback.insert("chat_id".to_string(), chat_id);
        }

        vec![Value::Object(fallback)]
    }
}

struct AutoEvolutionContext {
    workspace: PathBuf,
    tapes_dir: PathBuf,
    tape_name: String,
    policy: AutoEvolutionPolicy,
}

fn auto_evolution_context(session_id: &str, state: &State) -> Option<AutoEvolutionContext> {
    if auto_evolution_disabled() {
        return None;
    }
    let workspace = runtime_path(state, RUNTIME_WORKSPACE_KEY)?;
    let policy = auto_evolution_policy(&workspace)?;
    Some(AutoEvolutionContext {
        workspace: workspace.clone(),
        tapes_dir: runtime_path(state, RUNTIME_TAPES_DIR_KEY)?,
        tape_name: crate::builtin::tape::TapeService::session_tape_name(session_id, &workspace),
        policy,
    })
}

fn runtime_path(state: &State, key: &str) -> Option<PathBuf> {
    state.get(key).and_then(Value::as_str).map(PathBuf::from)
}

fn auto_evolution_disabled() -> bool {
    matches!(
        std::env::var("ELI_EVOLUTION_DISABLED").ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn auto_evolution_policy(workspace: &std::path::Path) -> Option<AutoEvolutionPolicy> {
    let policy = load_runtime_policy_for_workspace(workspace).ok()?;
    policy.apply_to_auto_policy(AutoEvolutionPolicy::default())
}

fn log_auto_evolution_result(
    ctx: &AutoEvolutionContext,
    result: anyhow::Result<crate::evolution::AutoEvolutionReport>,
) {
    match result {
        Ok(report) => tracing::info!(
            tape = %ctx.tape_name,
            distilled = report.distill.candidates.len(),
            observed = report.observed.len(),
            staged = report.staged.len(),
            promoted = report.promoted.len(),
            expired = report.expired.len(),
            "auto evolution completed"
        ),
        Err(error) => {
            tracing::warn!(tape = %ctx.tape_name, error = %error, "auto evolution failed")
        }
    }
}

impl Default for EliFramework {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{EliHookSpec, HookError};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Arc;

    struct SessionPlugin;

    #[async_trait]
    impl EliHookSpec for SessionPlugin {
        fn plugin_name(&self) -> &str {
            "session-plugin"
        }

        async fn resolve_session(&self, message: &Envelope) -> Result<Option<String>, HookError> {
            Ok(message
                .as_object()
                .and_then(|o| o.get("session_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned()))
        }
    }

    struct PromptPlugin {
        fragment: String,
    }

    #[async_trait]
    impl EliHookSpec for PromptPlugin {
        fn plugin_name(&self) -> &str {
            "prompt-plugin"
        }

        fn build_system_prompt(&self, _prompt_text: &str, _state: &State) -> Option<String> {
            Some(self.fragment.clone())
        }
    }

    struct ModelPlugin;

    #[async_trait]
    impl EliHookSpec for ModelPlugin {
        fn plugin_name(&self) -> &str {
            "model-plugin"
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

    struct SystemPromptStateModelPlugin {
        fragment: String,
    }

    #[async_trait]
    impl EliHookSpec for SystemPromptStateModelPlugin {
        fn plugin_name(&self) -> &str {
            "system-prompt-state-model-plugin"
        }

        fn build_system_prompt(&self, _prompt_text: &str, _state: &State) -> Option<String> {
            Some(self.fragment.clone())
        }

        async fn run_model(
            &self,
            _prompt: &PromptValue,
            _session_id: &str,
            state: &State,
        ) -> Result<Option<String>, HookError> {
            Ok(Some(
                state
                    .get(RUNTIME_SYSTEM_PROMPT_KEY)
                    .and_then(|v| v.as_str())
                    .unwrap_or("missing")
                    .to_owned(),
            ))
        }
    }

    struct BlockingModelPlugin {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    struct UnicodeBoundaryModelPlugin;

    #[async_trait]
    impl EliHookSpec for UnicodeBoundaryModelPlugin {
        fn plugin_name(&self) -> &str {
            "unicode-boundary-model-plugin"
        }

        async fn run_model(
            &self,
            _prompt: &PromptValue,
            _session_id: &str,
            _state: &State,
        ) -> Result<Option<String>, HookError> {
            Ok(Some(format!("{}完继续", "a".repeat(118))))
        }
    }

    #[async_trait]
    impl EliHookSpec for BlockingModelPlugin {
        fn plugin_name(&self) -> &str {
            "blocking-model-plugin"
        }

        async fn run_model(
            &self,
            prompt: &PromptValue,
            _session_id: &str,
            _state: &State,
        ) -> Result<Option<String>, HookError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(Some(format!("echo: {}", prompt.as_text())))
        }
    }

    // -- Creation tests -------------------------------------------------------

    #[tokio::test]
    async fn test_framework_creation_default() {
        let fw = EliFramework::new();
        let ws = fw.workspace.read().await;
        assert!(ws.is_absolute() || ws.as_path() == std::path::Path::new("."));
    }

    #[tokio::test]
    async fn test_framework_with_workspace() {
        let fw = EliFramework::with_workspace("/tmp/test".into());
        let ws = fw.workspace.read().await;
        assert_eq!(*ws, std::path::PathBuf::from("/tmp/test"));
    }

    // -- Plugin registration --------------------------------------------------

    #[tokio::test]
    async fn test_register_plugin_records_status() {
        let fw = EliFramework::new();
        fw.register_plugin("test", Arc::new(SessionPlugin)).await;
        let status = fw.plugin_status().await;
        assert!(status.contains_key("test"));
        assert!(status["test"].is_success);
    }

    #[tokio::test]
    async fn test_record_plugin_failure() {
        let fw = EliFramework::new();
        fw.record_plugin_failure("bad", "failed to load".into())
            .await;
        let status = fw.plugin_status().await;
        assert!(!status["bad"].is_success);
        assert_eq!(status["bad"].detail.as_deref(), Some("failed to load"));
    }

    #[tokio::test]
    async fn test_load_hooks_registers_multiple_plugins() {
        let fw = EliFramework::new();
        fw.load_hooks(vec![
            (
                "session".into(),
                Arc::new(SessionPlugin) as Arc<dyn EliHookSpec>,
            ),
            (
                "model".into(),
                Arc::new(ModelPlugin) as Arc<dyn EliHookSpec>,
            ),
        ])
        .await;
        let status = fw.plugin_status().await;
        assert_eq!(status.len(), 2);
        assert!(status["session"].is_success);
        assert!(status["model"].is_success);
    }

    // -- get_system_prompt ----------------------------------------------------

    #[tokio::test]
    async fn test_get_system_prompt_returns_first_result() {
        let fw = EliFramework::new();
        fw.register_plugin(
            "low",
            Arc::new(PromptPlugin {
                fragment: "low".into(),
            }),
        )
        .await;
        fw.register_plugin(
            "high",
            Arc::new(PromptPlugin {
                fragment: "high".into(),
            }),
        )
        .await;
        let prompt = PromptValue::Text("hello".into());
        let state = State::new();
        let result = fw.get_system_prompt(&prompt, &state).await;
        // Last-registered (high) wins
        assert_eq!(result, "high");
    }

    // -- process_inbound (full pipeline) --------------------------------------

    #[tokio::test]
    async fn test_process_inbound_full_pipeline() {
        let fw = EliFramework::new();
        fw.register_plugin("session", Arc::new(SessionPlugin) as Arc<dyn EliHookSpec>)
            .await;
        fw.register_plugin("model", Arc::new(ModelPlugin) as Arc<dyn EliHookSpec>)
            .await;

        let msg = json!({"content": "ping", "session_id": "test-session"});
        let result = fw.process_inbound(msg).await.unwrap();
        assert_eq!(result.session_id, "test-session");
        assert!(result.model_output.starts_with("echo: "));
        assert!(!result.outbounds.is_empty());
    }

    #[tokio::test]
    async fn test_process_inbound_default_session_id() {
        let fw = EliFramework::new();
        fw.register_plugin("model", Arc::new(ModelPlugin) as Arc<dyn EliHookSpec>)
            .await;

        let msg = json!({"content": "hello", "channel": "telegram", "chat_id": "42"});
        let result = fw.process_inbound(msg).await.unwrap();
        assert_eq!(result.session_id, "telegram:42");
    }

    #[tokio::test]
    async fn test_process_inbound_uses_hook_system_prompt_in_run_model_hot_path() {
        let fw = EliFramework::new();
        fw.register_plugin(
            "model",
            Arc::new(SystemPromptStateModelPlugin {
                fragment: "from-hook".into(),
            }) as Arc<dyn EliHookSpec>,
        )
        .await;

        let msg = json!({"content": "hello", "session_id": "system-prompt"});
        let result = fw.process_inbound(msg).await.unwrap();
        assert_eq!(result.model_output, "from-hook");
    }

    #[tokio::test]
    async fn test_process_inbound_logs_utf8_safe_preview() {
        let fw = EliFramework::new();
        fw.register_plugin(
            "model",
            Arc::new(UnicodeBoundaryModelPlugin) as Arc<dyn EliHookSpec>,
        )
        .await;

        let msg = json!({"content": "hello", "session_id": "utf8-preview"});
        let result = fw.process_inbound(msg).await.unwrap();
        assert!(result.model_output.starts_with(&"a".repeat(118)));
    }

    #[tokio::test]
    async fn test_process_inbound_snapshots_hook_runtime() {
        let fw = Arc::new(EliFramework::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        fw.register_plugin(
            "model",
            Arc::new(BlockingModelPlugin {
                started: started.clone(),
                release: release.clone(),
            }) as Arc<dyn EliHookSpec>,
        )
        .await;

        let inbound = json!({"content": "ping", "session_id": "lock-test"});
        let framework = fw.clone();
        let turn = tokio::spawn(async move { framework.process_inbound(inbound).await });
        started.notified().await;

        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            fw.register_plugin("late", Arc::new(SessionPlugin) as Arc<dyn EliHookSpec>),
        )
        .await
        .expect("register_plugin should not wait for an in-flight turn");

        release.notify_waiters();
        let result = turn.await.unwrap().unwrap();
        assert_eq!(result.session_id, "lock-test");
        assert!(fw.plugin_status().await.contains_key("late"));
    }

    // -- state merge precedence ------------------------------------------------

    /// Plugin that provides a state key "shared" = "first" and "only_first" = "yes".
    struct FirstStatePlugin;

    #[async_trait]
    impl EliHookSpec for FirstStatePlugin {
        fn plugin_name(&self) -> &str {
            "first-state"
        }

        async fn load_state(
            &self,
            _message: &Envelope,
            _session_id: &str,
        ) -> Result<Option<State>, HookError> {
            let mut s = State::new();
            s.insert("shared".into(), Value::String("first".into()));
            s.insert("only_first".into(), Value::String("yes".into()));
            Ok(Some(s))
        }
    }

    /// Plugin that provides a state key "shared" = "second" and "only_second" = "yes".
    struct SecondStatePlugin;

    #[async_trait]
    impl EliHookSpec for SecondStatePlugin {
        fn plugin_name(&self) -> &str {
            "second-state"
        }

        async fn load_state(
            &self,
            _message: &Envelope,
            _session_id: &str,
        ) -> Result<Option<State>, HookError> {
            let mut s = State::new();
            s.insert("shared".into(), Value::String("second".into()));
            s.insert("only_second".into(), Value::String("yes".into()));
            Ok(Some(s))
        }
    }

    #[tokio::test]
    async fn test_build_state_last_registered_wins_for_duplicate_keys() {
        let fw = EliFramework::new();
        // FirstStatePlugin registered first, SecondStatePlugin registered second.
        fw.register_plugin("first", Arc::new(FirstStatePlugin) as Arc<dyn EliHookSpec>)
            .await;
        fw.register_plugin(
            "second",
            Arc::new(SecondStatePlugin) as Arc<dyn EliHookSpec>,
        )
        .await;

        let rt = fw.hook_runtime_snapshot();
        let msg = json!({"content": "hello"});
        let state = fw
            .build_state(&rt, &msg, "test-session", std::path::Path::new("/tmp"))
            .await;

        // Last-registered plugin wins for the duplicate "shared" key.
        assert_eq!(state["shared"], Value::String("second".into()));
        // Non-overlapping keys are preserved from both plugins.
        assert_eq!(state["only_first"], Value::String("yes".into()));
        assert_eq!(state["only_second"], Value::String("yes".into()));
    }

    #[test]
    fn test_auto_evolution_context_uses_runtime_paths() {
        let mut state = State::new();
        let tmp = tempfile::tempdir().unwrap();
        state.insert(
            RUNTIME_WORKSPACE_KEY.to_owned(),
            Value::String(tmp.path().display().to_string()),
        );
        state.insert(
            RUNTIME_TAPES_DIR_KEY.to_owned(),
            Value::String("/tmp/tapes".into()),
        );
        let ctx = auto_evolution_context("cli:test", &state).unwrap();
        assert_eq!(ctx.workspace, tmp.path());
        assert_eq!(ctx.tapes_dir, std::path::PathBuf::from("/tmp/tapes"));
        assert_eq!(
            ctx.tape_name,
            crate::builtin::tape::TapeService::session_tape_name("cli:test", tmp.path(),)
        );
        assert_eq!(
            ctx.policy.min_score,
            AutoEvolutionPolicy::default().min_score
        );
    }

    #[test]
    fn test_auto_evolution_context_honors_kill_switch() {
        unsafe { std::env::set_var("ELI_EVOLUTION_DISABLED", "1") };
        let ctx = auto_evolution_context("cli:test", &State::new());
        unsafe { std::env::remove_var("ELI_EVOLUTION_DISABLED") };
        assert!(ctx.is_none());
    }

    #[test]
    fn test_auto_evolution_context_honors_runtime_policy_disable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".agents/evolution/runtime-policies");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auto-evolution.json"),
            "{\n  \"auto_evolution\": {\"enabled\": false}\n}",
        )
        .unwrap();
        let mut state = State::new();
        state.insert(
            RUNTIME_WORKSPACE_KEY.to_owned(),
            Value::String(tmp.path().display().to_string()),
        );
        state.insert(
            RUNTIME_TAPES_DIR_KEY.to_owned(),
            Value::String("/tmp/tapes".into()),
        );
        assert!(auto_evolution_context("cli:test", &state).is_none());
    }

    #[test]
    fn test_auto_evolution_context_applies_runtime_policy_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".agents/evolution/runtime-policies");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auto-evolution.json"),
            "{\n  \"auto_evolution\": {\"min_score\": 91, \"canary_observations\": 4}\n}",
        )
        .unwrap();
        let mut state = State::new();
        state.insert(
            RUNTIME_WORKSPACE_KEY.to_owned(),
            Value::String(tmp.path().display().to_string()),
        );
        state.insert(
            RUNTIME_TAPES_DIR_KEY.to_owned(),
            Value::String("/tmp/tapes".into()),
        );
        let ctx = auto_evolution_context("cli:test", &state).unwrap();
        assert_eq!(ctx.policy.min_score, 91);
        assert_eq!(ctx.policy.canary_observations, 4);
    }

    // -- hook_report ----------------------------------------------------------

    #[tokio::test]
    async fn test_hook_report_reflects_registered_plugins() {
        let fw = EliFramework::new();
        fw.register_plugin("session", Arc::new(SessionPlugin) as Arc<dyn EliHookSpec>)
            .await;
        let report = fw.hook_report().await;
        assert!(report.contains_key("resolve_session"));
        assert!(report["resolve_session"].contains(&"session-plugin".to_owned()));
    }
}
