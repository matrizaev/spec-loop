#![cfg(unix)]
#![forbid(unsafe_code)]

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

#[derive(Clone, Copy)]
struct FixtureOptions<'a> {
    checks: bool,
    mode: &'a str,
    features: FixtureFeatures,
}

#[derive(Clone, Copy)]
struct FixtureFeatures {
    archive: bool,
    plan: bool,
}

struct Fixture {
    temporary: TempDir,
    root: PathBuf,
    events: PathBuf,
    writer_calls: PathBuf,
}

impl Fixture {
    fn new(checks: bool, mode: &str, archive: bool) -> Self {
        Self::configured(FixtureOptions {
            checks,
            mode,
            features: FixtureFeatures {
                archive,
                plan: false,
            },
        })
    }

    fn with_plan() -> Self {
        Self::with_plan_mode("normal")
    }

    fn with_plan_mode(mode: &str) -> Self {
        Self::configured(FixtureOptions {
            checks: false,
            mode,
            features: FixtureFeatures {
                archive: false,
                plan: true,
            },
        })
    }

    fn configured(options: FixtureOptions<'_>) -> Self {
        let FixtureOptions {
            checks,
            mode,
            features,
        } = options;
        let FixtureFeatures { archive, plan } = features;
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("repo");
        let tools = temporary.path().join("tools");
        fs::create_dir_all(root.join("specs")).expect("repository dirs");
        fs::create_dir_all(root.join(".spec-loop/plans")).expect("plan directory");
        fs::write(root.join(".spec-loop/plans/001.md"), "# Existing plan\n")
            .expect("default implementation plan");
        fs::create_dir_all(&tools).expect("tools dir");
        if mode == "relative-executable" {
            fs::create_dir_all(root.join("tools")).expect("repository tools dir");
        }
        let events = temporary.path().join("events.log");
        let writer_calls = temporary.path().join("writer-calls");
        let review_calls = temporary.path().join("review-calls");
        let status_context = temporary.path().join("status-context");
        let status_args = temporary.path().join("status-args");
        let prompt_file = temporary.path().join("prompt.txt");
        let mode_file = temporary.path().join("mode");
        fs::write(&mode_file, mode).expect("mode");
        let fake_pi = if mode == "relative-executable" {
            root.join("tools/fake-pi")
        } else {
            tools.join("fake-pi")
        };
        write_executable(
            &fake_pi,
            &format!(
                "#!/bin/sh\nset -eu\nEVENTS='{}'\nCALLS='{}'\nREVIEWS='{}'\nMODE_FILE='{}'\nSTATUS_CONTEXT='{}'\nSTATUS_ARGS='{}'\nROOT='{}'\nPROMPT_FILE='{}'\nMODE=$(cat \"$MODE_FILE\")\nprintf '%s\\n' \"$@\" > \"$STATUS_ARGS\"\nPROMPT=''\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = '-p' ]; then shift; PROMPT=\"$1\"; fi\n  shift\ndone\nprintf '%s' \"$PROMPT\" > \"$PROMPT_FILE\"\nPROMPT=\"$PROMPT_FILE\"\ngrep -q 'selected active spec is read-only' \"$PROMPT\"\ngrep -q 'Plans are writable only by PlanAuthor in explicit --plan mode' \"$PROMPT\"\nif [ \"$MODE\" = 'agent-interrupted-output' ]; then printf 'partial agent stdout\\n'; printf 'partial agent stderr\\n' >&2; kill -INT \"$PPID\"; sleep 30; fi\nif [ \"$MODE\" = 'agent-malformed' ]; then printf '{{malformed jsonl}}\\n'; exit 0; fi\nif [ \"$MODE\" = 'agent-nonzero' ]; then printf 'backend failed\\n' >&2; exit 7; fi\nif grep -q 'Create or revise the selected implementation plan only' \"$PROMPT\"; then\n  printf '# Plan\\n' > .spec-loop/plans/001.md\n  printf 'plan-author\\n' >> \"$EVENTS\"\n  if [ \"$MODE\" = 'plan-ignored' ]; then printf 'ignored\\n' > ignored-plan-output.txt; fi\n  if [ \"$MODE\" = 'plan-git-metadata' ]; then git config user.name 'mutated by plan writer'; fi\n  if [ \"$MODE\" = 'plan-permission' ]; then chmod 600 specs/001.md; fi\n  TEXT='SPEC_LOOP_PLAN=READY_FOR_REVIEW'\nelif grep -q 'Independently review the plan against' \"$PROMPT\"; then\n  printf 'plan-review\\n' >> \"$EVENTS\"\n  if [ \"$MODE\" = 'plan-review-ignored' ]; then printf 'review mutation\\n' >> .spec-loop/plans/001.md; fi\n  if [ \"$MODE\" = 'plan-audit-ignored' ]; then TEXT='FINDING-1: plan finding\\nSPEC_LOOP_PLAN_REVIEW=ISSUES'; elif [ \"$MODE\" = 'plan-status-ignored' ]; then TEXT='The plan review reports no findings.'; else TEXT='SPEC_LOOP_PLAN_REVIEW=NO_ISSUES'; fi\nelif grep -q 'Audit only findings already reported by the plan reviewer' \"$PROMPT\"; then\n  printf 'plan-audit\\n' >> \"$EVENTS\"\n  if [ \"$MODE\" = 'plan-audit-ignored' ]; then printf 'audit mutation\\n' >> .spec-loop/plans/001.md; fi\n  TEXT='SUPPORTED_FINDINGS:\\nFINDING-1: plan finding | AUDIT RATIONALE: supported by the selected spec\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_PLAN_AUDIT=ACTIONABLE'\nelif grep -q 'Implement only the selected spec' \"$PROMPT\"; then\n  count=$(cat \"$CALLS\" 2>/dev/null || printf '0')\n  count=$((count + 1))\n  printf '%s' \"$count\" > \"$CALLS\"\n  if [ \"$MODE\" = 'supported-poor-remedy' ]; then\n    if [ \"$count\" -eq 1 ]; then\n      printf 'preserve me\\n' > unrelated.txt\n    else\n      grep -q 'smallest architecture-aligned correction' \"$PROMPT\"\n      grep -q 'proposed remedy: refactor the entire generator' \"$PROMPT\"\n      printf 'required\\nstable\\n' > generated.txt\n    fi\n  elif [ \"$MODE\" = 'out-of-scope' ]; then\n    printf 'implemented\\n' > generated.txt\n    printf 'unrelated change\\n' > out-of-scope.txt\n  else\n    printf 'implemented\\n' > generated.txt\n    if [ $MODE = continue ]; then printf 'run %s\\n' $count >> generated.txt; fi\n  fi\n  printf 'implementation\\n' >> \"$EVENTS\"\n  if [ \"$MODE\" = 'writer-stage' ]; then git add generated.txt; fi\n  if [ \"$MODE\" = 'agent-timeout-once' ]; then\n    if [ \"$count\" -eq 1 ]; then sleep 30; fi\n    if ! grep -q 'timed out' \"$PROMPT\"; then printf 'missing timeout feedback' >&2; exit 9; fi\n  fi\n  TEXT='SPEC_LOOP_IMPLEMENTATION=READY_FOR_REVIEW'\nelif grep -q 'Normalize status for this plan review response' \"$PROMPT\"; then\n  printf 'plan-status-audit\\n' >> \"$EVENTS\"\n  if [ \"$MODE\" = 'plan-status-ignored' ]; then printf 'status audit mutation\\n' >> \"$ROOT/.spec-loop/plans/001.md\"; fi\n  TEXT='SPEC_LOOP_PLAN_REVIEW_STATUS_AUDIT=NO_ISSUES'\nelif grep -q 'Normalize status for this implementation review' \"$PROMPT\"; then\n  printf 'status-audit\\n' >> \"$EVENTS\"\n  printf '%s\\n' \"$PWD\" > \"$STATUS_CONTEXT\"\n  case \"$MODE\" in\n    status-ambiguous) TEXT='SPEC_LOOP_REVIEW_STATUS_AUDIT=AMBIGUOUS' ;;\n    status-invalid) TEXT='NONCANONICAL_STATUS' ;;\n    status-issues|status-malformed-findings) TEXT='SPEC_LOOP_REVIEW_STATUS_AUDIT=ISSUES' ;;\n    *) TEXT='SPEC_LOOP_REVIEW_STATUS_AUDIT=NO_ISSUES' ;;\n  esac\nelif grep -q 'Audit only reported review findings' \"$PROMPT\"; then\n  if [ \"$MODE\" = 'audit-unsupported-agents' ]; then\n    grep -q 'Configured role instruction file: AGENTS.md' \"$PROMPT\"\n    if grep -q 'Implementation instructions: create generated.txt' \"$PROMPT\"; then exit 9; fi\n  fi\n  printf 'review-audit\\n' >> \"$EVENTS\"\n  if [ \"$MODE\" = 'audit-incomplete' ]; then\n    TEXT='SUPPORTED_FINDINGS:\\nNone\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=NO_ACTIONABLE_FINDINGS'\n  elif [ \"$MODE\" = 'audit-unsupported-spec' ]; then\n    TEXT='SUPPORTED_FINDINGS:\\nNone\\nUNSUPPORTED_FINDINGS:\\nFINDING-1: The repository forbids creating generated.txt | AUDIT RATIONALE: the active spec explicitly requires generated.txt\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=NO_ACTIONABLE_FINDINGS'\n  elif [ \"$MODE\" = 'audit-unsupported-agents' ]; then\n    TEXT='SUPPORTED_FINDINGS:\\nNone\\nUNSUPPORTED_FINDINGS:\\nFINDING-1: The repository forbids creating generated.txt | AUDIT RATIONALE: AGENTS.md explicitly requires generated.txt\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=NO_ACTIONABLE_FINDINGS'\n  elif [ \"$MODE\" = 'supported-poor-remedy' ]; then\n    TEXT='SUPPORTED_FINDINGS:\\nFINDING-1: generated.txt first line must be required; observed wrong; proposed remedy: refactor the entire generator | AUDIT RATIONALE: output violates selected specification; proposed remedy is unnecessary\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=ACTIONABLE'\n  elif [ \"$MODE\" = 'status-issues' ]; then\n    TEXT='SUPPORTED_FINDINGS:\\nFINDING-1: The reviewer reports one possible issue. | AUDIT RATIONALE: the reported concern is actionable\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=ACTIONABLE'\n  else\n    TEXT='SUPPORTED_FINDINGS:\\nFINDING-1: reviewer finding with location and evidence | AUDIT RATIONALE: supported by the applicable requirement\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=ACTIONABLE'\n  fi\nelif grep -q 'Independently review the implementation changes before staging' \"$PROMPT\"; then\n  if [ \"$MODE\" = 'normal' ] && git ls-files --error-unmatch generated.txt >/dev/null 2>&1; then exit 9; fi\nif [ \"$MODE\" = 'index-only-staged' ]; then if git diff --cached --quiet HEAD -- staged-only.txt; then exit 9; fi; if git diff --quiet -- staged-only.txt; then exit 10; fi; fi\ngrep -q 'Inspect complete Git-visible changes against HEAD' \"$PROMPT\"\n  grep -q 'git diff --cached HEAD' \"$PROMPT\"\n  grep -q 'git diff' \"$PROMPT\"\n  grep -q 'Selected spec path:' \"$PROMPT\"\n  grep -q 'Plan path:' \"$PROMPT\"\n  if grep -q 'Complete staged diff against HEAD' \"$PROMPT\"; then exit 9; fi\n  if grep -q 'Create generated.txt' \"$PROMPT\"; then exit 9; fi\n  if [ \"$MODE\" = 'audit-unsupported-agents' ]; then\n    grep -q 'Configured role instruction file: AGENTS.md' \"$PROMPT\"\n    if grep -q 'Implementation instructions: create generated.txt' \"$PROMPT\"; then exit 9; fi\n  fi\n  count=$(cat \"$REVIEWS\" 2>/dev/null || printf '0')\n  count=$((count + 1))\n  printf '%s' \"$count\" > \"$REVIEWS\"\n  printf 'review\\n' >> \"$EVENTS\"\n  case \"$MODE:$count\" in\n    audit:1|audit-incomplete:1) TEXT='FINDING-1: reviewer finding with location and evidence\\nSPEC_LOOP_REVIEW=ISSUES' ;;\n    audit-unsupported-spec:1|audit-unsupported-agents:1) TEXT='FINDING-1: The repository forbids creating generated.txt\\nSPEC_LOOP_REVIEW=ISSUES' ;;\n    status-issues:1) TEXT='FINDING-1: The reviewer reports one possible issue.' ;;\n    status-malformed-findings:1) TEXT='FINDING-2: non-sequential concern' ;;\n    status-malformed-findings:2) TEXT='SPEC_LOOP_REVIEW=NO_ISSUES' ;;\n    status-contradiction:1) TEXT='FINDING-1: conflicting concern' ;; status-prose-contradiction:1) TEXT='The reviewer describes an unresolved concern.\\nSPEC_LOOP_REVIEW=NO_ISSUES' ;; status:1|status-invalid:1) TEXT='The reviewer reports one possible issue.' ;;\n    status-ambiguous:1|status-ambiguous:2) TEXT='The reviewer reports conflicting conclusions.' ;;\n    relative-executable:1) TEXT='The reviewer reports no findings.' ;;\n    supported-poor-remedy:1) TEXT='FINDING-1: generated.txt first line must be required; observed wrong; proposed remedy: refactor the entire generator\\nSPEC_LOOP_REVIEW=ISSUES' ;;\n    *) TEXT='SPEC_LOOP_REVIEW=NO_ISSUES' ;;\n  esac\nelse\n  printf 'unexpected-role\\n' >> \"$EVENTS\"\n  TEXT='unexpected'\nfi\nprintf '{{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[{{\"type\":\"text\",\"text\":\"%s\"}}]}}}}\\n' \"$TEXT\"\nprintf '%s\\n' '{{\"type\":\"agent_settled\",\"aborted\":false}}'\n",
                events.display(),
                writer_calls.display(),
                review_calls.display(),
                mode_file.display(),
                status_context.display(),
                status_args.display(),
                root.display(),
                prompt_file.display()
            ),
        );
        let mut profile = format!(
            "schema_version = 1\n\n[specs]\nactive_globs = [\"specs/*.md\"]\nid_regex = {:?}\n[agent]\nexecutable = {:?}\n",
            if archive {
                r"^(?:specs|archive)/(?P<id>[0-9]+)\.md$"
            } else {
                r"^specs/(?P<id>[0-9]+)\.md$"
            },
            if mode == "relative-executable" {
                "tools/fake-pi".to_owned()
            } else {
                fake_pi.to_string_lossy().into_owned()
            }
        );
        if mode == "agent-timeout-once" {
            profile.push_str("timeout_seconds = 1\n");
        }
        if checks {
            let first = tools.join("check-one");
            let second = tools.join("check-two");
            write_executable(
                &first,
                &format!(
                    "#!/bin/sh\nset -eu\nprintf 'check-one\\n' >> '{}'\ncount=$(cat '{}' 2>/dev/null || printf '0')\n[ \"$count\" -ge 2 ]\n",
                    events.display(),
                    writer_calls.display()
                ),
            );
            write_executable(
                &second,
                &format!(
                    "#!/bin/sh\nset -eu\nprintf 'check-two\\n' >> '{}'\n",
                    events.display()
                ),
            );
            writeln!(
                profile,
                "\n[[checks]]\nname = \"first\"\nargv = [{:?}]\ncwd = \".\"\n\n[[checks]]\nname = \"second\"\nargv = [{:?}]\ncwd = \".\"",
                first.to_string_lossy(),
                second.to_string_lossy()
            )
            .expect("append checks to profile");
        }
        if plan {
            profile.push_str("\n[plan]\npath_template = \".spec-loop/plans/{spec_id}.md\"\n");
        }
        if archive {
            profile.push_str("\n[archive]\nenabled = true\ndestination = \"archive\"\n");
            fs::create_dir(root.join("archive")).expect("archive dir");
            fs::write(root.join("archive/.keep"), "").expect("archive keep file");
        }
        profile.push_str("\n[completion]\n");
        profile.push_str(if mode == "limit-one" {
            "max_passes = 1\n"
        } else {
            "max_passes = 2\n"
        });
        if mode == "audit-unsupported-agents" {
            profile.push_str(
                "\n[prompts]\nreview = [\"AGENTS.md\"]\nreview_audit = [\"AGENTS.md\"]\n",
            );
        }
        fs::write(root.join(".spec-loop.toml"), profile).expect("profile");
        fs::write(
            root.join(".gitignore"),
            ".spec-loop/\nignored-plan-output.txt\n",
        )
        .expect("gitignore");
        fs::write(
            root.join("specs/001.md"),
            if mode == "supported-poor-remedy" {
                "# Spec\n\nThe first line of generated.txt must be required; preserve later lines.\n"
            } else {
                "# Spec\n\nCreate generated.txt.\n"
            },
        )
        .expect("spec");
        if mode == "supported-poor-remedy" {
            fs::write(root.join("generated.txt"), "wrong\nstable\n")
                .expect("initial generated content");
        }
        if mode == "audit-unsupported-agents" {
            fs::write(
                root.join("AGENTS.md"),
                "Implementation instructions: create generated.txt as required by the selected spec.\n",
            )
            .expect("repository instructions");
        }
        if mode == "continue" {
            fs::write(
                root.join("specs/002.md"),
                "# Spec 2\n\nCreate another change.\n",
            )
            .expect("second spec");
            fs::write(root.join(".spec-loop/plans/002.md"), "# Existing plan 2\n")
                .expect("second implementation plan");
        }
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "Spec Loop Test"]);
        git(
            &root,
            &["config", "user.email", "spec-loop@example.invalid"],
        );
        git(
            &root,
            &["add", ".spec-loop.toml", ".gitignore", "specs/001.md"],
        );
        if mode == "supported-poor-remedy" {
            git(&root, &["add", "generated.txt"]);
        }
        if mode == "relative-executable" {
            git(&root, &["add", "tools/fake-pi"]);
        }
        if mode == "audit-unsupported-agents" {
            git(&root, &["add", "AGENTS.md"]);
        }
        if mode == "continue" {
            git(&root, &["add", "specs/002.md"]);
        }
        if archive {
            git(&root, &["add", "archive/.keep"]);
        }
        git(&root, &["commit", "-qm", "fixture"]);
        Self {
            temporary,
            root,
            events,
            writer_calls,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_spec-loop"))
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("run spec-loop")
    }
}

