//! Agent execution loop and command dispatch.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use chrono::Utc;
use nexil::core::results::ToolAutoResultKind;
use nexil::llm::{ChatRequest, LLM};
use nexil::{
    AnchorSelector, ConduitError, ErrorKind, TapeContext, TapeEntry, TapeEntryKind, TapeQuery,
    ToolAutoResult,
};
use serde_json::Value;

use crate::builtin::settings::AgentSettings;
use crate::builtin::tape::TapeService;
use crate::builtin::tools::with_tape_runtime;
use crate::types::PromptValue;

use super::agent_request::{
    build_tool_context, create_llm, lookup_registered_tool, run_tools_once, system_prompt_for_turn,
};

// ---------------------------------------------------------------------------
// Command parsing (inlined from agent_command)
// ---------------------------------------------------------------------------

fn parse_internal_command(line: &str) -> (String, Vec<String>) {
    let parts: Vec<String> = shell_words::split(line)
        .unwrap_or_else(|_| line.split_whitespace().map(|s| s.to_owned()).collect());
    if parts.is_empty() {
        return (String::new(), Vec::new());
    }
    let name = parts[0].clone();
    let rest = parts[1..].to_vec();
    (name, rest)
}

fn parse_args_to_json(tokens: &[String]) -> Value {
    let kwargs: serde_json::Map<String, Value> = tokens
        .iter()
        .filter_map(|t| {
            t.find('=').map(|pos| {
                let key = t[..pos].to_owned();
                let raw = &t[pos + 1..];
                let value = parse_scalar(raw);
                (key, value)
            })
        })
        .collect();

    if !kwargs.is_empty() {
        tokens
            .iter()
            .filter(|t| !t.contains('='))
            .for_each(|t| tracing::warn!("positional argument '{t}' after keyword arguments"));
        return Value::Object(kwargs);
    }

    let joined: String = tokens
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let mut map = serde_json::Map::new();
    if !joined.is_empty() {
        map.insert("value".to_owned(), Value::String(joined));
    }
    Value::Object(map)
}

/// Parse a scalar string value into the appropriate JSON type.
///
/// Handles booleans ("true"/"false"), integers, floats, JSON objects/arrays,
/// and falls back to strings. For objects/arrays, also supports the unquoted
/// form produced when the shell strips double quotes (e.g. `{FOO:bar}`).
fn parse_scalar(raw: &str) -> Value {
    match raw {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => {
            if let Ok(n) = raw.parse::<i64>() {
                Value::from(n)
            } else if let Ok(n) = raw.parse::<f64>() {
                Value::from(n)
            } else if (raw.starts_with('{') && raw.ends_with('}'))
                || (raw.starts_with('[') && raw.ends_with(']'))
            {
                parse_relaxed_json(raw)
            } else {
                Value::String(raw.to_owned())
            }
        }
    }
}

