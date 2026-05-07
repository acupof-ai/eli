//! Chat helpers for Conduit.

use std::collections::HashMap;

use futures::StreamExt;
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::clients::parsing::common::field_str;
use crate::clients::parsing::sse::SseDecoder;
use crate::clients::parsing::{
    BaseTransportParser, ToolCallDelta, TransportKind, parser_for_transport,
};
use crate::core::errors::ErrorKind;
use crate::core::execution::LLMCore;
use crate::core::response_parser::TransportResponse;
use crate::core::results::{
    AsyncStreamEvents, AsyncTextStream, ErrorPayload, StreamEvent, StreamEventKind, StreamState,
};
use crate::llm::StreamEventFilter;
use crate::tools::context::ToolContext;
use crate::tools::schema::ToolSet;

/// Types that output items in the responses format can have that indicate
/// metadata-only output (no user-facing text or tool calls).
const RESPONSES_METADATA_ONLY_ITEM_TYPES: &[&str] = &["reasoning", "compaction"];

// ---------------------------------------------------------------------------
// PreparedChat
// ---------------------------------------------------------------------------

/// A fully prepared chat request, ready for execution.
#[derive(Debug, Clone)]
pub struct PreparedChat {
    /// The messages payload to send to the model.
    pub payload: Vec<Value>,
    /// New messages added during preparation (e.g., the user message).
    pub new_messages: Vec<Value>,
    /// Normalized tool set for this request.
    pub toolset: ToolSet,
    /// The tape name, if conversation history is being tracked.
    pub tape: Option<String>,
    /// Whether the tape should be updated after this request.
    pub should_update: bool,
    /// An error encountered during preparation, deferred for the caller.
    pub context_error: Option<ErrorPayload>,
    /// A unique identifier for this execution run.
    pub run_id: String,
    /// The system prompt, if provided.
    pub system_prompt: Option<String>,
}

// ---------------------------------------------------------------------------
// ToolCallAssembler
// ---------------------------------------------------------------------------

/// Reconstructs complete tool calls from streaming delta chunks.
///
/// Handles the complexity of various providers sending deltas with
/// different combinations of id, index, and positional ordering.
pub struct ToolCallAssembler {
    calls: HashMap<AssemblerKey, Value>,
    order: Vec<AssemblerKey>,
    index_to_key: HashMap<String, AssemblerKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AssemblerKey {
    Id(String),
    Index(String),
    Position(usize),
}

impl ToolCallAssembler {
    /// Create a new empty assembler.
    pub fn new() -> Self {
        Self {
            calls: HashMap::new(),
            order: Vec::new(),
            index_to_key: HashMap::new(),
        }
    }

    fn replace_key(&mut self, old_key: &AssemblerKey, new_key: AssemblerKey) {
        if let Some(entry) = self.calls.remove(old_key) {
            self.calls.insert(new_key.clone(), entry);
        }
        if let Some(pos) = self.order.iter().position(|k| k == old_key) {
            self.order[pos] = new_key.clone();
        }
        for val in self.index_to_key.values_mut() {
            if val == old_key {
                *val = new_key.clone();
            }
        }
    }

    fn key_at_position(&self, position: usize) -> Option<&AssemblerKey> {
        self.order.get(position)
    }

