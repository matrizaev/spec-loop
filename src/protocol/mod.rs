use std::collections::BTreeSet;

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub use self::blocker::BlockerAdjudication;
use crate::error::{Result, SpecLoopError};

mod blocker;
/// Mechanically projects canonical role schemas for provider-native structured output.
pub mod provider;

#[cfg(test)]
mod adjudication_tests;
#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod validation_tests;
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

const MAX_IMPLEMENTATION_SUMMARY_CHARS: usize = 500;

/// Typed implementation handoff, including a concise account of completed work.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
pub struct ImplementationResponse {
    /// Protocol version in response envelope.
    #[schemars(range(min = 1, max = 1))]
    pub protocol_version: u32,
    /// Exact role identifier in response envelope.
    #[schemars(regex(pattern = "^implementation$"))]
    pub role: String,
    /// Brief factual summary printed when workflow enters automated checks.
    #[schemars(length(min = 1, max = 500))]
    pub summary: String,
    /// Whether implementation is reviewable or blocked.
    #[serde(flatten)]
    pub result: Readiness,
}

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
    /// Adjudication of the supplied agent blocker, not of findings or overall correctness.
    Adjudicated {
        /// Strict outcome-specific evidence and blocker details.
        adjudication: BlockerAdjudication,
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

/// Typed audit task retains the original response without inferring a task from prose.
#[derive(JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
#[serde(tag = "audit_kind", rename_all = "snake_case")]
pub enum AuditInput<'a, R> {
    /// Classify only the supplied findings, retaining their review and grounding evidence.
    Findings {
        /// Original typed reviewer response.
        review: &'a R,
        /// Original findings in reviewer order.
        findings: &'a [Finding],
        /// Optional grounding evidence for a follow-up finding audit.
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence: Option<&'a AuditEvidenceInput>,
    },
    /// Assess the exact supplied blocker without manufacturing findings.
    AgentBlocker {
        /// Original typed blocked non-auditor response, including role and exact reason.
        response: &'a R,
    },
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

/// Serializes canonical schemas for contract tests; production serializes shared role values.
#[cfg(test)]
pub fn schema<T: JsonSchema>() -> Result<String> {
    serde_json::to_string_pretty(&schema_value::<T>()?)
        .map_err(|error| SpecLoopError::Agent(format!("serialize response schema: {error}")))
}

/// Generates one closed canonical schema value without a JSON text round trip.
pub fn schema_value<T: JsonSchema>() -> Result<serde_json::Value> {
    let generator = SchemaGenerator::new(SchemaSettings::draft2020_12());
    let schema: Schema = generator.into_root_schema_for::<T>();
    let mut value = serde_json::to_value(schema).map_err(|error| {
        SpecLoopError::Agent(format!("serialize generated response schema: {error}"))
    })?;
    close_flattened_schema(&mut value)?;
    Ok(value)
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
pub fn parse<T: DeserializeOwned + JsonSchema + ResponseFields>(
    response: &str,
    role: &str,
) -> Result<T> {
    let supplied: serde_json::Value = serde_json::from_str(response)
        .map_err(|error| SpecLoopError::Agent(format!("invalid role JSON: {error}")))?;
    if !supplied.is_object() {
        return Err(SpecLoopError::Agent(
            "role response must be one JSON object".to_owned(),
        ));
    }
    let generated_schema = schema_value::<T>()?;
    let mut field_errors = Vec::new();
    validate_response_shape(&supplied, &generated_schema, role, &mut field_errors)?;
    if !field_errors.is_empty() {
        return Err(SpecLoopError::Agent(field_errors.join("; ")));
    }
    let parsed: T = serde_json::from_value(supplied)
        .map_err(|error| SpecLoopError::Agent(format!("invalid role JSON: {error}")))?;
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

fn validate_response_shape(
    response: &serde_json::Value,
    schema: &serde_json::Value,
    role: &str,
    field_errors: &mut Vec<String>,
) -> Result<()> {
    let object = response
        .as_object()
        .ok_or_else(|| SpecLoopError::Agent("role response must be one JSON object".to_owned()))?;
    let branches = schema
        .get("oneOf")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            SpecLoopError::Agent("generated response schema has no status branches".to_owned())
        })?;
    let selected_branch = object
        .get("status")
        .and_then(|status| select_status_branch(status, branches));
    if let Some(branch) = selected_branch {
        validate_schema_value(response, branch, schema, role, "$", field_errors)
    } else {
        validate_unselected_status_shape(object, branches, role, field_errors)
    }
}

fn select_status_branch<'a>(
    status: &serde_json::Value,
    branches: &'a [serde_json::Value],
) -> Option<&'a serde_json::Value> {
    branches.iter().find(|branch| {
        let Some(status_schema) = branch
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .and_then(|properties| properties.get("status"))
        else {
            return false;
        };
        status_schema.get("const") == Some(status)
            || status_schema
                .get("enum")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|values| values.contains(status))
    })
}

