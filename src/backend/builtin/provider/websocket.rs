use std::sync::atomic::AtomicBool;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use url::Url;

use super::request::encode_request;
use super::{
    MAX_PROVIDER_RETRIES, RequestAttempt, SseState, StreamFailure, advance_retry_counter,
    ensure_active, request_once, wait_before_retry,
};
use crate::backend::builtin::usage::UsageReporter;
use crate::error::{Result, SpecLoopError};

/// Per-role transport state. The socket and its response cache never enter continuation state.
pub(in crate::backend::builtin) struct ConversationTransport {
    mode: TransportMode,
    socket: Option<Box<Socket>>,
    previous_response_id: Option<String>,
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum TransportMode {
    WebSocket,
    HttpSse,
}

impl Default for ConversationTransport {
    fn default() -> Self {
        Self {
            mode: TransportMode::WebSocket,
            socket: None,
            previous_response_id: None,
        }
    }
}

impl ConversationTransport {
    /// Drops connection-local cache after compaction while preserving a sticky HTTP fallback.
    pub(in crate::backend::builtin) fn reset_chain(&mut self) {
        self.socket = None;
        self.previous_response_id = None;
    }
}

pub(in crate::backend::builtin) struct ConversationProviderRequest<'a> {
    pub(in crate::backend::builtin) client: &'a reqwest::Client,
    pub(in crate::backend::builtin) endpoint: &'a str,
    pub(in crate::backend::builtin) token: &'a str,
    pub(in crate::backend::builtin) body: &'a Value,
    pub(in crate::backend::builtin) incremental_input: Option<&'a [Value]>,
    pub(in crate::backend::builtin) transport: &'a mut ConversationTransport,
    pub(in crate::backend::builtin) deadline: Option<Instant>,
    pub(in crate::backend::builtin) cancelled: &'a AtomicBool,
}

#[derive(Default)]
struct RetryState {
    count: u8,
    request_bytes: Option<Vec<u8>>,
    uses_previous_response_id: bool,
}

impl RetryState {
    fn discard_request(&mut self) {
        self.request_bytes = None;
        self.uses_previous_response_id = false;
    }
}

pub(in crate::backend::builtin) async fn request_with_conversation_observations(
    mut context: ConversationProviderRequest<'_>,
    reporter: &mut UsageReporter<'_>,
) -> Result<Value> {
    ensure_active(context.deadline, context.cancelled)?;
    let http_encoded = encode_request(context.body)?;
    let websocket_endpoint = if context.transport.mode == TransportMode::WebSocket {
        Some(websocket_endpoint(context.endpoint)?)
    } else {
        None
    };
    let mut retry = RetryState::default();

    loop {
        ensure_active(context.deadline, context.cancelled)?;
        if context.transport.mode == TransportMode::HttpSse {
            match send_http_request(&context, &http_encoded, reporter).await? {
                RequestAttempt::Completed(response) => return Ok(response),
                RequestAttempt::Retryable {
                    retry_after,
                    exhausted_error,
                } => {
                    retry.count = retry_after_or_fail(
                        retry.count,
                        retry_after.as_deref(),
                        exhausted_error,
                        context.deadline,
                        context.cancelled,
                    )
                    .await?;
                }
            }
            continue;
        }

        let endpoint = websocket_endpoint.as_deref().ok_or_else(|| {
            SpecLoopError::Workflow("OpenAI Responses WebSocket endpoint is unavailable".to_owned())
        })?;
        if !ensure_websocket_connection(&mut context, endpoint).await? {
            continue;
        }
        match send_websocket_request(&mut context, &mut retry, reporter).await? {
            WebSocketAttempt::Completed(response) => {
                context.transport.previous_response_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
                if context.transport.previous_response_id.is_none() {
                    context.transport.reset_chain();
                }
                return Ok(response);
            }
            WebSocketAttempt::Retryable { recover_full_input } => {
                if recover_full_input {
                    context.transport.reset_chain();
                    retry.discard_request();
                }
                retry.count = retry_after_or_fail(
                    retry.count,
                    None,
                    websocket_exhausted_error(),
                    context.deadline,
                    context.cancelled,
                )
                .await?;
            }
            WebSocketAttempt::UseHttpSse => {
                context.transport.mode = TransportMode::HttpSse;
                context.transport.reset_chain();
                retry.discard_request();
                retry.count = retry_after_or_fail(
                    retry.count,
                    None,
                    SpecLoopError::Workflow(format!(
                        "OpenAI Responses WebSocket continuation failed after {MAX_PROVIDER_RETRIES} retries"
                    )),
                    context.deadline,
                    context.cancelled,
                )
                .await?;
            }
        }
    }
}

