//! Local inference backend auto-detection.
//!
//! Probes a short ordered list of candidate URLs for an OpenAI-compatible
//! `/v1/models` endpoint. Used by `eli login local` (and brand aliases:
//! `agent-infer`, `ollama`, `vllm`, `lmstudio`, `llama-cpp`) to locate a
//! running local server without flags.
//!
//! Default candidate ports cover the canonical defaults of the major local
//! servers; users can extend the list via `ELI_LOCAL_PORTS=8090,9000` or
//! pin a single endpoint via `ELI_LOCAL_URL=http://host:port` (which is
//! exclusive — explicit config never silently falls back to defaults).
//! `AGENT_INFER_URL` is honored as a back-compat alias of `ELI_LOCAL_URL`.

use std::time::Duration;

use serde::Deserialize;

/// Built-in candidate ports for autodetection. Order matters — first hit wins.
///
/// 8000  — agent-infer / vllm default
/// 8012  — agent-infer Metal alt port
/// 8080  — llama.cpp `server` default
/// 11434 — ollama default
/// 1234  — lmstudio default
const DEFAULT_LOCAL_PORTS: &[u16] = &[8000, 8012, 8080, 11434, 1234];

/// Outcome of a successful probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetectResult {
    /// Base URL (without trailing slash), e.g. `http://127.0.0.1:8000/v1`.
    pub api_base: String,
    /// First model id reported by the server's `/v1/models` endpoint.
    pub model_id: String,
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// Build the ordered list of candidate base URLs for local-backend detection.
///
/// Precedence:
///   1. `$ELI_LOCAL_URL` (or legacy `$AGENT_INFER_URL`) — exclusive when set.
///   2. Built-in default ports + any extras from `$ELI_LOCAL_PORTS` (a
///      comma-separated list of u16, applied additively after defaults).
///
/// Candidates are normalized to always end in `/v1` (the path `/v1/models`
/// is appended by the probe).
pub(crate) fn local_candidates() -> Vec<String> {
    if let Some(url) = exclusive_url_override() {
        return vec![normalize_base(&url)];
    }

    let mut ports: Vec<u16> = DEFAULT_LOCAL_PORTS.to_vec();
    for extra in extra_ports_from_env() {
        if !ports.contains(&extra) {
            ports.push(extra);
        }
    }
    ports
        .into_iter()
        .map(|p| format!("http://127.0.0.1:{p}/v1"))
        .collect()
}

fn exclusive_url_override() -> Option<String> {
    for var in ["ELI_LOCAL_URL", "AGENT_INFER_URL"] {
        if let Ok(url) = std::env::var(var) {
            let url = url.trim();
            if !url.is_empty() {
                return Some(url.to_owned());
            }
        }
    }
    None
}

fn extra_ports_from_env() -> Vec<u16> {
    std::env::var("ELI_LOCAL_PORTS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|s| s.trim().parse::<u16>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_base(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/v1")
    }
}

/// Parse a `/v1/models` response body, returning the first model id.
pub(crate) fn parse_first_model_id(body: &str) -> Option<String> {
    let parsed: ModelsResponse = serde_json::from_str(body).ok()?;
    parsed
        .data
        .into_iter()
        .map(|m| m.id.trim().to_owned())
        .find(|id| !id.is_empty())
}

/// Probe a single candidate base URL.
///
/// Returns `Some(DetectResult)` on HTTP 200 with at least one model id,
/// `None` on any failure (connection refused, timeout, malformed JSON,
/// empty model list).
pub(crate) async fn probe(api_base: &str) -> Option<DetectResult> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(1))
        .build()
        .ok()?;
    let url = format!("{api_base}/models");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let model_id = parse_first_model_id(&body)?;
    Some(DetectResult {
        api_base: api_base.to_owned(),
        model_id,
    })
}

