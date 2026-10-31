use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};

use super::super::shell::shell_tool_definition;
use super::super::test_support::{
    WATCHDOG, builtin_test_profile, completed_sse, read_request_until, sse_event, write_until,
};
use crate::backend::UsageSnapshot;
use crate::prompts::PromptRole;

mod chain;

struct WebSocketCapture {
    creates: Vec<String>,
    handshake_headers: Vec<(String, bool)>,
}

struct HttpFallbackCapture {
    creates: Vec<String>,
    requests: Vec<String>,
    handshake_headers: Vec<(String, bool)>,
}

type WebSocketServer = thread::JoinHandle<WebSocketCapture>;
type HttpFallbackServer = thread::JoinHandle<HttpFallbackCapture>;

fn shell_call_item(call_id: &str, command: &str) -> Value {
    json!({
        "type": "function_call",
        "call_id": call_id,
        "name": "spec_loop.shell",
        "arguments": serde_json::to_string(&json!({"command": command}))
            .expect("serialize shell call")
    })
}

#[derive(Clone, Copy)]
enum FatalWebSocketCase {
    MalformedJson,
    BinaryFrame,
    InvalidOrder,
    UnsupportedEvent,
    OrdinaryError,
    ResponseFailed,
    ResponseIncomplete,
}

#[test]
fn fatal_websocket_events_stop_without_retry_or_http_fallback() {
    let cases = [
        FatalWebSocketCase::MalformedJson,
        FatalWebSocketCase::BinaryFrame,
        FatalWebSocketCase::InvalidOrder,
        FatalWebSocketCase::UnsupportedEvent,
        FatalWebSocketCase::OrdinaryError,
        FatalWebSocketCase::ResponseFailed,
        FatalWebSocketCase::ResponseIncomplete,
    ];
    for case in cases {
        let temporary = TempDir::new().expect("create fatal WebSocket project");
        let (endpoint, server) = fatal_websocket_provider(case);
        let profile = builtin_test_profile();
        let cancelled = Arc::new(AtomicBool::new(false));
        let input = super::super::RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "fatal WebSocket provider event",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &endpoint,
            token: "fixture-access-token",
        };
        let mut conversation = None;
        let mut transcript = Vec::new();
        let mut usage = UsageSnapshot::default();
        let mut observer = |_: &UsageSnapshot| Ok(());
        let error = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
            input.with_test_deadline(),
            None,
            || async { Ok("fixture-access-token".to_owned()) },
            &mut conversation,
            &mut transcript,
            &mut usage,
            &mut observer,
        ))
        .expect("construct fatal WebSocket runtime")
        .expect_err("fatal WebSocket event must stop the role");

        assert_eq!(usage.provider_request_attempts, Some(1));
        assert_eq!(usage.completed_responses, Some(0));
        assert_eq!(usage.executed_shell_calls, Some(0));
        assert!(!error.to_string().contains("fixture-access-token"));
        assert!(!error.to_string().contains("provider-secret-prose"));
        let capture = server.join().expect("join fatal WebSocket fixture");
        assert_eq!(capture.creates.len(), 1);
    }
}

fn fatal_websocket_provider(
    case: FatalWebSocketCase,
) -> (String, thread::JoinHandle<WebSocketCapture>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fatal WebSocket fixture");
    listener
        .set_nonblocking(true)
        .expect("make fatal WebSocket fixture nonblocking");
    let address = listener
        .local_addr()
        .expect("fatal WebSocket fixture address");
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async fatal WebSocket listener");
            let (mut socket, handshake) = accept_fixture_websocket(&listener).await;
            let create = receive_websocket_text(&mut socket).await;
            match case {
                FatalWebSocketCase::MalformedJson => {
                    socket
                        .send(Message::text("{"))
                        .await
                        .expect("send malformed JSON event");
                }
                FatalWebSocketCase::BinaryFrame => {
                    socket
                        .send(Message::binary(vec![0, 1, 2]))
                        .await
                        .expect("send unsupported binary frame");
                }
                FatalWebSocketCase::InvalidOrder => {
                    send_response_event(
                        &mut socket,
                        json!({
                            "type":"response.completed",
                            "response":{"status":"completed","output":[]}
                        }),
                    )
                    .await;
                }
                FatalWebSocketCase::UnsupportedEvent => {
                    send_response_event(&mut socket, json!({"type":"response.future_event"})).await;
                }
                FatalWebSocketCase::OrdinaryError => {
                    send_response_event(
                        &mut socket,
                        json!({
                            "type":"error",
                            "error":{
                                "code":"invalid_request_error",
                                "message":"provider-secret-prose"
                            }
                        }),
                    )
                    .await;
                }
                FatalWebSocketCase::ResponseFailed => {
                    send_started_events(&mut socket, "resp_failed").await;
                    send_response_event(
                        &mut socket,
                        json!({
                            "type":"response.failed",
                            "response":{"error":{"code":"rate_limit_exceeded"}}
                        }),
                    )
                    .await;
                }
                FatalWebSocketCase::ResponseIncomplete => {
                    send_started_events(&mut socket, "resp_incomplete").await;
                    send_response_event(
                        &mut socket,
                        json!({
                            "type":"response.incomplete",
                            "response":{"error":{"code":"server_error"}}
                        }),
                    )
                    .await;
                }
            }
            drop(socket);
            WebSocketCapture {
                creates: vec![create],
                handshake_headers: vec![handshake],
            }
        })
        .expect("run fatal WebSocket fixture")
    });
    (format!("http://{address}/v1/responses"), server)
}

