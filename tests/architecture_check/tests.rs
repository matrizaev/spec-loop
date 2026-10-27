use std::collections::HashSet;
use std::fs;
use std::path::Path;

use super::{
    PRODUCTION_LIMIT, TEST_LIMIT, discover_rust_sources, parse_exclusion_inventory,
    physical_line_count, validate_rust_file,
};

#[test]
fn physical_line_counts_enforce_each_boundary_and_unterminated_tail() {
    assert_eq!(physical_line_count(b""), 0);
    assert_eq!(physical_line_count(b"one\n"), 1);
    assert_eq!(physical_line_count(b"one\ntwo"), 2);
    assert_eq!(physical_line_count(b"\n"), 1);

    let directory = tempfile::tempdir().unwrap();
    let production = directory.path().join("src.rs");
    write_lines(&production, PRODUCTION_LIMIT, false);
    assert!(
        validate_rust_file(directory.path(), &production).is_ok(),
        "{}",
        validate_rust_file(directory.path(), &production).unwrap_err()
    );
    write_lines(&production, PRODUCTION_LIMIT + 1, false);
    let error = validate_rust_file(directory.path(), &production).unwrap_err();
    assert!(error.contains("src.rs"));
    assert!(error.contains("1599"));
    assert!(error.contains("1598"));

    let test_file = directory.path().join("tests.rs");
    write_lines(&test_file, TEST_LIMIT, false);
    assert!(validate_rust_file(directory.path(), &test_file).is_ok());
    write_lines(&test_file, TEST_LIMIT + 1, false);
    let error = validate_rust_file(directory.path(), &test_file).unwrap_err();
    assert!(error.contains("tests.rs"));
    assert!(error.contains("2099"));
    assert!(error.contains("2098"));

    let integration_test = directory.path().join("tests/fixture.rs");
    write_lines(&integration_test, TEST_LIMIT, false);
    assert!(validate_rust_file(directory.path(), &integration_test).is_ok());
    write_lines(&integration_test, TEST_LIMIT + 1, false);
    let error = validate_rust_file(directory.path(), &integration_test).unwrap_err();
    assert!(error.contains("tests/fixture.rs"));
    assert!(error.contains("2099"));
    assert!(error.contains("2098"));

    let production_fixture = directory.path().join("src/tests/fixture.rs");
    write_lines(&production_fixture, PRODUCTION_LIMIT + 1, false);
    let error = validate_rust_file(directory.path(), &production_fixture).unwrap_err();
    assert!(error.contains("src/tests/fixture.rs"), "{error}");
    assert!(error.contains("1599"), "{error}");
    assert!(error.contains("1598"), "{error}");
}

#[test]
fn exact_inventory_entries_exclude_only_listed_rust_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "generated/marked.rs", "// generated\n");
    write(root, "generated/neighbor.rs", "// generated\n");
    write(root, "vendor/marked.rs", "// @generated\n");
    let inventory = "# kind\ngenerated\tgenerated/marked.rs\tgenerator: schema build\nvendored\tvendor/marked.rs\tupstream: project 1.2\n";
    fs::create_dir_all(root.join("generated/directory.rs")).unwrap();
    let directory_error =
        parse_exclusion_inventory(root, "generated\tgenerated/directory.rs\tprovenance\n")
            .unwrap_err();
    assert!(
        directory_error.contains("regular file"),
        "{directory_error}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        symlink(
            root.join("generated/marked.rs"),
            root.join("generated/symlink.rs"),
        )
        .unwrap();
        let symlink_error =
            parse_exclusion_inventory(root, "generated\tgenerated/symlink.rs\tprovenance\n")
                .unwrap_err();
        assert!(symlink_error.contains("regular file"), "{symlink_error}");
        fs::remove_file(root.join("generated/symlink.rs")).unwrap();
    }
    let exclusions = parse_exclusion_inventory(root, inventory).unwrap();
    let sources = discover_rust_sources(root, &exclusions).unwrap();
    assert!(!sources.contains(&root.join("generated/marked.rs")));
    assert!(sources.contains(&root.join("generated/neighbor.rs")));
    assert!(!sources.contains(&root.join("vendor/marked.rs")));

    for invalid in [
        "generated\tgenerated/marked.rs\tprovenance\textra\n",
        "vendored\tgenerated/marked.rs\tprovenance\ngenerated\tgenerated/marked.rs\tprovenance\n",
        "generated\tgenerated\tprovenance\n",
        "generated\t../outside.rs\tprovenance\n",
        "generated\t./generated/marked.rs\tprovenance\n",
        "generated\tgenerated//marked.rs\tprovenance\n",
        "generated\tmissing.rs\tprovenance\n",
        "generated\tgenerated/marked.rs\t\n",
        "generated\tgenerated/marked.rs\tprovenance\ngenerated\tgenerated/marked.rs\tprovenance\n",
    ] {
        let error = parse_exclusion_inventory(root, invalid).unwrap_err();
        assert!(error.contains("architecture-exclusions.txt"), "{error}");
        assert!(error.contains("exclusion"), "{error}");
    }
}

