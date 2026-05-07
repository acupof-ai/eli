//! Streaming chat completion.

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::clients::parsing::parser_for_transport;
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

    /// Consume SSE bytes from the response, parse text chunks, and forward
    /// them through `tx`. Respects an optional `CancellationToken` — when
    /// cancelled the loop stops and the channel closes, delivering whatever
    /// partial content was already sent.
    async fn stream_sse_loop(
        response: reqwest::Response,
        parser: &'static dyn crate::clients::parsing::types::BaseTransportParser,
        tx: tokio::sync::mpsc::Sender<String>,
        cancellation: Option<CancellationToken>,
    ) {
        use futures::StreamExt;

        let mut byte_stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();

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
            buffer.extend_from_slice(&bytes);

            // Parse complete SSE lines from the byte buffer, leaving
            // partial lines (which may contain incomplete multibyte
            // UTF-8 sequences) for the next chunk.
            let mut cursor = 0;
            while let Some(rel) = buffer[cursor..].iter().position(|&b| b == b'\n') {
                let line_end = cursor + rel;
                let mut end = line_end;
                if end > cursor && buffer[end - 1] == b'\r' {
                    end -= 1;
                }
                let line = String::from_utf8_lossy(&buffer[cursor..end]);
                cursor = line_end + 1;

                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        break;
                    }
                    if let Ok(val) = serde_json::from_str::<Value>(data) {
                        let content = parser.extract_chunk_text(&val);
                        if !content.is_empty() && tx.send(content).await.is_err() {
                            return;
                        }
                    }
                }
            }
            // Remove consumed bytes in one operation instead of per-line.
            if cursor > 0 {
                buffer.drain(..cursor);
            }
        }
    }
}