fn assert_role_response(output: &Output, role: &str) {
    let expected = format!("--- Agent {role} final response ---");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&expected),
        "missing role-labeled response for {role}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn run_with_git_interrupt(fixture: &Fixture, args: &[&str], phase: &str) -> (Output, PathBuf) {
    let tools = fixture.temporary.path().join("tools");
    let real_git = env::split_paths(&env::var_os("PATH").expect("PATH is set"))
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .expect("Git executable is available");
    let marker = fixture
        .temporary
        .path()
        .join(format!("git-{phase}-started"));
    let wrapper = tools.join("git");
    write_executable(
        &wrapper,
        "#!/bin/sh\nset -eu\ninterrupt=false\nif [ \"$SPEC_LOOP_INTERRUPT_AT\" = stage ] && [ \"${1-}\" = add ]; then interrupt=true; fi\nif [ \"$SPEC_LOOP_INTERRUPT_AT\" = commit ]; then\n  for argument do if [ \"$argument\" = commit ]; then interrupt=true; fi; done\nfi\nif [ \"$interrupt\" = true ]; then\n  printf 'started\\n' > \"$SPEC_LOOP_GIT_MARKER\"\n  kill -INT \"$PPID\"\n  sleep 30\nfi\nexec \"$SPEC_LOOP_REAL_GIT\" \"$@\"\n",
    );
    let mut search_path = vec![tools];
    search_path.extend(env::split_paths(&env::var_os("PATH").expect("PATH is set")));
    let search_path = env::join_paths(search_path).expect("join test PATH");
    let output = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(args)
        .current_dir(&fixture.root)
        .env("PATH", search_path)
        .env("SPEC_LOOP_REAL_GIT", real_git)
        .env("SPEC_LOOP_INTERRUPT_AT", phase)
        .env("SPEC_LOOP_GIT_MARKER", &marker)
        .output()
        .expect("run spec-loop with interrupting Git wrapper");
    (output, marker)
}

fn add_logged_check(fixture: &Fixture) {
    let check = fixture.temporary.path().join("tools/logged-check");
    write_executable(
        &check,
        &format!(
            "#!/bin/sh\nset -eu\nprintf 'check\\n' >> '{}'\n",
            fixture.events.display()
        ),
    );
    let profile_path = fixture.root.join(".spec-loop.toml");
    let mut profile = fs::read_to_string(&profile_path).expect("profile");
    writeln!(
        profile,
        "\n[[checks]]\nname = \"logged check\"\nargv = [{:?}]\ncwd = \".\"",
        check.to_string_lossy()
    )
    .expect("append logged check");
    fs::write(profile_path, profile).expect("configure logged check");
}

fn add_plan_mutating_check(fixture: &Fixture) {
    let check = fixture.temporary.path().join("tools/plan-mutating-check");
    write_executable(
        &check,
        &format!(
            "#!/bin/sh\nset -eu\nprintf 'check mutation\\n' >> '{}/.spec-loop/plans/001.md'\nprintf 'check\\n' >> '{}'\n",
            fixture.root.display(),
            fixture.events.display()
        ),
    );
    let profile_path = fixture.root.join(".spec-loop.toml");
    let mut profile = fs::read_to_string(&profile_path).expect("profile");
    writeln!(
        profile,
        "\n[[checks]]\nname = \"plan mutating check\"\nargv = [{:?}]\ncwd = \".\"",
        check.to_string_lossy()
    )
    .expect("append plan mutating check");
    fs::write(profile_path, profile).expect("configure plan mutating check");
}

fn configure_implementation_input_mutation(fixture: &Fixture, path: &str) {
    fs::write(
        fixture.temporary.path().join("mode"),
        "mutate-protected-input",
    )
    .expect("set input mutation mode");
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let replacement = format!(
        "printf 'implementation\\n' >> \"$EVENTS\"\nif [ \"$MODE\" = 'mutate-protected-input' ]; then printf 'tampered\\n' >> '{path}'; fi\n"
    );
    let updated = script.replace("printf 'implementation\\n' >> \"$EVENTS\"\n", &replacement);
    assert_ne!(
        updated, script,
        "implementation mutation hook was not installed"
    );
    fs::write(agent, updated).expect("update fake Pi mutation hook");
}

fn check_log_from_prompt(prompt: &str) -> PathBuf {
    prompt
        .lines()
        .find_map(|line| line.strip_prefix("Log: "))
        .map(PathBuf::from)
        .expect("check log path in prompt summary")
}

fn git(root: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn set_max_passes(fixture: &Fixture, max_passes: usize) {
    let path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&path).expect("read profile");
    let profile = profile.replace("max_passes = 2", &format!("max_passes = {max_passes}"));
    fs::write(&path, profile).expect("set retry limit");
    git(&fixture.root, &["add", ".spec-loop.toml"]);
    git(&fixture.root, &["commit", "-qm", "set retry limit"]);
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable");
    let mut permissions = fs::metadata(path)
        .expect("executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable bit");
}

#[test]
fn missing_check_executable_is_rejected_before_agent_invocation() {
    let fixture = Fixture::new(false, "normal", false);
    let mut profile = fs::read_to_string(fixture.root.join(".spec-loop.toml")).expect("profile");
    profile.push_str(
        "\n[[checks]]\nname = \"missing\"\nargv = [\"spec-loop-no-such-check-executable\"]\ncwd = \".\"\n",
    );
    fs::write(fixture.root.join(".spec-loop.toml"), profile).expect("update profile");
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("check \"missing\" executable"));
    assert!(!fixture.events.exists());
}

#[test]
fn unborn_repository_is_rejected_before_agent_invocation() {
    let fixture = Fixture::new(false, "normal", false);
    git(
        &fixture.root,
        &["symbolic-ref", "HEAD", "refs/heads/unborn"],
    );

    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no valid HEAD commit"));
    assert!(!fixture.events.exists(), "agent was invoked without HEAD");
    assert!(!fixture.root.join("generated.txt").exists());
}