#[test]
fn discovery_includes_first_party_locations_and_prunes_only_root_outputs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for file in [
        "src/module.rs",
        "tests/integration.rs",
        "examples/example.rs",
        "benches/bench.rs",
        "build.rs",
        ".spec-loop/nested.rs",
        "generated/source.rs",
        "dist/source.rs",
        "node_modules/source.rs",
        "third_party/source.rs",
        "vendor/source.rs",
        "build-output/source.rs",
        "nested/target/source.rs",
    ] {
        write(root, file, "fn source() {}\n");
    }
    write(root, ".git/ignored.rs", "fn ignored() {}\n");
    write(root, "target/ignored.rs", "fn ignored() {}\n");
    let sources = discover_rust_sources(root, &HashSet::default()).unwrap();
    assert_eq!(sources.len(), 13);
    assert!(!sources.iter().any(|path| path.ends_with(".git/ignored.rs")));
    assert!(
        !sources
            .iter()
            .any(|path| path.ends_with("target/ignored.rs"))
    );
    assert!(
        sources
            .iter()
            .any(|path| path.ends_with("nested/target/source.rs"))
    );
}

#[test]
fn syntax_rules_report_violations_and_accept_ordered_imports() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "valid/sibling.rs", "pub struct Item;\n");
    let accepted = "use std::path::Path; use serde::Deserialize; use crate::config::Profile; use self::sibling::Item; mod sibling; const VALUE: u8 = 1;";
    write(root, "valid/mod.rs", accepted);
    assert!(validate_rust_file(root, &root.join("valid/mod.rs")).is_ok());

    let cases = [
        ("inline.rs", "mod child {}", "inline module bodies"),
        (
            "local_inline.rs",
            "fn f() { mod local {} }",
            "inline module bodies",
        ),
        (
            "local_path.rs",
            "fn f() { #[path = \"other.rs\"] mod local {} }",
            "#[path] module relocation",
        ),
        ("local_glob.rs", "fn f() { use std::*; }", "glob imports"),
        (
            "path.rs",
            "#[path = \"other.rs\"] mod child;",
            "#[path] module relocation",
        ),
        (
            "cfg_attr_path.rs",
            "#[cfg_attr(unix, path = \"other.rs\")] mod child;",
            "#[path] module relocation",
        ),
        ("glob.rs", "use crate::{value, *};", "glob imports"),
        ("nested_glob.rs", "use crate::module::*;", "glob imports"),
        (
            "late_import.rs",
            "const VALUE: u8 = 1; use std::path::Path;",
            "imports must precede",
        ),
        (
            "late_mod.rs",
            "const VALUE: u8 = 1; mod child;",
            "module declarations must precede",
        ),
        (
            "non_macro.rs",
            "use std::path::Path; const VALUE: u8 = 1; mod child;",
            "module declarations must precede",
        ),
    ];
    for (name, source, rule) in cases {
        write(root, name, source);
        if matches!(name, "late_mod.rs" | "non_macro.rs") {
            write(
                root,
                &format!("{}/child.rs", name.trim_end_matches(".rs")),
                "pub struct Child;\n",
            );
        }
        let error = validate_rust_file(root, &root.join(name)).unwrap_err();
        assert!(error.contains(name), "{error}");
        assert!(error.contains(rule), "{error}");
    }
    write(root, "bare_sibling/sibling.rs", "pub struct Item;\n");
    write(
        root,
        "bare_sibling/mod.rs",
        "use sibling::Item; mod sibling;\n",
    );
    let error = validate_rust_file(root, &root.join("bare_sibling/mod.rs")).unwrap_err();
    assert!(error.contains("bare_sibling/mod.rs"));
    assert!(error.contains("sibling import requires self::"));
}

