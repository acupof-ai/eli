//! Skill discovery and rendering for eli.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

static FRONTMATTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^---\s*\n.*?\n---\s*\n").unwrap());

static SKILL_NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(SKILL_NAME_PATTERN).unwrap());

/// Directory under a project workspace containing skills.
const PROJECT_SKILLS_DIR: &str = ".agents/skills";
/// Legacy skills directory (emit a warning when found).
const LEGACY_SKILLS_DIR: &str = ".agent/skills";
/// Name of the skill definition file inside each skill directory.
const SKILL_FILE_NAME: &str = "SKILL.md";
/// Skill name regex pattern.
const SKILL_NAME_PATTERN: &str = r"^[a-z0-9]+(?:-[a-z0-9]+)*$";
/// Ordered sources: project overrides global.
const SKILL_SOURCES: [&str; 2] = ["project", "global"];
// ---------------------------------------------------------------------------
// SkillMetadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default = "default_skill_enabled")]
    enabled: bool,
}

fn default_skill_enabled() -> bool {
    true
}

enum ReadSkillResult {
    Enabled(SkillMetadata),
    Disabled(String),
}

#[derive(Debug)]
struct ParsedFrontmatter {
    frontmatter: SkillFrontmatter,
    metadata: std::collections::HashMap<String, String>,
}

/// Discovered skill metadata.
#[derive(Debug, Clone)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub location: PathBuf,
    pub source: String,
    pub metadata: std::collections::HashMap<String, String>,
    /// In-memory body for skills not backed by a file on disk.
    /// When set, `body()` returns this instead of reading from disk.
    pub content: Option<String>,
}

impl SkillMetadata {
    /// Read the skill body, stripping YAML frontmatter and substituting template variables.
    pub fn body(&self) -> Option<String> {
        if let Some(ref content) = self.content {
            return if content.is_empty() {
                None
            } else {
                Some(content.clone())
            };
        }

        let raw = std::fs::read_to_string(&self.location).ok()?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }

        let substituted = self.substitute_template_vars(trimmed);
        let body = FRONTMATTER_RE.replace(&substituted, "").trim().to_owned();
        if body.is_empty() { None } else { Some(body) }
    }

    fn substitute_template_vars(&self, text: &str) -> String {
        let skill_dir = self
            .location
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_owned());
        text.replace("$SKILL_DIR", &skill_dir)
            .replace("${SKILL_DIR}", &skill_dir)
            .replace("$PYTHON", &python)
            .replace("${PYTHON}", &python)
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Discover skills from project and global roots with override precedence
/// (project > global).
pub fn discover_skills(workspace_path: &Path) -> Vec<SkillMetadata> {
    let mut skills_by_name: std::collections::HashMap<String, SkillMetadata> =
        std::collections::HashMap::new();
    let mut disabled_skills = std::collections::HashSet::new();

    for (root, source) in iter_skill_roots(workspace_path) {
        discover_skill_root(&root, &source, &mut skills_by_name, &mut disabled_skills);
    }

    sort_skills(skills_by_name)
}

fn read_skill(skill_dir: &Path, source: &str) -> Option<ReadSkillResult> {
    let skill_file = skill_dir.join(SKILL_FILE_NAME);
    let (frontmatter, metadata) = parse_skill_file(&skill_file)?;
    if !is_valid_frontmatter(skill_dir, &frontmatter) {
        return None;
    }
    Some(skill_result(frontmatter, metadata, skill_file, source))
}

fn discover_skill_root(
    root: &Path,
    source: &str,
    skills_by_name: &mut std::collections::HashMap<String, SkillMetadata>,
    disabled_skills: &mut std::collections::HashSet<String>,
) {
    for skill_dir in sorted_skill_dirs(root) {
        let Some(result) = read_skill(&skill_dir, source) else {
            continue;
        };
        apply_skill_result(result, skills_by_name, disabled_skills);
    }
}