/// Parse a JSON-like string that may have unquoted keys/values.
///
/// The shell strips double quotes from CLI arguments, so `{"FOO":"bar"}`
/// arrives as `{FOO:bar}`. This function adds quotes around bare identifiers
/// and string values before delegating to `serde_json`.
fn parse_relaxed_json(raw: &str) -> Value {
    // First try strict JSON.
    if let Ok(v) = serde_json::from_str(raw) {
        return v;
    }
    // Fall back: quote bare identifiers (keys and unquoted string values).
    // This handles the common case of {KEY:VALUE,KEY2:VALUE2} where values
    // don't contain spaces or special characters.
    let quoted = raw
        .replace('{', "{\"")
        .replace('}', "\"}")
        .replace('[', "[\"")
        .replace(']', "\"]")
        .replace(':', "\":\"")
        .replace(',', "\",\"");
    serde_json::from_str(&quoted).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

fn command_tool_name(name: &str) -> &str {
    match name {
        "handoff" => "tape.handoff",
        _ => name,
    }
}

// ---------------------------------------------------------------------------
// Internal command execution
// ---------------------------------------------------------------------------

pub(super) async fn run_command(
    tapes: &TapeService,
    tape_name: &str,
    line: &str,
    tool_state: &HashMap<String, Value>,
) -> Result<String, ConduitError> {
    let body = line[1..].trim();
    if body.is_empty() {
        return Err(ConduitError::new(ErrorKind::InvalidInput, "empty command"));
    }

    let (name, arg_tokens) = parse_internal_command(body);
    let start = Instant::now();

    let result = with_tape_runtime(
        tapes.clone(),
        execute_tool_or_bash(&name, &arg_tokens, body, tape_name, tool_state),
    )
    .await;

    let elapsed_ms = start.elapsed().as_millis() as i64;
    record_command_event(body, &name, elapsed_ms, &result);

    match result {
        Ok(val) => Ok(value_to_string(val)),
        Err(e) => Err(e),
    }
}

async fn execute_tool_or_bash(
    name: &str,
    arg_tokens: &[String],
    body: &str,
    tape_name: &str,
    tool_state: &HashMap<String, Value>,
) -> Result<Value, ConduitError> {
    let ctx = build_tool_context("run_command", tape_name, tool_state);
    if let Some(tool) = lookup_registered_tool(command_tool_name(name)) {
        let json_args = parse_args_to_json(arg_tokens);
        let ctx_arg = if tool.context { Some(ctx) } else { None };
        tool.run(json_args, ctx_arg).await
    } else {
        let bash_tool = lookup_registered_tool("bash")
            .ok_or_else(|| ConduitError::new(ErrorKind::Tool, "bash tool not found"))?;
        bash_tool
            .run(serde_json::json!({"cmd": body}), Some(ctx))
            .await
    }
}

fn value_to_string(val: Value) -> String {
    match val {
        Value::String(s) => s,
        other => serde_json::to_string(&other).unwrap_or_default(),
    }
}

fn record_command_event(
    body: &str,
    name: &str,
    elapsed_ms: i64,
    result: &Result<Value, ConduitError>,
) {
    let (status, output) = match result {
        Ok(val) => ("ok", value_to_string(val.clone())),
        Err(e) => ("error", e.message.clone()),
    };
    let event = serde_json::json!({
        "raw": body,
        "name": name,
        "status": status,
        "elapsed_ms": elapsed_ms,
        "output": output,
        "date": Utc::now().to_rfc3339(),
    });
    crate::control_plane::push_save_event("command", event);
}

// ---------------------------------------------------------------------------
// Outbound media extraction
// ---------------------------------------------------------------------------

/// Scan tool results for media file paths and push them to the turn context.
fn extract_outbound_media(tool_results: &[Value]) {
    use crate::control_plane::{
        OutboundMedia, media_type_from_mime, mime_from_extension, push_outbound_media,
    };

    for result in tool_results {
        let obj = match result {
            Value::Object(_) => Some(result.clone()),
            Value::String(s) => serde_json::from_str::<Value>(s).ok(),
            _ => None,
        };
        let Some(obj) = obj else { continue };
        let Some(map) = obj.as_object() else { continue };

        // Only extract from successful results.
        if map.get("success").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }

        // Check known path keys: image_path, path.
        for key in &["image_path", "path"] {
            if let Some(path_str) = map.get(*key).and_then(|v| v.as_str()) {
                let path = Path::new(path_str);
                if path.exists() {
                    let mime = mime_from_extension(path);
                    let media_type = media_type_from_mime(mime);
                    tracing::info!(path = %path_str, media_type, "outbound_media: extracted from tool result");
                    push_outbound_media(OutboundMedia {
                        path: path_str.to_owned(),
                        media_type: media_type.to_owned(),
                        mime_type: mime.to_owned(),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Agent loop helpers
// ---------------------------------------------------------------------------

async fn log_active_decisions(tapes: &TapeService, tape_name: &str) {
    if let Ok(all_entries) = tapes
        .store()
        .fetch_all(&nexil::TapeQuery::new(tape_name))
        .await
    {
        let decisions = nexil::collect_active_decisions(&all_entries);
        if !decisions.is_empty() {
            tracing::info!("Resuming session. Active decisions:");
            for (i, d) in decisions.iter().enumerate() {
                tracing::info!("  {}. {}", i + 1, d);
            }
        }
    }
}

async fn resolve_tape_context_override(
    tapes: &TapeService,
    tape_name: &str,
) -> Option<TapeContext> {
    match tapes.auto_handoff_grace(tape_name).await {
        Ok(Some((remaining, ref prev_anchor))) if remaining > 0 && !prev_anchor.is_empty() => {
            tracing::info!(
                tape = tape_name,
                remaining,
                prev_anchor,
                "auto-handoff grace: using prev anchor"
            );
            Some(TapeContext {
                anchor: AnchorSelector::Named(prev_anchor.clone()),
                ..TapeContext::default()
            })
        }
        _ => None,
    }
}

async fn maybe_auto_handoff(
    tapes: &TapeService,
    tape_name: &str,
    output: &ToolAutoResult,
    response_text: &str,
    settings: &AgentSettings,
    compaction_summary: Option<String>,
) {
    // Prefer a real LLM distillation (when compaction is enabled and one was
    // produced); otherwise fall back to the historical 500-char-prefix summary.
    let summary = compaction_summary.unwrap_or_else(|| response_text.chars().take(500).collect());
    if try_decrement_grace(tapes, tape_name).await {
        // Bug B: new tool results appended during grace can push context over the
        // threshold again. If that happens, don't wait for grace to expire — fire a
        // new handoff immediately so the tape stays manageable.
        if let Some(input_tokens) = should_handoff(output, settings) {
            tracing::warn!(
                tape = tape_name,
                input_tokens,
                "auto-handoff: context re-exceeded threshold during grace period, \
                 triggering immediate handoff"
            );
            place_handoff_anchor(tapes, tape_name, &summary, input_tokens, settings).await;
        }
        return;
    }
    if let Some(input_tokens) = should_handoff(output, settings) {
        place_handoff_anchor(tapes, tape_name, &summary, input_tokens, settings).await;
    }
}

/// Returns true when the error is a context overflow or SSE timeout — both are
/// cases where a handoff is necessary even though we have no model response to
/// use as a summary.
///
/// Only matches against `e.message` (the provider error message), never against
/// user prompt content. Patterns are annotated with the providers that emit them.
fn is_context_or_timeout_error(e: &nexil::ConduitError) -> bool {
    // Use only the error message field, not any user content that may appear
    // elsewhere in a structured error payload.
    let msg = e.message.to_lowercase();

    // OpenAI / Azure OpenAI: error code "context_length_exceeded"
    msg.contains("context_length_exceeded")
    // OpenAI: "This model's maximum context length is N tokens"
    || msg.contains("maximum context length")
    // Anthropic: "prompt is too long"
    || msg.contains("prompt is too long")
    // Anthropic / common: "input too long"
    || msg.contains("input too long")
    // OpenAI / many providers: "context window"
    || msg.contains("context window")
    // Generic: "context length"
    || msg.contains("context length")
    // OpenRouter relay / OpenAI legacy: "context_length" (underscore form)
    || msg.contains("context_length")
    // Gemini / Google: "N tokens exceeds the limit"
    || msg.contains("tokens exceeds")
    // Common: "too many tokens"
    || msg.contains("too many tokens")
    // Cohere: "context limit exceeded"
    || msg.contains("context limit")
    // Mistral / Together AI: "Request too large"
    || msg.contains("request too large")
    // Provider-agnostic SSE network layer error
    || msg.contains("sse_stream_error")
    // Timeout conditions (reqwest / provider-side)
    || msg.contains("timed out")
    || msg.contains("timeout")
}

/// Bug 2: trigger handoff from the error path so that SSE timeouts and context
/// overflow errors (which bypass the normal success branch) still rotate the
/// tape anchor.
async fn maybe_auto_handoff_on_error(
    tapes: &TapeService,
    tape_name: &str,
    error: &nexil::ConduitError,
    settings: &AgentSettings,
) {
    if !is_context_or_timeout_error(error) {
        return;
    }
    let was_in_grace = try_decrement_grace(tapes, tape_name).await;
    tracing::info!(
        tape = tape_name,
        error = %error.message,
        was_in_grace,
        "auto-handoff: triggering from error path (context overflow or timeout)"
    );
    place_handoff_anchor(
        tapes,
        tape_name,
        "[auto-handoff triggered by context overflow or timeout]",
        settings.context_window,
        settings,
    )
    .await;
}

async fn try_decrement_grace(tapes: &TapeService, tape_name: &str) -> bool {
    let Ok(Some((remaining, prev_anchor))) = tapes.auto_handoff_grace(tape_name).await else {
        return false;
    };
    if remaining == 0 {
        return false;
    }
    let new_remaining = remaining - 1;
    let _ = tapes
        .append_event(
            tape_name,
            "auto-handoff.grace",
            serde_json::json!({ "remaining": new_remaining, "prev_anchor": prev_anchor }),
        )
        .await;
    if new_remaining == 0 {
        tracing::info!(
            tape = tape_name,
            "auto-handoff grace ended, context will be trimmed next turn"
        );
    }
    true
}

fn should_handoff(output: &ToolAutoResult, settings: &AgentSettings) -> Option<usize> {
    // Bug 5: use max() across all rounds instead of last() to avoid under-counting
    // in multi-round turns where an earlier round may have had the peak token count.
    let reported = output
        .usage
        .iter()
        .map(|u| u.input_tokens)
        .max()
        .unwrap_or(0) as usize;

    // Bug A: some providers omit usage fields entirely, leaving input_tokens=0 so
    // handoff never fires. Fall back to a char-count estimate from the response
    // content so we still have a signal even without server-side token counts.
    let input_tokens = if reported == 0 {
        let estimated = estimate_tokens_from_output(output);
        if estimated > 0 {
            tracing::warn!(
                estimated_tokens = estimated,
                "auto-handoff: provider did not return usage, \
                 estimating input tokens from response char count"
            );
        }
        estimated
    } else {
        reported
    };

    // Bug 1: default to 40% (was 70%) so large models (1M/200K windows) actually trigger.
    // Override with ELI_HANDOFF_THRESHOLD_PCT (1–99) for custom tuning.
    let pct: usize = std::env::var("ELI_HANDOFF_THRESHOLD_PCT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|p| p.clamp(1, 99))
        .unwrap_or(40);
    let threshold = settings.context_window * pct / 100;
    (input_tokens >= threshold).then_some(input_tokens)
}

/// Estimate input token count from the output content when the provider does
/// not return usage data. Uses a conservative mixed-language heuristic:
///   - ASCII/Latin chars ≈ 4 chars / token
///   - CJK / wide chars  ≈ 1.5 chars / token  (i.e. chars × 2 / 3)
fn estimate_tokens_from_output(output: &ToolAutoResult) -> usize {
    let text = output.text.as_deref().unwrap_or("");
    let tool_str: String = output
        .tool_results
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        })
        .collect::<Vec<_>>()
        .join("");

    let mut ascii = 0usize;
    let mut cjk = 0usize;
    for c in text.chars().chain(tool_str.chars()) {
        if is_cjk_char(c) {
            cjk += 1;
        } else {
            ascii += 1;
        }
    }
    ascii / 4 + cjk * 2 / 3
}

/// Returns true for characters in common CJK / wide-character Unicode blocks.
fn is_cjk_char(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0x3400..=0x4DBF  // CJK Extension A
        | 0x20000..=0x2A6DF// CJK Extension B
        | 0x3000..=0x303F  // CJK Symbols and Punctuation
        | 0xFF00..=0xFFEF  // Halfwidth and Fullwidth Forms
        | 0x3040..=0x309F  // Hiragana
        | 0x30A0..=0x30FF  // Katakana
        | 0xAC00..=0xD7AF  // Hangul Syllables
    )
}

/// Opt-in (`ELI_CONTEXT_COMPACTION=1`): when set, auto-handoff summaries are a
/// real LLM distillation of the work since the last anchor instead of a 500-char
/// prefix of the last reply. Off by default, so behavior is unchanged.
fn compaction_enabled() -> bool {
    std::env::var("ELI_CONTEXT_COMPACTION")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

const COMPACTION_SYSTEM_PROMPT: &str = "\
You are compacting an agent's working context before older turns leave the \
window. Summarize the transcript below into a dense handoff note so work can \
continue without re-reading it. Preserve, in priority order: (1) architecture / \
design decisions; (2) files modified and their key changes; (3) current \
verification status (build/test pass or fail); (4) open TODOs and next steps; \
(5) important facts, constraints, and identifiers. Drop chit-chat and verbose \
tool output, keeping only pass/fail and key values. The full history stays \
retrievable via tape.search. Output only the summary.";

/// Char cap on the digest fed to the summarizer so the compaction call itself
/// can't overflow the window. The most recent content is kept (tail).
const COMPACTION_INPUT_CHAR_CAP: usize = 120_000;
/// Output token budget for the compaction summary.
const COMPACTION_MAX_TOKENS: u32 = 1024;

/// Flatten a message `content` (string or multimodal block array) to text.
fn content_to_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| p.to_string())
            })
            .collect::<Vec<_>>()
            .join(" "),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Keep at most the last `max` chars, prefixing a marker when truncated.
fn tail_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_owned();
    }
    let kept: String = s.chars().skip(count - max).collect();
    format!("[... earlier detail omitted from summary input ...]\n{kept}")
}

/// Render one digest line for an entry that carries summarizable content.
fn entry_digest_line(entry: &TapeEntry) -> Option<String> {
    match entry.kind {
        TapeEntryKind::Message => {
            let role = entry
                .payload
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let content = content_to_text(entry.payload.get("content"));
            (!content.trim().is_empty()).then(|| format!("{role}: {content}"))
        }
        TapeEntryKind::ToolCall => {
            let names: Vec<&str> = entry
                .payload
                .get("calls")
                .and_then(|c| c.as_array())
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|c| {
                            c.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                        })
                        .collect()
                })
                .unwrap_or_default();
            (!names.is_empty()).then(|| format!("assistant called tools: {}", names.join(", ")))
        }
        TapeEntryKind::ToolResult => {
            let preview = content_to_text(entry.payload.get("results"));
            let preview: String = preview.chars().take(500).collect();
            (!preview.trim().is_empty()).then(|| format!("tool result: {preview}"))
        }
        _ => None,
    }
}

