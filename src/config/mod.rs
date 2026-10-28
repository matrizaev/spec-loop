use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::error::{Result, SpecLoopError, io_error};

#[cfg(test)]
mod tests;
const SCHEMA_VERSION: u32 = 3;
const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 1_048_576;
const SPEC_ID_PLACEHOLDER: &str = "{spec_id}";
const SPEC_PATH_PLACEHOLDER: &str = "{spec_path}";
const PLAN_PATH_PLACEHOLDER: &str = "{plan_path}";

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
    pub review: ReviewConfig,
    #[serde(default)]
    pub actions: ActionsConfig,
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
    pub plan_audit: Vec<PathBuf>,
    pub plan_review_clarification: Vec<PathBuf>,
    pub implementation: Vec<PathBuf>,
    pub review: Vec<PathBuf>,
    pub review_audit: Vec<PathBuf>,
    pub review_clarification: Vec<PathBuf>,
}

/// One argv-only configured check command.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfig {
    pub name: String,
    pub argv: Vec<String>,
    pub timeout_seconds: Option<u64>,
}

/// Review-context command delegated to selected-backend tools, never run by the engine.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    pub context: ReviewContextConfig,
}

/// Argv supplied to implementation review and grounding roles.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewContextConfig {
    pub argv: Vec<String>,
}

impl Default for ReviewContextConfig {
    fn default() -> Self {
        Self {
            argv: vec!["git".to_owned(), "diff".to_owned()],
        }
    }
}

/// Ordered user-authorized lifecycle command lists.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionsConfig {
    pub on_review_success: Vec<ActionConfig>,
    pub on_finish: Vec<ActionConfig>,
}

/// One argv-only lifecycle command.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionConfig {
    pub name: String,
    pub argv: Vec<String>,
    pub timeout_seconds: Option<u64>,
}

/// Selects built-in `OpenAI` or explicitly configured Pi agent execution.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendKind {
    /// ChatGPT-authorized `OpenAI` Responses API harness.
    #[default]
    Builtin,
    /// Pi CLI, using its effective user configuration and resources.
    Pi,
}

/// Reasoning level accepted by Pi and mapped to `OpenAI` Responses effort.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Disable extended reasoning.
    Off,
    /// Use minimal reasoning effort.
    Minimal,
    /// Use low reasoning effort.
    Low,
    /// Use medium reasoning effort.
    Medium,
    /// Use high reasoning effort.
    High,
    /// Use extra-high reasoning effort.
    Xhigh,
    /// Use maximum reasoning effort.
    Max,
}

impl ThinkingLevel {
    /// Returns the Pi spelling and `OpenAI` Responses effort setting, in that order.
    pub const fn provider_values(self) -> (&'static str, &'static str) {
        match self {
            Self::Off => ("off", "none"),
            Self::Minimal => ("minimal", "minimal"),
            Self::Low => ("low", "low"),
            Self::Medium => ("medium", "medium"),
            Self::High => ("high", "high"),
            Self::Xhigh => ("xhigh", "xhigh"),
            Self::Max => ("max", "max"),
        }
    }
}

/// Agent settings shared by supported backends.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub backend: AgentBackendKind,
    pub executable: PathBuf,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    /// Optional user-provided built-in model context limit.
    pub context_window_tokens: Option<u64>,
    pub stages: AgentStages,
    pub timeout_seconds: Option<u64>,
    pub max_tool_calls: Option<usize>,
    pub max_tool_output_bytes: Option<usize>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            backend: AgentBackendKind::default(),
            executable: PathBuf::from("pi"),
            model: None,
            thinking: None,
            context_window_tokens: None,
            stages: AgentStages::default(),
            timeout_seconds: None,
            max_tool_calls: None,
            max_tool_output_bytes: None,
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
    pub thinking: Option<ThinkingLevel>,
    /// Optional user-provided context limit for the stage's selected model.
    pub context_window_tokens: Option<u64>,
}

/// Role family used to resolve model and thinking settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStage {
    Planning,
    Implementation,
    Review,
}

impl AgentConfig {
    /// Returns configured or specification-default retained output bytes per shell call.
    pub const fn max_tool_output_bytes(&self) -> usize {
        match self.max_tool_output_bytes {
            Some(value) => value,
            None => DEFAULT_MAX_TOOL_OUTPUT_BYTES,
        }
    }

    pub fn model_for(&self, stage: AgentStage) -> Option<&str> {
        self.stage_config(stage)
            .model
            .as_deref()
            .or(self.model.as_deref())
    }

    pub fn thinking_for(&self, stage: AgentStage) -> Option<ThinkingLevel> {
        self.stage_config(stage).thinking.or(self.thinking)
    }

