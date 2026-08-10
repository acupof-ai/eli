//! Builtin tool implementations: bash, fs, tape, web, agent, etc.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::future::BoxFuture;
use nexil::tools::schema::ToolResult;
use nexil::{ConduitError, ErrorKind};
use nexil::{TapeEntry, TapeEntryKind, TapeQuery, Tool, ToolContext};
use serde_json::Value;
use tempfile::NamedTempFile;

use crate::builtin::command_semantics::{
    ExitOutcome, interpret_exit, is_blocking_sleep, is_silent_command,
};
use crate::builtin::config::eli_home;
use crate::builtin::shell_manager::shell_manager;
use crate::builtin::tape::TapeService;
use crate::envelope::ValueExt;
use crate::evolution::{
    AutoEvolutionPolicy, AutoJournalAction, AutoJournalEntry, CandidateStatus, DistillOutcome,
    EvaluationRun, EvolutionStore,
};
use crate::skills::discover_skills;
use crate::tools::{REGISTRY, shorten_text};
use crate::types::{RUNTIME_TAPES_DIR_KEY, RUNTIME_WORKSPACE_KEY};

const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 10;
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024; // 10MB
const DEFAULT_READ_LINE_LIMIT: usize = 500;

/// Tool output above this char count gets spilled to a file with a preview,
/// keeping the context window lean while the full output stays on disk
/// (recoverable via `fs.read` — the infinite-context view/store split).
const TOOL_OUTPUT_LARGE_THRESHOLD: usize = 30_000;
/// How many characters of preview to show for spilled output.
const TOOL_OUTPUT_PREVIEW_CHARS: usize = 2_000;

/// Maximum characters of CLI output included in the subagent completion message.
const SUBAGENT_OUTPUT_TAIL: usize = 2000;

// ---------------------------------------------------------------------------
// Subagent CLI detection
// ---------------------------------------------------------------------------

/// Info about a detected coding CLI binary.
#[derive(Clone, Debug)]
struct CliInfo {
    name: String,
    path: String,
}

/// Ordered list of coding CLIs to probe.
const CLI_CANDIDATES: &[&str] = &["claude", "codex", "kimi"];

static DETECTED_CLI: std::sync::LazyLock<parking_lot::Mutex<Option<CliInfo>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(None));

fn detect_cli() -> Option<CliInfo> {
    let mut cache = DETECTED_CLI.lock();
    if let Some(ref info) = *cache {
        return Some(info.clone());
    }
    for &name in CLI_CANDIDATES {
        if let Ok(output) = std::process::Command::new("which").arg(name).output()
            && output.status.success()
        {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let info = CliInfo {
                name: name.to_owned(),
                path,
            };
            *cache = Some(info.clone());
            return Some(info);
        }
    }
    None
}

fn resolve_cli(explicit: Option<&str>) -> Result<CliInfo, ConduitError> {
    if let Some(name) = explicit {
        let output = std::process::Command::new("which")
            .arg(name)
            .output()
            .map_err(|e| ConduitError::new(ErrorKind::Tool, format!("which {name}: {e}")))?;
        if !output.status.success() {
            return Err(ConduitError::new(
                ErrorKind::Tool,
                format!("CLI '{name}' not found in PATH"),
            ));
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        return Ok(CliInfo {
            name: name.to_owned(),
            path,
        });
    }
    detect_cli().ok_or_else(|| {
        ConduitError::new(
            ErrorKind::Tool,
            format!(
                "no coding CLI found in PATH (tried: {})",
                CLI_CANDIDATES.join(", ")
            ),
        )
    })
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_owned();
    }
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_-./=".contains(&b))
    {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn build_cli_command(cli: &CliInfo, prompt_file: &str) -> String {
    let bin = shell_quote(&cli.path);
    let file = shell_quote(prompt_file);
    match cli.name.as_str() {
        // claude -p reads from stdin when no positional prompt is given.
        "claude" => format!("{bin} -p --output-format text < {file}"),
        // codex exec reads from stdin when prompt arg is `-` or omitted.
        "codex" => format!("{bin} exec < {file}"),
        // kimi -p takes the prompt as a direct argument; use $() to read from file.
        "kimi" => format!("{bin} -p \"$(cat {file})\" --print"),
        // Fallback: assume stdin piping works.
        _ => format!("{bin} < {file}"),
    }
}

fn write_prompt_tempfile(prompt: &str) -> Result<NamedTempFile, ConduitError> {
    let mut f = tempfile::Builder::new()
        .prefix(".eli-prompt-")
        .tempfile()
        .map_err(|e| ConduitError::new(ErrorKind::Tool, format!("prompt tempfile: {e}")))?;
    f.write_all(prompt.as_bytes())
        .map_err(|e| ConduitError::new(ErrorKind::Tool, format!("write prompt: {e}")))?;
    Ok(f)
}

fn snapshot_git_head(workspace: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

async fn collect_artifacts(workspace: &str, pre_head: Option<&str>) -> String {
    let is_git = tokio::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(workspace)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !is_git {
        return "(not a git repo)".to_owned();
    }

    let mut parts: Vec<String> = Vec::new();

    // Current HEAD.
    let current_head = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());

    // Commits since spawn.
    if let (Some(pre), Some(cur)) = (pre_head, &current_head)
        && pre != cur
        && let Ok(o) = tokio::process::Command::new("git")
            .args(["log", "--oneline", &format!("{pre}..{cur}")])
            .current_dir(workspace)
            .output()
            .await
    {
        let log = String::from_utf8_lossy(&o.stdout).trim().to_owned();
        if !log.is_empty() {
            parts.push(format!("commits:\n{log}"));
        }
    }

    // Working tree status.
    if let Ok(o) = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace)
        .output()
        .await
    {
        let status = String::from_utf8_lossy(&o.stdout).trim().to_owned();
        if !status.is_empty() {
            parts.push(format!("working tree:\n{status}"));
        }
    }

    // Diff stat.
    if let Ok(o) = tokio::process::Command::new("git")
        .args(["diff", "--stat"])
        .current_dir(workspace)
        .output()
        .await
    {
        let stat = String::from_utf8_lossy(&o.stdout).trim().to_owned();
        if !stat.is_empty() {
            parts.push(stat);
        }
    }

    if parts.is_empty() {
        "(no changes)".to_owned()
    } else {
        parts.join("\n")
    }
}

fn build_completion_message(
    agent_id: &str,
    cli_name: &str,
    exit_code: Option<i32>,
    output: &str,
    artifacts: &str,
) -> String {
    let status = match exit_code {
        Some(0) => "success (exit 0)".to_owned(),
        Some(code) => format!("failed (exit {code})"),
        None => "running (no exit code yet)".to_owned(),
    };

    // The git changes string (commits / working tree / diff stat from
    // `collect_artifacts`) is the lossless, authoritative record of what the
    // subagent actually did — it survives the subprocess boundary intact. The
    // captured stdout is only a tail-truncated *preview* (the subagent's own tape
    // is the full record). So when there are real file changes we lead with them
    // and label the stdout a preview, rather than presenting a truncated blob as
    // the primary result (the telephone-game the inter-agent design warns about).
    let has_changes = !matches!(artifacts.trim(), "(no changes)" | "(not a git repo)" | "");

    let (output_label, output_section) = if output.trim().is_empty() {
        ("output", "(sub-agent produced no output)".to_owned())
    } else if output.len() > SUBAGENT_OUTPUT_TAIL {
        let tail_start = output.len() - SUBAGENT_OUTPUT_TAIL;
        let boundary = output.ceil_char_boundary(tail_start);
        (
            "output preview (last 2000 chars; not the full record)",
            format!("...(truncated)\n{}", &output[boundary..]),
        )
    } else {
        ("output", output.to_owned())
    };

    if has_changes {
        format!(
            "[subagent {agent_id} completed ({cli_name})]\n\n\
             status: {status}\n\n\
             changes (git — authoritative record of what changed):\n{artifacts}\n\n\
             {output_label}:\n{output_section}"
        )
    } else {
        format!(
            "[subagent {agent_id} completed ({cli_name})]\n\n\
             status: {status}\n\n\
             {output_label}:\n{output_section}\n\n\
             changes:\n{artifacts}"
        )
    }
}

tokio::task_local! {
    static CURRENT_TAPE_SERVICE: TapeService;
}

/// Register all builtin tools into the global `REGISTRY`.
pub fn register_builtin_tools() {
    let mut reg = REGISTRY.lock();
    reg.extend(builtin_tools().into_iter().map(|t| (t.name.clone(), t)));
}

/// Run a future with the current tape service bound for tool handlers.
pub async fn with_tape_runtime<F, T>(tape_service: TapeService, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CURRENT_TAPE_SERVICE.scope(tape_service, future).await
}

/// Build the full list of builtin tools.
fn builtin_tools() -> Vec<Tool> {
    let mut tools = vec![
        tool_bash(),
        tool_bash_output(),
        tool_bash_kill(),
        tool_fs_read(),
        tool_fs_write(),
        tool_fs_edit(),
        tool_skill(),
        tool_evolution_capture(),
        tool_evolution_distill(),
        tool_evolution_history(),
        tool_evolution_auto_run(),
        tool_evolution_list(),
        tool_evolution_show(),
        tool_evolution_evaluate(),
        tool_evolution_promote(),
        tool_evolution_reject(),
        tool_evolution_rollback(),
        tool_tape_info(),
        tool_tape_search(),
        tool_tape_reset(),
        tool_tape_handoff(),
        tool_tape_anchors(),
        tool_decision_set(),
        tool_decision_list(),
        tool_decision_remove(),
        tool_web_fetch(),
        tool_agent(),
        tool_agent_status(),
        tool_agent_kill(),
        tool_agent_result(),
        tool_message_send(),
        tool_help(),
        tool_quit(),
        tool_task_create(),
        tool_task_status(),
        tool_task_list(),
        tool_task_cancel(),
        tool_task_update(),
    ];
    // Tag read-only tools (MCP behavior hint) from a single auditable list, so
    // a read-only / plan mode can gate everything else. Anything not listed is
    // treated as potentially mutating (the safe default).
    for tool in &mut tools {
        if READ_ONLY_TOOLS.contains(&tool.name.as_str()) {
            tool.read_only = true;
        }
    }
    tools
}

/// Builtin tools that only read state — never mutate the workspace or session.
/// Single source of truth for the read-only behavior hint and plan-mode gating.
const READ_ONLY_TOOLS: &[&str] = &[
    "bash.output",
    "fs.read",
    "web.fetch",
    "help",
    "skill",
    "tape.search",
    "tape.info",
    "tape.anchors",
    "decision.list",
    "task.list",
    "task.status",
    "evolution.list",
    "evolution.show",
    "evolution.history",
    "agent.status",
    "agent.result",
];

fn resolve_path(state: &HashMap<String, Value>, raw_path: &str) -> Result<PathBuf, ConduitError> {
    let path = PathBuf::from(shellexpand::tilde(raw_path).as_ref());
    if path.is_absolute() {
        return sanitize_path(&path);
    }
    let workspace = state
        .get(RUNTIME_WORKSPACE_KEY)
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ConduitError::new(
                ErrorKind::InvalidInput,
                format!("relative path '{raw_path}' is not allowed without a workspace"),
            )
        })?;
    let joined = PathBuf::from(workspace).join(&path);
    sanitize_path(&joined)
}

/// Reject paths containing `..` components after normalization to prevent
/// directory traversal attacks (e.g. `../../etc/passwd`).
fn sanitize_path(path: &Path) -> Result<PathBuf, ConduitError> {
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(ConduitError::new(
                ErrorKind::InvalidInput,
                format!(
                    "path '{}' contains '..' traversal and is not allowed. \
                     Use absolute path or workspace-relative path without '..'.",
                    path.display()
                ),
            ));
        }
    }
    Ok(path.to_path_buf())
}

fn read_err(error: impl std::fmt::Display) -> ConduitError {
    ConduitError::new(ErrorKind::Tool, format!("read failed: {error}"))
}

fn write_err(error: impl std::fmt::Display) -> ConduitError {
    ConduitError::new(ErrorKind::Tool, format!("write failed: {error}"))
}

fn resolve_tool_path(ctx: Option<ToolContext>, raw_path: &str) -> Result<PathBuf, ConduitError> {
    resolve_path(&ctx.map(|c| c.state).unwrap_or_default(), raw_path)
}

fn open_text_reader(path: &Path) -> Result<BufReader<std::fs::File>, ConduitError> {
    std::fs::File::open(path)
        .map(BufReader::new)
        .map_err(read_err)
}

fn read_next_line(reader: &mut impl BufRead, line: &mut String) -> Result<bool, ConduitError> {
    line.clear();
    reader
        .read_line(line)
        .map(|count| count > 0)
        .map_err(read_err)
}

fn line_limit_reached(index: usize, offset: usize, limit: Option<usize>) -> bool {
    limit.is_some_and(|limit| index >= offset.saturating_add(limit))
}

fn read_text_window(
    path: &Path,
    offset: usize,
    limit: Option<usize>,
) -> Result<(String, bool), ConduitError> {
    use std::fmt::Write;
    let mut line = String::new();
    let mut index = 0;
    let mut output = String::new();
    let mut reader = open_text_reader(path)?;
    while !line_limit_reached(index, offset, limit) && read_next_line(&mut reader, &mut line)? {
        if index >= offset {
            let _ = write!(output, "{:>6}\t{}", index + 1, &line);
        }
        index += 1;
    }
    // Check if there are more lines beyond the window.
    let truncated = read_next_line(&mut reader, &mut line)?;
    Ok((output, truncated))
}

fn create_parent_dir(path: &Path) -> Result<(), ConduitError> {
    path.parent().map_or(Ok(()), |parent| {
        std::fs::create_dir_all(parent).map_err(write_err)
    })
}

fn existing_permissions(path: &Path) -> Option<std::fs::Permissions> {
    std::fs::metadata(path).ok().map(|meta| meta.permissions())
}

fn apply_permissions(
    path: &Path,
    permissions: Option<std::fs::Permissions>,
) -> Result<(), ConduitError> {
    permissions.map_or(Ok(()), |permissions| {
        std::fs::set_permissions(path, permissions).map_err(write_err)
    })
}

struct AtomicTextWriter {
    path: PathBuf,
    permissions: Option<std::fs::Permissions>,
    writer: BufWriter<NamedTempFile>,
}

