use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::{
    ProcessTermination, ensure_not_cancelled, run_process, run_process_until,
    run_process_until_with_spawn_observer, run_process_with_os_args,
};

#[test]
fn bounded_capture_drains_both_streams_and_marks_truncation() {
    let temporary = TempDir::new().expect("create temporary directory");
    let output = run_bounded_capture(
        temporary.path(),
        "head -c 65536 /dev/zero & head -c 65536 /dev/zero >&2 & wait",
        6,
    );

    assert!(output.status.is_some_and(|status| status.success()));
    assert!(output.stdout.len() + output.stderr.len() <= 6);
    assert!(output.stdout_truncated);
    assert!(output.stderr_truncated);
}

#[test]
fn bounded_capture_uses_all_capacity_for_either_single_stream() {
    let temporary = TempDir::new().expect("create temporary directory");
    for (stderr_only, expected_length, expected_truncated) in [
        (false, 5, false),
        (false, 6, false),
        (false, 6, true),
        (true, 5, false),
        (true, 6, false),
        (true, 6, true),
    ] {
        let produced = if expected_truncated {
            7
        } else {
            expected_length
        };
        let (stdout_size, stderr_size) = if stderr_only {
            (0, produced)
        } else {
            (produced, 0)
        };
        let output = run_capture_sizes(temporary.path(), stdout_size, stderr_size, 6);
        let captured = if stderr_only {
            &output.stderr
        } else {
            &output.stdout
        };
        assert_eq!(captured.len(), expected_length);
        assert_eq!(output.stdout_truncated, !stderr_only && expected_truncated);
        assert_eq!(output.stderr_truncated, stderr_only && expected_truncated);
        assert!(output.stdout.len() + output.stderr.len() <= 6);
    }
}

#[test]
fn bounded_capture_single_stream_retains_800_kib_at_1_mib_limit() {
    let temporary = TempDir::new().expect("create temporary directory");
    for stderr_only in [false, true] {
        let output = if stderr_only {
            run_capture_sizes(temporary.path(), 0, 819_200, 1_048_576)
        } else {
            run_capture_sizes(temporary.path(), 819_200, 0, 1_048_576)
        };
        assert_eq!(output.stdout.len(), if stderr_only { 0 } else { 819_200 });
        assert_eq!(output.stderr.len(), if stderr_only { 819_200 } else { 0 });
        assert!(!output.stdout_truncated);
        assert!(!output.stderr_truncated);
        assert!(output.status.is_some_and(|status| status.success()));
    }
}

#[test]
fn bounded_capture_marks_only_omitted_bytes_at_boundaries() {
    let temporary = TempDir::new().expect("create temporary directory");
    for (stdout_size, stderr_size) in [(2, 3), (3, 3), (4, 4)] {
        let output = run_capture_sizes(temporary.path(), stdout_size, stderr_size, 6);
        assert!(output.stdout.len() + output.stderr.len() <= 6);
        if stdout_size + stderr_size <= 6 {
            assert_eq!(output.stdout.len(), stdout_size);
            assert_eq!(output.stderr.len(), stderr_size);
            assert!(!output.stdout_truncated);
            assert!(!output.stderr_truncated);
        } else {
            assert_eq!(output.stdout_truncated, output.stdout.len() < stdout_size);
            assert_eq!(output.stderr_truncated, output.stderr.len() < stderr_size);
        }
    }
}

#[test]
fn uncapped_process_capture_retains_both_streams() {
    let temporary = TempDir::new().expect("create temporary directory");
    let output = run_process(
        Path::new("/bin/sh"),
        &["-c".to_owned(), "printf abc; printf def >&2".to_owned()],
        temporary.path(),
        None,
        &Arc::new(AtomicBool::new(false)),
    )
    .expect("run uncapped process");

    assert_eq!(output.stdout, b"abc");
    assert_eq!(output.stderr, b"def");
    assert!(!output.stdout_truncated);
    assert!(!output.stderr_truncated);
}

fn run_capture_sizes(
    root: &Path,
    stdout_bytes: usize,
    stderr_bytes: usize,
    limit: usize,
) -> super::ChildOutput {
    let command = format!("head -c {stdout_bytes} /dev/zero; head -c {stderr_bytes} /dev/zero >&2");
    run_bounded_capture(root, &command, limit)
}

fn run_bounded_capture(root: &Path, command: &str, limit: usize) -> super::ChildOutput {
    run_process_until(
        Path::new("/bin/sh"),
        &["-c".to_owned(), command.to_owned()],
        root,
        Some(Instant::now() + Duration::from_secs(5)),
        Some(limit),
        &Arc::new(AtomicBool::new(false)),
    )
    .expect("run bounded process")
}

