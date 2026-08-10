//! Tape manager helpers for Conduit.

use serde_json::Value;

use crate::core::errors::ConduitError;
use crate::core::results::ErrorPayload;
use crate::tape::context::{TapeContext, build_messages};
use crate::tape::entries::{TapeEntry, latest_system_content};
use crate::tape::query::TapeQuery;
use crate::tape::store::{AsyncTapeStore, AsyncTapeStoreAdapter, InMemoryTapeStore, TapeStore};

fn run_event_data(
    error: Option<&ErrorPayload>,
    usage: Option<Value>,
    provider: Option<&str>,
    model: Option<&str>,
) -> Value {
    let mut data = serde_json::Map::new();
    let status = if error.is_some() { "error" } else { "ok" };
    data.insert("status".into(), Value::String(status.into()));
    if let Some(u) = usage {
        data.insert("usage".into(), u);
    }
    if let Some(p) = provider
        && !p.is_empty()
    {
        data.insert("provider".into(), Value::String(p.into()));
    }
    if let Some(m) = model
        && !m.is_empty()
    {
        data.insert("model".into(), Value::String(m.into()));
    }
    Value::Object(data)
}

#[allow(clippy::too_many_arguments)]
fn build_chat_entries(
    meta: &Value,
    context_error: Option<&ErrorPayload>,
    new_messages: &[Value],
    tool_calls: Option<&[Value]>,
    response_text: Option<&str>,
    tool_results: Option<&[Value]>,
    error: Option<&ErrorPayload>,
    usage: Option<Value>,
    provider: Option<&str>,
    model: Option<&str>,
) -> Vec<TapeEntry> {
    let mut entries: Vec<TapeEntry> = context_error
        .iter()
        .map(|ce| TapeEntry::error(ce, meta.clone()))
        .chain(
            new_messages
                .iter()
                .map(|msg| TapeEntry::message(msg.clone(), meta.clone())),
        )
        .collect();

    if let Some(tc) = tool_calls
        && !tc.is_empty()
    {
        entries.push(TapeEntry::tool_call(tc.to_vec(), meta.clone()));
    }
    if let Some(rt) = response_text {
        let msg = serde_json::json!({ "role": "assistant", "content": rt });
        entries.push(TapeEntry::message(msg, meta.clone()));
    }
    if let Some(tr) = tool_results {
        entries.push(TapeEntry::tool_result(tr.to_vec(), meta.clone()));
    }
    if let Some(err) = error
        && !context_error.is_some_and(|ce| ce == err)
    {
        entries.push(TapeEntry::error(err, meta.clone()));
    }
    entries.push(TapeEntry::event(
        "run",
        Some(run_event_data(error, usage, provider, model)),
        meta.clone(),
    ));
    entries
}

// ---------------------------------------------------------------------------
// AsyncTapeManager
// ---------------------------------------------------------------------------

/// Async tape manager for async chat and tool-call paths.
pub struct AsyncTapeManager {
    store: Box<dyn AsyncTapeStore>,
    default_context: TapeContext,
}

impl AsyncTapeManager {
    /// Create a new AsyncTapeManager. Accepts either an AsyncTapeStore or a sync TapeStore
    /// (which will be wrapped in AsyncTapeStoreAdapter).
    pub fn new(
        store: Option<Box<dyn AsyncTapeStore>>,
        default_context: Option<TapeContext>,
    ) -> Self {
        Self {
            store: store
                .unwrap_or_else(|| Box::new(AsyncTapeStoreAdapter::new(InMemoryTapeStore::new()))),
            default_context: default_context.unwrap_or_default(),
        }
    }

