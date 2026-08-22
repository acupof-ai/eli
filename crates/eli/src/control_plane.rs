//! Control plane: turn-scoped context and budget infrastructure.
//!
//! The framework sets a [`TurnContext`] via task-local before each turn.
//! Agent internals read it transparently — no parameter threading needed.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use nexil::{CancellationToken, Tool};
use serde_json::Value;

/// Closure type for tool wrapping.
pub type WrapToolsFn = Arc<dyn Fn(Vec<Tool>) -> Vec<Tool> + Send + Sync>;

/// Closure type for mid-turn message dispatch.
/// Accepts an envelope and sends it to the user immediately.
pub type DispatchFn = Arc<dyn Fn(Value) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

// ---------------------------------------------------------------------------
// Task-local turn context
// ---------------------------------------------------------------------------

tokio::task_local! {
    static TURN_CTX: TurnContext;
}

/// Per-turn usage accumulator (input, output tokens).
#[derive(Clone, Default)]
pub struct TurnUsage {
    input: Arc<AtomicU64>,
    output: Arc<AtomicU64>,
    cache_read: Arc<AtomicU64>,
}

impl TurnUsage {
    pub fn record(&self, input_tokens: u64, output_tokens: u64, cache_read_tokens: u64) {
        self.input.fetch_add(input_tokens, Ordering::Relaxed);
        self.output.fetch_add(output_tokens, Ordering::Relaxed);
        self.cache_read.fetch_add(cache_read_tokens, Ordering::Relaxed);
    }

    pub fn input_tokens(&self) -> u64 {
        self.input.load(Ordering::Relaxed)
    }

    pub fn output_tokens(&self) -> u64 {
        self.output.load(Ordering::Relaxed)
    }

    pub fn cache_read_tokens(&self) -> u64 {
        self.cache_read.load(Ordering::Relaxed)
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens() + self.output_tokens()
    }
}

/// A media item to be sent outbound alongside the text reply.
#[derive(Debug, Clone)]
pub struct OutboundMedia {
    pub path: String,
    /// High-level type: "image", "audio", "video", "document".
    pub media_type: String,
    /// MIME type, e.g. "image/png".
    pub mime_type: String,
}

/// Per-turn context set by the framework, read by agent internals.
pub struct TurnContext {
    pub cancellation: CancellationToken,
    pub wrap_tools: Option<WrapToolsFn>,
    pub usage: TurnUsage,
    /// Accumulator for events that `save_state` hooks flush to tape after the turn.
    pub save_events: Arc<Mutex<Vec<(String, Value)>>>,
    /// Optional callback to dispatch a message to the user mid-turn.
    pub dispatch: Option<DispatchFn>,
    /// Media items accumulated during the turn for outbound delivery.
    pub outbound_media: Arc<Mutex<Vec<OutboundMedia>>>,
    /// Optional streaming sink: the agent forwards model prose deltas here as
    /// they are generated (JSON chat mode drains them as `text_delta` events).
    pub text_sink: Option<tokio::sync::mpsc::Sender<nexil::llm::StreamChunk>>,
}

/// Run `fut` with the given [`TurnContext`] bound to the current task.
pub async fn with_turn_context<F, T>(ctx: TurnContext, fut: F) -> T
where
    F: Future<Output = T>,
{
    TURN_CTX.scope(ctx, fut).await
}

/// Read the cancellation token from the current turn, if any.
pub fn turn_cancellation() -> Option<CancellationToken> {
    TURN_CTX.try_with(|ctx| ctx.cancellation.clone()).ok()
}

/// Read the usage accumulator from the current turn, if any.
pub fn turn_usage() -> Option<TurnUsage> {
    TURN_CTX.try_with(|ctx| ctx.usage.clone()).ok()
}

/// Record token usage into the current turn context.
pub fn record_turn_usage(input_tokens: u64, output_tokens: u64, cache_read_tokens: u64) {
    let _ = TURN_CTX.try_with(|ctx| ctx.usage.record(input_tokens, output_tokens, cache_read_tokens));
}

/// Push a save event into the current turn context.
///
/// Events are accumulated during the turn and flushed by the `save_state` hook.
pub fn push_save_event(name: &str, data: Value) {
    let _ = TURN_CTX.try_with(|ctx| {
        ctx.save_events.lock().push((name.to_owned(), data));
    });
}

