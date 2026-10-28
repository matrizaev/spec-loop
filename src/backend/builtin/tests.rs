use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::provider::request_with_retries;
use super::{
    RoleInput, RoleTask, classify_agent_run, run_after_shell_preflight, run_role_with_access_token,
    run_role_with_auth,
};
use crate::backend::BackendFailureKind;
use crate::config::{AgentStage, Profile};
use crate::prompts::PromptRole;

pub(super) const RESPONSE_STARTED: &str = concat!(
    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\"}}\n\n",
    "event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\"}}\n\n"
);

pub(super) fn sse_event(event_type: &str, event: &Value) -> String {
    format!("event: {event_type}\ndata: {event}\n\n")
}

pub(super) fn completed_sse(event: &Value) -> String {
    format!(
        "{RESPONSE_STARTED}{}",
        sse_event("response.completed", event)
    )
}

pub(super) fn fake_stream_request(body: String) -> (crate::error::Result<Value>, String) {
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

pub(super) fn assert_transient_sse_failure_retries(event_type: &str, event: &Value) {
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

pub(super) fn builtin_test_profile() -> Profile {
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

async fn run_role_with_usage(
    input: RoleInput<'_>,
    continuation: Option<super::BuiltinConversation>,
    completed_conversation: &mut Option<super::BuiltinConversation>,
    transcript: &mut Vec<u8>,
    usage: &mut crate::backend::UsageSnapshot,
) -> crate::error::Result<super::AgentResponse> {
    let token = input.token.to_owned();
    let mut observer = |_: &crate::backend::UsageSnapshot| Ok(());
    super::run_role_with_token_provider_observed(
        input,
        continuation,
        move || {
            let token = token.clone();
            async move { Ok(token) }
        },
        completed_conversation,
        transcript,
        usage,
        &mut observer,
    )
    .await
}

fn response_with_usage(output: &Value, input: u64, output_tokens: u64) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": output,
            "usage": {
                "input_tokens": input,
                "output_tokens": output_tokens,
                "total_tokens": input + output_tokens
            }
        }
    })
}

fn start_one_response_provider(response: &Value) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let body = completed_sse(response);
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
        vec![request]
    });
    (format!("http://{address}/v1/responses"), server)
}

fn provider_request_payload(request: &str) -> Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("provider request body");
    serde_json::from_str(body).expect("parse provider request body")
}

