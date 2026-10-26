use std::fs;
use std::future::Future;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{AgentBackend, AgentResponse, AgentRun, BackendContinuation, BackendFailure};
use crate::auth;
use crate::commands::{ProcessTermination, run_process_until};
use crate::config::{AgentStage, Profile};
use crate::error::{Result, SpecLoopError};
use crate::prompts::PromptRole;
use crate::runtime;

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const SYSTEM_INSTRUCTIONS: &str = "Use the shell tool for local commands. Tool calls and tool results are transport only. Return the role's final answer as assistant text.";
const MAX_PROVIDER_RETRIES: u8 = 3;
const MAX_RETRY_DELAY_SECONDS: u64 = 30;
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(25);

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
        continuation: Option<BackendContinuation>,
    ) -> Result<AgentRun> {
        Self::run_continued(root, profile, prompt, role, cancelled, continuation)
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
            auth::active_access_token(false, cancelled),
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

async fn run_role_with_auth<F>(
    token_future: F,
    task: RoleTask<'_>,
    continuation: Option<BuiltinConversation>,
    completed_conversation: &mut Option<BuiltinConversation>,
    transcript: &mut Vec<u8>,
) -> Result<AgentResponse>
where
    F: Future<Output = Result<String>>,
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
    run_after_shell_preflight(Path::new("/bin/sh"), cfg!(unix), || async {
        let token = await_limited(token_future, deadline, cancelled).await??;
        run_role_with_continuation(
            RoleInput {
                root,
                profile,
                prompt,
                role,
                cancelled,
                deadline,
                endpoint,
                token: &token,
            },
            continuation,
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
    token: &'a str,
}

#[cfg(test)]
async fn run_role_with_access_token(
    input: RoleInput<'_>,
    transcript: &mut Vec<u8>,
) -> Result<AgentResponse> {
    run_role_with_continuation(input, None, &mut None, transcript).await
}

async fn run_role_with_continuation(
    input: RoleInput<'_>,
    continuation: Option<BuiltinConversation>,
    completed_conversation: &mut Option<BuiltinConversation>,
    transcript: &mut Vec<u8>,
) -> Result<AgentResponse> {
    let correction_only = continuation.is_some();
    let RoleInput {
        root,
        profile,
        prompt,
        role,
        cancelled,
        deadline,
        endpoint,
        token,
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
        ensure_active(deadline, cancelled)?;
        let mut request = json!({
            "model": model,
            "store": false,
            "stream": true,
            "instructions": SYSTEM_INSTRUCTIONS,
            "input": conversation.input,
            "tools": [shell_tool_definition()],
            "tool_choice": if correction_only {"none"} else {"auto"}
        });
        if let Some(thinking) = profile.agent.thinking_for(stage_for_role(role)) {
            request["reasoning"] = json!({"effort": thinking.provider_values().1});
        }
        let response =
            request_with_retries(&client, endpoint, token, &request, deadline, cancelled).await?;
        let output = completed_output(&response)?;
        let (calls, final_text) = parse_output_items(&output)?;
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
        execute_shell_calls(
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

fn execute_shell_calls(
    root: &Path,
    profile: &Profile,
    deadline: Option<Instant>,
    cancelled: &Arc<AtomicBool>,
    calls: Vec<ShellCall>,
    conversation: &mut BuiltinConversation,
    transcript: &mut Vec<u8>,
) -> Result<()> {
    for call in calls {
        ensure_active(deadline, cancelled)?;
        if conversation.tool_calls >= profile.agent.max_tool_calls() {
            return Err(SpecLoopError::Workflow(format!(
                "built-in agent reached agent.max_tool_calls={} before executing another shell call; role stopped without replay",
                profile.agent.max_tool_calls()
            )));
        }
        conversation.tool_calls = conversation.tool_calls.checked_add(1).ok_or_else(|| {
            SpecLoopError::Workflow("built-in shell call counter overflowed".to_owned())
        })?;
        let result = run_shell(
            root,
            &call.command,
            profile.agent.max_tool_output_bytes(),
            deadline,
            cancelled,
        )?;
        append_transcript(transcript, &call.command, &result.log_output)?;
        match result.termination {
            ProcessTermination::Exited => {}
            ProcessTermination::Interrupted => return Err(SpecLoopError::Interrupted),
            ProcessTermination::TimedOut => {
                return Err(SpecLoopError::Timeout(
                    "built-in shell command exceeded the role deadline".to_owned(),
                ));
            }
        }
        conversation.input.push(json!({
            "type": "function_call_output",
            "call_id": call.call_id,
            "output": result.tool_output
        }));
    }
    Ok(())
}

fn shell_tool_definition() -> Value {
    json!({
        "type": "namespace",
        "name": "spec_loop",
        "description": "Spec Loop local tools.",
        "tools": [{
            "type": "function",
            "name": "shell",
            "description": "Run one shell command from the project root. Commands run automatically as the current user and are not sandboxed.",
            "parameters": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            },
            "strict": true
        }]
    })
}

async fn request_with_retries(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    body: &Value,
    deadline: Option<Instant>,
    cancelled: &AtomicBool,
) -> Result<Value> {
    let mut retry_count = 0_u8;
    loop {
        ensure_active(deadline, cancelled)?;
        let request = client.post(endpoint).bearer_auth(token).json(body).send();
        let response = match await_limited(request, deadline, cancelled).await? {
            Ok(response) => response,
            Err(_) if retry_count < MAX_PROVIDER_RETRIES => {
                wait_before_retry(retry_count, None, deadline, cancelled).await?;
                retry_count = retry_count.checked_add(1).ok_or_else(|| {
                    SpecLoopError::Workflow("provider retry counter overflowed".to_owned())
                })?;
                continue;
            }
            Err(_) => {
                return Err(SpecLoopError::Workflow(format!(
                    "OpenAI Responses request failed after {MAX_PROVIDER_RETRIES} retries"
                )));
            }
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if status.is_success() {
            match read_streamed_response(response, deadline, cancelled).await {
                Ok(completed) => return Ok(completed),
                Err(StreamFailure::Network | StreamFailure::RetryableProvider)
                    if retry_count < MAX_PROVIDER_RETRIES =>
                {
                    wait_before_retry(retry_count, retry_after.as_deref(), deadline, cancelled)
                        .await?;
                    retry_count = retry_count.checked_add(1).ok_or_else(|| {
                        SpecLoopError::Workflow("provider retry counter overflowed".to_owned())
                    })?;
                    continue;
                }
                Err(StreamFailure::Network | StreamFailure::RetryableProvider) => {
                    return Err(SpecLoopError::Workflow(format!(
                        "OpenAI Responses stream failed after {MAX_PROVIDER_RETRIES} retries"
                    )));
                }
                Err(StreamFailure::Fatal(error)) => return Err(error),
            }
        }
        let bytes = match await_limited(response.bytes(), deadline, cancelled).await? {
            Ok(bytes) => bytes,
            Err(_) if retry_count < MAX_PROVIDER_RETRIES => {
                wait_before_retry(retry_count, None, deadline, cancelled).await?;
                retry_count = retry_count.checked_add(1).ok_or_else(|| {
                    SpecLoopError::Workflow("provider retry counter overflowed".to_owned())
                })?;
                continue;
            }
            Err(_) => {
                return Err(SpecLoopError::Workflow(format!(
                    "could not read OpenAI Responses result after {MAX_PROVIDER_RETRIES} retries"
                )));
            }
        };
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(SpecLoopError::Workflow(
                "ChatGPT authorization or direct-use permission was rejected; run `spec-loop auth login` and verify ChatGPT plan access".to_owned(),
            ));
        }
        if status.as_u16() == 429 || status.is_server_error() {
            if usage_limit_error(&bytes) {
                return Err(SpecLoopError::Workflow(
                    "ChatGPT subscription or usage limit was reached; review plan access and usage limits".to_owned(),
                ));
            }
            if retry_count < MAX_PROVIDER_RETRIES {
                wait_before_retry(retry_count, retry_after.as_deref(), deadline, cancelled).await?;
                retry_count = retry_count.checked_add(1).ok_or_else(|| {
                    SpecLoopError::Workflow("provider retry counter overflowed".to_owned())
                })?;
                continue;
            }
            return Err(SpecLoopError::Workflow(format!(
                "OpenAI Responses provider failure (HTTP {}) after {} retries",
                status.as_u16(),
                MAX_PROVIDER_RETRIES
            )));
        }
        return Err(SpecLoopError::Workflow(format!(
            "OpenAI Responses rejected the request (HTTP {}); verify model and agent configuration",
            status.as_u16()
        )));
    }
}

enum StreamFailure {
    Network,
    RetryableProvider,
    Fatal(SpecLoopError),
}

async fn read_streamed_response(
    mut response: reqwest::Response,
    deadline: Option<Instant>,
    cancelled: &AtomicBool,
) -> std::result::Result<Value, StreamFailure> {
    let mut pending = Vec::new();
    let mut state = SseState::default();
    loop {
        let next = await_limited(response.chunk(), deadline, cancelled)
            .await
            .map_err(StreamFailure::Fatal)?;
        let Some(chunk) = next.map_err(|_| StreamFailure::Network)? else {
            break;
        };
        pending.extend_from_slice(&chunk);
        while let Some(line_end) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line_bytes = pending.drain(..=line_end).collect::<Vec<_>>();
            if line_bytes.pop().is_none() {
                return Err(StreamFailure::Fatal(SpecLoopError::Agent(
                    "OpenAI Responses stream contained an empty line buffer".to_owned(),
                )));
            }
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let line = String::from_utf8(line_bytes).map_err(|error| {
                StreamFailure::Fatal(SpecLoopError::Agent(format!(
                    "OpenAI Responses stream contains invalid UTF-8: {error}"
                )))
            })?;
            state.accept_line(&line)?;
        }
        if state.completed.is_some() {
            if !pending.is_empty() || !state.is_event_empty() {
                return Err(StreamFailure::Fatal(SpecLoopError::Agent(
                    "OpenAI Responses stream emitted data after completion".to_owned(),
                )));
            }
            return state.completed.take().ok_or_else(|| {
                StreamFailure::Fatal(SpecLoopError::Agent(
                    "OpenAI Responses stream lost its completed response".to_owned(),
                ))
            });
        }
    }
    if !pending.is_empty() || !state.is_event_empty() {
        return Err(StreamFailure::Fatal(SpecLoopError::Agent(
            "OpenAI Responses stream ended with a partial event".to_owned(),
        )));
    }
    state.completed.ok_or_else(|| {
        StreamFailure::Fatal(SpecLoopError::Agent(
            "OpenAI Responses stream ended without response.completed".to_owned(),
        ))
    })
}

#[derive(Default)]
struct SseState {
    event_name: Option<String>,
    data: String,
    lifecycle: ResponseLifecycle,
    output_items: Vec<Value>,
    completed: Option<Value>,
}

#[derive(Default)]
enum ResponseLifecycle {
    #[default]
    AwaitingCreated,
    AwaitingInProgress,
    InProgress,
    Settled,
}

impl SseState {
    fn accept_line(&mut self, line: &str) -> std::result::Result<(), StreamFailure> {
        if line.is_empty() {
            return self.finish_event();
        }
        if let Some(data) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
        } else if let Some(event) = line.strip_prefix("event:") {
            self.event_name = Some(event.trim().to_owned());
        } else if !line.starts_with(':') && !line.starts_with("id:") && !line.starts_with("retry:")
        {
            return Err(StreamFailure::Fatal(SpecLoopError::Agent(
                "OpenAI Responses stream contains an unknown SSE field".to_owned(),
            )));
        }
        Ok(())
    }

    fn finish_event(&mut self) -> std::result::Result<(), StreamFailure> {
        if self.data.is_empty() && self.event_name.is_none() {
            return Ok(());
        }
        if self.completed.is_some() {
            return Err(StreamFailure::Fatal(SpecLoopError::Agent(
                "OpenAI Responses stream emitted an event after completion".to_owned(),
            )));
        }
        let event: Value = serde_json::from_str(&self.data).map_err(|error| {
            StreamFailure::Fatal(SpecLoopError::Agent(format!(
                "OpenAI Responses SSE event is malformed JSON: {error}"
            )))
        })?;
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .or(self.event_name.as_deref())
            .map(str::to_owned)
            .ok_or_else(|| {
                StreamFailure::Fatal(SpecLoopError::Agent(
                    "OpenAI Responses SSE event type is missing".to_owned(),
                ))
            })?;
        if self
            .event_name
            .as_deref()
            .is_some_and(|name| name != event_type)
        {
            return Err(StreamFailure::Fatal(SpecLoopError::Agent(
                "OpenAI Responses stream event name disagrees with its payload".to_owned(),
            )));
        }
        self.accept_event(&event_type, &event)?;
        self.event_name = None;
        self.data.clear();
        Ok(())
    }

    fn accept_event(
        &mut self,
        event_type: &str,
        event: &Value,
    ) -> std::result::Result<(), StreamFailure> {
        match event_type {
            "error" => {
                let code = event
                    .get("error")
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_str);
                if code.is_some_and(is_usage_limit_code) {
                    return Err(StreamFailure::Fatal(SpecLoopError::Workflow(
                        "ChatGPT subscription or usage limit was reached; review plan access and usage limits"
                            .to_owned(),
                    )));
                }
                if code.is_some_and(is_transient_provider_failure_code) {
                    return Err(StreamFailure::RetryableProvider);
                }
                Err(StreamFailure::Fatal(SpecLoopError::Workflow(
                    "OpenAI Responses stream reported a provider failure".to_owned(),
                )))
            }
            "response.created" => {
                if !matches!(self.lifecycle, ResponseLifecycle::AwaitingCreated) {
                    return Err(invalid_event_order(event_type));
                }
                self.lifecycle = ResponseLifecycle::AwaitingInProgress;
                Ok(())
            }
            "response.in_progress" => {
                if !matches!(self.lifecycle, ResponseLifecycle::AwaitingInProgress) {
                    return Err(invalid_event_order(event_type));
                }
                self.lifecycle = ResponseLifecycle::InProgress;
                Ok(())
            }
            "response.failed" | "response.incomplete" => {
                if !matches!(self.lifecycle, ResponseLifecycle::InProgress) {
                    return Err(invalid_event_order(event_type));
                }
                self.lifecycle = ResponseLifecycle::Settled;
                let detail = event
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .or_else(|| event.get("error"));
                let code = detail
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_str);
                if code.is_some_and(is_usage_limit_code) {
                    return Err(StreamFailure::Fatal(SpecLoopError::Workflow(
                        "ChatGPT subscription or usage limit was reached; review plan access and usage limits"
                            .to_owned(),
                    )));
                }
                if event_type == "response.failed"
                    && code.is_some_and(is_transient_provider_failure_code)
                {
                    return Err(StreamFailure::RetryableProvider);
                }
                Err(StreamFailure::Fatal(SpecLoopError::Workflow(
                    "OpenAI Responses stream reported a provider failure".to_owned(),
                )))
            }
            "response.completed" => self.accept_completed_event(event_type, event),
            "response.output_item.done" => {
                if !matches!(self.lifecycle, ResponseLifecycle::InProgress) {
                    return Err(invalid_event_order(event_type));
                }
                let item = event.get("item").cloned().ok_or_else(|| {
                    StreamFailure::Fatal(SpecLoopError::Agent(
                        "response.output_item.done event omitted its item".to_owned(),
                    ))
                })?;
                self.output_items.push(item);
                Ok(())
            }
            _ if is_response_progress_event(event_type) => {
                if !matches!(self.lifecycle, ResponseLifecycle::InProgress) {
                    return Err(invalid_event_order(event_type));
                }
                Ok(())
            }
            _ => Err(StreamFailure::Fatal(SpecLoopError::Agent(format!(
                "OpenAI Responses stream contains unsupported event type `{event_type}`"
            )))),
        }
    }

    fn accept_completed_event(
        &mut self,
        event_type: &str,
        event: &Value,
    ) -> std::result::Result<(), StreamFailure> {
        if !matches!(self.lifecycle, ResponseLifecycle::InProgress) {
            return Err(invalid_event_order(event_type));
        }
        let mut response = event.get("response").cloned().ok_or_else(|| {
            StreamFailure::Fatal(SpecLoopError::Agent(
                "response.completed event omitted its response object".to_owned(),
            ))
        })?;
        if response.get("status").and_then(Value::as_str) != Some("completed") {
            return Err(StreamFailure::Fatal(SpecLoopError::Agent(
                "response.completed event did not contain a settled response".to_owned(),
            )));
        }
        let output = response
            .get_mut("output")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| {
                StreamFailure::Fatal(SpecLoopError::Agent(
                    "response.completed event omitted its output array".to_owned(),
                ))
            })?;
        if output.is_empty() {
            *output = std::mem::take(&mut self.output_items);
        }
        self.lifecycle = ResponseLifecycle::Settled;
        self.completed = Some(response);
        Ok(())
    }

    const fn is_event_empty(&self) -> bool {
        self.event_name.is_none() && self.data.is_empty()
    }
}

