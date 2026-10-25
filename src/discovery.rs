use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;

use crate::config::{OrderingPolicy, Profile, SelectionPolicy};
use crate::error::{Result, SpecLoopError, io_error};
use crate::git;

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

struct MissingSpecCandidate {
    id: String,
    path: PathBuf,
}

// Missing indexed paths indicate deleted required specs; disappearing untracked paths are races.
#[derive(Clone, Copy)]
enum PathOrigin {
    Indexed,
    Untracked,
}

pub fn select_spec(
    root: &Path,
    profile: &Profile,
    requested_id: Option<&str>,
    completed_ids: &BTreeSet<String>,
) -> Result<Option<SpecCandidate>> {
    let (candidates, missing) = discover_specs_with_missing(root, profile)?;
    let requested_candidate =
        requested_id.and_then(|id| candidates.iter().find(|candidate| candidate.id == id));
    if requested_id.is_some() && requested_candidate.is_none() {
        if let Some(candidate) =
            requested_id.and_then(|id| missing.iter().find(|candidate| candidate.id == id))
        {
            return Err(missing_selected_spec(&candidate.path));
        }
        return Err(SpecLoopError::Workflow(format!(
            "requested spec {requested_id:?} is not eligible"
        )));
    }

    let available: Vec<&SpecCandidate> = candidates
        .iter()
        .filter(|candidate| !completed_ids.contains(&candidate.id))
        .collect();
    let Some(next) = available.first().copied() else {
        return Ok(None);
    };
    if let Some(candidate) = requested_candidate {
        if profile.specs.selection == SelectionPolicy::Next && candidate.id != next.id {
            return Err(SpecLoopError::Workflow(format!(
                "requested spec {} is not next; expected {}",
                candidate.id, next.id
            )));
        }
        return Ok(Some(candidate.clone()));
    }
    Ok(Some(next.clone()))
}

pub fn discover_specs(root: &Path, profile: &Profile) -> Result<Vec<SpecCandidate>> {
    discover_specs_with_missing(root, profile).map(|(candidates, _)| candidates)
}

fn discover_specs_with_missing(
    root: &Path,
    profile: &Profile,
) -> Result<(Vec<SpecCandidate>, Vec<MissingSpecCandidate>)> {
    let active = compile_globs(&profile.specs.active_globs)?;
    let id_regex = Regex::new(&profile.specs.id_regex)
        .map_err(|error| SpecLoopError::Config(format!("specs.id_regex: {error}")))?;
    let path_groups = [
        (git::tracked_paths(root)?, PathOrigin::Indexed),
        (git::untracked_paths(root)?, PathOrigin::Untracked),
    ];
    let mut by_id: BTreeMap<String, SpecCandidate> = BTreeMap::new();
    let mut missing = Vec::new();
    for (paths, origin) in path_groups {
        for relative in paths {
            let Some(relative_text) = relative.to_str() else {
                // Profile glob and ID patterns are UTF-8; unrelated raw Git paths remain processable.
                continue;
            };
            if !active.is_match(relative_text) {
                continue;
            }
            let relative_text = relative_text.to_owned();
            let absolute = root.join(&relative);
            let metadata = match fs::metadata(&absolute) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if matches!(origin, PathOrigin::Indexed)
                        && let Some(capture) = id_regex
                            .captures(&relative_text)
                            .and_then(|captures| captures.name("id"))
                    {
                        missing.push(MissingSpecCandidate {
                            id: capture.as_str().to_owned(),
                            path: relative,
                        });
                    }
                    continue;
                }
                Err(source) => return Err(io_error("inspect spec path", &absolute, source)),
            };
            if !metadata.is_file() {
                return Err(SpecLoopError::Config(format!(
                    "spec path is not a regular file: {relative_text}"
                )));
            }
            let candidate =
                spec_candidate(relative, &relative_text, &id_regex, profile.specs.ordering)?;
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
    }
    let mut candidates: Vec<_> = by_id.into_values().collect();
    candidates.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));
    Ok((candidates, missing))
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

