use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::{self, AgentBackend};
use crate::cli::{Cli, WorkflowMode};
use crate::config::{AgentBackendKind, Profile};
use crate::discovery::{self, SpecCandidate};
use crate::error::{Result, SpecLoopError, io_error};
use crate::integrity::InputIntegrity;
use crate::output::Output;
use crate::paths::{self, expand_template, resolve_optional};
use crate::prompts::{self, PromptRole};
use crate::protocol::{Finding, FindingClassification};
use crate::run_dir::RunDir;

mod agents;
mod execution;
#[cfg(test)]
mod tests;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImplementationEntry {
    Implement,
    ResumeReview,
}

impl ImplementationEntry {
    const fn take_for_pass(&mut self) -> Self {
        std::mem::replace(self, Self::Implement)
    }
}

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
    output: Output,
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
    display_summary: String,
}

#[derive(Clone, Copy)]
enum ActionIntegrityPolicy {
    /// Check both protected inputs before and after each action.
    SelectedInputs,
    /// Check only the protected plan once finish actions begin.
    SelectedPlan,
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
    let root = std::env::current_dir()
        .map_err(|source| io_error("resolve current directory", ".", source))?;
    run_at(cli, root, None, true, None).map(|_| ())
}

fn run_at(
    cli: &Cli,
    root: PathBuf,
    backend_override: Option<Box<dyn AgentBackend>>,
    install_interrupt_handler: bool,
    run_dir_observer: Option<&mut Option<PathBuf>>,
) -> Result<Option<PathBuf>> {
    cli.validate()?;
    let prepared = prepare_run(cli, root)?;
    if cli.is_dry_run() {
        print_dry_run(
            cli,
            &prepared,
            &Output::new(cli.color_mode(), cli.verbose()),
        )?;
        return Ok(None);
    }
    let root = prepared.root;
    let profile = prepared.profile;
    let mut candidate = prepared.candidate;
    let mut files = prepared.files;
    let backend = backend_override.unwrap_or_else(|| backend::create(&profile));
    let run_dir = RunDir::create(&root)?;
    if let Some(observer) = run_dir_observer {
        *observer = Some(run_dir.root.clone());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    if install_interrupt_handler {
        let cancellation_flag = Arc::clone(&cancelled);
        ctrlc::set_handler(move || cancellation_flag.store(true, Ordering::Relaxed)).map_err(
            |error| SpecLoopError::Process(format!("install interrupt handler: {error}")),
        )?;
    }
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
        output: Output::new(cli.color_mode(), cli.verbose()),
    };
    context.output.stdout(&format!(
        "[RUNNING] Workflow started\nRun logs: {}\n",
        run_dir.root.display()
    ))?;
    let mut completed = BTreeSet::new();
    if cli.continue_requested() {
        for prior in discovery::discover_specs(&root, &profile)? {
            if prior.id == candidate.id {
                break;
            }
            completed.insert(prior.id);
        }
    }
    if cli.workflow_mode()? == WorkflowMode::Plan {
        execution::run_plan_workflow(&mut context, cli, candidate, files, completed)?;
        return Ok(Some(run_dir.root.clone()));
    }

    let mut entry = if cli.workflow_mode()? == WorkflowMode::Review {
        ImplementationEntry::ResumeReview
    } else {
        ImplementationEntry::Implement
    };
    loop {
        execution::run_implementation_workflow(
            &mut context,
            &candidate,
            &files.plan_path,
            files.archive_path.as_deref(),
            entry,
        )?;
        entry = ImplementationEntry::Implement;
        completed.insert(candidate.id.clone());
        if !cli.continue_requested() {
            break;
        }
        let Some(next) = discovery::select_spec(&root, &profile, None, &completed)? else {
            context
                .output
                .stdout("[PASS] No further eligible specifications.\n")?;
            break;
        };
        files = prepare_candidate_files(&root, &profile, &next, false)?;
        validate_action_executables(&root, &profile, &next, &files)?;
        context.integrity = InputIntegrity::capture(&root, &next.path, &files.plan_path, false)?;
        candidate = next;
    }
    context.output.stdout(&format!(
        "[PASS] Run logs retained at {}\n",
        run_dir.root.display()
    ))?;
    Ok(Some(run_dir.root.clone()))
}

