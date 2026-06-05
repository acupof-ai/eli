//! Governed self-evolution storage for prompt rules and skill candidates.

mod auto;
mod distill;
mod eval;

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::builtin::config::eli_home;
use crate::skills::is_valid_skill_name;

const EVOLUTION_DIR: &str = ".agents/evolution";
const CANDIDATES_DIR: &str = "candidates";
const EVALUATIONS_DIR: &str = "evaluations";
const PROMOTIONS_DIR: &str = "promotions";
const SNAPSHOTS_DIR: &str = "snapshots";
const RULES_FILE: &str = "rules.md";
const RULES_BUNDLE_FILE: &str = "rules.bundle.md";
const RULES_FRAGMENTS_DIR: &str = "rules";
const KNOWLEDGE_FILE: &str = "knowledge.md";
const KNOWLEDGE_BUNDLE_FILE: &str = "knowledge.bundle.md";
const KNOWLEDGE_FRAGMENTS_DIR: &str = "knowledge";
const RUNTIME_POLICY_FILE: &str = "runtime_policy.json";
const RUNTIME_POLICY_BUNDLE_FILE: &str = "runtime_policy.bundle.json";
const RUNTIME_POLICY_FRAGMENTS_DIR: &str = "runtime-policies";
const AUTO_DIR: &str = "auto";
const AUTO_CANARIES_DIR: &str = "canaries";
const AUTO_JOURNAL_FILE: &str = "journal.jsonl";

pub use auto::{
    AutoCanary, AutoCanaryStatus, AutoEvolutionLoop, AutoEvolutionPolicy, AutoEvolutionReport,
    AutoJournalAction, AutoJournalEntry,
};
pub use distill::{DistillEvidenceSummary, DistillOutcome, DistillSkip};
pub use eval::{EvaluationCheck, EvaluationRun};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    PromptRule,
    Skill,
    CompiledKnowledge,
    RuntimePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Pending,
    Promoted,
    Rejected,
    RolledBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionCandidate {
    pub id: String,
    pub kind: CandidateKind,
    pub title: String,
    pub summary: String,
    pub content: String,
    #[serde(default)]
    pub artifact_name: Option<String>,
    pub skill_name: Option<String>,
    pub source_tape: Option<String>,
    pub source: String,
    pub status: CandidateStatus,
    pub promoted_to: Option<String>,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
    #[serde(default = "default_requires_evaluation")]
    pub requires_evaluation: bool,
    #[serde(default)]
    pub risk_level: RiskLevel,
    #[serde(default)]
    pub latest_evaluation_id: Option<String>,
    #[serde(default)]
    pub evaluation_passed: Option<bool>,
    pub created_at: String,
    pub updated_at: String,
}

impl EvolutionCandidate {
    pub fn kind_string(&self) -> &'static str {
        self.kind.as_str()
    }

    pub fn risk_level_string(&self) -> &'static str {
        self.risk_level.as_str()
    }

    pub fn status_string(&self) -> &'static str {
        self.status.as_str()
    }

    pub fn effective_fingerprint(&self) -> String {
        if self.fingerprint.is_empty() {
            candidate_fingerprint(self.kind, &self.title, &self.content, self.target_name())
        } else {
            self.fingerprint.clone()
        }
    }

    pub fn target_name(&self) -> Option<&str> {
        self.artifact_name.as_deref().or(self.skill_name.as_deref())
    }
}

#[derive(Debug, Clone)]
pub struct PromotionOutcome {
    pub candidate: EvolutionCandidate,
    pub target: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RollbackOutcome {
    pub candidate: EvolutionCandidate,
    pub target: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionRecord {
    pub candidate_id: String,
    pub target: String,
    pub snapshot_path: Option<String>,
    pub had_existing_target: bool,
    pub evaluation_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub rolled_back_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EvolutionStore {
    workspace: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimePolicyDocument {
    pub auto_evolution: RuntimeAutoEvolutionPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeAutoEvolutionPolicy {
    pub enabled: Option<bool>,
    pub min_evidence: Option<usize>,
    pub min_score: Option<u8>,
    pub canary_observations: Option<usize>,
    pub canary_ttl_hours: Option<i64>,
    pub max_auto_risk: Option<RiskLevel>,
    pub max_direct_promote_risk: Option<RiskLevel>,
}

struct NewCandidate {
    kind: CandidateKind,
    title: String,
    summary: String,
    content: String,
    artifact_name: Option<String>,
    skill_name: Option<String>,
    source_tape: Option<String>,
    source: String,
}

struct SnapshotCapture {
    had_existing_target: bool,
    snapshot_path: Option<PathBuf>,
}

impl CandidateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PromptRule => "prompt_rule",
            Self::Skill => "skill",
            Self::CompiledKnowledge => "compiled_knowledge",
            Self::RuntimePolicy => "runtime_policy",
        }
    }
}

impl CandidateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Promoted => "promoted",
            Self::Rejected => "rejected",
            Self::RolledBack => "rolled_back",
        }
    }
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl SnapshotCapture {
    fn absent() -> Self {
        Self {
            had_existing_target: false,
            snapshot_path: None,
        }
    }

    fn existing(path: PathBuf) -> Self {
        Self {
            had_existing_target: true,
            snapshot_path: Some(path),
        }
    }
}

impl NewCandidate {
    fn rule(
        title: &str,
        summary: &str,
        content: &str,
        source_tape: Option<String>,
        source: &str,
    ) -> anyhow::Result<Self> {
        Self::base(
            CandidateKind::PromptRule,
            title,
            summary,
            content,
            source_tape,
            source,
        )
    }

    fn skill(
        skill_name: &str,
        title: &str,
        summary: &str,
        content: &str,
        source_tape: Option<String>,
        source: &str,
    ) -> anyhow::Result<Self> {
        let mut candidate = Self::base(
            CandidateKind::Skill,
            title,
            summary,
            content,
            source_tape,
            source,
        )?;
        candidate.artifact_name = Some(skill_name.to_owned());
        candidate.skill_name = Some(skill_name.to_owned());
        Ok(candidate)
    }

    fn compiled_knowledge(
        artifact_name: &str,
        title: &str,
        summary: &str,
        content: &str,
        source_tape: Option<String>,
        source: &str,
    ) -> anyhow::Result<Self> {
        let mut candidate = Self::base(
            CandidateKind::CompiledKnowledge,
            title,
            summary,
            content,
            source_tape,
            source,
        )?;
        candidate.artifact_name = Some(artifact_name.to_owned());
        Ok(candidate)
    }

