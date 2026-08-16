//! Streaming chat completion.

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::clients::parsing::parser_for_transport;
use crate::clients::parsing::types::{BaseTransportParser, ToolCallDelta};
use crate::core::errors::ConduitError;
use crate::core::results::AsyncTextStream;

use super::{LLM, build_messages, prepend_tape_history};

impl LLM {
    /// Stream chat completion as an async `TextStream`.
    pub async fn stream(
        &mut self,
        req: super::ChatRequest<'_>,
    ) -> Result<AsyncTextStream, ConduitError> {
        let super::ChatRequest {
            prompt,
            user_content,
            system_prompt,
            model,
            provider,
            messages,
            max_tokens,
            tape,
            cancellation,
            session_id,
            ..
        } = req;

        let tape_messages = match tape {
            Some(tape_name) => self.build_tape_messages(tape_name, None).await,
            None => Vec::new(),
        };

        let mut msgs = build_messages(
            prompt,
            user_content.as_deref(),
            system_prompt,
            messages.as_deref(),
        );
        prepend_tape_history(&mut msgs, tape_messages);

        if let Some(tape_name) = tape {
            let new_messages: Vec<Value> = msgs
                .iter()
                .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                .cloned()
                .collect();
            let run_id = Uuid::new_v4().to_string();
            if let Err(e) = self
                .async_tape
                .record_chat(
                    tape_name,
                    &run_id,
                    system_prompt,
                    None,
                    &new_messages,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(self.core.provider()),
                    Some(self.core.model()),
                )
                .await
            {
                tracing::error!(error = %e, tape = %tape_name, "failed to record streaming chat context");
            }
        }

        let (response, transport, _prov, _model) = self
            .core
            .run_chat_stream(
                msgs,
                None,
                model,
                provider,
                max_tokens,
                None,
                Default::default(),
                session_id,
            )
            .await?;

        let parser = parser_for_transport(transport);
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);

