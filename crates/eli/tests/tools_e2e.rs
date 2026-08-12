//! Agent e2e: drive the real agent loop (`Agent::run`) with a live LLM and
//! verify it calls every builtin tool in mixed order for N iterations, with
//! exact per-tool call counts.
//!
//! Unlike a direct `tool.run()` test, this exercises the full path:
//!   LLM → tool_calls → ToolExecutor → tool handlers → results → tape
//!
//! # Running
//!
//! Requires a configured provider (`eli login` or `ELI_API_KEY`) and a
//! tool-capable model (the active `eli` profile is used).
//!
//! ```bash
//! cargo test -p eli --test tools_e2e -- --ignored --nocapture
//! ELI_E2E_ITERATIONS=11 cargo test -p eli --test tools_e2e -- --ignored --nocapture  # smoke
//! ```
//!
//! Costs ~2 API calls per iteration (one tool call + one final text). At 100
//! iterations that's ~200 calls and a few minutes.

use std::collections::HashMap;
use std::env;

use eli::builtin::agent::Agent;
use eli::builtin::store::{FileTapeStore, ForkTapeStore};
use eli::builtin::tape::TapeService;
use eli::builtin::tools::register_builtin_tools;
use eli::taskboard;
use eli::types::{PromptValue, RUNTIME_WORKSPACE_KEY};
use nexil::{TapeEntryKind, TapeQuery};
use serde_json::{Value, json};

/// A tool-call instruction for one iteration: the prompt the agent receives
/// and the tool(s) it must call, in order. Most specs are a single tool; the
/// background-bash spec chains two calls (bash → bash.output) so the agent
/// threads the shell_id itself, exactly as a real session would.
struct CallSpec {
    tools: &'static [&'static str],
    prompt: &'static str,
}

fn tool_specs() -> Vec<CallSpec> {
    vec![
        CallSpec {
            tools: &["fs.write"],
            prompt: "Call the fs.write tool with path \"e2e.txt\" and content exactly these three lines: alpha, then beta, then gamma. Call no other tool. After it returns, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["fs.read"],
            prompt: "Call the fs.read tool with path \"e2e.txt\". Call no other tool. After it returns, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["fs.edit"],
            prompt: "Call the fs.edit tool on path \"e2e.txt\": replace old text \"beta\" with new text \"BETA\". Call no other tool. After it returns, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["bash"],
            prompt: "Call the bash tool with command \"echo ping\". Call no other tool. After it returns, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["bash", "bash.output"],
            prompt: "Call the bash tool to run \"sleep 3\" in the background (set background=true). From the result, copy the shell_id. Then call the bash.output tool with that shell_id. Call no other tools. After both return, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["tape.info"],
            prompt: "Call the tape.info tool. Call no other tool. After it returns, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["decision.set"],
            prompt: "Call the decision.set tool with text \"prefer rust over python\". Call no other tool. After it returns, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["decision.list"],
            prompt: "Call the decision.list tool. Call no other tool. After it returns, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["help"],
            prompt: "Call the help tool. Call no other tool. After it returns, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["task.create"],
            prompt: "Call the task.create tool with kind \"e2e\" and prompt \"verify tool call count\". Call no other tool. After it returns, reply with exactly: DONE",
        },
        CallSpec {
            tools: &["task.list"],
            prompt: "Call the task.list tool. Call no other tool. After it returns, reply with exactly: DONE",
        },
    ]
}

/// Extract the tool name from a normalized tape tool-call entry.
/// Handles both the nested `function.name` shape and a flat `name`.
fn call_name(call: &Value) -> Option<&str> {
    call.pointer("/function/name")
        .and_then(Value::as_str)
        .or_else(|| call.get("name").and_then(Value::as_str))
}

/// Canonical tool name for comparison. The LLM wire format uses underscores
/// (OpenAI rejects dots in function names), so the tape stores `fs_write`
/// while the registry uses `fs.read`. Normalize both to underscore form.
fn canonical(name: &str) -> String {
    name.replace('.', "_")
}

