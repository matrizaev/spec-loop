use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};

use super::{BuiltinConversation, RESPONSES_URL, build_responses_request};
use crate::auth;
use crate::config::Profile;
use crate::prompts::PromptRole;

// Temporary opt-in diagnostic. Variants change ordering or remove only $ref descriptions.
// No repository content is sent, no tools can execute, and no request enters a workflow.
// Legacy A/B controls intentionally bypass production request encoding. The fixed-provider
// smoke test below exercises the production serializer, projection, stream, and local parser.
#[derive(Clone, Copy)]
enum PropertyOrder {
    Required,
    Tags,
    Payload,
}

struct RequiredFirst<'a> {
    value: &'a Value,
    order: Option<&'a Value>,
    mode: PropertyOrder,
}

impl Serialize for RequiredFirst<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.value {
            Value::Object(object) => {
                let mut output = serializer.serialize_map(Some(object.len()))?;
                let mut names = object.keys().collect::<Vec<_>>();
                if let Some(order) = self.order.and_then(Value::as_array) {
                    if order.len() != object.len() {
                        return Err(serde::ser::Error::custom("required/property counts differ"));
                    }
                    let tag = order
                        .first()
                        .and_then(Value::as_str)
                        .filter(|name| matches!(*name, "status" | "outcome"));
                    match self.mode {
                        PropertyOrder::Required => names.sort_by_key(|name| {
                            order
                                .iter()
                                .position(|field| field.as_str() == Some(name.as_str()))
                        }),
                        PropertyOrder::Tags => {
                            if let Some(tag) = tag {
                                names.sort_by_key(|name| name.as_str() != tag);
                            }
                        }
                        PropertyOrder::Payload => {
                            if tag.is_none() {
                                names.sort_by_key(|name| {
                                    order
                                        .iter()
                                        .position(|field| field.as_str() == Some(name.as_str()))
                                });
                            }
                        }
                    }
                }
                for name in names {
                    let child = object
                        .get(name)
                        .ok_or_else(|| serde::ser::Error::custom("missing object property"))?;
                    let order = if name == "properties" {
                        object.get("required")
                    } else {
                        None
                    };
                    output.serialize_entry(
                        name,
                        &Self {
                            value: child,
                            order,
                            mode: self.mode,
                        },
                    )?;
                }
                output.end()
            }
            Value::Array(items) => {
                let mut output = serializer.serialize_seq(Some(items.len()))?;
                for value in items {
                    output.serialize_element(&Self {
                        value,
                        order: None,
                        mode: self.mode,
                    })?;
                }
                output.end()
            }
            value => value.serialize(serializer),
        }
    }
}

