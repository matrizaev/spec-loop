use super::{
    Actionability, AuditResult, Classification, Finding, FindingClassification, FindingEvidence,
    ImplementationAuditResponse, ImplementationClarificationResponse, ImplementationResponse,
    ImplementationReviewResponse, PlanAuditResponse, PlanAuthorResponse, PlanClarificationResponse,
    PlanReviewResponse, ReviewResult, schema, validate_classifications, validate_evidence,
    validate_findings,
};

#[test]
fn readiness_response_rejects_invalid_envelopes_and_unknown_fields() {
    let ready = super::parse::<PlanAuthorResponse>(
        r#"{"protocol_version":1,"role":"plan_author","status":"ready_for_review"}"#,
        "plan_author",
    );
    assert!(ready.is_ok(), "valid response rejected: {ready:?}");
    for response in [
        r#"{"protocol_version":2,"role":"plan_author","status":"ready_for_review"}"#,
        r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review"}"#,
        r#"{"protocol_version":1,"role":"plan_author","status":"ready_for_review","extra":true}"#,
        r#"{"protocol_version":1,"role":"plan_author","status":"blocked"}"#,
        r#"{"protocol_version":1,"role":"plan_author","status":"ready_for_review","reason":"extra"}"#,
        r#"prefix {"protocol_version":1,"role":"plan_author","status":"ready_for_review"}"#,
    ] {
        assert!(
            super::parse::<PlanAuthorResponse>(response, "plan_author").is_err(),
            "accepted invalid response: {response}"
        );
    }
}

#[test]
fn implementation_response_requires_brief_single_line_summary() {
    let valid = serde_json::json!({
        "protocol_version": 1,
        "role": "implementation",
        "summary": "Implemented requested changes and added regression coverage.",
        "status": "ready_for_review"
    })
    .to_string();
    assert!(super::parse::<ImplementationResponse>(&valid, "implementation").is_ok());

    let blocked = serde_json::json!({
        "protocol_version": 1,
        "role": "implementation",
        "summary": "No changes made; required input unavailable.",
        "status": "blocked",
        "reason": "required input unavailable"
    })
    .to_string();
    assert!(super::parse::<ImplementationResponse>(&blocked, "implementation").is_ok());

    let missing_summary =
        r#"{"protocol_version":1,"role":"implementation","status":"ready_for_review"}"#;
    assert!(super::parse::<ImplementationResponse>(missing_summary, "implementation").is_err());

    for summary in ["  ", "first line\nsecond line", "contains \u{1b} escape"] {
        let response = serde_json::json!({
            "protocol_version": 1,
            "role": "implementation",
            "summary": summary,
            "status": "ready_for_review"
        })
        .to_string();
        assert!(
            super::parse::<ImplementationResponse>(&response, "implementation").is_err(),
            "accepted invalid implementation summary: {summary:?}"
        );
    }

    let too_long = "x".repeat(super::MAX_IMPLEMENTATION_SUMMARY_CHARS + 1);
    let response = serde_json::json!({
        "protocol_version": 1,
        "role": "implementation",
        "summary": too_long,
        "status": "ready_for_review"
    })
    .to_string();
    assert!(super::parse::<ImplementationResponse>(&response, "implementation").is_err());
}

