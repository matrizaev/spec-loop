use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use super::PiBackend;
use crate::backend::{AgentBackend, AgentInvocation, AgentResponse, UsageSnapshot};
use crate::config::{AgentBackendKind, Profile};
use crate::integrity::InputIntegrity;
use crate::output::{ColorMode, Output};
use crate::prompts::{self, PromptContext, PromptRole};
use crate::protocol::{self, ImplementationReviewResponse, ReviewResult};
use crate::run_dir::RunDir;

// Temporary live diagnostic, deliberately excluded from normal CI and role retry loops.
// Reuses the user's prior check result; invokes one reviewer, never the workflow engine.
#[test]
#[ignore = "manual live Pi review; requires SPEC_LOOP_API_PROBE=1 and consumes provider usage"]
fn live_pi_review_status_probe() {
    assert_eq!(
        std::env::var("SPEC_LOOP_API_PROBE").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let root = std::env::current_dir().expect("project root");
    let spec = Path::new("specs/013-deduplication-and-simplification.md");
    let plan = Path::new(".spec-loop/plans/013.md");
    let integrity =
        InputIntegrity::capture(&root, spec, plan, false).expect("protect selected inputs");
    let profile = pi_profile(&root);
    let check_path = Path::new("/var/tmp/spec-loop-EU4DH6/check-0001.log");
    let checks = saved_check_summary(check_path);
    let prompt = prompts::build_prompt(
        &root,
        &profile,
        PromptContext {
            role: PromptRole::Review,
            spec_id: "013",
            spec_path: spec,
            plan_path: Some(plan),
            feedback: None,
            validation_feedback: None,
            checks: &checks,
            implementation_summary: None,
        },
    )
    .expect("normal review prompt");
    let logs = RunDir::create(&root).expect("private external diagnostic logs");
    logs.write_log("pi-review.prompt.txt", prompt.as_bytes())
        .expect("save prompt");
    eprintln!(
        "PI_STATUS_PROBE logs={} reused_checks={}",
        logs.root.display(),
        check_path.display()
    );
    integrity
        .verify(&root)
        .expect("selected inputs before review");
    let output = Output::new(ColorMode::Never, false);
    let run = PiBackend
        .invoke(
            &root,
            &profile,
            &prompt,
            PromptRole::Review,
            &Arc::new(AtomicBool::new(false)),
            AgentInvocation {
                continuation: None,
                output: &output,
                usage_baseline: UsageSnapshot::default(),
            },
        )
        .expect("invoke actual Pi backend once");
    logs.write_log("pi-review.stdout.jsonl", &run.stdout)
        .expect("save Pi events");
    logs.write_log("pi-review.stderr.log", &run.stderr)
        .expect("save Pi diagnostics");
    assert!(
        run.usage.is_none(),
        "Pi must not use built-in usage telemetry"
    );
    let response = run.outcome.unwrap_or_else(|error| {
        panic!(
            "Pi invocation failed: {error}; inspect {}",
            logs.root.display()
        )
    });
    logs.write_log("pi-review.response.json", response.response.as_bytes())
        .expect("save original final response");
    integrity
        .verify(&root)
        .expect("selected inputs remain unchanged");
    report_review("workspace", &response, &run.stdout);
}

#[test]
#[ignore = "manual live Pi status-only probe; requires SPEC_LOOP_API_PROBE=1 and consumes provider usage"]
fn live_pi_empty_review_handoff_probe() {
    assert_eq!(
        std::env::var("SPEC_LOOP_API_PROBE").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let root = std::env::current_dir().expect("project root");
    let logs = RunDir::create(&root).expect("private diagnostic logs");
    let saved = fs::read("/var/tmp/spec-loop-XOX25O/pi-review.stdout.jsonl")
        .expect("saved workspace probe events");
    let saved_response = super::parse_jsonl(&saved).expect("saved Pi response transport");
    logs.write_log(
        "saved-workspace-review.response.json",
        saved_response.response.as_bytes(),
    )
    .expect("recover original response without resuming workflow");
    report_review("saved-workspace", &saved_response, &saved);
    let mut profile = pi_profile(&root);
    profile.agent.timeout_seconds = Some(60);
    let schema = serde_json::to_string_pretty(
        &prompts::schema_for_role(PromptRole::Review).expect("canonical role schema"),
    )
    .expect("schema text");
    let prompt = format!(
        "Temporary status-formatting probe only, not a review or approval of Spec 013. Do not inspect workspace, edit files, or run tools. All input is provided here. A synthetic independent review is complete, with zero actionable findings, no genuine external blocker, and no unfinished required work. Choose the correct typed review outcome from those facts; do not invent findings or missing prerequisites. No workflow or actions will consume the result. Return exactly one JSON object matching this canonical role schema, with no Markdown or extra text:\n{schema}"
    );
    logs.write_log("empty-review.prompt.txt", prompt.as_bytes())
        .expect("save synthetic prompt");
    eprintln!("PI_STATUS_PROBE logs={}", logs.root.display());
    let output = Output::new(ColorMode::Never, false);
    let run = PiBackend
        .invoke(
            &root,
            &profile,
            &prompt,
            PromptRole::Review,
            &Arc::new(AtomicBool::new(false)),
            AgentInvocation {
                continuation: None,
                output: &output,
                usage_baseline: UsageSnapshot::default(),
            },
        )
        .expect("one status-only Pi invocation");
    logs.write_log("empty-review.stdout.jsonl", &run.stdout)
        .expect("save events");
    logs.write_log("empty-review.stderr.log", &run.stderr)
        .expect("save diagnostics");
    let response = run
        .outcome
        .unwrap_or_else(|error| panic!("Pi probe failed: {error}"));
    logs.write_log("empty-review.response.json", response.response.as_bytes())
        .expect("save handoff");
    report_review("synthetic-empty", &response, &run.stdout);
}

fn report_review(label: &str, response: &AgentResponse, events: &[u8]) {
    let parsed = protocol::parse::<ImplementationReviewResponse>(&response.response, "review")
        .expect("shared strict role parser");
    let tools = String::from_utf8_lossy(events)
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("Pi event JSON"))
        .filter(|event| event["type"] == "tool_execution_start")
        .count();
    eprintln!(
        "PI_STATUS_PROBE case={label} selected_model={:?} tools={tools}",
        response.selected_model
    );
    match parsed.result {
        ReviewResult::NoIssues { findings } => eprintln!(
            "PI_STATUS_PROBE case={label} status=no_issues findings={}",
            findings.len()
        ),
        ReviewResult::Issues { findings } => eprintln!(
            "PI_STATUS_PROBE case={label} status=issues findings={}",
            findings.len()
        ),
        ReviewResult::Blocked { reason } => eprintln!(
            "PI_STATUS_PROBE case={label} status=blocked no_findings_payload=true reason={reason}"
        ),
    }
}

fn pi_profile(root: &Path) -> Profile {
    let mut profile = Profile::load(&root.join(".spec-loop.toml")).expect("selected profile");
    // Test-only backend selection; preserve model/thinking and Pi's user-owned resources.
    profile.agent.backend = AgentBackendKind::Pi;
    profile.agent.context_window_tokens = None;
    profile.agent.max_tool_calls = None;
    profile.agent.max_tool_output_bytes = None;
    for stage in [
        &mut profile.agent.stages.planning,
        &mut profile.agent.stages.implementation,
        &mut profile.agent.stages.review,
    ] {
        stage.context_window_tokens = None;
    }
    profile.agent.timeout_seconds = Some(300);
    profile.validate().expect("valid Pi diagnostic profile");
    profile
}

fn saved_check_summary(path: &Path) -> String {
    let saved = fs::read_to_string(path).expect("saved check result");
    let (header, streams) = saved
        .split_once("--- stdout (")
        .expect("check log stdout marker");
    assert!(
        header.lines().any(|line| line == "Status: exit status: 0"),
        "reuse only observed successful check result"
    );
    let (_, streams) = streams.split_once('\n').expect("stdout header terminator");
    let (stdout, stderr) = streams
        .rsplit_once("\n--- stderr (")
        .expect("check log stderr marker");
    let (_, stderr) = stderr.split_once('\n').expect("stderr header terminator");
    let last_lines = |text: &str| {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "{header}Log: {}\nstdout excerpt:\n{}\nstderr excerpt:\n{}\n\nDiagnostic replay: this is the saved check result from the user's prior run, not a fresh check. Do not rerun checks or lifecycle actions for this probe. Review only, do not edit repository files. Return your own typed review outcome; no correction, adjudication, approval actions, or archival follow this diagnostic invocation.\n",
        path.display(),
        last_lines(stdout),
        last_lines(stderr)
    )
}
