use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use openidconnect::{AuthorizationCode, RedirectUrl};
use url::Url;

use super::Registration;
use crate::error::{Result, SpecLoopError, io_error};

pub(super) const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CALLBACK_BYTES: usize = 16 * 1024;

pub(super) fn callback_listener() -> Result<(TcpListener, u16, RedirectUrl)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|source| io_error("bind OAuth callback listener", "127.0.0.1", source))?;
    listener
        .set_nonblocking(true)
        .map_err(|source| io_error("configure OAuth callback listener", "127.0.0.1", source))?;
    let port = listener
        .local_addr()
        .map_err(|source| io_error("inspect OAuth callback listener", "127.0.0.1", source))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let redirect_url = RedirectUrl::new(redirect)
        .map_err(|error| SpecLoopError::Config(format!("invalid OAuth callback URL: {error}")))?;
    Ok((listener, port, redirect_url))
}

pub(super) fn validate_callback(
    callback: Callback,
    expected_state: &str,
    existing: Option<&Registration>,
) -> Result<(AuthorizationCode, String)> {
    if callback.state != expected_state {
        return Err(SpecLoopError::Config(
            "OAuth callback state validation failed".to_owned(),
        ));
    }
    if callback.error.is_some() {
        return Err(SpecLoopError::Workflow(
            "OpenAI authorization was denied or failed; run `spec-loop auth login` again"
                .to_owned(),
        ));
    }
    let authorization_code = callback.code.ok_or_else(|| {
        SpecLoopError::Config("OAuth callback did not contain an authorization code".to_owned())
    })?;
    let issued_client_id = match (existing, callback.client_id) {
        (Some(registration), Some(supplied)) if supplied != registration.client_id => {
            return Err(SpecLoopError::Config(
                "OAuth callback client ID does not match selected registration; run `spec-loop auth login` again".to_owned(),
            ));
        }
        (Some(registration), _) => registration.client_id.clone(),
        (None, Some(supplied)) if !supplied.trim().is_empty() => supplied,
        (None, _) => {
            return Err(SpecLoopError::Config(
                "OpenAI authorization callback omitted the dynamically issued client ID".to_owned(),
            ));
        }
    };
    Ok((AuthorizationCode::new(authorization_code), issued_client_id))
}

pub(super) struct Callback {
    pub(super) state: String,
    pub(super) code: Option<String>,
    pub(super) client_id: Option<String>,
    pub(super) error: Option<String>,
}

pub(super) fn await_callback(
    listener: TcpListener,
    port: u16,
    cancelled: &Arc<AtomicBool>,
) -> Result<Callback> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let deadline = Instant::now()
        .checked_add(CALLBACK_TIMEOUT)
        .ok_or_else(|| SpecLoopError::Config("OAuth callback deadline overflow".to_owned()))?;
    let worker_cancelled = Arc::clone(cancelled);
    let stop_worker = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop_worker);
    let worker = thread::Builder::new()
        .name("spec-loop-oauth-callback".to_owned())
        .spawn(move || {
            callback_worker(
                &listener,
                &sender,
                &worker_cancelled,
                &worker_stop,
                deadline,
            );
        })
        .map_err(|source| io_error("start OAuth callback worker", "127.0.0.1", source))?;
    loop {
        if worker.is_finished() {
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => {
                worker.join().map_err(|_| {
                    SpecLoopError::Workflow(
                        "OAuth callback worker terminated unexpectedly".to_owned(),
                    )
                })?;
                return result;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if cancelled.load(Ordering::Relaxed) {
            stop_worker.store(true, Ordering::Relaxed);
            worker.join().map_err(|_| {
                SpecLoopError::Workflow("OAuth callback worker terminated unexpectedly".to_owned())
            })?;
            return Err(SpecLoopError::Interrupted);
        }
        if Instant::now() >= deadline {
            stop_worker.store(true, Ordering::Relaxed);
            worker.join().map_err(|_| {
                SpecLoopError::Workflow("OAuth callback worker terminated unexpectedly".to_owned())
            })?;
            return Err(SpecLoopError::Timeout(format!(
                "OAuth callback on 127.0.0.1:{port} expired after five minutes"
            )));
        }
    }
    worker.join().map_err(|_| {
        SpecLoopError::Workflow("OAuth callback worker terminated unexpectedly".to_owned())
    })?;
    receiver
        .recv()
        .map_err(|_| SpecLoopError::Workflow("OAuth callback ended without a result".to_owned()))?
}

fn callback_worker(
    listener: &TcpListener,
    sender: &std::sync::mpsc::Sender<Result<Callback>>,
    cancelled: &AtomicBool,
    stop: &AtomicBool,
    deadline: Instant,
) {
    let result = loop {
        if cancelled.load(Ordering::Relaxed) {
            break Err(SpecLoopError::Interrupted);
        }
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break Err(SpecLoopError::Timeout(
                "OAuth callback listener stopped".to_owned(),
            ));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Err(source) = stream.set_read_timeout(Some(Duration::from_millis(50))) {
                    break Err(io_error(
                        "configure OAuth callback stream",
                        "127.0.0.1",
                        source,
                    ));
                }
                break parse_callback_interruptible(&mut stream, cancelled, stop, Some(deadline));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(source) => break Err(io_error("accept OAuth callback", "127.0.0.1", source)),
        }
    };
    if let Err(error) = sender.send(result)
        && let Err(send_error) = error.0
    {
        eprintln!("spec-loop OAuth callback failed: {send_error}");
    }
}