async fn send_http_request(
    context: &ConversationProviderRequest<'_>,
    encoded: &[u8],
    reporter: &mut UsageReporter<'_>,
) -> Result<RequestAttempt> {
    let request = context
        .client
        .post(context.endpoint)
        .bearer_auth(context.token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(encoded.to_vec());
    request_once(request, context.deadline, context.cancelled, reporter).await
}

async fn ensure_websocket_connection(
    context: &mut ConversationProviderRequest<'_>,
    endpoint: &str,
) -> Result<bool> {
    if context.transport.socket.is_some() {
        return Ok(true);
    }
    match connect(endpoint, context.token, context.deadline, context.cancelled).await? {
        Connection::Connected(socket) => {
            context.transport.socket = Some(socket);
            Ok(true)
        }
        Connection::Unavailable => {
            context.transport.mode = TransportMode::HttpSse;
            context.transport.reset_chain();
            Ok(false)
        }
    }
}

async fn send_websocket_request(
    context: &mut ConversationProviderRequest<'_>,
    retry: &mut RetryState,
    reporter: &mut UsageReporter<'_>,
) -> Result<WebSocketAttempt> {
    if retry.request_bytes.is_none() {
        retry.uses_previous_response_id =
            context.transport.previous_response_id.is_some() && context.incremental_input.is_some();
        let input = if retry.uses_previous_response_id {
            context.incremental_input.unwrap_or_default()
        } else {
            context
                .body
                .get("input")
                .and_then(Value::as_array)
                .map_or(&[] as &[Value], Vec::as_slice)
        };
        retry.request_bytes = Some(encode_create_request(
            context.body,
            retry
                .uses_previous_response_id
                .then_some(context.transport.previous_response_id.as_deref())
                .flatten(),
            input,
        )?);
    }
    let bytes = retry.request_bytes.as_deref().ok_or_else(|| {
        SpecLoopError::Workflow("OpenAI Responses request encoding was lost".to_owned())
    })?;
    let text = std::str::from_utf8(bytes).map_err(|_| {
        SpecLoopError::Agent("OpenAI Responses request is not valid UTF-8".to_owned())
    })?;
    let socket = context.transport.socket.as_mut().ok_or_else(|| {
        SpecLoopError::Workflow("OpenAI Responses WebSocket connection was lost".to_owned())
    })?;
    let send = async {
        let sent = socket.send(Message::text(text)).await;
        if sent.is_ok() {
            reporter.snapshot.request_sent();
            reporter.report()?;
        }
        Ok::<_, SpecLoopError>(sent)
    };
    if let Err(error) = super::await_limited(send, context.deadline, context.cancelled).await?? {
        if is_clean_connection_close(&error) {
            return Ok(WebSocketAttempt::Retryable {
                recover_full_input: true,
            });
        }
        return Err(SpecLoopError::Workflow(
            "OpenAI Responses WebSocket request could not be sent".to_owned(),
        ));
    }
    read_websocket_response(
        socket,
        retry.uses_previous_response_id,
        context.deadline,
        context.cancelled,
        reporter,
    )
    .await
}

async fn retry_after_or_fail(
    retry_count: u8,
    retry_after: Option<&str>,
    exhausted_error: SpecLoopError,
    deadline: Option<Instant>,
    cancelled: &AtomicBool,
) -> Result<u8> {
    if retry_count >= MAX_PROVIDER_RETRIES {
        return Err(exhausted_error);
    }
    wait_before_retry(retry_count, retry_after, deadline, cancelled).await?;
    advance_retry_counter(retry_count)
}

fn websocket_exhausted_error() -> SpecLoopError {
    SpecLoopError::Workflow(format!(
        "OpenAI Responses WebSocket stream failed after {MAX_PROVIDER_RETRIES} retries"
    ))
}

fn websocket_endpoint(endpoint: &str) -> Result<String> {
    let mut parsed = Url::parse(endpoint).map_err(|_| {
        SpecLoopError::Config("OpenAI Responses endpoint is not a valid URL".to_owned())
    })?;
    let scheme = match parsed.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => {
            return Err(SpecLoopError::Config(
                "OpenAI Responses endpoint must use HTTP or HTTPS".to_owned(),
            ));
        }
    };
    parsed.set_scheme(scheme).map_err(|()| {
        SpecLoopError::Config("OpenAI Responses WebSocket endpoint is invalid".to_owned())
    })?;
    Ok(parsed.into())
}

