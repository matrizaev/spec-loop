use serde::Serialize;

use super::{AgentRequest, WorkflowContext, agents};
use crate::error::{Result, SpecLoopError};
use crate::prompts::PromptRole;
use crate::protocol::{
    self, AuditInput, AuditResult, BlockerAdjudication, Finding, ImplementationAuditResponse,
    PlanAuditResponse,
};

/// Selects the existing auditor family without merging outer workflow policies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AuditScope {
    /// Planning roles delegate to the plan auditor without review context.
    Plan,
    /// Implementation roles delegate to the review auditor with delegated review context.
    Implementation,
}

impl AuditScope {
    /// Whether the request uses planning response types and policy.
    pub(super) const fn is_plan(self) -> bool {
        matches!(self, Self::Plan)
    }

    const fn auditor(self) -> PromptRole {
        match self {
            Self::Plan => PromptRole::PlanAudit,
            Self::Implementation => PromptRole::ReviewAudit,
        }
    }
}

/// Input-dependent contract checked inside the auditor's own response-retry loop.
#[derive(Clone, Copy)]
pub(super) enum AuditTask<'a> {
    /// Require classifications matching exactly these findings in order.
    Findings(&'a [Finding]),
    /// Require a typed adjudication, not finding classifications.
    AgentBlocker,
}

impl AuditTask<'_> {
    fn validate(self, result: &AuditResult) -> Result<()> {
        match (self, result) {
            (_, AuditResult::Blocked { .. })
            | (Self::AgentBlocker, AuditResult::Adjudicated { .. }) => Ok(()),
            (Self::Findings(findings), AuditResult::Complete { classifications }) => {
                protocol::validate_classification_order(classifications, findings)
            }
            (Self::Findings(_), AuditResult::Adjudicated { .. }) => Err(SpecLoopError::Agent(
                "finding audit requires finding classifications, not blocker adjudication"
                    .to_owned(),
            )),
            (Self::AgentBlocker, AuditResult::Complete { .. }) => Err(SpecLoopError::Agent(
                "agent-blocker audit requires blocker adjudication, not finding classifications"
                    .to_owned(),
            )),
        }
    }
}

/// Retries malformed or wrong-kind audits; a valid auditor blocker stops without recursion.
pub(super) fn request_audit(
    context: &mut WorkflowContext<'_>,
    request: AgentRequest<'_>,
    scope: AuditScope,
    task: AuditTask<'_>,
) -> Result<AuditResult> {
    // Auditors use the same strict retry path but never recursively adjudicate their own blockers.
    let result = if scope.is_plan() {
        agents::call_typed_attempts::<PlanAuditResponse>(context, request, |value| {
            task.validate(&value.result)
        })?
        .result
    } else {
        agents::call_typed_attempts::<ImplementationAuditResponse>(context, request, |value| {
            task.validate(&value.result)
        })?
        .result
    };
    match result {
        AuditResult::Blocked { reason } => Err(agents::role_blocked(
            if scope.is_plan() {
                "plan auditor"
            } else {
                "implementation finding auditor"
            },
            &reason,
        )),
        result => Ok(result),
    }
}

#[derive(Serialize)]
struct BlockerCorrection<'a, R> {
    response: &'a R,
    blocker_audit: &'a BlockerAdjudication,
}

/// Returns normal same-role feedback only for an unsupported claim; all other outcomes stop.
pub(super) fn adjudicate_blocker(
    context: &mut WorkflowContext<'_>,
    request: AgentRequest<'_>,
    response: &impl Serialize,
) -> Result<String> {
    let (scope, origin) = match request.role {
        PromptRole::PlanAuthor => (AuditScope::Plan, "plan author"),
        PromptRole::PlanReview => (AuditScope::Plan, "plan reviewer"),
        PromptRole::PlanClarification => (AuditScope::Plan, "evidence clarification"),
        PromptRole::Implementation => (AuditScope::Implementation, "implementation"),
        PromptRole::Review => (AuditScope::Implementation, "implementation reviewer"),
        PromptRole::ReviewClarification => (AuditScope::Implementation, "evidence clarification"),
        PromptRole::PlanAudit | PromptRole::ReviewAudit => {
            return Err(SpecLoopError::Workflow(
                "auditor blockers cannot be adjudicated recursively".to_owned(),
            ));
        }
    };
    let handoff = agents::canonical_json(&AuditInput::AgentBlocker { response })?;
    let result = request_audit(
        context,
        AgentRequest {
            role: scope.auditor(),
            feedback: Some(&handoff),
            validation_feedback: None,
            implementation_summary: None,
            ..request
        },
        scope,
        AuditTask::AgentBlocker,
    )?;
    let AuditResult::Adjudicated { adjudication } = result else {
        return Err(SpecLoopError::Workflow(
            "blocker audit returned an unexpected result after validation".to_owned(),
        ));
    };
    match &adjudication {
        BlockerAdjudication::Unsupported { .. } => {
            context.output.progress(&format!("[RETRY] {origin} blocker unsupported; returning audit feedback to the same originating role."))?;
            agents::canonical_json(&BlockerCorrection {
                response,
                blocker_audit: &adjudication,
            })
        }
        BlockerAdjudication::Confirmed {
            rationale,
            requirement,
            prerequisite,
            unavailable_reason,
            required_input,
            ..
        } => Err(agents::role_blocked(
            origin,
            &format!(
                "auditor confirmed blocker: {rationale}; unmet requirement: {requirement}; unavailable prerequisite: {prerequisite}; why tools cannot resolve it: {unavailable_reason}; required input: {required_input}"
            ),
        )),
        BlockerAdjudication::Ambiguous {
            rationale,
            question,
            ..
        } => Err(agents::role_blocked(
            origin,
            &format!(
                "blocker adjudication remains ambiguous: {rationale}; unresolved question: {question}"
            ),
        )),
    }
}
