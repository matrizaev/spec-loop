use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::super::shell::parse_output_items;
use super::super::shell::shell_tool_definition;
use super::super::tests::{
    RESPONSE_STARTED, assert_oauth_only_requests, assert_transient_sse_failure_retries,
    builtin_test_profile, completed_sse, fake_stream_request, read_http_request,
    run_fake_http_responses, sse_event,
};
use super::super::{
    RESPONSES_URL, RoleInput, RoleTask, classify_agent_run, run_role_with_access_token,
    run_role_with_auth,
};
use super::{
    SseState, StreamFailure, advance_retry_counter, request_with_observations,
    request_with_retries, retry_delay, usage_limit_error,
};
use crate::backend::BackendFailureKind;
use crate::backend::UsageSnapshot;
use crate::backend::builtin::usage::UsageReporter;
use crate::prompts::PromptRole;

#[test]
fn provider_retry_counts_each_send_and_completed_response_once() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let body = completed_sse(&json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 120,
                "output_tokens": 35,
                "total_tokens": 155,
                "input_tokens_details": {"cached_tokens": 12},
                "output_tokens_details": {"reasoning_tokens": 20}
            }
        }
    }));
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, response_body) in [
            ("429 Too Many Requests", "retry".to_owned()),
            ("200 OK", body),
        ] {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            requests.push(read_http_request(&mut stream));
            let delay = if status.starts_with("429") {
                "Retry-After: 0\r\n"
            } else {
                ""
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\n{delay}Content-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
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
    let mut usage = UsageSnapshot::default();
    let live = {
        let mut live = Vec::new();
        let mut observe = |snapshot: &UsageSnapshot| {
            live.push(snapshot.clone());
            Ok(())
        };
        let mut reporter = UsageReporter::new(&mut usage, &mut observe);
        let result = crate::runtime::block_on(request_with_observations(
            &client,
            &format!("http://{address}/v1/responses"),
            "fixture-access-token",
            &json!({"model": "fixture-model", "stream": true}),
            None,
            &AtomicBool::new(false),
            &mut reporter,
        ))
        .expect("construct provider runtime")
        .expect("retry succeeds");
        assert_eq!(result["status"], "completed");
        live
    };
    assert_eq!(usage.provider_request_attempts, Some(2));
    assert_eq!(usage.completed_responses, Some(1));
    assert_eq!(
        usage.latest_response.as_ref().and_then(|item| item.total),
        Some(155)
    );
    assert_eq!(live.len(), 3);
    let requests = server.join().expect("join provider fixture");
    assert_eq!(requests.len(), 2);
}

#[test]
fn request_attempt_is_reported_while_provider_response_is_held() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let (release_response, response_released) = mpsc::channel();
    let (request_arrived, request_received) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept provider request");
        let request = read_http_request(&mut stream);
        request_arrived
            .send(())
            .expect("notify that provider request arrived");
        response_released
            .recv_timeout(Duration::from_secs(2))
            .expect("wait until the request attempt is observed");
        let body = completed_sse(&json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": []}
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
    let (observed, observations) = mpsc::channel();
    let request_thread = thread::spawn(move || {
        let mut usage = UsageSnapshot::default();
        let mut observe = |snapshot: &UsageSnapshot| {
            observed
                .send(snapshot.clone())
                .map_err(|error| crate::error::SpecLoopError::Workflow(error.to_string()))
        };
        let mut reporter = UsageReporter::new(&mut usage, &mut observe);
        crate::runtime::block_on(request_with_observations(
            &client,
            &format!("http://{address}/v1/responses"),
            "fixture-access-token",
            &json!({"model": "fixture-model", "stream": true}),
            None,
            &AtomicBool::new(false),
            &mut reporter,
        ))
    });

    let in_flight = observations.recv_timeout(Duration::from_secs(2));
    let arrived = request_received.recv_timeout(Duration::from_secs(2));
    release_response
        .send(())
        .expect("release held provider response");
    let request_result = request_thread.join().expect("join request task");
    assert!(
        request_result.expect("construct request runtime").is_ok(),
        "request should complete successfully"
    );
    let in_flight = in_flight.expect("attempt report before provider response release");
    arrived.expect("request must be in flight before response release");
    assert_eq!(in_flight.provider_request_attempts, Some(1));
    assert_eq!(in_flight.completed_responses, Some(0));
    assert!(
        server
            .join()
            .expect("join provider server")
            .starts_with("POST ")
    );
}

#[test]
fn completed_usage_is_observed_before_malformed_output_validation() {
    let mut state = SseState::default();
    let mut usage = UsageSnapshot::default();
    let mut live_count = 0_u64;
    {
        let mut observe = |_: &UsageSnapshot| {
            live_count += 1;
            Ok(())
        };
        let mut reporter = UsageReporter::new(&mut usage, &mut observe);
        for (event_type, event) in [
            ("response.created", json!({"type": "response.created"})),
            (
                "response.in_progress",
                json!({"type": "response.in_progress"}),
            ),
        ] {
            assert!(
                state
                    .accept_event_observed(event_type, &event, &mut reporter)
                    .is_ok()
            );
        }
        let malformed = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
            }
        });
        assert!(matches!(
            state.accept_event_observed("response.completed", &malformed, &mut reporter),
            Err(StreamFailure::Fatal(_))
        ));
    }
    assert_eq!(usage.completed_responses, Some(1));
    assert_eq!(
        usage.latest_response.as_ref().and_then(|item| item.total),
        Some(3)
    );
    assert_eq!(live_count, 1);
}

