# Spec Loop

Spec Loop is a Rust CLI for specification lifecycle automation. This guide covers installation, configuration, backends, and security.

## Install and build

```sh
cargo install --path /path/to/spec-loop
spec-loop --help
make check
```

Run Spec Loop from a directory containing `.spec-loop.toml`. Start with [starter profile](examples/profiles/starter.toml). Profiles use strict TOML schema version 3; version 1 and 2 profiles require manual migration. Unknown fields and invalid values fail validation.

## Configuration

The default backend is built-in OpenAI. Set non-empty `agent.model` and configure ChatGPT authorization before use. Select Pi explicitly with `agent.backend = "pi"`; Pi uses its own model and resources. Built-in authentication works, but built-in agent execution remains incomplete.

Optional per-stage model and thinking settings override global values. Thinking levels are `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`; built-in maps `off` to `none` and other values directly to reasoning effort.

Built-in-only options:

- `agent.context_window_tokens` and per-stage limits are positive user-provided values, not model facts. A stage limit overrides the global limit. A stage model override without its own limit has unknown context capacity. No limit is inferred from model names or fetched from a catalog. Pi rejects these fields.
- If `agent.max_tool_calls` is omitted, there is no Spec Loop shell-call count ceiling. A positive value caps calls per role; zero is invalid. This does not imply unlimited provider access, runtime, output, or external resources. Pi rejects the field.
- `agent.max_tool_output_bytes` defaults to a combined stdout/stderr cap of 1,048,576 bytes per shell call; neither stream has a reserved share. Capture consumes the shared budget as pipes are read, without stable stream priority. If lossy UTF-8 decoding expands captured bytes, rendering keeps stdout first, then stderr within the remaining budget, without splitting characters. Overrides must be positive. Pi rejects the field.

## Backends and security

### Pi

Pi uses the user's configuration, tools, extensions, MCP servers, skills, project trust, context files, and inherited environment. Spec Loop adds no Pi resource overrides and cannot sandbox Pi resources or external effects.

### Built-in OpenAI

Built-in roles use OpenAI ChatGPT OAuth, the Responses API, and the built-in shell tool only. They do not use API keys, `OPENAI_API_KEY`, or authentication/provider fallbacks. Commands run automatically without per-command approval using `/bin/sh -c`, inherited environment, and same-user permissions. They may access or transmit credentials and affect files or systems outside the worktree. This is not a sandbox; Spec Loop cannot guarantee isolation or reversibility. Pi extensions, MCP, skills, trust, and context integration are not available. Unsupported platforms fail rather than changing shell semantics.

The provider is asked for strict Structured Outputs from shared role schemas; Spec Loop validates response shape and semantics locally. Compatibility of this OAuth path, account entitlement, selected model, and Structured Outputs combination is not established. Provider rejection fails without weaker-format or authentication fallback.

### ChatGPT authentication

Run `spec-loop auth login`. Login uses the official ChatGPT sign-in flow for eligible plans and requires `chatgpt.tokens.use.direct`. API keys and device-code login are unsupported. Login prints the authorization URL and needs a reachable localhost callback; remote users must provide loopback reachability. Use `auth login --account ID` to reauthorize a registration or `--new-account` to create one. `auth status` does not show tokens. `auth logout [--account ID]` removes local credentials but does not revoke the upstream grant. Auth commands do not load repository profiles.

Credentials use an OS credential store where available, otherwise an owner-restricted file. They stay outside the active project and run logs. Set `SPEC_LOOP_AUTH_DIR` to a non-empty absolute directory outside the project; it directly contains `auth.json`, overrides platform defaults and `XDG_CONFIG_HOME`, and does not migrate existing data or fall back if the path fails. If unset:

- Linux and other non-macOS/non-Windows systems: `$XDG_CONFIG_HOME/spec-loop/auth.json`, or `$HOME/.config/spec-loop/auth.json`.
- macOS: `~/Library/Application Support/Spec Loop/auth.json`.
- Windows: `%APPDATA%\Spec Loop\auth.json`.

Permissions are owner-restricted where supported. If the OS credential store is unavailable, `auth.json` may contain plaintext. A custom path does not encrypt credentials or isolate them from same-user processes. Access tokens expiring within five minutes are refreshed before a new Responses request. Rejected or revoked refresh tokens or lost permission require login. A 401/403 stops without replay.

## Logs and usage

Run logs are stored outside the project with restrictive permissions where supported. They may contain workspace data, credentials, or inherited secrets; secret removal is not guaranteed. Terminal output strips control sequences from displayed agent text, but retained logs and provider transcripts are unchanged.

Built-in usage reports observed completed-response token counts, not billing, quota, remaining access, or failed-request usage. Missing or invalid counts display as `unknown`, not zero. Context percentages compare the last response's input-plus-output tokens with a user-provided limit; they do not predict next-request capacity. Pi runs do not show built-in telemetry.

## Development and releases

`make check` runs the local quality gate. Release builds and smoke tests are CI gates. The release workflow builds Linux x86_64 musl binaries.
