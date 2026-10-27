#![cfg(unix)]
#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use tempfile::TempDir;

struct Fixture {
    temporary: TempDir,
    root: PathBuf,
    events: PathBuf,
    agent_events: PathBuf,
    prompts: PathBuf,
    review_context: PathBuf,
}

impl Fixture {
    fn new(archive: bool, multiple_specs: bool) -> Self {
        let temporary = TempDir::new().expect("create fixture temp directory");
        let root = temporary.path().join("project");
        let tools = temporary.path().join("tools");
        let responses = temporary.path().join("responses");
        fs::create_dir_all(root.join("specs")).expect("create specs directory");
        fs::create_dir_all(root.join(".spec-loop/plans")).expect("create plans directory");
        fs::create_dir_all(&tools).expect("create tools directory");
        fs::create_dir_all(&responses).expect("create response directory");
        fs::write(root.join("specs/001.md"), "# Spec one\n").expect("write spec");
        fs::write(root.join(".spec-loop/plans/001.md"), "# Approved plan\n").expect("write plan");
        if multiple_specs {
            fs::write(root.join("specs/002.md"), "# Spec two\n").expect("write second spec");
            fs::write(
                root.join(".spec-loop/plans/002.md"),
                "# Approved plan two\n",
            )
            .expect("write second plan");
        }
        if archive {
            fs::create_dir(root.join("archive")).expect("create archive directory");
        }

        let events = temporary.path().join("commands.log");
        let agent_events = temporary.path().join("agents.log");
        let prompts = temporary.path().join("prompts.log");
        let review_context = tools.join("review-context");
        write_executable(
            &review_context,
            &format!(
                "#!/bin/sh\nprintf 'ran\\n' >> {}\n",
                shell_quote(&temporary.path().join("context-ran"))
            ),
        );

        write_fake_responses(&responses);

        let fake_pi = tools.join("fake-pi");
        write_fake_pi(
            &fake_pi,
            &responses,
            &prompts,
            &agent_events,
            temporary.path(),
        );

        write_fake_commands(&tools, &events);
        let check_one = tools.join("check-one");
        let check_two = tools.join("check-two");
        let review_success = tools.join("review-success");
        let finish = tools.join("finish");

        let active_regex = if archive {
            "^(?:specs|archive)/(?P<id>[0-9]+)\\.md$"
        } else {
            "^specs/(?P<id>[0-9]+)\\.md$"
        };
        let mut profile = format!(
            "schema_version = 3\n\n[specs]\nactive_globs = [\"specs/*.md\"]\nid_regex = {:?}\nordering = \"numeric\"\nselection = \"next\"\n\n[plan]\npath_template = \".spec-loop/plans/{{spec_id}}.md\"\n\n[review.context]\nargv = [{:?}, \"--inspect\"]\n\n[agent]\nbackend = \"pi\"\nexecutable = {:?}\n\n[completion]\nmax_passes = 2\n\n[[checks]]\nname = \"first\"\nargv = [{:?}]\n\n[[checks]]\nname = \"second\"\nargv = [{:?}]\n\n[[actions.on_review_success]]\nname = \"record reviewed spec\"\nargv = [{:?}, \"{{spec_path}}\", \"{{plan_path}}\", \"{{spec_id}}\", \"argument with spaces\"]\n\n[[actions.on_finish]]\nname = \"record finished spec\"\nargv = [{:?}, \"{{spec_path}}\", \"{{plan_path}}\", \"{{spec_id}}\", \"argument with spaces\"]\n",
            active_regex,
            review_context.to_string_lossy(),
            fake_pi.to_string_lossy(),
            check_one.to_string_lossy(),
            check_two.to_string_lossy(),
            review_success.to_string_lossy(),
            finish.to_string_lossy(),
        );
        if archive {
            profile.push_str("\n[archive]\nenabled = true\ndestination = \"archive\"\n");
        }
        fs::write(root.join(".spec-loop.toml"), profile).expect("write profile");
        fs::write(root.join(".gitignore"), "specs/001.md\n").expect("write ignore rules");

        Self {
            temporary,
            root,
            events,
            agent_events,
            prompts,
            review_context,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_spec-loop"));
        command.args(args).current_dir(&self.root);
        for (key, value) in environment {
            command.env(key, value);
        }
        command.output().expect("run spec-loop")
    }

    fn append_finish_status_verifier(&self) {
        let profile_path = self.root.join(".spec-loop.toml");
        let mut profile = fs::read_to_string(&profile_path).expect("read fixture profile");
        let finish_path = self.temporary.path().join("tools/finish");
        let executable = finish_path.to_string_lossy();
        write!(
            profile,
            "\n[[actions.on_finish]]\nname = \"verify completion status\"\nargv = [{executable:?}, \"{{spec_path}}\", \"{{plan_path}}\", \"{{spec_id}}\", \"argument with spaces\", \"verify-status\"]\n"
        )
        .expect("append finish status verifier");
        fs::write(profile_path, profile).expect("append finish status verifier");
    }

    fn prepend_review_success_capture(&self, capture_path: &Path) {
        let executable = self.temporary.path().join("tools/capture-spec");
        write_executable(&executable, "#!/bin/sh\ncat \"$1\" > \"$2\"\n");
        let profile_path = self.root.join(".spec-loop.toml");
        let mut profile = fs::read_to_string(&profile_path).expect("read fixture profile");
        let action = format!(
            "[[actions.on_review_success]]\nname = \"capture spec before finish\"\nargv = [{:?}, \"{{spec_path}}\", {:?}]\n\n",
            executable.to_string_lossy(),
            capture_path.to_string_lossy()
        );
        let position = profile
            .find("[[actions.on_review_success]]")
            .expect("review-success action in fixture profile");
        profile.insert_str(position, &action);
        fs::write(profile_path, profile).expect("prepend review-success capture");
    }

    fn configure_retry_feedback_agent(&self) {
        let executable = self.temporary.path().join("tools/fake-pi-retry-feedback");
        let invalid_response = self
            .temporary
            .path()
            .join("responses/implementation-invalid");
        let ready_response = self.temporary.path().join("responses/implementation-ready");
        let review_response = self.temporary.path().join("responses/review-clear");
        fs::write(
            &invalid_response,
            response(
                r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","unexpected_z":true,"unexpected_a":true}"#,
            ),
        )
        .expect("write invalid implementation response");
        fs::write(
            &ready_response,
            response(
                r#"{"protocol_version":1,"role":"implementation","summary":"Implemented the fixture change.","status":"ready_for_review"}"#,
            ),
        )
        .expect("write corrected implementation response");
        fs::write(
            &review_response,
            response(
                r#"{"protocol_version":1,"role":"review","status":"no_issues","findings":[]}"#,
            ),
        )
        .expect("write clear review response");

        let script = format!(
            r#"#!/bin/sh
set -eu
prompt=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    -p) prompt="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s\n---\n' "$prompt" >> {prompts}
case "$prompt" in
  *'Implement only selected spec'*)
    count=0
    [ ! -f {attempts} ] || count=$(cat {attempts})
    count=$((count + 1))
    printf '%s' "$count" > {attempts}
    printf 'implementation:%s\n' "$count" >> {agent_events}
    printf 'agent-implementation:%s\n' "$count" >> {events}
    if [ "$count" -eq 1 ]; then
      printf 'implemented once\n' >> generated.txt
      cat {invalid_response}
    else
      cat {ready_response}
    fi ;;
  *'Independently review current workspace'*)
    printf 'review\n' >> {agent_events}
    cat {review_response} ;;
  *) exit 23 ;;