fn encode_create_request(
    body: &Value,
    previous_response_id: Option<&str>,
    input: &[Value],
) -> Result<Vec<u8>> {
    let mut request = body.clone();
    let object = request.as_object_mut().ok_or_else(|| {
        SpecLoopError::Agent("OpenAI Responses request must be a JSON object".to_owned())
    })?;
    object.remove("stream");
    object.insert(
        "type".to_owned(),
        Value::String("response.create".to_owned()),
    );
    object.insert("input".to_owned(), Value::Array(input.to_vec()));
    if let Some(previous_response_id) = previous_response_id {
        object.insert(
            "previous_response_id".to_owned(),
            Value::String(previous_response_id.to_owned()),
        );
    } else {
        object.remove("previous_response_id");
    }
    encode_request(&request)
}

pub(super) fn response_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
}

enum Connection {
    Connected(Box<Socket>),
    Unavailable,
}

async fn connect(
    endpoint: &str,
    token: &str,
    deadline: Option<Instant>,
    cancelled: &AtomicBool,
) -> Result<Connection> {
    let mut request = endpoint.into_client_request().map_err(|_| {
        SpecLoopError::Config("OpenAI Responses WebSocket endpoint is invalid".to_owned())
    })?;
    let authorization = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        SpecLoopError::Workflow("ChatGPT access token is not a valid HTTP header".to_owned())
    })?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    let result = super::await_limited(
        connect_async_with_config(request, Some(response_websocket_config()), false),
        deadline,
        cancelled,
    )
    .await?;
    match result {
        Ok((socket, _response)) => Ok(Connection::Connected(Box::new(socket))),
        Err(WebSocketError::Http(response))
            if response.status().as_u16() == 401 || response.status().as_u16() == 403 =>
        {
            Err(SpecLoopError::Workflow(
                "ChatGPT authorization or direct-use permission was rejected; run `spec-loop auth login` and verify ChatGPT plan access".to_owned(),
            ))
        }
        Err(_) => Ok(Connection::Unavailable),
    }
}

enum WebSocketAttempt {
    Completed(Value),
    Retryable { recover_full_input: bool },
    UseHttpSse,
}

