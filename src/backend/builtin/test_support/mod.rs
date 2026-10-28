use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::provider::request_with_retries;
use crate::config::Profile;

mod tests;

/// Ordinary fixture bound, shared by server I/O and client operations.
pub(super) const WATCHDOG: Duration = Duration::from_secs(10);
/// Minimal ordered start events required before a completed Responses stream.
pub(super) const RESPONSE_STARTED: &str = concat!(
    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\"}}\n\n",
    "event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\"}}\n\n"
);

/// Formats one complete SSE frame, leaving deliberately malformed streams to their tests.
pub(super) fn sse_event(event_type: &str, event: &Value) -> String {
    format!("event: {event_type}\ndata: {event}\n\n")
}

/// Prepends valid start events to a terminal completion fixture.
pub(super) fn completed_sse(event: &Value) -> String {
    format!(
        "{RESPONSE_STARTED}{}",
        sse_event("response.completed", event)
    )
}

/// One scripted HTTP reply; bodies may intentionally contain invalid provider output.
pub(super) struct Reply {
    status: &'static str,
    content_type: &'static str,
    /// Complete extra header lines, including CRLF terminators.
    pub(super) headers: &'static str,
    body: String,
}

impl Reply {
    /// Constructs a successful streamed reply.
    pub(super) fn sse(body: String) -> Self {
        Self {
            status: "200 OK",
            content_type: "text/event-stream",
            headers: "",
            body,
        }
    }

    /// Constructs a JSON reply, with immediate retry guidance for transient HTTP fixtures.
    pub(super) fn http(status: &'static str, body: String) -> Self {
        Self {
            status,
            content_type: "application/json",
            headers: if status.starts_with("429 ") || status.starts_with("503 ") {
                "Retry-After: 0\r\n"
            } else {
                ""
            },
            body,
        }
    }
}

/// Owns server cleanup even when the test returns early or unwinds.
/// Accept, partial reads, and writes all share one absolute watchdog, so joining is bounded.
pub(super) struct ProviderServer {
    worker: Option<thread::JoinHandle<io::Result<Vec<String>>>>,
    finished: Arc<AtomicBool>,
    deadline: Instant,
}

impl ProviderServer {
    /// Returns the server's absolute bound for the matching client operation.
    pub(super) const fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Signals completion, joins the worker, and reports I/O or missing scripted requests.
    pub(super) fn join(mut self) -> io::Result<Vec<String>> {
        self.finish()
    }

    fn finish(&mut self) -> io::Result<Vec<String>> {
        self.finished.store(true, Ordering::Relaxed);
        let Some(worker) = self.worker.take() else {
            return Ok(Vec::new());
        };
        worker
            .join()
            .map_err(|_| io::Error::other("provider fixture worker panicked"))?
    }
}

impl Drop for ProviderServer {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            if thread::panicking() {
                eprintln!("provider fixture cleanup: {error}");
            } else {
                panic!("provider fixture cleanup: {error}");
            }
        }
    }
}

/// Starts an owned loopback fixture; unexpected requests are captured and receive HTTP 400.
pub(super) fn serve_responses(
    listener: TcpListener,
    replies: impl IntoIterator<Item = Reply>,
) -> ProviderServer {
    serve_until(listener, replies.into_iter().collect(), WATCHDOG, None)
}

fn serve_until(
    listener: TcpListener,
    replies: Vec<Reply>,
    timeout: Duration,
    quiet: Option<Duration>,
) -> ProviderServer {
    listener
        .set_nonblocking(true)
        .expect("nonblocking fixture listener");
    let deadline = Instant::now()
        .checked_add(timeout)
        .expect("fixture deadline");
    let finished = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&finished);
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut last_request = Instant::now();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    requests.push(read_request_until(&mut stream, deadline)?);
                    let fallback = Reply::http(
                        "400 Bad Request",
                        r#"{"error":{"code":"unexpected_request"}}"#.to_owned(),
                    );
                    let reply = replies
                        .get(requests.len().checked_sub(1).expect("request index"))
                        .unwrap_or(&fallback);
                    let response = format!(
                        "HTTP/1.1 {}\r\nContent-Type: {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reply.status,
                        reply.content_type,
                        reply.headers,
                        reply.body.len(),
                        reply.body
                    );
                    write_until(&mut stream, response.as_bytes(), deadline)?;
                    last_request = Instant::now();
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if stopped.load(Ordering::Relaxed)
                        || quiet.is_some_and(|limit| {
                            !requests.is_empty() && last_request.elapsed() >= limit
                        })
                    {
                        break;
                    }
                    remaining(deadline)?;
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => return Err(error),
            }
        }
        if requests.len() < replies.len() {
            return Err(io::Error::other(format!(
                "provider fixture expected {} requests, received {}",
                replies.len(),
                requests.len()
            )));
        }
        Ok(requests)
    });
    ProviderServer {
        worker: Some(worker),
        finished,
        deadline,
    }
}