impl AtomicTextWriter {
    fn new(path: &Path) -> Result<Self, ConduitError> {
        create_parent_dir(path)?;
        let temp = tempfile::Builder::new()
            .prefix(".eli.")
            .tempfile_in(path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(write_err)?;
        Ok(Self {
            path: path.to_path_buf(),
            permissions: existing_permissions(path),
            writer: BufWriter::new(temp),
        })
    }

    fn write_str(&mut self, text: &str) -> Result<(), ConduitError> {
        self.writer.write_all(text.as_bytes()).map_err(write_err)
    }

    fn copy_from(&mut self, reader: &mut impl std::io::Read) -> Result<(), ConduitError> {
        std::io::copy(reader, &mut self.writer)
            .map(|_| ())
            .map_err(write_err)
    }

    fn persist(self) -> Result<(), ConduitError> {
        let mut temp = self.writer.into_inner().map_err(|e| write_err(e.error()))?;
        apply_permissions(temp.path(), self.permissions)?;
        temp.as_file_mut().sync_all().map_err(write_err)?;
        temp.persist(&self.path)
            .map(|_| ())
            .map_err(|e| write_err(e.error))
    }
}

fn write_text_file(path: &Path, content: &str) -> Result<(), ConduitError> {
    let mut writer = AtomicTextWriter::new(path)?;
    writer.write_str(content)?;
    writer.persist()
}

fn invalid_edit(path: &Path, old: &str, start: usize) -> ConduitError {
    let preview: String = old.chars().take(80).collect();
    let ellipsis = if old.len() > 80 { "..." } else { "" };
    ConduitError::new(
        ErrorKind::InvalidInput,
        format!(
            "'{preview}{ellipsis}' not found in {} from line {start}. \
             Read the file with fs.read first, then copy exact text into 'old'.",
            path.display()
        ),
    )
}

fn non_empty_old(old: &str) -> Result<(), ConduitError> {
    (!old.is_empty())
        .then_some(())
        .ok_or_else(|| ConduitError::new(ErrorKind::InvalidInput, "'old' must not be empty"))
}

fn flushable_prefix_len(text: &str, keep: usize) -> usize {
    let mut split = text.len().saturating_sub(keep);
    while split > 0 && !text.is_char_boundary(split) {
        split -= 1;
    }
    split
}

fn flush_pending(
    writer: &mut AtomicTextWriter,
    pending: &mut String,
    keep: usize,
) -> Result<(), ConduitError> {
    let split = flushable_prefix_len(pending, keep);
    if split == 0 {
        return Ok(());
    }
    writer.write_str(&pending[..split])?;
    pending.drain(..split);
    Ok(())
}

fn write_replacement(
    writer: &mut AtomicTextWriter,
    pending: &str,
    split: usize,
    old: &str,
    new: &str,
) -> Result<(), ConduitError> {
    writer.write_str(&pending[..split])?;
    writer.write_str(new)?;
    writer.write_str(&pending[split + old.len()..])
}

fn copy_prefix_lines(
    reader: &mut impl BufRead,
    writer: &mut AtomicTextWriter,
    start: usize,
) -> Result<(), ConduitError> {
    let mut line = String::new();
    for _ in 0..start {
        if !read_next_line(reader, &mut line)? {
            break;
        }
        writer.write_str(&line)?;
    }
    Ok(())
}

fn replace_stream(
    reader: &mut impl BufRead,
    writer: &mut AtomicTextWriter,
    old: &str,
    new: &str,
) -> Result<bool, ConduitError> {
    let mut line = String::new();
    let mut pending = String::new();
    while read_next_line(reader, &mut line)? {
        pending.push_str(&line);
        if let Some(split) = pending.find(old) {
            write_replacement(writer, &pending, split, old, new)?;
            writer.copy_from(reader)?;
            return Ok(true);
        }
        flush_pending(writer, &mut pending, old.len().saturating_sub(1))?;
    }
    writer.write_str(&pending)?;
    Ok(false)
}

fn edit_text_file(path: &Path, old: &str, new: &str, start: usize) -> Result<(), ConduitError> {
    non_empty_old(old)?;
    let mut reader = open_text_reader(path)?;
    let mut writer = AtomicTextWriter::new(path)?;
    copy_prefix_lines(&mut reader, &mut writer, start)?;
    if replace_stream(&mut reader, &mut writer, old, new)? {
        writer.persist()
    } else {
        Err(invalid_edit(path, old, start))
    }
}

fn invalid_input(error: anyhow::Error) -> ConduitError {
    ConduitError::new(ErrorKind::InvalidInput, error.to_string())
}

fn tool_error(error: anyhow::Error) -> ConduitError {
    ConduitError::new(ErrorKind::Tool, error.to_string())
}

/// Build a short, human-readable notice from the tool name and its arguments.
///
/// Examples: "读 src/main.rs", "执行 cargo build", "搜索 tape: error",
/// "写 config.toml", "编辑 lib.rs", "获取 https://…"
fn auto_notice(tool_name: &str, args: &Value) -> String {
    let primary = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let shorten = |s: &str, max: usize| -> String {
        if s.len() <= max {
            s.to_owned()
        } else {
            let end = s.floor_char_boundary(max);
            format!("{}…", &s[..end])
        }
    };
    match tool_name {
        "bash" => {
            let cmd = primary("cmd");
            let desc = primary("description");
            if !desc.is_empty() {
                shorten(desc, 60)
            } else {
                format!("执行 {}", shorten(cmd, 50))
            }
        }
        "fs.read" => format!("读 {}", shorten(primary("path"), 60)),
        "fs.write" => format!("写 {}", shorten(primary("path"), 60)),
        "fs.edit" => format!("编辑 {}", shorten(primary("path"), 60)),
        "evolution.capture" => format!("记录演进候选: {}", shorten(primary("title"), 40)),
        "evolution.distill" => {
            if args
                .get("persist")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                format!("蒸馏演进候选 {}", primary("tape"))
            } else {
                format!("预演蒸馏演进候选 {}", primary("tape"))
            }
        }
        "evolution.history" => {
            let limit = args
                .get("limit")
                .and_then(|value| value.as_i64())
                .unwrap_or(20);
            if limit > 0 {
                format!("查看演进历史 {limit}")
            } else {
                "查看演进历史".to_owned()
            }
        }
        "evolution.auto_run" => format!("自动运行演进 {}", primary("tape")),
        "evolution.list" => "列出演进候选".to_owned(),
        "evolution.show" => format!("查看演进候选 {}", primary("id")),
        "evolution.evaluate" => format!("评估演进候选 {}", primary("id")),
        "evolution.promote" => format!("提升演进候选 {}", primary("id")),
        "evolution.reject" => format!("拒绝演进候选 {}", primary("id")),
        "evolution.rollback" => format!("回滚演进候选 {}", primary("id")),
        "web.fetch" => format!("获取 {}", shorten(primary("url"), 60)),
        "tape.search" => format!("搜索 tape: {}", shorten(primary("query"), 40)),
        "tape.info" => "查看 tape 信息".to_owned(),
        "tape.reset" => "重置 tape".to_owned(),
        "tape.handoff" => {
            let name = primary("name");
            if name.is_empty() {
                "创建 handoff".to_owned()
            } else {
                format!("handoff: {name}")
            }
        }
        "tape.anchors" => "列出 anchors".to_owned(),
        "agent" => {
            let desc = primary("description");
            let bg = args
                .get("run_in_background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let prefix = if bg { "后台agent" } else { "agent" };
            if !desc.is_empty() {
                format!("{prefix}: {}", shorten(desc, 50))
            } else {
                format!("{prefix}: {}", shorten(primary("prompt"), 50))
            }
        }
        "agent.status" => "查看 agents".to_owned(),
        "agent.kill" => format!("终止 {}", primary("agent_id")),
        "agent.result" => format!("获取结果 {}", primary("agent_id")),
        _ => tool_name.to_owned(),
    }
}

fn ok_val(s: impl Into<String>) -> ToolResult {
    Ok(Value::String(s.into()))
}

fn current_tape_service() -> Result<TapeService, ConduitError> {
    CURRENT_TAPE_SERVICE.try_with(Clone::clone).map_err(|_| {
        ConduitError::new(
            ErrorKind::Tool,
            "tape tools require an active Eli runtime context",
        )
    })
}

fn tape_name_from_context(ctx: Option<&ToolContext>) -> Result<String, ConduitError> {
    ctx.and_then(|c| c.tape.clone()).ok_or_else(|| {
        ConduitError::new(
            ErrorKind::Tool,
            "tool requires an active tape name in context",
        )
    })
}

// Progress notices: compute a short human-readable label per tool call.
// Delivery to a channel is reconnected when native channels land (Phase 4);
// until then the label is emitted to the trace log. `auto_notice` is the
// load-bearing part and is reused by the channel send path.
async fn maybe_send_user_facing_notice(tool_name: &str, ctx: Option<&ToolContext>, args: &Value) {
    if !crate::builtin::config::EliConfig::load().tool_notices {
        return;
    }
    let Some(session_id) = ctx
        .and_then(|c| c.state.get("session_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    else {
        return;
    };
    let notice = auto_notice(tool_name, args);
    tracing::debug!(session_id, notice, "tool.notice");
}

fn format_tape_info(info: &crate::builtin::tape::TapeInfo) -> String {
    let last_anchor = info.last_anchor.as_deref().unwrap_or("(none)");
    let last_token_usage = info
        .last_token_usage
        .map(|v| v.to_string())
        .unwrap_or_else(|| "(unknown)".to_owned());
    format!(
        "name: {}\nentries: {}\nanchors: {}\nlast_anchor: {}\nentries_since_last_anchor: {}\nlast_token_usage: {}",
        info.name,
        info.entries,
        info.anchors,
        last_anchor,
        info.entries_since_last_anchor,
        last_token_usage,
    )
}

fn format_anchor_summaries(anchors: &[crate::builtin::tape::AnchorSummary]) -> String {
    if anchors.is_empty() {
        return "(no anchors)".to_owned();
    }

    anchors
        .iter()
        .map(|anchor| {
            let state = if anchor.state.is_empty() {
                "{}".to_owned()
            } else {
                serde_json::to_string(&anchor.state).unwrap_or_else(|_| "{}".to_owned())
            };
            format!("- {} {}", anchor.name, state)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn entry_search_text(entry: &TapeEntry) -> String {
    serde_json::json!({
        "kind": entry.kind,
        "date": entry.date,
        "payload": entry.payload,
        "meta": entry.meta,
    })
    .to_string()
    .to_lowercase()
}

fn render_search_entry(entry: &TapeEntry) -> String {
    let preview = entry
        .payload
        .get("content")
        .and_then(|v| v.as_str())
        .filter(|_| matches!(entry.kind, TapeEntryKind::Message | TapeEntryKind::System))
        .map(|content| shorten_text(content, 160))
        .unwrap_or_else(|| shorten_text(&entry.payload.to_string(), 160));
    let kind_label = serde_json::to_value(entry.kind)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| format!("{:?}", entry.kind));
    format!("#{} [{}] {} {}", entry.id, kind_label, entry.date, preview)
}

// ---------------------------------------------------------------------------
// bash — helpers
// ---------------------------------------------------------------------------

/// Inline preview fallback used when the spill file can't be written. Keeps the
/// model moving with a truncated view rather than failing the tool.
fn spill_preview_fallback(output: &str) -> String {
    let preview: String = output.chars().take(TOOL_OUTPUT_PREVIEW_CHARS).collect();
    format!(
        "{preview}\n\n[Output truncated — {total} chars total, showing first ~{shown}]",
        total = output.chars().count(),
        shown = TOOL_OUTPUT_PREVIEW_CHARS,
    )
}

/// Write large tool output to a spill file and return a preview + path. `label`
/// names the producing tool (e.g. "bash", "web") for the spill filename.
fn spill_large_output(output: &str, label: &str) -> String {
    let dir = eli_home().join("tool-results");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("{label} spill: failed to create {}: {e}", dir.display());
        return spill_preview_fallback(output);
    }

    let filename = format!("{label}-{}.txt", &uuid::Uuid::new_v4().to_string()[..8]);
    let path = dir.join(&filename);
    if let Err(e) = std::fs::write(&path, output) {
        tracing::warn!("{label} spill: failed to write {}: {e}", path.display());
        return spill_preview_fallback(output);
    }

    let total = output.chars().count();
    let preview: String = output.chars().take(TOOL_OUTPUT_PREVIEW_CHARS).collect();
    let abs = path.canonicalize().unwrap_or(path);
    format!(
        "[Output: {total} chars — showing first ~{shown}, full output saved to {path}]\n\n\
         {preview}\n\n\
         ...\n\n\
         [Use fs.read(path=\"{path}\") to read more]",
        shown = TOOL_OUTPUT_PREVIEW_CHARS,
        path = abs.display(),
    )
}

/// Return the output as-is, or spill to disk (preview + path) if it exceeds the
/// large threshold. Full output stays recoverable on disk — only the *view* is
/// trimmed, preserving the infinite-context design.
fn maybe_spill_output(output: &str, label: &str) -> String {
    if output.chars().count() > TOOL_OUTPUT_LARGE_THRESHOLD {
        spill_large_output(output, label)
    } else {
        output.to_owned()
    }
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

fn tool_bash() -> Tool {
    Tool::with_context(
        "bash",
        "Run a shell command and return its output.\n\n\
         Prefer fs.read/fs.write/fs.edit for file I/O — faster and more token-efficient than cat/sed/echo redirects.\n\
         Long-running: set background=true, then poll with bash.output.\n\
         Exceeding timeout_seconds kills the command and returns an error.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string"},
                "description": {"type": "string", "description": "Brief description of what this command does and why."},
                "cwd": {"type": "string", "description": "Absolute path. Defaults to workspace."},
                "timeout_seconds": {"type": "integer", "description": "Kill the process after N seconds (default 30). Ignored when background=true."},
                "background": {"type": "boolean", "description": "Returns shell_id; poll with bash.output."}
            },
            "required": ["cmd", "description"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                maybe_send_user_facing_notice("bash", ctx.as_ref(), &args).await;
                let cmd = args
                    .require_str_field("cmd")
                    .map_err(invalid_input)?
                    .to_owned();
                let cwd_arg = args
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned());
                let timeout_secs = args
                    .get_i64_field("timeout_seconds")
                    .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS as i64)
                    as u64;
                let background = args.get_bool_field("background").unwrap_or(false);

                // Sleep guard: block `sleep N` (N≥2) unless backgrounded
                if !background && let Some(reason) = is_blocking_sleep(&cmd) {
                    return Err(ConduitError::new(ErrorKind::InvalidInput, reason));
                }

                let workspace = ctx
                    .as_ref()
                    .and_then(|c| c.state.get(RUNTIME_WORKSPACE_KEY))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned());
                let target_cwd = cwd_arg.or(workspace);

                let mgr = shell_manager();
                let shell_id = mgr.start(&cmd, target_cwd.as_deref()).await.map_err(|e| {
                    ConduitError::new(ErrorKind::Tool, format!("Failed to start shell: {e}"))
                })?;

                if background {
                    return ok_val(format!(
                        "started: {shell_id} — poll with bash.output(shell_id=\"{shell_id}\"), \
                         stop with bash.kill(shell_id=\"{shell_id}\")"
                    ));
                }

                let result = tokio::time::timeout(
                    Duration::from_secs(timeout_secs),
                    mgr.wait_closed(&shell_id),
                )
                .await;

                match result {
                    Ok(Ok((output, returncode, _status))) => {
                        let code = returncode.unwrap_or(0);
                        let trimmed = output.trim();

                        if code != 0 {
                            // Semantic exit-code interpretation
                            match interpret_exit(&cmd, code) {
                                ExitOutcome::Info(msg) => {
                                    let body = if trimmed.is_empty() {
                                        format!("exit code {code}: {msg}")
                                    } else {
                                        format!("exit code {code}: {msg}\n{trimmed}")
                                    };
                                    return ok_val(maybe_spill_output(&body, "bash"));
                                }
                                ExitOutcome::Error => {
                                    let body = if trimmed.is_empty() {
                                        "(command failed with no output — check if the command exists or try adding 2>&1)".to_owned()
                                    } else {
                                        trimmed.to_owned()
                                    };
                                    return Err(ConduitError::new(
                                        ErrorKind::Tool,
                                        format!(
                                            "command exited with code {code}\noutput:\n{body}\n\n\
                                             [Tip: read the error above to diagnose.]"
                                        ),
                                    ));
                                }
                            }
                        }

                        // Exit 0 — success
                        if trimmed.is_empty() {
                            return ok_val(if is_silent_command(&cmd) {
                                "Done"
                            } else {
                                "(command succeeded, no output)"
                            });
                        }
                        ok_val(maybe_spill_output(trimmed, "bash"))
                    }
                    Ok(Err(e)) => Err(ConduitError::new(ErrorKind::Tool, format!("{e}"))),
                    Err(_) => {
                        let _ = mgr.terminate(&shell_id).await;
                        Err(ConduitError::new(
                            ErrorKind::Tool,
                            format!(
                                "command timed out after {timeout_secs}s and was killed. \
                                 Increase timeout_seconds, use background=true, \
                                 or simplify the command."
                            ),
                        ))
                    }
                }
            })
        },
    )
}