esac
"#,
            prompts = shell_quote(&self.prompts),
            attempts = shell_quote(&self.temporary.path().join("implementation-attempts")),
            agent_events = shell_quote(&self.agent_events),
            events = shell_quote(&self.events),
            invalid_response = shell_quote(&invalid_response),
            ready_response = shell_quote(&ready_response),
            review_response = shell_quote(&review_response),
        );
        write_executable(&executable, &script);

        let profile_path = self.root.join(".spec-loop.toml");
        let profile = fs::read_to_string(&profile_path).expect("read fixture profile");
        let existing_executable = format!(
            "{:?}",
            self.temporary
                .path()
                .join("tools/fake-pi")
                .to_string_lossy()
        );
        let replacement_executable = format!("{:?}", executable.to_string_lossy());
        let updated_profile = profile.replace(&existing_executable, &replacement_executable);
        assert_ne!(updated_profile, profile, "replace fixture Pi executable");
        fs::write(profile_path, updated_profile).expect("configure retry feedback agent");
    }
}

fn write_fake_responses(directory: &Path) {
    for (name, text) in [
        (
            "ready",
            "{\"protocol_version\":1,\"role\":\"implementation\",\"summary\":\"Added requested implementation changes and regression coverage.\",\"status\":\"ready_for_review\"}",
        ),
        (
            "blocked",
            "{\"protocol_version\":1,\"role\":\"implementation\",\"summary\":\"No changes made; required input unavailable.\",\"status\":\"blocked\",\"reason\":\"cannot proceed\"}",
        ),
        (
            "review-clear",
            "{\"protocol_version\":1,\"role\":\"review\",\"status\":\"no_issues\",\"findings\":[]}",
        ),
        (
            "review-issues",
            "{\"protocol_version\":1,\"role\":\"review\",\"status\":\"issues\",\"findings\":[{\"id\":1,\"requirement\":\"spec requirement\",\"location\":\"generated.txt\",\"evidence\":\"observed output\",\"violation\":\"missing detail\",\"proposed_correction\":\"add detail\"}]}",
        ),
        (
            "audit-supported",
            "{\"protocol_version\":1,\"role\":\"review_audit\",\"status\":\"complete\",\"classifications\":[{\"finding_id\":1,\"classification\":\"supported\",\"actionability\":\"required\",\"rationale\":\"verified requirement; smallest correct fix is substantial but required\"}]}",
        ),
        (
            "audit-supported-not-required",
            "{\"protocol_version\":1,\"role\":\"review_audit\",\"status\":\"complete\",\"classifications\":[{\"finding_id\":1,\"classification\":\"supported\",\"actionability\":\"not_required\",\"rationale\":\"fact is correct but existing requirements require no correction\"}]}",
        ),
        (
            "audit-unsupported",
            "{\"protocol_version\":1,\"role\":\"review_audit\",\"status\":\"complete\",\"classifications\":[{\"finding_id\":1,\"classification\":\"unsupported\",\"actionability\":\"not_required\",\"rationale\":\"claim is not supported\"}]}",
        ),
        (
            "clarification",
            "{\"protocol_version\":1,\"role\":\"review_clarification\",\"status\":\"complete\",\"evidence\":[{\"finding_id\":1,\"citations\":[{\"path\":\"generated.txt\",\"location\":\"line 1\",\"excerpt\":\"implemented\",\"relevance\":\"source confirms behavior\"}]}]}",
        ),
        (
            "review-blocked",
            "{\"protocol_version\":1,\"role\":\"review\",\"status\":\"blocked\",\"reason\":\"review context command failed\"}",
        ),
        (
            "audit-blocked",
            "{\"protocol_version\":1,\"role\":\"review_audit\",\"status\":\"blocked\",\"reason\":\"review context command failed\"}",
        ),
        (
            "clarification-blocked",
            "{\"protocol_version\":1,\"role\":\"review_clarification\",\"status\":\"blocked\",\"reason\":\"review context command failed\"}",
        ),
        (
            "plan-ready",
            "{\"protocol_version\":1,\"role\":\"plan_author\",\"status\":\"ready_for_review\"}",
        ),
        (
            "plan-clear",
            "{\"protocol_version\":1,\"role\":\"plan_review\",\"status\":\"no_issues\",\"findings\":[]}",
        ),
    ] {
        fs::write(directory.join(name), response(text)).expect("write fake Pi response");
    }
    fs::write(directory.join("malformed"), "not JSON\n").expect("write malformed response");
}

fn write_fake_pi(
    executable: &Path,
    responses: &Path,
    prompts: &Path,
    agent_events: &Path,
    temporary: &Path,
) {
    let script = format!(
        "#!/bin/sh\nset -eu\nPROMPT=''\nwhile [ \"$#\" -gt 0 ]; do case \"$1\" in -p) PROMPT=\"$2\"; shift 2 ;; *) shift ;; esac; done\nprintf '%s\\n---\\n' \"$PROMPT\" >> {prompts}\ncase \"$PROMPT\" in\n  *'Create or revise selected plan only'*) printf 'plan-author\\n' >> {agent_events}; printf '# Authored plan\\n' > .spec-loop/plans/001.md; cat {plan_ready} ;;\n  *'Independently review plan against selected spec'*) printf 'plan-review\\n' >> {agent_events}; cat {plan_clear} ;;\n  *'Implement only selected spec'*) printf 'implementation:%s\\n' \"$PWD\" >> {agent_events}; printf 'implemented\\n' >> generated.txt; if [ \"${{MODE-}}\" = blocked ]; then cat {blocked}; else cat {ready}; fi ;;\n  *'Audit only supplied implementation-review findings'*) printf 'review-audit:%s\\n' \"$PWD\" >> {agent_events}; count=0; [ ! -f {audit_count} ] || count=$(cat {audit_count}); count=$((count + 1)); printf '%s' \"$count\" > {audit_count}; if [ \"${{MODE-}}\" = audit-blocked ]; then cat {audit_blocked}; elif {{ [ \"${{MODE-}}\" = clarify ] || [ \"${{MODE-}}\" = clarification-blocked ]; }} && [ \"$count\" -eq 1 ]; then cat {audit_unsupported}; elif [ \"${{MODE-}}\" = nonactionable ]; then cat {audit_supported_not_required}; else cat {audit_supported}; fi ;;\n  *'Provide citations only for requested implementation-review finding IDs'*) printf 'review-clarification:%s\\n' \"$PWD\" >> {agent_events}; if [ \"${{MODE-}}\" = clarification-blocked ]; then cat {clarification_blocked}; else cat {clarification}; fi ;;\n  *'Independently review current workspace'*) printf 'review:%s\\n' \"$PWD\" >> {agent_events}; if [ \"${{MODE-}}\" = review-context-blocked ]; then cat {review_blocked}; elif [ \"${{MODE-}}\" = finding-repair ] || [ \"${{MODE-}}\" = clarify ] || [ \"${{MODE-}}\" = audit-blocked ] || [ \"${{MODE-}}\" = clarification-blocked ] || [ \"${{MODE-}}\" = nonactionable ] || [ \"${{MODE-}}\" = iterative-repair ]; then count=0; [ ! -f {review_count} ] || count=$(cat {review_count}); count=$((count + 1)); printf '%s' \"$count\" > {review_count}; if [ \"$count\" -eq 1 ] || {{ [ \"${{MODE-}}\" = iterative-repair ] && [ \"$count\" -eq 2 ]; }}; then cat {review_issues}; else cat {review_clear}; fi; elif [ \"${{MODE-}}\" = malformed-review-once ] || [ \"${{MODE-}}\" = malformed-review-always ]; then count=0; [ ! -f {review_count} ] || count=$(cat {review_count}); count=$((count + 1)); printf '%s' \"$count\" > {review_count}; if [ \"${{MODE-}}\" = malformed-review-always ] || [ \"$count\" -eq 1 ]; then cat {malformed}; else cat {review_clear}; fi; else cat {review_clear}; fi ;;\n  *) printf 'unexpected-role\\n' >> {agent_events}; exit 23 ;;\nesac\n",
        prompts = shell_quote(prompts),
        agent_events = shell_quote(agent_events),
        plan_ready = shell_quote(&responses.join("plan-ready")),
        plan_clear = shell_quote(&responses.join("plan-clear")),
        ready = shell_quote(&responses.join("ready")),
        blocked = shell_quote(&responses.join("blocked")),
        malformed = shell_quote(&responses.join("malformed")),
        audit_blocked = shell_quote(&responses.join("audit-blocked")),
        clarification_blocked = shell_quote(&responses.join("clarification-blocked")),
        review_clear = shell_quote(&responses.join("review-clear")),
        review_count = shell_quote(&temporary.join("review-count")),
        audit_count = shell_quote(&temporary.join("audit-count")),
        review_issues = shell_quote(&responses.join("review-issues")),
        audit_supported = shell_quote(&responses.join("audit-supported")),
        audit_supported_not_required = shell_quote(&responses.join("audit-supported-not-required")),
        audit_unsupported = shell_quote(&responses.join("audit-unsupported")),
        clarification = shell_quote(&responses.join("clarification")),
        review_blocked = shell_quote(&responses.join("review-blocked")),
    );
    write_executable(executable, &script);
}

