use serde_json::{Value, json};

use super::{normalize_envelope, project_schema, schema};
use crate::protocol::ResponseFields;
use crate::protocol::{
    Finding, ImplementationAuditResponse, ImplementationClarificationResponse,
    ImplementationResponse, ImplementationReviewResponse, PlanAuditResponse, PlanAuthorResponse,
    PlanClarificationResponse, PlanReviewResponse,
};

#[test]
fn provider_schemas_are_mechanical_closed_nested_status_projections_for_every_role() {
    let schemas = [
        schema::<PlanAuthorResponse>(),
        schema::<PlanReviewResponse>(),
        schema::<PlanAuditResponse>(),
        schema::<PlanClarificationResponse>(),
        schema::<ImplementationResponse>(),
        schema::<ImplementationReviewResponse>(),
        schema::<ImplementationAuditResponse>(),
        schema::<ImplementationClarificationResponse>(),
    ];
    for result in schemas {
        let schema = result.expect("provider role schema");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["response"]));
        assert!(schema["properties"]["response"]["anyOf"].is_array());
        assert!(schema.get("oneOf").is_none());
        assert!(schema.get("$schema").is_none());
        assert_provider_objects_are_closed_and_required(&schema);
        assert!(schema.to_string().contains("minimum"));
        assert!(!schema.to_string().contains("minLength"));
        assert!(!schema.to_string().contains("maxLength"));
        assert!(!schema.to_string().contains("uint32"));
        assert!(!schema.to_string().contains("uint64"));
    }
}

#[test]
fn provider_references_strip_annotations_but_reject_sibling_constraints() {
    let canonical = json!({
        "$defs":{"Payload":{"type":"string","description":"Referenced type documentation"}},
        "oneOf":[{"type":"object","properties":{
            "status":{"const":"ready","type":"string"},
            "payload":{"$ref":"#/$defs/Payload","description":"Field documentation"}
        },"required":["status","payload"],"additionalProperties":false}]
    });
    let projected = project_schema(&canonical).expect("annotation-only reference");
    assert_eq!(
        projected["properties"]["response"]["anyOf"][0]["properties"]["payload"],
        json!({"$ref":"#/$defs/Payload"})
    );
    assert_eq!(
        canonical["oneOf"][0]["properties"]["payload"]["description"],
        "Field documentation"
    );
    assert_eq!(
        projected["$defs"]["Payload"]["description"],
        "Referenced type documentation"
    );
    for (keyword, value) in [
        ("minimum", json!(1)),
        ("minLength", json!(1)),
        ("enum", json!(["restricted"])),
        ("unknown", json!(true)),
    ] {
        let mut invalid = canonical.clone();
        invalid["oneOf"][0]["properties"]["payload"]
            .as_object_mut()
            .expect("reference object")
            .insert(keyword.to_owned(), value);
        assert!(
            project_schema(&invalid)
                .expect_err("never drop a reference constraint")
                .to_string()
                .contains("unsupported keyword")
        );
    }
    for (keyword, value) in [("$ref", json!(12)), ("description", json!(false))] {
        let mut invalid = canonical.clone();
        invalid["oneOf"][0]["properties"]["payload"][keyword] = value;
        assert!(
            project_schema(&invalid).is_err(),
            "invalid {keyword} must fail closed"
        );
    }
}

#[test]
fn provider_projection_fails_closed_on_unrecognized_canonical_keywords() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "oneOf": [{
            "type": "object",
            "properties": {"status": {"const": "ready"}},
            "required": ["status"],
            "unevaluatedProperties": false
        }]
    });
    let error = project_schema(&schema)
        .expect_err("unsupported generated keyword must not be silently discarded");
    assert!(error.to_string().contains("unsupported keyword"));
}

#[test]
fn provider_projection_requires_status_discriminators_to_be_disjoint() {
    let schema = json!({
        "oneOf": [
            {"properties": {"status": {"const": "same"}}},
            {"properties": {"status": {"enum": ["same", "other"]}}}
        ]
    });
    let error = project_schema(&schema).expect_err("overlapping status branches are unsafe");
    assert!(error.to_string().contains("not pairwise disjoint"));
}

#[test]
fn nested_outcome_projection_rejects_overlapping_or_missing_discriminators() {
    for other in [
        json!({"outcome":{"const":"same"}}),
        json!({"different":{"const":"different"}}),
    ] {
        let canonical = json!({
            "oneOf":[{
                "type":"object","properties":{
                    "status":{"const":"adjudicated"},
                    "adjudication":{"oneOf":[
                        {"type":"object","properties":{"outcome":{"const":"same"}},"required":["outcome"],"additionalProperties":false},
                        {"type":"object","properties":other,"required":["outcome"],"additionalProperties":false}
                    ]}
                },"required":["status","adjudication"],"additionalProperties":false
            }]
        });
        assert!(project_schema(&canonical).is_err());
    }
}

