#![cfg_attr(docsrs, feature(doc_auto_cfg))]
//! Hook-first AI agent framework with multi-channel support.
//!
//! Eli processes every inbound message through a linear hook pipeline:
//!
//! ```text
//! resolve_session → load_state → build_prompt → run_model
//!     → save_state → render_outbound → dispatch_outbound
//! ```
//!
//! Each stage is a method on [`EliHookSpec`]. Builtins register first;
//! user plugins register after, and **last-registered wins** for chain-aborting hooks.
//!
//! # Feature flags
//!
//! | Flag | Default | Description |
//! |------|---------|-------------|
//! | `gateway` | yes | HTTP gateway channel via axum |
//! | `tape-viewer` | yes | Web UI for inspecting tape history |
//!
//! # Key types
//!
//! - [`EliFramework`] — Minimal framework core; everything grows from hook plugins.
//! - [`EliHookSpec`] — Trait defining all hook points in the turn pipeline.
//! - [`Channel`](channels::base::Channel) — Transport-level channel trait for inbound/outbound I/O.

pub mod builtin;
pub mod channels;
pub mod control_plane;
pub mod envelope;
pub mod evolution;
pub mod framework;
pub mod hooks;
pub mod prompt_builder;
pub mod skills;
pub mod smart_router;
pub mod taskboard;
pub mod tool_middleware;
pub mod tools;
pub mod types;

// Re-export key types at the crate root for convenience.
pub use framework::EliFramework;
pub use hooks::{ChannelHook, EliHookSpec, HookError, HookPoint, HookRuntime, TapeStoreKind};
pub use types::{Envelope, MessageHandler, PromptValue, State, TurnResult, TurnUsageInfo};