    /// Returns the user-provided context limit applicable to the selected stage model.
    ///
    /// A stage limit always applies to its stage. A global limit applies only when
    /// the stage inherits the global model, because a stage model override may have
    /// a different context window.
    pub fn context_window_tokens_for(&self, stage: AgentStage) -> Option<u64> {
        let stage_config = self.stage_config(stage);
        stage_config.context_window_tokens.or_else(|| {
            stage_config
                .model
                .is_none()
                .then_some(self.context_window_tokens)
                .flatten()
        })
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

/// Retry policy for agent roles.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompletionConfig {
    pub max_passes: usize,
}

impl Profile {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|source| io_error("read profile", path, source))?;
        let document: toml::Value = toml::from_str(&contents)
            .map_err(|error| SpecLoopError::Config(format!("{}: {error}", path.display())))?;
        if let Some(version @ (1 | 2)) = document
            .get("schema_version")
            .and_then(toml::Value::as_integer)
        {
            return Err(SpecLoopError::Config(format!(
                "{} uses profile schema v{version}; migrate manually to schema_version = 3 (no automatic migration is available). Set agent.backend = \"pi\" under [agent] to preserve pre-v3 backend behavior; the built-in backend requires agent.model",
                path.display()
            )));
        }
        let profile: Self = document
            .try_into()
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
        validate_actions(&self.actions)?;
        validate_review_context(&self.review.context)?;
        validate_resource_paths(&self.prompts)
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
    for (field, value) in [
        ("agent.context_window_tokens", agent.context_window_tokens),
        (
            "agent.stages.planning.context_window_tokens",
            agent.stages.planning.context_window_tokens,
        ),
        (
            "agent.stages.implementation.context_window_tokens",
            agent.stages.implementation.context_window_tokens,
        ),
        (
            "agent.stages.review.context_window_tokens",
            agent.stages.review.context_window_tokens,
        ),
    ] {
        if value == Some(0) {
            return Err(SpecLoopError::Config(format!(
                "{field} must be positive when set"
            )));
        }
        if agent.backend == AgentBackendKind::Pi && value.is_some() {
            return Err(SpecLoopError::Config(format!(
                "{field} is only valid for the built-in backend"
            )));
        }
    }
    match agent.backend {
        AgentBackendKind::Builtin => {
            if agent.model.as_deref().is_none_or(str::is_empty) {
                return Err(SpecLoopError::Config(
                    "agent.model is required and must be non-empty when agent.backend = \"builtin\""
                        .to_owned(),
                ));
            }
            if agent.max_tool_calls == Some(0) {
                return Err(SpecLoopError::Config(
                    "agent.max_tool_calls must be positive when set".to_owned(),
                ));
            }
            if agent.max_tool_output_bytes() == 0 {
                return Err(SpecLoopError::Config(
                    "agent.max_tool_output_bytes must be positive when set".to_owned(),
                ));
            }
        }
        AgentBackendKind::Pi => {
            if agent.executable.as_os_str().is_empty()
                || agent.executable.to_string_lossy().contains('\0')
            {
                return Err(SpecLoopError::Config(
                    "agent.executable cannot be empty or contain NUL".to_owned(),
                ));
            }
            if agent.max_tool_calls.is_some() || agent.max_tool_output_bytes.is_some() {
                return Err(SpecLoopError::Config(
                    "agent.max_tool_calls and agent.max_tool_output_bytes are only valid for the built-in backend"
                        .to_owned(),
                ));
            }
        }
    }
    if agent.timeout_seconds == Some(0) {
        return Err(SpecLoopError::Config(
            "agent.timeout_seconds must be positive when set".to_owned(),
        ));
    }
    if agent.timeout_seconds.is_some_and(|seconds| {
        Instant::now()
            .checked_add(Duration::from_secs(seconds))
            .is_none()
    }) {
        return Err(SpecLoopError::Config(
            "agent.timeout_seconds exceeds the supported monotonic clock range".to_owned(),
        ));
    }
    for (field, value) in [
        ("agent.model", &agent.model),
        ("agent.stages.planning.model", &agent.stages.planning.model),
        (
            "agent.stages.implementation.model",
            &agent.stages.implementation.model,
        ),
        ("agent.stages.review.model", &agent.stages.review.model),
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
        let field = format!("check {:?} argv", check.name);
        validate_argv(&check.argv, &field, false)?;
        if check.timeout_seconds == Some(0) {
            return Err(SpecLoopError::Config(format!(
                "check {:?} timeout_seconds must be positive when set",
                check.name
            )));
        }
    }
    Ok(())
}

fn validate_resource_paths(prompts: &PromptConfig) -> Result<()> {
    for path in prompts
        .plan_author
        .iter()
        .chain(&prompts.plan_review)
        .chain(&prompts.plan_audit)
        .chain(&prompts.plan_review_clarification)
        .chain(&prompts.implementation)
        .chain(&prompts.review)
        .chain(&prompts.review_audit)
        .chain(&prompts.review_clarification)
    {
        validate_relative_path(path, "profile resource path")?;
    }
    Ok(())
}

fn validate_review_context(context: &ReviewContextConfig) -> Result<()> {
    validate_argv(&context.argv, "review.context.argv", false)
}

fn validate_actions(actions: &ActionsConfig) -> Result<()> {
    for action in actions.on_review_success.iter().chain(&actions.on_finish) {
        if action.name.trim().is_empty() || action.name.contains('\0') {
            return Err(SpecLoopError::Config(
                "action name cannot be empty or contain NUL".to_owned(),
            ));
        }
        validate_argv(&action.argv, "action argv", true)?;
        if action.timeout_seconds == Some(0) {
            return Err(SpecLoopError::Config(format!(
                "action {:?} timeout_seconds must be positive when set",
                action.name
            )));
        }
    }
    Ok(())
}

fn validate_argv(argv: &[String], field: &str, placeholders: bool) -> Result<()> {
    let Some(program) = argv.first() else {
        return Err(SpecLoopError::Config(format!("{field} must be non-empty")));
    };
    if program.trim().is_empty() || program.contains('\0') {
        return Err(SpecLoopError::Config(format!(
            "{field} executable is invalid"
        )));
    }
    for argument in argv {
        if argument.contains('\0') {
            return Err(SpecLoopError::Config(format!(
                "{field} contains a NUL byte"
            )));
        }
        if placeholders {
            validate_placeholders(argument, field)?;
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