struct PreparedRun {
    root: PathBuf,
    profile: Profile,
    candidate: SpecCandidate,
    files: CandidateFiles,
    integrity: InputIntegrity,
}

fn prepare_run(cli: &Cli, root: PathBuf) -> Result<PreparedRun> {
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
    let files = prepare_candidate_files(
        &root,
        &profile,
        &candidate,
        cli.workflow_mode()? == WorkflowMode::Plan,
    )?;
    validate_check_executables(&root, &profile, &candidate, &files.plan_path)?;
    if cli.workflow_mode()? != WorkflowMode::Plan {
        validate_action_executables(&root, &profile, &candidate, &files)?;
    }
    let integrity = InputIntegrity::capture(
        &root,
        &candidate.path,
        &files.plan_path,
        cli.workflow_mode()? == WorkflowMode::Plan && !(cli.continue_requested() && files.has_plan),
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
    backend::validate_prerequisites(profile)?;
    if profile.agent.backend == AgentBackendKind::Pi {
        paths::resolve_executable(root, &profile.agent.executable, "agent")?;
    }
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
        paths::resolve_executable(root, Path::new(program), &format!("check {:?}", check.name))?;
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
        paths::resolve_executable(
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

fn plan_exists(root: &Path, path: &Path) -> Result<bool> {
    let absolute = resolve_optional(root, path);
    match std::fs::metadata(&absolute) {
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

fn print_dry_run(cli: &Cli, prepared: &PreparedRun, output: &Output) -> Result<()> {
    output.stage("Dry run — resolved work")?;
    output.stdout(&format!(
        "Project root: {}\nProfile: {}\nSpec: {} ({})\nMode: {}\nReview-context argv: {:?}\nReview-context working directory: {}\nPlan: {}\n",
        prepared.root.display(),
        prepared.root.join(".spec-loop.toml").display(),
        prepared.candidate.id,
        prepared.candidate.path.display(),
        cli.workflow_mode()?.name(),
        prepared.profile.review.context.argv,
        prepared.root.display(),
        prepared.files.plan_path.display()
    ))?;
    if let Some(path) = &prepared.files.archive_path {
        output.stdout(&format!("Archive destination: {}\n", path.display()))?;
    }
    for check in &prepared.profile.checks {
        let argv = expand_argv(
            &check.argv,
            &prepared.candidate.id,
            &prepared.candidate.path,
            &prepared.files.plan_path,
        );
        output.stdout(&format!(
            "Check {:?}: argv={argv:?}, cwd={}, timeout={}\n",
            check.name,
            prepared.root.display(),
            timeout_description(check.timeout_seconds)
        ))?;
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
            output.stdout(&format!(
                "Action {label} {:?}: argv={argv:?}, cwd={}, timeout={}{}\n",
                action.name,
                prepared.root.display(),
                timeout_description(action.timeout_seconds),
                if cli.workflow_mode()? == WorkflowMode::Plan {
                    " (not run in plan mode)"
                } else {
                    ""
                }
            ))?;
        }
    }
    output.stdout(&format!("Maximum calls per role: {} (0 means unlimited)\n[PASS] Dry run: no agents, checks, or actions invoked.\n", prepared.profile.completion.max_passes))?;
    Ok(())
}

fn expand_argv(argv: &[String], spec_id: &str, spec_path: &Path, plan_path: &Path) -> Vec<String> {
    argv.iter()
        .map(|argument| expand_template(argument, spec_id, spec_path, Some(plan_path)))
        .collect()
}

fn timeout_description(timeout_seconds: Option<u64>) -> String {
    timeout_seconds.map_or_else(|| "unlimited".to_owned(), |seconds| format!("{seconds}s"))
}
