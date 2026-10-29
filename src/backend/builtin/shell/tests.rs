use std::fs;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;

use super::super::test_support::{
    Reply, WATCHDOG, accept_until, completed_sse, read_request_until, serve_responses, write_until,
};
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
    let (_, streams) = result
        .tool_output
        .split_once("stdout:\n")
        .expect("stdout payload");
    let (stdout, stderr) = streams.split_once("\nstderr:\n").expect("stderr payload");
    let (stderr, _) = stderr
        .split_once("\n[tool output truncated at configured byte limit]")
        .expect("truncation marker");
    assert!(stdout.len() + stderr.len() <= 6);
    assert!(result.tool_output.contains("tool output truncated"));
    assert!(result.tool_output.contains("command exited unsuccessfully"));

    let exact = run_shell(
        temporary.path(),
        "printf 123456",
        6,
        None,
        &Arc::new(AtomicBool::new(false)),
    )
    .expect("run exactly fitting shell tool");
    assert!(exact.tool_output.contains("stdout:\n123456\nstderr:\n"));
    assert!(!exact.tool_output.contains("tool output truncated"));
}

#[test]
fn lossy_stream_rendering_respects_combined_output_byte_limit() {
    let invalid_stdout = [0xff; 3];
    let invalid_stderr = [0xfe; 3];

    let (stdout, stderr, stdout_truncated, stderr_truncated) =
        render_captured_streams(&invalid_stdout, &invalid_stderr, 6);

    assert_eq!(stdout.len() + stderr.len(), 6);
    assert!(stdout.is_char_boundary(stdout.len()));
    assert!(stderr.is_char_boundary(stderr.len()));
    assert_eq!(stdout, "��");
    assert_eq!(stderr, "");
    assert!(stdout_truncated);
    assert!(stderr_truncated);
}

#[test]
fn rendering_prioritizes_stdout_and_preserves_utf8_prefixes() {
    for (stdout_input, stderr_input, limit, expected_stdout, expected_stderr) in [
        ("", "", 0, "", ""),
        ("out", "err", 0, "", ""),
        ("out", "err", 5, "out", "er"),
        ("out", "err", 6, "out", "err"),
        ("🦀x", "err", 3, "", "err"),
        ("🦀x", "err", 4, "🦀", ""),
        ("x", "🦀z", 4, "x", ""),
        ("x", "🦀z", 5, "x", "🦀"),
    ] {
        let (stdout, stderr, stdout_truncated, stderr_truncated) =
            render_captured_streams(stdout_input.as_bytes(), stderr_input.as_bytes(), limit);
        assert_eq!(stdout, expected_stdout, "stdout for limit {limit}");
        assert_eq!(stderr, expected_stderr, "stderr for limit {limit}");
        assert!(stdout.len() + stderr.len() <= limit);
        assert_eq!(stdout_truncated, stdout != stdout_input);
        assert_eq!(stderr_truncated, stderr != stderr_input);
    }

    let (stdout, stderr, stdout_truncated, stderr_truncated) =
        render_captured_streams(b"x", &[0xff, 0xfe], 5);
    assert_eq!(stdout, "x");
    assert_eq!(stderr, "�");
    assert!(!stdout_truncated);
    assert!(stderr_truncated);
}

#[test]
fn rendering_uses_full_capacity_and_marks_only_omitted_payload() {
    for stderr_only in [false, true] {
        for (input_length, expected_truncated) in [(5, false), (6, false), (7, true)] {
            let input = vec![b'x'; input_length];
            let (stdout, stderr, stdout_truncated, stderr_truncated) = if stderr_only {
                render_captured_streams(&[], &input, 6)
            } else {
                render_captured_streams(&input, &[], 6)
            };
            let rendered = if stderr_only { &stderr } else { &stdout };
            assert_eq!(rendered.len(), input_length.min(6));
            assert_eq!(stdout.len() + stderr.len(), input_length.min(6));
            assert_eq!(stdout_truncated, !stderr_only && expected_truncated);
            assert_eq!(stderr_truncated, stderr_only && expected_truncated);
        }
    }

    for (stdout_length, stderr_length) in [(2, 3), (3, 3), (4, 4)] {
        let stdout_input = vec![b'o'; stdout_length];
        let stderr_input = vec![b'e'; stderr_length];
        let (stdout, stderr, stdout_truncated, stderr_truncated) =
            render_captured_streams(&stdout_input, &stderr_input, 6);
        assert!(stdout.len() + stderr.len() <= 6);
        if stdout_length + stderr_length <= 6 {
            assert_eq!(stdout.len(), stdout_length);
            assert_eq!(stderr.len(), stderr_length);
            assert!(!stdout_truncated);
            assert!(!stderr_truncated);
        } else {
            assert_eq!(stdout_truncated, stdout.len() < stdout_length);
            assert_eq!(stderr_truncated, stderr.len() < stderr_length);
        }
    }
}

