//! Managed shell processes for the `bash` tool.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// ManagedShell
// ---------------------------------------------------------------------------

/// A shell process with captured stdout/stderr.
#[derive(Debug)]
pub struct ManagedShell {
    pub shell_id: String,
    pub cmd: String,
    pub cwd: Option<String>,
    pub output_chunks: Vec<String>,
    child: Option<tokio::process::Child>,
    exit_code: Option<i32>,
    read_handles: Vec<tokio::task::JoinHandle<Vec<String>>>,
}

impl ManagedShell {
    /// Concatenated output of stdout and stderr.
    pub fn output(&self) -> String {
        self.output_chunks.join("")
    }

    /// Process return code, or `None` if still running.
    pub fn returncode(&self) -> Option<i32> {
        self.exit_code
    }

    /// Human-readable status.
    pub fn status(&self) -> &'static str {
        if self.exit_code.is_some() {
            "exited"
        } else {
            "running"
        }
    }
}

// ---------------------------------------------------------------------------
// ShellManager
// ---------------------------------------------------------------------------

/// Manages background shell processes, identified by unique IDs.
pub struct ShellManager {
    shells: Mutex<HashMap<String, Arc<Mutex<ManagedShell>>>>,
}

impl Default for ShellManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellManager {
    pub fn new() -> Self {
        Self {
            shells: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn a new shell process.
    pub async fn start(
        &self,
        cmd: &str,
        cwd: Option<&str>,
        env: Option<&HashMap<String, String>>,
        stdin: Option<&str>,
    ) -> anyhow::Result<String> {
        let shell_id = format!("bash-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let mut child = spawn_shell(cmd, cwd, env, stdin)?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut child_stdin = child.stdin.take();

        // Write stdin data (if any) and close the pipe so the child sees EOF.
        if let Some(input) = stdin
            && let Some(ref mut stdin_pipe) = child_stdin
        {
            use tokio::io::AsyncWriteExt;
            let _ = stdin_pipe.write_all(input.as_bytes()).await;
            let _ = stdin_pipe.shutdown().await;
        }
        drop(child_stdin);

        let shell = Arc::new(Mutex::new(ManagedShell {
            shell_id: shell_id.clone(),
            cmd: cmd.to_owned(),
            cwd: cwd.map(|s| s.to_owned()),
            output_chunks: Vec::new(),
            child: Some(child),
            exit_code: None,
            read_handles: Vec::new(),
        }));

        let stdout_shell = shell.clone();
        let stderr_shell = shell.clone();
        {
            let mut s = shell.lock().await;
            s.read_handles.push(tokio::spawn(async move {
                drain_stream(stdout_shell, stdout).await
            }));
            s.read_handles.push(tokio::spawn(async move {
                drain_stream(stderr_shell, stderr).await
            }));
        }

        self.shells.lock().await.insert(shell_id.clone(), shell);
        Ok(shell_id)
    }

    /// Get a snapshot of a shell's current state, auto-cleaning finished shells.
    pub async fn get_output(
        &self,
        shell_id: &str,
    ) -> anyhow::Result<(String, Option<i32>, String)> {
        let shell_arc = {
            let shells = self.shells.lock().await;
            shells
                .get(shell_id)
                .ok_or_else(|| anyhow::anyhow!("unknown shell id: {shell_id}"))?
                .clone()
        };
        let shell = shell_arc.lock().await;
        let result = (
            shell.output(),
            shell.returncode(),
            shell.status().to_owned(),
        );
        if result.1.is_some() {
            drop(shell);
            self.shells.lock().await.remove(shell_id);
        }
        Ok(result)
    }

    /// Get the shell ID, output, return code.
    pub async fn get(&self, shell_id: &str) -> anyhow::Result<(String, Option<i32>, String)> {
        self.get_output(shell_id).await
    }

    /// Terminate a shell process.
    pub async fn terminate(&self, shell_id: &str) -> anyhow::Result<(String, Option<i32>, String)> {
        let shells = self.shells.lock().await;
        let shell_arc = shells
            .get(shell_id)
            .ok_or_else(|| anyhow::anyhow!("unknown shell id: {shell_id}"))?
            .clone();
        drop(shells);

        let mut shell = shell_arc.lock().await;
        if shell.exit_code.is_none()
            && let Some(ref mut child) = shell.child
        {
            let _ = child.kill().await;
            let status = child.wait().await.ok();
            shell.exit_code = status.and_then(|s| s.code()).or(Some(-1));
        }
        finalize_shell(&mut shell).await;
        Ok((
            shell.output(),
            shell.returncode(),
            shell.status().to_owned(),
        ))
    }

    /// Wait for a shell to finish.
    pub async fn wait_closed(
        &self,
        shell_id: &str,
    ) -> anyhow::Result<(String, Option<i32>, String)> {
        let shells = self.shells.lock().await;
        let shell_arc = shells
            .get(shell_id)
            .ok_or_else(|| anyhow::anyhow!("unknown shell id: {shell_id}"))?
            .clone();
        drop(shells);

        let mut shell = shell_arc.lock().await;
        if shell.exit_code.is_none()
            && let Some(ref mut child) = shell.child
        {
            let status = child.wait().await.ok();
            shell.exit_code = status.and_then(|s| s.code()).or(Some(-1));
        }
        finalize_shell(&mut shell).await;
        Ok((
            shell.output(),
            shell.returncode(),
            shell.status().to_owned(),
        ))
    }
}

fn spawn_shell(
    cmd: &str,
    cwd: Option<&str>,
    env: Option<&HashMap<String, String>>,
    stdin: Option<&str>,
) -> std::io::Result<tokio::process::Child> {
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    if let Some(env_map) = env {
        // Inherit the parent environment and override with the provided vars.
        // This preserves HOME, PATH, LD_LIBRARY_PATH, etc. while letting the
        // caller override specific variables.
        command.envs(env_map);
    }
    if stdin.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
}

async fn finalize_shell(shell: &mut ManagedShell) {
    for handle in shell.read_handles.drain(..) {
        if let Ok(chunks) = handle.await {
            shell.output_chunks.extend(chunks);
        }
    }
}

async fn drain_stream(
    _shell: Arc<Mutex<ManagedShell>>,
    stream: Option<impl tokio::io::AsyncRead + Unpin>,
) -> Vec<String> {
    let mut chunks = Vec::new();
    let Some(mut reader) = stream else {
        return chunks;
    };
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                chunks.push(text);
            }
            Err(_) => break,
        }
    }
    chunks
}

