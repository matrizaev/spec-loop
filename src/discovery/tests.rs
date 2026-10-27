use std::collections::BTreeSet;
use std::fs;

use tempfile::TempDir;

use super::{discover_specs, select_spec, sort_key};
use crate::config::{OrderingPolicy, Profile};

fn profile() -> Profile {
    toml::from_str(
            "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '^specs/(?P<id>[0-9]+)(?:-[^/]*)?\\.md$'\n[agent]\nbackend = 'pi'\n",
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