#[test]
fn git_visible_change_during_checks_restarts_check_cycle() {
    let fixture = Fixture::new(true, "normal", false);
    let check_path = fixture.temporary.path().join("tools/check-one");
    let marker = fixture.temporary.path().join("check-mutated-input");
    let script = fs::read_to_string(&check_path).expect("read check script");
    let replacement = format!(
        "[ \"$count\" -ge 2 ]\nif [ ! -f '{}' ]; then printf 'late edit\\n' >> \"$PWD/generated.txt\"; : > '{}'; fi",
        marker.display(),
        marker.display()
    );
    let updated = script.replace("[ \"$count\" -ge 2 ]", &replacement);
    assert_ne!(updated, script, "check did not gain one-time mutation");
    fs::write(&check_path, updated).expect("configure mutating check");

    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(marker.is_file());
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Git-visible patch changed during checks; rerunning complete check list.")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation",
            "check-one",
            "check-two",
            "implementation",
            "check-one",
            "check-two",
            "check-one",
            "check-two",
            "review"
        ]
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("generated.txt")).expect("updated output"),
        "implemented\nlate edit\n"
    );
    assert!(
        String::from_utf8_lossy(&git(&fixture.root, &["status", "--porcelain"]).stdout)
            .contains("A  generated.txt")
    );
}
#[test]
fn failed_check_runs_all_checks_then_repairs_before_staging_or_review() {
    let fixture = Fixture::new(true, "normal", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let events = fs::read_to_string(&fixture.events).expect("events");
    assert_eq!(
        events.lines().collect::<Vec<_>>(),
        [
            "implementation",
            "check-one",
            "check-two",
            "implementation",
            "check-one",
            "check-two",
            "review"
        ]
    );
    assert_eq!(
        fs::read_to_string(&fixture.writer_calls).expect("writer count"),
        "2"
    );
    assert!(fs::read_to_string(fixture.root.join("generated.txt")).is_ok());
    let status = String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
        .expect("Git status UTF-8");
    assert!(
        status.contains("A  generated.txt"),
        "unexpected status: {status:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn timeout_check_terminates_descendant_holding_output_pipe() {
    let fixture = Fixture::new(false, "normal", false);
    let child_pid_path = fixture.temporary.path().join("timeout-child-pid");
    let check = fixture.temporary.path().join("timeout-check");
    write_executable(
        &check,
        &format!(
            "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n",
            child_pid_path.display()
        ),
    );
    let mut profile = fs::read_to_string(fixture.root.join(".spec-loop.toml")).expect("profile");
    writeln!(
        profile,
        "\n[[checks]]\nname = \"timed\"\nargv = [{:?}]\ncwd = \".\"\ntimeout_seconds = 1",
        check.to_string_lossy()
    )
    .expect("append timeout check");
    fs::write(fixture.root.join(".spec-loop.toml"), profile).expect("update profile");
    git(&fixture.root, &["add", ".spec-loop.toml"]);
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("timed out"));
    let pid = fs::read_to_string(child_pid_path)
        .expect("read child PID")
        .trim()
        .parse::<u32>()
        .expect("parse child PID");
    let process_stat = PathBuf::from(format!("/proc/{pid}/stat"));
    match fs::read_to_string(process_stat) {
        Ok(stat) => {
            let state = stat
                .rsplit_once(')')
                .map(|(_, fields)| fields.trim_start().chars().next());
            assert_eq!(
                state,
                Some(Some('Z')),
                "timeout descendant is still running"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("inspect timed-out descendant: {error}"),
    }
}

#[test]
fn malformed_agent_output_retries_then_preserves_changes_without_staging() {
    let fixture = Fixture::new(false, "agent-malformed", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("completion.max_passes limit"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("malformed Pi JSONL"));
    assert!(!fixture.events.exists());
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
}

#[test]
fn nonzero_agent_exit_retries_then_preserves_changes_without_staging() {
    let fixture = Fixture::new(false, "agent-nonzero", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("completion.max_passes limit"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Pi exited"));
    assert!(!fixture.events.exists());
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
}

#[test]
fn interrupted_agent_retains_partial_stdout_and_stderr_in_run_logs() {
    let fixture = Fixture::new(false, "agent-interrupted-output", false);

    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted"));
    let output_text = String::from_utf8_lossy(&output.stdout);
    assert!(!output_text.contains("partial agent stdout"));
    assert!(!output_text.contains("partial agent stderr"));
    let log_line = output_text
        .lines()
        .find(|line| line.starts_with("Agent implementation logs: "))
        .expect("agent log locations are reported");
    let paths = log_line
        .strip_prefix("Agent implementation logs: ")
        .expect("log prefix")
        .split_once(", ")
        .expect("stdout and stderr log paths");
    let stdout_log = fs::read(paths.0).expect("partial stdout log");
    let stderr_log = fs::read(paths.1).expect("partial stderr log");
    assert!(
        stdout_log
            .windows(b"partial agent stdout".len())
            .any(|window| { window == b"partial agent stdout" })
    );
    assert!(
        stderr_log
            .windows(b"partial agent stderr".len())
            .any(|window| { window == b"partial agent stderr" })
    );
}

#[test]
fn retry_limit_stops_without_staging_after_failed_check() {
    let fixture = Fixture::new(true, "normal", false);
    set_max_passes(&fixture, 1);
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("completion.max_passes limit"));
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "check-one", "check-two"]
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("status")
            .contains("?? generated.txt")
    );
}

#[test]
fn review_resume_ignores_check_generated_ignored_artifacts() {
    let fixture = Fixture::new(false, "normal", false);
    let build_check = fixture.temporary.path().join("ignored-build-check");
    write_executable(
        &build_check,
        &format!(
            "#!/bin/sh\nset -eu\nmkdir -p ignored-build\nprintf 'artifact\\n' > ignored-build/output\nprintf 'build\\n' >> '{}'\n",
            fixture.events.display()
        ),
    );
    let profile_path = fixture.root.join(".spec-loop.toml");
    let mut profile = fs::read_to_string(&profile_path).expect("profile");
    writeln!(
        profile,
        "\n[[checks]]\nname = \"build\"\nargv = [{:?}]\ncwd = \".\"",
        build_check.to_string_lossy()
    )
    .expect("append build check");
    fs::write(profile_path, profile).expect("configure build check");
    let gitignore_path = fixture.root.join(".gitignore");
    let mut gitignore = fs::read_to_string(&gitignore_path).expect("read ignore rules");
    gitignore.push_str("\n/ignored-build/\n");
    fs::write(&gitignore_path, gitignore).expect("ignore build artifacts");
    git(&fixture.root, &["add", ".spec-loop.toml", ".gitignore"]);
    git(&fixture.root, &["commit", "-qm", "configure build check"]);
    fs::create_dir_all(fixture.root.join("ignored-build")).expect("ignored build directory");
    fs::write(
        fixture.root.join("ignored-build/output"),
        "previous artifact\n",
    )
    .expect("previous artifact");
    let ignore_result = git(
        &fixture.root,
        &["check-ignore", "-v", "ignored-build/output"],
    );
    assert!(String::from_utf8_lossy(&ignore_result.stdout).contains("ignored-build/"));
    fs::write(fixture.root.join("prepared.txt"), "staged input\n").expect("prepared file");
    git(&fixture.root, &["add", "prepared.txt"]);

    let output = fixture.run(&["--spec", "001", "--review"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root.join("ignored-build/output").is_file());
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["build", "review"]
    );
}

#[test]
fn review_resume_accepts_staged_unstaged_and_untracked_changes() {
    let fixture = Fixture::new(false, "normal", false);
    fs::write(fixture.root.join("specs/001.md"), "# Spec\\nUser edit\\n").expect("modify spec");
    fs::write(fixture.root.join("prepared.txt"), "staged input\\n").expect("staged input");
    git(&fixture.root, &["add", "prepared.txt"]);
    fs::write(fixture.root.join("untracked.txt"), "untracked input\\n").expect("untracked input");
    let output = fixture.run(&["--spec", "001", "--review"]);
    assert!(
        output.status.success(),
        "stdout: {}\\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["review"]
    );
    let status =
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout).expect("status");
    assert!(
        status.contains("M  specs/001.md"),
        "unstaged input not staged: {status:?}"
    );
    assert!(
        status.contains("A  prepared.txt"),
        "staged input missing: {status:?}"
    );
    assert!(
        status.contains("A  untracked.txt"),
        "untracked input missing: {status:?}"
    );
}

#[test]
fn review_resume_runs_checks_first_then_repairs_failures() {
    let fixture = Fixture::new(true, "normal", false);
    fs::write(fixture.root.join("prepared.txt"), "staged input\n").expect("prepared file");
    git(&fixture.root, &["add", "prepared.txt"]);
    let output = fixture.run(&["--spec", "001", "--review"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "check-one",
            "check-two",
            "implementation",
            "check-one",
            "check-two",
            "implementation",
            "check-one",
            "check-two",
            "review"
        ]
    );
}

#[test]
fn timed_out_writer_is_retried_with_timeout_feedback_and_then_reviewed() {
    let fixture = Fixture::new(false, "agent-timeout-once", false);
    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.writer_calls).expect("writer count"),
        "2"
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "implementation", "review"]
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("Git status")
            .contains("A  generated.txt")
    );
}

#[test]
fn timed_out_writer_counts_against_limit_without_staging_or_review() {
    let fixture = Fixture::new(false, "agent-timeout-once", false);
    set_max_passes(&fixture, 1);
    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("completion.max_passes limit"));
    assert_eq!(
        fs::read_to_string(&fixture.writer_calls).expect("writer count"),
        "1"
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation"]
    );
    assert!(fixture.root.join("generated.txt").is_file());
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("Git status")
            .contains("?? generated.txt")
    );
}

#[test]
fn finding_audit_repairs_only_supported_findings() {
    let fixture = Fixture::new(false, "audit", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_role_response(&output, "implementation");
    assert_role_response(&output, "review");
    assert_role_response(&output, "review-audit");
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation",
            "review",
            "review-audit",
            "implementation",
            "review"
        ]
    );
    assert_eq!(
        fs::read_to_string(&fixture.writer_calls).expect("writer count"),
        "2"
    );
}

#[test]
fn agent_prompts_are_not_printed_to_terminal() {
    let fixture = Fixture::new(false, "normal", false);
    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("--- Prompt sent to agent"));
    assert!(!stdout.contains("Do not stage or commit changes"));
    assert_role_response(&output, "implementation");
    assert_role_response(&output, "review");
    assert!(
        fs::read_to_string(fixture.temporary.path().join("prompt.txt"))
            .expect("agent prompt")
            .contains("Do not stage or commit changes")
    );
}

#[test]
fn execution_reports_stages_and_redacted_child_commands_before_agent_output() {
    let fixture = Fixture::new(true, "normal", false);
    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let implementation_stage = stdout
        .find("Stage: agent implementation (attempt 1; spec 001)")
        .expect("implementation stage is reported");
    let agent_command = stdout
        .find("Command argv: [")
        .expect("agent command argv is reported");
    let implementation_response = stdout
        .find("--- Agent implementation final response ---")
        .expect("implementation response is reported");
    assert!(implementation_stage < agent_command);
    assert!(agent_command < implementation_response);
    assert!(stdout.contains("<prompt omitted>"));
    assert!(!stdout.contains("Do not stage or commit changes"));
    assert!(stdout.contains("Stage: check first"));
    assert!(stdout.contains("Stage: check second"));
    assert!(stdout.contains("check-one\"]"));
    assert!(stdout.contains("check-two\"]"));
}

#[test]
fn unsupported_findings_contradicted_by_spec_or_repository_instructions_are_not_repaired() {
    for mode in ["audit-unsupported-spec", "audit-unsupported-agents"] {
        let fixture = Fixture::new(false, mode, false);
        let output = fixture.run(&["--spec", "001"]);

        assert!(
            output.status.success(),
            "spec-loop failed in {mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("Auditor rejected unsupported findings")
        );
        assert_eq!(
            fs::read_to_string(&fixture.events)
                .expect("events")
                .lines()
                .collect::<Vec<_>>(),
            ["implementation", "review", "review-audit", "review"]
        );
        assert!(fixture.root.join("generated.txt").is_file());
        assert!(
            String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
                .expect("status")
                .contains("A  generated.txt")
        );
    }
}