    fn runtime_policy(
        artifact_name: &str,
        title: &str,
        summary: &str,
        content: &str,
        source_tape: Option<String>,
        source: &str,
    ) -> anyhow::Result<Self> {
        let mut candidate = Self::base(
            CandidateKind::RuntimePolicy,
            title,
            summary,
            content,
            source_tape,
            source,
        )?;
        candidate.artifact_name = Some(artifact_name.to_owned());
        Ok(candidate)
    }

    fn base(
        kind: CandidateKind,
        title: &str,
        summary: &str,
        content: &str,
        source_tape: Option<String>,
        source: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            kind,
            title: require_text("title", title)?,
            summary: require_text("summary", summary)?,
            content: require_text("content", content)?,
            artifact_name: None,
            skill_name: None,
            source_tape,
            source: require_text("source", source)?,
        })
    }
}

impl EvolutionStore {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }

    pub fn capture_rule(
        &self,
        title: &str,
        summary: &str,
        content: &str,
        source_tape: Option<String>,
        source: &str,
    ) -> anyhow::Result<EvolutionCandidate> {
        let input = NewCandidate::rule(title, summary, content, source_tape, source)?;
        let candidate = self.new_candidate(input)?;
        self.write_candidate(&candidate)?;
        Ok(candidate)
    }

    pub fn capture_skill(
        &self,
        skill_name: &str,
        title: &str,
        summary: &str,
        content: &str,
        source_tape: Option<String>,
        source: &str,
    ) -> anyhow::Result<EvolutionCandidate> {
        validate_skill_name(skill_name)?;
        let input = NewCandidate::skill(skill_name, title, summary, content, source_tape, source)?;
        let candidate = self.new_candidate(input)?;
        self.write_candidate(&candidate)?;
        Ok(candidate)
    }

    pub fn capture_compiled_knowledge(
        &self,
        artifact_name: &str,
        title: &str,
        summary: &str,
        content: &str,
        source_tape: Option<String>,
        source: &str,
    ) -> anyhow::Result<EvolutionCandidate> {
        validate_artifact_name(artifact_name)?;
        let input = NewCandidate::compiled_knowledge(
            artifact_name,
            title,
            summary,
            content,
            source_tape,
            source,
        )?;
        let candidate = self.new_candidate(input)?;
        self.write_candidate(&candidate)?;
        Ok(candidate)
    }

    pub fn capture_runtime_policy(
        &self,
        artifact_name: &str,
        title: &str,
        summary: &str,
        content: &str,
        source_tape: Option<String>,
        source: &str,
    ) -> anyhow::Result<EvolutionCandidate> {
        validate_artifact_name(artifact_name)?;
        parse_runtime_policy_text(content)?;
        let input = NewCandidate::runtime_policy(
            artifact_name,
            title,
            summary,
            content,
            source_tape,
            source,
        )?;
        let candidate = self.new_candidate(input)?;
        self.write_candidate(&candidate)?;
        Ok(candidate)
    }

    pub fn list_candidates(&self) -> anyhow::Result<Vec<EvolutionCandidate>> {
        let candidates = read_candidates(&self.candidates_dir())?;
        Ok(sort_candidates(candidates))
    }

    pub fn distill_tape(
        &self,
        tapes_dir: &Path,
        tape_name: &str,
        persist: bool,
    ) -> anyhow::Result<DistillOutcome> {
        distill::distill_tape(self, tapes_dir, tape_name, persist)
    }

    pub fn read_candidate(&self, id: &str) -> anyhow::Result<EvolutionCandidate> {
        read_candidate_file(&self.candidate_path(id))
    }

    pub fn evaluate(&self, id: &str) -> anyhow::Result<EvaluationRun> {
        let candidate = self.read_candidate(id)?;
        let source_tape = candidate.source_tape.clone();
        ensure_evaluable(&candidate)?;
        let run = eval::evaluate_candidate(self, &candidate)?;
        self.write_evaluation(&run)?;
        self.write_candidate(&with_evaluation(candidate, &run))?;
        self.record_candidate_action(
            id,
            &source_tape,
            AutoJournalAction::Evaluated,
            format!("manual evaluation {} score {}", run.id, run.score),
        )?;
        Ok(run)
    }

    pub fn reject(&self, id: &str) -> anyhow::Result<EvolutionCandidate> {
        let candidate = self.read_candidate(id)?;
        ensure_pending(&candidate)?;
        let rejected = with_status(candidate, CandidateStatus::Rejected, None);
        self.write_candidate(&rejected)?;
        self.record_candidate_action(
            &rejected.id,
            &rejected.source_tape,
            AutoJournalAction::Rejected,
            format!("candidate '{}' rejected manually", rejected.title),
        )?;
        Ok(rejected)
    }

    pub fn promote(&self, id: &str, force: bool) -> anyhow::Result<PromotionOutcome> {
        let candidate = self.read_candidate(id)?;
        ensure_pending(&candidate)?;
        ensure_promotion_gate(&candidate, force)?;
        let target = self.target_path(&candidate);
        let record = self.new_promotion_record(&candidate, &target)?;
        self.promote_candidate(&candidate, &target, force)?;
        self.write_promotion_record_file(&record)?;
        let promoted = with_promotion(candidate, target.clone());
        self.write_candidate(&promoted)?;
        self.record_candidate_action(
            &promoted.id,
            &promoted.source_tape,
            AutoJournalAction::Promoted,
            format!("manual promotion to {}", target.display()),
        )?;
        Ok(PromotionOutcome {
            candidate: promoted,
            target,
        })
    }

    pub fn rollback(&self, id: &str) -> anyhow::Result<RollbackOutcome> {
        let candidate = self.read_candidate(id)?;
        ensure_promoted(&candidate)?;
        let record = self.read_promotion_record(id)?;
        let target = PathBuf::from(&record.target);
        self.restore_target(&record, &target)?;
        self.write_promotion_record_file(&with_rollback_record(record))?;
        self.refresh_materialized_view(candidate.kind)?;
        let rolled_back = with_status(candidate, CandidateStatus::RolledBack, Some(target.clone()));
        self.write_candidate(&rolled_back)?;
        self.record_candidate_action(
            &rolled_back.id,
            &rolled_back.source_tape,
            AutoJournalAction::RolledBack,
            format!("manual rollback restored {}", target.display()),
        )?;
        Ok(RollbackOutcome {
            candidate: rolled_back,
            target,
        })
    }

    pub fn load_prompt_rules(&self) -> anyhow::Result<String> {
        self.refresh_prompt_rules_bundle()?;
        load_prompt_rules_for_workspace(&self.workspace)
    }

    pub fn load_compiled_knowledge(&self) -> anyhow::Result<String> {
        self.refresh_compiled_knowledge_bundle()?;
        load_compiled_knowledge_for_workspace(&self.workspace)
    }

    pub fn load_runtime_policy(&self) -> anyhow::Result<RuntimePolicyDocument> {
        self.refresh_runtime_policy_bundle()?;
        load_runtime_policy_for_workspace(&self.workspace)
    }

    pub fn auto_evolve_tape(
        &self,
        tapes_dir: &Path,
        tape_name: &str,
        policy: AutoEvolutionPolicy,
    ) -> anyhow::Result<AutoEvolutionReport> {
        auto::run_auto_evolution(self, tapes_dir, tape_name, policy)
    }

    pub fn load_auto_canaries(&self) -> anyhow::Result<Vec<AutoCanary>> {
        auto::load_canaries(self)
    }

    pub fn load_auto_journal(&self) -> anyhow::Result<Vec<AutoJournalEntry>> {
        auto::load_journal(self)
    }

    pub fn observe_auto_canaries(
        &self,
        tape_name: &str,
        fingerprints: &std::collections::HashSet<String>,
    ) -> anyhow::Result<Vec<AutoCanary>> {
        auto::observe_canaries(self, tape_name, fingerprints)
    }

    pub fn expire_auto_canaries(&self) -> anyhow::Result<Vec<AutoCanary>> {
        auto::expire_canaries(self)
    }

    pub fn stage_auto_canary(
        &self,
        candidate: &EvolutionCandidate,
        evaluation: &EvaluationRun,
        tape_name: &str,
        policy: &AutoEvolutionPolicy,
    ) -> anyhow::Result<AutoCanary> {
        auto::stage_canary(self, candidate, evaluation, tape_name, policy)
    }

    pub fn finalize_auto_canary(
        &self,
        candidate_id: &str,
        force: bool,
    ) -> anyhow::Result<PromotionOutcome> {
        auto::finalize_canary(self, candidate_id, force)
    }

    pub fn append_auto_journal(&self, entry: &AutoJournalEntry) -> anyhow::Result<()> {
        auto::append_journal(self, entry)
    }

    pub fn find_candidate_by_fingerprint(
        &self,
        fingerprint: &str,
    ) -> anyhow::Result<Option<EvolutionCandidate>> {
        let candidates = self.list_candidates()?;
        Ok(candidates
            .iter()
            .find(|candidate| {
                candidate.effective_fingerprint() == fingerprint
                    && candidate.status == CandidateStatus::Pending
            })
            .cloned()
            .or_else(|| {
                candidates
                    .into_iter()
                    .find(|candidate| candidate.effective_fingerprint() == fingerprint)
            }))
    }

    fn promote_candidate(
        &self,
        candidate: &EvolutionCandidate,
        target: &Path,
        force: bool,
    ) -> anyhow::Result<()> {
        match candidate.kind {
            CandidateKind::PromptRule => self.append_prompt_rule(candidate, target),
            CandidateKind::Skill => self.write_skill(candidate, target, force),
            CandidateKind::CompiledKnowledge => {
                self.write_compiled_knowledge(candidate, target, force)
            }
            CandidateKind::RuntimePolicy => self.write_runtime_policy(candidate, target, force),
        }
    }

    fn append_prompt_rule(
        &self,
        candidate: &EvolutionCandidate,
        path: &Path,
    ) -> anyhow::Result<()> {
        ensure_parent(path)?;
        let updated = append_rule_block(read_optional(path)?, candidate);
        fs::write(path, updated)?;
        self.refresh_prompt_rules_bundle()?;
        Ok(())
    }

    fn write_skill(
        &self,
        candidate: &EvolutionCandidate,
        path: &Path,
        force: bool,
    ) -> anyhow::Result<()> {
        let skill_name = candidate.skill_name.as_deref().unwrap_or_default();
        ensure_writable_target(path, force)?;
        ensure_parent(path)?;
        fs::write(path, render_skill(candidate, skill_name))?;
        Ok(())
    }

    fn write_compiled_knowledge(
        &self,
        candidate: &EvolutionCandidate,
        path: &Path,
        force: bool,
    ) -> anyhow::Result<()> {
        ensure_writable_target(path, force)?;
        ensure_parent(path)?;
        fs::write(path, render_compiled_knowledge(candidate))?;
        self.refresh_compiled_knowledge_bundle()?;
        Ok(())
    }

    fn write_runtime_policy(
        &self,
        candidate: &EvolutionCandidate,
        path: &Path,
        force: bool,
    ) -> anyhow::Result<()> {
        ensure_writable_target(path, force)?;
        ensure_parent(path)?;
        fs::write(path, render_runtime_policy(candidate)?)?;
        self.refresh_runtime_policy_bundle()?;
        Ok(())
    }

    fn new_candidate(&self, input: NewCandidate) -> anyhow::Result<EvolutionCandidate> {
        let timestamp = now_rfc3339();
        let fingerprint = candidate_fingerprint(
            input.kind,
            &input.title,
            &input.content,
            input
                .artifact_name
                .as_deref()
                .or(input.skill_name.as_deref()),
        );
        let evidence_ids = input
            .source_tape
            .iter()
            .map(|tape| format!("tape:{tape}"))
            .collect();
        Ok(EvolutionCandidate {
            id: new_candidate_id(),
            kind: input.kind,
            title: input.title,
            summary: input.summary,
            content: input.content,
            artifact_name: input.artifact_name,
            skill_name: input.skill_name,
            source_tape: input.source_tape,
            source: input.source,
            status: CandidateStatus::Pending,
            promoted_to: None,
            fingerprint,
            evidence_ids,
            requires_evaluation: true,
            risk_level: risk_level_for(input.kind),
            latest_evaluation_id: None,
            evaluation_passed: None,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        })
    }

    fn write_candidate(&self, candidate: &EvolutionCandidate) -> anyhow::Result<()> {
        let path = self.candidate_path(&candidate.id);
        ensure_parent(&path)?;
        let body = serde_json::to_string_pretty(candidate)?;
        fs::write(path, format!("{body}\n"))?;
        Ok(())
    }

    fn candidate_path(&self, id: &str) -> PathBuf {
        self.candidates_dir().join(format!("{id}.json"))
    }

    fn evaluation_path(&self, id: &str) -> PathBuf {
        self.evaluations_dir().join(format!("{id}.json"))
    }

    fn promotion_record_path(&self, id: &str) -> PathBuf {
        self.promotions_dir().join(format!("{id}.json"))
    }

    fn snapshot_path(&self, id: &str) -> PathBuf {
        self.snapshots_dir().join(format!("{id}.bak"))
    }

    fn candidates_dir(&self) -> PathBuf {
        self.evolution_root().join(CANDIDATES_DIR)
    }

    fn evaluations_dir(&self) -> PathBuf {
        self.evolution_root().join(EVALUATIONS_DIR)
    }

    fn promotions_dir(&self) -> PathBuf {
        self.evolution_root().join(PROMOTIONS_DIR)
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.evolution_root().join(SNAPSHOTS_DIR)
    }

    fn rules_path(&self) -> PathBuf {
        self.rules_bundle_path()
    }

    fn knowledge_path(&self, artifact_name: &str) -> PathBuf {
        self.knowledge_fragment_dir()
            .join(format!("{artifact_name}.md"))
    }

    fn runtime_policy_path(&self, artifact_name: &str) -> PathBuf {
        self.runtime_policy_fragment_dir()
            .join(format!("{artifact_name}.json"))
    }

    fn skill_path(&self, skill_name: &str) -> PathBuf {
        self.workspace
            .join(".agents/skills")
            .join(skill_name)
            .join("SKILL.md")
    }

    fn evolution_root(&self) -> PathBuf {
        self.workspace.join(EVOLUTION_DIR)
    }

    fn target_path(&self, candidate: &EvolutionCandidate) -> PathBuf {
        match candidate.kind {
            CandidateKind::PromptRule => self.rules_fragment_path(&candidate.id),
            CandidateKind::Skill => self.skill_path(candidate.target_name().unwrap_or("")),
            CandidateKind::CompiledKnowledge => {
                self.knowledge_path(candidate.target_name().unwrap_or(""))
            }
            CandidateKind::RuntimePolicy => {
                self.runtime_policy_path(candidate.target_name().unwrap_or(""))
            }
        }
    }

    fn write_evaluation(&self, run: &EvaluationRun) -> anyhow::Result<()> {
        let path = self.evaluation_path(&run.id);
        write_json_file(&path, run)
    }

    fn read_promotion_record(&self, id: &str) -> anyhow::Result<PromotionRecord> {
        let body = fs::read_to_string(self.promotion_record_path(id))?;
        Ok(serde_json::from_str(&body)?)
    }

    fn write_promotion_record_file(&self, record: &PromotionRecord) -> anyhow::Result<()> {
        write_json_file(&self.promotion_record_path(&record.candidate_id), record)
    }

    fn new_promotion_record(
        &self,
        candidate: &EvolutionCandidate,
        target: &Path,
    ) -> anyhow::Result<PromotionRecord> {
        let timestamp = now_rfc3339();
        let snapshot = self.capture_snapshot(&candidate.id, target)?;
        Ok(PromotionRecord {
            candidate_id: candidate.id.clone(),
            target: display_string(target.to_path_buf()),
            snapshot_path: snapshot.snapshot_path.map(display_string),
            had_existing_target: snapshot.had_existing_target,
            evaluation_id: candidate.latest_evaluation_id.clone(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
            rolled_back_at: None,
        })
    }

    fn capture_snapshot(&self, id: &str, target: &Path) -> anyhow::Result<SnapshotCapture> {
        if !target.exists() {
            return Ok(SnapshotCapture::absent());
        }
        let path = self.snapshot_path(id);
        ensure_parent(&path)?;
        fs::copy(target, &path)?;
        Ok(SnapshotCapture::existing(path))
    }

    fn restore_target(&self, record: &PromotionRecord, target: &Path) -> anyhow::Result<()> {
        match &record.snapshot_path {
            Some(path) => self.restore_snapshot(path, target),
            None => remove_target_if_exists(target),
        }
    }

    fn restore_snapshot(&self, snapshot: &str, target: &Path) -> anyhow::Result<()> {
        ensure_parent(target)?;
        fs::copy(snapshot, target)?;
        Ok(())
    }

    fn refresh_prompt_rules_bundle(&self) -> anyhow::Result<()> {
        auto::refresh_prompt_rules_bundle(self)
    }

    fn refresh_compiled_knowledge_bundle(&self) -> anyhow::Result<()> {
        let knowledge = load_local_compiled_knowledge_for_workspace(&self.workspace)?;
        write_optional_text(&self.knowledge_bundle_path(), &knowledge)
    }

    fn refresh_runtime_policy_bundle(&self) -> anyhow::Result<()> {
        let policy = load_local_runtime_policy_for_workspace(&self.workspace)?;
        write_optional_json(&self.runtime_policy_bundle_path(), &policy)
    }

    fn refresh_materialized_view(&self, kind: CandidateKind) -> anyhow::Result<()> {
        match kind {
            CandidateKind::PromptRule => self.refresh_prompt_rules_bundle(),
            CandidateKind::CompiledKnowledge => self.refresh_compiled_knowledge_bundle(),
            CandidateKind::RuntimePolicy => self.refresh_runtime_policy_bundle(),
            CandidateKind::Skill => Ok(()),
        }
    }

    fn record_candidate_action(
        &self,
        candidate_id: &str,
        source_tape: &Option<String>,
        action: AutoJournalAction,
        detail: String,
    ) -> anyhow::Result<()> {
        self.append_auto_journal(&AutoJournalEntry {
            id: new_candidate_id(),
            tape: source_tape.clone().unwrap_or_default(),
            candidate_id: Some(candidate_id.to_owned()),
            action,
            detail,
            created_at: now_rfc3339(),
        })
    }

    fn rules_bundle_path(&self) -> PathBuf {
        self.evolution_root().join(RULES_BUNDLE_FILE)
    }

    fn knowledge_bundle_path(&self) -> PathBuf {
        self.evolution_root().join(KNOWLEDGE_BUNDLE_FILE)
    }

    fn runtime_policy_bundle_path(&self) -> PathBuf {
        self.evolution_root().join(RUNTIME_POLICY_BUNDLE_FILE)
    }

    fn rules_fragment_dir(&self) -> PathBuf {
        self.evolution_root().join(RULES_FRAGMENTS_DIR)
    }

    fn knowledge_fragment_dir(&self) -> PathBuf {
        self.evolution_root().join(KNOWLEDGE_FRAGMENTS_DIR)
    }

    fn runtime_policy_fragment_dir(&self) -> PathBuf {
        self.evolution_root().join(RUNTIME_POLICY_FRAGMENTS_DIR)
    }

    fn rules_fragment_path(&self, id: &str) -> PathBuf {
        self.rules_fragment_dir().join(format!("{id}.md"))
    }

    fn legacy_rules_path(&self) -> PathBuf {
        self.evolution_root().join(RULES_FILE)
    }

    fn legacy_knowledge_path(&self) -> PathBuf {
        self.evolution_root().join(KNOWLEDGE_FILE)
    }

    fn legacy_runtime_policy_path(&self) -> PathBuf {
        self.evolution_root().join(RUNTIME_POLICY_FILE)
    }

    fn auto_root(&self) -> PathBuf {
        self.evolution_root().join(AUTO_DIR)
    }

    fn auto_canaries_dir(&self) -> PathBuf {
        self.auto_root().join(AUTO_CANARIES_DIR)
    }

    fn auto_journal_path(&self) -> PathBuf {
        self.auto_root().join(AUTO_JOURNAL_FILE)
    }
}

