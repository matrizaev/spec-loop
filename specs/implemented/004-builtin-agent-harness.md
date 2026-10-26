# Spec 004: Built-in Agent Harness

**Status:** Complete

## Goal

Make Spec Loop usable without installing Pi by providing a small built-in coding
agent harness as the default backend. Keep Pi as an explicitly selectable
alternative for users who want Pi's configuration, tools, extensions, MCP,
skills, trust, and context behavior.

The first built-in harness supports only official OpenAI Sign in with ChatGPT
for eligible ChatGPT plan usage. It uses OpenAI Responses API tool calls to run
shell commands and returns the same final text contract consumed by Spec Loop's
existing strict typed role protocol.

## Existing behavior to preserve

- Keep workflow policy, prompts, retries, role schemas, checks, review,
  integrity protection, lifecycle actions, and logging contracts independent of
  provider-specific request and response formats.
- Keep Pi available as a backend. Explicit `agent.backend = "pi"` preserves
  Pi's current invocation, effective user settings, inherited environment,
  selected model/thinking behavior, and ownership of tools and extensions.
- Keep configured checks and lifecycle actions as argv-based child processes.
  The built-in model's shell tool is the only new, intentional shell-command
  execution path; it is not a profile command or a shell hook.
- Keep the current self-host profile explicitly on Pi so self-hosting does not
  unexpectedly require ChatGPT authorization or provider credentials.

## Backend and profile behavior

- Add a built-in backend selected by `agent.backend = "builtin"`. In profile
  schema version 3, omitted `agent.backend` defaults to `builtin`; explicit
  `agent.backend = "pi"` continues to select Pi.
- Bump profile schema to version 3. Reject schema version 2 with an actionable
  migration error. Explain how to preserve prior behavior by setting
  `agent.backend = "pi"`; do not silently reinterpret a version-2 profile.
- The built-in backend is OpenAI-only and uses ChatGPT OAuth only. Do not add
  provider or authentication selectors for this release. Do not use API-key
  environment variables for provider authentication or fall back to API-key
  billing; API-key and Anthropic support are deferred.
- Never intentionally write OAuth credentials to TOML, command-line arguments,
  prompts, or logs. If ChatGPT credentials are missing or invalid, report the
  `spec-loop auth login` action; do not silently switch authentication modes.
  Shell output may still expose credentials accessible to the same user, as
  described below.
- Require non-empty global `agent.model` when the built-in backend is selected;
  do not guess or select a built-in model. Stage-specific
  `agent.stages.<stage>.model` overrides global `agent.model`, as it does for
  Pi. Preserve existing Pi model resolution.
- Support the existing Pi thinking-level enum (`off`, `minimal`, `low`,
  `medium`, `high`, `xhigh`, `max`) for the built-in backend by mapping it to
  OpenAI's documented reasoning-effort setting. Stage-specific thinking values
  override global `agent.thinking`, matching Pi precedence. Preserve Pi's
  current behavior. Validate the Pi executable only when Pi is selected.
- Add positive `[agent]` limits `max_tool_calls` and `max_tool_output_bytes`
  for the built-in backend. Defaults are 50 shell calls per role and 1,048,576
  combined stdout/stderr bytes returned per shell call. Reject zero values and
  reject these fields when Pi is selected rather than silently ignoring them.
- Update the checked-in profile to schema version 3 and retain its explicit Pi
  backend. Update valid profile examples to schema version 3; include an
  example of the built-in defaults and examples of explicit Pi selection.

## ChatGPT authentication

- Add `spec-loop auth login`, `spec-loop auth status`, and
  `spec-loop auth logout`. These commands must work without a project profile
  and must not alter repository files. Keep `--help` usable without a profile.
- Implement the documented OpenAI Sign in with ChatGPT flow for open-source
  applications: dynamic client registration as required by the official guide,
  browser authorization, PKCE, a localhost callback, state validation, and
  token exchange. Attempt to open the authorization URL in a browser and always
  print the URL so the user can open it manually. Document remote/SSH callback
  requirements; do not invent an unsupported device-code flow. Use only
  documented public authorization and API surfaces; do not reuse private
  Pi/Codex token formats or scrape credentials.
- Request and verify the permission required for direct ChatGPT-plan use
  (`chatgpt.tokens.use.direct`). Authentication without this permission is not
  sufficient. Fail with actionable guidance if the permission is absent,
  revoked, or no longer valid; do not switch to API-key billing implicitly.
- Use the official OpenAI Responses API with the ChatGPT-authorized access
  token. Refresh access tokens using the documented refresh flow and handle
  expired, revoked, and malformed credentials without retrying the role as if
  its model response were malformed.
