//! Channel subsystem — pluggable transports for user interaction.
//!
//! Each channel implements the [`Channel`] trait. Inbound messages are
//! dispatched to the framework's hook pipeline; outbound envelopes are
//! sent through the channel that originated the session.

pub mod base;
pub mod cli;
pub mod feishu;
pub mod media;
pub mod message;
pub mod telegram;
pub mod text;

pub use base::Channel;
pub use cli::{CliChannel, CliRenderer};
pub use feishu::{FeishuChannel, FeishuSettings};
pub use message::{ChannelMessage, DataFetcher, MediaItem, MediaType, MessageKind};
pub use telegram::{TelegramChannel, TelegramSettings};
