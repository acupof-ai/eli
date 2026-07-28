//! Channel manager — orchestrates all enabled channels, routes inbound and
//! outbound messages, and owns the lifecycle of in-flight processing tasks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::base::Channel;
use super::handler::BufferedMessageHandler;
use super::message::{ChannelMessage, MessageKind};
use crate::envelope::OutboundMessage;
use crate::types::Envelope;

// ---------------------------------------------------------------------------
// ChannelSettings
// ---------------------------------------------------------------------------

/// Runtime settings for the channel manager, typically loaded from environment
/// variables (prefix `ELI_`).
#[derive(Debug, Clone)]
pub struct ChannelSettings {
    /// Comma-separated list of enabled channels, or `"all"`.
    pub enabled_channels: String,
    /// Debounce seconds for buffered channels.
    pub debounce_seconds: f64,
    /// Maximum wait seconds before flushing follow-up messages.
    pub max_wait_seconds: f64,
    /// Time window (seconds) to consider a channel active.
    pub active_time_window: f64,
}

impl Default for ChannelSettings {
    fn default() -> Self {
        Self {
            enabled_channels: "all".to_owned(),
            debounce_seconds: 1.0,
            max_wait_seconds: 10.0,
            active_time_window: 60.0,
        }
    }
}

impl ChannelSettings {
    /// Load settings from environment variables with the `ELI_` prefix.
    pub fn from_env() -> Self {
        Self {
            enabled_channels: std::env::var("ELI_ENABLED_CHANNELS")
                .unwrap_or_else(|_| "all".to_owned()),
            debounce_seconds: std::env::var("ELI_DEBOUNCE_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0),
            max_wait_seconds: std::env::var("ELI_MAX_WAIT_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10.0),
            active_time_window: std::env::var("ELI_ACTIVE_TIME_WINDOW")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60.0),
        }
    }

    fn enabled_set(&self) -> Vec<String> {
        self.enabled_channels
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// InboundProcessor  (trait object that the framework supplies)
// ---------------------------------------------------------------------------

/// Trait for the framework's inbound processing pipeline.
///
/// The channel manager calls `process_inbound` for every message that has been
/// through debounce / batching.
#[async_trait::async_trait]
pub trait InboundProcessor: Send + Sync {
    async fn process_inbound(&self, message: ChannelMessage) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// OutboundRouter  (the manager itself acts as one)
// ---------------------------------------------------------------------------

/// Trait mirroring the Python `OutboundChannelRouter` protocol.
#[async_trait::async_trait]
pub trait OutboundRouter: Send + Sync {
    /// Dispatch an outbound envelope to the appropriate channel.
    /// Returns `true` if a channel handled the message.
    async fn dispatch(&self, message: &Envelope) -> bool;

    /// Cancel all tasks for a given session.
    async fn quit(&self, session_id: &str);
}

fn parse_message_kind(s: &str) -> MessageKind {
    match s {
        "error" => MessageKind::Error,
        "command" => MessageKind::Command,
        "join" => MessageKind::Join,
        _ => MessageKind::Normal,
    }
}

// ---------------------------------------------------------------------------
// ChannelManager
// ---------------------------------------------------------------------------

/// Owns every registered [`Channel`], routes inbound messages through optional
/// debounce buffering, and dispatches outbound envelopes to the correct
/// channel.
pub struct ChannelManager {
    channels: HashMap<String, Arc<dyn Channel>>,
    settings: ChannelSettings,
    enabled_list: Vec<String>,

    tx: mpsc::Sender<ChannelMessage>,
    rx: Mutex<mpsc::Receiver<ChannelMessage>>,
    session_handlers: Mutex<HashMap<String, Arc<BufferedMessageHandler>>>,
    ongoing_tasks: Mutex<HashMap<String, HashSet<tokio::task::Id>>>,
    task_handles: Mutex<HashMap<tokio::task::Id, tokio::task::JoinHandle<()>>>,
}

impl ChannelManager {
    /// Create a new manager from a map of channels.
    pub fn new(
        channels: HashMap<String, Arc<dyn Channel>>,
        settings: ChannelSettings,
        enabled_channels: Option<Vec<String>>,
    ) -> Arc<Self> {
        let enabled_list = enabled_channels.unwrap_or_else(|| settings.enabled_set());
        const INBOUND_CHANNEL_CAPACITY: usize = 256;
        let (tx, rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);

        Arc::new(Self {
            channels,
            settings,
            enabled_list,
            tx,
            rx: Mutex::new(rx),
            session_handlers: Mutex::new(HashMap::new()),
            ongoing_tasks: Mutex::new(HashMap::new()),
            task_handles: Mutex::new(HashMap::new()),
        })
    }

    // ----- inbound ----------------------------------------------------------

    /// Called by channels when they receive a message from the user.
    pub async fn on_receive(&self, message: ChannelMessage) {
        let channel_name = message.channel.clone();
        let session_id = message.session_id.clone();

        if !self.channels.contains_key(&channel_name) {
            warn!(
                channel = %channel_name,
                "received message from unknown channel, ignoring"
            );
            return;
        }

        let needs_debounce = self
            .channels
            .get(&channel_name)
            .map(|c| c.needs_debounce())
            .unwrap_or(false);

        if needs_debounce {
            let handler = {
                let mut handlers = self.session_handlers.lock().await;
                handlers
                    .entry(session_id.clone())
                    .or_insert_with(|| {
                        Arc::new(BufferedMessageHandler::new(
                            self.tx.clone(),
                            self.settings.active_time_window,
                            self.settings.max_wait_seconds,
                            self.settings.debounce_seconds,
                        ))
                    })
                    .clone()
            };
            handler.handle(message).await;
        } else if let Err(e) = self.tx.try_send(message) {
            warn!(
                error = %e,
                "inbound channel full, dropping message"
            );
        }
    }

    /// Get a reference to a channel by name.
    pub fn get_channel(&self, name: &str) -> Option<&Arc<dyn Channel>> {
        self.channels.get(name)
    }

    // ----- outbound ---------------------------------------------------------

    /// Maximum dispatch retry attempts.
    const DISPATCH_MAX_RETRIES: u32 = 3;

    /// Dispatch an outbound envelope to the correct channel.
    ///
    /// Retries on send failure with exponential backoff. All `send()` errors
    /// are connection-level by nature — HTTP-level errors are handled inside
    /// channel implementations.
    pub async fn dispatch(&self, message: &Envelope) -> bool {
        let Some(outbound) = self.build_outbound(message) else {
            return false;
        };
        let channel_name = outbound.channel.clone();
        let Some(channel) = self.channels.get(&channel_name).map(Arc::clone) else {
            return false;
        };

        for attempt in 0..Self::DISPATCH_MAX_RETRIES {
            match channel.send(outbound.clone()).await {
                Ok(()) => return true,
                Err(e) => {
                    if attempt + 1 < Self::DISPATCH_MAX_RETRIES {
                        let backoff =
                            std::time::Duration::from_millis((200u64 << attempt.min(4)).min(3000));
                        warn!(
                            error = %e,
                            attempt = attempt + 1,
                            max = Self::DISPATCH_MAX_RETRIES,
                            backoff_ms = backoff.as_millis() as u64,
                            "dispatch failed, retrying"
                        );
                        tokio::time::sleep(backoff).await;
                    } else {
                        error!(
                            error = %e,
                            channel = %channel_name,
                            "dispatch failed after {} attempts",
                            Self::DISPATCH_MAX_RETRIES
                        );
                        return false;
                    }
                }
            }
        }
        false
    }

    fn build_outbound(&self, message: &Envelope) -> Option<ChannelMessage> {
        let validated = OutboundMessage::from_envelope(message, "", "");
        if validated.channel.is_empty() {
            return None;
        }

        let session_id = if validated.session_id.is_empty() {
            format!("{}:default", validated.channel)
        } else {
            validated.session_id
        };

        let kind = parse_message_kind(
            message
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("normal"),
        );

        Some(ChannelMessage {
            session_id,
            channel: validated.channel,
            chat_id: validated.chat_id,
            content: validated.content,
            is_active: false,
            kind,
            context: validated.context,
            media: Vec::new(),
            output_channel: String::new(),
        })
    }

    /// Cancel all in-flight tasks for a session.
    pub async fn quit(&self, session_id: &str) {
        let task_ids = {
            let mut ongoing = self.ongoing_tasks.lock().await;
            ongoing.remove(session_id).unwrap_or_default()
        };
        let cancelled = self.abort_tasks(task_ids).await;
        info!(session_id = %session_id, cancelled, "channel.manager quit session");
    }

    /// Abort a set of tasks by their IDs and return the count of cancelled tasks.
    async fn abort_tasks(&self, task_ids: HashSet<tokio::task::Id>) -> usize {
        let mut handles = self.task_handles.lock().await;
        let mut cancelled = 0usize;
        for id in &task_ids {
            if let Some(handle) = handles.remove(id) {
                handle.abort();
                let _ = handle.await;
                cancelled += 1;
            }
        }
        cancelled
    }

    // ----- enabled channels -------------------------------------------------

    /// Return the list of channels that should be started.
    pub fn enabled_channels(&self) -> Vec<Arc<dyn Channel>> {
        let is_all = self.enabled_list.iter().any(|s| s == "all");
        self.channels
            .iter()
            .filter(|(name, _)| {
                if is_all {
                    name.as_str() != "cli"
                } else {
                    self.enabled_list.contains(name)
                }
            })
            .map(|(_, ch)| Arc::clone(ch))
            .collect()
    }

    // ----- main loop --------------------------------------------------------

    /// Start all enabled channels and process the message queue until the
    /// cancellation token fires.
    pub async fn listen_and_run(
        self: &Arc<Self>,
        processor: Arc<dyn InboundProcessor>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let enabled = self.enabled_channels();
        for ch in &enabled {
            ch.start(cancel.clone()).await?;
        }
        info!("channel.manager started listening");

        let result = self.run_loop(processor, cancel.clone()).await;

        self.shutdown().await;
        info!("channel.manager stopped");
        result
    }

    async fn run_loop(
        self: &Arc<Self>,
        processor: Arc<dyn InboundProcessor>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut rx = self.rx.lock().await;
        loop {
            let message = tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => m,
                    None => break,
                },
                () = cancel.cancelled() => {
                    info!("channel.manager received shutdown signal");
                    break;
                }
            };

            self.spawn_and_track(message, Arc::clone(&processor)).await;
        }
        Ok(())
    }

    async fn spawn_and_track(
        self: &Arc<Self>,
        message: ChannelMessage,
        processor: Arc<dyn InboundProcessor>,
    ) {
        let session_id = message.session_id.clone();
        let mgr = Arc::clone(self);

        let handle = tokio::spawn(async move {
            if let Err(e) = processor.process_inbound(message).await {
                error!(error = %e, "channel.manager process_inbound error");
            }
        });

        let task_id = handle.id();
        self.register_task(&session_id, task_id, handle).await;
        Self::spawn_cleanup(mgr, task_id, session_id);
    }

    async fn register_task(
        &self,
        session_id: &str,
        task_id: tokio::task::Id,
        handle: tokio::task::JoinHandle<()>,
    ) {
        self.ongoing_tasks
            .lock()
            .await
            .entry(session_id.to_owned())
            .or_default()
            .insert(task_id);
        self.task_handles.lock().await.insert(task_id, handle);
    }

    fn spawn_cleanup(mgr: Arc<Self>, task_id: tokio::task::Id, session_id: String) {
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let handle = mgr.task_handles.lock().await.remove(&task_id);
            if let Some(h) = handle {
                let _ = h.await;
            }
            let mut ongoing = mgr.ongoing_tasks.lock().await;
            if let Some(set) = ongoing.get_mut(&session_id) {
                set.remove(&task_id);
                if set.is_empty() {
                    ongoing.remove(&session_id);
                }
            }
        });
    }