    fn resolve_key_by_id(
        &mut self,
        call_id: &str,
        index: Option<&str>,
        position: usize,
    ) -> AssemblerKey {
        let id_key = AssemblerKey::Id(call_id.to_owned());

        if self.calls.contains_key(&id_key) {
            if let Some(idx) = index {
                self.index_to_key.insert(idx.to_owned(), id_key.clone());
            }
            return id_key;
        }

        // Check if there's an existing entry mapped by the same index
        if let Some(idx) = index {
            if let Some(mapped) = self.index_to_key.get(idx).cloned()
                && self.calls.contains_key(&mapped)
                && mapped != id_key
            {
                self.replace_key(&mapped, id_key.clone());
                self.index_to_key.insert(idx.to_owned(), id_key.clone());
                return id_key;
            }

            let index_key = AssemblerKey::Index(idx.to_owned());
            if self.calls.contains_key(&index_key) {
                self.replace_key(&index_key, id_key.clone());
                self.index_to_key.insert(idx.to_owned(), id_key.clone());
                return id_key;
            }
        }

        // Try positional match
        if let Some(position_key) = self.key_at_position(position).cloned()
            && self.calls.contains_key(&position_key)
        {
            self.replace_key(&position_key, id_key.clone());
            if let Some(idx) = index {
                self.index_to_key.insert(idx.to_owned(), id_key.clone());
            }
            return id_key;
        }

        if let Some(idx) = index {
            self.index_to_key.insert(idx.to_owned(), id_key.clone());
        }
        id_key
    }

    fn resolve_key_by_index(
        &mut self,
        delta: &ToolCallDelta,
        index: &str,
        position: usize,
    ) -> AssemblerKey {
        if let Some(mapped) = self.index_to_key.get(index).cloned()
            && self.calls.contains_key(&mapped)
        {
            return mapped;
        }

        let index_key = AssemblerKey::Index(index.to_owned());
        if self.calls.contains_key(&index_key) {
            self.index_to_key
                .insert(index.to_owned(), index_key.clone());
            return index_key;
        }

        if let Some(position_key) = self.key_at_position(position).cloned() {
            if (delta.name.is_empty()) && self.calls.contains_key(&position_key) {
                self.index_to_key
                    .insert(index.to_owned(), position_key.clone());
                return position_key;
            }
            if self.calls.contains_key(&position_key)
                && let AssemblerKey::Position(_) = position_key
            {
                self.replace_key(&position_key, index_key.clone());
                self.index_to_key
                    .insert(index.to_owned(), index_key.clone());
                return index_key;
            }
        }

        self.index_to_key
            .insert(index.to_owned(), index_key.clone());
        index_key
    }

    fn resolve_key(&mut self, delta: &ToolCallDelta, position: usize) -> AssemblerKey {
        if let Some(ref call_id) = delta.id {
            let index_str = delta.index.as_ref().and_then(|v| {
                v.as_str()
                    .map(|s| s.to_owned())
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            });
            return self.resolve_key_by_id(call_id, index_str.as_deref(), position);
        }

        if let Some(ref index_val) = delta.index {
            let index_str = index_val
                .as_str()
                .map(|s| s.to_owned())
                .or_else(|| index_val.as_u64().map(|n| n.to_string()));
            if let Some(idx) = index_str {
                return self.resolve_key_by_index(delta, &idx, position);
            }
        }

        // Fallback: use positional key
        if let Some(key) = self.key_at_position(position).cloned() {
            return key;
        }
        AssemblerKey::Position(position)
    }

    fn merge_arguments(entry: &mut Value, arguments: &str, arguments_complete: bool) {
        if arguments.is_empty() && !arguments_complete {
            return;
        }
        let func = entry
            .as_object_mut()
            .and_then(|o| o.get_mut("function"))
            .and_then(|f| f.as_object_mut());
        let func = match func {
            Some(f) => f,
            None => return,
        };

        if arguments_complete {
            func.insert("arguments".to_owned(), Value::String(arguments.to_owned()));
            return;
        }

        let existing = func.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
        let combined = format!("{}{}", existing, arguments);
        func.insert("arguments".to_owned(), Value::String(combined));
    }