fn write_fake_commands(tools: &Path, events: &Path) {
    let check_one = tools.join("check-one");
    write_executable(
        &check_one,
        &format!(
            "#!/bin/sh\nprintf 'check-one|%s\\n' \"$PWD\" >> {}\nif [ \"${{MUTATE_PLAN_CHECK-0}}\" = 1 ]; then printf 'mutated by check\\n' > .spec-loop/plans/001.md; fi\nif [ \"${{MUTATE_SPEC_CHECK-0}}\" = 1 ]; then printf 'mutated spec by check\\n' > specs/001.md; fi\n[ \"${{FAIL_CHECK-0}}\" != 1 ]\n",
            shell_quote(events)
        ),
    );
    write_executable(
        &tools.join("check-two"),
        &format!(
            "#!/bin/sh\nprintf 'check-two|%s\\n' \"$PWD\" >> {}\n",
            shell_quote(events)
        ),
    );
    write_executable(
        &tools.join("review-success"),
        &format!(
            "#!/bin/sh\nprintf 'review-success|%s|%s|%s|%s|%s|%s\\n' \"$PWD\" \"$1\" \"$2\" \"$3\" \"$4\" \"${{TEST_INHERITED-}}\" >> {}\nif [ \"${{MUTATE_PLAN_ACTION-0}}\" = 1 ]; then printf 'mutated by action\\n' > .spec-loop/plans/001.md; fi\nif [ \"${{MUTATE_SPEC_ACTION-0}}\" = 1 ]; then printf 'mutated spec by action\\n' > specs/001.md; fi\nif [ \"${{ACTION_SLEEP-0}}\" = 1 ]; then sleep 30; fi\n[ \"${{ACTION_FAIL-0}}\" != 1 ]\n",
            shell_quote(events)
        ),
    );
    write_executable(
        &tools.join("finish"),
        &format!(
            "#!/bin/sh\nprintf 'finish|%s|%s|%s|%s|%s|%s\\n' \"$PWD\" \"$1\" \"$2\" \"$3\" \"$4\" \"${{TEST_INHERITED-}}\" >> {}\n",
            shell_quote(events)
        ),
    );
}

fn response(text: &str) -> String {
    let event = json!({
        "type": "message_end",
        "message": {
            "role": "assistant",
            "stopReason": "stop",
            "content": [{"type": "text", "text": text}]
        }
    });
    let settled = json!({"type": "agent_settled", "aborted": false});
    format!("{event}\n{settled}\n")
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable fixture");
    let mut permissions = fs::metadata(path)
        .expect("read fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable permissions");
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn count_lines(path: &Path, prefix: &str) -> usize {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with(prefix))
        .count()
}

fn install_finish_status_command(fixture: &Fixture) {
    let script = format!(
        r#"#!/bin/sh
printf 'finish|%s|%s|%s|%s|%s|%s|%s\n' "$PWD" "$1" "$2" "$3" "$4" "${{TEST_INHERITED-}}" "$5" >> {events}
if [ "${{FINISH_UPDATE_STATUS-0}}" = 1 ] && [ "$5" != verify-status ]; then
  temporary_file="$1.tmp"
  while IFS= read -r line; do
    if [ "$line" = '**Status:** Active' ]; then
      printf '%s\n' '**Status:** Complete'
    else
      printf '%s\n' "$line"
    fi
  done < "$1" > "$temporary_file"
  mv "$temporary_file" "$1"
fi
if [ "$5" = verify-status ]; then
  grep -Fqx '**Status:** Complete' "$1" || exit 32
  printf 'finish-verified|%s\n' "$1" >> {events}
fi
if [ "${{MUTATE_PLAN_FINISH-0}}" = 1 ]; then
  printf 'mutated by finish\n' > "$2"
fi
[ "${{FINISH_FAIL-0}}" != 1 ]
"#,
        events = shell_quote(&fixture.events)
    );
    write_executable(&fixture.temporary.path().join("tools/finish"), &script);
}

