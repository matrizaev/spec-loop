use super::{
    Actionability, Citation, Classification, Finding, FindingClassification, FindingEvidence,
    validate_classification_order, validate_classifications, validate_evidence,
    validate_requested_evidence,
};

fn finding(id: u64) -> Finding {
    Finding {
        id,
        requirement: "required".to_owned(),
        location: "location".to_owned(),
        evidence: "evidence".to_owned(),
        violation: "violation".to_owned(),
        proposed_correction: "correction".to_owned(),
    }
}

fn classification(id: u64, rationale: &str) -> FindingClassification {
    FindingClassification {
        finding_id: id,
        classification: Classification::Supported,
        actionability: Actionability::Required,
        rationale: rationale.to_owned(),
    }
}

#[test]
fn standalone_audit_validation_retains_count_and_per_entry_error_precedence() {
    let findings = [finding(1), finding(2)];
    let classifications = [classification(1, ""), classification(3, "valid")];
    let error = validate_classifications(&classifications, &findings).expect_err("empty rationale");
    assert_eq!(
        error.to_string(),
        "agent response error: classification rationale cannot be empty"
    );
    let error = validate_classification_order(&classifications, &findings)
        .expect_err("input-only order check");
    assert!(error.to_string().contains("expected 2, got 3"));
    let error =
        validate_classifications(&classifications, &findings[..1]).expect_err("count first");
    assert!(
        error
            .to_string()
            .contains("audit returned 2 classifications for 1 findings")
    );
    let error = validate_classifications(&[classification(2, "")], &findings[..1])
        .expect_err("order before rationale within one entry");
    assert!(error.to_string().contains("expected 1, got 2"));
    validate_classifications(&[], &[]).expect("standalone empty input remains valid");
}

#[test]
fn standalone_evidence_validation_retains_citation_and_membership_guarantees() {
    let citation = Citation {
        path: " ".to_owned(),
        location: "line 1".to_owned(),
        excerpt: "source".to_owned(),
        relevance: "supports finding".to_owned(),
    };
    let evidence = [
        FindingEvidence {
            finding_id: 1,
            citations: vec![citation],
        },
        FindingEvidence {
            finding_id: 2,
            citations: Vec::new(),
        },
    ];
    let error = validate_evidence(&evidence, &[1]).expect_err("first citation before later ID");
    assert_eq!(
        error.to_string(),
        "agent response error: citation path cannot be empty"
    );
    let error = validate_requested_evidence(&evidence, &[1]).expect_err("input-only membership");
    assert!(error.to_string().contains("unrequested finding 2"));
    let repeated = [
        FindingEvidence {
            finding_id: 1,
            citations: Vec::new(),
        },
        FindingEvidence {
            finding_id: 1,
            citations: Vec::new(),
        },
    ];
    assert_eq!(
        validate_evidence(&repeated, &[1])
            .expect_err("duplicate")
            .to_string(),
        "agent response error: evidence repeats finding 1"
    );
    validate_evidence(
        &[FindingEvidence {
            finding_id: 0,
            citations: Vec::new(),
        }],
        &[0],
    )
    .expect("standalone API retains caller-supplied ID semantics");
}
