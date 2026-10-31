# Spec 016: Bounded Responses Stream Recovery with WebSocket Continuation

**Status:** Complete

## Goal

Recover from Responses API streams that end before `response.completed` without replaying the whole agent role or completed shell work. Reduce repeated request payloads without persistent response storage by using `store=false` with Responses WebSocket continuation.

## Requirements

- Retry clean stream termination before `response.completed` through the existing bounded provider-request retry loop. Reuse the current request and existing retry delays and limits; never retry the whole agent role. For HTTP SSE fallback, cover EOF inside a partial SSE event; for WebSocket, cover connection close before completion.
- Keep explicit `response.failed` and `response.incomplete`, malformed JSON, unsupported events, invalid event order, cancellation, timeout, and other fatal errors fatal.
- Do not return output items from an uncompleted response to the shell executor. Execute shell calls only from fully completed responses. Preserve completed shell results in local conversation state across retries; discard incomplete-attempt output items.
- Use a Responses WebSocket connection for a conversation and set `store=false` on every generation request. After a completed response with an ID, continue on the same connection using `previous_response_id` and only newly added input items. Keep instructions, tools, and strict role schema on each request.
- Use the implicit default WebSocket lane; omit `stream_id` unless ChatGPT OAuth direct-use support is confirmed. A probe with `stream_id` returned `Unsupported parameter: stream_id`.
- If WebSocket setup or continuation is unavailable, use HTTP SSE only with `store=false`, full local conversation input, and no `previous_response_id`.
- Keep complete local conversation input for compaction and recovery. If the socket closes, cached response state is lost, or continuation returns `previous_response_not_found`, start a fresh connection and send full local input without `previous_response_id`. Never fall back to `store=true`.
- Native `/responses/compact` may remain an HTTP request. After compaction, start a new WebSocket response with the validated compacted input plus current task; do not reference pre-compaction response IDs.
- Do not claim payload reduction lowers input-token billing: prior context remains model input. Document volatile connection-local state and full-input recovery after reconnect.
- Cover retry bounds, exact current-request reuse, incremental input, full-history recovery, completed-effect preservation, and non-execution of incomplete tool calls with deterministic fake-provider tests.

## Compatibility

This supersedes Spec 012 and Spec 015's no-generation-retry rule only for clean termination before a completed response. Other fatal provider and protocol failures retain existing behavior. A controlled synthetic HTTP comparison on the ChatGPT OAuth direct-use route succeeded with `store=false` and returned HTTP 400 with `{"detail":"Store must be set to false"}` for `store=true`. A WebSocket probe completed two chained responses on one connection with `store=false`; adding `stream_id` was rejected as unsupported, so use the default lane. The original workflow log omitted its response body, but the HTTP comparison reproduces the rejection. Public Responses HTTP documentation still supports `store=true` for API use generally. OpenAI does not publicly explain this OAuth-specific restriction; it is consistent with avoiding persisted Responses data, since HTTP storage enables 30-day retention while WebSocket continuation with `store=false` uses connection-local memory. Treat that rationale as inference, not a confirmed policy. See [data retention](https://developers.openai.com/api/docs/guides/your-data#v1responses) and [WebSocket continuation](https://developers.openai.com/api/docs/guides/websocket-mode). This design uses documented WebSocket connection-local continuation with `store=false`, avoiding persistent response storage. No whole-role retries, durable response-ID storage, or changes to workflow integrity guards.