// ---------------------------------------------------------------------------
// bash.output
// ---------------------------------------------------------------------------

fn tool_bash_output() -> Tool {
    Tool::new(
        "bash.output",
        "Read output from a background shell started with bash(background=true).\n\nExamples: tail a dev-server log, watch a long build, capture test output after completion. Pass offset to read only new bytes since last poll.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "shell_id": {"type": "string"},
                "offset": {"type": "integer", "description": "Resume from next_offset of previous call."},
                "limit": {"type": "integer"}
            },
            "required": ["shell_id"]
        }),
        |args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let shell_id = args
                    .require_str_field("shell_id")
                    .map_err(invalid_input)?
                    .to_owned();
                let offset = args.get_i64_field("offset").unwrap_or(0).max(0) as usize;
                let limit = args.get_i64_field("limit").map(|v| v.max(0) as usize);

                let mgr = shell_manager();
                let (output, returncode, status) = mgr
                    .get(&shell_id)
                    .await
                    .map_err(|e| ConduitError::new(ErrorKind::Tool, format!("{e}")))?;

                // If process exited, finalize.
                if returncode.is_some() {
                    let _ = mgr.wait_closed(&shell_id).await;
                }

                let start = offset.min(output.len());
                let end = match limit {
                    Some(l) => (start + l).min(output.len()),
                    None => output.len(),
                };
                let chunk = output[start..end].trim_end();
                let exit_code = match returncode {
                    Some(c) => c.to_string(),
                    None => "null".to_owned(),
                };
                let body = if chunk.is_empty() {
                    if returncode.is_some() {
                        "(process exited, no output)"
                    } else {
                        "(no new output since this offset)"
                    }
                } else {
                    chunk
                };
                ok_val(format!(
                    "id: {shell_id}\nstatus: {status}\nexit_code: {exit_code}\nnext_offset: {end}\noutput:\n{body}"
                ))
            })
        },
    )
}

// ---------------------------------------------------------------------------
// bash.kill
// ---------------------------------------------------------------------------

fn tool_bash_kill() -> Tool {
    Tool::new(
        "bash.kill",
        "Terminate a background shell by shell_id.\n\nExamples: stop a dev server after testing, cancel a hung compilation, clean up a finished log tail.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "shell_id": {"type": "string"}
            },
            "required": ["shell_id"]
        }),
        |args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let shell_id = args
                    .require_str_field("shell_id")
                    .map_err(invalid_input)?
                    .to_owned();
                let mgr = shell_manager();
                let (_output, returncode, status) = mgr
                    .terminate(&shell_id)
                    .await
                    .map_err(|e| ConduitError::new(ErrorKind::Tool, format!("{e}")))?;
                let exit_code = returncode
                    .map(|c| c.to_string())
                    .unwrap_or("null".to_owned());
                ok_val(format!(
                    "id: {shell_id}\nstatus: {status}\nexit_code: {exit_code}"
                ))
            })
        },
    )
}

// ---------------------------------------------------------------------------
// fs.read
// ---------------------------------------------------------------------------

struct FsReadRequest {
    raw_path: String,
    offset: usize,
    limit: Option<usize>,
}

struct FsWriteRequest {
    raw_path: String,
    content: String,
}

struct FsEditRequest {
    raw_path: String,
    old: String,
    new: String,
    start: usize,
}

fn fs_read_request(args: &Value) -> Result<FsReadRequest, ConduitError> {
    Ok(FsReadRequest {
        raw_path: args
            .require_str_field("path")
            .map_err(invalid_input)?
            .to_owned(),
        offset: args.get_i64_field("offset").unwrap_or(0).max(0) as usize,
        limit: args
            .get_i64_field("limit")
            .map(|value| value.max(0) as usize),
    })
}

fn fs_write_request(args: &Value) -> Result<FsWriteRequest, ConduitError> {
    Ok(FsWriteRequest {
        raw_path: args
            .require_str_field("path")
            .map_err(invalid_input)?
            .to_owned(),
        content: args
            .require_str_field("content")
            .map_err(invalid_input)?
            .to_owned(),
    })
}

fn fs_edit_request(args: &Value) -> Result<FsEditRequest, ConduitError> {
    Ok(FsEditRequest {
        raw_path: args
            .require_str_field("path")
            .map_err(invalid_input)?
            .to_owned(),
        old: args
            .require_str_field("old")
            .map_err(invalid_input)?
            .to_owned(),
        new: args
            .require_str_field("new")
            .map_err(invalid_input)?
            .to_owned(),
        start: args.get_i64_field("start").unwrap_or(0).max(0) as usize,
    })
}

async fn run_fs_read(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("fs.read", ctx.as_ref(), &args).await;
    let request = fs_read_request(&args)?;
    let path = resolve_tool_path(ctx, &request.raw_path)?;
    let effective_limit = request.limit.or(Some(DEFAULT_READ_LINE_LIMIT));
    let (mut text, truncated) = read_text_window(&path, request.offset, effective_limit)?;
    if truncated && request.limit.is_none() {
        let next = request.offset + DEFAULT_READ_LINE_LIMIT;
        text.push_str(&format!(
            "\n[... truncated at {DEFAULT_READ_LINE_LIMIT} lines. \
             Use offset={next} limit={DEFAULT_READ_LINE_LIMIT} to continue.]"
        ));
    }
    ok_val(text)
}

async fn run_fs_write(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("fs.write", ctx.as_ref(), &args).await;
    let request = fs_write_request(&args)?;
    let path = resolve_tool_path(ctx, &request.raw_path)?;
    let line_count = request.content.lines().count();
    let byte_count = request.content.len();
    write_text_file(&path, &request.content)?;
    ok_val(format!(
        "wrote: {} ({line_count} lines, {byte_count} bytes)",
        path.display()
    ))
}

/// Best-effort syntax check after an edit.  Returns `Some(errors)` when the
/// checker reports a problem, `None` when the file looks OK or no checker is
/// available for the file type.
fn syntax_check(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    let (cmd, args): (&str, Vec<&str>) = match ext {
        "rs" => ("rustfmt", vec!["--check", "--edition", "2021"]),
        "py" => ("python3", vec!["-m", "py_compile"]),
        "js" | "mjs" => ("node", vec!["--check"]),
        "json" => ("python3", vec!["-m", "json.tool"]),
        _ => return None,
    };
    let output = std::process::Command::new(cmd)
        .args(&args)
        .arg(path)
        .output()
        .ok()?;
    if output.status.success() {
        None
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let combined = if stderr.is_empty() { stdout } else { stderr };
        if combined.is_empty() {
            None
        } else {
            Some(combined)
        }
    }
}

async fn run_fs_edit(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("fs.edit", ctx.as_ref(), &args).await;
    let request = fs_edit_request(&args)?;
    let path = resolve_tool_path(ctx, &request.raw_path)?;
    let old_len = request.old.lines().count();
    let new_len = request.new.lines().count();
    edit_text_file(&path, &request.old, &request.new, request.start)?;
    let mut msg = format!(
        "edited: {} ({old_len} lines → {new_len} lines)",
        path.display()
    );
    if let Some(errors) = syntax_check(&path) {
        msg.push_str(&format!(
            "\n\n⚠ syntax check failed:\n{errors}\n\
             Fix the syntax error with another fs.edit call."
        ));
    }
    ok_val(msg)
}

fn tool_fs_read() -> Tool {
    Tool::with_context(
        "fs.read",
        "Read a text file with line numbers (1-based, like `cat -n`).\n\n\
         Default limit: 500 lines. Use offset/limit to paginate large files.\n\
         Line numbers are for reference only — do NOT include them in fs.edit 'old' parameter.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute or workspace-relative."},
                "offset": {"type": "integer", "description": "0-based line number."},
                "limit": {"type": "integer", "description": "Max lines."}
            },
            "required": ["path"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_fs_read(args, ctx))
        },
    )
}

// ---------------------------------------------------------------------------
// fs.write
// ---------------------------------------------------------------------------

fn tool_fs_write() -> Tool {
    Tool::with_context(
        "fs.write",
        "Create a new text file or fully overwrite an existing one.\n\nExamples: scaffold a new module, generate a config, write test fixtures, save structured output. Auto-creates parent dirs and writes atomically. For partial changes, use fs.edit.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute or workspace-relative."},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_fs_write(args, ctx))
        },
    )
}

// ---------------------------------------------------------------------------
// fs.edit
// ---------------------------------------------------------------------------

fn tool_fs_edit() -> Tool {
    Tool::with_context(
        "fs.edit",
        "Find-and-replace exact text in a file (first match only).\n\n\
         IMPORTANT: fs.read the target range first, then copy the exact file content \
         (without line numbers) into 'old'. Mismatched text is the #1 cause of failures.\n\
         Runs syntax check after edit and warns if errors are detected.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute or workspace-relative."},
                "old": {"type": "string"},
                "new": {"type": "string"},
                "start": {"type": "integer", "description": "0-based line to start search."}
            },
            "required": ["path", "old", "new"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_fs_edit(args, ctx))
        },
    )
}

// ---------------------------------------------------------------------------
// skill
// ---------------------------------------------------------------------------

fn tool_skill() -> Tool {
    Tool::with_context(
        "skill",
        "Load a skill by name and return its instructions.\n\nExamples: read a workflow's step-by-step guide, check what capabilities a plugin provides, look up a tool's parameter schema.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "e.g. 'deploy', 'feishu-calendar'."}
            },
            "required": ["name"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let name = args
                    .require_str_field("name")
                    .map_err(invalid_input)?
                    .to_owned();
                let state = ctx.map(|c| c.state).unwrap_or_default();

                if let Some(Value::Array(allowed)) = state.get("allowed_skills") {
                    let allowed_set: std::collections::HashSet<String> = allowed
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_lowercase())
                        .collect();
                    if !allowed_set.contains(&name.to_lowercase()) {
                        return ok_val(format!("(skill '{name}' is not allowed in this context)"));
                    }
                }

                let workspace = state
                    .get(RUNTIME_WORKSPACE_KEY)
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

                let skills = discover_skills(&workspace);
                let skill_index: HashMap<String, _> = skills
                    .into_iter()
                    .map(|s| (s.name.to_lowercase(), s))
                    .collect();

                match skill_index.get(&name.to_lowercase()) {
                    Some(skill) => {
                        let body = skill.body().unwrap_or_default();
                        let body_str = if body.is_empty() {
                            "(no content)".to_owned()
                        } else {
                            body
                        };
                        ok_val(format!(
                            "Location: {}\n---\n{body_str}",
                            skill.location.display()
                        ))
                    }
                    None => ok_val("(no such skill)"),
                }
            })
        },
    )
}

fn tool_evolution_capture() -> Tool {
    Tool::with_context(
        "evolution.capture",
        "Capture a governed self-evolution candidate for later review.\n\nExamples: save a stable collaboration rule learned during a task, draft a reusable procedure as a skill candidate, compile reusable knowledge into a searchable artifact, or record a runtime policy without applying it immediately.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["prompt_rule", "skill", "compiled_knowledge", "runtime_policy"]},
                "title": {"type": "string"},
                "summary": {"type": "string"},
                "content": {"type": "string"},
                "skill_name": {"type": "string", "description": "Required when kind=skill."},
                "artifact_name": {"type": "string", "description": "Required when kind=compiled_knowledge or kind=runtime_policy."}
            },
            "required": ["kind", "title", "summary", "content"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_capture(args, ctx))
        },
    )
}

fn tool_evolution_distill() -> Tool {
    Tool::with_context(
        "evolution.distill",
        "Distill tape evidence into pending prompt-rule candidates.\n\nExamples: preview what a tape would yield before persisting it, write distilled prompt-rule candidates from a named tape, derive candidates from the current tape when no tape is provided.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "tape": {"type": "string", "description": "Tape name; defaults to the active tape in context when omitted."},
                "persist": {"type": "boolean", "description": "Persist the distilled candidates instead of running a dry-run.", "default": false}
            }
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_distill(args, ctx))
        },
    )
}

fn tool_evolution_history() -> Tool {
    Tool::with_context(
        "evolution.history",
        "Inspect automation history for governed self-evolution.\n\nExamples: review the latest captured, evaluated, promoted, or rolled back candidates, scan the newest automation actions, inspect how a rule changed over time.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "description": "Maximum entries to return.", "default": 20}
            }
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_history(args, ctx))
        },
    )
}

fn tool_evolution_auto_run() -> Tool {
    Tool::with_context(
        "evolution.auto_run",
        "Run the auto-evolution loop on a tape.\n\nExamples: distill tape evidence, evaluate the resulting candidates, promote the passing ones in one step.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "tape": {"type": "string", "description": "Tape name to process."}
            },
            "required": ["tape"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_auto_run(args, ctx))
        },
    )
}

fn tool_evolution_list() -> Tool {
    Tool::with_context(
        "evolution.list",
        "List self-evolution candidates in the current workspace.\n\nExamples: review pending prompt-rule drafts, inspect promoted skill drafts, check whether a remembered workflow still needs approval.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "enum": ["pending", "promoted", "rejected", "rolled_back"]}
            }
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_list(args, ctx))
        },
    )
}

