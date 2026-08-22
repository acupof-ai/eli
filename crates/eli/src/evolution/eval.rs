use std::collections::{HashMap, HashSet};
use std::fs;

use serde::{Deserialize, Serialize};

use crate::builtin::settings::{
    AgentSettings, ApiFormat, DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MODEL,
    ProviderValue,
};
use crate::prompt_builder::{PromptBuilder, PromptMode};
use crate::skills::discover_skills;

use sha2::{Digest, Sha256};

use super::{
    CandidateKind, EvolutionCandidate, EvolutionStore, compiled_knowledge_block, now_rfc3339,
    parse_runtime_policy_text, read_optional, render_compiled_knowledge, render_runtime_policy,
    render_skill, rule_block, trimmed,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRun {
    pub id: String,
    pub candidate_id: String,
    pub passed: bool,
    pub score: u8,
    pub regressions: Vec<String>,
    pub checks: Vec<EvaluationCheck>,
    pub created_at: String,
}

pub(super) fn evaluate_candidate(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<EvaluationRun> {
    let checks = match candidate.kind {
        CandidateKind::PromptRule => evaluate_prompt_rule(store, candidate)?,
        CandidateKind::Skill => evaluate_skill(store, candidate)?,
        CandidateKind::CompiledKnowledge => evaluate_compiled_knowledge(store, candidate)?,
        CandidateKind::RuntimePolicy => evaluate_runtime_policy(store, candidate)?,
    };
    Ok(build_run(candidate, checks))
}

fn evaluate_prompt_rule(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<Vec<EvaluationCheck>> {
    Ok(vec![
        duplicate_rule_check(store, candidate)?,
        conflicting_rule_title_check(store, candidate)?,
        replay_rule_check(store, candidate, PromptMode::Minimal)?,
        replay_rule_check(store, candidate, PromptMode::Full)?,
    ])
}

fn evaluate_skill(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<Vec<EvaluationCheck>> {
    Ok(vec![
        skill_target_check(store, candidate)?,
        skill_materialization_check(candidate)?,
        skill_fingerprint_check(store, candidate)?,
    ])
}

fn evaluate_compiled_knowledge(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<Vec<EvaluationCheck>> {
    Ok(vec![
        target_available_check(
            store,
            candidate,
            "knowledge_target_available",
            "target knowledge file already exists",
        )?,
        compiled_knowledge_materialization_check(store, candidate)?,
        target_fingerprint_check(
            store,
            candidate,
            &render_compiled_knowledge(candidate),
            "knowledge_fingerprint_conflict",
        )?,
    ])
}

fn evaluate_runtime_policy(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<Vec<EvaluationCheck>> {
    Ok(vec![
        target_available_check(
            store,
            candidate,
            "runtime_policy_target_available",
            "target runtime policy file already exists",
        )?,
        runtime_policy_parse_check(candidate)?,
        runtime_policy_materialization_check(store, candidate)?,
        target_fingerprint_check(
            store,
            candidate,
            &render_runtime_policy(candidate)?,
            "runtime_policy_fingerprint_conflict",
        )?,
    ])
}

fn duplicate_rule_check(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<EvaluationCheck> {
    let rules = store.load_prompt_rules()?;
    Ok(pass_fail(
        "rule_not_duplicated",
        !rules_contains_block(&rules, candidate),
        "existing evolved rules already contain the same block",
    ))
}

fn conflicting_rule_title_check(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<EvaluationCheck> {
    let rules = store.load_prompt_rules()?;
    let title = format!("## {}", candidate.title.trim());
    let passes =
        !rules.lines().any(|line| line.trim() == title) || rules_contains_block(&rules, candidate);
    Ok(pass_fail(
        "rule_title_conflict",
        passes,
        "another evolved rule already uses this title with different content",
    ))
}

fn replay_rule_check(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
    mode: PromptMode,
) -> anyhow::Result<EvaluationCheck> {
    let prompt = replay_prompt(store, candidate, mode)?;
    let block = trimmed(&rule_block(candidate)).to_owned();
    Ok(pass_fail(
        replay_check_name(mode),
        prompt.contains(&block),
        "candidate block was truncated or omitted during prompt composition",
    ))
}

fn skill_target_check(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<EvaluationCheck> {
    target_available_check(
        store,
        candidate,
        "skill_target_available",
        "target skill already exists",
    )
}

fn skill_materialization_check(candidate: &EvolutionCandidate) -> anyhow::Result<EvaluationCheck> {
    let tmp = tempfile::tempdir()?;
    let name = candidate.skill_name.as_deref().unwrap_or("");
    let path = tmp
        .path()
        .join(".agents/skills")
        .join(name)
        .join("SKILL.md");
    write_rendered_skill(candidate, &path)?;
    let skills = discover_skills(tmp.path());
    Ok(pass_fail(
        "skill_materializes",
        skills.iter().any(|skill| skill.name == name),
        "rendered skill cannot be rediscovered by Eli",
    ))
}

fn skill_fingerprint_check(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<EvaluationCheck> {
    let rendered = render_skill(candidate, candidate.skill_name.as_deref().unwrap_or(""));
    target_fingerprint_check(store, candidate, &rendered, "skill_fingerprint_conflict")
}

fn replay_prompt(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
    mode: PromptMode,
) -> anyhow::Result<String> {
    let tmp = tempfile::tempdir()?;
    write_rules_workspace(store, candidate, tmp.path())?;
    Ok(build_prompt(tmp.path(), mode))
}

fn write_rules_workspace(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
    workspace: &std::path::Path,
) -> anyhow::Result<()> {
    let current = read_optional(&store.rules_path())?;
    let rules = super::append_rule_block(current, candidate);
    let path = workspace.join(".agents/evolution/rules.md");
    write_text(&path, &rules)
}

fn compiled_knowledge_materialization_check(
    _store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<EvaluationCheck> {
    let tmp = tempfile::tempdir()?;
    let workspace = tmp.path();
    let path = workspace.join(".agents/evolution/knowledge").join(format!(
        "{}.md",
        candidate.target_name().unwrap_or("knowledge")
    ));
    write_text(&path, &render_compiled_knowledge(candidate))?;
    let bundle = super::load_compiled_knowledge_for_workspace(workspace)?;
    Ok(pass_fail(
        "compiled_knowledge_materializes",
        bundle.contains(trimmed(&compiled_knowledge_block(candidate))),
        "compiled knowledge bundle did not include the candidate block",
    ))
}

fn runtime_policy_parse_check(candidate: &EvolutionCandidate) -> anyhow::Result<EvaluationCheck> {
    Ok(pass_fail(
        "runtime_policy_parses",
        parse_runtime_policy_text(&candidate.content).is_ok(),
        "runtime policy JSON is invalid or contains unsupported fields",
    ))
}

fn runtime_policy_materialization_check(
    _store: &EvolutionStore,
    candidate: &EvolutionCandidate,
) -> anyhow::Result<EvaluationCheck> {
    let tmp = tempfile::tempdir()?;
    let workspace = tmp.path();
    let path = workspace
        .join(".agents/evolution/runtime-policies")
        .join(format!(
            "{}.json",
            candidate.target_name().unwrap_or("policy")
        ));
    write_text(&path, &render_runtime_policy(candidate)?)?;
    let merged = super::load_runtime_policy_for_workspace(workspace)?;
    let rendered = parse_runtime_policy_text(&candidate.content)?;
    Ok(pass_fail(
        "runtime_policy_materializes",
        merged == rendered,
        "runtime policy bundle did not match the candidate document",
    ))
}

fn target_available_check(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
    name: &str,
    detail: &str,
) -> anyhow::Result<EvaluationCheck> {
    Ok(pass_fail(
        name,
        !store.target_path(candidate).exists(),
        detail,
    ))
}

fn target_fingerprint_check(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
    rendered: &str,
    name: &str,
) -> anyhow::Result<EvaluationCheck> {
    let current = read_optional(&store.target_path(candidate))?;
    let expected = rendered_fingerprint(candidate.kind, rendered);
    let current_fingerprint = rendered_fingerprint(candidate.kind, &current);
    let detail = fingerprint_failure_detail(current.is_empty(), current_fingerprint == expected);
    Ok(pass_fail(name, current.is_empty(), &detail))
}

fn write_rendered_skill(
    candidate: &EvolutionCandidate,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    let body = render_skill(candidate, candidate.skill_name.as_deref().unwrap_or(""));
    write_text(path, &body)
}

fn write_text(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    Ok(())
}

fn build_prompt(workspace: &std::path::Path, mode: PromptMode) -> String {
    PromptBuilder::new(mode).build(
        &evaluation_settings(workspace),
        "",
        &HashMap::new(),
        None,
        &HashSet::new(),
        workspace,
    )
}

fn evaluation_settings(home: &std::path::Path) -> AgentSettings {
    AgentSettings {
        home: home.to_path_buf(),
        model: DEFAULT_MODEL.to_owned(),
        fallback_models: None,
        api_key: ProviderValue::None,
        api_base: ProviderValue::None,
        api_format: ApiFormat::Auto,
        max_steps: 50,
        max_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        verbose: 0,
        context_window: DEFAULT_CONTEXT_WINDOW,
        max_turn_tokens: None,
    }
}

fn build_run(candidate: &EvolutionCandidate, checks: Vec<EvaluationCheck>) -> EvaluationRun {
    let regressions = failed_checks(&checks);
    let passed = regressions.is_empty();
    EvaluationRun {
        id: super::new_candidate_id(),
        candidate_id: candidate.id.clone(),
        passed,
        score: score(&checks),
        regressions,
        checks,
        created_at: now_rfc3339(),
    }
}

fn failed_checks(checks: &[EvaluationCheck]) -> Vec<String> {
    checks
        .iter()
        .filter(|check| !check.passed)
        .map(|check| check.name.clone())
        .collect()
}

fn score(checks: &[EvaluationCheck]) -> u8 {
    if checks.is_empty() {
        return 0;
    }
    let passed = checks.iter().filter(|check| check.passed).count();
    ((passed * 100) / checks.len()) as u8
}

fn pass_fail(name: &str, passed: bool, failure_detail: &str) -> EvaluationCheck {
    EvaluationCheck {
        name: name.to_owned(),
        passed,
        detail: check_detail(passed, failure_detail),
    }
}

fn check_detail(passed: bool, failure_detail: &str) -> String {
    if passed {
        "ok".to_owned()
    } else {
        failure_detail.to_owned()
    }
}

fn replay_check_name(mode: PromptMode) -> &'static str {
    match mode {
        PromptMode::Full => "prompt_replay_full",
        PromptMode::Minimal => "prompt_replay_minimal",
        PromptMode::None => "prompt_replay_none",
    }
}

fn rules_contains_block(rules: &str, candidate: &EvolutionCandidate) -> bool {
    rules.contains(trimmed(&rule_block(candidate)))
}

fn rendered_fingerprint(kind: CandidateKind, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str());
    hasher.update("\n");
    hasher.update(text.trim());
    format!("{:x}", hasher.finalize())
}

fn fingerprint_failure_detail(is_empty: bool, is_duplicate: bool) -> String {
    if is_empty {
        "ok".to_owned()
    } else if is_duplicate {
        "target skill already exists with identical content".to_owned()
    } else {
        "existing target fingerprint differs from this candidate".to_owned()
    }
}
