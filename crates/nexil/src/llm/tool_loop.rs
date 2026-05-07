//! Tool-calling loop — `run_tools`, `tool_calls`, and supporting helpers.

use serde_json::Value;
use uuid::Uuid;

use crate::core::errors::{ConduitError, ErrorKind};
use crate::core::response_parser::TransportResponse;
use crate::core::results::{ToolAutoResult, ToolAutoResultKind, ToolExecution, UsageEvent};
use crate::tape::entries::TapeEntry;
use crate::tape::spill::{self, DEFAULT_SPILL};
use crate::tape::{TapeContext, build_messages as tape_build_messages};
use crate::tools::context::ToolContext;
use crate::tools::executor::ToolCallResponse;
use crate::tools::schema::ToolSet;

use super::{
    LLM, build_assistant_tool_call_message, build_full_context_from_entries, build_messages,
    collect_active_decisions, extract_content, extract_tool_calls,
    inject_decisions_into_system_prompt, restore_last_user_content, slice_entries_by_anchor,
    strip_image_blocks_for_persistence,
};

// ---------------------------------------------------------------------------
// Internal types for run_tools decomposition
// ---------------------------------------------------------------------------

/// Parameters for a single tool-calling round (avoids too-many-arguments).
pub(super) struct RoundParams<'a> {
    pub schemas: &'a Option<Vec<Value>>,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub max_tokens: Option<u32>,
    pub tools: &'a ToolSet,
    pub tool_context: Option<&'a ToolContext>,
    pub session_id: Option<&'a str>,
}

/// Result of a single tool-calling round.
pub(super) struct ToolRound {
    pub usage_event: Option<UsageEvent>,
    pub outcome: ToolRoundOutcome,
}

/// Whether the model returned text (done) or tool calls (continue looping).
pub(super) enum ToolRoundOutcome {
    /// Model returned a text response — no more tool calls.
    Text(String),
    /// Model returned tool calls that were executed.
    Tools {
        response: Value,
        execution: ToolExecution,
    },
}

// ---------------------------------------------------------------------------
// impl LLM — tool calling
// ---------------------------------------------------------------------------

impl LLM {
    /// Get tool calls from the model without executing them.
    pub async fn tool_calls(
        &mut self,
        req: super::ChatRequest<'_>,
    ) -> Result<Vec<Value>, ConduitError> {
        let super::ChatRequest {
            prompt,
            user_content,
            system_prompt,
            model,
            provider,
            messages,
            max_tokens,
            tools,
            session_id,
            ..
        } = req;
        let tools = tools.ok_or_else(|| {
            ConduitError::new(ErrorKind::InvalidInput, "tool_calls requires tools")
        })?;
        let msgs = build_messages(
            prompt,
            user_content.as_deref(),
            system_prompt,
            messages.as_deref(),
        );
        let schemas = tools.payload().map(|s| s.to_vec());
        let response =
            self.core
                .run_chat(
                    msgs,
                    schemas,
                    model,
                    provider,
                    max_tokens,
                    false,
                    None,
                    Default::default(),
                    session_id,
                    |resp: TransportResponse, _prov: &str, _model: &str, _attempt: u32| {
                        Ok(resp.payload)
                    },
                )
                .await?;

        extract_tool_calls(&response)
    }