#[test]
fn rendering_retains_800_kib_from_either_single_stream() {
    let payload = vec![0_u8; 819_200];
    for stderr_only in [false, true] {
        let (stdout, stderr, stdout_truncated, stderr_truncated) = if stderr_only {
            render_captured_streams(&[], &payload, 1_048_576)
        } else {
            render_captured_streams(&payload, &[], 1_048_576)
        };
        assert_eq!(stdout.len(), if stderr_only { 0 } else { 819_200 });
        assert_eq!(stderr.len(), if stderr_only { 819_200 } else { 0 });
        assert!(!stdout_truncated);
        assert!(!stderr_truncated);
    }
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

        let deadline = Instant::now() + WATCHDOG;
        let mut stream = accept_until(&listener, deadline).expect("accept provider request");
        let request = read_request_until(&mut stream, deadline).expect("read provider request");
        thread::sleep(Duration::from_millis(650));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        write_until(&mut stream, response.as_bytes(), deadline).expect("write provider response");
        listener
            .set_nonblocking(true)
            .expect("make provider listener nonblocking");
        let observation_deadline = Instant::now() + Duration::from_millis(200);
        let mut extra_requests = 0_usize;
        while Instant::now() < observation_deadline {
            match listener.accept() {
                Ok((mut extra, _)) => {
                    extra_requests += 1;
                    read_request_until(&mut extra, observation_deadline)
                        .expect("read unexpected fixture request");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("inspect fixture listener: {error}"),
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
    let server = serve_responses(listener, [Reply::sse(body)]);
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
    assert_eq!(server.join().expect("join provider fixture").len(), 1);
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

#[cfg(unix)]
#[test]
fn shell_usage_counts_each_successfully_spawned_call() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::super::BuiltinConversation;
    use super::{ShellCall, ShellExecutionContext, execute_shell_calls};
    use crate::backend::UsageSnapshot;
    use crate::backend::builtin::test_support::builtin_test_profile;
    use crate::backend::builtin::usage::UsageReporter;

    let temporary = TempDir::new().expect("create shell usage fixture");
    let profile = builtin_test_profile();
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut conversation = BuiltinConversation {
        input: Vec::new(),
        tool_calls: None,
    };
    let calls = vec![
        ShellCall {
            call_id: "first".to_owned(),
            command: "printf first > first-call".to_owned(),
        },
        ShellCall {
            call_id: "second".to_owned(),
            command: "printf second > second-call".to_owned(),
        },
    ];
    let mut transcript = Vec::new();
    let mut usage = UsageSnapshot::default();
    let mut live = Vec::new();
    {
        let mut observer = |snapshot: &UsageSnapshot| {
            live.push(snapshot.executed_shell_calls);
            Ok(())
        };
        let mut reporter = UsageReporter::new(&mut usage, &mut observer);
        execute_shell_calls(
            ShellExecutionContext {
                root: temporary.path(),
                profile: &profile,
                deadline: None,
                cancelled: &cancelled,
                conversation: &mut conversation,
                transcript: &mut transcript,
            },
            calls,
            &mut reporter,
        )
        .expect("execute observed shell calls");
    }
    assert_eq!(usage.executed_shell_calls, Some(2));
    assert_eq!(live, [Some(1), Some(2)]);
    assert_eq!(
        fs::read_to_string(temporary.path().join("first-call")).expect("read first"),
        "first"
    );
    assert_eq!(
        fs::read_to_string(temporary.path().join("second-call")).expect("read second"),
        "second"
    );
}