fn assert_strict_role_format(request: &str, role: PromptRole) {
    let payload = provider_request_payload(request);
    let format = &payload["text"]["format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(format["name"], role.protocol_name());
    assert_eq!(format["strict"], true);
    assert_eq!(format["schema"]["type"], "object");
    assert_eq!(format["schema"]["required"], json!(["response"]));
    assert!(format["schema"]["properties"]["response"]["anyOf"].is_array());
}

fn assert_blocked_usage(response: &super::AgentResponse, usage: &crate::backend::UsageSnapshot) {
    let parsed = crate::protocol::parse::<crate::protocol::ImplementationResponse>(
        &response.response,
        PromptRole::Implementation.protocol_name(),
    )
    .expect("parse blocked implementation response");
    assert!(matches!(
        parsed.result,
        crate::protocol::Readiness::Blocked { .. }
    ));
    assert_eq!(usage.provider_request_attempts, Some(3));
    assert_eq!(usage.completed_responses, Some(3));
    assert_eq!(usage.executed_shell_calls, Some(2));
    assert_eq!(
        usage.latest_response.as_ref().and_then(|item| item.input),
        Some(30)
    );
    let report = usage.format_report("implementation", "fixture-model", 0, None);
    assert!(report.contains("∑ Observed completed-response usage: 60 in · 8 out · 68 total"));
}

#[cfg(unix)]
#[test]
fn multiple_tool_turns_and_blocked_handoff_preserve_role_usage_without_replay() {
    let temporary = TempDir::new().expect("create built-in project root");
    let marker = temporary.path().join("tool-turns");
    let commands = [
        format!("printf x >> '{}'", marker.display()),
        format!("printf y >> '{}'", marker.display()),
    ];
    let tool_response = |call_id: &str, command: &str| {
        response_with_usage(
            &json!([{
                "type": "function_call",
                "call_id": call_id,
                "name": "spec_loop.shell",
                "arguments": serde_json::to_string(&json!({"command": command}))
                    .expect("serialize shell command")
            }]),
            if call_id == "first" { 10 } else { 20 },
            2,
        )
    };
    let blocked_text = r#"{"protocol_version":1,"role":"implementation","status":"blocked","summary":"No changes made; required input unavailable.","reason":"required input unavailable"}"#;
    let responses = [
        completed_sse(&tool_response("first", &commands[0])),
        completed_sse(&tool_response("second", &commands[1])),
        completed_sse(&response_with_usage(
            &json!([{
                "type": "message",
                "content": [{"type": "output_text", "text": format!(r#"{{"response":{blocked_text}}}"#)}]
            }]),
            30,
            4,
        )),
    ];
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for body in responses {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            requests.push(read_http_request(&mut stream));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
        }
        requests
    });
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut transcript = Vec::new();
    let mut conversation = None;
    let mut usage = crate::backend::UsageSnapshot::default();
    let result = crate::runtime::block_on(run_role_with_usage(
        RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "perform two deterministic tool turns",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &format!("http://{address}/v1/responses"),
            token: "fixture-access-token",
        },
        None,
        &mut conversation,
        &mut transcript,
        &mut usage,
    ))
    .expect("construct provider runtime")
    .expect("receive blocked typed response");
    assert_blocked_usage(&result, &usage);
    assert_eq!(
        std::fs::read_to_string(marker).expect("read tool marker"),
        "xy"
    );
    let requests = server.join().expect("join provider fixture");
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_strict_role_format(request, PromptRole::Implementation);
    }
    assert_eq!(
        String::from_utf8_lossy(&transcript)
            .matches("$ printf")
            .count(),
        2
    );
}

#[test]
fn failed_provider_response_preserves_attempt_count_without_completion_or_shell_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let body = format!(
        "{RESPONSE_STARTED}{}",
        sse_event(
            "response.failed",
            &json!({
                "type": "response.failed",
                "response": {"status": "failed", "error": {"code": "invalid_response"}}
            })
        )
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept provider request");
        let request = read_http_request(&mut stream);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write provider failure");
        request
    });
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut transcript = Vec::new();
    let mut conversation = None;
    let mut usage = crate::backend::UsageSnapshot::default();
    let result = crate::runtime::block_on(run_role_with_usage(
        RoleInput {
            root: Path::new("."),
            profile: &profile,
            prompt: "failure fixture",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &format!("http://{address}/v1/responses"),
            token: "fixture-access-token",
        },
        None,
        &mut conversation,
        &mut transcript,
        &mut usage,
    ))
    .expect("construct provider runtime");
    assert!(result.is_err());
    assert_eq!(usage.provider_request_attempts, Some(1));
    assert_eq!(usage.completed_responses, Some(0));
    assert_eq!(usage.executed_shell_calls, Some(0));
    assert!(
        server
            .join()
            .expect("join provider fixture")
            .starts_with("POST ")
    );
}