#[test]
fn out_of_scope_file_change_alone_is_not_review_finding() {
    let fixture = Fixture::new(false, "out-of-scope", false);
    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(fixture.temporary.path().join("prompt.txt"))
            .expect("review prompt")
            .contains("An out-of-scope file change alone is not a finding")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review"]
    );
    assert!(fixture.root.join("out-of-scope.txt").is_file());
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("Git status")
            .contains("A  out-of-scope.txt")
    );
}

#[test]
fn supported_defect_survives_poor_remedy_and_gets_minimal_fix() {
    let fixture = Fixture::new(false, "supported-poor-remedy", false);
    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation",
            "review",
            "review-audit",
            "implementation",
            "review"
        ]
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("generated.txt")).expect("repaired output"),
        "required\nstable\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("unrelated.txt")).expect("preserved output"),
        "preserve me\n"
    );
    let changed_paths =
        String::from_utf8(git(&fixture.root, &["diff", "--name-only", "HEAD"]).stdout)
            .expect("changed paths");
    assert_eq!(
        changed_paths.lines().collect::<Vec<_>>(),
        ["generated.txt", "unrelated.txt"]
    );
    let minimal_diff = String::from_utf8(
        git(
            &fixture.root,
            &["diff", "--unified=0", "HEAD", "--", "generated.txt"],
        )
        .stdout,
    )
    .expect("generated file diff");
    assert!(minimal_diff.lines().any(|line| line == "-wrong"));
    assert!(minimal_diff.lines().any(|line| line == "+required"));
    assert!(
        !minimal_diff
            .lines()
            .any(|line| line == "-stable" || line == "+stable")
    );
}

#[test]
fn failed_check_prompt_uses_bounded_summary_and_retains_complete_log() {
    let fixture = Fixture::new(true, "check-summary-repair", false);
    let fake_pi = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&fake_pi).expect("read fake Pi script");
    let changed = script.replace(
        r#"  if [ "$MODE" = 'supported-poor-remedy' ]; then
"#,
        r#"  if [ "$MODE" = 'check-summary-repair' ]; then
    if [ "$count" -gt 1 ]; then
      cp "$PROMPT" "$MODE_FILE.implementation-prompt"
      grep -q 'Check: first' "$PROMPT"
      grep -q 'Status: exit status: 1' "$PROMPT"
      grep -q 'FAILURE_EXCERPT' "$PROMPT"
      grep -Eq 'Log: /.*check-[0-9]+\.log' "$PROMPT"
      if grep -q 'FULL_OUTPUT_SENTINEL_BEGIN' "$PROMPT"; then exit 9; fi
    fi
  elif [ "$MODE" = 'supported-poor-remedy' ]; then
"#,
    );
    assert_ne!(
        changed, script,
        "fake writer check-summary behavior was not inserted"
    );
    fs::write(fake_pi, changed).expect("update fake Pi script");
    let check = fixture.temporary.path().join("tools/check-one");
    write_executable(
        &check,
        &format!(
            r#"#!/bin/sh
set -eu
printf 'check-one\n' >> '{}'
count=$(cat '{}' 2>/dev/null || printf '0')
printf 'FULL_OUTPUT_SENTINEL_BEGIN\n'
i=0
while [ "$i" -lt 40 ]; do printf 'failure-noise-%s\n' "$i"; i=$((i + 1)); done
printf 'FAILURE_EXCERPT: check needs repair\n'
[ "$count" -ge 2 ]
"#,
            fixture.events.display(),
            fixture.writer_calls.display()
        ),
    );

    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let prompt = fs::read_to_string(fixture.temporary.path().join("mode.implementation-prompt"))
        .expect("implementation prompt");
    assert!(!prompt.contains("FULL_OUTPUT_SENTINEL_BEGIN"));
    assert!(prompt.contains("FAILURE_EXCERPT"));
    let log = fs::read_to_string(check_log_from_prompt(&prompt)).expect("complete check log");
    assert!(log.contains("FULL_OUTPUT_SENTINEL_BEGIN"));
    assert!(log.contains("failure-noise-0"));
    assert!(log.contains("failure-noise-39"));
    assert!(log.contains("FAILURE_EXCERPT"));
    let terminal = String::from_utf8_lossy(&output.stdout);
    assert!(terminal.contains("FAILURE_EXCERPT"));
    assert!(!terminal.contains("FULL_OUTPUT_SENTINEL_BEGIN"));
}

#[test]
fn review_prompt_gets_check_summary_and_log_path_not_full_output() {
    let fixture = Fixture::new(true, "check-summary-review", false);
    let fake_pi = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&fake_pi).expect("read fake Pi script");
    let changed = script.replace(
        r#"  if [ "$MODE" = 'normal' ] && git ls-files --error-unmatch generated.txt >/dev/null 2>&1; then exit 9; fi
"#,
        r#"  if [ "$MODE" = 'normal' ] && git ls-files --error-unmatch generated.txt >/dev/null 2>&1; then exit 9; fi
  if [ "$MODE" = 'check-summary-review' ]; then
    cp "$PROMPT" "$MODE_FILE.review-prompt"
    grep -q 'Configured check summaries' "$PROMPT"
    grep -q 'Check: first' "$PROMPT"
    grep -q 'PASS_EXCERPT' "$PROMPT"
    grep -Eq 'Log: /.*check-[0-9]+\.log' "$PROMPT"
    if grep -q 'FULL_OUTPUT_SENTINEL_BEGIN' "$PROMPT"; then exit 9; fi
  fi
"#,
    );
    assert_ne!(
        changed, script,
        "fake reviewer check-summary behavior was not inserted"
    );
    fs::write(fake_pi, changed).expect("update fake Pi script");
    let check = fixture.temporary.path().join("tools/check-one");
    write_executable(
        &check,
        &format!(
            r#"#!/bin/sh
set -eu
printf 'check-one\n' >> '{}'
count=$(cat '{}' 2>/dev/null || printf '0')
printf 'FULL_OUTPUT_SENTINEL_BEGIN\n'
i=0
while [ "$i" -lt 40 ]; do printf 'pass-noise-%s\n' "$i"; i=$((i + 1)); done
printf 'PASS_EXCERPT: check passed\n'
[ "$count" -ge 1 ]
"#,
            fixture.events.display(),
            fixture.writer_calls.display()
        ),
    );

    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let prompt = fs::read_to_string(fixture.temporary.path().join("mode.review-prompt"))
        .expect("review prompt");
    assert!(prompt.contains("Configured check summaries"));
    assert!(prompt.contains("PASS_EXCERPT"));
    assert!(!prompt.contains("FULL_OUTPUT_SENTINEL_BEGIN"));
    let log = fs::read_to_string(check_log_from_prompt(&prompt)).expect("complete check log");
    assert!(log.contains("FULL_OUTPUT_SENTINEL_BEGIN"));
    assert!(log.contains("pass-noise-0"));
    assert!(log.contains("pass-noise-39"));
    assert!(log.contains("PASS_EXCERPT"));
}

#[test]
fn check_logs_preserve_non_utf8_stdout_and_stderr_bytes() {
    let fixture = Fixture::new(false, "normal", false);
    let check = fixture.temporary.path().join("binary-output-check");
    write_executable(
        &check,
        "#!/bin/sh\nprintf 'stdout-before\\377stdout-after'\nprintf 'stderr-before\\376stderr-after' >&2\n",
    );
    let profile_path = fixture.root.join(".spec-loop.toml");
    let mut profile = fs::read_to_string(&profile_path).expect("profile");
    writeln!(
        profile,
        "\n[[checks]]\nname = \"bytes\"\nargv = [{:?}]\ncwd = \".\"",
        check.to_string_lossy()
    )
    .expect("append binary-output check");
    fs::write(&profile_path, profile).expect("configure binary-output check");
    git(&fixture.root, &["add", ".spec-loop.toml"]);
    git(
        &fixture.root,
        &["commit", "-qm", "configure binary-output check"],
    );

    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let output_text = String::from_utf8_lossy(&output.stdout);
    let log_path = output_text
        .lines()
        .find_map(|line| {
            line.strip_prefix("Check bytes: passed (log: ")
                .and_then(|line| line.strip_suffix(')'))
        })
        .expect("check log path");
    let log = fs::read(log_path).expect("read raw check log");
    for expected in [
        b"stdout-before\xffstdout-after".as_slice(),
        b"stderr-before\xfestderr-after".as_slice(),
    ] {
        assert!(
            log.windows(expected.len()).any(|window| window == expected),
            "missing raw output bytes {expected:?} in {log:?}"
        );
    }
}

#[test]
fn evidence_clarification_cannot_add_or_rewrite_findings() {
    let fixture = Fixture::new(false, "normal", false);
    fs::write(
        fixture.temporary.path().join("mode"),
        "finding-clarification-injection",
    )
    .expect("set clarification test mode");
    set_max_passes(&fixture, 4);
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("fake Pi script");
    let with_audit_behavior = script.replace(
        "  if [ \"$MODE\" = 'audit-incomplete' ]; then\n",
        "  if [ \"$MODE\" = 'finding-clarification-injection' ]; then\n    if grep -q 'Reviewer evidence clarification' \"$PROMPT\"; then\n      if grep -q 'FINDING-2: injected finding' \"$PROMPT\"; then\n        TEXT='SUPPORTED_FINDINGS:\\nFINDING-1: original finding | AUDIT RATIONALE: supported\\nFINDING-2: injected finding | AUDIT RATIONALE: supported\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=ACTIONABLE'\n      else\n        TEXT='SUPPORTED_FINDINGS:\\nNone\\nUNSUPPORTED_FINDINGS:\\nFINDING-1: original finding | AUDIT RATIONALE: no cited evidence supports the concern\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=NO_ACTIONABLE_FINDINGS'\n      fi\n    else\n      TEXT='SUPPORTED_FINDINGS:\\nNone\\nUNSUPPORTED_FINDINGS:\\nFINDING-1: original finding | AUDIT RATIONALE: evidence is not established\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=NO_ACTIONABLE_FINDINGS'\n    fi\n  elif [ \"$MODE\" = 'audit-incomplete' ]; then\n",
    );
    assert_ne!(
        with_audit_behavior, script,
        "fake Pi audit behavior was not inserted"
    );
    let with_review_behavior = with_audit_behavior.replace(
        "  case \"$MODE:$count\" in\n    audit:1|audit-incomplete:1)",
        "  case \"$MODE:$count\" in\n    finding-clarification-injection:1) TEXT='FINDING-1: original finding\\nSPEC_LOOP_REVIEW=ISSUES' ;;\n    finding-clarification-injection:2) TEXT='FINDING-1: rewritten finding\\nFINDING-2: injected finding\\nEVIDENCE-FINDING-1: fabricated citation\\nSPEC_LOOP_REVIEW=ISSUES' ;;\n    finding-clarification-injection:3) TEXT='FINDING-1: original finding\\nEVIDENCE-FINDING-1: source line 42 confirms behavior\\nSPEC_LOOP_REVIEW=ISSUES' ;;\n    audit:1|audit-incomplete:1)",
    );
    assert_ne!(
        with_review_behavior, with_audit_behavior,
        "fake Pi review responses were not inserted"
    );
    fs::write(agent, with_review_behavior).expect("update fake Pi script");

    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation",
            "review",
            "review-audit",
            "review",
            "review",
            "review-audit"
        ]
    );
    assert_eq!(
        fs::read_to_string(&fixture.writer_calls).expect("writer count"),
        "1"
    );
}