#[test]
fn field_shape_diagnostics_are_exact_and_deterministic() {
    let missing = serde_json::json!({
        "protocol_version": 1,
        "role": "implementation",
        "status": "ready_for_review"
    })
    .to_string();
    let error = super::parse::<ImplementationResponse>(&missing, "implementation")
        .expect_err("missing summary is rejected");
    assert_eq!(
        error.to_string(),
        "agent response error: implementation response object at $: missing fields [\"summary\"]"
    );

    let unknown = serde_json::json!({
        "protocol_version": 1,
        "role": "implementation",
        "summary": "Implemented the requested change.",
        "status": "ready_for_review",
        "unexpected_z": true
    })
    .to_string();
    let error = super::parse::<ImplementationResponse>(&unknown, "implementation")
        .expect_err("unknown field is rejected");
    assert_eq!(
        error.to_string(),
        "agent response error: implementation response object at $: unknown fields [\"unexpected_z\"]"
    );

    let combined = serde_json::json!({
        "protocol_version": 1,
        "role": "implementation",
        "status": "ready_for_review",
        "unexpected_z": true,
        "unexpected_a": true
    })
    .to_string();
    let error = super::parse::<ImplementationResponse>(&combined, "implementation")
        .expect_err("missing and unknown fields are both reported");
    assert_eq!(
        error.to_string(),
        "agent response error: implementation response object at $: missing fields [\"summary\"]; unknown fields [\"unexpected_a\", \"unexpected_z\"]"
    );

    let unrecognized_status = serde_json::json!({
        "protocol_version": 1,
        "role": "implementation",
        "status": "future_status",
        "unexpected": true
    })
    .to_string();
    let error = super::parse::<ImplementationResponse>(&unrecognized_status, "implementation")
        .expect_err("unrecognized status still reports conservative envelope defects");
    assert_eq!(
        error.to_string(),
        "agent response error: implementation response object at $: missing fields [\"summary\"]; unknown fields [\"unexpected\"]"
    );

    let nested = serde_json::json!({
        "protocol_version": 1,
        "role": "review",
        "status": "issues",
        "findings": [{
            "id": 1,
            "location": "src/example.rs",
            "evidence": "observed behavior",
            "violation": "requirement not met",
            "proposed_correction": "apply the required fix",
            "unexpected_z": true,
            "unexpected_a": true
        }]
    })
    .to_string();
    let error = super::parse::<ImplementationReviewResponse>(&nested, "review")
        .expect_err("nested missing and unknown fields are both reported");
    assert_eq!(
        error.to_string(),
        "agent response error: review response object at $.findings[0]: missing fields [\"requirement\"]; unknown fields [\"unexpected_a\", \"unexpected_z\"]"
    );

    let valid_root = serde_json::json!({
        "protocol_version": 1,
        "role": "implementation",
        "summary": "Implemented the requested change.",
        "status": "ready_for_review"
    })
    .to_string();
    assert!(super::parse::<ImplementationResponse>(&valid_root, "implementation").is_ok());

    let valid_nested = serde_json::json!({
        "protocol_version": 1,
        "role": "review",
        "status": "issues",
        "findings": [{
            "id": 1,
            "requirement": "required behavior",
            "location": "src/example.rs",
            "evidence": "observed behavior",
            "violation": "requirement not met",
            "proposed_correction": "apply the required fix"
        }]
    })
    .to_string();
    assert!(super::parse::<ImplementationReviewResponse>(&valid_nested, "review").is_ok());
}

#[test]
fn implementation_schema_requires_bounded_summary_for_each_status() {
    let generated = serde_json::from_str::<serde_json::Value>(
        &schema::<ImplementationResponse>().expect("generated implementation schema"),
    )
    .expect("parse implementation schema");
    let branches = generated["oneOf"].as_array().expect("status branches");
    assert_eq!(branches.len(), 2);
    for branch in branches {
        assert_eq!(branch["properties"]["summary"]["minLength"], 1);
        assert_eq!(
            branch["properties"]["summary"]["maxLength"],
            super::MAX_IMPLEMENTATION_SUMMARY_CHARS
        );
        assert!(
            branch["required"]
                .as_array()
                .expect("required fields")
                .iter()
                .any(|field| field == "summary")
        );
    }
}