#[test]
fn websocket_setup_and_receive_obey_role_deadline() {
    for await_response in [false, true] {
        let temporary = TempDir::new().expect("create WebSocket deadline project");
        let (endpoint, server) = websocket_timeout_provider(await_response);
        let profile = builtin_test_profile();
        let cancelled = Arc::new(AtomicBool::new(false));
        let input = super::super::RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "stop on the role deadline",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: Some(Instant::now() + Duration::from_millis(75)),
            endpoint: &endpoint,
            token: "fixture-access-token",
        };
        let mut conversation = None;
        let mut transcript = Vec::new();
        let mut usage = UsageSnapshot::default();
        let mut observer = |_: &UsageSnapshot| Ok(());
        let error = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
            input,
            None,
            || async { Ok("fixture-access-token".to_owned()) },
            &mut conversation,
            &mut transcript,
            &mut usage,
            &mut observer,
        ))
        .expect("construct WebSocket deadline runtime")
        .expect_err("deadline must stop the provider operation");

        assert!(error.to_string().contains("role deadline expired"));
        assert_eq!(
            usage.provider_request_attempts,
            Some(u64::from(await_response))
        );
        assert_eq!(usage.completed_responses, Some(0));
        assert_eq!(
            server
                .join()
                .expect("join WebSocket deadline fixture")
                .is_some(),
            await_response
        );
    }
}

fn websocket_timeout_provider(
    await_response: bool,
) -> (String, thread::JoinHandle<Option<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind WebSocket deadline fixture");
    listener
        .set_nonblocking(true)
        .expect("make WebSocket deadline fixture nonblocking");
    let address = listener
        .local_addr()
        .expect("WebSocket deadline fixture address");
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async WebSocket deadline listener");
            let (stream, _) = tokio::time::timeout(WATCHDOG, listener.accept())
                .await
                .expect("accept WebSocket deadline fixture timeout")
                .expect("accept WebSocket deadline fixture");
            if await_response {
                let mut socket = accept_async(stream)
                    .await
                    .expect("accept WebSocket deadline handshake");
                let create = receive_websocket_text(&mut socket).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                Some(create)
            } else {
                tokio::time::sleep(Duration::from_millis(200)).await;
                None
            }
        })
        .expect("run WebSocket deadline fixture")
    });
    (format!("http://{address}/v1/responses"), server)
}

#[test]
fn websocket_request_attempt_is_reported_after_create_is_sent() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sent-request fixture");
    listener
        .set_nonblocking(true)
        .expect("make sent-request fixture nonblocking");
    let address = listener.local_addr().expect("sent-request fixture address");
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async sent-request listener");
            let (mut socket, _) = accept_fixture_websocket(&listener).await;
            match tokio::time::timeout(Duration::from_millis(200), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => Some(text.to_string()),
                _ => None,
            }
        })
        .expect("run sent-request fixture")
    });
    let endpoint = format!("http://{address}/v1/responses");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build provider client");
    let cancelled = AtomicBool::new(false);
    let body = json!({
        "model":"fixture-model",
        "store":false,
        "stream":true,
        "input":[{"role":"user","content":"sent request accounting"}]
    });
    let mut transport = super::ConversationTransport::default();
    let mut usage = UsageSnapshot::default();
    let mut observer = |_: &UsageSnapshot| {
        Err(crate::error::SpecLoopError::Workflow(
            "fixture observer failure".to_owned(),
        ))
    };
    let mut reporter = super::super::usage::UsageReporter::new(&mut usage, &mut observer);
    let result = crate::runtime::block_on(super::request_with_conversation_observations(
        super::ConversationProviderRequest {
            client: &client,
            endpoint: &endpoint,
            token: "fixture-access-token",
            body: &body,
            incremental_input: None,
            transport: &mut transport,
            deadline: Some(Instant::now() + WATCHDOG),
            cancelled: &cancelled,
        },
        &mut reporter,
    ))
    .expect("construct provider runtime");

    assert!(result.is_err(), "result: {result:?}; usage: {usage:?}");
    assert_eq!(usage.provider_request_attempts, Some(1));
    let create = server.join().expect("join sent-request fixture");
    let create: Value = serde_json::from_str(&create.expect("create frame must be sent"))
        .expect("parse create frame");
    assert_eq!(create["type"], "response.create");
}

#[test]
fn websocket_cancellation_after_create_send_stops_before_response_handling() {
    let temporary = TempDir::new().expect("create WebSocket cancellation project");
    let (endpoint, server) = websocket_timeout_provider(true);
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = Arc::clone(&cancelled);
    let input = super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: "cancel after the WebSocket create is sent",
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let mut conversation = None;
    let mut transcript = Vec::new();
    let mut usage = UsageSnapshot::default();
    let mut observer = move |snapshot: &UsageSnapshot| {
        if snapshot.provider_request_attempts == Some(1) {
            cancellation.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    };
    let error = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
        input,
        None,
        || async { Ok("fixture-access-token".to_owned()) },
        &mut conversation,
        &mut transcript,
        &mut usage,
        &mut observer,
    ))
    .expect("construct WebSocket cancellation runtime")
    .expect_err("cancellation must stop before response handling");

    assert!(matches!(error, crate::error::SpecLoopError::Interrupted));
    assert_eq!(usage.provider_request_attempts, Some(1));
    assert_eq!(usage.completed_responses, Some(0));
    assert_eq!(usage.executed_shell_calls, Some(0));
    let create = server
        .join()
        .expect("join WebSocket cancellation fixture")
        .expect("sent create frame");
    let create: Value = serde_json::from_str(&create).expect("parse create frame");
    assert_eq!(create["type"], "response.create");
}

