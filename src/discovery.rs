use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;

use crate::config::{OrderingPolicy, Profile, SelectionPolicy};
use crate::error::{Result, SpecLoopError, io_error};

/// A validated active specification candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpecCandidate {
    pub id: String,
    pub path: PathBuf,
    sort_key: SortKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SortKey {
    Lexical(String),
    Numeric(u128, String),
}

/// Selects requested or next eligible spec using filesystem paths only.
pub fn select_spec(
    root: &Path,
    profile: &Profile,
    requested_id: Option<&str>,
    completed_ids: &BTreeSet<String>,
) -> Result<Option<SpecCandidate>> {
    let candidates = discover_specs(root, profile)?;
    let requested = requested_id.and_then(|id| candidates.iter().find(|item| item.id == id));
    if let Some(requested_id) = requested_id
        && requested.is_none()
    {
        return Err(SpecLoopError::Workflow(format!(
            "requested spec {requested_id:?} is not eligible"
        )));
    }
    let next = candidates
        .iter()
        .find(|candidate| !completed_ids.contains(&candidate.id));
    if let Some(candidate) = requested {
        if profile.specs.selection == SelectionPolicy::Next
            && next.is_some_and(|next| candidate.id != next.id)
        {
            let expected = next.map_or("none", |next| next.id.as_str());
            return Err(SpecLoopError::Workflow(format!(
                "requested spec {} is not next; expected {expected}",
                candidate.id
            )));
        }
        return Ok(Some(candidate.clone()));
    }
    Ok(next.cloned())
}

/// Discovers all active specs from matching filesystem paths, independent of Git.
pub fn discover_specs(root: &Path, profile: &Profile) -> Result<Vec<SpecCandidate>> {
    let active = compile_globs(&profile.specs.active_globs)?;
    let id_regex = Regex::new(&profile.specs.id_regex)
        .map_err(|error| SpecLoopError::Config(format!("specs.id_regex: {error}")))?;
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths)?;
    let mut by_id: BTreeMap<String, SpecCandidate> = BTreeMap::new();
    for absolute in paths {
        let relative = absolute.strip_prefix(root).map_err(|_| {
            SpecLoopError::Workflow(format!(
                "discovered path is outside project root: {}",
                absolute.display()
            ))
        })?;
        let Some(relative_text) = relative.to_str() else {
            continue;
        };
        if !active.is_match(relative_text) {
            continue;
        }
        let metadata = fs::metadata(&absolute)
            .map_err(|source| io_error("inspect active spec path", &absolute, source))?;
        if !metadata.is_file() {
            return Err(SpecLoopError::Config(format!(
                "spec path is not a regular file: {relative_text}"
            )));
        }
        let candidate = spec_candidate(
            relative.to_path_buf(),
            relative_text,
            &id_regex,
            profile.specs.ordering,
        )?;
        if let Some(previous) = by_id.get(&candidate.id) {
            return Err(SpecLoopError::Config(format!(
                "duplicate spec ID {:?} at {} and {}",
                candidate.id,
                previous.path.display(),
                candidate.path.display()
            )));
        }
        by_id.insert(candidate.id.clone(), candidate);
    }
    let mut candidates: Vec<_> = by_id.into_values().collect();
    candidates.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));
    Ok(candidates)
}

fn collect_files(root: &Path, directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(directory)
        .map_err(|source| io_error("read project directory", directory, source))?;
    for entry in entries {
        let entry =
            entry.map_err(|source| io_error("read project directory entry", directory, source))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| io_error("inspect project directory entry", &path, source))?;
        if file_type.is_dir() {
            collect_files(root, &path, paths)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            if path.strip_prefix(root).is_err() {
                return Err(SpecLoopError::Workflow(format!(
                    "discovered path is outside project root: {}",
                    path.display()
                )));
            }
            paths.push(path);
        }
    }
    Ok(())
}

fn spec_candidate(
    path: PathBuf,
    path_text: &str,
    id_regex: &Regex,
    ordering: OrderingPolicy,
) -> Result<SpecCandidate> {
    let captures = id_regex.captures(path_text).ok_or_else(|| {
        SpecLoopError::Config(format!(
            "spec path matches configured glob but not ID regex: {path_text}"
        ))
    })?;
    let id = captures
        .name("id")
        .map(|capture| capture.as_str().to_owned())
        .ok_or_else(|| SpecLoopError::Config("spec ID capture `id` is missing".to_owned()))?;
    Ok(SpecCandidate {
        sort_key: sort_key(&id, ordering)?,
        id,
        path,
    })
}

