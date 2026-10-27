#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use tempfile::TempDir;

fn registry_fixture() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "version": 1,
        "host_id": "d14149b3-410d-43bc-a7b6-555e79ac61b6",
        "active_registration_id": "324c1466308102826b42e39df00686baf739944a4b5290423a7d7ad11300783c",
        "registrations": [{
            "id": "324c1466308102826b42e39df00686baf739944a4b5290423a7d7ad11300783c",
            "issuer": "https://auth.openai.com",
            "subject": "subject",
            "client_id": "issued-client-id",
            "credential_storage": "file",
            "fallback_credential": {
                "access_token": "access-secret",
                "refresh_token": "refresh-secret",
                "id_token": "id-secret",
                "scopes": ["chatgpt.tokens.use.direct"],
                "expires_at_unix": 1_900_000_000
            }
        }]
    }))
    .expect("serialize local credential fixture")
}

fn write_fixture(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("registry parent")).expect("create registry parent");
    fs::write(path, bytes).expect("write registry fixture");
}

fn run_status(current_dir: &Path, environment: &[(String, PathBuf)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_spec-loop"));
    command
        .args(["auth", "status"])
        .current_dir(current_dir)
        .env_remove("SPEC_LOOP_AUTH_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .env_remove("APPDATA");
    for (key, value) in environment {
        command.env(key, value);
    }
    command.output().expect("run auth status")
}

fn assert_fixture_status(output: &Output) {
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("granted locally"));
    assert!(!stdout.contains("access-secret"));
    assert!(!stdout.contains("refresh-secret"));
    assert!(!stdout.contains("id-secret"));
}

fn native_default_registry(home: &Path, app_data: &Path, xdg: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = (app_data, xdg);
        home.join("Library/Application Support/Spec Loop/auth.json")
    }
    #[cfg(target_os = "windows")]
    {
        let _ = (home, xdg);
        app_data.join("Spec Loop/auth.json")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (home, app_data);
        xdg.join("spec-loop/auth.json")
    }
}

#[test]
fn auth_directory_override_is_direct_and_precedes_platform_defaults() {
    let temporary = TempDir::new().expect("create auth directory test root");
    let project = temporary.path().join("project");
    fs::create_dir(&project).expect("create profile-free project directory");
    let selected = temporary.path().join("selected");
    let home = temporary.path().join("home");
    let app_data = temporary.path().join("appdata");
    let xdg = temporary.path().join("xdg");
    let decoy = native_default_registry(&home, &app_data, &xdg);
    write_fixture(&selected.join("auth.json"), &registry_fixture());
    write_fixture(&decoy, b"{invalid-json");

    let output = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["auth", "status"])
        .current_dir(&project)
        .env("SPEC_LOOP_AUTH_DIR", &selected)
        .env("HOME", &home)
        .env("APPDATA", &app_data)
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .expect("run status using selected auth directory");

    assert_fixture_status(&output);
    assert!(selected.join("auth.json").is_file());
    assert!(!selected.join("spec-loop/auth.json").exists());
}

#[test]
fn default_auth_registry_paths_preserve_platform_conventions() {
    let temporary = TempDir::new().expect("create default path test root");
    let project = temporary.path().join("project");
    fs::create_dir(&project).expect("create profile-free project directory");
    let home = temporary.path().join("home");
    #[cfg(target_os = "windows")]
    let app_data = temporary.path().join("appdata");
    let xdg = temporary.path().join("xdg");

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let xdg_expected = xdg.join("spec-loop/auth.json");
        write_fixture(&xdg_expected, &registry_fixture());
        write_fixture(&home.join(".config/spec-loop/auth.json"), b"{invalid-json");
        let output = run_status(
            &project,
            &[
                ("HOME".to_owned(), home.clone()),
                ("XDG_CONFIG_HOME".to_owned(), xdg),
            ],
        );
        assert_fixture_status(&output);

        fs::remove_file(&xdg_expected).expect("remove XDG registry fixture");
        write_fixture(
            &home.join(".config/spec-loop/auth.json"),
            &registry_fixture(),
        );
        let output = run_status(&project, &[("HOME".to_owned(), home)]);
        assert_fixture_status(&output);
    }

    #[cfg(target_os = "macos")]
    {
        let expected = home.join("Library/Application Support/Spec Loop/auth.json");
        write_fixture(&expected, &registry_fixture());
        write_fixture(&xdg.join("spec-loop/auth.json"), b"{invalid-json");
        let output = run_status(
            &project,
            &[
                ("HOME".to_owned(), home),
                ("XDG_CONFIG_HOME".to_owned(), xdg),
            ],
        );
        assert_fixture_status(&output);
    }

    #[cfg(target_os = "windows")]
    {
        let expected = app_data.join("Spec Loop/auth.json");
        write_fixture(&expected, &registry_fixture());
        write_fixture(&xdg.join("spec-loop/auth.json"), b"{invalid-json");
        let output = run_status(
            &project,
            &[
                ("APPDATA".to_owned(), app_data),
                ("HOME".to_owned(), home),
                ("XDG_CONFIG_HOME".to_owned(), xdg),
            ],
        );
        assert_fixture_status(&output);
    }
}