    /// Incorporate a batch of tool-call deltas from one streaming chunk.
    pub fn add_deltas(&mut self, deltas: &[ToolCallDelta]) {
        for (position, delta) in deltas.iter().enumerate() {
            let key = self.resolve_key(delta, position);
            if !self.calls.contains_key(&key) {
                self.order.push(key.clone());
                let entry = serde_json::json!({
                    "function": {"name": "", "arguments": ""}
                });
                self.calls.insert(key.clone(), entry);
            }
            let entry = self
                .calls
                .get_mut(&key)
                .expect("SAFETY: key was just inserted above if absent");

            if let Some(ref call_id) = delta.id
                && let Some(obj) = entry.as_object_mut()
            {
                obj.insert("id".to_owned(), Value::String(call_id.clone()));
            }
            if let Some(ref call_type) = delta.call_type
                && let Some(obj) = entry.as_object_mut()
            {
                obj.insert("type".to_owned(), Value::String(call_type.clone()));
            }
            if !delta.name.is_empty()
                && let Some(func) = entry
                    .as_object_mut()
                    .and_then(|o| o.get_mut("function"))
                    .and_then(|f| f.as_object_mut())
            {
                func.insert("name".to_owned(), Value::String(delta.name.clone()));
            }
            Self::merge_arguments(entry, &delta.arguments, delta.arguments_complete);
        }
    }

    /// Finalize and return the assembled tool calls, expanding any
    /// concatenated JSON arguments.
    pub fn finalize(&self) -> Vec<Value> {
        let calls: Vec<Value> = self
            .order
            .iter()
            .filter_map(|k| self.calls.get(k).cloned())
            .collect();
        crate::clients::parsing::common::expand_tool_calls(calls)
    }
}

impl Default for ToolCallAssembler {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ChatClient
// ---------------------------------------------------------------------------

/// Chat operations with structured outputs.
pub struct ChatClient {
    core: LLMCore,
}

impl ChatClient {
    fn sse_stream_error(
        provider_name: &str,
        model_id: &str,
        error: impl std::fmt::Display,
    ) -> ErrorPayload {
        ErrorPayload::new(
            ErrorKind::Provider,
            format!("{}:{}: stream error: {}", provider_name, model_id, error),
        )
    }

    async fn forward_sse_chunk(
        decoder: &mut SseDecoder,
        tx: &tokio::sync::mpsc::Sender<Result<Value, ErrorPayload>>,
        bytes: &[u8],
    ) {
        for event in decoder.push(bytes) {
            if tx.send(Ok(event)).await.is_err() {
                return;
            }
        }
    }

    async fn forward_sse_events(
        response: reqwest::Response,
        tx: tokio::sync::mpsc::Sender<Result<Value, ErrorPayload>>,
        provider_name: String,
        model_id: String,
    ) {
        let mut decoder = SseDecoder::new();
        let mut byte_stream = response.bytes_stream();
        while let Some(chunk_result) = byte_stream.next().await {
            let bytes = match chunk_result {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = tx
                        .send(Err(Self::sse_stream_error(
                            &provider_name,
                            &model_id,
                            error,
                        )))
                        .await;
                    return;
                }
            };
            Self::forward_sse_chunk(&mut decoder, &tx, &bytes).await;
        }
        for event in decoder.finish() {
            if tx.send(Ok(event)).await.is_err() {
                return;
            }
        }
    }

    fn spawn_sse_event_stream(
        response: reqwest::Response,
        provider_name: String,
        model_id: String,
    ) -> ReceiverStream<Result<Value, ErrorPayload>> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Value, ErrorPayload>>(64);
        tokio::spawn(Self::forward_sse_events(
            response,
            tx,
            provider_name,
            model_id,
        ));
        ReceiverStream::new(rx)
    }

    /// Create a new `ChatClient` wrapping the given `LLMCore`.
    pub fn new(core: LLMCore) -> Self {
        Self { core }
    }

    /// Access the inner `LLMCore`.
    pub fn core(&self) -> &LLMCore {
        &self.core
    }

    /// Access the inner `LLMCore` mutably.
    pub fn core_mut(&mut self) -> &mut LLMCore {
        &mut self.core
    }

    pub fn unwrap_response(response: &TransportResponse) -> (&Value, TransportKind) {
        (&response.payload, response.transport)
    }