fn tool_evolution_show() -> Tool {
    Tool::with_context(
        "evolution.show",
        "Show a self-evolution candidate in full.\n\nExamples: inspect the exact text of a pending prompt rule, read the body of a skill draft before approving it, verify where a promoted candidate landed.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"}
            },
            "required": ["id"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_show(args, ctx))
        },
    )
}

fn tool_evolution_evaluate() -> Tool {
    Tool::with_context(
        "evolution.evaluate",
        "Run the deterministic self-evolution evaluator for a pending candidate.\n\nExamples: verify that a prompt rule survives prompt composition, check that a skill draft materializes cleanly, inspect integration regressions before promotion.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"}
            },
            "required": ["id"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_evaluate(args, ctx))
        },
    )
}

fn tool_evolution_promote() -> Tool {
    Tool::with_context(
        "evolution.promote",
        "Promote a governed self-evolution candidate into the active rules or skills store.\n\nExamples: publish a passing prompt rule into the Evolved section, materialize an approved skill draft, force-publish an already reviewed candidate.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "force": {"type": "boolean"}
            },
            "required": ["id"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_promote(args, ctx))
        },
    )
}

fn tool_evolution_reject() -> Tool {
    Tool::with_context(
        "evolution.reject",
        "Reject a pending self-evolution candidate.\n\nExamples: discard a noisy prompt rule, close out a low-quality skill draft, mark a candidate as intentionally not promoted.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"}
            },
            "required": ["id"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_reject(args, ctx))
        },
    )
}

fn tool_evolution_rollback() -> Tool {
    Tool::with_context(
        "evolution.rollback",
        "Roll back a promoted self-evolution candidate to its captured snapshot.\n\nExamples: undo a prompt-rule promotion that caused regressions, restore a previous skill file after a bad promotion, revert an experimental evolution safely.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"}
            },
            "required": ["id"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(run_evolution_rollback(args, ctx))
        },
    )
}

async fn run_evolution_capture(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.capture", ctx.as_ref(), &args).await;
    let kind = args.require_str_field("kind").map_err(invalid_input)?;
    let title = args.require_str_field("title").map_err(invalid_input)?;
    let summary = args.require_str_field("summary").map_err(invalid_input)?;
    let content = args.require_str_field("content").map_err(invalid_input)?;
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let tape = ctx.as_ref().and_then(|item| item.tape.clone());
    let candidate = capture_candidate(&store, kind, title, summary, content, &args, tape)?;
    ok_val(format!("candidate captured: {} ({kind})", candidate.id))
}

async fn run_evolution_list(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.list", ctx.as_ref(), &args).await;
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let status = parse_candidate_status(args.get_str_field("status"))?;
    let candidates = store.list_candidates().map_err(tool_error)?;
    let filtered = filter_candidates(candidates, status);
    ok_val(render_candidate_list(&filtered))
}

async fn run_evolution_show(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.show", ctx.as_ref(), &args).await;
    let id = args.require_str_field("id").map_err(invalid_input)?;
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let candidate = store.read_candidate(id).map_err(tool_error)?;
    ok_val(render_candidate_detail(&candidate))
}

async fn run_evolution_distill(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.distill", ctx.as_ref(), &args).await;
    let tape = args
        .get_str_field("tape")
        .map(str::to_owned)
        .or_else(|| ctx.as_ref().and_then(|item| item.tape.clone()))
        .ok_or_else(|| ConduitError::new(ErrorKind::Tool, "tool requires an active tape name"))?;
    let persist = args.get_bool_field("persist").unwrap_or(false);
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let outcome = store
        .distill_tape(&tapes_dir_from_context(ctx.as_ref()), &tape, persist)
        .map_err(tool_error)?;
    ok_val(render_distill_result(&outcome))
}

async fn run_evolution_history(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.history", ctx.as_ref(), &args).await;
    let limit = args.get_i64_field("limit").unwrap_or(20).max(0) as usize;
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let entries = store.load_auto_journal().map_err(tool_error)?;
    ok_val(render_history_output(&entries, limit))
}

async fn run_evolution_auto_run(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.auto_run", ctx.as_ref(), &args).await;
    let tape = args.require_str_field("tape").map_err(invalid_input)?;
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let policy = store
        .load_runtime_policy()
        .map_err(tool_error)?
        .apply_to_auto_policy(AutoEvolutionPolicy::default())
        .ok_or_else(|| {
            ConduitError::new(ErrorKind::Tool, "auto evolution disabled by runtime policy")
        })?;
    let outcome = store
        .auto_evolve_tape(&tapes_dir_from_context(ctx.as_ref()), tape, policy)
        .map_err(tool_error)?;
    ok_val(render_auto_run_result(&outcome))
}

async fn run_evolution_evaluate(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.evaluate", ctx.as_ref(), &args).await;
    let id = args.require_str_field("id").map_err(invalid_input)?;
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let run = store.evaluate(id).map_err(tool_error)?;
    ok_val(render_evaluation_run(&run))
}

async fn run_evolution_promote(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.promote", ctx.as_ref(), &args).await;
    let id = args.require_str_field("id").map_err(invalid_input)?;
    let force = args.get_bool_field("force").unwrap_or(false);
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let outcome = store.promote(id, force).map_err(tool_error)?;
    ok_val(format!(
        "promoted {} -> {}",
        outcome.candidate.id,
        outcome.target.display()
    ))
}

async fn run_evolution_reject(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.reject", ctx.as_ref(), &args).await;
    let id = args.require_str_field("id").map_err(invalid_input)?;
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let candidate = store.reject(id).map_err(tool_error)?;
    ok_val(format!("rejected {}", candidate.id))
}

async fn run_evolution_rollback(args: Value, ctx: Option<ToolContext>) -> ToolResult {
    maybe_send_user_facing_notice("evolution.rollback", ctx.as_ref(), &args).await;
    let id = args.require_str_field("id").map_err(invalid_input)?;
    let store = EvolutionStore::new(workspace_from_context(ctx.as_ref()));
    let outcome = store.rollback(id).map_err(tool_error)?;
    ok_val(format!(
        "rolled_back {} -> {}",
        outcome.candidate.id,
        outcome.target.display()
    ))
}

fn workspace_from_context(ctx: Option<&ToolContext>) -> PathBuf {
    ctx.and_then(|item| item.state.get(RUNTIME_WORKSPACE_KEY))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn tapes_dir_from_context(ctx: Option<&ToolContext>) -> PathBuf {
    ctx.and_then(|item| item.state.get(RUNTIME_TAPES_DIR_KEY))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| eli_home().join("tapes"))
}

fn capture_candidate(
    store: &EvolutionStore,
    kind: &str,
    title: &str,
    summary: &str,
    content: &str,
    args: &Value,
    tape: Option<String>,
) -> Result<crate::evolution::EvolutionCandidate, ConduitError> {
    match kind {
        "prompt_rule" => store
            .capture_rule(title, summary, content, tape, "tool")
            .map_err(tool_error),
        "skill" => capture_skill_candidate(store, title, summary, content, args, tape),
        "compiled_knowledge" => {
            capture_knowledge_candidate(store, title, summary, content, args, tape)
        }
        "runtime_policy" => {
            capture_runtime_policy_candidate(store, title, summary, content, args, tape)
        }
        _ => Err(ConduitError::new(
            ErrorKind::InvalidInput,
            "kind must be 'prompt_rule', 'skill', 'compiled_knowledge', or 'runtime_policy'",
        )),
    }
}

fn capture_skill_candidate(
    store: &EvolutionStore,
    title: &str,
    summary: &str,
    content: &str,
    args: &Value,
    tape: Option<String>,
) -> Result<crate::evolution::EvolutionCandidate, ConduitError> {
    let skill_name = args
        .require_str_field("skill_name")
        .map_err(invalid_input)?;
    store
        .capture_skill(skill_name, title, summary, content, tape, "tool")
        .map_err(tool_error)
}

fn capture_knowledge_candidate(
    store: &EvolutionStore,
    title: &str,
    summary: &str,
    content: &str,
    args: &Value,
    tape: Option<String>,
) -> Result<crate::evolution::EvolutionCandidate, ConduitError> {
    let artifact_name = args
        .require_str_field("artifact_name")
        .map_err(invalid_input)?;
    store
        .capture_compiled_knowledge(artifact_name, title, summary, content, tape, "tool")
        .map_err(tool_error)
}

fn capture_runtime_policy_candidate(
    store: &EvolutionStore,
    title: &str,
    summary: &str,
    content: &str,
    args: &Value,
    tape: Option<String>,
) -> Result<crate::evolution::EvolutionCandidate, ConduitError> {
    let artifact_name = args
        .require_str_field("artifact_name")
        .map_err(invalid_input)?;
    store
        .capture_runtime_policy(artifact_name, title, summary, content, tape, "tool")
        .map_err(tool_error)
}

fn parse_candidate_status(raw: Option<&str>) -> Result<Option<CandidateStatus>, ConduitError> {
    match raw {
        None | Some("") => Ok(None),
        Some("pending") => Ok(Some(CandidateStatus::Pending)),
        Some("promoted") => Ok(Some(CandidateStatus::Promoted)),
        Some("rejected") => Ok(Some(CandidateStatus::Rejected)),
        Some("rolled_back") => Ok(Some(CandidateStatus::RolledBack)),
        Some(_) => Err(ConduitError::new(
            ErrorKind::InvalidInput,
            "status must be pending, promoted, rejected, or rolled_back",
        )),
    }
}

fn filter_candidates(
    candidates: Vec<crate::evolution::EvolutionCandidate>,
    status: Option<CandidateStatus>,
) -> Vec<crate::evolution::EvolutionCandidate> {
    candidates
        .into_iter()
        .filter(|candidate| status.is_none_or(|expected| candidate.status == expected))
        .collect()
}

fn render_candidate_list(candidates: &[crate::evolution::EvolutionCandidate]) -> String {
    if candidates.is_empty() {
        return "No evolution candidates.".to_owned();
    }
    candidates
        .iter()
        .map(candidate_summary_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn candidate_summary_line(candidate: &crate::evolution::EvolutionCandidate) -> String {
    format!(
        "{}  {}  {}",
        candidate.id,
        candidate.status_string(),
        candidate.title
    )
}

fn render_candidate_detail(candidate: &crate::evolution::EvolutionCandidate) -> String {
    [
        format!("id: {}", candidate.id),
        format!("status: {}", candidate.status_string()),
        format!("kind: {}", candidate.kind_string()),
        format!("title: {}", candidate.title),
        format!("summary: {}", candidate.summary),
        format!("risk_level: {}", candidate.risk_level_string()),
        format!("fingerprint: {}", candidate.effective_fingerprint()),
        format!("requires_evaluation: {}", candidate.requires_evaluation),
        format!(
            "latest_evaluation_id: {}",
            candidate.latest_evaluation_id.clone().unwrap_or_default()
        ),
        format!(
            "evaluation_passed: {}",
            candidate
                .evaluation_passed
                .map(|value| value.to_string())
                .unwrap_or_default()
        ),
        format!(
            "promoted_to: {}",
            candidate.promoted_to.clone().unwrap_or_default()
        ),
        String::new(),
        candidate.content.clone(),
    ]
    .join("\n")
}

fn render_distill_result(outcome: &DistillOutcome) -> String {
    let mode = if outcome.persisted {
        "Distilled"
    } else {
        "Previewed"
    };
    format!(
        "{mode} tape {}: {} prompt-rule candidates, {} skipped.",
        outcome.tape,
        outcome.candidates.len(),
        outcome.skipped.len()
    )
}

fn render_history_output(entries: &[AutoJournalEntry], limit: usize) -> String {
    let lines = history_lines(entries, limit);
    if lines.is_empty() {
        "No evolution history.".to_owned()
    } else {
        lines.join("\n")
    }
}

fn render_auto_run_result(outcome: &crate::evolution::AutoEvolutionReport) -> String {
    format!(
        "Auto-ran tape {}: distilled {}, skipped {}, evaluated {}, observed {}, staged {}, promoted {}, expired {}.",
        outcome.tape,
        outcome.distill.candidates.len(),
        outcome.distill.skipped.len(),
        outcome.evaluations.len(),
        outcome.observed.len(),
        outcome.staged.len(),
        outcome.promoted.len(),
        outcome.expired.len(),
    )
}

fn history_lines(entries: &[AutoJournalEntry], limit: usize) -> Vec<String> {
    let mut entries = entries.to_vec();
    entries.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    entries
        .into_iter()
        .take(limit)
        .map(render_history_entry)
        .collect()
}

fn render_evaluation_run(run: &EvaluationRun) -> String {
    let mut lines = vec![
        format!("id: {}", run.id),
        format!("candidate_id: {}", run.candidate_id),
        format!("passed: {}", run.passed),
        format!("score: {}", run.score),
    ];
    lines.extend(run.checks.iter().map(render_evaluation_check));
    lines.join("\n")
}

fn render_evaluation_check(check: &crate::evolution::EvaluationCheck) -> String {
    format!("- {}: {} ({})", check.name, check.passed, check.detail)
}

fn render_history_entry(entry: AutoJournalEntry) -> String {
    format!(
        "{}  {}  {}  {}  {}",
        entry.created_at,
        render_action(entry.action),
        entry.candidate_id.unwrap_or_default(),
        shorten(&entry.tape, 24),
        entry.detail,
    )
}

fn render_action(action: AutoJournalAction) -> &'static str {
    match action {
        AutoJournalAction::Distilled => "distilled",
        AutoJournalAction::Evaluated => "evaluated",
        AutoJournalAction::Observed => "observed",
        AutoJournalAction::Staged => "staged",
        AutoJournalAction::Promoted => "promoted",
        AutoJournalAction::Rejected => "rejected",
        AutoJournalAction::RolledBack => "rolled_back",
        AutoJournalAction::Expired => "expired",
        AutoJournalAction::Held => "held",
    }
}

fn shorten(text: &str, width: usize) -> String {
    let mut chars = text.chars();
    let body: String = chars.by_ref().take(width).collect();
    if chars.next().is_none() {
        body
    } else {
        format!("{body}...")
    }
}

// ---------------------------------------------------------------------------
// tape.info
// ---------------------------------------------------------------------------

fn tool_tape_info() -> Tool {
    Tool::with_context(
        "tape.info",
        "Get tape metadata: entry count, anchors, token usage.\n\nExamples: check context size before a handoff, decide whether a reset is needed, monitor token consumption.",
        serde_json::json!({
            "type": "object",
            "properties": {
            }
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                maybe_send_user_facing_notice("tape.info", ctx.as_ref(), &args).await;
                let tape_name = tape_name_from_context(ctx.as_ref())?;
                let service = current_tape_service()?;
                let info = service.info(&tape_name).await?;
                ok_val(format_tape_info(&info))
            })
        },
    )
}