- Persist OAuth credentials outside the worktree and run logs using an
  operating-system credential store where available, or a platform-appropriate
  owner-only credential file otherwise. Restrict file permissions where
  supported. The file fallback is not encrypted by Spec Loop; document this
  plaintext-at-rest risk, storage location, and protection. Never inject OAuth
  tokens into shell environment or prompts. Because shell processes run as the
  same user and are unsandboxed, a model-requested command may read credentials
  or secrets in inherited environment variables accessible to that user, then
  expose or transmit them. Do not claim credential isolation.
  Avoid exposing secrets in diagnostics. `auth status` reports authorization
  and permission state without displaying tokens. `auth logout` removes only
  Spec Loop's stored ChatGPT credentials.
- Treat subscription/model access and usage limits as provider errors, not as
  proof of unlimited API access. Do not promise unlimited usage.

## Built-in provider and tool loop

- Implement the OpenAI Responses API adapter behind the built-in harness,
  using the documented ChatGPT-authorized endpoint and supported tool-calling
  format. Do not add provider SDKs or other dependencies without reviewing
  maintenance, MSRV, license, feature surface, and transitive dependencies.
- Use a minimal harness-level system prompt containing only tool-call
  mechanics and final-answer transport requirements. Pass the existing role
  prompt as the sole task-specific instruction; preserve its exact requirements,
  generated role schema, and strict final JSON contract. Do not duplicate or
  paraphrase workflow policy in provider-specific prompts. Tool-call messages
  and tool results are transport only; never treat them as a final role
  response. Return only final assistant text to existing typed validation.
- Provide one `shell` tool to every role, including planning, implementation,
  review, and audit. During the POSIX-only proof of concept, execute commands
  automatically via `/bin/sh -c`, without per-command confirmation, using the
  project root as working directory and Spec Loop's inherited environment.
  Fail actionably on unsupported platforms. Capture stdout and stderr without
  deadlock. Execute multiple tool calls from one provider response sequentially,
  in response order. Count every shell invocation against
  `agent.max_tool_calls`; capture and return at most
  `agent.max_tool_output_bytes` combined bytes per invocation, with an explicit
  truncation marker. Enforce capture limits while draining process output.
  Fail the role without replaying prior shell commands when a tool-call limit
  is reached.
- Shell commands run with the same operating-system permissions as Spec Loop.
  This is not a sandbox. Do not claim reviewer or auditor shell access or
  credential access is isolated; their prompts still request no mutation.
  Document that commands and external side effects may alter files or systems
  outside the worktree.
- Apply the configured agent timeout and cancellation to provider requests and
  tool execution. On cancellation or timeout, stop the active child process and
  stop the role. Fail immediately on invalid credentials, missing direct-use
  permission, unsupported model, usage-limit exhaustion, and invalid
  request/configuration errors. Retry transient network, rate-limit, and
  provider-server failures up to three times after the initial request, with
  exponential backoff starting at one second and capped at 30 seconds. Honor
  valid provider retry guidance only within that cap. Keep conversation/tool
  state across retries and never re-execute a shell call that already completed.
  Represent built-in backend failures as retryable or fatal so permanent errors,
  exhausted provider retries, tool-call limit failures, and role timeouts stop
  without entering the generic role retry loop, which could replay shell side
  effects. Preserve useful redacted diagnostics and never convert a failure
  into a successful final response. Existing Pi retry behavior remains
  unchanged.
- Never intentionally serialize OAuth credentials into prompts, tool
  environments, or HTTP diagnostics; redact authorization data from provider
  errors. Preserve shell commands and bounded stdout/stderr/tool results in
  existing external run logs for diagnosis. Tool output is untrusted and may
  contain sensitive workspace data or credentials accessible to the same user;
  document that transcript logging cannot guarantee secret removal.
- Do not add automatic file-edit, browser, Git, commit, MCP, extension, or
  arbitrary configured-hook tools. Opening the OAuth authorization URL is an
  auth-command behavior, not a model tool. The shell tool may invoke ordinary
  local commands by model request; Spec Loop's configured checks and actions
  remain separately executed by the engine using argv.

## Repository policy and documentation

- Update `AGENTS.md` to distinguish backend behavior: Pi roles preserve and use
  the user's effective Pi resources; built-in roles use only the documented
  OpenAI ChatGPT OAuth provider and shell tool, with inherited environment and
  no Pi extensions/MCP/skills/trust/context integration. State that model shell
  calls run automatically for every role without per-command approval and are
  not sandboxed.
- Make review-context prompts backend-neutral: delegate configured review
  context to the selected backend's available tools. Preserve exact argv and
  project-root working directory; the engine must not execute or capture the
  review-context command.
- Update CLI help and README requirements, profile examples, configuration
  documentation, and auth instructions. Explain that Pi installation is needed
  only for the Pi backend and ChatGPT authorization is required for the
  built-in backend. Do not advertise built-in API-key or Anthropic modes.
