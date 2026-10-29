use serde_json::json;

use super::{ResponseUsage, UsageSnapshot};

fn response(usage: &serde_json::Value) -> serde_json::Value {
    json!({"usage": usage})
}

fn valid_usage(input: u64, output: u64, total: u64) -> serde_json::Value {
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": total,
        "input_tokens_details": {"cached_tokens": 12},
        "output_tokens_details": {"reasoning_tokens": 20}
    })
}

#[test]
fn shared_optional_addition_preserves_zero_unknown_and_overflow() {
    for (left, right, expected) in [
        (Some(0), Some(0), Some(0)),
        (Some(1), Some(2), Some(3)),
        (Some(u64::MAX), Some(1), None),
        (None, Some(0), None),
        (Some(0), None, None),
        (None, None, None),
    ] {
        assert_eq!(super::add_options(left, right), expected);
    }
}

#[test]
fn validates_core_and_optional_breakdown_usage() {
    let parsed = ResponseUsage::from_response(&response(&valid_usage(120, 35, 155)));
    assert_eq!(parsed.input, Some(120));
    assert_eq!(parsed.output, Some(35));
    assert_eq!(parsed.total, Some(155));
    assert_eq!(parsed.cached_input, Some(12));
    assert_eq!(parsed.reasoning_output, Some(20));

    let zero = ResponseUsage::from_response(&response(&json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "total_tokens": 0,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0}
    })));
    assert_eq!(zero.input, Some(0));
    assert_eq!(zero.cached_input, Some(0));

    let large = ResponseUsage::from_response(&response(&json!({
        "input_tokens": 9_007_199_254_740_993_u64,
        "output_tokens": 7,
        "total_tokens": 9_007_199_254_741_000_u64
    })));
    assert_eq!(large.input, Some(9_007_199_254_740_993));
    assert_eq!(large.total, Some(9_007_199_254_741_000));
    assert_eq!(large.cached_input, None);
    assert_eq!(large.reasoning_output, None);
}

#[test]
fn missing_null_malformed_and_inconsistent_usage_are_unknown() {
    for body in [
        json!({}),
        response(&serde_json::Value::Null),
        response(&json!({"input_tokens": 1, "output_tokens": 2})),
        response(&json!({
            "input_tokens": 12,
            "output_tokens": 3,
            "total_tokens": 16
        })),
        response(&json!({
            "input_tokens": -1,
            "output_tokens": 1,
            "total_tokens": 0
        })),
        response(&json!({
            "input_tokens": 1.5,
            "output_tokens": 1,
            "total_tokens": 2.5
        })),
        response(&json!({
            "input_tokens": "1",
            "output_tokens": 1,
            "total_tokens": 2
        })),
    ] {
        let parsed = ResponseUsage::from_response(&body);
        assert_eq!(parsed.input, None);
        assert_eq!(parsed.output, None);
        assert_eq!(parsed.total, None);
    }
    let overflow: serde_json::Value = serde_json::from_str(
        r#"{"usage":{"input_tokens":18446744073709551616,"output_tokens":1,"total_tokens":2}}"#,
    )
    .expect("parse oversized provider count as JSON");
    assert_eq!(ResponseUsage::from_response(&overflow).input, None);
}

#[test]
fn invalid_optional_breakdowns_do_not_change_parent_counts() {
    for details in [
        json!({"input_tokens_details": {"cached_tokens": 121}}),
        json!({"input_tokens_details": {"cached_tokens": -1}}),
        json!({"input_tokens_details": "malformed"}),
    ] {
        let mut usage = valid_usage(120, 35, 155);
        usage["input_tokens_details"] = details
            .get("input_tokens_details")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let parsed = ResponseUsage::from_response(&response(&usage));
        assert_eq!(parsed.input, Some(120));
        assert_eq!(parsed.total, Some(155));
        assert_eq!(parsed.cached_input, None);
    }
    let mut usage = valid_usage(120, 35, 155);
    usage["output_tokens_details"]["reasoning_tokens"] = json!(36);
    let parsed = ResponseUsage::from_response(&response(&usage));
    assert_eq!(parsed.output, Some(35));
    assert_eq!(parsed.reasoning_output, None);
}

#[test]
fn aggregation_tracks_latest_separately_and_propagates_unknown_and_overflow() {
    let mut usage = UsageSnapshot::default();
    usage.request_sent();
    usage.response_completed(&response(&valid_usage(120, 35, 155)));
    usage.response_completed(&response(&json!({
        "input_tokens": 30,
        "output_tokens": 4,
        "total_tokens": 34
    })));
    let report = usage.format_report("implementation", "fixture-model", 65, Some(1000));
    assert!(
        report.lines().all(|line| line.chars().count() <= 60),
        "{report}"
    );
    assert!(report.contains("📊 Built-in · implementation · fixture-model"));
    assert!(report.contains("↻ 1 req · ✓ 2 completed · ⚙ 0 shell calls · 65s"));
    assert!(report.contains("🧠 Last response: 30 in · 4 out · 34 total"));
    assert!(report.contains("Context 3.4% / user limit 1,000"));
    assert!(report.contains("Cache unknown · reasoning unknown"));
    assert!(report.contains("∑ Observed completed-response usage\n  150 in · 39 out · 189 total"));

    let mut unknown = UsageSnapshot::default();
    unknown.response_completed(&response(&valid_usage(1, 1, 2)));
    unknown.response_completed(&response(&serde_json::Value::Null));
    let report = unknown.format_report("implementation", "fixture-model", 0, Some(100));
    assert!(report.contains("🧠 Last response: unknown in · unknown out · unknown total"));
    assert!(report.contains("Context unknown / user limit 100"));
    assert!(report.contains(
        "∑ Observed completed-response usage\n  unknown in · unknown out · unknown total"
    ));

    let mut large = UsageSnapshot::default();
    large.response_completed(&response(&json!({
        "input_tokens": u64::MAX,
        "output_tokens": 0,
        "total_tokens": u64::MAX
    })));
    let report = large.format_report("implementation", "fixture-model", 0, None);
    assert!(report.contains("18,446,744,073,709,551,615 in"));
    large.response_completed(&response(&json!({
        "input_tokens": 1,
        "output_tokens": 0,
        "total_tokens": 1
    })));
    let report = large.format_report("implementation", "fixture-model", 0, Some(100));
    assert!(
        report
            .contains("∑ Observed completed-response usage\n  unknown in · 0 out · unknown total")
    );
}