#[test]
fn review_findings_require_sequential_positive_ids_and_consistent_status() {
    let response = super::parse::<PlanReviewResponse>(
            r#"{"protocol_version":1,"role":"plan_review","status":"issues","findings":[{"id":1,"requirement":"r","location":"l","evidence":"e","violation":"v","proposed_correction":"c"}]}"#,
            "plan_review",
        )
        .expect("valid review response");
    if let ReviewResult::Issues { findings } = response.result {
        assert!(validate_findings(&findings).is_ok());
    } else {
        panic!("expected issues");
    }
    for response in [
        r#"{"protocol_version":1,"role":"plan_review","status":"no_issues","findings":[{"id":1,"requirement":"r","location":"l","evidence":"e","violation":"v","proposed_correction":"c"}]}"#,
        r#"{"protocol_version":1,"role":"plan_review","status":"issues","findings":[]}"#,
        r#"{"protocol_version":1,"role":"plan_review","status":"blocked","reason":"x","findings":[]}"#,
    ] {
        assert!(
            super::parse::<PlanReviewResponse>(response, "plan_review").is_err(),
            "accepted invalid response: {response}"
        );
    }
    let findings = vec![Finding {
        id: 2,
        requirement: "r".to_owned(),
        location: "l".to_owned(),
        evidence: "e".to_owned(),
        violation: "v".to_owned(),
        proposed_correction: "c".to_owned(),
    }];
    assert!(validate_findings(&findings).is_err());
}