#[cfg(unix)]
#[test]
fn shell_timeout_and_interruption_preserve_usage_and_never_replay_tool_effects() {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    for interrupted in [false, true] {
        let temporary = TempDir::new().expect("create shell project root");
        let marker = temporary.path().join("started");
        let command = format!("printf x > '{}'; sleep 30", marker.display());
        let response = response_with_usage(
            &json!([{
                "type": "function_call",
                "call_id": "once",
                "name": "spec_loop.shell",
                "arguments": serde_json::to_string(&json!({"command": command}))
                    .expect("serialize shell command")
            }]),
            8,
            2,
        );
        let (endpoint, server) = start_one_response_provider(&response);
        let profile = builtin_test_profile();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_task = if interrupted {
            let cancellation = Arc::clone(&cancelled);
            let marker = marker.clone();
            Some(thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline && !marker.exists() {
                    thread::sleep(Duration::from_millis(5));
                }
                let marker_started = marker.exists();
                cancellation.store(true, Ordering::Relaxed);
                marker_started
            }))
        } else {
            None
        };
        let mut transcript = Vec::new();
        let mut conversation = None;
        let mut usage = crate::backend::UsageSnapshot::default();
        let deadline = (!interrupted).then(|| Instant::now() + Duration::from_millis(300));
        let result = crate::runtime::block_on(run_role_with_usage(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "interrupted shell fixture",
                role: PromptRole::Implementation,
                cancelled: &cancelled,
                deadline,
                endpoint: &endpoint,
                token: "fixture-access-token",
            },
            None,
            &mut conversation,
            &mut transcript,
            &mut usage,
        ))
        .expect("construct provider runtime");
        let error = result.expect_err("shell timeout/interruption must stop the role");
        if interrupted {
            assert!(matches!(error, crate::error::SpecLoopError::Interrupted));
            assert!(
                cancel_task
                    .expect("interruption task")
                    .join()
                    .expect("join interruption task")
            );
        } else {
            assert!(matches!(error, crate::error::SpecLoopError::Timeout(_)));
        }
        assert_eq!(
            std::fs::read_to_string(marker).expect("read started side effect"),
            "x"
        );
        assert_eq!(usage.provider_request_attempts, Some(1));
        assert_eq!(usage.completed_responses, Some(1));
        assert_eq!(usage.executed_shell_calls, Some(1));
        assert_eq!(
            usage.latest_response.as_ref().and_then(|item| item.total),
            Some(10)
        );
        assert_eq!(server.join().expect("join provider fixture").len(), 1);
    }
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
        || crate::auth::active_access_token_at(&credential_path, false, cancelled.as_ref()),
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
    let roles = [
        (PromptRole::PlanAuthor, AgentStage::Planning),
        (PromptRole::PlanReview, AgentStage::Planning),
        (PromptRole::PlanAudit, AgentStage::Planning),
        (PromptRole::PlanClarification, AgentStage::Planning),
        (PromptRole::Implementation, AgentStage::Implementation),
        (PromptRole::Review, AgentStage::Review),
        (PromptRole::ReviewAudit, AgentStage::Review),
        (PromptRole::ReviewClarification, AgentStage::Review),
    ];
    for (role, stage) in roles {
        assert_eq!(role.agent_stage(), stage);
    }
}

#[test]
fn builtin_backend_accepts_typed_responses_for_every_role_from_fake_provider() {
    use std::sync::Arc;

    use crate::protocol::{
        ImplementationAuditResponse, ImplementationClarificationResponse, ImplementationResponse,
        ImplementationReviewResponse, PlanAuditResponse, PlanAuthorResponse,
        PlanClarificationResponse, PlanReviewResponse,
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
                        "content": [{"type": "output_text", "text": json!({"response": response}).to_string()}]
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
        expected_prompts.push((prompt, role));

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
                crate::protocol::parse::<PlanAuditResponse>(&result.response, role.protocol_name())
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
    for (request, (expected_prompt, role)) in requests.iter().zip(expected_prompts) {
        let (_, payload) = request
            .split_once("\r\n\r\n")
            .expect("provider request body");
        let payload: Value = serde_json::from_str(payload).expect("parse request body");
        assert_eq!(payload["input"][0]["content"], expected_prompt);
        assert_eq!(payload["text"]["format"]["type"], "json_schema");
        assert_eq!(payload["text"]["format"]["name"], role.protocol_name());
        assert_eq!(payload["text"]["format"]["strict"], true);
        assert_eq!(payload["text"]["format"]["schema"]["type"], "object");
        assert_eq!(
            payload["text"]["format"]["schema"]["required"],
            json!(["response"])
        );
        assert_eq!(
            payload["text"]["format"]["schema"]["additionalProperties"],
            false
        );
        assert!(payload["text"]["format"]["schema"]["properties"]["response"]["anyOf"].is_array());
        assert_eq!(payload["tools"][0]["type"], "namespace");
        assert_eq!(payload["tools"][0]["name"], "spec_loop");
        assert_eq!(payload["tools"][0]["tools"][0]["name"], "shell");
    }
}

#[cfg(unix)]
#[test]
fn refusal_with_shell_call_never_executes_the_call() {
    let temporary = TempDir::new().expect("create refusal fixture project");
    let marker = temporary.path().join("must-not-exist");
    let response = json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "refused-tool-call",
                    "name": "spec_loop.shell",
                    "arguments": serde_json::to_string(&json!({
                        "command": format!("touch {}", marker.display())
                    }))
                    .expect("serialize refusal shell command")
                },
                {
                    "type": "message",
                    "content": [{"type": "refusal", "refusal": "private refusal text sentinel"}]
                }
            ]
        }
    });
    let (endpoint, server) = start_one_response_provider(&response);
    let profile = builtin_test_profile();
    let mut transcript = Vec::new();
    let result = crate::runtime::block_on(run_role_with_access_token(
        RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "perform a task",
            role: PromptRole::Implementation,
            cancelled: &Arc::new(AtomicBool::new(false)),
            deadline: None,
            endpoint: &endpoint,
            token: "fixture-access-token",
        },
        &mut transcript,
    ))
    .expect("construct refusal test runtime");
    let error = result.expect_err("refusal must invalidate the provider response");
    assert!(error.to_string().contains("refused the role request"));
    assert!(!error.to_string().contains("private refusal text sentinel"));
    assert!(!marker.exists());
    assert_eq!(server.join().expect("join fake provider").len(), 1);
}