#[test]
fn merged_role_snapshots_keep_counts_and_last_context_snapshot() {
    let mut first = UsageSnapshot::default();
    first.request_sent();
    first.response_completed(&response(&valid_usage(12, 3, 15)));
    let mut second = UsageSnapshot::default();
    second.request_sent();
    second.request_sent();
    second.response_completed(&response(&valid_usage(7, 2, 9)));
    second.shell_spawned();
    let merged = first.merged(&second);
    let report = merged.format_report("implementation", "fixture-model", 1, Some(100));
    assert!(report.contains("↻ 3 req · ✓ 2 completed · ⚙ 1 shell calls"));
    assert!(report.contains("🧠 Last response: 7 in · 2 out · 9 total"));
    assert!(report.contains("∑ Observed completed-response usage\n  19 in · 5 out · 24 total"));
}

#[test]
fn context_percentage_uses_latest_snapshot_and_never_estimates_unavailable_values() {
    let mut usage = UsageSnapshot::default();
    let first = json!({
        "usage": {"input_tokens": 120, "output_tokens": 35, "total_tokens": 155},
        "prompt_sentinel": "must not appear"
    });
    usage.response_completed(&first);
    let report = usage.format_report("implementation", "fixture-model", 0, Some(1000));
    assert!(report.contains("Context 15.5% / user limit 1,000"));
    assert!(!report.contains("must not appear"));

    usage.response_completed(&response(&json!({
        "input_tokens": 30,
        "output_tokens": 4,
        "total_tokens": 34
    })));
    let report = usage.format_report("implementation", "fixture-model", 0, Some(100));
    assert!(report.contains("Context 34.0% / user limit 100"));

    let over_limit = usage.format_report("implementation", "fixture-model", 0, Some(10));
    assert!(over_limit.contains("Context 340.0% / user limit 10"));
    let without_limit = usage.format_report("implementation", "fixture-model", 0, None);
    assert!(without_limit.contains("↻ 0 req · ✓ 2 completed"));
    assert!(without_limit.contains("🧠 Last response: 30 in · 4 out · 34 total"));
    assert!(without_limit.contains("Context unknown / user limit unknown"));

    let no_usage = UsageSnapshot::default().format_report("review", "fixture-model", 0, Some(100));
    assert!(no_usage.contains("↻ 0 req · ✓ 0 completed"));
    assert!(no_usage.contains("🧠 Last response: unknown in · unknown out · unknown total"));
    assert!(no_usage.contains(
        "∑ Observed completed-response usage\n  unknown in · unknown out · unknown total"
    ));
    assert!(no_usage.contains("Context unknown / user limit 100"));
}

#[test]
fn compaction_trigger_uses_trustworthy_checked_eighty_percent_boundary() {
    for (total, expected) in [(799, false), (800, true)] {
        let usage = ResponseUsage {
            input: Some(total),
            output: Some(0),
            total: Some(total),
            ..ResponseUsage::default()
        };
        assert_eq!(
            super::compaction_threshold_reached(true, Some(1000), Some(&usage)),
            expected,
            "trigger at total={total}"
        );
    }
    let max_usage = ResponseUsage {
        input: Some(u64::MAX),
        output: Some(0),
        total: Some(u64::MAX),
        ..ResponseUsage::default()
    };
    assert!(super::compaction_threshold_reached(
        true,
        Some(u64::MAX),
        Some(&max_usage)
    ));
    assert!(!super::compaction_threshold_reached(
        false,
        Some(1000),
        Some(&max_usage)
    ));
    assert!(!super::compaction_threshold_reached(
        true,
        None,
        Some(&max_usage)
    ));
    assert!(!super::compaction_threshold_reached(true, Some(1000), None));
    assert!(!super::compaction_threshold_reached(
        true,
        Some(1000),
        Some(&ResponseUsage::default())
    ));
    assert!(!super::compaction_threshold_reached(
        true,
        Some(0),
        Some(&max_usage)
    ));
}

#[test]
fn compaction_usage_is_aggregated_and_reported_separately() {
    let compact = response(&valid_usage(42897, 12000, 54897));
    let mut first = UsageSnapshot::default();
    first.request_sent();
    first.compaction_completed(&compact);
    first.response_completed(&response(&valid_usage(10, 2, 12)));
    let mut second = UsageSnapshot::default();
    second.request_sent();
    second.compaction_completed(&response(&json!({
        "input_tokens": 1,
        "output_tokens": 2,
        "total_tokens": 99
    })));
    let merged = first.merged(&second);
    let report = merged.format_report("implementation", "fixture-model", 0, None);
    assert!(report.contains("↻ 2 req · ✓ 1 completed · ⚙ 0 shell calls"));
    assert!(report.contains("🧠 Last response: 10 in · 2 out · 12 total"));
    assert!(report.contains("∑ Observed completed-response usage\n  10 in · 2 out · 12 total"));
    assert!(
        report.contains("∑ Native compaction usage\n  unknown in · unknown out · unknown total")
    );
    assert!(!report.contains("54,897 in · 12,000 out · 66,897 total"));
}
