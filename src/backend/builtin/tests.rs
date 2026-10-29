use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::test_support::{
    ProviderServer, RESPONSE_STARTED, Reply, builtin_test_profile, completed_sse, serve_responses,
    sse_event,
};
use super::{
    RoleInput, classify_agent_run, run_after_shell_preflight, run_role_with_access_token,
    run_role_with_auth,
};
use crate::backend::BackendFailureKind;
use crate::config::{AgentStage, Profile};
use crate::prompts::PromptRole;

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
        input.with_test_deadline(),
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

fn start_one_response_provider(response: &Value) -> (String, ProviderServer) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let server = serve_responses(listener, [Reply::sse(completed_sse(response))]);
    (format!("http://{address}/v1/responses"), server)
}

fn provider_request_payload(request: &str) -> Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("provider request body");
    serde_json::from_str(body).expect("parse provider request body")
}

fn assert_strict_role_format(request: &str, role: PromptRole) {
    assert!(
        request
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert!(
        request.contains(r#""properties":{"status":"#),
        "choose status before payload on actual provider wire"
    );
    if matches!(role, PromptRole::PlanAudit | PromptRole::ReviewAudit) {
        assert_eq!(
            request.matches(r#""properties":{"outcome":"#).count(),
            3,
            "choose nested outcome before evidence"
        );
    }
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
    assert!(report.contains("∑ Observed completed-response usage\n  60 in · 8 out · 68 total"));
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
    let server = serve_responses(listener, responses.into_iter().map(Reply::sse));
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
    let server = serve_responses(listener, [Reply::sse(body)]);
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
            .first()
            .expect("provider request")
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
        RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "original task prompt",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &endpoint,
            token: "",
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
fn builtin_system_instruction_hierarchy_is_shared_by_every_role() {
    let profile = builtin_test_profile();
    let roles = [
        PromptRole::PlanAuthor,
        PromptRole::PlanReview,
        PromptRole::PlanAudit,
        PromptRole::PlanClarification,
        PromptRole::Implementation,
        PromptRole::Review,
        PromptRole::ReviewAudit,
        PromptRole::ReviewClarification,
    ];
    let conversation = super::BuiltinConversation {
        input: Vec::new(),
        current_task: String::new(),
        latest_generation_usage: None,
        tool_calls: None,
    };
    for role in roles {
        let request =
            super::build_responses_request(&profile, role, "fixture-model", &conversation, false)
                .expect("build role request");
        assert_eq!(request["instructions"], super::SYSTEM_INSTRUCTIONS);
        assert_eq!(request["text"]["format"]["name"], role.protocol_name());
    }

    let instructions = super::SYSTEM_INSTRUCTIONS;
    for required in [
        "Before inspecting workspace file contents or modifying a file",
        "from the workspace root through that file's parent directory, root to leaf",
        "reading each full instruction or reusing full contents already available in current context",
        "Path-only listings and searches",
        "For a file not yet created, use its intended parent",
        "sharing already checked ancestors",
        "sibling or unrelated directories",
        "Deeper applicable instructions override conflicting ancestor rules only within their own scope",
        "retain nonconflicting ancestor rules",
        "Configured role-instruction paths remain required wherever configured",
        "are not necessarily the complete hierarchy",
        "does not expand `AGENTS.md` scope, change precedence",
        "check each directory segment once and remember checked directories even when no `AGENTS.md` exists",
        "read each applicable instruction file once",
        "a path or summary alone is not full contents",
        "check only previously unchecked segments",
        "created, changed, moved, or deleted",
        "when full contents are no longer available",
        "A fresh role invocation establishes its own hierarchy",
        "Permission errors, failed discovery commands, and incomplete or truncated output do not establish absence or complete reading",
        "stop affected work and report the path and failure through the role's existing contract",
        "Report unresolved repository/configured-instruction conflicts",
        "Locate relevant paths, establish their instructions, then use scoped content searches and targeted file ranges",
        "batch independent bounded read-only operations only after instruction discovery",
        "preserve each operation's diagnostics",
        "Prefer quiet targeted tests during iteration without replacing required verification",
        "never replay a mutation solely to recover omitted output",
        "not hard inspection caps or a requirement for any particular search utility",
        "PlanAuthor may write only the selected plan",
        "review, audit, and clarification roles are read-only",
    ] {
        assert!(
            instructions.contains(required),
            "missing guidance: {required}"
        );
    }
    assert!(!instructions.contains("automatically discover"));
    assert!(!instructions.contains("new tool"));
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

    let mut roles = vec![
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
    let adjudications: Vec<Value> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/blocker-adjudications.json"
    ))
    .expect("typed adjudication fixtures");
    for role in [PromptRole::PlanAudit, PromptRole::ReviewAudit] {
        for adjudication in &adjudications {
            roles.push((
                role,
                json!({
                    "protocol_version":1, "role":role.protocol_name(), "status":"adjudicated",
                    "adjudication":adjudication,
                }),
            ));
        }
    }
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
    let server = serve_responses(
        listener,
        expected_requests
            .iter()
            .map(|(_, response)| Reply::sse(completed_sse(response))),
    );
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
        assert_strict_role_format(request, role);
        let (_, payload) = request
            .split_once("\r\n\r\n")
            .expect("provider request body");
        let payload: Value = serde_json::from_str(payload).expect("parse request body");
        assert_eq!(payload["input"][0]["content"], expected_prompt);
        assert_eq!(payload["instructions"], super::SYSTEM_INSTRUCTIONS);
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
    let server = serve_responses(listener, [Reply::sse(body)]);
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

    let requests = server.join().expect("join provider fixture");
    assert!(
        requests
            .first()
            .expect("request")
            .starts_with("POST /v1/responses ")
    );
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
    let server = serve_responses(listener, bodies.into_iter().map(Reply::sse));
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
    let server = serve_responses(listener, bodies.into_iter().map(Reply::sse));
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
    let server = serve_responses(
        listener,
        token_refresh_responses().into_iter().map(Reply::sse),
    );
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
        RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "request a command, then finish",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &endpoint,
            token: "",
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
    let server = serve_responses(listener, [Reply::sse(first_body)]);
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
        RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "request a command, then finish",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &endpoint,
            token: "",
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
            .first()
            .expect("request")
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
    let server = serve_responses(
        listener,
        [
            Reply::sse(completed_sse(&tool_response)),
            Reply::http("429 Too Many Requests", String::new()),
            Reply::sse(completed_sse(&final_response)),
        ],
    );
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
fn start_malformed_typed_response_provider(command: &str) -> (String, ProviderServer) {
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
    let server = serve_responses(listener, provider_responses.into_iter().map(Reply::sse));
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
    assert!(report.contains("∑ Observed completed-response usage\n  33 in · 9 out · 42 total"));
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

fn compaction_profile(context_limit: Option<u64>, stage_model_override: bool) -> Profile {
    let context_limit = context_limit.map_or_else(String::new, |limit| {
        format!("context_window_tokens={limit}\n")
    });
    let stage_override = if stage_model_override {
        "[agent.stages.implementation]\nmodel='stage-model'\n"
    } else {
        ""
    };
    let profile: Profile = toml::from_str(&format!(
        "schema_version=3\n[specs]\nactive_globs=['specs/*.md']\nid_regex='^specs/(?P<id>[0-9]+)\\\\.md$'\n[agent]\nbackend='builtin'\nmodel='fixture-model'\ncontext_compaction=true\n{context_limit}max_tool_calls=2\n{stage_override}"
    ))
    .expect("parse compaction test profile");
    profile
        .validate()
        .expect("validate compaction test profile");
    profile
}

fn compact_response(output: &Value) -> Value {
    json!({
        "id": "resp_001",
        "created_at": 1_731_459_200,
        "object": "response.compaction",
        "output": output,
        "usage": {"input_tokens": 42, "output_tokens": 10, "total_tokens": 52}
    })
}

fn shell_turn(command: &str, input_tokens: u64) -> Value {
    response_with_usage(
        &json!([{
            "type": "function_call",
            "call_id": "compact-tool-once",
            "name": "spec_loop.shell",
            "arguments": serde_json::to_string(&json!({"command": command}))
                .expect("serialize compaction shell command")
        }]),
        input_tokens,
        0,
    )
}

fn final_implementation_response() -> Value {
    response_with_usage(
        &json!([{
            "type": "message",
            "content": [{
                "type": "output_text",
                "text": r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Compacted continuation completed."}}"#
            }]
        }]),
        4,
        1,
    )
}

fn assert_compaction_wire(requests: &[String], task: &str, retained: &Value, compact_item: &Value) {
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("POST /v1/responses "));
    assert!(requests[1].starts_with("POST /v1/responses/compact "));
    assert!(requests[2].starts_with("POST /v1/responses "));
    assert!(requests.iter().all(|request| {
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-access-token")
            && !request.to_ascii_lowercase().contains("x-api-key")
    }));
    let compact_request = provider_request_payload(&requests[1]);
    assert_eq!(compact_request["model"], "fixture-model");
    assert_eq!(compact_request["instructions"], super::SYSTEM_INSTRUCTIONS);
    let original_input = compact_request["input"].as_array().expect("full input");
    assert_eq!(original_input.len(), 3);
    assert_eq!(original_input[0]["content"], task);
    assert_eq!(original_input[1]["type"], "function_call");
    assert_eq!(original_input[1]["call_id"], "compact-tool-once");
    assert_eq!(original_input[2]["type"], "function_call_output");
    assert!(
        original_input[2]["output"]
            .as_str()
            .is_some_and(|text| text.contains("exit_status=0"))
    );
    let next_generation = provider_request_payload(&requests[2]);
    assert_eq!(
        next_generation["input"],
        json!([retained, compact_item, {"role":"user","content":task}])
    );
    assert_eq!(next_generation["model"], "fixture-model");
    assert_eq!(next_generation["instructions"], super::SYSTEM_INSTRUCTIONS);
    assert_eq!(
        next_generation["tools"][0],
        super::shell::shell_tool_definition()
    );
    assert_eq!(next_generation["tool_choice"], "auto");
    assert_eq!(next_generation["text"]["format"]["type"], "json_schema");
    assert_eq!(next_generation["text"]["format"]["name"], "implementation");
    assert_eq!(next_generation["text"]["format"]["strict"], true);
}

#[cfg(unix)]
#[test]
fn native_compaction_runs_after_tool_results_and_reuses_canonical_typed_state() {
    let temporary = TempDir::new().expect("create compact project");
    let marker = temporary.path().join("compacted-side-effect");
    let command = format!("printf x >> '{}'", marker.display());
    let retained = json!({
        "type": "message",
        "id": "retained-message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "retained provider item"}]
    });
    let compact_item = json!({
        "type": "compaction",
        "id": "cmp_001",
        "encrypted_content": "opaque-native-state"
    });
    let canonical_output = json!([retained, compact_item]);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind compaction provider");
    let address = listener.local_addr().expect("compaction address");
    let server = serve_responses(
        listener,
        [
            Reply::sse(completed_sse(&shell_turn(&command, 800))),
            Reply::http("200 OK", compact_response(&canonical_output).to_string()),
            Reply::sse(completed_sse(&final_implementation_response())),
        ],
    );
    let profile = compaction_profile(Some(1000), false);
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut transcript = Vec::new();
    let mut completed_conversation = None;
    let mut usage = crate::backend::UsageSnapshot::default();
    let task = "execute once, then continue with native compaction";
    let result = crate::runtime::block_on(run_role_with_usage(
        RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: task,
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &format!("http://{address}/v1/responses"),
            token: "fixture-access-token",
        },
        None,
        &mut completed_conversation,
        &mut transcript,
        &mut usage,
    ))
    .expect("construct native compaction runtime")
    .expect("finish after compaction");
    assert!(result.response.contains("Compacted continuation completed"));
    assert_eq!(
        std::fs::read_to_string(&marker).expect("read shell effect"),
        "x"
    );
    assert_eq!(usage.provider_request_attempts, Some(3));
    assert_eq!(usage.completed_responses, Some(2));
    assert_eq!(usage.executed_shell_calls, Some(1));
    assert_eq!(
        usage.latest_response.as_ref().and_then(|item| item.total),
        Some(5)
    );
    assert_eq!(
        completed_conversation
            .as_ref()
            .and_then(|conversation| conversation.tool_calls),
        Some(1),
        "compaction must not reset per-role shell count"
    );
    assert_eq!(
        String::from_utf8_lossy(&transcript)
            .matches("$ printf")
            .count(),
        1
    );

    let requests = server.join().expect("join compaction provider");
    assert_compaction_wire(&requests, task, &retained, &compact_item);
    let report = usage.format_report("implementation", "fixture-model", 0, Some(1000));
    assert!(report.contains("∑ Native compaction usage\n  42 in · 10 out · 52 total"));
}

#[cfg(unix)]
#[test]
fn native_compaction_skips_below_threshold_missing_usage_and_unapplicable_limit() {
    for (label, limit, stage_override, usage_tokens) in [
        ("below", Some(1000), false, Some(799)),
        ("missing-usage", Some(1000), false, None),
        ("stage-limit-unknown", Some(1000), true, Some(900)),
    ] {
        let temporary = TempDir::new().expect("create no-compaction project");
        let marker = temporary.path().join(label);
        let command = format!("printf x >> '{}'", marker.display());
        let first = usage_tokens.map_or_else(
            || {
                json!({
                    "type": "response.completed",
                    "response": {
                        "status": "completed",
                        "output": shell_turn(&command, 1)["response"]["output"].clone()
                    }
                })
            },
            |tokens| shell_turn(&command, tokens),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind no-compaction provider");
        let address = listener.local_addr().expect("no-compaction address");
        let server = serve_responses(
            listener,
            [
                Reply::sse(completed_sse(&first)),
                Reply::sse(completed_sse(&final_implementation_response())),
            ],
        );
        let profile = compaction_profile(limit, stage_override);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut transcript = Vec::new();
        let mut conversation = None;
        let mut usage = crate::backend::UsageSnapshot::default();
        let result = crate::runtime::block_on(run_role_with_usage(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "one tool turn must not be replayed",
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
        .expect("construct no-compaction runtime")
        .expect("complete without compaction");
        assert!(result.response.contains("Compacted continuation completed"));
        assert_eq!(
            std::fs::read_to_string(marker).expect("read shell effect"),
            "x"
        );
        assert_eq!(usage.provider_request_attempts, Some(2));
        assert_eq!(usage.completed_responses, Some(2));
        assert_eq!(usage.executed_shell_calls, Some(1));
        let requests = server.join().expect("join no-compaction provider");
        assert_eq!(requests.len(), 2, "{label} must skip compact endpoint");
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with("POST /v1/responses "))
        );
    }
}

#[test]
fn compaction_response_validation_rejects_incomplete_native_state() {
    let valid = compact_response(&json!([
        {"type":"compaction","encrypted_content":"opaque"}
    ]));
    assert_eq!(
        super::validate_compaction_response(&valid)
            .expect("valid state")
            .len(),
        1
    );
    for item in [
        json!({"type":"compaction","encrypted_content":"opaque"}),
        json!({"type":"compaction","id":null,"encrypted_content":"opaque"}),
    ] {
        let response = compact_response(&json!([item]));
        assert_eq!(
            super::validate_compaction_response(&response)
                .expect("optional native item ID is valid")
                .len(),
            1
        );
    }
    for response in [
        json!({"created_at":1,"object":"response.compaction","output":[{"type":"compaction","encrypted_content":"x"}]}),
        json!({"id":"r","created_at":"1","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"x"}]}),
        json!({"id":"r","created_at":1,"object":"wrong","output":[{"type":"compaction","encrypted_content":"x"}]}),
        json!({"id":"r","created_at":1,"object":"response.compaction"}),
        json!({"id":"r","created_at":1,"object":"response.compaction","output":[]}),
        json!({"id":"r","created_at":1,"object":"response.compaction","output":[{}]}),
        json!({"id":"r","created_at":1,"object":"response.compaction","output":[{"type":"message"}]}),
        json!({"id":"r","created_at":1,"object":"response.compaction","output":[{"type":"compaction","encrypted_content":1}]}),
        json!({"id":"r","created_at":1,"object":"response.compaction","output":[{"type":"compaction","encrypted_content":"x","id":1}]}),
    ] {
        assert!(
            super::validate_compaction_response(&response).is_err(),
            "{response}"
        );
    }
}

#[cfg(unix)]
#[test]
fn failed_native_compaction_stops_after_effect_without_retry_or_state_replacement() {
    let temporary = TempDir::new().expect("create compact failure project");
    let marker = temporary.path().join("completed-effect");
    let command = format!("printf x > '{}'", marker.display());
    for compact_reply in [
        Reply::http(
            "404 Not Found",
            "unsupported-provider-detail-secret".to_owned(),
        ),
        Reply::http(
            "200 OK",
            json!({
                "id":"resp_001",
                "created_at":1_731_459_200,
                "object":"response.compaction",
                "output":[{"type":"message","content":[]}]
            })
            .to_string(),
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind failed compact provider");
        let address = listener.local_addr().expect("failed compact address");
        let server = serve_responses(
            listener,
            [
                Reply::sse(completed_sse(&shell_turn(&command, 800))),
                compact_reply,
            ],
        );
        let profile = compaction_profile(Some(1000), false);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut transcript = Vec::new();
        let mut completed_conversation = None;
        let mut usage = crate::backend::UsageSnapshot::default();
        let error = crate::runtime::block_on(run_role_with_usage(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "effect must not repeat after compaction fails",
                role: PromptRole::Implementation,
                cancelled: &cancelled,
                deadline: None,
                endpoint: &format!("http://{address}/v1/responses"),
                token: "fixture-access-token",
            },
            None,
            &mut completed_conversation,
            &mut transcript,
            &mut usage,
        ))
        .expect("construct failed compaction runtime")
        .expect_err("failed compaction stops role");
        assert!(error.to_string().contains("compaction"));
        assert!(
            !error
                .to_string()
                .contains("unsupported-provider-detail-secret")
        );
        assert!(!error.to_string().contains("opaque"));
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read completed effect"),
            "x"
        );
        assert!(
            completed_conversation.is_none(),
            "invalid state is never handed off"
        );
        assert_eq!(usage.provider_request_attempts, Some(2));
        assert_eq!(usage.completed_responses, Some(1));
        assert_eq!(usage.executed_shell_calls, Some(1));
        assert_eq!(
            String::from_utf8_lossy(&transcript)
                .matches("$ printf")
                .count(),
            1
        );
        let requests = server.join().expect("join failed compaction provider");
        assert_eq!(
            requests.len(),
            2,
            "must not generate or retry after compact failure"
        );
        assert!(requests[0].starts_with("POST /v1/responses "));
        assert!(requests[1].starts_with("POST /v1/responses/compact "));
    }
}

#[test]
fn correction_continuation_compacts_invocation_local_native_state_without_tool_replay() {
    let initial_text = response_with_usage(
        &json!([{
            "type": "message",
            "content": [{"type": "output_text", "text": r#"{"response":{"unexpected":"typed schema failure"}}"#}]
        }]),
        800,
        0,
    );
    let compacted_output = json!([
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"canonical retained"}]},
        {"type":"compaction","encrypted_content":"opaque-correction-state"}
    ]);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind correction compaction provider");
    let address = listener
        .local_addr()
        .expect("correction compaction address");
    let server = serve_responses(
        listener,
        [
            Reply::sse(completed_sse(&initial_text)),
            Reply::http("200 OK", compact_response(&compacted_output).to_string()),
            Reply::sse(completed_sse(&final_implementation_response())),
        ],
    );
    let temporary = TempDir::new().expect("create correction project");
    let profile = compaction_profile(Some(1000), false);
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut transcript = Vec::new();
    let mut conversation = None;
    let mut initial_usage = crate::backend::UsageSnapshot::default();
    let first = crate::runtime::block_on(run_role_with_usage(
        RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "original role task",
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &format!("http://{address}/v1/responses"),
            token: "fixture-access-token",
        },
        None,
        &mut conversation,
        &mut transcript,
        &mut initial_usage,
    ))
    .expect("construct initial role runtime")
    .expect("receive malformed typed handoff for correction");
    assert!(first.response.contains("typed schema failure"));
    let mut correction_usage = crate::backend::UsageSnapshot::default();
    let correction_task = "correct the typed handoff only";
    let corrected = crate::runtime::block_on(run_role_with_usage(
        RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: correction_task,
            role: PromptRole::Implementation,
            cancelled: &cancelled,
            deadline: None,
            endpoint: &format!("http://{address}/v1/responses"),
            token: "fixture-access-token",
        },
        conversation,
        &mut None,
        &mut transcript,
        &mut correction_usage,
    ))
    .expect("construct correction runtime")
    .expect("correct typed response after native compaction");
    assert!(
        corrected
            .response
            .contains("Compacted continuation completed")
    );
    assert_eq!(initial_usage.provider_request_attempts, Some(1));
    assert_eq!(correction_usage.provider_request_attempts, Some(2));
    assert_eq!(correction_usage.completed_responses, Some(1));
    assert_eq!(correction_usage.executed_shell_calls, Some(0));
    let requests = server.join().expect("join correction compaction provider");
    assert_eq!(requests.len(), 3);
    let compact_request = provider_request_payload(&requests[1]);
    assert_eq!(compact_request["input"][0]["content"], "original role task");
    assert!(
        compact_request["input"]
            .to_string()
            .contains("typed schema failure")
    );
    assert_eq!(compact_request["input"][2]["content"], correction_task);
    let next = provider_request_payload(&requests[2]);
    let mut expected_input = compacted_output
        .as_array()
        .expect("canonical array")
        .clone();
    expected_input.push(json!({"role":"user","content":correction_task}));
    assert_eq!(next["input"], json!(expected_input));
    assert_eq!(next["tool_choice"], "none");
}

#[test]
fn compaction_is_not_attempted_when_disabled_or_after_final_generation() {
    for (enabled, has_followup) in [(false, true), (true, false)] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind no-next-generation provider");
        let address = listener.local_addr().expect("no-next-generation address");
        let final_response = response_with_usage(
            &json!([{
                "type": "message",
                "content": [{
                    "type": "output_text",
                    "text": r#"{"response":{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Final response."}}"#
                }]
            }]),
            900,
            0,
        );
        let replies = if has_followup {
            vec![
                Reply::sse(completed_sse(&shell_turn("true", 900))),
                Reply::sse(completed_sse(&final_implementation_response())),
            ]
        } else {
            vec![Reply::sse(completed_sse(&final_response))]
        };
        let server = serve_responses(listener, replies);
        let profile = if enabled {
            compaction_profile(Some(1000), false)
        } else {
            let mut profile = compaction_profile(Some(1000), false);
            profile.agent.context_compaction = false;
            profile
        };
        let temporary = TempDir::new().expect("create no-compaction project");
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut transcript = Vec::new();
        let mut conversation = None;
        let mut usage = crate::backend::UsageSnapshot::default();
        crate::runtime::block_on(run_role_with_usage(
            RoleInput {
                root: temporary.path(),
                profile: &profile,
                prompt: "no compaction after final or when disabled",
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
        .expect("construct no-compaction runtime")
        .expect("complete without compaction");
        let requests = server.join().expect("join no-compaction provider");
        assert_eq!(requests.len(), if has_followup { 2 } else { 1 });
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with("POST /v1/responses "))
        );
        assert_eq!(
            usage.provider_request_attempts,
            Some(if has_followup { 2 } else { 1 })
        );
    }
}