fn compile_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| SpecLoopError::Config(format!("invalid glob {pattern:?}: {error}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|error| SpecLoopError::Config(format!("invalid glob set: {error}")))
}

fn sort_key(id: &str, ordering: OrderingPolicy) -> Result<SortKey> {
    match ordering {
        OrderingPolicy::Lexical => Ok(SortKey::Lexical(id.to_owned())),
        OrderingPolicy::Numeric => {
            let number = id.parse::<u128>().map_err(|error| {
                SpecLoopError::Config(format!(
                    "spec ID {id:?} is not an unsigned numeric ID: {error}"
                ))
            })?;
            Ok(SortKey::Numeric(number, id.to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use tempfile::TempDir;

    use super::{discover_specs, select_spec, sort_key};
    use crate::config::{OrderingPolicy, Profile};

    fn profile() -> Profile {
        toml::from_str(
            "schema_version = 2\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '^specs/(?P<id>[0-9]+)(?:-[^/]*)?\\.md$'\n",
        )
        .expect("parse test profile")
    }

    #[test]
    fn discovers_untracked_and_ignored_files_without_git() {
        let temporary = TempDir::new().expect("temporary project");
        fs::create_dir_all(temporary.path().join("specs/nested")).expect("spec directories");
        fs::write(temporary.path().join(".gitignore"), "specs/002.md\n").expect("ignore file");
        fs::write(temporary.path().join("specs/001.md"), "one").expect("spec one");
        fs::write(temporary.path().join("specs/002.md"), "two").expect("spec two");
        fs::write(temporary.path().join("specs/nested/003.md"), "nested").expect("nested");

        let candidates = discover_specs(temporary.path(), &profile()).expect("discover specs");

        assert_eq!(
            candidates
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["001", "002"]
        );
    }

    #[test]
    fn rejects_duplicate_ids_and_nonmatching_configured_paths() {
        let temporary = TempDir::new().expect("temporary project");
        fs::create_dir_all(temporary.path().join("specs")).expect("spec directory");
        fs::write(temporary.path().join("specs/001.md"), "one").expect("spec one");
        fs::write(temporary.path().join("specs/001-copy.md"), "duplicate").expect("duplicate");
        let error = discover_specs(temporary.path(), &profile()).expect_err("duplicate ID");
        assert!(error.to_string().contains("duplicate spec ID"));

        fs::remove_file(temporary.path().join("specs/001-copy.md")).expect("remove duplicate");
        fs::write(temporary.path().join("outside.md"), "outside").expect("outside file");
        assert_eq!(
            discover_specs(temporary.path(), &profile())
                .expect("discover")
                .len(),
            1
        );
    }

    #[test]
    fn request_and_continue_selection_obey_profile_ordering() {
        let temporary = TempDir::new().expect("temporary project");
        fs::create_dir_all(temporary.path().join("specs")).expect("spec directory");
        fs::write(temporary.path().join("specs/001.md"), "one").expect("spec one");
        fs::write(temporary.path().join("specs/002.md"), "two").expect("spec two");
        let selected = select_spec(temporary.path(), &profile(), Some("001"), &BTreeSet::new())
            .expect("select")
            .expect("selected");
        assert_eq!(selected.id, "001");
        let completed = BTreeSet::from(["001".to_owned()]);
        assert_eq!(
            select_spec(temporary.path(), &profile(), None, &completed)
                .expect("next")
                .map(|item| item.id.as_str().to_owned()),
            Some("002".to_owned())
        );
        assert!(select_spec(temporary.path(), &profile(), Some("002"), &BTreeSet::new()).is_err());
    }

    #[test]
    fn numeric_order_compares_numbers_then_original_text() {
        assert!(sort_key("2", OrderingPolicy::Numeric).is_ok_and(|left| {
            sort_key("10", OrderingPolicy::Numeric).is_ok_and(|right| left < right)
        }));
        assert!(sort_key("02", OrderingPolicy::Numeric).is_ok_and(|left| {
            sort_key("2", OrderingPolicy::Numeric).is_ok_and(|right| left < right)
        }));
    }
}
