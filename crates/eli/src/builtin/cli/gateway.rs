//! Gateway command: native channel listeners (Feishu, Telegram).

use std::sync::Arc;

use base64::Engine;
use serde_json::Value;

use crate::channels::message::{ChannelMessage, MediaItem, MediaType};

fn context_string_array(context: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    context
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn report_gateway_task(result: Result<(), tokio::task::JoinError>) {
    if let Err(err) = result
        && !err.is_cancelled()
    {
        eprintln!("Gateway task failed: {err}");
    }
}

async fn drain_gateway_tasks(tasks: &mut tokio::task::JoinSet<()>) {
    tasks.abort_all();
    while let Some(result) = tasks.join_next().await {
        report_gateway_task(result);
    }
}

async fn drain_processing_tasks(tasks: &mut tokio::task::JoinSet<()>) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !tasks.is_empty() {
        let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
        if wait.is_zero() {
            break;
        }
        match tokio::time::timeout(wait, tasks.join_next()).await {
            Ok(Some(result)) => report_gateway_task(result),
            Ok(None) | Err(_) => break,
        }
    }
    drain_gateway_tasks(tasks).await;
}

/// Build a [`ChannelMessage`] from a framework envelope (serde_json::Value).
fn channel_message_from_envelope(envelope: &Value) -> ChannelMessage {
    let session_id = envelope
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let channel = envelope
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("subagent")
        .to_owned();
    let content = envelope
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let chat_id = envelope
        .get("chat_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_owned();
    let output_channel = envelope
        .get("output_channel")
        .and_then(|v| v.as_str())
        .unwrap_or(&channel)
        .to_owned();
    let context = envelope
        .get("context")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    ChannelMessage {
        session_id,
        channel,
        content,
        chat_id,
        is_active: false,
        kind: crate::channels::message::MessageKind::Normal,
        context,
        media: Vec::new(),
        output_channel,
    }
}

// ---------------------------------------------------------------------------
// PID lock — prevents two `eli gateway` processes from fighting over the same
// Telegram bot token (Telegram's getUpdates is single-consumer).
// ---------------------------------------------------------------------------

struct GatewayLockGuard {
    path: std::path::PathBuf,
}

impl Drop for GatewayLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_gateway_lock() -> anyhow::Result<GatewayLockGuard> {
    let eli_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".eli");
    let _ = std::fs::create_dir_all(&eli_dir);
    let lock_path = eli_dir.join("gateway.lock");

    // If a lock file exists, check whether the owning process is still alive.
    if lock_path.exists()
        && let Ok(contents) = std::fs::read_to_string(&lock_path)
        && let Ok(pid) = contents.trim().parse::<u32>()
    {
        // Signal 0 checks if process exists without killing it.
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if alive {
            anyhow::bail!(
                "Another eli gateway is already running (PID {pid}).\n\
                 Kill it first or remove {}",
                lock_path.display()
            );
        }
        // Stale lock — fall through and overwrite it.
    }

    std::fs::write(&lock_path, std::process::id().to_string())?;
    Ok(GatewayLockGuard { path: lock_path })
}

