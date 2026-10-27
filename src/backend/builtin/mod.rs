use std::fs;
use std::future::Future;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{AgentBackend, AgentResponse, AgentRun, BackendContinuation, BackendFailure};
use crate::auth;
use crate::config::{AgentStage, Profile};
use crate::error::{Result, SpecLoopError};
use crate::prompts::PromptRole;
use crate::runtime;

mod provider;
mod shell;
#[cfg(test)]
mod tests;

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const SYSTEM_INSTRUCTIONS: &str = "Use the shell tool for local commands. Tool calls and tool results are transport only. Return the role's final answer as assistant text.";

/// `OpenAI` Responses API backend using `ChatGPT` OAuth and one POSIX shell tool.
pub(super) struct BuiltinBackend;

struct BuiltinConversation {
    input: Vec<Value>,
    tool_calls: usize,
}

impl AgentBackend for BuiltinBackend {
    fn run(
        &self,
        root: &Path,
        profile: &Profile,
        prompt: &str,
        role: PromptRole,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<AgentRun> {
        Self::run_continued(root, profile, prompt, role, cancelled, None)
    }

    fn run_with_continuation(
        &self,
        root: &Path,
        profile: &Profile,
        prompt: &str,
        role: PromptRole,
        cancelled: &Arc<AtomicBool>,
        invocation: super::AgentInvocation<'_>,
    ) -> Result<AgentRun> {
        Self::run_continued(
            root,
            profile,
            prompt,
            role,
            cancelled,
            invocation.continuation,
        )
    }
}

impl BuiltinBackend {
    fn run_continued(
        root: &Path,
        profile: &Profile,
        prompt: &str,
        role: PromptRole,
        cancelled: &Arc<AtomicBool>,
        continuation: Option<BackendContinuation>,
    ) -> Result<AgentRun> {
        let deadline = role_deadline(profile.agent.timeout_seconds)?;
        let continuation = continuation
            .map(BackendContinuation::into_state::<BuiltinConversation>)
            .transpose()?;
        let mut transcript = Vec::new();
        let mut completed_conversation = None;
        let response = runtime::block_on(run_role_with_auth(
            || auth::active_access_token(false, cancelled),
            RoleTask {
                root,
                profile,
                prompt,
                role,
                cancelled,
                deadline,
                endpoint: RESPONSES_URL,
            },
            continuation,
            &mut completed_conversation,
            &mut transcript,
        ))
        .and_then(|result| result);
        let mut run = classify_agent_run(response, transcript);
        if run.outcome.is_ok() {
            run.continuation = completed_conversation.map(BackendContinuation::new);
        }
        Ok(run)
    }
}

fn role_deadline(timeout_seconds: Option<u64>) -> Result<Option<Instant>> {
    timeout_seconds
        .map(|seconds| {
            Instant::now()
                .checked_add(Duration::from_secs(seconds))
                .ok_or_else(|| {
                    SpecLoopError::Config(
                        "agent.timeout_seconds exceeds monotonic clock range".to_owned(),
                    )
                })
        })
        .transpose()
}

fn classify_agent_run(response: Result<AgentResponse>, transcript: Vec<u8>) -> AgentRun {
    AgentRun {
        outcome: response.map_err(BackendFailure::fatal),
        continuation: None,
        stdout: transcript,
        stderr: Vec::new(),
    }
}

struct RoleTask<'a> {
    root: &'a Path,
    profile: &'a Profile,
    prompt: &'a str,
    role: PromptRole,
    cancelled: &'a Arc<AtomicBool>,
    deadline: Option<Instant>,
    endpoint: &'a str,
}

async fn run_role_with_auth<F, Fut>(
    token_provider: F,
    task: RoleTask<'_>,
    continuation: Option<BuiltinConversation>,
    completed_conversation: &mut Option<BuiltinConversation>,
    transcript: &mut Vec<u8>,
) -> Result<AgentResponse>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let RoleTask {
        root,
        profile,
        prompt,
        role,
        cancelled,
        deadline,
        endpoint,
    } = task;
    run_after_shell_preflight(Path::new("/bin/sh"), cfg!(unix), || async move {
        run_role_with_token_provider(
            RoleInput {
                root,
                profile,
                prompt,
                role,
                cancelled,
                deadline,
                endpoint,
                #[cfg(test)]
                token: "",
            },
            continuation,
            token_provider,
            completed_conversation,
            transcript,
        )
        .await
    })
    .await
}

async fn run_after_shell_preflight<T, F, Fut>(
    shell_path: &Path,
    supported_platform: bool,
    operation: F,
) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    validate_shell_at(shell_path, supported_platform)?;
    operation().await
}

/// Validates the platform shell prerequisite before credentials or provider access.
pub fn validate_shell_prerequisites() -> Result<()> {
    validate_shell_at(Path::new("/bin/sh"), cfg!(unix))
}

