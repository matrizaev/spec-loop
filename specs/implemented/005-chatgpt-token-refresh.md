# Spec 005: Refresh ChatGPT OAuth Access Tokens Per Request

**Status:** Complete

## Goal

Keep long-running built-in agent roles authenticated across multiple OpenAI Responses requests. Re-resolve the stored ChatGPT OAuth credential before each new model request and refresh access tokens before expiry, without replaying completed shell work.

## Existing behavior

- `active_access_token` refreshes and persists credentials when access token expires within 60 seconds.
- Built-in backend resolves one access token at role start and reuses it for every subsequent Responses request, including requests after shell-tool execution.
- OAuth refresh already stores rotated refresh tokens and fails actionably when credentials or direct-use permission are invalid.

## Requirements

- Resolve the active ChatGPT access token immediately before each new logical Responses API request, including each follow-up request after shell-tool results. Do not resolve tokens per SSE event/chunk.
- Refresh when access token expires within five minutes, or when expiry is unavailable. A token outside that window does not require a refresh exchange.
- Persist refreshed access token, expiry, and any rotated refresh token before sending the Responses request. Preserve existing refresh token if provider omits a replacement. Continue validating direct-use permission and fail actionably if refresh is rejected or permission is absent.
- Never send refresh tokens to the Responses API. Never log or display access tokens, refresh tokens, or authorization headers.
- Keep existing retry and workflow semantics. Transport retries for one logical request may reuse that request's resolved access token. A Responses 401/403 remains fatal; do not force-refresh and replay the request. Never replay completed shell calls or other workspace effects to recover authentication.
- Keep refresh behavior internal to built-in backend. Do not add profile fields, change Pi auth behavior, add dependencies, or require interactive login solely because an access token expired while refresh token remains valid.

## Acceptance criteria

1. Unit tests prove the five-minute refresh threshold, boundary behavior, and refresh when expiry is absent.
2. Deterministic fake-provider tests prove one token resolution per logical Responses request, including a follow-up after shell-tool execution; successive requests use their corresponding resolved tokens. No live provider or credentials are required.
3. Existing OAuth refresh tests prove rotated credentials are persisted and permission/refresh failures stop before provider dispatch. Diagnostics and logs contain no token material.
4. Existing 401/403 behavior remains fatal without auth retry or request replay; prior shell effects are not repeated.
5. `make check` passes.

## Non-goals

- Refresh-token lifetime management or guarantees against provider revocation.
- Automatic recovery from Responses 401/403, interactive login, or changes to auth status output.
- Cross-process credential-refresh coordination.
- Changes to Pi backend behavior or output formatting.
