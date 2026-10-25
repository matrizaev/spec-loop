use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regex::Regex;

use crate::backend::{self, AgentBackend, AgentResponse};
use crate::cli::Cli;
use crate::commands::{self, ProcessTermination, status_success};
use crate::config::{CheckConfig, Profile};
use crate::discovery::{self, SpecCandidate};
use crate::error::{Result, SpecLoopError, io_error};
use crate::git::{self, GitIdentity};
use crate::integrity::InputIntegrity;
use crate::paths::{self, expand_template, resolve_existing, resolve_optional};
use crate::prompts::{self, PromptContext, PromptRole};
use crate::run_dir::RunDir;

const PLAN_READY: &str = "SPEC_LOOP_PLAN=READY_FOR_REVIEW";
const IMPLEMENTATION_READY: &str = "SPEC_LOOP_IMPLEMENTATION=READY_FOR_REVIEW";
const PLAN_AUDIT_PREFIX: &str = "SPEC_LOOP_PLAN_AUDIT=";
const REVIEW_AUDIT_PREFIX: &str = "SPEC_LOOP_REVIEW_AUDIT=";
const PLAN_REVIEW_STATUS_AUDIT_PREFIX: &str = "SPEC_LOOP_PLAN_REVIEW_STATUS_AUDIT=";
const REVIEW_STATUS_AUDIT_PREFIX: &str = "SPEC_LOOP_REVIEW_STATUS_AUDIT=";

struct WorkflowContext<'a> {
    root: &'a Path,
    profile: &'a Profile,
    run_dir: &'a RunDir,
    cancelled: Arc<AtomicBool>,
    max_passes: usize,
    role_calls: BTreeMap<PromptRole, usize>,
    commit_requested: bool,
    log_index: usize,
    integrity: InputIntegrity,
    plan_input_identity: Option<GitIdentity>,
    backend: Box<dyn AgentBackend>,
}
#[derive(Clone, Copy)]
struct AgentRequest<'a> {
    candidate: &'a SpecCandidate,
    role: PromptRole,
    plan_path: Option<&'a Path>,
    feedback: Option<&'a str>,
    validation_feedback: Option<&'a str>,
    checks: &'a str,
}

struct CheckReport {
    passed: bool,
    input_changed: bool,
    checked_identity: GitIdentity,
    summary: String,
}

struct AuditReport {
    supported: String,
    unsupported: String,
    ambiguous: String,
    outcome: AuditOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuditOutcome {
    Actionable,
    NoActionableFindings,
    Ambiguous,
    InputChanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusAuditOutcome {
    Issues,
    NoIssues,
    Ambiguous,
}

impl WorkflowContext<'_> {
    fn next_log_name(&mut self, prefix: &str) -> Result<String> {
        self.log_index = self
            .log_index
            .checked_add(1)
            .ok_or_else(|| SpecLoopError::Workflow("run log counter overflowed".to_owned()))?;
        Ok(format!("{prefix}-{:04}.log", self.log_index))
    }
}

pub fn run(cli: &Cli) -> Result<()> {
    let prepared = prepare_run(cli)?;
    if cli.is_dry_run() {
        print_dry_run(
            cli,
            &prepared.root,
            &prepared.profile,
            &prepared.candidate,
            prepared.plan_path.as_deref(),
            prepared.archive_path.as_deref(),
        )?;
        return Ok(());
    }
    let PreparedRun {
        root,
        profile,
        candidate,
        plan_path,
        has_plan,
        archive_path,
        integrity,
    } = prepared;
    let plan_input_identity = if cli.is_plan() {
        let selected_plan = plan_path.as_deref().ok_or_else(|| {
            SpecLoopError::Config("plan mode has no resolved plan path".to_owned())
        })?;
        Some(git::input_identity_excluding(
            &root,
            &[candidate.path.as_path(), selected_plan],
        )?)
    } else {
        None
    };
    let mut candidate = candidate;
    let mut files = CandidateFiles {
        plan_path,
        has_plan,
        archive_path,
    };
    let backend = backend::create(&profile)?;
    let run_dir = RunDir::create(&root)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation_flag = Arc::clone(&cancelled);
    ctrlc::set_handler(move || cancellation_flag.store(true, Ordering::Relaxed))
        .map_err(|error| SpecLoopError::Process(format!("install interrupt handler: {error}")))?;
    let mut context = WorkflowContext {
        root: &root,
        profile: &profile,
        run_dir: &run_dir,
        cancelled,
        max_passes: profile.completion.max_passes,
        role_calls: BTreeMap::new(),
        commit_requested: cli.commit_requested(),
        log_index: 0,
        integrity,
        plan_input_identity,
        backend,
    };
    println!("Run logs: {}", run_dir.root.display());
    if cli.is_plan() {
        run_plan_workflow(&mut context, cli, candidate, files)?;
        return Ok(());
    }
    run_candidate_loop(&mut context, cli, &mut candidate, &mut files)?;
    println!("Run logs retained at {}", run_dir.root.display());
    Ok(())
}

struct PreparedRun {
    root: PathBuf,
    profile: Profile,
    candidate: SpecCandidate,
    plan_path: Option<PathBuf>,
    has_plan: bool,
    archive_path: Option<PathBuf>,
    integrity: InputIntegrity,
}

struct CandidateFiles {
    plan_path: Option<PathBuf>,
    has_plan: bool,
    archive_path: Option<PathBuf>,
}

fn prepare_run(cli: &Cli) -> Result<PreparedRun> {
    cli.validate()?;
    let current = std::env::current_dir()
        .map_err(|source| io_error("resolve current directory", ".", source))?;
    let root = git::discover_root(&current)?;
    git::require_head(&root)?;
    let profile_path = root.join(".spec-loop.toml");
    let profile = Profile::load(&profile_path)?;
    validate_resources(&root, &profile)?;
    validate_startup_git_state(&root, cli)?;
    let candidate = discovery::select_spec(&root, &profile, cli.spec.as_deref(), &BTreeSet::new())?
        .ok_or_else(|| SpecLoopError::Workflow("no eligible specification found".to_owned()))?;
    let files =
        prepare_candidate_files(&root, &profile, &candidate, cli.is_plan(), cli.is_review())?;
    let plan_path = files.plan_path.as_deref().ok_or_else(|| {
        SpecLoopError::Config("selected spec has no resolved plan path".to_owned())
    })?;
    let plan_author_may_update = cli.is_plan() && !(cli.continue_requested() && files.has_plan);
    let integrity =
        InputIntegrity::capture(&root, &candidate.path, plan_path, plan_author_may_update)?;
    Ok(PreparedRun {
        root,
        profile,
        candidate,
        plan_path: files.plan_path,
        has_plan: files.has_plan,
        archive_path: files.archive_path,
        integrity,
    })
}

fn prepare_candidate_files(
    root: &Path,
    profile: &Profile,
    candidate: &SpecCandidate,
    plan_mode: bool,
    review_resume: bool,
) -> Result<CandidateFiles> {
    let plan_path = Some(prompts::plan_path(
        root,
        profile,
        &candidate.id,
        &root.join(&candidate.path),
    )?);
    let has_plan = plan_exists(root, plan_path.as_deref())?;
    if !plan_mode && !has_plan {
        let displayed = plan_path
            .as_deref()
            .map_or_else(|| "<unknown>".to_owned(), |path| path.display().to_string());
        return Err(SpecLoopError::Config(format!(
            "required plan is missing: {}; run `spec-loop --spec {} --plan`",
            displayed, candidate.id
        )));
    }
    let archive_path = if review_resume {
        None
    } else {
        let destination = archive_destination(root, profile, candidate)?;
        if plan_mode { None } else { destination }
    };
    Ok(CandidateFiles {
        plan_path,
        has_plan,
        archive_path,
    })
}

fn run_plan_workflow(
    context: &mut WorkflowContext<'_>,
    cli: &Cli,
    mut candidate: SpecCandidate,
    mut files: CandidateFiles,
) -> Result<()> {
    let mut completed = BTreeSet::new();
    let mut found_start = false;
    for previous in discovery::discover_specs(context.root, context.profile)? {
        if previous.id == candidate.id {
            found_start = true;
            break;
        }
        completed.insert(previous.id);
    }
    if !found_start {
        return Err(SpecLoopError::Workflow(format!(
            "selected spec {} is no longer eligible for plan continuation",
            candidate.id
        )));
    }
    loop {
        if cli.continue_requested() && files.has_plan {
            context.integrity.verify(context.root)?;
            println!("Skipping spec {}: plan already exists.", candidate.id);
        } else {
            let plan_path = files.plan_path.as_deref().ok_or_else(|| {
                SpecLoopError::Config("plan mode has no configured plan path".to_owned())
            })?;
            run_plan_loop(context, &candidate, plan_path)?;
            context.integrity.verify(context.root)?;
            validate_plan_side_effects(context, &candidate, plan_path)?;
            println!("Plan approved for spec {}.", candidate.id);
        }
        if !cli.continue_requested() {
            return Ok(());
        }
        completed.insert(candidate.id.clone());
        loop {
            let next = discovery::select_spec(context.root, context.profile, None, &completed)?;
            let Some(next) = next else {
                println!("No further specifications need plans.");
                return Ok(());
            };
            let next_files =
                prepare_candidate_files(context.root, context.profile, &next, true, false)?;
            let next_plan_path = next_files.plan_path.as_deref().ok_or_else(|| {
                SpecLoopError::Config("selected spec has no resolved plan path".to_owned())
            })?;
            context.integrity.verify(context.root)?;
            context.integrity = InputIntegrity::capture(
                context.root,
                &next.path,
                next_plan_path,
                !next_files.has_plan,
            )?;
            context.plan_input_identity = Some(git::input_identity_excluding(
                context.root,
                &[next.path.as_path(), next_plan_path],
            )?);
            let has_plan = next_files.has_plan;
            candidate = next;
            files = next_files;
            if !has_plan {
                break;
            }
            println!("Skipping spec {}: plan already exists.", candidate.id);
            completed.insert(candidate.id.clone());
        }
    }
}

fn run_candidate_loop(
    context: &mut WorkflowContext<'_>,
    cli: &Cli,
    candidate: &mut SpecCandidate,
    files: &mut CandidateFiles,
) -> Result<()> {
    let mut completed = BTreeSet::new();
    if cli.continue_requested() {
        for previous in discovery::discover_specs(context.root, context.profile)? {
            if previous.id == candidate.id {
                break;
            }
            completed.insert(previous.id);
        }
    }
    let mut review_resume = cli.is_review();
    loop {
        let completed_current = run_implementation_workflow(
            context,
            candidate,
            files.plan_path.as_deref(),
            files.archive_path.as_deref(),
            review_resume,
        )?;
        if completed_current == CompletionOutcome::PendingManualGates {
            return Ok(());
        }
        completed.insert(candidate.id.clone());
        if !cli.continue_requested() {
            return Ok(());
        }
        let next = discovery::select_spec(context.root, context.profile, None, &completed)?;
        let Some(next) = next else {
            println!("No further eligible specifications.");
            return Ok(());
        };
        let next_files =
            prepare_candidate_files(context.root, context.profile, &next, false, false)?;
        let next_plan_path = next_files.plan_path.as_deref().ok_or_else(|| {
            SpecLoopError::Config("selected spec has no resolved plan path".to_owned())
        })?;
        context.integrity =
            InputIntegrity::capture(context.root, &next.path, next_plan_path, false)?;
        *candidate = next;
        *files = next_files;
        review_resume = false;
    }
}

fn validate_resources(root: &Path, profile: &Profile) -> Result<()> {
    for path in profile
        .prompts
        .plan_author
        .iter()
        .chain(&profile.prompts.plan_review)
        .chain(&profile.prompts.plan_review_status_audit)
        .chain(&profile.prompts.plan_audit)
        .chain(&profile.prompts.implementation)
        .chain(&profile.prompts.review)
        .chain(&profile.prompts.review_status_audit)
        .chain(&profile.prompts.review_audit)
    {
        paths::validate_regular_file(root, path, "prompts")?;
    }
    validate_agent_executable(root, &profile.agent.executable)?;
    for check in &profile.checks {
        let cwd = resolve_existing(root, &check.cwd, "check.cwd")?;
        if !cwd.is_dir() {
            return Err(SpecLoopError::Config(format!(
                "check {:?} cwd is not a directory: {}",
                check.name,
                cwd.display()
            )));
        }
        let executable = check.argv.first().ok_or_else(|| {
            SpecLoopError::Config(format!("check {:?} has an empty argv", check.name))
        })?;
        validate_executable(
            &cwd,
            Path::new(executable),
            &format!("check {:?}", check.name),
        )?;
    }
    Ok(())
}

fn validate_agent_executable(root: &Path, executable: &Path) -> Result<()> {
    validate_executable(root, executable, "agent")
}

fn validate_executable(cwd: &Path, executable: &Path, field: &str) -> Result<()> {
    let has_path_component = executable.is_absolute()
        || executable
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty());
    let candidates = if has_path_component {
        vec![if executable.is_absolute() {
            executable.to_path_buf()
        } else {
            cwd.join(executable)
        }]
    } else {
        let search_path = std::env::var_os("PATH").ok_or_else(|| {
            SpecLoopError::Config(format!(
                "PATH is unavailable while resolving {field} executable"
            ))
        })?;
        std::env::split_paths(&search_path)
            .map(|directory| {
                if directory.is_absolute() {
                    directory.join(executable)
                } else {
                    cwd.join(directory).join(executable)
                }
            })
            .collect()
    };
    if candidates.iter().any(|path| is_executable_file(path)) {
        return Ok(());
    }
    Err(SpecLoopError::Config(format!(
        "{field} executable was not found or is not executable: {}",
        executable.display()
    )))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn validate_startup_git_state(root: &Path, cli: &Cli) -> Result<()> {
    let status = git::status(root)?;
    let mut has_changes = false;
    let mut records = status.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            return Err(SpecLoopError::Workflow(
                "malformed Git porcelain status record at startup".to_owned(),
            ));
        }
        let index = record[0];
        let worktree = record[1];
        has_changes = true;
        if [index, worktree].contains(&b'R') || [index, worktree].contains(&b'C') {
            match records.next() {
                Some(original_path) if !original_path.is_empty() => {}
                _ => {
                    return Err(SpecLoopError::Workflow(
                        "malformed Git porcelain rename/copy record at startup".to_owned(),
                    ));
                }
            }
        }
    }
    if cli.is_review() && !has_changes {
        return Err(SpecLoopError::Workflow(
            "--review requires a non-empty combined Git-visible patch".to_owned(),
        ));
    }
    Ok(())
}

