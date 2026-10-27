use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tempfile::TempDir;

use super::{expand_template, resolve_executable};

#[test]
fn executable_resolution_uses_project_root_for_relative_paths() {
    let temporary = TempDir::new().expect("temporary project");
    let directory = temporary.path().join("tools");
    fs::create_dir(&directory).expect("create tools directory");
    let executable = directory.join("agent");
    fs::write(&executable, "fixture").expect("write executable");
    #[cfg(unix)]
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
        .expect("set executable permissions");
    assert_eq!(
        resolve_executable(temporary.path(), Path::new("tools/agent"), "agent")
            .expect("resolve executable"),
        executable
    );
    let error = resolve_executable(temporary.path(), Path::new("tools/missing"), "check")
        .expect_err("missing executable");
    assert!(error.to_string().contains("check executable was not found"));
}

#[test]
fn substituted_values_are_not_reinterpreted_as_placeholders() {
    assert_eq!(
        expand_template(
            "{spec_id}|{spec_path}|{plan_path}",
            "001{spec_path}",
            Path::new("specs/{plan_path}.md"),
            Some(Path::new("plans/{spec_id}.md")),
        ),
        "001{spec_path}|specs/{plan_path}.md|plans/{spec_id}.md"
    );
}
