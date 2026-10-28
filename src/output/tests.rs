use std::ffi::OsStr;

use super::{
    ColorMode, Output, TypedResult, UsageProgressState, preview, render_metadata, render_stage,
    render_usage_report, sanitize, style, style_status,
};
use crate::protocol::{
    Actionability, AuditResult, Citation, Classification, EvidenceResult, Finding,
    FindingClassification, FindingEvidence, ImplementationAuditResponse,
    ImplementationClarificationResponse, ImplementationResponse, ImplementationReviewResponse,
    PlanAuditResponse, PlanAuthorResponse, PlanClarificationResponse, PlanReviewResponse,
    Readiness, ReviewResult,
};

#[test]
fn adjudication_output_distinguishes_correction_from_stops_and_sanitizes_evidence() {
    let fixtures: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../tests/fixtures/blocker-adjudications.json"
    ))
    .expect("adjudication fixtures");
    for mut adjudication in fixtures {
        adjudication["evidence"] = serde_json::json!("source \u{1b}[31m evidence");
        let response = crate::protocol::parse::<PlanAuditResponse>(&serde_json::json!({
            "protocol_version":1,"role":"plan_audit","status":"adjudicated","adjudication":adjudication,
        }).to_string(), "plan_audit").expect("typed adjudication");
        let concise = response.render(false);
        let verbose = response.render(true);
        assert!(!concise.contains("[PASS]"));
        assert!(!verbose.contains('\u{1b}'));
        assert!(verbose.contains("Evidence:"));
        match adjudication["outcome"].as_str().expect("outcome") {
            "unsupported" => {
                assert!(concise.contains("[RETRY]") && concise.contains("this is not approval"));
            }
            "confirmed" => {
                assert!(concise.contains("[BLOCKED]") && concise.contains("Required input:"));
            }
            "ambiguous" => {
                assert!(concise.contains("[BLOCKED]") && concise.contains("Unresolved question:"));
            }
            _ => panic!("unexpected fixture outcome"),
        }
    }
}

#[test]
fn color_policy_detects_streams_independently_and_honors_overrides() {
    let auto = Output::with_environment(
        ColorMode::Auto,
        false,
        true,
        false,
        Some(OsStr::new("xterm")),
        None,
    );
    assert!(auto.stdout.color);
    assert!(!auto.stderr_color);
    assert!(auto.stdout.rewrite_progress);
    let reverse = Output::with_environment(
        ColorMode::Auto,
        false,
        false,
        true,
        Some(OsStr::new("xterm")),
        None,
    );
    assert!(!reverse.stdout.color);
    assert!(reverse.stderr_color);
    let blocked = Output::with_environment(
        ColorMode::Auto,
        false,
        true,
        true,
        Some(OsStr::new("dumb")),
        None,
    );
    assert!(!blocked.stdout.color && !blocked.stderr_color);
    assert!(!blocked.stdout.rewrite_progress);
    let no_color = Output::with_environment(
        ColorMode::Auto,
        false,
        true,
        true,
        None,
        Some(OsStr::new("1")),
    );
    assert!(!no_color.stdout.color && !no_color.stderr_color);
    let empty_no_color = Output::with_environment(
        ColorMode::Auto,
        false,
        true,
        true,
        None,
        Some(OsStr::new("")),
    );
    assert!(empty_no_color.stdout.color && empty_no_color.stderr_color);
    let forced = Output::with_environment(
        ColorMode::Always,
        false,
        false,
        false,
        Some(OsStr::new("dumb")),
        Some(OsStr::new("1")),
    );
    assert!(forced.stdout.color && forced.stderr_color);
    assert!(style_status("[PASS] ready", forced.stdout.color).starts_with("\x1b[32m"));
    for (label, color) in [
        ("[BLOCKED]", "\x1b[33m"),
        ("[RETRY]", "\x1b[33m"),
        ("[WARN]", "\x1b[33m"),
        ("[FAIL]", "\x1b[31m"),
        ("[ERROR]", "\x1b[31m"),
    ] {
        assert!(style_status(label, forced.stdout.color).starts_with(color));
    }
    assert!(style_status("  metadata", forced.stdout.color).starts_with("\x1b[2m"));
    assert!(style("[RUNNING] stage", "\x1b[1;36m", forced.stdout.color).contains("\x1b[1;36m"));
    let never = Output::with_environment(ColorMode::Never, false, true, true, None, None);
    assert!(!never.stdout.color && !never.stderr_color);
    assert!(never.stdout.rewrite_progress);
    assert_eq!(
        style_status("[PASS] ready", never.stdout.color),
        "[PASS] ready"
    );
}