fn plan_exists(root: &Path, path: Option<&Path>) -> Result<bool> {
    let Some(path) = path else {
        return Ok(false);
    };
    let absolute = paths::resolve_optional(root, path);
    match fs::metadata(&absolute) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(SpecLoopError::Config(format!(
            "plan path is not a regular file: {}",
            absolute.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error("inspect implementation plan", absolute, source)),
    }
}

fn archive_destination(
    root: &Path,
    profile: &Profile,
    candidate: &SpecCandidate,
) -> Result<Option<PathBuf>> {
    if !profile.archive.enabled {
        return Ok(None);
    }
    let directory = profile
        .archive
        .destination
        .as_deref()
        .ok_or_else(|| SpecLoopError::Config("archive destination is missing".to_owned()))?;
    let directory = PathBuf::from(directory);
    crate::config::validate_relative_path(&directory, "archive.destination")?;
    let filename = candidate.path.file_name().ok_or_else(|| {
        SpecLoopError::Config(format!(
            "selected spec path has no filename: {}",
            candidate.path.display()
        ))
    })?;
    let destination = directory.join(filename);
    let destination_text = paths::path_to_utf8(&destination, "archive destination")?;
    let id_regex = Regex::new(&profile.specs.id_regex)
        .map_err(|error| SpecLoopError::Config(format!("specs.id_regex: {error}")))?;
    let destination_id = id_regex
        .captures(&destination_text)
        .and_then(|captures| captures.name("id"))
        .map(|capture| capture.as_str());
    if destination_id != Some(candidate.id.as_str()) {
        return Err(SpecLoopError::Config(format!(
            "archive destination does not identify selected spec ID {:?}: {}",
            candidate.id,
            destination.display()
        )));
    }
    if git::is_ignored(root, &destination)? {
        return Err(SpecLoopError::Config(format!(
            "archive destination is ignored by Git and cannot be reviewed: {}",
            destination.display()
        )));
    }
    let absolute = paths::resolve_optional(root, &destination);
    let parent = absolute.parent().ok_or_else(|| {
        SpecLoopError::Config("archive destination has no parent directory".to_owned())
    })?;
    if !parent.is_dir() {
        return Err(SpecLoopError::Config(format!(
            "archive destination parent directory does not exist: {}",
            parent.display()
        )));
    }
    Ok(Some(destination))
}

fn print_dry_run(
    cli: &Cli,
    root: &Path,
    profile: &Profile,
    candidate: &SpecCandidate,
    plan_path: Option<&Path>,
    archive_path: Option<&Path>,
) -> Result<()> {
    println!("Repository: {}", root.display());
    println!("Profile: {}", root.join(".spec-loop.toml").display());
    println!("Spec: {} ({})", candidate.id, candidate.path.display());
    println!(
        "Mode: {}",
        if cli.is_plan() {
            "plan"
        } else if cli.is_review() {
            "review-resume"
        } else {
            "implementation"
        }
    );
    print_dry_run_spec_policy(root, profile, plan_path, archive_path);
    print_dry_run_agent(profile);
    print_dry_run_resources(root, profile)?;
    print_dry_run_checks(root, profile, candidate, plan_path, archive_path)?;
    print_dry_run_completion(cli, profile);
    Ok(())
}

fn print_dry_run_spec_policy(
    root: &Path,
    profile: &Profile,
    plan_path: Option<&Path>,
    archive_path: Option<&Path>,
) {
    println!("Spec ordering: {:?}", profile.specs.ordering);
    println!("Spec selection: {:?}", profile.specs.selection);
    println!("Active spec patterns: {:?}", profile.specs.active_globs);
    println!(
        "Plan policy: required for implementation; template={}",
        profile.plan.path_template
    );
    if let Some(path) = plan_path {
        let resolved = resolve_optional(root, path);
        println!("Resolved plan path: {}", resolved.display());
    }
    println!(
        "Archive policy: enabled={}, destination directory={}",
        profile.archive.enabled,
        profile
            .archive
            .destination
            .as_deref()
            .unwrap_or("not configured")
    );
    if let Some(path) = archive_path {
        let resolved = resolve_optional(root, path);
        println!("Resolved archive destination: {}", resolved.display());
    }
}

fn print_dry_run_agent(profile: &Profile) {
    println!("Agent:");
    println!("  backend: {}", profile.agent.backend);
    println!("  executable: {}", profile.agent.executable.display());
    println!(
        "  global model: {}",
        profile.agent.model.as_deref().unwrap_or("Pi default")
    );
    println!(
        "  global thinking: {}",
        profile.agent.thinking.as_deref().unwrap_or("Pi default")
    );
    for (name, stage) in [
        ("planning", &profile.agent.stages.planning),
        ("implementation", &profile.agent.stages.implementation),
        ("review", &profile.agent.stages.review),
    ] {
        println!(
            "  {name} model/thinking: {} / {}",
            stage.model.as_deref().unwrap_or("inherit"),
            stage.thinking.as_deref().unwrap_or("inherit")
        );
    }
    println!(
        "  timeout: {}",
        timeout_description(profile.agent.timeout_seconds)
    );
    println!("  Pi resources: inherited from user's Pi configuration for every role");
}

fn print_dry_run_resources(root: &Path, profile: &Profile) -> Result<()> {
    println!("Prompt and instruction resources:");
    print_named_resource_paths(root, "plan author", &profile.prompts.plan_author)?;
    print_named_resource_paths(root, "plan review", &profile.prompts.plan_review)?;
    print_named_resource_paths(
        root,
        "plan review status audit",
        &profile.prompts.plan_review_status_audit,
    )?;
    print_named_resource_paths(root, "plan finding audit", &profile.prompts.plan_audit)?;
    print_named_resource_paths(root, "implementation", &profile.prompts.implementation)?;
    print_named_resource_paths(root, "review", &profile.prompts.review)?;
    print_named_resource_paths(
        root,
        "review status audit",
        &profile.prompts.review_status_audit,
    )?;
    print_named_resource_paths(root, "review finding audit", &profile.prompts.review_audit)
}

fn print_dry_run_checks(
    root: &Path,
    profile: &Profile,
    candidate: &SpecCandidate,
    plan_path: Option<&Path>,
    archive_path: Option<&Path>,
) -> Result<()> {
    if profile.checks.is_empty() {
        println!("No automated checks are configured.");
        return Ok(());
    }
    println!("Configured checks:");
    for check in &profile.checks {
        let spec_path = archive_path.unwrap_or(&candidate.path);
        let argv: Vec<String> = check
            .argv
            .iter()
            .map(|argument| expand_template(argument, &candidate.id, spec_path, plan_path))
            .collect();
        println!(
            "  {}: {:?} (cwd: {}, timeout: {})",
            check.name,
            argv,
            resolve_existing(root, &check.cwd, "check.cwd")?.display(),
            timeout_description(check.timeout_seconds)
        );
    }
    Ok(())
}

fn print_dry_run_completion(cli: &Cli, profile: &Profile) {
    println!(
        "Maximum calls per Pi role: {} (profile; 0 means unlimited)",
        profile.completion.max_passes
    );
    println!("Commit requested: {}", cli.commit_requested());
    println!("Continue requested: {}", cli.continue_requested());
    println!(
        "Commit message template: {}",
        profile.completion.commit_message
    );
    if !profile.acceptance.manual_gates.is_empty() {
        println!("Manual acceptance gates pending after automated approval:");
        for gate in &profile.acceptance.manual_gates {
            println!("  - {gate}");
        }
    }
}

fn print_named_resource_paths(root: &Path, name: &str, paths: &[PathBuf]) -> Result<()> {
    print!("  {name}:");
    if paths.is_empty() {
        println!(" none");
    } else {
        println!();
        print_resource_paths(root, paths)?;
    }
    Ok(())
}

fn print_resource_paths(root: &Path, paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        println!("    none");
        return Ok(());
    }
    for path in paths {
        let resolved = resolve_existing(root, path, "dry-run profile resource")?;
        println!("    {}", resolved.display());
    }
    Ok(())
}

