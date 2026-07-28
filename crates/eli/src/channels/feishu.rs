//! Native Feishu/Lark channel — shells out to the external `lark-cli` binary
//! (event consume for inbound, `im +messages-*` for outbound). Auth lives
//! entirely in lark-cli's login state; this channel holds no credentials.
//!
//! Ported from the deleted `sidecar/plugins/feishu-cli.ts` at commit
//! `ecfa7c3^`. Inbound = a supervised `lark-cli event consume` subprocess whose
//! NDJSON stdout is parsed, deduped, anti-loop-filtered, @-mention-gated,
//! debounced per chat, and history-enriched before dispatch. Outbound (Phase 4)
//! renders the final reply via quote-reply + fresh sends.

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::base::Channel;
use super::message::ChannelMessage;
use super::text::{
    MAX_CHUNK_BYTES, chunk_text, combine_lines, friendlyize_error, normalize_escaped_whitespace,
    strip_mentions,
};

const EVENT_KEY: &str = "im.message.receive_v1";
const BATCH_DEBOUNCE: Duration = Duration::from_millis(1500);
const INFLIGHT_TTL: Duration = Duration::from_secs(30 * 60);
const SEEN_CAP: usize = 2000;
const SEEN_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const HISTORY_WINDOW: usize = 5;
const HISTORY_TEXT_CAP: usize = 200;

// ---------------------------------------------------------------------------
// FeishuSettings
// ---------------------------------------------------------------------------

/// Feishu channel configuration. Auth is lark-cli's; the only knob is the
/// account label (defaults to "default").
#[derive(Debug, Clone)]
pub struct FeishuSettings {
    pub account_id: String,
}

