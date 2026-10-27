use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::{Value, json};

use super::BuiltinConversation;
use super::provider::ensure_active;
use crate::commands::{ProcessTermination, run_process_until};
use crate::config::Profile;
use crate::error::{Result, SpecLoopError};

#[cfg(test)]
mod tests;

pub(super) fn execute_shell_calls(
    root: &Path,
    profile: &Profile,
    deadline: Option<Instant>,
    cancelled: &Arc<AtomicBool>,
    calls: Vec<ShellCall>,
    conversation: &mut BuiltinConversation,
    transcript: &mut Vec<u8>,
) -> Result<()> {
    for call in calls {
        ensure_active(deadline, cancelled)?;
        if let (Some(tool_calls), Some(limit)) =
            (&mut conversation.tool_calls, profile.agent.max_tool_calls)
        {
            if *tool_calls >= limit {
                return Err(SpecLoopError::Workflow(format!(
                    "built-in agent reached agent.max_tool_calls={limit} before executing another shell call; role stopped without replay"
                )));
            }
            *tool_calls = tool_calls.checked_add(1).ok_or_else(|| {
                SpecLoopError::Workflow("built-in shell call counter overflowed".to_owned())
            })?;
        }
        let result = run_shell(
            root,
            &call.command,
            profile.agent.max_tool_output_bytes(),
            deadline,
            cancelled,
        )?;
        append_transcript(transcript, &call.command, &result.log_output)?;
        match result.termination {
            ProcessTermination::Exited => {}
            ProcessTermination::Interrupted => return Err(SpecLoopError::Interrupted),
            ProcessTermination::TimedOut => {
                return Err(SpecLoopError::Timeout(
                    "built-in shell command exceeded the role deadline".to_owned(),
                ));
            }
        }
        conversation.input.push(json!({
            "type": "function_call_output",
            "call_id": call.call_id,
            "output": result.tool_output
        }));
    }
    Ok(())
}

pub(super) fn shell_tool_definition() -> Value {
    json!({
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
    })
}

pub(super) struct ShellCall {
    pub(super) call_id: String,
    pub(super) command: String,
}

pub(super) fn completed_output(response: &Value) -> Result<Vec<Value>> {
    if response.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(SpecLoopError::Agent(
            "OpenAI Responses result did not reach completed state".to_owned(),
        ));
    }
    response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| SpecLoopError::Agent("OpenAI Responses output is missing".to_owned()))
}

pub(super) fn parse_output_items(output: &[Value]) -> Result<(Vec<ShellCall>, Option<String>)> {
    let mut calls = Vec::new();
    let mut final_text = String::new();
    let mut saw_text = false;
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let name = item.get("name").and_then(Value::as_str);
                let namespace = item.get("namespace").and_then(Value::as_str);
                let is_shell = matches!(
                    (name, namespace),
                    (Some("spec_loop.shell"), None | Some("spec_loop"))
                        | (Some("shell"), Some("spec_loop"))
                );
                if !is_shell {
                    let received_name = match name {
                        Some("shell") => "shell",
                        Some(_) => "other",
                        None => "missing",
                    };
                    let received_namespace = match item.get("namespace").and_then(Value::as_str) {
                        Some("spec_loop") => "spec_loop",
                        Some(_) => "other",
                        None => "missing",
                    };
                    return Err(SpecLoopError::Agent(format!(
                        "OpenAI Responses requested unsupported tool `{received_name}` in namespace `{received_namespace}`"
                    )));
                }
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|call_id| !call_id.is_empty())
                    .ok_or_else(|| {
                        SpecLoopError::Agent("shell tool call omitted call_id".to_owned())
                    })?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        SpecLoopError::Agent("shell tool arguments are missing".to_owned())
                    })?;
                let arguments: Value = serde_json::from_str(arguments).map_err(|error| {
                    SpecLoopError::Agent(format!("shell tool arguments are malformed: {error}"))
                })?;
                let command = arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .filter(|command| !command.is_empty())
                    .ok_or_else(|| {
                        SpecLoopError::Agent(
                            "shell tool call requires a non-empty command".to_owned(),
                        )
                    })?;
                calls.push(ShellCall {
                    call_id: call_id.to_owned(),
                    command: command.to_owned(),
                });
            }
            Some("message") => {
                let Some(content) = item.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(text) = part.get("text").and_then(Value::as_str)
                    {
                        if saw_text {
                            final_text.push('\n');
                        }
                        final_text.push_str(text);
                        saw_text = true;
                    }
                }
            }
            Some("reasoning" | "file_search_call" | "web_search_call") => {}
            Some(_) | None => {
                return Err(SpecLoopError::Agent(
                    "OpenAI Responses output contains an unsupported or malformed item".to_owned(),
                ));
            }
        }
    }
    Ok((calls, saw_text.then_some(final_text)))
}

