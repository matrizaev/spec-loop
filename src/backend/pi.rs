use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::{AgentBackend, AgentResponse, AgentRun};
use crate::commands::{ProcessTermination, run_process, status_success};
use crate::config::{AgentConfig, AgentStage, Profile};
use crate::error::{Result, SpecLoopError, io_error};
use crate::prompts::PromptRole;

/// Pi's machine-readable subprocess backend.
pub struct PiBackend;

impl AgentBackend for PiBackend {
    fn run(
        &self,
        root: &Path,
        profile: &Profile,
        prompt: &str,
        role: PromptRole,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<AgentRun> {
        let args = arguments(&profile.agent, role, prompt);
        let timeout = profile.agent.timeout_seconds.map(Duration::from_secs);
        let cwd = PathBuf::from(root);
        let executable = resolve_executable(root, &profile.agent.executable)?;
        let mut command_argv = vec![executable.display().to_string()];
        command_argv.extend(display_arguments(&args).into_iter().map(str::to_owned));
        crate::commands::report_progress(&format!(
            "Command argv: {command_argv:?}\nWorking directory: {}",
            cwd.display()
        ))?;
        let output = run_process(&executable, &args, &cwd, timeout, cancelled)?;
        let outcome = if output.termination == ProcessTermination::Interrupted {
            Err(SpecLoopError::Interrupted)
        } else if output.termination == ProcessTermination::TimedOut {
            Err(SpecLoopError::Timeout(
                "Pi agent call exceeded configured timeout".to_owned(),
            ))
        } else if !status_success(&output) {
            let status = output
                .status
                .map_or_else(|| "unknown status".to_owned(), |status| status.to_string());
            Err(SpecLoopError::Agent(format!(
                "Pi exited with {status}; stderr: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        } else {
            parse_jsonl(&output.stdout)
        };
        Ok(AgentRun {
            outcome,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

fn display_arguments(args: &[String]) -> Vec<&str> {
    let mut redact_next = false;
    args.iter()
        .map(|argument| {
            if redact_next {
                redact_next = false;
                "<prompt omitted>"
            } else {
                redact_next = argument == "-p";
                argument
            }
        })
        .collect()
}

fn resolve_executable(root: &Path, executable: &Path) -> Result<PathBuf> {
    let has_path_component = executable.is_absolute()
        || executable
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty());
    let candidates = if executable.is_absolute() {
        vec![executable.to_path_buf()]
    } else if has_path_component {
        vec![root.join(executable)]
    } else {
        let search_path = env::var_os("PATH").ok_or_else(|| {
            SpecLoopError::Config("PATH is unavailable while resolving agent executable".to_owned())
        })?;
        env::split_paths(&search_path)
            .map(|directory| {
                if directory.is_absolute() {
                    directory.join(executable)
                } else {
                    root.join(directory).join(executable)
                }
            })
            .collect()
    };
    for candidate in candidates {
        if is_executable_file(&candidate) {
            return fs::canonicalize(&candidate)
                .map_err(|source| io_error("resolve agent executable", &candidate, source));
        }
    }
    Err(SpecLoopError::Config(format!(
        "agent executable was not found or is not executable: {}",
        executable.display()
    )))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn arguments(agent: &AgentConfig, role: PromptRole, prompt: &str) -> Vec<String> {
    let stage = match role {
        PromptRole::PlanAuthor
        | PromptRole::PlanReview
        | PromptRole::PlanAudit
        | PromptRole::PlanClarification => AgentStage::Planning,
        PromptRole::Implementation => AgentStage::Implementation,
        PromptRole::Review | PromptRole::ReviewAudit | PromptRole::ReviewClarification => {
            AgentStage::Review
        }
    };
    let mut args = vec![
        "--no-session".to_owned(),
        "--mode".to_owned(),
        "json".to_owned(),
    ];
    if let Some(model) = agent.model_for(stage) {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(thinking) = agent.thinking_for(stage) {
        args.extend(["--thinking".to_owned(), thinking.to_owned()]);
    }
    args.extend(["-p".to_owned(), prompt.to_owned()]);
    args
}

fn parse_jsonl(stdout: &[u8]) -> Result<AgentResponse> {
    let output = std::str::from_utf8(stdout)
        .map_err(|error| SpecLoopError::Agent(format!("Pi JSONL is not UTF-8: {error}")))?;
    let mut settled = false;
    let mut aborted = false;
    let mut final_message: Option<(String, String)> = None;
    let mut selected_model = None;
    let mut response_model = None;
    for (line_number, line) in output.lines().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            continue;
        }
        if settled {
            return Err(SpecLoopError::Agent(format!(
                "Pi emitted event after agent_settled at line {}",
                line_number + 1
            )));
        }
        let event = parse_event_line(line, line_number + 1)?;
        extract_model(&event.value, &mut selected_model, &mut response_model);
        match event.event_type.as_str() {
            "message_end" => {
                final_message = None;
                if let Some(message) = parse_assistant_message(&event.value, line_number + 1)? {
                    final_message = Some(message);
                }
            }
            "agent_settled" => {
                settled = true;
                aborted = parse_settled_aborted(&event.value, line_number + 1)?;
            }
            "error" | "agent_error" => {
                return Err(SpecLoopError::Agent(format!(
                    "Pi emitted an error event at line {}",
                    line_number + 1
                )));
            }
            _ => {}
        }
    }
    if !settled {
        return Err(SpecLoopError::Agent(
            "Pi JSONL ended before agent_settled".to_owned(),
        ));
    }
    if aborted {
        return Err(SpecLoopError::Agent(
            "Pi agent settlement was aborted".to_owned(),
        ));
    }
    let Some((stop_reason, response)) = final_message else {
        return Err(SpecLoopError::Agent(
            "Pi JSONL contains no final assistant message".to_owned(),
        ));
    };
    if stop_reason != "stop" {
        return Err(SpecLoopError::Agent(format!(
            "final assistant stopReason was {stop_reason:?}, expected `stop`"
        )));
    }
    if response.trim().is_empty() {
        return Err(SpecLoopError::Agent(
            "final assistant response was empty".to_owned(),
        ));
    }
    Ok(AgentResponse {
        response,
        selected_model,
        response_model,
    })
}

struct JsonEvent {
    value: Value,
    event_type: String,
}

fn parse_event_line(line: &str, line_number: usize) -> Result<JsonEvent> {
    let value: Value = serde_json::from_str(line).map_err(|error| {
        SpecLoopError::Agent(format!("malformed Pi JSONL at line {line_number}: {error}"))
    })?;
    if !value.is_object() {
        return Err(SpecLoopError::Agent(format!(
            "Pi JSONL event at line {line_number} is not an object"
        )));
    }
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SpecLoopError::Agent(format!(
                "Pi JSONL event at line {line_number} has no string type"
            ))
        })?
        .to_owned();
    Ok(JsonEvent { value, event_type })
}

fn extract_model(value: &Value, selected: &mut Option<String>, response: &mut Option<String>) {
    if let Some(message) = value.get("message")
        && message.get("role").and_then(Value::as_str) == Some("assistant")
        && let (Some(provider), Some(model)) = (
            message.get("provider").and_then(Value::as_str),
            message.get("model").and_then(Value::as_str),
        )
    {
        *selected = Some(format!("{provider}/{model}"));
        if let Some(actual) = message.get("responseModel").and_then(Value::as_str)
            && actual != model
        {
            *response = Some(format!("{provider}/{actual}"));
        }
    }
}

fn parse_assistant_message(value: &Value, line_number: usize) -> Result<Option<(String, String)>> {
    let Some(message) = value.get("message") else {
        return Ok(None);
    };
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Ok(None);
    }
    let stop_reason = message
        .get("stopReason")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SpecLoopError::Agent(format!(
                "assistant message at line {line_number} has no string stopReason"
            ))
        })?
        .to_owned();
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SpecLoopError::Agent(format!(
                "assistant message at line {line_number} has no content array"
            ))
        })?;
    let mut text_blocks = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            let text = block.get("text").and_then(Value::as_str).ok_or_else(|| {
                SpecLoopError::Agent(format!(
                    "assistant text block at line {line_number} has no string text"
                ))
            })?;
            text_blocks.push(text);
        }
    }
    Ok(Some((stop_reason, text_blocks.join(""))))
}

fn parse_settled_aborted(value: &Value, line_number: usize) -> Result<bool> {
    value
        .get("aborted")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            SpecLoopError::Agent(format!(
                "agent_settled event at line {line_number} has no boolean aborted field"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::{arguments, display_arguments, parse_jsonl};
    use crate::config::{AgentConfig, AgentStageConfig, AgentStages};
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
            thinking: Some("low".to_owned()),
            stages: AgentStages {
                planning: AgentStageConfig {
                    model: Some("provider/planning".to_owned()),
                    thinking: None,
                },
                implementation: AgentStageConfig {
                    model: None,
                    thinking: Some("medium".to_owned()),
                },
                review: AgentStageConfig {
                    model: Some("provider/review".to_owned()),
                    thinking: Some("high".to_owned()),
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
            thinking: Some("high".to_owned()),
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
}
