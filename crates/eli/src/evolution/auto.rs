use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use super::{
    CandidateStatus, DistillOutcome, EvaluationRun, EvolutionCandidate, EvolutionStore,
    PromotionOutcome, RiskLevel, ensure_parent, ensure_pending, now_rfc3339, trimmed,
    write_json_file,
};

const MIN_AUTO_SCORE: u8 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoEvolutionPolicy {
    pub min_evidence: usize,
    pub min_score: u8,
    pub canary_observations: usize,
    pub canary_ttl_hours: i64,
    pub max_auto_risk: RiskLevel,
    pub max_direct_promote_risk: RiskLevel,
}

impl Default for AutoEvolutionPolicy {
    fn default() -> Self {
        Self {
            min_evidence: 1,
            min_score: MIN_AUTO_SCORE,
            canary_observations: 2,
            canary_ttl_hours: 72,
            max_auto_risk: RiskLevel::Medium,
            max_direct_promote_risk: RiskLevel::Low,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoCanaryStatus {
    Active,
    Promoted,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoCanary {
    pub candidate_id: String,
    pub fingerprint: String,
    pub title: String,
    pub status: AutoCanaryStatus,
    pub observations: usize,
    pub required_observations: usize,
    pub evaluation_id: String,
    pub evaluation_passed: bool,
    pub evaluation_score: u8,
    pub expires_at: String,
    pub last_observed_tape: Option<String>,
    pub promoted_to: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoJournalAction {
    Distilled,
    Evaluated,
    Observed,
    Staged,
    Promoted,
    Rejected,
    RolledBack,
    Expired,
    Held,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoJournalEntry {
    pub id: String,
    pub tape: String,
    pub candidate_id: Option<String>,
    pub action: AutoJournalAction,
    pub detail: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct AutoEvolutionReport {
    pub tape: String,
    pub policy: AutoEvolutionPolicy,
    pub distill: DistillOutcome,
    pub evaluations: Vec<EvaluationRun>,
    pub observed: Vec<AutoCanary>,
    pub staged: Vec<AutoCanary>,
    pub promoted: Vec<PromotionOutcome>,
    pub expired: Vec<AutoCanary>,
    pub journal_path: String,
    pub journaled_actions: usize,
}

#[derive(Debug, Clone)]
pub struct AutoEvolutionLoop {
    policy: AutoEvolutionPolicy,
}

impl AutoEvolutionLoop {
    pub fn new(policy: AutoEvolutionPolicy) -> Self {
        Self { policy }
    }

    pub fn run(
        &self,
        store: &EvolutionStore,
        tapes_dir: &Path,
        tape_name: &str,
    ) -> anyhow::Result<AutoEvolutionReport> {
        run_auto_cycle(store, tapes_dir, tape_name, self.policy.clone())
    }
}

pub(super) fn run_auto_evolution(
    store: &EvolutionStore,
    tapes_dir: &Path,
    tape_name: &str,
    policy: AutoEvolutionPolicy,
) -> anyhow::Result<AutoEvolutionReport> {
    AutoEvolutionLoop::new(policy).run(store, tapes_dir, tape_name)
}

pub(super) fn refresh_prompt_rules_bundle(store: &EvolutionStore) -> anyhow::Result<()> {
    let rules = super::load_local_prompt_rules_for_workspace(&store.workspace)?;
    write_prompt_rules_bundle(store, &rules)
}

pub(super) fn load_canaries(store: &EvolutionStore) -> anyhow::Result<Vec<AutoCanary>> {
    read_canaries(&store.auto_canaries_dir())
}

pub(super) fn load_journal(store: &EvolutionStore) -> anyhow::Result<Vec<AutoJournalEntry>> {
    read_journal(&store.auto_journal_path())
}

pub(super) fn observe_canaries(
    store: &EvolutionStore,
    tape_name: &str,
    fingerprints: &HashSet<String>,
) -> anyhow::Result<Vec<AutoCanary>> {
    let mut changed = Vec::new();
    for mut canary in load_canaries(store)? {
        if !canary.is_active() || !fingerprints.contains(&canary.fingerprint) {
            continue;
        }
        canary.observations += 1;
        canary.last_observed_tape = Some(tape_name.to_owned());
        canary.updated_at = now_rfc3339();
        write_canary(store, &canary)?;
        append_journal(
            store,
            &journal_entry(
                tape_name,
                Some(&canary.candidate_id),
                AutoJournalAction::Observed,
                format!("observed canary '{}' again", canary.title),
            ),
        )?;
        changed.push(canary);
    }
    Ok(changed)
}

pub(super) fn expire_canaries(store: &EvolutionStore) -> anyhow::Result<Vec<AutoCanary>> {
    let now = now_rfc3339();
    let mut expired = Vec::new();
    for mut canary in load_canaries(store)? {
        if !canary.is_active() || canary.expires_at > now {
            continue;
        }
        canary.status = AutoCanaryStatus::Expired;
        canary.updated_at = now.clone();
        write_canary(store, &canary)?;
        let _ = store.reject(&canary.candidate_id);
        append_journal(
            store,
            &journal_entry(
                canary.last_observed_tape.as_deref().unwrap_or(""),
                Some(&canary.candidate_id),
                AutoJournalAction::Expired,
                format!("expired canary '{}' after TTL", canary.title),
            ),
        )?;
        expired.push(canary);
    }
    Ok(expired)
}

pub(super) fn stage_canary(
    store: &EvolutionStore,
    candidate: &EvolutionCandidate,
    evaluation: &EvaluationRun,
    tape_name: &str,
    policy: &AutoEvolutionPolicy,
) -> anyhow::Result<AutoCanary> {
    if let Some(canary) = load_canary(&store.auto_canaries_dir(), &candidate.id)? {
        return Ok(canary);
    }
    let canary = candidate_canary(candidate, evaluation, tape_name, policy)?;
    write_canary(store, &canary)?;
    append_journal(
        store,
        &journal_entry(
            tape_name,
            Some(&candidate.id),
            AutoJournalAction::Staged,
            format!(
                "canary staged for '{}' with {} observations",
                candidate.title, canary.observations
            ),
        ),
    )?;
    Ok(canary)
}

pub(super) fn finalize_canary(
    store: &EvolutionStore,
    candidate_id: &str,
    force: bool,
) -> anyhow::Result<PromotionOutcome> {
    let canary = load_canary(&store.auto_canaries_dir(), candidate_id)?
        .ok_or_else(|| anyhow::anyhow!("canary '{}' not found", candidate_id))?;
    let tape = canary.last_observed_tape.clone().unwrap_or_default();
    ensure_canary_ready(&canary, force)?;
    let outcome = store.promote(candidate_id, force)?;
    write_canary(
        store,
        &with_canary_status(canary, AutoCanaryStatus::Promoted, Some(&outcome)),
    )?;
    append_journal(
        store,
        &journal_entry(
            &tape,
            Some(candidate_id),
            AutoJournalAction::Promoted,
            format!("canary promoted to {}", outcome.target.display()),
        ),
    )?;
    Ok(outcome)
}

pub(super) fn append_journal(
    store: &EvolutionStore,
    entry: &AutoJournalEntry,
) -> anyhow::Result<()> {
    ensure_parent(&store.auto_journal_path())?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(store.auto_journal_path())?;
    serde_json::to_writer(&mut file, entry)?;
    writeln!(file)?;
    Ok(())
}

fn write_prompt_rules_bundle(store: &EvolutionStore, rules: &str) -> anyhow::Result<()> {
    let path = store.rules_bundle_path();
    if trimmed(rules).is_empty() {
        return remove_file_if_exists(&path);
    }
    ensure_parent(&path)?;
    fs::write(path, format!("{}\n", trimmed(rules)))?;
    Ok(())
}

fn run_auto_cycle(
    store: &EvolutionStore,
    tapes_dir: &Path,
    tape_name: &str,
    policy: AutoEvolutionPolicy,
) -> anyhow::Result<AutoEvolutionReport> {
    let distill = store.distill_tape(tapes_dir, tape_name, true)?;
    let candidates = candidates_for_cycle(store, &distill)?;
    append_journal(
        store,
        &journal_entry(
            tape_name,
            None,
            AutoJournalAction::Distilled,
            format!(
                "distilled {} candidates from tape {}",
                candidates.len(),
                tape_name
            ),
        ),
    )?;
    let evaluations = evaluate_candidates(store, tape_name, &candidates)?;
    let direct_promoted = promote_direct_candidates(
        store,
        tape_name,
        &policy,
        &candidates,
        &evaluations,
        &distill,
    )?;
    let fingerprints = fingerprints_for_cycle(&distill);
    let observed = store.observe_auto_canaries(tape_name, &fingerprints)?;
    let expired = store.expire_auto_canaries()?;
    let staged = stage_candidates(
        store,
        tape_name,
        &policy,
        &candidates,
        &evaluations,
        &distill,
    )?;
    let promoted = [direct_promoted, promote_ready_canaries(store)?].concat();
    let journaled_actions =
        1 + evaluations.len() + observed.len() + expired.len() + staged.len() + promoted.len();
    Ok(AutoEvolutionReport {
        tape: tape_name.to_owned(),
        policy,
        distill,
        evaluations,
        observed,
        staged,
        promoted,
        expired,
        journal_path: store.auto_journal_path().display().to_string(),
        journaled_actions,
    })
}

fn candidates_for_cycle(
    store: &EvolutionStore,
    distill: &DistillOutcome,
) -> anyhow::Result<Vec<EvolutionCandidate>> {
    let mut candidates = distill.candidates.clone();
    for skip in &distill.skipped {
        if let Some(candidate) = store.find_candidate_by_fingerprint(&skip.fingerprint)? {
            push_unique_candidate(&mut candidates, candidate);
        }
    }
    Ok(candidates)
}

fn push_unique_candidate(candidates: &mut Vec<EvolutionCandidate>, candidate: EvolutionCandidate) {
    if candidates
        .iter()
        .any(|existing| existing.id == candidate.id)
    {
        return;
    }
    if candidate.status == CandidateStatus::Pending {
        candidates.push(candidate);
    }
}

fn evaluate_candidates(
    store: &EvolutionStore,
    tape_name: &str,
    candidates: &[EvolutionCandidate],
) -> anyhow::Result<Vec<EvaluationRun>> {
    let mut runs = Vec::new();
    for candidate in candidates {
        let run = store.evaluate(&candidate.id)?;
        append_journal(
            store,
            &journal_entry(
                tape_name,
                Some(&candidate.id),
                AutoJournalAction::Evaluated,
                format!("evaluation {} score {}", run.id, run.score),
            ),
        )?;
        runs.push(run);
    }
    Ok(runs)
}

fn fingerprints_for_cycle(distill: &DistillOutcome) -> HashSet<String> {
    distill
        .candidates
        .iter()
        .map(EvolutionCandidate::effective_fingerprint)
        .chain(distill.skipped.iter().map(|skip| skip.fingerprint.clone()))
        .collect()
}

fn stage_candidates(
    store: &EvolutionStore,
    tape_name: &str,
    policy: &AutoEvolutionPolicy,
    candidates: &[EvolutionCandidate],
    evaluations: &[EvaluationRun],
    distill: &DistillOutcome,
) -> anyhow::Result<Vec<AutoCanary>> {
    let evaluations = evaluation_map(evaluations);
    let mut staged = Vec::new();
    for candidate in candidates {
        let Some(run) = evaluations.get(&candidate.id) else {
            continue;
        };
        if !should_stage(candidate, run, distill, policy) {
            continue;
        }
        if has_active_canary(store, &candidate.id)? {
            continue;
        }
        staged.push(store.stage_auto_canary(candidate, run, tape_name, policy)?);
    }
    Ok(staged)
}

fn promote_direct_candidates(
    store: &EvolutionStore,
    tape_name: &str,
    policy: &AutoEvolutionPolicy,
    candidates: &[EvolutionCandidate],
    evaluations: &[EvaluationRun],
    distill: &DistillOutcome,
) -> anyhow::Result<Vec<PromotionOutcome>> {
    let evaluations = evaluation_map(evaluations);
    let mut promoted = Vec::new();
    for candidate in candidates {
        let Some(run) = evaluations.get(&candidate.id) else {
            continue;
        };
        if !should_direct_promote(candidate, run, distill, policy) {
            continue;
        }
        let outcome = store.promote(&candidate.id, false)?;
        append_journal(
            store,
            &journal_entry(
                tape_name,
                Some(&outcome.candidate.id),
                AutoJournalAction::Promoted,
                format!("directly promoted to {}", outcome.target.display()),
            ),
        )?;
        promoted.push(outcome);
    }
    Ok(promoted)
}

fn promote_ready_canaries(store: &EvolutionStore) -> anyhow::Result<Vec<PromotionOutcome>> {
    let canaries = store.load_auto_canaries()?;
    canaries
        .into_iter()
        .filter(|canary| canary.is_ready() && canary.status == AutoCanaryStatus::Active)
        .map(|canary| store.finalize_auto_canary(&canary.candidate_id, false))
        .collect()
}

fn should_stage(
    candidate: &EvolutionCandidate,
    run: &EvaluationRun,
    distill: &DistillOutcome,
    policy: &AutoEvolutionPolicy,
) -> bool {
    run.passed
        && run.score >= policy.min_score
        && distill.evidence.tape_entries >= policy.min_evidence
        && risk_allows(candidate.risk_level, policy.max_auto_risk)
        && !risk_allows(candidate.risk_level, policy.max_direct_promote_risk)
}

fn should_direct_promote(
    candidate: &EvolutionCandidate,
    run: &EvaluationRun,
    distill: &DistillOutcome,
    policy: &AutoEvolutionPolicy,
) -> bool {
    run.passed
        && run.score >= policy.min_score
        && distill.evidence.tape_entries >= policy.min_evidence
        && risk_allows(candidate.risk_level, policy.max_direct_promote_risk)
}

fn evaluation_map(evaluations: &[EvaluationRun]) -> HashMap<String, EvaluationRun> {
    evaluations
        .iter()
        .cloned()
        .map(|run| (run.candidate_id.clone(), run))
        .collect()
}

fn has_active_canary(store: &EvolutionStore, candidate_id: &str) -> anyhow::Result<bool> {
    Ok(load_canary(&store.auto_canaries_dir(), candidate_id)?
        .map(|canary| canary.is_active())
        .unwrap_or(false))
}

fn candidate_canary(
    candidate: &EvolutionCandidate,
    evaluation: &EvaluationRun,
    tape_name: &str,
    policy: &AutoEvolutionPolicy,
) -> anyhow::Result<AutoCanary> {
    ensure_pending(candidate)?;
    Ok(AutoCanary {
        candidate_id: candidate.id.clone(),
        fingerprint: candidate.effective_fingerprint(),
        title: candidate.title.clone(),
        status: AutoCanaryStatus::Active,
        observations: 1,
        required_observations: policy.canary_observations,
        evaluation_id: evaluation.id.clone(),
        evaluation_passed: evaluation.passed,
        evaluation_score: evaluation.score,
        expires_at: canary_expires_at(policy.canary_ttl_hours),
        last_observed_tape: Some(tape_name.to_owned()),
        promoted_to: None,
        created_at: now_rfc3339(),
        updated_at: now_rfc3339(),
    })
}

fn canary_expires_at(hours: i64) -> String {
    (Utc::now() + Duration::hours(hours)).to_rfc3339()
}

fn ensure_canary_ready(canary: &AutoCanary, force: bool) -> anyhow::Result<()> {
    if force || canary.is_ready() {
        return Ok(());
    }
    anyhow::bail!("canary '{}' is not ready", canary.candidate_id)
}

fn write_canary(store: &EvolutionStore, canary: &AutoCanary) -> anyhow::Result<()> {
    let path = canary_path(store, &canary.candidate_id);
    write_json_file(&path, canary)
}

fn read_canaries(dir: &Path) -> anyhow::Result<Vec<AutoCanary>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut canaries = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .map(|path| read_canary_file(&path))
        .collect::<anyhow::Result<Vec<_>>>()?;
    canaries.sort_by(|a, b| a.candidate_id.cmp(&b.candidate_id));
    Ok(canaries)
}

fn load_canary(dir: &Path, candidate_id: &str) -> anyhow::Result<Option<AutoCanary>> {
    let path = canary_path_path(dir, candidate_id);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(read_canary_file(&path)?))
}

fn read_canary_file(path: &Path) -> anyhow::Result<AutoCanary> {
    let body = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&body)?)
}

fn canary_path(store: &EvolutionStore, candidate_id: &str) -> std::path::PathBuf {
    canary_path_path(&store.auto_canaries_dir(), candidate_id)
}

fn canary_path_path(dir: &Path, candidate_id: &str) -> std::path::PathBuf {
    dir.join(format!("{candidate_id}.json"))
}

fn read_journal(path: &Path) -> anyhow::Result<Vec<AutoJournalEntry>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    fs::read_to_string(path)?
        .lines()
        .filter(|line| !trimmed(line).is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn remove_file_if_exists(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn journal_entry(
    tape: &str,
    candidate_id: Option<&str>,
    action: AutoJournalAction,
    detail: String,
) -> AutoJournalEntry {
    AutoJournalEntry {
        id: super::new_candidate_id(),
        tape: tape.to_owned(),
        candidate_id: candidate_id.map(str::to_owned),
        action,
        detail,
        created_at: now_rfc3339(),
    }
}

fn risk_rank(risk: RiskLevel) -> u8 {
    match risk {
        RiskLevel::Low => 0,
        RiskLevel::Medium => 1,
        RiskLevel::High => 2,
    }
}

fn risk_allows(risk: RiskLevel, limit: RiskLevel) -> bool {
    risk_rank(risk) <= risk_rank(limit)
}

impl AutoCanary {
    fn is_ready(&self) -> bool {
        self.status == AutoCanaryStatus::Active && self.observations >= self.required_observations
    }

    fn is_active(&self) -> bool {
        self.status == AutoCanaryStatus::Active
    }
}

fn with_canary_status(
    mut canary: AutoCanary,
    status: AutoCanaryStatus,
    outcome: Option<&PromotionOutcome>,
) -> AutoCanary {
    canary.status = status;
    canary.promoted_to = outcome.map(|outcome| outcome.target.display().to_string());
    canary.updated_at = now_rfc3339();
    canary
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexil::tape::TapeEntry;
    use serde_json::Value;
    use std::fs;

    fn store(tmp: &tempfile::TempDir) -> EvolutionStore {
        EvolutionStore::new(tmp.path())
    }

    fn tape_entry(name: &str, data: Value) -> TapeEntry {
        TapeEntry::event(name, Some(data), Value::Object(Default::default()))
    }

    fn write_tape(tmp: &tempfile::TempDir, tape: &str, entries: &[TapeEntry]) {
        let body = entries
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(
            tmp.path().join(format!("{tape}.jsonl")),
            format!("{body}\n"),
        )
        .unwrap();
    }

    fn demo_tape(tmp: &tempfile::TempDir, tape: &str) {
        write_tape(
            tmp,
            tape,
            &[
                tape_entry(
                    "agent.run",
                    serde_json::json!({
                        "status": "ok",
                        "provider": "openai",
                        "model": "gpt-5",
                        "usage": { "total_tokens": 100 }
                    }),
                ),
                tape_entry(
                    "command",
                    serde_json::json!({
                        "status": "ok",
                        "raw": "cargo test",
                        "name": "cargo",
                        "output": "ok"
                    }),
                ),
                TapeEntry::decision("Keep concise feedback", serde_json::json!({})),
            ],
        );
    }

    #[test]
    fn test_refresh_bundle_merges_legacy_and_fragments() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        fs::create_dir_all(tmp.path().join(".agents/evolution/rules")).unwrap();
        fs::write(
            tmp.path().join(".agents/evolution/rules.md"),
            "## Legacy\n- One\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join(".agents/evolution/rules/b.md"),
            "## B\n- Two\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join(".agents/evolution/rules/a.md"),
            "## A\n- Three\n",
        )
        .unwrap();
        refresh_prompt_rules_bundle(&store).unwrap();
        let bundle =
            fs::read_to_string(tmp.path().join(".agents/evolution/rules.bundle.md")).unwrap();
        assert!(bundle.contains("## Legacy"));
        assert!(bundle.find("## A").unwrap() < bundle.find("## B").unwrap());
    }

    #[test]
    fn test_refresh_bundle_removes_stale_bundle_when_rules_are_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        let bundle = tmp.path().join(".agents/evolution/rules.bundle.md");
        fs::create_dir_all(bundle.parent().unwrap()).unwrap();
        fs::write(&bundle, "## Stale\n- Old rule\n").unwrap();
        refresh_prompt_rules_bundle(&store).unwrap();
        assert!(!bundle.exists());
    }

    #[test]
    fn test_stage_and_finalize_canary_promotes_rule_fragment() {
        let tmp = tempfile::tempdir().unwrap();
        demo_tape(&tmp, "demo__tape");
        let candidate = store(&tmp)
            .capture_rule(
                "Prefer evidence",
                "Cite files",
                "- Cite file paths.",
                Some("demo__tape".to_owned()),
                "test",
            )
            .unwrap();
        let run = store(&tmp).evaluate(&candidate.id).unwrap();
        let policy = AutoEvolutionPolicy {
            canary_observations: 1,
            ..Default::default()
        };
        let canary = store(&tmp)
            .stage_auto_canary(&candidate, &run, "demo__tape", &policy)
            .unwrap();
        assert_eq!(canary.status, AutoCanaryStatus::Active);
        assert!(
            tmp.path()
                .join(format!(
                    ".agents/evolution/auto/canaries/{}.json",
                    candidate.id
                ))
                .is_file()
        );
        let promoted = store(&tmp)
            .finalize_auto_canary(&candidate.id, false)
            .unwrap();
        assert!(
            promoted
                .target
                .ends_with(format!(".agents/evolution/rules/{}.md", candidate.id))
        );
        assert!(
            store(&tmp)
                .load_prompt_rules()
                .unwrap()
                .contains("Prefer evidence")
        );
    }

    #[test]
    fn test_observe_and_expire_canaries_update_statuses() {
        let tmp = tempfile::tempdir().unwrap();
        demo_tape(&tmp, "demo__tape");
        let candidate = store(&tmp)
            .capture_rule(
                "Prefer evidence",
                "Cite files",
                "- Cite file paths.",
                Some("demo__tape".to_owned()),
                "test",
            )
            .unwrap();
        let run = store(&tmp).evaluate(&candidate.id).unwrap();
        let policy = AutoEvolutionPolicy {
            canary_ttl_hours: -1,
            ..Default::default()
        };
        let canary = store(&tmp)
            .stage_auto_canary(&candidate, &run, "demo__tape", &policy)
            .unwrap();
        let fingerprints = HashSet::from([candidate.effective_fingerprint()]);
        let observed = observe_canaries(&store(&tmp), "demo__tape", &fingerprints).unwrap();
        assert_eq!(observed.len(), 1);
        let expired = expire_canaries(&store(&tmp)).unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].candidate_id, canary.candidate_id);
    }

    #[test]
    fn test_auto_loop_runs_end_to_end_on_tape() {
        let tmp = tempfile::tempdir().unwrap();
        demo_tape(&tmp, "demo__tape");
        let policy = AutoEvolutionPolicy {
            canary_observations: 2,
            canary_ttl_hours: 24,
            ..Default::default()
        };
        let first =
            run_auto_evolution(&store(&tmp), tmp.path(), "demo__tape", policy.clone()).unwrap();
        assert!(!first.distill.candidates.is_empty());
        let second = run_auto_evolution(&store(&tmp), tmp.path(), "demo__tape", policy).unwrap();
        assert!(!second.promoted.is_empty());
    }
}
