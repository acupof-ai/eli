//! Eli agent framework — Rust port.
//!
//! Entry point: parses CLI args with clap, initialises the framework, and
//! dispatches to the appropriate subcommand.

use clap::Parser;
use std::fs::{self, OpenOptions};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use eli::builtin::cli::{CliCommand, execute};
use eli::builtin::config::eli_home;

/// Eli — a developer-first AI agent framework.
#[derive(Parser, Debug)]
#[command(name = "eli", version, about = "Eli agent framework")]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,

    /// Exit when this parent pid dies. App-spawned sessions use it as an
    /// orphan guard: if the app is killed (SIGTERM/crash) SwiftUI runs no
    /// willTerminate, so without this the eli children leak — same problem
    /// arle solves with its own --parent-pid.
    #[arg(long, global = true)]
    parent_pid: Option<u32>,
}

/// Poll the parent pid once a second and exit when it's gone. `kill(pid, 0)`
/// probes liveness: ESRCH means dead; EPERM means alive in another session,
/// so only ESRCH triggers the exit.
fn spawn_parent_watch(parent: u32) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if unsafe { libc::kill(parent as libc::pid_t, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                std::process::exit(0);
            }
        }
    });
}

fn init_tracing() -> anyhow::Result<()> {
    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive("eli_trace=off".parse()?);

    let trace_enabled = std::env::var("ELI_TRACE")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    if trace_enabled {
        let trace_log_dir = eli_home().join("logs");
        fs::create_dir_all(&trace_log_dir)?;
        let trace_log_path = trace_log_dir.join("eli-trace.log");
        let trace_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&trace_log_path)?;

        let console_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_filter(console_filter);

        let trace_file_for_writer = std::sync::Arc::new(parking_lot::Mutex::new(trace_file));
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .with_writer(move || ArcMutexWriter(trace_file_for_writer.clone()))
            .with_filter(filter_fn(|metadata| metadata.target() == "eli_trace"));

        tracing_subscriber::registry()
            .with(console_layer)
            .with(file_layer)
            .init();

        tracing::info!(trace_log = %trace_log_path.display(), "eli trace log enabled");
    } else {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        // Always suppress eli_trace on console — it contains full LLM payloads.
        let filter = filter.add_directive("eli_trace=off".parse().expect("valid directive"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init();
    }

    Ok(())
}

/// Thread-safe writer that wraps an `Arc<Mutex<File>>`, avoiding
/// `File::try_clone` (which can panic under fd pressure).
struct ArcMutexWriter(std::sync::Arc<parking_lot::Mutex<std::fs::File>>);

impl std::io::Write for ArcMutexWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut f = self.0.lock();
        f.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut f = self.0.lock();
        f.flush()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env once, before anything else reads env vars.
    let _ = dotenvy::dotenv();

    init_tracing()?;

    let cli = Cli::parse();
    if let Some(ppid) = cli.parent_pid {
        spawn_parent_watch(ppid);
    }
    execute(cli.command).await
}
