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
    run_role_with_auth, run_role_with_continuation, stage_for_role,
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
                    "text": "final after fresh token"
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

    assert_eq!(result.response, "final after fresh token");
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
