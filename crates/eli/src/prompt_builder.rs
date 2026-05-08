//! Sectioned system prompt builder with mode-based composition and size management.
//!
//! Replaces the monolithic `build_system_prompt()` with composable section builders,
//! supporting Full/Minimal/None modes and per-section truncation.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use std::sync::LazyLock;

use crate::builtin::settings::AgentSettings;
use crate::evolution::load_prompt_rules_for_workspace;
use crate::skills::{SkillMetadata, render_skills_prompt};

static HINT_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\$([A-Za-z0-9_.\-]+)").expect("SAFETY: static regex"));

/// Default hard cap for the total system prompt size (chars).
const DEFAULT_HARD_CAP: usize = 32_000;

/// Which sections to include in the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    /// All sections: identity, tools, skills, runtime.
    Full,
    /// Lightweight: identity + runtime only.
    Minimal,
    /// Empty system prompt.
    None,
}

/// A named section of the system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKind {
    /// Base system prompt from SOUL.md or built-in default.
    Identity,
    /// System instructions from SYSTEM.md (behavioral posture, tool usage).
    System,
    /// Safety hard limits and project policies.
    Guardrails,
    /// Workspace conventions and file rules.
    Workspace,
    /// Approved self-evolved rules.
    Evolved,
    /// `<available_skills>` block listing discovered skills.
    Skills,
    /// Runtime context (date, workspace, environment).
    Runtime,
}

/// Configuration for a single section.
#[derive(Debug, Clone)]
pub struct SectionConfig {
    pub kind: SectionKind,
    /// Optional per-section character limit.
    pub max_chars: Option<usize>,
    /// Lower values are truncated first when the total exceeds the hard cap.
    /// Identity (255) is never truncated.
    pub truncation_priority: u8,
}

/// Builder that assembles a system prompt from composable sections.
pub struct PromptBuilder {
    mode: PromptMode,
    hard_cap: usize,
    sections: Vec<SectionConfig>,
}

impl PromptBuilder {
    /// Create a builder with sensible default sections for the given mode.
    pub fn new(mode: PromptMode) -> Self {
        let sections = match mode {
            PromptMode::Full => vec![
                SectionConfig {
                    kind: SectionKind::Identity,
                    max_chars: None,
                    truncation_priority: 255,
                },
                SectionConfig {
                    kind: SectionKind::System,
                    max_chars: Some(4_000),
                    truncation_priority: 250,
                },
                SectionConfig {
                    kind: SectionKind::Guardrails,
                    max_chars: Some(1_500),
                    truncation_priority: 240,
                },
                SectionConfig {
                    kind: SectionKind::Workspace,
                    max_chars: Some(1_000),
                    truncation_priority: 180,
                },
                SectionConfig {
                    kind: SectionKind::Evolved,
                    max_chars: Some(2_000),
                    truncation_priority: 230,
                },
                SectionConfig {
                    kind: SectionKind::Skills,
                    max_chars: Some(8_000),
                    truncation_priority: 10,
                },
                SectionConfig {
                    kind: SectionKind::Runtime,
                    max_chars: Some(1_500),
                    truncation_priority: 200,
                },
            ],
            PromptMode::Minimal => vec![
                SectionConfig {
                    kind: SectionKind::Identity,
                    max_chars: None,
                    truncation_priority: 255,
                },
                SectionConfig {
                    kind: SectionKind::System,
                    max_chars: Some(4_000),
                    truncation_priority: 250,
                },
                SectionConfig {
                    kind: SectionKind::Guardrails,
                    max_chars: Some(1_500),
                    truncation_priority: 240,
                },
                SectionConfig {
                    kind: SectionKind::Evolved,
                    max_chars: Some(2_000),
                    truncation_priority: 230,
                },
                SectionConfig {
                    kind: SectionKind::Runtime,
                    max_chars: Some(1_000),
                    truncation_priority: 200,
                },
            ],
            PromptMode::None => vec![],
        };

        Self {
            mode,
            hard_cap: DEFAULT_HARD_CAP,
            sections,
        }
    }

    /// Override the total character hard cap.
    pub fn with_hard_cap(mut self, chars: usize) -> Self {
        self.hard_cap = chars;
        self
    }

    /// Build the final system prompt string.
    pub fn build(
        &self,
        settings: &AgentSettings,
        prompt_text: &str,
        _state: &HashMap<String, Value>,
        allowed_skills: Option<&HashSet<String>>,
        expanded_skills: &HashSet<String>,
        workspace: &Path,
    ) -> String {
        if self.mode == PromptMode::None {
            return String::new();
        }

        let mut rendered = self.render_all_sections(
            settings,
            prompt_text,
            allowed_skills,
            expanded_skills,
            workspace,
        );

        let total = Self::total_size(&rendered);
        if total > self.hard_cap {
            Self::truncate_to_budget(&mut rendered, total, self.hard_cap);
        }

        Self::join_sections(rendered)
    }