#[cfg(unix)]
#[test]
fn configured_shell_call_limit_stops_after_permitted_calls_without_replay() {
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let temporary = TempDir::new().expect("create built-in project root");
    let marker = temporary.path().join("shell-invocations");
    let first_command = "printf x >> shell-invocations; printf prior-effect > prior-effect";
    let second_command = "printf y >> shell-invocations; printf over-limit > over-limit-effect";
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
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("prior-effect"))
            .expect("read permitted command effect"),
        "prior-effect"
    );
    assert!(!temporary.path().join("over-limit-effect").exists());
    let transcript = String::from_utf8_lossy(&run.stdout);
    assert!(transcript.contains(first_command));
    assert!(!transcript.contains(second_command));
    assert!(transcript.contains("exit_status=0"));

    let request = server.join().expect("join provider fixture");
    assert!(request.starts_with("POST /v1/responses "));
}

#[cfg(unix)]
#[test]
fn omitted_call_limit_allows_more_than_fifty_shell_calls() {
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let temporary = TempDir::new().expect("create built-in project root");
    let calls = (0..51)
        .map(|index| {
            json!({
                "type": "function_call",
                "call_id": format!("call-{index}"),
                "name": "spec_loop.shell",
                "arguments": serde_json::to_string(&json!({
                    "command": "printf x >> shell-call-count; printf 1234; printf 5678 >&2"
                }))
                .expect("serialize shell command")
            })
        })
        .collect::<Vec<_>>();
    let tool_response = json!({
        "type": "response.completed",
        "response": {"status": "completed", "output": calls}
    });
    let final_text = r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Completed 51 shell calls"}"#;
    let final_response = json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "model": "fixture-model",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": format!(r#"{{"response":{final_text}}}"#)}]
            }]
        }
    });
    let bodies = [
        completed_sse(&tool_response),
        completed_sse(&final_response),
    ];
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            requests.push(read_http_request(&mut stream));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
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
max_tool_output_bytes=6
",
    )
    .expect("parse built-in profile without a call limit");
    profile.validate().expect("validate built-in profile");
    assert_eq!(profile.agent.max_tool_calls, None);
    let mut transcript = Vec::new();
    let response = crate::runtime::block_on(run_role_with_access_token(
        RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "run more than fifty calls",
            role: PromptRole::Implementation,
            cancelled: &Arc::new(AtomicBool::new(false)),
            deadline: None,
            endpoint: &format!("http://{address}/v1/responses"),
            token: "fixture-access-token",
        },
        &mut transcript,
    ))
    .expect("construct runtime")
    .expect("run role after 51 successful commands");
    crate::protocol::parse::<crate::protocol::ImplementationResponse>(
        &response.response,
        PromptRole::Implementation.protocol_name(),
    )
    .expect("validate final implementation response");
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("shell-call-count"))
            .expect("read all shell effects"),
        "x".repeat(51)
    );
    assert_eq!(
        String::from_utf8_lossy(&transcript)
            .matches("$ printf")
            .count(),
        51
    );

    let requests = server.join().expect("join provider fixture");
    assert_eq!(requests.len(), 2);
    let second_request = requests
        .get(1)
        .expect("second provider request after tool calls");
    let (_, payload) = second_request
        .split_once("\r\n\r\n")
        .expect("second provider request body");
    let payload: Value = serde_json::from_str(payload).expect("parse second request");
    let outputs = payload
        .get("input")
        .and_then(Value::as_array)
        .expect("provider input items")
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 51);
    for output in outputs {
        let text = output
            .get("output")
            .and_then(Value::as_str)
            .expect("shell output string");
        let (_, streams) = text.split_once("stdout:\n").expect("stdout payload");
        let (stdout, stderr) = streams.split_once("\nstderr:\n").expect("stderr payload");
        let (stderr, _) = stderr
            .split_once("\n[tool output truncated at configured byte limit]")
            .expect("truncation marker");
        assert!(stdout.len() + stderr.len() <= 6, "{text:?}");
        assert!(text.contains("exit_status=0"));
        assert!(!text.contains("command exited unsuccessfully"));
    }
    let transcript = String::from_utf8_lossy(&transcript);
    assert_eq!(
        transcript
            .matches("[tool output truncated at configured byte limit]")
            .count(),
        51
    );
    assert_eq!(transcript.matches("exit_status=0").count(), 51);
    assert!(!transcript.contains("command exited unsuccessfully"));
}

