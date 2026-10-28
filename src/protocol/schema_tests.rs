use schemars::JsonSchema;
use serde_json::{Value, json};

use super::{
    ImplementationAuditResponse, ImplementationClarificationResponse, ImplementationResponse,
    ImplementationReviewResponse, PlanAuditResponse, PlanAuthorResponse, PlanClarificationResponse,
    PlanReviewResponse, provider, schema,
};

fn contract<T: JsonSchema>() -> Value {
    json!({
        "canonical": serde_json::from_str::<Value>(&schema::<T>().expect("canonical schema"))
            .expect("parse canonical schema"),
        "provider": provider::schema::<T>().expect("provider projection"),
    })
}

#[test]
fn existing_role_schema_contracts_match_frozen_pre_change_baseline() {
    let baseline: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/role-response-schema-contracts.json"
    ))
    .expect("frozen pre-change contracts");
    for (role, mut actual) in [
        ("plan_author", contract::<PlanAuthorResponse>()),
        ("plan_review", contract::<PlanReviewResponse>()),
        ("plan_audit", contract::<PlanAuditResponse>()),
        (
            "plan_review_clarification",
            contract::<PlanClarificationResponse>(),
        ),
        ("implementation", contract::<ImplementationResponse>()),
        ("review", contract::<ImplementationReviewResponse>()),
        ("review_audit", contract::<ImplementationAuditResponse>()),
        (
            "review_clarification",
            contract::<ImplementationClarificationResponse>(),
        ),
    ] {
        if matches!(role, "plan_audit" | "review_audit") {
            for (kind, pointer) in [
                ("canonical", "/oneOf"),
                ("provider", "/properties/response/anyOf"),
            ] {
                let root = actual.get_mut(kind).expect("schema kind");
                let branches = root
                    .pointer_mut(pointer)
                    .and_then(Value::as_array_mut)
                    .expect("status branches");
                assert_eq!(branches.len(), 3, "only one additive status branch");
                branches.retain(|branch| {
                    let status = &branch["properties"]["status"];
                    status["const"] != "adjudicated" && status["enum"] != json!(["adjudicated"])
                });
                assert_eq!(branches.len(), 2, "preserve both existing audit forms");
                assert!(
                    root.get_mut("$defs")
                        .and_then(Value::as_object_mut)
                        .expect("audit definitions")
                        .remove("BlockerAdjudication")
                        .is_some()
                );
            }
        }
        let mut expected = baseline[role].clone();
        // Historical provider snapshots contain $ref descriptions rejected by the live API.
        // Ignore only those annotations; canonical schemas and all validation keywords still
        // compare exactly against the untouched pre-change fixture.
        remove_provider_reference_descriptions(&mut expected["provider"]);
        assert_eq!(
            actual, expected,
            "existing schema validation contract for {role}"
        );
    }
}

fn remove_provider_reference_descriptions(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.contains_key("$ref")
                && let Some(description) = object.remove("description")
            {
                assert!(
                    description.is_string(),
                    "only annotation text may be omitted"
                );
            }
            for child in object.values_mut() {
                remove_provider_reference_descriptions(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                remove_provider_reference_descriptions(child);
            }
        }
        _ => {}
    }
}