fn missing_selected_spec(path: &Path) -> SpecLoopError {
    SpecLoopError::Workflow(format!("selected spec path is missing: {}", path.display()))
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
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::{discover_specs, select_spec, sort_key};
    use crate::config::{OrderingPolicy, Profile};

    fn initialize_repository(root: &Path) {
        for args in [
            &["init", "-q"][..],
            &["config", "user.name", "Discovery Test"],
            &["config", "user.email", "discovery@example.invalid"],
        ] {
            let output = Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("run Git command");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn profile() -> Profile {
        toml::from_str(
            "schema_version = 1\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '^specs/(?P<id>[0-9]+)(?:-[^/]*)?\\.md$'\n",
        )
        .expect("parse test profile")
    }

    #[test]
    fn discovery_includes_non_ignored_untracked_specs_and_excludes_ignored_specs() {
        let temporary = TempDir::new().expect("create temporary repository");
        let root = temporary.path();
        fs::create_dir(root.join("specs")).expect("create specs directory");
        fs::write(root.join("specs/001.md"), "tracked spec\n").expect("write tracked spec");
        fs::write(root.join(".gitignore"), "specs/003-ignored.md\n").expect("write ignore rules");
        initialize_repository(root);
        let output = Command::new("git")
            .args(["add", ".gitignore", "specs/001.md"])
            .current_dir(root)
            .output()
            .expect("stage tracked specs");
        assert!(output.status.success());
        let output = Command::new("git")
            .args(["commit", "-qm", "fixture"])
            .current_dir(root)
            .output()
            .expect("commit fixture");
        assert!(output.status.success());
        fs::write(root.join("specs/002.md"), "untracked spec\n").expect("write untracked spec");
        fs::create_dir(root.join("specs/nested")).expect("create nested specs directory");
        fs::write(
            root.join("specs/nested/004.md"),
            "nested file is not a spec\n",
        )
        .expect("write nested file");
        fs::write(root.join("specs/003-ignored.md"), "ignored spec\n").expect("write ignored spec");

        let candidates = discover_specs(root, &profile()).expect("discover specs");

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            ["001", "002"]
        );
    }

    #[test]
    fn untracked_duplicate_spec_id_is_rejected() {
        let temporary = TempDir::new().expect("create temporary repository");
        let root = temporary.path();
        fs::create_dir(root.join("specs")).expect("create specs directory");
        fs::write(root.join("specs/001.md"), "tracked spec\n").expect("write tracked spec");
        initialize_repository(root);
        let output = Command::new("git")
            .args(["add", "specs/001.md"])
            .current_dir(root)
            .output()
            .expect("stage tracked spec");
        assert!(output.status.success());
        let output = Command::new("git")
            .args(["commit", "-qm", "fixture"])
            .current_dir(root)
            .output()
            .expect("commit fixture");
        assert!(output.status.success());
        fs::write(root.join("specs/001-duplicate.md"), "duplicate spec\n")
            .expect("write duplicate untracked spec");

        let error = discover_specs(root, &profile()).expect_err("duplicate ID must fail");

        assert!(error.to_string().contains("duplicate spec ID \"001\""));
        assert!(error.to_string().contains("specs/001.md"));
        assert!(error.to_string().contains("specs/001-duplicate.md"));
    }

    #[test]
    fn missing_tracked_spec_still_fails_explicit_selection() {
        let temporary = TempDir::new().expect("create temporary repository");
        let root = temporary.path();
        fs::create_dir(root.join("specs")).expect("create specs directory");
        fs::write(root.join("specs/004.md"), "tracked spec\n").expect("write tracked spec");
        initialize_repository(root);
        let output = Command::new("git")
            .args(["add", "specs/004.md"])
            .current_dir(root)
            .output()
            .expect("stage tracked spec");
        assert!(output.status.success());
        let output = Command::new("git")
            .args(["commit", "-qm", "fixture"])
            .current_dir(root)
            .output()
            .expect("commit fixture");
        assert!(output.status.success());
        fs::remove_file(root.join("specs/004.md")).expect("remove tracked spec");

        let error = select_spec(root, &profile(), Some("004"), &BTreeSet::new())
            .expect_err("missing tracked spec must fail selection");

        assert!(error.to_string().contains("selected spec path is missing"));
    }

    #[cfg(unix)]
    #[test]
    fn discovery_ignores_non_utf8_paths_when_matching_profile_patterns() {
        use std::ffi::OsString;
        use std::fs;
        use std::os::unix::ffi::OsStringExt;
        use std::process::Command;

        use tempfile::TempDir;

        let temporary = TempDir::new().expect("create temporary repository");
        let root = temporary.path();
        fs::create_dir(root.join("specs")).expect("create specs directory");
        fs::write(root.join("specs/001.md"), "valid spec\n").expect("write valid spec");
        let invalid_name = OsString::from_vec(b"invalid-\xff.md".to_vec());
        fs::write(root.join("specs").join(invalid_name), "unmatched file\n")
            .expect("write non-UTF-8 path");

        let run_git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("run Git command");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.name", "Discovery Test"]);
        run_git(&["config", "user.email", "discovery@example.invalid"]);
        run_git(&["add", "--all"]);
        run_git(&["commit", "-qm", "fixture"]);

        let profile: Profile = toml::from_str(
            "schema_version = 1\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '^specs/(?P<id>[0-9]+)\\.md$'\n",
        )
        .expect("parse test profile");
        let candidates = discover_specs(root, &profile).expect("discover valid spec");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "001");
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

    #[test]
    fn lexical_order_compares_text() {
        assert!(sort_key("10", OrderingPolicy::Lexical).is_ok_and(|left| {
            sort_key("2", OrderingPolicy::Lexical).is_ok_and(|right| left < right)
        }));
    }
}