pub fn load_prompt_rules_for_workspace(workspace: &Path) -> anyhow::Result<String> {
    let store = EvolutionStore::new(workspace);
    load_prompt_rules_from_paths(
        &global_rules_path(),
        &store.legacy_rules_path(),
        &store.rules_fragment_dir(),
    )
}

pub(super) fn load_local_prompt_rules_for_workspace(workspace: &Path) -> anyhow::Result<String> {
    let store = EvolutionStore::new(workspace);
    load_local_prompt_rules_from_paths(&store.legacy_rules_path(), &store.rules_fragment_dir())
}

pub fn load_compiled_knowledge_for_workspace(workspace: &Path) -> anyhow::Result<String> {
    let store = EvolutionStore::new(workspace);
    load_markdown_bundle(
        &global_knowledge_path(),
        &store.legacy_knowledge_path(),
        &store.knowledge_fragment_dir(),
    )
}

pub fn load_runtime_policy_for_workspace(
    workspace: &Path,
) -> anyhow::Result<RuntimePolicyDocument> {
    let store = EvolutionStore::new(workspace);
    load_runtime_policy_from_paths(
        &global_runtime_policy_path(),
        &store.legacy_runtime_policy_path(),
        &store.runtime_policy_fragment_dir(),
    )
}

pub(super) fn load_local_compiled_knowledge_for_workspace(
    workspace: &Path,
) -> anyhow::Result<String> {
    let store = EvolutionStore::new(workspace);
    load_markdown_bundle(
        Path::new(""),
        &store.legacy_knowledge_path(),
        &store.knowledge_fragment_dir(),
    )
}