#[test]
fn usage_reports_replace_the_same_terminal_block_and_append_when_redirected() {
    let (initial, mut state) =
        render_usage_report("first line\nsecond line", UsageProgressState::Initial, true);
    assert_eq!(initial, "first line\nsecond line\n");

    let (updated, next_state) = render_usage_report("short update", state, true);
    assert_eq!(updated, "\x1b[2A\r\x1b[2Kshort update\n\r\x1b[2K\n");
    state = next_state;

    let (expanded, state) = render_usage_report("one\ntwo\nthree", state, true);
    assert_eq!(
        expanded,
        "\x1b[2A\r\x1b[2Kone\n\r\x1b[2Ktwo\n\r\x1b[2Kthree\n"
    );
    assert!(matches!(state, UsageProgressState::Replace { lines: 3 }));

    let (first, state) = render_usage_report(
        "role header\nstatus one\nusage one",
        UsageProgressState::Initial,
        false,
    );
    let (second, _) = render_usage_report("role header\nstatus two\nusage two", state, false);
    assert_eq!(first, "role header\nstatus one\nusage one\n");
    assert_eq!(second, "status two\nusage two\n");
}

#[test]
fn stage_headings_and_secondary_metadata_have_distinct_styles() {
    let stage = render_stage(
        "Stage: check tests\nCommand argv: [\"cargo\"]\nWorking directory: /tmp/project",
        true,
    );
    assert!(stage.contains("\x1b[1;36m[RUNNING] Stage: check tests\x1b[0m"));
    assert!(stage.contains("\x1b[2mCommand argv: [\"cargo\"]\n\x1b[0m"));
    assert!(stage.contains("\x1b[2mWorking directory: /tmp/project\x1b[0m\n"));
    assert!(!render_metadata("Command argv: [\"cargo\"]", true).contains("\x1b[1;36m"));
    assert_eq!(
        render_metadata("Command argv: [\"cargo\"]", false),
        "  Command argv: [\"cargo\"]\n"
    );
}

#[test]
fn sanitizes_terminal_sequences_and_escapes_controls() {
    assert_eq!(
        sanitize("a\x1b[31mred\x1b[0m\x1b]0;title\x07b\n\t\r\u{7f}\u{0085}"),
        "aredb\n\t\\u{d}\\u{7f}\\u{85}"
    );
    assert_eq!(sanitize("x\x1b]unterminated"), "x");
    assert_eq!(sanitize("x\u{009b}31mred"), "xred");
    assert_eq!(sanitize("a\u{009d}0;title\u{7}b"), "ab");
    assert_eq!(sanitize("a\u{009d}0;title\u{009c}b"), "ab");
    assert_eq!(sanitize("a\u{0090}payload\u{009c}b"), "ab");
    assert_eq!(sanitize("a\u{0098}payload\u{009c}b"), "ab");
    assert_eq!(sanitize("a\u{009e}payload\u{009c}b"), "ab");
    assert_eq!(sanitize("a\u{009f}payload\u{009c}b"), "ab");
    assert_eq!(
        sanitize("a\x1bPprivate\x1b\\b\x1bXignore\x1b\\c\x1b^ignore\x1b\\d\x1b_ignore\x1b\\e"),
        "abcde"
    );
    assert_eq!(sanitize("a\x1b[31m"), "a");
    assert_eq!(sanitize("a\x1bc"), "a");
    assert_eq!(sanitize("a\x1b]unterminated\nb"), "a\n");
    assert_eq!(sanitize("a\x1b]title\nb\x07c"), "a\nc");
    assert_eq!(sanitize("a\x1b\nb"), "a\nb");
}