fn validate_unselected_status_shape(
    response: &serde_json::Map<String, serde_json::Value>,
    branches: &[serde_json::Value],
    role: &str,
    field_errors: &mut Vec<String>,
) -> Result<()> {
    let mut allowed_fields = BTreeSet::new();
    let mut common_required: Option<BTreeSet<String>> = None;
    for branch in branches {
        let properties = branch
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                SpecLoopError::Agent(
                    "generated status schema has no envelope properties".to_owned(),
                )
            })?;
        allowed_fields.extend(properties.keys().cloned());
        let required = schema_string_set(branch.get("required"), "required fields")?;
        common_required = Some(match common_required {
            Some(common) => common.intersection(&required).cloned().collect(),
            None => required,
        });
    }
    let common_required = common_required.ok_or_else(|| {
        SpecLoopError::Agent("generated response schema has no status branches".to_owned())
    })?;
    append_field_error(
        response,
        &allowed_fields,
        &common_required,
        role,
        "$",
        true,
        field_errors,
    );
    Ok(())
}

fn validate_schema_value(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    root_schema: &serde_json::Value,
    role: &str,
    path: &str,
    field_errors: &mut Vec<String>,
) -> Result<()> {
    let schema = resolve_schema_reference(schema, root_schema, role, path)?;
    if value.is_object()
        && let Some(branches) = schema.get("oneOf").and_then(serde_json::Value::as_array)
    {
        let branch = branches
            .iter()
            .find(|branch| {
                branch
                    .get("properties")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|properties| {
                        properties.iter().any(|(name, property)| {
                            value.get(name).is_some_and(|tag| {
                                property.get("const") == Some(tag)
                                    || property
                                        .get("enum")
                                        .and_then(serde_json::Value::as_array)
                                        .is_some_and(|values| values.contains(tag))
                            })
                        })
                    })
            })
            .ok_or_else(|| {
                SpecLoopError::Agent(format!("invalid {role} discriminator at {path}"))
            })?;
        return validate_schema_value(value, branch, root_schema, role, path, field_errors);
    }
    if let Some(object) = value.as_object() {
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object);
        let allowed_fields = properties.map_or_else(BTreeSet::new, |properties| {
            properties.keys().cloned().collect()
        });
        let required_fields = schema_string_set(schema.get("required"), "required fields")?;
        let reject_unknown = schema
            .get("additionalProperties")
            .and_then(serde_json::Value::as_bool)
            == Some(false);
        append_field_error(
            object,
            &allowed_fields,
            &required_fields,
            role,
            path,
            reject_unknown,
            field_errors,
        );
        if let Some(properties) = properties {
            let mut names = properties.keys().collect::<Vec<_>>();
            names.sort_unstable();
            for name in names {
                if let (Some(child_value), Some(child_schema)) =
                    (object.get(name), properties.get(name))
                {
                    let child_path = format!("{path}.{name}");
                    validate_schema_value(
                        child_value,
                        child_schema,
                        root_schema,
                        role,
                        &child_path,
                        field_errors,
                    )?;
                }
            }
        }
    } else if let (Some(array), Some(item_schema)) = (
        value.as_array(),
        schema.get("items").filter(|items| items.is_object()),
    ) {
        for (index, item) in array.iter().enumerate() {
            let item_path = format!("{path}[{index}]");
            validate_schema_value(
                item,
                item_schema,
                root_schema,
                role,
                &item_path,
                field_errors,
            )?;
        }
    }
    Ok(())
}