#[test]
fn required_macros_may_precede_declared_children() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(
        root,
        "macro_unused/mod.rs",
        "macro_rules! shared { () => {} } mod child;\n",
    );
    write(root, "macro_unused/child.rs", "pub struct Child;\n");
    let error = validate_rust_file(root, &root.join("macro_unused/mod.rs")).unwrap_err();
    assert!(error.contains("macro_unused/mod.rs"));
    assert!(error.contains("macro_rules!"));
    write(
        root,
        "macro_used/mod.rs",
        "macro_rules! shared { () => {} } mod child;\n",
    );
    write(
        root,
        "macro_used/child.rs",
        "shared!();\npub struct Child;\n",
    );
    assert!(validate_rust_file(root, &root.join("macro_used/mod.rs")).is_ok());
    write(
        root,
        "macros_used/mod.rs",
        "macro_rules! first { () => {} } macro_rules! second { () => {} } mod child;\n",
    );
    write(
        root,
        "macros_used/child.rs",
        "first!(); second!(); pub struct Child;\n",
    );
    assert!(validate_rust_file(root, &root.join("macros_used/mod.rs")).is_ok());
    write(
        root,
        "macro_between_children/mod.rs",
        "mod first; macro_rules! second { () => {} } mod second;\n",
    );
    write(
        root,
        "macro_between_children/first.rs",
        "pub struct First;\n",
    );
    write(
        root,
        "macro_between_children/second.rs",
        "second!(); pub struct Second;\n",
    );
    assert!(validate_rust_file(root, &root.join("macro_between_children/mod.rs")).is_ok());
}

#[test]
fn import_group_ordering_rejects_reversed_interleaved_and_mixed_trees() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for (name, source) in [
        (
            "reversed.rs",
            "use serde::Deserialize; use std::path::Path;",
        ),
        (
            "interleaved.rs",
            "use std::path::Path; use serde::Deserialize; use crate::config::Profile; use std::io::Read;",
        ),
        (
            "mixed.rs",
            "use {std::path::Path, serde::Deserialize, crate::config::Profile};",
        ),
    ] {
        write(root, name, source);
        let error = validate_rust_file(root, &root.join(name)).unwrap_err();
        assert!(error.contains(name), "{error}");
        assert!(error.contains("import-group ordering"), "{error}");
    }
    write(root, "same_group.rs", "use std::{io::Read, path::Path};\n");
    assert!(validate_rust_file(root, &root.join("same_group.rs")).is_ok());
}

fn write_lines(path: &Path, count: usize, terminate: bool) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut content = "// fixture\n".repeat(count);
    if !terminate {
        content.pop();
    }
    fs::write(path, content).unwrap();
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

#[test]
fn module_root_rules_reject_alternate_and_noncanonical_layouts() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "lib.rs", "mod leaf; mod nested;\n");
    write(root, "leaf.rs", "pub struct Leaf;\n");
    write(root, "nested/mod.rs", "mod child;\n");
    write(root, "nested/child.rs", "pub struct Child;\n");
    assert!(validate_rust_file(root, &root.join("lib.rs")).is_ok());
    assert!(validate_rust_file(root, &root.join("nested/mod.rs")).is_ok());

    write(root, "nested.rs", "pub struct Alternate;\n");
    let error = validate_rust_file(root, &root.join("lib.rs")).unwrap_err();
    assert!(
        error.contains("ambiguous/noncanonical module roots"),
        "{error}"
    );

    let noncanonical = tempfile::tempdir().unwrap();
    let root = noncanonical.path();
    write(root, "lib.rs", "mod nested;\n");
    write(root, "nested.rs", "mod child;\n");
    write(root, "nested/child.rs", "pub struct Child;\n");
    let error = validate_rust_file(root, &root.join("lib.rs")).unwrap_err();
    assert!(error.contains("noncanonical module root"), "{error}");
    assert!(error.contains("nested.rs"), "{error}");

    let missing = tempfile::tempdir().unwrap();
    write(missing.path(), "lib.rs", "mod missing;\n");
    let error = validate_rust_file(missing.path(), &missing.path().join("lib.rs")).unwrap_err();
    assert!(
        error.contains("cannot resolve filesystem-backed module missing"),
        "{error}"
    );
}

