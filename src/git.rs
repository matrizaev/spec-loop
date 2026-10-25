use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::commands::{self, status_success};
use crate::error::{Result, SpecLoopError, io_error, process_error};

/// Compact identity for current Git-visible changes, excluding ignored files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitIdentity(Vec<u8>);

/// Finds and canonicalizes repository root containing `start`.
pub fn discover_root(start: &Path) -> Result<PathBuf> {
    let output = command(start, ["rev-parse", "--show-toplevel"])?;
    let stdout = String::from_utf8(output.stdout).map_err(|error| {
        SpecLoopError::Process(format!("git rev-parse returned non-UTF8 root: {error}"))
    })?;
    let root_text = stdout.strip_suffix('\n').ok_or_else(|| {
        SpecLoopError::Process("git rev-parse root output has no terminal newline".to_owned())
    })?;
    if root_text.is_empty() {
        return Err(SpecLoopError::Process(
            "git rev-parse returned empty root".to_owned(),
        ));
    }
    let root = PathBuf::from(root_text);
    std::fs::canonicalize(&root).map_err(|source| io_error("canonicalize Git root", root, source))
}

/// Rejects an unborn repository before workflow agents can modify its worktree.
pub fn require_head(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .output()
        .map_err(|source| io_error("validate Git HEAD", root, source))?;
    if output.status.success() {
        return Ok(());
    }
    if output.status.code() == Some(1) {
        return Err(SpecLoopError::Workflow(
            "repository has no valid HEAD commit; create an initial Git commit before running Spec Loop"
                .to_owned(),
        ));
    }
    Err(process_error(
        "git rev-parse --verify HEAD",
        output.status,
        &output.stderr,
    ))
}

fn command<const N: usize>(root: &Path, args: [&str; N]) -> Result<Output> {
    let arguments: Vec<OsString> = args.iter().map(OsString::from).collect();
    command_os(root, &arguments)
}

fn command_os(root: &Path, args: &[OsString]) -> Result<Output> {
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|source| io_error("run git", root, source))?;
    if !output.status.success() {
        return Err(process_error("git", output.status, &output.stderr));
    }
    Ok(output)
}

/// Lists paths present in Git index.
pub fn tracked_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let output = command(root, ["ls-files", "--cached", "-z"])?;
    parse_path_list(&output.stdout)
}

/// Lists non-ignored worktree paths not present in Git index.
pub fn untracked_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let output = command(root, ["ls-files", "--others", "--exclude-standard", "-z"])?;
    parse_path_list(&output.stdout)
}

