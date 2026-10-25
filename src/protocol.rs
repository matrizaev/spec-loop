use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::{Result, SpecLoopError};

const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    #[schemars(range(min = 1))]
    pub id: u64,
    #[schemars(length(min = 1))]
    pub requirement: String,
    #[schemars(length(min = 1))]
    pub location: String,
    #[schemars(length(min = 1))]
    pub evidence: String,
    #[schemars(length(min = 1))]
    pub violation: String,
    #[schemars(length(min = 1))]
    pub proposed_correction: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub enum Readiness {
    ReadyForReview,
    Blocked {
        #[schemars(length(min = 1))]
        reason: String,
    },
}

macro_rules! readiness_response {
    ($name:ident, $role:literal) => {
        #[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
        #[schemars(deny_unknown_fields)]
        pub struct $name {
            #[schemars(range(min = 1, max = 1))]
            pub protocol_version: u32,
            #[schemars(regex(pattern = concat!("^", $role, "$")))]
            pub role: String,
            #[serde(flatten)]
            pub result: Readiness,
        }
    };
}

readiness_response!(PlanAuthorResponse, "plan_author");
readiness_response!(ImplementationResponse, "implementation");

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub enum ReviewResult {
    NoIssues {
        #[schemars(length(max = 0))]
        findings: Vec<Finding>,
    },
    Issues {
        #[schemars(length(min = 1))]
        findings: Vec<Finding>,
    },
    Blocked {
        #[schemars(length(min = 1))]
        reason: String,
    },
}

macro_rules! review_response {
    ($name:ident, $role:literal) => {
        #[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
        #[schemars(deny_unknown_fields)]
        pub struct $name {
            #[schemars(range(min = 1, max = 1))]
            pub protocol_version: u32,
            #[schemars(regex(pattern = concat!("^", $role, "$")))]
            pub role: String,
            #[serde(flatten)]
            pub result: ReviewResult,
        }
    };
}

review_response!(PlanReviewResponse, "plan_review");
review_response!(ImplementationReviewResponse, "review");

/// Describes whether a finding's factual claim is established by evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// Evidence establishes the finding's factual claim.
    Supported,
    /// Evidence disproves the finding's factual claim.
    Unsupported,
    /// Available evidence cannot settle the finding's factual claim.
    Ambiguous,
}

/// Describes whether a factually supported finding requires a correction.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Actionability {
    /// Existing requirements require a correction.
    Required,
    /// No correction is needed under existing requirements and scope.
    NotRequired,
    /// Available evidence cannot settle whether correction is required.
    Ambiguous,
}

/// Records factual validity separately from whether a correction is necessary.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
#[serde(deny_unknown_fields)]
pub struct FindingClassification {
    /// ID of corresponding input finding.
    #[schemars(range(min = 1))]
    pub finding_id: u64,
    /// Factual validity of the reported finding.
    pub classification: Classification,
    /// Whether repository requirements require a correction.
    pub actionability: Actionability,
    /// Evidence-based rationale, including correction proportionality.
    #[schemars(length(min = 1))]
    pub rationale: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub enum AuditResult {
    Complete {
        #[schemars(length(min = 1))]
        classifications: Vec<FindingClassification>,
    },
    Blocked {
        #[schemars(length(min = 1))]
        reason: String,
    },
}

macro_rules! audit_response {
    ($name:ident, $role:literal) => {
        #[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
        #[schemars(deny_unknown_fields)]
        pub struct $name {
            #[schemars(range(min = 1, max = 1))]
            pub protocol_version: u32,
            #[schemars(regex(pattern = concat!("^", $role, "$")))]
            pub role: String,
            #[serde(flatten)]
            pub result: AuditResult,
        }
    };
}

audit_response!(PlanAuditResponse, "plan_audit");
audit_response!(ImplementationAuditResponse, "review_audit");

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    #[schemars(length(min = 1))]
    pub path: String,
    #[schemars(length(min = 1))]
    pub location: String,
    #[schemars(length(min = 1))]
    pub excerpt: String,
    #[schemars(length(min = 1))]
    pub relevance: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
#[serde(deny_unknown_fields)]
pub struct FindingEvidence {
    #[schemars(range(min = 1))]
    pub finding_id: u64,
    pub citations: Vec<Citation>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub enum EvidenceResult {
    Complete {
        evidence: Vec<FindingEvidence>,
    },
    Blocked {
        #[schemars(length(min = 1))]
        reason: String,
    },
}

macro_rules! evidence_response {
    ($name:ident, $role:literal) => {
        #[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
        #[schemars(deny_unknown_fields)]
        pub struct $name {
            #[schemars(range(min = 1, max = 1))]
            pub protocol_version: u32,
            #[schemars(regex(pattern = concat!("^", $role, "$")))]
            pub role: String,
            #[serde(flatten)]
            pub result: EvidenceResult,
        }
    };
}

evidence_response!(PlanClarificationResponse, "plan_review_clarification");
evidence_response!(ImplementationClarificationResponse, "review_clarification");

/// Typed audit input keeps original reviewer findings paired with their review response.
#[derive(JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
pub struct AuditInput<'a, R> {
    pub review: &'a R,
    pub findings: &'a [Finding],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<&'a AuditEvidenceInput>,
}

/// Cited evidence and findings supplied to a follow-up grounding audit.
#[derive(JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
pub struct AuditEvidenceInput {
    pub findings: Vec<Finding>,
    pub evidence: Vec<FindingEvidence>,
}

/// Typed evidence-clarification request for selected finding IDs.
#[derive(JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
pub struct ClarificationInput<'a, R> {
    pub review: &'a R,
    pub findings: &'a [Finding],
    pub requested_finding_ids: &'a [u64],
}