async fn read_websocket_response(
    socket: &mut Socket,
    used_previous_response_id: bool,
    deadline: Option<Instant>,
    cancelled: &AtomicBool,
    reporter: &mut UsageReporter<'_>,
) -> Result<WebSocketAttempt> {
    let mut state = SseState::default();
    loop {
        let next_message = async { Ok::<_, SpecLoopError>(socket.next().await) };
        let message = super::await_limited(next_message, deadline, cancelled).await??;
        match message {
            None | Some(Ok(Message::Close(_))) => {
                return Ok(WebSocketAttempt::Retryable {
                    recover_full_input: true,
                });
            }
            Some(Err(error)) if is_clean_connection_close(&error) => {
                return Ok(WebSocketAttempt::Retryable {
                    recover_full_input: true,
                });
            }
            Some(Err(_)) => {
                return Err(SpecLoopError::Workflow(
                    "OpenAI Responses WebSocket stream failed".to_owned(),
                ));
            }
            Some(Ok(Message::Text(text))) => {
                let event: Value = serde_json::from_str(text.as_str()).map_err(|_| {
                    SpecLoopError::Agent(
                        "OpenAI Responses WebSocket event is malformed JSON".to_owned(),
                    )
                })?;
                let event_type = event.get("type").and_then(Value::as_str).ok_or_else(|| {
                    SpecLoopError::Agent(
                        "OpenAI Responses WebSocket event type is missing".to_owned(),
                    )
                })?;
                match classify_continuation_error(event_type, &event, used_previous_response_id) {
                    ContinuationError::PreviousResponseNotFound => {
                        return Ok(WebSocketAttempt::Retryable {
                            recover_full_input: true,
                        });
                    }
                    ContinuationError::Unsupported => return Ok(WebSocketAttempt::UseHttpSse),
                    ContinuationError::Ordinary => {}
                }
                if let Err(failure) = state.accept_event_observed(event_type, &event, reporter) {
                    match failure {
                        StreamFailure::RetryableProvider => {
                            return Ok(WebSocketAttempt::Retryable {
                                recover_full_input: false,
                            });
                        }
                        StreamFailure::Network | StreamFailure::Fatal(_) => {
                            return Err(stream_failure(failure));
                        }
                    }
                }
                if let Some(response) = state.completed.take() {
                    return Ok(WebSocketAttempt::Completed(response));
                }
            }
            Some(Ok(Message::Ping(payload))) => {
                let pong =
                    super::await_limited(socket.send(Message::Pong(payload)), deadline, cancelled)
                        .await?;
                if let Err(error) = pong {
                    if is_clean_connection_close(&error) {
                        return Ok(WebSocketAttempt::Retryable {
                            recover_full_input: true,
                        });
                    }
                    return Err(SpecLoopError::Workflow(
                        "OpenAI Responses WebSocket control frame failed".to_owned(),
                    ));
                }
            }
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Binary(_) | Message::Frame(_))) => {
                return Err(SpecLoopError::Agent(
                    "OpenAI Responses WebSocket emitted an unsupported frame".to_owned(),
                ));
            }
        }
    }
}

fn is_clean_connection_close(error: &WebSocketError) -> bool {
    match error {
        WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => true,
        WebSocketError::Io(error) => matches!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

enum ContinuationError {
    PreviousResponseNotFound,
    Unsupported,
    Ordinary,
}

fn classify_continuation_error(
    event_type: &str,
    event: &Value,
    used_previous_response_id: bool,
) -> ContinuationError {
    if event_type != "error" {
        return ContinuationError::Ordinary;
    }
    let Some(error) = event.get("error") else {
        return ContinuationError::Ordinary;
    };
    let code = error.get("code").and_then(Value::as_str);
    if used_previous_response_id && code == Some("previous_response_not_found") {
        return ContinuationError::PreviousResponseNotFound;
    }
    let parameter = error.get("param").and_then(Value::as_str);
    let error_type = error.get("type").and_then(Value::as_str);
    if used_previous_response_id
        && parameter == Some("previous_response_id")
        && (error_type == Some("invalid_request_error") || code == Some("unsupported_parameter"))
    {
        return ContinuationError::Unsupported;
    }
    ContinuationError::Ordinary
}

fn stream_failure(failure: StreamFailure) -> SpecLoopError {
    match failure {
        StreamFailure::Fatal(error) => error,
        StreamFailure::Network | StreamFailure::RetryableProvider => SpecLoopError::Workflow(
            "OpenAI Responses WebSocket stream reported an incomplete response".to_owned(),
        ),
    }
}