#[test]
fn preview_counts_unicode_scalars_and_marks_truncation() {
    let exact = "🦀".repeat(160);
    assert_eq!(preview(&exact, 160), exact);
    let longer = "🦀".repeat(161);
    assert_eq!(
        preview(&longer, 160),
        format!("{}… [truncated; use --verbose]", "🦀".repeat(160))
    );
    assert_eq!(preview("x\rmore", 1), "x… [truncated; use --verbose]");
    assert_eq!(
        preview("\rmore", 1),
        "… [truncated; use --verbose]",
        "escaped control tokens are atomic at the preview boundary"
    );
}

#[test]
fn plan_author_and_implementation_results_render_readably() {
    let ready = PlanAuthorResponse {
        protocol_version: 1,
        role: "plan_author".to_owned(),
        result: Readiness::ReadyForReview,
    };
    assert!(ready.render(false).contains("[PASS] Plan ready for review"));
    assert!(ready.render(true).contains("Protocol version: 1"));
    let blocked = PlanAuthorResponse {
        result: Readiness::Blocked {
            reason: "evidence unavailable".to_owned(),
        },
        ..ready
    };
    assert!(
        blocked
            .render(false)
            .contains("[BLOCKED] Reason: evidence unavailable")
    );

    let implementation = ImplementationResponse {
        protocol_version: 1,
        role: "implementation".to_owned(),
        summary: "implemented typed output".to_owned(),
        result: Readiness::ReadyForReview,
    };
    assert!(
        implementation
            .render(false)
            .contains("Summary: implemented typed output")
    );
    assert!(implementation.render(true).contains("Protocol version: 1"));
    let blocked_implementation = ImplementationResponse {
        protocol_version: 1,
        role: "implementation".to_owned(),
        summary: "work remains".to_owned(),
        result: Readiness::Blocked {
            reason: "needs an approved prerequisite".to_owned(),
        },
    };
    let blocked_text = blocked_implementation.render(false);
    assert!(blocked_text.contains("[BLOCKED]"));
    assert!(blocked_text.contains("needs an approved prerequisite"));
}

#[test]
fn review_results_render_findings_empty_and_blocked() {
    let finding = Finding {
        id: 1,
        requirement: "render typed data".to_owned(),
        location: "src/example.rs:10".to_owned(),
        evidence: "source fixture".to_owned(),
        violation: "field is missing".to_owned(),
        proposed_correction: "show field".to_owned(),
    };
    let review = PlanReviewResponse {
        protocol_version: 1,
        role: "plan_review".to_owned(),
        result: ReviewResult::Issues {
            findings: vec![finding],
        },
    };
    let concise = review.render(false);
    for expected in [
        "Findings: 1",
        "Finding #1 — src/example.rs:10",
        "Requirement: render typed data",
        "Violation: field is missing",
    ] {
        assert!(concise.contains(expected));
    }
    let verbose = review.render(true);
    assert!(verbose.contains("Evidence: source fixture"));
    assert!(verbose.contains("Proposed correction: show field"));

    let no_issues = ImplementationReviewResponse {
        protocol_version: 1,
        role: "review".to_owned(),
        result: ReviewResult::NoIssues {
            findings: Vec::new(),
        },
    };
    assert!(no_issues.render(false).contains("No findings"));
    let blocked = ImplementationReviewResponse {
        protocol_version: 1,
        role: "review".to_owned(),
        result: ReviewResult::Blocked {
            reason: "cannot verify source".to_owned(),
        },
    };
    assert!(blocked.render(false).contains("[BLOCKED]"));
}

