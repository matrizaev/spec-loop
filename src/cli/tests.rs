use clap::Parser;

use super::{Cli, WorkflowMode};
use crate::output::ColorMode;

#[test]
fn workflow_output_options_parse_and_are_rejected_for_auth() {
    let workflow = Cli::try_parse_from(["spec-loop", "--verbose", "--color", "always"])
        .expect("workflow options parse");
    assert!(workflow.verbose());
    assert_eq!(workflow.color_mode(), ColorMode::Always);

    let auth = Cli::try_parse_from(["spec-loop", "--color", "auto", "auth", "status"])
        .expect("auth with workflow option parses for explicit rejection");
    assert!(auth.validate_auth_options().is_err());
    let auth_verbose = Cli::try_parse_from(["spec-loop", "--verbose", "auth", "status"])
        .expect("auth with verbose parses for explicit rejection");
    assert!(auth_verbose.validate_auth_options().is_err());
    assert!(Cli::try_parse_from(["spec-loop", "--color", "rainbow"]).is_err());
}

#[test]
fn workflow_mode_is_a_validated_finite_choice() {
    let implementation = Cli::try_parse_from(["spec-loop"]).expect("default workflow mode");
    assert_eq!(
        implementation.workflow_mode().expect("implementation mode"),
        WorkflowMode::Implement
    );

    let review = Cli::try_parse_from(["spec-loop", "--review", "--spec", "001"])
        .expect("review workflow mode");
    assert_eq!(
        review.workflow_mode().expect("review mode"),
        WorkflowMode::Review
    );
    let plan =
        Cli::try_parse_from(["spec-loop", "--plan", "--spec", "001"]).expect("plan workflow mode");
    assert_eq!(plan.workflow_mode().expect("plan mode"), WorkflowMode::Plan);
    let conflict = Cli::try_parse_from(["spec-loop", "--review", "--plan", "--spec", "001"])
        .expect_err("review and plan are mutually exclusive");
    assert!(
        conflict.to_string().contains("cannot be used"),
        "{conflict}"
    );
    let mut invalid_mode = Cli::try_parse_from(["spec-loop"]).expect("default CLI");
    invalid_mode.mode.review = true;
    invalid_mode.mode.plan = true;
    assert_eq!(
        invalid_mode
            .workflow_mode()
            .expect_err("contradictory workflow choice is rejected")
            .to_string(),
        "configuration error: --review and --plan cannot be used together"
    );
    let auth_conflict = Cli::try_parse_from([
        "spec-loop",
        "auth",
        "login",
        "--account",
        "registration",
        "--new-account",
    ])
    .expect_err("login registration options are mutually exclusive");
    assert!(auth_conflict.to_string().contains("cannot be used"));
}