    fn render_all_sections(
        &self,
        settings: &AgentSettings,
        prompt_text: &str,
        allowed_skills: Option<&HashSet<String>>,
        expanded_skills: &HashSet<String>,
        workspace: &Path,
    ) -> Vec<(u8, String)> {
        self.sections
            .iter()
            .filter_map(|sec| {
                let content = self.render_section(
                    sec,
                    settings,
                    prompt_text,
                    allowed_skills,
                    expanded_skills,
                    workspace,
                );
                if content.is_empty() {
                    return None;
                }
                let content = match sec.max_chars {
                    Some(max) => truncate_chars(&content, max),
                    None => content,
                };
                Some((sec.truncation_priority, content))
            })
            .collect()
    }

    fn total_size(sections: &[(u8, String)]) -> usize {
        sections.iter().map(|(_, s)| s.len()).sum::<usize>() + sections.len().saturating_sub(1) * 2
    }

    fn truncate_to_budget(
        sections: &mut [(u8, String)],
        mut current_total: usize,
        hard_cap: usize,
    ) {
        let mut indices: Vec<usize> = (0..sections.len()).collect();
        indices.sort_by_key(|&i| sections[i].0);

        for &idx in &indices {
            if current_total <= hard_cap {
                break;
            }
            if sections[idx].0 == 255 {
                continue;
            }
            let section_len = sections[idx].1.len();
            let need_to_remove = current_total - hard_cap;
            if section_len <= need_to_remove {
                current_total -= section_len + 2;
                sections[idx].1.clear();
            } else {
                let new_len = section_len - need_to_remove;
                sections[idx].1 = truncate_chars(&sections[idx].1, new_len);
                current_total = hard_cap;
            }
        }
    }

    fn merge_expanded_skills(base: &HashSet<String>, prompt_text: &str) -> HashSet<String> {
        let mut merged = base.clone();
        for cap in HINT_RE.captures_iter(prompt_text) {
            if let Some(m) = cap.get(1) {
                merged.insert(m.as_str().to_owned());
            }
        }
        merged
    }