/// Actionable plan findings passed to `PlanAuthor` for repair.
#[derive(JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
pub struct PlanRepairInput<'a> {
    /// Findings audited as supported and requiring correction.
    pub actionable_findings: Vec<&'a Finding>,
    /// Audit rationale for each finding sent for repair.
    pub classifications: Vec<&'a FindingClassification>,
}

/// Actionable and unresolved findings passed to implementation for repair.
#[derive(JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
pub struct ImplementationRepairInput<'a> {
    /// Findings audited as supported and requiring correction.
    pub actionable_findings: Vec<&'a Finding>,
    /// Findings whose factual validity or actionability remains unresolved.
    pub ambiguous_findings: Vec<&'a Finding>,
    /// Complete audit rationale, including findings that require no repair.
    pub audit: &'a [FindingClassification],
}

/// Returns generated JSON Schema 2020-12 for a role response type.
pub fn schema<T: JsonSchema>() -> Result<String> {
    let generator = SchemaGenerator::new(SchemaSettings::draft2020_12());
    let schema: Schema = generator.into_root_schema_for::<T>();
    let mut value = serde_json::to_value(schema).map_err(|error| {
        SpecLoopError::Agent(format!("serialize generated response schema: {error}"))
    })?;
    close_flattened_schema(&mut value)?;
    serde_json::to_string_pretty(&value)
        .map_err(|error| SpecLoopError::Agent(format!("serialize response schema: {error}")))
}

fn close_flattened_schema(schema: &mut serde_json::Value) -> Result<()> {
    let object = schema.as_object_mut().ok_or_else(|| {
        SpecLoopError::Agent("generated response schema root is not an object".to_owned())
    })?;
    let base_properties = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .ok_or_else(|| {
            SpecLoopError::Agent("generated response schema has no envelope properties".to_owned())
        })?;
    let base_required = object
        .get("required")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .ok_or_else(|| {
            SpecLoopError::Agent(
                "generated response schema has no required envelope fields".to_owned(),
            )
        })?;
    let variants = object
        .get_mut("oneOf")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| {
            SpecLoopError::Agent("generated response schema has no status variants".to_owned())
        })?;
    for variant in variants {
        let variant = variant.as_object_mut().ok_or_else(|| {
            SpecLoopError::Agent("generated status schema is not an object".to_owned())
        })?;
        let properties = variant
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| {
                SpecLoopError::Agent("generated status schema has no properties".to_owned())
            })?;
        for (name, property) in &base_properties {
            properties.insert(name.clone(), property.clone());
        }
        for property in properties.values_mut() {
            if let Some(property) = property.as_object_mut()
                && property.get("maxItems").and_then(serde_json::Value::as_u64) == Some(0)
            {
                property.insert("minItems".to_owned(), serde_json::Value::from(0));
            }
        }
        let required = variant
            .get_mut("required")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| {
                SpecLoopError::Agent("generated status schema has no required fields".to_owned())
            })?;
        for field in &base_required {
            if !required.contains(field) {
                required.push(field.clone());
            }
        }
        variant.insert(
            "additionalProperties".to_owned(),
            serde_json::Value::Bool(false),
        );
    }
    object.remove("type");
    object.remove("properties");
    object.remove("required");
    object.remove("unevaluatedProperties");
    Ok(())
}