pub(super) struct ShellResult {
    pub(super) tool_output: String,
    pub(super) log_output: String,
    pub(super) termination: ProcessTermination,
}

pub(super) fn render_captured_streams(
    stdout: &[u8],
    stderr: &[u8],
    max_output_bytes: usize,
) -> (String, String, bool, bool) {
    let stdout_limit = max_output_bytes / 2 + max_output_bytes % 2;
    let stderr_limit = max_output_bytes / 2;
    let (stdout, stdout_truncated) = render_captured_stream(stdout, stdout_limit);
    let (stderr, stderr_truncated) = render_captured_stream(stderr, stderr_limit);
    (stdout, stderr, stdout_truncated, stderr_truncated)
}

fn render_captured_stream(bytes: &[u8], limit: usize) -> (String, bool) {
    let rendered = String::from_utf8_lossy(bytes);
    if rendered.len() <= limit {
        return (rendered.into_owned(), false);
    }
    let mut end = limit;
    while !rendered.is_char_boundary(end) {
        end -= 1;
    }
    (rendered[..end].to_owned(), true)
}

pub(super) fn run_shell(
    root: &Path,
    command: &str,
    max_output_bytes: usize,
    deadline: Option<Instant>,
    cancelled: &Arc<AtomicBool>,
) -> Result<ShellResult> {
    #[cfg(unix)]
    {
        let output = run_process_until(
            Path::new("/bin/sh"),
            &["-c".to_owned(), command.to_owned()],
            root,
            deadline,
            Some(max_output_bytes),
            cancelled,
        )?;
        let termination = if cancelled.load(Ordering::Relaxed) {
            ProcessTermination::Interrupted
        } else {
            output.termination
        };
        let status = output
            .status
            .and_then(|status| status.code())
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        let (stdout, stderr, stdout_render_truncated, stderr_render_truncated) =
            render_captured_streams(&output.stdout, &output.stderr, max_output_bytes);
        let termination_name = match termination {
            ProcessTermination::Exited => "exited",
            ProcessTermination::TimedOut => "timed_out",
            ProcessTermination::Interrupted => "interrupted",
        };
        let mut tool_output = format!(
            "termination={termination_name}\nexit_status={status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        if output.stdout_truncated
            || output.stderr_truncated
            || stdout_render_truncated
            || stderr_render_truncated
        {
            tool_output.push_str("\n[tool output truncated at configured byte limit]");
        }
        if !status_success_for_tool(&output) {
            tool_output.push_str("\n[command exited unsuccessfully]");
        }
        Ok(ShellResult {
            log_output: tool_output.clone(),
            tool_output,
            termination,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (root, command, max_output_bytes, deadline, cancelled);
        Err(SpecLoopError::Workflow(
            "built-in shell tool is supported only on POSIX platforms with `/bin/sh`".to_owned(),
        ))
    }
}

#[cfg(unix)]
fn status_success_for_tool(output: &crate::commands::ChildOutput) -> bool {
    output.termination == ProcessTermination::Exited
        && output.status.is_some_and(|status| status.success())
}

pub(super) fn append_transcript(
    transcript: &mut Vec<u8>,
    command: &str,
    output: &str,
) -> Result<()> {
    let entry = format!("$ {command}\n{output}\n\n");
    transcript.try_reserve(entry.len()).map_err(|error| {
        SpecLoopError::Workflow(format!("reserve built-in shell transcript space: {error}"))
    })?;
    transcript.extend_from_slice(entry.as_bytes());
    Ok(())
}