#[test]
fn ambiguous_finding_is_requested_once_then_discarded_if_still_unproven() {
    let fixture = Fixture::new(false, "audit", false);
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let changed = script.replace(
        "TEXT='SUPPORTED_FINDINGS:\\nFINDING-1: reviewer finding with location and evidence | AUDIT RATIONALE: supported by the applicable requirement\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=ACTIONABLE'",
        "TEXT='SUPPORTED_FINDINGS:\\nNone\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nFINDING-1: reviewer finding with location and evidence | AUDIT RATIONALE: evidence is inconclusive\\nSPEC_LOOP_REVIEW_AUDIT=AMBIGUOUS'",
    );
    assert_ne!(changed, script, "fake Pi audit response was not updated");
    fs::write(agent, changed).expect("update fake Pi script");
    let head_before = git(&fixture.root, &["rev-parse", "HEAD"]).stdout;

    let output = fixture.run(&["--spec", "001", "--commit"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Auditor marked findings ambiguous"));
    assert_ne!(
        git(&fixture.root, &["rev-parse", "HEAD"]).stdout,
        head_before
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "review-audit", "review"]
    );
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
}

#[test]
fn unproven_finding_without_reviewer_evidence_is_discarded_after_one_request() {
    let fixture = Fixture::new(false, "audit", false);
    fs::write(
        fixture.temporary.path().join("mode"),
        "clarification-no-evidence",
    )
    .expect("set clarification mode");
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let changed = script
        .replace(
            "TEXT='SUPPORTED_FINDINGS:\\nFINDING-1: reviewer finding with location and evidence | AUDIT RATIONALE: supported by the applicable requirement\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nNone\\nSPEC_LOOP_REVIEW_AUDIT=ACTIONABLE'",
            "TEXT='SUPPORTED_FINDINGS:\\nNone\\nUNSUPPORTED_FINDINGS:\\nNone\\nAMBIGUOUS_FINDINGS:\\nFINDING-1: reviewer finding with location and evidence | AUDIT RATIONALE: evidence is inconclusive\\nSPEC_LOOP_REVIEW_AUDIT=AMBIGUOUS'",
        )
        .replace(
            "  case \"$MODE:$count\" in\n    audit:1|audit-incomplete:1)",
            "  case \"$MODE:$count\" in\n    clarification-no-evidence:1|clarification-no-evidence:2) TEXT='FINDING-1: reviewer finding with location and evidence\\nSPEC_LOOP_REVIEW=ISSUES' ;;\n    audit:1|audit-incomplete:1)",
        );
    assert_ne!(changed, script, "clarification behavior was not configured");
    fs::write(agent, changed).expect("configure fake Pi clarification behavior");

    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Auditor marked findings ambiguous"));
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "review-audit", "review"]
    );
    assert_eq!(
        fs::read_to_string(&fixture.writer_calls).expect("writer count"),
        "1"
    );
    assert!(fixture.root.join("generated.txt").is_file());
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("Git status")
            .contains("A  generated.txt")
    );
}

#[test]
fn noncanonical_review_status_auditor_uses_repository_pi_context() {
    let fixture = Fixture::new(false, "status", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "status-audit"]
    );
    assert_role_response(&output, "implementation");
    assert_role_response(&output, "review");
    assert_role_response(&output, "review-status-audit");
    let auditor_cwd = fs::read_to_string(fixture.temporary.path().join("status-context"))
        .expect("status auditor working directory");
    assert_eq!(Path::new(auditor_cwd.trim()), fixture.root);
    let auditor_args = fs::read_to_string(fixture.temporary.path().join("status-args"))
        .expect("status auditor arguments");
    for restricted_option in [
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
            !auditor_args
                .lines()
                .any(|argument| argument == restricted_option),
            "{restricted_option} unexpectedly passed to status auditor"
        );
    }
    assert!(
        auditor_args
            .lines()
            .any(|argument| argument == "--no-session")
    );
    assert!(auditor_args.lines().any(|argument| argument == "--mode"));
    assert!(auditor_args.lines().any(|argument| argument == "json"));
}

#[test]
fn relative_agent_executable_works_for_status_auditor_in_repository_cwd() {
    let fixture = Fixture::new(false, "relative-executable", false);
    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "status-audit"]
    );
    let auditor_cwd = fs::read_to_string(fixture.temporary.path().join("status-context"))
        .expect("status auditor working directory");
    assert_eq!(Path::new(auditor_cwd.trim()), fixture.root);
}

#[test]
fn noncanonical_review_status_normalizes_to_issues_and_preserves_raw_response() {
    let fixture = Fixture::new(false, "status-issues", false);
    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation",
            "review",
            "status-audit",
            "review-audit",
            "implementation",
            "review"
        ]
    );
    let log_root = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Run logs: "))
        .map(PathBuf::from)
        .expect("run log path");
    let mut logs = Vec::new();
    for entry in fs::read_dir(log_root).expect("run logs") {
        let path = entry.expect("run log entry").path();
        if path.is_file() {
            logs.push(fs::read_to_string(path).expect("read run log"));
        }
    }
    assert!(
        logs.iter()
            .any(|log| { log == "FINDING-1: The reviewer reports one possible issue." })
    );
    assert!(
        logs.iter()
            .any(|log| log == "SPEC_LOOP_REVIEW_STATUS_AUDIT=ISSUES")
    );
}

#[test]
fn ambiguous_status_audit_requests_one_reviewer_clarification_then_pauses() {
    let fixture = Fixture::new(false, "status-ambiguous", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("remains ambiguous after reviewer clarification")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation",
            "review",
            "status-audit",
            "review",
            "status-audit"
        ]
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("status")
            .contains("?? generated.txt")
    );
}

#[test]
fn malformed_noncanonical_findings_retry_reviewer_not_status_auditor() {
    let fixture = Fixture::new(false, "status-malformed-findings", false);
    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "review"]
    );
    assert!(
        fs::read_to_string(fixture.temporary.path().join("prompt.txt"))
            .expect("retry prompt")
            .contains("malformed or non-sequential findings")
    );
}

#[test]
fn contradictory_status_audit_retries_then_stops_at_profile_limit() {
    let fixture = Fixture::new(false, "status-contradiction", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("completion.max_passes limit"));
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "status-audit", "status-audit"]
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("status")
            .contains("?? generated.txt")
    );
}

#[test]
fn malformed_no_issues_reviewer_response_retries_with_format_feedback() {
    let fixture = Fixture::new(false, "status-prose-contradiction", false);
    let output = fixture.run(&["--spec", "001"]);

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "review"]
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("status")
            .contains("A  generated.txt")
    );
}

#[test]
fn malformed_status_audit_fails_closed_and_preserves_reviewed_changes() {
    let fixture = Fixture::new(false, "status-invalid", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("completion.max_passes limit"));
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "status-audit", "status-audit"]
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("status")
            .contains("?? generated.txt")
    );
}

#[test]
fn archive_transition_is_included_in_pre_staging_review() {
    let fixture = Fixture::new(false, "normal", true);
    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.root.join("specs/001.md").exists());
    assert!(fixture.root.join("archive/001.md").exists());
    let status =
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout).expect("status");
    assert!(
        status.contains("archive/001.md"),
        "unexpected status: {status:?}"
    );
    assert!(
        status.contains("specs/001.md"),
        "unexpected status: {status:?}"
    );
}

#[test]
fn archive_transition_preserves_original_spec_filename() {
    let fixture = Fixture::new(false, "normal", true);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("profile");
    let profile = profile.replace(
        "id_regex = \"^(?:specs|archive)/(?P<id>[0-9]+)\\\\.md$\"",
        "id_regex = \"^(?:specs|archive)/(?P<id>[0-9]+)(?:-[a-z0-9-]+)?\\\\.md$\"",
    );
    assert_ne!(
        profile,
        fs::read_to_string(&profile_path).expect("original profile")
    );
    fs::write(profile_path, profile).expect("configure filename-preserving archive");
    fs::rename(
        fixture.root.join("specs/001.md"),
        fixture.root.join("specs/001-spec-loop.md"),
    )
    .expect("rename fixture spec to its original filename");

    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.root.join("specs/001-spec-loop.md").exists());
    assert!(fixture.root.join("archive/001-spec-loop.md").is_file());
}

#[test]
fn archive_repair_uses_archived_spec_path_for_checks_and_agent_prompts() {
    let fixture = Fixture::new(true, "normal", true);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("profile");
    let check_one = fixture.temporary.path().join("tools/check-one");
    let original_argv = format!("argv = [{:?}]", check_one.to_string_lossy());
    let archived_argv = format!(
        "argv = [{:?}, \"{{spec_path}}\"]",
        check_one.to_string_lossy()
    );
    let profile = profile.replace(&original_argv, &archived_argv);
    assert_ne!(
        profile,
        fs::read_to_string(&profile_path).expect("original profile")
    );
    fs::write(&profile_path, profile).expect("configure spec-path check argument");

    let check = format!(
        "#!/bin/sh\nset -eu\nprintf 'check-one\\n' >> '{}'\n[ \"${{1-}}\" = \"archive/001.md\" ]\n[ -f \"$1\" ]\ncount=$(cat '{}' 2>/dev/null || printf '0')\n[ \"$count\" -ge 2 ]\n",
        fixture.events.display(),
        fixture.writer_calls.display()
    );
    write_executable(&check_one, &check);

    let dry_run = fixture.run(&["--spec", "001", "--dry-run"]);
    assert!(
        dry_run.status.success(),
        "dry-run failed: {}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    assert!(String::from_utf8_lossy(&dry_run.stdout).contains("archive/001.md"));
    assert!(fixture.root.join("specs/001.md").is_file());
    assert!(!fixture.root.join("archive/001.md").exists());

    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("fake Pi script");
    let updated = script.replace(
        "  printf 'implementation\\n' >> \"$EVENTS\"\n",
        "  printf 'implementation\\n' >> \"$EVENTS\"\n  if [ \"$count\" -gt 1 ]; then grep -q '^Selected spec path: archive/001.md$' \"$PROMPT\"; fi\n",
    );
    assert_ne!(updated, script, "fake Pi repair assertion was not inserted");
    fs::write(agent, updated).expect("update fake Pi script");

    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation",
            "check-one",
            "check-two",
            "implementation",
            "check-one",
            "check-two",
            "review"
        ]
    );
    assert!(!fixture.root.join("specs/001.md").exists());
    assert!(fixture.root.join("archive/001.md").is_file());
    assert!(
        fs::read_to_string(fixture.temporary.path().join("prompt.txt"))
            .expect("review prompt")
            .contains("Selected spec path: archive/001.md")
    );
}