impl FeishuSettings {
    pub fn from_env() -> Self {
        Self {
            account_id: std::env::var("ELI_FEISHU_ACCOUNT")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "default".to_owned()),
        }
    }

    /// True if lark-cli is available on PATH (gateway enables Feishu on this).
    pub fn detect() -> bool {
        std::process::Command::new("lark-cli")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

// ---------------------------------------------------------------------------
// lark-cli invocation
// ---------------------------------------------------------------------------

/// Result of a lark-cli call: parsed JSON on success, or an error string.
enum LarkResult {
    Ok(Value),
    Err(String),
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// Run `lark-cli <args>`, capturing stdout/stderr. Exit 0 → parse stdout as
/// JSON (or wrap raw string); non-zero → `Err(stderr or "lark-cli exit N")`.
async fn run_lark_cli(args: &[String]) -> LarkResult {
    let output = Command::new("lark-cli")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            match serde_json::from_str::<Value>(stdout.trim()) {
                Ok(v) => LarkResult::Ok(v),
                Err(_) => LarkResult::Ok(Value::String(stdout.trim().to_owned())),
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            let msg = if stderr.is_empty() {
                format!("lark-cli exit {}", out.status.code().unwrap_or(-1))
            } else {
                stderr
            };
            LarkResult::Err(msg)
        }
        Err(e) => LarkResult::Err(e.to_string()),
    }
}

/// Map a Feishu id to the lark-cli routing flag: `ou_` → user, else chat.
fn route_args(to: &str) -> [String; 2] {
    if to.starts_with("ou_") {
        ["--user-id".to_owned(), to.to_owned()]
    } else {
        ["--chat-id".to_owned(), to.to_owned()]
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct BotIdentity {
    name: Option<String>,
    open_id: Option<String>,
}

/// A single inbound message tracked through batching → inflight → reply.
#[derive(Clone)]
struct InboundItem {
    message_id: String,
    text: String,
}

/// A flushed batch awaiting its reply.
struct InflightBatch {
    items: Vec<InboundItem>,
    started_at: Instant,
}

/// A pending (not-yet-flushed) per-chat batch.
struct QueuedBatch {
    items: Vec<InboundItem>,
    /// Generation counter: each new message bumps it; the flush task only fires
    /// if its captured generation still matches (race-free reset-on-message).
    generation: u64,
}

/// Shared mutable channel state behind a single lock.
#[derive(Default)]
struct FeishuState {
    seen: VecDeque<(String, Instant)>,
    queued: HashMap<String, QueuedBatch>,
    inflight: HashMap<String, VecDeque<InflightBatch>>,
    /// message_id → Typing reaction_id, so outbound can clear the ack.
    reactions: HashMap<String, String>,
    bot: BotIdentity,
}

impl FeishuState {
    /// Dedup check + insert. Unknown/empty ids are never deduped. Evicts by
    /// TTL then FIFO when at capacity.
    fn already_seen(&mut self, event_id: &str, now: Instant) -> bool {
        if event_id.is_empty() {
            return false;
        }
        if self.seen.len() >= SEEN_CAP {
            while let Some((_, ts)) = self.seen.front() {
                if self.seen.len() < SEEN_CAP || now.duration_since(*ts) <= SEEN_TTL {
                    break;
                }
                self.seen.pop_front();
            }
            while self.seen.len() >= SEEN_CAP {
                if self.seen.pop_front().is_none() {
                    break;
                }
            }
        }
        if self.seen.iter().any(|(id, _)| id == event_id) {
            return true;
        }
        self.seen.push_back((event_id.to_owned(), now));
        false
    }

    /// Take the oldest inflight batch for a chat (FIFO), discarding heads older
    /// than the TTL first. Removes the chat key when its queue empties.
    fn take_inflight(&mut self, chat_id: &str, now: Instant) -> Option<InflightBatch> {
        let queue = self.inflight.get_mut(chat_id)?;
        while let Some(head) = queue.front() {
            if now.duration_since(head.started_at) > INFLIGHT_TTL {
                queue.pop_front();
            } else {
                break;
            }
        }
        let head = queue.pop_front();
        if queue.is_empty() {
            self.inflight.remove(chat_id);
        }
        head
    }

    /// Shift the head inflight batch without a TTL sweep (turn-end cleanup).
    fn shift_inflight(&mut self, chat_id: &str) -> Option<InflightBatch> {
        let queue = self.inflight.get_mut(chat_id)?;
        let head = queue.pop_front();
        if queue.is_empty() {
            self.inflight.remove(chat_id);
        }
        head
    }
}

// ---------------------------------------------------------------------------
// FeishuInner — shared across the channel handle and the consumer task
// ---------------------------------------------------------------------------

struct FeishuInner {
    on_receive_tx: mpsc::UnboundedSender<ChannelMessage>,
    account_id: String,
    state: Mutex<FeishuState>,
}

impl FeishuInner {
    /// Load bot identity via `lark-cli api GET /open-apis/bot/v3/info`.
    async fn load_bot_identity(&self) {
        let args = owned(&["api", "GET", "/open-apis/bot/v3/info", "--as", "bot"]);
        match run_lark_cli(&args).await {
            LarkResult::Ok(v) => {
                let bot = v.get("bot").or_else(|| v.pointer("/data/bot"));
                let name = bot
                    .and_then(|b| b.get("app_name"))
                    .and_then(|n| n.as_str())
                    .map(str::to_owned);
                let open_id = bot
                    .and_then(|b| b.get("open_id"))
                    .and_then(|n| n.as_str())
                    .map(str::to_owned);
                let mut st = self.state.lock();
                st.bot = BotIdentity { name, open_id };
                info!(name = ?st.bot.name, "feishu: bot identity loaded");
            }
            LarkResult::Err(e) => {
                warn!(error = %e, "feishu: bot identity load failed; @-gate uses heuristic");
            }
        }
    }

    fn group_mentions_bot(&self, raw_text: &str) -> bool {
        let name = { self.state.lock().bot.name.clone() };
        match name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(n) => raw_text.contains(&format!("@{n}")),
            None => strip_mentions(raw_text).len() != raw_text.len(),
        }
    }

    fn is_app_sender(sender_id: &str) -> bool {
        sender_id.starts_with("cli_") || sender_id.starts_with("app_")
    }

    fn is_self_sender(&self, sender_id: &str) -> bool {
        self.state
            .lock()
            .bot
            .open_id
            .as_deref()
            .is_some_and(|id| id == sender_id)
    }

    /// Parse one NDJSON event into routing fields + item, or `None` to drop.
    fn parse_event(&self, evt: &Value) -> Option<ParsedInbound> {
        let now = Instant::now();
        let event_id = evt.get("event_id").and_then(|v| v.as_str()).unwrap_or("");
        if self.state.lock().already_seen(event_id, now) {
            debug!(event_id, "feishu: duplicate event dropped");
            return None;
        }

        let chat_id = evt.get("chat_id").and_then(|v| v.as_str())?;
        let sender_id = json_id(evt.get("sender_id")?);
        if self.is_self_sender(&sender_id) || Self::is_app_sender(&sender_id) {
            return None;
        }

        let chat_type = if evt.get("chat_type").and_then(|v| v.as_str()) == Some("p2p") {
            "direct"
        } else {
            "group"
        };
        let message_type = evt
            .get("message_type")
            .or_else(|| evt.get("msg_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if message_type == "sticker" {
            return None;
        }
        let message_id = evt
            .get("message_id")
            .or_else(|| evt.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let raw_text = evt.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if chat_type == "group" && !self.group_mentions_bot(raw_text) {
            return None;
        }

        Some(ParsedInbound {
            chat_id: chat_id.to_owned(),
            chat_type,
            sender_id,
            item: InboundItem {
                message_id,
                text: strip_mentions(raw_text),
            },
        })
    }

    /// Enqueue an item into its chat's debounce batch, (re)arming a 1500ms
    /// flush timer (race-free reset-on-message via a generation counter).
    fn enqueue(self: &Arc<Self>, parsed: ParsedInbound) {
        let ParsedInbound {
            chat_id,
            chat_type,
            sender_id,
            item,
        } = parsed;
        let message_id = item.message_id.clone();
        let generation = {
            let mut st = self.state.lock();
            let batch = st.queued.entry(chat_id.clone()).or_insert_with(|| QueuedBatch {
                items: Vec::new(),
                generation: 0,
            });
            batch.items.push(item);
            batch.generation += 1;
            batch.generation
        };

        // Fire-and-forget Typing reaction (ack the inbound message).
        let acker = Arc::clone(self);
        tokio::spawn(async move {
            acker.ack_inbound(&message_id).await;
        });

        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(BATCH_DEBOUNCE).await;
            this.flush_batch(chat_id, chat_type, sender_id, generation).await;
        });
    }

    /// Create a Typing reaction on an inbound message; stash its reaction_id.
    async fn ack_inbound(&self, message_id: &str) {
        let params = json!({ "message_id": message_id }).to_string();
        let data = json!({ "reaction_type": { "emoji_type": "Typing" } }).to_string();
        let args = owned(&[
            "im", "reactions", "create", "--as", "bot", "--params", &params, "--data", &data,
        ]);
        if let LarkResult::Ok(v) = run_lark_cli(&args).await
            && let Some(rid) = v.pointer("/data/reaction_id").and_then(|r| r.as_str())
        {
            self.state.lock().reactions.insert(message_id.to_owned(), rid.to_owned());
        }
    }

    /// Delete the Typing reaction for a message, if one was recorded.
    async fn delete_reaction(&self, message_id: &str) {
        let reaction_id = { self.state.lock().reactions.remove(message_id) };
        let Some(reaction_id) = reaction_id else { return };
        let params = json!({ "message_id": message_id, "reaction_id": reaction_id }).to_string();
        let args = owned(&["im", "reactions", "delete", "--as", "bot", "--params", &params]);
        if let LarkResult::Err(e) = run_lark_cli(&args).await {
            debug!(error = %e, message_id, "feishu: reaction delete failed (may be gone)");
        }
    }

    /// Clear Typing reactions for every item in a batch.
    async fn clear_reactions(&self, items: &[InboundItem]) {
        for item in items {
            self.delete_reaction(&item.message_id).await;
        }
    }

    /// Flush a chat's batch if its generation still matches (no newer message
    /// arrived). Combine, record inflight, enrich, dispatch.
    async fn flush_batch(&self, chat_id: String, chat_type: &'static str, sender_id: String, generation: u64) {
        let items = {
            let mut st = self.state.lock();
            match st.queued.get(&chat_id) {
                Some(b) if b.generation == generation => {}
                _ => return,
            }
            let batch = st.queued.remove(&chat_id).unwrap();
            st.inflight
                .entry(chat_id.clone())
                .or_default()
                .push_back(InflightBatch {
                    items: batch.items.clone(),
                    started_at: Instant::now(),
                });
            batch.items
        };

        let combined_text = combine_batch_text(&items);
        let enriched = self.enrich_with_history(&chat_id, &items, &combined_text).await;
        self.dispatch(&chat_id, chat_type, &sender_id, &items, enriched);
    }

    /// Prepend a window of recent chat history. Degrades to the input text on
    /// any failure.
    async fn enrich_with_history(&self, chat_id: &str, items: &[InboundItem], text: &str) -> String {
        let current_ids: Vec<&str> = items.iter().map(|i| i.message_id.as_str()).collect();
        let page_size = HISTORY_WINDOW + current_ids.len().max(1);
        let args = owned(&[
            "im",
            "+chat-messages-list",
            "--as",
            "bot",
            "--chat-id",
            chat_id,
            "--page-size",
            &page_size.to_string(),
            "--sort",
            "desc",
        ]);
        let result = match run_lark_cli(&args).await {
            LarkResult::Ok(v) => v,
            LarkResult::Err(_) => return text.to_owned(),
        };
        let messages = result
            .pointer("/data/messages")
            .or_else(|| result.get("messages"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut prior: Vec<&Value> = messages
            .iter()
            .filter(|m| {
                m.get("message_id")
                    .and_then(|v| v.as_str())
                    .is_none_or(|id| !current_ids.contains(&id))
            })
            .take(HISTORY_WINDOW)
            .collect();
        if prior.is_empty() {
            return text.to_owned();
        }
        prior.reverse();
        let lines: Vec<String> = prior.iter().map(|m| format_history_line(m)).collect();
        format!(
            "[最近 {} 条历史]\n{}\n\n[当前消息]\n{}",
            prior.len(),
            lines.join("\n"),
            text
        )
    }

    /// Build the framework inbound envelope and enqueue it.
    fn dispatch(&self, chat_id: &str, chat_type: &str, sender_id: &str, items: &[InboundItem], text: String) {
        let reply_to_id = items.last().map(|i| i.message_id.clone()).unwrap_or_default();
        let mut context = serde_json::Map::new();
        context.insert("source_channel".into(), json!("feishu"));
        context.insert("account_id".into(), json!(self.account_id));
        context.insert("sender_id".into(), json!(sender_id));
        context.insert("chat_type".into(), json!(chat_type));
        context.insert("reply_to_id".into(), json!(reply_to_id));
        context.insert("channel_target".into(), json!(chat_id));

        let session_id = format!("feishu:{}:{}", self.account_id, chat_id);
        let message = ChannelMessage::new(session_id, "feishu", text)
            .with_chat_id(chat_id.to_owned())
            .with_is_active(true)
            .with_context(context)
            .finalize();
        let _ = self.on_receive_tx.send(message);
    }

    /// Parse + route one NDJSON line.
    fn handle_event_line(self: &Arc<Self>, line: &str) {
        let evt: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, snippet = %truncate(line, 200), "feishu: bad event JSON");
                return;
            }
        };
        if let Some(parsed) = self.parse_event(&evt) {
            self.enqueue(parsed);
        }
    }

    /// Outbound state machine: notice (mid-turn) / cleanup / final reply.
    /// `to` is the routing target (channel_target or chat_id); `reply_to_id`
    /// is the quote target; flags come from the reply context.
    async fn send_outbound(
        &self,
        to: &str,
        text: &str,
        reply_to_id: Option<&str>,
        is_notice: bool,
        is_cleanup: bool,
    ) {
        // Cleanup / empty final: drop the inflight head and clear its reactions.
        if is_cleanup || (text.trim().is_empty() && !is_notice) {
            let batch = { self.state.lock().shift_inflight(to) };
            if let Some(batch) = batch {
                self.clear_reactions(&batch.items).await;
            }
            return;
        }

        let (rendered, _) = normalize_escaped_whitespace(&friendlyize_error(text));

        // Notice: fresh plain-text message, does NOT consume inflight.
        if is_notice {
            let mut args = owned(&["im", "+messages-send", "--as", "bot"]);
            args.extend(route_args(to));
            args.push("--text".to_owned());
            args.push(rendered);
            if let LarkResult::Err(e) = run_lark_cli(&args).await {
                warn!(error = %e, "feishu: notice send failed");
            }
            return;
        }

        // Final reply: consume inflight, quote-reply chunk 0, fresh-send the rest.
        let inflight = { self.state.lock().take_inflight(to, Instant::now()) };
        let latest_message_id = inflight
            .as_ref()
            .and_then(|b| b.items.last())
            .map(|i| i.message_id.clone());
        let target = reply_to_id
            .map(str::to_owned)
            .or(latest_message_id);

        let chunks = chunk_text(&rendered, MAX_CHUNK_BYTES);
        // Chunk 0: quote-reply if we have a target, else fresh send.
        if let Some(first) = chunks.first() {
            let result = match &target {
                Some(mid) => {
                    let args = owned(&[
                        "im", "+messages-reply", "--as", "bot", "--message-id", mid, "--markdown",
                        first,
                    ]);
                    run_lark_cli(&args).await
                }
                None => {
                    let mut args = owned(&["im", "+messages-send", "--as", "bot"]);
                    args.extend(route_args(to));
                    args.push("--markdown".to_owned());
                    args.push(first.clone());
                    run_lark_cli(&args).await
                }
            };
            if let LarkResult::Err(e) = result {
                warn!(error = %e, "feishu: final reply chunk 0 failed");
            }
        }

        // Clear Typing reactions now that the bot has spoken.
        if let Some(batch) = &inflight {
            self.clear_reactions(&batch.items).await;
        }

        // Continuation chunks: fresh non-quoted sends, in order.
        for chunk in chunks.iter().skip(1) {
            let mut args = owned(&["im", "+messages-send", "--as", "bot"]);
            args.extend(route_args(to));
            args.push("--markdown".to_owned());
            args.push(chunk.clone());
            if let LarkResult::Err(e) = run_lark_cli(&args).await {
                warn!(error = %e, "feishu: continuation chunk failed");
            }
        }
    }

    /// Supervise the `lark-cli event consume` subprocess: spawn, read NDJSON,
    /// respawn with backoff on crash, exit on cancellation.
    async fn run_consumer(self: Arc<Self>, cancel: CancellationToken) {
        let mut attempt: u32 = 0;
        while !cancel.is_cancelled() {
            match self.spawn_consumer_once(&cancel).await {
                ConsumerExit::Cancelled => break,
                ConsumerExit::Ready => attempt = 0,
                ConsumerExit::Crashed => {}
            }
            if cancel.is_cancelled() {
                break;
            }
            attempt += 1;
            let delay = Duration::from_millis((3000u64 * 2u64.pow(attempt - 1)).min(60_000));
            if attempt <= 2 {
                warn!(attempt, "feishu: consumer exited; respawning");
            } else {
                debug!(attempt, "feishu: consumer still failing; backing off");
            }
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = cancel.cancelled() => break,
            }
        }
    }

    async fn spawn_consumer_once(self: &Arc<Self>, cancel: &CancellationToken) -> ConsumerExit {
        let mut child = match Command::new("lark-cli")
            .args(["event", "consume", EVENT_KEY, "--quiet", "--as", "bot"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "feishu: failed to spawn lark-cli");
                return ConsumerExit::Crashed;
            }
        };

        // Keep stdin open forever (EOF = graceful exit; we shut down via kill).
        let _stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();

        let ready = Arc::new(AtomicBool::new(false));
        let ready_reader = Arc::clone(&ready);
        let stderr_task = tokio::spawn(async move {
            while let Ok(Some(line)) = stderr_lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.contains("[event] ready") {
                    ready_reader.store(true, Ordering::Relaxed);
                }
                if is_routine_marker(trimmed) {
                    debug!(text = %truncate(trimmed, 500), "feishu: consumer stderr");
                } else if !trimmed.is_empty() {
                    info!(text = %truncate(trimmed, 500), "feishu: consumer stderr");
                }
            }
        });

        let exit = loop {
            tokio::select! {
                line = stdout_lines.next_line() => match line {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            self.handle_event_line(trimmed);
                        }
                    }
                    Ok(None) | Err(_) => break None,
                },
                () = cancel.cancelled() => break Some(ConsumerExit::Cancelled),
            }
        };

        stderr_task.abort();
        if matches!(exit, Some(ConsumerExit::Cancelled)) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return ConsumerExit::Cancelled;
        }
        let _ = child.wait().await;
        if ready.load(Ordering::Relaxed) {
            ConsumerExit::Ready
        } else {
            ConsumerExit::Crashed
        }
    }
}