    pub fn resolve_transport(payload: &Value, transport: Option<TransportKind>) -> TransportKind {
        if let Some(t) = transport {
            return t;
        }
        if payload.is_array() {
            return TransportKind::Responses;
        }
        if payload.get("output").is_some() || payload.get("output_text").is_some() {
            return TransportKind::Responses;
        }
        if let Some(event_type) = payload.get("type").and_then(|v| v.as_str())
            && event_type.starts_with("response.")
        {
            return TransportKind::Responses;
        }
        TransportKind::Completion
    }

    pub fn parser_for_payload(
        payload: &Value,
        transport: Option<TransportKind>,
    ) -> &'static dyn BaseTransportParser {
        let effective = Self::resolve_transport(payload, transport);
        parser_for_transport(effective)
    }

    /// Check if a completed responses payload contains only metadata items.
    pub fn is_completed_responses_metadata_only(
        payload: &Value,
        transport: Option<TransportKind>,
    ) -> bool {
        let effective = Self::resolve_transport(payload, transport);
        if effective != TransportKind::Responses {
            return false;
        }
        if field_str(payload, "status") != "completed" {
            return false;
        }
        if payload.get("incomplete_details").is_some() {
            return false;
        }
        let output = match payload.get("output").and_then(|o| o.as_array()) {
            Some(arr) if !arr.is_empty() => arr,
            _ => return false,
        };
        output.iter().all(|item| {
            let item_type = field_str(item, "type");
            RESPONSES_METADATA_ONLY_ITEM_TYPES.contains(&item_type)
        })
    }

    /// Check if a streaming chunk's output items are all metadata-only.
    pub fn is_completed_responses_stream_metadata_only(
        completed: bool,
        output_item_types: &[String],
    ) -> bool {
        if !completed || output_item_types.is_empty() {
            return false;
        }
        output_item_types
            .iter()
            .all(|t| RESPONSES_METADATA_ONLY_ITEM_TYPES.contains(&t.as_str()))
    }

    /// Get the output item type from a responses stream chunk, if applicable.
    pub fn responses_output_item_type(
        chunk: &Value,
        transport: Option<TransportKind>,
    ) -> Option<String> {
        let effective = Self::resolve_transport(chunk, transport);
        if effective != TransportKind::Responses {
            return None;
        }
        let chunk_type = chunk.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if chunk_type != "response.output_item.added" && chunk_type != "response.output_item.done" {
            return None;
        }
        chunk
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_owned())
    }

    pub fn extract_text(response: &Value, transport: Option<TransportKind>) -> String {
        let parser = Self::parser_for_payload(response, transport);
        parser.extract_text(response)
    }

    pub fn extract_tool_calls(response: &Value, transport: Option<TransportKind>) -> Vec<Value> {
        let parser = Self::parser_for_payload(response, transport);
        parser.extract_tool_calls(response)
    }

    pub fn extract_usage(response: &Value, transport: Option<TransportKind>) -> Option<Value> {
        let parser = Self::parser_for_payload(response, transport);
        parser.extract_usage(response)
    }

    pub fn extract_chunk_text(chunk: &Value, transport: Option<TransportKind>) -> String {
        let parser = Self::parser_for_payload(chunk, transport);
        parser.extract_chunk_text(chunk)
    }

    pub fn extract_chunk_tool_call_deltas(
        chunk: &Value,
        transport: Option<TransportKind>,
    ) -> Vec<ToolCallDelta> {
        let parser = Self::parser_for_payload(chunk, transport);
        parser.extract_chunk_tool_call_deltas(chunk)
    }

    pub fn validate_chat_input(
        prompt: Option<&str>,
        messages: Option<&[Value]>,
        system_prompt: Option<&str>,
        tape: Option<&str>,
    ) -> Result<(), ErrorPayload> {
        if prompt.is_some() && messages.is_some() {
            return Err(ErrorPayload::new(
                ErrorKind::InvalidInput,
                "Provide either prompt or messages, not both.",
            ));
        }
        if prompt.is_none() && messages.is_none() {
            return Err(ErrorPayload::new(
                ErrorKind::InvalidInput,
                "Either prompt or messages is required.",
            ));
        }
        if messages.is_some() && (system_prompt.is_some() || tape.is_some()) {
            return Err(ErrorPayload::new(
                ErrorKind::InvalidInput,
                "system_prompt and tape are not supported with messages input.",
            ));
        }
        Ok(())
    }

    pub fn prepare_messages(
        prompt: Option<&str>,
        system_prompt: Option<&str>,
        messages: Option<&[Value]>,
        history: Option<&[Value]>,
    ) -> Result<(Vec<Value>, Vec<Value>), ErrorPayload> {
        if let Some(msgs) = messages {
            let payload: Vec<Value> = msgs.to_vec();
            return Ok((payload, Vec::new()));
        }

        let prompt = prompt.ok_or_else(|| {
            ErrorPayload::new(
                ErrorKind::InvalidInput,
                "prompt is required when messages is not provided",
            )
        })?;

        let user_message = serde_json::json!({"role": "user", "content": prompt});

        let mut payload = Vec::new();
        if let Some(sp) = system_prompt
            && !sp.is_empty()
        {
            payload.push(serde_json::json!({"role": "system", "content": sp}));
        }
        if let Some(hist) = history {
            payload.extend_from_slice(hist);
        }
        let new_messages = vec![user_message.clone()];
        payload.push(user_message);
        Ok((payload, new_messages))
    }

    /// Prepare a full chat request.
    pub fn prepare_request(
        prompt: Option<&str>,
        system_prompt: Option<&str>,
        messages: Option<&[Value]>,
        tape: Option<&str>,
        history: Option<&[Value]>,
        toolset: ToolSet,
    ) -> PreparedChat {
        let mut context_error: Option<ErrorPayload> = None;
        let mut payload = Vec::new();
        let mut new_messages = Vec::new();

        if let Err(e) = Self::validate_chat_input(prompt, messages, system_prompt, tape) {
            context_error = Some(e);
        } else {
            let tape_history = if tape.is_some() { history } else { None };
            match Self::prepare_messages(prompt, system_prompt, messages, tape_history) {
                Ok((p, nm)) => {
                    payload = p;
                    new_messages = nm;
                }
                Err(e) => {
                    context_error = Some(e);
                }
            }
        }

        let should_update = tape.is_some() && messages.is_none();
        let run_id = Uuid::new_v4().to_string().replace('-', "");

        PreparedChat {
            payload,
            new_messages,
            toolset,
            tape: tape.map(|s| s.to_owned()),
            should_update,
            context_error,
            run_id,
            system_prompt: system_prompt.map(|s| s.to_owned()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn chat(
        &mut self,
        prompt: Option<&str>,
        system_prompt: Option<&str>,
        messages: Option<&[Value]>,
        model: Option<&str>,
        provider: Option<&str>,
        max_tokens: Option<u32>,
        kwargs: serde_json::Map<String, Value>,
    ) -> Result<String, ErrorPayload> {
        let prepared = Self::prepare_request(
            prompt,
            system_prompt,
            messages,
            None,
            None,
            ToolSet::empty(),
        );
        if let Some(ref err) = prepared.context_error {
            return Err(err.clone());
        }

        let result = self
            .core
            .run_chat(
                prepared.payload,
                None,
                model,
                provider,
                max_tokens,
                false,
                None,
                kwargs,
                None,
                |response, _prov, _mdl, _attempt| {
                    let (payload, transport) = Self::unwrap_response(&response);
                    let text = Self::extract_text(payload, Some(transport));
                    if !text.is_empty() {
                        return Ok(text);
                    }
                    if Self::is_completed_responses_metadata_only(payload, Some(transport)) {
                        return Ok(String::new());
                    }
                    Err(None)
                },
            )
            .await
            .map_err(|e| ErrorPayload::new(e.kind, e.message))?;

        Ok(result)
    }

    /// Execute a non-streaming tool-calls request.
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_calls(
        &mut self,
        prompt: Option<&str>,
        system_prompt: Option<&str>,
        messages: Option<&[Value]>,
        model: Option<&str>,
        provider: Option<&str>,
        max_tokens: Option<u32>,
        tools: &ToolSet,
        kwargs: serde_json::Map<String, Value>,
    ) -> Result<Vec<Value>, ErrorPayload> {
        let prepared =
            Self::prepare_request(prompt, system_prompt, messages, None, None, tools.clone());
        if let Some(ref err) = prepared.context_error {
            return Err(err.clone());
        }

        let tools_payload = tools.payload().map(|s| s.to_vec());

        let result = self
            .core
            .run_chat(
                prepared.payload,
                tools_payload,
                model,
                provider,
                max_tokens,
                false,
                None,
                kwargs,
                None,
                |response, _prov, _mdl, _attempt| {
                    let (payload, transport) = Self::unwrap_response(&response);
                    let calls = Self::extract_tool_calls(payload, Some(transport));
                    Ok(calls)
                },
            )
            .await
            .map_err(|e| ErrorPayload::new(e.kind, e.message))?;

        Ok(result)
    }

    /// Execute a streaming chat call, returning an async text stream.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream(
        &mut self,
        prompt: Option<&str>,
        system_prompt: Option<&str>,
        messages: Option<&[Value]>,
        model: Option<&str>,
        provider: Option<&str>,
        max_tokens: Option<u32>,
        kwargs: serde_json::Map<String, Value>,
    ) -> Result<AsyncTextStream, ErrorPayload> {
        let prepared = Self::prepare_request(
            prompt,
            system_prompt,
            messages,
            None,
            None,
            ToolSet::empty(),
        );
        if let Some(ref err) = prepared.context_error {
            return Err(err.clone());
        }

        let (response, transport, _provider_name, _model_id) = self
            .core
            .run_chat_stream(
                prepared.payload,
                None,
                model,
                provider,
                max_tokens,
                None,
                kwargs,
                None,
            )
            .await
            .map_err(|e| ErrorPayload::new(e.kind, e.message))?;

        let state = StreamState::new();

        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        let mut events = Self::spawn_sse_event_stream(response, _provider_name, _model_id);
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                let chunk_val = match event {
                    Ok(chunk_val) => chunk_val,
                    Err(_) => break,
                };
                let text = Self::extract_chunk_text(&chunk_val, Some(transport));
                if !text.is_empty() && tx.send(text).await.is_err() {
                    return;
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(AsyncTextStream::new(stream, Some(state)))
    }

    /// Execute a streaming chat call, returning an async event stream with
    /// text deltas, tool calls, usage, and a final event.
    ///
    /// When `stream_filter` is provided, each event is passed through the filter
    /// before being sent to the receiver. Events for which the filter returns
    /// `None` are silently dropped.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_events(
        &mut self,
        prompt: Option<&str>,
        system_prompt: Option<&str>,
        messages: Option<&[Value]>,
        model: Option<&str>,
        provider: Option<&str>,
        max_tokens: Option<u32>,
        tools: Option<&ToolSet>,
        kwargs: serde_json::Map<String, Value>,
        stream_filter: Option<StreamEventFilter>,
    ) -> Result<AsyncStreamEvents, ErrorPayload> {
        let toolset = tools.cloned().unwrap_or_else(ToolSet::empty);
        let prepared =
            Self::prepare_request(prompt, system_prompt, messages, None, None, toolset.clone());
        if let Some(ref err) = prepared.context_error {
            return Err(err.clone());
        }

        let tools_payload = toolset.payload().map(|s| s.to_vec());

        let (response, transport, provider_name, model_id) = self
            .core
            .run_chat_stream(
                prepared.payload,
                tools_payload,
                model,
                provider,
                max_tokens,
                None,
                kwargs,
                None,
            )
            .await
            .map_err(|e| ErrorPayload::new(e.kind, e.message))?;

        let state = StreamState::new();

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);
        let mut events =
            Self::spawn_sse_event_stream(response, provider_name.clone(), model_id.clone());
        tokio::spawn(async move {
            // Helper: apply filter and send, returning whether the send succeeded.
            async fn emit(
                tx: &tokio::sync::mpsc::Sender<StreamEvent>,
                event: StreamEvent,
                filter: &Option<StreamEventFilter>,
            ) -> bool {
                let event = match filter {
                    Some(f) => match f(event) {
                        Some(e) => e,
                        None => return true, // dropped by filter, not an error
                    },
                    None => event,
                };
                tx.send(event).await.is_ok()
            }

            let mut parts: Vec<String> = Vec::new();
            let mut assembler = ToolCallAssembler::new();
            let mut usage: Option<Value> = None;
            let mut response_completed = false;
            let mut output_item_types: Vec<String> = Vec::new();
            let mut had_error = false;

            while let Some(event) = events.next().await {
                let chunk_val = match event {
                    Ok(chunk_val) => chunk_val,
                    Err(error) => {
                        let _ = emit(
                            &tx,
                            StreamEvent::new(
                                StreamEventKind::Error,
                                serde_json::to_value(error.as_map()).unwrap_or_default(),
                            ),
                            &stream_filter,
                        )
                        .await;
                        had_error = true;
                        break;
                    }
                };

                if chunk_val.get("type").and_then(|v| v.as_str()) == Some("response.completed") {
                    response_completed = true;
                }

                if let Some(item_type) =
                    Self::responses_output_item_type(&chunk_val, Some(transport))
                {
                    output_item_types.push(item_type);
                }

                let chunk_parser = Self::parser_for_payload(&chunk_val, Some(transport));
                if let Some(u) = chunk_parser.extract_usage(&chunk_val) {
                    usage = Some(u);
                }

                let deltas = chunk_parser.extract_chunk_tool_call_deltas(&chunk_val);
                if !deltas.is_empty() {
                    assembler.add_deltas(&deltas);
                }

                let text = chunk_parser.extract_chunk_text(&chunk_val);
                if !text.is_empty() {
                    parts.push(text.clone());
                    let _ = emit(
                        &tx,
                        StreamEvent::new(StreamEventKind::Text, serde_json::json!({"delta": text})),
                        &stream_filter,
                    )
                    .await;
                }
            }

            let tool_calls = assembler.finalize();
            for (idx, call) in tool_calls.iter().enumerate() {
                let _ = emit(
                    &tx,
                    StreamEvent::new(
                        StreamEventKind::ToolCall,
                        serde_json::json!({"index": idx, "call": call}),
                    ),
                    &stream_filter,
                )
                .await;
            }

            let full_text = if parts.is_empty() {
                None
            } else {
                Some(parts.join(""))
            };

            let error = if !had_error
                && full_text.is_none()
                && tool_calls.is_empty()
                && !Self::is_completed_responses_stream_metadata_only(
                    response_completed,
                    &output_item_types,
                ) {
                let e = ErrorPayload::new(
                    ErrorKind::Temporary,
                    format!("{}:{}: empty response", provider_name, model_id),
                );
                let _ = emit(
                    &tx,
                    StreamEvent::new(
                        StreamEventKind::Error,
                        serde_json::to_value(e.as_map()).unwrap_or_default(),
                    ),
                    &stream_filter,
                )
                .await;
                Some(e)
            } else {
                None
            };

            if let Some(ref u) = usage {
                let _ = emit(
                    &tx,
                    StreamEvent::new(StreamEventKind::Usage, u.clone()),
                    &stream_filter,
                )
                .await;
            }

            let _ = emit(
                &tx,
                StreamEvent::new(
                    StreamEventKind::Final,
                    serde_json::json!({
                        "text": full_text,
                        "tool_calls": tool_calls,
                        "tool_results": [],
                        "usage": usage,
                        "ok": error.is_none() && !had_error,
                    }),
                ),
                &stream_filter,
            )
            .await;
        });

        let stream = ReceiverStream::new(rx);
        Ok(AsyncStreamEvents::new(stream, Some(state)))
    }

    /// Create a `ToolContext` for tool execution.
    pub fn make_tool_context(
        prepared: &PreparedChat,
        provider_name: &str,
        model_id: &str,
    ) -> ToolContext {
        let meta = HashMap::from([
            (
                "provider".to_owned(),
                Value::String(provider_name.to_owned()),
            ),
            ("model".to_owned(), Value::String(model_id.to_owned())),
        ]);
        let mut ctx = ToolContext::new(&prepared.run_id);
        ctx.meta = meta;
        ctx.tape = prepared.tape.clone();
        ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_validate_chat_input_both() {
        let result = ChatClient::validate_chat_input(
            Some("hi"),
            Some(&[json!({"role": "user", "content": "hi"})]),
            None,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_chat_input_none() {
        let result = ChatClient::validate_chat_input(None, None, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_chat_input_valid() {
        let result = ChatClient::validate_chat_input(Some("hi"), None, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_prepare_messages_prompt() {
        let (payload, new) =
            ChatClient::prepare_messages(Some("hi"), Some("Be helpful"), None, None).unwrap();
        assert_eq!(payload.len(), 2);
        assert_eq!(payload[0]["role"], "system");
        assert_eq!(payload[1]["role"], "user");
        assert_eq!(new.len(), 1);
    }

    #[test]
    fn test_prepare_messages_raw() {
        let msgs = vec![json!({"role": "user", "content": "hello"})];
        let (payload, new) = ChatClient::prepare_messages(None, None, Some(&msgs), None).unwrap();
        assert_eq!(payload.len(), 1);
        assert!(new.is_empty());
    }

    #[test]
    fn test_tool_call_assembler_basic() {
        let mut asm = ToolCallAssembler::new();
        let deltas = vec![ToolCallDelta {
            id: Some("call_1".into()),
            index: Some(Value::Number(0.into())),
            call_type: Some("function".into()),
            name: "greet".into(),
            arguments: "{\"name\":".into(),
            arguments_complete: false,
        }];
        asm.add_deltas(&deltas);

        let deltas2 = vec![ToolCallDelta {
            id: Some("call_1".into()),
            index: Some(Value::Number(0.into())),
            call_type: None,
            name: String::new(),
            arguments: "\"Bob\"}".into(),
            arguments_complete: false,
        }];
        asm.add_deltas(&deltas2);

        let calls = asm.finalize();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "greet");
        assert_eq!(
            calls[0]["function"]["arguments"].as_str().unwrap(),
            "{\"name\":\"Bob\"}"
        );
    }

    #[test]
    fn test_resolve_transport_responses() {
        let payload = json!({"output": []});
        assert_eq!(
            ChatClient::resolve_transport(&payload, None),
            TransportKind::Responses
        );
    }

    #[test]
    fn test_resolve_transport_completion() {
        let payload = json!({"choices": []});
        assert_eq!(
            ChatClient::resolve_transport(&payload, None),
            TransportKind::Completion
        );
    }

    #[test]
    fn test_is_completed_responses_metadata_only_true() {
        let payload = json!({
            "status": "completed",
            "output": [{"type": "reasoning"}]
        });
        assert!(ChatClient::is_completed_responses_metadata_only(
            &payload,
            Some(TransportKind::Responses)
        ));
    }

    #[test]
    fn test_is_completed_responses_metadata_only_false() {
        let payload = json!({
            "status": "completed",
            "output": [{"type": "message"}]
        });
        assert!(!ChatClient::is_completed_responses_metadata_only(
            &payload,
            Some(TransportKind::Responses)
        ));
    }
}
