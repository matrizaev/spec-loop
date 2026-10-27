use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;

use super::super::tests::{completed_sse, read_http_request};
use super::super::{RoleInput, classify_agent_run, run_role_with_access_token};
use super::{
    completed_output, parse_output_items, render_captured_streams, run_shell, shell_tool_definition,
};
use crate::backend::BackendFailureKind;
use crate::config::Profile;
use crate::prompts::PromptRole;

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
    use super::super::role_deadline;

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
    assert_eq!(
        result.termination,
        crate::commands::ProcessTermination::Interrupted
    );
    let mut transcript = Vec::new();
    super::append_transcript(&mut transcript, command, &result.log_output)
        .expect("append interrupted shell transcript");
    let log = String::from_utf8(transcript).expect("shell transcript is UTF-8");
    assert!(log.contains("$ printf interrupted-output; sleep 30"));
    assert!(log.contains("termination=interrupted"));
    assert!(log.contains("stdout:\ninterrupted-output"));
}
