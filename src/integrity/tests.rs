use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::InputIntegrity;

fn fixture(plan: bool, plan_author_may_update: bool) -> (TempDir, InputIntegrity) {
    let temporary = TempDir::new().expect("temporary directory");
    fs::create_dir_all(temporary.path().join("specs")).expect("spec directory");
    fs::write(temporary.path().join("specs/001.md"), "spec\n").expect("spec");
    if plan {
        fs::create_dir_all(temporary.path().join("plans")).expect("plan directory");
        fs::write(temporary.path().join("plans/001.md"), "plan\n").expect("plan");
    }
    let integrity = InputIntegrity::capture(
        temporary.path(),
        Path::new("specs/001.md"),
        Path::new("plans/001.md"),
        plan_author_may_update,
    )
    .expect("capture hashes");
    (temporary, integrity)
}

#[test]
fn plan_author_can_revise_plan_but_not_spec() {
    let (temporary, mut integrity) = fixture(true, true);
    fs::write(temporary.path().join("plans/001.md"), "revised plan\n").expect("revise plan");
    integrity
        .agent_finished(temporary.path(), true)
        .expect("allow PlanAuthor plan revision");
    integrity
        .verify(temporary.path())
        .expect("verify new baseline");

    fs::write(temporary.path().join("specs/001.md"), "changed spec\n").expect("edit spec");
    assert!(integrity.agent_finished(temporary.path(), true).is_err());
}

#[test]
fn frozen_plan_change_is_detected_and_preserved() {
    let (temporary, integrity) = fixture(true, false);
    fs::write(temporary.path().join("plans/001.md"), "changed plan\n").expect("edit plan");

    let result = integrity.verify(temporary.path());

    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(temporary.path().join("plans/001.md")).expect("preserved plan"),
        "changed plan\n"
    );
}

#[test]
fn plan_only_verification_ignores_spec_changes_but_protects_plan() {
    let (temporary, integrity) = fixture(true, false);
    fs::write(temporary.path().join("specs/001.md"), "finished spec\n").expect("edit spec");
    integrity
        .verify_plan(temporary.path())
        .expect("finish may update selected spec");

    fs::write(temporary.path().join("plans/001.md"), "changed plan\n").expect("edit plan");
    assert!(integrity.verify_plan(temporary.path()).is_err());
}

#[test]
fn missing_plan_can_be_created_by_plan_author() {
    let (temporary, mut integrity) = fixture(false, true);
    integrity
        .verify(temporary.path())
        .expect("initial missing plan");
    fs::create_dir_all(temporary.path().join("plans")).expect("create plans directory");
    fs::write(temporary.path().join("plans/001.md"), "new plan\n").expect("create plan");

    integrity
        .agent_finished(temporary.path(), true)
        .expect("allow PlanAuthor plan creation");
    integrity
        .verify(temporary.path())
        .expect("verify new plan hash");
}

#[test]
fn relocated_spec_must_keep_same_content() {
    let (temporary, mut integrity) = fixture(true, false);
    fs::create_dir_all(temporary.path().join("archive")).expect("archive directory");
    fs::rename(
        temporary.path().join("specs/001.md"),
        temporary.path().join("archive/001.md"),
    )
    .expect("move spec");

    integrity
        .relocate_spec(temporary.path(), Path::new("archive/001.md"))
        .expect("verify archived spec hash");
}
