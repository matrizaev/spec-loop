use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::validate_text;
use crate::error::Result;

/// An auditor's assessment of a non-auditor blocker, never workflow approval.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub enum BlockerAdjudication {
    /// Evidence establishes an unavailable prerequisite required for compliant progress.
    Confirmed {
        /// Evidence-backed explanation of the decision.
        #[schemars(length(min = 1))]
        rationale: String,
        /// Evidence and its provenance, including relevant spec or repository citations.
        #[schemars(length(min = 1))]
        evidence: String,
        /// Requirement that cannot currently be met.
        #[schemars(length(min = 1))]
        requirement: String,
        /// External prerequisite that is genuinely unavailable.
        #[schemars(length(min = 1))]
        prerequisite: String,
        /// Why available tools cannot obtain or resolve the prerequisite.
        #[schemars(length(min = 1))]
        unavailable_reason: String,
        /// Specific user input, access, or approval needed to unblock progress.
        #[schemars(length(min = 1))]
        required_input: String,
    },
    /// Available evidence disproves the claimed blocker; the reviewer must respond again.
    Unsupported {
        /// Evidence-backed explanation of why the blocker is unsupported.
        #[schemars(length(min = 1))]
        rationale: String,
        /// Evidence and its provenance; the supplied review can itself be evidence.
        #[schemars(length(min = 1))]
        evidence: String,
    },
    /// Evidence cannot resolve the blocker; progress stops pending user input.
    Ambiguous {
        /// Explanation of the unresolved evidence.
        #[schemars(length(min = 1))]
        rationale: String,
        /// Evidence and its provenance.
        #[schemars(length(min = 1))]
        evidence: String,
        /// Precise unresolved question for the user.
        #[schemars(length(min = 1))]
        question: String,
    },
}

impl BlockerAdjudication {
    /// Validates the text required by this state; shape validation belongs to the typed parser.
    pub(super) fn validate(&self) -> Result<()> {
        let (rationale, evidence) = self.explanation();
        validate_text(rationale, "blocker adjudication rationale")?;
        validate_text(evidence, "blocker adjudication evidence")?;
        match self {
            Self::Confirmed {
                requirement,
                prerequisite,
                unavailable_reason,
                required_input,
                ..
            } => {
                for (field, value) in [
                    ("blocker requirement", requirement),
                    ("blocker prerequisite", prerequisite),
                    ("blocker unavailable_reason", unavailable_reason),
                    ("blocker required_input", required_input),
                ] {
                    validate_text(value, field)?;
                }
                Ok(())
            }
            Self::Ambiguous { question, .. } => validate_text(question, "blocker question"),
            Self::Unsupported { .. } => Ok(()),
        }
    }

    /// Returns common evidence-bearing fields without erasing the outcome-specific requirements.
    pub(crate) fn explanation(&self) -> (&str, &str) {
        match self {
            Self::Confirmed {
                rationale,
                evidence,
                ..
            }
            | Self::Unsupported {
                rationale,
                evidence,
            }
            | Self::Ambiguous {
                rationale,
                evidence,
                ..
            } => (rationale, evidence),
        }
    }

    /// Stable human-readable outcome label.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Confirmed { .. } => "confirmed",
            Self::Unsupported { .. } => "unsupported",
            Self::Ambiguous { .. } => "ambiguous",
        }
    }
}
