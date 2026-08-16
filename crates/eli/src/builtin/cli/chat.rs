//! Interactive REPL chat session, plus `--json` event mode for GUI front-ends.
//!
//! JSON mode speaks newline-delimited JSON on stdout (one event per line):
//!   {"type":"text_delta","delta":"..."}            — live, as the model generates prose
//!   {"type":"reasoning_delta","delta":"..."}        — live, as the model generates reasoning
//!   {"type":"tool_call","id","name","arguments"}   — live, from the tape
//!   {"type":"tool_result","id","output","is_error"} — live, from the tape
//!   {"type":"assistant","text"}                     — turn end
//!   {"type":"usage","input_tokens","output_tokens"} — turn end
//!   {"type":"error","message"}                      — turn failure
//! stdin takes {"content": "..."} per line (raw text tolerated). Tracing logs
//! stay on stderr; the builtin's CLI-mode stdout prints are silenced via
//! crate::builtin::JSON_MODE (see builtin/mod.rs).

use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::framework::EliFramework;

/// Start an interactive REPL chat session.
pub(crate) async fn chat_command(
    chat_id: String,
    session_id: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let session = session_id.unwrap_or_else(|| format!("cli:{chat_id}"));
    let (framework, _builtin) = super::builtin_framework().await;

    if json {
        return chat_json(framework, session, chat_id).await;
    }

    // Wire inbound injector so subagent results trigger new turns.
    {
        let fw = framework.clone();
        crate::control_plane::set_inbound_injector(std::sync::Arc::new(move |envelope| {
            let fw = fw.clone();
            Box::pin(async move {
                if let Err(e) = fw.process_inbound(envelope).await {
                    tracing::error!(error = %e, "inject_inbound failed in chat mode");
                }
            })
        }));
    }

    println!("Eli chat session started. Type /quit to exit.");

    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    use tokio::io::AsyncBufReadExt;
    let mut lines = reader.lines();

    loop {
        eprint!("> ");
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(_) => break,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed == "/quit" || trimmed == "quit" {
            println!("Goodbye.");
            break;
        }

        let inbound = serde_json::json!({
            "session_id": session,
            "channel": "cli",
            "chat_id": chat_id,
            "content": trimmed,
            "output_channel": "cli",
        });

        match framework.process_inbound(inbound).await {
            Ok(result) => {
                super::print_usage(&result.usage);
            }
            Err(e) => eprintln!("Error: {e}"),
        }
    }

    Ok(())
}

/// GUI event mode. One persistent process per conversation; history is
/// tape-backed, so it survives restarts with the same session id.
async fn chat_json(
    framework: Arc<EliFramework>,
    session: String,
    chat_id: String,
) -> anyhow::Result<()> {
    // The builtin's CLI-mode dispatch prints assistant text to stdout; in JSON
    // mode stdout is an event stream, so silence it (read in builtin/mod.rs).
    crate::builtin::JSON_MODE.store(true, Ordering::SeqCst);

    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let tape_name = crate::builtin::tape::TapeService::session_tape_name(&session, &cwd);
    let tape_path = crate::builtin::config::eli_home()
        .join("tapes")
        .join(format!("{tape_name}.jsonl"));

    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    use tokio::io::AsyncBufReadExt;
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Tolerate both {"content": "..."} and raw text.
        let content = serde_json::from_str::<Value>(trimmed)
            .ok()
            .and_then(|v| v.get("content").and_then(|c| c.as_str()).map(str::to_string))
            .unwrap_or_else(|| trimmed.to_string());
        if content.is_empty() {
            continue;
        }

        // Tool entries are written to the tape during the run; tail them so
        // the front-end sees activity before the turn completes.
        let stop = Arc::new(AtomicBool::new(false));
        let tail = {
            let (stop, path, out) = (stop.clone(), tape_path.clone(), stdout.clone());
            std::thread::spawn(move || tail_tape(&path, stop, out))
        };

        // Streaming text deltas: install a sink the agent forwards prose into
        // as the model generates it, drained here as text_delta events. The
        // framework takes the sink into this turn's context; clear it after
        // the run so a stale sender can't leak into the next turn.
        let (text_tx, mut text_rx) =
            tokio::sync::mpsc::channel::<nexil::llm::StreamChunk>(256);
        crate::control_plane::set_text_sink(Some(text_tx));
        let text_drain = {
            let stdout = stdout.clone();
            tokio::spawn(async move {
                while let Some(chunk) = text_rx.recv().await {
                    match chunk {
                        nexil::llm::StreamChunk::Text(delta) => {
                            emit(json!({"type": "text_delta", "delta": delta}), &stdout);
                        }
                        nexil::llm::StreamChunk::Reasoning(delta) => {
                            emit(json!({"type": "reasoning_delta", "delta": delta}), &stdout);
                        }
                    }
                }
            })
        };

        let inbound = json!({
            "session_id": session,
            "channel": "cli",
            "chat_id": chat_id,
            "content": content,
            "output_channel": "cli",
        });
        let result = framework.process_inbound(inbound).await;
        stop.store(true, Ordering::SeqCst);
        // Let the tailer drain entries written in the final moments.
        std::thread::sleep(Duration::from_millis(30));
        let _ = tail.join();

        // All senders are gone once the turn ends; awaiting the drain task
        // flushes every delta before the turn-end events, so ordering stays
        // text_delta* -> assistant -> usage.
        crate::control_plane::set_text_sink(None);
        let _ = text_drain.await;

        match result {
            Ok(r) => {
                // Model failures are swallowed into model_output by the framework.
                if r.model_output.starts_with("[Error:")
                    || r.model_output.starts_with("(model returned empty response)")
                {
                    emit(json!({"type": "error", "message": r.model_output}), &stdout);
                } else if !r.model_output.trim().is_empty() {
                    emit(json!({"type": "assistant", "text": r.model_output}), &stdout);
                }
                emit(
                    json!({
                        "type": "usage",
                        "input_tokens": r.usage.input_tokens,
                        "output_tokens": r.usage.output_tokens,
                    }),
                    &stdout,
                );
            }
            Err(e) => emit(json!({"type": "error", "message": e.to_string()}), &stdout),
        }
    }
    Ok(())
}

