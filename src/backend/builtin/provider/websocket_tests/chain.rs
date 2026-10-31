use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Instant;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::super::super::test_support::{
    WATCHDOG, builtin_test_profile, read_request_until, write_until,
};
use crate::backend::UsageSnapshot;
use crate::prompts::PromptRole;

struct CompactionCapture {
    creates: Vec<String>,
    compaction_request: String,
    handshake_headers: Vec<(String, bool)>,
}

#[cfg(unix)]
#[test]
fn native_compaction_starts_a_fresh_websocket_response_chain() {
    let temporary = TempDir::new().expect("create WebSocket compaction project");
    let marker = temporary.path().join("compaction-shell-effect");
    let command = format!("printf x > '{}'", marker.display());
    let task = "compact before the next WebSocket generation";
    let retained = json!({
        "type":"message",
        "id":"retained-message",
        "role":"assistant",
        "content":[{"type":"output_text","text":"retained canonical state"}]
    });
    let compact_item = json!({
        "type":"compaction",
        "id":"cmp_ws_001",
        "encrypted_content":"opaque-websocket-compaction-state"
    });
    let compacted_input = json!([retained, compact_item]);
    let (endpoint, server) = websocket_compaction_provider(&command, &compacted_input);
    let mut profile = builtin_test_profile();
    profile.agent.context_compaction = true;
    profile.agent.context_window_tokens = Some(1000);
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = super::super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: task,
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
    let result =
        crate::runtime::block_on(super::super::super::run_role_with_token_provider_observed(
            input.with_test_deadline(),
            None,
            || async { Ok("fixture-access-token".to_owned()) },
            &mut conversation,
            &mut transcript,
            &mut usage,
            &mut observer,
        ))
        .expect("construct WebSocket compaction runtime")
        .expect("finish role after native compaction");

    assert!(
        result
            .response
            .contains("Compacted WebSocket chain completed")
    );
    assert_eq!(
        std::fs::read_to_string(marker).expect("read shell effect"),
        "x"
    );
    assert_eq!(usage.provider_request_attempts, Some(3));
    assert_eq!(usage.completed_responses, Some(2));
    assert_eq!(usage.executed_shell_calls, Some(1));

    let capture = server.join().expect("join WebSocket compaction fixture");
    assert_eq!(capture.creates.len(), 2);
    assert_eq!(capture.handshake_headers.len(), 2);
    let initial: Value = serde_json::from_str(&capture.creates[0]).expect("parse initial create");
    assert_eq!(initial["input"], json!([{"role":"user","content":task}]));
    assert_eq!(initial["store"], false);
    assert!(initial.get("previous_response_id").is_none());
    let compact_request = capture
        .compaction_request
        .split_once("\r\n\r\n")
        .expect("compaction request body");
    assert!(
        capture
            .compaction_request
            .starts_with("POST /v1/responses/compact ")
    );
    let compact_body: Value = serde_json::from_str(compact_request.1).expect("parse compact body");
    assert_eq!(compact_body["input"][0]["content"], task);
    assert_eq!(
        compact_body["input"][1]["call_id"],
        "compact-websocket-call"
    );
    let next: Value = serde_json::from_str(&capture.creates[1]).expect("parse post-compact create");
    assert_eq!(next["store"], false);
    assert!(next.get("previous_response_id").is_none());
    assert_eq!(
        next["input"],
        json!([retained, compact_item, {"role":"user","content":task}])
    );
}