#[cfg(unix)]
#[test]
fn websocket_close_recovery_ignores_provider_metadata_and_timing_events() {
    let temporary = TempDir::new().expect("create WebSocket recovery project");
    let completed_marker = temporary.path().join("completed-effect");
    let incomplete_marker = temporary.path().join("incomplete-effect");
    let (endpoint, server) =
        websocket_recovery_provider(&completed_marker, &incomplete_marker, false);
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: "use WebSocket continuation without replaying completed shell work",
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let mut conversation = None;
    let mut transcript = Vec::new();
    let mut usage = UsageSnapshot::default();
    let mut observer = |_: &UsageSnapshot| Ok(());
    let result = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
        input.with_test_deadline(),
        None,
        || async { Ok("fixture-access-token".to_owned()) },
        &mut conversation,
        &mut transcript,
        &mut usage,
        &mut observer,
    ))
    .expect("construct WebSocket recovery runtime")
    .expect("recover WebSocket response");

    assert!(result.response.contains("Recovered over WebSocket"));
    assert_eq!(
        std::fs::read_to_string(&completed_marker).expect("read completed shell effect"),
        "x"
    );
    assert!(
        !incomplete_marker.exists(),
        "incomplete call must not execute"
    );
    assert_eq!(usage.provider_request_attempts, Some(4));
    assert_eq!(usage.completed_responses, Some(2));
    assert_eq!(usage.executed_shell_calls, Some(1));

    let capture = server.join().expect("join WebSocket provider");
    assert_websocket_wire_contract(
        &capture,
        "use WebSocket continuation without replaying completed shell work",
    );
}

fn assert_websocket_wire_contract(capture: &WebSocketCapture, prompt: &str) {
    assert_eq!(capture.creates.len(), 4);
    assert!(
        capture
            .handshake_headers
            .iter()
            .all(|(authorization, has_api_key)| {
                authorization == "Bearer fixture-access-token" && !has_api_key
            })
    );
    let first: Value = serde_json::from_str(&capture.creates[0]).expect("parse initial create");
    assert_eq!(first["type"], "response.create");
    assert_eq!(first["store"], false);
    assert_eq!(first["input"], json!([{"role":"user","content":prompt}]));
    assert!(first.get("stream").is_none());
    assert!(first.get("background").is_none());
    assert!(first.get("stream_id").is_none());
    assert!(first.get("previous_response_id").is_none());
    assert_eq!(first["instructions"], super::super::SYSTEM_INSTRUCTIONS);
    assert_eq!(first["tools"][0], shell_tool_definition());
    assert_eq!(first["text"]["format"]["strict"], true);

    let continuation: Value =
        serde_json::from_str(&capture.creates[1]).expect("parse continuation");
    assert_eq!(
        continuation["previous_response_id"],
        "resp_completed_tool_turn"
    );
    assert_eq!(continuation["store"], false);
    assert_eq!(
        continuation["input"]
            .as_array()
            .expect("incremental input")
            .len(),
        1
    );
    assert_eq!(continuation["input"][0]["type"], "function_call_output");
    assert_eq!(continuation["input"][0]["call_id"], "completed-shell-call");
    assert_eq!(continuation["instructions"], first["instructions"]);
    assert_eq!(continuation["tools"], first["tools"]);
    assert_eq!(continuation["text"]["format"], first["text"]["format"]);

    let recovery: Value = serde_json::from_str(&capture.creates[2]).expect("parse full recovery");
    assert_eq!(recovery["store"], false);
    assert!(recovery.get("previous_response_id").is_none());
    assert_eq!(
        recovery["input"]
            .as_array()
            .expect("full recovery input")
            .len(),
        3
    );
    assert_eq!(recovery["input"][1]["call_id"], "completed-shell-call");
    assert_eq!(recovery["input"][2]["call_id"], "completed-shell-call");
    assert_eq!(
        capture.creates[2], capture.creates[3],
        "full recovery requests reuse exact bytes"
    );
}

#[cfg(unix)]
#[test]
fn websocket_handshake_rejection_uses_sticky_full_history_http() {
    let temporary = TempDir::new().expect("create handshake fallback project");
    let (endpoint, server) = websocket_handshake_rejection_provider();
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: "use HTTP when the WebSocket handshake is unavailable",
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let mut conversation = None;
    let mut transcript = Vec::new();
    let mut usage = UsageSnapshot::default();
    let mut observer = |_: &UsageSnapshot| Ok(());
    let result = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
        input.with_test_deadline(),
        None,
        || async { Ok("fixture-access-token".to_owned()) },
        &mut conversation,
        &mut transcript,
        &mut usage,
        &mut observer,
    ))
    .expect("construct handshake fallback runtime")
    .expect("finish role over sticky HTTP fallback");

    assert!(result.response.contains("Finished after HTTP fallback"));
    assert_eq!(usage.provider_request_attempts, Some(2));
    assert_eq!(usage.completed_responses, Some(2));
    assert_eq!(usage.executed_shell_calls, Some(1));

    let capture = server.join().expect("join handshake fallback fixture");
    assert_eq!(capture.handshake_headers.len(), 1);
    assert_eq!(
        capture.handshake_headers[0],
        ("Bearer fixture-access-token".to_owned(), false)
    );
    assert_eq!(capture.requests.len(), 2);
    for (index, request) in capture.requests.iter().enumerate() {
        let payload: Value =
            serde_json::from_str(request.split_once("\r\n\r\n").expect("HTTP request body").1)
                .expect("parse sticky fallback body");
        assert_eq!(payload["store"], false);
        assert_eq!(payload["stream"], true);
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(
            payload["input"].as_array().expect("full input").len(),
            if index == 0 { 1 } else { 3 }
        );
    }
}

