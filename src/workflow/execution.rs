use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use super::{
    ActionIntegrityPolicy, AgentRequest, AuditReport, CandidateFiles, CheckReport,
    FindingDisposition, ImplementationEntry, WorkflowContext, expand_argv, plan_exists,
    prepare_candidate_files,
};
use crate::cli::Cli;
use crate::commands::{self, ProcessTermination, status_success};
use crate::config::{ActionConfig, CheckConfig};
use crate::discovery::{self, SpecCandidate};
use crate::error::{Result, SpecLoopError, io_error};
use crate::integrity::InputIntegrity;
use crate::output::Output;
use crate::prompts::PromptRole;
use crate::protocol::{
    self, Actionability, AuditEvidenceInput, AuditInput, AuditResult, ClarificationInput,
    Classification, EvidenceResult, Finding, FindingClassification, ImplementationAuditResponse,
    ImplementationClarificationResponse, ImplementationRepairInput, ImplementationResponse,
    ImplementationReviewResponse, PlanAuditResponse, PlanAuthorResponse, PlanClarificationResponse,
    PlanRepairInput, PlanReviewResponse, Readiness, ReviewResult,
};

pub(super) fn run_plan_workflow(
    context: &mut WorkflowContext<'_>,
    cli: &Cli,
    mut candidate: SpecCandidate,
    mut files: CandidateFiles,
    mut completed: BTreeSet<String>,
) -> Result<()> {
    loop {
        if cli.continue_requested() && files.has_plan {
            context.integrity.verify(context.root)?;
            context.output.stdout(&format!(
                "[WARN] Skipping spec {}: plan already exists.\n",
                candidate.id
            ))?;
        } else {
            run_plan_loop(context, &candidate, &files.plan_path)?;
            context.integrity.verify(context.root)?;
            context.output.stdout(&format!(
                "[PASS] Plan approved for spec {}.\n",
                candidate.id
            ))?;
        }
        if !cli.continue_requested() {
            return Ok(());
        }
        completed.insert(candidate.id.clone());
        let Some(next) = discovery::select_spec(context.root, context.profile, None, &completed)?
        else {
            context
                .output
                .stdout("[PASS] No further specifications need plans.\n")?;
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
        let response = super::agents::call_typed::<PlanAuthorResponse>(
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
            Readiness::Blocked { reason } => {
                return Err(super::agents::role_blocked("plan author", &reason));
            }
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
            ReviewResult::Blocked { reason } => {
                return Err(super::agents::role_blocked("plan reviewer", &reason));
            }
            ReviewResult::NoIssues { .. } => return Ok(()),
            ReviewResult::Issues { ref findings } => {
                let report = audit_findings(
                    context,
                    AuditRequest {
                        candidate,
                        plan_path,
                        scope: AuditScope::Plan,
                        findings,
                        review: &review,
                        checks: "",
                    },
                )?;
                let findings = disposition(findings, &report)?;
                if !findings.unsupported.is_empty() {
                    context.output.stdout(&format!(
                        "[WARN] Plan auditor classified findings unsupported: {}\n",
                        ids(&findings.unsupported)
                    ))?;
                }
                if !findings.not_actionable.is_empty() {
                    context.output.stdout(&format!(
                        "[WARN] Plan auditor found no required correction for findings: {}\n",
                        ids(&findings.not_actionable)
                    ))?;
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
                feedback = Some(super::agents::canonical_json(&handoff)?);
            }
        }
    }
}

fn review_plan(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
) -> Result<PlanReviewResponse> {
    super::agents::call_typed(
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

pub(super) fn run_implementation_workflow(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
    archive_path: Option<&Path>,
    mut entry: ImplementationEntry,
) -> Result<()> {
    let mut feedback: Option<String> = None;
    loop {
        let implementation_summary = match entry.take_for_pass() {
            ImplementationEntry::Implement => {
                let response = super::agents::call_typed::<ImplementationResponse>(
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
                        return Err(super::agents::role_blocked("implementation", &reason));
                    }
                    Readiness::ReadyForReview => Some(response.summary),
                }
            }
            ImplementationEntry::ResumeReview => None,
        };
        context.integrity.verify(context.root)?;
        let mut transition = format!("Stage: automated checks for spec {}", candidate.id);
        if let Some(summary) = implementation_summary {
            transition.push_str("\nImplementation summary: ");
            transition.push_str(&Output::display_text(&summary, 160));
        }
        context.output.stage(&transition)?;
        let checks = run_checks(context, candidate, plan_path)?;
        context.output.stdout(&format!(
            "[{}] Check summary\n{}\n",
            if context.profile.checks.is_empty() {
                "WARN"
            } else if checks.passed {
                "PASS"
            } else {
                "FAIL"
            },
            checks.display_summary
        ))?;
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
    let review = super::agents::call_typed::<ImplementationReviewResponse>(
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
        ReviewResult::Blocked { reason } => Err(super::agents::role_blocked(
            "implementation reviewer",
            &reason,
        )),
        ReviewResult::NoIssues { .. } => Ok(ReviewDisposition::Approved),
        ReviewResult::Issues { ref findings } => {
            let report = audit_findings(
                context,
                AuditRequest {
                    candidate,
                    plan_path,
                    scope: AuditScope::Implementation,
                    findings,
                    review: &review,
                    checks,
                },
            )?;
            let findings = disposition(findings, &report)?;
            if !findings.not_actionable.is_empty() {
                context.output.stdout(&format!(
                    "[WARN] Review auditor found no required correction for findings: {}\n",
                    ids(&findings.not_actionable)
                ))?;
            }
            if findings.actionable.is_empty() && findings.ambiguous.is_empty() {
                return Ok(ReviewDisposition::Approved);
            }
            let handoff = ImplementationRepairInput {
                actionable_findings: findings.actionable,
                ambiguous_findings: findings.ambiguous,
                audit: &report.classifications,
            };
            let message = super::agents::canonical_json(&handoff)?;
            Ok(ReviewDisposition::Repair(format!(
                "Resume implementation from current workspace and preserve prior work. Repair only findings marked supported and required in the audit. Follow audit rationale for the smallest sufficient correct fix, even when substantial. Do not change code for supported findings marked not_required. Resolve ambiguous findings from available evidence; do not treat ambiguity as authorization to change code, and return blocked if consequential ambiguity remains. If required in-scope work remains but can be implemented compliantly, make a concrete increment and return ready_for_review; the workflow will rerun checks and review, then route remaining findings back here. ready_for_review marks readiness for another pass, not full spec completion. Unfinished work or its size alone is not a blocker.\n{message}"
            )))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuditScope {
    Plan,
    Implementation,
}

impl AuditScope {
    const fn is_plan(self) -> bool {
        matches!(self, Self::Plan)
    }
}

struct AuditRequest<'a, R> {
    candidate: &'a SpecCandidate,
    plan_path: &'a Path,
    scope: AuditScope,
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
    let role = if request.scope.is_plan() {
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
    let request_json = super::agents::canonical_json(&clarification)?;
    let agent_request = AgentRequest {
        candidate: request.candidate,
        role,
        plan_path: Some(request.plan_path),
        feedback: Some(&request_json),
        validation_feedback: None,
        checks: if request.scope.is_plan() {
            ""
        } else {
            request.checks
        },
    };
    let evidence = if request.scope.is_plan() {
        let response = super::agents::call_typed::<PlanClarificationResponse>(
            context,
            agent_request,
            |value| match &value.result {
                EvidenceResult::Complete { evidence } => {
                    protocol::validate_evidence(evidence, requested)
                }
                EvidenceResult::Blocked { reason } => {
                    protocol::validate_text(reason, "blocked reason")
                }
            },
        )?;
        match response.result {
            EvidenceResult::Blocked { reason } => {
                return Err(super::agents::role_blocked(
                    "evidence clarification",
                    &reason,
                ));
            }
            EvidenceResult::Complete { evidence } => evidence,
        }
    } else {
        let response = super::agents::call_typed::<ImplementationClarificationResponse>(
            context,
            agent_request,
            |value| match &value.result {
                EvidenceResult::Complete { evidence } => {
                    protocol::validate_evidence(evidence, requested)
                }
                EvidenceResult::Blocked { reason } => {
                    protocol::validate_text(reason, "blocked reason")
                }
            },
        )?;
        match response.result {
            EvidenceResult::Blocked { reason } => {
                return Err(super::agents::role_blocked(
                    "evidence clarification",
                    &reason,
                ));
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
    let role = if request.scope.is_plan() {
        PromptRole::PlanAudit
    } else {
        PromptRole::ReviewAudit
    };
    let handoff = AuditInput {
        review: request.review,
        findings: request.findings,
        evidence,
    };
    let request_json = super::agents::canonical_json(&handoff)?;
    let agent_request = AgentRequest {
        candidate: request.candidate,
        role,
        plan_path: Some(request.plan_path),
        feedback: Some(&request_json),
        validation_feedback: None,
        checks: if request.scope.is_plan() {
            ""
        } else {
            request.checks
        },
    };
    if request.scope.is_plan() {
        let response =
            super::agents::call_typed::<PlanAuditResponse>(context, agent_request, |value| {
                match &value.result {
                    AuditResult::Complete { classifications } => {
                        protocol::validate_classifications(classifications, request.findings)
                    }
                    AuditResult::Blocked { reason } => {
                        protocol::validate_text(reason, "blocked reason")
                    }
                }
            })?;
        match response.result {
            AuditResult::Blocked { reason } => {
                Err(super::agents::role_blocked("plan auditor", &reason))
            }
            AuditResult::Complete { classifications } => Ok(AuditReport { classifications }),
        }
    } else {
        let response = super::agents::call_typed::<ImplementationAuditResponse>(
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
            AuditResult::Blocked { reason } => Err(super::agents::role_blocked(
                "implementation finding auditor",
                &reason,
            )),
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
        archive_spec(context, candidate, destination)?;
        context.integrity.relocate_spec(context.root, destination)?;
        completed_candidate.path = destination.to_path_buf();
    }
    context.output.stdout(&format!(
        "[PASS] Review approved for spec {}.\n",
        candidate.id
    ))?;
    run_actions(
        context,
        &context.profile.actions.on_review_success,
        &completed_candidate,
        plan_path,
        ActionIntegrityPolicy::SelectedInputs,
    )?;
    if !context.profile.actions.on_finish.is_empty() {
        context.integrity.verify(context.root)?;
    }
    run_actions(
        context,
        &context.profile.actions.on_finish,
        &completed_candidate,
        plan_path,
        ActionIntegrityPolicy::SelectedPlan,
    )?;
    Ok(())
}

fn archive_spec(
    context: &WorkflowContext<'_>,
    candidate: &SpecCandidate,
    destination: &Path,
) -> Result<()> {
    let root = context.root;
    context.output.stage(&format!(
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
    context.output.stdout(&format!(
        "[PASS] Archived spec: {} -> {}\n",
        candidate.path.display(),
        destination.display()
    ))?;
    Ok(())
}

fn run_actions(
    context: &mut WorkflowContext<'_>,
    actions: &[ActionConfig],
    candidate: &SpecCandidate,
    plan_path: &Path,
    integrity_policy: ActionIntegrityPolicy,
) -> Result<()> {
    for action in actions {
        verify_action_integrity(context, integrity_policy)?;
        let argv = expand_argv(&action.argv, &candidate.id, &candidate.path, plan_path);
        let action_result = run_action(context, action, &argv);
        let integrity_result = verify_action_integrity(context, integrity_policy);
        combine_action_and_integrity_results(action_result, integrity_result)?;
    }
    Ok(())
}

fn verify_action_integrity(
    context: &WorkflowContext<'_>,
    policy: ActionIntegrityPolicy,
) -> Result<()> {
    match policy {
        ActionIntegrityPolicy::SelectedInputs => context.integrity.verify(context.root),
        ActionIntegrityPolicy::SelectedPlan => context.integrity.verify_plan(context.root),
    }
}

/// Keeps diagnostics from both action failure and post-action integrity failure.
fn combine_action_and_integrity_results(
    action_result: Result<()>,
    integrity_result: Result<()>,
) -> Result<()> {
    match (action_result, integrity_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(action_error), Ok(())) => Err(action_error),
        (Ok(()), Err(integrity_error)) => Err(integrity_error),
        (Err(action_error), Err(integrity_error)) => Err(SpecLoopError::Workflow(format!(
            "{action_error}; post-action integrity verification also failed: {integrity_error}"
        ))),
    }
}

fn run_action(
    context: &mut WorkflowContext<'_>,
    action: &ActionConfig,
    argv: &[String],
) -> Result<()> {
    context.output.stage(&format!(
        "Action {:?}\nCommand argv: {argv:?}\nWorking directory: {}",
        action.name,
        context.root.display()
    ))?;
    let CommandReport {
        success,
        status,
        termination,
        stdout,
        stderr,
        log_path,
    } = run_logged_command(
        context,
        "Action",
        &action.name,
        argv,
        action.timeout_seconds,
    )?;
    context.output.stdout(&format!(
        "[{}] Action {:?}: {}\n  Log: {}\n{}",
        if success { "PASS" } else { "FAIL" },
        action.name,
        if success { "completed" } else { "failed" },
        log_path.display(),
        display_output_excerpt(&stdout, &stderr)
    ))?;
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
    let mut display_summary = String::new();
    if context.profile.checks.is_empty() {
        summary.push_str("No automated checks ran (profile has no configured checks).\n");
        display_summary.push_str("No automated checks ran (profile has no configured checks).\n");
    }
    for check in &context.profile.checks {
        context.integrity.verify(context.root)?;
        let result = run_check(context, candidate, plan_path, check)?;
        context.integrity.verify(context.root)?;
        all_passed &= result.0;
        summary.push_str(&result.1);
        summary.push('\n');
        display_summary.push_str(&result.2);
        display_summary.push('\n');
    }
    Ok(CheckReport {
        passed: all_passed,
        summary,
        display_summary,
    })
}

fn run_check(
    context: &mut WorkflowContext<'_>,
    candidate: &SpecCandidate,
    plan_path: &Path,
    check: &CheckConfig,
) -> Result<(bool, String, String)> {
    context
        .output
        .stage(&format!("Stage: check {}", check.name))?;
    let argv = expand_argv(&check.argv, &candidate.id, &candidate.path, plan_path);
    context.output.metadata(&format!(
        "Command argv: {argv:?}\nWorking directory: {}",
        context.root.display()
    ))?;
    let CommandReport {
        success,
        status,
        termination,
        stdout,
        stderr,
        log_path,
    } = run_logged_command(context, "Check", &check.name, &argv, check.timeout_seconds)?;
    context.output.stdout(&format!(
        "[{}] Check {}: {}\n  Log: {}\n",
        if success { "PASS" } else { "FAIL" },
        check.name,
        if success { "passed" } else { "failed" },
        log_path.display()
    ))?;
    if termination == Some(ProcessTermination::Interrupted) {
        return Err(SpecLoopError::Interrupted);
    }
    let summary_prefix = format!(
        "Check: {}\nCommand argv: {argv:?}\nStatus: {status}{}\nLog: {}\n",
        check.name,
        termination.map_or_else(String::new, |value| match value {
            ProcessTermination::TimedOut => " (timed out)".to_owned(),
            ProcessTermination::Interrupted => " (interrupted)".to_owned(),
            ProcessTermination::Exited => String::new(),
        }),
        log_path.display()
    );
    let summary = format!("{summary_prefix}{}", output_excerpt(&stdout, &stderr));
    let display_summary = format!(
        "{summary_prefix}{}",
        display_output_excerpt(&stdout, &stderr)
    );
    Ok((success, summary, display_summary))
}

struct CommandReport {
    success: bool,
    status: String,
    termination: Option<ProcessTermination>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    log_path: PathBuf,
}

fn run_logged_command(
    context: &mut WorkflowContext<'_>,
    kind: &str,
    name: &str,
    argv: &[String],
    timeout_seconds: Option<u64>,
) -> Result<CommandReport> {
    let Some((program, arguments)) = argv.split_first() else {
        return Err(SpecLoopError::Config(format!(
            "{kind} {name:?} has empty argv"
        )));
    };
    let outcome = commands::run_process(
        Path::new(program),
        arguments,
        context.root,
        timeout_seconds.map(Duration::from_secs),
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
        "{kind}: {name}\nCommand argv: {argv:?}\nWorking directory: {}\nStatus: {status}\n--- stdout ({} bytes) ---\n",
        context.root.display(), stdout.len()
    ).into_bytes();
    log.extend_from_slice(&stdout);
    log.extend_from_slice(format!("\n--- stderr ({} bytes) ---\n", stderr.len()).as_bytes());
    log.extend_from_slice(&stderr);
    let log_name = format!(
        "{}-{:04}.log",
        kind.to_ascii_lowercase(),
        context.next_log_index()?
    );
    let log_path = context.run_dir.write_log(&log_name, &log)?;
    Ok(CommandReport {
        success,
        status,
        termination,
        stdout,
        stderr,
        log_path,
    })
}

pub(super) fn output_excerpt(stdout: &[u8], stderr: &[u8]) -> String {
    output_excerpt_with_line_endings(stdout, stderr, false)
}

pub(super) fn display_output_excerpt(stdout: &[u8], stderr: &[u8]) -> String {
    output_excerpt_with_line_endings(stdout, stderr, true)
}

fn output_excerpt_with_line_endings(
    stdout: &[u8],
    stderr: &[u8],
    preserve_carriage_returns: bool,
) -> String {
    let mut summary = String::new();
    append_output_excerpt(&mut summary, "stdout", stdout, preserve_carriage_returns);
    append_output_excerpt(&mut summary, "stderr", stderr, preserve_carriage_returns);
    summary
}

fn append_output_excerpt(
    summary: &mut String,
    stream: &str,
    output: &[u8],
    preserve_carriage_returns: bool,
) {
    const MAX_LINES: usize = 4;
    const MAX_LINE_CHARS: usize = 300;

    let decoded = String::from_utf8_lossy(output);
    let lines: Vec<&str> = if preserve_carriage_returns {
        decoded.split_terminator('\n').collect()
    } else {
        decoded.lines().collect()
    };
    let lines: Vec<&str> = lines
        .into_iter()
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