/// Start native channel listeners (Feishu, Telegram).
pub(crate) async fn gateway_command() -> anyhow::Result<()> {
    use std::collections::HashMap;

    use crate::channels::base::Channel;
    use tokio_util::sync::CancellationToken;

    // Acquire a PID lock to prevent concurrent gateway instances.
    let _lock_guard = acquire_gateway_lock()?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChannelMessage>(256);
    let (ingress_tx, mut ingress_rx) = tokio::sync::mpsc::unbounded_channel::<ChannelMessage>();
    let cancel = CancellationToken::new();
    #[allow(unused_mut)]
    let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    let mut workers = tokio::task::JoinSet::new();

    let ingress_cancel = cancel.clone();
    tasks.spawn(async move {
        loop {
            let msg = match tokio::select! {
                msg = ingress_rx.recv() => msg,
                () = ingress_cancel.cancelled() => None,
            } {
                Some(msg) => msg,
                None => break,
            };

            tokio::select! {
                res = tx.send(msg) => {
                    if res.is_err() {
                        break;
                    }
                }
                () = ingress_cancel.cancelled() => break,
            }
        }
    });

    // Telegram: long-poll getUpdates when a bot token is configured.
    if let Some(settings) = crate::channels::TelegramSettings::from_env() {
        let ch = Arc::new(crate::channels::TelegramChannel::new(
            ingress_tx.clone(),
            settings,
        ));
        println!("Starting Telegram channel...");
        let started = ch.clone();
        let c = cancel.clone();
        tasks.spawn(async move {
            if let Err(e) = Channel::start(&*started, c).await {
                eprintln!("Telegram channel error: {e}");
            }
        });
        channels.insert("telegram".to_owned(), ch);
    }

    // Feishu (via lark-cli) wires in here in Phase 3.

    if channels.is_empty() {
        anyhow::bail!(
            "No channels configured.\n\
             Set ELI_TELEGRAM_TOKEN for Telegram, or log in with lark-cli for Feishu."
        );
    }

    let cancel_for_signal = cancel.clone();
    let signal_shutdown = cancel.clone();
    tasks.spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\nShutting down...");
                cancel_for_signal.cancel();
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("\nForce exit.");
                        std::process::exit(1);
                    }
                    _ = signal_shutdown.cancelled() => {}
                }
            }
            _ = signal_shutdown.cancelled() => {}
        }
    });

    let (framework, builtin) = super::builtin_framework().await;
    builtin.set_channels(channels.clone());

    // Wire inbound injector so subagent results flow back into the pipeline.
    {
        let itx = ingress_tx.clone();
        crate::control_plane::set_inbound_injector(Arc::new(move |envelope| {
            let tx = itx.clone();
            Box::pin(async move {
                let msg = channel_message_from_envelope(&envelope);
                let _ = tx.send(msg);
            })
        }));
    }

    loop {
        tokio::select! {
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                report_gateway_task(result);
            }
            maybe_msg = rx.recv() => {
                let Some(msg) = maybe_msg else {
                    break;
                };
                let output_channel = if msg.output_channel.is_empty() {
                    msg.channel.clone()
                } else {
                    msg.output_channel.clone()
                };

                let media_from_context = reconstruct_context_media(&msg);
                let combined_media: Vec<MediaItem> =
                    [msg.media.as_slice(), media_from_context.as_slice()].concat();
                let resolved = resolve_image_media(&combined_media).await;
                let content = if resolved.errors.is_empty() {
                    msg.content.clone()
                } else {
                    format!("{}\n{}", msg.content, resolved.errors.join("\n"))
                };

                let mut inbound = serde_json::json!({
                    "session_id": msg.session_id,
                    "channel": msg.channel,
                    "chat_id": msg.chat_id,
                    "content": content,
                    "context": msg.context,
                    "kind": msg.kind,
                    "output_channel": output_channel,
                });
                if !resolved.parts.is_empty() {
                    inbound["media_parts"] = serde_json::json!(resolved.parts);
                }

                let fw = framework.clone();
                let cancel_inner = cancel.clone();
                workers.spawn(async move {
                    let result = tokio::select! {
                        r = fw.process_inbound(inbound) => r,
                        () = cancel_inner.cancelled() => return,
                    };
                    match result {
                        Ok(result) => {
                            tracing::info!(session = %result.session_id, "framework run completed");
                        }
                        Err(e) => eprintln!("Framework error: {e}"),
                    }
                });
            }
            () = cancel.cancelled() => {
                break;
            }
        }
    }

    drain_processing_tasks(&mut workers).await;

    for (name, ch) in &channels {
        if let Err(e) = ch.stop().await {
            eprintln!("Error stopping {name}: {e}");
        }
    }
    drain_gateway_tasks(&mut tasks).await;
    println!("Gateway stopped.");
    Ok(())
}

fn reconstruct_context_media(msg: &ChannelMessage) -> Vec<MediaItem> {
    let mut items: Vec<(String, String)> = Vec::new();

    if let Some(outbound) = msg.context.get("outbound_media").and_then(|v| v.as_array()) {
        for item in outbound {
            let Some(path) = item.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            let media_type = item
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("image");
            items.push((path.to_owned(), media_type.to_owned()));
        }
    }

    let paths = context_string_array(&msg.context, "media_paths");
    let types = context_string_array(&msg.context, "media_types");
    tracing::debug!(
        session = %msg.session_id,
        outbound = items.len(),
        paths = paths.len(),
        types = types.len(),
        "reconstructing media from context"
    );
    for (i, path) in paths.into_iter().enumerate() {
        let media_type = types.get(i).map(|s| s.as_str()).unwrap_or("image");
        items.push((path, media_type.to_owned()));
    }

    items
        .into_iter()
        .filter(|(_, media_type)| media_type.starts_with("image"))
        .map(|(path, _)| {
            let mime = mime_from_path(&path);
            let path_clone = path.clone();
            let fetcher: crate::channels::message::DataFetcher = Arc::new(move || {
                let p = path_clone.clone();
                Box::pin(async move { tokio::fs::read(&p).await.unwrap_or_default() })
            });
            MediaItem {
                media_type: MediaType::Image,
                mime_type: mime.to_owned(),
                filename: Some(path),
                data_fetcher: Some(fetcher),
            }
        })
        .collect()
}