fn websocket_handshake_rejection_provider() -> (String, HttpFallbackServer) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind handshake rejection fixture");
    listener
        .set_nonblocking(true)
        .expect("make handshake rejection fixture nonblocking");
    let address = listener
        .local_addr()
        .expect("handshake rejection fixture address");
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async handshake rejection listener");
            let (stream, _) = tokio::time::timeout(WATCHDOG, listener.accept())
                .await
                .expect("accept WebSocket handshake timeout")
                .expect("accept WebSocket handshake");
            let handshake = read_websocket_handshake(&stream).await;
            let mut stream = stream.into_std().expect("convert handshake socket");
            stream
                .set_nonblocking(false)
                .expect("make handshake socket blocking");
            write_until(
                &mut stream,
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                Instant::now() + WATCHDOG,
            )
            .expect("reject WebSocket upgrade");
            drop(stream);

            let mut requests = Vec::new();
            for (output, call_id) in [(true, "handshake-shell-call"), (false, "unused")] {
                let mut stream = accept_fixture_http(&listener).await;
                requests.push(
                    read_request_until(&mut stream, Instant::now() + WATCHDOG)
                        .expect("read HTTP fallback request"),
                );
                let response = if output {
                    json!({
                        "type":"response.completed",
                        "response":{
                            "id":"resp_handshake_shell",
                            "status":"completed",
                            "output":[shell_call_item(call_id, "true")]
                        }
                    })
                } else {
                    let final_text = r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Finished after HTTP fallback."}}"#;
                    json!({
                        "type":"response.completed",
                        "response":{
                            "id":"resp_handshake_final",
                            "status":"completed",
                            "output":[{"type":"message","content":[{"type":"output_text","text":final_text}]}]
                        }
                    })
                };
                write_sse_reply(&mut stream, &completed_sse(&response));
            }
            HttpFallbackCapture {
                creates: Vec::new(),
                requests,
                handshake_headers: vec![handshake],
            }
        })
        .expect("run handshake rejection fixture")
    });
    (format!("http://{address}/v1/responses"), server)
}

#[cfg(unix)]
#[test]
fn websocket_reconnect_setup_rejection_falls_back_without_unfinished_output() {
    let temporary = TempDir::new().expect("create reconnect fallback project");
    let completed_marker = temporary.path().join("completed-effect");
    let incomplete_marker = temporary.path().join("incomplete-effect");
    let completed_command = format!("printf x > '{}'", completed_marker.display());
    let incomplete_command = format!("touch '{}'", incomplete_marker.display());
    let (endpoint, server) =
        websocket_reconnect_rejection_provider(&completed_command, &incomplete_command);
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: "fall back when a WebSocket recovery connection is unavailable",
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let mut conversation = None;
    let mut transcript = Vec::new();
    let mut usage = UsageSnapshot::default();
    let mut observer = |_: &UsageSnapshot| Ok(());
    let result = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
        input.with_test_deadline(),
        None,
        || async { Ok("fixture-access-token".to_owned()) },
        &mut conversation,
        &mut transcript,
        &mut usage,
        &mut observer,
    ))
    .expect("construct reconnect fallback runtime")
    .expect("finish role over HTTP after reconnect rejection");

    assert!(
        result
            .response
            .contains("Recovered after reconnect fallback")
    );
    assert_eq!(
        std::fs::read_to_string(completed_marker).expect("completed effect"),
        "x"
    );
    assert!(!incomplete_marker.exists());
    assert_eq!(usage.provider_request_attempts, Some(3));
    assert_eq!(usage.completed_responses, Some(2));
    assert_eq!(usage.executed_shell_calls, Some(1));

    let capture = server.join().expect("join reconnect fallback fixture");
    assert_eq!(capture.creates.len(), 2);
    assert_eq!(capture.handshake_headers.len(), 2);
    assert_eq!(capture.requests.len(), 1);
    let continuation: Value =
        serde_json::from_str(&capture.creates[1]).expect("parse unfinished continuation");
    assert_eq!(
        continuation["previous_response_id"],
        "resp_reconnect_complete"
    );
    let fallback: Value = serde_json::from_str(
        capture.requests[0]
            .split_once("\r\n\r\n")
            .expect("fallback body")
            .1,
    )
    .expect("parse reconnect fallback body");
    assert_eq!(fallback["store"], false);
    assert_eq!(fallback["stream"], true);
    assert!(fallback.get("previous_response_id").is_none());
    assert_eq!(
        fallback["input"]
            .as_array()
            .expect("full local history")
            .len(),
        3
    );
    assert_eq!(fallback["input"][1]["call_id"], "reconnect-completed-call");
    assert_eq!(fallback["input"][2]["call_id"], "reconnect-completed-call");
    assert!(
        !fallback["input"]
            .to_string()
            .contains("incomplete-recovery-call")
    );
}

