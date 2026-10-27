use std::ffi::{OsStr, OsString};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Result, SpecLoopError, io_error};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessTermination {
    /// The child exited without a timeout or cancellation.
    Exited,
    /// The configured process timeout expired.
    TimedOut,
    /// Cancellation terminated the child after it started.
    Interrupted,
}

/// Child exit status and captured streams produced by process supervision.
#[derive(Clone, Debug)]
pub struct ChildOutput {
    /// Child exit status, when the process was waited successfully.
    pub status: Option<ExitStatus>,
    /// Captured stdout, bounded only for built-in tool invocations.
    pub stdout: Vec<u8>,
    /// Captured stderr, bounded only for built-in tool invocations.
    pub stderr: Vec<u8>,
    /// Whether stdout exceeded its assigned capture allowance.
    pub stdout_truncated: bool,
    /// Whether stderr exceeded its assigned capture allowance.
    pub stderr_truncated: bool,
    /// Why process supervision stopped; captured output is retained for every outcome.
    pub termination: ProcessTermination,
}

#[derive(Clone, Copy)]
struct ProcessOptions<'a> {
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    max_output_bytes: Option<usize>,
    environment: &'a [(OsString, OsString)],
    cancelled: &'a Arc<AtomicBool>,
}

struct ReaderWait<'a> {
    stdout: &'a thread::JoinHandle<std::io::Result<CapturedOutput>>,
    stderr: &'a thread::JoinHandle<std::io::Result<CapturedOutput>>,
    child: &'a mut std::process::Child,
    program: &'a Path,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    started: Instant,
    cancelled: &'a Arc<AtomicBool>,
    termination: &'a mut ProcessTermination,
}

pub fn run_process(
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Option<Duration>,
    cancelled: &Arc<AtomicBool>,
) -> Result<ChildOutput> {
    let deadline = timeout
        .map(|duration| {
            Instant::now().checked_add(duration).ok_or_else(|| {
                SpecLoopError::Config("process timeout exceeds monotonic clock range".to_owned())
            })
        })
        .transpose()?;
    run_process_until(program, args, cwd, deadline, None, cancelled)
}

/// Runs a process under an absolute role deadline with an optional combined output cap.
pub fn run_process_until(
    program: &Path,
    args: &[String],
    cwd: &Path,
    deadline: Option<Instant>,
    max_output_bytes: Option<usize>,
    cancelled: &Arc<AtomicBool>,
) -> Result<ChildOutput> {
    run_process_with_options(
        program,
        args,
        cwd,
        ProcessOptions {
            timeout: None,
            deadline,
            max_output_bytes,
            environment: &[],
            cancelled,
        },
    )
}

#[cfg(test)]
fn run_process_with_os_args(
    program: &Path,
    args: &[impl AsRef<OsStr>],
    cwd: &Path,
    timeout: Option<Duration>,
    environment: &[(OsString, OsString)],
    cancelled: &Arc<AtomicBool>,
) -> Result<ChildOutput> {
    run_process_with_options(
        program,
        args,
        cwd,
        ProcessOptions {
            timeout,
            deadline: None,
            max_output_bytes: None,
            environment,
            cancelled,
        },
    )
}

fn run_process_with_options(
    program: &Path,
    args: &[impl AsRef<OsStr>],
    cwd: &Path,
    options: ProcessOptions<'_>,
) -> Result<ChildOutput> {
    let ProcessOptions {
        timeout,
        deadline,
        max_output_bytes,
        environment,
        cancelled,
    } = options;
    ensure_not_cancelled(cancelled)?;
    let started = Instant::now();
    if deadline.is_some_and(|limit| started >= limit) {
        return Ok(ChildOutput {
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            termination: ProcessTermination::TimedOut,
        });
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    // A private process group lets timeout/interruption terminate descendants too.
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|source| io_error("start child process", program, source))?;
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child, program)?;
        child
            .wait()
            .map_err(|source| io_error("wait for child without stdout pipe", program, source))?;
        return Err(SpecLoopError::Process(format!(
            "{} stdout pipe was not created",
            program.display()
        )));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child(&mut child, program)?;
        child
            .wait()
            .map_err(|source| io_error("wait for child without stderr pipe", program, source))?;
        return Err(SpecLoopError::Process(format!(
            "{} stderr pipe was not created",
            program.display()
        )));
    };
    let (stdout_limit, stderr_limit) = max_output_bytes.map_or((None, None), |limit| {
        (Some(limit / 2 + limit % 2), Some(limit / 2))
    });
    let stdout_reader = thread::spawn(move || read_captured(stdout, stdout_limit));
    let stderr_reader = thread::spawn(move || read_captured(stderr, stderr_limit));
    let (status, mut termination) =
        wait_for_child(&mut child, program, timeout, deadline, started, cancelled)?;
    wait_for_readers(ReaderWait {
        stdout: &stdout_reader,
        stderr: &stderr_reader,
        child: &mut child,
        program,
        timeout,
        deadline,
        started,
        cancelled,
        termination: &mut termination,
    })?;
    let stdout = join_reader(stdout_reader, program)?;
    let stderr = join_reader(stderr_reader, program)?;
    Ok(ChildOutput {
        status: Some(status),
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        termination,
    })
}