#[test]
fn workflow_runs_without_git_and_actions_receive_archived_paths_and_root_cwd() {
    let fixture = Fixture::new(true, false);
    let output = fixture.run_with_env(&["--spec", "001"], &[("TEST_INHERITED", "kept")]);

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(
        "Stage: automated checks for spec 001\nImplementation summary: Added requested implementation changes and regression coverage."
    ));
    assert!(stdout.contains("[RUNNING] Workflow started"));
    assert!(stdout.contains("[RUNNING] Stage: agent implementation (attempt 1; spec 001)"));
    assert!(stdout.contains("[PASS] Check first: passed"));
    assert!(stdout.contains("[PASS] Review approved for spec 001."));
    assert!(stdout.contains("[PASS] Archived spec:"));
    assert!(stdout.contains("[PASS] Action \"record reviewed spec\": completed"));
    let summary_position = stdout
        .find("Implementation summary: Added requested implementation changes")
        .expect("summary transition output");
    let first_check_position = stdout
        .find("Stage: check first")
        .expect("first check stage output");
    assert!(summary_position < first_check_position);
    assert!(!fixture.root.join(".git").exists());
    assert!(!fixture.root.join("specs/001.md").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("archive/001.md")).expect("archived spec"),
        "# Spec one\n"
    );
    assert!(fs::read_to_string(fixture.root.join("generated.txt")).is_ok());
    assert!(fs::read_to_string(fixture.temporary.path().join("context-ran")).is_err());

    let events = fs::read_to_string(&fixture.events).expect("command events");
    let lines = events.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 4);
    assert!(lines[0].starts_with(&format!("check-one|{}", fixture.root.display())));
    assert!(lines[1].starts_with(&format!("check-two|{}", fixture.root.display())));
    for line in &lines[2..] {
        assert!(line.starts_with(&format!("review-success|{}|archive/001.md|.spec-loop/plans/001.md|001|argument with spaces|kept", fixture.root.display()))
            || line.starts_with(&format!("finish|{}|archive/001.md|.spec-loop/plans/001.md|001|argument with spaces|kept", fixture.root.display())));
    }
    assert!(lines[2].starts_with("review-success|"));
    assert!(lines[3].starts_with("finish|"));

    let prompt_log = fs::read_to_string(&fixture.prompts).expect("prompts");
    assert!(prompt_log.contains(&format!(
        "[\"{}\",\"--inspect\"]",
        fixture.review_context.display()
    )));
    assert!(prompt_log.contains(&format!(
        "Review-context working directory: {}",
        fixture.root.display()
    )));
    assert!(prompt_log.contains("Generated response schema"));
    assert!(prompt_log.contains("concise, factual, single-line `summary`"));
    assert!(prompt_log.contains("draft/2020-12"));
    assert_eq!(count_lines(&fixture.agent_events, "implementation:"), 1);
    assert_eq!(count_lines(&fixture.agent_events, "review:"), 1);
}

#[test]
fn dry_run_reports_actions_without_invoking_agents_or_commands() {
    let fixture = Fixture::new(true, false);
    let output = fixture.run(&["--dry-run", "--spec", "001"]);

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Action on_review_success"));
    assert!(stdout.contains("Action on_finish"));
    assert!(stdout.contains("archive/001.md"));
    assert!(stdout.contains("Maximum calls per role: 2"));
    assert!(!fixture.agent_events.exists());
    assert!(!fixture.events.exists());
    assert!(!fixture.root.join("generated.txt").exists());
}

#[test]
fn typed_result_modes_show_concise_and_verbose_validated_fields() {
    let concise_fixture = Fixture::new(false, false);
    let concise = concise_fixture.run_with_env(&["--spec", "001"], &[("MODE", "clarify")]);
    assert_success(&concise);
    let concise_stdout = String::from_utf8_lossy(&concise.stdout);
    assert!(concise_stdout.contains("Review — findings"));
    assert!(concise_stdout.contains("Requirement: spec requirement"));
    assert!(concise_stdout.contains("Violation: missing detail"));
    assert!(!concise_stdout.contains("Evidence: observed output"));
    assert!(!concise_stdout.contains("Proposed correction: add detail"));
    assert!(concise_stdout.contains("Finding #1 — generated.txt"));
    assert!(concise_stdout.contains("generated.txt — line 1"));
    assert!(!concise_stdout.contains("Excerpt: implemented"));
    assert!(!concise_stdout.contains("Rationale: verified requirement"));

    let verbose_fixture = Fixture::new(false, false);
    let verbose =
        verbose_fixture.run_with_env(&["--spec", "001", "--verbose"], &[("MODE", "clarify")]);
    assert_success(&verbose);
    let verbose_stdout = String::from_utf8_lossy(&verbose.stdout);
    for field in [
        "Evidence: observed output",
        "Proposed correction: add detail",
        "Rationale: verified requirement; smallest correct fix is substantial but required",
        "Excerpt: implemented",
        "Relevance: source confirms behavior",
    ] {
        assert!(
            verbose_stdout.contains(field),
            "missing verbose field: {field}"
        );
    }
    assert!(!verbose_stdout.contains("{\"protocol_version\""));
    assert!(!verbose_stdout.contains("agent_settled"));
}

#[test]
fn agent_supplied_terminal_controls_are_sanitized_without_rewriting_raw_response() {
    let fixture = Fixture::new(false, false);
    let response_text = serde_json::to_string(&json!({
        "protocol_version": 1,
        "role": "review",
        "status": "issues",
        "findings": [{
            "id": 1,
            "requirement": "Req\u{1b}[31mtext\u{1b}[0m\ttab\r\u{7f}",
            "location": "\u{1b}]0;title\u{7}src/escaped.rs:9",
            "evidence": "source evidence",
            "violation": "missing field",
            "proposed_correction": "add it"
        }]
    }))
    .expect("serialize response fixture");
    let raw_response = response(&response_text);
    fs::write(
        fixture.temporary.path().join("responses/review-issues"),
        &raw_response,
    )
    .expect("write response containing escaped terminal controls");

    let output = fixture.run_with_env(&["--spec", "001"], &[("MODE", "finding-repair")]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains('\u{1b}'));
    assert!(!stdout.contains('\r'));
    assert!(stdout.contains("Finding #1 — src/escaped.rs:9"));
    assert!(stdout.contains("Requirement: Reqtext\ttab\\u{d}\\u{7f}"));
    let run_dir = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run logs: "))
        .expect("run log directory");
    let preserved_response = fs::read_dir(run_dir)
        .expect("run log directory contents")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("-review.response.json"))
        })
        .any(|path| fs::read(path).is_ok_and(|bytes| bytes == response_text.as_bytes()));
    assert!(
        preserved_response,
        "original validated response bytes retained"
    );
}

#[test]
fn workflow_help_documents_output_modes_and_auth_rejects_them() {
    let help = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .arg("--help")
        .output()
        .expect("run workflow help");
    assert_success(&help);
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(help_text.contains("--verbose"));
    assert!(help_text.contains("--color <auto|always|never>"));
    assert!(help_text.contains("never raw JSON or provider transcripts"));
    assert!(help_text.contains("NO_COLOR"));

    let fixture = Fixture::new(false, false);
    for arguments in [
        &["--verbose", "auth", "status"][..],
        &["--color", "auto", "auth", "status"][..],
    ] {
        let output = fixture.run(arguments);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("apply to workflow invocations, not auth")
        );
        assert!(!fixture.agent_events.exists());
    }
}