fn websocket_reconnect_rejection_provider(
    completed_command: &str,
    incomplete_command: &str,
) -> (String, HttpFallbackServer) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind reconnect fallback fixture");
    listener
        .set_nonblocking(true)
        .expect("make reconnect fallback fixture nonblocking");
    let address = listener
        .local_addr()
        .expect("reconnect fallback fixture address");
    let completed_command = completed_command.to_owned();
    let incomplete_command = incomplete_command.to_owned();
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async reconnect fallback listener");
            let (mut socket, first_handshake) = accept_fixture_websocket(&listener).await;
            let first_create = receive_websocket_text(&mut socket).await;
            send_started_events(&mut socket, "resp_reconnect_complete").await;
            send_response_event(
                &mut socket,
                json!({
                    "type":"response.completed",
                    "response":{
                        "id":"resp_reconnect_complete",
                        "status":"completed",
                        "output":[shell_call_item("reconnect-completed-call", &completed_command)]
                    }
                }),
            )
            .await;
            let continuation = receive_websocket_text(&mut socket).await;
            send_started_events(&mut socket, "resp_reconnect_incomplete").await;
            send_response_event(
                &mut socket,
                json!({
                    "type":"response.output_item.done",
                    "item":shell_call_item("incomplete-recovery-call", &incomplete_command)
                }),
            )
            .await;
            socket
                .send(Message::Close(None))
                .await
                .expect("close before recovery completion");
            drop(socket);

            let (stream, _) = tokio::time::timeout(WATCHDOG, listener.accept())
                .await
                .expect("accept reconnect handshake timeout")
                .expect("accept reconnect handshake");
            let reconnect_handshake = read_websocket_handshake(&stream).await;
            let mut stream = stream.into_std().expect("convert reconnect socket");
            stream
                .set_nonblocking(false)
                .expect("make reconnect socket blocking");
            write_until(
                &mut stream,
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                Instant::now() + WATCHDOG,
            )
            .expect("reject recovery WebSocket handshake");
            drop(stream);

            let mut http = accept_fixture_http(&listener).await;
            let request = read_request_until(&mut http, Instant::now() + WATCHDOG)
                .expect("read full-history recovery request");
            let final_text = r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Recovered after reconnect fallback."}}"#;
            let reply = completed_sse(&json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_reconnect_http_final",
                    "status":"completed",
                    "output":[{"type":"message","content":[{"type":"output_text","text":final_text}]}]
                }
            }));
            write_sse_reply(&mut http, &reply);
            HttpFallbackCapture {
                creates: vec![first_create, continuation],
                requests: vec![request],
                handshake_headers: vec![first_handshake, reconnect_handshake],
            }
        })
        .expect("run reconnect fallback fixture")
    });
    (format!("http://{address}/v1/responses"), server)
}

#[cfg(unix)]
#[test]
fn mixed_websocket_and_http_retries_share_the_four_send_limit() {
    let temporary = TempDir::new().expect("create mixed retry project");
    let (endpoint, server) = mixed_retry_provider();
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: "share the retry cap across WebSocket and HTTP",
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let mut conversation = None;
    let mut transcript = Vec::new();
    let mut usage = UsageSnapshot::default();
    let mut observer = |_: &UsageSnapshot| Ok(());
    let result = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
        input.with_test_deadline(),
        None,
        || async { Ok("fixture-access-token".to_owned()) },
        &mut conversation,
        &mut transcript,
        &mut usage,
        &mut observer,
    ))
    .expect("construct mixed retry runtime");

    assert!(result.is_err());
    assert_eq!(usage.provider_request_attempts, Some(5));
    assert_eq!(usage.completed_responses, Some(1));
    assert_eq!(usage.executed_shell_calls, Some(1));

    let capture = server.join().expect("join mixed retry fixture");
    assert_eq!(capture.creates.len(), 2);
    assert_eq!(capture.requests.len(), 3);
    assert!(capture.requests.iter().all(|request| {
        request.starts_with("POST /v1/responses ")
            && request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-access-token")
    }));
    let continuation: Value =
        serde_json::from_str(&capture.creates[1]).expect("parse mixed retry continuation");
    assert_eq!(continuation["previous_response_id"], "resp_mixed_retry");
    assert_eq!(continuation["input"][0]["type"], "function_call_output");
    for request in &capture.requests {
        let payload: Value = serde_json::from_str(
            request
                .split_once("\r\n\r\n")
                .expect("HTTP fallback body")
                .1,
        )
        .expect("parse mixed retry request");
        assert_eq!(payload["store"], false);
        assert_eq!(payload["stream"], true);
        assert!(payload.get("previous_response_id").is_none());
    }
}

fn mixed_retry_provider() -> (String, HttpFallbackServer) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mixed retry fixture");
    listener
        .set_nonblocking(true)
        .expect("make mixed retry fixture nonblocking");
    let address = listener.local_addr().expect("mixed retry fixture address");
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async mixed retry listener");
            let (mut socket, handshake) = accept_fixture_websocket(&listener).await;
            let first_create = receive_websocket_text(&mut socket).await;
            send_started_events(&mut socket, "resp_mixed_retry").await;
            send_response_event(
                &mut socket,
                json!({
                    "type":"response.completed",
                    "response":{
                        "id":"resp_mixed_retry",
                        "status":"completed",
                        "output":[shell_call_item("mixed-retry-call", "true")]
                    }
                }),
            )
            .await;
            let continuation = receive_websocket_text(&mut socket).await;
            send_response_event(
                &mut socket,
                json!({
                    "type":"error",
                    "error":{
                        "type":"invalid_request_error",
                        "code":"unsupported_parameter",
                        "param":"previous_response_id"
                    }
                }),
            )
            .await;
            drop(socket);

            let mut requests = Vec::new();
            for attempt in 0..3 {
                let mut stream = accept_fixture_http(&listener).await;
                requests.push(
                    read_request_until(&mut stream, Instant::now() + WATCHDOG)
                        .expect("read mixed retry HTTP request"),
                );
                let (status, content_type, body) = if attempt == 0 {
                    (
                        "503 Service Unavailable",
                        "application/json",
                        "{}".to_owned(),
                    )
                } else {
                    (
                        "200 OK",
                        "text/event-stream",
                        sse_event(
                            "error",
                            &json!({"type":"error","error":{"code":"server_error"}}),
                        ),
                    )
                };
                write_retryable_reply(&mut stream, status, content_type, &body);
            }
            HttpFallbackCapture {
                creates: vec![first_create, continuation],
                requests,
                handshake_headers: vec![handshake],
            }
        })
        .expect("run mixed retry fixture")
    });
    (format!("http://{address}/v1/responses"), server)
}