#[cfg(unix)]
#[test]
fn incomplete_response_after_shell_effect_is_fatal_without_replay_or_handoff() {
    let temporary = TempDir::new().expect("create incomplete-response project");
    let completed_marker = temporary.path().join("completed-effect");
    let forbidden_marker = temporary.path().join("incomplete-effect");
    let (endpoint, server) = start_incomplete_response_provider(incomplete_response_events(
        &completed_marker,
        &forbidden_marker,
    ));
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: "execute one completed shell call and await a final role handoff",
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let token = input.token.to_owned();
    let mut conversation = None;
    let mut transcript = Vec::new();
    let mut usage = UsageSnapshot::default();
    let mut observer = |_: &UsageSnapshot| Ok(());
    let result = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
        input,
        None,
        move || {
            let token = token.clone();
            async move { Ok(token) }
        },
        &mut conversation,
        &mut transcript,
        &mut usage,
        &mut observer,
    ))
    .expect("construct incomplete-response runtime");
    let error = result.expect_err("incomplete provider response must not become a handoff");
    assert!(error.to_string().contains("provider failure"));
    assert_eq!(
        std::fs::read_to_string(&completed_marker).expect("read completed side effect"),
        "x"
    );
    assert!(!forbidden_marker.exists());
    assert_eq!(usage.provider_request_attempts, Some(2));
    assert_eq!(usage.completed_responses, Some(1));
    assert_eq!(usage.executed_shell_calls, Some(1));
    assert_eq!(
        usage
            .latest_response
            .as_ref()
            .and_then(|snapshot| snapshot.total),
        Some(12)
    );
    assert_eq!(
        String::from_utf8_lossy(&transcript)
            .matches("$ printf")
            .count(),
        1
    );
    let requests = server.join().expect("join incomplete-response fixture");
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.starts_with("POST ")));
}

fn incomplete_response_events(completed_marker: &Path, forbidden_marker: &Path) -> [String; 2] {
    let shell_command = format!("printf x >> '{}'", completed_marker.display());
    let completed = completed_sse(&json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "completed-shell-call",
                "name": "spec_loop.shell",
                "arguments": serde_json::to_string(&json!({"command": shell_command}))
                    .expect("serialize completed shell call")
            }],
            "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}
        }
    }));
    let incomplete_call = json!({
        "type": "function_call",
        "call_id": "incomplete-shell-call",
        "name": "spec_loop.shell",
        "arguments": serde_json::to_string(&json!({
            "command": format!("touch {}", forbidden_marker.display())
        }))
        .expect("serialize incomplete shell call")
    });
    let incomplete = format!(
        "{RESPONSE_STARTED}{}",
        sse_event(
            "response.incomplete",
            &json!({
                "type": "response.incomplete",
                "response": {
                    "status": "incomplete",
                    "output": [incomplete_call],
                    "incomplete_details": {"reason": "max_output_tokens"}
                }
            })
        )
    );
    [completed, incomplete]
}

fn start_incomplete_response_provider(
    responses: [String; 2],
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind incomplete-response fixture");
    let address = listener
        .local_addr()
        .expect("incomplete-response fixture address");
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
                .expect("write incomplete-response fixture");
        }
        requests
    });
    (format!("http://{address}/v1/responses"), server)
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
    let mut response_state = SseState::default();
    assert!(
        response_state
            .accept_event("response.created", &json!({"type": "response.created"}))
            .is_ok()
    );
    assert!(
        response_state
            .accept_event(
                "response.in_progress",
                &json!({"type": "response.in_progress"}),
            )
            .is_ok()
    );
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
fn provider_retry_counter_advancement_is_checked() {
    assert_eq!(advance_retry_counter(0).expect("advance retry counter"), 1);
    assert!(advance_retry_counter(u8::MAX).is_err());
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
            || async { Ok("fixture-access-token".to_owned()) },
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