#[cfg(unix)]
#[test]
fn configured_shell_call_limit_resets_for_each_role() {
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let temporary = TempDir::new().expect("create built-in project root");
    let role_responses = [
        (
            PromptRole::Implementation,
            "i",
            r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Implementation complete"}"#,
        ),
        (
            PromptRole::Review,
            "r",
            r#"{"protocol_version":1,"role":"review","status":"no_issues","findings":[]}"#,
        ),
    ];
    let mut bodies = Vec::new();
    for (index, (_, marker, final_text)) in role_responses.iter().enumerate() {
        let tool_response = json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": [{
                "type": "function_call",
                "call_id": format!("call-role-{index}"),
                "name": "spec_loop.shell",
                "arguments": serde_json::to_string(&json!({
                    "command": format!("printf {marker} >> role-call-count")
                }))
                .expect("serialize role shell command")
            }]}
        });
        let final_response = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "model": "fixture-model",
                "output": [{"type": "message", "content": [{
                    "type": "output_text",
                    "text": format!(r#"{{"response":{final_text}}}"#)
                }]}]
            }
        });
        bodies.push(completed_sse(&tool_response));
        bodies.push(completed_sse(&final_response));
    }
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            requests.push(read_http_request(&mut stream));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
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
max_tool_calls=1
",
    )
    .expect("parse limited built-in profile");
    profile.validate().expect("validate limited profile");

    for (role, _, _) in role_responses {
        let mut transcript = Vec::new();
        let response = crate::runtime::block_on(run_role_with_access_token(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "run one command for this role",
                role,
                cancelled: &Arc::new(AtomicBool::new(false)),
                deadline: None,
                endpoint: &format!("http://{address}/v1/responses"),
                token: "fixture-access-token",
            },
            &mut transcript,
        ))
        .expect("construct runtime")
        .expect("one limited shell call should succeed per role");
        match role {
            PromptRole::Implementation => {
                crate::protocol::parse::<crate::protocol::ImplementationResponse>(
                    &response.response,
                    role.protocol_name(),
                )
                .expect("validate implementation response");
            }
            PromptRole::Review => {
                crate::protocol::parse::<crate::protocol::ImplementationReviewResponse>(
                    &response.response,
                    role.protocol_name(),
                )
                .expect("validate review response");
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("role-call-count"))
            .expect("read role effects"),
        "ir"
    );
    assert_eq!(server.join().expect("join provider fixture").len(), 4);
}