fn write_retryable_reply(
    stream: &mut std::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) {
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    write_until(stream, reply.as_bytes(), Instant::now() + WATCHDOG)
        .expect("write mixed retry provider response");
}

#[cfg(unix)]
#[test]
fn unsupported_websocket_continuation_falls_back_to_sticky_full_history_sse() {
    let temporary = TempDir::new().expect("create fallback project");
    let marker = temporary.path().join("fallback-shell-effects");
    let (endpoint, server) = websocket_unsupported_continuation_provider(&marker);
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: "fall back to full-history HTTP after an unsupported continuation",
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let mut conversation = None;
    let mut transcript = Vec::new();
    let mut usage = UsageSnapshot::default();
    let mut observer = |_: &UsageSnapshot| Ok(());
    let result = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
        input.with_test_deadline(),
        None,
        || async { Ok("fixture-access-token".to_owned()) },
        &mut conversation,
        &mut transcript,
        &mut usage,
        &mut observer,
    ))
    .expect("construct HTTP fallback runtime")
    .expect("finish role with sticky HTTP fallback");

    assert!(result.response.contains("Finished after HTTP fallback"));
    assert_eq!(
        std::fs::read_to_string(&marker).expect("read both completed shell effects"),
        "xy"
    );
    assert_eq!(usage.provider_request_attempts, Some(4));
    assert_eq!(usage.completed_responses, Some(3));
    assert_eq!(usage.executed_shell_calls, Some(2));

    let capture = server.join().expect("join fallback provider");
    assert_eq!(capture.creates.len(), 2);
    assert_eq!(
        capture.requests.len(),
        2,
        "HTTP SSE remains sticky after fallback"
    );
    assert!(
        capture
            .handshake_headers
            .iter()
            .all(|(authorization, has_api_key)| {
                authorization == "Bearer fixture-access-token" && !has_api_key
            })
    );
    for (index, request) in capture.requests.iter().enumerate() {
        assert!(request.starts_with("POST /v1/responses "));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-access-token")
        );
        assert!(!request.to_ascii_lowercase().contains("x-api-key"));
        let (_, body) = request.split_once("\r\n\r\n").expect("fallback HTTP body");
        let payload: Value = serde_json::from_str(body).expect("parse fallback HTTP body");
        assert_eq!(payload["store"], false);
        assert_eq!(payload["stream"], true);
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(
            payload["input"]
                .as_array()
                .expect("full local history")
                .len(),
            if index == 0 { 3 } else { 5 }
        );
    }
    let continuation: Value =
        serde_json::from_str(&capture.creates[1]).expect("parse unsupported continuation");
    assert_eq!(
        continuation["previous_response_id"],
        "resp_completed_tool_turn"
    );
    assert_eq!(
        continuation["input"]
            .as_array()
            .expect("incremental input")
            .len(),
        1
    );
}

fn websocket_unsupported_continuation_provider(
    marker: &std::path::Path,
) -> (String, HttpFallbackServer) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fallback fixture");
    listener
        .set_nonblocking(true)
        .expect("make fallback fixture nonblocking");
    let address = listener.local_addr().expect("fallback fixture address");
    let first_command = format!("printf x >> '{}'", marker.display());
    let second_command = format!("printf y >> '{}'", marker.display());
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async fallback listener");
            serve_unsupported_continuation(&listener, &first_command, &second_command).await
        })
        .expect("run fallback fixture")
    });
    (format!("http://{address}/v1/responses"), server)
}

