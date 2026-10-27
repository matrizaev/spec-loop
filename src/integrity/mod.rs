use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Result, SpecLoopError, io_error};

#[cfg(test)]
mod tests;
#[derive(Clone)]
pub struct InputIntegrity {
    selected_spec: HashedFile,
    selected_plan: HashedFile,
    plan_author_may_update: bool,
}

#[derive(Clone)]
struct HashedFile {
    path: PathBuf,
    digest: Option<[u8; 32]>,
    label: &'static str,
}

impl InputIntegrity {
    pub fn capture(
        root: &Path,
        spec_path: &Path,
        plan_path: &Path,
        plan_author_may_update: bool,
    ) -> Result<Self> {
        Ok(Self {
            selected_spec: HashedFile::capture(root, spec_path, false, "selected spec")?,
            selected_plan: HashedFile::capture(
                root,
                plan_path,
                !plan_author_may_update,
                "selected plan",
            )?,
            plan_author_may_update,
        })
    }

    pub fn verify(&self, root: &Path) -> Result<()> {
        self.selected_spec.verify(root)?;
        self.selected_plan.verify(root)
    }

    /// Verifies only the selected plan after finish actions may update the selected spec.
    pub fn verify_plan(&self, root: &Path) -> Result<()> {
        self.selected_plan.verify(root)
    }

    pub fn agent_finished(&mut self, root: &Path, is_plan_author: bool) -> Result<()> {
        self.selected_spec.verify(root)?;
        if self.plan_author_may_update && is_plan_author {
            self.selected_plan.refresh(root)
        } else {
            self.selected_plan.verify(root)
        }
    }

    pub fn relocate_spec(&mut self, root: &Path, new_path: &Path) -> Result<()> {
        self.selected_spec.path = new_path.to_path_buf();
        self.selected_spec.verify(root)
    }
}

impl HashedFile {
    fn capture(root: &Path, path: &Path, required: bool, label: &'static str) -> Result<Self> {
        let digest = Self::read_digest(root, path)?;
        if required && digest.is_none() {
            return Err(SpecLoopError::Workflow(format!(
                "cannot track missing {label}: {}",
                path.display()
            )));
        }
        Ok(Self {
            path: path.to_path_buf(),
            digest,
            label,
        })
    }

    fn verify(&self, root: &Path) -> Result<()> {
        if self.digest != Self::read_digest(root, &self.path)? {
            return Err(SpecLoopError::Workflow(format!(
                "protected {} content changed during workflow: {}; changes preserved",
                self.label,
                self.path.display()
            )));
        }
        Ok(())
    }

    fn refresh(&mut self, root: &Path) -> Result<()> {
        self.digest = Self::read_digest(root, &self.path)?;
        Ok(())
    }

    fn read_digest(root: &Path, path: &Path) -> Result<Option<[u8; 32]>> {
        let absolute = root.join(path);
        match fs::read(&absolute) {
            Ok(contents) => Ok(Some(Sha256::digest(contents).into())),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(io_error("read protected workflow input", absolute, source)),
        }
    }
}
