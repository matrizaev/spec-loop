use super::{arguments, display_arguments, parse_jsonl};
use crate::config::{AgentConfig, AgentStageConfig, AgentStages, ThinkingLevel};
use crate::prompts::PromptRole;

fn event_json() -> String {
    let message = serde_json::json!({
        "role": "assistant",
        "provider": "test",
        "model": "model",
        "stopReason": "stop",
        "content": [{"type": "text", "text": "done\nSPEC_LOOP_IMPLEMENTATION=READY_FOR_REVIEW"}]
    });
    let message_end = serde_json::json!({"type": "message_end", "message": message});
    let settled = serde_json::json!({"type": "agent_settled", "aborted": false});
    format!("{message_end}\n{settled}\n")
}

#[test]
fn accepts_settled_final_assistant_message() -> Result<(), Box<dyn std::error::Error>> {
    let parsed = parse_jsonl(event_json().as_bytes())?;
    assert_eq!(parsed.selected_model.as_deref(), Some("test/model"));
    assert_eq!(
        parsed
            .response
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty()),
        Some("SPEC_LOOP_IMPLEMENTATION=READY_FOR_REVIEW")
    );
    Ok(())
}

#[test]
fn command_display_redacts_prompt_argument() {
    let args = vec![
        "--mode".to_owned(),
        "json".to_owned(),
        "-p".to_owned(),
        "sensitive prompt".to_owned(),
    ];

    assert_eq!(
        display_arguments(&args),
        ["--mode", "json", "-p", "<prompt omitted>"]
    );
}

#[test]
fn invocation_preserves_pi_resources_and_adds_only_protocol_arguments() {
    let args = arguments(
        &AgentConfig::default(),
        PromptRole::Implementation,
        "review prompt",
    );
    for resource_option in [
        "--tools",
        "--no-tools",
        "--mcp",
        "--no-mcp",
        "--extension",
        "--no-extensions",
        "--skill",
        "--no-skills",
        "--no-context-files",
        "--approve",
        "--no-approve",
    ] {
        assert!(
            !args.iter().any(|argument| argument == resource_option),
            "{resource_option} unexpectedly passed to Pi"
        );
    }
    assert_eq!(&args[..3], ["--no-session", "--mode", "json"]);
    assert_eq!(&args[3..], ["-p", "review prompt"]);
}

#[test]
fn stage_overrides_apply_to_every_role_family_and_fall_back_globally() {
    let agent = AgentConfig {
        model: Some("provider/global".to_owned()),
        thinking: Some(ThinkingLevel::Low),
        stages: AgentStages {
            planning: AgentStageConfig {
                model: Some("provider/planning".to_owned()),
                thinking: None,
            },
            implementation: AgentStageConfig {
                model: None,
                thinking: Some(ThinkingLevel::Medium),
            },
            review: AgentStageConfig {
                model: Some("provider/review".to_owned()),
                thinking: Some(ThinkingLevel::High),
            },
        },
        ..AgentConfig::default()
    };
    for role in [PromptRole::PlanAuthor, PromptRole::PlanReview] {
        let args = arguments(&agent, role, "review prompt");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "provider/planning"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--thinking", "low"]));
    }
    let implementation = arguments(&agent, PromptRole::Implementation, "review prompt");
    assert!(
        implementation
            .windows(2)
            .any(|pair| pair == ["--model", "provider/global"])
    );
    assert!(
        implementation
            .windows(2)
            .any(|pair| pair == ["--thinking", "medium"])
    );
    for role in [PromptRole::PlanAudit, PromptRole::PlanClarification] {
        let args = arguments(&agent, role, "plan prompt");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "provider/planning"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--thinking", "low"]));
    }
    for role in [
        PromptRole::Review,
        PromptRole::ReviewAudit,
        PromptRole::ReviewClarification,
    ] {
        let args = arguments(&agent, role, "review prompt");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "provider/review"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--thinking", "high"]));
    }
}

#[test]
fn explicit_model_and_thinking_profile_settings_are_passed_through() {
    let agent = AgentConfig {
        model: Some("provider/model".to_owned()),
        thinking: Some(ThinkingLevel::High),
        ..AgentConfig::default()
    };
    let args = arguments(&agent, PromptRole::Implementation, "review prompt");
    assert!(
        args.windows(2)
            .any(|pair| { pair[0] == "--model" && pair[1] == "provider/model" })
    );
    assert!(
        args.windows(2)
            .any(|pair| { pair[0] == "--thinking" && pair[1] == "high" })
    );
}

#[test]
fn rejects_aborted_settlement() {
    let output = event_json().replace("\"aborted\":false", "\"aborted\":true");
    assert!(
        parse_jsonl(output.as_bytes())
            .is_err_and(|error| error.to_string().contains("settlement was aborted"))
    );
}

#[test]
fn rejects_length_stop_reason() {
    let output = event_json().replace("\"stopReason\":\"stop\"", "\"stopReason\":\"length\"");
    assert!(
        parse_jsonl(output.as_bytes())
            .is_err_and(|error| error.to_string().contains("expected `stop`"))
    );
}

#[test]
fn rejects_unsettled_stream() {
    let output = serde_json::json!({
        "type": "message_end",
        "message": {"role":"assistant", "stopReason":"stop", "content":[{"type":"text","text":"done"}]}
    });
    assert!(parse_jsonl(output.to_string().as_bytes()).is_err());
}

#[test]
fn rejects_stale_marked_assistant_message_followed_by_tool_message() {
    let assistant = serde_json::json!({
        "type": "message_end",
        "message": {
            "role": "assistant",
            "stopReason": "stop",
            "content": [{
                "type": "text",
                "text": "SPEC_LOOP_IMPLEMENTATION=READY_FOR_REVIEW"
            }]
        }
    });
    let tool = serde_json::json!({
        "type": "message_end",
        "message": {
            "role": "toolResult",
            "content": [{"type": "text", "text": "tool completed"}]
        }
    });
    let settled = serde_json::json!({"type": "agent_settled", "aborted": false});
    let output = format!("{assistant}\n{tool}\n{settled}\n");
    assert!(parse_jsonl(output.as_bytes()).is_err_and(|error| {
        error
            .to_string()
            .contains("contains no final assistant message")
    }));
}

#[test]
fn rejects_error_events_even_when_stream_settles() {
    let output = format!(
        "{}\n{}\n",
        serde_json::json!({"type":"error", "message":"failed"}),
        serde_json::json!({"type":"agent_settled", "aborted":false})
    );
    assert!(parse_jsonl(output.as_bytes()).is_err());
}

#[test]
fn rejects_events_after_settlement() {
    let output = format!(
        "{}\n{}\n",
        serde_json::json!({"type":"agent_settled", "aborted": false}),
        serde_json::json!({"type":"turn_end"})
    );
    assert!(parse_jsonl(output.as_bytes()).is_err());
}
