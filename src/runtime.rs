#![forbid(unsafe_code)]

use crate::error::{Result, SpecLoopError};

/// Runs one async operation on an owned current-thread runtime at a synchronous boundary.
pub fn block_on<T>(future: impl std::future::Future<Output = T>) -> Result<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| SpecLoopError::Workflow(format!("construct async runtime: {error}")))?;
    Ok(runtime.block_on(future))
}
