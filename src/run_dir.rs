use std::fs;
use std::path::{Path, PathBuf};

use tempfile::Builder;

use crate::error::{Result, SpecLoopError, io_error};

pub struct RunDir {
    pub root: PathBuf,
}

impl RunDir {
    pub fn create(repository_root: &Path) -> Result<Self> {
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os("TMPDIR") {
            candidates.push(PathBuf::from(path));
        }
        candidates.push(PathBuf::from("/var/tmp"));
        candidates.push(std::env::temp_dir());
        let mut errors = Vec::new();
        for base in candidates {
            if !base.is_dir() {
                continue;
            }
            let directory = match Builder::new().prefix("spec-loop-").tempdir_in(&base) {
                Ok(directory) => directory,
                Err(error) => {
                    errors.push(format!("{}: {error}", base.display()));
                    continue;
                }
            };
            let canonical = fs::canonicalize(directory.path()).map_err(|source| {
                io_error("canonicalize run directory", directory.path(), source)
            })?;
            if canonical.starts_with(repository_root) {
                drop(directory);
                continue;
            }
            return Ok(Self {
                root: directory.keep(),
            });
        }
        if errors.is_empty() {
            return Err(SpecLoopError::Workflow(
                "could not create temporary run directory outside repository".to_owned(),
            ));
        }
        Err(SpecLoopError::Workflow(format!(
            "could not create temporary run directory outside repository: {}",
            errors.join("; ")
        )))
    }

    pub fn log_path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn write_log(&self, name: &str, contents: &[u8]) -> Result<PathBuf> {
        let path = self.log_path(name);
        fs::write(&path, contents).map_err(|source| io_error("write run log", &path, source))?;
        Ok(path)
    }
}
