use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::config::Profile;
use crate::error::{Result, SpecLoopError};
use crate::prompts::PromptRole;

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

/// Process output plus a separately parsed final response.
pub struct AgentRun {
    /// Parsed final response or the backend's validation/process error.
    pub outcome: Result<AgentResponse>,
    /// Raw backend stdout, retained independently for diagnostics.
    pub stdout: Vec<u8>,
    /// Raw backend stderr, retained independently for diagnostics.
    pub stderr: Vec<u8>,
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
}

/// Constructs the configured backend after profile validation.
pub fn create(profile: &Profile) -> Result<Box<dyn AgentBackend>> {
    match profile.agent.backend.as_str() {
        "pi" => Ok(Box::new(pi::PiBackend)),
        backend => Err(SpecLoopError::Config(format!(
            "unsupported agent backend {backend:?}"
        ))),
    }
}
