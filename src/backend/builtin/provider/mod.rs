use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{Result, SpecLoopError};

#[cfg(test)]
mod tests;

const MAX_PROVIDER_RETRIES: u8 = 3;
const MAX_RETRY_DELAY_SECONDS: u64 = 30;
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub(super) async fn request_with_retries(
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

pub(super) enum StreamFailure {
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
pub(super) struct SseState {
    event_name: Option<String>,
    data: String,
    pub(super) lifecycle: ResponseLifecycle,
    output_items: Vec<Value>,
    completed: Option<Value>,
}

#[derive(Default)]
pub(super) enum ResponseLifecycle {
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

    pub(super) fn accept_event(
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

pub(super) fn usage_limit_error(body: &[u8]) -> bool {
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

pub(super) fn retry_delay(retry_count: u8, retry_after: Option<&str>) -> Duration {
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

pub(super) async fn await_limited<T, E>(
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

pub(super) fn ensure_active(deadline: Option<Instant>, cancelled: &AtomicBool) -> Result<()> {
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