#[cfg(test)]
pub(super) fn parse_callback(stream: &mut TcpStream) -> Result<Callback> {
    let never_cancelled = AtomicBool::new(false);
    parse_callback_interruptible(stream, &never_cancelled, &never_cancelled, None)
}

fn parse_callback_interruptible(
    stream: &mut TcpStream,
    cancelled: &AtomicBool,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<Callback> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(SpecLoopError::Interrupted);
        }
        if stop.load(Ordering::Relaxed) {
            return Err(SpecLoopError::Timeout(
                "OAuth callback listener stopped".to_owned(),
            ));
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            return Err(SpecLoopError::Timeout(
                "OAuth callback listener stopped".to_owned(),
            ));
        }
        let read = match stream.read(&mut buffer) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(source) => return Err(io_error("read OAuth callback", "127.0.0.1", source)),
        };
        if read == 0 {
            if cancelled.load(Ordering::Relaxed) {
                return Err(SpecLoopError::Interrupted);
            }
            if stop.load(Ordering::Relaxed) {
                return Err(SpecLoopError::Timeout(
                    "OAuth callback listener stopped".to_owned(),
                ));
            }
            return Err(SpecLoopError::Config(
                "OAuth callback ended before HTTP headers completed".to_owned(),
            ));
        }
        request.try_reserve(read).map_err(|error| {
            SpecLoopError::Workflow(format!("retain OAuth callback headers: {error}"))
        })?;
        request.extend_from_slice(&buffer[..read]);
        if request.len() > MAX_CALLBACK_BYTES {
            return Err(SpecLoopError::Config(
                "OAuth callback headers exceeded 16 KiB".to_owned(),
            ));
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let callback = parse_callback_request(&request)?;
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nAuthorization received. Return to Spec Loop.")
        .map_err(|source| io_error("respond to OAuth callback", "127.0.0.1", source))?;
    Ok(callback)
}

fn parse_callback_request(request: &[u8]) -> Result<Callback> {
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| SpecLoopError::Config("OAuth callback headers are incomplete".to_owned()))?;
    let body_start = header_end.checked_add(4).ok_or_else(|| {
        SpecLoopError::Config("OAuth callback header length overflowed".to_owned())
    })?;
    if body_start != request.len() {
        return Err(SpecLoopError::Config(
            "OAuth callback request body is not allowed".to_owned(),
        ));
    }
    let text = std::str::from_utf8(&request[..header_end]).map_err(|error| {
        SpecLoopError::Config(format!("OAuth callback headers are not UTF-8: {error}"))
    })?;
    let Some(line) = text.lines().next() else {
        return Err(SpecLoopError::Config(
            "OAuth callback request line is missing".to_owned(),
        ));
    };
    for header in text.lines().skip(1) {
        let (name, value) = header.split_once(':').ok_or_else(|| {
            SpecLoopError::Config("OAuth callback contains malformed HTTP header".to_owned())
        })?;
        if name.eq_ignore_ascii_case("content-length") && value.trim() != "0"
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            return Err(SpecLoopError::Config(
                "OAuth callback request body is not allowed".to_owned(),
            ));
        }
    }
    let mut parts = line.split_ascii_whitespace();
    let method = parts.next();
    let target = parts.next();
    let version = parts.next();
    if method != Some("GET") || version != Some("HTTP/1.1") || parts.next().is_some() {
        return Err(SpecLoopError::Config(
            "OAuth callback must use a bodyless HTTP/1.1 GET request".to_owned(),
        ));
    }
    let target = target
        .ok_or_else(|| SpecLoopError::Config("OAuth callback target is missing".to_owned()))?;
    let callback_url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|error| SpecLoopError::Config(format!("invalid OAuth callback URL: {error}")))?;
    parse_callback_parameters(&callback_url)
}

fn parse_callback_parameters(callback_url: &Url) -> Result<Callback> {
    if callback_url.path() != CALLBACK_PATH {
        return Err(SpecLoopError::Config(
            "OAuth callback path did not match expected endpoint".to_owned(),
        ));
    }
    if callback_url.query().is_none() {
        return Err(SpecLoopError::Config(
            "OAuth callback query is missing".to_owned(),
        ));
    }
    let mut query = std::collections::HashMap::new();
    for (name, value) in callback_url.query_pairs() {
        if query.insert(name.to_string(), value.to_string()).is_some() {
            return Err(SpecLoopError::Config(
                "OAuth callback contains duplicate parameters".to_owned(),
            ));
        }
    }
    let state = query
        .remove("state")
        .ok_or_else(|| SpecLoopError::Config("OAuth callback state is missing".to_owned()))?;
    let code = query.remove("code");
    let client_id = query.remove("client_id");
    let error = query.remove("error");
    // OpenAI may echo granted scopes; the token response remains authoritative.
    let _callback_scope = query.remove("scope");
    if !query.is_empty() || code.is_some() == error.is_some() {
        return Err(SpecLoopError::Config(
            "OAuth callback parameters are invalid".to_owned(),
        ));
    }
    Ok(Callback {
        state,
        code,
        client_id,
        error,
    })
}