#[test]
fn terminal_sanitization_preserves_raw_child_logs_and_labels_excerpts() {
    let fixture = Fixture::new(false, false);
    write_executable(
        &fixture.temporary.path().join("tools/check-one"),
        "#!/bin/sh\nprintf '\\033[31mred\\033[0m\\r\\nkeep\\tvalue\\rDEL\\177\\n'\nprintf '\\033]0;title\\007bad\\n' >&2\n",
    );
    write_executable(
        &fixture.temporary.path().join("tools/review-success"),
        "#!/bin/sh\nprintf '\\033[32maction\\033[0m\\n'\nprintf '\\033]0;title\\007action-error\\n' >&2\n",
    );
    write_executable(
        &fixture.temporary.path().join("tools/finish"),
        "#!/bin/sh\nexit 0\n",
    );

    let output = fixture.run(&["--spec", "001", "--color", "never"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("\\x1b["));
    assert!(stdout.contains("stdout excerpt:\nred\\u{d}\nkeep\tvalue\\u{d}DEL\\u{7f}"));
    assert!(stdout.contains("stderr excerpt:\nbad"));
    assert!(stdout.contains("stdout excerpt:\naction"));
    assert!(stdout.contains("stderr excerpt:\naction-error"));

    let log_paths = stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Log: "))
        .collect::<Vec<_>>();
    let check_log = log_paths
        .iter()
        .find(|path| path.ends_with("check-0002.log"))
        .expect("check log path");
    let action_log = log_paths
        .iter()
        .find(|path| path.ends_with("action-0005.log"))
        .expect("action log path");
    let check_bytes = fs::read(check_log).expect("raw check log");
    assert!(check_bytes.windows(5).any(|window| window == b"\x1b[31m"));
    assert!(
        check_bytes
            .windows(10)
            .any(|window| window == b"\x1b[0m\r\nkeep")
    );
    assert!(check_bytes.windows(2).any(|window| window == b"\x7f\n"));
    assert!(check_bytes.windows(4).any(|window| window == b"\x1b]0;"));
    let action_bytes = fs::read(action_log).expect("raw action log");
    assert!(action_bytes.windows(5).any(|window| window == b"\x1b[32m"));
    assert!(action_bytes.windows(4).any(|window| window == b"\x1b]0;"));

    let check_argv = vec![
        fixture
            .temporary
            .path()
            .join("tools/check-one")
            .to_string_lossy()
            .into_owned(),
    ];
    let check_output = Command::new(&check_argv[0])
        .current_dir(&fixture.root)
        .output()
        .expect("capture expected raw check bytes");
    let mut expected_check = format!(
        "Check: first\nCommand argv: {check_argv:?}\nWorking directory: {}\nStatus: {}\n--- stdout ({} bytes) ---\n",
        fixture.root.display(),
        check_output.status,
        check_output.stdout.len()
    )
    .into_bytes();
    expected_check.extend_from_slice(&check_output.stdout);
    expected_check.extend_from_slice(
        format!("\n--- stderr ({} bytes) ---\n", check_output.stderr.len()).as_bytes(),
    );
    expected_check.extend_from_slice(&check_output.stderr);
    assert_eq!(check_bytes, expected_check);

    let action_argv = vec![
        fixture
            .temporary
            .path()
            .join("tools/review-success")
            .to_string_lossy()
            .into_owned(),
        "specs/001.md".to_owned(),
        ".spec-loop/plans/001.md".to_owned(),
        "001".to_owned(),
        "argument with spaces".to_owned(),
    ];
    let action_output = Command::new(&action_argv[0])
        .args(&action_argv[1..])
        .current_dir(&fixture.root)
        .output()
        .expect("capture expected raw action bytes");
    let mut expected_action = format!(
        "Action: record reviewed spec\nCommand argv: {action_argv:?}\nWorking directory: {}\nStatus: {}\n--- stdout ({} bytes) ---\n",
        fixture.root.display(),
        action_output.status,
        action_output.stdout.len()
    )
    .into_bytes();
    expected_action.extend_from_slice(&action_output.stdout);
    expected_action.extend_from_slice(
        format!("\n--- stderr ({} bytes) ---\n", action_output.stderr.len()).as_bytes(),
    );
    expected_action.extend_from_slice(&action_output.stderr);
    assert_eq!(action_bytes, expected_action);
}

#[test]
fn color_modes_style_only_when_requested_and_keep_status_labels() {
    let fixture = Fixture::new(false, false);
    let auto = fixture.run_with_env(
        &["--dry-run", "--spec", "001"],
        &[("TERM", "xterm"), ("NO_COLOR", "")],
    );
    assert_success(&auto);
    assert!(!String::from_utf8_lossy(&auto.stdout).contains("\x1b["));

    let forced = fixture.run_with_env(
        &["--dry-run", "--spec", "001", "--color", "always"],
        &[("TERM", "dumb"), ("NO_COLOR", "1")],
    );
    assert_success(&forced);
    let forced_stdout = String::from_utf8_lossy(&forced.stdout);
    assert!(forced_stdout.contains("\x1b[1;36m"));
    assert!(forced_stdout.contains("[RUNNING]"));

    let never = fixture.run_with_env(
        &["--dry-run", "--spec", "001", "--color", "never"],
        &[("TERM", "xterm"), ("NO_COLOR", "")],
    );
    assert_success(&never);
    let never_stdout = String::from_utf8_lossy(&never.stdout);
    assert!(!never_stdout.contains("\x1b["));
    assert!(never_stdout.contains("[RUNNING]"));
}

#[test]
fn review_resume_accepts_non_git_workspace_without_a_patch() {
    let fixture = Fixture::new(false, false);
    let output = fixture.run(&["--review", "--spec", "001"]);

    assert_success(&output);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Implementation summary:"));
    assert_eq!(count_lines(&fixture.agent_events, "implementation:"), 0);
    assert_eq!(count_lines(&fixture.agent_events, "review:"), 1);
    assert!(!fixture.root.join(".git").exists());
    let events = fs::read_to_string(&fixture.events).expect("action events");
    assert!(
        events.contains("review-success|")
            && events.contains("|specs/001.md|.spec-loop/plans/001.md|001|")
    );
    assert!(
        events.contains("finish|") && events.contains("|specs/001.md|.spec-loop/plans/001.md|001|")
    );
}

#[test]
fn auth_status_and_help_work_without_project_profile() {
    let temporary = TempDir::new().expect("create profile-free directory");
    let config_root = TempDir::new().expect("create external auth config root");
    let config = config_root.path().join("config");
    let status = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["auth", "status"])
        .current_dir(temporary.path())
        .env_remove("SPEC_LOOP_AUTH_DIR")
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .expect("run auth status");
    assert_success(&status);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("ChatGPT authorization: not configured"));
    assert!(!temporary.path().join(".spec-loop.toml").exists());
    assert!(!config.exists());

    fs::create_dir_all(config.join("spec-loop"))
        .expect("create conflicting XDG registry directory");
    fs::write(config.join("spec-loop/auth.json"), b"{invalid-json")
        .expect("write conflicting XDG registry");
    let auth_directory = config_root.path().join("selected-auth");
    fs::create_dir_all(&auth_directory).expect("create auth registry directory");
    let registry_path = auth_directory.join("auth.json");
    fs::write(
        &registry_path,
        serde_json::to_vec(&json!({
            "version": 1,
            "host_id": "d14149b3-410d-43bc-a7b6-555e79ac61b6",
            "active_registration_id": "324c1466308102826b42e39df00686baf739944a4b5290423a7d7ad11300783c",
            "registrations": [{
                "id": "324c1466308102826b42e39df00686baf739944a4b5290423a7d7ad11300783c",
                "issuer": "https://auth.openai.com",
                "subject": "subject",
                "client_id": "issued-client-id",
                "credential_storage": "file",
                "fallback_credential": {
                    "access_token": "access-secret",
                    "refresh_token": "refresh-secret",
                    "id_token": "id-secret",
                    "scopes": ["chatgpt.tokens.use.direct"],
                    "expires_at_unix": 1_900_000_000
                }
            }]
        }))
        .expect("serialize fixture credentials"),
    )
    .expect("write auth registry fixture");
    let status = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["auth", "status"])
        .current_dir(temporary.path())
        .env("SPEC_LOOP_AUTH_DIR", &auth_directory)
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .expect("run configured auth status");
    assert_success(&status);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("granted locally"));
    assert!(!stdout.contains("access-secret"));
    assert!(!stdout.contains("refresh-secret"));
    assert!(!stdout.contains("id-secret"));

    let logout = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args([
            "auth",
            "logout",
            "--account",
            "324c1466308102826b42e39df00686baf739944a4b5290423a7d7ad11300783c",
        ])
        .current_dir(temporary.path())
        .env("SPEC_LOOP_AUTH_DIR", &auth_directory)
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .expect("run auth logout");
    assert_success(&logout);
    let registry = fs::read_to_string(registry_path).expect("read logged-out registry");
    assert!(registry.contains("issued-client-id"));
    assert!(!registry.contains("access-secret"));
    assert!(!registry.contains("refresh-secret"));
    assert!(!registry.contains("id-secret"));
    assert!(!temporary.path().join(".spec-loop.toml").exists());

    let help = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .arg("--help")
        .current_dir(temporary.path())
        .output()
        .expect("run top-level help");
    assert_success(&help);
    assert!(String::from_utf8_lossy(&help.stdout).contains("Authentication commands do not load"));
}