    // ----- shutdown ---------------------------------------------------------

    /// Cancel all in-flight tasks and stop every enabled channel.
    pub async fn shutdown(&self) {
        let count = self.abort_all_sessions().await;
        info!(
            cancelled = count,
            "channel.manager cancelled in-flight tasks"
        );

        for ch in self.enabled_channels() {
            if let Err(e) = ch.stop().await {
                error!(channel = %ch.name(), error = %e, "error stopping channel");
            }
        }
    }

    async fn abort_all_sessions(&self) -> usize {
        let all_task_ids: HashSet<tokio::task::Id> = {
            let mut ongoing = self.ongoing_tasks.lock().await;
            ongoing.drain().flat_map(|(_, ids)| ids).collect()
        };
        self.abort_tasks(all_task_ids).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A mock channel that fails a configurable number of times then succeeds.
    struct MockChannel {
        fail_count: AtomicU32,
        fails_remaining: AtomicU32,
    }

    impl MockChannel {
        fn new(fail_n_times: u32) -> Self {
            Self {
                fail_count: AtomicU32::new(0),
                fails_remaining: AtomicU32::new(fail_n_times),
            }
        }
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn name(&self) -> &str {
            "mock"
        }
        async fn start(&self, _cancel: CancellationToken) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, _message: ChannelMessage) -> anyhow::Result<()> {
            let remaining = self.fails_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fails_remaining.fetch_sub(1, Ordering::SeqCst);
                self.fail_count.fetch_add(1, Ordering::SeqCst);
                return Err(anyhow::anyhow!("connection refused"));
            }
            Ok(())
        }
    }

    fn test_envelope(channel: &str) -> Envelope {
        json!({
            "channel": channel,
            "chat_id": "test",
            "content": "hello",
        })
    }

    fn make_manager(channel: Arc<dyn Channel>) -> Arc<ChannelManager> {
        let mut channels = HashMap::new();
        channels.insert("mock".to_string(), channel);
        ChannelManager::new(channels, ChannelSettings::default(), None)
    }

    #[tokio::test]
    async fn test_dispatch_retry_succeeds_second_attempt() {
        let ch = Arc::new(MockChannel::new(1));
        let mgr = make_manager(ch.clone());
        let result = mgr.dispatch(&test_envelope("mock")).await;
        assert!(result, "dispatch should succeed after retry");
        assert_eq!(ch.fail_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_dispatch_retry_exhausted() {
        let ch = Arc::new(MockChannel::new(10)); // fails more than max retries
        let mgr = make_manager(ch.clone());
        let result = mgr.dispatch(&test_envelope("mock")).await;
        assert!(!result, "dispatch should fail after retries exhausted");
        assert_eq!(
            ch.fail_count.load(Ordering::SeqCst),
            ChannelManager::DISPATCH_MAX_RETRIES
        );
    }

    #[tokio::test]
    async fn test_dispatch_unknown_channel_returns_false() {
        let ch: Arc<dyn Channel> = Arc::new(MockChannel::new(0));
        let mgr = make_manager(ch);
        let result = mgr.dispatch(&test_envelope("nonexistent")).await;
        assert!(!result);
    }
}