        tokio::spawn(Self::stream_sse_loop(response, parser, tx, cancellation));

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(AsyncTextStream::new(stream, None))
    }

    /// Stream one tool-calling round.
    ///
    /// Same inputs and result shape as the non-streaming `run_chat` call in
    /// `_execute_tool_round`: prose deltas are forwarded to `text_sink` as
    /// they arrive, tool-call deltas are accumulated (grouped by index) and
    /// usage is merged across chunks, then a completion-shaped response is
    /// reconstructed so `extract_content`, `extract_tool_calls`, and
    /// `UsageEvent::from_raw` work on it unchanged.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn stream_round(
        &mut self,
        msgs: Vec<Value>,
        schemas: Option<Vec<Value>>,
        model: Option<&str>,
        provider: Option<&str>,
        max_tokens: Option<u32>,
        session_id: Option<&str>,
        cancellation: Option<&CancellationToken>,
        text_sink: &tokio::sync::mpsc::Sender<crate::llm::StreamChunk>,
    ) -> Result<Value, ConduitError> {
        let (response, transport, _prov, resolved_model) = self
            .core
            .run_chat_stream(
                msgs,
                schemas,
                model,
                provider,
                max_tokens,
                None,
                Default::default(),
                session_id,
            )
            .await?;

        let parser = parser_for_transport(transport);
        let mut content = String::new();
        let mut tool_builders: Vec<(Value, ToolCallBuilder)> = Vec::new();
        let mut usage: serde_json::Map<String, Value> = serde_json::Map::new();
        let mut chunk_model: Option<String> = None;

        use futures::StreamExt;
        let mut byte_stream = response.bytes_stream();
        let mut splitter = SseLineSplitter::new();

        'outer: loop {
            let chunk_result = match cancellation {
                Some(token) => {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => {
                            tracing::info!("SSE stream cancelled");
                            break;
                        }
                        chunk = byte_stream.next() => chunk,
                    }
                }
                None => byte_stream.next().await,
            };
            let Some(chunk_result) = chunk_result else {
                break; // stream finished
            };
            let bytes = match chunk_result {
                Ok(b) => b,
                Err(_) => break,
            };
            for line in splitter.push(&bytes) {
                match line {
                    SseLine::Done => break 'outer,
                    SseLine::Data(data) => {
                        let Ok(val) = serde_json::from_str::<Value>(&data) else {
                            continue;
                        };
                        let text = parser.extract_chunk_text(&val);
                        if !text.is_empty() {
                            content.push_str(&text);
                            // A closed receiver just means nobody is listening;
                            // the reconstructed response still carries the text.
                            if text_sink
                                .send(crate::llm::StreamChunk::Text(text))
                                .await
                                .is_err()
                            {
                                tracing::debug!(
                                    "text delta sink closed; dropping remaining deltas"
                                );
                            }
                        }
                        let reasoning = parser.extract_chunk_reasoning(&val);
                        if !reasoning.is_empty()
                            && text_sink
                                .send(crate::llm::StreamChunk::Reasoning(reasoning))
                                .await
                                .is_err()
                        {
                            tracing::debug!("reasoning delta sink closed; dropping remaining deltas");
                        }
                        for delta in parser.extract_chunk_tool_call_deltas(&val) {
                            accumulate_tool_delta(&mut tool_builders, delta);
                        }
                        // Merge rather than replace: Anthropic splits usage
                        // across message_start (input) and message_delta (output).
                        if let Some(u) = parser.extract_usage(&val)
                            && let Some(obj) = u.as_object()
                        {
                            for (k, v) in obj {
                                usage.insert(k.clone(), v.clone());
                            }
                        }
                        if chunk_model.is_none()
                            && let Some(m) = val
                                .get("model")
                                .and_then(|m| m.as_str())
                                .filter(|s| !s.is_empty())
                        {
                            chunk_model = Some(m.to_owned());
                        }
                    }
                }
            }
        }

        Ok(build_round_response(
            content,
            tool_builders,
            usage,
            chunk_model.unwrap_or(resolved_model),
        ))
    }

    /// Consume SSE bytes from the response, parse text chunks, and forward
    /// them through `tx`. Respects an optional `CancellationToken` — when
    /// cancelled the loop stops and the channel closes, delivering whatever
    /// partial content was already sent.
    async fn stream_sse_loop(
        response: reqwest::Response,
        parser: &'static dyn BaseTransportParser,
        tx: tokio::sync::mpsc::Sender<String>,
        cancellation: Option<CancellationToken>,
    ) {
        use futures::StreamExt;

        let mut byte_stream = response.bytes_stream();
        let mut splitter = SseLineSplitter::new();

        loop {
            // Obtain the next chunk, racing against cancellation when a
            // token was provided.
            let chunk_result = match cancellation {
                Some(ref token) => {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => {
                            tracing::info!("SSE stream cancelled");
                            break;
                        }
                        chunk = byte_stream.next() => chunk,
                    }
                }
                None => byte_stream.next().await,
            };

            let Some(chunk_result) = chunk_result else {
                break; // stream finished
            };
            let bytes = match chunk_result {
                Ok(b) => b,
                Err(_) => break,
            };
            for line in splitter.push(&bytes) {
                match line {
                    SseLine::Done => return,
                    SseLine::Data(data) => {
                        if let Ok(val) = serde_json::from_str::<Value>(&data) {
                            let content = parser.extract_chunk_text(&val);
                            if !content.is_empty() && tx.send(content).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One complete SSE payload line.
enum SseLine {
    /// A `data: <payload>` line.
    Data(String),
    /// The `data: [DONE]` sentinel.
    Done,
}

/// Incremental SSE line splitter. Feeds raw body chunks and yields complete
/// `data:` payloads; partial lines (which may end mid-multibyte-UTF-8) stay
/// buffered until the next chunk. SSE keep-alive comments (any line not
/// prefixed with `data: `) are ignored.
struct SseLineSplitter {
    buffer: Vec<u8>,
}

impl SseLineSplitter {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<SseLine> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut cursor = 0;
        while let Some(rel) = self.buffer[cursor..].iter().position(|&b| b == b'\n') {
            let line_end = cursor + rel;
            let mut end = line_end;
            if end > cursor && self.buffer[end - 1] == b'\r' {
                end -= 1;
            }
            let line = String::from_utf8_lossy(&self.buffer[cursor..end]);
            cursor = line_end + 1;

            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    out.push(SseLine::Done);
                } else {
                    out.push(SseLine::Data(data.to_owned()));
                }
            }
        }
        // Remove consumed bytes in one operation instead of per-line.
        if cursor > 0 {
            self.buffer.drain(..cursor);
        }
        out
    }
}

/// One tool call being assembled from streaming deltas.
struct ToolCallBuilder {
    id: Option<String>,
    call_type: Option<String>,
    name: String,
    arguments: String,
}

/// Fold one [`ToolCallDelta`] into the builder keyed by its correlation index
/// (falling back to the call id, then a null key). The first delta carrying an
/// id / type / name wins; argument fragments are concatenated, except an
/// `arguments_complete` delta carries the full arguments and replaces them.
fn accumulate_tool_delta(builders: &mut Vec<(Value, ToolCallBuilder)>, delta: ToolCallDelta) {
    let key = delta
        .index
        .clone()
        .or_else(|| delta.id.clone().map(Value::String))
        .unwrap_or(Value::Null);
    let pos = builders.iter().position(|(k, _)| k == &key);
    let builder = match pos {
        Some(i) => &mut builders[i].1,
        None => {
            builders.push((
                key,
                ToolCallBuilder {
                    id: None,
                    call_type: None,
                    name: String::new(),
                    arguments: String::new(),
                },
            ));
            &mut builders.last_mut().unwrap().1
        }
    };
    if builder.id.is_none()
        && let Some(id) = delta.id
    {
        builder.id = Some(id);
    }
    if builder.call_type.is_none()
        && let Some(call_type) = delta.call_type
    {
        builder.call_type = Some(call_type);
    }
    if builder.name.is_empty() && !delta.name.is_empty() {
        builder.name = delta.name;
    }
    if delta.arguments_complete {
        builder.arguments = delta.arguments;
    } else {
        builder.arguments.push_str(&delta.arguments);
    }
}

/// Reconstruct a completion-shaped response (`choices[0].message...`) from the
/// accumulated stream state, matching the payload shape `run_chat` returns so
/// downstream extraction helpers work unchanged. Tool calls are emitted in the
/// `{"id","type":"function","function":{"name","arguments"}}` shape; calls that
/// never got a name are dropped (a nameless call cannot be executed).
fn build_round_response(
    content: String,
    tool_builders: Vec<(Value, ToolCallBuilder)>,
    usage: serde_json::Map<String, Value>,
    model: String,
) -> Value {
    let tool_calls: Vec<Value> = tool_builders
        .into_iter()
        .filter(|(_, b)| !b.name.is_empty())
        .map(|(_, b)| {
            let mut call = serde_json::Map::new();
            if let Some(id) = b.id {
                call.insert("id".to_owned(), Value::String(id));
            }
            call.insert(
                "type".to_owned(),
                Value::String(b.call_type.unwrap_or_else(|| "function".to_owned())),
            );
            call.insert(
                "function".to_owned(),
                serde_json::json!({"name": b.name, "arguments": b.arguments}),
            );
            Value::Object(call)
        })
        .collect();

    let had_tool_calls = !tool_calls.is_empty();
    let mut message = serde_json::Map::new();
    message.insert("role".to_owned(), Value::String("assistant".to_owned()));
    message.insert(
        "content".to_owned(),
        if content.is_empty() {
            Value::Null
        } else {
            Value::String(content)
        },
    );
    if had_tool_calls {
        message.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }

    let mut root = serde_json::Map::new();
    root.insert("model".to_owned(), Value::String(model));
    root.insert(
        "choices".to_owned(),
        serde_json::json!([{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": if had_tool_calls { "tool_calls" } else { "stop" },
        }]),
    );
    if !usage.is_empty() {
        root.insert("usage".to_owned(), Value::Object(usage));
    }
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(
        index: Option<i64>,
        id: Option<&str>,
        name: &str,
        arguments: &str,
        complete: bool,
    ) -> ToolCallDelta {
        ToolCallDelta {
            id: id.map(str::to_owned),
            index: index.map(Value::from),
            call_type: Some("function".to_owned()),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            arguments_complete: complete,
        }
    }

    #[test]
    fn accumulator_groups_by_index_and_concatenates_arguments() {
        let mut builders = Vec::new();
        accumulate_tool_delta(&mut builders, delta(Some(0), Some("call_1"), "echo", "", false));
        accumulate_tool_delta(&mut builders, delta(Some(0), None, "", "{\"x\":", false));
        accumulate_tool_delta(&mut builders, delta(Some(0), None, "", " 1}", false));
        assert_eq!(builders.len(), 1);
        let b = &builders[0].1;
        assert_eq!(b.id.as_deref(), Some("call_1"));
        assert_eq!(b.name, "echo");
        assert_eq!(b.arguments, "{\"x\": 1}");
    }

    #[test]
    fn accumulator_complete_arguments_replaces_fragments() {
        let mut builders = Vec::new();
        accumulate_tool_delta(&mut builders, delta(Some(0), Some("c"), "f", "{\"x\":", false));
        // Responses API: the .done event carries the full arguments string.
        accumulate_tool_delta(&mut builders, delta(Some(0), None, "", "{\"x\": 1}", true));
        assert_eq!(builders[0].1.arguments, "{\"x\": 1}");
    }

    #[test]
    fn build_round_response_text_only_matches_completion_shape() {
        let resp = build_round_response(
            "hello".to_owned(),
            Vec::new(),
            serde_json::Map::new(),
            "gpt-4o-mini".to_owned(),
        );
        // extract_content / extract_tool_calls must work unchanged.
        assert_eq!(super::super::helpers::extract_content(&resp).unwrap(), "hello");
        assert!(super::super::helpers::extract_tool_calls(&resp)
            .unwrap()
            .is_empty());
        assert_eq!(resp["model"], "gpt-4o-mini");
        assert!(resp.get("usage").is_none());
    }

    #[test]
    fn build_round_response_with_tool_calls_roundtrips() {
        let mut builders = Vec::new();
        accumulate_tool_delta(&mut builders, delta(Some(0), Some("call_1"), "echo", "", false));
        accumulate_tool_delta(
            &mut builders,
            delta(Some(0), None, "", "{\"msg\":\"hi\"}", false),
        );
        let mut usage = serde_json::Map::new();
        usage.insert("input_tokens".to_owned(), Value::from(10));
        usage.insert("output_tokens".to_owned(), Value::from(5));
        let resp = build_round_response("".to_owned(), builders, usage, "m".to_owned());

        let calls = super::super::helpers::extract_tool_calls(&resp).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["name"], "echo");
        assert_eq!(calls[0]["function"]["arguments"], "{\"msg\":\"hi\"}");
        // Tool-only round: content is null, extract_content yields empty.
        assert_eq!(super::super::helpers::extract_content(&resp).unwrap(), "");
        let event = crate::core::results::UsageEvent::from_raw(resp.get("usage").unwrap(), "m")
            .unwrap();
        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.output_tokens, 5);
    }

    #[test]
    fn splitter_handles_partial_utf8_done_and_comments() {
        let mut s = SseLineSplitter::new();
        // "é" = 0xC3 0xA9 split across chunks; keep-alive comment ignored.
        let mut out = s.push(b"data: {\"v\":\"\xC3");
        assert!(out.is_empty());
        out = s.push(b"\xA9\"}\n: keep-alive\n\ndata: [DONE]\r\n");
        assert_eq!(out.len(), 2);
        match &out[0] {
            SseLine::Data(d) => assert_eq!(d, "{\"v\":\"é\"}"),
            SseLine::Done => panic!("expected data"),
        }
        assert!(matches!(out[1], SseLine::Done));
    }
}