#[test]
fn commit_cli_switch_authorizes_commit() {
    let fixture = Fixture::new(false, "normal", false);
    let output = fixture.run(&["--spec", "001", "--commit"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
    let message = String::from_utf8(git(&fixture.root, &["log", "-1", "--pretty=%s"]).stdout)
        .expect("commit message");
    assert_eq!(message.trim(), "Implement 001");
}

fn configure_review_commit_case(fixture: &Fixture, mode: &str) {
    fs::write(fixture.temporary.path().join("mode"), mode).expect("set workflow mode");
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let changed = script
        .replace(
            "  elif [ \"$MODE\" = 'out-of-scope' ]; then\n",
            "  elif [ \"$MODE\" = 'review-created-change' ] || [ \"$MODE\" = 'no-changes' ]; then\n    :\n  elif [ \"$MODE\" = 'out-of-scope' ]; then\n",
        )
        .replace(
            "  printf 'review\\n' >> \"$EVENTS\"\n",
            "  printf 'review\\n' >> \"$EVENTS\"\n  if [ \"$MODE\" = 'review-created-change' ]; then printf 'created during review\\n' > reviewer-created.txt; fi\n",
        );
    assert_ne!(
        changed, script,
        "fake Pi review-commit behavior was not updated"
    );
    fs::write(agent, changed).expect("update fake Pi script");
}

#[test]
fn commit_includes_git_visible_changes_created_during_review() {
    let fixture = Fixture::new(false, "normal", false);
    configure_review_commit_case(&fixture, "review-created-change");
    let initial_head = git(&fixture.root, &["rev-parse", "HEAD"]).stdout;

    let output = fixture.run(&["--spec", "001", "--commit"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        git(&fixture.root, &["rev-parse", "HEAD"]).stdout,
        initial_head
    );
    assert_eq!(
        git(&fixture.root, &["show", "HEAD:reviewer-created.txt"]).stdout,
        b"created during review\n"
    );
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
}

#[test]
fn commit_without_changes_returns_standard_git_error() {
    let fixture = Fixture::new(false, "normal", false);
    configure_review_commit_case(&fixture, "no-changes");
    let initial_head = git(&fixture.root, &["rev-parse", "HEAD"]).stdout;

    let output = fixture.run(&["--spec", "001", "--commit"]);

    assert!(
        !output.status.success(),
        "no-op commit should retain standard Git failure semantics: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("git commit exited with"));
    assert_eq!(
        git(&fixture.root, &["rev-parse", "HEAD"]).stdout,
        initial_head
    );
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
}

#[test]
fn standard_commit_runs_configured_hooks() {
    let fixture = Fixture::new(false, "normal", false);
    let hooks = fixture.temporary.path().join("hooks");
    let marker = fixture.temporary.path().join("pre-commit-ran");
    fs::create_dir_all(&hooks).expect("create hooks directory");
    write_executable(
        &hooks.join("pre-commit"),
        &format!("#!/bin/sh\nprintf 'ran\\n' > '{}'\n", marker.display()),
    );
    git(
        &fixture.root,
        &[
            "config",
            "core.hooksPath",
            hooks.to_str().expect("hooks path"),
        ],
    );

    let output = fixture.run(&["--spec", "001", "--commit"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(marker).expect("pre-commit marker"),
        "ran\n"
    );
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
}

#[test]
fn failed_commit_hook_returns_standard_error_and_preserves_staging() {
    let fixture = Fixture::new(false, "normal", false);
    let hooks = fixture.temporary.path().join("hooks");
    fs::create_dir_all(&hooks).expect("create hooks directory");
    write_executable(
        &hooks.join("pre-commit"),
        "#!/bin/sh\nprintf 'hook rejected commit\\n'\nexit 1\n",
    );
    git(
        &fixture.root,
        &[
            "config",
            "core.hooksPath",
            hooks.to_str().expect("hooks path"),
        ],
    );
    let initial_head = git(&fixture.root, &["rev-parse", "HEAD"]).stdout;

    let output = fixture.run(&["--spec", "001", "--commit"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("hook rejected commit"));
    assert_eq!(
        git(&fixture.root, &["rev-parse", "HEAD"]).stdout,
        initial_head
    );
    assert!(
        git(&fixture.root, &["diff", "--cached", "--name-only"])
            .stdout
            .windows(b"generated.txt".len())
            .any(|window| window == b"generated.txt")
    );
}

#[test]
fn interrupt_during_staging_preserves_worktree_after_review() {
    let fixture = Fixture::new(false, "normal", false);

    let (output, marker) = run_with_git_interrupt(&fixture, &["--spec", "001"], "stage");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted"));
    assert!(marker.is_file());
    assert!(fixture.root.join("generated.txt").is_file());
    assert!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .contains("review")
    );
    let status = String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
        .expect("Git status");
    assert!(status.contains("?? generated.txt"), "status was {status:?}");
}

#[test]
fn interrupt_during_commit_preserves_head_and_staged_changes() {
    let fixture = Fixture::new(false, "normal", false);
    let head_before = git(&fixture.root, &["rev-parse", "HEAD"]).stdout;

    let (output, marker) =
        run_with_git_interrupt(&fixture, &["--spec", "001", "--commit"], "commit");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted"));
    assert!(marker.is_file());
    assert_eq!(
        git(&fixture.root, &["rev-parse", "HEAD"]).stdout,
        head_before
    );
    assert!(
        git(&fixture.root, &["diff", "--cached", "--name-only"])
            .stdout
            .windows(b"generated.txt".len())
            .any(|window| window == b"generated.txt")
    );
}

#[test]
fn manual_acceptance_gate_blocks_commit_and_continuation() {
    let fixture = Fixture::new(false, "continue", false);
    let mut profile = fs::read_to_string(fixture.root.join(".spec-loop.toml")).expect("profile");
    profile.push_str("\n[acceptance]\nmanual_gates = [\"user verification\"]\n");
    fs::write(fixture.root.join(".spec-loop.toml"), profile).expect("update profile");
    git(&fixture.root, &["add", ".spec-loop.toml"]);
    let initial_head = String::from_utf8(git(&fixture.root, &["rev-parse", "HEAD"]).stdout)
        .expect("initial HEAD")
        .trim()
        .to_owned();
    let output = fixture.run(&["--spec", "001", "--commit", "--continue"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("completion is paused"));
    assert_eq!(
        String::from_utf8(git(&fixture.root, &["rev-parse", "HEAD"]).stdout)
            .expect("final HEAD")
            .trim(),
        initial_head
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review"]
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("status")
            .contains("A  generated.txt")
    );
}

#[test]
fn authorized_continuation_selects_next_spec_after_commit() {
    let fixture = Fixture::new(false, "continue", false);
    let output = fixture.run(&["--spec", "001", "--commit", "--continue"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "implementation", "review"]
    );
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
    assert_eq!(
        String::from_utf8(git(&fixture.root, &["log", "-1", "--pretty=%s"]).stdout)
            .expect("commit message")
            .trim(),
        "Implement 002"
    );
}

#[test]
fn any_selection_continuation_does_not_return_before_explicit_start() {
    let fixture = Fixture::new(false, "continue", false);
    fs::write(
        fixture.root.join("specs/003.md"),
        "# Spec 3\n\nCreate another change.\n",
    )
    .expect("third spec");
    fs::write(
        fixture.root.join(".spec-loop/plans/003.md"),
        "# Existing plan 3\n",
    )
    .expect("third implementation plan");
    git(&fixture.root, &["add", "specs/003.md"]);
    git(&fixture.root, &["commit", "-qm", "add third spec"]);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("profile").replace(
        "active_globs = [\"specs/*.md\"]",
        "active_globs = [\"specs/*.md\"]\nselection = \"any\"",
    );
    fs::write(profile_path, profile).expect("enable any spec selection");
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("fake Pi script");
    let updated = script.replace(
        "printf 'implementation\\n' >> \"$EVENTS\"\n",
        "sed -n 's/^Selected spec ID: /implementation-/p' \"$PROMPT\" >> \"$EVENTS\"\n",
    );
    assert_ne!(updated, script, "fake Pi writer event hook was not updated");
    fs::write(agent, updated).expect("record selected implementation IDs");

    let output = fixture.run(&["--spec", "002", "--commit", "--continue"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation-002",
            "review",
            "implementation-003",
            "review",
        ]
    );
}

#[test]
fn implementation_spec_hash_change_stops_and_preserves_change() {
    let fixture = Fixture::new(false, "normal", false);
    configure_implementation_input_mutation(&fixture, "specs/001.md");

    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected spec content changed")
    );
    assert!(
        fs::read_to_string(fixture.root.join("specs/001.md"))
            .expect("preserved spec")
            .contains("tampered")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation"]
    );
    assert_eq!(
        git(&fixture.root, &["diff", "--cached", "--name-only"]).stdout,
        b""
    );
}

#[test]
fn implementation_plan_hash_change_stops_and_preserves_change() {
    let fixture = Fixture::new(false, "normal", false);
    configure_implementation_input_mutation(&fixture, ".spec-loop/plans/001.md");

    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected plan content changed")
    );
    assert!(
        fs::read_to_string(fixture.root.join(".spec-loop/plans/001.md"))
            .expect("preserved plan")
            .contains("tampered")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation"]
    );
    assert_eq!(
        git(&fixture.root, &["diff", "--cached", "--name-only"]).stdout,
        b""
    );
}

#[test]
fn configured_check_plan_hash_change_stops_before_review() {
    let fixture = Fixture::new(false, "normal", false);
    add_plan_mutating_check(&fixture);

    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected plan content changed")
    );
    assert!(
        fs::read_to_string(fixture.root.join(".spec-loop/plans/001.md"))
            .expect("preserved plan")
            .contains("check mutation")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "check"]
    );
}

#[test]
fn omitting_commit_switch_stages_but_does_not_commit() {
    let fixture = Fixture::new(false, "normal", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(output.status.success());
    assert!(fixture.events.exists());
    assert_eq!(
        git(&fixture.root, &["rev-list", "--count", "HEAD"]).stdout,
        b"1\n"
    );
    assert!(
        String::from_utf8_lossy(&git(&fixture.root, &["status", "--porcelain"]).stdout)
            .contains("A  generated.txt")
    );
}

#[test]
fn malformed_finding_audit_retries_then_stops_at_profile_limit() {
    let fixture = Fixture::new(false, "audit-incomplete", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("completion.max_passes limit"));
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review", "review-audit", "review-audit"]
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("status")
            .contains("?? generated.txt")
    );
}

#[test]
fn writer_staged_changes_are_preserved_and_reviewed_with_complete_patch() {
    let fixture = Fixture::new(false, "writer-stage", false);
    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .contains("review")
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("status")
            .contains("A  generated.txt")
    );
}

#[test]
fn index_only_change_is_present_for_review_before_standard_staging() {
    let fixture = Fixture::new(false, "index-only-staged", false);
    fs::write(fixture.root.join("staged-only.txt"), "staged content\n")
        .expect("write staged-only content");
    git(&fixture.root, &["add", "staged-only.txt"]);
    fs::remove_file(fixture.root.join("staged-only.txt")).expect("revert worktree content");

    let diff_output = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .args(args)
            .output()
            .expect("inspect staged and worktree diffs")
    };
    assert_eq!(
        diff_output(&[
            "diff",
            "--cached",
            "--quiet",
            "HEAD",
            "--",
            "staged-only.txt"
        ])
        .status
        .code(),
        Some(1),
        "fixture must contain staged-only content"
    );
    assert_eq!(
        diff_output(&["diff", "--quiet", "--", "staged-only.txt"])
            .status
            .code(),
        Some(1),
        "fixture must contain a worktree reversal"
    );
    assert!(
        diff_output(&["diff", "--quiet", "HEAD", "--", "staged-only.txt"])
            .status
            .success(),
        "net worktree diff should hide the cancelling index/worktree changes"
    );

    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        ["implementation", "review"]
    );
    let status =
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout).expect("status");
    assert!(
        status.contains("A  generated.txt"),
        "patch not staged: {status:?}"
    );
    assert!(
        !status.contains("staged-only.txt"),
        "reviewed cancelling change should follow standard git add behavior: {status:?}"
    );
}

#[test]
fn auditor_git_visible_mutation_is_checked_and_reviewed_before_completion() {
    let fixture = Fixture::new(false, "audit", false);
    add_logged_check(&fixture);
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let mutated = script.replace(
        "printf 'review-audit\\n' >> \"$EVENTS\"\n",
        "printf 'review-audit\\n' >> \"$EVENTS\"\nprintf 'audit mutation\\n' >> generated.txt\n",
    );
    assert_ne!(mutated, script, "fake Pi audit hook was not updated");
    fs::write(agent, mutated).expect("update fake Pi script");
    let output = fixture.run(&["--spec", "001", "--commit"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation",
            "check",
            "review",
            "review-audit",
            "check",
            "review"
        ]
    );
    assert!(
        fs::read_to_string(fixture.root.join("generated.txt"))
            .expect("output")
            .contains("audit mutation")
    );
    assert!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .contains("review-audit")
    );
}

#[test]
fn no_actionable_final_grounding_mutation_does_not_trigger_recheck() {
    let fixture = Fixture::new(false, "audit-unsupported-spec", false);
    add_logged_check(&fixture);
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("fake Pi script");
    let mutated = script
        .replace(
            "printf 'review-audit\\n' >> \"$EVENTS\"\n",
            "printf 'review-audit\\n' >> \"$EVENTS\"\nif [ \"$MODE\" = 'audit-unsupported-spec' ] && [ \"$(cat \"$REVIEWS\")\" -ge 2 ] && ! grep -q 'audit mutation' generated.txt; then printf 'audit mutation\\n' >> generated.txt; fi\n",
        )
        .replace(
            "  case \"$MODE:$count\" in\n    audit:1|audit-incomplete:1)",
            "  case \"$MODE:$count\" in\n    audit-unsupported-spec:2) TEXT='FINDING-1: The repository forbids creating generated.txt\\nEVIDENCE-FINDING-1: specs/001.md requires generated.txt\\nSPEC_LOOP_REVIEW=ISSUES' ;;\n    audit:1|audit-incomplete:1)",
        );
    assert_ne!(
        mutated, script,
        "fake Pi audit/reviewer hooks were not updated"
    );
    fs::write(agent, mutated).expect("update fake Pi script");

    let output = fixture.run(&["--spec", "001", "--commit"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "implementation",
            "check",
            "review",
            "review-audit",
            "review",
            "review-audit",
        ]
    );
    assert_eq!(
        git(&fixture.root, &["show", "HEAD:generated.txt"]).stdout,
        b"implemented\naudit mutation\n"
    );
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
}