fn resolve_schema_reference<'a>(
    schema: &'a serde_json::Value,
    root_schema: &'a serde_json::Value,
    role: &str,
    path: &str,
) -> Result<&'a serde_json::Value> {
    let Some(reference) = schema.get("$ref").and_then(serde_json::Value::as_str) else {
        return Ok(schema);
    };
    if !reference.starts_with("#/") {
        return Err(SpecLoopError::Agent(format!(
            "generated {role} schema has unsupported reference {reference:?} at {path}"
        )));
    }
    let pointer = reference.strip_prefix('#').ok_or_else(|| {
        SpecLoopError::Agent(format!(
            "generated {role} schema has unsupported reference {reference:?} at {path}"
        ))
    })?;
    root_schema.pointer(pointer).ok_or_else(|| {
        SpecLoopError::Agent(format!(
            "generated {role} schema reference {reference:?} does not resolve at {path}"
        ))
    })
}

fn schema_string_set(value: Option<&serde_json::Value>, field: &str) -> Result<BTreeSet<String>> {
    let Some(value) = value else {
        return Ok(BTreeSet::new());
    };
    let values = value.as_array().ok_or_else(|| {
        SpecLoopError::Agent(format!("generated response schema {field} is not an array"))
    })?;
    let mut strings = BTreeSet::new();
    for value in values {
        let value = value.as_str().ok_or_else(|| {
            SpecLoopError::Agent(format!(
                "generated response schema {field} contains a non-string"
            ))
        })?;
        strings.insert(value.to_owned());
    }
    Ok(strings)
}