struct ParsedInbound {
    chat_id: String,
    chat_type: &'static str,
    sender_id: String,
    item: InboundItem,
}

enum ConsumerExit {
    Cancelled,
    Ready,
    Crashed,
}

// ---------------------------------------------------------------------------
// FeishuChannel
// ---------------------------------------------------------------------------

pub struct FeishuChannel {
    inner: Arc<FeishuInner>,
    cancel: Mutex<Option<CancellationToken>>,
}

impl FeishuChannel {
    pub fn new(
        on_receive_tx: mpsc::UnboundedSender<ChannelMessage>,
        settings: FeishuSettings,
    ) -> Self {
        Self {
            inner: Arc::new(FeishuInner {
                on_receive_tx,
                account_id: settings.account_id,
                state: Mutex::new(FeishuState::default()),
            }),
            cancel: Mutex::new(None),
        }
    }
}

fn combine_batch_text(items: &[InboundItem]) -> String {
    if items.len() == 1 {
        return items[0].text.clone();
    }
    let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
    combine_lines(&texts)
}

fn format_history_line(m: &Value) -> String {
    let role = if m.pointer("/sender/sender_type").and_then(|v| v.as_str()) == Some("app") {
        "助手"
    } else {
        "用户"
    };
    let time = m
        .get("create_time")
        .and_then(|v| v.as_str())
        .map(|t| t.chars().skip(5).collect::<String>())
        .unwrap_or_default();
    let mut content = match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    if content.chars().count() > HISTORY_TEXT_CAP {
        content = format!("{}…", content.chars().take(HISTORY_TEXT_CAP).collect::<String>());
    }
    if time.is_empty() {
        format!("{role}: {content}")
    } else {
        format!("[{time}] {role}: {content}")
    }
}