#[test]
fn reviewer_ignored_file_mutation_does_not_block_progress() {
    let fixture = Fixture::new(false, "normal", false);
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let changed = script.replace(
        "printf 'review\\n' >> \"$EVENTS\"\n",
        "printf 'review\\n' >> \"$EVENTS\"\nprintf 'ignored reviewer mutation\\n' > ignored-review.txt\n",
    );
    assert_ne!(changed, script, "fake Pi reviewer hook was not updated");
    fs::write(agent, changed).expect("update fake Pi script");
    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("ignored-review.txt")).expect("ignored file"),
        "ignored reviewer mutation\n"
    );
}

#[test]
fn auditor_ignored_file_mutation_does_not_block_progress() {
    let fixture = Fixture::new(false, "audit", false);
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let changed = script.replace(
        "printf 'review-audit\\n' >> \"$EVENTS\"\n",
        "printf 'review-audit\\n' >> \"$EVENTS\"\nprintf 'ignored auditor mutation\\n' > ignored-review.txt\n",
    );
    assert_ne!(changed, script, "fake Pi auditor hook was not updated");
    fs::write(agent, changed).expect("update fake Pi script");
    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("ignored-review.txt")).expect("ignored file"),
        "ignored auditor mutation\n"
    );
}

#[test]
fn missing_non_selected_spec_does_not_block_and_deletion_is_preserved() {
    let fixture = Fixture::new(false, "normal", false);
    fs::write(fixture.root.join("specs/002.md"), "# Spec 2\n").expect("second spec");
    git(&fixture.root, &["add", "specs/002.md"]);
    git(&fixture.root, &["commit", "-qm", "add second spec"]);
    fs::remove_file(fixture.root.join("specs/002.md")).expect("delete non-selected spec");

    let output = fixture.run(&["--spec", "001"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let status = String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
        .expect("Git status");
    assert!(
        status.contains("D  specs/002.md"),
        "deletion not preserved: {status:?}"
    );
}

#[test]
fn missing_requested_spec_reports_selected_path_error() {
    let fixture = Fixture::new(false, "normal", false);
    fs::remove_file(fixture.root.join("specs/001.md")).expect("delete selected spec");

    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("selected spec path is missing: specs/001.md")
    );
    assert!(!fixture.events.exists());
    assert!(!fixture.root.join("specs/001.md").exists());
}

#[test]
fn startup_preserves_and_includes_staged_unstaged_and_untracked_changes() {
    let fixture = Fixture::new(false, "normal", false);
    fs::write(
        fixture.root.join("specs/001.md"),
        "# Spec\n\nUser spec edit.\n",
    )
    .expect("modify spec");
    fs::write(fixture.root.join("prepared.txt"), "staged user change\n").expect("staged file");
    git(&fixture.root, &["add", "prepared.txt"]);
    fs::write(
        fixture.root.join("untracked.txt"),
        "untracked user change\n",
    )
    .expect("untracked file");
    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let status =
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout).expect("status");
    assert!(
        status.contains("M  specs/001.md"),
        "spec edit not staged: {status:?}"
    );
    assert!(
        status.contains("A  prepared.txt"),
        "staged input lost: {status:?}"
    );
    assert!(
        status.contains("A  untracked.txt"),
        "untracked input not staged: {status:?}"
    );
    assert!(
        status.contains("A  generated.txt"),
        "implementation patch missing: {status:?}"
    );
}

#[test]
fn startup_accepts_staged_rename_records_without_misreading_old_path() {
    let fixture = Fixture::new(false, "normal", false);
    fs::write(fixture.root.join("extra.txt"), "rename input\n").expect("create rename input");
    git(&fixture.root, &["add", "extra.txt"]);
    git(&fixture.root, &["commit", "-qm", "add rename input"]);
    git(&fixture.root, &["mv", "extra.txt", "renamed.txt"]);
    let output = fixture.run(&["--spec", "001"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout)
            .expect("status")
            .contains("A  generated.txt")
    );
}

#[test]
fn duplicate_spec_ids_are_rejected_before_agent_invocation() {
    let fixture = Fixture::new(false, "normal", false);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("profile");
    let profile = profile.replace("(?P<id>[0-9]+)", "(?:copy-)?(?P<id>[0-9]+)");
    fs::write(profile_path, profile).expect("update profile");
    fs::write(fixture.root.join("specs/copy-001.md"), "duplicate ID\n")
        .expect("create duplicate spec");
    git(
        &fixture.root,
        &["add", ".spec-loop.toml", "specs/copy-001.md"],
    );
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate spec ID"));
    assert!(!fixture.events.exists());
}

#[test]
fn next_selection_rejects_a_later_requested_spec() {
    let fixture = Fixture::new(false, "continue", false);
    let output = fixture.run(&["--spec", "002"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("is not next; expected 001"));
    assert!(!fixture.events.exists());
}

#[test]
fn plan_mode_preserves_preexisting_git_changes_as_baseline() {
    let fixture = Fixture::with_plan();
    fs::write(
        fixture.root.join("preexisting-user-change.txt"),
        "preserve me\n",
    )
    .expect("preexisting user change");

    let output = fixture.run(&["--spec", "001", "--plan"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("preexisting-user-change.txt"))
            .expect("preserved user change"),
        "preserve me\n"
    );
    assert_eq!(
        git(&fixture.root, &["status", "--porcelain"]).stdout,
        b"?? preexisting-user-change.txt\n"
    );
}

#[test]
fn plan_author_unrelated_git_change_runs_checks_and_review_without_staging() {
    let fixture = Fixture::with_plan();
    add_logged_check(&fixture);
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let updated = script.replace(
        "  printf 'plan-author\\n' >> \"$EVENTS\"\n",
        "  printf 'plan-author\\n' >> \"$EVENTS\"\n  printf 'unexpected source edit\\n' > generated.txt\n",
    );
    assert_ne!(updated, script, "plan mutation hook was not installed");
    fs::write(agent, updated).expect("configure plan mutation");
    let initial_head = git(&fixture.root, &["rev-parse", "HEAD"]).stdout;

    let output = fixture.run(&["--spec", "001", "--plan"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("generated.txt"))
            .expect("preserved unexpected change"),
        "unexpected source edit\n"
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-review", "check", "review"]
    );
    assert_eq!(
        git(&fixture.root, &["diff", "--cached", "--name-only"]).stdout,
        b""
    );
    assert_eq!(
        git(&fixture.root, &["rev-parse", "HEAD"]).stdout,
        initial_head
    );
    assert_eq!(
        git(&fixture.root, &["status", "--porcelain"]).stdout,
        b" M .spec-loop.toml\n?? generated.txt\n"
    );
}

#[test]
fn plan_reviewer_unrelated_git_change_is_validated_without_staging() {
    let fixture = Fixture::with_plan_mode("plan-review-unrelated");
    add_logged_check(&fixture);
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let updated = script.replace(
        "  printf 'plan-review\\n' >> \"$EVENTS\"\n",
        "  printf 'plan-review\\n' >> \"$EVENTS\"\n  if [ \"$MODE\" = 'plan-review-unrelated' ]; then printf 'reviewer source edit\\n' > generated.txt; fi\n",
    );
    assert_ne!(
        updated, script,
        "plan reviewer mutation hook was not installed"
    );
    fs::write(agent, updated).expect("configure plan reviewer mutation");

    let output = fixture.run(&["--spec", "001", "--plan"]);

    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("generated.txt")).expect("preserved reviewer change"),
        "reviewer source edit\n"
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-review", "check", "review"]
    );
    assert_eq!(
        git(&fixture.root, &["diff", "--cached", "--name-only"]).stdout,
        b""
    );
}

#[test]
fn plan_mode_failed_check_preserves_unrelated_change_without_review_or_staging() {
    let fixture = Fixture::with_plan();
    let check = fixture.temporary.path().join("tools/failing-plan-check");
    write_executable(
        &check,
        &format!(
            "#!/bin/sh\nset -eu\nprintf 'failing-check\\n' >> '{}'\nprintf 'check failed\\n' >&2\nexit 1\n",
            fixture.events.display()
        ),
    );
    let profile_path = fixture.root.join(".spec-loop.toml");
    let mut profile = fs::read_to_string(&profile_path).expect("profile");
    writeln!(
        profile,
        "\n[[checks]]\nname = \"failing\"\nargv = [{:?}]\ncwd = \".\"",
        check.to_string_lossy()
    )
    .expect("append failing check");
    fs::write(profile_path, profile).expect("configure failing check");

    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let updated = script.replace(
        "  printf 'plan-author\\n' >> \"$EVENTS\"\n",
        "  printf 'plan-author\\n' >> \"$EVENTS\"\n  printf 'unexpected source edit\\n' > generated.txt\n",
    );
    assert_ne!(updated, script, "plan mutation hook was not installed");
    fs::write(agent, updated).expect("configure plan mutation");

    let output = fixture.run(&["--spec", "001", "--plan"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("configured checks failed"));
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("workflow events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-review", "failing-check"]
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("generated.txt")).expect("preserved generated change"),
        "unexpected source edit\n"
    );
    assert_eq!(
        git(&fixture.root, &["diff", "--cached", "--name-only"]).stdout,
        b""
    );
}

#[test]
fn plan_mode_authors_and_reviews_without_staging_or_implementation() {
    let fixture = Fixture::with_plan();
    let output = fixture.run(&["--spec", "001", "--plan"]);
    assert!(
        output.status.success(),
        "spec-loop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_role_response(&output, "plan-author");
    assert_role_response(&output, "plan-review");
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-review"]
    );
    assert!(fixture.root.join(".spec-loop/plans/001.md").exists());
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
    assert!(!fixture.root.join("generated.txt").exists());
}