#[test]
fn auth_login_rejects_relative_xdg_config_home_before_storage_or_network() {
    let temporary = TempDir::new().expect("create temporary project");
    let output = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["auth", "login", "--new-account"])
        .current_dir(temporary.path())
        .env_remove("SPEC_LOOP_AUTH_DIR")
        .env("XDG_CONFIG_HOME", ".")
        .output()
        .expect("run login with unsafe XDG_CONFIG_HOME");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("authentication storage path must be absolute")
    );
    assert!(!temporary.path().join("spec-loop/auth.json").exists());
}

#[test]
fn profile_must_be_in_current_directory_without_parent_search() {
    let fixture = Fixture::new(false, false);
    let child = fixture.root.join("child");
    fs::create_dir(&child).expect("child working directory");
    let output = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["--spec", "001"])
        .current_dir(&child)
        .output()
        .expect("invoke in child directory");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("required profile is missing from current directory")
    );
    assert!(!fixture.agent_events.exists());
}

#[test]
fn schema_v1_profile_fails_with_migration_message_before_agent_invocation() {
    let fixture = Fixture::new(false, false);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path)
        .expect("profile")
        .replace("schema_version = 3", "schema_version = 1");
    fs::write(profile_path, profile).expect("downgrade profile");

    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("profile schema v1"));
    assert!(error.contains("migrate manually to schema_version = 3"));
    assert!(error.contains("no automatic migration is available"));
    assert!(error.contains("backend = \"pi\""));
    assert!(!fixture.agent_events.exists());
}

#[test]
fn schema_v2_profile_fails_with_migration_message_before_agent_invocation() {
    let fixture = Fixture::new(false, false);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path)
        .expect("profile")
        .replace("schema_version = 3", "schema_version = 2");
    fs::write(profile_path, profile).expect("downgrade profile");

    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("profile schema v2"));
    assert!(error.contains("migrate manually to schema_version = 3"));
    assert!(error.contains("no automatic migration is available"));
    assert!(error.contains("backend = \"pi\""));
    assert!(!fixture.agent_events.exists());
}

#[test]
fn typed_retry_feedback_preserves_handoff_context() {
    let fixture = Fixture::new(false, false);
    fixture.configure_retry_feedback_agent();

    let output = fixture.run(&["--spec", "001"]);

    assert_success(&output);
    assert_eq!(count_lines(&fixture.agent_events, "implementation:"), 2);
    assert_eq!(count_lines(&fixture.agent_events, "review"), 1);
    let events = fs::read_to_string(&fixture.events).expect("workflow events");
    let event_lines = events.lines().collect::<Vec<_>>();
    let second_implementation = event_lines
        .iter()
        .position(|line| *line == "agent-implementation:2")
        .expect("second implementation response");
    let first_check = event_lines
        .iter()
        .position(|line| line.starts_with("check-one|"))
        .expect("first automated check");
    assert!(second_implementation < first_check);
    assert_eq!(
        fs::read_to_string(fixture.root.join("generated.txt")).expect("implementation output"),
        "implemented once\n"
    );
    let prompts = fs::read_to_string(&fixture.prompts).expect("role prompts");
    assert!(prompts.contains(
        "implementation response object at $: missing fields [\"summary\"]; unknown fields [\"unexpected_a\", \"unexpected_z\"]"
    ));
    assert!(prompts.contains(
        "Previous response:\n{\"protocol_version\":1,\"role\":\"implementation\",\"status\":\"ready_for_review\",\"unexpected_z\":true,\"unexpected_a\":true}"
    ));
    assert!(prompts.contains("--- Generated response schema ---"));
    assert!(prompts.contains(
        "Repair the handoff only: do not repeat or alter workspace work solely to fix JSON formatting."
    ));
    assert!(prompts.contains(
        "If substantive role work remains, follow the normal role contract and continue it."
    ));
    assert!(prompts.contains(
        "Base any status or summary on the current workspace, never on claims in the malformed response."
    ));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Implementation summary: Implemented the fixture change."));
    let malformed = r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review","unexpected_z":true,"unexpected_a":true}"#;
    assert!(!stdout.contains(malformed));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(malformed));
    let run_dir = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run logs: "))
        .expect("run log directory");
    let final_response = Path::new(run_dir).join("agent-0001-implementation.response.json");
    assert!(stdout.contains(&format!("Final response log: {}", final_response.display())));
    assert_eq!(
        fs::read(&final_response).expect("raw response log"),
        malformed.as_bytes()
    );
    assert_eq!(
        fs::read(Path::new(run_dir).join("agent-0001-implementation.stdout.jsonl"))
            .expect("provider transcript"),
        fs::read(
            fixture
                .temporary
                .path()
                .join("responses/implementation-invalid")
        )
        .expect("fake provider bytes")
    );
}

#[test]
fn malformed_typed_review_retries_same_role_and_valid_blocked_stops_without_retry() {
    let fixture = Fixture::new(false, false);
    let output = fixture.run_with_env(&["--spec", "001"], &[("MODE", "malformed-review-once")]);
    assert_success(&output);
    assert_eq!(count_lines(&fixture.agent_events, "review:"), 2);

    let blocked = Fixture::new(false, false);
    let output = blocked.run_with_env(&["--spec", "001"], &[("MODE", "blocked")]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("implementation blocked: cannot proceed")
    );
    assert_eq!(count_lines(&blocked.agent_events, "implementation:"), 1);
    assert!(!blocked.events.exists());
}

