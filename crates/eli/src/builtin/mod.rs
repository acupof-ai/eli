//! Builtin module — default hook implementations and runtime components.

pub mod agent;
pub mod cli;
pub(crate) mod coding_plan;
pub mod command_semantics;
pub mod config;
mod model_specs;
pub mod settings;
pub mod shell_manager;
pub mod store;
pub mod subagent;
pub mod tape;
#[cfg(feature = "tape-viewer")]
pub mod tape_viewer;
pub mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use nexil::ConduitError;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::builtin::agent::Agent;
use crate::builtin::store::{FileTapeStore, ForkTapeStore};
use crate::builtin::tape::TapeService;
use crate::channels::base::Channel;
use crate::channels::message::{ChannelMessage, MessageKind};
use crate::hooks::EliHookSpec;
use crate::smart_router::SmartRouter;
use crate::tool_middleware::MiddlewareChain;
use crate::types::{Envelope, PromptValue, RUNTIME_TAPES_DIR_KEY, RUNTIME_WORKSPACE_KEY, State};

pub(crate) const CLEANUP_ONLY_CONTEXT_KEY: &str = "_eli_cleanup_only";

/// Default session TTL: 30 minutes.
const DEFAULT_SESSION_TTL_SECS: u64 = 30 * 60;

/// Interval between session cleanup sweeps.
const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Cached system-prompt entry, gated on SOUL.md / rules.bundle.md mtime so
/// intentional edits invalidate but spurious per-turn drift (timestamps,
/// HashSet iter order, transient FS read failures) doesn't.
#[derive(Clone)]
struct SysPromptCacheEntry {
    prompt: Arc<str>,
    soul_mtime_ns: Option<i128>,
    rules_bundle_mtime_ns: Option<i128>,
}

/// Best-effort mtime in nanoseconds since UNIX_EPOCH; `None` if the file
/// is absent or stat fails.
fn mtime_ns(path: &std::path::Path) -> Option<i128> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
}

/// Default hook implementations for basic runtime operations.
pub struct BuiltinImpl {
    agents: parking_lot::RwLock<HashMap<String, Arc<Mutex<Agent>>>>,
    last_active: parking_lot::RwLock<HashMap<String, Instant>>,
    home: PathBuf,
    router: SmartRouter,
    middleware_chain: MiddlewareChain,
    tape_service: std::sync::OnceLock<TapeService>,
    /// Channels for outbound dispatch (populated by gateway; empty in CLI mode).
    channels: parking_lot::RwLock<HashMap<String, Arc<dyn Channel>>>,
    session_ttl: Duration,
    /// M_e.10 — Per-session cache of the full system prompt. Pinned at first
    /// turn; mtime gate on SOUL.md + rules.bundle.md handles intentional
    /// edits. Evicted alongside agents/last_active in `sweep_stale_sessions`.
    sys_prompt_cache: parking_lot::RwLock<HashMap<String, SysPromptCacheEntry>>,
}