// ---------------------------------------------------------------------------
// tape.search
// ---------------------------------------------------------------------------

fn tool_tape_search() -> Tool {
    Tool::with_context(
        "tape.search",
        "Search the conversation tape by keyword.\n\nExamples: recall a previous decision, find an earlier tool result, locate an error from a past turn, review what was discussed in a date range. For file content search, use bash(grep).",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "Default 20."},
                "start": {"type": "string", "description": "ISO date."},
                "end": {"type": "string", "description": "ISO date."},
                "kinds": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Default: message, tool_result."
                }
            },
            "required": ["query"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                maybe_send_user_facing_notice("tape.search", ctx.as_ref(), &args).await;
                let query_text = args
                    .require_str_field("query")
                    .map_err(invalid_input)?
                    .to_owned();
                if query_text.trim().is_empty() {
                    return Err(ConduitError::new(
                        ErrorKind::InvalidInput,
                        "query must not be empty",
                    ));
                }
                let limit = args.get_i64_field("limit").unwrap_or(20) as usize;
                let tape_name = tape_name_from_context(ctx.as_ref())?;
                let service = current_tape_service()?;

                let kinds = args
                    .get("kinds")
                    .and_then(|v| v.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| {
                                serde_json::from_value::<TapeEntryKind>(value.clone()).ok()
                            })
                            .collect::<Vec<_>>()
                    })
                    .filter(|kinds| !kinds.is_empty())
                    .unwrap_or_else(|| vec![TapeEntryKind::Message, TapeEntryKind::ToolResult]);

                let mut query = TapeQuery::new(&tape_name).kinds(kinds);
                if let (Some(start), Some(end)) =
                    (args.get_str_field("start"), args.get_str_field("end"))
                {
                    query = query.between_dates(start.to_owned(), end.to_owned());
                }

                let entries = service.search(&query).await?;
                let needle = query_text.to_lowercase();
                let matches = entries
                    .into_iter()
                    .filter(|entry| entry_search_text(entry).contains(&needle))
                    .take(limit)
                    .map(|entry| render_search_entry(&entry))
                    .collect::<Vec<_>>();

                if matches.is_empty() {
                    ok_val("(no matches)")
                } else {
                    ok_val(matches.join("\n"))
                }
            })
        },
    )
}

// ---------------------------------------------------------------------------
// tape.reset
// ---------------------------------------------------------------------------

fn tool_tape_reset() -> Tool {
    Tool::with_context(
        "tape.reset",
        "Wipe the current tape and start fresh.\n\nExamples: context grew too large, task shifted entirely, need to discard a failed exploration. Set archive=true to preserve a snapshot — without it the wipe is irreversible.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "archive": {"type": "boolean", "description": "Default false."}
            }
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                maybe_send_user_facing_notice("tape.reset", ctx.as_ref(), &args).await;
                let archive = args.get_bool_field("archive").unwrap_or(false);
                let tape_name = tape_name_from_context(ctx.as_ref())?;
                let service = current_tape_service()?;
                let result = service.reset(&tape_name, archive).await?;
                ok_val(result)
            })
        },
    )
}

// ---------------------------------------------------------------------------
// tape.handoff
// ---------------------------------------------------------------------------

fn tool_tape_handoff() -> Tool {
    Tool::with_context(
        "tape.handoff",
        "Save a named checkpoint (anchor) to the tape with a summary for later resumption.\n\n\
         Summary priority: 1) architecture decisions (never summarize) 2) modified files + key changes 3) verification status 4) open TODOs/rollback notes 5) tool outputs (keep pass/fail only).",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Default: handoff."},
                "summary": {"type": "string", "description": "Context for resuming later."}
            }
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                maybe_send_user_facing_notice("tape.handoff", ctx.as_ref(), &args).await;
                let name = args.get_str_field("name").unwrap_or("handoff").to_owned();
                let summary = args.get_str_field("summary").unwrap_or("").to_owned();
                let tape_name = tape_name_from_context(ctx.as_ref())?;
                let service = current_tape_service()?;
                // Capture entries since last anchor before creating the new one.
                let info = service.info(&tape_name).await?;
                let captured = info.entries_since_last_anchor;
                let state = if summary.is_empty() {
                    None
                } else {
                    Some(serde_json::json!({"summary": summary}))
                };
                service.handoff(&tape_name, &name, state).await?;
                ok_val(format!(
                    "anchor added: {name} (captured {captured} entries since last anchor)"
                ))
            })
        },
    )
}

// ---------------------------------------------------------------------------
// tape.anchors
// ---------------------------------------------------------------------------

fn tool_tape_anchors() -> Tool {
    Tool::with_context(
        "tape.anchors",
        "List all anchors (checkpoints) in the tape.\n\nExamples: review the session timeline, find a handoff point to resume from, check how many phases have been completed.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "description": "Default 20."}
            }
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                maybe_send_user_facing_notice("tape.anchors", ctx.as_ref(), &args).await;
                let limit = args.get_i64_field("limit").unwrap_or(20) as usize;
                let tape_name = tape_name_from_context(ctx.as_ref())?;
                let service = current_tape_service()?;
                let anchors = service.anchors(&tape_name, limit).await?;
                ok_val(format_anchor_summaries(&anchors))
            })
        },
    )
}

// ---------------------------------------------------------------------------
// decision.set / decision.list / decision.remove
// ---------------------------------------------------------------------------

/// Maximum decision text length before truncation.
const MAX_DECISION_TEXT_LEN: usize = 500;

fn tool_decision_set() -> Tool {
    Tool::with_context(
        "decision.set",
        "Pin a decision so it persists across turns and anchor boundaries.\n\nExamples: lock in a tech choice after discussion, record an agreed architecture constraint before moving on, capture a deployment target once confirmed by the user.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"}
            },
            "required": ["text"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let text = args
                    .require_str_field("text")
                    .map_err(invalid_input)?
                    .to_owned();
                if text.trim().is_empty() {
                    return Err(ConduitError::new(
                        ErrorKind::InvalidInput,
                        "decision text must not be empty",
                    ));
                }
                let text = if text.len() > MAX_DECISION_TEXT_LEN {
                    let truncated = &text[..text.floor_char_boundary(MAX_DECISION_TEXT_LEN)];
                    format!("{}...", truncated)
                } else {
                    text
                };
                let tape_name = tape_name_from_context(ctx.as_ref())?;
                let service = current_tape_service()?;
                let meta = serde_json::json!({});
                let entry = TapeEntry::decision(&text, meta);
                service.store().append(&tape_name, &entry).await?;
                // Count active decisions after this append.
                let query = TapeQuery::new(&tape_name);
                let entries = service.store().fetch_all(&query).await?;
                let total = nexil::collect_active_decisions(&entries).len();
                tracing::info!(decision = %text, tape = %tape_name, "decision.set");
                ok_val(format!("Decision recorded: {text} ({total} active)"))
            })
        },
    )
}

fn tool_decision_list() -> Tool {
    Tool::with_context(
        "decision.list",
        "Show active decisions for this session.\n\nExamples: verify assumptions before starting a new task, check for stale decisions after scope changes, recap context when resuming after a break.",
        serde_json::json!({
            "type": "object",
            "properties": {}
        }),
        |_args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let tape_name = tape_name_from_context(ctx.as_ref())?;
                let service = current_tape_service()?;
                let query = TapeQuery::new(&tape_name);
                let entries = service.store().fetch_all(&query).await?;
                let decisions = nexil::collect_active_decisions(&entries);
                if decisions.is_empty() {
                    return ok_val("No active decisions.");
                }
                let mut output = format!("Active decisions ({}):\n", decisions.len());
                for (i, d) in decisions.iter().enumerate() {
                    output.push_str(&format!("  {}. {}\n", i + 1, d));
                }
                ok_val(output.trim_end())
            })
        },
    )
}

fn tool_decision_remove() -> Tool {
    Tool::with_context(
        "decision.remove",
        "Revoke a decision by its number (from decision.list).\n\nExamples: drop a tech choice after pivoting, clear a constraint the user overruled, remove a duplicate created by mistake.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "index": {"type": "integer", "description": "1-based, from decision.list."}
            },
            "required": ["index"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let index = args.get_i64_field("index").ok_or_else(|| {
                    ConduitError::new(ErrorKind::InvalidInput, "missing required argument 'index'")
                })? as usize;
                if index == 0 {
                    return Err(ConduitError::new(
                        ErrorKind::InvalidInput,
                        "index must be 1 or greater",
                    ));
                }
                let tape_name = tape_name_from_context(ctx.as_ref())?;
                let service = current_tape_service()?;
                let query = TapeQuery::new(&tape_name);
                let entries = service.store().fetch_all(&query).await?;
                let decisions = nexil::collect_active_decisions(&entries);
                if index > decisions.len() {
                    return Err(ConduitError::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "no decision #{index}. There are {} active decisions.",
                            decisions.len()
                        ),
                    ));
                }
                let text = &decisions[index - 1];
                let remaining = decisions.len() - 1;
                let meta = serde_json::json!({});
                let tombstone = TapeEntry::decision_revoked(text, meta);
                service.store().append(&tape_name, &tombstone).await?;
                tracing::info!(decision = %text, tape = %tape_name, "decision.remove");
                ok_val(format!("Removed decision: {text} ({remaining} remaining)"))
            })
        },
    )
}

// ---------------------------------------------------------------------------
// web.fetch
// ---------------------------------------------------------------------------

fn tool_web_fetch() -> Tool {
    Tool::with_context(
        "web.fetch",
        "Fetch a URL (HTTP GET) and return content as markdown.\n\nExamples: read documentation, check a REST API response, pull a raw GitHub file, retrieve release notes. Supports custom headers and timeout. Static content only — no JS rendering.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "headers": {"type": "object"},
                "timeout": {"type": "integer", "description": "Seconds. Default 10."}
            },
            "required": ["url"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                maybe_send_user_facing_notice("web.fetch", ctx.as_ref(), &args).await;
                let url = args
                    .require_str_field("url")
                    .map_err(invalid_input)?
                    .to_owned();
                let timeout_secs = args
                    .get_i64_field("timeout")
                    .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECONDS as i64)
                    as u64;

                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(timeout_secs))
                    .build()
                    .map_err(|e| {
                        ConduitError::new(ErrorKind::Tool, format!("http client error: {e}"))
                    })?;

                let mut request = client.get(&url);
                request = request.header("accept", "text/markdown");

                if let Some(Value::Object(headers)) = args.get("headers") {
                    /// Headers that must not be set by LLM-controlled input to
                    /// prevent request smuggling and protocol-level attacks.
                    const BLOCKED_HEADERS: &[&str] = &[
                        "host",
                        "content-length",
                        "transfer-encoding",
                        "connection",
                        "upgrade",
                        "proxy-authorization",
                        "te",
                    ];
                    for (k, v) in headers {
                        if BLOCKED_HEADERS.contains(&k.to_ascii_lowercase().as_str()) {
                            continue;
                        }
                        if let Some(val) = v.as_str() {
                            request = request.header(k.as_str(), val);
                        }
                    }
                }

                let response = request.send().await.map_err(|e| {
                    ConduitError::new(ErrorKind::Tool, format!("fetch failed: {e}"))
                })?;
                let status = response.status();
                if !status.is_success() {
                    return Err(ConduitError::new(
                        ErrorKind::Tool,
                        format!(
                            "HTTP {status} for {url}. \
                             For 404: check URL. For 401/403: set headers."
                        ),
                    ));
                }
                let bytes = response.bytes().await.map_err(|e| {
                    ConduitError::new(ErrorKind::Tool, format!("read body failed: {e}"))
                })?;
                if bytes.len() > MAX_RESPONSE_BYTES {
                    return Err(ConduitError::new(
                        ErrorKind::Tool,
                        format!(
                            "response too large ({} bytes, limit {}). \
                             Try a more specific endpoint, add query params to narrow results, \
                             or use bash with curl piped through head/jq.",
                            bytes.len(),
                            MAX_RESPONSE_BYTES
                        ),
                    ));
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                // Spill large pages to disk (preview + path) so a big-but-under-cap
                // response doesn't flood the context window; full content stays
                // recoverable via fs.read, exactly like bash output.
                ok_val(maybe_spill_output(&text, "web"))
            })
        },
    )
}

// ---------------------------------------------------------------------------
// agent (replaces subagent)
// ---------------------------------------------------------------------------