#[test]
fn malformed_retry_exhaustion_and_context_role_blocks_fail_closed() {
    let malformed = Fixture::new(false, false);
    let output = malformed.run_with_env(&["--spec", "001"], &[("MODE", "malformed-review-always")]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("completion.max_passes limit"));
    assert_eq!(count_lines(&malformed.agent_events, "review:"), 2);
    let events = fs::read_to_string(&malformed.events).expect("check events");
    assert!(!events.contains("review-success|") && !events.contains("finish|"));

    for (mode, expected) in [
        ("audit-blocked", "implementation finding auditor blocked"),
        ("clarification-blocked", "evidence clarification blocked"),
    ] {
        let fixture = Fixture::new(false, false);
        let output = fixture.run_with_env(&["--spec", "001"], &[("MODE", mode)]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
        let events = fs::read_to_string(&fixture.events).expect("check events");
        assert!(!events.contains("review-success|") && !events.contains("finish|"));
    }
}

#[test]
fn typed_finding_audit_and_one_time_evidence_clarification_run_with_review_context() {
    for mode in ["finding-repair", "clarify"] {
        let fixture = Fixture::new(false, false);
        let output = fixture.run_with_env(&["--spec", "001"], &[("MODE", mode)]);
        assert_success(&output);
        assert_eq!(
            count_lines(&fixture.agent_events, "review-audit:"),
            if mode == "clarify" { 2 } else { 1 }
        );
        assert_eq!(
            count_lines(&fixture.agent_events, "review-clarification:"),
            usize::from(mode == "clarify")
        );
        assert_eq!(count_lines(&fixture.agent_events, "implementation:"), 2);
        let prompts = fs::read_to_string(&fixture.prompts).expect("role prompts");
        assert!(prompts.contains("Review-context command argv"));
        assert!(prompts.contains("review_audit"));
        assert!(prompts.contains("smallest correct fix is substantial but required"));
        if mode == "clarify" {
            assert!(prompts.contains("review_clarification"));
            assert!(prompts.contains("source confirms behavior"));
        }
    }
}

#[test]
fn supported_non_actionable_finding_does_not_trigger_repair() {
    let fixture = Fixture::new(false, false);
    let output = fixture.run_with_env(&["--spec", "001"], &[("MODE", "nonactionable")]);
    assert_success(&output);
    assert_eq!(count_lines(&fixture.agent_events, "review-audit:"), 1);
    assert_eq!(count_lines(&fixture.agent_events, "implementation:"), 1);
    assert_eq!(count_lines(&fixture.agent_events, "review:"), 1);
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Review auditor found no required correction for findings: 1")
    );
}

#[test]
fn review_resume_repeats_review_and_implementation_until_findings_clear() {
    let fixture = Fixture::new(false, false);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("fixture profile");
    fs::write(
        profile_path,
        profile.replace("max_passes = 2", "max_passes = 3"),
    )
    .expect("allow three review passes");
    let output = fixture.run_with_env(
        &["--review", "--spec", "001"],
        &[("MODE", "iterative-repair")],
    );

    assert_success(&output);
    let roles = fs::read_to_string(&fixture.agent_events)
        .expect("agent events")
        .lines()
        .map(|event| event.split(':').next().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        [
            "review",
            "review-audit",
            "implementation",
            "review",
            "review-audit",
            "implementation",
            "review",
        ]
    );
    assert_eq!(count_lines(&fixture.events, "check-one|"), 3);
    assert_eq!(count_lines(&fixture.events, "check-two|"), 3);
    assert_eq!(
        fs::read_to_string(fixture.root.join("generated.txt")).expect("implementation progress"),
        "implemented\nimplemented\n"
    );
    let prompts = fs::read_to_string(&fixture.prompts).expect("role prompts");
    assert!(prompts.contains("ready_for_review marks readiness for another pass"));
    assert!(
        prompts.contains("Resume implementation from current workspace and preserve prior work")
    );
}

#[test]
fn review_context_failure_reported_as_typed_blocked_stops_completion() {
    let fixture = Fixture::new(false, false);
    let output = fixture.run_with_env(&["--spec", "001"], &[("MODE", "review-context-blocked")]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("implementation reviewer blocked: review context command failed")
    );
    let events = fs::read_to_string(&fixture.events).expect("check events");
    assert!(!events.contains("review-success|"));
    assert!(!events.contains("finish|"));
    assert!(fixture.root.join("generated.txt").exists());
}

#[test]
fn selected_plan_hash_mutations_by_checks_and_actions_stop_without_rollback() {
    let check_fixture = Fixture::new(false, false);
    let output = check_fixture.run_with_env(&["--spec", "001"], &[("MUTATE_PLAN_CHECK", "1")]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected plan content changed")
    );
    assert_eq!(
        fs::read_to_string(check_fixture.root.join(".spec-loop/plans/001.md"))
            .expect("preserved plan"),
        "mutated by check\n"
    );
    let check_events = fs::read_to_string(&check_fixture.events).expect("check event");
    assert!(check_events.contains("check-one|"));
    assert!(!check_events.contains("review-success|"));
    assert!(!check_events.contains("finish|"));

    let action_fixture = Fixture::new(false, false);
    let output = action_fixture.run_with_env(&["--spec", "001"], &[("MUTATE_PLAN_ACTION", "1")]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected plan content changed")
    );
    assert_eq!(
        fs::read_to_string(action_fixture.root.join(".spec-loop/plans/001.md"))
            .expect("preserved plan"),
        "mutated by action\n"
    );
    assert!(
        fs::read_to_string(&action_fixture.events)
            .expect("action output")
            .contains("review-success|")
    );
    assert!(
        !fs::read_to_string(&action_fixture.events)
            .expect("action output")
            .contains("finish|")
    );
}

#[test]
fn selected_spec_hash_mutations_by_checks_and_actions_stop_without_rollback() {
    let check_fixture = Fixture::new(false, false);
    let output = check_fixture.run_with_env(&["--spec", "001"], &[("MUTATE_SPEC_CHECK", "1")]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected spec content changed")
    );
    assert_eq!(
        fs::read_to_string(check_fixture.root.join("specs/001.md")).expect("preserved spec"),
        "mutated spec by check\n"
    );

    let action_fixture = Fixture::new(false, false);
    let output = action_fixture.run_with_env(&["--spec", "001"], &[("MUTATE_SPEC_ACTION", "1")]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected spec content changed")
    );
    assert_eq!(
        fs::read_to_string(action_fixture.root.join("specs/001.md")).expect("preserved spec"),
        "mutated spec by action\n"
    );
    assert!(
        !fs::read_to_string(&action_fixture.events)
            .expect("action events")
            .contains("finish|")
    );
}

#[test]
fn finish_actions_update_selected_spec_after_last_spec_hash_check() {
    for archive in [false, true] {
        let fixture = Fixture::new(archive, false);
        let spec_path = fixture.root.join("specs/001.md");
        fs::write(&spec_path, "**Status:** Active\n# Spec content remains.\n")
            .expect("write active status");
        let pre_finish = fixture.temporary.path().join("pre-finish-spec.md");
        fixture.prepend_review_success_capture(&pre_finish);
        fixture.append_finish_status_verifier();
        install_finish_status_command(&fixture);

        let output = fixture.run_with_env(&["--spec", "001"], &[("FINISH_UPDATE_STATUS", "1")]);

        assert_success(&output);
        assert_eq!(
            fs::read_to_string(pre_finish).expect("pre-finish snapshot"),
            "**Status:** Active\n# Spec content remains.\n"
        );
        let completed_path = if archive {
            fixture.root.join("archive/001.md")
        } else {
            spec_path
        };
        assert_eq!(
            fs::read_to_string(&completed_path).expect("completed selected spec"),
            "**Status:** Complete\n# Spec content remains.\n"
        );
        if archive {
            assert!(!fixture.root.join("specs/001.md").exists());
        }

        let events = fs::read_to_string(&fixture.events).expect("action events");
        let lines = events.lines().collect::<Vec<_>>();
        assert_eq!(count_lines(&fixture.events, "finish|"), 2);
        assert!(lines.iter().any(|line| {
            line.starts_with(&format!(
                "finish-verified|{}",
                if archive {
                    "archive/001.md"
                } else {
                    "specs/001.md"
                }
            ))
        }));
        let first_finish = lines
            .iter()
            .position(|line| line.starts_with("finish|"))
            .expect("first finish action");
        let verified_finish = lines
            .iter()
            .position(|line| line.starts_with("finish-verified|"))
            .expect("second finish action verified status");
        assert!(first_finish < verified_finish);
    }
}