fn invalid_event_order(event_type: &str) -> StreamFailure {
    StreamFailure::Fatal(SpecLoopError::Agent(format!(
        "OpenAI Responses stream contains invalid event order at `{event_type}`"
    )))
}

fn is_response_progress_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.content_part.added"
            | "response.content_part.done"
            | "response.file_search_call.completed"
            | "response.file_search_call.in_progress"
            | "response.file_search_call.searching"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.image_generation_call.completed"
            | "response.mcp_call.completed"
            | "response.mcp_call.failed"
            | "response.mcp_call.in_progress"
            | "response.mcp_call_arguments.delta"
            | "response.mcp_call_arguments.done"
            | "response.metadata"
            | "response.output_item.added"
            | "response.output_text.annotation.added"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "response.web_search_call.completed"
            | "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.code_interpreter_call.completed"
            | "response.code_interpreter_call.in_progress"
            | "response.code_interpreter_call.interpreting"
            | "response.computer_call.completed"
            | "response.computer_call.in_progress"
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done"
    )
}

fn is_transient_provider_failure_code(code: &str) -> bool {
    matches!(
        code,
        "rate_limit_exceeded"
            | "server_error"
            | "temporarily_unavailable"
            | "subscription_sharing_usage_unavailable"
    )
}

fn usage_limit_error(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("code").or_else(|| error.get("type")))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|code| is_usage_limit_code(&code))
}

fn is_usage_limit_code(code: &str) -> bool {
    let code = code.to_ascii_lowercase();
    if code == "subscription_sharing_usage_unavailable" {
        return false;
    }
    code.contains("quota") || code.contains("usage_limit") || code.contains("usage_unavailable")
}

fn retry_delay(retry_count: u8, retry_after: Option<&str>) -> Duration {
    let exponential_seconds = 1_u64
        .checked_shl(u32::from(retry_count))
        .unwrap_or(MAX_RETRY_DELAY_SECONDS);
    let exponential = Duration::from_secs(exponential_seconds);
    let hinted = retry_after.and_then(parse_retry_after);
    hinted
        .unwrap_or(exponential)
        .min(Duration::from_secs(MAX_RETRY_DELAY_SECONDS))
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value).ok().map(|retry_at| {
        retry_at
            .duration_since(std::time::SystemTime::now())
            .unwrap_or(Duration::ZERO)
    })
}

async fn wait_before_retry(
    retry_count: u8,
    retry_after: Option<&str>,
    deadline: Option<Instant>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let delay = retry_delay(retry_count, retry_after);
    await_limited(
        async move {
            tokio::time::sleep(delay).await;
            Ok::<(), SpecLoopError>(())
        },
        deadline,
        cancelled,
    )
    .await??;
    Ok(())
}

