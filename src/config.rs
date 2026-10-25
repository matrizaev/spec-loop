use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Result, SpecLoopError, io_error};

const SCHEMA_VERSION: u32 = 1;
const SPEC_ID_PLACEHOLDER: &str = "{spec_id}";
const SPEC_PATH_PLACEHOLDER: &str = "{spec_path}";
const PLAN_PATH_PLACEHOLDER: &str = "{plan_path}";
const PI_THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Strict repository policy loaded from `.spec-loop.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub schema_version: u32,
    pub specs: SpecsConfig,
    #[serde(default)]
    pub plan: PlanConfig,
    #[serde(default)]
    pub prompts: PromptConfig,
    #[serde(default)]
    pub checks: Vec<CheckConfig>,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub archive: ArchiveConfig,
    #[serde(default)]
    pub completion: CompletionConfig,
    #[serde(default)]
    pub acceptance: AcceptanceConfig,
}

/// Spec file patterns, identifier syntax, and ordering policy.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecsConfig {
    pub active_globs: Vec<String>,
    pub id_regex: String,
    #[serde(default)]
    pub ordering: OrderingPolicy,
    #[serde(default)]
    pub selection: SelectionPolicy,
}

/// Ordering used for eligible specs.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OrderingPolicy {
    /// Compare identifiers lexically.
    #[default]
    Lexical,
    /// Compare identifiers as unsigned decimal integers, then lexically.
    Numeric,
}

/// Requested-spec selection policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionPolicy {
    /// A requested spec must be the first eligible active spec.
    #[default]
    Next,
    /// Any eligible active spec may be requested.
    Any,
}

/// Repository-relative path template for the required implementation plan.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlanConfig {
    pub path_template: String,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            path_template: ".spec-loop/plans/{spec_id}.md".to_owned(),
        }
    }
}

/// Role-specific repository instruction and prompt files.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub plan_author: Vec<PathBuf>,
    pub plan_review: Vec<PathBuf>,
    pub plan_review_status_audit: Vec<PathBuf>,
    pub plan_audit: Vec<PathBuf>,
    pub implementation: Vec<PathBuf>,
    pub review: Vec<PathBuf>,
    pub review_status_audit: Vec<PathBuf>,
    pub review_audit: Vec<PathBuf>,
}

/// One argv-only configured check command.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfig {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default = "default_cwd")]
    pub cwd: PathBuf,
    pub timeout_seconds: Option<u64>,
}

fn default_cwd() -> PathBuf {
    PathBuf::from(".")
}

/// Pi backend settings. Resource availability is controlled only by Pi configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub backend: String,
    pub executable: PathBuf,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub stages: AgentStages,
    pub timeout_seconds: Option<u64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            backend: "pi".to_owned(),
            executable: PathBuf::from("pi"),
            model: None,
            thinking: None,
            stages: AgentStages::default(),
            timeout_seconds: None,
        }
    }
}

/// Model and thinking overrides for planning, implementation, and review roles.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentStages {
    pub planning: AgentStageConfig,
    pub implementation: AgentStageConfig,
    pub review: AgentStageConfig,
}

/// Optional per-stage values; omitted fields inherit global agent settings.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentStageConfig {
    pub model: Option<String>,
    pub thinking: Option<String>,
}

/// Role family used to resolve model and thinking settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStage {
    Planning,
    Implementation,
    Review,
}

impl AgentConfig {
    pub fn model_for(&self, stage: AgentStage) -> Option<&str> {
        self.stage_config(stage)
            .model
            .as_deref()
            .or(self.model.as_deref())
    }

    pub fn thinking_for(&self, stage: AgentStage) -> Option<&str> {
        self.stage_config(stage)
            .thinking
            .as_deref()
            .or(self.thinking.as_deref())
    }

    const fn stage_config(&self, stage: AgentStage) -> &AgentStageConfig {
        match stage {
            AgentStage::Planning => &self.stages.planning,
            AgentStage::Implementation => &self.stages.implementation,
            AgentStage::Review => &self.stages.review,
        }
    }
}

/// Optional deterministic spec archival.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveConfig {
    pub enabled: bool,
    pub destination: Option<String>,
}

/// Commit message and retry policy.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompletionConfig {
    pub max_passes: usize,
    pub commit_message: String,
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            max_passes: 0,
            commit_message: "Implement {spec_id}".to_owned(),
        }
    }
}

/// User-owned acceptance gates that cannot be run by the engine.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AcceptanceConfig {
    pub manual_gates: Vec<String>,
}

impl Profile {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|source| io_error("read profile", path, source))?;
        let profile: Self = toml::from_str(&contents)
            .map_err(|error| SpecLoopError::Config(format!("{}: {error}", path.display())))?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(SpecLoopError::Config(format!(
                "schema_version must be {SCHEMA_VERSION}, got {}",
                self.schema_version
            )));
        }
        validate_specs(&self.specs)?;
        validate_plan_and_archive(&self.plan, &self.archive)?;
        validate_agent(&self.agent)?;
        validate_checks(&self.checks)?;
        validate_resource_paths(&self.prompts)?;
        validate_completion(&self.completion, &self.acceptance)
    }
}

fn validate_specs(specs: &SpecsConfig) -> Result<()> {
    if specs.active_globs.is_empty() {
        return Err(SpecLoopError::Config(
            "specs.active_globs must contain at least one pattern".to_owned(),
        ));
    }
    let id_regex = regex::Regex::new(&specs.id_regex)
        .map_err(|error| SpecLoopError::Config(format!("specs.id_regex: {error}")))?;
    if !id_regex.capture_names().flatten().any(|name| name == "id") {
        return Err(SpecLoopError::Config(
            "specs.id_regex must define a named capture group `id`".to_owned(),
        ));
    }
    for pattern in &specs.active_globs {
        globset::Glob::new(pattern).map_err(|error| {
            SpecLoopError::Config(format!("invalid spec glob {pattern:?}: {error}"))
        })?;
    }
    Ok(())
}

fn validate_plan_and_archive(plan: &PlanConfig, archive: &ArchiveConfig) -> Result<()> {
    validate_template_path(&plan.path_template, "plan.path_template")?;
    match archive.destination.as_deref() {
        Some(destination) => validate_relative_path(Path::new(destination), "archive.destination")?,
        None if archive.enabled => {
            return Err(SpecLoopError::Config(
                "archive.destination is required when archive.enabled is true".to_owned(),
            ));
        }
        None => {}
    }
    Ok(())
}

fn validate_agent(agent: &AgentConfig) -> Result<()> {
    if agent.backend != "pi" {
        return Err(SpecLoopError::Config(format!(
            "unsupported agent backend {:?}; only `pi` is available",
            agent.backend
        )));
    }
    if agent.executable.as_os_str().is_empty() || agent.executable.to_string_lossy().contains('\0')
    {
        return Err(SpecLoopError::Config(
            "agent.executable cannot be empty or contain NUL".to_owned(),
        ));
    }
    if agent.timeout_seconds == Some(0) {
        return Err(SpecLoopError::Config(
            "agent.timeout_seconds must be positive when set".to_owned(),
        ));
    }
    for (field, value) in [
        ("agent.model", &agent.model),
        ("agent.thinking", &agent.thinking),
        ("agent.stages.planning.model", &agent.stages.planning.model),
        (
            "agent.stages.planning.thinking",
            &agent.stages.planning.thinking,
        ),
        (
            "agent.stages.implementation.model",
            &agent.stages.implementation.model,
        ),
        (
            "agent.stages.implementation.thinking",
            &agent.stages.implementation.thinking,
        ),
        ("agent.stages.review.model", &agent.stages.review.model),
        (
            "agent.stages.review.thinking",
            &agent.stages.review.thinking,
        ),
    ] {
        if value
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.contains('\0'))
        {
            return Err(SpecLoopError::Config(format!(
                "{field} cannot be empty or contain NUL"
            )));
        }
    }
    for (field, value) in [
        ("agent.thinking", agent.thinking.as_deref()),
        (
            "agent.stages.planning.thinking",
            agent.stages.planning.thinking.as_deref(),
        ),
        (
            "agent.stages.implementation.thinking",
            agent.stages.implementation.thinking.as_deref(),
        ),
        (
            "agent.stages.review.thinking",
            agent.stages.review.thinking.as_deref(),
        ),
    ] {
        if let Some(value) = value
            && !PI_THINKING_LEVELS.contains(&value)
        {
            return Err(SpecLoopError::Config(format!(
                "unsupported {field} value {value:?}; expected one of {}",
                PI_THINKING_LEVELS.join(", ")
            )));
        }
    }
    Ok(())
}

fn validate_checks(checks: &[CheckConfig]) -> Result<()> {
    let mut names = BTreeSet::new();
    for check in checks {
        if check.name.trim().is_empty() {
            return Err(SpecLoopError::Config(
                "check name cannot be empty".to_owned(),
            ));
        }
        if !names.insert(&check.name) {
            return Err(SpecLoopError::Config(format!(
                "duplicate check name {:?}",
                check.name
            )));
        }
        let Some(program) = check.argv.first() else {
            return Err(SpecLoopError::Config(format!(
                "check {:?} must have a non-empty argv",
                check.name
            )));
        };
        if program.trim().is_empty() {
            return Err(SpecLoopError::Config(format!(
                "check {:?} must have a non-empty argv",
                check.name
            )));
        }
        if program.contains('{') || program.contains('}') {
            return Err(SpecLoopError::Config(format!(
                "check {:?} executable argv[0] cannot contain placeholders",
                check.name
            )));
        }
        if check.timeout_seconds == Some(0) {
            return Err(SpecLoopError::Config(format!(
                "check {:?} timeout_seconds must be positive when set",
                check.name
            )));
        }
        validate_relative_path(&check.cwd, "check.cwd")?;
        for argument in &check.argv {
            if argument.contains('\0') {
                return Err(SpecLoopError::Config(format!(
                    "check {:?} argv contains a NUL byte",
                    check.name
                )));
            }
            validate_placeholders(argument, "check argv")?;
        }
    }
    Ok(())
}

fn validate_resource_paths(prompts: &PromptConfig) -> Result<()> {
    for path in prompts
        .plan_author
        .iter()
        .chain(&prompts.plan_review)
        .chain(&prompts.plan_review_status_audit)
        .chain(&prompts.plan_audit)
        .chain(&prompts.implementation)
        .chain(&prompts.review)
        .chain(&prompts.review_status_audit)
        .chain(&prompts.review_audit)
    {
        validate_relative_path(path, "profile resource path")?;
    }
    Ok(())
}

fn validate_completion(completion: &CompletionConfig, acceptance: &AcceptanceConfig) -> Result<()> {
    if completion.commit_message.trim().is_empty() || completion.commit_message.contains('\0') {
        return Err(SpecLoopError::Config(
            "completion.commit_message cannot be empty or contain NUL".to_owned(),
        ));
    }
    validate_placeholders(&completion.commit_message, "completion.commit_message")?;
    if completion.commit_message.contains(SPEC_PATH_PLACEHOLDER)
        || completion.commit_message.contains(PLAN_PATH_PLACEHOLDER)
    {
        return Err(SpecLoopError::Config(
            "completion.commit_message only supports `{spec_id}`".to_owned(),
        ));
    }
    for gate in &acceptance.manual_gates {
        if gate.trim().is_empty() {
            return Err(SpecLoopError::Config(
                "acceptance.manual_gates cannot contain empty names".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_placeholders(value: &str, field: &str) -> Result<()> {
    const ALLOWED: [&str; 3] = [
        SPEC_ID_PLACEHOLDER,
        SPEC_PATH_PLACEHOLDER,
        PLAN_PATH_PLACEHOLDER,
    ];
    let mut remaining = value;
    while let Some(opening) = remaining.find('{') {
        if remaining
            .get(..opening)
            .is_some_and(|prefix| prefix.contains('}'))
        {
            return Err(SpecLoopError::Config(format!(
                "unmatched closing brace in {field}"
            )));
        }
        let placeholder = remaining
            .get(opening..)
            .ok_or_else(|| SpecLoopError::Config(format!("invalid placeholder in {field}")))?;
        let closing = placeholder
            .find('}')
            .ok_or_else(|| SpecLoopError::Config(format!("unclosed placeholder in {field}")))?;
        let token = placeholder
            .get(..=closing)
            .ok_or_else(|| SpecLoopError::Config(format!("invalid placeholder in {field}")))?;
        if !ALLOWED.contains(&token) {
            return Err(SpecLoopError::Config(format!(
                "unsupported placeholder {token:?} in {field}"
            )));
        }
        remaining = placeholder
            .get(closing + 1..)
            .ok_or_else(|| SpecLoopError::Config(format!("invalid placeholder in {field}")))?;
    }
    if remaining.contains('}') {
        return Err(SpecLoopError::Config(format!(
            "unmatched closing brace in {field}"
        )));
    }
    Ok(())
}

pub fn validate_relative_path(path: &Path, field: &str) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() || path.to_string_lossy().contains('\0') {
        return Err(SpecLoopError::Config(format!(
            "{field} must be a non-empty repository-relative path"
        )));
    }
    Ok(())
}

fn validate_template_path(template: &str, field: &str) -> Result<()> {
    let path = Path::new(template);
    validate_relative_path(path, field)?;
    validate_placeholders(template, field)?;
    if template.contains(PLAN_PATH_PLACEHOLDER) {
        return Err(SpecLoopError::Config(format!(
            "{field} cannot contain `{PLAN_PATH_PLACEHOLDER}`"
        )));
    }
    if !template.contains(SPEC_ID_PLACEHOLDER) {
        return Err(SpecLoopError::Config(format!(
            "{field} must contain `{{spec_id}}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Profile;

    #[test]
    fn documented_profiles_parse_and_validate() {
        for example in [
            include_str!("../examples/profiles/basic.toml"),
            include_str!("../examples/profiles/planned-archive.toml"),
            include_str!("../.spec-loop.toml"),
        ] {
            let profile: Profile = toml::from_str(example).expect("valid example TOML");
            assert!(profile.validate().is_ok());
        }
    }

    #[test]
    fn rejects_pi_resource_overrides_in_profile() {
        for (name, value) in [
            ("trust_project", "false"),
            ("allow_mcp", "false"),
            ("extensions", "[]"),
            ("skills", "[]"),
            ("writer_tools", "['read']"),
        ] {
            let profile = format!(
                "schema_version = 1\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\n{name} = {value}\n"
            );
            assert!(
                toml::from_str::<Profile>(&profile).is_err(),
                "agent.{name} unexpectedly accepted"
            );
        }
    }

    #[test]
    fn rejects_unknown_profile_fields() {
        let result = toml::from_str::<Profile>(
            "schema_version = 1\nunknown = true\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n",
        );
        assert!(result.is_err());
    }

    #[test]
    fn validates_pi_thinking_levels_for_global_and_stage_settings() {
        for level in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let profile: Profile = toml::from_str(&format!(
                "schema_version = 1\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nthinking = {level:?}\n"
            ))
            .expect("parse supported thinking level");
            assert!(profile.validate().is_ok(), "thinking level {level:?}");
        }

        for (section, field) in [
            ("[agent]", "agent.thinking"),
            ("[agent.stages.planning]", "agent.stages.planning.thinking"),
            (
                "[agent.stages.implementation]",
                "agent.stages.implementation.thinking",
            ),
            ("[agent.stages.review]", "agent.stages.review.thinking"),
        ] {
            let profile: Profile = toml::from_str(&format!(
                "schema_version = 1\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n{section}\nthinking = 'unsupported'\n"
            ))
            .expect("parse unsupported thinking level for validation");
            let error = profile
                .validate()
                .expect_err("unsupported thinking level must fail validation");
            assert!(error.to_string().contains(field));
            assert!(error.to_string().contains("unsupported"));
        }
    }

    #[test]
    fn rejects_unmatched_closing_brace_before_placeholder() {
        for value in ["}{spec_id}", "prefix}{spec_id}"] {
            let profile: Profile = toml::from_str(&format!(
                "schema_version = 1\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[[checks]]\nname = 'placeholder test'\nargv = ['printf', {value:?}]\n"
            ))
            .expect("parse malformed placeholder profile");
            assert!(
                profile
                    .validate()
                    .is_err_and(|error| error.to_string().contains("unmatched closing brace"))
            );
        }
    }

    #[test]
    fn minimal_profile_uses_engine_owned_complete_patch_staging() {
        let profile: Profile = toml::from_str(
            "schema_version = 1\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n",
        )
        .expect("parse minimal profile");
        profile.validate().expect("validate minimal profile");
        assert_eq!(profile.completion.max_passes, 0);
    }
}