fn is_routine_marker(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "[event] consuming as",
        "[event] listening",
        "[event] to stop gracefully",
        "[event] ready",
        "[event] local bus",
        "[event] started bus daemon",
        "[event] remote connection check",
    ];
    MARKERS.iter().any(|m| text.contains(m))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

fn json_id(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

#[async_trait]
impl Channel for FeishuChannel {
    fn name(&self) -> &str {
        "feishu"
    }

    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        info!("feishu.start");
        *self.cancel.lock() = Some(cancel.clone());
        self.inner.load_bot_identity().await;
        // Recycle any stale event bus before consuming (non-zero exit is fine).
        let _ = run_lark_cli(&owned(&["event", "stop", "--force"])).await;
        // Run the supervised consumer loop; blocks until cancelled (the gateway
        // spawns Channel::start in its own task).
        Arc::clone(&self.inner).run_consumer(cancel).await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        if let Some(cancel) = self.cancel.lock().take() {
            cancel.cancel();
        }
        info!("feishu.stopped");
        Ok(())
    }

    async fn send(&self, message: ChannelMessage) -> anyhow::Result<()> {
        let ctx = &message.context;
        let flag = |key: &str| ctx.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
        let is_notice = flag("_eli_mid_turn");
        let is_cleanup = flag("_eli_cleanup_only");
        // Routing target: channel_target (set on inbound) else chat_id.
        let to = ctx
            .get("channel_target")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&message.chat_id)
            .to_owned();
        let reply_to_id = ctx
            .get("reply_to_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        self.inner
            .send_outbound(&to, &message.content, reply_to_id.as_deref(), is_notice, is_cleanup)
            .await;
        Ok(())
    }

    fn needs_debounce(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_inner() -> Arc<FeishuInner> {
        let (tx, _rx) = mpsc::unbounded_channel();
        Arc::new(FeishuInner {
            on_receive_tx: tx,
            account_id: "default".to_owned(),
            state: Mutex::new(FeishuState::default()),
        })
    }

    #[test]
    fn dedup_unknown_id_never_seen() {
        let mut st = FeishuState::default();
        let now = Instant::now();
        assert!(!st.already_seen("", now));
        assert!(!st.already_seen("", now));
    }

    #[test]
    fn dedup_known_id_second_time() {
        let mut st = FeishuState::default();
        let now = Instant::now();
        assert!(!st.already_seen("e1", now));
        assert!(st.already_seen("e1", now));
        assert!(!st.already_seen("e2", now));
    }

    #[test]
    fn anti_loop_app_and_self_senders() {
        assert!(FeishuInner::is_app_sender("cli_abc"));
        assert!(FeishuInner::is_app_sender("app_xyz"));
        assert!(!FeishuInner::is_app_sender("ou_user"));
        assert!(!FeishuInner::is_app_sender("on_union"));

        let inner = make_inner();
        inner.state.lock().bot.open_id = Some("ou_bot".to_owned());
        assert!(inner.is_self_sender("ou_bot"));
        assert!(!inner.is_self_sender("ou_user"));
    }

    #[test]
    fn mention_gate_name_and_heuristic() {
        let inner = make_inner();
        inner.state.lock().bot.name = Some("小助手".to_owned());
        assert!(inner.group_mentions_bot("@小助手 你好"));
        assert!(!inner.group_mentions_bot("你好"));

        // No identity → heuristic (stripMentions changes length).
        let inner2 = make_inner();
        assert!(inner2.group_mentions_bot("@someone hi"));
        assert!(!inner2.group_mentions_bot("just text"));
    }

    #[test]
    fn parse_event_drops_sticker_and_self() {
        let inner = make_inner();
        inner.state.lock().bot.open_id = Some("ou_bot".to_owned());

        let sticker = json!({
            "event_id": "e1", "chat_id": "oc_1", "sender_id": "ou_u",
            "chat_type": "p2p", "message_type": "sticker", "content": "x"
        });
        assert!(inner.parse_event(&sticker).is_none());

        let from_self = json!({
            "event_id": "e2", "chat_id": "oc_1", "sender_id": "ou_bot",
            "chat_type": "p2p", "message_type": "text", "content": "hi"
        });
        assert!(inner.parse_event(&from_self).is_none());
    }

    #[test]
    fn parse_event_group_requires_mention() {
        let inner = make_inner();
        inner.state.lock().bot.name = Some("Eli".to_owned());
        let no_mention = json!({
            "event_id": "e3", "chat_id": "oc_g", "sender_id": "ou_u",
            "chat_type": "group", "message_type": "text", "content": "hello"
        });
        assert!(inner.parse_event(&no_mention).is_none());

        let mention = json!({
            "event_id": "e4", "chat_id": "oc_g", "sender_id": "ou_u",
            "chat_type": "group", "message_type": "text", "content": "@Eli hello"
        });
        let parsed = inner.parse_event(&mention).unwrap();
        assert_eq!(parsed.chat_type, "group");
        assert_eq!(parsed.item.text, "hello");
    }

    #[test]
    fn parse_event_direct_always_processed() {
        let inner = make_inner();
        let dm = json!({
            "event_id": "e5", "chat_id": "oc_p", "sender_id": "ou_u",
            "chat_type": "p2p", "message_type": "text", "content": "hi there"
        });
        let parsed = inner.parse_event(&dm).unwrap();
        assert_eq!(parsed.chat_type, "direct");
    }

    #[test]
    fn combine_batch_single_verbatim() {
        let items = vec![InboundItem { message_id: "m".into(), text: "solo".into() }];
        assert_eq!(combine_batch_text(&items), "solo");
    }

    #[test]
    fn combine_batch_multi_numbered() {
        let items = vec![
            InboundItem { message_id: "m1".into(), text: "first".into() },
            InboundItem { message_id: "m2".into(), text: "second".into() },
        ];
        assert_eq!(combine_batch_text(&items), "[消息 1/2] first\n[消息 2/2] second");
    }

    #[test]
    fn route_args_by_prefix() {
        assert_eq!(route_args("oc_chat"), ["--chat-id", "oc_chat"]);
        assert_eq!(route_args("ou_user"), ["--user-id", "ou_user"]);
        assert_eq!(route_args("xyz"), ["--chat-id", "xyz"]);
    }
}