fn tool_agent() -> Tool {
    Tool::with_context(
        "agent",
        "Launch a sub-agent (claude/codex/kimi) for an independent, well-scoped task.\n\n\
         Default: synchronous (waits, returns output). Set `run_in_background: true` for async — result arrives as an inbound message.\n\
         Use for: parallelizable work, long-running changes (refactors, migrations), cross-repo work, research-while-you-build.\n\
         Don't use for: tasks depending on your current work, trivial tasks (<30s), tasks needing interactive input.\n\
         Set `isolation: \"worktree\"` to run in a temporary git worktree (auto-removed if no changes).",
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Complete task description for the sub-agent."
                },
                "description": {
                    "type": "string",
                    "description": "Short (3-5 word) summary of the task."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "If true, returns immediately with agent_id. Result injected as inbound message later. Default: false (sync)."
                },
                "isolation": {
                    "type": "string",
                    "enum": ["worktree"],
                    "description": "Run in an isolated git worktree."
                },
                "cwd": {
                    "type": "string",
                    "description": "Absolute path. Defaults to workspace."
                },
                "cli": {
                    "type": "string",
                    "description": "CLI to use: 'claude', 'codex', 'kimi'. Auto-detected if omitted."
                }
            },
            "required": ["prompt", "description"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                maybe_send_user_facing_notice("agent", ctx.as_ref(), &args).await;

                let prompt = args
                    .require_str_field("prompt")
                    .map_err(invalid_input)?
                    .to_owned();
                if prompt.trim().is_empty() {
                    return Err(ConduitError::new(
                        ErrorKind::InvalidInput,
                        "prompt must not be empty",
                    ));
                }
                let description = args
                    .get_str_field("description")
                    .unwrap_or("agent task")
                    .to_owned();
                let run_in_background = args
                    .get("run_in_background")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let isolation = args.get_str_field("isolation").map(|s| s.to_owned());

                let cli_arg = args
                    .get_str_field("cli")
                    .map(str::trim)
                    .filter(|s| !s.is_empty());

                let state = ctx.map(|c| c.state).unwrap_or_default();
                let workspace = args
                    .get_str_field("cwd")
                    .map(|s| s.to_owned())
                    .or_else(|| {
                        state
                            .get(RUNTIME_WORKSPACE_KEY)
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_owned())
                    })
                    .unwrap_or_else(|| {
                        std::env::current_dir()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| ".".to_owned())
                    });

                // --- Worktree isolation ---
                let effective_cwd = if isolation.as_deref() == Some("worktree") {
                    match crate::builtin::subagent::worktree::create_worktree(Path::new(&workspace))
                        .await
                    {
                        Ok(wt_path) => wt_path.to_string_lossy().to_string(),
                        Err(e) => {
                            tracing::warn!(error = %e, "worktree creation failed, using workspace");
                            workspace.clone()
                        }
                    }
                } else {
                    workspace.clone()
                };

                // --- Resolve CLI (fallback to in-process if not found) ---
                let cli = match resolve_cli(cli_arg) {
                    Ok(c) => c,
                    Err(_) if !run_in_background => {
                        // In-process fallback for sync mode.
                        tracing::info!("no external CLI found, running agent in-process");
                        let result = crate::builtin::subagent::fallback::run_in_process(
                            &prompt,
                            &effective_cwd,
                            None,
                        )
                        .await;

                        // Cleanup worktree if applicable.
                        let worktree_info = if isolation.as_deref() == Some("worktree")
                            && effective_cwd != workspace
                        {
                            Some(
                                crate::builtin::subagent::worktree::cleanup_worktree(Path::new(
                                    &effective_cwd,
                                ))
                                .await,
                            )
                        } else {
                            None
                        };

                        return match result {
                            Ok(r) => {
                                let mut result_json = serde_json::json!({
                                    "status": "completed",
                                    "engine": "in-process",
                                    "content": r.content,
                                    "duration_ms": r.duration_ms,
                                });
                                append_worktree_info(&mut result_json, worktree_info);
                                Ok(result_json)
                            }
                            Err(e) => Ok(serde_json::json!({
                                "status": "error",
                                "engine": "in-process",
                                "error": e.message,
                            })),
                        };
                    }
                    Err(e) => return Err(e),
                };

                // --- CLI-based execution ---
                let prompt_tempfile = write_prompt_tempfile(&prompt)?;
                let prompt_path = prompt_tempfile
                    .path()
                    .to_str()
                    .ok_or_else(|| {
                        ConduitError::new(ErrorKind::Tool, "prompt tempfile path not UTF-8")
                    })?
                    .to_owned();

                let pre_head = snapshot_git_head(&effective_cwd);
                let full_cmd = build_cli_command(&cli, &prompt_path);

                let mgr = shell_manager();
                let shell_id = mgr
                    .start(&full_cmd, Some(&effective_cwd))
                    .await
                    .map_err(|e| {
                        ConduitError::new(ErrorKind::Tool, format!("failed to start CLI: {e}"))
                    })?;

                let agent_id = shell_id.replace("bash-", "agent-");
                let cli_name = cli.name.clone();

                // --- Sync mode (default): wait for completion ---
                if !run_in_background {
                    // Keep prompt file alive during execution.
                    let _prompt_file = prompt_tempfile;
                    let start = std::time::Instant::now();

                    let (output, exit_code, _) = mgr
                        .wait_closed(&shell_id)
                        .await
                        .unwrap_or_else(|e| (e.to_string(), Some(-1), "error".to_owned()));

                    let artifacts = collect_artifacts(&effective_cwd, pre_head.as_deref()).await;
                    let duration_ms = start.elapsed().as_millis() as u64;

                    // Cleanup worktree if applicable.
                    let worktree_info =
                        if isolation.as_deref() == Some("worktree") && effective_cwd != workspace {
                            Some(
                                crate::builtin::subagent::worktree::cleanup_worktree(Path::new(
                                    &effective_cwd,
                                ))
                                .await,
                            )
                        } else {
                            None
                        };

                    let status = match exit_code {
                        Some(0) => "completed",
                        Some(_) => "failed",
                        None => "unknown",
                    };
                    let content = truncate_output(&output);

                    let mut result_json = serde_json::json!({
                        "status": status,
                        "engine": cli_name,
                        "exit_code": exit_code,
                        "content": content,
                        "changes": artifacts,
                        "duration_ms": duration_ms,
                    });
                    append_worktree_info(&mut result_json, worktree_info);

                    return Ok(result_json);
                }

                // --- Async mode: fire-and-forget ---
                let tracker = crate::builtin::subagent::tracker::agent_tracker();
                let prompt_summary: String = description.chars().take(100).collect();
                if !tracker
                    .register(
                        &agent_id,
                        Some(shell_id.clone()),
                        "general-purpose",
                        &prompt_summary,
                        &effective_cwd,
                        &cli_name,
                    )
                    .await
                {
                    // At capacity — kill the process and return error.
                    let _ = mgr.terminate(&shell_id).await;
                    return Err(ConduitError::new(
                        ErrorKind::Tool,
                        format!(
                            "max concurrent background agents reached ({}). \
                             Wait for a running agent to finish, or run synchronously.",
                            tracker.running_count().await + 1
                        ),
                    ));
                }

                let inject_fn = crate::control_plane::inbound_injector();
                let session_id = state
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let chat_id = state
                    .get("chat_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let output_channel = state
                    .get("output_channel")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let inbound_context = state
                    .get("_inbound_context")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();

                let monitor_agent_id = agent_id.clone();
                let monitor_cli_name = cli_name.clone();
                let monitor_shell_id = shell_id.clone();
                let monitor_workspace = effective_cwd.clone();
                let monitor_isolation = isolation.clone();
                let monitor_orig_workspace = workspace.clone();

                tokio::spawn(async move {
                    let _prompt_file = prompt_tempfile;

                    let mgr = shell_manager();
                    let start = std::time::Instant::now();
                    let (output, exit_code, _) = mgr
                        .wait_closed(&monitor_shell_id)
                        .await
                        .unwrap_or_else(|e| (e.to_string(), Some(-1), "error".to_owned()));

                    let artifacts =
                        collect_artifacts(&monitor_workspace, pre_head.as_deref()).await;
                    let duration_ms = start.elapsed().as_millis() as u64;

                    // Cleanup worktree if applicable.
                    let mut worktree_note = String::new();
                    if monitor_isolation.as_deref() == Some("worktree")
                        && monitor_workspace != monitor_orig_workspace
                    {
                        use crate::builtin::subagent::worktree::{
                            WorktreeOutcome, cleanup_worktree,
                        };
                        match cleanup_worktree(Path::new(&monitor_workspace)).await {
                            WorktreeOutcome::NoChanges => {
                                worktree_note = "\n\nworktree: removed (no changes)".to_owned();
                            }
                            WorktreeOutcome::HasChanges { path, branch } => {
                                worktree_note = format!(
                                    "\n\nworktree: changes at {} (branch: {branch})",
                                    path.display()
                                );
                            }
                            WorktreeOutcome::NotApplicable(_) => {}
                        }
                    }

                    // Record in tracker.
                    let tracker = crate::builtin::subagent::tracker::agent_tracker();
                    tracker
                        .complete(
                            &monitor_agent_id,
                            crate::builtin::subagent::tracker::AgentResult {
                                exit_code,
                                output: output.clone(),
                                artifacts: artifacts.clone(),
                                duration_ms,
                            },
                        )
                        .await;

                    let message = format!(
                        "{}{}",
                        build_completion_message(
                            &monitor_agent_id,
                            &monitor_cli_name,
                            exit_code,
                            &output,
                            &artifacts,
                        ),
                        worktree_note
                    );

                    if let Some(inject) = inject_fn {
                        let mut ctx = inbound_context;
                        ctx.insert("source".to_owned(), serde_json::json!("subagent"));
                        ctx.insert("agent_id".to_owned(), serde_json::json!(monitor_agent_id));
                        ctx.insert("exit_code".to_owned(), serde_json::json!(exit_code));
                        // Additive inter-agent correlation fields (optional; no
                        // consumer requires them yet). task_id = the subagent's
                        // own id (its work-unit key); intent marks this envelope
                        // as a delegated-work result for future routing.
                        ctx.insert("task_id".to_owned(), serde_json::json!(monitor_agent_id));
                        ctx.insert("intent".to_owned(), serde_json::json!("result"));

                        inject(serde_json::json!({
                            "session_id": session_id,
                            "channel": "subagent",
                            "chat_id": chat_id,
                            "content": message,
                            "output_channel": output_channel,
                            "context": ctx
                        }))
                        .await;
                    } else {
                        tracing::warn!(
                            agent_id = %monitor_agent_id,
                            "agent completed but no inbound injector set"
                        );
                    }
                });

                Ok(serde_json::json!({
                    "status": "background_launched",
                    "agent_id": agent_id,
                    "engine": cli_name,
                    "description": description,
                }))
            })
        },
    )
}

/// Truncate output to tail portion for inclusion in results.
fn truncate_output(output: &str) -> String {
    if output.trim().is_empty() {
        "(agent produced no output)".to_owned()
    } else if output.len() > SUBAGENT_OUTPUT_TAIL {
        let tail_start = output.len() - SUBAGENT_OUTPUT_TAIL;
        let boundary = output.ceil_char_boundary(tail_start);
        format!("...(truncated)\n{}", &output[boundary..])
    } else {
        output.to_owned()
    }
}

/// Append worktree info to a result JSON object.
fn append_worktree_info(
    result: &mut Value,
    outcome: Option<crate::builtin::subagent::worktree::WorktreeOutcome>,
) {
    use crate::builtin::subagent::worktree::WorktreeOutcome;
    if let Some(outcome) = outcome {
        match outcome {
            WorktreeOutcome::NoChanges => {
                result["worktree"] = serde_json::json!("removed (no changes)");
            }
            WorktreeOutcome::HasChanges { path, branch } => {
                result["worktree"] = serde_json::json!({
                    "path": path.to_string_lossy(),
                    "branch": branch,
                    "status": "changes preserved"
                });
            }
            WorktreeOutcome::NotApplicable(reason) => {
                result["worktree"] = serde_json::json!(reason);
            }
        }
    }
}

// -- Agent management tools --------------------------------------------------

fn tool_agent_status() -> Tool {
    Tool::new(
        "agent.status",
        "List all background agents and their current status.",
        serde_json::json!({"type": "object", "properties": {}}),
        |_args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let tracker = crate::builtin::subagent::tracker::agent_tracker();
                let agents = tracker.list().await;
                if agents.is_empty() {
                    return ok_val("No background agents.");
                }
                let lines: Vec<String> = agents
                    .iter()
                    .map(|(id, s)| {
                        let status = if s.running {
                            format!("running ({:.1}s)", s.elapsed_ms as f64 / 1000.0)
                        } else {
                            format!(
                                "done (exit {})",
                                s.exit_code.map(|c| c.to_string()).unwrap_or("?".into())
                            )
                        };
                        format!(
                            "{id}  {status}  [{}/{}]  {}",
                            s.cli, s.agent_type, s.prompt_summary
                        )
                    })
                    .collect();
                ok_val(lines.join("\n"))
            })
        },
    )
}

fn tool_agent_kill() -> Tool {
    Tool::new(
        "agent.kill",
        "Kill a running background agent by its agent ID.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string"}
            },
            "required": ["agent_id"]
        }),
        |args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let agent_id = args
                    .require_str_field("agent_id")
                    .map_err(invalid_input)?
                    .to_owned();
                let tracker = crate::builtin::subagent::tracker::agent_tracker();
                match tracker.kill(&agent_id).await {
                    Some(result) => Ok(serde_json::json!({
                        "status": "killed",
                        "agent_id": agent_id,
                        "exit_code": result.exit_code,
                        "output": truncate_output(&result.output),
                    })),
                    None => Err(ConduitError::new(
                        ErrorKind::NotFound,
                        format!("agent '{agent_id}' not found or already completed"),
                    )),
                }
            })
        },
    )
}

fn tool_agent_result() -> Tool {
    Tool::new(
        "agent.result",
        "Retrieve the result of a completed background agent.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string"}
            },
            "required": ["agent_id"]
        }),
        |args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let agent_id = args
                    .require_str_field("agent_id")
                    .map_err(invalid_input)?
                    .to_owned();
                let tracker = crate::builtin::subagent::tracker::agent_tracker();
                match tracker.get_result(&agent_id).await {
                    Some(result) => Ok(serde_json::json!({
                        "agent_id": agent_id,
                        "exit_code": result.exit_code,
                        "content": truncate_output(&result.output),
                        "changes": result.artifacts,
                        "duration_ms": result.duration_ms,
                    })),
                    None => Err(ConduitError::new(
                        ErrorKind::NotFound,
                        format!("agent '{agent_id}' not found or still running"),
                    )),
                }
            })
        },
    )
}

// ---------------------------------------------------------------------------
// help
// ---------------------------------------------------------------------------

fn tool_help() -> Tool {
    Tool::new(
        "help",
        "Show available commands and their syntax.",
        serde_json::json!({
            "type": "object",
            "properties": {}
        }),
        |_args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                ok_val(
                    "Commands use '/' at line start.\n\
                     Known internal commands:\n\
                     \x20 /help\n\
                     \x20 /skill name=foo\n\
                     \x20 /tape.info\n\
                     \x20 /tape.search query=error\n\
                     \x20 /tape.handoff name=phase-1 summary='done'\n\
                     \x20 /tape.anchors\n\
                     \x20 /fs.read path=README.md\n\
                     \x20 /fs.write path=tmp.txt content='hello'\n\
                     \x20 /fs.edit path=tmp.txt old=hello new=world\n\
                     \x20 /bash cmd='sleep 5' background=true\n\
                     \x20 /bash.output shell_id=bsh-12345678\n\
                     \x20 /bash.kill shell_id=bsh-12345678\n\
                     \x20 /quit\n\
                     Any unknown command after '/' is executed as shell via bash.",
                )
            })
        },
    )
}

// ---------------------------------------------------------------------------
// quit
// ---------------------------------------------------------------------------

fn tool_message_send() -> Tool {
    Tool::with_context(
        "message.send",
        "Send a message to the user immediately, without waiting for the turn to finish.\n\nUse this to acknowledge the user's request before starting long-running work, or to provide progress updates mid-task. The message is dispatched to the same channel the user sent from.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "media_path": {"type": "string", "description": "Local file path."},
                "media_paths": {"type": "array", "items": {"type": "string"}, "description": "Multiple local file paths."},
                "image_path": {"type": "string", "description": "Deprecated; use media_path."}
            },
            "required": ["text"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let text = args
                    .require_str_field("text")
                    .map_err(invalid_input)?
                    .to_owned();
                let image_path = args
                    .get("image_path")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_owned());
                if text.trim().is_empty() && image_path.is_none() {
                    return ok_val("skipped: empty message");
                }

                let ctx = ctx.ok_or_else(|| {
                    ConduitError::new(ErrorKind::InvalidInput, "no tool context available")
                })?;
                let state = &ctx.state;

                let mut envelope = serde_json::json!({
                    "content": text,
                    "session_id": state.get("session_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "channel": state.get("channel").and_then(|v| v.as_str()).unwrap_or(""),
                    "chat_id": state.get("chat_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "output_channel": state.get("output_channel").and_then(|v| v.as_str()).unwrap_or(""),
                    // Marker so channel plugins (e.g. Feishu) know this is a
                    // mid-turn progress message, not the final turn reply.
                    // Prevents the channel from consuming per-turn state
                    // (inflight batch, quote-reply target, reactions).
                    "context": { "_eli_mid_turn": true },
                });
                if let Some(path) = image_path {
                    let path_obj = std::path::Path::new(&path);
                    if !path_obj.exists() {
                        return Err(ConduitError::new(
                            ErrorKind::InvalidInput,
                            format!("image_path not found: {path}"),
                        ));
                    }
                    let mime = crate::control_plane::mime_from_extension(path_obj);
                    let media_type = crate::control_plane::media_type_from_mime(mime);
                    envelope["outbound_media"] = serde_json::json!([
                        {
                            "path": path,
                            "mime_type": mime,
                            "media_type": media_type,
                        }
                    ]);
                }

                crate::control_plane::dispatch_mid_turn(envelope).await;
                ok_val("sent")
            })
        },
    )
}