#[allow(clippy::new_without_default)]
impl BuiltinImpl {
    /// Create a new `BuiltinImpl`, registering builtin tools.
    pub fn new() -> Self {
        tools::register_builtin_tools();
        crate::tools::populate_model_tools_cache();
        let home = settings::AgentSettings::from_env().home;
        crate::taskboard::init_task_store(&home);
        let session_ttl = Duration::from_secs(
            std::env::var("ELI_SESSION_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_SESSION_TTL_SECS),
        );
        Self {
            agents: parking_lot::RwLock::new(HashMap::new()),
            last_active: parking_lot::RwLock::new(HashMap::new()),
            home,
            router: SmartRouter::new(),
            middleware_chain: MiddlewareChain::with_defaults(),
            tape_service: std::sync::OnceLock::new(),
            channels: parking_lot::RwLock::new(HashMap::new()),
            session_ttl,
            sys_prompt_cache: parking_lot::RwLock::new(HashMap::new()),
        }
    }

    /// Get or create a per-session Agent, enabling concurrent model execution across sessions.
    fn get_or_create_agent(&self, session_id: &str) -> Arc<Mutex<Agent>> {
        // Update last-active timestamp.
        {
            let mut active = self.last_active.write();
            active.insert(session_id.to_owned(), Instant::now());
        }

        {
            let agents = self.agents.read();
            if let Some(agent) = agents.get(session_id) {
                return Arc::clone(agent);
            }
        }
        let mut agents = self.agents.write();
        agents
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(Agent::new())))
            .clone()
    }

    /// Start the background session cleanup task.
    ///
    /// Evicts sessions idle longer than `session_ttl`. Skips sessions whose
    /// Agent mutex is currently locked (active turn in progress).
    pub fn start_cleanup_task(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SESSION_CLEANUP_INTERVAL).await;
                this.sweep_stale_sessions();
            }
        });
    }

    /// Sweep stale sessions, evicting those idle past the TTL.
    fn sweep_stale_sessions(&self) {
        let now = Instant::now();
        let stale_keys: Vec<String> = {
            let active = self.last_active.read();
            active
                .iter()
                .filter(|(_, ts)| now.duration_since(**ts) > self.session_ttl)
                .map(|(k, _)| k.clone())
                .collect()
        };

        if stale_keys.is_empty() {
            return;
        }

        // Check which stale sessions are not currently locked (active).
        let agents = self.agents.read();
        let evictable: Vec<String> = stale_keys
            .into_iter()
            .filter(|key| {
                agents
                    .get(key)
                    .map(|a| a.try_lock().is_ok())
                    .unwrap_or(true)
            })
            .collect();
        drop(agents);

        if evictable.is_empty() {
            return;
        }

        let mut agents = self.agents.write();
        let mut active = self.last_active.write();
        let mut sys_cache = self.sys_prompt_cache.write();

        for key in &evictable {
            agents.remove(key);
            active.remove(key);
            sys_cache.remove(key);
        }

        info!(
            evicted = evictable.len(),
            "session cleanup: evicted idle sessions"
        );
        debug!(sessions = ?evictable, "evicted session IDs");
    }

    /// Resolve a session ID from a channel message.
    pub fn resolve_session(&self, message: &ChannelMessage) -> String {
        if !message.session_id.trim().is_empty() {
            return message.session_id.clone();
        }
        let channel = &message.channel;
        let chat_id = &message.chat_id;
        format!("{channel}:{chat_id}")
    }

    /// Load initial state for a session.
    pub fn load_state(&self, session_id: &str) -> HashMap<String, Value> {
        let mut state: HashMap<String, Value> = HashMap::new();
        state.insert(
            "session_id".to_owned(),
            Value::String(session_id.to_owned()),
        );
        insert_runtime_paths(&mut state, &self.home);
        state
    }

    /// Build a prompt from an inbound message.
    pub fn build_prompt(&self, message: &ChannelMessage) -> PromptValue {
        let content = extract_message_text(&message.content);
        if content.starts_with('/') {
            return PromptValue::Text(content);
        }
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let context_str = message.context_str();
        let text = if context_str.is_empty() {
            content
        } else {
            format!("{context_str}\n---Date: {now}---\n{content}")
        };
        PromptValue::Text(text)
    }

    /// Run the model on a prompt within a session.
    pub async fn run_model(
        &self,
        prompt: PromptValue,
        session_id: &str,
        state: &HashMap<String, Value>,
    ) -> Result<String, ConduitError> {
        let agent = self.get_or_create_agent(session_id);
        agent
            .lock()
            .await
            .run(session_id, prompt, state, None, None, None)
            .await
    }

    /// Populate channels for gateway-mode outbound dispatch.
    pub fn set_channels(&self, channels: HashMap<String, Arc<dyn Channel>>) {
        let mut chs = self.channels.write();
        *chs = channels;
    }

    /// Get or create the shared TapeService for hook-level writes.
    fn tape_service(&self) -> &TapeService {
        self.tape_service.get_or_init(|| {
            let tapes_dir = self.home.join("tapes");
            let file_store = FileTapeStore::new(tapes_dir.clone());
            let fork_store = ForkTapeStore::from_sync(file_store);
            TapeService::new(tapes_dir, fork_store)
        })
    }

    /// Provide the tape store (FileTapeStore backed by the agent's home directory).
    pub fn provide_tape_store(&self) -> FileTapeStore {
        FileTapeStore::new(self.home.join("tapes"))
    }

    /// Handle errors by logging them.
    pub async fn on_error(&self, stage: &str, error: &str, message: Option<&ChannelMessage>) {
        tracing::error!(stage = stage, error = error, "pipeline error");
        if let Some(msg) = message {
            tracing::error!(
                session_id = %msg.session_id,
                channel = %msg.channel,
                "error occurred in session"
            );
        }
    }

    /// Render outbound messages from model output.
    pub fn render_outbound(
        &self,
        message: &ChannelMessage,
        session_id: &str,
        model_output: &str,
    ) -> Vec<ChannelMessage> {
        let output_channel = effective_output_channel(message);
        let clean = crate::builtin::cli::strip_fake_tool_calls(model_output);
        let (content, mut context) = render_content_and_context(&clean, message, session_id);

        // Drain any media accumulated during the turn and attach to context.
        let media = crate::control_plane::drain_outbound_media();
        if !media.is_empty() {
            let media_json: Vec<Value> = media
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "path": m.path,
                        "media_type": m.media_type,
                        "mime_type": m.mime_type,
                    })
                })
                .collect();
            context.insert("outbound_media".to_owned(), Value::Array(media_json));
        }

        let outbound = ChannelMessage::new(session_id, &message.channel, content)
            .with_chat_id(&message.chat_id)
            .with_output_channel(output_channel)
            .with_kind(message.kind)
            .with_context(context)
            .finalize();
        vec![outbound]
    }
}