fn token_refresh_responses() -> [String; 2] {
    let tool_response = json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call-once",
                "name": "spec_loop.shell",
                "arguments": serde_json::to_string(&json!({
                    "command": "printf x >> shell-invocations"
                }))
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
                    "text": r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Refreshed request completed."}}"#
                }]
            }]
        }
    });
    [
        completed_sse(&tool_response),
        completed_sse(&final_response),
    ]
}

#[cfg(unix)]
#[test]
fn resolves_access_token_again_after_shell_tool_before_followup_request() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let temporary = TempDir::new().expect("create built-in project root");
    let bodies = token_refresh_responses();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            requests.push(read_http_request(&mut stream));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
        }
        requests
    });
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let mut completed_conversation = None;
    let mut transcript = Vec::new();
    let endpoint = format!("http://{address}/v1/responses");
    let result = crate::runtime::block_on(run_role_with_auth(
        move || {
            let call = observed_calls.fetch_add(1, Ordering::Relaxed);
            async move { Ok(format!("fixture-access-token-{call}")) }
        },
        RoleTask {
            root: temporary.path(),
            profile: &profile,
            prompt: "request a command, then finish",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &endpoint,
        },
        None,
        &mut completed_conversation,
        &mut transcript,
    ))
    .expect("construct runtime")
    .expect("run role with refreshed request credentials");

    assert_eq!(
        result.response,
        r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Refreshed request completed."}"#
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("shell-invocations"))
            .expect("read completed shell effect"),
        "x"
    );
    let requests = server.join().expect("join provider fixture");
    assert_eq!(requests.len(), 2);
    let authorization = requests
        .iter()
        .map(|request| {
            request
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("authorization")
                        .then_some(value.trim())
                })
                .expect("authorization header")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        authorization,
        [
            "Bearer fixture-access-token-0",
            "Bearer fixture-access-token-1"
        ]
    );
}