pub(super) fn load_local_runtime_policy_for_workspace(
    workspace: &Path,
) -> anyhow::Result<RuntimePolicyDocument> {
    let store = EvolutionStore::new(workspace);
    load_runtime_policy_from_paths(
        Path::new(""),
        &store.legacy_runtime_policy_path(),
        &store.runtime_policy_fragment_dir(),
    )
}

fn global_rules_path() -> PathBuf {
    eli_home().join("evolution").join(RULES_FILE)
}

fn global_knowledge_path() -> PathBuf {
    eli_home().join("evolution").join(KNOWLEDGE_FILE)
}

fn global_runtime_policy_path() -> PathBuf {
    eli_home().join("evolution").join(RUNTIME_POLICY_FILE)
}

fn load_prompt_rules_from_paths(
    global: &Path,
    local: &Path,
    fragments_dir: &Path,
) -> anyhow::Result<String> {
    load_markdown_bundle(global, local, fragments_dir)
}

fn load_local_prompt_rules_from_paths(
    local: &Path,
    fragments_dir: &Path,
) -> anyhow::Result<String> {
    load_markdown_bundle(Path::new(""), local, fragments_dir)
}

fn load_markdown_bundle(
    global: &Path,
    local: &Path,
    fragments_dir: &Path,
) -> anyhow::Result<String> {
    let mut sources = vec![read_optional(global)?, read_optional(local)?];
    sources.extend(read_markdown_fragments(fragments_dir)?);
    Ok(join_markdown_sources(sources))
}