- Document required model configuration and stage overrides, reasoning-effort
  mapping, ChatGPT authentication setup, token storage and plaintext-at-rest
  fallback risk, same-user shell access to credentials and inherited environment
  secrets (including possible exposure or transmission), tool limits,
  timeout/cancellation behavior, automatic POSIX shell execution, sensitive
  tool logs, and supported platform/provider limitations. Keep examples valid
  under the strict profile parser.

## Acceptance criteria

1. Profile schema version 3 accepts `builtin` and `pi`, defaults omitted
   `agent.backend` to `builtin`, rejects unknown fields, and permits only
   ChatGPT OAuth for the built-in backend. Version-2 profiles fail with precise
   migration guidance; explicit Pi profiles retain prior Pi behavior. Built-in
   configuration requires `agent.model`; stage model and thinking overrides
   take precedence over global values. Unit tests cover backend validation,
   required model, thinking-level mapping, and positive tool-limit
   validation/defaults.
2. The checked-in profile remains explicitly configured for Pi. Examples and
   README accurately distinguish default built-in use from explicit Pi use.
3. Workflow integration tests run every existing role through a fake built-in
   backend and prove identical prompt inputs, final typed-response validation,
   retry, blocked, and log behavior. Provider event/tool-call data cannot be
   mistaken for final role JSON. Existing fake-Pi tests continue to pass.
4. OAuth tests use a local fake authorization/token server and fake browser or
   callback harness; they cover PKCE/state validation, permission granted or
   absent, malformed/expired/revoked credentials, refresh success/failure,
   browser-open failure with printed URL, cancellation, logout, and credential
   redaction. Tests require no live account, network, or credentials.
5. Credential-storage tests verify tokens stay outside the worktree and are
   not intentionally logged, file permissions are restrictive where supported,
   status output never contains token values, the file fallback is documented as
   unencrypted, shell subprocesses are not deliberately given OAuth tokens, and
   logout removes only Spec Loop credentials. Documentation states same-user
   shell access to credentials and inherited environment secrets cannot be
   isolated and may expose or transmit them.
6. Provider tests use deterministic fake HTTP responses for the OpenAI
   Responses API. Cover successful final text, multiple tool calls, malformed
   and partial responses, authentication and permission failures, transient
   network/rate-limit/server retries, permanent errors, and model/usage limits.
   Verify at most three retries per request, one-second exponential backoff
   capped at 30 seconds, and provider retry guidance never exceeds the cap.
   Prove no API-key or other-provider fallback occurs. Default tests require no
   live provider or network.
7. Shell-tool tests prove every role receives the tool and commands execute
   without confirmation; verify POSIX `/bin/sh -c`, clear unsupported-platform
   errors, project-root working directory, inherited environment, sequential
   tool-call order, captured stdout/stderr, default and configured call/output
   limits, truncation, timeout, cancellation, child termination, and preserved
   workspace changes after failures. Tool-limit failures and timeouts after
   shell execution stop without generic role retries. Tests explicitly
   demonstrate that shell commands are not sandboxed and configured
   checks/actions still use argv execution.
8. Tests prove a missing/revoked ChatGPT direct-use permission stops before
   model invocation and that valid ChatGPT OAuth requests use the documented
   Responses API route. Tests prove transient provider failures retry within
   finite bounds without replaying completed shell calls, exhausted retries and
   other fatal backend errors stop without generic role retries, and permanent
   auth/model/usage-limit errors fail fast. Unit/integration tests inspect a
   fake transport; no live OpenAI request is required.
9. Review-context instructions identify the selected backend's tool access,
   pass configured argv unchanged, and continue to delegate execution to the
   agent rather than the engine. Prompt tests cover Pi and built-in wording.
10. CLI auth commands work without `.spec-loop.toml`; invalid auth/profile
    configuration fails before agent invocation. `--help` remains available
    without a profile.
11. README, CLI help, `AGENTS.md`, examples, and profile migration guidance
    describe actual parser and runtime behavior, including automatic shell
    execution, OAuth browser fallback, sensitive transcript logging, and lack of
    same-user credential/environment isolation, including possible exposure or
    transmission; no docs claim sandboxing, unlimited subscription use, or Pi
    parity.
12. Run `make check` after implementation. Add deterministic process/provider
    integration tests; no default test may require network access, live model
    access, Git, or developer worktree state.

## Non-goals

- Replacing or removing Pi, or reproducing Pi's extensions, MCP, skills,
  project trust, context loading, provider catalog, or session features.
- OpenAI API-key authentication, Anthropic support, or other built-in providers.
- Sandboxing shell commands or claiming prompts prevent tool side effects.
- Reading credentials from profile files, falling back to API-key billing, or
  accessing undocumented subscription endpoints.
- Built-in Git status/diff/stage/commit integration, shell hooks for configured
  checks/actions, or additional model tools beyond shell.
