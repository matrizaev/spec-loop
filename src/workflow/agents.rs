use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Serialize;

use super::{AgentRequest, WorkflowContext};
use crate::backend::{self, AgentResponse, BackendFailure, UsageSnapshot};
use crate::config::AgentBackendKind;
use crate::error::{Result, SpecLoopError};
use crate::output::TypedResult;
use crate::prompts::{self, PromptContext, PromptRole};
use crate::protocol::{self, ResponseFields};

pub(super) fn call_typed<T>(
    context: &mut WorkflowContext<'_>,
    request: AgentRequest<'_>,
    mut validate: impl FnMut(&T) -> Result<()>,
) -> Result<T>
where
    T: serde::de::DeserializeOwned + JsonSchema + ResponseFields + TypedResult,
{
    let mut validation_feedback = request.validation_feedback.map(str::to_owned);
    let mut continuation = None;
    loop {
        let attempt = AgentRequest {
            validation_feedback: validation_feedback.as_deref(),
            ..request
        };
        let (response, next_continuation, response_log) =
            call_agent(context, attempt, continuation.take())?;
        match protocol::parse::<T>(&response, request.role.protocol_name()).and_then(|parsed| {
            validate(&parsed)?;
            Ok(parsed)
        }) {
            Ok(parsed) => {
                context.output.role_result(&parsed)?;
                return Ok(parsed);
            }
            Err(error) => {
                context.output.progress(&format!(
                    "[RETRY] Agent {} response failed validation: {error}; retrying same role. Final response log: {}",
                    role_display(request.role),
                    response_log.as_deref().map_or_else(|| "unavailable".to_owned(), |path| path.display().to_string())
                ))?;
                if next_continuation.is_some() {
                    continuation = next_continuation;
                    validation_feedback = Some(format!(
                        "Typed final response failed validation for role {}: {error}\nPrevious response:\n{response}\n\nReturn one corrected canonical role JSON object only. This is a correction-only retry: do not invoke tools or repeat workspace work. Preserve prior completed work and base any status or summary on the current workspace.",
                        role_display(request.role)
                    ));
                } else {
                    continuation = None;
                    validation_feedback = Some(format!(
                        "Typed response failed validation for role {}: {error}\nPrevious response:\n{response}\n\nReturn one corrected canonical role JSON object only. Repair the handoff only: do not repeat or alter workspace work solely to fix JSON formatting. If substantive role work remains, follow the normal role contract and continue it. Base any status or summary on the current workspace, never on claims in the malformed response.",
                        role_display(request.role)
                    ));
                }
            }
        }
    }
}

fn call_agent(
    context: &mut WorkflowContext<'_>,
    request: AgentRequest<'_>,
    mut continuation: Option<backend::BackendContinuation>,
) -> Result<(
    String,
    Option<backend::BackendContinuation>,
    Option<PathBuf>,
)> {
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
        context.output.stage(&format!(
            "Stage: agent {} (attempt {attempt}; spec {})",
            role_display(role),
            request.candidate.id
        ))?;
        let usage_key = (request.candidate.id.clone(), role);
        let usage_baseline = context
            .role_usage
            .get(&usage_key)
            .cloned()
            .unwrap_or_default();
        let agent_run = invoke_and_log_agent(
            context,
            call_index,
            role,
            &request.candidate.id,
            &prompt,
            continuation.take(),
            usage_baseline,
        )?;
        match agent_run.outcome {
            Ok(response) => {
                return Ok((
                    response.response,
                    agent_run.continuation,
                    agent_run.response_log,
                ));
            }
            Err(failure) => {
                let fatal = failure.kind() == backend::BackendFailureKind::Fatal;
                let error = failure.into_error();
                if matches!(&error, SpecLoopError::Interrupted) {
                    return Err(error);
                }
                if fatal {
                    return Err(error);
                }
                context.output.progress(&format!(
                    "[RETRY] Agent {} attempt failed: {error}",
                    role_display(role)
                ))?;
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

struct LoggedAgentRun {
    outcome: std::result::Result<AgentResponse, BackendFailure>,
    continuation: Option<backend::BackendContinuation>,
    response_log: Option<PathBuf>,
}

fn invoke_and_log_agent(
    context: &mut WorkflowContext<'_>,
    call_index: usize,
    role: PromptRole,
    spec_id: &str,
    prompt: &str,
    continuation: Option<backend::BackendContinuation>,
    usage_baseline: UsageSnapshot,
) -> Result<LoggedAgentRun> {
    let outcome = match context.backend.run_with_continuation(
        context.root,
        context.profile,
        prompt,
        role,
        &context.cancelled,
        backend::AgentInvocation {
            continuation,
            output: &context.output,
            usage_baseline,
        },
    ) {
        Ok(run) => {
            if let Some(delta) = &run.usage {
                let key = (spec_id.to_owned(), role);
                let cumulative = context.role_usage.entry(key).or_default();
                *cumulative = cumulative.merged(delta);
            }
            let stdout_name = format!("agent-{call_index:04}-{}.stdout.jsonl", role_display(role));
            let stderr_name = format!("agent-{call_index:04}-{}.stderr.log", role_display(role));
            context.run_dir.write_log(&stdout_name, &run.stdout)?;
            context.run_dir.write_log(&stderr_name, &run.stderr)?;
            context.output.stdout(&format!(
                "  Agent {} logs:\n    stdout: {}\n    stderr: {}\n",
                role_display(role),
                context.run_dir.log_path(&stdout_name).display(),
                context.run_dir.log_path(&stderr_name).display()
            ))?;
            let response_log = match &run.outcome {
                Ok(response) => {
                    if let Some(model) = &response.selected_model {
                        context
                            .output
                            .stdout(&format!("  Selected model: {model}\n"))?;
                    }
                    if let Some(model) = &response.response_model {
                        context
                            .output
                            .stdout(&format!("  Provider-reported model: {model}\n"))?;
                    }
                    let response_name =
                        format!("agent-{call_index:04}-{}.response.json", role_display(role));
                    context
                        .run_dir
                        .write_log(&response_name, response.response.as_bytes())?;
                    Some(context.run_dir.log_path(&response_name))
                }
                Err(error) => {
                    context.output.progress(&format!(
                        "[RETRY] Agent {} diagnostics: {error}",
                        role_display(role)
                    ))?;
                    None
                }
            };
            LoggedAgentRun {
                outcome: run.outcome,
                continuation: run.continuation,
                response_log,
            }
        }
        Err(error) => {
            let failure = if context.profile.agent.backend == AgentBackendKind::Builtin {
                BackendFailure::fatal(error)
            } else {
                BackendFailure::retryable(error)
            };
            LoggedAgentRun {
                outcome: Err(failure),
                continuation: None,
                response_log: None,
            }
        }
    };
    context
        .integrity
        .agent_finished(context.root, role == PromptRole::PlanAuthor)?;
    Ok(outcome)
}

pub(super) fn role_blocked(role: &str, reason: &str) -> SpecLoopError {
    SpecLoopError::Workflow(format!("{role} blocked: {reason}; changes preserved"))
}

pub(super) fn canonical_json(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|error| SpecLoopError::Agent(format!("serialize typed handoff: {error}")))
}
pub(super) const fn role_display(role: PromptRole) -> &'static str {
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