/// Deserializes one complete typed role response and validates common invariants.
pub fn parse<T: DeserializeOwned + ResponseFields + Serialize>(
    response: &str,
    role: &str,
) -> Result<T> {
    let parsed: T = serde_json::from_str(response)
        .map_err(|error| SpecLoopError::Agent(format!("invalid role JSON: {error}")))?;
    let supplied: serde_json::Value = serde_json::from_str(response)
        .map_err(|error| SpecLoopError::Agent(format!("invalid role JSON: {error}")))?;
    let supplied = supplied
        .as_object()
        .ok_or_else(|| SpecLoopError::Agent("role response must be one JSON object".to_owned()))?;
    let canonical = serde_json::to_value(&parsed)
        .map_err(|error| SpecLoopError::Agent(format!("serialize typed role response: {error}")))?;
    let canonical = canonical.as_object().ok_or_else(|| {
        SpecLoopError::Agent("typed role response did not serialize as an object".to_owned())
    })?;
    if supplied.keys().collect::<std::collections::BTreeSet<_>>()
        != canonical.keys().collect::<std::collections::BTreeSet<_>>()
    {
        return Err(SpecLoopError::Agent(
            "role response contains missing or unknown fields".to_owned(),
        ));
    }
    if parsed.protocol_version() != PROTOCOL_VERSION {
        return Err(SpecLoopError::Agent(format!(
            "protocol_version must be {PROTOCOL_VERSION}"
        )));
    }
    if parsed.role() != role {
        return Err(SpecLoopError::Agent(format!(
            "role must be {role:?}, got {:?}",
            parsed.role()
        )));
    }
    parsed.validate_response()?;
    Ok(parsed)
}

/// Common fields shared by all role response envelopes.
pub trait ResponseFields {
    /// Protocol version in response envelope.
    fn protocol_version(&self) -> u32;
    /// Exact role identifier in response envelope.
    fn role(&self) -> &str;
    /// Validates status and payload consistency after deserialization.
    fn validate_response(&self) -> Result<()>;
}

macro_rules! impl_response_fields {
    ($($name:ty => $validator:path),+ $(,)?) => {
        $(impl ResponseFields for $name {
            fn protocol_version(&self) -> u32 { self.protocol_version }
            fn role(&self) -> &str { &self.role }
            fn validate_response(&self) -> Result<()> { $validator(&self.result) }
        })+
    };
}

impl_response_fields!(
    PlanAuthorResponse => validate_readiness,
    ImplementationResponse => validate_readiness,
    PlanReviewResponse => validate_review,
    ImplementationReviewResponse => validate_review,
    PlanAuditResponse => validate_audit,
    ImplementationAuditResponse => validate_audit,
    PlanClarificationResponse => validate_evidence_result,
    ImplementationClarificationResponse => validate_evidence_result,
);

fn validate_readiness(result: &Readiness) -> Result<()> {
    if let Readiness::Blocked { reason } = result {
        validate_text(reason, "blocked reason")?;
    }
    Ok(())
}

fn validate_review(result: &ReviewResult) -> Result<()> {
    match result {
        ReviewResult::NoIssues { findings } if findings.is_empty() => Ok(()),
        ReviewResult::NoIssues { .. } => Err(SpecLoopError::Agent(
            "no_issues requires an empty findings array".to_owned(),
        )),
        ReviewResult::Issues { findings } if !findings.is_empty() => validate_findings(findings),
        ReviewResult::Issues { .. } => Err(SpecLoopError::Agent(
            "issues requires a non-empty findings array".to_owned(),
        )),
        ReviewResult::Blocked { reason } => validate_text(reason, "blocked reason"),
    }
}

fn validate_audit(result: &AuditResult) -> Result<()> {
    match result {
        AuditResult::Complete { classifications } if !classifications.is_empty() => {
            for item in classifications {
                if item.finding_id == 0 {
                    return Err(SpecLoopError::Agent(
                        "classification finding_id must be positive".to_owned(),
                    ));
                }
                validate_assessment(item)?;
                validate_text(&item.rationale, "classification rationale")?;
            }
            Ok(())
        }
        AuditResult::Complete { .. } => Err(SpecLoopError::Agent(
            "complete audit requires non-empty classifications".to_owned(),
        )),
        AuditResult::Blocked { reason } => validate_text(reason, "blocked reason"),
    }
}

