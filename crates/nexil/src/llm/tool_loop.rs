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
// run_tools helpers (shared across the loop's exit points)
// ---------------------------------------------------------------------------

/// Per-tool-result char cap for the in-memory LLM context. Chosen so that even a
/// long run with many large results stays under the ~400K-char context budget
/// (see `char_limit` in run_tools and `MAX_TOTAL_CONTEXT_CHARS`): ~16 capped
/// results fit. The full, un-truncated result is always persisted to the tape via
/// `maybe_spill_result`; this only bounds what re-enters the model's context.
const MAX_TOOL_RESULT_CONTEXT_CHARS: usize = 24_000;

/// Cap one tool result's content for the LLM context, keeping a head + tail with
/// a truncation marker (the head holds the answer/structure, the tail holds any
/// error/summary). Char-boundary safe. A single multi-MB result (e.g. a big
/// web.fetch or bash dump) would otherwise blow the context window and abort the
/// loop at the `char_limit` guard mid-task — the long-horizon failure mode.
fn cap_tool_result_for_context(content: &str) -> String {
    let total = content.chars().count();
    if total <= MAX_TOOL_RESULT_CONTEXT_CHARS {
        return content.to_string();
    }
    let head_n = MAX_TOOL_RESULT_CONTEXT_CHARS * 3 / 4;
    let tail_n = MAX_TOOL_RESULT_CONTEXT_CHARS - head_n;
    let head: String = content.chars().take(head_n).collect();
    let tail: String = content.chars().skip(total - tail_n).collect();
    let omitted = total - head_n - tail_n;
    format!(
        "{head}\n\n…[{omitted} chars truncated for context; full result preserved in the tape]…\n\n{tail}"
    )
}

/// Build a terminal text [`ToolAutoResult`], carrying the tool calls/results
/// and usage accumulated so far. Shared by normal text completion and every
/// early clean stop (cancellation, context-window limit, budget exhaustion) so
/// the loop has exactly one way to finish with text.
fn text_result(
    text: impl Into<String>,
    tool_calls: Vec<Value>,
    tool_results: Vec<Value>,
    usage: Vec<UsageEvent>,
) -> ToolAutoResult {
    ToolAutoResult {
        kind: ToolAutoResultKind::Text,
        text: Some(text.into()),
        tool_calls,
        tool_results,
        error: None,
        usage,
    }
}

/// Total input+output tokens recorded across all usage events this turn.
fn tokens_spent(usage: &[UsageEvent]) -> u64 {
    usage.iter().map(UsageEvent::total_tokens).sum()
}

/// Whether this turn's accumulated usage has reached the optional token
/// `budget`. A `None` budget is unlimited and never trips (the sum is skipped).
fn turn_budget_exhausted(usage: &[UsageEvent], budget: Option<u64>) -> bool {
    budget.is_some_and(|limit| tokens_spent(usage) >= limit)
}

/// Append an ephemeral tail reminder to the round's messages. Merges into a
/// trailing plain-text user message when present (avoids illegal consecutive
/// user turns), otherwise pushes a new user message. Operates on the transient
/// per-round message list only — never the tape.
fn append_tail_reminder(msgs: &mut Vec<Value>, reminder: &str) {
    if let Some(last) = msgs.last_mut()
        && last.get("role").and_then(|r| r.as_str()) == Some("user")
        && let Some(content) = last.get("content").and_then(|c| c.as_str())
    {
        last["content"] = Value::String(format!("{content}\n\n{reminder}"));
        return;
    }
    msgs.push(serde_json::json!({"role": "user", "content": reminder}));
}

