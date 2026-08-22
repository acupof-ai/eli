//! Conduit-driven runtime engine to process prompts.

mod agent_request;
mod agent_run;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use nexil::{ConduitError, ErrorKind};
use serde_json::Value;

use crate::builtin::settings::AgentSettings;
use crate::builtin::store::{FileTapeStore, ForkTapeStore};
use crate::builtin::tape::TapeService;
use crate::types::{PromptValue, RUNTIME_WORKSPACE_KEY};

use agent_request::{build_system_prompt, build_tool_state};
use agent_run::{agent_loop, run_command};

fn workspace_from_state(state: &HashMap<String, Value>) -> PathBuf {
    state
        .get(RUNTIME_WORKSPACE_KEY)
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// Whether a user message should be intercepted as a slash command.
/// A path-like first token (e.g. `/Users/.../file.pdf`) is NOT a command —
/// it falls through to the model, which can read the file.
fn is_slash_command(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return false;
    }
    let first = trimmed[1..].split_whitespace().next().unwrap_or("");
    !first.contains('/')
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// Agent that processes prompts using hooks and tools. Backed by conduit.
pub struct Agent {
    pub settings: AgentSettings,
    tapes: Option<TapeService>,
}

#[allow(clippy::new_without_default)]
impl Agent {
    /// Create a new agent with settings loaded from the environment.
    pub fn new() -> Self {
        Self {
            settings: AgentSettings::from_env(),
            tapes: None,
        }
    }

    fn ensure_tapes(&mut self) {
        if self.tapes.is_none() {
            let tapes_dir = self.settings.home.join("tapes");
            let file_store = FileTapeStore::new(tapes_dir.clone());
            let fork_store = ForkTapeStore::from_sync(file_store);
            self.tapes = Some(TapeService::new(tapes_dir, fork_store));
        }
    }

    /// Replace the tape service (used by in-process fallback for ephemeral tapes).
    pub fn set_tapes(&mut self, tapes: TapeService) {
        self.tapes = Some(tapes);
    }

    pub fn tapes(&mut self) -> &TapeService {
        self.ensure_tapes();
        // SAFETY: ensure_tapes() guarantees self.tapes is Some
        self.tapes.as_ref().expect("SAFETY: ensure_tapes called")
    }

    pub fn tapes_mut(&mut self) -> &mut TapeService {
        self.ensure_tapes();
        // SAFETY: ensure_tapes() guarantees self.tapes is Some
        self.tapes.as_mut().expect("SAFETY: ensure_tapes called")
    }

    /// Run a prompt to completion within a session.
    pub async fn run(
        &mut self,
        session_id: &str,
        prompt: PromptValue,
        state: &HashMap<String, Value>,
        model: Option<&str>,
        allowed_skills: Option<&HashSet<String>>,
        allowed_tools: Option<&HashSet<String>>,
    ) -> Result<String, ConduitError> {
        if prompt.is_blank() {
            return Err(ConduitError::new(ErrorKind::InvalidInput, "empty prompt"));
        }

        let workspace = workspace_from_state(state);
        let tape_name = TapeService::session_tape_name(session_id, &workspace);

        let settings = self.settings.clone();
        let tapes = self.tapes_mut();
        let tool_state = build_tool_state(state, &settings, allowed_skills, allowed_tools);

        tapes.ensure_bootstrap_anchor(&tape_name).await?;

        if let PromptValue::Text(ref text) = prompt
            && is_slash_command(text)
        {
            return run_command(tapes, &tape_name, text.trim(), &tool_state).await;
        }

        agent_loop(
            tapes,
            &tape_name,
            prompt,
            &settings,
            model,
            state,
            allowed_skills,
            allowed_tools,
            &tool_state,
            &workspace,
            session_id,
        )
        .await
    }

    pub fn system_prompt(
        &self,
        prompt_text: &str,
        state: &HashMap<String, Value>,
        allowed_skills: Option<&HashSet<String>>,
    ) -> String {
        let workspace = workspace_from_state(state);
        build_system_prompt(
            &self.settings,
            prompt_text,
            state,
            allowed_skills,
            &workspace,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::builtin::settings::ProviderValue;
    use crate::builtin::store::{FileTapeStore, ForkTapeStore};
    use crate::builtin::tools::register_builtin_tools;
    use nexil::llm::ApiFormat;
    use serde_json::json;

    fn test_tape_service() -> (
        tempfile::TempDir,
        TapeService,
        String,
        HashMap<String, Value>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let tapes_dir = tmp.path().join("tapes");
        let store = ForkTapeStore::from_sync(FileTapeStore::new(tapes_dir.clone()));
        let service = TapeService::new(tapes_dir, store);
        let tape_name = "workspace__session".to_owned();

        let mut tool_state = HashMap::new();
        tool_state.insert(
            RUNTIME_WORKSPACE_KEY.to_owned(),
            json!(workspace.display().to_string()),
        );

        (tmp, service, tape_name, tool_state)
    }

    fn test_settings(home: &Path) -> AgentSettings {
        AgentSettings {
            home: home.to_path_buf(),
            model: "test-model".into(),
            fallback_models: None,
            api_key: ProviderValue::None,
            api_base: ProviderValue::None,
            api_format: ApiFormat::Auto,
            max_steps: 5,
            max_tokens: 256,
            verbose: 0,
            context_window: 128_000,
            max_turn_tokens: None,
        }
    }

    #[test]
    fn slash_command_detects_commands_not_paths() {
        assert!(is_slash_command("/help"));
        assert!(is_slash_command("/fs.read path=note.txt"));
        assert!(is_slash_command("/ls -la"));
        // Absolute paths must fall through to the model, not execute as bash.
        assert!(!is_slash_command(
            "/Users/bytedance/Downloads/paper.pdf 看下这个观点"
        ));
        assert!(!is_slash_command("/tmp/foo.txt"));
        assert!(!is_slash_command("hello"));
        assert!(!is_slash_command(""));
    }

    #[tokio::test]
    async fn test_run_command_passes_workspace_state_to_tools() {
        register_builtin_tools();

        let (tmp, service, tape_name, tool_state) = test_tape_service();
        let file_path = tmp.path().join("workspace").join("note.txt");
        std::fs::write(&file_path, "hello from workspace").unwrap();

        let output = run_command(&service, &tape_name, "/fs.read path=note.txt", &tool_state)
            .await
            .unwrap();

        assert!(output.ends_with("     1\thello from workspace"));
    }

    #[tokio::test]
    async fn test_run_command_binds_tape_runtime_for_tape_tools() {
        register_builtin_tools();

        let (_tmp, service, tape_name, tool_state) = test_tape_service();
        service.ensure_bootstrap_anchor(&tape_name).await.unwrap();

        let output = run_command(&service, &tape_name, "/tape_info", &tool_state)
            .await
            .unwrap();

        assert!(output.contains("name: workspace__session"));
        assert!(output.contains("anchors: 1"));
    }

    #[tokio::test]
    async fn test_run_command_handoff_alias_creates_tape_anchor() {
        register_builtin_tools();
        let (_tmp, service, tape_name, tool_state) = test_tape_service();
        service.ensure_bootstrap_anchor(&tape_name).await.unwrap();

        let output = run_command(&service, &tape_name, "/handoff summary=done", &tool_state)
            .await
            .unwrap();
        let anchors = service.anchors(&tape_name, 10).await.unwrap();
        let last = anchors.last().unwrap();

        assert!(output.contains("anchor added: handoff"));
        assert_eq!(last.name, "handoff");
        assert_eq!(last.state.get("summary"), Some(&json!("done")));
    }

    #[test]
    fn test_build_system_prompt_ignores_workspace_agents_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(workspace.join(".agents")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(workspace.join(".agents").join("SOUL.md"), "base prompt").unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "workspace agents guidance").unwrap();

        let prompt = build_system_prompt(
            &test_settings(&home),
            "hello",
            &HashMap::new(),
            None,
            &workspace,
        );

        assert!(prompt.contains("base prompt"));
        assert!(!prompt.contains("workspace agents guidance"));
    }
}