    /// Get tool calls and execute them against the provided tools.
    pub async fn run_tools(
        &mut self,
        req: super::ChatRequest<'_>,
    ) -> Result<ToolAutoResult, ConduitError> {
        let super::ChatRequest {
            prompt,
            user_content,
            system_prompt,
            model,
            provider,
            messages,
            max_tokens,
            tools,
            tool_context: context,
            tape,
            tape_context,
            cancellation,
            context_window,
            max_tool_iterations,
            session_id,
        } = req;
        let tools = tools.ok_or_else(|| {
            ConduitError::new(ErrorKind::InvalidInput, "run_tools requires tools")
        })?;
        let schemas = tools.payload().map(|s| s.to_vec());

        let mut all_tool_calls: Vec<Value> = Vec::new();
        let mut all_tool_results: Vec<Value> = Vec::new();
        let mut usage_events: Vec<UsageEvent> = Vec::new();

        let initial_round_msgs = build_messages(
            prompt,
            user_content.as_deref(),
            system_prompt,
            messages.as_deref(),
        );
        let mut in_memory_msgs = initial_round_msgs.clone();

        if let Some(tape_name) = tape
            && !initial_round_msgs.is_empty()
        {
            self.persist_initial_messages(tape_name, &initial_round_msgs)
                .await?;
        }

        let round_params = RoundParams {
            schemas: &schemas,
            model,
            provider,
            max_tokens,
            tools,
            tool_context: context,
            session_id,
        };

        let max_iterations: usize = max_tool_iterations.unwrap_or(250);
        // Resolve the effective context window: prefer request-level, then LLM-level.
        let effective_context_window = context_window.or(self.context_window);
        let mut iteration: usize = 0;
        let mut last_round_had_errors = false;
        let mut recovery_nudges: u8 = 0;
        const MAX_RECOVERY_NUDGES: u8 = 1;

        loop {
            iteration += 1;

            if cancellation.as_ref().is_some_and(|t| t.is_cancelled()) {
                tracing::info!(iteration, "run_tools cancelled");
                return Ok(ToolAutoResult {
                    kind: ToolAutoResultKind::Text,
                    text: Some("[Cancelled]".to_owned()),
                    tool_calls: all_tool_calls,
                    tool_results: all_tool_results,
                    error: None,
                    usage: usage_events,
                });
            }

            if iteration > max_iterations {
                return Err(ConduitError::new(
                    ErrorKind::Unknown,
                    format!("run_tools exceeded max iterations ({})", max_iterations),
                ));
            }

            // Build context from tape (includes history + current turn).
            // On the first iteration only, restore the original multimodal
            // user content (images) that was stripped during tape persistence.
            // Subsequent iterations don't need images again — the model's own
            // response already captured the image content in text form.
            let mut msgs = self
                ._prepare_messages(tape, tape_context, &in_memory_msgs)
                .await?;
            if iteration == 1
                && let Some(ref parts) = user_content
            {
                restore_last_user_content(&mut msgs, parts);
            }

            let round = self._execute_tool_round(&msgs, &round_params).await?;

            if let Some(event) = round.usage_event {
                usage_events.push(event);
            }

            match round.outcome {
                ToolRoundOutcome::Text(content) => {
                    // If the model gave up right after tool errors and we haven't
                    // nudged yet, inject a recovery prompt and let it try again.
                    if last_round_had_errors && recovery_nudges < MAX_RECOVERY_NUDGES {
                        recovery_nudges += 1;
                        last_round_had_errors = false;
                        tracing::info!(
                            iteration,
                            nudge = recovery_nudges,
                            "model returned text after tool error — injecting recovery nudge"
                        );
                        let nudge = serde_json::json!({
                            "role": "user",
                            "content": "The previous tool call failed. \
                                Try a different approach or use alternative tools \
                                to accomplish the task. Do not give up."
                        });
                        in_memory_msgs.push(nudge.clone());
                        if let Some(tape_name) = tape {
                            let meta = serde_json::json!({ "run_id": Uuid::new_v4().to_string() });
                            self.async_tape
                                .append_entry(tape_name, &TapeEntry::message(nudge, meta))
                                .await?;
                        }
                        continue;
                    }

                    if let Some(tape_name) = tape {
                        let meta = serde_json::json!({ "run_id": Uuid::new_v4().to_string() });
                        let assistant_msg =
                            serde_json::json!({"role": "assistant", "content": &content});
                        self.async_tape
                            .append_entry(tape_name, &TapeEntry::message(assistant_msg, meta))
                            .await?;
                    }

                    return Ok(ToolAutoResult {
                        kind: ToolAutoResultKind::Text,
                        text: Some(content),
                        tool_calls: all_tool_calls,
                        tool_results: all_tool_results,
                        error: None,
                        usage: usage_events,
                    });
                }
                ToolRoundOutcome::Tools {
                    response,
                    execution,
                } => {
                    last_round_had_errors = execution.error.is_some();
                    all_tool_calls.extend(execution.tool_calls.clone());
                    all_tool_results.extend(execution.tool_results.clone());
                    self._persist_round(tape, &response, &execution, &mut in_memory_msgs)
                        .await?;

                    // Check if accumulated context approaches the model's window.
                    if let Some(cw) = effective_context_window {
                        let total_chars: usize = in_memory_msgs
                            .iter()
                            .map(|m| {
                                m.get("content")
                                    .and_then(|c| c.as_str())
                                    .map_or(0, str::len)
                            })
                            .sum();
                        // Use ~4 chars/token as a rough estimate; break at 80%.
                        let char_limit = cw * 4 * 80 / 100;
                        if total_chars > char_limit {
                            tracing::warn!(
                                iteration,
                                total_chars,
                                char_limit,
                                context_window = cw,
                                "tool loop stopped: approaching context window limit"
                            );
                            return Ok(ToolAutoResult {
                                kind: ToolAutoResultKind::Text,
                                text: Some(
                                    "Tool loop stopped: approaching context window limit. \
                                     Please continue in a new turn or session."
                                        .to_owned(),
                                ),
                                tool_calls: all_tool_calls,
                                tool_results: all_tool_results,
                                error: None,
                                usage: usage_events,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Build conversation messages from a tape, including decision injection.
    ///
    /// Reads the full tape once, applies anchor slicing in memory for context,
    /// then injects active decisions from the full tape into the system prompt.
    /// Respects custom `TapeContext.select` when set.
    pub(super) async fn build_tape_messages(
        &self,
        tape_name: &str,
        tape_context: Option<&TapeContext>,
    ) -> Vec<Value> {
        let full_query = self.async_tape.query_tape(tape_name);
        let all_entries = match self.async_tape.fetch_entries(&full_query).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!(error = %e, tape = %tape_name, "failed to read tape entries");
                return Vec::new();
            }
        };

        let default_ctx = self.async_tape.default_context().clone();
        let ctx = tape_context.unwrap_or(&default_ctx);
        let sliced = slice_entries_by_anchor(&all_entries, &ctx.anchor);

        let mut tape_msgs = if ctx.select.is_some() {
            tape_build_messages(&sliced, ctx)
        } else {
            build_full_context_from_entries(&sliced)
        };

        let decisions = collect_active_decisions(&all_entries);
        inject_decisions_into_system_prompt(&mut tape_msgs, &decisions);
        crate::tape::context::apply_context_budget(&mut tape_msgs, self.context_window);
        tape_msgs
    }

    pub(super) async fn _prepare_messages(
        &self,
        tape: Option<&str>,
        tape_context: Option<&TapeContext>,
        in_memory_msgs: &[Value],
    ) -> Result<Vec<Value>, ConduitError> {
        if let Some(tape_name) = tape {
            Ok(self.build_tape_messages(tape_name, tape_context).await)
        } else {
            Ok(in_memory_msgs.to_vec())
        }
    }

    pub(super) async fn persist_initial_messages(
        &self,
        tape_name: &str,
        initial_round_msgs: &[Value],
    ) -> Result<(), ConduitError> {
        let run_id = Uuid::new_v4().to_string();
        let meta = serde_json::json!({ "run_id": run_id });

        for message in initial_round_msgs {
            let role = message.get("role").and_then(|v| v.as_str());
            if role == Some("system")
                && let Some(content) = message.get("content").and_then(|v| v.as_str())
            {
                self.async_tape
                    .append_system_if_changed(tape_name, content, meta.clone())
                    .await?;
            } else {
                let persisted = strip_image_blocks_for_persistence(message);
                self.async_tape
                    .append_entry(tape_name, &TapeEntry::message(persisted, meta.clone()))
                    .await?;
            }
        }

        Ok(())
    }

    pub(super) async fn _execute_tool_round(
        &mut self,
        msgs: &[Value],
        params: &RoundParams<'_>,
    ) -> Result<ToolRound, ConduitError> {
        let response =
            self.core
                .run_chat(
                    msgs.to_vec(),
                    params.schemas.clone(),
                    params.model,
                    params.provider,
                    params.max_tokens,
                    false,
                    None,
                    Default::default(),
                    params.session_id,
                    |resp: TransportResponse, _prov: &str, _model: &str, _attempt: u32| {
                        Ok(resp.payload)
                    },
                )
                .await?;

        let model_name = response
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(params.model.unwrap_or("unknown"));
        let usage_event = response
            .get("usage")
            .and_then(|raw| UsageEvent::from_raw(raw, model_name, 0, true));
        let raw_calls = extract_tool_calls(&response)?;

        if raw_calls.is_empty() {
            let content = extract_content(&response)?;
            // Detect empty output with consumed tokens (known GPT-5 bug / content filter).
            // Retry once before giving up.
            if content.is_empty() {
                let used_tokens = response
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                if used_tokens > 0 {
                    tracing::warn!(
                        output_tokens = used_tokens,
                        "empty output with non-zero tokens — retrying once"
                    );
                    let retry_response = self
                        .core
                        .run_chat(
                            msgs.to_vec(),
                            params.schemas.clone(),
                            params.model,
                            params.provider,
                            params.max_tokens,
                            false,
                            None,
                            Default::default(),
                            params.session_id,
                            |resp: TransportResponse, _prov: &str, _model: &str, _attempt: u32| {
                                Ok(resp.payload)
                            },
                        )
                        .await?;
                    let retry_content = extract_content(&retry_response)?;
                    let retry_usage = retry_response
                        .get("usage")
                        .and_then(|raw| UsageEvent::from_raw(raw, model_name, 0, true));
                    return Ok(ToolRound {
                        usage_event: retry_usage,
                        outcome: ToolRoundOutcome::Text(retry_content),
                    });
                }
            }
            return Ok(ToolRound {
                usage_event,
                outcome: ToolRoundOutcome::Text(content),
            });
        }

        let execution = self
            .tool_executor
            .execute_async(
                ToolCallResponse::List(raw_calls),
                &params.tools.runnable,
                params.tool_context,
            )
            .await?;

        if let Some(ref err) = execution.error {
            tracing::warn!(
                error = %err,
                "tool execution error — feeding back to LLM for recovery"
            );
        }

        Ok(ToolRound {
            usage_event,
            outcome: ToolRoundOutcome::Tools {
                response,
                execution,
            },
        })
    }

    pub(super) async fn _persist_round(
        &self,
        tape: Option<&str>,
        response: &Value,
        execution: &ToolExecution,
        in_memory_msgs: &mut Vec<Value>,
    ) -> Result<(), ConduitError> {
        // Always maintain in_memory_msgs with full (unspilled) content so
        // the current run_tools invocation sees complete context.
        let assistant_msg = build_assistant_tool_call_message(response);
        in_memory_msgs.push(assistant_msg);
        for (i, result) in execution.tool_results.iter().enumerate() {
            let call_id = execution
                .tool_calls
                .get(i)
                .and_then(|c| c.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let content_str = match result {
                Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            in_memory_msgs.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": content_str,
            }));
        }

        // Persist to tape with spilled (compact) versions.
        if let Some(tape_name) = tape {
            let meta = serde_json::json!({ "run_id": Uuid::new_v4().to_string() });
            let spilled_calls: Vec<Value> = execution
                .tool_calls
                .iter()
                .map(|call| self.maybe_spill_tool_call(call, tape_name))
                .collect();
            let assistant_text = in_memory_msgs
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
                .and_then(|m| m.get("content"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            self.async_tape
                .append_entry(
                    tape_name,
                    &TapeEntry::tool_call_with_content(spilled_calls, assistant_text, meta.clone()),
                )
                .await?;

            let paired: Vec<Value> = execution
                .tool_calls
                .iter()
                .zip(execution.tool_results.iter())
                .map(|(call, result)| {
                    let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let output = self.maybe_spill_result(result, tape_name, call_id);
                    serde_json::json!({"call_id": call_id, "output": output})
                })
                .collect();
            self.async_tape
                .append_entry(tape_name, &TapeEntry::tool_result(paired, meta))
                .await?;
        }
        Ok(())
    }

    /// If spill is configured and `text` is large, write the full content to
    /// a spill file and return the truncated version. The `suffix` distinguishes
    /// args vs results (e.g. `"call_123"` or `"call_123.args"`).
    pub(super) fn maybe_spill(
        &self,
        text: &str,
        tape_name: &str,
        file_stem: &str,
    ) -> Option<String> {
        let base_dir = self.spill_dir.as_ref()?;
        let dir = spill::spill_dir_for_tape(base_dir, tape_name);
        match spill::spill_if_needed(text, file_stem, &dir, &DEFAULT_SPILL) {
            Ok(spilled) => spilled,
            Err(e) => {
                tracing::warn!(error = %e, file_stem, "failed to spill to disk");
                None
            }
        }
    }

    /// Spill a tool result value if it's a large string.
    pub(super) fn maybe_spill_result(
        &self,
        result: &Value,
        tape_name: &str,
        call_id: &str,
    ) -> Value {
        let Some(text) = result.as_str() else {
            return result.clone();
        };
        match self.maybe_spill(text, tape_name, call_id) {
            Some(truncated) => Value::String(truncated),
            None => result.clone(),
        }
    }

    /// Spill tool call arguments if the arguments string is large.
    /// Returns a new tool call with truncated arguments, or the original.
    pub(super) fn maybe_spill_tool_call(&self, call: &Value, tape_name: &str) -> Value {
        let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let Some(func) = call.get("function") else {
            return call.clone();
        };
        let Some(args_str) = func.get("arguments").and_then(|v| v.as_str()) else {
            return call.clone();
        };

        let file_stem = format!("{call_id}.args");
        match self.maybe_spill(args_str, tape_name, &file_stem) {
            Some(truncated) => {
                let mut new_call = call.clone();
                new_call["function"]["arguments"] = Value::String(truncated);
                new_call
            }
            None => call.clone(),
        }
    }
}