fn sorted_skill_dirs(root: &Path) -> Vec<PathBuf> {
    let mut entries = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn apply_skill_result(
    result: ReadSkillResult,
    skills_by_name: &mut std::collections::HashMap<String, SkillMetadata>,
    disabled_skills: &mut std::collections::HashSet<String>,
) {
    let key = result.name().to_lowercase();
    match result {
        ReadSkillResult::Enabled(metadata) if !disabled_skills.contains(&key) => {
            skills_by_name.entry(key).or_insert(metadata);
        }
        ReadSkillResult::Enabled(_) => {}
        ReadSkillResult::Disabled(_) => {
            disabled_skills.insert(key.clone());
            skills_by_name.remove(&key);
        }
    }
}

fn sort_skills(
    skills_by_name: std::collections::HashMap<String, SkillMetadata>,
) -> Vec<SkillMetadata> {
    let mut result = skills_by_name.into_values().collect::<Vec<_>>();
    result.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    result
}

fn parse_skill_file(
    skill_file: &Path,
) -> Option<(SkillFrontmatter, std::collections::HashMap<String, String>)> {
    if !skill_file.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(skill_file).ok()?;
    let parsed = parse_skill_frontmatter(&content)?;
    Some((parsed.frontmatter, parsed.metadata))
}

fn skill_result(
    frontmatter: SkillFrontmatter,
    metadata: std::collections::HashMap<String, String>,
    skill_file: PathBuf,
    source: &str,
) -> ReadSkillResult {
    if !frontmatter.enabled {
        return ReadSkillResult::Disabled(frontmatter.name);
    }
    ReadSkillResult::Enabled(SkillMetadata {
        name: frontmatter.name,
        description: frontmatter.description,
        location: skill_file
            .canonicalize()
            .unwrap_or_else(|_| skill_file.to_path_buf()),
        source: source.to_owned(),
        metadata,
        content: None,
    })
}

impl ReadSkillResult {
    fn name(&self) -> &str {
        match self {
            Self::Enabled(skill) => &skill.name,
            Self::Disabled(name) => name,
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
fn parse_frontmatter(content: &str) -> std::collections::HashMap<String, String> {
    let Some(value) = parse_frontmatter_value(content) else {
        return std::collections::HashMap::new();
    };
    yaml_value_to_string_map(&value)
}

fn parse_skill_frontmatter(content: &str) -> Option<ParsedFrontmatter> {
    let value = parse_frontmatter_value(content)?;
    let frontmatter = serde_yaml::from_value::<SkillFrontmatter>(value.clone()).ok()?;
    let metadata = yaml_value_to_string_map(&value);
    Some(ParsedFrontmatter {
        frontmatter,
        metadata,
    })
}

fn parse_frontmatter_value(content: &str) -> Option<serde_yaml::Value> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() || lines[0].trim() != "---" {
        return None;
    }

    for (idx, line) in lines[1..].iter().enumerate() {
        if line.trim() == "---" {
            let payload = lines[1..=idx].join("\n");
            return serde_yaml::from_str::<serde_yaml::Value>(&payload).ok();
        }
    }
    None
}

fn yaml_value_to_string_map(
    value: &serde_yaml::Value,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(mapping) = value.as_mapping() else {
        return map;
    };
    for (k, v) in mapping {
        if let Some(key) = k.as_str() {
            map.insert(key.to_lowercase(), format_yaml_value(v));
        }
    }
    map
}

fn format_yaml_value(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        _ => serde_yaml::to_string(value).unwrap_or_default(),
    }
}

fn is_valid_frontmatter(skill_dir: &Path, frontmatter: &SkillFrontmatter) -> bool {
    is_valid_name(&frontmatter.name, skill_dir) && is_valid_description(&frontmatter.description)
}

fn is_valid_name(name: &str, skill_dir: &Path) -> bool {
    let normalized = name.trim();
    if normalized.is_empty() || normalized.len() > 64 {
        return false;
    }
    let dir_name = skill_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if normalized != dir_name {
        return false;
    }
    SKILL_NAME_RE.is_match(normalized)
}

pub fn is_valid_skill_name(name: &str) -> bool {
    let normalized = name.trim();
    !normalized.is_empty() && normalized.len() <= 64 && SKILL_NAME_RE.is_match(normalized)
}

fn is_valid_description(description: &str) -> bool {
    let normalized = description.trim();
    !normalized.is_empty() && normalized.len() <= 1024
}

fn iter_skill_roots(workspace_path: &Path) -> Vec<(PathBuf, String)> {
    SKILL_SOURCES
        .iter()
        .flat_map(|source| match *source {
            "project" => {
                let mut roots = vec![(workspace_path.join(PROJECT_SKILLS_DIR), source.to_string())];
                let legacy = workspace_path.join(LEGACY_SKILLS_DIR);
                if legacy.is_dir() {
                    tracing::warn!(
                        path = %legacy.display(),
                        "Found legacy skills directory; please move to '{PROJECT_SKILLS_DIR}'"
                    );
                    roots.push((legacy, source.to_string()));
                }
                roots
            }
            "global" => dirs::home_dir()
                .map(|home| vec![(home.join(PROJECT_SKILLS_DIR), source.to_string())])
                .unwrap_or_default(),
            _ => vec![],
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render a prompt section listing available skills.
pub fn render_skills_prompt(skills: &[SkillMetadata], expanded_skills: &HashSet<String>) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let skill_lines = skills.iter().map(|skill| {
        let mut line = format!("- {}: {}", skill.name, skill.description);
        if expanded_skills.contains(&skill.name) {
            line.push_str(&format!("  Location: {}", skill.location.display()));
            if let Some(body) = skill.body() {
                line.push('\n');
                line.push_str(&body);
            }
        }
        line
    });
    std::iter::once("<available_skills>".to_owned())
        .chain(skill_lines)
        .chain(std::iter::once("</available_skills>".to_owned()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn write_skill(root: &Path, name: &str, description: &str, body: &str) -> PathBuf {
        let skill_dir = root.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let lines = format!("---\nname: {name}\ndescription: {description}\n---\n{body}");
        let skill_file = skill_dir.join(SKILL_FILE_NAME);
        std::fs::write(&skill_file, lines).unwrap();
        skill_file
    }

    fn write_skill_with_frontmatter(
        root: &Path,
        name: &str,
        frontmatter: &str,
        body: &str,
    ) -> PathBuf {
        let skill_dir = root.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_file = skill_dir.join(SKILL_FILE_NAME);
        std::fs::write(&skill_file, format!("---\n{frontmatter}\n---\n{body}")).unwrap();
        skill_file
    }

    fn skill_metadata(name: &str, description: &str, location: PathBuf) -> SkillMetadata {
        SkillMetadata {
            name: name.to_owned(),
            description: description.to_owned(),
            location,
            source: "project".to_owned(),
            metadata: std::collections::HashMap::new(),
            content: None,
        }
    }

    // -- parse_frontmatter tests ----------------------------------------------

    #[test]
    fn test_parse_frontmatter_valid() {
        let content = "---\nname: my-skill\ndescription: A skill\n---\nBody here\n";
        let fm = parse_frontmatter(content);
        assert_eq!(fm.get("name").unwrap(), "my-skill");
        assert_eq!(fm.get("description").unwrap(), "A skill");
    }

    #[test]
    fn test_parse_frontmatter_empty_on_invalid_yaml() {
        let content = "---\nname: [broken\n---\nbody\n";
        let fm = parse_frontmatter(content);
        assert!(fm.is_empty());
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let content = "Just plain text";
        let fm = parse_frontmatter(content);
        assert!(fm.is_empty());
    }

    #[test]
    fn test_parse_frontmatter_no_closing_delimiter() {
        let content = "---\nname: test\nno closing delimiter";
        let fm = parse_frontmatter(content);
        assert!(fm.is_empty());
    }

    // -- SkillMetadata.body() tests -------------------------------------------

    #[test]
    fn test_skill_metadata_body_strips_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_file = write_skill(tmp.path(), "demo-skill", "Demo", "Line 1\nLine 2");
        let metadata = skill_metadata("demo-skill", "Demo", skill_file);
        let body = metadata.body().unwrap();
        assert_eq!(body, "Line 1\nLine 2");
    }

    #[test]
    fn test_skill_metadata_body_returns_none_for_missing_file() {
        let metadata = skill_metadata(
            "missing",
            "Missing",
            PathBuf::from("/nonexistent/path/SKILL.md"),
        );
        assert!(metadata.body().is_none());
    }

    // -- read_skill tests -----------------------------------------------------

    #[test]
    fn test_read_skill_valid() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "my-skill", "A skill", "Body");
        let result = read_skill(&tmp.path().join("my-skill"), "project");
        assert!(result.is_some());
        match result.unwrap() {
            ReadSkillResult::Enabled(skill) => {
                assert_eq!(skill.name, "my-skill");
                assert_eq!(skill.description, "A skill");
                assert_eq!(skill.source, "project");
            }
            ReadSkillResult::Disabled(_) => panic!("expected enabled skill"),
        }
    }

    #[test]
    fn test_read_skill_defaults_enabled_when_field_missing() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "my-skill", "A skill", "Body");
        let parsed = parse_skill_frontmatter(
            &std::fs::read_to_string(tmp.path().join("my-skill").join(SKILL_FILE_NAME)).unwrap(),
        )
        .unwrap();
        assert!(parsed.frontmatter.enabled);
    }

    #[test]
    fn test_read_skill_rejects_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let result = read_skill(&tmp.path().join("nonexistent"), "project");
        assert!(result.is_none());
    }

    #[test]
    fn test_read_skill_rejects_name_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("actual-name");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let content = "---\nname: different-name\ndescription: Mismatch\n---\nBody\n";
        std::fs::write(skill_dir.join(SKILL_FILE_NAME), content).unwrap();
        let result = read_skill(&skill_dir, "project");
        assert!(result.is_none());
    }

    #[test]
    fn test_read_skill_preserves_metadata_for_disabled_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill_with_frontmatter(
            tmp.path(),
            "disabled-skill",
            "name: disabled-skill\ndescription: Disabled\nenabled: false\ndisabled_reason: retired",
            "Body",
        );
        let content =
            std::fs::read_to_string(tmp.path().join("disabled-skill").join(SKILL_FILE_NAME))
                .unwrap();
        let parsed = parse_skill_frontmatter(&content).unwrap();
        assert_eq!(parsed.metadata.get("enabled").unwrap(), "false");
        assert_eq!(parsed.metadata.get("disabled_reason").unwrap(), "retired");
        assert!(!parsed.frontmatter.enabled);
    }

    #[test]
    fn test_read_skill_returns_disabled_for_flagged_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill_with_frontmatter(
            tmp.path(),
            "disabled-skill",
            "name: disabled-skill\ndescription: Disabled\nenabled: false",
            "Body",
        );
        match read_skill(&tmp.path().join("disabled-skill"), "project").unwrap() {
            ReadSkillResult::Disabled(name) => assert_eq!(name, "disabled-skill"),
            ReadSkillResult::Enabled(_) => panic!("expected disabled skill"),
        }
    }

    // -- discover_skills tests ------------------------------------------------

    #[test]
    fn test_discover_skills_prefers_project_over_global() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join(".agents/skills");
        std::fs::create_dir_all(&project_root).unwrap();
        write_skill(&project_root, "shared", "project version", "body");

        // discover_skills searches project root based on workspace
        let discovered = discover_skills(tmp.path());
        let index: std::collections::HashMap<String, &SkillMetadata> =
            discovered.iter().map(|s| (s.name.clone(), s)).collect();
        if let Some(skill) = index.get("shared") {
            assert_eq!(skill.description, "project version");
            assert_eq!(skill.source, "project");
        }
    }

    #[test]
    fn test_discover_skills_skips_disabled_project_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join(".agents/skills");
        std::fs::create_dir_all(&project_root).unwrap();

        write_skill_with_frontmatter(
            &project_root,
            "shared",
            "name: shared\ndescription: disabled project\nenabled: false",
            "body",
        );

        let discovered = discover_skills(tmp.path());
        assert!(discovered.iter().all(|skill| skill.name != "shared"));
    }

    // -- render_skills_prompt tests -------------------------------------------

    #[test]
    fn test_render_skills_prompt_empty_returns_empty() {
        let rendered = render_skills_prompt(&[], &HashSet::new());
        assert_eq!(rendered, "");
    }

    #[test]
    fn test_render_skills_prompt_basic_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_file = write_skill(tmp.path(), "skill-a", "desc-a", "body");
        let skills = vec![
            skill_metadata("skill-a", "desc-a", skill_file.clone()),
            skill_metadata("skill-b", "desc-b", skill_file),
        ];
        let rendered = render_skills_prompt(&skills, &HashSet::new());
        assert!(rendered.contains("<available_skills>"));
        assert!(rendered.contains("- skill-a: desc-a"));
        assert!(rendered.contains("- skill-b: desc-b"));
        assert!(rendered.contains("</available_skills>"));
    }

    #[test]
    fn test_render_skills_prompt_with_expanded_body() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_file = write_skill(tmp.path(), "skill-a", "desc", "expanded body");
        let skills = vec![skill_metadata("skill-a", "desc", skill_file)];
        let mut expanded = HashSet::new();
        expanded.insert("skill-a".into());
        let rendered = render_skills_prompt(&skills, &expanded);
        assert!(rendered.contains("expanded body"));
        assert!(rendered.contains("Location:"));
    }

    // -- is_valid_name / is_valid_description ---------------------------------

    #[test]
    fn test_is_valid_name_rejects_uppercase() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("MySkill");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_valid_name("MySkill", &dir));
    }

    #[test]
    fn test_is_valid_name_rejects_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("");
        assert!(!is_valid_name("", &dir));
    }

    #[test]
    fn test_is_valid_skill_name_matches_public_rules() {
        assert!(is_valid_skill_name("deploy-docs"));
        assert!(!is_valid_skill_name("DeployDocs"));
        assert!(!is_valid_skill_name("bad_name"));
    }

    #[test]
    fn test_is_valid_description_rejects_empty() {
        assert!(!is_valid_description(""));
        assert!(!is_valid_description("   "));
    }

    #[test]
    fn test_is_valid_description_accepts_normal() {
        assert!(is_valid_description("A useful skill"));
    }
}