fn read_markdown_fragments(dir: &Path) -> anyhow::Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
        .collect::<Vec<_>>();
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    paths.into_iter().map(|path| read_optional(&path)).collect()
}

fn join_markdown_sources(parts: Vec<String>) -> String {
    parts
        .into_iter()
        .map(|part| part.trim().to_owned())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn load_runtime_policy_from_paths(
    global: &Path,
    local: &Path,
    fragments_dir: &Path,
) -> anyhow::Result<RuntimePolicyDocument> {
    let mut document = RuntimePolicyDocument::default();
    merge_runtime_policy_text(&mut document, &read_optional(global)?)?;
    merge_runtime_policy_text(&mut document, &read_optional(local)?)?;
    for path in sorted_json_fragments(fragments_dir)? {
        merge_runtime_policy_text(&mut document, &read_optional(&path)?)?;
    }
    Ok(document)
}

fn sorted_json_fragments(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(paths)
}

fn merge_runtime_policy_text(
    document: &mut RuntimePolicyDocument,
    text: &str,
) -> anyhow::Result<()> {
    if trimmed(text).is_empty() {
        return Ok(());
    }
    document.merge(parse_runtime_policy_text(text)?);
    Ok(())
}

fn sort_candidates(mut candidates: Vec<EvolutionCandidate>) -> Vec<EvolutionCandidate> {
    candidates.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    candidates
}

fn read_candidates(dir: &Path) -> anyhow::Result<Vec<EvolutionCandidate>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    fs::read_dir(dir)?
        .map(read_dir_entry_candidate)
        .collect::<anyhow::Result<Vec<_>>>()
}

fn read_dir_entry_candidate(
    entry: std::io::Result<fs::DirEntry>,
) -> anyhow::Result<EvolutionCandidate> {
    let path = entry?.path();
    read_candidate_file(&path)
}

fn read_candidate_file(path: &Path) -> anyhow::Result<EvolutionCandidate> {
    let body = fs::read_to_string(path)?;
    Ok(hydrate_candidate(serde_json::from_str(&body)?))
}

fn read_optional(path: &Path) -> anyhow::Result<String> {
    if !path.is_file() {
        return Ok(String::new());
    }
    Ok(fs::read_to_string(path)?)
}

fn append_rule_block(existing: String, candidate: &EvolutionCandidate) -> String {
    let block = rule_block(candidate);
    if trimmed(&existing).contains(trimmed(&block)) {
        return ensure_trailing_newline(existing);
    }
    [ensure_trailing_newline(existing), block]
        .into_iter()
        .map(|part| part.trim().to_owned())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n"
}

fn rule_block(candidate: &EvolutionCandidate) -> String {
    format!(
        "## {}\n{}",
        candidate.title.trim(),
        candidate.content.trim()
    )
}

