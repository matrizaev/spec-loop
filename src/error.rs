use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

/// Errors produced by configuration, process execution, and workflow guards.
#[derive(Debug, thiserror::Error)]
pub enum SpecLoopError {
    /// A filesystem operation failed.
    #[error("{operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A child process could not be started or returned a failure status.
    #[error("{0}")]
    Process(String),
    /// Profile content or values are invalid.
    #[error("configuration error: {0}")]
    Config(String),
    /// A required repository invariant failed.
    #[error("workflow stopped: {0}")]
    Workflow(String),
    /// A child process exceeded its configured timeout.
    #[error("timed out: {0}")]
    Timeout(String),
    /// A child process was interrupted by the user.
    #[error("interrupted; changes and logs were preserved")]
    Interrupted,
    /// The agent returned malformed or incomplete output.
    #[error("agent response error: {0}")]
    Agent(String),
}

pub type Result<T> = std::result::Result<T, SpecLoopError>;

pub fn io_error(
    operation: &'static str,
    path: impl Into<PathBuf>,
    source: io::Error,
) -> SpecLoopError {
    SpecLoopError::Io {
        operation,
        path: path.into(),
        source,
    }
}

pub fn process_error(program: &str, status: ExitStatus, stderr: &[u8]) -> SpecLoopError {
    let diagnostic = String::from_utf8_lossy(stderr);
    SpecLoopError::Process(format!(
        "{program} exited with {status}; stderr: {}",
        diagnostic.trim()
    ))
}
