//! Channel trait — the abstract interface every channel must implement.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::message::ChannelMessage;

/// A communication channel (CLI, Telegram, etc.) that can send and receive
/// [`ChannelMessage`] values.
///
/// All channels are required to be `Send + Sync` so they can be shared across
/// tokio tasks.
#[async_trait]
pub trait Channel: Send + Sync {
    /// Human-readable name used as a key in the channel registry.
    fn name(&self) -> &str;

    /// Start listening for events and dispatching to the registered handler.
    ///
    /// The implementation should respect `cancel` and return / stop spawned
    /// tasks once it is cancelled.
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()>;

    /// Gracefully shut down the channel and release resources.
    async fn stop(&self) -> anyhow::Result<()>;

    /// Send an outbound message through this channel.
    ///
    /// The default implementation is a no-op — channels that only *receive*
    /// messages do not need to override this.
    async fn send(&self, _message: ChannelMessage) -> anyhow::Result<()> {
        Ok(())
    }
}