    /// Create from a sync TapeStore, wrapping it automatically.
    pub fn from_sync_store<S: TapeStore + 'static>(
        store: S,
        default_context: Option<TapeContext>,
    ) -> Self {
        Self {
            store: Box::new(AsyncTapeStoreAdapter::new(store)),
            default_context: default_context.unwrap_or_default(),
        }
    }

    /// Get a reference to the default context.
    pub fn default_context(&self) -> &TapeContext {
        &self.default_context
    }

    /// Set the default context.
    pub fn set_default_context(&mut self, ctx: TapeContext) {
        self.default_context = ctx;
    }

    /// Create a TapeQuery for the given tape.
    pub fn query_tape(&self, tape: &str) -> TapeQuery {
        TapeQuery::new(tape)
    }

    /// List all tape names.
    pub async fn list_tapes(&self) -> Result<Vec<String>, ConduitError> {
        self.store.list_tapes().await
    }

    /// Read messages from a tape, applying the given or default context.
    pub async fn read_messages(
        &self,
        tape: &str,
        context: Option<&TapeContext>,
    ) -> Result<Vec<Value>, ConduitError> {
        let active_context = context.unwrap_or(&self.default_context);
        let query = self.query_tape(tape);
        let query = active_context.build_query(query);
        let entries = self.store.fetch_all(&query).await?;
        Ok(build_messages(&entries, active_context))
    }

    /// Fetch raw tape entries matching a query.
    pub async fn fetch_entries(&self, query: &TapeQuery) -> Result<Vec<TapeEntry>, ConduitError> {
        self.store.fetch_all(query).await
    }

    /// Append an entry to a tape.
    pub async fn append_entry(&self, tape: &str, entry: &TapeEntry) -> Result<(), ConduitError> {
        self.store.append(tape, entry).await
    }

    /// Reset (delete) a tape.
    pub async fn reset_tape(&self, tape: &str) -> Result<(), ConduitError> {
        self.store.reset(tape).await
    }

    /// Record a handoff: creates an anchor and handoff event, appends both.
    pub async fn handoff(
        &self,
        tape: &str,
        name: &str,
        state: Option<Value>,
        meta: Value,
    ) -> Result<Vec<TapeEntry>, ConduitError> {
        let entry = TapeEntry::anchor(name, state.clone(), meta.clone());
        let handoff_data = serde_json::json!({
            "name": name,
            "state": state.clone().unwrap_or_else(|| serde_json::json!({})),
        });
        let event = TapeEntry::event("handoff", Some(handoff_data), meta);
        self.store.append(tape, &entry).await?;
        self.store.append(tape, &event).await?;
        Ok(vec![entry, event])
    }

    /// Append a system entry only if the tape has no system entry or the content changed.
    /// Returns true if the entry was written.
    pub async fn append_system_if_changed(
        &self,
        tape: &str,
        content: &str,
        meta: Value,
    ) -> Result<bool, ConduitError> {
        let query = self.query_tape(tape);
        let dominated = self
            .store
            .fetch_all(&query)
            .await
            .ok()
            .and_then(|entries| latest_system_content(&entries).map(|s| s == content))
            .unwrap_or(false);
        if dominated {
            return Ok(false);
        }
        self.store
            .append(tape, &TapeEntry::system(content, meta))
            .await?;
        Ok(true)
    }

    /// Record a complete chat turn to the tape.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_chat(
        &self,
        tape: &str,
        run_id: &str,
        system_prompt: Option<&str>,
        context_error: Option<&ErrorPayload>,
        new_messages: &[Value],
        response_text: Option<&str>,
        tool_calls: Option<&[Value]>,
        tool_results: Option<&[Value]>,
        error: Option<&ErrorPayload>,
        usage: Option<Value>,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), ConduitError> {
        let meta = serde_json::json!({ "run_id": run_id });
        if let Some(sp) = system_prompt
            && !sp.is_empty()
        {
            self.append_system_if_changed(tape, sp, meta.clone())
                .await?;
        }
        for entry in build_chat_entries(
            &meta,
            context_error,
            new_messages,
            tool_calls,
            response_text,
            tool_results,
            error,
            usage,
            provider,
            model,
        ) {
            self.store.append(tape, &entry).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::entries::TapeEntryKind;
    use crate::tape::store::InMemoryTapeStore;
    use serde_json::json;
    #[tokio::test]
    async fn test_async_record_chat_skips_duplicate_system_prompt() {
        let store = InMemoryTapeStore::new();
        let manager = AsyncTapeManager::new(
            Some(Box::new(AsyncTapeStoreAdapter::new(store.clone()))),
            None,
        );
        let tape = "async-record-chat-dedup";

        manager
            .record_chat(
                tape,
                "run-1",
                Some("system prompt"),
                None,
                &[json!({"role": "user", "content": "hello"})],
                Some("hi"),
                None,
                None,
                None,
                None,
                Some("openai"),
                Some("gpt-4o"),
            )
            .await
            .unwrap();

        manager
            .record_chat(
                tape,
                "run-2",
                Some("system prompt"),
                None,
                &[json!({"role": "user", "content": "again"})],
                Some("there"),
                None,
                None,
                None,
                None,
                Some("openai"),
                Some("gpt-4o"),
            )
            .await
            .unwrap();

        let entries = store.read(tape).unwrap();
        let system_count = entries
            .iter()
            .filter(|e| e.kind == TapeEntryKind::System)
            .count();
        let message_count = entries
            .iter()
            .filter(|e| e.kind == TapeEntryKind::Message)
            .count();

        assert_eq!(system_count, 1);
        assert_eq!(latest_system_content(&entries), Some("system prompt"));
        assert_eq!(message_count, 4);
    }

    #[tokio::test]
    async fn test_async_append_system_if_changed_first_write() {
        let store = InMemoryTapeStore::new();
        let manager =
            AsyncTapeManager::new(Some(Box::new(AsyncTapeStoreAdapter::new(store))), None);
        assert!(
            manager
                .append_system_if_changed("t", "hello", json!({}))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_async_append_system_if_changed_duplicate() {
        let store = InMemoryTapeStore::new();
        let manager =
            AsyncTapeManager::new(Some(Box::new(AsyncTapeStoreAdapter::new(store))), None);
        assert!(
            manager
                .append_system_if_changed("t", "hello", json!({}))
                .await
                .unwrap()
        );
        assert!(
            !manager
                .append_system_if_changed("t", "hello", json!({}))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_async_append_system_if_changed_on_change() {
        let store = InMemoryTapeStore::new();
        let manager =
            AsyncTapeManager::new(Some(Box::new(AsyncTapeStoreAdapter::new(store))), None);
        assert!(
            manager
                .append_system_if_changed("t", "v1", json!({}))
                .await
                .unwrap()
        );
        assert!(
            manager
                .append_system_if_changed("t", "v2", json!({}))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_async_record_chat_with_none_system() {
        let store = InMemoryTapeStore::new();
        let manager = AsyncTapeManager::new(
            Some(Box::new(AsyncTapeStoreAdapter::new(store.clone()))),
            None,
        );
        manager
            .record_chat(
                "t",
                "r1",
                None,
                None,
                &[json!({"role": "user", "content": "hi"})],
                Some("hey"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let entries = store.read("t").unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.kind == TapeEntryKind::System)
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn test_async_record_chat_with_empty_system() {
        let store = InMemoryTapeStore::new();
        let manager = AsyncTapeManager::new(
            Some(Box::new(AsyncTapeStoreAdapter::new(store.clone()))),
            None,
        );
        manager
            .record_chat(
                "t",
                "r1",
                Some(""),
                None,
                &[json!({"role": "user", "content": "hi"})],
                Some("hey"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let entries = store.read("t").unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.kind == TapeEntryKind::System)
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn test_async_record_chat_writes_on_change() {
        let store = InMemoryTapeStore::new();
        let manager = AsyncTapeManager::new(
            Some(Box::new(AsyncTapeStoreAdapter::new(store.clone()))),
            None,
        );
        manager
            .record_chat(
                "t",
                "r1",
                Some("v1"),
                None,
                &[json!({"role": "user", "content": "hi"})],
                Some("hey"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        manager
            .record_chat(
                "t",
                "r2",
                Some("v2"),
                None,
                &[json!({"role": "user", "content": "bye"})],
                Some("cya"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let entries = store.read("t").unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.kind == TapeEntryKind::System)
                .count(),
            2
        );
        assert_eq!(latest_system_content(&entries), Some("v2"));
    }

    #[tokio::test]
    async fn test_async_record_chat_three_calls_same_prompt() {
        let store = InMemoryTapeStore::new();
        let manager = AsyncTapeManager::new(
            Some(Box::new(AsyncTapeStoreAdapter::new(store.clone()))),
            None,
        );
        for i in 0..3 {
            manager
                .record_chat(
                    "t",
                    &format!("r{i}"),
                    Some("stable"),
                    None,
                    &[json!({"role": "user", "content": format!("msg {i}")})],
                    Some("ok"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let entries = store.read("t").unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.kind == TapeEntryKind::System)
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.kind == TapeEntryKind::Message)
                .count(),
            6
        );
    }
}