fn compiled_knowledge_block(candidate: &EvolutionCandidate) -> String {
    format!(
        "## {}\n{}\n\n{}",
        candidate.title.trim(),
        candidate.summary.trim(),
        candidate.content.trim()
    )
}

fn render_skill(candidate: &EvolutionCandidate, skill_name: &str) -> String {
    format!(
        "---\nname: {skill_name}\ndescription: {}\n---\n{}{}\n",
        candidate.summary.trim(),
        render_skill_title(candidate),
        candidate.content.trim()
    )
}

fn render_compiled_knowledge(candidate: &EvolutionCandidate) -> String {
    format!("{}\n", compiled_knowledge_block(candidate))
}

fn render_runtime_policy(candidate: &EvolutionCandidate) -> anyhow::Result<String> {
    let body = serde_json::to_string_pretty(&parse_runtime_policy_text(&candidate.content)?)?;
    Ok(format!("{body}\n"))
}

fn render_skill_title(candidate: &EvolutionCandidate) -> String {
    let title = candidate.title.trim();
    if title.is_empty() {
        String::new()
    } else {
        format!("# {title}\n\n")
    }
}

fn with_status(
    mut candidate: EvolutionCandidate,
    status: CandidateStatus,
    promoted_to: Option<PathBuf>,
) -> EvolutionCandidate {
    candidate.status = status;
    candidate.promoted_to = promoted_to.map(display_string);
    candidate.updated_at = now_rfc3339();
    candidate
}

fn with_evaluation(mut candidate: EvolutionCandidate, run: &EvaluationRun) -> EvolutionCandidate {
    candidate.latest_evaluation_id = Some(run.id.clone());
    candidate.evaluation_passed = Some(run.passed);
    candidate.updated_at = now_rfc3339();
    candidate
}

fn with_promotion(mut candidate: EvolutionCandidate, target: PathBuf) -> EvolutionCandidate {
    candidate.status = CandidateStatus::Promoted;
    candidate.promoted_to = Some(display_string(target));
    candidate.updated_at = now_rfc3339();
    candidate
}

fn with_rollback_record(mut record: PromotionRecord) -> PromotionRecord {
    let timestamp = now_rfc3339();
    record.rolled_back_at = Some(timestamp.clone());
    record.updated_at = timestamp;
    record
}

fn display_string(path: PathBuf) -> String {
    path.display().to_string()
}

fn hydrate_candidate(mut candidate: EvolutionCandidate) -> EvolutionCandidate {
    if candidate.artifact_name.is_none() && candidate.kind == CandidateKind::Skill {
        candidate.artifact_name = candidate.skill_name.clone();
    }
    if candidate.fingerprint.is_empty() {
        candidate.fingerprint = candidate.effective_fingerprint();
        candidate.risk_level = risk_level_for(candidate.kind);
    }
    if candidate.evidence_ids.is_empty() {
        candidate.evidence_ids = candidate
            .source_tape
            .iter()
            .map(|tape| format!("tape:{tape}"))
            .collect();
    }
    candidate
}

fn ensure_pending(candidate: &EvolutionCandidate) -> anyhow::Result<()> {
    if candidate.status == CandidateStatus::Pending {
        return Ok(());
    }
    anyhow::bail!("candidate '{}' is not pending", candidate.id)
}

fn ensure_evaluable(candidate: &EvolutionCandidate) -> anyhow::Result<()> {
    if candidate.status == CandidateStatus::Pending {
        return Ok(());
    }
    anyhow::bail!(
        "candidate '{}' cannot be evaluated from status {}",
        candidate.id,
        candidate.status_string()
    )
}

fn ensure_promoted(candidate: &EvolutionCandidate) -> anyhow::Result<()> {
    if candidate.status == CandidateStatus::Promoted {
        return Ok(());
    }
    anyhow::bail!("candidate '{}' is not promoted", candidate.id)
}

fn ensure_promotion_gate(candidate: &EvolutionCandidate, force: bool) -> anyhow::Result<()> {
    if force || !candidate.requires_evaluation || candidate.evaluation_passed == Some(true) {
        return Ok(());
    }
    anyhow::bail!(
        "candidate '{}' requires a passing evaluation before promotion",
        candidate.id
    )
}

fn ensure_writable_target(path: &Path, force: bool) -> anyhow::Result<()> {
    if !path.exists() || force {
        return Ok(());
    }
    anyhow::bail!("target already exists: {}", path.display())
}

fn ensure_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn validate_skill_name(skill_name: &str) -> anyhow::Result<()> {
    if is_valid_skill_name(skill_name) {
        return Ok(());
    }
    anyhow::bail!("invalid skill name: {skill_name}")
}

fn validate_artifact_name(name: &str) -> anyhow::Result<()> {
    if is_valid_skill_name(name) {
        return Ok(());
    }
    anyhow::bail!("invalid artifact name: {name}")
}

fn require_text(field: &str, value: &str) -> anyhow::Result<String> {
    let text = value.trim();
    if !text.is_empty() {
        return Ok(text.to_owned());
    }
    anyhow::bail!("{field} must not be empty")
}

fn write_json_file(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    ensure_parent(path)?;
    let body = serde_json::to_string_pretty(value)?;
    fs::write(path, format!("{body}\n"))?;
    Ok(())
}

fn write_optional_text(path: &Path, text: &str) -> anyhow::Result<()> {
    if trimmed(text).is_empty() {
        return remove_target_if_exists(path);
    }
    ensure_parent(path)?;
    fs::write(path, ensure_trailing_newline(text.to_owned()))?;
    Ok(())
}

fn write_optional_json(path: &Path, value: &RuntimePolicyDocument) -> anyhow::Result<()> {
    if value.is_empty() {
        return remove_target_if_exists(path);
    }
    write_json_file(path, value)
}

fn remove_target_if_exists(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn default_requires_evaluation() -> bool {
    true
}

fn risk_level_for(kind: CandidateKind) -> RiskLevel {
    match kind {
        CandidateKind::PromptRule => RiskLevel::Medium,
        CandidateKind::Skill => RiskLevel::High,
        CandidateKind::CompiledKnowledge => RiskLevel::Low,
        CandidateKind::RuntimePolicy => RiskLevel::High,
    }
}

fn candidate_fingerprint(
    kind: CandidateKind,
    title: &str,
    content: &str,
    target_name: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str());
    hasher.update("\n");
    hasher.update(target_name.unwrap_or(""));
    hasher.update("\n");
    hasher.update(trimmed(title));
    hasher.update("\n");
    hasher.update(trimmed(content));
    format!("{:x}", hasher.finalize())
}