#[test]
fn implementation_requires_existing_plan_before_agent_invocation() {
    let fixture = Fixture::new(false, "normal", false);
    fs::remove_file(fixture.root.join(".spec-loop/plans/001.md")).expect("remove plan");
    let output = fixture.run(&["--spec", "001"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("required plan is missing"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--plan"));
    assert!(!fixture.events.exists());
}

#[test]
fn plan_author_retries_ready_response_when_plan_file_is_missing() {
    let fixture = Fixture::with_plan();
    fs::remove_file(fixture.root.join(".spec-loop/plans/001.md")).expect("remove initial plan");
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let updated = script.replace(
        r#"  printf '# Plan\n' > .spec-loop/plans/001.md
  printf 'plan-author\n' >> "$EVENTS""#,
        r#"  plan_count=$(cat "$CALLS" 2>/dev/null || printf '0')
  plan_count=$((plan_count + 1))
  printf '%s' "$plan_count" > "$CALLS"
  if [ "$plan_count" -gt 1 ]; then
    grep -q 'required plan file was missing' "$PROMPT"
    printf '# Plan\n' > .spec-loop/plans/001.md
  fi
  printf 'plan-author\n' >> "$EVENTS""#,
    );
    assert_ne!(updated, script, "fake Pi plan writer was not updated");
    fs::write(agent, updated).expect("configure incomplete plan attempt");

    let output = fixture.run(&["--spec", "001", "--plan"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_role_response(&output, "plan-author");
    assert_role_response(&output, "plan-review");
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-author", "plan-review"]
    );
    assert_eq!(
        fs::read_to_string(&fixture.writer_calls).expect("plan author call count"),
        "2"
    );
    assert!(fixture.root.join(".spec-loop/plans/001.md").is_file());
}

#[test]
fn plan_continue_skips_existing_plan_and_creates_next_missing_plan() {
    let fixture = Fixture::with_plan();
    fs::write(fixture.root.join("specs/002.md"), "# Spec 2\n").expect("second spec");
    git(&fixture.root, &["add", "specs/002.md"]);
    git(&fixture.root, &["commit", "-qm", "add second spec"]);
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let updated = script.replace(
        "printf '# Plan\\n' > .spec-loop/plans/001.md\n  printf 'plan-author\\n'",
        "if grep -q 'Plan path: .spec-loop/plans/002.md' \"$PROMPT\"; then printf '# Plan\\n' > .spec-loop/plans/002.md; else printf '# Plan\\n' > .spec-loop/plans/001.md; fi\n  printf 'plan-author\\n'",
    );
    assert_ne!(updated, script, "fake Pi plan writer was not updated");
    fs::write(agent, updated).expect("update fake Pi script");
    let output = fixture.run(&["--spec", "001", "--plan", "--continue"]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root.join(".spec-loop/plans/002.md").is_file());
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-review"]
    );
}

#[test]
fn plan_continue_from_later_explicit_any_selection_never_goes_backward() {
    let fixture = Fixture::with_plan();
    fs::remove_file(fixture.root.join(".spec-loop/plans/001.md")).expect("remove earlier plan");
    for id in ["002", "003"] {
        fs::write(
            fixture.root.join(format!("specs/{id}.md")),
            "# Later spec\n",
        )
        .expect("write later spec");
    }
    git(&fixture.root, &["add", "specs/002.md", "specs/003.md"]);
    git(&fixture.root, &["commit", "-qm", "add later specs"]);

    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("profile").replace(
        "active_globs = [\"specs/*.md\"]",
        "active_globs = [\"specs/*.md\"]\nselection = \"any\"",
    );
    fs::write(&profile_path, profile).expect("enable any selection");

    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let updated = script.replace(
        "printf '# Plan\\n' > .spec-loop/plans/001.md\n  printf 'plan-author\\n'",
        "if grep -q 'Plan path: .spec-loop/plans/002.md' \"$PROMPT\"; then printf '# Plan\\n' > .spec-loop/plans/002.md; elif grep -q 'Plan path: .spec-loop/plans/003.md' \"$PROMPT\"; then printf '# Plan\\n' > .spec-loop/plans/003.md; else printf '# Plan\\n' > .spec-loop/plans/001.md; fi\n  printf 'plan-author\\n'",
    );
    assert_ne!(updated, script, "fake Pi plan writer was not updated");
    fs::write(agent, updated).expect("update fake Pi script");

    let output = fixture.run(&["--spec", "002", "--plan", "--continue"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root.join(".spec-loop/plans/002.md").is_file());
    assert!(fixture.root.join(".spec-loop/plans/003.md").is_file());
    assert!(!fixture.root.join(".spec-loop/plans/001.md").exists());
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-review", "plan-author", "plan-review"]
    );
}

#[test]
fn unsupported_thinking_level_is_rejected_before_agent_invocation() {
    let fixture = Fixture::new(false, "normal", false);
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path)
        .expect("profile")
        .replace("[agent]\n", "[agent]\nthinking = \"bogus\"\n");
    fs::write(profile_path, profile).expect("configure unsupported thinking level");

    let output = fixture.run(&["--spec", "001"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported agent.thinking value"));
    assert!(!fixture.events.exists());
    assert!(!fixture.root.join("generated.txt").exists());
    assert_eq!(
        git(&fixture.root, &["diff", "--cached", "--name-only"]).stdout,
        b""
    );
}

#[test]
fn plan_reviewer_plan_mutation_stops_and_preserves_change() {
    let fixture = Fixture::with_plan_mode("plan-review-ignored");
    let output = fixture.run(&["--spec", "001", "--plan"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected plan content changed")
    );
    assert!(
        fs::read_to_string(fixture.root.join(".spec-loop/plans/001.md"))
            .expect("plan")
            .contains("review mutation")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-review"]
    );
}

#[test]
fn plan_status_auditor_mutation_stops_and_preserves_change() {
    let fixture = Fixture::with_plan_mode("plan-status-ignored");
    let output = fixture.run(&["--spec", "001", "--plan"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected plan content changed")
    );
    assert_role_response(&output, "plan-author");
    assert_role_response(&output, "plan-review");
    assert_role_response(&output, "plan-review-status-audit");
    assert!(
        fs::read_to_string(fixture.root.join(".spec-loop/plans/001.md"))
            .expect("plan")
            .contains("status audit mutation")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-review", "plan-status-audit"]
    );
}

#[test]
fn plan_finding_auditor_plan_mutation_stops_and_preserves_change() {
    let fixture = Fixture::with_plan_mode("plan-audit-ignored");
    let output = fixture.run(&["--spec", "001", "--plan"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected plan content changed")
    );
    assert_role_response(&output, "plan-author");
    assert_role_response(&output, "plan-review");
    assert_role_response(&output, "plan-audit");
    assert!(
        fs::read_to_string(fixture.root.join(".spec-loop/plans/001.md"))
            .expect("plan")
            .contains("audit mutation")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author", "plan-review", "plan-audit"]
    );
}

#[test]
fn plan_finding_auditor_response_is_role_labeled() {
    let fixture = Fixture::with_plan_mode("plan-audit-ignored");
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let without_audit_mutation = script.replace(
        "if [ \"$MODE\" = 'plan-audit-ignored' ]; then printf 'audit mutation\\n' >> .spec-loop/plans/001.md; fi",
        "if [ \"$MODE\" = 'plan-audit-ignored' ]; then :; fi",
    );
    let changed = without_audit_mutation.replace(
        "if [ \"$MODE\" = 'plan-audit-ignored' ]; then TEXT='FINDING-1: plan finding\\nSPEC_LOOP_PLAN_REVIEW=ISSUES'; elif [ \"$MODE\" = 'plan-status-ignored' ]; then TEXT='The plan review reports no findings.'; else TEXT='SPEC_LOOP_PLAN_REVIEW=NO_ISSUES'; fi",
        "count=$(cat \"$REVIEWS\" 2>/dev/null || printf '0'); count=$((count + 1)); printf '%s' \"$count\" > \"$REVIEWS\"; if [ \"$MODE\" = 'plan-audit-ignored' ] && [ \"$count\" -eq 1 ]; then TEXT='FINDING-1: plan finding\\nSPEC_LOOP_PLAN_REVIEW=ISSUES'; else TEXT='SPEC_LOOP_PLAN_REVIEW=NO_ISSUES'; fi",
    );
    assert_ne!(
        changed, script,
        "plan reviewer one-shot behavior was not inserted"
    );
    fs::write(agent, changed).expect("update fake Pi plan reviewer");
    let output = fixture.run(&["--spec", "001", "--plan"]);

    assert!(
        output.status.success(),
        "plan workflow failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_role_response(&output, "plan-author");
    assert_role_response(&output, "plan-review");
    assert_role_response(&output, "plan-audit");
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("plan workflow events")
            .lines()
            .collect::<Vec<_>>(),
        [
            "plan-author",
            "plan-review",
            "plan-audit",
            "plan-author",
            "plan-review"
        ]
    );
}

#[test]
fn plan_writer_git_metadata_change_is_preserved_without_blocking_plan() {
    let fixture = Fixture::with_plan_mode("plan-git-metadata");
    let output = fixture.run(&["--spec", "001", "--plan"]);
    assert!(output.status.success());
    let config = fs::read_to_string(fixture.root.join(".git/config")).expect("Git config");
    assert!(config.contains("mutated by plan writer"));
    assert!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .contains("plan-review")
    );
}

#[test]
fn plan_writer_permission_change_is_preserved_without_blocking_plan() {
    let fixture = Fixture::with_plan_mode("plan-permission");
    let output = fixture.run(&["--spec", "001", "--plan"]);
    assert!(output.status.success());
    assert_eq!(
        fs::metadata(fixture.root.join("specs/001.md"))
            .expect("spec metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .contains("plan-review")
    );
}

#[test]
fn plan_author_spec_content_change_stops_and_preserves_change() {
    let fixture = Fixture::with_plan_mode("plan-spec-mutation");
    let agent = fixture.temporary.path().join("tools/fake-pi");
    let script = fs::read_to_string(&agent).expect("read fake Pi script");
    let updated = script.replace(
        "if [ \"$MODE\" = 'plan-permission' ]; then chmod 600 specs/001.md; fi",
        "if [ \"$MODE\" = 'plan-permission' ]; then chmod 600 specs/001.md; fi\n  if [ \"$MODE\" = 'plan-spec-mutation' ]; then printf 'tampered\\n' >> specs/001.md; fi",
    );
    assert_ne!(updated, script, "spec mutation hook was not installed");
    fs::write(agent, updated).expect("update fake Pi spec mutation hook");

    let output = fixture.run(&["--spec", "001", "--plan"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected selected spec content changed")
    );
    assert!(
        fs::read_to_string(fixture.root.join("specs/001.md"))
            .expect("preserved spec")
            .contains("tampered")
    );
    assert_eq!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .lines()
            .collect::<Vec<_>>(),
        ["plan-author"]
    );
}

#[test]
fn engine_stages_complete_non_ignored_patch_after_review() {
    let fixture = Fixture::new(false, "normal", false);
    fs::write(
        fixture.root.join("existing-user-change.txt"),
        "user work\\n",
    )
    .expect("write user change");
    let output = fixture.run(&["--spec", "001"]);
    assert!(output.status.success());
    let status =
        String::from_utf8(git(&fixture.root, &["status", "--porcelain"]).stdout).expect("status");
    assert!(
        status.contains("A  existing-user-change.txt"),
        "user patch not staged: {status:?}"
    );
    assert!(
        status.contains("A  generated.txt"),
        "implementation patch not staged: {status:?}"
    );
}

#[test]
fn plan_writer_ignored_artifacts_do_not_block_plan_completion() {
    let fixture = Fixture::with_plan_mode("plan-ignored");
    let output = fixture.run(&["--spec", "001", "--plan"]);
    assert!(output.status.success());
    assert!(fixture.root.join("ignored-plan-output.txt").exists());
    assert!(
        fs::read_to_string(&fixture.events)
            .expect("events")
            .contains("plan-review")
    );
}

#[test]
fn dry_run_does_not_invoke_agent_or_modify_repository() {
    let fixture = Fixture::new(false, "normal", false);
    fs::create_dir_all(fixture.root.join("instructions")).expect("instruction directory");
    fs::create_dir_all(fixture.root.join("skills/demo")).expect("skill directory");
    fs::write(
        fixture.root.join("instructions/implementation.md"),
        "Implementation instruction.\n",
    )
    .expect("write implementation instruction");
    fs::write(
        fixture.root.join("skills/demo/SKILL.md"),
        "Skill instruction.\n",
    )
    .expect("write skill");
    let profile_path = fixture.root.join(".spec-loop.toml");
    let profile = fs::read_to_string(&profile_path).expect("read profile");
    let profile = profile.replace(
        "[agent]\nexecutable = ",
        "[prompts]\nimplementation = [\"instructions/implementation.md\"]\n\n[agent]\nmodel = \"dry-run-model\"\ntimeout_seconds = 17\nexecutable = ",
    );
    assert_ne!(
        profile,
        fs::read_to_string(&profile_path).expect("read original profile")
    );
    fs::write(&profile_path, profile).expect("configure dry-run resources");
    git(
        &fixture.root,
        &[
            "add",
            ".spec-loop.toml",
            "instructions/implementation.md",
            "skills/demo/SKILL.md",
        ],
    );
    git(
        &fixture.root,
        &["commit", "-qm", "configure dry-run resources"],
    );
    let touch = Command::new("touch")
        .args(["-m", "-t", "200001010000.00"])
        .arg(fixture.root.join("specs/001.md"))
        .output()
        .expect("change tracked-file timestamp");
    assert!(
        touch.status.success(),
        "touch failed: {}",
        String::from_utf8_lossy(&touch.stderr)
    );
    let index_before = fs::read(fixture.root.join(".git/index")).expect("read Git index");

    let output = fixture.run(&["--spec", "001", "--dry-run"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No automated checks are configured."));
    assert!(stdout.contains("Agent:"));
    assert!(stdout.contains(&format!(
        "executable: {}",
        fixture.temporary.path().join("tools/fake-pi").display()
    )));
    assert!(stdout.contains("model: dry-run-model"));
    assert!(stdout.contains("timeout: 17s"));
    assert!(stdout.contains("Pi resources: inherited from user's Pi configuration for every role"));
    assert!(
        stdout.contains(
            &fixture
                .root
                .join("instructions/implementation.md")
                .display()
                .to_string()
        )
    );
    assert!(stdout.contains("Maximum calls per Pi role: 2 (profile; 0 means unlimited)"));
    assert!(stdout.contains("Prompt and instruction resources:"));
    assert!(!fixture.events.exists());
    assert_eq!(
        fs::read(fixture.root.join(".git/index")).expect("read Git index after dry-run"),
        index_before,
        "dry-run changed the exact Git index bytes"
    );
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]).stdout, b"");
}
