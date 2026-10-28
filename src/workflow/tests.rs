use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Parser;
use serde_json::json;
use tempfile::TempDir;

use super::execution::{display_output_excerpt, output_excerpt};
use super::{
    CandidateFiles, ImplementationEntry, archive_destination, run_at, validate_action_executables,
    validate_check_executables,
};
use crate::backend::{
    AgentBackend, AgentResponse, AgentRun, BackendContinuation, BackendFailure, UsageSnapshot,
};
use crate::config::Profile;
use crate::error::{Result, SpecLoopError};
use crate::paths::is_executable_file;
use crate::prompts::PromptRole;

struct FakeBuiltinBackend {
    responses: Mutex<VecDeque<(PromptRole, String)>>,
    observed: Arc<Mutex<Vec<(PromptRole, String)>>>,
}

impl AgentBackend for FakeBuiltinBackend {
    fn invoke(
        &self,
        _root: &Path,
        _profile: &Profile,
        prompt: &str,
        role: PromptRole,
        _cancelled: &Arc<std::sync::atomic::AtomicBool>,
        invocation: crate::backend::AgentInvocation<'_>,
    ) -> Result<AgentRun> {
        assert!(
            invocation.continuation.is_none(),
            "ordinary scripted invocation"
        );
        let mut observed = self
            .observed
            .lock()
            .expect("lock fake backend observations");
        observed.push((role, prompt.to_owned()));
        drop(observed);
        let (expected_role, response) = self
            .responses
            .lock()
            .expect("lock fake backend responses")
            .pop_front()
            .expect("fake backend response for workflow role");
        assert_eq!(role, expected_role);
        Ok(AgentRun {
            outcome: Ok(AgentResponse {
                response,
                selected_model: Some("fake-model".to_owned()),
                response_model: None,
            }),
            continuation: None,
            stdout: b"fake built-in provider transcript".to_vec(),
            stderr: Vec::new(),
            usage: None,
        })
    }
}

struct FatalToolLimitBackend {
    calls: Arc<Mutex<Vec<PromptRole>>>,
    diagnostic: String,
}

struct CorrectionContinuationBackend {
    calls: Arc<Mutex<Vec<(PromptRole, String, bool)>>>,
    implementation_calls: Mutex<usize>,
}

type ScopedUsageBaseline = (String, PromptRole, Option<u64>, Option<u64>, Option<u64>);
type ScopedUsageBaselines = Arc<Mutex<Vec<ScopedUsageBaseline>>>;

struct ScopedUsageBackend {
    baselines: ScopedUsageBaselines,
    first_implementation: Mutex<bool>,
}

impl ScopedUsageBackend {
    fn run_fixture(
        &self,
        prompt: &str,
        role: PromptRole,
        continuation: Option<BackendContinuation>,
        baseline: &UsageSnapshot,
    ) -> Result<AgentRun> {
        let spec_id = if continuation.is_some() || prompt.contains("specs/001.md") {
            "001"
        } else {
            "002"
        };
        self.baselines
            .lock()
            .expect("lock scoped usage baselines")
            .push((
                spec_id.to_owned(),
                role,
                baseline.provider_request_attempts,
                baseline.completed_responses,
                baseline.executed_shell_calls,
            ));
        let correction = continuation.is_some();
        if let Some(state) = continuation {
            assert_eq!(state.into_state::<u8>()?, 23);
        }
        let malformed_first_response =
            if role == PromptRole::Implementation && spec_id == "001" && !correction {
                let mut first = self
                    .first_implementation
                    .lock()
                    .expect("lock first implementation flag");
                if *first {
                    false
                } else {
                    *first = true;
                    true
                }
            } else {
                false
            };
        let response = match (role, correction, malformed_first_response) {
            (PromptRole::Implementation, true, _) => {
                r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Corrected fixture response."}"#
            }
            (PromptRole::Implementation, false, true) => {
                r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"invalid","extra":true}"#
            }
            (PromptRole::Implementation, false, false) => {
                r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Fixture implementation."}"#
            }
            (PromptRole::Review, false, _) => {
                r#"{"protocol_version":1,"role":"review","status":"no_issues","findings":[]}"#
            }
            _ => panic!("unexpected scoped-usage backend role {role:?}"),
        };
        let mut usage = fixture_usage_delta();
        if malformed_first_response {
            usage.shell_spawned();
        }
        Ok(AgentRun {
            outcome: Ok(AgentResponse {
                response: response.to_owned(),
                selected_model: Some("fixture-model".to_owned()),
                response_model: None,
            }),
            continuation: malformed_first_response.then(|| BackendContinuation::new(23_u8)),
            stdout: Vec::new(),
            stderr: Vec::new(),
            usage: Some(usage),
        })
    }
}

impl AgentBackend for ScopedUsageBackend {
    fn invoke(
        &self,
        _root: &Path,
        _profile: &Profile,
        prompt: &str,
        role: PromptRole,
        _cancelled: &Arc<std::sync::atomic::AtomicBool>,
        invocation: crate::backend::AgentInvocation<'_>,
    ) -> Result<AgentRun> {
        let crate::backend::AgentInvocation {
            continuation,
            usage_baseline,
            ..
        } = invocation;
        self.run_fixture(prompt, role, continuation, &usage_baseline)
    }
}

fn fixture_usage_delta() -> UsageSnapshot {
    let mut usage = UsageSnapshot::default();
    usage.request_sent();
    usage.response_completed(&json!({
        "usage": {"input_tokens": 120, "output_tokens": 35, "total_tokens": 155}
    }));
    usage
}

impl AgentBackend for CorrectionContinuationBackend {
    fn invoke(
        &self,
        _root: &Path,
        _profile: &Profile,
        prompt: &str,
        role: PromptRole,
        _cancelled: &Arc<std::sync::atomic::AtomicBool>,
        invocation: crate::backend::AgentInvocation<'_>,
    ) -> Result<AgentRun> {
        let correcting = invocation.continuation.is_some();
        let expected = u64::from(correcting);
        assert_eq!(
            invocation.usage_baseline.provider_request_attempts,
            Some(expected)
        );
        assert_eq!(
            invocation.usage_baseline.completed_responses,
            Some(expected)
        );
        if let Some(continuation) = invocation.continuation {
            assert_eq!(continuation.into_state::<u8>()?, 17);
            let report =
                invocation
                    .usage_baseline
                    .format_report("implementation", "fixture-model", 0, None);
            assert!(report.contains("∑ Observed completed-response usage\n  120 in"));
        }
        self.calls
            .lock()
            .expect("lock correction backend calls")
            .push((role, prompt.to_owned(), correcting));
        let first_implementation_call = {
            let mut count = self
                .implementation_calls
                .lock()
                .expect("lock correction implementation count");
            let first = *count == 0;
            if first {
                *count = count
                    .checked_add(1)
                    .expect("increment implementation count");
            }
            first
        };
        let (response, continuation) = match role {
            PromptRole::Implementation if first_implementation_call && !correcting => (
                r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"invalid","extra":true}"#,
                Some(BackendContinuation::new(17_u8)),
            ),
            PromptRole::Implementation => (
                r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Corrected response."}"#,
                None,
            ),
            PromptRole::Review => (
                r#"{"protocol_version":1,"role":"review","status":"no_issues","findings":[]}"#,
                None,
            ),
            _ => panic!("unexpected role {role:?}"),
        };
        Ok(AgentRun {
            outcome: Ok(AgentResponse {
                response: response.to_owned(),
                selected_model: Some("fixture-model".to_owned()),
                response_model: None,
            }),
            continuation,
            stdout: Vec::new(),
            stderr: Vec::new(),
            usage: Some(fixture_usage_delta()),
        })
    }
}

impl AgentBackend for FatalToolLimitBackend {
    fn invoke(
        &self,
        _root: &Path,
        profile: &Profile,
        _prompt: &str,
        role: PromptRole,
        _cancelled: &Arc<std::sync::atomic::AtomicBool>,
        _invocation: crate::backend::AgentInvocation<'_>,
    ) -> Result<AgentRun> {
        self.calls
            .lock()
            .expect("lock fatal backend calls")
            .push(role);
        assert_eq!(profile.agent.max_tool_calls, Some(1));
        Ok(AgentRun {
            outcome: Err(BackendFailure::fatal(SpecLoopError::Workflow(
                self.diagnostic.clone(),
            ))),
            continuation: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            usage: None,
        })
    }
}