fn parse_runtime_policy_text(text: &str) -> anyhow::Result<RuntimePolicyDocument> {
    Ok(serde_json::from_str(text)?)
}

impl RuntimePolicyDocument {
    pub fn is_empty(&self) -> bool {
        self.auto_evolution.is_empty()
    }

    pub fn apply_to_auto_policy(&self, base: AutoEvolutionPolicy) -> Option<AutoEvolutionPolicy> {
        if self.auto_evolution.enabled == Some(false) {
            return None;
        }
        Some(self.auto_evolution.apply(base))
    }

    fn merge(&mut self, other: Self) {
        self.auto_evolution.merge(other.auto_evolution);
    }
}

impl RuntimeAutoEvolutionPolicy {
    fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.min_evidence.is_none()
            && self.min_score.is_none()
            && self.canary_observations.is_none()
            && self.canary_ttl_hours.is_none()
            && self.max_auto_risk.is_none()
            && self.max_direct_promote_risk.is_none()
    }

    fn apply(&self, mut base: AutoEvolutionPolicy) -> AutoEvolutionPolicy {
        if let Some(value) = self.min_evidence {
            base.min_evidence = value;
        }
        if let Some(value) = self.min_score {
            base.min_score = value;
        }
        if let Some(value) = self.canary_observations {
            base.canary_observations = value;
        }
        if let Some(value) = self.canary_ttl_hours {
            base.canary_ttl_hours = value;
        }
        if let Some(value) = self.max_auto_risk {
            base.max_auto_risk = value;
        }
        if let Some(value) = self.max_direct_promote_risk {
            base.max_direct_promote_risk = value;
        }
        base
    }

    fn merge(&mut self, other: Self) {
        self.enabled = other.enabled.or(self.enabled);
        self.min_evidence = other.min_evidence.or(self.min_evidence);
        self.min_score = other.min_score.or(self.min_score);
        self.canary_observations = other.canary_observations.or(self.canary_observations);
        self.canary_ttl_hours = other.canary_ttl_hours.or(self.canary_ttl_hours);
        self.max_auto_risk = other.max_auto_risk.or(self.max_auto_risk);
        self.max_direct_promote_risk = other
            .max_direct_promote_risk
            .or(self.max_direct_promote_risk);
    }
}

