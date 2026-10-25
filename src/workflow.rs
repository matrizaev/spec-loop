use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::backend::{self, AgentBackend, AgentResponse};
use crate::cli::Cli;
use crate::commands::{self, ProcessTermination, status_success};
use crate::config::{ActionConfig, CheckConfig, Profile};
use crate::discovery::{self, SpecCandidate};
use crate::error::{Result, SpecLoopError, io_error};
use crate::integrity::InputIntegrity;
use crate::paths::{self, expand_template, resolve_optional};
use crate::prompts::{self, PromptContext, PromptRole};
use crate::protocol::{
    self, Actionability, AuditEvidenceInput, AuditInput, AuditResult, ClarificationInput,
    Classification, EvidenceResult, Finding, FindingClassification, ImplementationAuditResponse,
    ImplementationClarificationResponse, ImplementationRepairInput, ImplementationResponse,
    ImplementationReviewResponse, PlanAuditResponse, PlanAuthorResponse, PlanClarificationResponse,
    PlanRepairInput, PlanReviewResponse, Readiness, ResponseFields, ReviewResult,
};
use crate::run_dir::RunDir;

struct WorkflowContext<'a> {
    root: &'a Path,
    profile: &'a Profile,
    run_dir: &'a RunDir,
    cancelled: Arc<AtomicBool>,
    max_passes: usize,
    role_calls: BTreeMap<PromptRole, usize>,
    log_index: usize,
    integrity: InputIntegrity,
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

struct CandidateFiles {
    plan_path: PathBuf,
    has_plan: bool,
    archive_path: Option<PathBuf>,
}

struct CheckReport {
    passed: bool,
    summary: String,
}

struct AuditReport {
    classifications: Vec<FindingClassification>,
}

struct FindingDisposition<'a> {
    actionable: Vec<&'a Finding>,
    not_actionable: Vec<&'a Finding>,
    unsupported: Vec<&'a Finding>,
    ambiguous: Vec<&'a Finding>,
}

impl WorkflowContext<'_> {
    fn next_log_index(&mut self) -> Result<usize> {
        self.log_index = self
            .log_index
            .checked_add(1)
            .ok_or_else(|| SpecLoopError::Workflow("run log counter overflowed".to_owned()))?;
        Ok(self.log_index)
    }
}

/// Runs one selected workflow from the current working directory.
pub fn run(cli: &Cli) -> Result<()> {
    cli.validate()?;
    let prepared = prepare_run(cli)?;
    if cli.is_dry_run() {
        print_dry_run(cli, &prepared);
        return Ok(());
    }
    let root = prepared.root;
    let profile = prepared.profile;
    let mut candidate = prepared.candidate;
    let mut files = prepared.files;
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
        log_index: 0,
        integrity: prepared.integrity,
        backend,
    };
    println!("Run logs: {}", run_dir.root.display());
    if cli.is_plan() {
        return run_plan_workflow(&mut context, cli, candidate, files);
    }

    let mut completed = BTreeSet::new();
    if cli.continue_requested() {
        for prior in discovery::discover_specs(&root, &profile)? {
            if prior.id == candidate.id {
                break;
            }
            completed.insert(prior.id);
        }
    }
    let mut review_resume = cli.is_review();
    loop {
        run_implementation_workflow(
            &mut context,
            &candidate,
            &files.plan_path,
            files.archive_path.as_deref(),
            review_resume,
        )?;
        completed.insert(candidate.id.clone());
        if !cli.continue_requested() {
            break;
        }
        let Some(next) = discovery::select_spec(&root, &profile, None, &completed)? else {
            println!("No further eligible specifications.");
            break;
        };
        files = prepare_candidate_files(&root, &profile, &next, false)?;
        validate_action_executables(&root, &profile, &next, &files)?;
        context.integrity = InputIntegrity::capture(&root, &next.path, &files.plan_path, false)?;
        candidate = next;
        review_resume = false;
    }
    println!("Run logs retained at {}", run_dir.root.display());
    Ok(())
}

struct PreparedRun {
    root: PathBuf,
    profile: Profile,
    candidate: SpecCandidate,
    files: CandidateFiles,
    integrity: InputIntegrity,
}

fn prepare_run(cli: &Cli) -> Result<PreparedRun> {
    let root = std::env::current_dir()
        .map_err(|source| io_error("resolve current directory", ".", source))?;
    let profile_path = root.join(".spec-loop.toml");
    if !profile_path.is_file() {
        return Err(SpecLoopError::Config(format!(
            "required profile is missing from current directory: {}; run Spec Loop from project root",
            profile_path.display()
        )));
    }
    let profile = Profile::load(&profile_path)?;
    validate_resources(&root, &profile)?;
    let candidate = discovery::select_spec(&root, &profile, cli.spec.as_deref(), &BTreeSet::new())?
        .ok_or_else(|| SpecLoopError::Workflow("no eligible specification found".to_owned()))?;
    let files = prepare_candidate_files(&root, &profile, &candidate, cli.is_plan())?;
    validate_check_executables(&root, &profile, &candidate, &files.plan_path)?;
    if !cli.is_plan() {
        validate_action_executables(&root, &profile, &candidate, &files)?;
    }
    let integrity = InputIntegrity::capture(
        &root,
        &candidate.path,
        &files.plan_path,
        cli.is_plan() && !(cli.continue_requested() && files.has_plan),
    )?;
    Ok(PreparedRun {
        root,
        profile,
        candidate,
        files,
        integrity,
    })
}