/// Emit tool_call/tool_result tape entries as ndjson until `stop` is set.
fn tail_tape(path: &Path, stop: Arc<AtomicBool>, stdout: Arc<Mutex<std::io::Stdout>>) {
    // The file is created when the run starts; a greeting short-circuit never
    // creates it, so a missing file is not an error.
    let file = {
        let mut handle = None;
        for _ in 0..200 {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if let Ok(f) = std::fs::File::open(path) {
                handle = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let Some(f) = handle else { return };
        f
    };
    let mut reader = BufReader::new(file);
    // Only entries written during this turn.
    if reader.seek(SeekFrom::End(0)).is_err() {
        return;
    }
    let mut line = String::new();
    while !stop.load(Ordering::SeqCst) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => std::thread::sleep(Duration::from_millis(50)),
            Ok(_) => emit_tape_entry(line.trim_end(), &stdout),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    for _ in 0..10 {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        emit_tape_entry(line.trim_end(), &stdout);
    }
}

/// Map one tape entry to zero or one ndjson event.
fn emit_tape_entry(line: &str, stdout: &Arc<Mutex<std::io::Stdout>>) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
        "tool_call" => {
            let Some(calls) = v.pointer("/payload/calls").and_then(|c| c.as_array()) else {
                return;
            };
            for call in calls {
                emit(
                    json!({
                        "type": "tool_call",
                        "id": call.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                        "name": call.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or(""),
                        "arguments": call.pointer("/function/arguments").and_then(|v| v.as_str()).unwrap_or(""),
                    }),
                    stdout,
                );
            }
        }
        "tool_result" => {
            let Some(results) = v.pointer("/payload/results").and_then(|r| r.as_array()) else {
                return;
            };
            for res in results {
                let (output, is_error) = match res.get("output") {
                    Some(Value::String(s)) => (s.clone(), false),
                    Some(other) => (
                        other
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("")
                            .to_string(),
                        other
                            .get("is_error")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false),
                    ),
                    None => (String::new(), false),
                };
                emit(
                    json!({
                        "type": "tool_result",
                        "id": res.get("call_id").and_then(|v| v.as_str()).unwrap_or(""),
                        "output": output,
                        "is_error": is_error,
                    }),
                    stdout,
                );
            }
        }
        _ => {}
    }
}

fn emit(obj: Value, stdout: &Arc<Mutex<std::io::Stdout>>) {
    let Ok(line) = serde_json::to_string(&obj) else {
        return;
    };
    if let Ok(mut out) = stdout.lock() {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}