/// Build a text digest of the work recorded since the last anchor, capped to the
/// most recent [`COMPACTION_INPUT_CHAR_CAP`] chars.
fn digest_since_last_anchor(entries: &[TapeEntry]) -> String {
    let start = entries
        .iter()
        .rposition(|e| e.kind == TapeEntryKind::Anchor)
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut out = String::new();
    for entry in &entries[start..] {
        if let Some(line) = entry_digest_line(entry) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    tail_chars(&out, COMPACTION_INPUT_CHAR_CAP)
}

/// Summarize the work since the last anchor via a single model call. Returns
/// `None` on any failure (empty span, read error, model error) so the caller
/// falls back to the prefix summary.
async fn summarize_since_anchor(
    llm: &mut LLM,
    tapes: &TapeService,
    tape_name: &str,
) -> Option<String> {
    let entries = tapes
        .store()
        .fetch_all(&TapeQuery::new(tape_name))
        .await
        .ok()?;
    let digest = digest_since_last_anchor(&entries);
    if digest.trim().is_empty() {
        return None;
    }
    let summary = llm
        .chat_async(ChatRequest {
            system_prompt: Some(COMPACTION_SYSTEM_PROMPT),
            prompt: Some(&digest),
            max_tokens: Some(COMPACTION_MAX_TOKENS),
            ..Default::default()
        })
        .await
        .ok()?;
    let trimmed = summary.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// When compaction is enabled and a handoff will fire this turn, produce a real
/// LLM summary of the since-anchor span. `None` ⇒ caller uses the prefix summary.
async fn maybe_make_compaction_summary(
    llm: &mut LLM,
    tapes: &TapeService,
    tape_name: &str,
    result: &Result<ToolAutoResult, ConduitError>,
    settings: &AgentSettings,
) -> Option<String> {
    if !compaction_enabled() {
        return None;
    }
    let output = match result {
        Ok(o) if o.kind == ToolAutoResultKind::Text => o,
        _ => return None,
    };
    // Only spend a summarization call when a handoff is actually going to fire.
    should_handoff(output, settings)?;
    summarize_since_anchor(llm, tapes, tape_name).await
}

async fn place_handoff_anchor(
    tapes: &TapeService,
    tape_name: &str,
    summary: &str,
    input_tokens: usize,
    settings: &AgentSettings,
) {
    let prev_anchor_name = tapes
        .last_anchor_name(tape_name)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    write_handoff_anchor(tapes, tape_name, summary, input_tokens, settings).await;
    write_handoff_summary(tapes, tape_name, summary).await;
    write_handoff_grace(tapes, tape_name, &prev_anchor_name).await;

    tracing::info!(
        tape = tape_name,
        input_tokens,
        context_window = settings.context_window,
        "auto-handoff: anchor placed, grace period 2"
    );
}

async fn write_handoff_anchor(
    tapes: &TapeService,
    tape_name: &str,
    summary: &str,
    input_tokens: usize,
    settings: &AgentSettings,
) {
    let anchor_name = format!("auto-handoff/{}", Utc::now().format("%Y%m%dT%H%M%S"));
    let anchor_state = serde_json::json!({
        "reason": "auto-handoff: context approaching limit",
        "input_tokens": input_tokens,
        "context_window": settings.context_window,
        "summary": summary,
    });
    if let Err(e) = tapes
        .handoff(tape_name, &anchor_name, Some(anchor_state))
        .await
    {
        tracing::warn!(error = %e, "auto-handoff: failed to write anchor");
    }
}

async fn write_handoff_summary(tapes: &TapeService, tape_name: &str, summary: &str) {
    let sys_entry = TapeEntry::system(
        &format!("[Context summary from auto-handoff]\n{summary}"),
        Value::Object(Default::default()),
    );
    if let Err(e) = tapes.store().append(tape_name, &sys_entry).await {
        tracing::warn!(error = %e, "auto-handoff: failed to write summary");
    }
}

async fn write_handoff_grace(tapes: &TapeService, tape_name: &str, prev_anchor_name: &str) {
    if let Err(e) = tapes
        .append_event(
            tape_name,
            "auto-handoff.grace",
            serde_json::json!({ "remaining": 2, "prev_anchor": prev_anchor_name }),
        )
        .await
    {
        tracing::warn!(error = %e, "auto-handoff: failed to write grace event");
    }
}

fn record_run_event(
    elapsed_ms: i64,
    status: &str,
    error: Option<&str>,
    usage: &[nexil::UsageEvent],
    tool_calls: usize,
) {
    let total_input: u64 = usage.iter().map(|u| u.input_tokens).sum();
    let total_output: u64 = usage.iter().map(|u| u.output_tokens).sum();
    let total_tokens = total_input + total_output;
    let cache_read: u64 = usage.iter().map(|u| u.cache_read_input_tokens).sum();
    let cache_write: u64 = usage.iter().map(|u| u.cache_creation_input_tokens).sum();
    // Fraction of prompt tokens served from cache — the prompt-caching win (#2)
    // made observable per turn. Denominator includes cache reads since the
    // provider reports them separately from `input_tokens`.
    let cache_hit_ratio = {
        let prompt = total_input + cache_read + cache_write;
        if prompt == 0 {
            0.0
        } else {
            cache_read as f64 / prompt as f64
        }
    };

    crate::control_plane::record_turn_usage(total_input, total_output, cache_read);

    let mut event = serde_json::json!({
        "elapsed_ms": elapsed_ms,
        "status": status,
        "date": Utc::now().to_rfc3339(),
        "usage": {
            "input_tokens": total_input,
            "output_tokens": total_output,
            "total_tokens": total_tokens,
            "rounds": usage.len(),
            "cache_read_tokens": cache_read,
            "cache_write_tokens": cache_write,
            "cache_hit_ratio": cache_hit_ratio,
        },
        "tool_calls": tool_calls,
    });
    if let Some(err) = error {
        event["error"] = Value::String(err.to_owned());
    }
    // Structured per-turn summary: also emit to tracing so cache savings and
    // tool activity are visible in logs without reading the tape.
    tracing::info!(
        status,
        elapsed_ms,
        input_tokens = total_input,
        output_tokens = total_output,
        cache_read_tokens = cache_read,
        cache_write_tokens = cache_write,
        cache_hit_ratio = format!("{:.2}", cache_hit_ratio),
        tool_calls,
        rounds = usage.len(),
        "agent.run summary"
    );
    crate::control_plane::push_save_event("agent.run", event);
}

// ---------------------------------------------------------------------------
// Verify / self-correct (opt-in, hard-signal grounded)
// ---------------------------------------------------------------------------

/// Hard-signal verification command (`ELI_VERIFY_CMD`, e.g. "cargo test").
/// `None` (default) disables the whole verify loop, so behavior is unchanged.
fn verify_command() -> Option<String> {
    std::env::var("ELI_VERIFY_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Max self-correction re-runs after a failing verification (`ELI_VERIFY_MAX_RETRIES`, default 1).
fn verify_max_retries() -> u32 {
    std::env::var("ELI_VERIFY_MAX_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// Run the verification command in `workspace`. `Ok(())` on exit 0; otherwise
/// `Err` with a tail-capped stdout+stderr the model can act on.
async fn run_verify_command(cmd: &str, workspace: &Path) -> Result<(), String> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .output()
        .await
        .map_err(|e| format!("verify command failed to start: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(tail_chars(&format!("{stdout}\n{stderr}"), 4000))
}

/// Reflexion-style verify loop: when `ELI_VERIFY_CMD` is set, run it after a
/// successful, tool-using turn and — on failure — feed the failure back as a
/// follow-up turn for the model to fix, bounded by `ELI_VERIFY_MAX_RETRIES`.
/// Grounded in a hard external signal (build/test), not LLM self-judgment.
#[allow(clippy::too_many_arguments)]
async fn verify_and_correct(
    llm: &mut LLM,
    tapes: &TapeService,
    tape_name: &str,
    system_prompt: &str,
    tool_state: &HashMap<String, Value>,
    settings: &AgentSettings,
    allowed_tools: Option<&HashSet<String>>,
    tape_ctx: Option<&TapeContext>,
    session_id: &str,
    workspace: &Path,
    mut result: Result<ToolAutoResult, ConduitError>,
) -> Result<ToolAutoResult, ConduitError> {
    let Some(cmd) = verify_command() else {
        return result;
    };
    let max_retries = verify_max_retries();
    let mut attempt = 0;
    loop {
        // Only verify successful turns that actually used tools (a pure-chat
        // turn changed nothing to verify).
        let verifiable = matches!(&result,
            Ok(o) if o.kind == ToolAutoResultKind::Text && !o.tool_calls.is_empty());
        if !verifiable {
            return result;
        }
        match run_verify_command(&cmd, workspace).await {
            Ok(()) => {
                tracing::info!(tape = tape_name, attempt, "verify passed");
                return result;
            }
            Err(failure) if attempt < max_retries => {
                attempt += 1;
                tracing::warn!(
                    tape = tape_name,
                    attempt,
                    "verify failed — re-running for self-correction"
                );
                let _ = tapes
                    .append_event(
                        tape_name,
                        "agent.verify.failed",
                        serde_json::json!({ "attempt": attempt, "cmd": cmd }),
                    )
                    .await;
                let followup = PromptValue::Text(format!(
                    "Verification failed. The command `{cmd}` reported:\n\n{failure}\n\n\
                     Fix the cause and continue. Do not claim success until it passes."
                ));
                result = with_tape_runtime(
                    tapes.clone(),
                    run_tools_once(
                        llm,
                        system_prompt,
                        tape_name,
                        &followup,
                        tool_state,
                        settings,
                        allowed_tools,
                        tape_ctx,
                        session_id,
                    ),
                )
                .await;
            }
            Err(_failure) => {
                tracing::warn!(
                    tape = tape_name,
                    attempt,
                    "verify still failing after max retries"
                );
                return result;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Agent loop
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(super) async fn agent_loop(
    tapes: &TapeService,
    tape_name: &str,
    initial_prompt: PromptValue,
    settings: &AgentSettings,
    model: Option<&str>,
    state: &HashMap<String, Value>,
    allowed_skills: Option<&HashSet<String>>,
    allowed_tools: Option<&HashSet<String>>,
    tool_state: &HashMap<String, Value>,
    workspace: &Path,
    session_id: &str,
) -> Result<String, ConduitError> {
    let mut llm = create_llm(settings, model, tapes.store().clone())?;
    let prompt_text = initial_prompt.strict_text();
    let system_prompt =
        system_prompt_for_turn(settings, &prompt_text, state, allowed_skills, workspace);
    let display_model = model.unwrap_or(&settings.model);

    let start = Instant::now();
    tracing::info!(tape = tape_name, model = display_model, "agent.run");
    let _ = tapes
        .append_event(
            tape_name,
            "agent.run.start",
            serde_json::json!({"prompt": prompt_text}),
        )
        .await;

    log_active_decisions(tapes, tape_name).await;
    let tape_ctx_override = resolve_tape_context_override(tapes, tape_name).await;

    let result = with_tape_runtime(
        tapes.clone(),
        run_tools_once(
            &mut llm,
            &system_prompt,
            tape_name,
            &initial_prompt,
            tool_state,
            settings,
            allowed_tools,
            tape_ctx_override.as_ref(),
            session_id,
        ),
    )
    .await;

    // Hard-signal verify / self-correct loop (opt-in via ELI_VERIFY_CMD; a no-op
    // otherwise, so normal turns are unaffected).
    let result = verify_and_correct(
        &mut llm,
        tapes,
        tape_name,
        &system_prompt,
        tool_state,
        settings,
        allowed_tools,
        tape_ctx_override.as_ref(),
        session_id,
        workspace,
        result,
    )
    .await;

    // Generate the LLM compaction summary here, where `llm` is in scope, before
    // the result is moved into process_agent_result. No-op unless compaction is
    // enabled and a handoff will fire this turn.
    let compaction_summary =
        maybe_make_compaction_summary(&mut llm, tapes, tape_name, &result, settings).await;

    let elapsed_ms = start.elapsed().as_millis() as i64;
    process_agent_result(
        tapes,
        tape_name,
        result,
        elapsed_ms,
        settings,
        compaction_summary,
    )
    .await
}

async fn process_agent_result(
    tapes: &TapeService,
    tape_name: &str,
    result: Result<ToolAutoResult, ConduitError>,
    elapsed_ms: i64,
    settings: &AgentSettings,
    compaction_summary: Option<String>,
) -> Result<String, ConduitError> {
    match result {
        Err(e) => {
            tracing::warn!(
                tape = tape_name,
                elapsed_ms,
                error = %e.message,
                "agent.run finished with error"
            );
            record_run_event(elapsed_ms, "error", Some(&e.message), &[], 0);
            // Bug 2: context overflow and SSE timeouts never reach the success
            // branch, so check them here and trigger handoff if needed.
            maybe_auto_handoff_on_error(tapes, tape_name, &e, settings).await;
            Err(e)
        }
        Ok(ref output) if output.kind == ToolAutoResultKind::Text => {
            let text = output.text.clone().unwrap_or_default();
            tracing::info!(
                tape = tape_name,
                elapsed_ms,
                text_len = text.len(),
                tool_calls = output.tool_calls.len(),
                "agent.run finished ok"
            );
            extract_outbound_media(&output.tool_results);
            record_run_event(
                elapsed_ms,
                "ok",
                None,
                &output.usage,
                output.tool_calls.len(),
            );
            maybe_auto_handoff(
                tapes,
                tape_name,
                output,
                &text,
                settings,
                compaction_summary,
            )
            .await;
            Ok(text)
        }
        Ok(ref output) => {
            let error_msg = output
                .error
                .as_ref()
                .map(|e| format!("{}: {}", e.kind.as_str(), e.message))
                .unwrap_or_else(|| "tool_auto_error: unknown".to_owned());
            tracing::warn!(
                tape = tape_name,
                elapsed_ms,
                error = %error_msg,
                kind = ?output.kind,
                "agent.run finished with non-text result"
            );
            record_run_event(
                elapsed_ms,
                "error",
                Some(&error_msg),
                &output.usage,
                output.tool_calls.len(),
            );
            Err(ConduitError::new(ErrorKind::Unknown, error_msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;
    use crate::builtin::store::{FileTapeStore, ForkTapeStore};
    use crate::control_plane::{TurnContext, drain_save_events, with_turn_context};
    use nexil::{TapeEntryKind, TapeQuery, UsageEvent};

    const OVERFLOW_SUMMARY: &str = "[auto-handoff triggered by context overflow or timeout]";
    const SUMMARY_PREFIX: &str = "[Context summary from auto-handoff]\n";

    // -- compaction digest helpers (pure) -------------------------------------

    #[test]
    fn content_to_text_handles_string_array_and_none() {
        assert_eq!(content_to_text(Some(&serde_json::json!("hi"))), "hi");
        let blocks = serde_json::json!([{"type":"text","text":"a"},{"type":"text","text":"b"}]);
        assert_eq!(content_to_text(Some(&blocks)), "a b");
        assert_eq!(content_to_text(None), "");
    }

    #[test]
    fn tail_chars_keeps_recent_and_marks_truncation() {
        assert_eq!(tail_chars("short", 100), "short");
        let big = "z".repeat(200);
        let tail = tail_chars(&big, 50);
        assert!(tail.contains("earlier detail omitted"));
        assert!(tail.ends_with(&"z".repeat(50)));
    }

    #[test]
    fn entry_digest_line_renders_messages_skips_anchors() {
        let msg = TapeEntry::message(
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({}),
        );
        assert_eq!(entry_digest_line(&msg).as_deref(), Some("user: hello"));
        let anchor = TapeEntry::anchor("a", None, serde_json::json!({}));
        assert!(entry_digest_line(&anchor).is_none());
    }

    #[test]
    fn digest_covers_only_work_since_last_anchor() {
        let entries = vec![
            TapeEntry::message(
                serde_json::json!({"role": "user", "content": "BEFORE the anchor"}),
                serde_json::json!({}),
            ),
            TapeEntry::anchor("a1", None, serde_json::json!({})),
            TapeEntry::message(
                serde_json::json!({"role": "user", "content": "AFTER one"}),
                serde_json::json!({}),
            ),
            TapeEntry::message(
                serde_json::json!({"role": "assistant", "content": "AFTER two"}),
                serde_json::json!({}),
            ),
        ];
        let digest = digest_since_last_anchor(&entries);
        assert!(!digest.contains("BEFORE the anchor"));
        assert!(digest.contains("AFTER one"));
        assert!(digest.contains("AFTER two"));
        assert!(digest.contains("user:") && digest.contains("assistant:"));
    }

    // -- verify command (hard-signal) -----------------------------------------

    #[tokio::test]
    async fn verify_command_passes_on_zero_exit() {
        assert!(run_verify_command("true", Path::new(".")).await.is_ok());
    }

    #[tokio::test]
    async fn verify_command_reports_output_on_nonzero_exit() {
        let err = run_verify_command("echo boom 1>&2; exit 1", Path::new("."))
            .await
            .unwrap_err();
        assert!(err.contains("boom"), "err was: {err}");
    }

    fn make_tape_service() -> (tempfile::TempDir, TapeService) {
        let tmp = tempfile::tempdir().unwrap();
        let tapes_dir = tmp.path().join("tapes");
        let store = ForkTapeStore::from_sync(FileTapeStore::new(tapes_dir.clone()));
        (tmp, TapeService::new(tapes_dir, store))
    }

    fn test_settings(context_window: usize) -> AgentSettings {
        let mut settings = AgentSettings::from_env();
        settings.context_window = context_window;
        settings
    }

    fn make_usage(input: u64, output: u64) -> Vec<UsageEvent> {
        vec![UsageEvent {
            model: "test-model".into(),
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            timestamp: "2026-01-01T00:00:00Z".into(),
        }]
    }

    fn test_turn_context() -> TurnContext {
        TurnContext {
            cancellation: nexil::CancellationToken::new(),
            wrap_tools: None,
            usage: Default::default(),
            save_events: Default::default(),
            dispatch: None,
            outbound_media: Default::default(),
            text_sink: None,
        }
    }

    fn text_with_usage(text: &str, input_tokens: u64) -> ToolAutoResult {
        let mut result = ToolAutoResult::text_result(text);
        result.usage = make_usage(input_tokens, 0);
        result
    }

    fn tool_result_without_usage(chars: usize) -> ToolAutoResult {
        let mut result = ToolAutoResult::text_result("");
        result.tool_results = vec![serde_json::json!({ "payload": "x".repeat(chars) })];
        result
    }

    fn context_overflow_error() -> ConduitError {
        ConduitError::new(ErrorKind::Provider, "context_length_exceeded")
    }

    async fn run_injected_turn<F>(
        tapes: &TapeService,
        tape_name: &str,
        settings: &AgentSettings,
        injected: F,
    ) -> (Option<TapeContext>, Result<String, ConduitError>)
    where
        F: FnOnce(Option<&TapeContext>) -> Result<ToolAutoResult, ConduitError>,
    {
        let tape_context = resolve_tape_context_override(tapes, tape_name).await;
        let result = injected(tape_context.as_ref());
        let text = process_agent_result(tapes, tape_name, result, 1, settings, None).await;
        (tape_context, text)
    }

    async fn inject_text(
        tapes: &TapeService,
        tape_name: &str,
        settings: &AgentSettings,
        text: &str,
        input_tokens: u64,
    ) -> Result<String, ConduitError> {
        run_injected_turn(tapes, tape_name, settings, |_| {
            Ok(text_with_usage(text, input_tokens))
        })
        .await
        .1
    }

    async fn inject_overflow_error(
        tapes: &TapeService,
        tape_name: &str,
        settings: &AgentSettings,
    ) -> Result<String, ConduitError> {
        run_injected_turn(
            tapes,
            tape_name,
            settings,
            |_| Err(context_overflow_error()),
        )
        .await
        .1
    }

    async fn assert_auto_anchor_count(tapes: &TapeService, tape_name: &str, expected: usize) {
        let actual = auto_anchor_names(tapes, tape_name).await.len();
        assert_eq!(actual, expected);
    }

    async fn auto_anchor_names(tapes: &TapeService, tape_name: &str) -> Vec<String> {
        tapes
            .anchors(tape_name, 20)
            .await
            .unwrap()
            .into_iter()
            .filter(|a| a.name.starts_with("auto-handoff/"))
            .map(|a| a.name)
            .collect()
    }

    async fn assert_last_auto_anchor(
        tapes: &TapeService,
        tape_name: &str,
        input_tokens: u64,
        summary: &str,
    ) {
        let anchors = tapes.anchors(tape_name, 20).await.unwrap();
        let anchor = anchors.last().unwrap();
        assert!(anchor.name.starts_with("auto-handoff/"));
        assert_eq!(anchor.state["input_tokens"].as_u64(), Some(input_tokens));
        assert_eq!(anchor.state["summary"].as_str(), Some(summary));
    }

    async fn assert_grace(tapes: &TapeService, tape_name: &str, remaining: u32, anchor: &str) {
        let grace = tapes.auto_handoff_grace(tape_name).await.unwrap();
        assert_eq!(grace, Some((remaining, anchor.to_owned())));
    }

    async fn latest_system_content(tapes: &TapeService, tape_name: &str) -> String {
        let query = TapeQuery::new(tape_name);
        let entries = tapes.store().fetch_all(&query).await.unwrap();
        entries
            .iter()
            .rev()
            .find(|e| e.kind == TapeEntryKind::System)
            .and_then(|e| e.payload.get("content").and_then(Value::as_str))
            .unwrap_or("")
            .to_owned()
    }

    async fn assert_summary_written(tapes: &TapeService, tape_name: &str, summary: &str) {
        let expected = format!("{SUMMARY_PREFIX}{summary}");
        assert_eq!(latest_system_content(tapes, tape_name).await, expected);
    }

    fn assert_named_context(ctx: Option<TapeContext>, expected: &str) {
        let Some(ctx) = ctx else {
            panic!("expected injected turn to receive tape context");
        };
        match ctx.anchor {
            AnchorSelector::Named(name) => assert_eq!(name, expected),
            other => panic!("expected named anchor, got {other:?}"),
        }
    }

    async fn with_injected_tape<F, Fut>(tape_name: &str, f: F)
    where
        F: FnOnce(TapeService, String, AgentSettings) -> Fut,
        Fut: Future<Output = ()>,
    {
        let (_tmp, tapes) = make_tape_service();
        tapes.ensure_bootstrap_anchor(tape_name).await.unwrap();
        let settings = test_settings(1000);
        with_turn_context(
            test_turn_context(),
            f(tapes, tape_name.to_owned(), settings),
        )
        .await;
    }

    async fn injected_small_result_does_not_handoff(
        tapes: TapeService,
        tape_name: String,
        settings: AgentSettings,
    ) {
        let (_, result) = run_injected_turn(&tapes, &tape_name, &settings, |_| {
            Ok(text_with_usage("small", 0))
        })
        .await;
        assert_eq!(result.unwrap(), "small");
        assert_auto_anchor_count(&tapes, &tape_name, 0).await;
    }

    async fn injected_high_usage_result_places_handoff_anchor(
        tapes: TapeService,
        tape_name: String,
        settings: AgentSettings,
    ) {
        let result = inject_text(&tapes, &tape_name, &settings, "large response", 1000).await;
        assert_eq!(result.unwrap(), "large response");
        assert_last_auto_anchor(&tapes, &tape_name, 1000, "large response").await;
        assert_summary_written(&tapes, &tape_name, "large response").await;
        assert_grace(&tapes, &tape_name, 2, "session/start").await;
    }

    async fn injected_tool_result_estimate_places_handoff_anchor(
        tapes: TapeService,
        tape_name: String,
        settings: AgentSettings,
    ) {
        let (_, result) = run_injected_turn(&tapes, &tape_name, &settings, |_| {
            Ok(tool_result_without_usage(5000))
        })
        .await;
        assert_eq!(result.unwrap(), "");
        assert_auto_anchor_count(&tapes, &tape_name, 1).await;
    }

    async fn injected_overflow_error_places_handoff_anchor(
        tapes: TapeService,
        tape_name: String,
        settings: AgentSettings,
    ) {
        let result = inject_overflow_error(&tapes, &tape_name, &settings).await;
        assert!(result.is_err());
        assert_last_auto_anchor(&tapes, &tape_name, 1000, OVERFLOW_SUMMARY).await;
        assert_summary_written(&tapes, &tape_name, OVERFLOW_SUMMARY).await;
    }

    async fn injected_grace_turn_uses_previous_anchor(
        tapes: TapeService,
        tape_name: String,
        settings: AgentSettings,
    ) {
        inject_text(&tapes, &tape_name, &settings, "large", 1000)
            .await
            .unwrap();
        let (ctx, _) = run_injected_turn(&tapes, &tape_name, &settings, |_| {
            Ok(text_with_usage("small", 0))
        })
        .await;
        assert_named_context(ctx, "session/start");
        assert_grace(&tapes, &tape_name, 1, "session/start").await;
    }

    async fn injected_overflow_error_during_grace_advances_handoff(
        tapes: TapeService,
        tape_name: String,
        settings: AgentSettings,
    ) {
        inject_text(&tapes, &tape_name, &settings, "large", 1000)
            .await
            .unwrap();
        assert!(
            inject_overflow_error(&tapes, &tape_name, &settings)
                .await
                .is_err()
        );
        let anchors = auto_anchor_names(&tapes, &tape_name).await;
        assert_eq!(anchors.len(), 2);
        assert_grace(&tapes, &tape_name, 2, &anchors[0]).await;
    }

    #[tokio::test]
    async fn test_record_run_event_pushes_save_event() {
        with_turn_context(test_turn_context(), async {
            record_run_event(500, "ok", None, &make_usage(1000, 200), 0);

            let events = drain_save_events();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0, "agent.run");
            let data = &events[0].1;
            assert_eq!(data["usage"]["input_tokens"], 1000);
            assert_eq!(data["usage"]["output_tokens"], 200);
            assert_eq!(data["usage"]["total_tokens"], 1200);
            assert_eq!(data["usage"]["rounds"], 1);
        })
        .await;
    }

    #[tokio::test]
    async fn test_record_run_event_aggregates_multi_round_usage() {
        with_turn_context(test_turn_context(), async {
            let usage = vec![
                UsageEvent {
                    model: "m".into(),
                    input_tokens: 500,
                    output_tokens: 100,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    timestamp: "2026-01-01T00:00:00Z".into(),
                },
                UsageEvent {
                    model: "m".into(),
                    input_tokens: 800,
                    output_tokens: 150,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    timestamp: "2026-01-01T00:00:01Z".into(),
                },
            ];
            record_run_event(1000, "ok", None, &usage, 0);

            let events = drain_save_events();
            assert_eq!(events.len(), 1);
            let data = &events[0].1;
            assert_eq!(data["usage"]["input_tokens"], 1300);
            assert_eq!(data["usage"]["output_tokens"], 250);
            assert_eq!(data["usage"]["total_tokens"], 1550);
            assert_eq!(data["usage"]["rounds"], 2);
        })
        .await;
    }

    #[tokio::test]
    async fn test_record_run_event_surfaces_cache_and_tool_metrics() {
        with_turn_context(test_turn_context(), async {
            let usage = vec![UsageEvent {
                model: "m".into(),
                input_tokens: 200,
                output_tokens: 50,
                cache_creation_input_tokens: 100,
                cache_read_input_tokens: 700,
                timestamp: "2026-01-01T00:00:00Z".into(),
            }];
            record_run_event(42, "ok", None, &usage, 3);

            let events = drain_save_events();
            let data = &events[0].1;
            assert_eq!(data["usage"]["cache_read_tokens"], 700);
            assert_eq!(data["usage"]["cache_write_tokens"], 100);
            assert_eq!(data["tool_calls"], 3);
            // hit ratio = cache_read / (input + cache_read + cache_write)
            //           = 700 / (200 + 700 + 100) = 0.7
            let ratio = data["usage"]["cache_hit_ratio"].as_f64().unwrap();
            assert!((ratio - 0.7).abs() < 1e-9, "ratio was {ratio}");
        })
        .await;
    }

    #[tokio::test]
    async fn test_process_agent_result_ok_pushes_save_event() {
        let (_tmp, tapes) = make_tape_service();
        let tape_name = "test_tape";
        tapes.ensure_bootstrap_anchor(tape_name).await.unwrap();

        with_turn_context(test_turn_context(), async {
            let result = Ok(ToolAutoResult {
                kind: ToolAutoResultKind::Text,
                text: Some("hello".into()),
                tool_calls: vec![],
                tool_results: vec![],
                error: None,
                usage: make_usage(2000, 400),
            });

            let settings = AgentSettings::from_env();
            let text = process_agent_result(&tapes, tape_name, result, 100, &settings, None)
                .await
                .unwrap();
            assert_eq!(text, "hello");

            let events = drain_save_events();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0, "agent.run");
            assert_eq!(events[0].1["usage"]["total_tokens"], 2400);
        })
        .await;
    }

    #[tokio::test]
    async fn test_injected_small_result_does_not_handoff() {
        with_injected_tape("inject_small", injected_small_result_does_not_handoff).await;
    }

    #[tokio::test]
    async fn test_injected_high_usage_result_places_handoff_anchor() {
        with_injected_tape(
            "inject_high_usage",
            injected_high_usage_result_places_handoff_anchor,
        )
        .await;
    }

    #[tokio::test]
    async fn test_injected_tool_result_estimate_places_handoff_anchor() {
        with_injected_tape(
            "inject_tool_estimate",
            injected_tool_result_estimate_places_handoff_anchor,
        )
        .await;
    }

    #[tokio::test]
    async fn test_injected_overflow_error_places_handoff_anchor() {
        with_injected_tape(
            "inject_overflow_error",
            injected_overflow_error_places_handoff_anchor,
        )
        .await;
    }

    #[tokio::test]
    async fn test_injected_grace_turn_uses_previous_anchor() {
        with_injected_tape(
            "inject_grace_context",
            injected_grace_turn_uses_previous_anchor,
        )
        .await;
    }

    #[tokio::test]
    async fn test_injected_overflow_error_during_grace_advances_handoff() {
        with_injected_tape(
            "inject_grace_error",
            injected_overflow_error_during_grace_advances_handoff,
        )
        .await;
    }

    #[tokio::test]
    async fn test_extract_outbound_media_from_tool_results() {
        use crate::control_plane::drain_outbound_media;

        // Create a real temp file so path.exists() passes.
        let tmp = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();

        with_turn_context(test_turn_context(), async {
            let results = vec![serde_json::json!({
                "success": true,
                "image_path": path,
            })];
            extract_outbound_media(&results);

            let media = drain_outbound_media();
            assert_eq!(media.len(), 1);
            assert_eq!(media[0].path, path);
            assert_eq!(media[0].media_type, "image");
            assert_eq!(media[0].mime_type, "image/png");
        })
        .await;
    }

    #[tokio::test]
    async fn test_extract_outbound_media_ignores_failed_results() {
        use crate::control_plane::drain_outbound_media;

        let tmp = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();

        with_turn_context(test_turn_context(), async {
            let results = vec![serde_json::json!({
                "success": false,
                "image_path": path,
            })];
            extract_outbound_media(&results);

            assert!(
                drain_outbound_media().is_empty(),
                "failed results must be ignored"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_extract_outbound_media_ignores_missing_files() {
        use crate::control_plane::drain_outbound_media;

        with_turn_context(test_turn_context(), async {
            let results = vec![serde_json::json!({
                "success": true,
                "image_path": "/tmp/nonexistent_file_12345.png",
            })];
            extract_outbound_media(&results);

            assert!(
                drain_outbound_media().is_empty(),
                "non-existent files must be skipped"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_extract_outbound_media_handles_path_key() {
        use crate::control_plane::drain_outbound_media;

        let tmp = tempfile::NamedTempFile::with_suffix(".mp4").unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();

        with_turn_context(test_turn_context(), async {
            let results = vec![serde_json::json!({
                "success": true,
                "path": path,
            })];
            extract_outbound_media(&results);

            let media = drain_outbound_media();
            assert_eq!(media.len(), 1);
            assert_eq!(media[0].media_type, "video");
            assert_eq!(media[0].mime_type, "video/mp4");
        })
        .await;
    }

    #[tokio::test]
    async fn test_extract_outbound_media_from_stringified_json() {
        use crate::control_plane::drain_outbound_media;

        let tmp = tempfile::NamedTempFile::with_suffix(".jpg").unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();

        with_turn_context(test_turn_context(), async {
            // Tool results can be stringified JSON.
            let results = vec![Value::String(
                serde_json::to_string(&serde_json::json!({
                    "success": true,
                    "image_path": path,
                }))
                .unwrap(),
            )];
            extract_outbound_media(&results);

            let media = drain_outbound_media();
            assert_eq!(media.len(), 1);
            assert_eq!(media[0].mime_type, "image/jpeg");
        })
        .await;
    }

    #[tokio::test]
    async fn test_process_agent_result_extracts_media() {
        use crate::control_plane::drain_outbound_media;

        let (_tmp_dir, tapes) = make_tape_service();
        let tape_name = "test_media_tape";
        tapes.ensure_bootstrap_anchor(tape_name).await.unwrap();

        // Create a real temp file.
        let tmp = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();

        with_turn_context(test_turn_context(), async {
            let result = Ok(ToolAutoResult {
                kind: ToolAutoResultKind::Text,
                text: Some("Generated an image".into()),
                tool_calls: vec![],
                tool_results: vec![serde_json::json!({
                    "success": true,
                    "image_path": path,
                })],
                error: None,
                usage: make_usage(1000, 200),
            });

            let settings = AgentSettings::from_env();
            let text = process_agent_result(&tapes, tape_name, result, 100, &settings, None)
                .await
                .unwrap();
            assert_eq!(text, "Generated an image");

            let media = drain_outbound_media();
            assert_eq!(media.len(), 1, "media must be extracted from tool_results");
            assert_eq!(media[0].path, path);
        })
        .await;
    }
}
