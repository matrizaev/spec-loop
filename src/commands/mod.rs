use std::ffi::{OsStr, OsString};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Result, SpecLoopError, io_error};

#[cfg(all(test, target_os = "linux"))]
mod tests;

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
    /// Captured stdout bytes, bounded by the invocation's shared output allowance.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes, bounded by the invocation's shared output allowance.
    pub stderr: Vec<u8>,
    /// Whether bytes from stdout were discarded by the shared capture allowance.
    pub stdout_truncated: bool,
    /// Whether bytes from stderr were discarded by the shared capture allowance.
    pub stderr_truncated: bool,
    /// Why process supervision stopped; captured output is retained for every outcome.
    pub termination: ProcessTermination,
}

#[derive(Clone, Copy)]
struct ProcessOptions<'a> {
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
    deadline: Option<Instant>,
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
    let deadline = process_deadline(timeout)?;
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
    run_process_until_with_spawn_observer(
        program,
        args,
        cwd,
        deadline,
        max_output_bytes,
        cancelled,
        || Ok(()),
    )
}

/// Runs a process and invokes the observer immediately after its successful spawn.
pub fn run_process_until_with_spawn_observer(
    program: &Path,
    args: &[String],
    cwd: &Path,
    deadline: Option<Instant>,
    max_output_bytes: Option<usize>,
    cancelled: &Arc<AtomicBool>,
    on_spawn: impl FnOnce() -> Result<()>,
) -> Result<ChildOutput> {
    run_process_with_options(
        program,
        args,
        cwd,
        ProcessOptions {
            deadline,
            max_output_bytes,
            environment: &[],
            cancelled,
        },
        on_spawn,
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
            deadline: process_deadline(timeout)?,
            max_output_bytes: None,
            environment,
            cancelled,
        },
        || Ok(()),
    )
}

fn run_process_with_options(
    program: &Path,
    args: &[impl AsRef<OsStr>],
    cwd: &Path,
    options: ProcessOptions<'_>,
    on_spawn: impl FnOnce() -> Result<()>,
) -> Result<ChildOutput> {
    let ProcessOptions {
        deadline,
        max_output_bytes,
        environment,
        cancelled,
    } = options;
    ensure_not_cancelled(cancelled)?;
    if deadline_reached(deadline) {
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
    if let Err(error) = on_spawn() {
        terminate_child(&mut child, program)?;
        child.wait().map_err(|source| {
            io_error(
                "wait for child after spawn observer failure",
                program,
                source,
            )
        })?;
        return Err(error);
    }
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
    let capture_budget = max_output_bytes.map(CaptureBudget::new).map(Arc::new);
    let stdout_budget = capture_budget.as_ref().map(Arc::clone);
    let stdout_reader = thread::spawn(move || read_captured(stdout, stdout_budget.as_deref()));
    let stderr_reader = thread::spawn(move || read_captured(stderr, capture_budget.as_deref()));
    let (status, mut termination) = wait_for_child(&mut child, program, deadline, cancelled)?;
    wait_for_readers(ReaderWait {
        stdout: &stdout_reader,
        stderr: &stderr_reader,
        child: &mut child,
        program,
        deadline,
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
    deadline: Option<Instant>,
    cancelled: &AtomicBool,
) -> Result<(ExitStatus, ProcessTermination)> {
    loop {
        let termination = if cancelled.load(Ordering::Relaxed) {
            Some(ProcessTermination::Interrupted)
        } else if deadline_reached(deadline) {
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
        deadline,
        cancelled,
        termination,
    } = wait;
    while !stdout.is_finished() || !stderr.is_finished() {
        if cancelled.load(Ordering::Relaxed) && *termination != ProcessTermination::Interrupted {
            terminate_child(child, program)?;
            *termination = ProcessTermination::Interrupted;
        }
        if *termination == ProcessTermination::Exited && deadline_reached(deadline) {
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

struct CaptureBudget {
    limit: usize,
    retained: AtomicUsize,
}

impl CaptureBudget {
    const fn new(limit: usize) -> Self {
        Self {
            limit,
            retained: AtomicUsize::new(0),
        }
    }

    fn claim(&self, requested: usize) -> std::io::Result<usize> {
        let mut retained = self.retained.load(Ordering::Relaxed);
        loop {
            let remaining = self.limit.checked_sub(retained).ok_or_else(|| {
                std::io::Error::other("captured output exceeded its configured limit")
            })?;
            let claimed = requested.min(remaining);
            let updated = retained
                .checked_add(claimed)
                .ok_or_else(|| std::io::Error::other("captured output byte count overflowed"))?;
            match self.retained.compare_exchange_weak(
                retained,
                updated,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(claimed),
                Err(observed) => retained = observed,
            }
        }
    }
}

fn read_captured(
    mut reader: impl Read,
    budget: Option<&CaptureBudget>,
) -> std::io::Result<CapturedOutput> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let retained = match budget {
            Some(budget) => budget.claim(read)?,
            None => read,
        };
        truncated |= retained < read;
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

fn process_deadline(timeout: Option<Duration>) -> Result<Option<Instant>> {
    timeout
        .map(|duration| {
            Instant::now().checked_add(duration).ok_or_else(|| {
                SpecLoopError::Config("process timeout exceeds monotonic clock range".to_owned())
            })
        })
        .transpose()
}

fn deadline_reached(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|limit| Instant::now() >= limit)
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