async fn serve_unsupported_continuation(
    listener: &tokio::net::TcpListener,
    first_command: &str,
    second_command: &str,
) -> HttpFallbackCapture {
    let (mut socket, handshake) = accept_fixture_websocket(listener).await;
    let first_create = receive_websocket_text(&mut socket).await;
    send_started_events(&mut socket, "resp_completed_tool_turn").await;
    send_response_event(
        &mut socket,
        json!({
            "type":"response.completed",
            "response": {
                "id":"resp_completed_tool_turn",
                "status":"completed",
                "output":[shell_call_item("completed-shell-call", first_command)]
            }
        }),
    )
    .await;
    let continuation = receive_websocket_text(&mut socket).await;
    // The continuation's provider error intentionally omits all prose.
    send_response_event(
        &mut socket,
        json!({
            "type":"error",
            "error": {
                "type":"invalid_request_error",
                "code":"unsupported_parameter",
                "param":"previous_response_id"
            }
        }),
    )
    .await;
    drop(socket);

    let mut stream = accept_fixture_http(listener).await;
    let fallback_request = read_request_until(&mut stream, Instant::now() + WATCHDOG)
        .expect("read full-history HTTP fallback");
    let first_http_body = completed_sse(&json!({
        "type":"response.completed",
        "response": {
            "id":"resp_fallback_tool",
            "status":"completed",
            "output":[shell_call_item("fallback-shell-call", second_command)]
        }
    }));
    write_sse_reply(&mut stream, &first_http_body);

    let mut stream = accept_fixture_http(listener).await;
    let sticky_request = read_request_until(&mut stream, Instant::now() + WATCHDOG)
        .expect("read sticky HTTP request");
    let final_text = r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Finished after HTTP fallback."}}"#;
    let final_body = completed_sse(&json!({
        "type":"response.completed",
        "response": {
            "id":"resp_final_http",
            "status":"completed",
            "output":[{"type":"message","content":[{"type":"output_text","text":final_text}]}]
        }
    }));
    write_sse_reply(&mut stream, &final_body);
    HttpFallbackCapture {
        creates: vec![first_create, continuation],
        requests: vec![fallback_request, sticky_request],
        handshake_headers: vec![handshake],
    }
}

async fn accept_fixture_http(listener: &tokio::net::TcpListener) -> std::net::TcpStream {
    let (stream, _) = tokio::time::timeout(WATCHDOG, listener.accept())
        .await
        .expect("accept HTTP fixture timeout")
        .expect("accept HTTP fixture request");
    let stream = stream.into_std().expect("convert HTTP fixture socket");
    stream
        .set_nonblocking(false)
        .expect("make HTTP fixture socket blocking");
    stream
}

fn write_sse_reply(stream: &mut std::net::TcpStream, body: &str) {
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    write_until(stream, reply.as_bytes(), Instant::now() + WATCHDOG)
        .expect("write deterministic SSE fixture reply");
}

#[cfg(unix)]
#[test]
fn websocket_previous_response_not_found_retries_full_history_without_old_id() {
    let temporary = TempDir::new().expect("create WebSocket cache-recovery project");
    let completed_marker = temporary.path().join("completed-effect");
    let incomplete_marker = temporary.path().join("incomplete-effect");
    let (endpoint, server) =
        websocket_recovery_provider(&completed_marker, &incomplete_marker, true);
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: "recover full local context when the response cache is missing",
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let mut conversation = None;
    let mut transcript = Vec::new();
    let mut usage = UsageSnapshot::default();
    let mut observer = |_: &UsageSnapshot| Ok(());
    let result = crate::runtime::block_on(super::super::run_role_with_token_provider_observed(
        input.with_test_deadline(),
        None,
        || async { Ok("fixture-access-token".to_owned()) },
        &mut conversation,
        &mut transcript,
        &mut usage,
        &mut observer,
    ))
    .expect("construct missing-cache runtime")
    .expect("recover missing WebSocket response cache");

    assert!(result.response.contains("Recovered over WebSocket"));
    assert_eq!(
        std::fs::read_to_string(&completed_marker).expect("read completed shell effect"),
        "x"
    );
    assert!(!incomplete_marker.exists());
    assert_eq!(usage.provider_request_attempts, Some(3));
    assert_eq!(usage.completed_responses, Some(2));
    assert_eq!(usage.executed_shell_calls, Some(1));

    let capture = server.join().expect("join missing-cache fixture");
    assert_eq!(capture.creates.len(), 3);
    let continuation: Value =
        serde_json::from_str(&capture.creates[1]).expect("parse continuation");
    assert_eq!(
        continuation["previous_response_id"],
        "resp_completed_tool_turn"
    );
    let recovery: Value = serde_json::from_str(&capture.creates[2]).expect("parse cache recovery");
    assert!(recovery.get("previous_response_id").is_none());
    assert_eq!(recovery["store"], false);
    assert_eq!(
        recovery["input"]
            .as_array()
            .expect("full local input")
            .len(),
        3
    );
    assert_eq!(recovery["input"][1]["call_id"], "completed-shell-call");
    assert_eq!(recovery["input"][2]["call_id"], "completed-shell-call");
}

fn websocket_recovery_provider(
    completed_marker: &std::path::Path,
    incomplete_marker: &std::path::Path,
    previous_response_not_found: bool,
) -> (String, WebSocketServer) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind WebSocket fixture");
    listener
        .set_nonblocking(true)
        .expect("make WebSocket fixture nonblocking");
    let address = listener.local_addr().expect("WebSocket fixture address");
    let command = format!("printf x >> '{}'", completed_marker.display());
    let incomplete_command = format!("touch '{}'", incomplete_marker.display());
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async WebSocket fixture listener");
            serve_websocket_recovery(
                &listener,
                &command,
                &incomplete_command,
                previous_response_not_found,
            )
            .await
        })
        .expect("run WebSocket provider fixture")
    });
    (format!("http://{address}/v1/responses"), server)
}

async fn serve_websocket_recovery(
    listener: &tokio::net::TcpListener,
    command: &str,
    incomplete_command: &str,
    previous_response_not_found: bool,
) -> WebSocketCapture {
    let mut capture = WebSocketCapture {
        creates: Vec::new(),
        handshake_headers: Vec::new(),
    };
    let connection_count = if previous_response_not_found { 2 } else { 3 };
    for connection_index in 0..connection_count {
        let (mut socket, handshake) = accept_fixture_websocket(listener).await;
        capture.handshake_headers.push(handshake);
        match connection_index {
            0 => {
                serve_first_recovery_connection(
                    &mut socket,
                    &mut capture.creates,
                    command,
                    incomplete_command,
                    previous_response_not_found,
                )
                .await;
            }
            1 if !previous_response_not_found => {
                serve_incomplete_recovery_connection(
                    &mut socket,
                    &mut capture.creates,
                    incomplete_command,
                )
                .await;
            }
            _ => {
                serve_final_recovery_connection(&mut socket, &mut capture.creates).await;
            }
        }
    }
    capture
}

