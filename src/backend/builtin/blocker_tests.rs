use std::fs;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::test_support::{Reply, builtin_test_profile, completed_sse, serve_responses};
use super::{RoleInput, run_role_with_continuation};
use crate::prompts::PromptRole;

fn role_response(role: PromptRole, blocked: bool) -> Value {
    let mut response = json!({"protocol_version":1,"role":role.protocol_name()});
    if blocked {
        response["status"] = json!("blocked");
        response["reason"] = json!("No actionable gaps and no genuine external blocker.");
    } else {
        match role {
            PromptRole::PlanAuthor | PromptRole::Implementation => {
                response["status"] = json!("ready_for_review");
            }
            PromptRole::PlanReview | PromptRole::Review => {
                response["status"] = json!("no_issues");
                response["findings"] = json!([]);
            }
            PromptRole::PlanClarification | PromptRole::ReviewClarification => {
                response["status"] = json!("complete");
                response["evidence"] = json!([]);
            }
            _ => panic!("non-auditor fixture only"),
        }
    }
    if role == PromptRole::Implementation {
        response["summary"] = json!("Fixture tool work preserved.");
    }
    response
}

fn completed_reply(output: &Value) -> Reply {
    Reply::sse(completed_sse(
        &json!({"type":"response.completed","response":{"status":"completed","output":output}}),
    ))
}

fn handoff(role: PromptRole, blocked: bool) -> Reply {
    completed_reply(
        &json!([{"type":"message","content":[{"type":"output_text","text":json!({"response":role_response(role,blocked)}).to_string()}]}]),
    )
}

fn tool(call_id: &str, command: &str) -> Reply {
    completed_reply(
        &json!([{"type":"function_call","call_id":call_id,"name":"spec_loop.shell","arguments":json!({"command":command}).to_string()}]),
    )
}

#[cfg(unix)]
#[test]
fn fresh_role_invocations_keep_tools_after_valid_blockers_for_all_non_auditors() {
    for role in [
        PromptRole::PlanAuthor,
        PromptRole::PlanReview,
        PromptRole::PlanClarification,
        PromptRole::Implementation,
        PromptRole::Review,
        PromptRole::ReviewClarification,
    ] {
        let temporary = TempDir::new().expect("non-Git role fixture");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider");
        let endpoint = format!(
            "http://{}/v1/responses",
            listener.local_addr().expect("address")
        );
        let server = serve_responses(
            listener,
            [
                tool("completed", "printf x >> completed-effect"),
                handoff(role, true),
                tool("unfinished", "printf y >> unfinished-effect"),
                handoff(role, false),
            ],
        );
        let profile = builtin_test_profile();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut previous_conversation = None;
        let mut transcript = Vec::new();
        let mut input = RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "original role-native task",
            role,
            cancelled: &cancelled,
            deadline: Some(server.deadline()),
            endpoint: &endpoint,
            token: "fixture-access-token",
        };
        let blocked = crate::runtime::block_on(run_role_with_continuation(
            input,
            None,
            &mut previous_conversation,
            &mut transcript,
        ))
        .expect("runtime")
        .expect("blocked handoff");
        assert_eq!(
            serde_json::from_str::<Value>(&blocked.response).expect("canonical response"),
            role_response(role, true)
        );
        assert!(
            previous_conversation.is_some(),
            "backend offers correction-only state"
        );
        input = RoleInput {
            root: temporary.path(),
            profile: &profile,
            prompt: "original role-native task; blocker_audit unsupported; finish required work without replay",
            role,
            cancelled: &cancelled,
            deadline: Some(server.deadline()),
            endpoint: &endpoint,
            token: "fixture-access-token",
        };
        // Workflow deliberately does not pass correction-only state after a valid unsupported claim.
        let corrected = crate::runtime::block_on(run_role_with_continuation(
            input,
            None,
            &mut None,
            &mut transcript,
        ))
        .expect("runtime")
        .expect("tools remain enabled");
        assert_eq!(
            serde_json::from_str::<Value>(&corrected.response).expect("canonical response"),
            role_response(role, false)
        );
        assert_eq!(
            fs::read_to_string(temporary.path().join("completed-effect")).expect("completed work"),
            "x"
        );
        assert_eq!(
            fs::read_to_string(temporary.path().join("unfinished-effect"))
                .expect("unfinished work"),
            "y"
        );
        let requests = server.join().expect("join provider");
        assert_eq!(requests.len(), 4);
        for request in requests {
            let (_, body) = request.split_once("\r\n\r\n").expect("request body");
            let payload: Value = serde_json::from_str(body).expect("provider payload");
            assert_eq!(payload["tool_choice"], "auto");
            assert_eq!(payload["text"]["format"]["name"], role.protocol_name());
            assert_eq!(payload["text"]["format"]["strict"], true);
        }
    }
}