fn websocket_compaction_provider(
    command: &str,
    compacted_input: &Value,
) -> (String, thread::JoinHandle<CompactionCapture>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind WebSocket compaction fixture");
    listener
        .set_nonblocking(true)
        .expect("make WebSocket compaction fixture nonblocking");
    let address = listener
        .local_addr()
        .expect("WebSocket compaction fixture address");
    let command = command.to_owned();
    let compacted_input = compacted_input.clone();
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async WebSocket compaction listener");
            let (mut first_socket, first_handshake) =
                super::accept_fixture_websocket(&listener).await;
            let first_create = super::receive_websocket_text(&mut first_socket).await;
            super::send_started_events(&mut first_socket, "resp_before_compaction").await;
            super::send_response_event(
                &mut first_socket,
                json!({
                    "type":"response.completed",
                    "response":{
                        "id":"resp_before_compaction",
                        "status":"completed",
                        "usage":{"input_tokens":800,"output_tokens":0,"total_tokens":800},
                        "output":[super::shell_call_item("compact-websocket-call", &command)]
                    }
                }),
            )
            .await;
            drop(first_socket);

            let mut compact_stream = super::accept_fixture_http(&listener).await;
            let compaction_request =
                read_request_until(&mut compact_stream, Instant::now() + WATCHDOG)
                    .expect("read native compaction request");
            let compact_response = json!({
                "id":"resp_compaction_ws",
                "created_at":1_731_459_200_u64,
                "object":"response.compaction",
                "output":compacted_input
            });
            let compact_body = compact_response.to_string();
            let compact_reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{compact_body}",
                compact_body.len()
            );
            write_until(
                &mut compact_stream,
                compact_reply.as_bytes(),
                Instant::now() + WATCHDOG,
            )
            .expect("write native compaction response");

            let (mut next_socket, next_handshake) =
                super::accept_fixture_websocket(&listener).await;
            let next_create = super::receive_websocket_text(&mut next_socket).await;
            super::send_started_events(&mut next_socket, "resp_after_compaction").await;
            let final_text = r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Compacted WebSocket chain completed."}}"#;
            super::send_response_event(
                &mut next_socket,
                json!({
                    "type":"response.completed",
                    "response":{
                        "id":"resp_after_compaction",
                        "status":"completed",
                        "output":[{"type":"message","content":[{"type":"output_text","text":final_text}]}]
                    }
                }),
            )
            .await;
            drop(next_socket);
            CompactionCapture {
                creates: vec![first_create, next_create],
                compaction_request,
                handshake_headers: vec![first_handshake, next_handshake],
            }
        })
        .expect("run WebSocket compaction fixture")
    });
    (format!("http://{address}/v1/responses"), server)
}

