//! Native Telegram channel — long-poll `getUpdates`, access control, group
//! gating, media download, and Markdown-with-plain-fallback outbound.
//!
//! Ported from the deleted `sidecar/plugins/telegram.ts` at commit `ecfa7c3^`,
//! using `reqwest` directly (no teloxide). Telegram JSON is accessed via
//! `serde_json::Value` (the envelope pattern) rather than typed structs.
//
// ponytail: IPv4/IPv6 pinning from the TS plugin is intentionally dropped. It
// existed to dodge a Node/undici IPv6 connect-timeout bug that does not affect
// reqwest/hyper. Re-add via a hickory-dns resolver + ELI_TELEGRAM_IP_FAMILY if
// a real v6 path breaks.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::base::Channel;
use super::media::download_to_temp;
use super::message::{ChannelMessage, MessageKind};

const API_BASE: &str = "https://api.telegram.org";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const LONG_POLL_SECS: u64 = 30;
const LONG_POLL_GRACE: Duration = Duration::from_secs(5);
const POLL_ERROR_BACKOFF: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// TelegramSettings
// ---------------------------------------------------------------------------

/// Telegram channel configuration, resolved from `ELI_TELEGRAM_*` env vars
/// (with `SIDECAR_TELEGRAM_*` accepted as aliases for backward compat).
#[derive(Debug, Clone)]
pub struct TelegramSettings {
    pub token: String,
    pub allow_users: Vec<String>,
    pub allow_chats: Vec<String>,
}

fn env_alias(primary: &str, alias: &str) -> Option<String> {
    std::env::var(primary)
        .ok()
        .or_else(|| std::env::var(alias).ok())
        .filter(|s| !s.is_empty())
}