fn prepare_candidate_files(
    root: &Path,
    profile: &Profile,
    candidate: &SpecCandidate,
    plan_mode: bool,
) -> Result<CandidateFiles> {
    let spec_absolute = root.join(&candidate.path);
    let plan_path = prompts::plan_path(root, profile, &candidate.id, &spec_absolute)?;
    let has_plan = plan_exists(root, &plan_path)?;
    if !plan_mode && !has_plan {
        return Err(SpecLoopError::Config(format!(
            "required plan is missing: {}; run `spec-loop --spec {} --plan`",
            plan_path.display(),
            candidate.id
        )));
    }
    let archive_path = if plan_mode {
        None
    } else {
        archive_destination(root, profile, candidate)?
    };
    Ok(CandidateFiles {
        plan_path,
        has_plan,
        archive_path,
    })
}

fn validate_resources(root: &Path, profile: &Profile) -> Result<()> {
    for path in profile
        .prompts
        .plan_author
        .iter()
        .chain(&profile.prompts.plan_review)
        .chain(&profile.prompts.plan_audit)
        .chain(&profile.prompts.plan_review_clarification)
        .chain(&profile.prompts.implementation)
        .chain(&profile.prompts.review)
        .chain(&profile.prompts.review_audit)
        .chain(&profile.prompts.review_clarification)
    {
        paths::validate_regular_file(root, path, "prompts")?;
    }
    validate_executable(root, &profile.agent.executable, "agent")?;
    Ok(())
}

fn validate_check_executables(
    root: &Path,
    profile: &Profile,
    candidate: &SpecCandidate,
    plan_path: &Path,
) -> Result<()> {
    for check in &profile.checks {
        let argv = expand_argv(&check.argv, &candidate.id, &candidate.path, plan_path);
        let Some(program) = argv.first() else {
            return Err(SpecLoopError::Config(format!(
                "check {:?} has an empty argv",
                check.name
            )));
        };
        validate_executable(root, Path::new(program), &format!("check {:?}", check.name))?;
    }
    Ok(())
}

fn validate_action_executables(
    root: &Path,
    profile: &Profile,
    candidate: &SpecCandidate,
    files: &CandidateFiles,
) -> Result<()> {
    let spec_path = files.archive_path.as_deref().unwrap_or(&candidate.path);
    for action in profile
        .actions
        .on_review_success
        .iter()
        .chain(&profile.actions.on_finish)
    {
        let program = action.argv.first().ok_or_else(|| {
            SpecLoopError::Config(format!("action {:?} has an empty argv", action.name))
        })?;
        let expanded = expand_template(program, &candidate.id, spec_path, Some(&files.plan_path));
        let executable = if let Some(archive_path) = files.archive_path.as_deref()
            && same_path(root, Path::new(&expanded), archive_path)
        {
            // Archive rename preserves the selected file's executable mode, but its target does
            // not exist until after review. Validate source before agent invocation instead.
            expand_template(
                program,
                &candidate.id,
                &candidate.path,
                Some(&files.plan_path),
            )
        } else {
            expanded
        };
        validate_executable(
            root,
            Path::new(&executable),
            &format!("action {:?}", action.name),
        )?;
    }
    Ok(())
}

fn same_path(root: &Path, left: &Path, right: &Path) -> bool {
    fn normalized(root: &Path, path: &Path) -> PathBuf {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let mut result = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    result.pop();
                }
                Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                    result.push(component.as_os_str());
                }
            }
        }
        result
    }

    normalized(root, left) == normalized(root, right)
}