fn timeout_description(timeout_seconds: Option<u64>) -> String {
    timeout_seconds.map_or_else(|| "unlimited".to_owned(), |seconds| format!("{seconds}s"))
}

fn run_plan_loop(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
) -> Result<()> {
    let mut feedback = None;
    loop {
        let response = call_agent(
            context,
            AgentRequest {
                candidate,
                role: PromptRole::PlanAuthor,
                plan_path: Some(plan_path),
                feedback: feedback.as_deref(),
                validation_feedback: None,
                checks: "",
            },
        )?;
        let response = response?;
        if last_line(&response) != Some(PLAN_READY) {
            feedback = Some(response);
            continue;
        }
        if !plan_exists(context.root, Some(plan_path))? {
            feedback = Some(format!(
                "{response}\n\nThe previous response reported readiness, but required plan file was missing: {}. Create or update this file before reporting readiness again.",
                plan_path.display()
            ));
            continue;
        }
        let review_request = AgentRequest {
            candidate,
            role: PromptRole::PlanReview,
            plan_path: Some(plan_path),
            feedback: None,
            validation_feedback: None,
            checks: "",
        };
        let (reviewer, status) = review_with_status(context, review_request, true)?;
        match status {
            ReviewStatus::NoIssues => return Ok(()),
            ReviewStatus::Issues => {}
        }
        let report = audit_findings_with_evidence(context, review_request, &reviewer, true, None)?;
        report_audit_dispositions(&report);
        match report.outcome {
            AuditOutcome::Actionable => feedback = Some(report.supported),
            AuditOutcome::NoActionableFindings => return Ok(()),
            AuditOutcome::Ambiguous => {
                return Err(SpecLoopError::Workflow(format!(
                    "plan review audit is ambiguous; inspect run logs at {}",
                    context.run_dir.root.display()
                )));
            }
            AuditOutcome::InputChanged => {
                return Err(SpecLoopError::Workflow(
                    "plan finding audit unexpectedly checked implementation input identity"
                        .to_owned(),
                ));
            }
        }
    }
}

fn validate_plan_side_effects(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
) -> Result<()> {
    let expected_identity = context.plan_input_identity.as_ref().ok_or_else(|| {
        SpecLoopError::Config("plan-only workflow has no initial Git-visible identity".to_owned())
    })?;
    let actual_identity =
        git::input_identity_excluding(context.root, &[candidate.path.as_path(), plan_path])?;
    if &actual_identity == expected_identity {
        return Ok(());
    }
    println!(
        "Plan role changed other Git-visible files; running configured checks and review without staging or committing."
    );
    loop {
        let checks = run_checks(context, candidate, Some(plan_path))?;
        println!("{}", checks.summary);
        if !checks.passed {
            return Err(SpecLoopError::Workflow(format!(
                "configured checks failed for Git-visible changes during plan mode; changes preserved and no review, staging, or commit performed:\n{}",
                checks.summary
            )));
        }
        if checks.input_changed {
            println!("Git-visible patch changed during checks; rerunning complete check list.");
            continue;
        }
        let review_request = AgentRequest {
            candidate,
            role: PromptRole::Review,
            plan_path: Some(plan_path),
            feedback: None,
            validation_feedback: None,
            checks: &checks.summary,
        };
        let (reviewer, status) = review_with_status(context, review_request, false)?;
        if status == ReviewStatus::NoIssues {
            println!(
                "Git-visible changes passed checks and review; no staging or commit performed."
            );
            return Ok(());
        }
        if git::input_identity(context.root)? != checks.checked_identity {
            println!("Git-visible patch changed during review; rerunning checks and review.");
            continue;
        }
        let report = audit_findings_with_evidence(
            context,
            review_request,
            &reviewer,
            false,
            Some(&checks.checked_identity),
        )?;
        report_audit_dispositions(&report);
        match report.outcome {
            AuditOutcome::Actionable => {
                return Err(SpecLoopError::Workflow(format!(
                    "review found actionable Git-visible changes during plan mode; changes preserved and no staging or commit performed:\n{}",
                    report.supported
                )));
            }
            AuditOutcome::Ambiguous => {
                return Err(SpecLoopError::Workflow(format!(
                    "review of Git-visible changes during plan mode remains ambiguous; changes preserved and no staging or commit performed; inspect run logs at {}",
                    context.run_dir.root.display()
                )));
            }
            AuditOutcome::InputChanged => println!(
                "Git-visible patch changed during finding audit; rerunning checks and review."
            ),
            AuditOutcome::NoActionableFindings => {
                println!(
                    "Git-visible changes passed checks and review; no staging or commit performed."
                );
                return Ok(());
            }
        }
    }
}

fn run_implementation_workflow(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: Option<&Path>,
    archive_path: Option<&Path>,
    mut review_resume: bool,
) -> Result<CompletionOutcome> {
    let mut feedback = None;
    let mut archived = review_resume || archive_path.is_none();
    let mut effective_candidate = candidate.clone();
    loop {
        if !review_resume || feedback.is_some() {
            let response = call_agent(
                context,
                AgentRequest {
                    candidate: &effective_candidate,
                    role: PromptRole::Implementation,
                    plan_path,
                    feedback: feedback.as_deref(),
                    validation_feedback: None,
                    checks: "",
                },
            )?;
            let response = response?;
            if last_line(&response) != Some(IMPLEMENTATION_READY) {
                feedback = Some(response);
                // A review-resume run becomes a normal repair loop after a failed check.
                // The first pass still skips implementation.
                review_resume = false;
                continue;
            }
        }
        if !archived {
            if let Some(path) = archive_path {
                context.integrity.verify(context.root)?;
                archive_spec(context.root, candidate, path)?;
                context.integrity.relocate_spec(context.root, path)?;
                effective_candidate.path = path.to_path_buf();
            }
            archived = true;
        }
        commands::report_progress(&format!(
            "Stage: automated checks for spec {}",
            effective_candidate.id
        ))?;
        let checks = run_checks(context, &effective_candidate, plan_path)?;
        println!("{}", checks.summary);
        if !checks.passed {
            feedback = Some(checks.summary);
            review_resume = false;
            continue;
        }
        if checks.input_changed {
            println!("Git-visible patch changed during checks; rerunning complete check list.");
            feedback = None;
            review_resume = true;
            continue;
        }
        match review_changes(
            context,
            &effective_candidate,
            plan_path,
            &checks.checked_identity,
            &checks.summary,
        )? {
            ReviewOutcome::Approved => return Ok(CompletionOutcome::Completed),
            ReviewOutcome::PendingManualGates => {
                return Ok(CompletionOutcome::PendingManualGates);
            }
            ReviewOutcome::Repair(findings) => {
                feedback = Some(findings);
                review_resume = false;
            }
            ReviewOutcome::Recheck => {
                feedback = None;
                review_resume = true;
            }
        }
    }
}

