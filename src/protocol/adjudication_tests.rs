use serde_json::{Value, json};

use super::{ImplementationAuditResponse, PlanAuditResponse, parse, provider};

fn outcomes() -> [Value; 3] {
    [
        json!({
            "outcome":"confirmed", "rationale":"Spec requires unavailable approval.",
            "evidence":"specs/001.md: approval requirement; review.reason: approval unavailable",
            "requirement":"External approval before implementation", "prerequisite":"Approval",
            "unavailable_reason":"Only the external approver can grant approval",
            "required_input":"Provide external approval",
        }),
        json!({
            "outcome":"unsupported", "rationale":"The supplied reason identifies no blocker.",
            "evidence":"review.reason explicitly says no gaps and no genuine blocker",
        }),
        json!({
            "outcome":"ambiguous", "rationale":"Available evidence cannot establish approval.",
            "evidence":"specs/001.md requires approval but its availability is unresolved",
            "question":"Has the required external approval been granted?",
        }),
    ]
}

fn accepted(value: &Value, role: &str) -> bool {
    match role {
        "plan_audit" => parse::<PlanAuditResponse>(&value.to_string(), role).is_ok(),
        "review_audit" => parse::<ImplementationAuditResponse>(&value.to_string(), role).is_ok(),
        _ => panic!("unexpected audit role"),
    }
}

#[test]
fn adjudications_require_exact_outcome_specific_fields_on_both_wire_paths() {
    for role in ["plan_audit", "review_audit"] {
        for adjudication in outcomes() {
            let value = json!({"protocol_version":1,"role":role,"status":"adjudicated","adjudication":adjudication});
            assert!(accepted(&value, role));
            let normalized = provider::normalize_envelope(&json!({"response":value}))
                .expect("normalize adjudication");
            assert!(accepted(&normalized, role));
            let fields = adjudication.as_object().expect("adjudication fields");
            for field in fields.keys() {
                let mut missing = value.clone();
                missing["adjudication"]
                    .as_object_mut()
                    .expect("adjudication")
                    .remove(field);
                assert!(
                    !accepted(&missing, role),
                    "accepted missing {field}: {missing}"
                );
                let mut whitespace = value.clone();
                whitespace["adjudication"][field] = json!("  ");
                assert!(!accepted(&whitespace, role), "accepted blank {field}");
            }
            for invalid in [
                json!({"protocol_version":1,"role":role,"status":"adjudicated","adjudication":null}),
                json!({"protocol_version":2,"role":role,"status":"adjudicated","adjudication":adjudication}),
                json!({"protocol_version":1,"role":"wrong","status":"adjudicated","adjudication":adjudication}),
            ] {
                assert!(!accepted(&invalid, role));
            }
            let mut extra = value.clone();
            extra["adjudication"]["extra"] = json!(true);
            assert!(!accepted(&extra, role));
            let mut invalid = value;
            invalid["adjudication"]["outcome"] = json!("approved");
            assert!(!accepted(&invalid, role));
        }
    }
}

#[test]
fn nested_adjudication_shape_errors_have_exact_paths() {
    let value = json!({
        "protocol_version":1,"role":"plan_audit","status":"adjudicated",
        "adjudication":{"outcome":"unsupported","evidence":"review.reason", "extra":true}
    });
    let error = parse::<PlanAuditResponse>(&value.to_string(), "plan_audit")
        .expect_err("invalid nested shape");
    assert_eq!(
        error.to_string(),
        "agent response error: plan_audit response object at $.adjudication: missing fields [\"rationale\"]; unknown fields [\"extra\"]"
    );
}