fn mime_from_path(path: &str) -> &'static str {
    const MIME_MAP: &[(&str, &str)] = &[
        (".png", "image/png"),
        (".gif", "image/gif"),
        (".webp", "image/webp"),
    ];
    MIME_MAP
        .iter()
        .find(|(ext, _)| path.ends_with(ext))
        .map(|(_, mime)| *mime)
        .unwrap_or("image/jpeg")
}

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

struct ResolvedMedia {
    parts: Vec<Value>,
    errors: Vec<String>,
}

/// Resolve image `MediaItem`s into base64 content blocks, collecting errors for failures.
async fn resolve_image_media(media: &[MediaItem]) -> ResolvedMedia {
    let mut parts = Vec::new();
    let mut errors = Vec::new();
    for item in media {
        if item.media_type != MediaType::Image {
            continue;
        }
        let Some(ref fetcher) = item.data_fetcher else {
            continue;
        };
        let label = item.filename.as_deref().unwrap_or(&item.mime_type);
        let bytes = fetcher().await;
        if bytes.is_empty() {
            tracing::warn!(mime = %item.mime_type, "image fetch returned empty bytes, skipping");
            errors.push(format!("[Media download failed: {label}]"));
            continue;
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            tracing::warn!(
                size = bytes.len(),
                limit = MAX_IMAGE_BYTES,
                "image exceeds size limit, skipping"
            );
            errors.push(format!(
                "[Media too large ({:.1} MB): {label}]",
                bytes.len() as f64 / (1024.0 * 1024.0)
            ));
            continue;
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        parts.push(serde_json::json!({
            "type": "image_base64",
            "mime_type": item.mime_type,
            "data": b64,
        }));
    }
    ResolvedMedia { parts, errors }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::Arc;

    use crate::channels::message::DataFetcher;

    /// Build a test `MediaItem` with a `DataFetcher` returning the given bytes.
    fn image_item(mime: &str, bytes: Vec<u8>) -> MediaItem {
        let fetcher: DataFetcher = Arc::new(move || {
            let b = bytes.clone();
            Box::pin(async move { b }) as Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>
        });
        MediaItem {
            media_type: MediaType::Image,
            mime_type: mime.to_owned(),
            filename: None,
            data_fetcher: Some(fetcher),
        }
    }

    #[tokio::test]
    async fn resolve_image_happy_path() {
        let media = vec![image_item("image/jpeg", vec![0xFF, 0xD8, 0xFF])];
        let resolved = resolve_image_media(&media).await;
        assert_eq!(resolved.parts.len(), 1);
        assert!(resolved.errors.is_empty());
        assert_eq!(resolved.parts[0]["type"], "image_base64");
        assert_eq!(resolved.parts[0]["mime_type"], "image/jpeg");
        // Verify base64 round-trips.
        let b64 = resolved.parts[0]["data"].as_str().unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(decoded, vec![0xFF, 0xD8, 0xFF]);
    }

    #[tokio::test]
    async fn resolve_image_skips_non_image() {
        let audio = MediaItem {
            media_type: MediaType::Audio,
            mime_type: "audio/mpeg".to_owned(),
            filename: None,
            data_fetcher: Some(Arc::new(|| {
                Box::pin(async { vec![1u8, 2, 3] })
                    as Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>
            })),
        };
        let resolved = resolve_image_media(&[audio]).await;
        assert!(resolved.parts.is_empty());
        assert!(resolved.errors.is_empty());
    }

    #[tokio::test]
    async fn resolve_image_skips_empty_bytes() {
        let media = vec![image_item("image/png", vec![])];
        let resolved = resolve_image_media(&media).await;
        assert!(resolved.parts.is_empty());
        assert_eq!(resolved.errors.len(), 1);
        assert!(resolved.errors[0].contains("Media download failed"));
    }

    #[tokio::test]
    async fn resolve_image_skips_oversized() {
        let big = vec![0u8; MAX_IMAGE_BYTES + 1];
        let media = vec![image_item("image/png", big)];
        let resolved = resolve_image_media(&media).await;
        assert!(resolved.parts.is_empty());
        assert_eq!(resolved.errors.len(), 1);
        assert!(resolved.errors[0].contains("Media too large"));
    }

    #[tokio::test]
    async fn resolve_image_skips_no_fetcher() {
        let item = MediaItem {
            media_type: MediaType::Image,
            mime_type: "image/png".to_owned(),
            filename: None,
            data_fetcher: None,
        };
        let resolved = resolve_image_media(&[item]).await;
        assert!(resolved.parts.is_empty());
        assert!(resolved.errors.is_empty());
    }

    #[tokio::test]
    async fn resolve_image_multiple_mixed() {
        let media = vec![
            image_item("image/jpeg", vec![1, 2]),
            MediaItem {
                media_type: MediaType::Document,
                mime_type: "application/pdf".to_owned(),
                filename: None,
                data_fetcher: None,
            },
            image_item("image/png", vec![3, 4, 5]),
        ];
        let resolved = resolve_image_media(&media).await;
        assert_eq!(resolved.parts.len(), 2);
        assert_eq!(resolved.parts[0]["mime_type"], "image/jpeg");
        assert_eq!(resolved.parts[1]["mime_type"], "image/png");
    }

    #[tokio::test]
    async fn resolve_image_exactly_at_size_limit() {
        let exact = vec![0u8; MAX_IMAGE_BYTES];
        let media = vec![image_item("image/png", exact)];
        let resolved = resolve_image_media(&media).await;
        // Exactly at limit should be accepted (only > limit is rejected).
        assert_eq!(resolved.parts.len(), 1);
        assert!(resolved.errors.is_empty());
    }

    #[tokio::test]
    async fn resolve_image_preserves_mime_type() {
        let media = vec![
            image_item("image/webp", vec![1]),
            image_item("image/gif", vec![2]),
        ];
        let resolved = resolve_image_media(&media).await;
        assert_eq!(resolved.parts[0]["mime_type"], "image/webp");
        assert_eq!(resolved.parts[1]["mime_type"], "image/gif");
    }

    #[tokio::test]
    async fn resolve_image_mixed_with_one_oversized() {
        let media = vec![
            image_item("image/jpeg", vec![1, 2, 3]),
            image_item("image/png", vec![0u8; MAX_IMAGE_BYTES + 1]),
            image_item("image/gif", vec![4, 5]),
        ];
        let resolved = resolve_image_media(&media).await;
        // Only the oversized one should be skipped.
        assert_eq!(resolved.parts.len(), 2);
        assert_eq!(resolved.parts[0]["mime_type"], "image/jpeg");
        assert_eq!(resolved.parts[1]["mime_type"], "image/gif");
        assert_eq!(resolved.errors.len(), 1);
        assert!(resolved.errors[0].contains("Media too large"));
    }

    #[tokio::test]
    async fn resolve_image_empty_media_list() {
        let resolved = resolve_image_media(&[]).await;
        assert!(resolved.parts.is_empty());
        assert!(resolved.errors.is_empty());
    }

    #[test]
    fn reconstruct_context_media_accepts_image_mime_types() {
        let mut msg = ChannelMessage::new("session", "cli", "content");
        msg.context.insert(
            "media_paths".to_owned(),
            serde_json::json!(["/tmp/inbound.jpg", "/tmp/doc.pdf"]),
        );
        msg.context.insert(
            "media_types".to_owned(),
            serde_json::json!(["image/jpeg", "application/pdf"]),
        );

        let media = reconstruct_context_media(&msg);
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].media_type, MediaType::Image);
        assert_eq!(media[0].mime_type, "image/jpeg");
        assert_eq!(media[0].filename.as_deref(), Some("/tmp/inbound.jpg"));
    }

    #[tokio::test]
    async fn resolve_image_all_non_image_types() {
        let media = vec![
            MediaItem {
                media_type: MediaType::Audio,
                mime_type: "audio/mpeg".to_owned(),
                filename: None,
                data_fetcher: None,
            },
            MediaItem {
                media_type: MediaType::Video,
                mime_type: "video/mp4".to_owned(),
                filename: None,
                data_fetcher: None,
            },
            MediaItem {
                media_type: MediaType::Document,
                mime_type: "application/pdf".to_owned(),
                filename: None,
                data_fetcher: None,
            },
        ];
        let resolved = resolve_image_media(&media).await;
        assert!(resolved.parts.is_empty());
        assert!(resolved.errors.is_empty());
    }
}