#[cfg(unix)]
#[test]
fn correction_invocation_uses_fresh_websocket_and_full_local_history() {
    let temporary = TempDir::new().expect("create WebSocket correction project");
    let marker = temporary.path().join("correction-shell-effect");
    let command = format!("printf x > '{}'", marker.display());
    let (endpoint, server) = websocket_correction_provider(&command);
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut transcript = Vec::new();
    let mut first_conversation = None;
    let mut first_usage = UsageSnapshot::default();
    let mut observer = |_: &UsageSnapshot| Ok(());
    let first_input = super::super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: "complete the original implementation task",
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let first =
        crate::runtime::block_on(super::super::super::run_role_with_token_provider_observed(
            first_input.with_test_deadline(),
            None,
            || async { Ok("fixture-access-token".to_owned()) },
            &mut first_conversation,
            &mut transcript,
            &mut first_usage,
            &mut observer,
        ))
        .expect("construct initial WebSocket runtime")
        .expect("receive malformed typed handoff");
    assert_eq!(first.response, r#"{"unexpected":"typed schema failure"}"#);

    let mut corrected_conversation = None;
    let mut correction_usage = UsageSnapshot::default();
    let correction_task = "correct the typed response only";
    let correction_input = super::super::super::RoleInput {
        root: temporary.path(),
        profile: &profile,
        prompt: correction_task,
        role: PromptRole::Implementation,
        cancelled: &cancelled,
        deadline: None,
        endpoint: &endpoint,
        token: "fixture-access-token",
    };
    let corrected =
        crate::runtime::block_on(super::super::super::run_role_with_token_provider_observed(
            correction_input.with_test_deadline(),
            first_conversation,
            || async { Ok("fixture-access-token".to_owned()) },
            &mut corrected_conversation,
            &mut transcript,
            &mut correction_usage,
            &mut observer,
        ))
        .expect("construct correction WebSocket runtime")
        .expect("receive corrected typed handoff");

    assert!(corrected.response.contains("Corrected WebSocket handoff"));
    assert_eq!(
        std::fs::read_to_string(marker).expect("read shell effect"),
        "x"
    );
    assert_correction_usage(&first_usage, &correction_usage);

    let capture = server.join().expect("join WebSocket correction fixture");
    assert_eq!(capture.handshake_headers.len(), 2);
    assert_eq!(capture.creates.len(), 3);
    let first_create: Value =
        serde_json::from_str(&capture.creates[0]).expect("parse initial create");
    assert!(first_create.get("previous_response_id").is_none());
    let in_role_continuation: Value =
        serde_json::from_str(&capture.creates[1]).expect("parse in-role continuation");
    assert_eq!(in_role_continuation["previous_response_id"], "resp_ws_tool");
    assert_eq!(
        in_role_continuation["input"]
            .as_array()
            .expect("increment")
            .len(),
        1
    );
    let correction: Value =
        serde_json::from_str(&capture.creates[2]).expect("parse correction create");
    assert!(correction.get("previous_response_id").is_none());
    assert_eq!(correction["store"], false);
    assert_eq!(correction["tool_choice"], "none");
    assert_eq!(
        correction["input"]
            .as_array()
            .expect("full local history")
            .len(),
        5
    );
    assert_eq!(correction["input"][1]["call_id"], "ws-correction-call");
    assert_eq!(correction["input"][2]["type"], "function_call_output");
    assert!(
        correction["input"][3]
            .to_string()
            .contains("typed schema failure")
    );
    assert_eq!(correction["input"][4]["content"], correction_task);
}

fn assert_correction_usage(first: &UsageSnapshot, correction: &UsageSnapshot) {
    assert_eq!(first.provider_request_attempts, Some(2));
    assert_eq!(first.completed_responses, Some(2));
    assert_eq!(first.executed_shell_calls, Some(1));
    assert_eq!(correction.provider_request_attempts, Some(1));
    assert_eq!(correction.completed_responses, Some(1));
    assert_eq!(correction.executed_shell_calls, Some(0));
}

fn websocket_correction_provider(
    command: &str,
) -> (String, thread::JoinHandle<super::WebSocketCapture>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind WebSocket correction fixture");
    listener
        .set_nonblocking(true)
        .expect("make WebSocket correction fixture nonblocking");
    let address = listener
        .local_addr()
        .expect("WebSocket correction fixture address");
    let command = command.to_owned();
    let server = thread::spawn(move || {
        crate::runtime::block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("construct async WebSocket correction listener");
            let mut capture = super::WebSocketCapture {
                creates: Vec::new(),
                handshake_headers: Vec::new(),
            };
            let (mut first_socket, first_handshake) =
                super::accept_fixture_websocket(&listener).await;
            capture.handshake_headers.push(first_handshake);
            capture
                .creates
                .push(super::receive_websocket_text(&mut first_socket).await);
            super::send_started_events(&mut first_socket, "resp_ws_tool").await;
            super::send_response_event(
                &mut first_socket,
                json!({
                    "type":"response.completed",
                    "response":{
                        "id":"resp_ws_tool",
                        "status":"completed",
                        "output":[super::shell_call_item("ws-correction-call", &command)]
                    }
                }),
            )
            .await;
            capture
                .creates
                .push(super::receive_websocket_text(&mut first_socket).await);
            super::send_started_events(&mut first_socket, "resp_ws_malformed").await;
            super::send_response_event(
                &mut first_socket,
                json!({
                    "type":"response.completed",
                    "response":{
                        "id":"resp_ws_malformed",
                        "status":"completed",
                        "output":[{"type":"message","content":[{"type":"output_text","text":r#"{"response":{"unexpected":"typed schema failure"}}"#}]}]
                    }
                }),
            )
            .await;
            drop(first_socket);

            let (mut correction_socket, correction_handshake) =
                super::accept_fixture_websocket(&listener).await;
            capture.handshake_headers.push(correction_handshake);
            capture
                .creates
                .push(super::receive_websocket_text(&mut correction_socket).await);
            super::send_started_events(&mut correction_socket, "resp_ws_corrected").await;
            let final_text = r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Corrected WebSocket handoff."}}"#;
            super::send_response_event(
                &mut correction_socket,
                json!({
                    "type":"response.completed",
                    "response":{
                        "id":"resp_ws_corrected",
                        "status":"completed",
                        "output":[{"type":"message","content":[{"type":"output_text","text":final_text}]}]
                    }
                }),
            )
            .await;
            capture
        })
        .expect("run WebSocket correction fixture")
    });
    (format!("http://{address}/v1/responses"), server)
}