#[test]
fn auth_directory_override_rejects_empty_and_relative_values_before_login() {
    let temporary = TempDir::new().expect("create invalid override test root");
    let project = temporary.path().join("project");
    fs::create_dir(&project).expect("create project directory");
    let xdg = temporary.path().join("xdg");
    for value in ["", "relative-auth"] {
        let output = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
            .args(["auth", "login", "--new-account"])
            .current_dir(&project)
            .env("SPEC_LOOP_AUTH_DIR", value)
            .env("XDG_CONFIG_HOME", &xdg)
            .output()
            .expect("run login with invalid auth directory");
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("SPEC_LOOP_AUTH_DIR"), "{error}");
        assert!(error.contains("absolute"), "{error}");
    }
    assert!(!xdg.exists());
    assert!(!project.join("auth.json").exists());
}

#[test]
fn auth_directory_inside_project_is_rejected_before_registry_read() {
    let temporary = TempDir::new().expect("create project boundary test root");
    let project = temporary.path().join("project");
    let selected = project.join("credentials");
    fs::create_dir_all(&selected).expect("create inside-project auth directory");
    fs::write(project.join(".spec-loop.toml"), "not valid profile syntax").expect("mark project");
    let registry = selected.join("auth.json");
    fs::write(&registry, b"{invalid-json").expect("write malformed registry");

    let output = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["auth", "status"])
        .current_dir(&project)
        .env("SPEC_LOOP_AUTH_DIR", &selected)
        .output()
        .expect("run status with project-local auth directory");

    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("outside the project directory"), "{error}");
    assert!(
        !error.contains("invalid authentication registry"),
        "{error}"
    );
    assert_eq!(
        fs::read(&registry).expect("read unchanged registry"),
        b"{invalid-json"
    );
}

#[test]
fn auth_directory_filesystem_failure_does_not_fall_back() {
    let temporary = TempDir::new().expect("create filesystem failure test root");
    let project = temporary.path().join("project");
    fs::create_dir(&project).expect("create profile-free project directory");
    let selected_file = temporary.path().join("not-a-directory");
    fs::write(&selected_file, "selected location is unusable").expect("create file as parent");
    let xdg = temporary.path().join("xdg");
    write_fixture(&xdg.join("spec-loop/auth.json"), &registry_fixture());

    let output = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["auth", "status"])
        .current_dir(&project)
        .env("SPEC_LOOP_AUTH_DIR", &selected_file)
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .expect("run status using unusable selected path");

    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("read authentication registry"), "{error}");
    assert!(
        error.contains(&selected_file.display().to_string()),
        "{error}"
    );
    assert!(!error.contains("granted locally"), "{error}");
}

#[test]
fn builtin_workflow_reads_registry_from_selected_directory_before_network() {
    let temporary = TempDir::new().expect("create builtin workflow test root");
    let project = temporary.path().join("project");
    fs::create_dir_all(project.join("specs")).expect("create specs directory");
    fs::write(project.join("specs/001.md"), "# Local fixture spec\n").expect("write spec");
    fs::create_dir_all(project.join(".spec-loop/plans")).expect("create plan directory");
    fs::write(
        project.join(".spec-loop/plans/001.md"),
        "# Approved fixture plan\n",
    )
    .expect("write plan");
    fs::write(
        project.join(".spec-loop.toml"),
        "schema_version = 3\n\n[specs]\nactive_globs = [\"specs/*.md\"]\nid_regex = '^specs/(?P<id>[0-9]+)\\.md$'\n\n[agent]\nbackend = \"builtin\"\nmodel = \"fixture-model\"\n",
    )
    .expect("write builtin profile");
    let selected = temporary.path().join("selected");
    let registry = selected.join("auth.json");
    fs::create_dir_all(&selected).expect("create selected auth directory");
    fs::write(&registry, b"{invalid-json").expect("write malformed selected registry");
    let xdg = temporary.path().join("xdg");
    write_fixture(&xdg.join("spec-loop/auth.json"), &registry_fixture());

    let output = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["--spec", "001"])
        .current_dir(&project)
        .env("SPEC_LOOP_AUTH_DIR", &selected)
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .expect("run builtin workflow with selected registry");

    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(&registry.display().to_string()), "{error}");
    assert!(error.contains("invalid authentication registry"), "{error}");
}

#[test]
fn authentication_help_documents_directory_and_security_boundary() {
    let temporary = TempDir::new().expect("create help fixture directory");
    let auth = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["auth", "--help"])
        .current_dir(temporary.path())
        .output()
        .expect("run auth help");
    assert!(auth.status.success());
    let auth_help = String::from_utf8_lossy(&auth.stdout);
    assert!(auth_help.contains("SPEC_LOOP_AUTH_DIR"));
    assert!(auth_help.contains("directly contains auth.json"));
    assert!(auth_help.contains("XDG_CONFIG_HOME"));
    assert!(auth_help.contains("plaintext credentials"));
    assert!(auth_help.contains("same-user processes"));

    let login = Command::new(env!("CARGO_BIN_EXE_spec-loop"))
        .args(["auth", "login", "--help"])
        .current_dir(temporary.path())
        .output()
        .expect("run auth login help");
    assert!(login.status.success());
    assert!(String::from_utf8_lossy(&login.stdout).contains("SPEC_LOOP_AUTH_DIR"));
}