fn builtin_workflow_fixture() -> (TempDir, PathBuf) {
    let temporary = TempDir::new().expect("create workflow project");
    let root = temporary.path().join("project");
    fs::create_dir_all(root.join("specs")).expect("create specs directory");
    fs::create_dir_all(root.join(".spec-loop/plans")).expect("create plans directory");
    fs::write(root.join("specs/001.md"), "# Selected specification\n").expect("write spec");
    fs::write(root.join(".spec-loop/plans/001.md"), "# Approved plan\n").expect("write plan");
    fs::write(
        root.join(".spec-loop.toml"),
        r"schema_version = 3
[specs]
active_globs = ['specs/*.md']
id_regex = '^specs/(?P<id>[0-9]+)\.md$'
[plan]
path_template = '.spec-loop/plans/{spec_id}.md'
[agent]
backend = 'builtin'
model = 'fixture-model'
max_tool_calls = 1
[completion]
max_passes = 4
",
    )
    .expect("write built-in profile");
    (temporary, root)
}

fn run_fake_builtin(
    root: &Path,
    plan_mode: bool,
    responses: Vec<(PromptRole, String)>,
    observed: Arc<Mutex<Vec<(PromptRole, String)>>>,
    run_dir_observer: &mut Option<PathBuf>,
) -> Result<Option<PathBuf>> {
    let arguments = if plan_mode {
        vec!["spec-loop", "--plan", "--spec", "001"]
    } else {
        vec!["spec-loop", "--spec", "001"]
    };
    let cli = crate::cli::Cli::try_parse_from(arguments).expect("parse workflow arguments");
    run_at(
        &cli,
        root.to_path_buf(),
        Some(Box::new(FakeBuiltinBackend {
            responses: Mutex::new(responses.into()),
            observed,
        })),
        false,
        Some(run_dir_observer),
    )
}

#[cfg(unix)]
fn blocker_fixture(backend: &str, max_passes: usize) -> (TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let (temporary, root) = builtin_workflow_fixture();
    fs::create_dir(root.join("archive")).expect("archive directory");
    for (name, marker) in [
        ("agent", "unused"),
        ("check", "checks-ran"),
        ("review-success", "success-ran"),
        ("finish", "finish-ran"),
    ] {
        let path = root.join(name);
        fs::write(&path, format!("#!/bin/sh\nprintf x >> {marker}\n"))
            .expect("write fake executable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("executable permissions");
    }
    fs::write(root.join("user-change"), "preserve me").expect("user file");
    fs::write(root.join(".spec-loop.toml"), format!(
        "schema_version=3\n[specs]\nactive_globs=['specs/*.md']\nid_regex='^(?:specs|archive)/(?P<id>[0-9]+)\\.md$'\n[agent]\nbackend='{backend}'\nexecutable='./agent'\nmodel='fixture-model'\n[review.context]\nargv=['context-sentinel','argument with spaces']\n[completion]\nmax_passes={max_passes}\n[archive]\nenabled=true\ndestination='archive'\n[[checks]]\nname='fixture check'\nargv=['./check']\n[[actions.on_review_success]]\nname='success'\nargv=['./review-success']\n[[actions.on_finish]]\nname='finish'\nargv=['./finish']\n"
    )).expect("blocker profile");
    (temporary, root)
}

#[cfg(unix)]
fn blocker_response(role: PromptRole, mut value: serde_json::Value) -> (PromptRole, String) {
    value["protocol_version"] = json!(1);
    value["role"] = json!(role.protocol_name());
    (role, value.to_string())
}

#[cfg(unix)]
fn blocker_initial(reviewer: PromptRole) -> (PromptRole, String) {
    if reviewer == PromptRole::PlanReview {
        blocker_response(PromptRole::PlanAuthor, json!({"status":"ready_for_review"}))
    } else {
        blocker_response(
            PromptRole::Implementation,
            json!({"status":"ready_for_review","summary":"Preserved fixture work."}),
        )
    }
}

#[cfg(unix)]
fn reviewer_blocked(reviewer: PromptRole) -> (PromptRole, String) {
    blocker_response(
        reviewer,
        json!({
            "status":"blocked",
            "reason":"This plan review found no actionable gaps and no genuine external blocker."
        }),
    )
}

#[cfg(unix)]
fn blocker_auditor(reviewer: PromptRole) -> PromptRole {
    if reviewer == PromptRole::PlanReview {
        PromptRole::PlanAudit
    } else {
        PromptRole::ReviewAudit
    }
}

#[cfg(unix)]
fn adjudication_response(role: PromptRole, outcome: &str) -> (PromptRole, String) {
    let mut adjudication = json!({"outcome":outcome,"rationale":"Fixture audit decision.","evidence":"Supplied review.reason and selected specification."});
    match outcome {
        "confirmed" => {
            adjudication["requirement"] = json!("Required approval");
            adjudication["prerequisite"] = json!("External approval");
            adjudication["unavailable_reason"] = json!("Tools cannot grant external approval");
            adjudication["required_input"] = json!("Provide approval");
        }
        "ambiguous" => adjudication["question"] = json!("Is the required approval available?"),
        "unsupported" => {}
        _ => panic!("invalid fixture outcome"),
    }
    blocker_response(
        role,
        json!({"status":"adjudicated","adjudication":adjudication}),
    )
}

#[cfg(unix)]
fn feedback_json(prompt: &str) -> serde_json::Value {
    let tail = prompt
        .split("--- Validated typed handoff or prior feedback (JSON where specified) ---\n")
        .nth(1)
        .expect("typed feedback section");
    let tail = tail
        .rsplit("--- Blocker adjudication feedback ---\n")
        .next()
        .expect("feedback tail");
    let tail = tail
        .get(tail.find('{').expect("JSON object start")..)
        .expect("JSON feedback tail");
    serde_json::Deserializer::from_str(tail)
        .into_iter::<serde_json::Value>()
        .next()
        .expect("feedback value")
        .expect("typed feedback JSON")
}

#[cfg(unix)]
#[test]
fn reviewer_blocker_unsupported_returns_to_same_reviewer_without_repeating_gates() {
    for backend in ["builtin", "pi"] {
        for reviewer in [PromptRole::PlanReview, PromptRole::Review] {
            let (_temporary, root) = blocker_fixture(backend, 4);
            let auditor = blocker_auditor(reviewer);
            let initial = blocker_initial(reviewer);
            let blocked = reviewer_blocked(reviewer);
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut logs = None;
            run_fake_builtin(
                &root,
                reviewer == PromptRole::PlanReview,
                vec![
                    initial.clone(),
                    blocked.clone(),
                    adjudication_response(auditor, "unsupported"),
                    blocker_response(reviewer, json!({"status":"no_issues","findings":[]})),
                ],
                Arc::clone(&calls),
                &mut logs,
            )
            .expect("corrected reviewer approves");
            let calls = calls.lock().expect("observations");
            assert_eq!(
                calls.iter().map(|(role, _)| *role).collect::<Vec<_>>(),
                [initial.0, reviewer, auditor, reviewer]
            );
            let audit_prompt = &calls[2].1;
            let audit_input = feedback_json(audit_prompt);
            assert_eq!(audit_input["audit_kind"], "agent_blocker");
            assert_eq!(
                audit_input["response"],
                serde_json::from_str::<serde_json::Value>(&blocked.1).expect("original review")
            );
            assert!(audit_input.get("findings").is_none());
            assert_eq!(
                audit_prompt.contains("Review-context command argv"),
                reviewer == PromptRole::Review
            );
            assert_eq!(
                feedback_json(&calls[3].1)["blocker_audit"]["outcome"],
                "unsupported"
            );
            assert_eq!(
                fs::read_to_string(root.join("user-change")).expect("preserved file"),
                "preserve me"
            );
            if reviewer == PromptRole::Review {
                assert_eq!(
                    fs::read_to_string(root.join("checks-ran")).expect("checks"),
                    "x"
                );
                assert_eq!(
                    fs::read_to_string(root.join("success-ran")).expect("success"),
                    "x"
                );
                assert_eq!(
                    fs::read_to_string(root.join("finish-ran")).expect("finish"),
                    "x"
                );
                assert!(root.join("archive/001.md").is_file());
            } else {
                for marker in ["checks-ran", "success-ran", "finish-ran", "archive/001.md"] {
                    assert!(!root.join(marker).exists());
                }
                assert!(root.join("specs/001.md").is_file());
            }
            let logs = logs.expect("run logs");
            let mut original_logs = Vec::new();
            for entry in fs::read_dir(&logs).expect("read run logs") {
                let entry = entry.expect("log entry");
                if entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".response.json")
                    && fs::read_to_string(entry.path()).expect("response log") == blocked.1
                {
                    original_logs.push(entry.path());
                }
            }
            assert_eq!(
                original_logs.len(),
                1,
                "exact original response retained once"
            );
            fs::remove_dir_all(logs).expect("remove test logs");
        }
    }
}