fn tool_quit() -> Tool {
    Tool::with_context(
        "quit",
        "End the session and stop all running tasks.",
        serde_json::json!({
            "type": "object",
            "properties": {}
        }),
        |_args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move { ok_val("Session tasks stopped.") })
        },
    )
}

// ---------------------------------------------------------------------------
// Task tools — persistent, queryable work units
// ---------------------------------------------------------------------------

/// Max tasks per session per minute.
const TASK_RATE_LIMIT: usize = 20;
/// Max total active (non-terminal) tasks.
const TASK_MAX_ACTIVE: usize = 100;
/// Max parent→child nesting depth.
const TASK_MAX_DEPTH: u32 = 5;

fn require_task_store() -> Result<&'static crate::taskboard::store::TaskStore, ConduitError> {
    crate::taskboard::task_store().ok_or_else(|| {
        ConduitError::new(
            ErrorKind::Tool,
            "taskboard not initialized — run `eli gateway` or `eli chat` first",
        )
    })
}

fn tool_task_create() -> Tool {
    Tool::new(
        "task.create",
        "Create a persistent task on the task board. Tasks are tracked, queryable, and can be consumed by background workers.\n\nUse for work that needs persistence and status tracking. For quick one-off background jobs, use `agent` instead.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "Task kind (e.g. 'explore', 'implement', 'review', 'test', 'research')."
                },
                "prompt": {
                    "type": "string",
                    "description": "Task description / prompt."
                },
                "priority": {
                    "type": "integer",
                    "description": "Priority: 0=low, 1=normal (default), 2=high, 3=urgent."
                },
                "parent": {
                    "type": "string",
                    "description": "Parent task ID for sub-task decomposition."
                }
            },
            "required": ["kind", "prompt"]
        }),
        |args: Value, ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let store = require_task_store()?;
                let kind = args
                    .require_str_field("kind")
                    .map_err(invalid_input)?
                    .to_owned();
                let prompt = args
                    .require_str_field("prompt")
                    .map_err(invalid_input)?
                    .to_owned();
                let priority = args.get("priority").and_then(|v| v.as_u64()).unwrap_or(1) as u8;
                let parent = args
                    .get("parent")
                    .and_then(|v| v.as_str())
                    .and_then(|s| uuid::Uuid::parse_str(s).ok());

                let session = ctx
                    .as_ref()
                    .and_then(|c| c.tape.clone())
                    .unwrap_or_else(|| "unknown".into());

                // Rate limit: max tasks per session per minute
                let recent = store.count_recent(&session, 60).await;
                if recent >= TASK_RATE_LIMIT {
                    return Err(ConduitError::new(
                        ErrorKind::InvalidInput,
                        format!("rate limit: max {TASK_RATE_LIMIT} tasks per minute per session"),
                    ));
                }

                // Active task limit
                let active = store.active_count().await;
                if active >= TASK_MAX_ACTIVE {
                    return Err(ConduitError::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "max {TASK_MAX_ACTIVE} active tasks — complete or cancel existing tasks first"
                        ),
                    ));
                }

                // Depth limit
                if let Some(parent_id) = parent {
                    let depth = store.task_depth(parent_id).await;
                    if depth >= TASK_MAX_DEPTH {
                        return Err(ConduitError::new(
                            ErrorKind::InvalidInput,
                            format!(
                                "max nesting depth is {TASK_MAX_DEPTH} — cannot create deeper sub-tasks"
                            ),
                        ));
                    }
                }

                let prompt_preview = if prompt.len() > 80 {
                    format!("{}...", &prompt[..prompt.floor_char_boundary(80)])
                } else {
                    prompt.clone()
                };
                let kind_label = kind.clone();

                let new_task = crate::taskboard::NewTask {
                    kind,
                    session_origin: session,
                    context: serde_json::json!({"prompt": prompt}),
                    parent,
                    priority,
                    metadata: serde_json::Value::Null,
                };

                let id = store
                    .create(new_task)
                    .await
                    .map_err(|e| ConduitError::new(ErrorKind::Tool, e.to_string()))?;

                ok_val(format!(
                    "created {id} [{kind_label}] p{priority} ({} active)\n{prompt_preview}",
                    active + 1
                ))
            })
        },
    )
}

fn tool_task_status() -> Tool {
    Tool::new(
        "task.status",
        "Get the status and details of a task by ID.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID."}
            },
            "required": ["task_id"]
        }),
        |args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let store = require_task_store()?;
                let id_str = args.require_str_field("task_id").map_err(invalid_input)?;
                let id = uuid::Uuid::parse_str(id_str).map_err(|e| {
                    ConduitError::new(ErrorKind::InvalidInput, format!("invalid task ID: {e}"))
                })?;

                match store.get(id).await {
                    Some(task) => {
                        let prompt = task
                            .context
                            .get("prompt")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let mut lines = vec![
                            format!(
                                "{} {} [{}] p{}",
                                task.id,
                                task.status.label(),
                                task.kind,
                                task.priority
                            ),
                            format!(
                                "created {} updated {}",
                                task.created_at.format("%m-%d %H:%M"),
                                task.updated_at.format("%m-%d %H:%M")
                            ),
                        ];
                        if let Some(ref agent) = task.assigned_to {
                            lines.push(format!("assigned: {agent}"));
                        }
                        if let Some(p) = task.parent {
                            lines.push(format!("parent: {p}"));
                        }
                        lines.push(format!("prompt: {prompt}"));
                        if let Some(ref r) = task.result {
                            let r_str = serde_json::to_string(r).unwrap_or_default();
                            lines.push(format!("result: {r_str}"));
                        }
                        if let crate::taskboard::Status::Failed {
                            ref error,
                            retries,
                            ref suggested_fix,
                            ..
                        } = task.status
                        {
                            lines.push(format!("error: {error} (retries: {retries})"));
                            if let Some(fix) = suggested_fix {
                                lines.push(format!("fix: {fix}"));
                            }
                        }
                        ok_val(lines.join("\n"))
                    }
                    None => Err(ConduitError::new(
                        ErrorKind::NotFound,
                        format!("task '{id_str}' not found"),
                    )),
                }
            })
        },
    )
}

fn tool_task_list() -> Tool {
    Tool::new(
        "task.list",
        "List tasks on the board, optionally filtered by status or kind.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Filter by status: todo, claimed, running, done, failed, blocked, cancelled."
                },
                "kind": {
                    "type": "string",
                    "description": "Filter by task kind."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 20)."
                }
            }
        }),
        |args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                tracing::debug!(args = %args, "task.list called");
                let store = require_task_store()?;
                let filter = crate::taskboard::TaskFilter {
                    status: args
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    kind: args.get("kind").and_then(|v| v.as_str()).map(String::from),
                    limit: Some(args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize),
                    ..Default::default()
                };

                let tasks = store.list(filter).await;
                if tasks.is_empty() {
                    return ok_val("0 tasks.");
                }
                let mut lines = vec![format!("{} task(s):", tasks.len())];
                for t in &tasks {
                    let prompt = t
                        .context
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let prompt_short = if prompt.len() > 50 {
                        format!("{}...", &prompt[..prompt.floor_char_boundary(50)])
                    } else {
                        prompt.to_string()
                    };
                    let mut parts = format!(
                        "{} {} [{}] p{}",
                        &t.id.to_string()[..8],
                        t.status.label(),
                        t.kind,
                        t.priority,
                    );
                    if let Some(ref a) = t.assigned_to {
                        parts.push_str(&format!(" @{a}"));
                    }
                    parts.push_str(&format!(" {prompt_short}"));
                    lines.push(parts);
                }
                ok_val(lines.join("\n"))
            })
        },
    )
}

fn tool_task_cancel() -> Tool {
    Tool::new(
        "task.cancel",
        "Cancel a task by ID. Only non-terminal tasks can be cancelled.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID."},
                "reason": {"type": "string", "description": "Cancellation reason."}
            },
            "required": ["task_id"]
        }),
        |args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let store = require_task_store()?;
                let id_str = args.require_str_field("task_id").map_err(invalid_input)?;
                let id = uuid::Uuid::parse_str(id_str).map_err(|e| {
                    ConduitError::new(ErrorKind::InvalidInput, format!("invalid task ID: {e}"))
                })?;
                let reason = args
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("cancelled by user")
                    .to_string();

                store
                    .cancel(id, reason.clone())
                    .await
                    .map_err(|e| ConduitError::new(ErrorKind::Tool, e.to_string()))?;

                ok_val(format!("cancelled {id_str}: {reason}"))
            })
        },
    )
}