#[tokio::test]
#[ignore = "hits real LLM API; run with --ignored"]
async fn agent_e2e_mixed_tool_calls_count_matches() {
    // Auth gate: skip when no provider is configured.
    let has_auth =
        env::var("ELI_API_KEY").is_ok() || eli::builtin::config::EliConfig::config_path().exists();
    if !has_auth {
        eprintln!("skipping: no API key configured (run `eli login` or set ELI_API_KEY)");
        return;
    }

    let iterations = env::var("ELI_E2E_ITERATIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(100);

    // --- isolated fixtures (auth still comes from the real ~/.eli) ----------
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();

    let mut agent = Agent::new();
    let tapes_dir = tmp.path().join("tapes");
    let store = ForkTapeStore::from_sync(FileTapeStore::new(tapes_dir.clone()));
    agent.set_tapes(TapeService::new(tapes_dir, store));

    register_builtin_tools();
    taskboard::init_task_store(tmp.path());

    let mut state = HashMap::new();
    state.insert(
        RUNTIME_WORKSPACE_KEY.to_owned(),
        json!(workspace.display().to_string()),
    );

    let specs = tool_specs();
    let n = specs.len();

    // --- run N mixed-order iterations through the real agent loop -----------
    // Fresh session per iteration keeps each turn's context O(1) instead of
    // growing the tape quadratically across 100 turns.
    let mut actual: HashMap<String, usize> = HashMap::new();
    let mut errors: Vec<(usize, String)> = Vec::new();

    for i in 0..iterations {
        let idx = (i * 7) % n; // gcd(7, 11) = 1 → every spec each cycle
        let spec = &specs[idx];
        let session_id = format!("e2e-{i}");

        match agent
            .run(
                &session_id,
                PromptValue::Text(spec.prompt.to_owned()),
                &state,
                None,
                None,
                None,
            )
            .await
        {
            Ok(_) => {
                let tape_name = TapeService::session_tape_name(&session_id, &workspace);
                let entries = agent
                    .tapes()
                    .search(&TapeQuery::new(&tape_name).kinds(vec![TapeEntryKind::ToolCall]))
                    .await
                    .unwrap();
                let mut called: Vec<String> = Vec::new();
                for entry in &entries {
                    let Some(calls) = entry.payload.pointer("/calls").and_then(Value::as_array)
                    else {
                        continue;
                    };
                    for call in calls {
                        if let Some(name) = call_name(call) {
                            *actual.entry(canonical(name)).or_insert(0) += 1;
                            called.push(canonical(name));
                        }
                    }
                }
                eprintln!("[{i:>3}] expected {:?} → called {:?}", spec.tools, called);
            }
            Err(e) => {
                eprintln!("[{i:>3}] ERROR: {e}");
                errors.push((i, e.message.clone()));
            }
        }
    }

    // --- expected counts from the same schedule -----------------------------
    let mut expected: HashMap<String, usize> = HashMap::new();
    for i in 0..iterations {
        for tool in specs[(i * 7) % n].tools {
            *expected.entry(canonical(tool)).or_insert(0) += 1;
        }
    }

    // --- assert exact counts -------------------------------------------------
    let mut all_tools: Vec<String> = expected
        .keys()
        .cloned()
        .chain(actual.keys().cloned())
        .collect();
    all_tools.sort_unstable();
    all_tools.dedup();

    eprintln!("\n=== tool call counts ({iterations} iterations) ===");
    let mut mismatches = Vec::new();
    for tool in &all_tools {
        let e = expected.get(tool).copied().unwrap_or(0);
        let a = actual.get(tool).copied().unwrap_or(0);
        let mark = if e == a { "✓" } else { "✗" };
        eprintln!("  {mark} {tool}: expected {e}, actual {a}");
        if e != a {
            mismatches.push((tool.clone(), e, a));
        }
    }
    if !errors.is_empty() {
        eprintln!("  iteration errors: {}", errors.len());
    }

    assert!(
        mismatches.is_empty(),
        "tool call count mismatch (expected, actual): {mismatches:?}"
    );
}