fn append_field_error(
    object: &serde_json::Map<String, serde_json::Value>,
    allowed_fields: &BTreeSet<String>,
    required_fields: &BTreeSet<String>,
    role: &str,
    path: &str,
    reject_unknown: bool,
    field_errors: &mut Vec<String>,
) {
    let missing = required_fields
        .iter()
        .filter(|field| !object.contains_key(field.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let mut unknown = if reject_unknown {
        object
            .keys()
            .filter(|field| !allowed_fields.contains(field.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    unknown.sort_unstable();
    let mut details = Vec::new();
    if !missing.is_empty() {
        details.push(format!("missing fields {missing:?}"));
    }
    if !unknown.is_empty() {
        details.push(format!("unknown fields {unknown:?}"));
    }
    if !details.is_empty() {
        field_errors.push(format!(
            "{role} response object at {path}: {}",
            details.join("; ")
        ));
    }
}

/// Common fields shared by all role response envelopes.
pub trait ResponseFields {
    /// Protocol version in response envelope.
    fn protocol_version(&self) -> u32;
    /// Exact role identifier in response envelope.
    fn role(&self) -> &str;
    /// Validates status and payload consistency after deserialization.
    fn validate_response(&self) -> Result<()>;
    /// Returns the exact typed blocker claim, without interpreting its prose.
    fn blocked_reason(&self) -> Option<&str>;
}

macro_rules! impl_response_fields {
    ($($name:ty => $result:ident, $validator:path),+ $(,)?) => {
        $(impl ResponseFields for $name {
            fn protocol_version(&self) -> u32 { self.protocol_version }
            fn role(&self) -> &str { &self.role }
            fn validate_response(&self) -> Result<()> { $validator(&self.result) }
            fn blocked_reason(&self) -> Option<&str> {
                match &self.result {
                    $result::Blocked { reason } => Some(reason),
                    _ => None,
                }
            }
        })+
    };
}

impl_response_fields!(
    PlanAuthorResponse => Readiness, validate_readiness,
    PlanReviewResponse => ReviewResult, validate_review_result,
    ImplementationReviewResponse => ReviewResult, validate_review_result,
    PlanAuditResponse => AuditResult, validate_audit,
    ImplementationAuditResponse => AuditResult, validate_audit,
    PlanClarificationResponse => EvidenceResult, validate_evidence_result,
    ImplementationClarificationResponse => EvidenceResult, validate_evidence_result,
);

impl ResponseFields for ImplementationResponse {
    fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    fn role(&self) -> &str {
        &self.role
    }

    fn validate_response(&self) -> Result<()> {
        validate_readiness(&self.result)?;
        validate_implementation_summary(&self.summary)
    }

    fn blocked_reason(&self) -> Option<&str> {
        match &self.result {
            Readiness::Blocked { reason } => Some(reason),
            Readiness::ReadyForReview => None,
        }
    }
}

fn validate_implementation_summary(summary: &str) -> Result<()> {
    validate_text(summary, "implementation summary")?;
    if summary.chars().count() > MAX_IMPLEMENTATION_SUMMARY_CHARS {
        return Err(SpecLoopError::Agent(format!(
            "implementation summary cannot exceed {MAX_IMPLEMENTATION_SUMMARY_CHARS} characters"
        )));
    }
    if summary.chars().any(char::is_control) {
        return Err(SpecLoopError::Agent(
            "implementation summary cannot contain control characters".to_owned(),
        ));
    }
    Ok(())
}

fn validate_readiness(result: &Readiness) -> Result<()> {
    if let Readiness::Blocked { reason } = result {
        validate_text(reason, "blocked reason")?;
    }
    Ok(())
}

/// Validates status-specific review findings and blocked reasons for every backend.
pub fn validate_review_result(result: &ReviewResult) -> Result<()> {
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
            }
            Ok(())
        }
        AuditResult::Complete { .. } => Err(SpecLoopError::Agent(
            "complete audit requires non-empty classifications".to_owned(),
        )),
        AuditResult::Adjudicated { adjudication } => adjudication.validate(),
        AuditResult::Blocked { reason } => validate_text(reason, "blocked reason"),
    }
}

fn validate_evidence_result(result: &EvidenceResult) -> Result<()> {
    match result {
        EvidenceResult::Complete { evidence } => validate_evidence_entries(evidence, None),
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
    validate_classification_count(classifications, findings)?;
    for (classification, finding) in classifications.iter().zip(findings) {
        validate_classification_id(classification.finding_id, finding.id)?;
        validate_assessment(classification)?;
    }
    Ok(())
}

/// Validates only input-dependent count and order after intrinsic typed parsing.
pub fn validate_classification_order(
    classifications: &[FindingClassification],
    findings: &[Finding],
) -> Result<()> {
    validate_classification_count(classifications, findings)?;
    for (classification, finding) in classifications.iter().zip(findings) {
        validate_classification_id(classification.finding_id, finding.id)?;
    }
    Ok(())
}

fn validate_classification_count(
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
    Ok(())
}

fn validate_classification_id(actual: u64, expected: u64) -> Result<()> {
    if actual != expected {
        return Err(SpecLoopError::Agent(format!(
            "audit classifications must match finding order; expected {expected}, got {actual}"
        )));
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
    validate_text(&assessment.rationale, "classification rationale")
}

/// Retains standalone unvalidated-evidence guarantees for contract tests.
#[cfg(test)]
pub fn validate_evidence(evidence: &[FindingEvidence], requested_ids: &[u64]) -> Result<()> {
    validate_evidence_entries(evidence, Some(requested_ids))
}

/// Validates only request membership after intrinsic typed parsing.
pub fn validate_requested_evidence(
    evidence: &[FindingEvidence],
    requested_ids: &[u64],
) -> Result<()> {
    for entry in evidence {
        validate_requested_finding(entry.finding_id, requested_ids)?;
    }
    Ok(())
}

fn validate_requested_finding(id: u64, requested_ids: &[u64]) -> Result<()> {
    if !requested_ids.contains(&id) {
        return Err(SpecLoopError::Agent(format!(
            "evidence refers to unrequested finding {id}"
        )));
    }
    Ok(())
}

fn validate_evidence_entries(
    evidence: &[FindingEvidence],
    requested_ids: Option<&[u64]>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for entry in evidence {
        if let Some(requested_ids) = requested_ids {
            validate_requested_finding(entry.finding_id, requested_ids)?;
            if !seen.insert(entry.finding_id) {
                return Err(SpecLoopError::Agent(format!(
                    "evidence repeats finding {}",
                    entry.finding_id
                )));
            }
        } else if entry.finding_id == 0 || !seen.insert(entry.finding_id) {
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