#[cfg(unix)]
#[test]
fn confirmed_ambiguous_and_auditor_blockers_stop_without_later_stages() {
    for backend in ["builtin", "pi"] {
        for reviewer in [PromptRole::PlanReview, PromptRole::Review] {
            for outcome in ["confirmed", "ambiguous", "auditor_blocked"] {
                let (_temporary, root) = blocker_fixture(backend, 4);
                let auditor = blocker_auditor(reviewer);
                let audit = if outcome == "auditor_blocked" {
                    blocker_response(
                        auditor,
                        json!({"status":"blocked","reason":"Audit evidence unavailable"}),
                    )
                } else {
                    adjudication_response(auditor, outcome)
                };
                let calls = Arc::new(Mutex::new(Vec::new()));
                let mut logs = None;
                let error = run_fake_builtin(&root, reviewer == PromptRole::PlanReview, vec![
                    blocker_initial(reviewer),
                    if outcome == "confirmed" {
                        blocker_response(reviewer, json!({"status":"blocked","reason":"The required external approval is unavailable."}))
                    } else { reviewer_blocked(reviewer) }, audit,
                ], Arc::clone(&calls), &mut logs).expect_err("unresolved blocker stops");
                let message = error.to_string();
                match outcome {
                    "confirmed" => assert!(
                        message.contains("Provide approval")
                            && message.contains("Tools cannot grant external approval")
                    ),
                    "ambiguous" => assert!(message.contains("Is the required approval available?")),
                    "auditor_blocked" => {
                        assert!(message.contains("auditor blocked: Audit evidence unavailable"));
                    }
                    _ => panic!("unexpected outcome"),
                }
                assert_eq!(calls.lock().expect("calls").len(), 3);
                assert!(root.join("specs/001.md").is_file());
                assert_eq!(
                    fs::read_to_string(root.join("user-change")).expect("preserved work"),
                    "preserve me"
                );
                for marker in ["success-ran", "finish-ran", "archive/001.md"] {
                    assert!(!root.join(marker).exists());
                }
                if reviewer == PromptRole::Review {
                    assert_eq!(
                        fs::read_to_string(root.join("checks-ran")).expect("prior check effects"),
                        "x"
                    );
                }
                fs::remove_dir_all(logs.expect("run logs")).expect("remove test logs");
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn wrong_audit_kinds_and_malformed_adjudications_retry_same_auditor() {
    for backend in ["builtin", "pi"] {
        for reviewer in [PromptRole::PlanReview, PromptRole::Review] {
            let (_temporary, root) = blocker_fixture(backend, 8);
            let auditor = blocker_auditor(reviewer);
            let findings = json!([{"id":1,"requirement":"fixture requirement","location":"fixture","evidence":"fixture fact","violation":"fixture gap","proposed_correction":"fixture correction"}]);
            let classification = blocker_response(
                auditor,
                json!({"status":"complete","classifications":[{"finding_id":1,"classification":"supported","actionability":"not_required","rationale":"No required correction"}]}),
            );
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut logs = None;
            run_fake_builtin(&root, reviewer == PromptRole::PlanReview, vec![
                blocker_initial(reviewer), reviewer_blocked(reviewer), classification.clone(),
                blocker_response(auditor, json!({"status":"adjudicated","adjudication":{"outcome":"unsupported","rationale":"missing evidence"}})),
                adjudication_response(auditor, "unsupported"),
                blocker_response(reviewer, json!({"status":"issues","findings":findings})),
                adjudication_response(auditor, "unsupported"), classification,
            ], Arc::clone(&calls), &mut logs).expect("wrong audit kinds corrected");
            let calls = calls.lock().expect("calls");
            assert_eq!(
                calls.iter().map(|(role, _)| *role).collect::<Vec<_>>(),
                [
                    blocker_initial(reviewer).0,
                    reviewer,
                    auditor,
                    auditor,
                    auditor,
                    reviewer,
                    auditor,
                    auditor
                ]
            );
            assert!(
                calls[3]
                    .1
                    .contains("requires blocker adjudication, not finding classifications")
            );
            assert!(calls[4].1.contains("missing fields"));
            assert!(
                calls[7]
                    .1
                    .contains("requires finding classifications, not blocker adjudication")
            );
            drop(calls);
            fs::remove_dir_all(logs.expect("run logs")).expect("remove test logs");
        }
    }
}

#[cfg(unix)]
#[test]
fn renewed_blockers_are_audited_and_attempt_limits_preserve_prior_effects() {
    for reviewer in [PromptRole::PlanReview, PromptRole::Review] {
        for limit in [1, 4] {
            let (_temporary, root) = blocker_fixture("builtin", limit);
            let auditor = blocker_auditor(reviewer);
            let mut responses = vec![
                blocker_initial(reviewer),
                reviewer_blocked(reviewer),
                adjudication_response(auditor, "unsupported"),
            ];
            if limit > 1 {
                responses.extend([
                    reviewer_blocked(reviewer),
                    adjudication_response(auditor, "ambiguous"),
                ]);
            }
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut logs = None;
            let error = run_fake_builtin(
                &root,
                reviewer == PromptRole::PlanReview,
                responses,
                Arc::clone(&calls),
                &mut logs,
            )
            .expect_err("stop without approval");
            assert!(error.to_string().contains(if limit == 1 {
                "completion.max_passes limit"
            } else {
                "adjudication remains ambiguous"
            }));
            assert_eq!(
                calls.lock().expect("calls").len(),
                if limit == 1 { 3 } else { 5 }
            );
            assert!(root.join("specs/001.md").is_file());
            for marker in ["success-ran", "finish-ran", "archive/001.md"] {
                assert!(!root.join(marker).exists());
            }
            if reviewer == PromptRole::Review {
                assert_eq!(
                    fs::read_to_string(root.join("checks-ran")).expect("check effects"),
                    "x"
                );
            }
            fs::remove_dir_all(logs.expect("run logs")).expect("remove test logs");
        }
    }
}

#[cfg(unix)]
struct InterruptingAuditBackend {
    inner: FakeBuiltinBackend,
}

#[cfg(unix)]
impl AgentBackend for InterruptingAuditBackend {
    fn invoke(
        &self,
        root: &Path,
        profile: &Profile,
        prompt: &str,
        role: PromptRole,
        cancelled: &Arc<std::sync::atomic::AtomicBool>,
        invocation: crate::backend::AgentInvocation<'_>,
    ) -> Result<AgentRun> {
        if matches!(role, PromptRole::PlanAudit | PromptRole::ReviewAudit) {
            self.inner
                .observed
                .lock()
                .expect("calls")
                .push((role, prompt.to_owned()));
            return Err(SpecLoopError::Interrupted);
        }
        self.inner
            .invoke(root, profile, prompt, role, cancelled, invocation)
    }
}

#[cfg(unix)]
#[test]
fn interruption_during_blocker_audit_preserves_files_and_prior_check_effects() {
    for backend in ["builtin", "pi"] {
        for reviewer in [PromptRole::PlanReview, PromptRole::Review] {
            let (_temporary, root) = blocker_fixture(backend, 4);
            let observed = Arc::new(Mutex::new(Vec::new()));
            let backend = InterruptingAuditBackend {
                inner: FakeBuiltinBackend {
                    responses: Mutex::new(
                        [blocker_initial(reviewer), reviewer_blocked(reviewer)].into(),
                    ),
                    observed: Arc::clone(&observed),
                },
            };
            let args = if reviewer == PromptRole::PlanReview {
                vec!["spec-loop", "--plan", "--spec", "001"]
            } else {
                vec!["spec-loop", "--spec", "001"]
            };
            let cli = crate::cli::Cli::try_parse_from(args).expect("arguments");
            let mut logs = None;
            let error = run_at(
                &cli,
                root.clone(),
                Some(Box::new(backend)),
                false,
                Some(&mut logs),
            )
            .expect_err("interrupted audit stops");
            assert!(matches!(error, SpecLoopError::Interrupted));
            assert_eq!(observed.lock().expect("calls").len(), 3);
            assert!(root.join("specs/001.md").is_file());
            assert_eq!(
                fs::read_to_string(root.join("user-change")).expect("user file"),
                "preserve me"
            );
            assert_eq!(
                fs::read_to_string(root.join(".spec-loop/plans/001.md")).expect("plan"),
                "# Approved plan\n"
            );
            for marker in ["archive/001.md", "success-ran", "finish-ran"] {
                assert!(!root.join(marker).exists());
            }
            if reviewer == PromptRole::Review {
                assert_eq!(
                    fs::read_to_string(root.join("checks-ran")).expect("check effects"),
                    "x"
                );
            }
            fs::remove_dir_all(logs.expect("logs")).expect("remove fixture logs");
        }
    }
}

#[cfg(unix)]
#[test]
fn every_prompt_collection_receives_filesystem_validation_before_invocation() {
    let (_temporary, root) = builtin_workflow_fixture();
    let base = fs::read_to_string(root.join(".spec-loop.toml")).expect("profile");
    for role in [
        "plan_author",
        "plan_review",
        "plan_audit",
        "plan_review_clarification",
        "implementation",
        "review",
        "review_audit",
        "review_clarification",
    ] {
        for path in [format!("missing-{role}.md"), ".".to_owned()] {
            let profile: Profile =
                toml::from_str(&format!("{base}\n[prompts]\n{role}=['{path}']\n"))
                    .expect("profile with prompt");
            let error = super::validate_resources(&root, &profile).expect_err("invalid resource");
            assert!(error.to_string().contains("prompts"));
            assert!(error.to_string().contains(&path));
        }
        fs::write(root.join(format!("{role}.md")), "instructions").expect("role file");
        let profile: Profile =
            toml::from_str(&format!("{base}\n[prompts]\n{role}=['{role}.md']\n"))
                .expect("profile with prompt");
        super::validate_resources(&root, &profile).expect("valid resource");
    }
}

#[test]
fn builtin_typed_correction_uses_continuation_and_correction_only_prompt() {
    let (_temporary, root) = builtin_workflow_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend = CorrectionContinuationBackend {
        calls: Arc::clone(&calls),
        implementation_calls: Mutex::new(0),
    };
    let cli = crate::cli::Cli::try_parse_from(["spec-loop", "--spec", "001"])
        .expect("parse workflow arguments");
    let mut run_dir_observer = None;
    run_at(
        &cli,
        root,
        Some(Box::new(backend)),
        false,
        Some(&mut run_dir_observer),
    )
    .expect("run correction workflow");

    let calls = calls.lock().expect("lock correction backend calls");
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0, PromptRole::Implementation);
    assert!(!calls[0].2);
    assert_eq!(calls[1].0, PromptRole::Implementation);
    assert!(calls[1].2);
    assert!(
        calls[1]
            .1
            .contains("Typed final response failed validation")
    );
    assert!(calls[1].1.contains("do not invoke tools"));
    assert!(calls[1].1.contains("Implement only selected spec"));
    let initial_instructions = calls[0]
        .1
        .split("\n\nWorkflow constraints:")
        .next()
        .expect("initial role instructions");
    let correction_instructions = calls[1]
        .1
        .split("\n\nWorkflow constraints:")
        .next()
        .expect("correction role instructions");
    assert_eq!(initial_instructions, correction_instructions);
    let schema_section = |prompt: &str| {
        prompt
            .split("--- Generated response schema ---\n")
            .nth(1)
            .and_then(|schema| {
                schema
                    .split("\n--- End generated response schema ---")
                    .next()
            })
            .expect("canonical role schema section")
            .to_owned()
    };
    assert_eq!(schema_section(&calls[0].1), schema_section(&calls[1].1));
    assert_eq!(calls[2].0, PromptRole::Review);
    drop(calls);
    fs::remove_dir_all(run_dir_observer.expect("workflow log path"))
        .expect("remove correction workflow logs");
}

#[test]
fn workflow_usage_isolated_by_spec_and_role_across_correction_continuation() {
    let (_temporary, root) = builtin_workflow_fixture();
    fs::write(
        root.join("specs/002.md"),
        "# Second selected specification\n",
    )
    .expect("write second spec");
    fs::write(
        root.join(".spec-loop/plans/002.md"),
        "# Second approved plan\n",
    )
    .expect("write second plan");
    let cli = crate::cli::Cli::try_parse_from(["spec-loop", "--continue", "--spec", "001"])
        .expect("parse continue workflow arguments");
    let baselines = Arc::new(Mutex::new(Vec::new()));
    let backend = ScopedUsageBackend {
        baselines: Arc::clone(&baselines),
        first_implementation: Mutex::new(false),
    };
    let mut run_dir_observer = None;
    run_at(
        &cli,
        root,
        Some(Box::new(backend)),
        false,
        Some(&mut run_dir_observer),
    )
    .expect("run both specs with scoped usage");
    let observed = baselines.lock().expect("lock observed usage baselines");
    assert_eq!(observed.len(), 5);
    assert_eq!(
        observed[0],
        (
            "001".to_owned(),
            PromptRole::Implementation,
            Some(0),
            Some(0),
            Some(0)
        )
    );
    assert_eq!(
        observed[1],
        (
            "001".to_owned(),
            PromptRole::Implementation,
            Some(1),
            Some(1),
            Some(1)
        )
    );
    assert_eq!(
        observed[2],
        (
            "001".to_owned(),
            PromptRole::Review,
            Some(0),
            Some(0),
            Some(0)
        )
    );
    assert_eq!(
        observed[3],
        (
            "002".to_owned(),
            PromptRole::Implementation,
            Some(0),
            Some(0),
            Some(0)
        )
    );
    assert_eq!(
        observed[4],
        (
            "002".to_owned(),
            PromptRole::Review,
            Some(0),
            Some(0),
            Some(0)
        )
    );
    drop(observed);
    fs::remove_dir_all(run_dir_observer.expect("workflow log path"))
        .expect("remove scoped usage workflow logs");
}

#[test]
fn builtin_workflow_retries_typed_response_preserves_prompt_and_writes_logs() {
    let (_temporary, root) = builtin_workflow_fixture();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut observed_run_dir = None;
    let run_dir = run_fake_builtin(
            &root,
            false,
            vec![
                (
                    PromptRole::Implementation,
                    r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"invalid","extra":true}"#.to_owned(),
                ),
                (
                    PromptRole::Implementation,
                    r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Implemented the requested change."}"#.to_owned(),
                ),
                (
                    PromptRole::Review,
                    r#"{"protocol_version":1,"role":"review","status":"no_issues","findings":[]}"#.to_owned(),
                ),
            ],
            Arc::clone(&observed),
            &mut observed_run_dir,
        )
        .expect("complete workflow with fake built-in backend")
        .expect("workflow should create run logs");

    let calls = observed.lock().expect("lock fake backend observations");
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0, PromptRole::Implementation);
    assert_eq!(calls[1].0, PromptRole::Implementation);
    assert_eq!(calls[2].0, PromptRole::Review);
    assert!(calls[0].1.contains("Implement only selected spec"));
    assert!(calls[1].1.contains("Implement only selected spec"));
    assert!(calls[1].1.contains("Typed response failed validation"));
    assert!(calls[0].1.contains("Selected spec path: specs/001.md"));
    drop(calls);

    assert_eq!(
        fs::read(run_dir.join("agent-0001-implementation.stdout.jsonl"))
            .expect("read first attempt transcript"),
        b"fake built-in provider transcript"
    );
    assert!(
        fs::read_to_string(run_dir.join("agent-0002-implementation.response.json"))
            .expect("read typed response log")
            .contains("Implemented the requested change")
    );
    fs::remove_dir_all(run_dir).expect("remove test run logs");
}

#[test]
fn builtin_workflow_routes_every_role_through_fake_backend() {
    let (_temporary, root) = builtin_workflow_fixture();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let finding = r#"{"id":1,"requirement":"exercise audit routing","location":"src/example.rs","evidence":"fixture evidence","violation":"fixture violation","proposed_correction":"no correction required"}"#;
    let mut observed_plan_run_dir = None;
    let run_plan_dir = run_fake_builtin(
            &root,
            true,
            vec![
                (
                    PromptRole::PlanAuthor,
                    r#"{"protocol_version":1,"role":"plan_author","status":"ready_for_review"}"#.to_owned(),
                ),
                (
                    PromptRole::PlanReview,
                    format!(r#"{{"protocol_version":1,"role":"plan_review","status":"issues","findings":[{finding}]}}"#),
                ),
                (
                    PromptRole::PlanAudit,
                    r#"{"protocol_version":1,"role":"plan_audit","status":"complete","classifications":[{"finding_id":1,"classification":"unsupported","actionability":"not_required","rationale":"Fixture finding is unsupported and needs no correction."}]}"#.to_owned(),
                ),
                (
                    PromptRole::PlanClarification,
                    r#"{"protocol_version":1,"role":"plan_review_clarification","status":"complete","evidence":[]}"#.to_owned(),
                ),
            ],
            Arc::clone(&observed),
            &mut observed_plan_run_dir,
        )
        .expect("run fake plan workflow")
        .expect("plan workflow log directory");
    let mut observed_implementation_run_dir = None;
    let run_implementation_dir = run_fake_builtin(
            &root,
            false,
            vec![
                (
                    PromptRole::Implementation,
                    r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Workflow fixture implementation."}"#.to_owned(),
                ),
                (
                    PromptRole::Review,
                    format!(r#"{{"protocol_version":1,"role":"review","status":"issues","findings":[{finding}]}}"#),
                ),
                (
                    PromptRole::ReviewAudit,
                    r#"{"protocol_version":1,"role":"review_audit","status":"complete","classifications":[{"finding_id":1,"classification":"unsupported","actionability":"not_required","rationale":"Fixture finding is unsupported and needs no correction."}]}"#.to_owned(),
                ),
                (
                    PromptRole::ReviewClarification,
                    r#"{"protocol_version":1,"role":"review_clarification","status":"complete","evidence":[]}"#.to_owned(),
                ),
            ],
            Arc::clone(&observed),
            &mut observed_implementation_run_dir,
        )
        .expect("run fake implementation workflow")
        .expect("implementation workflow log directory");

    let calls = observed.lock().expect("lock fake backend observations");
    assert_eq!(
        calls.iter().map(|(role, _)| *role).collect::<Vec<_>>(),
        [
            PromptRole::PlanAuthor,
            PromptRole::PlanReview,
            PromptRole::PlanAudit,
            PromptRole::PlanClarification,
            PromptRole::Implementation,
            PromptRole::Review,
            PromptRole::ReviewAudit,
            PromptRole::ReviewClarification,
        ]
    );
    drop(calls);
    fs::remove_dir_all(run_plan_dir).expect("remove fake plan logs");
    fs::remove_dir_all(run_implementation_dir).expect("remove fake implementation logs");
}

#[test]
fn builtin_workflow_fatal_tool_limit_does_not_retry_role() {
    let (_temporary, root) = builtin_workflow_fixture();
    let cli = crate::cli::Cli::try_parse_from(["spec-loop", "--spec", "001"])
        .expect("parse workflow arguments");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut run_dir_observer = None;
    let result = run_at(
            &cli,
            root,
            Some(Box::new(FatalToolLimitBackend {
                calls: Arc::clone(&calls),
                diagnostic: "built-in agent reached agent.max_tool_calls=1 before another shell call; role stopped without replay".to_owned(),
            })),
            false,
            Some(&mut run_dir_observer),
        );
    let error = result.expect_err("fatal tool limit must stop workflow");
    assert!(error.to_string().contains("agent.max_tool_calls=1"));
    assert_eq!(
        *calls.lock().expect("lock fatal backend calls"),
        [PromptRole::Implementation]
    );
    fs::remove_dir_all(run_dir_observer.expect("fatal run retains logs"))
        .expect("remove fatal workflow logs");
}

#[test]
fn invalid_shell_call_limits_fail_before_agent_invocation() {
    for (backend, limit, expected_diagnostic) in [
        ("builtin", 0, "must be positive when set"),
        ("pi", 1, "only valid for the built-in backend"),
    ] {
        let (_temporary, root) = builtin_workflow_fixture();
        fs::write(
            root.join(".spec-loop.toml"),
            format!(
                "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[plan]\npath_template = '.spec-loop/plans/{{spec_id}}.md'\n[agent]\nbackend = '{backend}'\nmodel = 'fixture-model'\nmax_tool_calls = {limit}\n"
            ),
        )
        .expect("write invalid call-limit profile");
        let cli = crate::cli::Cli::try_parse_from(["spec-loop", "--spec", "001"])
            .expect("parse workflow arguments");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let result = run_at(
            &cli,
            root,
            Some(Box::new(FakeBuiltinBackend {
                responses: Mutex::new(VecDeque::new()),
                observed: Arc::clone(&observed),
            })),
            false,
            None,
        );
        let error = result.expect_err("invalid shell call limit must fail profile validation");
        assert!(error.to_string().contains(expected_diagnostic));
        assert!(
            observed
                .lock()
                .expect("lock backend observations")
                .is_empty()
        );
    }
}

#[test]
fn builtin_workflow_fatal_auth_admission_does_not_retry_role() {
    let (_temporary, root) = builtin_workflow_fixture();
    let cli = crate::cli::Cli::try_parse_from(["spec-loop", "--spec", "001"])
        .expect("parse workflow arguments");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut run_dir_observer = None;
    let result = run_at(
        &cli,
        root,
        Some(Box::new(FatalToolLimitBackend {
            calls: Arc::clone(&calls),
            diagnostic: "ChatGPT direct-use permission denied (HTTP 403)".to_owned(),
        })),
        false,
        Some(&mut run_dir_observer),
    );
    let error = result.expect_err("fatal admission denial must stop workflow");
    assert!(error.to_string().contains("HTTP 403"));
    assert_eq!(
        *calls.lock().expect("lock fatal backend calls"),
        [PromptRole::Implementation]
    );
    fs::remove_dir_all(run_dir_observer.expect("fatal run retains logs"))
        .expect("remove fatal workflow logs");
}

#[test]
fn builtin_workflow_blocked_response_stops_before_review() {
    let (_temporary, root) = builtin_workflow_fixture();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut observed_run_dir = None;
    let result = run_fake_builtin(
            &root,
            false,
            vec![(
                PromptRole::Implementation,
                r#"{"protocol_version":1,"role":"implementation","status":"blocked","summary":"No changes made; required input unavailable.","reason":"required external approval unavailable"}"#.to_owned(),
            ), adjudication_response(PromptRole::ReviewAudit, "confirmed")],
            Arc::clone(&observed),
            &mut observed_run_dir,
        );
    assert!(result.is_err());
    let calls = observed.lock().expect("lock fake backend observations");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, PromptRole::Implementation);
    assert_eq!(calls[1].0, PromptRole::ReviewAudit);
    drop(calls);
    fs::remove_dir_all(observed_run_dir.expect("blocked workflow retained its log path"))
        .expect("remove blocked workflow logs");
}

#[test]
fn child_output_excerpt_keeps_last_four_nonempty_lines_and_bounds_lines() {
    let stdout = format!("first\n\nsecond\nthird\nfourth\n{}\n", "x".repeat(301));
    let excerpt = output_excerpt(stdout.as_bytes(), b"stderr line\n");
    assert!(excerpt.contains("... 1 earlier lines omitted ..."));
    assert!(!excerpt.contains("first"));
    assert!(excerpt.contains("second\nthird\nfourth\n"));
    assert!(excerpt.contains(&format!("{} ... [line truncated]", "x".repeat(300))));
    assert!(excerpt.contains("stderr excerpt:\nstderr line\n"));
}

#[test]
fn display_child_output_excerpt_preserves_crlf_carriage_return() {
    let raw = b"child output\r\nnext line\n";
    let prompt_excerpt = output_excerpt(raw, b"");
    let display_excerpt = display_output_excerpt(raw, b"");

    assert_eq!(prompt_excerpt, "stdout excerpt:\nchild output\nnext line\n");
    assert_eq!(
        display_excerpt,
        "stdout excerpt:\nchild output\r\nnext line\n"
    );
    assert_eq!(raw, b"child output\r\nnext line\n");
}

#[test]
fn archive_path_is_filesystem_policy_without_git_checks() {
    let temporary = TempDir::new().expect("project");
    fs::create_dir_all(temporary.path().join("archive")).expect("archive directory");
    fs::create_dir_all(temporary.path().join("specs")).expect("spec directory");
    fs::write(temporary.path().join("specs/001.md"), "spec").expect("spec file");
    let profile: Profile = toml::from_str(
            "schema_version=2\n[specs]\nactive_globs=['specs/*.md']\nid_regex='^(?:specs|archive)/(?P<id>[0-9]+)\\.md$'\n[archive]\nenabled=true\ndestination='archive'\n",
        )
        .expect("profile");
    let candidate = crate::discovery::discover_specs(temporary.path(), &profile)
        .expect("discover spec")
        .into_iter()
        .next()
        .expect("candidate");
    assert_eq!(
        archive_destination(temporary.path(), &profile, &candidate)
            .expect("archive destination")
            .as_deref(),
        Some(Path::new("archive/001.md"))
    );
}

#[test]
fn executable_check_rejects_missing_program() {
    assert!(!is_executable_file(Path::new("/no/such/program")));
}

#[cfg(unix)]
#[test]
fn check_executable_templates_are_expanded_before_preflight() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = TempDir::new().expect("project");
    fs::create_dir_all(temporary.path().join("specs")).expect("specs directory");
    fs::create_dir_all(temporary.path().join("tools")).expect("tools directory");
    fs::write(temporary.path().join("specs/001.md"), "spec").expect("spec");
    let executable = temporary.path().join("tools/check-001");
    fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("check executable");
    let mut permissions = fs::metadata(&executable)
        .expect("check metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).expect("set executable mode");

    let profile: Profile = toml::from_str(
            "schema_version=2\n[specs]\nactive_globs=['specs/*.md']\nid_regex='^specs/(?P<id>[0-9]+)\\.md$'\n[[checks]]\nname='templated'\nargv=['tools/check-{spec_id}', '^[0-9]{3}$', '{literal']\n",
        )
        .expect("profile");
    let candidate = crate::discovery::discover_specs(temporary.path(), &profile)
        .expect("discover spec")
        .into_iter()
        .next()
        .expect("candidate");

    validate_check_executables(
        temporary.path(),
        &profile,
        &candidate,
        Path::new(".spec-loop/plans/001.md"),
    )
    .expect("preflight expanded executable and preserved literal brace args");
}

#[cfg(unix)]
#[test]
fn archived_spec_executable_is_preflighted_at_source_path() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = TempDir::new().expect("project");
    fs::create_dir_all(temporary.path().join("specs")).expect("specs directory");
    fs::create_dir_all(temporary.path().join("archive")).expect("archive directory");
    let source = temporary.path().join("specs/001.md");
    fs::write(&source, "#!/bin/sh\nexit 0\n").expect("executable spec");
    let mut permissions = fs::metadata(&source).expect("spec metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&source, permissions).expect("set executable mode");
    let profile: Profile = toml::from_str(
            "schema_version=2\n[specs]\nactive_globs=['specs/*.md']\nid_regex='^specs/(?P<id>[0-9]+)\\.md$'\n[archive]\nenabled=true\ndestination='archive'\n[[actions.on_review_success]]\nname='run selected spec'\nargv=['{spec_path}']\n",
        )
        .expect("profile");
    let candidate = crate::discovery::discover_specs(temporary.path(), &profile)
        .expect("discover spec")
        .into_iter()
        .next()
        .expect("candidate");
    let files = CandidateFiles {
        plan_path: PathBuf::from(".spec-loop/plans/001.md"),
        has_plan: true,
        archive_path: Some(PathBuf::from("archive/001.md")),
    };

    validate_action_executables(temporary.path(), &profile, &candidate, &files)
        .expect("preflight validates source before archive rename");
    assert!(!temporary.path().join("archive/001.md").exists());
}

#[test]
fn implementation_entry_transitions_from_review_resume_to_implementation() {
    let mut entry = ImplementationEntry::ResumeReview;
    assert_eq!(entry.take_for_pass(), ImplementationEntry::ResumeReview);
    assert_eq!(entry, ImplementationEntry::Implement);
    assert_eq!(entry.take_for_pass(), ImplementationEntry::Implement);
    assert_eq!(entry, ImplementationEntry::Implement);

    let mut ordinary = ImplementationEntry::Implement;
    assert_eq!(ordinary.take_for_pass(), ImplementationEntry::Implement);
    assert_eq!(ordinary, ImplementationEntry::Implement);
}

#[cfg(unix)]
const NON_AUDITORS: [PromptRole; 6] = [
    PromptRole::PlanAuthor,
    PromptRole::PlanReview,
    PromptRole::PlanClarification,
    PromptRole::Implementation,
    PromptRole::Review,
    PromptRole::ReviewClarification,
];

#[cfg(unix)]
type RoleScript = Vec<(PromptRole, String)>;

#[cfg(unix)]
fn role_blocker_script(origin: PromptRole) -> (bool, RoleScript, RoleScript) {
    let planning = matches!(
        origin,
        PromptRole::PlanAuthor | PromptRole::PlanReview | PromptRole::PlanClarification
    );
    let worker = if planning {
        PromptRole::PlanAuthor
    } else {
        PromptRole::Implementation
    };
    let reviewer = if planning {
        PromptRole::PlanReview
    } else {
        PromptRole::Review
    };
    let ready = blocker_response(
        worker,
        if planning {
            json!({"status":"ready_for_review"})
        } else {
            json!({"status":"ready_for_review","summary":"Fixture work completed."})
        },
    );
    let approval = blocker_response(reviewer, json!({"status":"no_issues","findings":[]}));
    if origin == worker {
        return (planning, Vec::new(), vec![ready, approval]);
    }
    if origin == reviewer {
        return (planning, vec![ready], vec![approval]);
    }
    let auditor = if planning {
        PromptRole::PlanAudit
    } else {
        PromptRole::ReviewAudit
    };
    let finding = json!({"id":1,"requirement":"required behavior","location":"src/example.rs:10","evidence":"observed fixture condition","violation":"required behavior is absent","proposed_correction":"implement required behavior"});
    (
        planning,
        vec![
            ready,
            blocker_response(reviewer, json!({"status":"issues","findings":[finding]})),
            blocker_response(
                auditor,
                json!({"status":"complete","classifications":[{"finding_id":1,"classification":"unsupported","actionability":"not_required","rationale":"Fixture evidence disproves claim"}]}),
            ),
        ],
        vec![blocker_response(
            origin,
            json!({"status":"complete","evidence":[]}),
        )],
    )
}

#[cfg(unix)]
#[test]
fn every_non_auditor_blocker_routes_and_preserves_gates_under_both_backends() {
    for backend in ["builtin", "pi"] {
        for origin in NON_AUDITORS {
            for outcome in [
                "unsupported",
                "confirmed",
                "ambiguous",
                "auditor_blocked",
                "renewed",
                "malformed",
                "exhausted",
            ] {
                exercise_role_blocker(backend, origin, outcome);
            }
        }
    }
}

#[cfg(unix)]
fn exercise_role_blocker(backend: &str, origin: PromptRole, outcome: &str) {
    let (planning, mut script, corrected) = role_blocker_script(origin);
    let clarification = matches!(
        origin,
        PromptRole::PlanClarification | PromptRole::ReviewClarification
    );
    let (_temporary, root) = blocker_fixture(
        backend,
        if outcome == "exhausted" {
            if clarification { 2 } else { 1 }
        } else {
            6
        },
    );
    let prefix = script.len();
    let auditor = if planning {
        PromptRole::PlanAudit
    } else {
        PromptRole::ReviewAudit
    };
    let mut blocked = json!({"status":"blocked","reason": if outcome == "confirmed" {
                    "Required external approval unavailable despite available tools."
                } else {"No actionable gaps and no genuine external blocker."}});
    if origin == PromptRole::Implementation {
        blocked["summary"] = json!("Prior work preserved.");
    }
    let original = blocker_response(origin, blocked);
    script.push(original.clone());
    if outcome == "malformed" {
        script.push(blocker_response(auditor, json!({"status":"complete","classifications":[{"finding_id":1,"classification":"supported","actionability":"required","rationale":"Wrong audit kind"}]})));
        script.push(blocker_response(auditor, json!({"status":"adjudicated","adjudication":{"outcome":"unsupported","rationale":"Missing evidence"}})));
    }
    script.push(if outcome == "auditor_blocked" {
        blocker_response(
            auditor,
            json!({"status":"blocked","reason":"Audit evidence unavailable"}),
        )
    } else {
        adjudication_response(
            auditor,
            if ["renewed", "exhausted", "malformed"].contains(&outcome) {
                "unsupported"
            } else {
                outcome
            },
        )
    });
    if ["unsupported", "malformed"].contains(&outcome) {
        script.extend(corrected);
    }
    if outcome == "exhausted" && clarification {
        script.push(original.clone());
    }
    if outcome == "renewed" {
        script.push(original.clone());
        script.push(adjudication_response(auditor, "ambiguous"));
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut logs = None;
    let result = run_fake_builtin(&root, planning, script, Arc::clone(&calls), &mut logs);
    if ["unsupported", "malformed"].contains(&outcome) {
        result.expect("origin must correct its own outcome");
    } else {
        let error = result.expect_err("unresolved blocker stops").to_string();
        let expected = match outcome {
            "confirmed" => "Provide approval",
            "ambiguous" | "renewed" => "Is the required approval available?",
            "auditor_blocked" => "auditor blocked",
            "exhausted" => "completion.max_passes",
            _ => panic!("outcome"),
        };
        assert!(error.contains(expected), "{error}");
        assert!(root.join("specs/001.md").is_file());
        assert!(!root.join("success-ran").exists());
        assert!(!root.join("finish-ran").exists());
    }
    let calls = calls.lock().expect("calls");
    assert_blocker_handoff(origin, prefix, outcome, &original.1, &calls);
    let checks = root.join("checks-ran");
    if checks.exists() {
        assert_eq!(fs::read_to_string(checks).expect("checks ran once"), "x");
    }
    assert_eq!(
        fs::read_to_string(root.join("user-change")).expect("user work"),
        "preserve me"
    );
    drop(calls);
    fs::remove_dir_all(logs.expect("logs retained")).expect("remove logs");
}

#[cfg(unix)]
fn assert_blocker_handoff(
    origin: PromptRole,
    prefix: usize,
    outcome: &str,
    original: &str,
    calls: &[(PromptRole, String)],
) {
    let planning = matches!(
        origin,
        PromptRole::PlanAuthor | PromptRole::PlanReview | PromptRole::PlanClarification
    );
    let auditor = if planning {
        PromptRole::PlanAudit
    } else {
        PromptRole::ReviewAudit
    };
    assert_eq!(calls.get(prefix).expect("origin").0, origin);
    let audit = calls.get(prefix + 1).expect("audit");
    assert_eq!(audit.0, auditor);
    let input = feedback_json(&audit.1);
    assert_eq!(input["audit_kind"], "agent_blocker");
    assert_eq!(
        input["response"],
        serde_json::from_str::<serde_json::Value>(original).expect("original")
    );
    assert_eq!(audit.1.contains("Review-context command argv"), !planning);
    if ["unsupported", "renewed", "malformed"].contains(&outcome) {
        let extra = if outcome == "malformed" {
            assert_eq!(calls.get(prefix + 2).expect("same auditor").0, auditor);
            assert_eq!(calls.get(prefix + 3).expect("same auditor").0, auditor);
            assert!(
                calls
                    .get(prefix + 2)
                    .expect("kind retry")
                    .1
                    .contains("requires blocker adjudication")
            );
            assert!(
                calls
                    .get(prefix + 3)
                    .expect("shape retry")
                    .1
                    .contains("missing fields")
            );
            2
        } else {
            0
        };
        let resumed = calls.get(prefix + 2 + extra).expect("same role resumes");
        assert_eq!(resumed.0, origin);
        assert_eq!(
            feedback_json(&resumed.1)["blocker_audit"]["outcome"],
            "unsupported"
        );
        assert!(
            resumed
                .1
                .contains("use available backend tools to complete unfinished required work")
        );
        assert!(resumed.1.contains("Selected spec path: specs/001.md"));
        assert!(
            resumed
                .1
                .contains("Selected plan path: .spec-loop/plans/001.md")
        );
        if matches!(
            origin,
            PromptRole::PlanClarification | PromptRole::ReviewClarification
        ) {
            assert!(resumed.1.contains("\"requested_finding_ids\":[1]"));
            assert!(resumed.1.contains("required behavior"));
        }
    }
}

#[cfg(unix)]
struct NativeContextBackend {
    inner: FakeBuiltinBackend,
    origin: PromptRole,
    interrupt_audit: bool,
}

#[cfg(unix)]
impl AgentBackend for NativeContextBackend {
    fn invoke(
        &self,
        root: &Path,
        profile: &Profile,
        prompt: &str,
        role: PromptRole,
        cancelled: &Arc<std::sync::atomic::AtomicBool>,
        invocation: crate::backend::AgentInvocation<'_>,
    ) -> Result<AgentRun> {
        if self.interrupt_audit && matches!(role, PromptRole::PlanAudit | PromptRole::ReviewAudit) {
            return Err(SpecLoopError::Interrupted);
        }
        let mut initial = false;
        if role == self.origin {
            assert!(
                invocation.continuation.is_none(),
                "unsupported resumes must allow tools, not correction-only continuations"
            );
            for required in [
                "native-task-sentinel",
                "repair-finding-sentinel",
                "check-diagnostic-sentinel",
                "\"requested_finding_ids\":[3,1]",
                "previous-feedback-sentinel",
            ] {
                assert!(prompt.contains(required), "lost {required}: {role:?}");
            }
            assert!(!root.join("success-ran").exists());
            assert!(!root.join("finish-ran").exists());
            assert!(!root.join("archive/001.md").exists());
            let completed = root.join("completed-tool-effect");
            initial = !completed.exists();
            if initial {
                fs::write(completed, "once").expect("fake initial tool work");
            } else {
                assert_eq!(fs::read_to_string(completed).expect("prior effect"), "once");
                assert!(prompt.contains("--- Blocker adjudication feedback ---"));
                fs::write(root.join("unfinished-tool-effect"), "completed")
                    .expect("tools-enabled unfinished work");
            }
        }
        let mut run = self
            .inner
            .invoke(root, profile, prompt, role, cancelled, invocation)?;
        if initial {
            // Even a backend returning resumable state with a valid blocker must not force
            // the unsupported-blocker resume onto the no-tools JSON correction path.
            run.continuation = Some(BackendContinuation::new(71_u8));
        }
        Ok(run)
    }
}

#[cfg(unix)]
fn call_native_origin(
    context: &mut super::WorkflowContext<'_>,
    request: super::AgentRequest<'_>,
) -> Result<()> {
    use crate::protocol::{
        ImplementationClarificationResponse, ImplementationResponse, ImplementationReviewResponse,
        PlanAuthorResponse, PlanClarificationResponse, PlanReviewResponse,
    };
    match request.role {
        PromptRole::PlanAuthor => {
            super::agents::call_typed::<PlanAuthorResponse>(context, request).map(|_| ())
        }
        PromptRole::PlanReview => {
            super::agents::call_typed::<PlanReviewResponse>(context, request).map(|_| ())
        }
        PromptRole::PlanClarification => {
            super::agents::call_typed::<PlanClarificationResponse>(context, request).map(|_| ())
        }
        PromptRole::Implementation => {
            super::agents::call_typed::<ImplementationResponse>(context, request).map(|_| ())
        }
        PromptRole::Review => {
            super::agents::call_typed::<ImplementationReviewResponse>(context, request).map(|_| ())
        }
        PromptRole::ReviewClarification => {
            super::agents::call_typed::<ImplementationClarificationResponse>(context, request)
                .map(|_| ())
        }
        _ => panic!("non-auditor fixture only"),
    }
}

#[cfg(unix)]
#[test]
fn all_origin_resumes_retain_native_inputs_enable_tools_and_preserve_completed_effects() {
    for backend in ["builtin", "pi"] {
        for origin in NON_AUDITORS {
            for interrupt_audit in [false, true] {
                exercise_native_context(backend, origin, interrupt_audit);
            }
        }
    }
}

#[cfg(unix)]
fn exercise_native_context(backend: &str, origin: PromptRole, interrupt_audit: bool) {
    let (planning, _, corrected) = role_blocker_script(origin);
    let (_temporary, root) = blocker_fixture(backend, 6);
    let cli = crate::cli::Cli::try_parse_from(if planning {
        vec!["spec-loop", "--plan", "--spec", "001"]
    } else {
        vec!["spec-loop", "--spec", "001"]
    })
    .expect("cli");
    let prepared = super::prepare_run(&cli, root.clone()).expect("prepare workflow");
    let run_dir = crate::run_dir::RunDir::create(&root).expect("run logs");
    let auditor = if planning {
        PromptRole::PlanAudit
    } else {
        PromptRole::ReviewAudit
    };
    let mut blocked =
        json!({"status":"blocked","reason":"No actionable gaps and no genuine external blocker."});
    if origin == PromptRole::Implementation {
        blocked["summary"] = json!("Initial work preserved.");
    }
    let responses = vec![
        blocker_response(origin, blocked),
        adjudication_response(auditor, "unsupported"),
        corrected.first().expect("origin correction").clone(),
    ];
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut context = super::WorkflowContext {
        root: &root,
        profile: &prepared.profile,
        run_dir: &run_dir,
        cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_passes: 6,
        role_calls: std::collections::BTreeMap::new(),
        role_usage: std::collections::BTreeMap::new(),
        log_index: 0,
        integrity: prepared.integrity,
        backend: Box::new(NativeContextBackend {
            inner: FakeBuiltinBackend {
                responses: Mutex::new(responses.into()),
                observed: Arc::clone(&observed),
            },
            origin,
            interrupt_audit,
        }),
        output: crate::output::Output::new(crate::output::ColorMode::Never, false),
    };
    let result = call_native_origin(
        &mut context,
        super::AgentRequest {
            candidate: &prepared.candidate,
            role: origin,
            plan_path: Some(&prepared.files.plan_path),
            feedback: Some(
                r#"{"task":"native-task-sentinel","actionable_findings":["repair-finding-sentinel"],"requested_finding_ids":[3,1]}"#,
            ),
            checks: "check-diagnostic-sentinel",
            validation_feedback: Some("previous-feedback-sentinel"),
        },
    );
    if interrupt_audit {
        assert!(matches!(result, Err(SpecLoopError::Interrupted)));
    } else {
        result.expect("tools-enabled originating role finishes");
    }
    assert_eq!(
        root.join("unfinished-tool-effect").exists(),
        !interrupt_audit
    );
    assert_eq!(
        fs::read_to_string(root.join("completed-tool-effect")).expect("prior tool work"),
        "once"
    );
    assert!(root.join("specs/001.md").is_file());
    context
        .integrity
        .verify(&root)
        .expect("selected inputs unchanged");
    drop(context);
    fs::remove_dir_all(run_dir.root).expect("remove logs");
}

#[cfg(unix)]
fn grounding_responses(planning: bool, cited: bool) -> RoleScript {
    let reviewer = if planning {
        PromptRole::PlanReview
    } else {
        PromptRole::Review
    };
    let auditor = if planning {
        PromptRole::PlanAudit
    } else {
        PromptRole::ReviewAudit
    };
    let clarification = if planning {
        PromptRole::PlanClarification
    } else {
        PromptRole::ReviewClarification
    };
    let initial = blocker_initial(reviewer);
    let findings: Vec<_> = (1..=3).map(|id| json!({"id":id,"requirement":"required behavior","location":"src/example.rs:10","evidence":"observed fixture condition","violation":"required behavior is absent","proposed_correction":"implement required behavior"})).collect();
    let assessment = |id, classification, actionability| json!({"finding_id":id,"classification":classification,"actionability":actionability,"rationale":"Fixture assessment"});
    let classifications = vec![
        assessment(1, "supported", "required"),
        assessment(2, "unsupported", "not_required"),
        assessment(3, "ambiguous", "ambiguous"),
    ];
    let audit = |items: Vec<serde_json::Value>| {
        blocker_response(
            auditor,
            json!({"status":"complete","classifications":items}),
        )
    };
    let mut script = vec![
        initial.clone(),
        blocker_response(reviewer, json!({"status":"issues","findings":findings})),
        audit(vec![classifications[0].clone()]),
        audit(classifications.iter().rev().cloned().collect()),
        audit(classifications),
        blocker_response(
            clarification,
            json!({"status":"complete","evidence":[{"finding_id":4,"citations":[]}]}),
        ),
        blocker_response(
            clarification,
            json!({"status":"complete","evidence":[{"finding_id":3,"citations":if cited {json!([{"path":"src/example.rs","location":"line 10","excerpt":"fixture source excerpt","relevance":"supports the requested evidence"}])} else {json!([])}}]}),
        ),
    ];
    if cited {
        script.push(audit(vec![assessment(2, "supported", "not_required")]));
        script.push(audit(vec![assessment(3, "supported", "not_required")]));
    }
    if cited || !planning {
        script.push(initial);
        script.push(blocker_response(
            reviewer,
            json!({"status":"no_issues","findings":[]}),
        ));
    }
    script
}

#[cfg(unix)]
#[test]
fn grounding_retries_preserve_selection_merge_order_and_distinct_ambiguity_policy() {
    for backend in ["builtin", "pi"] {
        for planning in [false, true] {
            for cited in [false, true] {
                let (_temporary, root) = blocker_fixture(backend, 6);
                let calls = Arc::new(Mutex::new(Vec::new()));
                let mut logs = None;
                let result = run_fake_builtin(
                    &root,
                    planning,
                    grounding_responses(planning, cited),
                    Arc::clone(&calls),
                    &mut logs,
                );
                if planning && !cited {
                    assert!(
                        result
                            .expect_err("plan ambiguity stops")
                            .to_string()
                            .contains("audit remains ambiguous")
                    );
                } else {
                    result.expect("grounding and repair complete");
                }
                let calls = calls.lock().expect("calls");
                let clarification = feedback_json(&calls[5].1);
                assert_eq!(clarification["requested_finding_ids"], json!([2, 3]));
                assert_eq!(
                    clarification["findings"]
                        .as_array()
                        .expect("requested findings")
                        .iter()
                        .map(|finding| finding["id"].as_u64().expect("id"))
                        .collect::<Vec<_>>(),
                    [2, 3]
                );
                assert!(
                    calls[3]
                        .1
                        .contains("audit returned 1 classifications for 3 findings")
                );
                assert!(
                    calls[4]
                        .1
                        .contains("audit classifications must match finding order")
                );
                assert!(
                    calls[6]
                        .1
                        .contains("evidence refers to unrequested finding 4")
                );
                if cited {
                    let follow_up = feedback_json(&calls[7].1);
                    assert_eq!(
                        follow_up["findings"]
                            .as_array()
                            .expect("cited findings")
                            .len(),
                        1
                    );
                    assert_eq!(follow_up["findings"][0]["id"], 3);
                    assert_eq!(follow_up["evidence"]["evidence"][0]["finding_id"], 3);
                    assert!(
                        calls[8]
                            .1
                            .contains("audit classifications must match finding order")
                    );
                }
                assert_grounding_repair(&calls, cited, planning, &root);
                assert_eq!(
                    calls.len(),
                    if cited {
                        11
                    } else if planning {
                        7
                    } else {
                        9
                    }
                );
                drop(calls);
                fs::remove_dir_all(logs.expect("logs")).expect("remove logs");
            }
        }
    }
}

#[cfg(unix)]
fn assert_grounding_repair(
    calls: &[(PromptRole, String)],
    cited: bool,
    planning: bool,
    root: &Path,
) {
    if cited || !planning {
        let repair = feedback_json(&calls[if cited { 9 } else { 7 }].1);
        assert_eq!(
            repair["actionable_findings"]
                .as_array()
                .expect("required repairs")
                .len(),
            1
        );
        assert_eq!(repair["actionable_findings"][0]["id"], 1);
        if !planning {
            assert_eq!(
                repair["audit"]
                    .as_array()
                    .expect("merged audit")
                    .iter()
                    .map(|item| item["finding_id"].as_u64().expect("id"))
                    .collect::<Vec<_>>(),
                [1, 2, 3]
            );
            assert_eq!(
                repair["ambiguous_findings"]
                    .as_array()
                    .expect("ambiguity")
                    .len(),
                usize::from(!cited)
            );
            assert_eq!(
                fs::read_to_string(root.join("checks-ran")).expect("two real repair passes"),
                "xx"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn clarification_ids_keep_unsupported_then_ambiguous_order_but_findings_keep_source_order() {
    for backend in ["builtin", "pi"] {
        for planning in [false, true] {
            let (_temporary, root) = blocker_fixture(backend, 6);
            let mut responses = grounding_responses(planning, false);
            let audit = responses.get_mut(4).expect("initial valid audit");
            let mut value: serde_json::Value =
                serde_json::from_str(&audit.1).expect("audit fixture");
            value["classifications"][0]["classification"] = json!("ambiguous");
            value["classifications"][0]["actionability"] = json!("ambiguous");
            value["classifications"][2]["classification"] = json!("supported");
            value["classifications"][2]["actionability"] = json!("required");
            audit.1 = value.to_string();
            let clarification = responses.get_mut(6).expect("valid evidence response");
            *clarification =
                blocker_response(clarification.0, json!({"status":"complete","evidence":[]}));
            let observed = Arc::new(Mutex::new(Vec::new()));
            let mut logs = None;
            let result =
                run_fake_builtin(&root, planning, responses, Arc::clone(&observed), &mut logs);
            assert_eq!(result.is_ok(), !planning);
            let calls = observed.lock().expect("calls");
            let request = feedback_json(&calls[5].1);
            assert_eq!(request["requested_finding_ids"], json!([2, 1]));
            assert_eq!(request["findings"][0]["id"], 1);
            assert_eq!(request["findings"][1]["id"], 2);
            drop(calls);
            fs::remove_dir_all(logs.expect("logs")).expect("remove logs");
        }
    }
}