fn wait_for_child(
    child: &mut std::process::Child,
    program: &Path,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    started: Instant,
    cancelled: &AtomicBool,
) -> Result<(ExitStatus, ProcessTermination)> {
    loop {
        let termination = if cancelled.load(Ordering::Relaxed) {
            Some(ProcessTermination::Interrupted)
        } else if deadline_reached(timeout, deadline, started) {
            Some(ProcessTermination::TimedOut)
        } else {
            None
        };
        if let Some(termination) = termination {
            terminate_child(child, program)?;
            let status = child
                .wait()
                .map_err(|source| io_error("wait for stopped child process", program, source))?;
            return Ok((status, termination));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|source| io_error("wait for child process", program, source))?
        {
            return Ok((status, ProcessTermination::Exited));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn terminate_child(child: &mut std::process::Child, program: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        terminate_process_group(child, program)?;
    }
    #[cfg(not(unix))]
    {
        terminate_direct_child(child, program)?;
    }
    Ok(())
}

#[cfg(unix)]
fn terminate_process_group(child: &mut std::process::Child, program: &Path) -> Result<()> {
    // Killing the whole group closes inherited pipes before reader threads join.
    let process_group = format!("-{}", child.id());
    let output = Command::new("/bin/kill")
        .args(["-KILL", "--", &process_group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|source| io_error("terminate child process group", program, source))?;
    if output.status.success() {
        return Ok(());
    }
    if child
        .try_wait()
        .map_err(|source| io_error("inspect child after group termination", program, source))?
        .is_some()
    {
        return Ok(());
    }
    Err(SpecLoopError::Process(format!(
        "could not terminate process group for {} (group {})",
        program.display(),
        child.id()
    )))
}

#[cfg(not(unix))]
fn terminate_direct_child(child: &mut std::process::Child, program: &Path) -> Result<()> {
    if child
        .try_wait()
        .map_err(|source| io_error("inspect child before termination", program, source))?
        .is_some()
    {
        return Ok(());
    }
    match child.kill() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            if child
                .try_wait()
                .map_err(|source| {
                    io_error("inspect child after termination race", program, source)
                })?
                .is_some()
            {
                Ok(())
            } else {
                Err(io_error("terminate child process", program, error))
            }
        }
        Err(source) => Err(io_error("terminate child process", program, source)),
    }
}

fn wait_for_readers(wait: ReaderWait<'_>) -> Result<()> {
    let ReaderWait {
        stdout,
        stderr,
        child,
        program,
        timeout,
        deadline,
        started,
        cancelled,
        termination,
    } = wait;
    while !stdout.is_finished() || !stderr.is_finished() {
        if cancelled.load(Ordering::Relaxed) && *termination != ProcessTermination::Interrupted {
            terminate_child(child, program)?;
            *termination = ProcessTermination::Interrupted;
        }
        if *termination == ProcessTermination::Exited
            && deadline_reached(timeout, deadline, started)
        {
            terminate_child(child, program)?;
            *termination = ProcessTermination::TimedOut;
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_captured(mut reader: impl Read, limit: Option<usize>) -> std::io::Result<CapturedOutput> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let retained = match limit {
            Some(limit) => {
                let remaining = limit.checked_sub(output.len()).ok_or_else(|| {
                    std::io::Error::other("captured output exceeded its configured limit")
                })?;
                let retained = read.min(remaining);
                truncated |= retained < read;
                retained
            }
            None => read,
        };
        output
            .try_reserve(retained)
            .map_err(std::io::Error::other)?;
        output.extend_from_slice(&buffer[..retained]);
    }
    Ok(CapturedOutput {
        bytes: output,
        truncated,
    })
}

fn deadline_reached(
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    started: Instant,
) -> bool {
    deadline.is_some_and(|limit| Instant::now() >= limit)
        || timeout.is_some_and(|limit| started.elapsed() >= limit)
}

fn join_reader(
    handle: thread::JoinHandle<std::io::Result<CapturedOutput>>,
    program: &Path,
) -> Result<CapturedOutput> {
    let result = handle.join().map_err(|_| {
        SpecLoopError::Process(format!("output reader for {} panicked", program.display()))
    })?;
    result.map_err(|source| io_error("read child output", program, source))
}

/// Returns [`SpecLoopError::Interrupted`] when cancellation has been requested.
pub fn ensure_not_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        Err(SpecLoopError::Interrupted)
    } else {
        Ok(())
    }
}

pub fn status_success(output: &ChildOutput) -> bool {
    output.termination == ProcessTermination::Exited
        && output.status.is_some_and(|status| status.success())
        && !output.stdout_truncated
        && !output.stderr_truncated
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
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

        assert!(
            result.is_err_and(|error| matches!(error, crate::error::SpecLoopError::Interrupted))
        );
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
}