fn validate_shell_at(shell_path: &Path, supported_platform: bool) -> Result<()> {
    if !supported_platform {
        return Err(SpecLoopError::Config(
            r#"built-in backend requires a POSIX platform with executable `/bin/sh`; select agent.backend = "pi" on this platform"#.to_owned(),
        ));
    }
    let metadata = fs::metadata(shell_path).map_err(|error| {
        SpecLoopError::Config(format!(
            r#"built-in backend requires executable `/bin/sh` at {}: {error}; select agent.backend = "pi" if POSIX shell is unavailable"#,
            shell_path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(SpecLoopError::Config(format!(
            r#"built-in backend requires `/bin/sh` to be a regular executable file: {}; select agent.backend = "pi" if POSIX shell is unavailable"#,
            shell_path.display()
        )));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(SpecLoopError::Config(format!(
            r#"built-in backend requires executable `/bin/sh`: {}; select agent.backend = "pi" if POSIX shell is unavailable"#,
            shell_path.display()
        )));
    }
    Ok(())
}

struct RoleInput<'a> {
    root: &'a Path,
    profile: &'a Profile,
    prompt: &'a str,
    role: PromptRole,
    cancelled: &'a Arc<AtomicBool>,
    deadline: Option<Instant>,
    endpoint: &'a str,
    #[cfg(test)]
    token: &'a str,
}

#[cfg(test)]
async fn run_role_with_access_token(
    input: RoleInput<'_>,
    transcript: &mut Vec<u8>,
) -> Result<AgentResponse> {
    run_role_with_continuation(input, None, &mut None, transcript).await
}

#[cfg(test)]
async fn run_role_with_continuation(
    input: RoleInput<'_>,
    continuation: Option<BuiltinConversation>,
    completed_conversation: &mut Option<BuiltinConversation>,
    transcript: &mut Vec<u8>,
) -> Result<AgentResponse> {
    let token = input.token.to_owned();
    run_role_with_token_provider(
        input,
        continuation,
        move || {
            let token = token.clone();
            async move { Ok(token) }
        },
        completed_conversation,
        transcript,
    )
    .await
}

async fn run_role_with_token_provider<F, Fut>(
    input: RoleInput<'_>,
    continuation: Option<BuiltinConversation>,
    mut token_provider: F,
    completed_conversation: &mut Option<BuiltinConversation>,
    transcript: &mut Vec<u8>,
) -> Result<AgentResponse>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let correction_only = continuation.is_some();
    let RoleInput {
        root,
        profile,
        prompt,
        role,
        cancelled,
        deadline,
        endpoint,
        ..
    } = input;
    let model = profile
        .agent
        .model_for(stage_for_role(role))
        .ok_or_else(|| SpecLoopError::Config("agent.model is required for builtin".to_owned()))?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            SpecLoopError::Workflow(format!("construct OpenAI Responses HTTP client: {error}"))
        })?;
    let mut conversation = match continuation {
        Some(mut conversation) => {
            conversation
                .input
                .push(json!({"role": "user", "content": prompt}));
            conversation
        }
        None => BuiltinConversation {
            input: vec![json!({"role": "user", "content": prompt})],
            tool_calls: 0,
        },
    };
    loop {
        provider::ensure_active(deadline, cancelled)?;
        let mut request = json!({
            "model": model,
            "store": false,
            "stream": true,
            "instructions": SYSTEM_INSTRUCTIONS,
            "input": conversation.input,
            "tools": [shell::shell_tool_definition()],
            "tool_choice": if correction_only {"none"} else {"auto"}
        });
        if let Some(thinking) = profile.agent.thinking_for(stage_for_role(role)) {
            request["reasoning"] = json!({"effort": thinking.provider_values().1});
        }
        let token = provider::await_limited(token_provider(), deadline, cancelled).await??;
        let response = provider::request_with_retries(
            &client, endpoint, &token, &request, deadline, cancelled,
        )
        .await?;
        let output = shell::completed_output(&response)?;
        let (calls, final_text) = shell::parse_output_items(&output)?;
        if calls.is_empty() {
            conversation.input.extend(output);
            *completed_conversation = Some(conversation);
            return Ok(AgentResponse {
                response: final_text.unwrap_or_default(),
                selected_model: Some(model.to_owned()),
                response_model: response
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .filter(|reported| reported != model),
            });
        }
        if correction_only {
            return Err(SpecLoopError::Workflow(
                "built-in response correction requested a tool call; completed workspace work was not replayed".to_owned(),
            ));
        }

        conversation.input.extend(output);
        shell::execute_shell_calls(
            root,
            profile,
            deadline,
            cancelled,
            calls,
            &mut conversation,
            transcript,
        )?;
    }
}

const fn stage_for_role(role: PromptRole) -> AgentStage {
    match role {
        PromptRole::PlanAuthor
        | PromptRole::PlanReview
        | PromptRole::PlanAudit
        | PromptRole::PlanClarification => AgentStage::Planning,
        PromptRole::Implementation => AgentStage::Implementation,
        PromptRole::Review | PromptRole::ReviewAudit | PromptRole::ReviewClarification => {
            AgentStage::Review
        }
    }
}