/// Global shell manager singleton.
static SHELL_MANAGER: std::sync::LazyLock<ShellManager> =
    std::sync::LazyLock::new(ShellManager::new);

/// Access the global shell manager.
pub fn shell_manager() -> &'static ShellManager {
    &SHELL_MANAGER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_shell_manager_start_and_wait() {
        let mgr = ShellManager::new();
        let shell_id = mgr.start("echo hello", None, None, None).await.unwrap();
        assert!(shell_id.starts_with("bash-"));

        let (output, code, status) = mgr.wait_closed(&shell_id).await.unwrap();
        assert!(output.contains("hello"));
        assert_eq!(code, Some(0));
        assert_eq!(status, "exited");
    }

    #[tokio::test]
    async fn test_shell_manager_terminate() {
        let mgr = ShellManager::new();
        let shell_id = mgr.start("sleep 60", None, None, None).await.unwrap();

        let (_, code, status) = mgr.terminate(&shell_id).await.unwrap();
        assert!(code.is_some());
        assert_eq!(status, "exited");
    }

    #[tokio::test]
    async fn test_shell_manager_get_unknown_id_returns_error() {
        let mgr = ShellManager::new();
        let result = mgr.get("nonexistent-id").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_shell_manager_start_with_cwd() {
        let mgr = ShellManager::new();
        let shell_id = mgr.start("pwd", Some("/tmp"), None, None).await.unwrap();
        let (output, code, _) = mgr.wait_closed(&shell_id).await.unwrap();
        // On macOS /tmp may be a symlink to /private/tmp
        assert!(output.contains("tmp"));
        assert_eq!(code, Some(0));
    }

    #[tokio::test]
    async fn test_shell_manager_get_output() {
        let mgr = ShellManager::new();
        let shell_id = mgr
            .start("echo test_output", None, None, None)
            .await
            .unwrap();
        let (output, code, status) = mgr.wait_closed(&shell_id).await.unwrap();
        assert!(output.contains("test_output"));
        assert_eq!(code, Some(0));
        assert_eq!(status, "exited");
    }

    #[test]
    fn test_managed_shell_status() {
        let shell = ManagedShell {
            shell_id: "test".into(),
            cmd: "echo hi".into(),
            cwd: None,
            output_chunks: vec!["hello ".into(), "world".into()],
            child: None,
            exit_code: Some(0),
            read_handles: Vec::new(),
        };
        assert_eq!(shell.output(), "hello world");
        assert_eq!(shell.returncode(), Some(0));
        assert_eq!(shell.status(), "exited");
    }

    #[test]
    fn test_managed_shell_running_status() {
        let shell = ManagedShell {
            shell_id: "test".into(),
            cmd: "sleep 10".into(),
            cwd: None,
            output_chunks: Vec::new(),
            child: None,
            exit_code: None,
            read_handles: Vec::new(),
        };
        assert_eq!(shell.status(), "running");
        assert_eq!(shell.returncode(), None);
    }

    #[test]
    fn test_global_shell_manager_exists() {
        // Just verify the global singleton is accessible
        let _mgr = shell_manager();
    }
}
