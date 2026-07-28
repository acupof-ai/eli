//! Media helpers shared by native channels — media-type parsing, MIME
//! defaults, on-demand byte fetchers, and URL→temp-file download.
//!
//! Ported from the deleted `channels/webhook.rs` (media helpers) and
//! `sidecar/plugins/telegram.ts` (download) at commit `ecfa7c3^`, generalized
//! off the sidecar contract types.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use tracing::warn;

use super::message::{DataFetcher, MediaType};

/// Parse a free-form media-type string into a [`MediaType`]. Trims and
/// lowercases first; unknown non-empty values warn and return `None`.
pub fn parse_media_type(value: &str) -> Option<MediaType> {
    match value.trim().to_ascii_lowercase().as_str() {
        "image" | "img" | "photo" | "picture" | "sticker" => Some(MediaType::Image),
        "audio" | "voice" => Some(MediaType::Audio),
        "video" => Some(MediaType::Video),
        "document" | "doc" | "file" => Some(MediaType::Document),
        other => {
            if !other.is_empty() {
                warn!(media_type = other, "media: unsupported inbound media type");
            }
            None
        }
    }
}

/// Default MIME string for a media type.
pub fn default_mime_type(media_type: MediaType) -> &'static str {
    match media_type {
        MediaType::Image => "image/jpeg",
        MediaType::Audio => "audio/mpeg",
        MediaType::Video => "video/mp4",
        MediaType::Document => "application/octet-stream",
    }
}

/// Effective MIME for an inbound item: explicit non-empty MIME wins, else the
/// type default. (No trim — whitespace-only is treated as non-empty, matching
/// the original.)
pub fn inbound_mime_type(explicit_mime: &str, media_type: MediaType) -> String {
    if !explicit_mime.is_empty() {
        explicit_mime.to_owned()
    } else {
        default_mime_type(media_type).to_owned()
    }
}

/// Build a [`DataFetcher`] yielding the decoded base64 bytes, or `None` if the
/// data is invalid base64. Callers must filter empty strings before calling.
pub fn base64_data_fetcher(data: &str) -> Option<DataFetcher> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| warn!(%error, "media: invalid inbound media base64"))
        .ok()?;
    Some(Arc::new(move || {
        let bytes = bytes.clone();
        Box::pin(async move { bytes })
    }))
}

/// Build a [`DataFetcher`] that lazily reads `path` on each call; read errors
/// warn and yield empty bytes (never panic).
pub fn path_data_fetcher(path: &str) -> DataFetcher {
    let path = path.to_owned();
    Arc::new(move || {
        let path = path.clone();
        Box::pin(async move {
            tokio::fs::read(&path).await.unwrap_or_else(|error| {
                warn!(%error, path = %path, "media: failed to read media file");
                Vec::new()
            })
        })
    })
}

/// Precedence: non-empty base64 first, else non-empty path. `None` when neither
/// yields a fetcher (caller drops the item).
pub fn build_inbound_fetcher(data_base64: Option<&str>, path: Option<&str>) -> Option<DataFetcher> {
    data_base64
        .filter(|data| !data.is_empty())
        .and_then(base64_data_fetcher)
        .or_else(|| path.filter(|path| !path.is_empty()).map(path_data_fetcher))
}

fn media_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Download `url` to a persistent temp file with suffix `ext` (verbatim,
/// including a leading dot; `""` for no extension). The file is NOT
/// delete-on-drop — the returned path is handed to the turn pipeline.
pub async fn download_to_temp(url: &str, ext: &str) -> anyhow::Result<PathBuf> {
    use std::io::Write;

    let resp = media_http_client().get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("download failed: {}", resp.status());
    }
    let bytes = resp.bytes().await?;

    let mut tmp = tempfile::Builder::new()
        .prefix("eli-media-")
        .suffix(ext)
        .tempfile()?;
    tmp.as_file_mut().write_all(&bytes)?;
    let (_file, path) = tmp.keep()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_media_type_aliases_and_case() {
        for s in [
            "IMAGE", "Image", "image", " img ", "photo", "picture", "sticker",
        ] {
            assert_eq!(parse_media_type(s), Some(MediaType::Image), "{s}");
        }
        assert_eq!(parse_media_type("audio"), Some(MediaType::Audio));
        assert_eq!(parse_media_type("voice"), Some(MediaType::Audio));
        assert_eq!(parse_media_type("Video"), Some(MediaType::Video));
        for s in ["document", "DOC", "File"] {
            assert_eq!(parse_media_type(s), Some(MediaType::Document), "{s}");
        }
    }

    #[test]
    fn parse_media_type_unknown_and_empty() {
        assert_eq!(parse_media_type("binary"), None);
        assert_eq!(parse_media_type("unknown"), None);
        assert_eq!(parse_media_type(""), None);
    }

    #[test]
    fn default_mime_type_mapping() {
        assert_eq!(default_mime_type(MediaType::Image), "image/jpeg");
        assert_eq!(default_mime_type(MediaType::Audio), "audio/mpeg");
        assert_eq!(default_mime_type(MediaType::Video), "video/mp4");
        assert_eq!(
            default_mime_type(MediaType::Document),
            "application/octet-stream"
        );
    }

    #[test]
    fn inbound_mime_type_explicit_wins_else_default() {
        assert_eq!(
            inbound_mime_type("image/webp", MediaType::Image),
            "image/webp"
        );
        assert_eq!(inbound_mime_type("", MediaType::Audio), "audio/mpeg");
        assert_eq!(inbound_mime_type("", MediaType::Image), "image/jpeg");
    }

    #[tokio::test]
    async fn base64_fetcher_decodes() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        let f = base64_data_fetcher(&b64).unwrap();
        assert_eq!(f().await, vec![1, 2, 3, 4]);
    }

    #[test]
    fn base64_fetcher_invalid_returns_none() {
        assert!(base64_data_fetcher("not-valid-base64!!!").is_none());
    }

    #[tokio::test]
    async fn path_fetcher_reads_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), [9u8, 8, 7]).unwrap();
        let f = path_data_fetcher(&tmp.path().to_string_lossy());
        assert_eq!(f().await, vec![9, 8, 7]);
    }

    #[tokio::test]
    async fn path_fetcher_missing_file_empty() {
        let f = path_data_fetcher("/tmp/nonexistent_test_file_12345.png");
        assert!(f().await.is_empty());
    }

    #[tokio::test]
    async fn build_inbound_fetcher_precedence() {
        // base64 present + valid, no path
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        let f = build_inbound_fetcher(Some(&b64), None).unwrap();
        assert_eq!(f().await, vec![1, 2, 3, 4]);

        // empty base64, path present
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), [9u8, 8, 7]).unwrap();
        let path = tmp.path().to_string_lossy().to_string();
        let f = build_inbound_fetcher(Some(""), Some(&path)).unwrap();
        assert_eq!(f().await, vec![9, 8, 7]);

        // invalid base64, no path -> dropped
        assert!(build_inbound_fetcher(Some("not-valid-base64!!!"), None).is_none());
        // neither -> dropped
        assert!(build_inbound_fetcher(None, None).is_none());
    }
}