#[test]
fn import_classification_accepts_all_categories_and_rejects_bare_sibling() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "sibling.rs", "pub struct Item;\n");
    write(
        root,
        "lib.rs",
        "use std::path::Path; use core::fmt::Debug; use alloc::string::String; use serde as serde_alias; use self::sibling::Item; mod sibling;\n",
    );
    assert!(validate_rust_file(root, &root.join("lib.rs")).is_ok());

    write(root, "bare/mod.rs", "use sibling; mod sibling;\n");
    write(root, "bare/sibling.rs", "pub struct Item;\n");
    let error = validate_rust_file(root, &root.join("bare/mod.rs")).unwrap_err();
    assert!(error.contains("sibling import requires self::"), "{error}");
}

#[test]
fn syntax_parser_failures_report_the_file_and_rule() {
    let directory = tempfile::tempdir().unwrap();
    write(directory.path(), "broken.rs", "fn broken( {\n");
    let error =
        validate_rust_file(directory.path(), &directory.path().join("broken.rs")).unwrap_err();
    assert!(error.contains("broken.rs"), "{error}");
    assert!(error.contains("Rust syntax parse failed"), "{error}");
}

#[cfg(unix)]
#[test]
fn discovery_rejects_symlinked_rust_and_directory_paths() {
    use std::os::unix::fs::symlink;

    let rust_alias = tempfile::tempdir().unwrap();
    write(rust_alias.path(), "tests/source.rs", "fn source() {}\n");
    let alias = rust_alias.path().join("src/alias.rs");
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    symlink(rust_alias.path().join("tests/source.rs"), &alias).unwrap();
    let error = discover_rust_sources(rust_alias.path(), &HashSet::default()).unwrap_err();
    assert!(error.contains("src/alias.rs"), "{error}");
    assert!(error.contains("symlink rule"), "{error}");

    let directory_alias = tempfile::tempdir().unwrap();
    let target = directory_alias.path().join("outside");
    write(
        directory_alias.path(),
        "outside/hidden.rs",
        "fn hidden() {}\n",
    );
    let alias = directory_alias.path().join("linked");
    symlink(target, &alias).unwrap();
    let error = discover_rust_sources(directory_alias.path(), &HashSet::default()).unwrap_err();
    assert!(error.contains("linked"), "{error}");
    assert!(error.contains("directory symlinks"), "{error}");
}

#[test]
fn cargo_test_target_roots_resolve_directory_modules() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "tests/architecture.rs", "mod architecture_check;\n");
    write(root, "tests/architecture_check/mod.rs", "mod tests;\n");
    write(
        root,
        "tests/architecture_check/tests.rs",
        "#[test] fn fixture() {}\n",
    );
    let sources = discover_rust_sources(root, &HashSet::default()).unwrap();
    let crate_roots = super::discover_crate_roots(root, &sources).unwrap();
    for path in sources {
        super::validate_rust_file_with_roots(root, &path, &crate_roots)
            .unwrap_or_else(|error| panic!("{error}"));
    }

    let build = tempfile::tempdir().unwrap();
    let root = build.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"scripts/build.rs\"\n",
    );
    write(root, "scripts/build.rs", "mod helpers;\n");
    write(root, "scripts/helpers.rs", "pub fn helper() {}\n");
    let sources = discover_rust_sources(root, &HashSet::default()).unwrap();
    let crate_roots = super::discover_crate_roots(root, &sources).unwrap();
    let build_script = root.join("scripts/build.rs");
    super::validate_rust_file_with_roots(root, &build_script, &crate_roots)
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn unit_test_module_rejects_nested_mod_rs_root() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "module/mod.rs", "#[cfg(test)] mod tests;\n");
    write(root, "module/tests/mod.rs", "#[test] fn nested() {}\n");

    let error = validate_rust_file(root, &root.join("module/mod.rs")).unwrap_err();
    assert!(error.contains("module/mod.rs"), "{error}");
    assert!(error.contains("unit-test module tests"), "{error}");
    assert!(error.contains("module/tests/mod.rs"), "{error}");
    assert!(error.contains("nested module root"), "{error}");
}