/// Bounded accept seam for tests whose synchronization or malformed framing must stay explicit.
pub(super) fn accept_until(listener: &TcpListener, deadline: Instant) -> io::Result<TcpStream> {
    listener.set_nonblocking(true)?;
    loop {
        remaining(deadline)?;
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error),
        }
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|time| !time.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "provider fixture watchdog expired"))
}

/// Writes a full reply without allowing partial writes to restart the watchdog.
pub(super) fn write_until(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let written = stream.write(bytes)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "provider fixture response closed",
            ));
        }
        bytes = bytes
            .get(written..)
            .ok_or_else(|| io::Error::other("invalid fixture write count"))?;
    }
    Ok(())
}

/// Captures one complete length-delimited request under the original absolute watchdog.
pub(super) fn read_request_until(stream: &mut TcpStream, deadline: Instant) -> io::Result<String> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 2048];
    loop {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "provider request closed early",
            ));
        }
        request.extend_from_slice(
            buffer
                .get(..count)
                .ok_or_else(|| io::Error::other("invalid fixture read count"))?,
        );
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(
            request
                .get(..header_end)
                .ok_or_else(|| io::Error::other("invalid header boundary"))?,
        );
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>())
            })
            .ok_or_else(|| io::Error::other("missing fixture Content-Length"))?
            .map_err(io::Error::other)?;
        let end = header_end
            .checked_add(4)
            .and_then(|start| start.checked_add(length))
            .ok_or_else(|| io::Error::other("fixture Content-Length overflow"))?;
        if request.len() >= end {
            return String::from_utf8(request).map_err(io::Error::other);
        }
    }
}

/// Builds a validated built-in profile without credentials or external resources.
pub(super) fn builtin_test_profile() -> Profile {
    let profile: Profile = toml::from_str(
        r"schema_version=3
[specs]
active_globs=['specs/*.md']
id_regex='^specs/(?P<id>[0-9]+)\\.md$'
[agent]
backend='builtin'
model='fixture-model'
",
    )
    .expect("parse built-in test profile");
    profile.validate().expect("validate built-in test profile");
    profile
}

/// Sends one fake-token request and returns its result plus captured transport input.
pub(super) fn fake_stream_request(body: String) -> (crate::error::Result<Value>, String) {
    let (requests, result) = request_script(vec![Reply::sse(body)], WATCHDOG, None);
    (
        result,
        requests.into_iter().next().expect("provider request"),
    )
}

/// Verifies one transient streamed failure retries and recovers on the next scripted reply.
pub(super) fn assert_transient_sse_failure_retries(event_type: &str, event: &Value) {
    let failure = if event_type == "error" {
        sse_event(event_type, event)
    } else {
        format!("{RESPONSE_STARTED}{}", sse_event(event_type, event))
    };
    let mut failure = Reply::sse(failure);
    failure.headers = "Retry-After: 0\r\n";
    let success = Reply::sse(completed_sse(
        &json!({"type":"response.completed","response":{"status":"completed","output":[]}}),
    ));
    let (requests, result) = request_script(vec![failure, success], WATCHDOG, None);
    assert_eq!(
        result.expect("transient stream error should retry")["status"],
        "completed"
    );
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with("POST /v1/responses "))
    );
}

fn request_script(
    replies: Vec<Reply>,
    timeout: Duration,
    quiet: Option<Duration>,
) -> (Vec<String>, crate::error::Result<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    let address = listener.local_addr().expect("fixture address");
    let server = serve_until(listener, replies, timeout, quiet);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build provider client");
    let result = crate::runtime::block_on(request_with_retries(
        &client,
        &format!("http://{address}/v1/responses"),
        "oauth-only-fixture-token",
        &json!({"model":"fixture-model","stream":true}),
        Some(server.deadline()),
        &AtomicBool::new(false),
    ))
    .expect("provider runtime");
    (server.join().expect("join fake provider"), result)
}

/// Runs JSON retry fixtures with their existing three-second bound and short quiet window.
pub(super) fn run_fake_http_responses(
    responses: Vec<(&'static str, &'static str)>,
) -> (Vec<String>, crate::error::Result<Value>) {
    request_script(
        responses
            .into_iter()
            .map(|(status, body)| Reply::http(status, body.to_owned()))
            .collect(),
        Duration::from_secs(3),
        Some(Duration::from_millis(200)),
    )
}

/// Asserts fake requests use only the expected OAuth bearer credential.
pub(super) fn assert_oauth_only_requests(requests: &[String]) {
    assert!(requests.iter().all(|request| {
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer oauth-only-fixture-token")
            && !request.to_ascii_lowercase().contains("x-api-key")
    }));
}