fn validate_evidence_result(result: &EvidenceResult) -> Result<()> {
    match result {
        EvidenceResult::Complete { evidence } => {
            let mut seen = std::collections::BTreeSet::new();
            for entry in evidence {
                if entry.finding_id == 0 || !seen.insert(entry.finding_id) {
                    return Err(SpecLoopError::Agent(
                        "evidence finding IDs must be positive and unique".to_owned(),
                    ));
                }
                for citation in &entry.citations {
                    for (field, value) in [
                        ("citation path", &citation.path),
                        ("citation location", &citation.location),
                        ("citation excerpt", &citation.excerpt),
                        ("citation relevance", &citation.relevance),
                    ] {
                        validate_text(value, field)?;
                    }
                }
            }
            Ok(())
        }
        EvidenceResult::Blocked { reason } => validate_text(reason, "blocked reason"),
    }
}

/// Validates required free-text fields after JSON deserialization.
pub fn validate_text(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(SpecLoopError::Agent(format!("{field} cannot be empty")));
    }
    Ok(())
}

/// Ensures finding IDs are positive and sequential starting at one.
pub fn validate_findings(findings: &[Finding]) -> Result<()> {
    for (index, finding) in findings.iter().enumerate() {
        let expected = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or_else(|| SpecLoopError::Agent("finding count overflowed".to_owned()))?;
        if finding.id != expected {
            return Err(SpecLoopError::Agent(format!(
                "finding IDs must be sequential from 1; expected {expected}, got {}",
                finding.id
            )));
        }
        for (field, value) in [
            ("requirement", &finding.requirement),
            ("location", &finding.location),
            ("evidence", &finding.evidence),
            ("violation", &finding.violation),
            ("proposed_correction", &finding.proposed_correction),
        ] {
            validate_text(value, field)?;
        }
    }
    Ok(())
}

/// Validates one-per-finding audit classifications in input order.
pub fn validate_classifications(
    classifications: &[FindingClassification],
    findings: &[Finding],
) -> Result<()> {
    if classifications.len() != findings.len() {
        return Err(SpecLoopError::Agent(format!(
            "audit returned {} classifications for {} findings",
            classifications.len(),
            findings.len()
        )));
    }
    for (classification, finding) in classifications.iter().zip(findings) {
        if classification.finding_id != finding.id {
            return Err(SpecLoopError::Agent(format!(
                "audit classifications must match finding order; expected {}, got {}",
                finding.id, classification.finding_id
            )));
        }
        validate_assessment(classification)?;
        validate_text(&classification.rationale, "classification rationale")?;
    }
    Ok(())
}

fn validate_assessment(assessment: &FindingClassification) -> Result<()> {
    if assessment.classification == Classification::Unsupported
        && assessment.actionability != Actionability::NotRequired
    {
        return Err(SpecLoopError::Agent(
            "unsupported findings cannot require correction".to_owned(),
        ));
    }
    if assessment.classification == Classification::Ambiguous
        && assessment.actionability != Actionability::Ambiguous
    {
        return Err(SpecLoopError::Agent(
            "ambiguous findings require ambiguous actionability".to_owned(),
        ));
    }
    Ok(())
}

/// Validates evidence entries refer only to requested findings and occur once.
pub fn validate_evidence(evidence: &[FindingEvidence], requested_ids: &[u64]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for entry in evidence {
        if !requested_ids.contains(&entry.finding_id) {
            return Err(SpecLoopError::Agent(format!(
                "evidence refers to unrequested finding {}",
                entry.finding_id
            )));
        }
        if !seen.insert(entry.finding_id) {
            return Err(SpecLoopError::Agent(format!(
                "evidence repeats finding {}",
                entry.finding_id
            )));
        }
        for citation in &entry.citations {
            for (field, value) in [
                ("citation path", &citation.path),
                ("citation location", &citation.location),
                ("citation excerpt", &citation.excerpt),
                ("citation relevance", &citation.relevance),
            ] {
                validate_text(value, field)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Actionability, AuditResult, Classification, Finding, FindingClassification,
        FindingEvidence, ImplementationAuditResponse, ImplementationClarificationResponse,
        ImplementationResponse, ImplementationReviewResponse, PlanAuditResponse,
        PlanAuthorResponse, PlanClarificationResponse, PlanReviewResponse, ReviewResult, schema,
        validate_classifications, validate_evidence, validate_findings,
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
}
