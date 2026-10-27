use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Parser;
use tempfile::TempDir;

use super::execution::{display_output_excerpt, output_excerpt};
use super::{
    CandidateFiles, ImplementationEntry, archive_destination, run_at, validate_action_executables,
    validate_check_executables,
};
use crate::backend::{AgentBackend, AgentResponse, AgentRun, BackendContinuation, BackendFailure};
use crate::config::Profile;
use crate::error::{Result, SpecLoopError};
use crate::paths::is_executable_file;
use crate::prompts::PromptRole;

struct FakeBuiltinBackend {
    responses: Mutex<VecDeque<(PromptRole, String)>>,
    observed: Arc<Mutex<Vec<(PromptRole, String)>>>,
}

impl AgentBackend for FakeBuiltinBackend {
    fn run(
        &self,
        _root: &Path,
        _profile: &Profile,
        prompt: &str,
        role: PromptRole,
        _cancelled: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<AgentRun> {
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

impl AgentBackend for CorrectionContinuationBackend {
    fn run(
        &self,
        _root: &Path,
        _profile: &Profile,
        prompt: &str,
        role: PromptRole,
        _cancelled: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<AgentRun> {
        self.calls
            .lock()
            .expect("lock correction backend calls")
            .push((role, prompt.to_owned(), false));
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
            PromptRole::Implementation if first_implementation_call => (
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
        })
    }

    fn run_with_continuation(
        &self,
        root: &Path,
        profile: &Profile,
        prompt: &str,
        role: PromptRole,
        cancelled: &Arc<std::sync::atomic::AtomicBool>,
        invocation: crate::backend::AgentInvocation<'_>,
    ) -> Result<AgentRun> {
        if let Some(continuation) = invocation.continuation {
            assert_eq!(
                continuation.into_state::<u8>().expect("continuation state"),
                17
            );
            self.calls
                .lock()
                .expect("lock correction backend calls")
                .push((role, prompt.to_owned(), true));
            return Ok(AgentRun {
                    outcome: Ok(AgentResponse {
                        response: r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","summary":"Corrected response."}"#.to_owned(),
                        selected_model: Some("fixture-model".to_owned()),
                        response_model: None,
                    }),
                    continuation: None,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
        }
        self.run(root, profile, prompt, role, cancelled)
    }
}

impl AgentBackend for FatalToolLimitBackend {
    fn run(
        &self,
        _root: &Path,
        profile: &Profile,
        _prompt: &str,
        role: PromptRole,
        _cancelled: &Arc<std::sync::atomic::AtomicBool>,
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
    assert!(!calls[1].1.contains("Implement only selected spec"));
    assert_eq!(calls[2].0, PromptRole::Review);
    drop(calls);
    fs::remove_dir_all(run_dir_observer.expect("workflow log path"))
        .expect("remove correction workflow logs");
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
            )],
            Arc::clone(&observed),
            &mut observed_run_dir,
        );
    assert!(result.is_err());
    let calls = observed.lock().expect("lock fake backend observations");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, PromptRole::Implementation);
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