async fn await_limited<T, E>(
    future: impl Future<Output = std::result::Result<T, E>>,
    deadline: Option<Instant>,
    cancelled: &AtomicBool,
) -> Result<std::result::Result<T, E>> {
    tokio::pin!(future);
    let stop = async {
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return SpecLoopError::Interrupted;
            }
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return SpecLoopError::Timeout("built-in agent role deadline expired".to_owned());
            }
            tokio::time::sleep(STOP_POLL_INTERVAL).await;
        }
    };
    tokio::pin!(stop);
    tokio::select! {
        biased;
        error = &mut stop => Err(error),
        result = &mut future => {
            ensure_active(deadline, cancelled)?;
            Ok(result)
        }
    }
}

fn ensure_active(deadline: Option<Instant>, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(SpecLoopError::Interrupted);
    }
    if deadline.is_some_and(|limit| Instant::now() >= limit) {
        return Err(SpecLoopError::Timeout(
            "built-in agent role deadline expired".to_owned(),
        ));
    }
    Ok(())
}

struct ShellCall {
    call_id: String,
    command: String,
}

fn completed_output(response: &Value) -> Result<Vec<Value>> {
    if response.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(SpecLoopError::Agent(
            "OpenAI Responses result did not reach completed state".to_owned(),
        ));
    }
    response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| SpecLoopError::Agent("OpenAI Responses output is missing".to_owned()))
}

fn parse_output_items(output: &[Value]) -> Result<(Vec<ShellCall>, Option<String>)> {
    let mut calls = Vec::new();
    let mut final_text = String::new();
    let mut saw_text = false;
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let name = item.get("name").and_then(Value::as_str);
                let namespace = item.get("namespace").and_then(Value::as_str);
                let is_shell = matches!(
                    (name, namespace),
                    (Some("spec_loop.shell"), None | Some("spec_loop"))
                        | (Some("shell"), Some("spec_loop"))
                );
                if !is_shell {
                    let received_name = match name {
                        Some("shell") => "shell",
                        Some(_) => "other",
                        None => "missing",
                    };
                    let received_namespace = match item.get("namespace").and_then(Value::as_str) {
                        Some("spec_loop") => "spec_loop",
                        Some(_) => "other",
                        None => "missing",
                    };
                    return Err(SpecLoopError::Agent(format!(
                        "OpenAI Responses requested unsupported tool `{received_name}` in namespace `{received_namespace}`"
                    )));
                }
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|call_id| !call_id.is_empty())
                    .ok_or_else(|| {
                        SpecLoopError::Agent("shell tool call omitted call_id".to_owned())
                    })?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        SpecLoopError::Agent("shell tool arguments are missing".to_owned())
                    })?;
                let arguments: Value = serde_json::from_str(arguments).map_err(|error| {
                    SpecLoopError::Agent(format!("shell tool arguments are malformed: {error}"))
                })?;
                let command = arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .filter(|command| !command.is_empty())
                    .ok_or_else(|| {
                        SpecLoopError::Agent(
                            "shell tool call requires a non-empty command".to_owned(),
                        )
                    })?;
                calls.push(ShellCall {
                    call_id: call_id.to_owned(),
                    command: command.to_owned(),
                });
            }
            Some("message") => {
                let Some(content) = item.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(text) = part.get("text").and_then(Value::as_str)
                    {
                        if saw_text {
                            final_text.push('\n');
                        }
                        final_text.push_str(text);
                        saw_text = true;
                    }
                }
            }
            Some("reasoning" | "file_search_call" | "web_search_call") => {}
            Some(_) | None => {
                return Err(SpecLoopError::Agent(
                    "OpenAI Responses output contains an unsupported or malformed item".to_owned(),
                ));
            }
        }
    }
    Ok((calls, saw_text.then_some(final_text)))
}

struct ShellResult {
    tool_output: String,
    log_output: String,
    termination: ProcessTermination,
}

fn render_captured_streams(
    stdout: &[u8],
    stderr: &[u8],
    max_output_bytes: usize,
) -> (String, String, bool, bool) {
    let stdout_limit = max_output_bytes / 2 + max_output_bytes % 2;
    let stderr_limit = max_output_bytes / 2;
    let (stdout, stdout_truncated) = render_captured_stream(stdout, stdout_limit);
    let (stderr, stderr_truncated) = render_captured_stream(stderr, stderr_limit);
    (stdout, stderr, stdout_truncated, stderr_truncated)
}

fn render_captured_stream(bytes: &[u8], limit: usize) -> (String, bool) {
    let rendered = String::from_utf8_lossy(bytes);
    if rendered.len() <= limit {
        return (rendered.into_owned(), false);
    }
    let mut end = limit;
    while !rendered.is_char_boundary(end) {
        end -= 1;
    }
    (rendered[..end].to_owned(), true)
}

