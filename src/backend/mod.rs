use std::any::Any;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Numeric usage observations produced by built-in role execution.
pub use self::builtin::usage::UsageSnapshot;
use crate::config::{AgentBackendKind, Profile};
use crate::error::{Result, SpecLoopError};
use crate::output::Output;
use crate::prompts::PromptRole;

#[cfg(test)]
mod tests;

mod builtin;
pub mod pi;

/// The validated final response from an agent backend.
#[derive(Clone, Debug)]
pub struct AgentResponse {
    /// Complete final assistant response text, excluding backend event framing.
    pub response: String,
    /// Model selected for the invocation, when reported by the backend.
    pub selected_model: Option<String>,
    /// Model reported for the response when it differs from the selected model.
    pub response_model: Option<String>,
}

/// Classifies backend failures before workflow-level role retries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendFailureKind {
    /// Failure may be retried by the existing role retry policy.
    Retryable,
    /// Failure must stop the role to avoid replaying side effects.
    Fatal,
}

/// Backend failure with retry policy kept separate from diagnostic details.
pub struct BackendFailure {
    kind: BackendFailureKind,
    cause: SpecLoopError,
}

impl BackendFailure {
    /// Creates a backend failure eligible for generic role retry.
    pub const fn retryable(cause: SpecLoopError) -> Self {
        Self {
            kind: BackendFailureKind::Retryable,
            cause,
        }
    }

    /// Creates a backend failure that must not replay a role's side effects.
    pub const fn fatal(cause: SpecLoopError) -> Self {
        Self {
            kind: BackendFailureKind::Fatal,
            cause,
        }
    }

    /// Returns failure retry classification.
    pub const fn kind(&self) -> BackendFailureKind {
        self.kind
    }

    /// Consumes failure into actionable workflow error, preserving interruption.
    pub fn into_error(self) -> SpecLoopError {
        if matches!(&self.cause, SpecLoopError::Interrupted) {
            return self.cause;
        }
        match self.kind {
            BackendFailureKind::Retryable => self.cause,
            BackendFailureKind::Fatal => {
                SpecLoopError::Workflow(format!("fatal agent backend failure: {}", self.cause))
            }
        }
    }
}

impl fmt::Display for BackendFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.cause, formatter)
    }
}

/// Opaque backend state retained for correction-only retries.
pub struct BackendContinuation(Box<dyn Any + Send>);

impl BackendContinuation {
    /// Wraps backend-owned state without exposing its representation to workflow policy.
    pub fn new<T: Any + Send>(state: T) -> Self {
        Self(Box::new(state))
    }

    /// Recovers state created by matching backend or reports a contract error.
    pub fn into_state<T: Any + Send>(self) -> Result<T> {
        self.0.downcast::<T>().map(|state| *state).map_err(|_| {
            SpecLoopError::Workflow(
                "agent backend continuation does not match selected backend".to_owned(),
            )
        })
    }
}

/// Process output plus a separately parsed final response.
pub struct AgentRun {
    /// Parsed final response or a backend-classified failure.
    pub outcome: std::result::Result<AgentResponse, BackendFailure>,
    /// State needed to correct final-response formatting without repeating tools.
    pub continuation: Option<BackendContinuation>,
    /// Raw backend stdout, retained independently for diagnostics.
    pub stdout: Vec<u8>,
    /// Raw backend stderr, retained independently for diagnostics.
    pub stderr: Vec<u8>,
    /// Numeric usage observations made by this built-in invocation, if applicable.
    pub usage: Option<UsageSnapshot>,
}

/// Invocation settings that carry backend continuation and formatter-only presentation.
pub struct AgentInvocation<'a> {
    /// Backend-owned correction state, if the previous response needs repair.
    pub continuation: Option<BackendContinuation>,
    /// Workflow formatter used only for terminal diagnostics.
    pub output: &'a Output,
    /// Cumulative observations for this spec and role before the current invocation.
    pub usage_baseline: UsageSnapshot,
}

/// Backend contract consumed by workflow policy without backend-specific parsing.
pub trait AgentBackend {
    /// Invoke one role-specific agent and return raw logs plus a validated response.
    ///
    fn run(
        &self,
        root: &Path,
        profile: &Profile,
        prompt: &str,
        role: PromptRole,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<AgentRun>;

    /// Continues a prior response solely to repair its typed handoff.
    fn run_with_continuation(
        &self,
        root: &Path,
        profile: &Profile,
        prompt: &str,
        role: PromptRole,
        cancelled: &Arc<AtomicBool>,
        invocation: AgentInvocation<'_>,
    ) -> Result<AgentRun> {
        if invocation.continuation.is_some() {
            return Err(SpecLoopError::Workflow(
                "selected backend cannot resume a typed-response correction".to_owned(),
            ));
        }
        self.run(root, profile, prompt, role, cancelled)
    }
}

/// Constructs the configured backend after profile validation.
pub fn create(profile: &Profile) -> Box<dyn AgentBackend> {
    match profile.agent.backend {
        AgentBackendKind::Builtin => Box::new(builtin::BuiltinBackend),
        AgentBackendKind::Pi => Box::new(pi::PiBackend),
    }
}

/// Validates selected-backend prerequisites before workflow agent invocation.
pub fn validate_prerequisites(profile: &Profile) -> Result<()> {
    match profile.agent.backend {
        AgentBackendKind::Builtin => builtin::validate_shell_prerequisites(),
        AgentBackendKind::Pi => Ok(()),
    }
}