fn insert_runtime_paths(state: &mut State, home: &std::path::Path) {
    state.insert(
        RUNTIME_WORKSPACE_KEY.to_owned(),
        Value::String(display_path(current_workspace())),
    );
    state.insert(
        RUNTIME_TAPES_DIR_KEY.to_owned(),
        Value::String(display_path(home.join("tapes"))),
    );
}

fn current_workspace() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

fn display_path(path: PathBuf) -> String {
    path.display().to_string()
}

fn effective_output_channel(message: &ChannelMessage) -> &str {
    if message.output_channel.is_empty() {
        message.channel.as_str()
    } else {
        message.output_channel.as_str()
    }
}

fn render_content_and_context(
    clean: &str,
    message: &ChannelMessage,
    session_id: &str,
) -> (String, serde_json::Map<String, Value>) {
    if clean.trim().is_empty() {
        tracing::info!(
            target: "eli_trace",
            session_id = %session_id,
            "builtin.render_outbound.empty_after_cleanup"
        );
        let mut extra = message.context.clone();
        extra.insert(CLEANUP_ONLY_CONTEXT_KEY.to_owned(), Value::Bool(true));
        (String::new(), extra)
    } else {
        (clean.to_owned(), message.context.clone())
    }
}

fn extract_message_text(content: &str) -> String {
    match serde_json::from_str::<Value>(content) {
        Ok(val) => val
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or(content)
            .to_owned(),
        Err(_) => content.to_owned(),
    }
}