#[test]
fn generated_schemas_use_2020_12_and_static_array_bounds() {
    let review = serde_json::from_str::<serde_json::Value>(
        &schema::<PlanReviewResponse>().expect("generated review schema"),
    )
    .expect("parse review schema");
    assert_eq!(
        review["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(review["oneOf"][0]["properties"]["findings"]["minItems"], 0);
    assert_eq!(review["oneOf"][0]["properties"]["findings"]["maxItems"], 0);
    assert_eq!(review["oneOf"][1]["properties"]["findings"]["minItems"], 1);
    assert!(
        review["oneOf"][1]["properties"]["findings"]
            .get("maxItems")
            .is_none()
    );
    assert_eq!(
        review["oneOf"][0]["properties"]["role"]["pattern"],
        "^plan_review$"
    );
    assert_eq!(
        review["oneOf"][0]["properties"]["protocol_version"]["minimum"],
        1
    );
    assert_eq!(
        review["oneOf"][0]["properties"]["protocol_version"]["maximum"],
        1
    );
    for branch in review["oneOf"].as_array().expect("review branches") {
        assert_eq!(branch["additionalProperties"], false);
    }

    let audit = serde_json::from_str::<serde_json::Value>(
        &schema::<PlanAuditResponse>().expect("generated audit schema"),
    )
    .expect("parse audit schema");
    assert_eq!(
        audit["oneOf"][0]["properties"]["classifications"]["minItems"],
        1
    );
    assert!(
        audit["oneOf"][0]["properties"]["classifications"]
            .get("maxItems")
            .is_none()
    );

    let evidence = serde_json::from_str::<serde_json::Value>(
        &schema::<PlanClarificationResponse>().expect("generated evidence schema"),
    )
    .expect("parse evidence schema");
    assert!(
        evidence["oneOf"][0]["properties"]["evidence"]
            .get("minItems")
            .is_none()
    );
    assert!(
        evidence["oneOf"][0]["properties"]["evidence"]
            .get("maxItems")
            .is_none()
    );
    let citation_list = &evidence["$defs"]["FindingEvidence"]["properties"]["citations"];
    assert!(citation_list.get("minItems").is_none());
    assert!(citation_list.get("maxItems").is_none());
    assert!(
        !serde_json::to_string(&evidence)
            .expect("serialize schema")
            .contains("\"maxItems\":100")
    );
}

#[test]
fn every_role_schema_is_generated_with_exact_role_and_closed_variants() {
    let schemas = [
        (
            schema::<PlanAuthorResponse>().expect("plan author schema"),
            "plan_author",
        ),
        (
            schema::<PlanReviewResponse>().expect("plan review schema"),
            "plan_review",
        ),
        (
            schema::<PlanAuditResponse>().expect("plan audit schema"),
            "plan_audit",
        ),
        (
            schema::<PlanClarificationResponse>().expect("plan clarification schema"),
            "plan_review_clarification",
        ),
        (
            schema::<ImplementationResponse>().expect("implementation schema"),
            "implementation",
        ),
        (
            schema::<ImplementationReviewResponse>().expect("review schema"),
            "review",
        ),
        (
            schema::<ImplementationAuditResponse>().expect("audit schema"),
            "review_audit",
        ),
        (
            schema::<ImplementationClarificationResponse>().expect("clarification schema"),
            "review_clarification",
        ),
    ];
    for (generated, expected_role) in schemas {
        let generated = serde_json::from_str::<serde_json::Value>(&generated)
            .expect("parse generated role schema");
        let branches = generated["oneOf"]
            .as_array()
            .expect("schema status branches");
        assert!(branches.len() >= 2);
        for branch in branches {
            assert_eq!(
                branch["properties"]["role"]["pattern"],
                format!("^{expected_role}$")
            );
            assert_eq!(branch["properties"]["protocol_version"]["minimum"], 1);
            assert_eq!(branch["properties"]["protocol_version"]["maximum"], 1);
            assert_eq!(branch["additionalProperties"], false);
        }
    }
}

#[test]
fn audits_and_evidence_enforce_input_dependent_ids_and_blocked_shapes() {
    let finding = Finding {
        id: 1,
        requirement: "r".to_owned(),
        location: "l".to_owned(),
        evidence: "e".to_owned(),
        violation: "v".to_owned(),
        proposed_correction: "c".to_owned(),
    };
    let classification = FindingClassification {
        finding_id: 1,
        classification: Classification::Supported,
        actionability: Actionability::Required,
        rationale: "supported by evidence".to_owned(),
    };
    assert!(
        validate_classifications(
            std::slice::from_ref(&classification),
            std::slice::from_ref(&finding)
        )
        .is_ok()
    );
    assert!(validate_classifications(&[], std::slice::from_ref(&finding)).is_err());
    let supported_observation = FindingClassification {
        actionability: Actionability::NotRequired,
        ..classification.clone()
    };
    assert!(
        validate_classifications(
            std::slice::from_ref(&supported_observation),
            std::slice::from_ref(&finding)
        )
        .is_ok()
    );
    let wrong_order = FindingClassification {
        finding_id: 2,
        ..classification.clone()
    };
    assert!(validate_classifications(&[wrong_order], std::slice::from_ref(&finding)).is_err());
    let inconsistent = FindingClassification {
        classification: Classification::Unsupported,
        ..classification
    };
    assert!(validate_classifications(&[inconsistent], std::slice::from_ref(&finding)).is_err());

    let valid = super::parse::<PlanAuditResponse>(
            r#"{"protocol_version":1,"role":"plan_audit","status":"complete","classifications":[{"finding_id":1,"classification":"supported","actionability":"required","rationale":"verified"}]}"#,
            "plan_audit",
        )
        .expect("valid audit");
    assert!(matches!(valid.result, AuditResult::Complete { .. }));
    let blocked = super::parse::<ImplementationAuditResponse>(
            r#"{"protocol_version":1,"role":"review_audit","status":"blocked","reason":"context command failed"}"#,
            "review_audit",
        )
        .expect("valid blocked audit");
    assert!(matches!(blocked.result, AuditResult::Blocked { .. }));
    assert!(super::parse::<PlanAuditResponse>(
            r#"{"protocol_version":1,"role":"plan_audit","status":"complete","classifications":[{"finding_id":1,"classification":"supported","actionability":"required","rationale":"ok","extra":true}]}"#,
            "plan_audit",
        )
        .is_err());
    assert!(super::parse::<PlanAuditResponse>(
            r#"{"protocol_version":1,"role":"plan_audit","status":"complete","classifications":[{"finding_id":1,"classification":"supported","rationale":"missing actionability"}]}"#,
            "plan_audit",
        )
        .is_err());

    let entries = vec![FindingEvidence {
        finding_id: 1,
        citations: Vec::new(),
    }];
    assert!(validate_evidence(&entries, &[1]).is_ok());
    assert!(validate_evidence(&entries, &[2]).is_err());
    assert!(validate_evidence(&[entries[0].clone(), entries[0].clone()], &[1]).is_err());
    let clarification = super::parse::<ImplementationClarificationResponse>(
        r#"{"protocol_version":1,"role":"review_clarification","status":"complete","evidence":[]}"#,
        "review_clarification",
    )
    .expect("empty evidence is valid");
    assert!(
        matches!(clarification.result, super::EvidenceResult::Complete { evidence } if evidence.is_empty())
    );
}