fn parse_path_list(output: &[u8]) -> Result<Vec<PathBuf>> {
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(path_from_git_bytes)
        .collect()
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    if bytes.contains(&0) {
        return Err(SpecLoopError::Workflow(
            "Git path contains a NUL byte".to_owned(),
        ));
    }
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    if bytes.contains(&0) {
        return Err(SpecLoopError::Workflow(
            "Git path contains a NUL byte".to_owned(),
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|error| {
        SpecLoopError::Workflow(format!(
            "Git path cannot be represented on this platform: {error}"
        ))
    })?;
    Ok(PathBuf::from(text))
}

/// Returns NUL-delimited porcelain status for non-ignored worktree changes.
pub fn status(root: &Path) -> Result<Vec<u8>> {
    Ok(command(
        root,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?
    .stdout)
}

/// Identifies Git-visible input without retaining a worktree or index snapshot.
pub fn input_identity(root: &Path) -> Result<GitIdentity> {
    input_identity_excluding(root, &[])
}

/// Identifies Git-visible changes except exact paths protected by content hashes.
pub fn input_identity_excluding(root: &Path, excluded_paths: &[&Path]) -> Result<GitIdentity> {
    let mut identity = Vec::new();
    let staged = raw_diff_excluding(
        root,
        &["diff", "--raw", "--cached", "-z", "HEAD"],
        excluded_paths,
    )?;
    if !staged.is_empty() {
        identity.extend_from_slice(b"staged\0");
        identity.extend_from_slice(&staged);
    }
    let unstaged = raw_diff_excluding(root, &["diff", "--raw", "-z"], excluded_paths)?;
    if !unstaged.is_empty() {
        identity.extend_from_slice(b"unstaged\0");
        identity.extend_from_slice(&unstaged);
    }
    for path in untracked_paths(root)? {
        if excluded_paths.contains(&path.as_path()) {
            continue;
        }
        identity.extend_from_slice(path.as_os_str().as_encoded_bytes());
        identity.push(0);
        let absolute = root.join(&path);
        let metadata = std::fs::symlink_metadata(&absolute)
            .map_err(|source| io_error("inspect untracked Git path", &absolute, source))?;
        if metadata.file_type().is_symlink() {
            identity.push(b'L');
            let target = std::fs::read_link(&absolute)
                .map_err(|source| io_error("read untracked symbolic link", &absolute, source))?;
            identity.extend_from_slice(target.as_os_str().as_encoded_bytes());
        } else if metadata.is_file() {
            identity.push(b'F');
            let args = [
                OsString::from("hash-object"),
                OsString::from("--no-filters"),
                OsString::from("--"),
                path.as_os_str().to_owned(),
            ];
            identity.extend_from_slice(&command_os(root, &args)?.stdout);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                identity.push(u8::from(metadata.permissions().mode() & 0o111 != 0));
            }
        } else {
            identity.push(b'O');
            identity.extend_from_slice(format!("{:?}", metadata.file_type()).as_bytes());
        }
        identity.push(0);
    }
    Ok(GitIdentity(identity))
}

fn raw_diff_excluding(root: &Path, args: &[&str], excluded_paths: &[&Path]) -> Result<Vec<u8>> {
    let mut arguments: Vec<OsString> = args.iter().map(OsString::from).collect();
    arguments.push(OsString::from("--"));
    arguments.push(OsString::from("."));
    for path in excluded_paths {
        let mut pathspec = OsString::from(":(exclude,literal)");
        pathspec.push(path.as_os_str());
        arguments.push(pathspec);
    }
    Ok(command_os(root, &arguments)?.stdout)
}

/// Reports whether Git ignore rules exclude `relative` from ordinary staging.
pub fn is_ignored(root: &Path, relative: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "--quiet", "--no-index", "--"])
        .arg(relative)
        .output()
        .map_err(|source| io_error("check archive ignore rules", root, source))?;
    if output.status.success() {
        Ok(true)
    } else if output.status.code() == Some(1) {
        Ok(false)
    } else {
        Err(process_error(
            "git check-ignore",
            output.status,
            &output.stderr,
        ))
    }
}

/// Runs standard `git add --all`, excluding paths ignored by Git.
pub fn stage_all(root: &Path, cancelled: &Arc<AtomicBool>) -> Result<()> {
    run_git_mutation(root, &["add", "--all", "--"], cancelled, "git add")
}

/// Runs ordinary `git commit -m`, preserving standard hooks and Git behavior.
pub fn commit(root: &Path, message: &str, cancelled: &Arc<AtomicBool>) -> Result<()> {
    run_git_mutation(root, &["commit", "-m", message], cancelled, "git commit")
}