#[test]
fn audit_results_render_classifications_rationale_and_blocked() {
    let classifications = vec![
        FindingClassification {
            finding_id: 1,
            classification: Classification::Supported,
            actionability: Actionability::Required,
            rationale: "required correction".to_owned(),
        },
        FindingClassification {
            finding_id: 2,
            classification: Classification::Supported,
            actionability: Actionability::NotRequired,
            rationale: "no correction needed".to_owned(),
        },
        FindingClassification {
            finding_id: 3,
            classification: Classification::Unsupported,
            actionability: Actionability::NotRequired,
            rationale: "evidence disproves claim".to_owned(),
        },
        FindingClassification {
            finding_id: 4,
            classification: Classification::Ambiguous,
            actionability: Actionability::Ambiguous,
            rationale: "evidence is incomplete".to_owned(),
        },
    ];
    let audit = PlanAuditResponse {
        protocol_version: 1,
        role: "plan_audit".to_owned(),
        result: AuditResult::Complete { classifications },
    };
    let concise = audit.render(false);
    for expected in [
        "supported / required",
        "supported / not_required",
        "unsupported / not_required",
        "ambiguous / ambiguous",
    ] {
        assert!(concise.contains(expected));
    }
    assert!(
        audit
            .render(true)
            .contains("Rationale: evidence is incomplete")
    );
    let empty = PlanAuditResponse {
        protocol_version: 1,
        role: "plan_audit".to_owned(),
        result: AuditResult::Complete {
            classifications: Vec::new(),
        },
    };
    assert!(empty.render(false).contains("No findings"));
    let blocked = ImplementationAuditResponse {
        protocol_version: 1,
        role: "review_audit".to_owned(),
        result: AuditResult::Blocked {
            reason: "audit evidence unavailable".to_owned(),
        },
    };
    assert!(blocked.render(false).contains("[BLOCKED]"));
}

#[test]
fn clarification_results_render_citations_and_blocked() {
    let citation = Citation {
        path: "src/example.rs".to_owned(),
        location: "line 10".to_owned(),
        excerpt: "source excerpt".to_owned(),
        relevance: "supports the requirement".to_owned(),
    };
    let complete = PlanClarificationResponse {
        protocol_version: 1,
        role: "plan_review_clarification".to_owned(),
        result: EvidenceResult::Complete {
            evidence: vec![FindingEvidence {
                finding_id: 1,
                citations: vec![citation],
            }],
        },
    };
    let concise = complete.render(false);
    assert!(concise.contains("Finding #1 — 1 citation(s)"));
    assert!(concise.contains("src/example.rs — line 10"));
    let verbose = complete.render(true);
    assert!(verbose.contains("Excerpt: source excerpt"));
    assert!(verbose.contains("Relevance: supports the requirement"));

    let empty = PlanClarificationResponse {
        protocol_version: 1,
        role: "plan_review_clarification".to_owned(),
        result: EvidenceResult::Complete {
            evidence: Vec::new(),
        },
    };
    assert!(empty.render(false).contains("No citations"));
    let no_citations = ImplementationClarificationResponse {
        protocol_version: 1,
        role: "review_clarification".to_owned(),
        result: EvidenceResult::Complete {
            evidence: vec![FindingEvidence {
                finding_id: 2,
                citations: Vec::new(),
            }],
        },
    };
    assert!(no_citations.render(false).contains("No citations"));
    let blocked = ImplementationClarificationResponse {
        protocol_version: 1,
        role: "review_clarification".to_owned(),
        result: EvidenceResult::Blocked {
            reason: "citation unavailable".to_owned(),
        },
    };
    assert!(blocked.render(false).contains("[BLOCKED]"));
}

#[test]
fn concise_role_previews_truncate_and_verbose_does_not() {
    let response = ImplementationResponse {
        protocol_version: 1,
        role: "implementation".to_owned(),
        summary: "🦀".repeat(161),
        result: Readiness::ReadyForReview,
    };
    let concise = response.render(false);
    assert!(concise.contains(&format!("{}… [truncated; use --verbose]", "🦀".repeat(160))));
    assert!(response.render(true).contains(&"🦀".repeat(161)));
}
