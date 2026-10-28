use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, SpecLoopError, io_error};

#[cfg(test)]
mod tests;
const SPEC_ID_PLACEHOLDER: &str = "{spec_id}";
const SPEC_PATH_PLACEHOLDER: &str = "{spec_path}";
const PLAN_PATH_PLACEHOLDER: &str = "{plan_path}";

pub fn resolve_executable(root: &Path, executable: &Path, field: &str) -> Result<PathBuf> {
    let has_path_component = executable.is_absolute()
        || executable
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty());
    let candidates = if has_path_component {
        vec![if executable.is_absolute() {
            executable.to_path_buf()
        } else {
            root.join(executable)
        }]
    } else {
        let search_path = std::env::var_os("PATH").ok_or_else(|| {
            SpecLoopError::Config(format!(
                "PATH is unavailable while resolving {field} executable"
            ))
        })?;
        std::env::split_paths(&search_path)
            .map(|directory| {
                if directory.is_absolute() {
                    directory.join(executable)
                } else {
                    root.join(directory).join(executable)
                }
            })
            .collect()
    };
    candidates
        .into_iter()
        .find(|path| is_executable_file(path))
        .ok_or_else(|| {
            SpecLoopError::Config(format!(
                "{field} executable was not found or is not executable: {}",
                executable.display()
            ))
        })
}

pub fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn validate_regular_file(root: &Path, relative: &Path, field: &str) -> Result<()> {
    let path = root.join(relative);
    let metadata = fs::metadata(&path).map_err(|source| {
        io_error(
            "inspect configured path",
            format!("{field}: {}", path.display()),
            source,
        )
    })?;
    if !metadata.is_file() {
        return Err(SpecLoopError::Config(format!(
            "{field} is not a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

pub fn expand_template(
    template: &str,
    spec_id: &str,
    spec_path: &Path,
    plan_path: Option<&Path>,
) -> String {
    let spec_path = spec_path.to_string_lossy();
    let plan_path = plan_path.map(Path::to_string_lossy);
    let mut expanded = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(opening) = remaining.find('{') {
        let Some(prefix) = remaining.get(..opening) else {
            expanded.push_str(remaining);
            return expanded;
        };
        expanded.push_str(prefix);
        let Some(after_opening) = remaining.get(opening..) else {
            return expanded;
        };
        let Some(closing) = after_opening.find('}') else {
            expanded.push_str(after_opening);
            return expanded;
        };
        let Some(token) = after_opening.get(..=closing) else {
            return expanded;
        };
        let replacement = match token {
            SPEC_ID_PLACEHOLDER => Some(spec_id),
            SPEC_PATH_PLACEHOLDER => Some(spec_path.as_ref()),
            PLAN_PATH_PLACEHOLDER => plan_path.as_ref().map(std::borrow::Cow::as_ref),
            _ => None,
        };
        if let Some(replacement) = replacement {
            expanded.push_str(replacement);
        } else {
            expanded.push_str(token);
        }
        let Some(tail) = after_opening.get(closing + 1..) else {
            return expanded;
        };
        remaining = tail;
    }
    expanded.push_str(remaining);
    expanded
}

pub fn path_to_utf8(path: &Path, field: &str) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        SpecLoopError::Config(format!("{field} is not valid UTF-8: {}", path.display()))
    })
}