/// Probe all local candidates in order; return the first that responds.
pub(crate) async fn detect_local() -> Option<DetectResult> {
    for candidate in local_candidates() {
        if let Some(hit) = probe(&candidate).await {
            return Some(hit);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    fn local_env_guard() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap()
    }

    fn clear_local_env() {
        unsafe {
            std::env::remove_var("ELI_LOCAL_URL");
            std::env::remove_var("AGENT_INFER_URL");
            std::env::remove_var("ELI_LOCAL_PORTS");
        }
    }

    #[test]
    fn parse_extracts_first_model_id() {
        let body = r#"{"object":"list","data":[
            {"id":"mlx-community/Qwen3-0.6B-4bit","object":"model"},
            {"id":"second/model","object":"model"}
        ]}"#;
        assert_eq!(
            parse_first_model_id(body).as_deref(),
            Some("mlx-community/Qwen3-0.6B-4bit")
        );
    }

    #[test]
    fn parse_returns_none_on_empty_list() {
        let body = r#"{"object":"list","data":[]}"#;
        assert!(parse_first_model_id(body).is_none());
    }

    #[test]
    fn parse_returns_none_on_malformed_json() {
        assert!(parse_first_model_id("not json").is_none());
        assert!(parse_first_model_id("{").is_none());
    }

    #[test]
    fn parse_skips_empty_ids() {
        let body = r#"{"data":[{"id":""},{"id":"real-model"}]}"#;
        assert_eq!(parse_first_model_id(body).as_deref(), Some("real-model"));
    }

    #[test]
    fn candidates_use_default_ports_without_env() {
        let _guard = local_env_guard();
        clear_local_env();
        let candidates = local_candidates();
        assert_eq!(
            candidates,
            vec![
                "http://127.0.0.1:8000/v1".to_owned(),
                "http://127.0.0.1:8012/v1".to_owned(),
                "http://127.0.0.1:8080/v1".to_owned(),
                "http://127.0.0.1:11434/v1".to_owned(),
                "http://127.0.0.1:1234/v1".to_owned(),
            ]
        );
    }

    #[test]
    fn normalize_appends_v1_when_missing() {
        assert_eq!(normalize_base("http://host:9000"), "http://host:9000/v1");
        assert_eq!(normalize_base("http://host:9000/"), "http://host:9000/v1");
        assert_eq!(normalize_base("http://host:9000/v1"), "http://host:9000/v1");
        assert_eq!(
            normalize_base("http://host:9000/v1/"),
            "http://host:9000/v1"
        );
    }

    #[test]
    fn candidates_local_url_env_is_exclusive() {
        let _guard = local_env_guard();
        clear_local_env();
        unsafe {
            std::env::set_var("ELI_LOCAL_URL", "http://explicit-host:9000");
        }
        let candidates = local_candidates();
        clear_local_env();
        assert_eq!(candidates, vec!["http://explicit-host:9000/v1".to_owned()]);
    }

    #[test]
    fn candidates_legacy_agent_infer_url_still_works() {
        let _guard = local_env_guard();
        clear_local_env();
        unsafe {
            std::env::set_var("AGENT_INFER_URL", "http://legacy-host:7000");
        }
        let candidates = local_candidates();
        clear_local_env();
        assert_eq!(candidates, vec!["http://legacy-host:7000/v1".to_owned()]);
    }

    #[test]
    fn candidates_extra_ports_appended_after_defaults() {
        let _guard = local_env_guard();
        clear_local_env();
        unsafe {
            std::env::set_var("ELI_LOCAL_PORTS", "8000,9090, 7000 ,bogus,1234");
        }
        let candidates = local_candidates();
        clear_local_env();
        // 8000 and 1234 already in defaults → not duplicated; bogus dropped;
        // 9090 and 7000 appended in order.
        assert!(candidates.contains(&"http://127.0.0.1:9090/v1".to_owned()));
        assert!(candidates.contains(&"http://127.0.0.1:7000/v1".to_owned()));
        let port_count = candidates.iter().filter(|c| c.contains(":8000/")).count();
        assert_eq!(port_count, 1, "8000 should not be duplicated");
    }

    #[tokio::test]
    async fn probe_returns_none_on_connection_refused() {
        // Port 1 is privileged and unbound in standard environments; reqwest
        // fails fast. Timeout still bounds the wait at 1s regardless.
        assert!(probe("http://127.0.0.1:1/v1").await.is_none());
    }
}
