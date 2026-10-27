use super::{BackendFailure, BackendFailureKind};
use crate::error::SpecLoopError;

#[test]
fn retryable_backend_failure_preserves_original_error() {
    let retryable = BackendFailure::retryable(SpecLoopError::Agent("retry".to_owned()));
    assert_eq!(retryable.kind(), BackendFailureKind::Retryable);
    assert!(matches!(retryable.into_error(), SpecLoopError::Agent(_)));
}

#[test]
fn backend_interruption_remains_interruption() {
    let retryable = BackendFailure::retryable(SpecLoopError::Interrupted);
    assert!(matches!(retryable.into_error(), SpecLoopError::Interrupted));
}