#[test]
fn finish_action_plan_mutation_is_detected_even_when_action_fails() {
    for finish_fails in [false, true] {
        let fixture = Fixture::new(false, false);
        fixture.append_finish_status_verifier();
        install_finish_status_command(&fixture);
        let finish_failure = if finish_fails { "1" } else { "0" };
        let output = fixture.run_with_env(
            &["--spec", "001"],
            &[("MUTATE_PLAN_FINISH", "1"), ("FINISH_FAIL", finish_failure)],
        );

        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("protected selected plan content changed"));
        if finish_fails {
            assert!(stderr.contains("record finished spec"));
            assert!(stderr.contains("failed with exit status"));
        }
        assert_eq!(
            fs::read_to_string(fixture.root.join(".spec-loop/plans/001.md"))
                .expect("preserved mutated plan"),
            "mutated by finish\n"
        );
        assert_eq!(count_lines(&fixture.events, "finish|"), 1);
        assert_eq!(count_lines(&fixture.events, "finish-verified|"), 0);
    }
}

#[test]
fn failed_finish_action_preserves_status_update_and_stops_later_work() {
    let fixture = Fixture::new(false, true);
    fs::write(
        fixture.root.join("specs/001.md"),
        "**Status:** Active\n# Spec content remains.\n",
    )
    .expect("write active status");
    fixture.append_finish_status_verifier();
    install_finish_status_command(&fixture);

    let output = fixture.run_with_env(
        &["--continue"],
        &[("FINISH_UPDATE_STATUS", "1"), ("FINISH_FAIL", "1")],
    );

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("record finished spec"));
    assert_eq!(
        fs::read_to_string(fixture.root.join("specs/001.md")).expect("preserved status update"),
        "**Status:** Complete\n# Spec content remains.\n"
    );
    assert_eq!(count_lines(&fixture.agent_events, "implementation:"), 1);
    assert_eq!(count_lines(&fixture.events, "finish|"), 1);
    assert_eq!(count_lines(&fixture.events, "finish-verified|"), 0);
    assert_eq!(count_lines(&fixture.events, "review-success|"), 1);
}

#[test]
fn all_checks_run_after_failure_and_review_and_actions_are_skipped() {
    let fixture = Fixture::new(false, false);
    let output = fixture.run_with_env(&["--spec", "001"], &[("FAIL_CHECK", "1")]);

    assert!(!output.status.success());
    let events = fs::read_to_string(&fixture.events).expect("check events");
    assert!(events.contains("check-one|"));
    assert!(events.contains("check-two|"));
    assert!(!events.contains("review-success|"));
    assert!(!events.contains("finish|"));
    assert_eq!(count_lines(&fixture.agent_events, "review:"), 0);
    assert!(fixture.root.join("generated.txt").exists());
}

#[test]
fn timed_out_action_is_not_retried_and_skips_finish_action() {
    let fixture = Fixture::new(false, false);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("profile").replace(
        "argument with spaces\"]\n\n[[actions.on_finish]]",
        "argument with spaces\"]\ntimeout_seconds = 1\n\n[[actions.on_finish]]",
    );
    fs::write(profile_path, profile).expect("set action timeout");

    let output = fixture.run_with_env(&["--spec", "001"], &[("ACTION_SLEEP", "1")]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    let events = fs::read_to_string(&fixture.events).expect("action events");
    assert_eq!(
        events
            .lines()
            .filter(|line| line.starts_with("review-success|"))
            .count(),
        1
    );
    assert!(!events.contains("finish|"));
}

#[test]
fn lifecycle_action_failure_preserves_archive_and_skips_later_actions() {
    let fixture = Fixture::new(true, false);
    let output = fixture.run_with_env(&["--spec", "001"], &[("ACTION_FAIL", "1")]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("record reviewed spec"));
    assert!(fixture.root.join("archive/001.md").exists());
    assert!(fixture.root.join("generated.txt").exists());
    let events = fs::read_to_string(&fixture.events).expect("action events");
    assert!(events.contains("review-success|"));
    assert!(!events.contains("finish|"));
}

#[test]
fn plan_continue_does_not_process_specs_before_requested_spec() {
    let fixture = Fixture::new(false, true);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("read profile");
    fs::write(
        &profile_path,
        profile.replace("selection = \"next\"", "selection = \"any\""),
    )
    .expect("allow selecting later specs");
    let output = fixture.run(&["--plan", "--spec", "002", "--continue"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Skipping spec 002"));
    assert!(!stdout.contains("Skipping spec 001"));
}

#[test]
fn continue_runs_actions_once_per_spec_without_commit_authorization() {
    let fixture = Fixture::new(false, true);
    let output = fixture.run(&["--continue"]);

    assert_success(&output);
    let events = fs::read_to_string(&fixture.events).expect("action events");
    assert_eq!(
        events
            .lines()
            .filter(|line| line.starts_with("review-success|"))
            .count(),
        2
    );
    assert_eq!(
        events
            .lines()
            .filter(|line| line.starts_with("finish|"))
            .count(),
        2
    );
    assert!(events.contains("|001|argument with spaces|"));
    assert!(events.contains("|002|argument with spaces|"));
    assert_eq!(count_lines(&fixture.agent_events, "implementation:"), 2);
    assert_eq!(count_lines(&fixture.agent_events, "review:"), 2);
}

#[test]
fn plan_only_creates_and_reviews_plan_without_lifecycle_actions() {
    let fixture = Fixture::new(false, false);
    fs::remove_file(fixture.root.join(".spec-loop/plans/001.md")).expect("remove existing plan");
    let output = fixture.run(&["--plan", "--spec", "001"]);

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.root.join(".spec-loop/plans/001.md")).expect("new plan"),
        "# Authored plan\n"
    );
    assert_eq!(count_lines(&fixture.agent_events, "plan-author"), 1);
    assert_eq!(count_lines(&fixture.agent_events, "plan-review"), 1);
    assert!(!fixture.events.exists());
}

#[test]
fn no_commit_flag_and_continue_has_no_commit_requirement() {
    let fixture = Fixture::new(false, false);
    let output = fixture.run(&["--commit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--commit'"));
}

#[test]
fn profile_rejects_removed_cwd_and_unknown_action_placeholders() {
    let fixture = Fixture::new(false, false);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("profile");
    fs::write(
        profile_path,
        profile.replace("argv = [\"", "cwd = \".\"\nargv = [\""),
    )
    .expect("add removed cwd field");
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field `cwd`"));
    assert!(!fixture.agent_events.exists());

    let second = Fixture::new(false, false);
    let path = second.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&path)
        .expect("profile")
        .replace("argument with spaces", "{unknown}");
    fs::write(path, profile).expect("set unknown placeholder");
    let output = second.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported placeholder"));

    let third = Fixture::new(false, false);
    let path = third.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&path)
        .expect("profile")
        .replace("argument with spaces", "{spec_id");
    fs::write(path, profile).expect("set malformed placeholder");
    let output = third.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unclosed placeholder"));
}
