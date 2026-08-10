//! CLI channel — interactive REPL with colored terminal output.

use std::io::{self, BufRead, Write as IoWrite};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use crossterm::execute;
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::base::Channel;
use super::message::{ChannelMessage, MessageKind};

// ---------------------------------------------------------------------------
// CliRenderer
// ---------------------------------------------------------------------------

/// Rich-like terminal renderer using crossterm for colored output.
pub struct CliRenderer;

impl CliRenderer {
    pub fn new() -> Self {
        Self
    }

    pub fn welcome(&self, model: &str, workspace: &str) {
        let mut out = io::stdout();
        let border = "─".repeat(60);
        let _ = execute!(
            out,
            SetForegroundColor(Color::Cyan),
            Print(format!("┌{border}┐\n")),
            Print(format!("│{:^60}│\n", "Eli")),
            Print(format!("├{border}┤\n")),
            ResetColor,
            Print(format!("│ workspace: {:<47}│\n", workspace)),
            Print(format!("│ model: {:<51}│\n", model)),
            Print(format!("│ internal command prefix: '/'{:<31}│\n", "")),
            Print(format!(
                "│ shell command prefix: '/' at line start{:<18}│\n",
                ""
            )),
            Print(format!("│ type '/help' for command list{:<30}│\n", "")),
            SetForegroundColor(Color::Cyan),
            Print(format!("└{border}┘\n")),
            ResetColor,
        );
    }

    pub fn info(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let mut out = io::stdout();
        let _ = execute!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print(text),
            Print("\n"),
            ResetColor,
        );
    }

    pub fn command_output(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.panel(text, "Command", Color::Green);
    }

    pub fn assistant_output(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.panel(text, "Assistant", Color::Blue);
    }

    pub fn error(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.panel(text, "Error", Color::Red);
    }

    fn panel(&self, text: &str, title: &str, color: Color) {
        let mut out = io::stdout();
        let width = 60usize;
        Self::panel_header(&mut out, title, width, color);
        Self::panel_body(&mut out, text, width, color);
        Self::panel_footer(&mut out, width, color);
    }

    fn panel_header(out: &mut io::Stdout, title: &str, width: usize, color: Color) {
        let title_line = format!("─ {title} ");
        let padding = if title_line.len() < width {
            "─".repeat(width - title_line.len())
        } else {
            String::new()
        };
        let _ = execute!(out, SetForegroundColor(color), Print("┌"),);
        let _ = execute!(out, Print(&title_line), Print(&padding), Print("┐\n"),);
        let _ = execute!(out, ResetColor,);
    }

    fn panel_body(out: &mut io::Stdout, text: &str, width: usize, color: Color) {
        for line in text.lines() {
            let _ = execute!(
                out,
                SetForegroundColor(color),
                Print("│ "),
                ResetColor,
                Print(line),
            );
            if line.len() + 2 < width {
                let pad = " ".repeat(width - 2 - line.len());
                let _ = execute!(out, Print(&pad),);
            }
            let _ = execute!(out, SetForegroundColor(color), Print(" │\n"), ResetColor,);
        }
    }

    fn panel_footer(out: &mut io::Stdout, width: usize, color: Color) {
        let border = "─".repeat(width);
        let _ = execute!(
            out,
            SetForegroundColor(color),
            Print(format!("└{border}┘\n")),
            ResetColor,
        );
    }
}

impl Default for CliRenderer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CliChannel
// ---------------------------------------------------------------------------

/// An interactive CLI channel that runs a REPL, reads user input, and
/// forwards it to the framework as [`ChannelMessage`]s.
pub struct CliChannel {
    on_receive_tx: mpsc::UnboundedSender<ChannelMessage>,
    renderer: CliRenderer,
    mode: Mutex<CliMode>,
    model: String,
    workspace: PathBuf,
    repl_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    request_done: Arc<Notify>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliMode {
    Agent,
    Shell,
}

impl CliChannel {
    pub fn new(
        on_receive_tx: mpsc::UnboundedSender<ChannelMessage>,
        model: impl Into<String>,
        workspace: impl Into<PathBuf>,
    ) -> Self {
        Self {
            on_receive_tx,
            renderer: CliRenderer::new(),
            mode: Mutex::new(CliMode::Agent),
            model: model.into(),
            workspace: workspace.into(),
            repl_handle: Mutex::new(None),
            request_done: Arc::new(Notify::new()),
        }
    }