fn new_candidate_id() -> String {
    UuidLike::new().to_string()
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn ensure_trailing_newline(value: String) -> String {
    if value.is_empty() || value.ends_with('\n') {
        return value;
    }
    format!("{value}\n")
}

fn trimmed(value: &str) -> &str {
    value.trim()
}

struct UuidLike(uuid::Uuid);

impl UuidLike {
    fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl std::fmt::Display for UuidLike {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let compact = self.0.simple().to_string();
        f.write_str(&compact[..12])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tmp: &tempfile::TempDir) -> EvolutionStore {
        EvolutionStore::new(tmp.path())
    }

    fn skill_candidate(store: &EvolutionStore) -> EvolutionCandidate {
        store
            .capture_skill(
                "deploy-docs",
                "Deploy docs",
                "Deploy docs to the site",
                "## When to Use\nWhen the docs need publishing.\n\n## Procedure\n1. Run deploy.\n",
                Some("workspace__local".to_owned()),
                "test",
            )
            .unwrap()
    }

    fn knowledge_candidate(store: &EvolutionStore) -> EvolutionCandidate {
        store
            .capture_compiled_knowledge(
                "incident-handbook",
                "Incident Handbook",
                "Escalation notes",
                "- Page infra first.\n- Capture timeline and blast radius.\n",
                Some("workspace__local".to_owned()),
                "test",
            )
            .unwrap()
    }

    fn runtime_policy_candidate(store: &EvolutionStore, enabled: bool) -> EvolutionCandidate {
        let content = format!(
            "{{\n  \"auto_evolution\": {{\n    \"enabled\": {enabled},\n    \"min_score\": 95,\n    \"canary_observations\": 3\n  }}\n}}"
        );
        store
            .capture_runtime_policy(
                "auto-evolution",
                "Auto Evolution Policy",
                "Tune auto evolution thresholds",
                &content,
                Some("workspace__local".to_owned()),
                "test",
            )
            .unwrap()
    }

    #[test]
    fn test_capture_rule_candidate_writes_json() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = store(&tmp)
            .capture_rule(
                "Keep updates terse",
                "Prefer terse progress updates",
                "- Keep status updates to two sentences.\n",
                Some("workspace__local".to_owned()),
                "test",
            )
            .unwrap();
        let saved = store(&tmp).read_candidate(&candidate.id).unwrap();
        assert_eq!(saved.title, "Keep updates terse");
        assert_eq!(saved.status, CandidateStatus::Pending);
    }

    #[test]
    fn test_promote_rule_writes_fragment_and_refreshes_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = store(&tmp)
            .capture_rule(
                "Prefer evidence",
                "Cite files",
                "- Cite file paths when possible.",
                None,
                "test",
            )
            .unwrap();
        store(&tmp).evaluate(&candidate.id).unwrap();
        let outcome = store(&tmp).promote(&candidate.id, false).unwrap();
        assert!(
            outcome
                .target
                .ends_with(format!(".agents/evolution/rules/{}.md", candidate.id))
        );
        let fragment = fs::read_to_string(&outcome.target).unwrap();
        let bundle =
            fs::read_to_string(tmp.path().join(".agents/evolution/rules.bundle.md")).unwrap();
        assert!(fragment.contains("Prefer evidence"));
        assert!(bundle.contains("Cite file paths when possible."));
    }

    #[test]
    fn test_promote_skill_writes_skill_file() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = skill_candidate(&store(&tmp));
        store(&tmp).evaluate(&candidate.id).unwrap();
        let outcome = store(&tmp).promote(&candidate.id, false).unwrap();
        let skill = fs::read_to_string(outcome.target).unwrap();
        assert!(skill.contains("name: deploy-docs"));
        assert!(skill.contains("description: Deploy docs to the site"));
    }

    #[test]
    fn test_promote_compiled_knowledge_writes_fragment_and_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = knowledge_candidate(&store(&tmp));
        store(&tmp).evaluate(&candidate.id).unwrap();
        let outcome = store(&tmp).promote(&candidate.id, false).unwrap();
        let fragment = fs::read_to_string(outcome.target).unwrap();
        let bundle = store(&tmp).load_compiled_knowledge().unwrap();
        assert!(fragment.contains("Incident Handbook"));
        assert!(bundle.contains("Capture timeline and blast radius"));
    }

    #[test]
    fn test_promote_runtime_policy_writes_fragment_and_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = runtime_policy_candidate(&store(&tmp), true);
        store(&tmp).evaluate(&candidate.id).unwrap();
        let outcome = store(&tmp).promote(&candidate.id, false).unwrap();
        let fragment = fs::read_to_string(outcome.target).unwrap();
        let bundle = store(&tmp).load_runtime_policy().unwrap();
        assert!(fragment.contains("\"min_score\": 95"));
        assert_eq!(bundle.auto_evolution.min_score, Some(95));
        assert_eq!(bundle.auto_evolution.canary_observations, Some(3));
    }

    #[test]
    fn test_reject_updates_status() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = skill_candidate(&store(&tmp));
        let rejected = store(&tmp).reject(&candidate.id).unwrap();
        assert_eq!(rejected.status, CandidateStatus::Rejected);
        let history = store(&tmp).load_auto_journal().unwrap();
        assert!(history.iter().any(|entry| {
            entry.candidate_id.as_deref() == Some(candidate.id.as_str())
                && entry.action == AutoJournalAction::Rejected
        }));
    }

    #[test]
    fn test_load_prompt_rules_joins_global_and_local() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let global = home.path().join("rules.md");
        let local = tmp.path().join("rules.md");
        let fragments = tmp.path().join("rules");
        fs::write(&global, "## Global\n- One").unwrap();
        fs::write(&local, "## Local\n- Two").unwrap();
        fs::create_dir_all(&fragments).unwrap();
        fs::write(fragments.join("b.md"), "## B\n- Three").unwrap();
        fs::write(fragments.join("a.md"), "## A\n- Four").unwrap();
        let rules = load_prompt_rules_from_paths(&global, &local, &fragments).unwrap();
        assert!(rules.contains("## Global"));
        assert!(rules.contains("## Local"));
        assert!(rules.find("## A").unwrap() < rules.find("## B").unwrap());
    }

    #[test]
    fn test_load_runtime_policy_merges_global_local_and_fragments() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global.json");
        let local = tmp.path().join("local.json");
        let fragments = tmp.path().join("runtime-policies");
        fs::write(
            &global,
            "{\n  \"auto_evolution\": {\"min_score\": 90, \"enabled\": true}\n}",
        )
        .unwrap();
        fs::write(&local, "{\n  \"auto_evolution\": {\"min_evidence\": 2}\n}").unwrap();
        fs::create_dir_all(&fragments).unwrap();
        fs::write(
            fragments.join("z.json"),
            "{\n  \"auto_evolution\": {\"canary_observations\": 4}\n}",
        )
        .unwrap();
        let policy = load_runtime_policy_from_paths(&global, &local, &fragments).unwrap();
        assert_eq!(policy.auto_evolution.enabled, Some(true));
        assert_eq!(policy.auto_evolution.min_score, Some(90));
        assert_eq!(policy.auto_evolution.min_evidence, Some(2));
        assert_eq!(policy.auto_evolution.canary_observations, Some(4));
    }

    #[test]
    fn test_runtime_policy_can_disable_auto_policy() {
        let candidate = RuntimePolicyDocument {
            auto_evolution: RuntimeAutoEvolutionPolicy {
                enabled: Some(false),
                ..Default::default()
            },
        };
        assert!(
            candidate
                .apply_to_auto_policy(AutoEvolutionPolicy::default())
                .is_none()
        );
    }

    #[test]
    fn test_evaluate_rule_updates_candidate_with_result() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = store(&tmp)
            .capture_rule(
                "Prefer evidence",
                "Cite files",
                "- Cite file paths.",
                None,
                "test",
            )
            .unwrap();
        let run = store(&tmp).evaluate(&candidate.id).unwrap();
        let saved = store(&tmp).read_candidate(&candidate.id).unwrap();
        assert!(run.passed);
        assert_eq!(saved.latest_evaluation_id, Some(run.id));
        assert_eq!(saved.evaluation_passed, Some(true));
    }

    #[test]
    fn test_promote_requires_passing_evaluation_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = store(&tmp)
            .capture_rule(
                "Prefer evidence",
                "Cite files",
                "- Cite file paths.",
                None,
                "test",
            )
            .unwrap();
        let error = store(&tmp).promote(&candidate.id, false).unwrap_err();
        assert!(error.to_string().contains("requires a passing evaluation"));
    }

    #[test]
    fn test_rollback_restores_previous_rule_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join(".agents/evolution");
        fs::create_dir_all(&rules_dir).unwrap();
        fs::write(rules_dir.join("rules.md"), "## Existing\n- Keep this.\n").unwrap();
        let first = store(&tmp)
            .capture_rule(
                "Prefer evidence one",
                "Cite files",
                "- Cite file paths.",
                None,
                "test",
            )
            .unwrap();
        let second = store(&tmp)
            .capture_rule(
                "Prefer evidence two",
                "Cite more files",
                "- Cite every file.",
                None,
                "test",
            )
            .unwrap();
        store(&tmp).evaluate(&first.id).unwrap();
        store(&tmp).evaluate(&second.id).unwrap();
        store(&tmp).promote(&first.id, false).unwrap();
        store(&tmp).promote(&second.id, false).unwrap();
        let outcome = store(&tmp).rollback(&first.id).unwrap();
        let rules = store(&tmp).load_prompt_rules().unwrap();
        assert!(rules.contains("## Existing"));
        assert!(rules.contains("Prefer evidence two"));
        assert!(!rules.contains("Prefer evidence one"));
        assert_eq!(outcome.candidate.status, CandidateStatus::RolledBack);
    }

    #[test]
    fn test_manual_actions_are_written_to_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = store(&tmp)
            .capture_rule(
                "Prefer evidence",
                "Cite files",
                "- Cite file paths.",
                Some("workspace__local".to_owned()),
                "test",
            )
            .unwrap();
        let run = store(&tmp).evaluate(&candidate.id).unwrap();
        store(&tmp).promote(&candidate.id, false).unwrap();
        store(&tmp).rollback(&candidate.id).unwrap();
        let journal = store(&tmp).load_auto_journal().unwrap();
        assert!(journal.iter().any(|entry| {
            entry.candidate_id.as_deref() == Some(candidate.id.as_str())
                && entry.action == AutoJournalAction::Evaluated
                && entry.detail.contains(&run.id)
        }));
        assert!(journal.iter().any(|entry| {
            entry.candidate_id.as_deref() == Some(candidate.id.as_str())
                && entry.action == AutoJournalAction::Promoted
        }));
        assert!(journal.iter().any(|entry| {
            entry.candidate_id.as_deref() == Some(candidate.id.as_str())
                && entry.action == AutoJournalAction::RolledBack
        }));
    }
}