fn run_shell(
    root: &Path,
    command: &str,
    max_output_bytes: usize,
    deadline: Option<Instant>,
    cancelled: &Arc<AtomicBool>,
) -> Result<ShellResult> {
    #[cfg(unix)]
    {
        let output = run_process_until(
            Path::new("/bin/sh"),
            &["-c".to_owned(), command.to_owned()],
            root,
            deadline,
            Some(max_output_bytes),
            cancelled,
        )?;
        let termination = if cancelled.load(Ordering::Relaxed) {
            ProcessTermination::Interrupted
        } else {
            output.termination
        };
        let status = output
            .status
            .and_then(|status| status.code())
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        let (stdout, stderr, stdout_render_truncated, stderr_render_truncated) =
            render_captured_streams(&output.stdout, &output.stderr, max_output_bytes);
        let termination_name = match termination {
            ProcessTermination::Exited => "exited",
            ProcessTermination::TimedOut => "timed_out",
            ProcessTermination::Interrupted => "interrupted",
        };
        let mut tool_output = format!(
            "termination={termination_name}\nexit_status={status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        if output.stdout_truncated
            || output.stderr_truncated
            || stdout_render_truncated
            || stderr_render_truncated
        {
            tool_output.push_str("\n[tool output truncated at configured byte limit]");
        }
        if !status_success_for_tool(&output) {
            tool_output.push_str("\n[command exited unsuccessfully]");
        }
        Ok(ShellResult {
            log_output: tool_output.clone(),
            tool_output,
            termination,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (root, command, max_output_bytes, deadline, cancelled);
        Err(SpecLoopError::Workflow(
            "built-in shell tool is supported only on POSIX platforms with `/bin/sh`".to_owned(),
        ))
    }
}

#[cfg(unix)]
fn status_success_for_tool(output: &crate::commands::ChildOutput) -> bool {
    output.termination == ProcessTermination::Exited
        && output.status.is_some_and(|status| status.success())
}

fn append_transcript(transcript: &mut Vec<u8>, command: &str, output: &str) -> Result<()> {
    let entry = format!("$ {command}\n{output}\n\n");
    transcript.try_reserve(entry.len()).map_err(|error| {
        SpecLoopError::Workflow(format!("reserve built-in shell transcript space: {error}"))
    })?;
    transcript.extend_from_slice(entry.as_bytes());
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{
        RESPONSES_URL, ResponseLifecycle, RoleInput, RoleTask, SseState, StreamFailure,
        classify_agent_run, completed_output, parse_output_items, render_captured_streams,
        request_with_retries, retry_delay, run_after_shell_preflight, run_role_with_access_token,
        run_role_with_auth, run_role_with_continuation, run_shell, shell_tool_definition,
        stage_for_role, usage_limit_error,
    };
    use crate::backend::BackendFailureKind;
    use crate::config::{AgentStage, Profile};
    use crate::prompts::PromptRole;

    const RESPONSE_STARTED: &str = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\"}}\n\n",
        "event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\"}}\n\n"
    );

    fn sse_event(event_type: &str, event: &Value) -> String {
        format!("event: {event_type}\ndata: {event}\n\n")
    }

    fn completed_sse(event: &Value) -> String {
        format!(
            "{RESPONSE_STARTED}{}",
            sse_event("response.completed", event)
        )
    }

    fn fake_stream_request(body: String) -> (crate::error::Result<Value>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            let request = read_http_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
            request
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build provider client");
        let result = crate::runtime::block_on(request_with_retries(
            &client,
            &format!("http://{address}/v1/responses"),
            "fixture-access-token",
            &json!({"model": "fixture-model", "stream": true}),
            None,
            &AtomicBool::new(false),
        ))
        .expect("construct provider runtime");
        (result, server.join().expect("join provider fixture"))
    }

    fn assert_transient_sse_failure_retries(event_type: &str, event: &Value) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let failure = if event_type == "error" {
            sse_event(event_type, event)
        } else {
            format!("{RESPONSE_STARTED}{}", sse_event(event_type, event))
        };
        let success = completed_sse(&json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": []}
        }));
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for body in [failure, success] {
                let (mut stream, _) = listener.accept().expect("accept provider request");
                requests.push(read_http_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write provider response");
            }
            requests
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build provider client");
        let result = crate::runtime::block_on(request_with_retries(
            &client,
            &format!("http://{address}/v1/responses"),
            "fixture-access-token",
            &json!({"model": "fixture-model", "stream": true}),
            None,
            &AtomicBool::new(false),
        ))
        .expect("construct provider runtime")
        .expect("transient stream error should retry");
        assert_eq!(result["status"], "completed");
        let requests = server.join().expect("join provider fixture");
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with("POST /v1/responses "))
        );
    }

    fn builtin_test_profile() -> Profile {
        let profile: Profile = toml::from_str(
            r"schema_version=3
[specs]
active_globs=['specs/*.md']
id_regex='^specs/(?P<id>[0-9]+)\\.md$'
[agent]
backend='builtin'
model='fixture-model'
",
        )
        .expect("parse built-in test profile");
        profile.validate().expect("validate built-in test profile");
        profile
    }

    #[cfg(unix)]
    #[test]
    fn missing_local_direct_permission_sends_zero_responses_requests() {
        let temporary = TempDir::new().expect("create auth fixture");
        let credential_path = temporary.path().join("auth/auth.json");
        crate::auth::write_test_credentials(&credential_path, vec!["openid".to_owned()])
            .expect("save credential without direct-use scope");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider counter");
        listener
            .set_nonblocking(true)
            .expect("make provider counter nonblocking");
        let address = listener.local_addr().expect("provider counter address");
        let profile = builtin_test_profile();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut completed_conversation = None;
        let mut transcript = Vec::new();
        let endpoint = format!("http://{address}/v1/responses");
        let result = crate::runtime::block_on(run_role_with_auth(
            crate::auth::active_access_token_at(&credential_path, false, cancelled.as_ref()),
            RoleTask {
                root: temporary.path(),
                profile: &profile,
                prompt: "original task prompt",
                role: PromptRole::Implementation,
                cancelled: &cancelled,
                deadline: None,
                endpoint: &endpoint,
            },
            None,
            &mut completed_conversation,
            &mut transcript,
        ))
        .expect("construct test runtime");
        let error = result.expect_err("missing local scope must stop before provider");
        assert!(error.to_string().contains("chatgpt.tokens.use.direct"));
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn unsupported_platform_fails_before_provider_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let provider_requests = AtomicUsize::new(0);
        let result = crate::runtime::block_on(run_after_shell_preflight(
            Path::new("/bin/sh"),
            false,
            || async {
                provider_requests.fetch_add(1, Ordering::Relaxed);
                Ok("final text without shell call")
            },
        ))
        .expect("construct runtime");

        let error = result.expect_err("unsupported platform must fail preflight");
        assert!(error.to_string().contains("POSIX platform"));
        assert_eq!(provider_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn missing_shell_fails_before_provider_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temporary = TempDir::new().expect("create shell prerequisite fixture");
        let missing_shell = temporary.path().join("missing-sh");
        let provider_requests = AtomicUsize::new(0);
        let result =
            crate::runtime::block_on(run_after_shell_preflight(&missing_shell, true, || async {
                provider_requests.fetch_add(1, Ordering::Relaxed);
                Ok("final text without shell call")
            }))
            .expect("construct runtime");

        let error = result.expect_err("missing shell must fail preflight");
        assert!(error.to_string().contains("executable `/bin/sh`"));
        assert_eq!(provider_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn all_roles_map_to_their_configured_stage_family() {
        assert_eq!(stage_for_role(PromptRole::PlanAuthor), AgentStage::Planning);
        assert_eq!(
            stage_for_role(PromptRole::PlanClarification),
            AgentStage::Planning
        );
        assert_eq!(
            stage_for_role(PromptRole::Implementation),
            AgentStage::Implementation
        );
        assert_eq!(stage_for_role(PromptRole::ReviewAudit), AgentStage::Review);
    }

    #[test]
    fn shell_tool_schema_has_only_command_argument() {
        let tool = shell_tool_definition();
        assert_eq!(tool["type"], "namespace");
        assert_eq!(tool["name"], "spec_loop");
        assert_eq!(tool["tools"][0]["type"], "function");
        assert_eq!(tool["tools"][0]["name"], "shell");
        assert_eq!(
            tool["tools"][0]["parameters"]["required"],
            json!(["command"])
        );
        assert_eq!(
            tool["tools"][0]["parameters"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn completed_function_calls_are_transport_not_final_text() {
        let response = json!({
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call-1",
                "name": "shell",
                "namespace": "spec_loop",
                "arguments": "{\"command\":\"printf ok\"}"
            }]
        });
        let output = completed_output(&response).expect("completed response output");
        let (calls, final_text) = parse_output_items(&output).expect("parse function call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "call-1");
        assert_eq!(calls[0].command, "printf ok");
        assert_eq!(final_text, None);
        assert!(
            parse_output_items(&[json!({
                "type": "function_call",
                "call_id": "qualified-call",
                "name": "spec_loop.shell",
                "arguments": "{\"command\":\"printf ok\"}"
            })])
            .is_ok()
        );
        let error = parse_output_items(&[json!({
            "type": "function_call",
            "call_id": "unqualified-call",
            "name": "shell",
            "arguments": "{\"command\":\"printf nope\"}"
        })])
        .err()
        .map(|error| error.to_string())
        .expect("reject an unqualified tool name");
        assert!(error.contains("unsupported tool `shell`"));
    }

    #[test]
    fn final_message_text_is_separate_from_tool_output() {
        let output = vec![json!({
            "type": "message",
            "content": [{"type": "output_text", "text": "{\"status\":\"ready_for_review\"}"}]
        })];
        let (calls, final_text) = parse_output_items(&output).expect("parse final message");
        assert!(calls.is_empty());
        assert_eq!(
            final_text.as_deref(),
            Some("{\"status\":\"ready_for_review\"}")
        );
    }

    #[test]
    fn responses_request_uses_oauth_and_expected_body_with_fake_http_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            let request = read_http_request(&mut stream);
            let body = completed_sse(&json!({
                "type": "response.completed",
                "response": {"status": "completed", "output": [], "model": "fixture-model"}
            }));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
            request
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build provider client");
        let body = json!({
            "model": "fixture-model",
            "store": false,
            "stream": true,
            "instructions": "tool transport",
            "input": [{"role": "user", "content": "role prompt"}],
            "tools": [shell_tool_definition()],
            "tool_choice": "auto"
        });
        let address = format!("http://{address}/v1/responses");
        let response = crate::runtime::block_on(request_with_retries(
            &client,
            &address,
            "fixture-access-token",
            &body,
            None,
            &AtomicBool::new(false),
        ))
        .expect("construct request runtime")
        .expect("successful fake provider response");
        assert_eq!(response["status"], "completed");
        assert_eq!(RESPONSES_URL, "https://api.openai.com/v1/responses");
        let request = server.join().expect("join provider server");
        let (headers, payload) = request
            .split_once("\r\n\r\n")
            .expect("provider request headers");
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-access-token")
        );
        let payload: Value = serde_json::from_str(payload).expect("parse provider request body");
        assert_eq!(payload["model"], "fixture-model");
        assert_eq!(payload["store"], false);
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["input"][0]["content"], "role prompt");
        assert_eq!(
            payload["tools"],
            json!([{
                "type": "namespace",
                "name": "spec_loop",
                "description": "Spec Loop local tools.",
                "tools": [{
                    "type": "function",
                    "name": "shell",
                    "description": "Run one shell command from the project root. Commands run automatically as the current user and are not sandboxed.",
                    "parameters": {
                        "type": "object",
                        "properties": {"command": {"type": "string"}},
                        "required": ["command"],
                        "additionalProperties": false
                    },
                    "strict": true
                }]
            }])
        );
    }

    #[test]
    fn out_of_order_completed_event_fails_fatally_without_role_retry() {
        let malformed = format!(
            "{}{}",
            sse_event(
                "response.created",
                &json!({"type": "response.created", "response": {"status": "in_progress"}})
            ),
            sse_event(
                "response.completed",
                &json!({
                    "type": "response.completed",
                    "response": {"status": "completed", "output": []}
                })
            )
        );
        let (response, request) = fake_stream_request(malformed);
        let error = response.expect_err("completion before in-progress must fail");
        assert!(error.to_string().contains("invalid event order"));
        let run = classify_agent_run(Err(error), Vec::new());
        let failure = run
            .outcome
            .expect_err("malformed stream must be fatal, not generically retried");
        assert_eq!(failure.kind(), BackendFailureKind::Fatal);
        assert!(request.starts_with("POST /v1/responses "));
    }

    #[test]
    fn streamed_output_item_recovers_empty_completed_output() {
        let call = json!({
            "type": "function_call",
            "call_id": "call_fixture",
            "name": "spec_loop.shell",
            "arguments": "{\"command\":\"printf test\"}"
        });
        let body = format!(
            "{RESPONSE_STARTED}{}{}{}",
            sse_event(
                "response.metadata",
                &json!({"type": "response.metadata", "metadata": {"fixture": true}})
            ),
            sse_event(
                "response.output_item.done",
                &json!({"type": "response.output_item.done", "output_index": 0, "item": call})
            ),
            sse_event(
                "response.completed",
                &json!({
                    "type": "response.completed",
                    "response": {"status": "completed", "output": []}
                })
            )
        );
        let (response, request) = fake_stream_request(body);
        let response = response.expect("recover the completed streamed item");
        assert_eq!(response["output"], json!([call]));
        let output = response["output"]
            .as_array()
            .expect("recovered response output array");
        let (calls, final_text) =
            parse_output_items(output).expect("parse recovered function call as transport");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "printf test");
        assert_eq!(final_text, None);
        assert!(request.starts_with("POST /v1/responses "));
    }

    #[test]
    fn unknown_response_event_is_rejected_after_stream_starts() {
        let malformed = format!(
            "{RESPONSE_STARTED}{}{}",
            sse_event(
                "response.future_event",
                &json!({"type": "response.future_event"})
            ),
            sse_event(
                "response.completed",
                &json!({
                    "type": "response.completed",
                    "response": {"status": "completed", "output": []}
                })
            )
        );
        let (response, request) = fake_stream_request(malformed);
        let error = response.expect_err("unknown response event must fail");
        assert!(
            error
                .to_string()
                .contains("unsupported event type `response.future_event`")
        );
        let run = classify_agent_run(Err(error), Vec::new());
        assert_eq!(
            run.outcome
                .expect_err("malformed stream must be fatal")
                .kind(),
            BackendFailureKind::Fatal
        );
        assert!(request.starts_with("POST /v1/responses "));
    }

    #[test]
    fn builtin_backend_accepts_typed_responses_for_every_role_from_fake_provider() {
        use std::sync::Arc;

        use crate::protocol::{
            ImplementationAuditResponse, ImplementationClarificationResponse,
            ImplementationResponse, ImplementationReviewResponse, PlanAuditResponse,
            PlanAuthorResponse, PlanClarificationResponse, PlanReviewResponse,
        };

        let roles = [
            (
                PromptRole::PlanAuthor,
                json!({"protocol_version": 1, "role": "plan_author", "status": "ready_for_review"}),
            ),
            (
                PromptRole::PlanReview,
                json!({"protocol_version": 1, "role": "plan_review", "status": "no_issues", "findings": []}),
            ),
            (
                PromptRole::PlanAudit,
                json!({"protocol_version": 1, "role": "plan_audit", "status": "complete", "classifications": [{"finding_id": 1, "classification": "supported", "actionability": "not_required", "rationale": "Evidence confirms observation; no correction is required."}]}),
            ),
            (
                PromptRole::PlanClarification,
                json!({"protocol_version": 1, "role": "plan_review_clarification", "status": "complete", "evidence": []}),
            ),
            (
                PromptRole::Implementation,
                json!({"protocol_version": 1, "role": "implementation", "status": "ready_for_review", "summary": "Fixture implementation response."}),
            ),
            (
                PromptRole::Review,
                json!({"protocol_version": 1, "role": "review", "status": "no_issues", "findings": []}),
            ),
            (
                PromptRole::ReviewAudit,
                json!({"protocol_version": 1, "role": "review_audit", "status": "complete", "classifications": [{"finding_id": 1, "classification": "supported", "actionability": "not_required", "rationale": "Evidence confirms observation; no correction is required."}]}),
            ),
            (
                PromptRole::ReviewClarification,
                json!({"protocol_version": 1, "role": "review_clarification", "status": "complete", "evidence": []}),
            ),
        ];
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
        let address = listener.local_addr().expect("fake provider address");
        let expected_requests = roles
            .iter()
            .map(|(role, response)| {
                let prompt = format!("exact prompt for {}", role.protocol_name());
                let response = json!({
                    "type": "response.completed",
                    "response": {
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "content": [{"type": "output_text", "text": response.to_string()}]
                        }]
                    }
                });
                (prompt, response)
            })
            .collect::<Vec<_>>();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for (_, response) in expected_requests {
                let body = completed_sse(&response);
                let (mut stream, _) = listener.accept().expect("accept fake provider request");
                requests.push(read_http_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write fake provider response");
            }
            requests
        });
        let profile: Profile = toml::from_str(
            r"schema_version=3
[specs]
active_globs=['specs/*.md']
id_regex='^specs/(?P<id>[0-9]+)\.md$'
[agent]
backend='builtin'
model='fixture-model'
",
        )
        .expect("parse built-in profile");
        profile.validate().expect("validate built-in profile");

        let mut expected_prompts = Vec::new();
        for (role, response) in roles {
            let prompt = format!("exact prompt for {}", role.protocol_name());
            let mut transcript = Vec::new();
            let result = crate::runtime::block_on(run_role_with_access_token(
                RoleInput {
                    root: Path::new("."),
                    profile: &profile,
                    prompt: &prompt,
                    role,
                    cancelled: &Arc::new(AtomicBool::new(false)),
                    deadline: None,
                    endpoint: &format!("http://{address}/v1/responses"),
                    token: "fixture-access-token",
                },
                &mut transcript,
            ))
            .expect("construct runtime")
            .expect("run role through fake provider");
            assert_eq!(result.response, response.to_string());
            assert_eq!(transcript, Vec::<u8>::default());
            expected_prompts.push(prompt);

            match role {
                PromptRole::PlanAuthor => {
                    crate::protocol::parse::<PlanAuthorResponse>(
                        &result.response,
                        role.protocol_name(),
                    )
                    .expect("validate plan-author final response");
                }
                PromptRole::PlanReview => {
                    crate::protocol::parse::<PlanReviewResponse>(
                        &result.response,
                        role.protocol_name(),
                    )
                    .expect("validate plan-review final response");
                }
                PromptRole::PlanAudit => {
                    crate::protocol::parse::<PlanAuditResponse>(
                        &result.response,
                        role.protocol_name(),
                    )
                    .expect("validate plan-audit final response");
                }
                PromptRole::PlanClarification => {
                    crate::protocol::parse::<PlanClarificationResponse>(
                        &result.response,
                        role.protocol_name(),
                    )
                    .expect("validate plan-clarification final response");
                }
                PromptRole::Implementation => {
                    crate::protocol::parse::<ImplementationResponse>(
                        &result.response,
                        role.protocol_name(),
                    )
                    .expect("validate implementation final response");
                }
                PromptRole::Review => {
                    crate::protocol::parse::<ImplementationReviewResponse>(
                        &result.response,
                        role.protocol_name(),
                    )
                    .expect("validate review final response");
                }
                PromptRole::ReviewAudit => {
                    crate::protocol::parse::<ImplementationAuditResponse>(
                        &result.response,
                        role.protocol_name(),
                    )
                    .expect("validate review-audit final response");
                }
                PromptRole::ReviewClarification => {
                    crate::protocol::parse::<ImplementationClarificationResponse>(
                        &result.response,
                        role.protocol_name(),
                    )
                    .expect("validate review-clarification final response");
                }
            }
        }

        let requests = server.join().expect("join fake provider");
        assert_eq!(requests.len(), expected_prompts.len());
        for (request, expected_prompt) in requests.iter().zip(expected_prompts) {
            let (_, payload) = request
                .split_once("\r\n\r\n")
                .expect("provider request body");
            let payload: Value = serde_json::from_str(payload).expect("parse request body");
            assert_eq!(payload["input"][0]["content"], expected_prompt);
            assert_eq!(payload["tools"][0]["type"], "namespace");
            assert_eq!(payload["tools"][0]["name"], "spec_loop");
            assert_eq!(payload["tools"][0]["tools"][0]["name"], "shell");
        }
    }

    #[cfg(unix)]
    #[test]
    fn configured_shell_call_limit_stops_after_permitted_calls_without_replay() {
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let temporary = TempDir::new().expect("create built-in project root");
        let marker = temporary.path().join("shell-invocations");
        let first_command = "printf x >> shell-invocations";
        let second_command = "printf y >> shell-invocations";
        let tool_response = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [
                    {
                        "type": "function_call",
                        "call_id": "call-permitted",
                        "name": "spec_loop.shell",
                        "arguments": serde_json::to_string(&json!({"command": first_command}))
                            .expect("serialize permitted shell command")
                    },
                    {
                        "type": "function_call",
                        "call_id": "call-over-limit",
                        "name": "spec_loop.shell",
                        "arguments": serde_json::to_string(&json!({"command": second_command}))
                            .expect("serialize over-limit shell command")
                    }
                ]
            }
        });
        let body = completed_sse(&tool_response);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            let request = read_http_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write multiple-call response");
            request
        });
        let profile: Profile = toml::from_str(
            r"schema_version=3
[specs]
active_globs=['specs/*.md']
id_regex='^specs/(?P<id>[0-9]+)\\.md$'
[agent]
backend='builtin'
model='fixture-model'
max_tool_calls=1
",
        )
        .expect("parse limited built-in profile");
        profile.validate().expect("validate limited profile");
        let mut transcript = Vec::new();
        let response = crate::runtime::block_on(run_role_with_access_token(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "tool-call limit fixture",
                role: PromptRole::Implementation,
                cancelled: &Arc::new(AtomicBool::new(false)),
                deadline: None,
                endpoint: &format!("http://{address}/v1/responses"),
                token: "fixture-access-token",
            },
            &mut transcript,
        ))
        .expect("construct runtime");
        let run = classify_agent_run(response, transcript);
        let failure = run
            .outcome
            .expect_err("configured call limit must fail role fatally");
        assert_eq!(failure.kind(), BackendFailureKind::Fatal);
        assert!(failure.to_string().contains("agent.max_tool_calls=1"));
        assert_eq!(
            std::fs::read_to_string(marker).expect("read permitted shell side effect"),
            "x"
        );
        assert!(String::from_utf8_lossy(&run.stdout).contains("exit_status=0"));

        let request = server.join().expect("join provider fixture");
        assert!(request.starts_with("POST /v1/responses "));
    }

    #[cfg(unix)]
    #[test]
    fn transient_followup_retry_reuses_completed_shell_tool_result() {
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let temporary = TempDir::new().expect("create built-in project root");
        let marker = temporary.path().join("shell-invocations");
        let command = format!("printf x >> {}", marker.display());
        let tool_response = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "call_id": "call-once",
                    "name": "spec_loop.shell",
                    "arguments": serde_json::to_string(&json!({"command": command}))
                        .expect("serialize shell command")
                }]
            }
        });
        let final_response = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "model": "fixture-model",
                "output": [{
                    "type": "message",
                    "content": [{
                        "type": "output_text",
                        "text": r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Retried follow-up without replay."}"#
                    }]
                }]
            }
        });
        let event_body = completed_sse;
        let server = thread::spawn(move || {
            let successful_responses = [event_body(&tool_response), event_body(&final_response)];
            let responses = [
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    successful_responses[0].len(),
                    successful_responses[0]
                ),
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    successful_responses[1].len(),
                    successful_responses[1]
                ),
            ];
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept provider request");
                requests.push(read_http_request(&mut stream));
                stream
                    .write_all(response.as_bytes())
                    .expect("write provider response");
            }
            requests
        });
        let profile: Profile = toml::from_str(
            r"schema_version=3
[specs]
active_globs=['specs/*.md']
id_regex='^specs/(?P<id>[0-9]+)\.md$'
[agent]
backend='builtin'
model='fixture-model'
",
        )
        .expect("parse built-in profile");
        profile.validate().expect("validate built-in profile");
        let mut transcript = Vec::new();
        let result = crate::runtime::block_on(run_role_with_access_token(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "unchanged implementation prompt",
                role: PromptRole::Implementation,
                cancelled: &Arc::new(AtomicBool::new(false)),
                deadline: None,
                endpoint: &format!("http://{address}/v1/responses"),
                token: "fixture-access-token",
            },
            &mut transcript,
        ))
        .expect("construct runtime")
        .expect("run built-in role");
        assert!(result.response.contains("Retried follow-up"));
        assert_eq!(
            std::fs::read_to_string(marker).expect("read shell marker"),
            "x"
        );
        assert!(String::from_utf8_lossy(&transcript).contains("exit_status=0"));

        let requests = server.join().expect("join fake provider");
        assert_eq!(requests.len(), 3);
        let parse_body = |request: &str| {
            let (_, body) = request
                .split_once("\r\n\r\n")
                .expect("provider request body");
            serde_json::from_str::<Value>(body).expect("parse provider request")
        };
        let initial = parse_body(&requests[0]);
        let failed_followup = parse_body(&requests[1]);
        let retried_followup = parse_body(&requests[2]);
        assert_eq!(
            initial["input"][0]["content"],
            "unchanged implementation prompt"
        );
        assert_eq!(failed_followup, retried_followup);
        assert_eq!(retried_followup["input"][2]["call_id"], "call-once");
        assert!(
            retried_followup["input"][2]["output"]
                .as_str()
                .is_some_and(|output| output.contains("exit_status=0"))
        );
    }

    #[cfg(unix)]
    fn start_malformed_typed_response_provider(
        command: &str,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let tool_response = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "call_id": "typed-call-once",
                    "name": "spec_loop.shell",
                    "arguments": serde_json::to_string(&json!({"command": command}))
                        .expect("serialize shell command")
                }]
            }
        });
        let malformed_response = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "not valid typed JSON"}]
                }]
            }
        });
        let corrected_response = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "model": "fixture-model",
                "output": [{
                    "type": "message",
                    "content": [{
                        "type": "output_text",
                        "text": r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Corrected handoff only."}"#
                    }]
                }]
            }
        });
        let provider_responses = [
            completed_sse(&tool_response),
            completed_sse(&malformed_response),
            completed_sse(&corrected_response),
        ];
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for response_body in provider_responses {
                let (mut stream, _) = listener.accept().expect("accept provider request");
                requests.push(read_http_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write provider response");
            }
            requests
        });
        (format!("http://{address}/v1/responses"), server)
    }

    #[cfg(unix)]
    #[test]
    fn malformed_typed_response_correction_reuses_completed_shell_state() {
        let temporary = TempDir::new().expect("create project root");
        let marker = temporary.path().join("shell-invocations");
        let command = format!("printf x >> {}", marker.display());
        let (endpoint, server) = start_malformed_typed_response_provider(&command);
        let profile = builtin_test_profile();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut transcript = Vec::new();
        let mut conversation = None;
        let malformed = crate::runtime::block_on(run_role_with_continuation(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "original implementation task",
                role: PromptRole::Implementation,
                cancelled: &cancelled,
                deadline: None,
                endpoint: &endpoint,
                token: "fixture-access-token",
            },
            None,
            &mut conversation,
            &mut transcript,
        ))
        .expect("construct initial runtime")
        .expect("receive malformed final text");
        assert_eq!(malformed.response, "not valid typed JSON");
        let conversation = conversation.expect("retain completed conversation for correction");

        let mut corrected_conversation = None;
        let corrected = crate::runtime::block_on(run_role_with_continuation(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "Correct malformed typed response only; do not invoke tools.",
                role: PromptRole::Implementation,
                cancelled: &cancelled,
                deadline: None,
                endpoint: &endpoint,
                token: "fixture-access-token",
            },
            Some(conversation),
            &mut corrected_conversation,
            &mut transcript,
        ))
        .expect("construct correction runtime")
        .expect("receive corrected final text");
        assert!(corrected.response.contains("Corrected handoff only"));
        assert_eq!(
            std::fs::read_to_string(marker).expect("read shell side effect"),
            "x"
        );

        let requests = server.join().expect("join fake provider");
        assert_eq!(requests.len(), 3);
        let bodies = requests
            .iter()
            .map(|request| {
                let (_, body) = request.split_once("\r\n\r\n").expect("request body");
                serde_json::from_str::<Value>(body).expect("parse provider request")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            bodies[0]["input"][0]["content"],
            "original implementation task"
        );
        assert_eq!(bodies[2]["input"][2]["call_id"], "typed-call-once");
        assert!(
            bodies[2]["input"][3]
                .to_string()
                .contains("not valid typed JSON")
        );
        assert!(
            bodies[2]["input"][2]["output"]
                .as_str()
                .is_some_and(|text| text.contains("exit_status=0"))
        );
        assert_eq!(bodies[2]["tool_choice"], "none");
        assert_eq!(
            bodies[2]["input"][4]["content"],
            "Correct malformed typed response only; do not invoke tools."
        );
    }

    #[derive(Clone, Copy)]
    enum StallPoint {
        BeforeHeaders,
        DuringStream,
        DuringBackoff,
    }

    fn start_stalled_provider(stall: StallPoint) -> (String, thread::JoinHandle<(String, usize)>) {
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled provider");
        let address = listener.local_addr().expect("stalled provider address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept stalled request");
            let first_request = read_http_request(&mut stream);
            match stall {
                StallPoint::BeforeHeaders => {}
                StallPoint::DuringStream => {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 4096\r\nConnection: keep-alive\r\n\r\n: waiting\n\n")
                        .expect("write initial stream data");
                }
                StallPoint::DuringBackoff => {
                    stream
                        .write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .expect("write rate-limit response");
                }
            }
            thread::sleep(Duration::from_millis(250));
            listener
                .set_nonblocking(true)
                .expect("make fixture listener nonblocking");
            let mut extra_requests = 0_usize;
            let observation_deadline = Instant::now() + Duration::from_millis(150);
            while Instant::now() < observation_deadline {
                match listener.accept() {
                    Ok((mut extra, _)) => {
                        extra_requests += 1;
                        let _ = read_http_request(&mut extra);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            (first_request, extra_requests)
        });
        (format!("http://{address}/v1/responses"), server)
    }

    #[test]
    fn provider_request_timeout_aborts_headers_and_stream_without_retry() {
        use std::time::{Duration, Instant};

        let profile = builtin_test_profile();
        let temporary = TempDir::new().expect("create built-in project root");
        for stall in [StallPoint::BeforeHeaders, StallPoint::DuringStream] {
            let (endpoint, server) = start_stalled_provider(stall);
            let cancelled = std::sync::Arc::new(AtomicBool::new(false));
            let result = crate::runtime::block_on(run_role_with_access_token(
                RoleInput {
                    root: temporary.path(),
                    profile: &profile,
                    prompt: "provider timeout fixture",
                    role: PromptRole::Implementation,
                    cancelled: &cancelled,
                    deadline: Some(Instant::now() + Duration::from_millis(120)),
                    endpoint: &endpoint,
                    token: "fixture-access-token",
                },
                &mut Vec::new(),
            ))
            .expect("construct runtime");
            let run = classify_agent_run(result, Vec::new());
            let failure = run.outcome.expect_err("timeout must fail backend attempt");
            assert_eq!(failure.kind(), BackendFailureKind::Fatal);
            assert!(failure.to_string().contains("role deadline expired"));
            let (request, extra_requests) = server.join().expect("join stalled provider");
            assert!(request.starts_with("POST /v1/responses "));
            assert_eq!(extra_requests, 0);
        }
    }

    #[test]
    fn provider_cancellation_aborts_headers_stream_and_retry_backoff() {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let profile = builtin_test_profile();
        let temporary = TempDir::new().expect("create built-in project root");
        for stall in [
            StallPoint::BeforeHeaders,
            StallPoint::DuringStream,
            StallPoint::DuringBackoff,
        ] {
            let (endpoint, server) = start_stalled_provider(stall);
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancellation_flag = Arc::clone(&cancelled);
            let cancel_task = thread::spawn(move || {
                thread::sleep(Duration::from_millis(60));
                cancellation_flag.store(true, Ordering::Relaxed);
            });
            let result = crate::runtime::block_on(run_role_with_access_token(
                RoleInput {
                    root: temporary.path(),
                    profile: &profile,
                    prompt: "provider cancellation fixture",
                    role: PromptRole::Implementation,
                    cancelled: &cancelled,
                    deadline: None,
                    endpoint: &endpoint,
                    token: "fixture-access-token",
                },
                &mut Vec::new(),
            ))
            .expect("construct runtime");
            cancel_task.join().expect("join cancellation task");
            let run = classify_agent_run(result, Vec::new());
            let failure = run
                .outcome
                .expect_err("cancellation must fail backend attempt");
            assert_eq!(failure.kind(), BackendFailureKind::Fatal);
            assert!(matches!(
                failure.into_error(),
                crate::error::SpecLoopError::Interrupted
            ));
            let (request, extra_requests) = server.join().expect("join stalled provider");
            assert!(request.starts_with("POST /v1/responses "));
            assert_eq!(extra_requests, 0);
        }
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2048];
        loop {
            let read = stream.read(&mut buffer).expect("read provider request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("request content length");
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8(request).expect("provider request is UTF-8")
    }

    #[cfg(unix)]
    #[test]
    fn shell_runs_from_project_root_and_bounds_combined_output() {
        use std::sync::Arc;

        let temporary = TempDir::new().expect("create shell project root");
        let root = run_shell(
            temporary.path(),
            "pwd",
            1024,
            None,
            &Arc::new(AtomicBool::new(false)),
        )
        .expect("run shell in project root");
        assert!(
            root.tool_output
                .contains(&temporary.path().display().to_string())
        );
        let result = run_shell(
            temporary.path(),
            "printf 1234; printf 5678 >&2; exit 7",
            6,
            None,
            &Arc::new(AtomicBool::new(false)),
        )
        .expect("run bounded shell tool");
        assert!(result.tool_output.contains("exit_status=7"));
        assert!(result.tool_output.contains("stdout:\n123"));
        assert!(result.tool_output.contains("stderr:\n56"));
        assert!(result.tool_output.contains("tool output truncated"));
        assert!(result.tool_output.contains("command exited unsuccessfully"));
    }

    #[test]
    fn lossy_stream_rendering_respects_combined_output_byte_limit() {
        let invalid_utf8 = [0xff; 6];

        let (stdout, stderr, stdout_truncated, stderr_truncated) =
            render_captured_streams(&invalid_utf8, &invalid_utf8, 6);

        assert!(stdout.len() + stderr.len() <= 6);
        assert_eq!(stdout, "�");
        assert_eq!(stderr, "�");
        assert!(stdout_truncated);
        assert!(stderr_truncated);
    }

    #[cfg(unix)]
    #[test]
    fn builtin_sync_backend_enforces_one_role_deadline_across_provider_and_shell() {
        use std::time::{Duration, Instant};

        use super::role_deadline;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let temporary = TempDir::new().expect("create built-in project root");
        let marker = temporary.path().join("deadline-shell-started");
        let command = "printf started > deadline-shell-started; sleep 30";
        let event = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "call_id": "deadline-call",
                    "name": "spec_loop.shell",
                    "arguments": serde_json::to_string(&json!({"command": command}))
                        .expect("serialize shell command")
                }]
            }
        });
        let body = completed_sse(&event);
        let server = thread::spawn(move || {
            use std::time::Duration;

            let (mut stream, _) = listener.accept().expect("accept provider request");
            let request = read_http_request(&mut stream);
            thread::sleep(Duration::from_millis(650));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
            listener
                .set_nonblocking(true)
                .expect("make provider listener nonblocking");
            let observation_deadline = Instant::now() + Duration::from_millis(200);
            let mut extra_requests = 0_usize;
            while Instant::now() < observation_deadline {
                match listener.accept() {
                    Ok((mut extra, _)) => {
                        extra_requests += 1;
                        let _ = read_http_request(&mut extra);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            (request, extra_requests)
        });
        let profile: Profile = toml::from_str(
            r"schema_version=3
[specs]
active_globs=['specs/*.md']
id_regex='^specs/(?P<id>[0-9]+)\\.md$'
[agent]
backend='builtin'
model='fixture-model'
timeout_seconds=1
",
        )
        .expect("parse one-second built-in profile");
        profile.validate().expect("validate built-in profile");
        let deadline = role_deadline(profile.agent.timeout_seconds)
            .expect("calculate role deadline")
            .expect("configured deadline");
        let started = Instant::now();
        let mut transcript = Vec::new();
        let result = crate::runtime::block_on(run_role_with_access_token(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "shared deadline fixture",
                role: PromptRole::Implementation,
                cancelled: &Arc::new(AtomicBool::new(false)),
                deadline: Some(deadline),
                endpoint: &format!("http://{address}/v1/responses"),
                token: "fixture-access-token",
            },
            &mut transcript,
        ))
        .expect("construct runtime");
        let elapsed = started.elapsed();
        let run = classify_agent_run(result, transcript.clone());
        let failure = run
            .outcome
            .expect_err("shared role deadline must stop the shell call");
        assert_eq!(failure.kind(), BackendFailureKind::Fatal);
        let error = failure.into_error();
        assert!(
            error
                .to_string()
                .contains("built-in shell command exceeded the role deadline")
        );
        assert!(elapsed < Duration::from_millis(1_500));
        assert_eq!(
            std::fs::read_to_string(marker).expect("read shell side effect"),
            "started"
        );
        let transcript = String::from_utf8(transcript).expect("shell transcript is UTF-8");
        assert!(transcript.contains("termination=timed_out"));
        assert!(transcript.contains("$ printf started > deadline-shell-started; sleep 30"));
        let (request, extra_requests) = server.join().expect("join provider fixture");
        assert!(request.starts_with("POST /v1/responses "));
        assert_eq!(extra_requests, 0);
    }

    #[cfg(unix)]
    #[test]
    fn shell_timeout_preserves_command_and_captured_output_in_transcript() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let temporary = TempDir::new().expect("create built-in project root");
        let command = "printf retained; sleep 30";
        let event = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "call_id": "call-timeout",
                    "name": "spec_loop.shell",
                    "arguments": serde_json::to_string(&json!({"command": command}))
                        .expect("serialize shell command")
                }]
            }
        });
        let body = completed_sse(&event);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            let request = read_http_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
            request
        });
        let profile: Profile = toml::from_str(
            r"schema_version=3
[specs]
active_globs=['specs/*.md']
id_regex='^specs/(?P<id>[0-9]+)\\.md$'
[agent]
backend='builtin'
model='fixture-model'
",
        )
        .expect("parse built-in profile");
        profile.validate().expect("validate built-in profile");
        let endpoint = format!("http://{address}/v1/responses");
        let mut transcript = Vec::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let result = crate::runtime::block_on(run_role_with_access_token(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "timeout fixture",
                role: PromptRole::Implementation,
                cancelled: &cancelled,
                deadline: Some(Instant::now() + Duration::from_millis(500)),
                endpoint: &endpoint,
                token: "fixture-access-token",
            },
            &mut transcript,
        ))
        .expect("construct runtime");
        assert!(matches!(
            result,
            Err(crate::error::SpecLoopError::Timeout(_))
        ));
        assert_ne!(server.join().expect("join provider fixture"), "");
        let log = String::from_utf8(transcript).expect("shell transcript is UTF-8");
        assert!(log.contains("$ printf retained; sleep 30"));
        assert!(log.contains("termination=timed_out"));
        assert!(log.contains("stdout:\nretained"));
    }

    #[cfg(unix)]
    #[test]
    fn shell_cancellation_stops_active_process() {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let temporary = TempDir::new().expect("create shell project root");
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_flag = Arc::clone(&cancelled);
        let cancel_task = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancellation_flag.store(true, Ordering::Relaxed);
        });
        let command = "printf interrupted-output; sleep 30";
        let result = run_shell(temporary.path(), command, 64, None, &cancelled)
            .expect("capture cancelled shell process output");
        cancel_task.join().expect("join cancellation task");
        assert_eq!(result.termination, super::ProcessTermination::Interrupted);
        let mut transcript = Vec::new();
        super::append_transcript(&mut transcript, command, &result.log_output)
            .expect("append interrupted shell transcript");
        let log = String::from_utf8(transcript).expect("shell transcript is UTF-8");
        assert!(log.contains("$ printf interrupted-output; sleep 30"));
        assert!(log.contains("termination=interrupted"));
        assert!(log.contains("stdout:\ninterrupted-output"));
    }

    #[test]
    fn rate_limit_retry_reuses_request_without_api_key_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body, retry_after) in [
                (
                    "429 Too Many Requests",
                    r#"{"error":{"code":"rate_limit_exceeded"}}"#,
                    Some("Retry-After: 0\r\n"),
                ),
                (
                    "200 OK",
                    &completed_sse(&json!({
                        "type": "response.completed",
                        "response": {"status": "completed", "output": []}
                    })),
                    None,
                ),
            ] {
                let (mut stream, _) = listener.accept().expect("accept provider request");
                requests.push(read_http_request(&mut stream));
                let retry_after = retry_after.unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\n{retry_after}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write provider response");
            }
            requests
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build provider client");
        let body = json!({"model": "fixture", "stream": true});
        let address = format!("http://{address}/v1/responses");
        let result = crate::runtime::block_on(request_with_retries(
            &client,
            &address,
            "oauth-only-token",
            &body,
            None,
            &AtomicBool::new(false),
        ))
        .expect("construct request runtime")
        .expect("retry should recover");
        assert_eq!(result["status"], "completed");
        let requests = server.join().expect("join fake provider");
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer oauth-only-token")
        }));
    }

    #[test]
    fn rate_limit_error_event_retries_stream_request() {
        assert_transient_sse_failure_retries(
            "error",
            &json!({"type": "error", "error": {"code": "rate_limit_exceeded"}}),
        );
    }

    #[test]
    fn rate_limit_response_failed_event_retries_stream_request() {
        assert_transient_sse_failure_retries(
            "response.failed",
            &json!({
                "type": "response.failed",
                "response": {"error": {"code": "rate_limit_exceeded"}}
            }),
        );
    }

    #[test]
    fn subscription_sharing_unavailable_error_event_retries_stream_request() {
        assert_transient_sse_failure_retries(
            "error",
            &json!({
                "type": "error",
                "error": {"code": "subscription_sharing_usage_unavailable"}
            }),
        );
    }

    #[test]
    fn subscription_sharing_unavailable_response_failed_retries_stream_request() {
        assert_transient_sse_failure_retries(
            "response.failed",
            &json!({
                "type": "response.failed",
                "response": {
                    "error": {"code": "subscription_sharing_usage_unavailable"}
                }
            }),
        );
    }

    #[test]
    fn streamed_usage_limit_errors_remain_fatal() {
        let usage_error = json!({"error": {"code": "insufficient_quota"}});
        let mut error_state = SseState::default();
        assert!(matches!(
            error_state.accept_event("error", &usage_error),
            Err(StreamFailure::Fatal(error)) if error.to_string().contains("usage limit")
        ));

        let failed_response = json!({
            "response": {"error": {"code": "insufficient_quota"}}
        });
        let mut response_state = SseState {
            lifecycle: ResponseLifecycle::InProgress,
            ..SseState::default()
        };
        assert!(matches!(
            response_state.accept_event("response.failed", &failed_response),
            Err(StreamFailure::Fatal(error)) if error.to_string().contains("usage limit")
        ));
    }

    #[test]
    fn transient_response_failed_server_error_retries_stream_request() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let server = thread::spawn(move || {
            let responses = [
                (
                    "Retry-After: 0\r\n",
                    format!(
                        "{RESPONSE_STARTED}{}",
                        sse_event(
                            "response.failed",
                            &json!({"type": "response.failed", "response": {"error": {"code": "server_error"}}})
                        )
                    ),
                ),
                (
                    "",
                    completed_sse(&json!({
                        "type": "response.completed",
                        "response": {"status": "completed", "output": []}
                    })),
                ),
            ];
            let mut requests = Vec::new();
            for (retry_after, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept provider request");
                requests.push(read_http_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n{retry_after}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write provider response");
            }
            requests
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build provider client");
        let body = json!({"model": "fixture", "stream": true});
        let result = crate::runtime::block_on(request_with_retries(
            &client,
            &format!("http://{address}/v1/responses"),
            "fixture-token",
            &body,
            None,
            &AtomicBool::new(false),
        ))
        .expect("construct request runtime")
        .expect("transient streamed failure should retry");
        assert_eq!(result["status"], "completed");
        assert_eq!(server.join().expect("join provider fixture").len(), 2);
    }

    #[test]
    fn permanent_response_failed_does_not_retry_stream_request() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            let request = read_http_request(&mut stream);
            let body = format!(
                "{RESPONSE_STARTED}{}",
                sse_event(
                    "response.failed",
                    &json!({"type": "response.failed", "response": {"error": {"code": "invalid_request_error"}}})
                )
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
            request
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build provider client");
        let body = json!({"model": "fixture", "stream": true});
        let result = crate::runtime::block_on(request_with_retries(
            &client,
            &format!("http://{address}/v1/responses"),
            "fixture-token",
            &body,
            None,
            &AtomicBool::new(false),
        ))
        .expect("construct request runtime");
        assert!(result.is_err());
        assert_ne!(server.join().expect("join provider fixture"), "");
    }

    #[test]
    fn provider_retry_delays_use_exponential_backoff_and_clamp_guidance() {
        for (retry, expected) in [(0, 1), (1, 2), (2, 4), (3, 8), (4, 16), (5, 30)] {
            assert_eq!(
                retry_delay(retry, None),
                std::time::Duration::from_secs(expected)
            );
        }
        assert_eq!(
            retry_delay(0, Some("120")),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            retry_delay(0, Some("not-a-date")),
            std::time::Duration::from_secs(1)
        );
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(90);
        let date = httpdate::fmt_http_date(future);
        assert_eq!(
            retry_delay(0, Some(&date)),
            std::time::Duration::from_secs(30)
        );
    }

    #[test]
    fn usage_limits_are_distinguished_from_retryable_provider_errors() {
        assert!(usage_limit_error(
            br#"{"error":{"code":"insufficient_quota","message":"private body"}}"#
        ));
        assert!(!usage_limit_error(
            br#"{"error":{"code":"subscription_sharing_usage_unavailable"}}"#
        ));
        assert!(usage_limit_error(
            br#"{"error":{"code":"usage_unavailable"}}"#
        ));
        assert!(!usage_limit_error(
            br#"{"error":{"code":"rate_limit_exceeded"}}"#
        ));
    }

    fn run_fake_http_responses(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (Vec<String>, crate::error::Result<Value>) {
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
        listener
            .set_nonblocking(true)
            .expect("make fake provider listener nonblocking");
        let address = listener.local_addr().expect("fake provider address");
        let server = std::thread::spawn(move || {
            let started = Instant::now();
            let mut last_request = started;
            let mut requests = Vec::new();
            while started.elapsed() < Duration::from_secs(3) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        last_request = Instant::now();
                        let request = read_http_request(&mut stream);
                        let response_index = requests.len();
                        requests.push(request);
                        let (status, body) = responses.get(response_index).copied().unwrap_or((
                            "400 Bad Request",
                            r#"{"error":{"code":"unexpected_request"}}"#,
                        ));
                        let retry_after =
                            if status.starts_with("429 ") || status.starts_with("503 ") {
                                "Retry-After: 0\r\n"
                            } else {
                                ""
                            };
                        let reply = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{retry_after}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream
                            .write_all(reply.as_bytes())
                            .expect("write fake provider response");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if !requests.is_empty()
                            && last_request.elapsed() > Duration::from_millis(200)
                        {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept fake provider request: {error}"),
                }
            }
            requests
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build fake provider client");
        let request = crate::runtime::block_on(request_with_retries(
            &client,
            &format!("http://{address}/v1/responses"),
            "oauth-only-fixture-token",
            &json!({"model": "fixture-model", "stream": true}),
            None,
            &AtomicBool::new(false),
        ))
        .expect("construct provider test runtime");
        (server.join().expect("join fake provider"), request)
    }

    fn assert_oauth_only_requests(requests: &[String]) {
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer oauth-only-fixture-token")
                && !request.to_ascii_lowercase().contains("x-api-key")
        }));
    }

    #[test]
    fn http_authentication_and_permission_failures_are_fatal_without_fallback() {
        for (status, body) in [
            ("401 Unauthorized", r#"{"error":{"code":"invalid_token"}}"#),
            ("403 Forbidden", r#"{"error":{"code":"permission_denied"}}"#),
        ] {
            let (requests, response) = run_fake_http_responses(vec![(status, body)]);
            let error = response.expect_err("authentication failure must stop");
            let run = classify_agent_run(Err(error), Vec::new());
            assert_eq!(
                run.outcome
                    .expect_err("backend failure classification")
                    .kind(),
                BackendFailureKind::Fatal
            );
            assert_eq!(requests.len(), 1);
            assert_oauth_only_requests(&requests);
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_route_admission_denials_are_fatal_before_model_dispatch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for status in ["401 Unauthorized", "403 Forbidden"] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
            let address = listener.local_addr().expect("provider fixture address");
            let model_dispatches = Arc::new(AtomicUsize::new(0));
            let observed_dispatches = Arc::clone(&model_dispatches);
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept admission request");
                let request = read_http_request(&mut stream);
                let body = r#"{"error":{"code":"direct_permission_denied"}}"#;
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write admission denial");
                if status.starts_with("200 ") {
                    observed_dispatches.fetch_add(1, Ordering::Relaxed);
                }
                request
            });
            let temporary = TempDir::new().expect("create project root");
            let profile = builtin_test_profile();
            let cancelled = Arc::new(AtomicBool::new(false));
            let mut completed_conversation = None;
            let mut transcript = Vec::new();
            let endpoint = format!("http://{address}/v1/responses");
            let result = crate::runtime::block_on(run_role_with_auth(
                async { Ok("fixture-access-token".to_owned()) },
                RoleTask {
                    root: temporary.path(),
                    profile: &profile,
                    prompt: "direct permission test",
                    role: PromptRole::Implementation,
                    cancelled: &cancelled,
                    deadline: None,
                    endpoint: &endpoint,
                },
                None,
                &mut completed_conversation,
                &mut transcript,
            ))
            .expect("construct test runtime");
            let failure = classify_agent_run(result, transcript)
                .outcome
                .expect_err("admission denial must stop built-in role");
            assert_eq!(failure.kind(), BackendFailureKind::Fatal);
            let request = server.join().expect("join admission server");
            assert!(request.starts_with("POST /v1/responses "));
            assert_eq!(model_dispatches.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn permanent_model_rejection_is_fatal_without_retry_or_fallback() {
        let (requests, response) = run_fake_http_responses(vec![(
            "400 Bad Request",
            r#"{"error":{"code":"unsupported_model","message":"private provider detail"}}"#,
        )]);
        let error = response.expect_err("model rejection must stop");
        assert!(error.to_string().contains("HTTP 400"));
        assert!(!error.to_string().contains("private provider detail"));
        let run = classify_agent_run(Err(error), Vec::new());
        assert_eq!(
            run.outcome
                .expect_err("backend failure classification")
                .kind(),
            BackendFailureKind::Fatal
        );
        assert_eq!(requests.len(), 1);
        assert_oauth_only_requests(&requests);
    }

    #[test]
    fn temporary_subscription_usage_unavailability_retries_and_recovers() {
        let (requests, response) = run_fake_http_responses(vec![
            (
                "503 Service Unavailable",
                r#"{"error":{"code":"subscription_sharing_usage_unavailable"}}"#,
            ),
            (
                "200 OK",
                concat!(
                    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\"}}\n\n",
                    "event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\"}}\n\n",
                    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n"
                ),
            ),
        ]);
        let completed = response.expect("temporary availability failure should recover");
        assert_eq!(completed["status"], "completed");
        assert_eq!(requests.len(), 2);
        assert_oauth_only_requests(&requests);
    }

    #[test]
    fn usage_limit_http_response_is_fatal_without_retry_or_fallback() {
        let (requests, response) = run_fake_http_responses(vec![(
            "429 Too Many Requests",
            r#"{"error":{"code":"insufficient_quota"}}"#,
        )]);
        let error = response.expect_err("usage limit must stop");
        assert!(error.to_string().contains("usage limit"));
        let run = classify_agent_run(Err(error), Vec::new());
        assert_eq!(
            run.outcome
                .expect_err("backend failure classification")
                .kind(),
            BackendFailureKind::Fatal
        );
        assert_eq!(requests.len(), 1);
        assert_oauth_only_requests(&requests);
    }

    #[test]
    fn transient_http_failures_stop_after_three_retries() {
        let (requests, response) = run_fake_http_responses(vec![
            (
                "503 Service Unavailable",
                r#"{"error":{"code":"server_error"}}"#,
            ),
            (
                "503 Service Unavailable",
                r#"{"error":{"code":"server_error"}}"#,
            ),
            (
                "503 Service Unavailable",
                r#"{"error":{"code":"server_error"}}"#,
            ),
            (
                "503 Service Unavailable",
                r#"{"error":{"code":"server_error"}}"#,
            ),
        ]);
        let error = response.expect_err("exhausted provider retries must fail");
        assert!(error.to_string().contains("after 3 retries"));
        let run = classify_agent_run(Err(error), Vec::new());
        assert_eq!(
            run.outcome
                .expect_err("backend failure classification")
                .kind(),
            BackendFailureKind::Fatal
        );
        assert_eq!(requests.len(), 4);
        assert_oauth_only_requests(&requests);
    }
}