    pub fn notify_request_done(&self) {
        self.request_done.notify_one();
    }

    fn prompt_symbol(mode: CliMode) -> &'static str {
        match mode {
            CliMode::Agent => "> ",
            CliMode::Shell => "/ ",
        }
    }

    fn cwd_name() -> String {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".to_owned())
    }

    fn normalize_input(raw: &str, mode: CliMode) -> String {
        match mode {
            CliMode::Agent => raw.to_owned(),
            CliMode::Shell if raw.starts_with('/') => raw.to_owned(),
            CliMode::Shell => format!("/{raw}"),
        }
    }

    fn mode_name(mode: CliMode) -> &'static str {
        match mode {
            CliMode::Agent => "agent",
            CliMode::Shell => "shell",
        }
    }

    fn show_help() {
        let r = CliRenderer::new();
        r.info("Commands:");
        r.info("  /quit / /exit   — exit the REPL");
        r.info("  /help           — show this help");
        r.info("  Ctrl-X          — toggle agent/shell mode");
        r.info("");
    }

    fn read_line(reader: &mut io::StdinLock<'_>) -> Option<String> {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }

    fn send_and_wait(
        tx: &mpsc::UnboundedSender<ChannelMessage>,
        raw: &str,
        mode: CliMode,
        rt: &tokio::runtime::Handle,
        request_done: &Notify,
        cancel: &CancellationToken,
    ) -> bool {
        let request = Self::normalize_input(raw, mode);
        let message = ChannelMessage::new("cli_session", "cli", &request)
            .with_chat_id("cli_chat")
            .finalize();

        if tx.send(message).is_err() {
            return false;
        }

        rt.block_on(async {
            tokio::select! {
                () = request_done.notified() => {}
                () = cancel.cancelled() => {}
            }
        });
        true
    }
}

#[async_trait]
impl Channel for CliChannel {
    fn name(&self) -> &str {
        "cli"
    }

    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let tx = self.on_receive_tx.clone();
        let renderer = CliRenderer::new();
        let model = self.model.clone();
        let workspace = self.workspace.clone();
        let initial_mode = *self.mode.lock().await;
        let request_done = Arc::clone(&self.request_done);

        renderer.welcome(&model, &workspace.display().to_string());

        if let Some(greeting) = crate::builtin::config::EliConfig::load().greeting_message() {
            renderer.assistant_output(&greeting);
        }

        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            let stdin = io::stdin();
            let mut reader = stdin.lock();
            let mut mode = initial_mode;

            loop {
                if cancel.is_cancelled() {
                    break;
                }

                print!("{} {}", Self::cwd_name(), Self::prompt_symbol(mode));
                let _ = io::stdout().flush();

                let Some(line) = Self::read_line(&mut reader) else {
                    break;
                };
                let raw = line.trim().to_owned();
                if raw.is_empty() {
                    continue;
                }

                match raw.as_str() {
                    "/quit" | "/exit" => break,
                    "/help" => {
                        Self::show_help();
                        continue;
                    }
                    "/mode" => {
                        mode = match mode {
                            CliMode::Agent => CliMode::Shell,
                            CliMode::Shell => CliMode::Agent,
                        };
                        CliRenderer::new()
                            .info(&format!("Mode switched to {}", Self::mode_name(mode)));
                        continue;
                    }
                    _ => {}
                }

                if !Self::send_and_wait(&tx, &raw, mode, &rt, &request_done, &cancel) {
                    break;
                }
            }

            CliRenderer::new().info("Bye.");
            cancel.cancel();
        });

        *self.repl_handle.lock().await = Some(handle);
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        if let Some(handle) = self.repl_handle.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
        info!("cli channel stopped");
        Ok(())
    }

    async fn send(&self, message: ChannelMessage) -> anyhow::Result<()> {
        match message.kind {
            MessageKind::Error => self.renderer.error(&message.content),
            MessageKind::Command => self.renderer.command_output(&message.content),
            MessageKind::Normal | MessageKind::Join => {
                self.renderer.assistant_output(&message.content)
            }
        }
        self.request_done.notify_one();
        Ok(())
    }
}