enum ReviewOutcome {
    Approved,
    PendingManualGates,
    Recheck,
    Repair(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionOutcome {
    Completed,
    PendingManualGates,
}

fn review_changes(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: Option<&Path>,
    checked_identity: &GitIdentity,
    checks: &str,
) -> Result<ReviewOutcome> {
    commands::ensure_not_cancelled(&context.cancelled)?;
    if &git::input_identity(context.root)? != checked_identity {
        println!("Git-visible input changed after checks; rerunning checks before review.");
        return Ok(ReviewOutcome::Recheck);
    }
    let review_request = AgentRequest {
        candidate,
        role: PromptRole::Review,
        plan_path,
        feedback: None,
        validation_feedback: None,
        checks,
    };
    let (reviewer, status) = review_with_status(context, review_request, false)?;
    if status == ReviewStatus::NoIssues {
        return match complete_spec(context, candidate)? {
            CompletionOutcome::Completed => Ok(ReviewOutcome::Approved),
            CompletionOutcome::PendingManualGates => Ok(ReviewOutcome::PendingManualGates),
        };
    }
    if &git::input_identity(context.root)? != checked_identity {
        return Ok(ReviewOutcome::Recheck);
    }
    let report = audit_findings_with_evidence(
        context,
        review_request,
        &reviewer,
        false,
        Some(checked_identity),
    )?;
    report_audit_dispositions(&report);
    match report.outcome {
        AuditOutcome::Actionable => Ok(ReviewOutcome::Repair(report.supported)),
        AuditOutcome::NoActionableFindings => match complete_spec(context, candidate)? {
            CompletionOutcome::Completed => Ok(ReviewOutcome::Approved),
            CompletionOutcome::PendingManualGates => Ok(ReviewOutcome::PendingManualGates),
        },
        AuditOutcome::Ambiguous => Ok(ReviewOutcome::Repair(format!(
            "Finding audit left these findings unproven; request reviewer evidence once, then discard if still unproven:\n{}",
            report.ambiguous
        ))),
        AuditOutcome::InputChanged => Ok(ReviewOutcome::Recheck),
    }
}

fn archive_spec(root: &Path, candidate: &SpecCandidate, destination: &Path) -> Result<()> {
    commands::report_progress(&format!(
        "Stage: archive spec {} ({} -> {})",
        candidate.id,
        candidate.path.display(),
        destination.display()
    ))?;
    let source = root.join(&candidate.path);
    let target = root.join(destination);
    let parent = target
        .parent()
        .ok_or_else(|| SpecLoopError::Workflow("archive destination has no parent".to_owned()))?;
    if !parent.is_dir() {
        return Err(SpecLoopError::Workflow(format!(
            "archive destination parent is not a directory: {}",
            parent.display()
        )));
    }
    fs::rename(&source, &target).map_err(|error| {
        io_error(
            "move spec to archive",
            format!("{} -> {}", source.display(), target.display()),
            error,
        )
    })?;
    println!(
        "Archived spec: {} -> {}",
        candidate.path.display(),
        destination.display()
    );
    Ok(())
}

fn run_checks(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: Option<&Path>,
) -> Result<CheckReport> {
    let mut previous = git::input_identity(context.root)?;
    let mut input_changed = false;
    let mut all_passed = true;
    let mut summary = String::new();
    if context.profile.checks.is_empty() {
        summary.push_str("No automated checks ran (profile has no configured checks).\n");
    } else {
        for check in &context.profile.checks {
            context.integrity.verify(context.root)?;
            let check_result = run_check(context, candidate, plan_path, check);
            context.integrity.verify(context.root)?;
            let output = check_result?;
            all_passed &= output.0;
            summary.push_str(&output.1);
            summary.push('\n');
            let after_check = git::input_identity(context.root)?;
            input_changed |= previous != after_check;
            previous = after_check;
        }
    }
    let after = git::input_identity(context.root)?;
    input_changed |= previous != after;
    Ok(CheckReport {
        passed: all_passed,
        input_changed,
        checked_identity: after,
        summary,
    })
}

fn run_check(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: Option<&Path>,
    check: &CheckConfig,
) -> Result<(bool, String)> {
    commands::report_progress(&format!("Stage: check {}", check.name))?;
    let relative_cwd = if check.cwd == Path::new(".") {
        PathBuf::from(".")
    } else {
        check.cwd.clone()
    };
    let cwd = match resolve_existing(context.root, &relative_cwd, "check.cwd") {
        Ok(cwd) if cwd.is_dir() => cwd,
        Ok(cwd) => {
            return record_check_setup_failure(
                context,
                check,
                &format!("check cwd is not a directory: {}", cwd.display()),
            );
        }
        Err(error) => return record_check_setup_failure(context, check, &error.to_string()),
    };
    let mut args = check
        .argv
        .iter()
        .map(|argument| expand_template(argument, &candidate.id, &candidate.path, plan_path));
    let program =
        PathBuf::from(args.next().ok_or_else(|| {
            SpecLoopError::Config(format!("check {:?} has empty argv", check.name))
        })?);
    let arguments: Vec<String> = args.collect();
    let timeout = check.timeout_seconds.map(Duration::from_secs);
    let mut command_argv = vec![program.display().to_string()];
    command_argv.extend(arguments.iter().cloned());
    commands::report_progress(&format!(
        "Command argv: {command_argv:?}\nWorking directory: {}",
        cwd.display()
    ))?;
    let process_result =
        commands::run_process(&program, &arguments, &cwd, timeout, &context.cancelled);
    let (success, status, termination, stdout, stderr) = match process_result {
        Ok(result) => (
            status_success(&result),
            result
                .status
                .map_or_else(|| "no exit status".to_owned(), |value| value.to_string()),
            Some(result.termination),
            result.stdout,
            result.stderr,
        ),
        Err(SpecLoopError::Interrupted) => return Err(SpecLoopError::Interrupted),
        Err(error) => (
            false,
            "failed to start or monitor".to_owned(),
            None,
            Vec::new(),
            error.to_string().into_bytes(),
        ),
    };
    let mut log_contents = format!(
        "Check: {}\nCommand: {} {:?}\nWorking directory: {}\nStatus: {}{}\n--- stdout ({} bytes) ---\n",
        check.name,
        program.display(),
        arguments,
        cwd.display(),
        status,
        match termination {
            Some(ProcessTermination::TimedOut) => " (timed out)",
            Some(ProcessTermination::Interrupted) => " (interrupted)",
            Some(ProcessTermination::Exited) | None => "",
        },
        stdout.len(),
    )
    .into_bytes();
    log_contents.extend_from_slice(&stdout);
    log_contents
        .extend_from_slice(format!("\n--- stderr ({} bytes) ---\n", stderr.len()).as_bytes());
    log_contents.extend_from_slice(&stderr);
    let log_name = context.next_log_name("check")?;
    let log_path = context.run_dir.write_log(&log_name, &log_contents)?;
    if termination == Some(ProcessTermination::Interrupted) {
        return Err(SpecLoopError::Interrupted);
    }
    let summary = check_summary(
        &check.name,
        &format!("{} {:?}", program.display(), arguments),
        &status,
        match termination {
            Some(ProcessTermination::TimedOut) => Some("timed out"),
            Some(ProcessTermination::Interrupted) => Some("interrupted"),
            Some(ProcessTermination::Exited) | None => None,
        },
        &log_path,
        &stdout,
        &stderr,
    );
    println!(
        "Check {}: {} (log: {})",
        check.name,
        if success { "passed" } else { "FAILED" },
        log_path.display()
    );
    Ok((success, summary))
}

fn record_check_setup_failure(
    context: &mut WorkflowContext<'_>,
    check: &CheckConfig,
    reason: &str,
) -> Result<(bool, String)> {
    let diagnostic = format!(
        "Check: {}\nCommand: {:?}\nStatus: failed before execution\nWorking directory: {}\nReason: {reason}\n",
        check.name,
        check.argv,
        check.cwd.display()
    );
    let log_name = context.next_log_name("check")?;
    let log_path = context
        .run_dir
        .write_log(&log_name, diagnostic.as_bytes())?;
    let summary = check_summary(
        &check.name,
        &format!("{:?}", check.argv),
        "failed before execution",
        None,
        &log_path,
        &[],
        reason.as_bytes(),
    );
    println!("Check {}: FAILED (log: {})", check.name, log_path.display());
    Ok((false, summary))
}

fn check_summary(
    name: &str,
    command: &str,
    status: &str,
    termination: Option<&str>,
    log_path: &Path,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let mut summary = format!(
        "Check: {name}\nCommand: {command}\nStatus: {status}{}\nLog: {}\n",
        termination.map_or_else(String::new, |reason| format!(" ({reason})")),
        log_path.display()
    );
    append_output_excerpt(&mut summary, "stdout", stdout);
    append_output_excerpt(&mut summary, "stderr", stderr);
    summary
}

fn append_output_excerpt(summary: &mut String, stream: &str, output: &[u8]) {
    const MAX_LINES: usize = 4;
    const MAX_LINE_CHARS: usize = 300;

    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let omitted = lines.len().saturating_sub(MAX_LINES);
    if lines.is_empty() {
        return;
    }
    summary.push_str(stream);
    summary.push_str(" excerpt:\n");
    if omitted > 0 {
        summary.push_str("... ");
        summary.push_str(&omitted.to_string());
        summary.push_str(" earlier lines omitted ...\n");
    }
    for line in lines.iter().skip(omitted) {
        let mut characters = line.chars();
        let excerpt: String = characters.by_ref().take(MAX_LINE_CHARS).collect();
        summary.push_str(&excerpt);
        if characters.next().is_some() {
            summary.push_str(" ... [line truncated]");
        }
        summary.push('\n');
    }
}

fn complete_spec(
    context: &WorkflowContext<'_>,
    candidate: &SpecCandidate,
) -> Result<CompletionOutcome> {
    context.integrity.verify(context.root)?;
    println!("Review approved for spec {}.", candidate.id);
    commands::report_progress(&format!(
        "Stage: stage approved patch for spec {}\nCommand argv: [\"git\", \"add\", \"--all\", \"--\"]\nWorking directory: {}",
        candidate.id,
        context.root.display()
    ))?;
    let stage_result = git::stage_all(context.root, &context.cancelled);
    context.integrity.verify(context.root)?;
    stage_result?;
    for gate in &context.profile.acceptance.manual_gates {
        println!("Manual acceptance gate pending: {gate}");
    }
    if !context.profile.acceptance.manual_gates.is_empty() {
        println!(
            "Manual gates remain user-owned; completion is paused and no commit or continuation will occur."
        );
        println!("Staged changes are preserved for the user-owned gates.");
        return Ok(CompletionOutcome::PendingManualGates);
    }
    if !context.commit_requested {
        println!("Commit not requested; staged changes preserved.");
        return Ok(CompletionOutcome::Completed);
    }
    let message = context
        .profile
        .completion
        .commit_message
        .replace("{spec_id}", &candidate.id);
    commands::report_progress(&format!(
        "Stage: commit spec {}\nCommand argv: [\"git\", \"commit\", \"-m\", {message:?}]\nWorking directory: {}",
        candidate.id,
        context.root.display()
    ))?;
    context.integrity.verify(context.root)?;
    let commit_result = git::commit(context.root, &message, &context.cancelled);
    match commit_result {
        Ok(()) => println!("Committed spec {}: {message}", candidate.id),
        Err(error) => {
            context.integrity.verify(context.root)?;
            return Err(error);
        }
    }
    context.integrity.verify(context.root)?;
    Ok(CompletionOutcome::Completed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewStatus {
    Issues,
    NoIssues,
}

fn review_with_status(
    context: &mut WorkflowContext<'_>,
    request: AgentRequest<'_>,
    plan: bool,
) -> Result<(String, ReviewStatus)> {
    let (issues, no_issues, audit_role, audit_prefix) = if plan {
        (
            "SPEC_LOOP_PLAN_REVIEW=ISSUES",
            "SPEC_LOOP_PLAN_REVIEW=NO_ISSUES",
            PromptRole::PlanReviewStatusAudit,
            PLAN_REVIEW_STATUS_AUDIT_PREFIX,
        )
    } else {
        (
            "SPEC_LOOP_REVIEW=ISSUES",
            "SPEC_LOOP_REVIEW=NO_ISSUES",
            PromptRole::ReviewStatusAudit,
            REVIEW_STATUS_AUDIT_PREFIX,
        )
    };
    let mut reviewer_feedback = request.feedback.map(str::to_owned);
    let mut validation_feedback = None;
    let mut clarification_requested = false;
    loop {
        let review_request = AgentRequest {
            feedback: reviewer_feedback.as_deref(),
            validation_feedback: validation_feedback.as_deref(),
            ..request
        };
        let response = call_agent(context, review_request)??;
        if let Some(status) = canonical_review_status(&response, issues, no_issues) {
            if let Err(error) =
                validate_canonical_review_response(&response, issues, no_issues, status)
            {
                reviewer_feedback = Some(response.clone());
                validation_feedback = Some(format!(
                    "Your previous response failed format validation: {error}. Correct the response and retain exact finding identifiers."
                ));
                continue;
            }
            return Ok((response, status));
        }
        if let Err(error) = parse_reviewer_findings(&response) {
            reviewer_feedback = Some(response.clone());
            validation_feedback = Some(format!(
                "Your previous response had malformed or non-sequential findings: {error}. Correct the finding format and retain only sequential FINDING-N entries."
            ));
            continue;
        }
        let audit_request = AgentRequest {
            role: audit_role,
            feedback: Some(&response),
            validation_feedback: None,
            ..review_request
        };
        let (_, audit_status) = call_agent_parsed(context, audit_request, |audit_response| {
            let status = parse_status_audit_response(audit_response, audit_prefix)?;
            match status {
                StatusAuditOutcome::Issues if parse_reviewer_findings(&response)?.is_empty() => {
                    Err(SpecLoopError::Agent(
                        "status auditor classified ISSUES but reviewer supplied no structured findings".to_owned(),
                    ))
                }
                StatusAuditOutcome::NoIssues
                    if response.lines().any(|line| line.starts_with("FINDING-")) =>
                {
                    Err(SpecLoopError::Agent(
                        "status auditor classified NO_ISSUES despite structured findings".to_owned(),
                    ))
                }
                _ => Ok(status),
            }
        })?;
        match audit_status {
            StatusAuditOutcome::Issues => return Ok((response, ReviewStatus::Issues)),
            StatusAuditOutcome::NoIssues => return Ok((response, ReviewStatus::NoIssues)),
            StatusAuditOutcome::Ambiguous if !clarification_requested => {
                reviewer_feedback = Some(response);
                validation_feedback = Some(
                    "Status audit could not determine your intended review status. Clarify whether actionable findings exist. Preserve exact finding IDs and end with the canonical status marker.".to_owned(),
                );
                clarification_requested = true;
            }
            StatusAuditOutcome::Ambiguous => {
                return Err(SpecLoopError::Workflow(format!(
                    "review status remains ambiguous after reviewer clarification; response preserved:\n{response}"
                )));
            }
        }
    }
}

fn call_agent_parsed<T>(
    context: &mut WorkflowContext<'_>,
    request: AgentRequest<'_>,
    mut parse: impl FnMut(&str) -> Result<T>,
) -> Result<(String, T)> {
    let mut validation_feedback: Option<String> = None;
    loop {
        let combined_feedback = match (request.validation_feedback, validation_feedback.as_deref())
        {
            (Some(base), Some(retry)) => Some(format!("{base}\n\n{retry}")),
            (Some(base), None) => Some(base.to_owned()),
            (None, Some(retry)) => Some(retry.to_owned()),
            (None, None) => None,
        };
        let attempt_request = AgentRequest {
            validation_feedback: combined_feedback.as_deref(),
            ..request
        };
        let response = call_agent(context, attempt_request)??;
        match parse(&response) {
            Ok(parsed) => return Ok((response, parsed)),
            Err(error) => {
                commands::report_progress(&format!(
                    "Agent {} response failed validation ({error}); retrying with format feedback.",
                    role_name(request.role)
                ))?;
                validation_feedback = Some(format!(
                    "Previous complete response failed validation: {error}\n--- Previous response ---\n{response}\n--- End previous response ---\nReturn a corrected response in the exact required format."
                ));
            }
        }
    }
}

fn validate_canonical_review_response(
    response: &str,
    issues_marker: &str,
    no_issues_marker: &str,
    status: ReviewStatus,
) -> Result<()> {
    let expected_marker = match status {
        ReviewStatus::Issues => issues_marker,
        ReviewStatus::NoIssues => no_issues_marker,
    };
    let mut lines: Vec<&str> = response
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let final_line = lines.pop().ok_or_else(|| {
        SpecLoopError::Agent("reviewer response has no final status marker".to_owned())
    })?;
    if final_line != expected_marker {
        return Err(SpecLoopError::Agent(format!(
            "reviewer response must end with exactly {expected_marker}"
        )));
    }
    if status == ReviewStatus::NoIssues {
        if lines.iter().any(|line| line.starts_with("FINDING-")) {
            return Err(SpecLoopError::Agent(
                "reviewer reported structured findings with a NO_ISSUES status; refusing approval"
                    .to_owned(),
            ));
        }
        if !lines.is_empty() {
            return Err(SpecLoopError::Agent(
                "reviewer response contains prose or unknown markers with a NO_ISSUES status; refusing approval"
                    .to_owned(),
            ));
        }
        return Ok(());
    }
    let findings = parse_reviewer_findings(response)?;
    if findings.is_empty() {
        return Err(SpecLoopError::Agent(
            "reviewer reported ISSUES without structured FINDING-N entries".to_owned(),
        ));
    }
    Ok(())
}

fn parse_status_audit_response(
    response: &str,
    expected_prefix: &str,
) -> Result<StatusAuditOutcome> {
    let mut marker = None;
    for line in response
        .lines()
        .filter(|line| line.starts_with("SPEC_LOOP_"))
    {
        if marker.is_some() {
            return Err(SpecLoopError::Agent(format!(
                "status auditor must return exactly one canonical marker beginning with {expected_prefix}"
            )));
        }
        marker = Some(line);
    }
    let marker = marker.ok_or_else(|| {
        SpecLoopError::Agent(format!(
            "status auditor must return exactly one canonical marker beginning with {expected_prefix}"
        ))
    })?;
    let Some(status) = marker.strip_prefix(expected_prefix) else {
        return Err(SpecLoopError::Agent(format!(
            "status auditor returned a marker for the wrong role; expected {expected_prefix}"
        )));
    };
    if last_line(response) != Some(marker) {
        return Err(SpecLoopError::Agent(format!(
            "status auditor marker must be the final non-empty line: {expected_prefix}ISSUES, {expected_prefix}NO_ISSUES, or {expected_prefix}AMBIGUOUS"
        )));
    }
    match status {
        "ISSUES" => Ok(StatusAuditOutcome::Issues),
        "NO_ISSUES" => Ok(StatusAuditOutcome::NoIssues),
        "AMBIGUOUS" => Ok(StatusAuditOutcome::Ambiguous),
        _ => Err(SpecLoopError::Agent(format!(
            "status auditor returned an unknown canonical marker; expected {expected_prefix}ISSUES, {expected_prefix}NO_ISSUES, or {expected_prefix}AMBIGUOUS"
        ))),
    }
}

fn canonical_review_status(response: &str, issues: &str, no_issues: &str) -> Option<ReviewStatus> {
    match last_line(response)? {
        line if line == issues => Some(ReviewStatus::Issues),
        line if line == no_issues => Some(ReviewStatus::NoIssues),
        _ => None,
    }
}

fn last_line(response: &str) -> Option<&str> {
    response.lines().rev().find(|line| !line.trim().is_empty())
}

fn audit_findings_with_evidence(
    context: &mut WorkflowContext<'_>,
    reviewer_request: AgentRequest<'_>,
    initial_response: &str,
    plan: bool,
    reviewed_identity: Option<&GitIdentity>,
) -> Result<AuditReport> {
    let (audit_role, marker_prefix) = if plan {
        (PromptRole::PlanAudit, PLAN_AUDIT_PREFIX)
    } else {
        (PromptRole::ReviewAudit, REVIEW_AUDIT_PREFIX)
    };
    let initial = run_grounder(
        context,
        reviewer_request,
        audit_role,
        marker_prefix,
        initial_response,
        None,
    )?;
    if initial.outcome != AuditOutcome::NoActionableFindings
        && audit_input_changed(context, reviewed_identity)?
    {
        return Ok(input_changed_audit_report());
    }
    if initial.unsupported.is_empty() && initial.ambiguous.is_empty() {
        return Ok(initial);
    }
    let unresolved = [initial.unsupported.as_str(), initial.ambiguous.as_str()]
        .into_iter()
        .filter(|findings| !findings.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let mut clarification_feedback = format!(
        "Provide cited evidence for these previously reported findings. Repeat every original FINDING-N line exactly, then add evidence only as `EVIDENCE-FINDING-N: <citation and evidence>`. Do not add, remove, or rewrite findings, or change repository files:\n{unresolved}"
    );
    let clarified = loop {
        let evidence_request = AgentRequest {
            feedback: Some(initial_response),
            validation_feedback: Some(&clarification_feedback),
            ..reviewer_request
        };
        let clarified = review_with_status(context, evidence_request, plan)?;
        if audit_input_changed(context, reviewed_identity)? {
            return Ok(input_changed_audit_report());
        }
        if clarified.1 == ReviewStatus::NoIssues {
            break (clarified, BTreeSet::new());
        }
        match validate_finding_clarification(initial_response, &clarified.0) {
            Ok(evidence_ids) => break (clarified, evidence_ids),
            Err(error) => {
                clarification_feedback = format!(
                    "Your evidence clarification failed validation: {error}. Repeat every original FINDING-N line exactly, then provide evidence only with EVIDENCE-FINDING-N lines for those existing IDs. Do not add, remove, or rewrite findings."
                );
            }
        }
    };
    let unresolved_ids = finding_ids_in_audit_text(&initial.unsupported)?
        .into_iter()
        .chain(finding_ids_in_audit_text(&initial.ambiguous)?)
        .collect::<BTreeSet<_>>();
    let evidence_ids: BTreeSet<usize> =
        clarified.1.intersection(&unresolved_ids).copied().collect();
    let second = run_evidence_grounder(
        context,
        reviewer_request,
        audit_role,
        marker_prefix,
        initial_response,
        &clarified.0.0,
        &evidence_ids,
    )?;
    let rechecked_supported = findings_for_ids(&second.supported, &evidence_ids)?;
    let supported = merge_supported_findings(&initial.supported, &rechecked_supported)?;
    let outcome = if supported.is_empty() {
        AuditOutcome::NoActionableFindings
    } else {
        AuditOutcome::Actionable
    };
    if outcome != AuditOutcome::NoActionableFindings
        && audit_input_changed(context, reviewed_identity)?
    {
        return Ok(input_changed_audit_report());
    }
    let unresolved_without_evidence: BTreeSet<usize> =
        unresolved_ids.difference(&evidence_ids).copied().collect();
    let unsupported = merge_audit_findings(
        &findings_for_ids(&initial.unsupported, &unresolved_without_evidence)?,
        &findings_for_ids(&second.unsupported, &evidence_ids)?,
    )?;
    let ambiguous = merge_audit_findings(
        &findings_for_ids(&initial.ambiguous, &unresolved_without_evidence)?,
        &findings_for_ids(&second.ambiguous, &evidence_ids)?,
    )?;
    Ok(AuditReport {
        supported,
        unsupported,
        ambiguous,
        outcome,
    })
}

fn run_evidence_grounder(
    context: &mut WorkflowContext<'_>,
    reviewer_request: AgentRequest<'_>,
    role: PromptRole,
    marker_prefix: &str,
    initial_response: &str,
    clarification: &str,
    evidence_ids: &BTreeSet<usize>,
) -> Result<AuditReport> {
    if evidence_ids.is_empty() {
        return Ok(AuditReport {
            supported: String::new(),
            unsupported: String::new(),
            ambiguous: String::new(),
            outcome: AuditOutcome::NoActionableFindings,
        });
    }
    let evidence_context = build_evidence_context(clarification, evidence_ids)?;
    run_grounder(
        context,
        reviewer_request,
        role,
        marker_prefix,
        initial_response,
        Some(&evidence_context),
    )
}

fn audit_input_changed(
    context: &WorkflowContext<'_>,
    reviewed_identity: Option<&GitIdentity>,
) -> Result<bool> {
    let Some(reviewed_identity) = reviewed_identity else {
        return Ok(false);
    };
    Ok(&git::input_identity(context.root)? != reviewed_identity)
}

const fn input_changed_audit_report() -> AuditReport {
    AuditReport {
        supported: String::new(),
        unsupported: String::new(),
        ambiguous: String::new(),
        outcome: AuditOutcome::InputChanged,
    }
}

fn validate_finding_clarification(original: &str, clarification: &str) -> Result<BTreeSet<usize>> {
    let original_findings = parse_reviewer_findings(original)?;
    let clarified_findings = parse_reviewer_findings(clarification)?;
    if clarified_findings != original_findings {
        return Err(SpecLoopError::Agent(
            "evidence clarification changed, removed, or introduced reviewer findings".to_owned(),
        ));
    }
    Ok(reviewer_evidence_lines(clarification)?
        .into_keys()
        .collect())
}

fn reviewer_evidence_lines(response: &str) -> Result<BTreeMap<usize, &str>> {
    let mut evidence_lines = BTreeMap::new();
    for line in response.lines() {
        let Some(evidence) = line.strip_prefix("EVIDENCE-FINDING-") else {
            continue;
        };
        let (number, text) = evidence.split_once(':').ok_or_else(|| {
            SpecLoopError::Agent(
                "reviewer evidence line must use EVIDENCE-FINDING-N: <evidence>".to_owned(),
            )
        })?;
        let id = number.parse::<usize>().map_err(|error| {
            SpecLoopError::Agent(format!(
                "reviewer evidence has an invalid finding ID: {error}"
            ))
        })?;
        if id == 0 || id.to_string() != number || text.trim().is_empty() {
            return Err(SpecLoopError::Agent(
                "reviewer evidence finding ID or evidence text is invalid".to_owned(),
            ));
        }
        if evidence_lines.insert(id, line).is_some() {
            return Err(SpecLoopError::Agent(format!(
                "reviewer response repeats evidence for FINDING-{id}"
            )));
        }
    }
    Ok(evidence_lines)
}

fn finding_ids_in_audit_text(text: &str) -> Result<BTreeSet<usize>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| parse_finding_line(line, "grounding report"))
        .collect()
}

fn build_evidence_context(clarification: &str, evidence_ids: &BTreeSet<usize>) -> Result<String> {
    let evidence_lines = reviewer_evidence_lines(clarification)?;
    let mut context = String::from(
        "Reviewer evidence clarification (ground only findings with supplied citations):\n",
    );
    for id in evidence_ids {
        let evidence = evidence_lines.get(id).ok_or_else(|| {
            SpecLoopError::Agent(format!("missing reviewer evidence for FINDING-{id}"))
        })?;
        context.push_str(evidence);
        context.push('\n');
    }
    Ok(context)
}

fn findings_for_ids(findings: &str, ids: &BTreeSet<usize>) -> Result<String> {
    findings
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match parse_finding_line(line, "audit disposition") {
            Ok(id) if ids.contains(&id) => Some(Ok(line.to_owned())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()
        .map(|lines| lines.join("\n"))
}

fn merge_audit_findings(first: &str, second: &str) -> Result<String> {
    let mut findings = BTreeMap::new();
    for finding in first.lines().chain(second.lines()) {
        let id = parse_finding_line(finding, "audit disposition")?;
        findings.insert(id, finding.to_owned());
    }
    Ok(findings.into_values().collect::<Vec<_>>().join("\n"))
}

fn run_grounder(
    context: &mut WorkflowContext<'_>,
    reviewer_request: AgentRequest<'_>,
    role: PromptRole,
    marker_prefix: &str,
    reviewer_response: &str,
    evidence_context: Option<&str>,
) -> Result<AuditReport> {
    let feedback = evidence_context.map_or_else(
        || reviewer_response.to_owned(),
        |context| format!("{reviewer_response}\n\n{context}"),
    );
    let audit_request = AgentRequest {
        role,
        feedback: Some(&feedback),
        validation_feedback: None,
        ..reviewer_request
    };
    let (_, report) = call_agent_parsed(context, audit_request, |response| {
        parse_audit_response(response, marker_prefix, reviewer_response)
    })?;
    Ok(report)
}

fn merge_supported_findings(first: &str, second: &str) -> Result<String> {
    let mut findings = BTreeMap::new();
    for finding in first.lines().chain(second.lines()) {
        let id = parse_finding_line(finding, "supported audit finding")?;
        findings.entry(id).or_insert_with(|| finding.to_owned());
    }
    Ok(findings.into_values().collect::<Vec<_>>().join("\n"))
}

fn report_audit_dispositions(report: &AuditReport) {
    if !report.unsupported.is_empty() {
        println!(
            "Auditor rejected unsupported findings:\n{}",
            report.unsupported
        );
    }
    if !report.ambiguous.is_empty() {
        println!("Auditor marked findings ambiguous:\n{}", report.ambiguous);
    }
}

const AUDIT_HEADERS: [&str; 3] = [
    "SUPPORTED_FINDINGS:",
    "UNSUPPORTED_FINDINGS:",
    "AMBIGUOUS_FINDINGS:",
];

fn parse_audit_response(
    response: &str,
    marker_prefix: &str,
    reviewer_response: &str,
) -> Result<AuditReport> {
    let reviewer_findings = parse_reviewer_findings(reviewer_response)?;
    if reviewer_findings.is_empty() {
        return Err(SpecLoopError::Agent(
            "reviewer reported issues without any structured FINDING-N entries".to_owned(),
        ));
    }
    let reviewer_findings: BTreeMap<usize, String> = reviewer_findings.into_iter().collect();
    let marker = last_line(response)
        .ok_or_else(|| SpecLoopError::Agent("audit response has no final marker".to_owned()))?;
    let marker_value = marker.strip_prefix(marker_prefix).ok_or_else(|| {
        SpecLoopError::Agent(format!("audit response must end with {marker_prefix}..."))
    })?;
    let lines: Vec<&str> = response.lines().collect();
    let marker_index = lines
        .iter()
        .rposition(|line| *line == marker)
        .ok_or_else(|| SpecLoopError::Agent("audit marker line could not be located".to_owned()))?;
    let body = lines
        .get(..marker_index)
        .ok_or_else(|| SpecLoopError::Agent("audit body is malformed".to_owned()))?;
    let mut positions = Vec::new();
    for header in AUDIT_HEADERS {
        let matches: Vec<usize> = body
            .iter()
            .enumerate()
            .filter_map(|(index, line)| (*line == header).then_some(index))
            .collect();
        if matches.len() != 1 {
            return Err(SpecLoopError::Agent(format!(
                "audit response must contain exactly one {header} section"
            )));
        }
        positions.push(matches[0]);
    }
    if positions[0] >= positions[1]
        || positions[1] >= positions[2]
        || body[..positions[0]]
            .iter()
            .any(|line| !line.trim().is_empty())
    {
        return Err(SpecLoopError::Agent(
            "audit sections are out of order or contain text outside sections".to_owned(),
        ));
    }
    let section =
        |start: usize, end: usize| -> String { body[start + 1..end].join("\n").trim().to_owned() };
    let supported = section(positions[0], positions[1]);
    let unsupported = section(positions[1], positions[2]);
    let ambiguous = body[positions[2] + 1..].join("\n").trim().to_owned();
    let (supported_ids, supported_text) =
        parse_audit_findings(&supported, "SUPPORTED_FINDINGS", &reviewer_findings, true)?;
    let (unsupported_ids, unsupported_text) = parse_audit_findings(
        &unsupported,
        "UNSUPPORTED_FINDINGS",
        &reviewer_findings,
        false,
    )?;
    let (ambiguous_ids, ambiguous_text) =
        parse_audit_findings(&ambiguous, "AMBIGUOUS_FINDINGS", &reviewer_findings, false)?;
    let mut classified = BTreeSet::new();
    for id in supported_ids
        .iter()
        .chain(&unsupported_ids)
        .chain(&ambiguous_ids)
    {
        if !classified.insert(*id) {
            return Err(SpecLoopError::Agent(format!(
                "audit classified FINDING-{id} more than once"
            )));
        }
    }
    let reviewer_ids: BTreeSet<usize> = reviewer_findings.keys().copied().collect();
    if classified != reviewer_ids {
        return Err(SpecLoopError::Agent(
            "audit did not classify every reported finding exactly once".to_owned(),
        ));
    }
    let empty = |value: &str| value.is_empty();
    let outcome = match marker_value {
        "ACTIONABLE" if !empty(&supported_text) && empty(&ambiguous_text) => {
            AuditOutcome::Actionable
        }
        "NO_ACTIONABLE_FINDINGS" if empty(&supported_text) && empty(&ambiguous_text) => {
            AuditOutcome::NoActionableFindings
        }
        "AMBIGUOUS" if !empty(&ambiguous_text) => AuditOutcome::Ambiguous,
        _ => {
            return Err(SpecLoopError::Agent(format!(
                "audit marker {marker:?} conflicts with classified findings"
            )));
        }
    };
    Ok(AuditReport {
        supported: supported_text,
        unsupported: unsupported_text,
        ambiguous: ambiguous_text,
        outcome,
    })
}

fn parse_reviewer_findings(response: &str) -> Result<Vec<(usize, String)>> {
    let mut lines: Vec<&str> = response
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let final_line = lines.last().copied();
    if final_line.is_some_and(|line| {
        matches!(
            line,
            "SPEC_LOOP_PLAN_REVIEW=ISSUES"
                | "SPEC_LOOP_PLAN_REVIEW=NO_ISSUES"
                | "SPEC_LOOP_REVIEW=ISSUES"
                | "SPEC_LOOP_REVIEW=NO_ISSUES"
        )
    }) || final_line.is_some_and(|line| !line.starts_with("FINDING-"))
    {
        lines.pop();
    }
    let mut findings = Vec::new();
    let mut evidence_ids = BTreeSet::new();
    for line in lines {
        if let Some(evidence) = line.strip_prefix("EVIDENCE-FINDING-") {
            let (number, text) = evidence.split_once(':').ok_or_else(|| {
                SpecLoopError::Agent(
                    "reviewer evidence line must use EVIDENCE-FINDING-N: <evidence>".to_owned(),
                )
            })?;
            let id = number.parse::<usize>().map_err(|error| {
                SpecLoopError::Agent(format!(
                    "reviewer evidence has an invalid finding ID: {error}"
                ))
            })?;
            if id == 0 || id.to_string() != number || text.trim().is_empty() {
                return Err(SpecLoopError::Agent(
                    "reviewer evidence finding ID or evidence text is invalid".to_owned(),
                ));
            }
            evidence_ids.insert(id);
            continue;
        }
        let id = parse_finding_line(line, "reviewer response")?;
        let expected = findings
            .len()
            .checked_add(1)
            .ok_or_else(|| SpecLoopError::Agent("finding count overflowed".to_owned()))?;
        if id != expected {
            return Err(SpecLoopError::Agent(
                "reviewer response finding identifiers must be sequential from FINDING-1"
                    .to_owned(),
            ));
        }
        findings.push((id, line.to_owned()));
    }
    if evidence_ids
        .iter()
        .any(|id| findings.iter().all(|(finding_id, _)| finding_id != id))
    {
        return Err(SpecLoopError::Agent(
            "reviewer evidence refers to an unreported finding".to_owned(),
        ));
    }
    Ok(findings)
}

fn parse_audit_findings(
    section: &str,
    name: &str,
    reviewer_findings: &BTreeMap<usize, String>,
    preserve_original: bool,
) -> Result<(BTreeSet<usize>, String)> {
    if section.is_empty() || section == "None" {
        return Ok((BTreeSet::new(), String::new()));
    }
    let mut findings = BTreeSet::new();
    let mut output = Vec::new();
    for line in section.lines().filter(|line| !line.trim().is_empty()) {
        let id = parse_finding_line(line, name)?;
        if !findings.insert(id) {
            return Err(SpecLoopError::Agent(format!(
                "audit section {name} repeats FINDING-{id}"
            )));
        }
        let original = reviewer_findings.get(&id).ok_or_else(|| {
            SpecLoopError::Agent(format!(
                "audit section {name} introduced unreported FINDING-{id}"
            ))
        })?;
        let rationale = line
            .strip_prefix(original)
            .and_then(|suffix| suffix.strip_prefix(" | AUDIT RATIONALE: "))
            .filter(|rationale| !rationale.trim().is_empty())
            .ok_or_else(|| {
                SpecLoopError::Agent(format!(
                    "audit section {name} changed FINDING-{id} or omitted its separate rationale"
                ))
            })?;
        if preserve_original {
            output.push(original.clone());
        } else {
            output.push(format!("{original} | AUDIT RATIONALE: {rationale}"));
        }
    }
    Ok((findings, output.join("\n")))
}

fn parse_finding_line(line: &str, source: &str) -> Result<usize> {
    let identifier = line.strip_prefix("FINDING-").ok_or_else(|| {
        SpecLoopError::Agent(format!("{source} entries must begin with FINDING-N"))
    })?;
    let (number, rationale) = identifier.split_once(':').ok_or_else(|| {
        SpecLoopError::Agent(format!("{source} finding has no rationale separator"))
    })?;
    let id = number.parse::<usize>().map_err(|error| {
        SpecLoopError::Agent(format!(
            "{source} has an invalid finding identifier: {error}"
        ))
    })?;
    if id == 0 || id.to_string() != number || rationale.trim().is_empty() {
        return Err(SpecLoopError::Agent(format!(
            "{source} finding identifier or rationale is invalid"
        )));
    }
    Ok(id)
}

fn call_agent(
    context: &mut WorkflowContext<'_>,
    request: AgentRequest<'_>,
) -> Result<Result<String>> {
    let role = request.role;
    let role_name = role_name(role);
    let mut retry_feedback: Option<String> = None;
    loop {
        context.integrity.verify(context.root)?;
        begin_agent_attempt(context, role, &request.candidate.id)?;
        let combined_feedback = match (request.validation_feedback, retry_feedback.as_deref()) {
            (Some(original), Some(retry)) => Some(format!("{original}\n\n{retry}")),
            (Some(original), None) => Some(original.to_owned()),
            (None, Some(retry)) => Some(retry.to_owned()),
            (None, None) => None,
        };
        let attempt_request = AgentRequest {
            validation_feedback: combined_feedback.as_deref(),
            ..request
        };
        let prompt = build_role_prompt(context, attempt_request)?;
        let call_index = next_agent_call(context)?;
        let attempt = context.role_calls.get(&role).copied().ok_or_else(|| {
            SpecLoopError::Workflow(format!("missing attempt count for {role_name}"))
        })?;
        commands::report_progress(&format!(
            "Stage: agent {role_name} (attempt {attempt}; spec {})",
            request.candidate.id
        ))?;
        let agent_result = invoke_and_log_agent(context, call_index, role_name, role, &prompt)?;
        match agent_result {
            Ok(response) => return Ok(Ok(response.response)),
            Err(SpecLoopError::Interrupted) => return Ok(Err(SpecLoopError::Interrupted)),
            Err(error) => {
                println!("Agent {role_name} attempt failed: {error}");
                retry_feedback = Some(format!(
                    "Previous {role_name} attempt failed: {error}. Retry the same task; preserve all repository changes."
                ));
            }
        }
    }
}

fn begin_agent_attempt(
    context: &mut WorkflowContext<'_>,
    role: PromptRole,
    spec_id: &str,
) -> Result<()> {
    let attempts = context.role_calls.entry(role).or_default();
    if context.max_passes > 0 && *attempts >= context.max_passes {
        let resume = match role {
            PromptRole::PlanAuthor
            | PromptRole::PlanReview
            | PromptRole::PlanReviewStatusAudit
            | PromptRole::PlanAudit => format!("--plan --spec {spec_id}"),
            PromptRole::Implementation => format!("--spec {spec_id}"),
            PromptRole::Review | PromptRole::ReviewStatusAudit | PromptRole::ReviewAudit => {
                format!("--review --spec {spec_id}")
            }
        };
        return Err(SpecLoopError::Workflow(format!(
            "{} reached profile completion.max_passes limit ({}); changes preserved. Increase `completion.max_passes` in `.spec-loop.toml` and resume with `{resume}`.",
            role_name(role),
            context.max_passes
        )));
    }
    *attempts = attempts
        .checked_add(1)
        .ok_or_else(|| SpecLoopError::Workflow("agent attempt counter overflowed".to_owned()))?;
    Ok(())
}

fn build_role_prompt(context: &WorkflowContext<'_>, request: AgentRequest<'_>) -> Result<String> {
    prompts::build_prompt(
        context.root,
        context.profile,
        PromptContext {
            role: request.role,
            spec_id: &request.candidate.id,
            spec_path: &request.candidate.path,
            plan_path: request.plan_path,
            feedback: request.feedback,
            validation_feedback: request.validation_feedback,
            checks: request.checks,
        },
    )
}

fn next_agent_call(context: &mut WorkflowContext<'_>) -> Result<usize> {
    context.log_index = context
        .log_index
        .checked_add(1)
        .ok_or_else(|| SpecLoopError::Workflow("agent call counter overflowed".to_owned()))?;
    Ok(context.log_index)
}

fn invoke_and_log_agent(
    context: &mut WorkflowContext<'_>,
    call_index: usize,
    role_name: &str,
    role: PromptRole,
    prompt: &str,
) -> Result<Result<AgentResponse>> {
    let run = match context.backend.run(
        context.root,
        context.profile,
        prompt,
        role,
        &context.cancelled,
    ) {
        Ok(run) => run,
        Err(error) => {
            context
                .integrity
                .agent_finished(context.root, role == PromptRole::PlanAuthor)?;
            return Ok(Err(error));
        }
    };
    let stdout_name = format!("agent-{call_index:04}-{role_name}.stdout.jsonl");
    let stderr_name = format!("agent-{call_index:04}-{role_name}.stderr.log");
    let log_result = context
        .run_dir
        .write_log(&stdout_name, &run.stdout)
        .and_then(|_| {
            context
                .run_dir
                .write_log(&stderr_name, &run.stderr)
                .map(|_| ())
        });
    println!(
        "Agent {role_name} logs: {}, {}",
        context.run_dir.log_path(&stdout_name).display(),
        context.run_dir.log_path(&stderr_name).display()
    );
    let outcome = run.outcome;
    log_result?;
    match &outcome {
        Ok(response) => {
            if let Some(model) = &response.selected_model {
                println!("Agent {role_name} selected model: {model}");
            }
            if let Some(model) = &response.response_model {
                println!("Agent {role_name} provider-reported model: {model}");
            }
            context.run_dir.write_log(
                &format!("agent-{call_index:04}-{role_name}.response.txt"),
                response.response.as_bytes(),
            )?;
            println!(
                "--- Agent {role_name} final response ---\n{}",
                response.response
            );
        }
        Err(error) => {
            println!("Agent {role_name} diagnostics: {error}");
            if !matches!(error, SpecLoopError::Interrupted) {
                let stdout = String::from_utf8_lossy(&run.stdout);
                if !stdout.trim().is_empty() {
                    println!("Agent {role_name} available stdout:\n{}", stdout.trim_end());
                }
                let stderr = String::from_utf8_lossy(&run.stderr);
                if !stderr.trim().is_empty() {
                    println!("Agent {role_name} available stderr:\n{}", stderr.trim_end());
                }
            }
        }
    }
    context
        .integrity
        .agent_finished(context.root, role == PromptRole::PlanAuthor)?;
    Ok(outcome)
}

const fn role_name(role: PromptRole) -> &'static str {
    match role {
        PromptRole::PlanAuthor => "plan-author",
        PromptRole::PlanReview => "plan-review",
        PromptRole::PlanReviewStatusAudit => "plan-review-status-audit",
        PromptRole::PlanAudit => "plan-audit",
        PromptRole::Implementation => "implementation",
        PromptRole::Review => "review",
        PromptRole::ReviewStatusAudit => "review-status-audit",
        PromptRole::ReviewAudit => "review-audit",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        AuditOutcome, ReviewStatus, StatusAuditOutcome, canonical_review_status,
        parse_audit_response, parse_reviewer_findings, parse_status_audit_response,
        validate_finding_clarification,
    };
    #[test]
    fn audit_parser_returns_supported_findings_only() {
        let reviewer =
            "FINDING-1: supported concern\nFINDING-2: unsupported concern\nSPEC_LOOP_REVIEW=ISSUES";
        let response = "SUPPORTED_FINDINGS:\nFINDING-1: supported concern | AUDIT RATIONALE: supported by the spec\nUNSUPPORTED_FINDINGS:\nFINDING-2: unsupported concern | AUDIT RATIONALE: contradicted by repository policy\nAMBIGUOUS_FINDINGS:\nNone\nSPEC_LOOP_REVIEW_AUDIT=ACTIONABLE";
        let report = parse_audit_response(response, "SPEC_LOOP_REVIEW_AUDIT=", reviewer);
        assert!(report.is_ok());
        if let Ok(report) = report {
            assert_eq!(report.outcome, AuditOutcome::Actionable);
            assert_eq!(report.supported, "FINDING-1: supported concern");
            assert_eq!(
                report.unsupported,
                "FINDING-2: unsupported concern | AUDIT RATIONALE: contradicted by repository policy"
            );
            assert_eq!(report.ambiguous, "");
        }
    }

    #[test]
    fn audit_parser_preserves_trailing_whitespace_in_reviewer_finding() {
        let reviewer = "FINDING-1: supported concern  \nSPEC_LOOP_PLAN_REVIEW=ISSUES";
        let response = "SUPPORTED_FINDINGS:\nFINDING-1: supported concern   | AUDIT RATIONALE: supported by the spec\nUNSUPPORTED_FINDINGS:\nNone\nAMBIGUOUS_FINDINGS:\nNone\nSPEC_LOOP_PLAN_AUDIT=ACTIONABLE";

        let report = parse_audit_response(response, "SPEC_LOOP_PLAN_AUDIT=", reviewer);

        assert!(report.is_ok_and(|report| {
            report.outcome == AuditOutcome::Actionable
                && report.supported == "FINDING-1: supported concern  "
        }));
    }

    #[test]
    fn unsupported_findings_are_non_actionable() {
        let reviewer = "FINDING-1: unsupported concern\nSPEC_LOOP_REVIEW=ISSUES";
        let response = "SUPPORTED_FINDINGS:\nNone\nUNSUPPORTED_FINDINGS:\nFINDING-1: unsupported concern | AUDIT RATIONALE: unsupported by requirements\nAMBIGUOUS_FINDINGS:\nNone\nSPEC_LOOP_REVIEW_AUDIT=NO_ACTIONABLE_FINDINGS";
        let report = parse_audit_response(response, "SPEC_LOOP_REVIEW_AUDIT=", reviewer);
        assert!(report.is_ok_and(|report| {
            report.outcome == AuditOutcome::NoActionableFindings
                && report.supported.is_empty()
                && !report.unsupported.is_empty()
        }));
    }

    #[test]
    fn audit_must_classify_every_reviewer_finding_exactly_once() {
        let reviewer = "FINDING-1: first issue\nFINDING-2: second issue\nSPEC_LOOP_REVIEW=ISSUES";
        let incomplete = "SUPPORTED_FINDINGS:\nNone\nUNSUPPORTED_FINDINGS:\nFINDING-1: unsupported\nAMBIGUOUS_FINDINGS:\nNone\nSPEC_LOOP_REVIEW_AUDIT=NO_ACTIONABLE_FINDINGS";
        assert!(parse_audit_response(incomplete, "SPEC_LOOP_REVIEW_AUDIT=", reviewer).is_err());
        let duplicate = "SUPPORTED_FINDINGS:\nFINDING-1: first issue | AUDIT RATIONALE: supported\nUNSUPPORTED_FINDINGS:\nFINDING-1: first issue | AUDIT RATIONALE: unsupported\nAMBIGUOUS_FINDINGS:\nNone\nSPEC_LOOP_REVIEW_AUDIT=ACTIONABLE";
        assert!(parse_audit_response(duplicate, "SPEC_LOOP_REVIEW_AUDIT=", reviewer).is_err());
    }

    #[test]
    fn audit_marker_must_match_classified_findings() {
        let reviewer = "FINDING-1: concern\nSPEC_LOOP_REVIEW=ISSUES";
        let response = "SUPPORTED_FINDINGS:\nFINDING-1: concern | AUDIT RATIONALE: supported\nUNSUPPORTED_FINDINGS:\nNone\nAMBIGUOUS_FINDINGS:\nNone\nSPEC_LOOP_REVIEW_AUDIT=NO_ACTIONABLE_FINDINGS";
        assert!(parse_audit_response(response, "SPEC_LOOP_REVIEW_AUDIT=", reviewer).is_err());
    }

    #[test]
    fn audit_cannot_rewrite_reported_finding_text() {
        let reviewer = "FINDING-1: original concern\nSPEC_LOOP_REVIEW=ISSUES";
        let response = "SUPPORTED_FINDINGS:\nFINDING-1: substituted concern | AUDIT RATIONALE: supported\nUNSUPPORTED_FINDINGS:\nNone\nAMBIGUOUS_FINDINGS:\nNone\nSPEC_LOOP_REVIEW_AUDIT=ACTIONABLE";
        let result = parse_audit_response(response, "SPEC_LOOP_REVIEW_AUDIT=", reviewer);
        assert!(result.is_err_and(|error| { error.to_string().contains("changed FINDING-1") }));
    }

    #[test]
    fn evidence_clarification_accepts_only_unchanged_findings() {
        let original = "FINDING-1: original concern\nSPEC_LOOP_REVIEW=ISSUES";
        let clarification = "FINDING-1: original concern\nEVIDENCE-FINDING-1: source line 42 confirms behavior\nSPEC_LOOP_REVIEW=ISSUES";
        assert!(
            validate_finding_clarification(original, clarification)
                .is_ok_and(|ids| ids == BTreeSet::from([1]))
        );

        let no_evidence = "FINDING-1: original concern\nSPEC_LOOP_REVIEW=ISSUES";
        assert!(
            validate_finding_clarification(original, no_evidence).is_ok_and(|ids| ids.is_empty())
        );

        let added = "FINDING-1: original concern\nFINDING-2: newly invented concern\nEVIDENCE-FINDING-1: source line 42\nSPEC_LOOP_REVIEW=ISSUES";
        assert!(validate_finding_clarification(original, added).is_err());

        let rewritten = "FINDING-1: rewritten concern\nEVIDENCE-FINDING-1: source line 42\nSPEC_LOOP_REVIEW=ISSUES";
        assert!(validate_finding_clarification(original, rewritten).is_err());

        let changed_whitespace = "FINDING-1: original concern  \nEVIDENCE-FINDING-1: source line 42\nSPEC_LOOP_REVIEW=ISSUES";
        assert!(validate_finding_clarification(original, changed_whitespace).is_err());

        let original_pair =
            "FINDING-1: first concern\nFINDING-2: second concern\nSPEC_LOOP_REVIEW=ISSUES";
        let reordered = "FINDING-2: second concern\nFINDING-1: first concern\nEVIDENCE-FINDING-1: source line 42\nSPEC_LOOP_REVIEW=ISSUES";
        assert!(validate_finding_clarification(original_pair, reordered).is_err());
    }

    #[test]
    fn reviewer_findings_preserve_raw_lines_and_require_encounter_order() {
        let response =
            "FINDING-1: first concern  \nFINDING-2: second concern\nSPEC_LOOP_REVIEW=ISSUES";
        assert_eq!(
            parse_reviewer_findings(response).expect("parse sequential reviewer findings"),
            vec![
                (1, "FINDING-1: first concern  ".to_owned()),
                (2, "FINDING-2: second concern".to_owned()),
            ]
        );

        let reordered =
            "FINDING-2: second concern\nFINDING-1: first concern\nSPEC_LOOP_REVIEW=ISSUES";
        assert!(parse_reviewer_findings(reordered).is_err());
    }

    #[test]
    fn evidence_clarification_cannot_cite_unreported_finding() {
        let original = "FINDING-1: original concern\nSPEC_LOOP_REVIEW=ISSUES";
        let clarification = "FINDING-1: original concern\nEVIDENCE-FINDING-2: unrelated source line\nSPEC_LOOP_REVIEW=ISSUES";
        assert!(validate_finding_clarification(original, clarification).is_err());
    }

    #[test]
    fn status_auditor_requires_one_final_canonical_marker_for_both_review_modes() {
        for (prefix, opposite_prefix) in [
            (
                "SPEC_LOOP_PLAN_REVIEW_STATUS_AUDIT=",
                "SPEC_LOOP_REVIEW_STATUS_AUDIT=",
            ),
            (
                "SPEC_LOOP_REVIEW_STATUS_AUDIT=",
                "SPEC_LOOP_PLAN_REVIEW_STATUS_AUDIT=",
            ),
        ] {
            assert!(
                parse_status_audit_response(
                    &format!("Status interpreted.\n{prefix}AMBIGUOUS"),
                    prefix
                )
                .is_ok_and(|outcome| outcome == StatusAuditOutcome::Ambiguous)
            );
            assert!(
                parse_status_audit_response(&format!("{prefix}NO_ISSUES"), prefix)
                    .is_ok_and(|outcome| outcome == StatusAuditOutcome::NoIssues)
            );
            assert!(
                parse_status_audit_response(
                    &format!("{prefix}AMBIGUOUS\n{prefix}NO_ISSUES"),
                    prefix
                )
                .is_err()
            );
            assert!(
                parse_status_audit_response(&format!("{prefix}ISSUES\n{prefix}ISSUES"), prefix)
                    .is_err()
            );
            assert!(
                parse_status_audit_response(&format!("{opposite_prefix}NO_ISSUES"), prefix)
                    .is_err()
            );
            assert!(
                parse_status_audit_response(&format!("{prefix}NO_ISSUES\nextra text"), prefix)
                    .is_err()
            );
        }
    }

    #[test]
    fn reviewer_status_markers_must_be_exact() {
        assert_eq!(
            canonical_review_status(
                "A finding.\nSPEC_LOOP_REVIEW=ISSUES",
                "SPEC_LOOP_REVIEW=ISSUES",
                "SPEC_LOOP_REVIEW=NO_ISSUES"
            ),
            Some(ReviewStatus::Issues)
        );
        assert_eq!(
            canonical_review_status(
                "SPEC_LOOP_REVIEW=NO_ISSUES ",
                "SPEC_LOOP_REVIEW=ISSUES",
                "SPEC_LOOP_REVIEW=NO_ISSUES"
            ),
            None
        );
    }
}