async fn serve_first_recovery_connection(
    socket: &mut WebSocketStream<tokio::net::TcpStream>,
    creates: &mut Vec<String>,
    command: &str,
    incomplete_command: &str,
    previous_response_not_found: bool,
) {
    creates.push(receive_websocket_text(socket).await);
    send_started_events(socket, "resp_completed_tool_turn").await;
    send_response_event(
        socket,
        json!({
            "type":"response.completed",
            "response": {
                "id":"resp_completed_tool_turn",
                "status":"completed",
                "output":[shell_call_item("completed-shell-call", command)]
            }
        }),
    )
    .await;
    creates.push(receive_websocket_text(socket).await);
    send_started_events(socket, "resp_incomplete_attempt").await;
    if previous_response_not_found {
        send_response_event(
            socket,
            json!({
                "type":"error",
                "error":{"code":"previous_response_not_found"}
            }),
        )
        .await;
    } else {
        send_response_event(
            socket,
            json!({
                "type":"response.output_item.done",
                "item":shell_call_item("incomplete-shell-call", incomplete_command)
            }),
        )
        .await;
        socket
            .send(Message::Close(None))
            .await
            .expect("close continuation before completion");
    }
}

async fn serve_incomplete_recovery_connection(
    socket: &mut WebSocketStream<tokio::net::TcpStream>,
    creates: &mut Vec<String>,
    incomplete_command: &str,
) {
    creates.push(receive_websocket_text(socket).await);
    send_started_events(socket, "resp_recovery_attempt").await;
    send_response_event(
        socket,
        json!({
            "type":"response.output_item.done",
            "item":shell_call_item("discarded-recovery-call", incomplete_command)
        }),
    )
    .await;
    socket
        .send(Message::Close(None))
        .await
        .expect("close recovery attempt before completion");
}

async fn serve_final_recovery_connection(
    socket: &mut WebSocketStream<tokio::net::TcpStream>,
    creates: &mut Vec<String>,
) {
    creates.push(receive_websocket_text(socket).await);
    send_response_event(socket, json!({"type":"codex.response.metadata"})).await;
    send_response_event(socket, json!({"type":"responsesapi.websocket_timing"})).await;
    send_started_events(socket, "resp_finished").await;
    let final_text = r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Recovered over WebSocket."}}"#;
    send_response_event(
        socket,
        json!({
            "type":"response.completed",
            "response": {
                "id":"resp_final_websocket",
                "status":"completed",
                "output":[{
                    "type":"message",
                    "content":[{"type":"output_text","text":final_text}]
                }]
            }
        }),
    )
    .await;
}

async fn accept_fixture_websocket(
    listener: &tokio::net::TcpListener,
) -> (WebSocketStream<tokio::net::TcpStream>, (String, bool)) {
    let (stream, _) = tokio::time::timeout(WATCHDOG, listener.accept())
        .await
        .expect("WebSocket fixture accept timeout")
        .expect("accept WebSocket fixture connection");
    let handshake = read_websocket_handshake(&stream).await;
    let socket = accept_async(stream)
        .await
        .expect("complete WebSocket fixture handshake");
    (socket, handshake)
}

async fn read_websocket_handshake(stream: &tokio::net::TcpStream) -> (String, bool) {
    let mut bytes = [0_u8; 4096];
    loop {
        let count = tokio::time::timeout(WATCHDOG, stream.peek(&mut bytes))
            .await
            .expect("read WebSocket handshake timeout")
            .expect("peek WebSocket handshake");
        assert_ne!(count, 0, "WebSocket client closed before its handshake");
        let available = bytes.get(..count).expect("bounded handshake byte range");
        if let Some(header_end) = available
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&available[..header_end]);
            let authorization = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("authorization")
                        .then(|| value.trim().to_owned())
                })
                .unwrap_or_default();
            let has_api_key = headers.lines().any(|line| {
                line.split_once(':')
                    .is_some_and(|(name, _)| name.eq_ignore_ascii_case("x-api-key"))
            });
            return (authorization, has_api_key);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn receive_websocket_text(socket: &mut WebSocketStream<tokio::net::TcpStream>) -> String {
    match tokio::time::timeout(WATCHDOG, socket.next())
        .await
        .expect("WebSocket fixture receive timeout")
        .expect("WebSocket fixture closed before create")
        .expect("read WebSocket fixture create")
    {
        Message::Text(text) => text.to_string(),
        _ => panic!("WebSocket fixture expected a text create"),
    }
}

async fn send_started_events(socket: &mut WebSocketStream<tokio::net::TcpStream>, id: &str) {
    send_response_event(
        socket,
        json!({"type":"response.created","response":{"id":id,"status":"in_progress"}}),
    )
    .await;
    send_response_event(
        socket,
        json!({"type":"response.in_progress","response":{"id":id,"status":"in_progress"}}),
    )
    .await;
}

async fn send_response_event(socket: &mut WebSocketStream<tokio::net::TcpStream>, event: Value) {
    socket
        .send(Message::text(event.to_string()))
        .await
        .expect("send WebSocket fixture event");
}