/// Recovery prompt injected when the model gives up right after a tool error.
/// Grounding it in the actual error (kind + message) gives the model a concrete
/// signal to self-correct instead of a generic "try again".
fn recovery_nudge_text(last_error: Option<&str>) -> String {
    match last_error {
        Some(err) => format!(
            "The previous tool call failed with: {err}\n\nFix the cause — check the \
             arguments and tool name, or use an alternative tool — then continue. \
             Do not give up."
        ),
        None => "The previous tool call failed. Try a different approach or use \
                 alternative tools to accomplish the task. Do not give up."
            .to_owned(),
    }
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
        let response = self
            .core
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
                |resp: TransportResponse| Ok(resp.payload),
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
            token_budget,
            tail_reminder,
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
        let mut last_error: Option<String> = None;
        let mut recovery_nudges: u8 = 0;
        const MAX_RECOVERY_NUDGES: u8 = 1;

        loop {
            iteration += 1;

            if cancellation.as_ref().is_some_and(|t| t.is_cancelled()) {
                tracing::info!(iteration, "run_tools cancelled");
                return Ok(text_result(
                    "[Cancelled]",
                    all_tool_calls,
                    all_tool_results,
                    usage_events,
                ));
            }

            if iteration > max_iterations {
                return Err(ConduitError::new(
                    ErrorKind::Unknown,
                    format!("run_tools exceeded max iterations ({})", max_iterations),
                ));
            }

            // Cost circuit breaker: stop before the next model call once this
            // turn's accumulated token usage reaches the budget. The first round
            // always runs (no prior usage); later rounds are gated by the cost of
            // earlier ones, bounding a runaway loop's spend.
            if turn_budget_exhausted(&usage_events, token_budget) {
                let spent = tokens_spent(&usage_events);
                tracing::warn!(
                    iteration,
                    spent,
                    budget = ?token_budget,
                    "tool loop stopped: per-turn token budget reached"
                );
                return Ok(text_result(
                    format!(
                        "Tool loop stopped: per-turn token budget reached \
                         ({spent} tokens used). Please continue in a new turn or session."
                    ),
                    all_tool_calls,
                    all_tool_results,
                    usage_events,
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
            // Re-surface the live plan at the tail every round (ephemeral; not
            // persisted), so it stays in the model's most-attended position.
            if let Some(ref reminder) = tail_reminder {
                append_tail_reminder(&mut msgs, reminder);
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
                            "content": recovery_nudge_text(last_error.as_deref()),
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

                    return Ok(text_result(
                        content,
                        all_tool_calls,
                        all_tool_results,
                        usage_events,
                    ));
                }
                ToolRoundOutcome::Tools {
                    response,
                    execution,
                } => {
                    last_round_had_errors = execution.error.is_some();
                    last_error = execution.error.as_ref().map(|e| e.to_string());
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
                            return Ok(text_result(
                                "Tool loop stopped: approaching context window limit. \
                                 Please continue in a new turn or session.",
                                all_tool_calls,
                                all_tool_results,
                                usage_events,
                            ));
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
        let response = self
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
                |resp: TransportResponse| Ok(resp.payload),
            )
            .await?;

        let model_name = response
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(params.model.unwrap_or("unknown"));
        let usage_event = response
            .get("usage")
            .and_then(|raw| UsageEvent::from_raw(raw, model_name));
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
                            |resp: TransportResponse| Ok(resp.payload),
                        )
                        .await?;
                    let retry_content = extract_content(&retry_response)?;
                    let retry_usage = retry_response
                        .get("usage")
                        .and_then(|raw| UsageEvent::from_raw(raw, model_name));
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
        let assistant_reasoning = assistant_msg
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let assistant_text = assistant_msg
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
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
            // Cap the per-result content fed back into the LLM context. The full
            // result is still persisted to the tape (maybe_spill_result below);
            // this only bounds the in-memory context so a single huge result
            // (e.g. a multi-MB web.fetch / bash dump) cannot blow the context
            // window and abort a long-horizon run at the `char_limit` guard.
            let content_str = cap_tool_result_for_context(&content_str);
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
            let entry = TapeEntry::tool_call_with_assistant_fields(
                spilled_calls,
                assistant_text,
                assistant_reasoning,
                meta.clone(),
            );
            self.async_tape.append_entry(tape_name, &entry).await?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(input: u64, output: u64) -> UsageEvent {
        UsageEvent {
            model: "test".to_owned(),
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            timestamp: String::new(),
        }
    }

    #[test]
    fn tokens_spent_sums_input_and_output() {
        assert_eq!(tokens_spent(&[]), 0);
        assert_eq!(tokens_spent(&[usage(10, 5), usage(3, 2)]), 20);
    }

    #[test]
    fn unlimited_budget_never_trips() {
        // None budget is the default; behavior must match the pre-circuit-breaker
        // loop exactly, even at absurd usage.
        assert!(!turn_budget_exhausted(&[usage(1_000_000, 1_000_000)], None));
    }

    #[test]
    fn first_round_always_runs_under_budget() {
        // No usage recorded yet ⇒ never exhausted, so the first model call is
        // never blocked regardless of how small the budget is.
        assert!(!turn_budget_exhausted(&[], Some(1)));
    }

    #[test]
    fn budget_trips_at_or_above_limit() {
        let events = [usage(10, 5)]; // 15 tokens spent
        assert!(!turn_budget_exhausted(&events, Some(16)));
        assert!(turn_budget_exhausted(&events, Some(15))); // reached
        assert!(turn_budget_exhausted(&events, Some(10))); // exceeded
    }

    #[test]
    fn tail_reminder_merges_into_trailing_user_text() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "do the thing"}),
        ];
        append_tail_reminder(&mut msgs, "[Active tasks: #1]");
        assert_eq!(msgs.len(), 2, "should merge, not push");
        let content = msgs[1]["content"].as_str().unwrap();
        assert!(content.contains("do the thing"));
        assert!(content.contains("[Active tasks: #1]"));
    }

    #[test]
    fn tail_reminder_pushes_when_trailing_not_user_text() {
        // Trailing assistant message (or array content) ⇒ push a fresh user
        // message rather than create an illegal consecutive-user merge target.
        let mut msgs = vec![json!({"role": "assistant", "content": "ok"})];
        append_tail_reminder(&mut msgs, "[Active tasks: #1]");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "[Active tasks: #1]");
    }

    #[test]
    fn recovery_nudge_grounds_in_the_actual_error() {
        let grounded = recovery_nudge_text(Some("[tool] Tool 'bash' execution failed: boom"));
        assert!(grounded.contains("execution failed: boom"));
        assert!(grounded.contains("Do not give up"));
        // Falls back to a generic prompt when no error text is available.
        let generic = recovery_nudge_text(None);
        assert!(!generic.contains("failed with:"));
        assert!(generic.contains("Do not give up"));
    }

    #[test]
    fn text_result_carries_accumulated_work() {
        let r = text_result(
            "done",
            vec![json!({"call": 1})],
            vec![json!({"res": 1})],
            vec![usage(2, 3)],
        );
        assert_eq!(r.kind, ToolAutoResultKind::Text);
        assert_eq!(r.text.as_deref(), Some("done"));
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_results.len(), 1);
        assert_eq!(r.usage.len(), 1);
        assert!(r.error.is_none());
    }
}