    fn join_sections(sections: Vec<(u8, String)>) -> String {
        sections
            .into_iter()
            .map(|(_, s)| s)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn render_section(
        &self,
        sec: &SectionConfig,
        settings: &AgentSettings,
        prompt_text: &str,
        allowed_skills: Option<&HashSet<String>>,
        expanded_skills: &HashSet<String>,
        workspace: &Path,
    ) -> String {
        match sec.kind {
            SectionKind::Identity => load_system_prompt_base(settings, workspace),
            SectionKind::System => load_system_instructions(settings, workspace),
            SectionKind::Guardrails => build_guardrails_section(),
            SectionKind::Workspace => build_workspace_section(workspace),
            SectionKind::Evolved => build_evolved_section(workspace),
            SectionKind::Skills => {
                let skills = crate::skills::discover_skills(workspace);
                let filtered: Vec<SkillMetadata> = match allowed_skills {
                    Some(allowed) => skills
                        .into_iter()
                        .filter(|s| allowed.contains(&s.name.to_lowercase()))
                        .collect(),
                    None => skills,
                };
                let all_expanded = Self::merge_expanded_skills(expanded_skills, prompt_text);
                render_skills_prompt(&filtered, &all_expanded)
            }
            SectionKind::Runtime => build_runtime_section(workspace),
        }
    }
}

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

fn build_guardrails_section() -> String {
    "\
# Hard Lines

Don't make stuff up — no fake tool output, no invented file contents, no claiming something's done when it isn't.
Don't leak secrets, keys, or creds.

Ground your claims in what you actually saw. Ambiguity after real attempts → one sharp question, then move."
        .to_owned()
}

fn build_workspace_section(workspace: &Path) -> String {
    format!(
        "\
# Workspace

- Root: {}
- Work from the repo root.
- Temp files go to /tmp — don't dump generated files into the repo unless asked.
- Docs live under ./docs — read them before touching architecture or config contracts.",
        workspace.display()
    )
}

fn build_runtime_section(workspace: &Path) -> String {
    // Date-only (not second-precision) so the runtime section is stable
    // across turns within a session. Seconds-precision broke ARLE's prefix
    // cache hit-rate (M_e.10): every turn produced a different `Date:` line
    // and therefore a different system prompt, so the inference server's
    // prefix cache never matched after turn 1.
    let now = chrono::Utc::now().format("%Y-%m-%d");
    let tz = chrono::Local::now().format("%Z");
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    format!(
        "<runtime>\n\
         Date: {now}\n\
         Timezone: {tz}\n\
         Workspace: {}\n\
         Platform: {os}/{arch}\n\
         </runtime>",
        workspace.display()
    )
}

fn build_evolved_section(workspace: &Path) -> String {
    match load_prompt_rules_for_workspace(workspace) {
        Ok(rules) if !rules.trim().is_empty() => rules,
        _ => String::new(),
    }
}

/// Load persona and system prompt with precedence:
/// 1. `.agents/SOUL.md` (project-level persona)
/// 2. `~/.eli/SOUL.md` (global user-level persona)
/// 3. Built-in default
///
/// The SOUL.md file defines the agent's personality and behavioral rules.
fn load_system_prompt_base(settings: &AgentSettings, workspace: &Path) -> String {
    // Try SOUL.md first (persona file)
    for path in [
        workspace.join(".agents").join("SOUL.md"),
        settings.home.join("SOUL.md"),
    ] {
        if path.is_file()
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }

    default_system_prompt().to_owned()
}

/// Load system instructions from SYSTEM.md with precedence:
/// 1. `.agents/SYSTEM.md` (project-level)
/// 2. `~/.eli/SYSTEM.md` (global user-level)
/// 3. Empty (no system instructions — the guardrails section covers essentials)
///
/// SYSTEM.md defines behavioral posture, tool usage patterns, and operational rules,
/// separate from the persona/personality in SOUL.md.
fn load_system_instructions(settings: &AgentSettings, workspace: &Path) -> String {
    for path in [
        workspace.join(".agents").join("SYSTEM.md"),
        settings.home.join("SYSTEM.md"),
    ] {
        if path.is_file()
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }

    String::new()
}

/// Fallback system prompt used only when no SOUL.md is found in the project
/// (`.agents/SOUL.md`) or user home (`~/.eli/SOUL.md`). In practice most
/// deployments have a SOUL.md, so edits here rarely take effect — update the
/// SOUL.md file instead.
fn default_system_prompt() -> &'static str {
    "You are Eli, a helpful AI coding assistant. Put the answer, recommendation, or diagnosis \
     first. Default to a tight summary unless the user asks for detail. Prefer specific facts, \
     constraints, and next actions over generic background. \
     Solve the user's actual problem, not just the literal wording; infer obvious intent and \
     make reversible choices yourself. Explain only when it changes the decision. Ask only \
     when missing information would cause an expensive or irreversible mistake. Match the \
     user's language. Skip greetings, flattery, repetition, and post-hoc summaries."
}

/// Truncate a string to at most `max_chars` characters, appending "..." if truncated.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_owned();
    }
    let end = max_chars.saturating_sub(3);
    let mut result = String::with_capacity(max_chars);
    for (i, ch) in s.char_indices() {
        if i >= end {
            break;
        }
        result.push(ch);
    }
    result.push_str("...");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_chars() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello world", 8), "hello...");
    }

    #[test]
    fn test_none_mode_returns_empty() {
        let builder = PromptBuilder::new(PromptMode::None);
        let result = builder.build(
            &AgentSettings::from_env(),
            "test",
            &HashMap::new(),
            None,
            &HashSet::new(),
            Path::new("/tmp"),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn test_minimal_mode_has_identity_and_runtime() {
        let builder = PromptBuilder::new(PromptMode::Minimal);
        let result = builder.build(
            &AgentSettings::from_env(),
            "test",
            &HashMap::new(),
            None,
            &HashSet::new(),
            Path::new("/tmp"),
        );
        assert!(result.contains("Eli"));
        assert!(result.contains("<runtime>"));
        // Minimal mode should NOT have tools or skills
        assert!(!result.contains("<available_tools>"));
    }

    #[test]
    fn test_build_includes_evolved_rules() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".agents/evolution")).unwrap();
        std::fs::write(
            tmp.path().join(".agents/evolution/rules.md"),
            "## Evolved\n- Keep updates terse.\n",
        )
        .unwrap();

        let result = PromptBuilder::new(PromptMode::Minimal).build(
            &AgentSettings::from_env(),
            "test",
            &HashMap::new(),
            None,
            &HashSet::new(),
            tmp.path(),
        );

        assert!(result.contains("## Evolved"));
        assert!(result.contains("Keep updates terse."));
    }
}