#[test]
fn absolute_deadline_stops_running_child() {
    let temporary = TempDir::new().expect("create temporary directory");
    let args = ["-c".to_owned(), "sleep 30".to_owned()];
    let started = Instant::now();

    let output = run_process_until(
        Path::new("/bin/sh"),
        &args,
        temporary.path(),
        Some(started + Duration::from_millis(100)),
        Some(32),
        &Arc::new(AtomicBool::new(false)),
    )
    .expect("run process with deadline");

    assert_eq!(output.termination, ProcessTermination::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn cancellation_before_spawn_does_not_run_the_child() {
    let temporary = TempDir::new().expect("create temporary directory");
    let marker = temporary.path().join("child-ran");
    let script = temporary.path().join("write-marker");
    fs::write(
        &script,
        format!("#!/bin/sh\nprintf ran > {}\n", marker.display()),
    )
    .expect("write process fixture");
    let mut permissions = fs::metadata(&script)
        .expect("read fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make fixture executable");
    let cancelled = Arc::new(AtomicBool::new(true));

    let result = run_process(&script, &[], temporary.path(), None, &cancelled);

    assert!(result.is_err_and(|error| matches!(error, crate::error::SpecLoopError::Interrupted)));
    assert!(!marker.exists());
    assert!(ensure_not_cancelled(&cancelled).is_err());
}

#[test]
fn timeout_after_direct_child_exit_terminates_pipe_holding_descendants() {
    let temporary = TempDir::new().expect("create temporary directory");
    let script = temporary.path().join("spawn-detached-child");
    let child_pid_path = temporary.path().join("child-pid");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 30 &\necho $! > {}\nexit 0\n",
            child_pid_path.display()
        ),
    )
    .expect("write process fixture");
    let mut permissions = fs::metadata(&script)
        .expect("read fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make fixture executable");
    let started = Instant::now();
    let script_arg = [script.as_os_str()];
    let output = run_process_with_os_args(
        Path::new("/bin/sh"),
        &script_arg,
        temporary.path(),
        Some(Duration::from_millis(150)),
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .expect("run timed process");
    assert_eq!(output.termination, ProcessTermination::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5));
    let child_pid = fs::read_to_string(child_pid_path)
        .expect("read descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse descendant PID");
    assert_process_terminated(child_pid);
}

#[test]
fn timeout_terminates_descendants_holding_output_pipes() {
    let temporary = TempDir::new().expect("create temporary directory");
    let script = temporary.path().join("spawn-child");
    let child_pid_path = temporary.path().join("child-pid");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 30 &\necho $! > {}\nwait\n",
            child_pid_path.display()
        ),
    )
    .expect("write process fixture");
    let mut permissions = fs::metadata(&script)
        .expect("read fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make fixture executable");
    let started = Instant::now();
    let script_arg = [script.as_os_str()];
    let output = run_process_with_os_args(
        Path::new("/bin/sh"),
        &script_arg,
        temporary.path(),
        Some(Duration::from_millis(100)),
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .expect("run timed process");
    assert_eq!(output.termination, ProcessTermination::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5));
    let child_pid = fs::read_to_string(child_pid_path)
        .expect("read descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse descendant PID");
    assert_process_terminated(child_pid);
}

fn assert_process_terminated(pid: u32) {
    let process_stat = Path::new("/proc").join(pid.to_string()).join("stat");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs::read_to_string(&process_stat) {
            Ok(stat) => {
                let state = stat
                    .rsplit_once(')')
                    .map(|(_, fields)| fields.trim_start().chars().next());
                if state == Some(Some('Z')) {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("inspect child process: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "descendant process {pid} survived timeout"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn spawn_observer_counts_only_successful_process_spawns() {
    let temporary = TempDir::new().expect("create spawn observer fixture");
    let spawned = AtomicUsize::new(0);
    let output = run_process_until_with_spawn_observer(
        Path::new("/bin/sh"),
        &["-c".to_owned(), "exit 0".to_owned()],
        temporary.path(),
        None,
        None,
        &Arc::new(AtomicBool::new(false)),
        || {
            spawned.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    )
    .expect("run successful observed process");
    assert_eq!(output.termination, ProcessTermination::Exited);
    assert_eq!(spawned.load(Ordering::Relaxed), 1);

    let failed = run_process_until_with_spawn_observer(
        &temporary.path().join("missing-executable"),
        &[],
        temporary.path(),
        None,
        None,
        &Arc::new(AtomicBool::new(false)),
        || {
            spawned.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    );
    assert!(failed.is_err());
    assert_eq!(spawned.load(Ordering::Relaxed), 1);
}