fn validate_executable(root: &Path, executable: &Path, field: &str) -> Result<()> {
    let has_path_component = executable.is_absolute()
        || executable
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty());
    let candidates = if has_path_component {
        vec![if executable.is_absolute() {
            executable.to_path_buf()
        } else {
            root.join(executable)
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
                    root.join(directory).join(executable)
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

fn plan_exists(root: &Path, path: &Path) -> Result<bool> {
    let absolute = resolve_optional(root, path);
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
    let filename = candidate.path.file_name().ok_or_else(|| {
        SpecLoopError::Config(format!(
            "selected spec has no filename: {}",
            candidate.path.display()
        ))
    })?;
    let destination = directory.join(filename);
    let destination_text = paths::path_to_utf8(&destination, "archive destination")?;
    let id_regex = regex::Regex::new(&profile.specs.id_regex)
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
    let parent = root
        .join(&destination)
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| {
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

fn print_dry_run(cli: &Cli, prepared: &PreparedRun) {
    println!("Project root: {}", prepared.root.display());
    println!(
        "Profile: {}",
        prepared.root.join(".spec-loop.toml").display()
    );
    println!(
        "Spec: {} ({})",
        prepared.candidate.id,
        prepared.candidate.path.display()
    );
    println!(
        "Mode: {}",
        if cli.is_plan() {
            "plan"
        } else if cli.is_review() {
            "review"
        } else {
            "implementation"
        }
    );
    println!(
        "Review-context argv: {:?}",
        prepared.profile.review.context.argv
    );
    println!(
        "Review-context working directory: {}",
        prepared.root.display()
    );
    println!("Plan: {}", prepared.files.plan_path.display());
    if let Some(path) = &prepared.files.archive_path {
        println!("Archive destination: {}", path.display());
    }
    for check in &prepared.profile.checks {
        let argv = expand_argv(
            &check.argv,
            &prepared.candidate.id,
            &prepared.candidate.path,
            &prepared.files.plan_path,
        );
        println!(
            "Check {:?}: argv={argv:?}, cwd={}, timeout={}",
            check.name,
            prepared.root.display(),
            timeout_description(check.timeout_seconds)
        );
    }
    for (label, actions) in [
        (
            "on_review_success",
            &prepared.profile.actions.on_review_success,
        ),
        ("on_finish", &prepared.profile.actions.on_finish),
    ] {
        for action in actions {
            let spec_path = prepared
                .files
                .archive_path
                .as_deref()
                .unwrap_or(&prepared.candidate.path);
            let argv = expand_argv(
                &action.argv,
                &prepared.candidate.id,
                spec_path,
                &prepared.files.plan_path,
            );
            println!(
                "Action {label} {:?}: argv={argv:?}, cwd={}, timeout={}{}",
                action.name,
                prepared.root.display(),
                timeout_description(action.timeout_seconds),
                if cli.is_plan() {
                    " (not run in plan mode)"
                } else {
                    ""
                }
            );
        }
    }
    println!(
        "Maximum calls per Pi role: {} (0 means unlimited)",
        prepared.profile.completion.max_passes
    );
    println!("Dry run: no agents, checks, or actions invoked.");
}

fn expand_argv(argv: &[String], spec_id: &str, spec_path: &Path, plan_path: &Path) -> Vec<String> {
    argv.iter()
        .map(|argument| expand_template(argument, spec_id, spec_path, Some(plan_path)))
        .collect()
}

fn timeout_description(timeout_seconds: Option<u64>) -> String {
    timeout_seconds.map_or_else(|| "unlimited".to_owned(), |seconds| format!("{seconds}s"))
}

fn run_plan_workflow(
    context: &mut WorkflowContext<'_>,
    cli: &Cli,
    mut candidate: SpecCandidate,
    mut files: CandidateFiles,
) -> Result<()> {
    let mut completed = BTreeSet::new();
    loop {
        if cli.continue_requested() && files.has_plan {
            context.integrity.verify(context.root)?;
            println!("Skipping spec {}: plan already exists.", candidate.id);
        } else {
            run_plan_loop(context, &candidate, &files.plan_path)?;
            context.integrity.verify(context.root)?;
            println!("Plan approved for spec {}.", candidate.id);
        }
        if !cli.continue_requested() {
            return Ok(());
        }
        completed.insert(candidate.id.clone());
        let Some(next) = discovery::select_spec(context.root, context.profile, None, &completed)?
        else {
            println!("No further specifications need plans.");
            return Ok(());
        };
        let next_files = prepare_candidate_files(context.root, context.profile, &next, true)?;
        context.integrity = InputIntegrity::capture(
            context.root,
            &next.path,
            &next_files.plan_path,
            !next_files.has_plan,
        )?;
        candidate = next;
        files = next_files;
    }
}

fn run_plan_loop(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
) -> Result<()> {
    let mut feedback = None;
    loop {
        let response = call_typed::<PlanAuthorResponse>(
            context,
            AgentRequest {
                candidate,
                role: PromptRole::PlanAuthor,
                plan_path: Some(plan_path),
                feedback: feedback.as_deref(),
                validation_feedback: None,
                checks: "",
            },
            |value| match &value.result {
                Readiness::ReadyForReview => Ok(()),
                Readiness::Blocked { reason } => protocol::validate_text(reason, "blocked reason"),
            },
        )?;
        match response.result {
            Readiness::Blocked { reason } => return Err(role_blocked("plan author", &reason)),
            Readiness::ReadyForReview => {}
        }
        if !plan_exists(context.root, plan_path)? {
            feedback = Some(format!(
                "{{\"status\":\"ready_for_review\"}} was returned, but required plan file is missing: {}. Create it before reporting readiness.",
                plan_path.display()
            ));
            continue;
        }
        let review = review_plan(context, candidate, plan_path)?;
        match review.result {
            ReviewResult::Blocked { reason } => return Err(role_blocked("plan reviewer", &reason)),
            ReviewResult::NoIssues { .. } => return Ok(()),
            ReviewResult::Issues { ref findings } => {
                let report = audit_findings(
                    context,
                    AuditRequest {
                        candidate,
                        plan_path,
                        plan: true,
                        findings,
                        review: &review,
                        checks: "",
                    },
                )?;
                let findings = disposition(findings, &report)?;
                if !findings.unsupported.is_empty() {
                    println!(
                        "Plan auditor classified findings unsupported: {}",
                        ids(&findings.unsupported)
                    );
                }
                if !findings.not_actionable.is_empty() {
                    println!(
                        "Plan auditor found no required correction for findings: {}",
                        ids(&findings.not_actionable)
                    );
                }
                if !findings.ambiguous.is_empty() {
                    return Err(SpecLoopError::Workflow(format!(
                        "plan finding audit remains ambiguous for finding IDs {}; inspect run logs at {}",
                        ids(&findings.ambiguous),
                        context.run_dir.root.display()
                    )));
                }
                if findings.actionable.is_empty() {
                    return Ok(());
                }
                let handoff = PlanRepairInput {
                    actionable_findings: findings.actionable,
                    classifications: report
                        .classifications
                        .iter()
                        .filter(|item| {
                            item.classification == Classification::Supported
                                && item.actionability == Actionability::Required
                        })
                        .collect(),
                };
                feedback = Some(canonical_json(&handoff)?);
            }
        }
    }
}

fn review_plan(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
) -> Result<PlanReviewResponse> {
    call_typed(
        context,
        AgentRequest {
            candidate,
            role: PromptRole::PlanReview,
            plan_path: Some(plan_path),
            feedback: None,
            validation_feedback: None,
            checks: "",
        },
        |response: &PlanReviewResponse| match &response.result {
            ReviewResult::NoIssues { findings } => {
                if !findings.is_empty() {
                    return Err(SpecLoopError::Agent(
                        "no_issues requires empty findings".to_owned(),
                    ));
                }
                Ok(())
            }
            ReviewResult::Issues { findings } => protocol::validate_findings(findings),
            ReviewResult::Blocked { reason } => protocol::validate_text(reason, "blocked reason"),
        },
    )
}

fn run_implementation_workflow(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
    archive_path: Option<&Path>,
    mut review_resume: bool,
) -> Result<()> {
    let mut feedback: Option<String> = None;
    loop {
        if !review_resume || feedback.is_some() {
            let response = call_typed::<ImplementationResponse>(
                context,
                AgentRequest {
                    candidate,
                    role: PromptRole::Implementation,
                    plan_path: Some(plan_path),
                    feedback: feedback.as_deref(),
                    validation_feedback: None,
                    checks: "",
                },
                |value| match &value.result {
                    Readiness::ReadyForReview => Ok(()),
                    Readiness::Blocked { reason } => {
                        protocol::validate_text(reason, "blocked reason")
                    }
                },
            )?;
            match response.result {
                Readiness::Blocked { reason } => {
                    return Err(role_blocked("implementation", &reason));
                }
                Readiness::ReadyForReview => {}
            }
        }
        review_resume = false;
        context.integrity.verify(context.root)?;
        commands::report_progress(&format!(
            "Stage: automated checks for spec {}",
            candidate.id
        ))?;
        let checks = run_checks(context, candidate, plan_path)?;
        println!("{}", checks.summary);
        if !checks.passed {
            feedback = Some(format!(
                "Configured checks failed. Fix reported failures, then return ready_for_review.\n{}",
                checks.summary
            ));
            continue;
        }
        match review_implementation(context, candidate, plan_path, &checks.summary)? {
            ReviewDisposition::Approved => {
                complete_spec(context, candidate, plan_path, archive_path)?;
                return Ok(());
            }
            ReviewDisposition::Repair(reason) => {
                feedback = Some(reason);
            }
        }
    }
}

enum ReviewDisposition {
    Approved,
    Repair(String),
}

fn review_implementation(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
    checks: &str,
) -> Result<ReviewDisposition> {
    let review = call_typed::<ImplementationReviewResponse>(
        context,
        AgentRequest {
            candidate,
            role: PromptRole::Review,
            plan_path: Some(plan_path),
            feedback: None,
            validation_feedback: None,
            checks,
        },
        |response| match &response.result {
            ReviewResult::NoIssues { findings } => {
                if !findings.is_empty() {
                    return Err(SpecLoopError::Agent(
                        "no_issues requires empty findings".to_owned(),
                    ));
                }
                Ok(())
            }
            ReviewResult::Issues { findings } => protocol::validate_findings(findings),
            ReviewResult::Blocked { reason } => protocol::validate_text(reason, "blocked reason"),
        },
    )?;
    match review.result {
        ReviewResult::Blocked { reason } => Err(role_blocked("implementation reviewer", &reason)),
        ReviewResult::NoIssues { .. } => Ok(ReviewDisposition::Approved),
        ReviewResult::Issues { ref findings } => {
            let report = audit_findings(
                context,
                AuditRequest {
                    candidate,
                    plan_path,
                    plan: false,
                    findings,
                    review: &review,
                    checks,
                },
            )?;
            let findings = disposition(findings, &report)?;
            if !findings.not_actionable.is_empty() {
                println!(
                    "Review auditor found no required correction for findings: {}",
                    ids(&findings.not_actionable)
                );
            }
            if findings.actionable.is_empty() && findings.ambiguous.is_empty() {
                return Ok(ReviewDisposition::Approved);
            }
            let handoff = ImplementationRepairInput {
                actionable_findings: findings.actionable,
                ambiguous_findings: findings.ambiguous,
                audit: &report.classifications,
            };
            let message = canonical_json(&handoff)?;
            Ok(ReviewDisposition::Repair(format!(
                "Repair only findings marked supported and required in the audit. Follow audit rationale for the smallest sufficient correct fix, even when substantial. Do not change code for supported findings marked not_required. Resolve ambiguous findings from available evidence; do not treat ambiguity as authorization to change code, and return blocked if consequential ambiguity remains.\n{message}"
            )))
        }
    }
}

struct AuditRequest<'a, R> {
    candidate: &'a SpecCandidate,
    plan_path: &'a Path,
    plan: bool,
    findings: &'a [Finding],
    review: &'a R,
    checks: &'a str,
}

impl<R> Copy for AuditRequest<'_, R> {}

impl<R> Clone for AuditRequest<'_, R> {
    fn clone(&self) -> Self {
        *self
    }
}

fn audit_findings<R: Serialize>(
    context: &mut WorkflowContext<'_>,
    request: AuditRequest<'_, R>,
) -> Result<AuditReport> {
    let initial = invoke_audit(context, request, None)?;
    let findings = disposition(request.findings, &initial)?;
    let requested = findings
        .unsupported
        .iter()
        .chain(&findings.ambiguous)
        .map(|finding| finding.id)
        .collect::<Vec<_>>();
    if requested.is_empty() {
        return Ok(initial);
    }
    clarify_findings(context, request, initial, &requested)
}

fn clarify_findings<R: Serialize>(
    context: &mut WorkflowContext<'_>,
    request: AuditRequest<'_, R>,
    initial: AuditReport,
    requested: &[u64],
) -> Result<AuditReport> {
    let evidence = request_evidence(context, request, requested)?;
    let cited_ids = evidence
        .iter()
        .filter(|entry| !entry.citations.is_empty())
        .map(|entry| entry.finding_id)
        .collect::<BTreeSet<_>>();
    if cited_ids.is_empty() {
        return Ok(initial);
    }
    let evidence_input = AuditEvidenceInput {
        findings: request
            .findings
            .iter()
            .filter(|finding| cited_ids.contains(&finding.id))
            .cloned()
            .collect(),
        evidence: evidence
            .into_iter()
            .filter(|entry| cited_ids.contains(&entry.finding_id))
            .collect(),
    };
    let clarified_request = AuditRequest {
        findings: &evidence_input.findings,
        ..request
    };
    let clarified = invoke_audit(context, clarified_request, Some(&evidence_input))?;
    Ok(merge_audit_reports(initial, clarified))
}

fn request_evidence<R: Serialize>(
    context: &mut WorkflowContext<'_>,
    request: AuditRequest<'_, R>,
    requested: &[u64],
) -> Result<Vec<crate::protocol::FindingEvidence>> {
    let role = if request.plan {
        PromptRole::PlanClarification
    } else {
        PromptRole::ReviewClarification
    };
    let requested_findings = request
        .findings
        .iter()
        .filter(|finding| requested.contains(&finding.id))
        .cloned()
        .collect::<Vec<_>>();
    let clarification = ClarificationInput {
        review: request.review,
        findings: &requested_findings,
        requested_finding_ids: requested,
    };
    let request_json = canonical_json(&clarification)?;
    let agent_request = AgentRequest {
        candidate: request.candidate,
        role,
        plan_path: Some(request.plan_path),
        feedback: Some(&request_json),
        validation_feedback: None,
        checks: if request.plan { "" } else { request.checks },
    };
    let evidence = if request.plan {
        let response =
            call_typed::<PlanClarificationResponse>(context, agent_request, |value| match &value
                .result
            {
                EvidenceResult::Complete { evidence } => {
                    protocol::validate_evidence(evidence, requested)
                }
                EvidenceResult::Blocked { reason } => {
                    protocol::validate_text(reason, "blocked reason")
                }
            })?;
        match response.result {
            EvidenceResult::Blocked { reason } => {
                return Err(role_blocked("evidence clarification", &reason));
            }
            EvidenceResult::Complete { evidence } => evidence,
        }
    } else {
        let response =
            call_typed::<ImplementationClarificationResponse>(context, agent_request, |value| {
                match &value.result {
                    EvidenceResult::Complete { evidence } => {
                        protocol::validate_evidence(evidence, requested)
                    }
                    EvidenceResult::Blocked { reason } => {
                        protocol::validate_text(reason, "blocked reason")
                    }
                }
            })?;
        match response.result {
            EvidenceResult::Blocked { reason } => {
                return Err(role_blocked("evidence clarification", &reason));
            }
            EvidenceResult::Complete { evidence } => evidence,
        }
    };
    Ok(evidence)
}

fn merge_audit_reports(initial: AuditReport, clarified: AuditReport) -> AuditReport {
    let mut classifications: BTreeMap<u64, FindingClassification> = initial
        .classifications
        .into_iter()
        .map(|item| (item.finding_id, item))
        .collect();
    for item in clarified.classifications {
        classifications.insert(item.finding_id, item);
    }
    AuditReport {
        classifications: classifications.into_values().collect(),
    }
}

fn invoke_audit(
    context: &mut WorkflowContext<'_>,
    request: AuditRequest<'_, impl Serialize>,
    evidence: Option<&AuditEvidenceInput>,
) -> Result<AuditReport> {
    let role = if request.plan {
        PromptRole::PlanAudit
    } else {
        PromptRole::ReviewAudit
    };
    let handoff = AuditInput {
        review: request.review,
        findings: request.findings,
        evidence,
    };
    let request_json = canonical_json(&handoff)?;
    let agent_request = AgentRequest {
        candidate: request.candidate,
        role,
        plan_path: Some(request.plan_path),
        feedback: Some(&request_json),
        validation_feedback: None,
        checks: if request.plan { "" } else { request.checks },
    };
    if request.plan {
        let response =
            call_typed::<PlanAuditResponse>(context, agent_request, |value| match &value.result {
                AuditResult::Complete { classifications } => {
                    protocol::validate_classifications(classifications, request.findings)
                }
                AuditResult::Blocked { reason } => {
                    protocol::validate_text(reason, "blocked reason")
                }
            })?;
        match response.result {
            AuditResult::Blocked { reason } => Err(role_blocked("plan auditor", &reason)),
            AuditResult::Complete { classifications } => Ok(AuditReport { classifications }),
        }
    } else {
        let response = call_typed::<ImplementationAuditResponse>(
            context,
            agent_request,
            |value| match &value.result {
                AuditResult::Complete { classifications } => {
                    protocol::validate_classifications(classifications, request.findings)
                }
                AuditResult::Blocked { reason } => {
                    protocol::validate_text(reason, "blocked reason")
                }
            },
        )?;
        match response.result {
            AuditResult::Blocked { reason } => {
                Err(role_blocked("implementation finding auditor", &reason))
            }
            AuditResult::Complete { classifications } => Ok(AuditReport { classifications }),
        }
    }
}

fn disposition<'a>(
    findings: &'a [Finding],
    report: &AuditReport,
) -> Result<FindingDisposition<'a>> {
    protocol::validate_classifications(&report.classifications, findings)?;
    let mut disposition = FindingDisposition {
        actionable: Vec::new(),
        not_actionable: Vec::new(),
        unsupported: Vec::new(),
        ambiguous: Vec::new(),
    };
    for (finding, assessment) in findings.iter().zip(&report.classifications) {
        match (assessment.classification, assessment.actionability) {
            (Classification::Unsupported, Actionability::NotRequired) => {
                disposition.unsupported.push(finding);
            }
            (Classification::Supported, Actionability::Required) => {
                disposition.actionable.push(finding);
            }
            (Classification::Supported, Actionability::NotRequired) => {
                disposition.not_actionable.push(finding);
            }
            (Classification::Supported | Classification::Ambiguous, Actionability::Ambiguous) => {
                disposition.ambiguous.push(finding);
            }
            (Classification::Unsupported, Actionability::Required | Actionability::Ambiguous)
            | (Classification::Ambiguous, Actionability::Required | Actionability::NotRequired) => {
                return Err(SpecLoopError::Agent(
                    "audit classification and actionability conflict".to_owned(),
                ));
            }
        }
    }
    Ok(disposition)
}

fn ids(findings: &[&Finding]) -> String {
    findings
        .iter()
        .map(|finding| finding.id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn complete_spec(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
    archive_path: Option<&Path>,
) -> Result<()> {
    context.integrity.verify(context.root)?;
    let mut completed_candidate = candidate.clone();
    if let Some(destination) = archive_path {
        archive_spec(context.root, candidate, destination)?;
        context.integrity.relocate_spec(context.root, destination)?;
        completed_candidate.path = destination.to_path_buf();
    }
    println!("Review approved for spec {}.", candidate.id);
    run_actions(
        context,
        &context.profile.actions.on_review_success,
        &completed_candidate,
        plan_path,
    )?;
    run_actions(
        context,
        &context.profile.actions.on_finish,
        &completed_candidate,
        plan_path,
    )?;
    Ok(())
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

fn run_actions(
    context: &mut WorkflowContext<'_>,
    actions: &[ActionConfig],
    candidate: &SpecCandidate,
    plan_path: &Path,
) -> Result<()> {
    for action in actions {
        context.integrity.verify(context.root)?;
        let argv = expand_argv(&action.argv, &candidate.id, &candidate.path, plan_path);
        run_action(context, action, &argv)?;
        context.integrity.verify(context.root)?;
    }
    Ok(())
}

fn run_action(
    context: &mut WorkflowContext<'_>,
    action: &ActionConfig,
    argv: &[String],
) -> Result<()> {
    let Some(program_text) = argv.first() else {
        return Err(SpecLoopError::Config(format!(
            "action {:?} has empty argv",
            action.name
        )));
    };
    let program = PathBuf::from(program_text);
    let arguments = &argv[1..];
    commands::report_progress(&format!(
        "Action {:?}\nCommand argv: {argv:?}\nWorking directory: {}",
        action.name,
        context.root.display()
    ))?;
    let timeout = action.timeout_seconds.map(Duration::from_secs);
    let outcome = commands::run_process(
        &program,
        arguments,
        context.root,
        timeout,
        &context.cancelled,
    );
    let (success, status, termination, stdout, stderr) = match outcome {
        Ok(output) => (
            status_success(&output),
            output
                .status
                .map_or_else(|| "no exit status".to_owned(), |value| value.to_string()),
            Some(output.termination),
            output.stdout,
            output.stderr,
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
    let mut log = format!(
        "Action: {}\nCommand argv: {argv:?}\nWorking directory: {}\nStatus: {status}\n--- stdout ({} bytes) ---\n",
        action.name,
        context.root.display(),
        stdout.len()
    )
    .into_bytes();
    log.extend_from_slice(&stdout);
    log.extend_from_slice(format!("\n--- stderr ({} bytes) ---\n", stderr.len()).as_bytes());
    log.extend_from_slice(&stderr);
    let log_name = format!("action-{:04}.log", context.next_log_index()?);
    let log_path = context.run_dir.write_log(&log_name, &log)?;
    println!(
        "Action {:?}: {} (log: {})",
        action.name,
        if success { "completed" } else { "FAILED" },
        log_path.display()
    );
    if termination == Some(ProcessTermination::Interrupted) {
        return Err(SpecLoopError::Interrupted);
    }
    if !success {
        return Err(SpecLoopError::Workflow(format!(
            "action {:?} failed with {status}{}; prior effects preserved; log: {}",
            action.name,
            termination.map_or_else(String::new, |value| match value {
                ProcessTermination::TimedOut => " (timed out)".to_owned(),
                ProcessTermination::Interrupted => " (interrupted)".to_owned(),
                ProcessTermination::Exited => String::new(),
            }),
            log_path.display()
        )));
    }
    Ok(())
}

fn run_checks(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
) -> Result<CheckReport> {
    let mut all_passed = true;
    let mut summary = String::new();
    if context.profile.checks.is_empty() {
        summary.push_str("No automated checks ran (profile has no configured checks).\n");
    }
    for check in &context.profile.checks {
        context.integrity.verify(context.root)?;
        let result = run_check(context, candidate, plan_path, check)?;
        context.integrity.verify(context.root)?;
        all_passed &= result.0;
        summary.push_str(&result.1);
        summary.push('\n');
    }
    Ok(CheckReport {
        passed: all_passed,
        summary,
    })
}

fn run_check(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
    check: &CheckConfig,
) -> Result<(bool, String)> {
    commands::report_progress(&format!("Stage: check {}", check.name))?;
    let argv = expand_argv(&check.argv, &candidate.id, &candidate.path, plan_path);
    let Some(program_text) = argv.first() else {
        return Err(SpecLoopError::Config(format!(
            "check {:?} has empty argv",
            check.name
        )));
    };
    let program = PathBuf::from(program_text);
    let arguments = &argv[1..];
    commands::report_progress(&format!(
        "Command argv: {argv:?}\nWorking directory: {}",
        context.root.display()
    ))?;
    let timeout = check.timeout_seconds.map(Duration::from_secs);
    let outcome = commands::run_process(
        &program,
        arguments,
        context.root,
        timeout,
        &context.cancelled,
    );
    let (success, status, termination, stdout, stderr) = match outcome {
        Ok(output) => (
            status_success(&output),
            output
                .status
                .map_or_else(|| "no exit status".to_owned(), |value| value.to_string()),
            Some(output.termination),
            output.stdout,
            output.stderr,
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
    let mut log = format!(
        "Check: {}\nCommand argv: {argv:?}\nWorking directory: {}\nStatus: {status}\n--- stdout ({} bytes) ---\n",
        check.name,
        context.root.display(),
        stdout.len()
    )
    .into_bytes();
    log.extend_from_slice(&stdout);
    log.extend_from_slice(format!("\n--- stderr ({} bytes) ---\n", stderr.len()).as_bytes());
    log.extend_from_slice(&stderr);
    let log_name = format!("check-{:04}.log", context.next_log_index()?);
    let log_path = context.run_dir.write_log(&log_name, &log)?;
    println!(
        "Check {}: {} (log: {})",
        check.name,
        if success { "passed" } else { "FAILED" },
        log_path.display()
    );
    if termination == Some(ProcessTermination::Interrupted) {
        return Err(SpecLoopError::Interrupted);
    }
    let summary = format!(
        "Check: {}\nCommand argv: {argv:?}\nStatus: {status}{}\nLog: {}\n{}",
        check.name,
        termination.map_or_else(String::new, |value| match value {
            ProcessTermination::TimedOut => " (timed out)".to_owned(),
            ProcessTermination::Interrupted => " (interrupted)".to_owned(),
            ProcessTermination::Exited => String::new(),
        }),
        log_path.display(),
        output_excerpt(&stdout, &stderr)
    );
    Ok((success, summary))
}

fn output_excerpt(stdout: &[u8], stderr: &[u8]) -> String {
    let mut summary = String::new();
    append_output_excerpt(&mut summary, "stdout", stdout);
    append_output_excerpt(&mut summary, "stderr", stderr);
    summary
}

fn append_output_excerpt(summary: &mut String, stream: &str, output: &[u8]) {
    const MAX_LINES: usize = 4;
    const MAX_LINE_CHARS: usize = 300;

    let decoded = String::from_utf8_lossy(output);
    let lines: Vec<&str> = decoded
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return;
    }
    let omitted = lines.len().saturating_sub(MAX_LINES);
    summary.push_str(stream);
    summary.push_str(" excerpt:\n");
    if omitted > 0 {
        summary.push_str("... ");
        summary.push_str(&omitted.to_string());
        summary.push_str(" earlier lines omitted ...\n");
    }
    for line in lines.iter().skip(omitted) {
        let mut characters = line.chars();
        summary.extend(characters.by_ref().take(MAX_LINE_CHARS));
        if characters.next().is_some() {
            summary.push_str(" ... [line truncated]");
        }
        summary.push('\n');
    }
}

fn call_typed<T>(
    context: &mut WorkflowContext<'_>,
    request: AgentRequest<'_>,
    mut validate: impl FnMut(&T) -> Result<()>,
) -> Result<T>
where
    T: serde::de::DeserializeOwned + ResponseFields + Serialize,
{
    let mut validation_feedback = request.validation_feedback.map(str::to_owned);
    loop {
        let attempt = AgentRequest {
            validation_feedback: validation_feedback.as_deref(),
            ..request
        };
        let response = call_agent(context, attempt)?;
        match protocol::parse::<T>(&response, request.role.protocol_name()).and_then(|parsed| {
            validate(&parsed)?;
            Ok(parsed)
        }) {
            Ok(parsed) => return Ok(parsed),
            Err(error) => {
                commands::report_progress(&format!(
                    "Agent {} response failed validation ({error}); retrying same role.",
                    role_display(request.role)
                ))?;
                validation_feedback = Some(format!(
                    "Previous complete response failed typed JSON validation: {error}\nPrevious response:\n{response}\nReturn one corrected JSON object only."
                ));
            }
        }
    }
}

fn call_agent(context: &mut WorkflowContext<'_>, request: AgentRequest<'_>) -> Result<String> {
    let role = request.role;
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
        let call_index = context.next_log_index()?;
        let attempt = context.role_calls.get(&role).copied().ok_or_else(|| {
            SpecLoopError::Workflow(format!("missing attempt count for {}", role_display(role)))
        })?;
        commands::report_progress(&format!(
            "Stage: agent {} (attempt {attempt}; spec {})",
            role_display(role),
            request.candidate.id
        ))?;
        match invoke_and_log_agent(context, call_index, role, &prompt)? {
            Ok(response) => return Ok(response.response),
            Err(SpecLoopError::Interrupted) => return Err(SpecLoopError::Interrupted),
            Err(error) => {
                println!("Agent {} attempt failed: {error}", role_display(role));
                retry_feedback = Some(format!(
                    "Previous {} attempt failed: {error}. Retry same task; preserve repository changes.",
                    role_display(role)
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
            | PromptRole::PlanAudit
            | PromptRole::PlanClarification => format!("--plan --spec {spec_id}"),
            PromptRole::Implementation => format!("--spec {spec_id}"),
            PromptRole::Review | PromptRole::ReviewAudit | PromptRole::ReviewClarification => {
                format!("--review --spec {spec_id}")
            }
        };
        return Err(SpecLoopError::Workflow(format!(
            "{} reached profile completion.max_passes limit ({}); changes preserved. Increase completion.max_passes and resume with `{resume}`.",
            role_display(role),
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

fn invoke_and_log_agent(
    context: &mut WorkflowContext<'_>,
    call_index: usize,
    role: PromptRole,
    prompt: &str,
) -> Result<Result<AgentResponse>> {
    let outcome = match context.backend.run(
        context.root,
        context.profile,
        prompt,
        role,
        &context.cancelled,
    ) {
        Ok(run) => {
            let stdout_name = format!("agent-{call_index:04}-{}.stdout.jsonl", role_display(role));
            let stderr_name = format!("agent-{call_index:04}-{}.stderr.log", role_display(role));
            context.run_dir.write_log(&stdout_name, &run.stdout)?;
            context.run_dir.write_log(&stderr_name, &run.stderr)?;
            println!(
                "Agent {} logs: {}, {}",
                role_display(role),
                context.run_dir.log_path(&stdout_name).display(),
                context.run_dir.log_path(&stderr_name).display()
            );
            match &run.outcome {
                Ok(response) => {
                    if let Some(model) = &response.selected_model {
                        println!("Agent {} selected model: {model}", role_display(role));
                    }
                    if let Some(model) = &response.response_model {
                        println!(
                            "Agent {} provider-reported model: {model}",
                            role_display(role)
                        );
                    }
                    context.run_dir.write_log(
                        &format!("agent-{call_index:04}-{}.response.json", role_display(role)),
                        response.response.as_bytes(),
                    )?;
                    println!(
                        "--- Agent {} final response ---\n{}",
                        role_display(role),
                        response.response
                    );
                }
                Err(error) => println!("Agent {} diagnostics: {error}", role_display(role)),
            }
            run.outcome
        }
        Err(error) => Err(error),
    };
    context
        .integrity
        .agent_finished(context.root, role == PromptRole::PlanAuthor)?;
    Ok(outcome)
}

const fn role_display(role: PromptRole) -> &'static str {
    match role {
        PromptRole::PlanAuthor => "plan-author",
        PromptRole::PlanReview => "plan-review",
        PromptRole::PlanAudit => "plan-audit",
        PromptRole::PlanClarification => "plan-review-clarification",
        PromptRole::Implementation => "implementation",
        PromptRole::Review => "review",
        PromptRole::ReviewAudit => "review-audit",
        PromptRole::ReviewClarification => "review-clarification",
    }
}

fn role_blocked(role: &str, reason: &str) -> SpecLoopError {
    SpecLoopError::Workflow(format!("{role} blocked: {reason}; changes preserved"))
}

fn canonical_json(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|error| SpecLoopError::Agent(format!("serialize typed handoff: {error}")))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{
        CandidateFiles, archive_destination, is_executable_file, validate_action_executables,
        validate_check_executables,
    };
    use crate::config::Profile;

    #[test]
    fn archive_path_is_filesystem_policy_without_git_checks() {
        let temporary = TempDir::new().expect("project");
        fs::create_dir_all(temporary.path().join("archive")).expect("archive directory");
        fs::create_dir_all(temporary.path().join("specs")).expect("spec directory");
        fs::write(temporary.path().join("specs/001.md"), "spec").expect("spec file");
        let profile: Profile = toml::from_str(
            "schema_version=2\n[specs]\nactive_globs=['specs/*.md']\nid_regex='^(?:specs|archive)/(?P<id>[0-9]+)\\.md$'\n[archive]\nenabled=true\ndestination='archive'\n",
        )
        .expect("profile");
        let candidate = crate::discovery::discover_specs(temporary.path(), &profile)
            .expect("discover spec")
            .into_iter()
            .next()
            .expect("candidate");
        assert_eq!(
            archive_destination(temporary.path(), &profile, &candidate)
                .expect("archive destination")
                .as_deref(),
            Some(Path::new("archive/001.md"))
        );
    }

    #[test]
    fn executable_check_rejects_missing_program() {
        assert!(!is_executable_file(Path::new("/no/such/program")));
    }

    #[cfg(unix)]
    #[test]
    fn check_executable_templates_are_expanded_before_preflight() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = TempDir::new().expect("project");
        fs::create_dir_all(temporary.path().join("specs")).expect("specs directory");
        fs::create_dir_all(temporary.path().join("tools")).expect("tools directory");
        fs::write(temporary.path().join("specs/001.md"), "spec").expect("spec");
        let executable = temporary.path().join("tools/check-001");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("check executable");
        let mut permissions = fs::metadata(&executable)
            .expect("check metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).expect("set executable mode");

        let profile: Profile = toml::from_str(
            "schema_version=2\n[specs]\nactive_globs=['specs/*.md']\nid_regex='^specs/(?P<id>[0-9]+)\\.md$'\n[[checks]]\nname='templated'\nargv=['tools/check-{spec_id}', '^[0-9]{3}$', '{literal']\n",
        )
        .expect("profile");
        let candidate = crate::discovery::discover_specs(temporary.path(), &profile)
            .expect("discover spec")
            .into_iter()
            .next()
            .expect("candidate");

        validate_check_executables(
            temporary.path(),
            &profile,
            &candidate,
            Path::new(".spec-loop/plans/001.md"),
        )
        .expect("preflight expanded executable and preserved literal brace args");
    }

    #[cfg(unix)]
    #[test]
    fn archived_spec_executable_is_preflighted_at_source_path() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = TempDir::new().expect("project");
        fs::create_dir_all(temporary.path().join("specs")).expect("specs directory");
        fs::create_dir_all(temporary.path().join("archive")).expect("archive directory");
        let source = temporary.path().join("specs/001.md");
        fs::write(&source, "#!/bin/sh\nexit 0\n").expect("executable spec");
        let mut permissions = fs::metadata(&source).expect("spec metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&source, permissions).expect("set executable mode");
        let profile: Profile = toml::from_str(
            "schema_version=2\n[specs]\nactive_globs=['specs/*.md']\nid_regex='^specs/(?P<id>[0-9]+)\\.md$'\n[archive]\nenabled=true\ndestination='archive'\n[[actions.on_review_success]]\nname='run selected spec'\nargv=['{spec_path}']\n",
        )
        .expect("profile");
        let candidate = crate::discovery::discover_specs(temporary.path(), &profile)
            .expect("discover spec")
            .into_iter()
            .next()
            .expect("candidate");
        let files = CandidateFiles {
            plan_path: PathBuf::from(".spec-loop/plans/001.md"),
            has_plan: true,
            archive_path: Some(PathBuf::from("archive/001.md")),
        };

        validate_action_executables(temporary.path(), &profile, &candidate, &files)
            .expect("preflight validates source before archive rename");
        assert!(!temporary.path().join("archive/001.md").exists());
    }
}