fn run_git_mutation(
    root: &Path,
    args: &[&str],
    cancelled: &Arc<AtomicBool>,
    operation: &str,
) -> Result<()> {
    commands::ensure_not_cancelled(cancelled)?;
    let arguments: Vec<OsString> = args.iter().map(OsString::from).collect();
    let output = commands::run_process_os(Path::new("git"), &arguments, root, cancelled)?;
    if !status_success(&output) {
        let status = output.status.ok_or_else(|| {
            SpecLoopError::Process(format!("{operation} finished without an exit status"))
        })?;
        return Err(process_error(operation, status, &output.stderr));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use tempfile::TempDir;

    use super::{
        commit, input_identity, input_identity_excluding, stage_all, status, tracked_paths,
        untracked_paths,
    };

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> TempDir {
        let temp = TempDir::new().expect("temporary directory");
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.name", "Spec Loop Test"]);
        git(
            temp.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(temp.path().join("tracked.txt"), "base\n").expect("tracked file");
        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        temp
    }

    #[test]
    fn no_changes_returns_standard_git_commit_error() {
        let temp = repository();
        let initial_head = Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("read initial HEAD");
        assert!(initial_head.status.success());

        let result = commit(
            temp.path(),
            "empty commit",
            &Arc::new(AtomicBool::new(false)),
        );

        assert!(result.is_err());
        let final_head = Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("read final HEAD");
        assert!(final_head.status.success());
        assert_eq!(initial_head.stdout, final_head.stdout);
    }

    #[test]
    fn identity_changes_for_tracked_and_untracked_content() {
        let temp = repository();
        let initial = input_identity(temp.path()).expect("initial identity");
        fs::write(temp.path().join("tracked.txt"), "changed\n").expect("edit tracked file");
        let tracked = input_identity(temp.path()).expect("tracked identity");
        assert_ne!(initial, tracked);
        fs::write(temp.path().join("new.txt"), "new\n").expect("untracked file");
        let untracked = input_identity(temp.path()).expect("untracked identity");
        assert_ne!(tracked, untracked);
        fs::write(temp.path().join("new.txt"), "other\n").expect("edit untracked file");
        assert_ne!(
            untracked,
            input_identity(temp.path()).expect("updated identity")
        );
    }

    #[test]
    fn excluded_protected_paths_are_left_to_content_hash_checks() {
        let temp = repository();
        fs::create_dir_all(temp.path().join("specs")).expect("spec directory");
        fs::create_dir_all(temp.path().join("plans")).expect("plan directory");
        fs::write(temp.path().join("specs/001.md"), "spec\n").expect("spec");
        fs::write(temp.path().join("plans/001.md"), "plan\n").expect("plan");
        git(temp.path(), &["add", "specs/001.md", "plans/001.md"]);
        git(temp.path(), &["commit", "-qm", "add protected inputs"]);
        let protected_paths = [Path::new("specs/001.md"), Path::new("plans/001.md")];
        let initial = input_identity_excluding(temp.path(), &protected_paths)
            .expect("identity before protected changes");

        fs::write(temp.path().join("specs/001.md"), "revised spec\n").expect("revise spec");
        fs::write(temp.path().join("plans/001.md"), "revised plan\n").expect("revise plan");
        assert_eq!(
            initial,
            input_identity_excluding(temp.path(), &protected_paths)
                .expect("identity after protected changes")
        );

        fs::write(temp.path().join("tracked.txt"), "unrelated change\n")
            .expect("edit unrelated file");
        assert_ne!(
            initial,
            input_identity_excluding(temp.path(), &protected_paths)
                .expect("identity after unrelated change")
        );
    }

    #[test]
    fn identity_includes_staged_edit_reverted_in_worktree() {
        let temp = repository();
        let initial = input_identity(temp.path()).expect("initial identity");

        fs::write(temp.path().join("tracked.txt"), "staged edit\n").expect("staged edit");
        git(temp.path(), &["add", "tracked.txt"]);
        fs::write(temp.path().join("tracked.txt"), "base\n").expect("revert worktree edit");

        let staged_only = input_identity(temp.path()).expect("staged-only identity");
        assert_ne!(initial, staged_only);
        let net_diff = Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["diff", "--raw", "HEAD"])
            .output()
            .expect("inspect net worktree diff");
        assert!(net_diff.status.success());
        assert_eq!(net_diff.stdout, Vec::<u8>::new());
    }

    #[test]
    fn ignored_files_do_not_enter_identity_or_untracked_paths() {
        let temp = repository();
        fs::write(temp.path().join(".gitignore"), "ignored.txt\n").expect("gitignore");
        git(temp.path(), &["add", ".gitignore"]);
        git(temp.path(), &["commit", "-qm", "ignore output"]);
        let before = input_identity(temp.path()).expect("identity before ignored file");
        fs::write(temp.path().join("ignored.txt"), "ignored\n").expect("ignored file");
        assert_eq!(
            before,
            input_identity(temp.path()).expect("identity after ignored file")
        );
        assert!(
            !untracked_paths(temp.path())
                .expect("untracked paths")
                .contains(&"ignored.txt".into())
        );
    }

    #[test]
    fn staging_preserves_complete_non_ignored_patch() {
        let temp = repository();
        fs::write(temp.path().join("tracked.txt"), "changed\n").expect("edit tracked file");
        fs::write(temp.path().join("new.txt"), "new\n").expect("new file");
        stage_all(temp.path(), &Arc::new(AtomicBool::new(false))).expect("stage changes");
        let staged = Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["diff", "--cached", "--quiet"])
            .output()
            .expect("check staged changes");
        assert_eq!(staged.status.code(), Some(1));
        let status = status(temp.path()).expect("status");
        assert!(status.windows(2).any(|record| record == b"M "));
        assert!(
            tracked_paths(temp.path())
                .expect("tracked paths")
                .contains(&"new.txt".into())
        );
    }
}