fn envelope_str<'a>(message: &'a Envelope, key: &str, default: &'a str) -> &'a str {
    message.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

fn envelope_content(message: &Envelope) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn parse_message_kind(message: &Envelope) -> MessageKind {
    match envelope_str(message, "kind", "normal") {
        "error" => MessageKind::Error,
        "command" => MessageKind::Command,
        _ => MessageKind::Normal,
    }
}

fn envelope_to_channel_message(message: &Envelope) -> ChannelMessage {
    let channel = envelope_str(message, "channel", "cli");
    let mut context = message
        .get("context")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    if !context.contains_key("outbound_media")
        && let Some(media) = message.get("outbound_media")
    {
        context.insert("outbound_media".to_owned(), media.clone());
    }

    ChannelMessage {
        session_id: envelope_str(message, "session_id", "").to_owned(),
        channel: channel.to_owned(),
        content: envelope_content(message),
        chat_id: envelope_str(message, "chat_id", "default").to_owned(),
        is_active: message
            .get("is_active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        kind: parse_message_kind(message),
        context,
        media: Vec::new(),
        output_channel: envelope_str(message, "output_channel", channel).to_owned(),
    }
}

#[async_trait]
impl EliHookSpec for BuiltinImpl {
    fn plugin_name(&self) -> &str {
        "builtin"
    }

    fn classify_inbound(&self, message: &Envelope) -> Option<String> {
        let content = message
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        self.router.classify(content)
    }

    async fn resolve_session(
        &self,
        message: &Envelope,
    ) -> Result<Option<String>, crate::hooks::HookError> {
        Ok(Some(
            self.resolve_session(&envelope_to_channel_message(message)),
        ))
    }

    async fn load_state(
        &self,
        message: &Envelope,
        session_id: &str,
    ) -> Result<Option<State>, crate::hooks::HookError> {
        let mut state = self.load_state(session_id);
        for field in ["sender_id", "chat_id", "channel", "output_channel"] {
            if let Some(value) = message.get(field).cloned() {
                state.insert(field.to_owned(), value);
            }
        }

        // Preserve the full inbound context so downstream tools (e.g. subagent)
        // can forward routing fields in injected messages.
        if let Some(ctx) = message.get("context").cloned() {
            state.insert("_inbound_context".to_owned(), ctx);
        }

        // Detect new session: tape has no entries yet.
        let workspace = std::env::current_dir().unwrap_or_default();
        let tape_name = TapeService::session_tape_name(session_id, &workspace);
        let has_entries = self.tape_service().tape_has_entries(&tape_name).await;
        if !has_entries {
            state.insert("_is_new_session".to_owned(), Value::Bool(true));
        }

        Ok(Some(state))
    }

    async fn build_user_prompt(
        &self,
        message: &Envelope,
        _session_id: &str,
        _state: &State,
    ) -> Option<PromptValue> {
        let text_prompt = self.build_prompt(&envelope_to_channel_message(message));

        // If the envelope carries resolved image content blocks, return a
        // multimodal Parts prompt so the LLM receives them as vision input.
        if let Some(parts) = message.get("media_parts").and_then(|v| v.as_array())
            && !parts.is_empty()
        {
            let mut content = Vec::new();
            let text = text_prompt.as_text();
            if !text.trim().is_empty() {
                content.push(serde_json::json!({"type": "text", "text": text}));
            }
            content.extend(parts.iter().cloned());
            return Some(PromptValue::Parts(content));
        }

        Some(text_prompt)
    }

    async fn run_model(
        &self,
        prompt: &PromptValue,
        session_id: &str,
        state: &State,
    ) -> Result<Option<String>, crate::hooks::HookError> {
        match self.run_model(prompt.clone(), session_id, state).await {
            Ok(output) => Ok(Some(output)),
            Err(e) => {
                tracing::error!(error = %e, session_id = %session_id, "run_model failed");
                Err(crate::hooks::HookError::Plugin {
                    plugin: self.plugin_name().to_owned(),
                    hook_point: crate::hooks::HookPoint::RunModel,
                    source: e.into(),
                })
            }
        }
    }

    async fn render_outbound(
        &self,
        message: &Envelope,
        session_id: &str,
        _state: &State,
        model_output: &str,
    ) -> Option<Vec<Envelope>> {
        let outbounds = self.render_outbound(
            &envelope_to_channel_message(message),
            session_id,
            model_output,
        );
        Some(
            outbounds
                .into_iter()
                .filter_map(|message| serde_json::to_value(message).ok())
                .collect(),
        )
    }

    async fn save_state(
        &self,
        session_id: &str,
        state: &State,
        _message: &Envelope,
        _model_output: &str,
    ) {
        let events = crate::control_plane::drain_save_events();
        if events.is_empty() {
            return;
        }

        let workspace = state
            .get(RUNTIME_WORKSPACE_KEY)
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let tape_name = TapeService::session_tape_name(session_id, &workspace);
        let tapes = self.tape_service();

        for (name, data) in events {
            if let Err(e) = tapes.append_event(&tape_name, &name, data).await {
                tracing::warn!(error = %e, event = %name, "save_state: failed to write event");
            }
        }
    }

    async fn dispatch_outbound(&self, message: &Envelope) -> Option<bool> {
        let out_ch = envelope_str(message, "output_channel", "").to_owned();
        let channel_field = envelope_str(message, "channel", "").to_owned();
        let content = envelope_content(message);
        let cleanup_only = message
            .get("context")
            .and_then(|v| v.as_object())
            .and_then(|ctx| ctx.get(CLEANUP_ONLY_CONTEXT_KEY))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Resolve channel under the lock, then drop it before any await.
        let resolved = {
            let channels = self.channels.read();
            if channels.is_empty() {
                None // CLI mode
            } else {
                Some(
                    channels
                        .get(out_ch.as_str())
                        .or_else(|| channels.get(channel_field.as_str()))
                        .cloned(),
                )
            }
        };

        let Some(maybe_channel) = resolved else {
            // CLI mode: print to stdout.
            if !content.trim().is_empty() {
                println!("{content}");
            }
            // Show media paths in CLI.
            if let Some(media) = message
                .get("context")
                .and_then(|v| v.get("outbound_media"))
                .and_then(|v| v.as_array())
            {
                for item in media {
                    if let Some(path) = item.get("path").and_then(|v| v.as_str()) {
                        println!("[media] {path}");
                    }
                }
            }
            return Some(true);
        };

        // Gateway mode: route to channel.
        let has_media = message
            .get("context")
            .and_then(|v| v.get("outbound_media"))
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        let session_id = envelope_str(message, "session_id", "").to_owned();
        if content.trim().is_empty() && !cleanup_only && !has_media {
            tracing::warn!(
                session_id = %session_id,
                channel = %channel_field,
                output_channel = %out_ch,
                "dispatch_outbound skipped empty message"
            );
            return Some(false);
        }

        let chat_id = envelope_str(message, "chat_id", "").to_owned();
        if chat_id.is_empty() {
            tracing::warn!(
                session_id = %session_id,
                channel = %channel_field,
                "dispatch_outbound skipped missing chat_id"
            );
            return Some(false);
        }

        let target_ch = if !out_ch.is_empty() {
            &out_ch
        } else {
            &channel_field
        };
        let Some(ch) = maybe_channel else {
            tracing::warn!(
                session_id = %session_id,
                channel = %target_ch,
                "dispatch_outbound skipped missing channel"
            );
            return Some(false);
        };

        let reply_context = message
            .get("context")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let reply = ChannelMessage::new(&session_id, target_ch, &content)
            .with_chat_id(&chat_id)
            .with_context(reply_context)
            .finalize();
        if let Err(e) = ch.send(reply).await {
            tracing::error!(error = %e, channel = %target_ch, "dispatch_outbound: failed to send");
            return Some(false);
        }
        Some(true)
    }

    async fn on_error(&self, stage: &str, error: &anyhow::Error, message: Option<&Envelope>) {
        let channel_message = message.map(envelope_to_channel_message);
        self.on_error(stage, &error.to_string(), channel_message.as_ref())
            .await;
    }

    fn build_system_prompt(&self, prompt_text: &str, state: &State) -> Option<String> {
        // M_e.10 prefix-cache fix: cache the full system prompt per session
        // so the inference server's prefix cache hits across turns. Without
        // this the prompt re-derives every turn and drifts on (a) Runtime
        // section timestamps, (b) HashSet iter order in skills discovery,
        // (c) eli's own auto-evolution writes to rules.bundle.md, and (d)
        // transient SOUL.md read failures from external tooling. Mtime
        // gates on SOUL.md + rules.bundle.md let intentional edits invalidate.
        let session_id = state.get("session_id").and_then(Value::as_str)?;

        let workspace = state
            .get(RUNTIME_WORKSPACE_KEY)
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let soul_path = workspace.join(".agents/SOUL.md");
        let rules_bundle_path = workspace.join(".agents/evolution/rules.bundle.md");
        let soul_mtime = mtime_ns(&soul_path);
        let rules_mtime = mtime_ns(&rules_bundle_path);

        // Hot path: cache hit on matching mtimes.
        if let Some(entry) = self.sys_prompt_cache.read().get(session_id).cloned()
            && entry.soul_mtime_ns == soul_mtime
            && entry.rules_bundle_mtime_ns == rules_mtime
        {
            return Some(entry.prompt.to_string());
        }

        // Cache miss or invalidated: rebuild.
        let built = Agent::new().system_prompt(prompt_text, state, None);

        // Defensive: if a SOUL.md path exists on disk but the build returned
        // the fallback default, this turn hit a transient read failure. Don't
        // pin — let the next turn recover with the real content.
        let soul_exists = soul_path.is_file() || self.home.join("SOUL.md").is_file();
        let is_fallback =
            built.starts_with("You are Eli, a helpful AI coding assistant. Put the answer");
        if soul_exists && is_fallback {
            tracing::warn!(
                session_id = %session_id,
                soul_path = %soul_path.display(),
                "M_e.10 build_system_prompt: SOUL.md exists but build returned fallback string; not pinning (transient read failure?)"
            );
            return Some(built);
        }

        let arc: Arc<str> = Arc::from(built.as_str());
        self.sys_prompt_cache.write().insert(
            session_id.to_owned(),
            SysPromptCacheEntry {
                prompt: arc.clone(),
                soul_mtime_ns: soul_mtime,
                rules_bundle_mtime_ns: rules_mtime,
            },
        );
        Some(arc.to_string())
    }

    fn wrap_tool(&self, tool: &nexil::Tool) -> nexil::ToolAction {
        let wrapped = self.middleware_chain.wrap_tools(std::slice::from_ref(tool));
        match wrapped.into_iter().next() {
            Some(t) => nexil::ToolAction::Replace(t),
            None => nexil::ToolAction::Remove,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_outbound_empty_emits_cleanup_only_message() {
        let builtin = BuiltinImpl::new();
        let mut extra = serde_json::Map::new();
        extra.insert(
            "source_channel".to_owned(),
            Value::String("feishu".to_owned()),
        );
        let message = ChannelMessage::new("feishu:default:user_1", "webhook", "hello")
            .with_chat_id("user_1")
            .with_context(extra)
            .finalize();

        let outbounds = builtin.render_outbound(&message, "feishu:default:user_1", "");

        assert_eq!(outbounds.len(), 1);
        assert!(outbounds[0].content.is_empty());
        assert_eq!(
            outbounds[0]
                .context
                .get(CLEANUP_ONLY_CONTEXT_KEY)
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            outbounds[0]
                .context
                .get("source_channel")
                .and_then(|v| v.as_str()),
            Some("feishu")
        );
    }

    #[test]
    fn test_load_state_includes_runtime_tapes_dir() {
        let builtin = BuiltinImpl::new();
        let state = builtin.load_state("session:1");
        assert!(state.contains_key(RUNTIME_WORKSPACE_KEY));
        assert_eq!(
            state
                .get(RUNTIME_TAPES_DIR_KEY)
                .and_then(Value::as_str)
                .map(|value| value.ends_with("/tapes")),
            Some(true)
        );
    }

    #[test]
    fn test_get_or_create_agent_returns_same_instance_for_same_session() {
        let builtin = BuiltinImpl::new();
        let a1 = builtin.get_or_create_agent("session:1");
        let a2 = builtin.get_or_create_agent("session:1");
        assert!(Arc::ptr_eq(&a1, &a2), "same session must return same Arc");
    }

    #[test]
    fn test_get_or_create_agent_returns_different_instances_for_different_sessions() {
        let builtin = BuiltinImpl::new();
        let a1 = builtin.get_or_create_agent("session:1");
        let a2 = builtin.get_or_create_agent("session:2");
        assert!(
            !Arc::ptr_eq(&a1, &a2),
            "different sessions must return different Arcs"
        );
    }

    #[tokio::test]
    async fn test_concurrent_sessions_do_not_block_each_other() {
        let builtin = Arc::new(BuiltinImpl::new());

        // Lock session:1's agent
        let agent1 = builtin.get_or_create_agent("session:1");
        let guard = agent1.lock().await;

        // session:2 should still be lockable (not blocked by session:1)
        let agent2 = builtin.get_or_create_agent("session:2");
        let try_lock = agent2.try_lock();
        assert!(
            try_lock.is_ok(),
            "session:2 must not be blocked by session:1"
        );

        drop(guard);
    }

    #[tokio::test]
    async fn test_same_session_serializes() {
        let builtin = BuiltinImpl::new();

        let agent = builtin.get_or_create_agent("session:1");
        let _guard = agent.lock().await;

        // Same session should be locked
        let agent_again = builtin.get_or_create_agent("session:1");
        let try_lock = agent_again.try_lock();
        assert!(
            try_lock.is_err(),
            "same session must serialize (lock should be held)"
        );
    }

    #[test]
    fn test_get_or_create_agent_concurrent_creation() {
        use std::thread;

        let builtin = Arc::new(BuiltinImpl::new());
        let mut handles = vec![];

        // Spawn 10 threads all requesting the same session simultaneously
        for _ in 0..10 {
            let b = Arc::clone(&builtin);
            handles.push(thread::spawn(move || b.get_or_create_agent("shared")));
        }

        let agents: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All must point to the same Arc
        for agent in &agents[1..] {
            assert!(
                Arc::ptr_eq(&agents[0], agent),
                "concurrent creation must converge to single instance"
            );
        }
    }

    #[test]
    fn test_render_outbound_normal_propagates_inbound_context() {
        let builtin = BuiltinImpl::new();
        let mut extra = serde_json::Map::new();
        extra.insert(
            "source_channel".to_owned(),
            Value::String("feishu".to_owned()),
        );
        extra.insert("account_id".to_owned(), Value::String("default".to_owned()));
        extra.insert(
            "channel_target".to_owned(),
            Value::String("user:ou_abc".to_owned()),
        );
        let message = ChannelMessage::new("feishu:default:ou_abc", "webhook", "hello")
            .with_chat_id("ou_abc")
            .with_context(extra)
            .finalize();

        let outbounds = builtin.render_outbound(&message, "feishu:default:ou_abc", "reply text");

        assert_eq!(outbounds.len(), 1);
        assert_eq!(outbounds[0].content, "reply text");
        // Inbound context must be propagated so the channel can route
        // the outbound correctly and clean up typing indicators.
        assert_eq!(
            outbounds[0]
                .context
                .get("source_channel")
                .and_then(|v| v.as_str()),
            Some("feishu"),
            "normal outbound must carry source_channel from inbound context"
        );
        assert_eq!(
            outbounds[0]
                .context
                .get("account_id")
                .and_then(|v| v.as_str()),
            Some("default"),
        );
        assert_eq!(
            outbounds[0]
                .context
                .get("channel_target")
                .and_then(|v| v.as_str()),
            Some("user:ou_abc"),
        );
    }

    #[tokio::test]
    async fn test_render_outbound_attaches_outbound_media_from_turn_context() {
        use crate::control_plane::{
            OutboundMedia, TurnContext, push_outbound_media, with_turn_context,
        };

        let ctx = TurnContext {
            cancellation: nexil::CancellationToken::new(),
            wrap_tools: None,
            usage: Default::default(),
            save_events: Default::default(),
            dispatch: None,
            outbound_media: Default::default(),
        };
        with_turn_context(ctx, async {
            // Simulate tool producing media during the turn.
            push_outbound_media(OutboundMedia {
                path: "/tmp/test_image.png".into(),
                media_type: "image".into(),
                mime_type: "image/png".into(),
            });

            let builtin = BuiltinImpl::new();
            let message = ChannelMessage::new("s1", "webhook", "hello")
                .with_chat_id("u1")
                .finalize();

            let outbounds = builtin.render_outbound(&message, "s1", "here is the image");

            assert_eq!(outbounds.len(), 1);
            assert_eq!(outbounds[0].content, "here is the image");

            // Must have outbound_media in context.
            let media = outbounds[0]
                .context
                .get("outbound_media")
                .and_then(|v| v.as_array())
                .expect("outbound_media must be present in context");
            assert_eq!(media.len(), 1);
            assert_eq!(media[0]["path"], "/tmp/test_image.png");
            assert_eq!(media[0]["media_type"], "image");
            assert_eq!(media[0]["mime_type"], "image/png");
        })
        .await;
    }

    #[tokio::test]
    async fn test_render_outbound_no_media_omits_outbound_media_key() {
        use crate::control_plane::{TurnContext, with_turn_context};

        let ctx = TurnContext {
            cancellation: nexil::CancellationToken::new(),
            wrap_tools: None,
            usage: Default::default(),
            save_events: Default::default(),
            dispatch: None,
            outbound_media: Default::default(),
        };
        with_turn_context(ctx, async {
            let builtin = BuiltinImpl::new();
            let message = ChannelMessage::new("s1", "webhook", "hello")
                .with_chat_id("u1")
                .finalize();

            let outbounds = builtin.render_outbound(&message, "s1", "text only reply");

            assert_eq!(outbounds.len(), 1);
            assert_eq!(outbounds[0].content, "text only reply");
            // No outbound_media key when no media was produced.
            assert!(outbounds[0].context.get("outbound_media").is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn test_build_user_prompt_with_media_parts() {
        let builtin = BuiltinImpl::new();
        let envelope = serde_json::json!({
            "session_id": "test",
            "channel": "telegram",
            "chat_id": "123",
            "content": "What is this image?",
            "context": {},
            "kind": "normal",
            "output_channel": "telegram",
            "media_parts": [
                {"type": "image_base64", "mime_type": "image/jpeg", "data": "AQID"}
            ]
        });

        let prompt = builtin
            .build_user_prompt(&envelope, "test", &HashMap::new())
            .await
            .unwrap();

        match prompt {
            PromptValue::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0]["type"], "text");
                assert!(
                    parts[0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("What is this image?")
                );
                assert_eq!(parts[1]["type"], "image_base64");
                assert_eq!(parts[1]["data"], "AQID");
            }
            PromptValue::Text(_) => panic!("expected Parts, got Text"),
        }
    }

    #[tokio::test]
    async fn test_build_user_prompt_without_media_returns_text() {
        let builtin = BuiltinImpl::new();
        let envelope = serde_json::json!({
            "session_id": "test",
            "channel": "telegram",
            "chat_id": "123",
            "content": "hello",
            "context": {},
            "kind": "normal",
            "output_channel": "telegram",
        });

        let prompt = builtin
            .build_user_prompt(&envelope, "test", &HashMap::new())
            .await
            .unwrap();

        match prompt {
            PromptValue::Text(t) => assert!(t.contains("hello")),
            PromptValue::Parts(_) => panic!("expected Text, got Parts"),
        }
    }

    #[tokio::test]
    async fn test_build_user_prompt_empty_media_parts_returns_text() {
        let builtin = BuiltinImpl::new();
        let envelope = serde_json::json!({
            "session_id": "test",
            "channel": "telegram",
            "chat_id": "123",
            "content": "no images",
            "context": {},
            "kind": "normal",
            "output_channel": "telegram",
            "media_parts": []
        });

        let prompt = builtin
            .build_user_prompt(&envelope, "test", &HashMap::new())
            .await
            .unwrap();

        match prompt {
            PromptValue::Text(t) => assert!(t.contains("no images")),
            PromptValue::Parts(_) => panic!("expected Text when media_parts is empty"),
        }
    }

    #[tokio::test]
    async fn test_build_user_prompt_multiple_images() {
        let builtin = BuiltinImpl::new();
        let envelope = serde_json::json!({
            "session_id": "test",
            "channel": "telegram",
            "chat_id": "123",
            "content": "compare",
            "context": {},
            "kind": "normal",
            "output_channel": "telegram",
            "media_parts": [
                {"type": "image_base64", "mime_type": "image/png", "data": "A"},
                {"type": "image_base64", "mime_type": "image/jpeg", "data": "B"}
            ]
        });

        let prompt = builtin
            .build_user_prompt(&envelope, "test", &HashMap::new())
            .await
            .unwrap();

        match prompt {
            PromptValue::Parts(parts) => {
                assert_eq!(parts.len(), 3); // text + 2 images
                assert_eq!(parts[0]["type"], "text");
                assert_eq!(parts[1]["mime_type"], "image/png");
                assert_eq!(parts[2]["mime_type"], "image/jpeg");
            }
            PromptValue::Text(_) => panic!("expected Parts with multiple images"),
        }
    }

    #[tokio::test]
    async fn test_build_user_prompt_image_only_omits_empty_text_part() {
        let builtin = BuiltinImpl::new();
        let envelope = serde_json::json!({
            "session_id": "test",
            "channel": "telegram",
            "chat_id": "123",
            "content": "",
            "context": {},
            "kind": "normal",
            "output_channel": "telegram",
            "media_parts": [
                {"type": "image_base64", "mime_type": "image/png", "data": "X"}
            ]
        });

        let prompt = builtin
            .build_user_prompt(&envelope, "test", &HashMap::new())
            .await
            .unwrap();

        match prompt {
            PromptValue::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0]["type"], "image_base64");
            }
            PromptValue::Text(_) => panic!("expected Parts for image-only prompts"),
        }
    }

    #[tokio::test]
    async fn test_build_user_prompt_media_parts_null_returns_text() {
        let builtin = BuiltinImpl::new();
        let envelope = serde_json::json!({
            "session_id": "test",
            "channel": "telegram",
            "chat_id": "123",
            "content": "null media",
            "context": {},
            "kind": "normal",
            "output_channel": "telegram",
            "media_parts": null
        });

        let prompt = builtin
            .build_user_prompt(&envelope, "test", &HashMap::new())
            .await
            .unwrap();

        match prompt {
            PromptValue::Text(t) => assert!(t.contains("null media")),
            PromptValue::Parts(_) => panic!("expected Text when media_parts is null"),
        }
    }

    #[tokio::test]
    async fn test_build_user_prompt_parts_text_extraction() {
        let builtin = BuiltinImpl::new();
        let envelope = serde_json::json!({
            "session_id": "test",
            "channel": "telegram",
            "chat_id": "123",
            "content": "describe this",
            "context": {},
            "kind": "normal",
            "output_channel": "telegram",
            "media_parts": [
                {"type": "image_base64", "mime_type": "image/png", "data": "X"}
            ]
        });

        let prompt = builtin
            .build_user_prompt(&envelope, "test", &HashMap::new())
            .await
            .unwrap();

        // strict_text() should extract only the text part, ignoring image blocks.
        let text = prompt.strict_text();
        assert!(text.contains("describe this"));
        assert!(!text.contains("image_base64"));
        assert!(!text.contains("image/png"));
    }

    // ----- Session TTL & Cleanup -----

    fn builtin_with_ttl(ttl: Duration) -> BuiltinImpl {
        tools::register_builtin_tools();
        let home = settings::AgentSettings::from_env().home;
        BuiltinImpl {
            agents: parking_lot::RwLock::new(HashMap::new()),
            last_active: parking_lot::RwLock::new(HashMap::new()),
            home,
            router: SmartRouter::new(),
            middleware_chain: MiddlewareChain::with_defaults(),
            tape_service: std::sync::OnceLock::new(),
            channels: parking_lot::RwLock::new(HashMap::new()),
            session_ttl: ttl,
            sys_prompt_cache: parking_lot::RwLock::new(HashMap::new()),
        }
    }

    #[test]
    fn test_session_evicted_after_ttl() {
        let builtin = builtin_with_ttl(Duration::from_millis(0));
        builtin.get_or_create_agent("stale:1");
        // TTL is 0ms, so the session is immediately stale.
        std::thread::sleep(Duration::from_millis(1));
        builtin.sweep_stale_sessions();
        let agents = builtin.agents.read();
        assert!(agents.is_empty(), "stale session should have been evicted");
    }

    #[test]
    fn test_active_session_preserved() {
        let builtin = builtin_with_ttl(Duration::from_secs(3600));
        builtin.get_or_create_agent("active:1");
        builtin.sweep_stale_sessions();
        let agents = builtin.agents.read();
        assert_eq!(agents.len(), 1, "active session should be preserved");
    }

    #[test]
    fn test_evicted_session_re_bootstraps() {
        let builtin = builtin_with_ttl(Duration::from_millis(0));
        let a1 = builtin.get_or_create_agent("session:x");
        std::thread::sleep(Duration::from_millis(1));
        builtin.sweep_stale_sessions();
        let a2 = builtin.get_or_create_agent("session:x");
        assert!(
            !Arc::ptr_eq(&a1, &a2),
            "re-bootstrapped agent should be new"
        );
    }

    #[tokio::test]
    async fn test_locked_session_not_evicted() {
        let builtin = builtin_with_ttl(Duration::from_millis(0));
        let agent = builtin.get_or_create_agent("locked:1");
        // Hold the lock to simulate an active turn.
        let _guard = agent.lock().await;
        std::thread::sleep(Duration::from_millis(1));
        builtin.sweep_stale_sessions();
        let agents = builtin.agents.read();
        assert_eq!(
            agents.len(),
            1,
            "locked session should not be evicted during active turn"
        );
    }
}