#[cfg(unix)]
#[test]
fn token_refresh_failure_preserves_completed_shell_effect_without_replay() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let temporary = TempDir::new().expect("create built-in project root");
    let first_body = token_refresh_responses()
        .into_iter()
        .next()
        .expect("tool response body");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept provider request");
        let request = read_http_request(&mut stream);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{first_body}",
            first_body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write tool response");
        request
    });
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let mut completed_conversation = None;
    let mut transcript = Vec::new();
    let endpoint = format!("http://{address}/v1/responses");
    let result = crate::runtime::block_on(run_role_with_auth(
        move || {
            let call = observed_calls.fetch_add(1, Ordering::Relaxed);
            async move {
                if call == 0 {
                    Ok("fixture-access-token".to_owned())
                } else {
                    Err(crate::error::SpecLoopError::Workflow(
                        "fixture refresh failure".to_owned(),
                    ))
                }
            }
        },
        RoleTask {
            root: temporary.path(),
            profile: &profile,
            prompt: "request a command, then finish",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &endpoint,
        },
        None,
        &mut completed_conversation,
        &mut transcript,
    ))
    .expect("construct runtime");

    let error = result.expect_err("follow-up credential refresh must stop role");
    assert!(error.to_string().contains("fixture refresh failure"));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("shell-invocations"))
            .expect("read completed shell effect"),
        "x"
    );
    assert!(
        server
            .join()
            .expect("join provider fixture")
            .starts_with("POST /v1/responses ")
    );
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
                    "text": r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Retried follow-up without replay."}}"#
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
    for request in &requests {
        assert_strict_role_format(request, PromptRole::Implementation);
    }
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
    let tool_response = response_with_usage(
        &json!([{
            "type": "function_call",
            "call_id": "typed-call-once",
            "name": "spec_loop.shell",
            "arguments": serde_json::to_string(&json!({"command": command}))
                .expect("serialize shell command")
        }]),
        10,
        2,
    );
    let malformed_response = response_with_usage(
        &json!([{
            "type": "message",
            "content": [{"type": "output_text", "text": r#"{"response":{"unexpected":"typed schema failure"}}"#}]
        }]),
        11,
        3,
    );
    let corrected_response = response_with_usage(
        &json!([{
            "type": "message",
            "content": [{
                "type": "output_text",
                "text": r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Corrected handoff only."}}"#
            }]
        }]),
        12,
        4,
    );
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

fn assert_correction_usage(
    initial: &crate::backend::UsageSnapshot,
    correction: &crate::backend::UsageSnapshot,
) {
    assert_eq!(initial.provider_request_attempts, Some(2));
    assert_eq!(initial.completed_responses, Some(2));
    assert_eq!(initial.executed_shell_calls, Some(1));
    assert_eq!(correction.provider_request_attempts, Some(1));
    assert_eq!(correction.completed_responses, Some(1));
    assert_eq!(correction.executed_shell_calls, Some(0));
    let cumulative = initial.merged(correction);
    assert_eq!(cumulative.provider_request_attempts, Some(3));
    assert_eq!(cumulative.completed_responses, Some(3));
    assert_eq!(cumulative.executed_shell_calls, Some(1));
    assert_eq!(
        cumulative
            .latest_response
            .as_ref()
            .and_then(|usage| usage.total),
        Some(16)
    );
    let report = cumulative.format_report("implementation", "fixture-model", 0, None);
    assert!(report.contains("∑ Observed completed-response usage: 33 in · 9 out · 42 total"));
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
    let mut initial_usage = crate::backend::UsageSnapshot::default();
    let malformed = crate::runtime::block_on(run_role_with_usage(
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
        &mut initial_usage,
    ))
    .expect("construct initial runtime")
    .expect("receive malformed final text");
    assert_eq!(
        malformed.response,
        r#"{"unexpected":"typed schema failure"}"#
    );
    let conversation = conversation.expect("retain completed conversation for correction");

    let mut corrected_conversation = None;
    let mut correction_usage = crate::backend::UsageSnapshot::default();
    let corrected = crate::runtime::block_on(run_role_with_usage(
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
        &mut correction_usage,
    ))
    .expect("construct correction runtime")
    .expect("receive corrected final text");
    assert!(corrected.response.contains("Corrected handoff only"));
    assert_eq!(
        std::fs::read_to_string(marker).expect("read shell side effect"),
        "x"
    );
    assert_correction_usage(&initial_usage, &correction_usage);

    let requests = server.join().expect("join fake provider");
    assert_eq!(requests.len(), 3);
    let bodies = requests
        .iter()
        .map(|request| provider_request_payload(request))
        .collect::<Vec<_>>();
    for request in &requests {
        assert_strict_role_format(request, PromptRole::Implementation);
    }
    assert_eq!(
        bodies[0]["input"][0]["content"],
        "original implementation task"
    );
    assert_eq!(bodies[2]["input"][2]["call_id"], "typed-call-once");
    assert!(
        bodies[2]["input"][3]
            .to_string()
            .contains("typed schema failure")
    );
    assert!(
        bodies[2]["input"][2]["output"]
            .as_str()
            .is_some_and(|text| text.contains("exit_status=0"))
    );
    assert_eq!(bodies[2]["tool_choice"], "none");
    assert_eq!(bodies[2]["text"]["format"]["strict"], true);
    assert_eq!(bodies[2]["text"]["format"]["name"], "implementation");
    assert_eq!(
        bodies[2]["input"][4]["content"],
        "Correct malformed typed response only; do not invoke tools."
    );
}

pub(super) fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 2048];
    loop {
        let read = stream.read(&mut buffer).expect("read provider request");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
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

pub(super) fn run_fake_http_responses(
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
                    let retry_after = if status.starts_with("429 ") || status.starts_with("503 ") {
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
                    if !requests.is_empty() && last_request.elapsed() > Duration::from_millis(200) {
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

pub(super) fn assert_oauth_only_requests(requests: &[String]) {
    assert!(requests.iter().all(|request| {
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer oauth-only-fixture-token")
            && !request.to_ascii_lowercase().contains("x-api-key")
    }));
}
