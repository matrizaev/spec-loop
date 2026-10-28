use serde_json::{Value, json};

use super::encode_request;
use crate::prompts::{self, PromptRole};
use crate::protocol::provider::project_schema;

#[test]
fn every_role_sends_status_first_and_audits_send_outcome_first_without_semantic_changes() {
    for role in [
        PromptRole::PlanAuthor,
        PromptRole::PlanReview,
        PromptRole::PlanAudit,
        PromptRole::PlanClarification,
        PromptRole::Implementation,
        PromptRole::Review,
        PromptRole::ReviewAudit,
        PromptRole::ReviewClarification,
    ] {
        let canonical = prompts::schema_for_role(role).expect("canonical schema");
        let projected = project_schema(&canonical).expect("provider projection");
        let branches = projected
            .pointer("/properties/response/anyOf")
            .and_then(Value::as_array)
            .expect("status alternatives")
            .len();
        let body = json!({"model":"fixture","input":[{"role":"user","content":"do not rewrite input"}], "tool_choice":"none", "text":{"format":{"type":"json_schema","strict":true,"schema":projected}}});
        let encoded = encode_request(&body).expect("wire request");
        assert_eq!(
            serde_json::from_slice::<Value>(&encoded).expect("valid wire JSON"),
            body,
            "only schema key order changes"
        );
        let text = String::from_utf8(encoded).expect("UTF-8 wire");
        assert_eq!(
            text.matches(r#""properties":{"status":"#).count(),
            branches,
            "every {role:?} branch must choose status before payload"
        );
        let expected_outcomes = if matches!(role, PromptRole::PlanAudit | PromptRole::ReviewAudit) {
            3
        } else {
            0
        };
        assert_eq!(
            text.matches(r#""properties":{"outcome":"#).count(),
            expected_outcomes
        );
    }
}

#[test]
fn only_discriminated_union_properties_are_reordered_and_user_data_is_untouched() {
    let branch = |tag: &str| {
        json!({"type":"object","properties":{
        "aaa":{"type":"object","properties":{"aaa":{"type":"string"},"color":{"type":"string","enum":["red"]}},"required":["aaa","color"],"additionalProperties":false},
        "variant":{"type":"string","const":tag}
    },"required":["aaa","variant"],"additionalProperties":false})
    };
    let canonical = json!({"oneOf":[branch("one"),branch("two")]});
    let data = json!({"properties":{"aaa":{"type":"string"},"variant":{"enum":["input-value"]}},"required":["variant","aaa"]});
    let body = json!({"input":data,"text":{"format":{"schema":project_schema(&canonical).expect("custom-tag schema")}}});
    let encoded = encode_request(&body).expect("wire request");
    let text = String::from_utf8(encoded).expect("UTF-8 wire");
    assert_eq!(
        text.matches(r#""properties":{"variant":"#).count(),
        2,
        "tag name is schema-derived, not hard-coded"
    );
    assert!(
        text.contains(r#""properties":{"aaa":{"type":"string"},"color":"#),
        "non-union payload object order stays unchanged"
    );
    assert!(text.contains(&format!(
        r#""input":{}"#,
        serde_json::to_string(&data).expect("original input")
    )));
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("wire JSON"),
        body
    );
}

#[test]
fn ordinary_requests_without_a_structured_schema_keep_identical_wire_bytes() {
    for body in [
        json!({"model":"fixture","stream":true}),
        json!({"text":{"format":{"type":"text"}},"input":[]}),
    ] {
        assert_eq!(
            encode_request(&body).expect("request"),
            serde_json::to_vec(&body).expect("original JSON")
        );
    }
}

#[test]
fn malformed_provider_schema_fails_serialization_instead_of_losing_fields() {
    for schema in [
        json!({"properties":1}),
        json!({"anyOf":[
            {"properties":{"tag":{"enum":["one"]}}},
            {"properties":{"other":{"type":"string"}}}
        ]}),
    ] {
        let body = json!({"text":{"format":{"schema":schema}}});
        assert!(
            encode_request(&body)
                .expect_err("invalid schema must fail closed")
                .to_string()
                .contains("serialize OpenAI Responses request")
        );
    }
}