fn parse_set(raw: Option<String>) -> Vec<String> {
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

impl TelegramSettings {
    /// Load from env; returns `None` if no bot token is configured.
    pub fn from_env() -> Option<Self> {
        let token = env_alias("ELI_TELEGRAM_TOKEN", "SIDECAR_TELEGRAM_TOKEN")?;
        Some(Self {
            token,
            allow_users: parse_set(env_alias(
                "ELI_TELEGRAM_ALLOW_USERS",
                "SIDECAR_TELEGRAM_ALLOW_USERS",
            )),
            allow_chats: parse_set(env_alias(
                "ELI_TELEGRAM_ALLOW_CHATS",
                "SIDECAR_TELEGRAM_ALLOW_CHATS",
            )),
        })
    }
}

// ---------------------------------------------------------------------------
// Access control
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Access {
    Allowed,
    DeniedChat,
    DeniedUser,
    Start,
}

/// Evaluate access for an inbound message. Chats checked before users; an empty
/// allow-list means allow-all; a `/start` from an allowed sender is `Start`.
fn check_access(msg: &Value, allow_users: &[String], allow_chats: &[String]) -> Access {
    let chat_id = msg
        .pointer("/chat/id")
        .map(json_id_string)
        .unwrap_or_default();
    if !allow_chats.is_empty() && !allow_chats.iter().any(|c| c == &chat_id) {
        return Access::DeniedChat;
    }
    if !allow_users.is_empty()
        && let Some(from) = msg.get("from")
    {
        let uid = from.get("id").map(json_id_string).unwrap_or_default();
        let uname = from
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let ok = allow_users.iter().any(|u| u == &uid || u == &uname);
        if !ok {
            return Access::DeniedUser;
        }
    }
    if text_field(msg).is_some_and(|t| t.starts_with("/start")) {
        return Access::Start;
    }
    Access::Allowed
}

/// Whether a group message should be processed (mentions the bot by "eli"
/// substring or `@username`, or replies to the bot). Media-only messages
/// require a reply to the bot.
fn should_process_group(msg: &Value, bot_id: i64, bot_username: &str) -> bool {
    let text = msg
        .get("text")
        .or_else(|| msg.get("caption"))
        .and_then(|v| v.as_str());
    let replies_to_bot = msg
        .pointer("/reply_to_message/from/id")
        .and_then(Value::as_i64)
        == Some(bot_id);
    match text {
        None => replies_to_bot,
        Some(t) => {
            let lower = t.to_lowercase();
            let mentions = lower.contains("eli")
                || (!bot_username.is_empty()
                    && lower.contains(&format!("@{}", bot_username.to_lowercase())));
            mentions || replies_to_bot
        }
    }
}

// ---------------------------------------------------------------------------
// Content shaping
// ---------------------------------------------------------------------------

/// Drop a leading `/eli ` command prefix.
fn strip_eli_prefix(text: &str) -> &str {
    text.strip_prefix("/eli ").unwrap_or(text)
}

/// Render message content, producing typed placeholders for media without a
/// text body. First match wins.
fn format_content(msg: &Value) -> String {
    if let Some(t) = msg.get("text").and_then(|v| v.as_str()) {
        return t.to_owned();
    }
    let caption = msg.get("caption").and_then(|v| v.as_str()).unwrap_or("");
    let dur = |key: &str| {
        msg.pointer(&format!("/{key}/duration"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };

    if msg.get("photo").is_some() {
        return if caption.is_empty() {
            "[Photo]".into()
        } else {
            format!("[Photo] {caption}")
        };
    }
    if let Some(audio) = msg.get("audio") {
        let title = audio
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");
        let performer = audio
            .get("performer")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return if performer.is_empty() {
            format!("[Audio: {title}]")
        } else {
            format!("[Audio: {performer} - {title}]")
        };
    }
    if msg.get("voice").is_some() {
        return format!("[Voice: {}s]", dur("voice"));
    }
    if msg.get("video").is_some() {
        return if caption.is_empty() {
            format!("[Video: {}s]", dur("video"))
        } else {
            format!("[Video: {}s] {caption}", dur("video"))
        };
    }
    if msg.get("video_note").is_some() {
        return format!("[Video note: {}s]", dur("video_note"));
    }
    if let Some(doc) = msg.get("document") {
        let name = doc
            .get("file_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return if caption.is_empty() {
            format!("[Document: {name}]")
        } else {
            format!("[Document: {name}] {caption}")
        };
    }
    if let Some(sticker) = msg.get("sticker") {
        let emoji = sticker.get("emoji").and_then(|v| v.as_str()).unwrap_or("");
        return if emoji.is_empty() {
            "[Sticker]".into()
        } else {
            format!("[Sticker: {emoji}]")
        };
    }
    caption.to_owned()
}

/// Detected media: (Telegram file_id, media_type label, file extension).
fn detect_media(msg: &Value) -> Option<(String, &'static str, &'static str)> {
    if let Some(photos) = msg.get("photo").and_then(|v| v.as_array())
        && let Some(largest) = photos.last()
        && let Some(id) = largest.get("file_id").and_then(|v| v.as_str())
    {
        return Some((id.to_owned(), "image", ".jpg"));
    }
    for (key, label, ext) in [
        ("audio", "audio", ".mp3"),
        ("voice", "audio", ".ogg"),
        ("video", "video", ".mp4"),
        ("video_note", "video", ".mp4"),
        ("document", "file", ""),
    ] {
        if let Some(id) = msg
            .pointer(&format!("/{key}/file_id"))
            .and_then(|v| v.as_str())
        {
            return Some((id.to_owned(), label, ext));
        }
    }
    if let Some(sticker) = msg.get("sticker")
        && let Some(id) = sticker.get("file_id").and_then(|v| v.as_str())
    {
        let animated = sticker
            .get("is_animated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return Some((
            id.to_owned(),
            "image",
            if animated { ".webm" } else { ".webp" },
        ));
    }
    None
}

fn text_field(msg: &Value) -> Option<&str> {
    msg.get("text").and_then(|v| v.as_str())
}

/// Stringify a JSON number/string id without quotes or `.0`.
fn json_id_string(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn full_name(from: Option<&Value>) -> String {
    let field = |k: &str| {
        from.and_then(|f| f.get(k))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    [field("first_name"), field("last_name")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// TelegramChannel
// ---------------------------------------------------------------------------

pub struct TelegramChannel {
    on_receive_tx: mpsc::UnboundedSender<ChannelMessage>,
    settings: TelegramSettings,
    client: reqwest::Client,
}

impl TelegramChannel {
    pub fn new(
        on_receive_tx: mpsc::UnboundedSender<ChannelMessage>,
        settings: TelegramSettings,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            on_receive_tx,
            settings,
            client,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("{API_BASE}/bot{}/{method}", self.settings.token)
    }

    /// POST a JSON API call, returning `result` or an error (HTTP or `ok:false`).
    async fn call_api(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let resp = self
            .client
            .post(self.api_url(method))
            .timeout(timeout)
            .json(&params)
            .send()
            .await?;
        let data: Value = resp.json().await?;
        if data.get("ok").and_then(Value::as_bool) != Some(true) {
            let desc = data
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("Telegram API {method}: {desc}");
        }
        Ok(data.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Send a text message (MarkdownV2, falling back to plain on any error).
    /// Empty/whitespace text is a no-op.
    pub async fn send_text(&self, chat_id: i64, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let md = json!({"chat_id": chat_id, "text": text, "parse_mode": "MarkdownV2"});
        if self
            .call_api("sendMessage", md, REQUEST_TIMEOUT)
            .await
            .is_err()
        {
            let plain = json!({"chat_id": chat_id, "text": text});
            if let Err(e) = self.call_api("sendMessage", plain, REQUEST_TIMEOUT).await {
                warn!(error = %e, "telegram: sendMessage failed");
            }
        }
    }

    async fn send_typing(&self, chat_id: i64) {
        let _ = self
            .call_api(
                "sendChatAction",
                json!({"chat_id": chat_id, "action": "typing"}),
                REQUEST_TIMEOUT,
            )
            .await;
    }

    /// Download a Telegram file by id into a temp file, returning its path.
    async fn download_file(&self, file_id: &str, ext: &str) -> anyhow::Result<std::path::PathBuf> {
        let file = self
            .call_api("getFile", json!({"file_id": file_id}), REQUEST_TIMEOUT)
            .await?;
        let file_path = file
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("getFile: no file_path"))?;
        let url = format!("{API_BASE}/file/bot{}/{file_path}", self.settings.token);
        download_to_temp(&url, ext).await
    }

    /// Collect media from the message and its replied-to message, downloading
    /// each to a temp file. Returns (paths, mime-type labels).
    async fn collect_media(&self, msg: &Value) -> (Vec<String>, Vec<String>) {
        let mut paths = Vec::new();
        let mut types = Vec::new();
        for source in [Some(msg), msg.get("reply_to_message")]
            .into_iter()
            .flatten()
        {
            let Some((file_id, label, ext)) = detect_media(source) else {
                continue;
            };
            match self.download_file(&file_id, ext).await {
                Ok(path) => {
                    paths.push(path.to_string_lossy().into_owned());
                    types.push(label.to_owned());
                }
                Err(e) => error!(error = %e, "telegram: media download failed"),
            }
        }
        (paths, types)
    }

    /// Build the inbound envelope for a normal message and enqueue it.
    async fn handle_message(&self, msg: &Value, bot_id: i64, bot_username: &str) {
        match check_access(msg, &self.settings.allow_users, &self.settings.allow_chats) {
            Access::DeniedChat => {
                if text_field(msg).is_some_and(|t| t.starts_with("/start"))
                    && let Some(chat_id) = chat_id_of(msg)
                {
                    self.send_text(
                        chat_id,
                        "You are not allowed to chat with me. Please deploy your own instance of Eli.",
                    )
                    .await;
                }
                return;
            }
            Access::DeniedUser => {
                if let Some(chat_id) = chat_id_of(msg) {
                    self.send_text(chat_id, "Access denied.").await;
                }
                return;
            }
            Access::Start => {
                if let Some(chat_id) = chat_id_of(msg) {
                    self.send_text(chat_id, "Eli is online. Send text to start.")
                        .await;
                }
                return;
            }
            Access::Allowed => {}
        }

        let chat_type = msg
            .pointer("/chat/type")
            .and_then(|v| v.as_str())
            .unwrap_or("private");
        let is_group = chat_type != "private";
        if is_group && !should_process_group(msg, bot_id, bot_username) {
            return;
        }

        let Some(chat_id) = chat_id_of(msg) else {
            return;
        };
        self.send_typing(chat_id).await;

        let content = strip_eli_prefix(&format_content(msg)).to_owned();
        let (paths, types) = self.collect_media(msg).await;

        let sender_id = msg
            .pointer("/from/id")
            .map(json_id_string)
            .unwrap_or_default();
        let sender_name = full_name(msg.get("from"));
        let mut context = serde_json::Map::new();
        context.insert("source_channel".into(), json!("telegram"));
        context.insert("account_id".into(), json!("default"));
        context.insert("sender_id".into(), json!(sender_id));
        context.insert("sender_name".into(), json!(sender_name));
        context.insert(
            "chat_type".into(),
            json!(if is_group { "group" } else { "direct" }),
        );
        if !paths.is_empty() {
            context.insert("media_paths".into(), json!(paths));
            context.insert("media_types".into(), json!(types));
        }

        let session_id = format!("telegram:default:{chat_id}");
        let message = ChannelMessage::new(session_id, "telegram", content)
            .with_chat_id(chat_id.to_string())
            .with_is_active(true)
            .with_context(context)
            .finalize();
        let _ = self.on_receive_tx.send(message);
    }

    /// Handle a `my_chat_member` update: emit a Join envelope when the bot is
    /// newly added to a chat.
    fn handle_my_chat_member(&self, cm: &Value, bot_id: i64) {
        let status = |ptr: &str| cm.pointer(ptr).and_then(|v| v.as_str()).unwrap_or("");
        let was_absent = matches!(status("/old_chat_member/status"), "left" | "kicked");
        let is_present = matches!(
            status("/new_chat_member/status"),
            "member" | "administrator" | "creator"
        );
        let is_bot = cm
            .pointer("/new_chat_member/user/id")
            .and_then(Value::as_i64)
            == Some(bot_id);
        if !(was_absent && is_present && is_bot) {
            return;
        }
        let Some(chat_id) = cm.pointer("/chat/id").and_then(Value::as_i64) else {
            return;
        };
        let chat_type = cm
            .pointer("/chat/type")
            .and_then(|v| v.as_str())
            .unwrap_or("group");
        let mut context = serde_json::Map::new();
        context.insert("source_channel".into(), json!("telegram"));
        context.insert("account_id".into(), json!("default"));
        context.insert(
            "chat_type".into(),
            json!(if chat_type == "private" {
                "direct"
            } else {
                "group"
            }),
        );
        let session_id = format!("telegram:default:{chat_id}");
        let message = ChannelMessage::new(session_id, "telegram", "")
            .with_chat_id(chat_id.to_string())
            .with_is_active(true)
            .with_kind(MessageKind::Join)
            .with_context(context)
            .finalize();
        let _ = self.on_receive_tx.send(message);
    }

    async fn poll_loop(&self, cancel: CancellationToken) {
        // Resolve bot identity.
        let me = match self.call_api("getMe", json!({}), REQUEST_TIMEOUT).await {
            Ok(me) => me,
            Err(e) => {
                error!(error = %e, "telegram: getMe failed, channel not started");
                return;
            }
        };
        let bot_id = me.get("id").and_then(Value::as_i64).unwrap_or(0);
        let bot_username = me
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        info!(bot_id, username = %bot_username, "telegram: bot identity resolved");

        let mut offset: i64 = 0;
        let poll_timeout =
            REQUEST_TIMEOUT.max(Duration::from_secs(LONG_POLL_SECS) + LONG_POLL_GRACE);

        while !cancel.is_cancelled() {
            let params = json!({
                "offset": offset,
                "timeout": LONG_POLL_SECS,
                "allowed_updates": ["message", "my_chat_member"],
            });
            let updates = tokio::select! {
                r = self.call_api("getUpdates", params, poll_timeout) => r,
                () = cancel.cancelled() => break,
            };
            let updates = match updates {
                Ok(Value::Array(u)) => u,
                Ok(_) => continue,
                Err(e) => {
                    if cancel.is_cancelled() {
                        break;
                    }
                    if e.to_string().contains("Conflict:") {
                        error!(
                            "telegram: another instance is polling this bot token; \
                             stopping poller (Conflict)"
                        );
                        break;
                    }
                    error!(error = %e, "telegram: polling error");
                    tokio::select! {
                        () = tokio::time::sleep(POLL_ERROR_BACKOFF) => {}
                        () = cancel.cancelled() => break,
                    }
                    continue;
                }
            };

            for update in &updates {
                if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                    offset = id + 1;
                }
                if let Some(cm) = update.get("my_chat_member") {
                    self.handle_my_chat_member(cm, bot_id);
                    continue;
                }
                if let Some(msg) = update.get("message") {
                    self.handle_message(msg, bot_id, &bot_username).await;
                }
            }
        }
    }
}

fn chat_id_of(msg: &Value) -> Option<i64> {
    msg.pointer("/chat/id").and_then(Value::as_i64)
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        // Runs inline: the gateway spawns `Channel::start` in its own task, so
        // blocking here is correct. Shutdown is driven by `cancel`.
        info!("telegram.start");
        self.poll_loop(cancel).await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        // The poll loop exits on its CancellationToken; nothing else to release.
        info!("telegram.stopped");
        Ok(())
    }

    async fn send(&self, message: ChannelMessage) -> anyhow::Result<()> {
        let chat_id: i64 = message
            .chat_id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid chat_id: {}", message.chat_id))?;
        if chat_id == 0 {
            anyhow::bail!("invalid chat_id: {}", message.chat_id);
        }
        self.send_text(chat_id, &message.content).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_eli_prefix_drops_command() {
        assert_eq!(strip_eli_prefix("/eli hello"), "hello");
        assert_eq!(strip_eli_prefix("hello"), "hello");
        assert_eq!(strip_eli_prefix("/elihello"), "/elihello");
    }

    #[test]
    fn parse_set_splits_and_trims() {
        assert_eq!(parse_set(Some(" a , b ,,c ".into())), vec!["a", "b", "c"]);
        assert!(parse_set(None).is_empty());
        assert!(parse_set(Some("".into())).is_empty());
    }

    #[test]
    fn access_empty_lists_allow_all() {
        let msg = json!({"chat": {"id": 42}, "from": {"id": 7}, "text": "hi"});
        assert_eq!(check_access(&msg, &[], &[]), Access::Allowed);
    }

    #[test]
    fn access_denied_chat() {
        let msg = json!({"chat": {"id": 42}, "text": "hi"});
        assert_eq!(check_access(&msg, &[], &["99".into()]), Access::DeniedChat);
    }

    #[test]
    fn access_denied_user_by_id_or_username() {
        let msg = json!({"chat": {"id": 42}, "from": {"id": 7, "username": "bob"}, "text": "hi"});
        assert_eq!(check_access(&msg, &["9".into()], &[]), Access::DeniedUser);
        assert_eq!(check_access(&msg, &["7".into()], &[]), Access::Allowed);
        assert_eq!(check_access(&msg, &["bob".into()], &[]), Access::Allowed);
    }

    #[test]
    fn access_start() {
        let msg = json!({"chat": {"id": 42}, "from": {"id": 7}, "text": "/start"});
        assert_eq!(check_access(&msg, &[], &[]), Access::Start);
    }

    #[test]
    fn group_gating_mention_and_reply() {
        // "eli" substring triggers
        let m1 = json!({"text": "hey eli help"});
        assert!(should_process_group(&m1, 100, "mybot"));
        // @username triggers
        let m2 = json!({"text": "yo @MyBot"});
        assert!(should_process_group(&m2, 100, "mybot"));
        // unrelated text ignored
        let m3 = json!({"text": "just chatting"});
        assert!(!should_process_group(&m3, 100, "mybot"));
        // reply to bot triggers even without mention
        let m4 = json!({"text": "thanks", "reply_to_message": {"from": {"id": 100}}});
        assert!(should_process_group(&m4, 100, "mybot"));
        // media-only requires reply-to-bot
        let m5 = json!({"photo": [{"file_id": "x"}]});
        assert!(!should_process_group(&m5, 100, "mybot"));
        let m6 = json!({"photo": [{"file_id": "x"}], "reply_to_message": {"from": {"id": 100}}});
        assert!(should_process_group(&m6, 100, "mybot"));
    }

    #[test]
    fn format_content_text_verbatim() {
        assert_eq!(format_content(&json!({"text": "hello"})), "hello");
    }

    #[test]
    fn format_content_typed_placeholders() {
        assert_eq!(format_content(&json!({"photo": []})), "[Photo]");
        assert_eq!(
            format_content(&json!({"photo": [], "caption": "cap"})),
            "[Photo] cap"
        );
        assert_eq!(
            format_content(&json!({"audio": {"title": "Song", "performer": "Artist"}})),
            "[Audio: Artist - Song]"
        );
        assert_eq!(
            format_content(&json!({"audio": {"title": "Song"}})),
            "[Audio: Song]"
        );
        assert_eq!(
            format_content(&json!({"voice": {"duration": 5}})),
            "[Voice: 5s]"
        );
        assert_eq!(
            format_content(&json!({"video": {"duration": 8}})),
            "[Video: 8s]"
        );
        assert_eq!(
            format_content(&json!({"document": {"file_name": "a.pdf"}})),
            "[Document: a.pdf]"
        );
        assert_eq!(
            format_content(&json!({"sticker": {"emoji": "😀"}})),
            "[Sticker: 😀]"
        );
        assert_eq!(format_content(&json!({"sticker": {}})), "[Sticker]");
    }

    #[test]
    fn detect_media_types_and_ext() {
        assert_eq!(
            detect_media(&json!({"photo": [{"file_id": "a"}, {"file_id": "b"}]})),
            Some(("b".to_owned(), "image", ".jpg"))
        );
        assert_eq!(
            detect_media(&json!({"document": {"file_id": "d"}})),
            Some(("d".to_owned(), "file", ""))
        );
        assert_eq!(
            detect_media(&json!({"sticker": {"file_id": "s", "is_animated": true}})),
            Some(("s".to_owned(), "image", ".webm"))
        );
        assert_eq!(detect_media(&json!({"text": "hi"})), None);
    }

    #[test]
    fn full_name_joins_present_parts() {
        assert_eq!(
            full_name(Some(&json!({"first_name": "Ada", "last_name": "L"}))),
            "Ada L"
        );
        assert_eq!(full_name(Some(&json!({"first_name": "Ada"}))), "Ada");
        assert_eq!(full_name(None), "");
    }
}