fn tool_task_update() -> Tool {
    Tool::new(
        "task.update",
        "Update a task's progress or mark it complete/failed.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID."},
                "progress": {"type": "number", "description": "Progress 0.0 to 1.0."},
                "result": {"type": "string", "description": "Set result and mark task as done."},
                "error": {"type": "string", "description": "Set error and mark task as failed."}
            },
            "required": ["task_id"]
        }),
        |args: Value, _ctx: Option<ToolContext>| -> BoxFuture<'static, ToolResult> {
            Box::pin(async move {
                let store = require_task_store()?;
                let id_str = args.require_str_field("task_id").map_err(invalid_input)?;
                let id = uuid::Uuid::parse_str(id_str).map_err(|e| {
                    ConduitError::new(ErrorKind::InvalidInput, format!("invalid task ID: {e}"))
                })?;

                if let Some(result) = args.get("result").and_then(|v| v.as_str()) {
                    store
                        .complete(id, serde_json::json!({"output": result}))
                        .await
                        .map_err(|e| ConduitError::new(ErrorKind::Tool, e.to_string()))?;
                    return ok_val(format!("{id_str} done"));
                }

                if let Some(error) = args.get("error").and_then(|v| v.as_str()) {
                    store
                        .fail(id, error.to_string())
                        .await
                        .map_err(|e| ConduitError::new(ErrorKind::Tool, e.to_string()))?;
                    return ok_val(format!("{id_str} failed: {error}"));
                }

                if let Some(progress) = args.get("progress").and_then(|v| v.as_f64()) {
                    store
                        .update_status(
                            id,
                            crate::taskboard::Status::Running {
                                progress: progress as f32,
                                last_heartbeat: chrono::Utc::now(),
                            },
                        )
                        .await
                        .map_err(|e| ConduitError::new(ErrorKind::Tool, e.to_string()))?;
                    return ok_val(format!("{id_str} {:.0}%", progress * 100.0));
                }

                Err(ConduitError::new(
                    ErrorKind::InvalidInput,
                    "provide one of: progress, result, or error",
                ))
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::store::{FileTapeStore, ForkTapeStore};
    use serde_json::json;
    use std::io::BufWriter;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const LARGE_FILE_BYTES: u64 = 50 * 1024 * 1024;

    // -- tool-output spill (view-layer trim, full content recoverable) --------

    #[test]
    fn small_output_passes_through_unspilled() {
        let out = "short output";
        assert_eq!(maybe_spill_output(out, "bash"), out);
    }

    #[test]
    fn large_output_spills_to_preview_with_steering() {
        let big = "x".repeat(TOOL_OUTPUT_LARGE_THRESHOLD + 1);
        let view = maybe_spill_output(&big, "web");
        // The view is trimmed (not the full output) but steers to the full copy.
        assert!(view.chars().count() < big.chars().count());
        assert!(view.contains("fs.read"));
        assert!(view.contains(&format!("{} chars", big.chars().count())));
    }

    #[test]
    fn read_only_tools_are_tagged_mutating_are_not() {
        let tools = builtin_tools();
        let by_name = |n: &str| tools.iter().find(|t| t.name == n);
        // Read-only tools carry the hint; mutating ones do not.
        assert!(by_name("fs.read").is_some_and(|t| t.read_only));
        assert!(by_name("tape.search").is_some_and(|t| t.read_only));
        assert!(by_name("web.fetch").is_some_and(|t| t.read_only));
        assert!(by_name("fs.write").is_some_and(|t| !t.read_only));
        assert!(by_name("fs.edit").is_some_and(|t| !t.read_only));
        assert!(by_name("bash").is_some_and(|t| !t.read_only));
    }

    #[test]
    fn spill_preview_fallback_reports_total_and_truncation() {
        let big = "y".repeat(TOOL_OUTPUT_PREVIEW_CHARS + 500);
        let fallback = spill_preview_fallback(&big);
        assert!(fallback.contains("truncated"));
        assert!(fallback.contains(&format!("{} chars total", big.chars().count())));
        assert!(fallback.chars().count() < big.chars().count());
    }

    fn test_tape_service() -> (tempfile::TempDir, TapeService, String) {
        let tmp = tempfile::tempdir().unwrap();
        let tapes_dir = tmp.path().join("tapes");
        let store = ForkTapeStore::from_sync(FileTapeStore::new(tapes_dir.clone()));
        let service = TapeService::new(tapes_dir, store);
        let tape_name = "workspace__session".to_owned();
        (tmp, service, tape_name)
    }

    #[tokio::test]
    async fn test_tape_info_tool_uses_runtime_service() {
        let (_tmp, service, tape_name) = test_tape_service();
        service.ensure_bootstrap_anchor(&tape_name).await.unwrap();
        service
            .append_event(&tape_name, "run", json!({"usage": {"total_tokens": 42}}))
            .await
            .unwrap();

        let tool = tool_tape_info();
        let ctx = ToolContext::new("test-run").with_tape(tape_name.clone());
        let value = with_tape_runtime(service.clone(), async move {
            tool.run(json!({}), Some(ctx)).await.unwrap()
        })
        .await;

        let output = value.as_str().unwrap();
        assert!(output.contains("name: workspace__session"));
        assert!(output.contains("anchors: 1"));
    }

    #[tokio::test]
    async fn test_tape_search_tool_filters_entries() {
        let (_tmp, service, tape_name) = test_tape_service();
        service.ensure_bootstrap_anchor(&tape_name).await.unwrap();
        service
            .store()
            .append(
                &tape_name,
                &TapeEntry::message(
                    json!({"role": "user", "content": "hello needle"}),
                    json!({}),
                ),
            )
            .await
            .unwrap();
        service
            .store()
            .append(
                &tape_name,
                &TapeEntry::message(json!({"role": "user", "content": "different"}), json!({})),
            )
            .await
            .unwrap();

        let tool = tool_tape_search();
        let ctx = ToolContext::new("test-run").with_tape(tape_name.clone());
        let value = with_tape_runtime(service.clone(), async move {
            tool.run(json!({"query": "needle"}), Some(ctx))
                .await
                .unwrap()
        })
        .await;

        let output = value.as_str().unwrap();
        assert!(output.contains("needle"));
        assert!(!output.contains("different"));
    }

    #[tokio::test]
    async fn test_fs_edit_preserves_crlf_and_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("note.txt");
        std::fs::write(&path, "first\r\nsecond\r\nthird\r\n").unwrap();
        tool_fs_edit()
            .run(
                json!({"path": path.to_string_lossy(), "old": "second", "new": "2nd"}),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first\r\n2nd\r\nthird\r\n");
    }

    #[tokio::test]
    async fn test_fs_read_preserves_original_newlines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("note.txt");
        std::fs::write(&path, "first\r\nsecond\r\nthird").unwrap();
        let value = tool_fs_read()
            .run(
                json!({"path": path.to_string_lossy(), "offset": 1, "limit": 1}),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        assert_eq!(value.as_str().unwrap(), "     2\tsecond\r\n");
    }

    #[tokio::test]
    async fn test_fs_edit_streams_large_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("huge.txt");
        let line = "a".repeat(8 * 1024);
        let mut writer = BufWriter::new(std::fs::File::create(&path).unwrap());
        for _ in 0..6_300 {
            writeln!(writer, "{line}").unwrap();
        }
        writeln!(writer, "prefix NEEDLE suffix").unwrap();
        for _ in 0..200 {
            writeln!(writer, "{line}").unwrap();
        }
        writer.flush().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > LARGE_FILE_BYTES);
        tool_fs_edit()
            .run(
                json!({"path": path.to_string_lossy(), "old": "NEEDLE", "new": "updated"}),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("prefix updated suffix"));
        assert!(!text.contains("prefix NEEDLE suffix"));
    }

    #[tokio::test]
    async fn test_fs_edit_start_skips_earlier_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("note.txt");
        std::fs::write(&path, "target\nkeep\ntarget\n").unwrap();
        tool_fs_edit()
            .run(
                json!({"path": path.to_string_lossy(), "old": "target", "new": "done", "start": 2}),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "target\nkeep\ndone\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_fs_write_preserves_existing_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("script.sh");
        std::fs::write(&path, "echo hi\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        tool_fs_write()
            .run(
                json!({"path": path.to_string_lossy(), "content": "echo bye\n"}),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn test_bash_exposes_description_field() {
        let tool = tool_bash();
        assert_eq!(
            tool.parameters["properties"]["description"]["type"],
            json!("string"),
            "bash should expose a description field for command purpose"
        );
    }

    #[test]
    fn test_non_bash_tools_omit_description_field() {
        let tools = [
            tool_fs_read(),
            tool_fs_write(),
            tool_fs_edit(),
            tool_evolution_capture(),
            tool_evolution_distill(),
            tool_evolution_history(),
            tool_evolution_auto_run(),
            tool_evolution_list(),
            tool_evolution_show(),
            tool_evolution_evaluate(),
            tool_evolution_promote(),
            tool_evolution_reject(),
            tool_evolution_rollback(),
            tool_tape_info(),
            tool_tape_search(),
            tool_tape_reset(),
            tool_tape_handoff(),
            tool_tape_anchors(),
            tool_web_fetch(),
        ];

        for tool in tools {
            assert!(
                tool.parameters["properties"].get("description").is_none(),
                "tool {} should not expose a description field (auto-generated notices instead)",
                tool.name
            );
        }

        // agent tool intentionally exposes description as a task summary param.
        let agent = tool_agent();
        assert!(agent.parameters["properties"].get("description").is_some());
    }

    #[test]
    fn test_auto_notice_generates_semantic_descriptions() {
        assert_eq!(
            auto_notice("fs.read", &json!({"path": "src/main.rs"})),
            "读 src/main.rs"
        );
        assert_eq!(
            auto_notice("fs.write", &json!({"path": "out.txt"})),
            "写 out.txt"
        );
        assert_eq!(
            auto_notice("fs.edit", &json!({"path": "lib.rs"})),
            "编辑 lib.rs"
        );
        assert_eq!(
            auto_notice("evolution.capture", &json!({"title": "Keep updates terse"})),
            "记录演进候选: Keep updates terse"
        );
        assert_eq!(
            auto_notice("evolution.distill", &json!({"tape": "abc123"})),
            "预演蒸馏演进候选 abc123"
        );
        assert_eq!(
            auto_notice(
                "evolution.distill",
                &json!({"tape": "abc123", "persist": true})
            ),
            "蒸馏演进候选 abc123"
        );
        assert_eq!(
            auto_notice("evolution.history", &json!({"limit": 3})),
            "查看演进历史 3"
        );
        assert_eq!(
            auto_notice("evolution.auto_run", &json!({"tape": "abc123"})),
            "自动运行演进 abc123"
        );
        assert_eq!(auto_notice("evolution.list", &json!({})), "列出演进候选");
        assert_eq!(
            auto_notice("evolution.show", &json!({"id": "cand123"})),
            "查看演进候选 cand123"
        );
        assert_eq!(
            auto_notice("evolution.evaluate", &json!({"id": "cand123"})),
            "评估演进候选 cand123"
        );
        assert_eq!(
            auto_notice("evolution.promote", &json!({"id": "cand123"})),
            "提升演进候选 cand123"
        );
        assert_eq!(
            auto_notice("evolution.reject", &json!({"id": "cand123"})),
            "拒绝演进候选 cand123"
        );
        assert_eq!(
            auto_notice("evolution.rollback", &json!({"id": "cand123"})),
            "回滚演进候选 cand123"
        );
        assert_eq!(
            auto_notice("web.fetch", &json!({"url": "https://example.com"})),
            "获取 https://example.com"
        );
        assert_eq!(
            auto_notice(
                "bash",
                &json!({"cmd": "cargo build", "description": "编译项目"})
            ),
            "编译项目"
        );
        assert_eq!(
            auto_notice("bash", &json!({"cmd": "cargo build"})),
            "执行 cargo build"
        );
        assert_eq!(
            auto_notice("tape.search", &json!({"query": "error"})),
            "搜索 tape: error"
        );
        assert_eq!(auto_notice("tape.info", &json!({})), "查看 tape 信息");
        assert_eq!(auto_notice("tape.reset", &json!({})), "重置 tape");
        assert_eq!(auto_notice("tape.anchors", &json!({})), "列出 anchors");
        assert_eq!(
            auto_notice("tape.handoff", &json!({"name": "phase-1"})),
            "handoff: phase-1"
        );
        assert_eq!(auto_notice("tape.handoff", &json!({})), "创建 handoff");
        // Unknown tools fall back to tool name
        assert_eq!(auto_notice("unknown.tool", &json!({})), "unknown.tool");
    }

    // -----------------------------------------------------------------------
    // Subagent helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_cli_command_claude() {
        let cli = CliInfo {
            name: "claude".to_owned(),
            path: "/usr/local/bin/claude".to_owned(),
        };
        let cmd = build_cli_command(&cli, "/tmp/prompt.txt");
        assert_eq!(
            cmd,
            "/usr/local/bin/claude -p --output-format text < /tmp/prompt.txt"
        );
    }

    #[test]
    fn test_build_cli_command_codex() {
        let cli = CliInfo {
            name: "codex".to_owned(),
            path: "/usr/bin/codex".to_owned(),
        };
        let cmd = build_cli_command(&cli, "/tmp/prompt.txt");
        assert_eq!(cmd, "/usr/bin/codex exec < /tmp/prompt.txt");
    }

    #[test]
    fn test_build_cli_command_kimi() {
        let cli = CliInfo {
            name: "kimi".to_owned(),
            path: "/opt/bin/kimi".to_owned(),
        };
        let cmd = build_cli_command(&cli, "/tmp/prompt.txt");
        assert!(cmd.contains("-p"));
        assert!(cmd.contains("--print"));
        assert!(cmd.contains("$(cat /tmp/prompt.txt)"));
    }

    #[test]
    fn test_build_cli_command_path_with_spaces() {
        let cli = CliInfo {
            name: "claude".to_owned(),
            path: "/my path/claude".to_owned(),
        };
        let cmd = build_cli_command(&cli, "/tmp/prompt.txt");
        assert!(cmd.starts_with("'/my path/claude'"));
    }

    #[test]
    fn test_shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("/usr/bin/foo"), "/usr/bin/foo");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_shell_quote_special_chars() {
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_build_completion_message_success() {
        let msg =
            build_completion_message("agent-abc", "claude", Some(0), "all good", "(no changes)");
        assert!(msg.contains("agent-abc"));
        assert!(msg.contains("claude"));
        assert!(msg.contains("success (exit 0)"));
        assert!(msg.contains("all good"));
        assert!(msg.contains("(no changes)"));
    }

    #[test]
    fn test_build_completion_message_failure() {
        let msg = build_completion_message("agent-def", "codex", Some(1), "error!", "M foo.rs");
        assert!(msg.contains("failed (exit 1)"));
        assert!(msg.contains("error!"));
        assert!(msg.contains("M foo.rs"));
    }

    #[test]
    fn test_build_completion_message_truncates_long_output() {
        let long_output = "x".repeat(5000);
        let msg = build_completion_message(
            "agent-trunc",
            "claude",
            Some(0),
            &long_output,
            "(no changes)",
        );
        assert!(msg.contains("(truncated)"));
        // A truncated stdout is labeled a preview, not presented as the full record.
        assert!(
            msg.contains("preview"),
            "long stdout must be labeled a preview"
        );
        // At most SUBAGENT_OUTPUT_TAIL of the original output chars are retained
        // (+overhead for incidental 'x' in labels like "exit").
        assert!(msg.matches('x').count() <= SUBAGENT_OUTPUT_TAIL + 20);
    }

    #[test]
    fn test_build_completion_message_leads_with_git_changes() {
        // When the subagent made real file changes, the git record (authoritative,
        // lossless) must precede the truncated stdout preview — not the reverse.
        let long_output = "y".repeat(5000);
        let artifacts = "commits:\nabc123 fix bug\n\n M src/foo.rs";
        let msg = build_completion_message("agent-w", "claude", Some(0), &long_output, artifacts);
        let changes_pos = msg.find("src/foo.rs").expect("git changes present");
        let preview_pos = msg.find("preview").expect("stdout labeled preview");
        assert!(
            changes_pos < preview_pos,
            "git changes must precede the stdout preview"
        );
        assert!(msg.matches('y').count() <= SUBAGENT_OUTPUT_TAIL);
    }

    #[test]
    fn test_build_completion_message_empty_output() {
        let msg = build_completion_message("agent-empty", "claude", Some(0), "", "(clean)");
        assert!(msg.contains("(sub-agent produced no output)"));
    }

    #[test]
    fn test_write_prompt_tempfile() {
        let f = write_prompt_tempfile("hello world").unwrap();
        let content = std::fs::read_to_string(f.path()).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_collect_artifacts_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = collect_artifacts(tmp.path().to_str().unwrap(), None).await;
        assert_eq!(result, "(not a git repo)");
    }

    #[tokio::test]
    async fn test_collect_artifacts_clean_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap();
        // Initialize a git repo with one commit.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
        let head = snapshot_git_head(dir);
        let result = collect_artifacts(dir, head.as_deref()).await;
        assert_eq!(result, "(no changes)");
    }

    // -----------------------------------------------------------------------
    // Tool polish: new coverage
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fs_read_line_numbers_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lines.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let value = tool_fs_read()
            .run(
                json!({"path": path.to_string_lossy()}),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        let text = value.as_str().unwrap();
        assert!(text.starts_with("     1\talpha\n"));
        assert!(text.contains("     2\tbeta\n"));
        assert!(text.contains("     3\tgamma\n"));
    }

    #[tokio::test]
    async fn test_fs_read_default_limit_truncates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.txt");
        let content: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &content).unwrap();
        let value = tool_fs_read()
            .run(
                json!({"path": path.to_string_lossy()}),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        let text = value.as_str().unwrap();
        assert!(text.contains("truncated at 500 lines"));
        assert!(text.contains("offset=500"));
        assert!(text.contains("   500\t"));
        assert!(!text.contains("   501\t"));
    }

    #[tokio::test]
    async fn test_fs_read_explicit_limit_no_truncation_note() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.txt");
        let content: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &content).unwrap();
        let value = tool_fs_read()
            .run(
                json!({"path": path.to_string_lossy(), "limit": 10}),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        let text = value.as_str().unwrap();
        assert!(!text.contains("truncated"));
    }

    #[tokio::test]
    async fn test_fs_edit_invalid_old_shows_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("note.txt");
        std::fs::write(&path, "hello world").unwrap();
        let result = tool_fs_edit()
            .run(
                json!({"path": path.to_string_lossy(), "old": "not here", "new": "x"}),
                Some(ToolContext::new("test-run")),
            )
            .await;
        let err = result.unwrap_err();
        assert!(
            err.message.contains("fs.read"),
            "error should suggest fs.read: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_fs_edit_syntax_check_warns_on_bad_python() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.py");
        std::fs::write(&path, "def foo():\n    pass\n").unwrap();
        let value = tool_fs_edit()
            .run(
                json!({
                    "path": path.to_string_lossy().as_ref(),
                    "old": "def foo():\n    pass",
                    "new": "def foo(\n    pass"
                }),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        let text = value.as_str().unwrap();
        assert!(
            text.contains("syntax check failed"),
            "should warn about syntax: {text}"
        );
    }

    #[tokio::test]
    async fn test_fs_edit_syntax_check_silent_on_valid_python() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("good.py");
        std::fs::write(&path, "x = 1\n").unwrap();
        let value = tool_fs_edit()
            .run(
                json!({"path": path.to_string_lossy().as_ref(), "old": "x = 1", "new": "x = 2"}),
                Some(ToolContext::new("test-run")),
            )
            .await
            .unwrap();
        let text = value.as_str().unwrap();
        assert!(
            !text.contains("syntax check"),
            "valid edit should not warn: {text}"
        );
    }

    #[test]
    fn test_invalid_edit_truncates_long_old_text() {
        let long_old = "a".repeat(200);
        let err = invalid_edit(Path::new("test.rs"), &long_old, 0);
        assert!(
            err.message.contains("..."),
            "long old text should be truncated: {}",
            err.message
        );
        assert!(
            err.message.contains("fs.read"),
            "should suggest fs.read: {}",
            err.message
        );
    }
}