/// Drain all accumulated save events from the current turn context.
pub fn drain_save_events() -> Vec<(String, Value)> {
    TURN_CTX
        .try_with(|ctx| std::mem::take(&mut *ctx.save_events.lock()))
        .unwrap_or_default()
}

/// Dispatch a message to the user mid-turn (e.g. from a tool handler).
/// Returns immediately if no dispatch function is set.
pub async fn dispatch_mid_turn(envelope: Value) {
    let dispatch = TURN_CTX.try_with(|ctx| ctx.dispatch.clone()).ok().flatten();
    if let Some(f) = dispatch {
        f(envelope).await;
    }
}

/// Push a media item for outbound delivery in the current turn.
pub fn push_outbound_media(media: OutboundMedia) {
    let _ = TURN_CTX.try_with(|ctx| {
        ctx.outbound_media.lock().push(media);
    });
}

/// Drain all accumulated outbound media from the current turn context.
pub fn drain_outbound_media() -> Vec<OutboundMedia> {
    TURN_CTX
        .try_with(|ctx| std::mem::take(&mut *ctx.outbound_media.lock()))
        .unwrap_or_default()
}

/// Infer MIME type from a file extension.
pub fn mime_from_extension(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Derive high-level media type from a MIME string.
pub fn media_type_from_mime(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("video/") {
        "video"
    } else if mime.starts_with("audio/") {
        "audio"
    } else {
        "document"
    }
}

/// Read the tool-wrapping function from the current turn, if any.
pub fn turn_wrap_tools() -> Option<WrapToolsFn> {
    TURN_CTX
        .try_with(|ctx| ctx.wrap_tools.clone())
        .ok()
        .flatten()
}

// ---------------------------------------------------------------------------
// Streaming text sink (JSON chat mode)
// ---------------------------------------------------------------------------

/// Process-wide slot holding the sink for the next turn. JSON chat installs it
/// before `process_inbound`; the framework moves it into that turn's
/// [`TurnContext`] (taking it out, so it never leaks into a later turn).
static TEXT_SINK: std::sync::LazyLock<
    Mutex<Option<tokio::sync::mpsc::Sender<nexil::llm::StreamChunk>>>,
> = std::sync::LazyLock::new(|| Mutex::new(None));

/// Install (or clear) the streaming text sink for the next turn.
pub fn set_text_sink(sink: Option<tokio::sync::mpsc::Sender<nexil::llm::StreamChunk>>) {
    *TEXT_SINK.lock() = sink;
}

/// Take the installed sink into a turn. Leaves the slot empty.
pub fn take_text_sink() -> Option<tokio::sync::mpsc::Sender<nexil::llm::StreamChunk>> {
    TEXT_SINK.lock().take()
}

/// Clone the current turn's text sink, if one was installed.
pub fn turn_text_sink() -> Option<tokio::sync::mpsc::Sender<nexil::llm::StreamChunk>> {
    TURN_CTX
        .try_with(|ctx| ctx.text_sink.clone())
        .ok()
        .flatten()
}

// ---------------------------------------------------------------------------
// Inbound injection (subagent results, synthetic messages)
// ---------------------------------------------------------------------------

/// Closure type for injecting a synthetic inbound message into the framework
/// pipeline. Set once at startup by the chat/gateway entry point.
pub type InjectInboundFn =
    Arc<dyn Fn(Value) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

static INBOUND_INJECTOR: std::sync::LazyLock<Mutex<Option<InjectInboundFn>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// Register the global inbound injector. Called once at startup by the
/// chat/gateway entry point.
///
/// There is a single global slot, so a second registrant silently reroutes all
/// background result delivery (e.g. async subagent completions) through whatever
/// transport registered last. Today chat and gateway are mutually-exclusive
/// subcommands, so this never fires — but if a second inbound source ever shares
/// the process it is a latent foot-gun. We warn-and-overwrite (rather than
/// refuse) so the currently-working last-write-wins behaviour is preserved while
/// the collision is surfaced loudly; a true source-keyed registry is deferred to
/// the multi-source (Phase 3) work.
pub fn set_inbound_injector(f: InjectInboundFn) {
    let mut slot = INBOUND_INJECTOR.lock();
    if slot.is_some() {
        tracing::warn!(
            "inbound injector already registered — overwriting. If two inbound \
             sources are active in one process, background result delivery may \
             route through the wrong transport."
        );
    }
    *slot = Some(f);
}

/// Clone the current inbound injector, if set.
pub fn inbound_injector() -> Option<InjectInboundFn> {
    INBOUND_INJECTOR.lock().clone()
}

/// Inject a synthetic inbound envelope into the framework pipeline.
/// No-op if no injector has been registered.
pub async fn inject_inbound(envelope: Value) {
    if let Some(f) = inbound_injector() {
        f(envelope).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn turn_context_propagates() {
        let token = CancellationToken::new();
        let token2 = token.clone();
        let ctx = TurnContext {
            cancellation: token,
            wrap_tools: None,
            usage: Default::default(),
            save_events: Default::default(),
            dispatch: None,
            outbound_media: Default::default(),
            text_sink: None,
        };
        let result = with_turn_context(ctx, async {
            let t = turn_cancellation().unwrap();
            assert!(!t.is_cancelled());
            token2.cancel();
            assert!(t.is_cancelled());
            true
        })
        .await;
        assert!(result);
    }

    #[tokio::test]
    async fn turn_context_absent_returns_none() {
        assert!(turn_cancellation().is_none());
        assert!(turn_wrap_tools().is_none());
    }

    #[tokio::test]
    async fn outbound_media_push_and_drain() {
        let ctx = TurnContext {
            cancellation: CancellationToken::new(),
            wrap_tools: None,
            usage: Default::default(),
            save_events: Default::default(),
            dispatch: None,
            outbound_media: Default::default(),
            text_sink: None,
        };
        with_turn_context(ctx, async {
            push_outbound_media(OutboundMedia {
                path: "/tmp/a.png".into(),
                media_type: "image".into(),
                mime_type: "image/png".into(),
            });
            push_outbound_media(OutboundMedia {
                path: "/tmp/b.mp4".into(),
                media_type: "video".into(),
                mime_type: "video/mp4".into(),
            });

            let drained = drain_outbound_media();
            assert_eq!(drained.len(), 2);
            assert_eq!(drained[0].path, "/tmp/a.png");
            assert_eq!(drained[1].path, "/tmp/b.mp4");

            // Drain again — should be empty.
            assert!(drain_outbound_media().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn outbound_media_drain_without_context_returns_empty() {
        // No TurnContext set — drain should safely return empty vec.
        assert!(drain_outbound_media().is_empty());
    }

    #[tokio::test]
    async fn inject_inbound_noop_without_injector() {
        // Should not panic when no injector is registered.
        inject_inbound(serde_json::json!({"content": "test"})).await;
    }

    #[test]
    fn mime_from_extension_known_types() {
        assert_eq!(
            mime_from_extension(std::path::Path::new("/tmp/photo.png")),
            "image/png"
        );
        assert_eq!(
            mime_from_extension(std::path::Path::new("file.jpg")),
            "image/jpeg"
        );
        assert_eq!(
            mime_from_extension(std::path::Path::new("video.mp4")),
            "video/mp4"
        );
        assert_eq!(
            mime_from_extension(std::path::Path::new("doc.pdf")),
            "application/pdf"
        );
    }

    #[test]
    fn mime_from_extension_unknown_fallback() {
        assert_eq!(
            mime_from_extension(std::path::Path::new("file.xyz")),
            "application/octet-stream"
        );
        assert_eq!(
            mime_from_extension(std::path::Path::new("noext")),
            "application/octet-stream"
        );
    }

    #[test]
    fn media_type_from_mime_categorizes() {
        assert_eq!(media_type_from_mime("image/png"), "image");
        assert_eq!(media_type_from_mime("image/jpeg"), "image");
        assert_eq!(media_type_from_mime("video/mp4"), "video");
        assert_eq!(media_type_from_mime("audio/mpeg"), "audio");
        assert_eq!(media_type_from_mime("application/pdf"), "document");
        assert_eq!(media_type_from_mime("application/octet-stream"), "document");
    }
}
