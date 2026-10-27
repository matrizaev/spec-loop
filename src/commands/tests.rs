use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::{
    ProcessTermination, ensure_not_cancelled, run_process, run_process_until,
    run_process_with_os_args,
};

#[test]
fn bounded_capture_drains_both_streams_and_marks_truncation() {
    let temporary = TempDir::new().expect("create temporary directory");
    let args = [
        "-c".to_owned(),
        "head -c 65536 /dev/zero; head -c 65536 /dev/zero >&2".to_owned(),
    ];

    let output = run_process_until(
        Path::new("/bin/sh"),
        &args,
        temporary.path(),
        Some(Instant::now() + Duration::from_secs(5)),
        Some(6),
        &Arc::new(AtomicBool::new(false)),
    )
    .expect("run bounded process");

    assert!(output.status.is_some_and(|status| status.success()));
    assert_eq!(output.stdout, [0; 3]);
    assert_eq!(output.stderr, [0; 3]);
    assert!(output.stdout_truncated);
    assert!(output.stderr_truncated);
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