#[test]
#[ignore = "manual live ChatGPT OAuth probe; requires SPEC_LOOP_API_PROBE=1 and may consume provider usage"]
fn live_audit_schema_ordering_probe() {
    assert_eq!(
        std::env::var("SPEC_LOOP_API_PROBE").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let profile = Profile::load(Path::new(".spec-loop.toml")).expect("load selected profile");
    let role = PromptRole::ReviewAudit;
    let model = profile
        .agent
        .model_for(role.agent_stage())
        .expect("configured model");
    let conversation = BuiltinConversation {
        input: vec![
            json!({"role":"user","content":"Schema acceptance probe only. Do not use tools. Return a blocked response with reason: schema probe only."}),
        ],
        tool_calls: None,
    };
    let mut request = build_responses_request(&profile, role, model, &conversation, true)
        .expect("generated request");
    request["max_output_tokens"] = json!(256);
    assert_eq!(request["tool_choice"], "none");
    let current = serde_json::to_vec(&request).expect("current serialized request");
    let ordered = serde_json::to_vec(&RequiredFirst {
        value: &request,
        order: None,
        mode: PropertyOrder::Required,
    })
    .expect("discriminator-first request");
    assert_ne!(current, ordered, "probe must change wire ordering");
    for body in [&current, &ordered] {
        assert_eq!(
            serde_json::from_slice::<Value>(body).expect("request JSON"),
            request,
            "schema semantics must remain identical"
        );
    }
    assert!(String::from_utf8_lossy(&ordered).contains(r#""properties":{"outcome":"#));
    let mut without_annotations = request;
    remove_reference_descriptions(
        without_annotations
            .pointer_mut("/text/format/schema")
            .expect("generated schema"),
    );
    let without_annotations =
        serde_json::to_vec(&without_annotations).expect("annotation-free references");
    let requests = vec![
        ("legacy-sorted-control", current),
        ("discriminator-first", ordered),
        ("reference-annotations-removed", without_annotations),
    ];
    crate::runtime::block_on(probe_requests(model, requests)).expect("probe runtime");
}

#[test]
#[ignore = "manual live built-in review A/B; requires SPEC_LOOP_API_PROBE=1 and consumes provider usage"]
fn live_builtin_review_status_ordering_probe() {
    assert_eq!(
        std::env::var("SPEC_LOOP_API_PROBE").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let profile = Profile::load(Path::new(".spec-loop.toml")).expect("selected model settings");
    let role = PromptRole::Review;
    let model = profile
        .agent
        .model_for(role.agent_stage())
        .expect("selected review model");
    let empty_prompt = fs::read_to_string("/var/tmp/spec-loop-HLlp4j/empty-review.prompt.txt")
        .expect("same zero-finding prompt tested with Pi");
    let issues_prompt = format!(
        "Temporary synthetic review-status probe, not approval of repository work. Do not use tools or change files. An independent review is complete. Requirement: the artifact must equal the string `ok`. Artifact: the string `wrong`. Evidence and location: inline fixture, line 1. A real actionable gap exists, the smallest correction is changing `wrong` to `ok`, and no genuine external blocker exists. Return the appropriate canonical review JSON with a fully populated finding, no Markdown. Schema:\n{}",
        serde_json::to_string_pretty(
            &crate::prompts::schema_for_role(role).expect("canonical schema")
        )
        .expect("schema JSON")
    );
    let blocker_prompt = format!(
        "Temporary synthetic review-status probe. Do not use tools or change files. Review found zero actionable code findings, but a mandatory external prerequisite is unavailable: release requires written approval from the user, no approval has been supplied, and tools cannot grant that approval. This is a genuine external blocker. Return the appropriate canonical review outcome, not automatic approval, with no Markdown. Schema:\n{}",
        serde_json::to_string_pretty(
            &crate::prompts::schema_for_role(role).expect("canonical schema")
        )
        .expect("schema JSON")
    );
    let mut requests = Vec::new();
    for (case, prompt) in [
        ("empty", empty_prompt),
        ("issues", issues_prompt),
        ("blocker", blocker_prompt),
    ] {
        let conversation = BuiltinConversation {
            input: vec![json!({"role":"user","content":prompt})],
            tool_calls: None,
        };
        let mut request = build_responses_request(&profile, role, model, &conversation, true)
            .expect("normal built-in review format");
        request["max_output_tokens"] = json!(2048);
        assert_eq!(request["tool_choice"], "none");
        let current = serde_json::to_vec(&request).expect("current wire request");
        let ordered = ordered_body(&request, PropertyOrder::Required);
        let tags = ordered_body(&request, PropertyOrder::Tags);
        let payload = ordered_body(&request, PropertyOrder::Payload);
        for body in [&current, &ordered, &tags, &payload] {
            assert_eq!(
                serde_json::from_slice::<Value>(body).expect("wire semantics"),
                request,
                "only key order may change"
            );
        }
        assert!(String::from_utf8_lossy(&ordered).contains(r#""properties":{"status":"#));
        match case {
            "empty" => {
                requests.push(("review-empty-sorted-control", current));
                requests.push(("review-empty-status-first", ordered));
            }
            "issues" => {
                requests.push(("review-issues-sorted-control", current));
                requests.push(("review-issues-status-first", ordered));
                requests.push(("review-issues-status-only", tags));
                requests.push(("review-issues-payload-only", payload));
            }
            "blocker" => {
                requests.push(("review-blocker-sorted-control", current));
                requests.push(("review-blocker-status-first", ordered));
            }
            _ => panic!("fixed probe case"),
        }
    }
    crate::runtime::block_on(probe_requests(model, requests)).expect("live A/B probe runtime");
}

struct ProductionCase {
    role: PromptRole,
    prompt: String,
    status: &'static str,
    outcome: Option<&'static str>,
}

#[test]
#[ignore = "manual live production transport smoke test; requires SPEC_LOOP_API_PROBE=1 and consumes provider usage"]
fn live_fixed_provider_handoffs_probe() {
    assert_eq!(
        std::env::var("SPEC_LOOP_API_PROBE").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let profile = Profile::load(Path::new(".spec-loop.toml")).expect("selected model settings");
    crate::runtime::block_on(async {
        let token = auth::active_access_token(false, &AtomicBool::new(false)).await.expect("existing ChatGPT OAuth");
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(60)).build().expect("probe client");
        for case in production_cases() {
            let model = profile.agent.model_for(case.role.agent_stage()).expect("configured model");
            let conversation = BuiltinConversation {input:vec![json!({"role":"user","content":case.prompt})],tool_calls:None};
            let mut request = build_responses_request(&profile, case.role, model, &conversation, true).expect("production schema projection");
            request["max_output_tokens"] = json!(2048);
            let completed = super::provider::request_with_retries(&client, RESPONSES_URL, &token, &request, Some(Instant::now() + Duration::from_secs(60)), &AtomicBool::new(false)).await.expect("production HTTP serialization and streaming parser");
            let items = super::shell::completed_output(&completed).expect("completed provider output");
            let (calls, text) = super::shell::parse_output_items(&items).expect("provider output items");
            assert!(calls.is_empty(), "live probes must not invoke tools");
            let canonical = super::normalize_structured_role_text(text).expect("production transport normalization");
            let value = validate_production_response(case.role, &canonical);
            assert_eq!(value["status"], case.status, "role {:?}: {canonical}", case.role);
            if let Some(outcome) = case.outcome {assert_eq!(value["adjudication"]["outcome"], outcome);}
            eprintln!("FIX_PROBE role={} model={model} status={} outcome={} findings={} strict_validation=passed tools=0", case.role.protocol_name(), value["status"], value.pointer("/adjudication/outcome").unwrap_or(&Value::Null), value["findings"].as_array().map_or(0, Vec::len));
        }
    }).expect("live probe runtime");
}

fn production_cases() -> Vec<ProductionCase> {
    let mut cases = Vec::new();
    for role in [PromptRole::PlanReview, PromptRole::Review] {
        let schema = serde_json::to_string_pretty(
            &crate::prompts::schema_for_role(role).expect("canonical schema"),
        )
        .expect("schema JSON");
        for (status, facts) in [
            (
                "no_issues",
                "All required review work is complete. Zero actionable findings and no genuine external blocker exist.",
            ),
            (
                "issues",
                "Requirement: artifact equals `ok`. Observed artifact: `wrong`, in inline fixture line 1. Correct it to `ok`. This is an actionable finding; no genuine external blocker exists.",
            ),
            (
                "blocked",
                "Zero actionable code findings exist, but a mandatory external prerequisite is unavailable: written user approval is required and has not been provided. Tools cannot grant that approval.",
            ),
        ] {
            cases.push(ProductionCase {role, prompt:format!("Synthetic role-handoff test only, not approval of repository work. Do not use tools. Choose the correct review outcome from these facts and populate all required fields: {facts}\nReturn canonical JSON only, conforming to:\n{schema}"), status, outcome:None});
        }
    }
    let adjudications: Vec<Value> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/blocker-adjudications.json"
    ))
    .expect("typed adjudication fixtures");
    for role in [PromptRole::PlanAudit, PromptRole::ReviewAudit] {
        for (outcome, fixture) in ["confirmed", "unsupported", "ambiguous"]
            .into_iter()
            .zip(&adjudications)
        {
            assert_eq!(fixture["outcome"], outcome, "fixture order");
            let expected = json!({"protocol_version":1,"role":role.protocol_name(),"status":"adjudicated","adjudication":fixture});
            cases.push(ProductionCase {role, prompt:format!("Schema acceptance fixture only, not a workflow or approval. Do not use tools. Return exactly this supplied canonical role response without Markdown: {expected}"),status:"adjudicated",outcome:Some(outcome)});
        }
    }
    cases
}

fn validate_production_response(role: PromptRole, text: &str) -> Value {
    use crate::protocol::{
        self, ImplementationAuditResponse, ImplementationReviewResponse, PlanAuditResponse,
        PlanReviewResponse,
    };
    match role {
        PromptRole::PlanReview => {
            protocol::parse::<PlanReviewResponse>(text, role.protocol_name())
                .expect("strict plan review");
        }
        PromptRole::Review => {
            protocol::parse::<ImplementationReviewResponse>(text, role.protocol_name())
                .expect("strict implementation review");
        }
        PromptRole::PlanAudit => {
            protocol::parse::<PlanAuditResponse>(text, role.protocol_name())
                .expect("strict plan audit");
        }
        PromptRole::ReviewAudit => {
            protocol::parse::<ImplementationAuditResponse>(text, role.protocol_name())
                .expect("strict review audit");
        }
        _ => panic!("unexpected probe role"),
    }
    serde_json::from_str(text).expect("validated canonical response")
}

fn ordered_body(request: &Value, mode: PropertyOrder) -> Vec<u8> {
    serde_json::to_vec(&RequiredFirst {
        value: request,
        order: None,
        mode,
    })
    .expect("ordered request serialization")
}

fn remove_reference_descriptions(schema: &mut Value) {
    match schema {
        Value::Object(object) => {
            if object.contains_key("$ref") {
                object.remove("description");
            }
            for value in object.values_mut() {
                remove_reference_descriptions(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                remove_reference_descriptions(item);
            }
        }
        _ => {}
    }
}

async fn probe_requests(model: &str, requests: Vec<(&str, Vec<u8>)>) {
    let selected = match std::env::var("SPEC_LOOP_API_PROBE_CASE") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => panic!("invalid probe case: {error}"),
    };
    assert!(
        selected
            .as_ref()
            .is_none_or(|selected| requests.iter().any(|(name, _)| name == selected)),
        "unknown probe case"
    );
    let token = auth::active_access_token(false, &AtomicBool::new(false))
        .await
        .expect("existing ChatGPT OAuth credential");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .expect("probe client");
    for (name, body) in requests {
        if selected.as_ref().is_some_and(|selected| selected != name) {
            continue;
        }
        let response = client
            .post(RESPONSES_URL)
            .bearer_auth(&token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .expect("schema-only API request");
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await.expect("provider response body");
        eprintln!(
            "SCHEMA_PROBE variant={name} model={model} HTTP={} request_id={request_id:?}",
            status.as_u16()
        );
        if status.is_success() {
            eprintln!(
                "SCHEMA_PROBE variant={name} schema accepted; received {} stream bytes; no tools executed",
                body.len()
            );
            report_stream_result(name, &body, &token);
        } else {
            let diagnostic: String = String::from_utf8_lossy(&body)
                .replace(&token, "<redacted-token>")
                .chars()
                .take(8192)
                .collect();
            eprintln!("SCHEMA_PROBE variant={name} provider rejection: {diagnostic}");
        }
    }
}

fn report_stream_result(name: &str, body: &[u8], token: &str) {
    let mut terminal = None;
    for line in String::from_utf8_lossy(body).split('\n') {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(data.trim()).expect("live probe SSE data JSON");
        if matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.incomplete" | "response.failed" | "error")
        ) {
            terminal = Some(event);
        }
    }
    let terminal = terminal.expect("provider terminal event");
    let response = &terminal["response"];
    eprintln!(
        "SCHEMA_PROBE variant={name} terminal={} status={} usage={}",
        terminal["type"], response["status"], response["usage"]
    );
    if let Some(output) = response["output"].as_array() {
        for item in output {
            if let Some(content) = item["content"].as_array() {
                for block in content {
                    if block["type"] == "output_text" {
                        let text = block["text"].as_str().expect("assistant final text");
                        eprintln!(
                            "SCHEMA_PROBE variant={name} raw_final={}",
                            text.replace(token, "<redacted-token>")
                        );
                        let normalized =
                            super::normalize_structured_role_text(Some(text.to_owned()))
                                .expect("normal transport normalization");
                        let wire: Value =
                            serde_json::from_str(&normalized).expect("canonical JSON");
                        let validation = match wire["role"].as_str() {
                            Some("review") => crate::protocol::parse::<
                                crate::protocol::ImplementationReviewResponse,
                            >(&normalized, "review")
                            .map(|parsed| parsed.role),
                            Some("review_audit") => crate::protocol::parse::<
                                crate::protocol::ImplementationAuditResponse,
                            >(
                                &normalized, "review_audit"
                            )
                            .map(|parsed| parsed.role),
                            other => panic!("unexpected probe role: {other:?}"),
                        };
                        match validation {
                            Ok(role) => eprintln!(
                                "SCHEMA_PROBE variant={name} canonical_status={} typed_role={role}",
                                wire["status"]
                            ),
                            Err(error) => eprintln!(
                                "SCHEMA_PROBE variant={name} canonical_status={} typed_validation_error={error}",
                                wire["status"]
                            ),
                        }
                    }
                }
            }
        }
    }
}