#[test]
fn transport_normalization_accepts_only_exact_wrapper_and_object_value() {
    let canonical =
        json!({"protocol_version": 1, "role": "implementation", "status": "ready_for_review"});
    assert_eq!(
        normalize_envelope(&json!({"response": canonical.clone()}))
            .expect("normalize exact envelope"),
        canonical
    );
    for invalid in [
        json!({"response": canonical, "extra": true}),
        json!({"protocol_version": 1, "role": "implementation"}),
        json!({"response": null}),
        json!("not an envelope"),
    ] {
        assert!(normalize_envelope(&invalid).is_err());
    }
}

fn assert_provider_objects_are_closed_and_required(value: &Value) {
    match value {
        Value::Object(object) => {
            if object.contains_key("$ref") {
                assert_eq!(object.len(), 1, "provider references cannot have siblings");
            }
            if object.get("type").and_then(Value::as_str) == Some("object") {
                assert_eq!(
                    object.get("additionalProperties"),
                    Some(&Value::Bool(false))
                );
                let properties = object["properties"].as_object().expect("object properties");
                let required = object["required"]
                    .as_array()
                    .expect("object required fields")
                    .iter()
                    .map(|field| field.as_str().expect("required string"))
                    .collect::<std::collections::BTreeSet<_>>();
                let names = properties
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>();
                assert_eq!(required, names);
            }
            for child in object.values() {
                assert_provider_objects_are_closed_and_required(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_provider_objects_are_closed_and_required(item);
            }
        }
        _ => {}
    }
}

#[test]
fn provider_envelopes_and_pi_wire_values_share_role_status_validation() {
    assert_readiness_wire_parity();
    assert_review_wire_parity();
    assert_audit_wire_parity();
    assert_clarification_wire_parity();
}

#[test]
fn pi_and_provider_responses_share_input_dependent_validators() {
    let requested = [finding(1), finding(2)];
    let valid_review = json!({
        "protocol_version": 1,
        "role": "plan_review",
        "status": "issues",
        "findings": [{"id": 1, "requirement": "required", "location": "src/example.rs:10", "evidence": "observed", "violation": "absent", "proposed_correction": "implement"}]
    });
    let invalid_review_order = json!({
        "protocol_version": 1,
        "role": "review",
        "status": "issues",
        "findings": [
            {"id": 2, "requirement": "second", "location": "src/example.rs:20", "evidence": "second", "violation": "second", "proposed_correction": "second"},
            {"id": 1, "requirement": "first", "location": "src/example.rs:10", "evidence": "first", "violation": "first", "proposed_correction": "first"}
        ]
    });
    assert_execution_validation_parity::<PlanReviewResponse>(
        &valid_review,
        "plan_review",
        |response| crate::protocol::validate_review_result(&response.result),
        true,
    );
    assert_execution_validation_parity::<ImplementationReviewResponse>(
        &invalid_review_order,
        "review",
        |response| crate::protocol::validate_review_result(&response.result),
        false,
    );

    let classifications = json!({
        "protocol_version": 1,
        "role": "plan_audit",
        "status": "complete",
        "classifications": [
            {"finding_id": 1, "classification": "supported", "actionability": "required", "rationale": "Required behavior is absent."},
            {"finding_id": 2, "classification": "supported", "actionability": "not_required", "rationale": "No correction is required."}
        ]
    });
    assert_execution_validation_parity::<PlanAuditResponse>(
        &classifications,
        "plan_audit",
        |response| validate_audit_result(&response.result, &requested),
        true,
    );
    let reversed_classifications = json!({
        "protocol_version": 1,
        "role": "review_audit",
        "status": "complete",
        "classifications": [
            {"finding_id": 2, "classification": "supported", "actionability": "not_required", "rationale": "No correction is required."},
            {"finding_id": 1, "classification": "supported", "actionability": "required", "rationale": "Required behavior is absent."}
        ]
    });
    assert_execution_validation_parity::<ImplementationAuditResponse>(
        &reversed_classifications,
        "review_audit",
        |response| validate_audit_result(&response.result, &requested),
        false,
    );
    let unrequested_classification = json!({
        "protocol_version": 1,
        "role": "review_audit",
        "status": "complete",
        "classifications": [{"finding_id": 3, "classification": "supported", "actionability": "required", "rationale": "Required behavior is absent."}]
    });
    assert_execution_validation_parity::<ImplementationAuditResponse>(
        &unrequested_classification,
        "review_audit",
        |response| validate_audit_result(&response.result, &requested[..1]),
        false,
    );

    let evidence = json!({
        "protocol_version": 1,
        "role": "plan_review_clarification",
        "status": "complete",
        "evidence": [{"finding_id": 1, "citations": [{"path": "src/example.rs", "location": "line 10", "excerpt": "fixture evidence", "relevance": "supports finding"}]}]
    });
    assert_execution_validation_parity::<PlanClarificationResponse>(
        &evidence,
        "plan_review_clarification",
        |response| validate_evidence_result(&response.result, &[1, 2]),
        true,
    );
    let unrequested_evidence = json!({
        "protocol_version": 1,
        "role": "review_clarification",
        "status": "complete",
        "evidence": [{"finding_id": 3, "citations": []}]
    });
    assert_execution_validation_parity::<ImplementationClarificationResponse>(
        &unrequested_evidence,
        "review_clarification",
        |response| validate_evidence_result(&response.result, &[1, 2]),
        false,
    );
}

fn validate_audit_result(
    result: &crate::protocol::AuditResult,
    requested: &[Finding],
) -> crate::error::Result<()> {
    match result {
        crate::protocol::AuditResult::Complete { classifications } => {
            crate::protocol::validate_classifications(classifications, requested)
        }
        crate::protocol::AuditResult::Blocked { reason } => {
            crate::protocol::validate_text(reason, "blocked reason")
        }
        crate::protocol::AuditResult::Adjudicated { .. } => Err(
            crate::error::SpecLoopError::Agent("expected finding classifications".to_owned()),
        ),
    }
}

fn validate_evidence_result(
    result: &crate::protocol::EvidenceResult,
    requested_ids: &[u64],
) -> crate::error::Result<()> {
    match result {
        crate::protocol::EvidenceResult::Complete { evidence } => {
            crate::protocol::validate_evidence(evidence, requested_ids)
        }
        crate::protocol::EvidenceResult::Blocked { reason } => {
            crate::protocol::validate_text(reason, "blocked reason")
        }
    }
}

fn finding(id: u64) -> Finding {
    Finding {
        id,
        requirement: format!("requirement {id}"),
        location: format!("src/example.rs:{id}"),
        evidence: format!("evidence {id}"),
        violation: format!("violation {id}"),
        proposed_correction: format!("correction {id}"),
    }
}

fn assert_execution_validation_parity<T>(
    canonical: &Value,
    role: &str,
    validate: impl Fn(&T) -> crate::error::Result<()>,
    expected_valid: bool,
) where
    T: serde::de::DeserializeOwned + schemars::JsonSchema + serde::Serialize + ResponseFields,
{
    let pi = crate::protocol::parse::<T>(&canonical.to_string(), role)
        .and_then(|response| validate(&response));
    let provider = normalize_envelope(&json!({"response": canonical})).and_then(|value| {
        crate::protocol::parse::<T>(&value.to_string(), role)
            .and_then(|response| validate(&response))
    });
    assert_eq!(pi.is_ok(), expected_valid, "Pi input: {canonical}");
    assert_eq!(
        provider.is_ok(),
        expected_valid,
        "provider input: {canonical}"
    );
    assert_eq!(pi.is_ok(), provider.is_ok());
}

fn assert_readiness_wire_parity() {
    assert_response_parity::<PlanAuthorResponse>(
        "plan_author",
        &[
            json!({"protocol_version": 1, "role": "plan_author", "status": "ready_for_review"}),
            json!({"protocol_version": 1, "role": "plan_author", "status": "blocked", "reason": "required input unavailable"}),
        ],
        &[
            json!({"protocol_version": 1, "role": "plan_author", "status": "ready_for_review", "extra": true}),
        ],
    );
    assert_response_parity::<ImplementationResponse>(
        "implementation",
        &[
            json!({"protocol_version": 1, "role": "implementation", "status": "ready_for_review", "summary": "Implemented requested behavior."}),
            json!({"protocol_version": 1, "role": "implementation", "status": "blocked", "summary": "No changes made; required input unavailable.", "reason": "required input unavailable"}),
        ],
        &[
            json!({"protocol_version": 1, "role": "implementation", "status": "ready_for_review", "summary": "", "extra": true}),
        ],
    );
}

fn assert_review_wire_parity() {
    let finding = json!({
        "id": 1,
        "requirement": "required behavior",
        "location": "src/example.rs:10",
        "evidence": "observed fixture condition",
        "violation": "required behavior is absent",
        "proposed_correction": "implement required behavior"
    });
    assert_response_parity::<PlanReviewResponse>(
        "plan_review",
        &[
            json!({"protocol_version": 1, "role": "plan_review", "status": "no_issues", "findings": []}),
            json!({"protocol_version": 1, "role": "plan_review", "status": "issues", "findings": [finding]}),
            json!({"protocol_version": 1, "role": "plan_review", "status": "blocked", "reason": "review context unavailable"}),
        ],
        &[
            json!({"protocol_version": 1, "role": "plan_review", "status": "no_issues", "findings": [{"id": 1, "requirement": "required behavior", "location": "src/example.rs:10", "evidence": "observed fixture condition", "violation": "required behavior is absent", "proposed_correction": "implement required behavior"}]}),
        ],
    );
    assert_response_parity::<ImplementationReviewResponse>(
        "review",
        &[
            json!({"protocol_version": 1, "role": "review", "status": "no_issues", "findings": []}),
            json!({"protocol_version": 1, "role": "review", "status": "issues", "findings": [finding]}),
            json!({"protocol_version": 1, "role": "review", "status": "blocked", "reason": "review context unavailable"}),
        ],
        &[json!({"protocol_version": 1, "role": "review", "status": "issues", "findings": []})],
    );
}

fn assert_audit_wire_parity() {
    let classification = json!({
        "finding_id": 1,
        "classification": "supported",
        "actionability": "required",
        "rationale": "The cited source confirms the required behavior is unmet."
    });
    assert_response_parity::<PlanAuditResponse>(
        "plan_audit",
        &[
            json!({"protocol_version": 1, "role": "plan_audit", "status": "complete", "classifications": [classification]}),
            json!({"protocol_version": 1, "role": "plan_audit", "status": "blocked", "reason": "audit evidence unavailable"}),
        ],
        &[
            json!({"protocol_version": 1, "role": "plan_audit", "status": "complete", "classifications": []}),
        ],
    );
    assert_response_parity::<ImplementationAuditResponse>(
        "review_audit",
        &[
            json!({"protocol_version": 1, "role": "review_audit", "status": "complete", "classifications": [classification]}),
            json!({"protocol_version": 1, "role": "review_audit", "status": "blocked", "reason": "audit evidence unavailable"}),
        ],
        &[
            json!({"protocol_version": 1, "role": "review_audit", "status": "complete", "classifications": []}),
        ],
    );
}

fn assert_clarification_wire_parity() {
    let evidence = json!({
        "finding_id": 1,
        "citations": [{
            "path": "src/example.rs",
            "location": "line 10",
            "excerpt": "fixture source excerpt",
            "relevance": "supports the requested evidence"
        }]
    });
    assert_response_parity::<PlanClarificationResponse>(
        "plan_review_clarification",
        &[
            json!({"protocol_version": 1, "role": "plan_review_clarification", "status": "complete", "evidence": []}),
            json!({"protocol_version": 1, "role": "plan_review_clarification", "status": "complete", "evidence": [evidence]}),
            json!({"protocol_version": 1, "role": "plan_review_clarification", "status": "blocked", "reason": "citation source unavailable"}),
        ],
        &[
            json!({"protocol_version": 1, "role": "plan_review_clarification", "status": "complete", "evidence": [{"finding_id": 0, "citations": []}]}),
        ],
    );
    assert_response_parity::<ImplementationClarificationResponse>(
        "review_clarification",
        &[
            json!({"protocol_version": 1, "role": "review_clarification", "status": "complete", "evidence": []}),
            json!({"protocol_version": 1, "role": "review_clarification", "status": "complete", "evidence": [evidence]}),
            json!({"protocol_version": 1, "role": "review_clarification", "status": "blocked", "reason": "citation source unavailable"}),
        ],
        &[
            json!({"protocol_version": 1, "role": "review_clarification", "status": "complete", "evidence": [{"finding_id": 1, "citations": []}, {"finding_id": 1, "citations": []}]}),
        ],
    );
}

fn assert_response_parity<T>(role: &str, valid: &[Value], invalid: &[Value])
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema + serde::Serialize + ResponseFields,
{
    for value in valid {
        assert_equal_acceptance::<T>(value, role, true);
    }
    for value in invalid {
        assert_equal_acceptance::<T>(value, role, false);
    }
}

fn assert_equal_acceptance<T>(canonical: &Value, role: &str, expected_valid: bool)
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema + serde::Serialize + ResponseFields,
{
    let canonical_text = canonical.to_string();
    let pi_result = crate::protocol::parse::<T>(&canonical_text, role);
    let provider_transport = json!({"response": canonical});
    let provider_result = normalize_envelope(&provider_transport).and_then(|normalized| {
        serde_json::to_string(&normalized)
            .map_err(|error| crate::error::SpecLoopError::Agent(error.to_string()))
            .and_then(|text| crate::protocol::parse::<T>(&text, role))
    });
    assert_eq!(
        pi_result.is_ok(),
        expected_valid,
        "Pi wire value: {canonical}"
    );
    assert_eq!(
        provider_result.is_ok(),
        expected_valid,
        "provider envelope: {provider_transport}"
    );
    if let (Ok(pi), Ok(provider)) = (pi_result, provider_result) {
        assert_eq!(
            serde_json::to_value(pi).expect("serialize parsed Pi result"),
            serde_json::to_value(provider).expect("serialize parsed provider result")
        );
    }
}
