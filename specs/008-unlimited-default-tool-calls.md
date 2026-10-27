# Spec 008: Unlimited Default Tool Calls

**Status:** Active

## Goal

Remove the built-in agent's default per-role shell-call ceiling while retaining an optional profile limit. Document the security boundary and external isolation options without adding sandboxing to Spec Loop.

This spec is scheduled after the preceding Rust architecture spec. It has no technical dependency on that refactor.

## Existing behavior

- Built-in profiles that omit `agent.max_tool_calls` receive a default limit of 50 shell calls per role.
- `agent.max_tool_calls` is an optional positive integer under `[agent]` and is valid only for the built-in backend.
- Built-in shell commands execute automatically with the same-user permissions and inherited environment. They are not sandboxed by Spec Loop.

## Requirements

### Call limit behavior

- When `agent.max_tool_calls` is omitted, impose no engine-configured shell-call count limit per built-in role. Do not substitute a numeric sentinel or another implicit finite default. No call-count bookkeeping may introduce a count-dependent failure in this mode.
- Preserve `agent.max_tool_calls` as an optional positive integer under `[agent]`. When configured, it limits shell invocations per role; the limit resets for each role. Reject zero and reject this setting for the Pi backend.
- With a configured limit, stop before invoking a shell command that would exceed it. Do not replay completed shell commands or roll back their effects.
- Do not change `agent.max_tool_output_bytes`, timeout, cancellation, provider retry, role retry, structured response validation, or logging behavior as part of removing the default call ceiling.
- The absence of a Spec Loop call-count limit does not promise unlimited provider access, subscription usage, execution time, output bytes, or external resources. Provider and operating-system limits still apply.

### Security documentation

- Explain that built-in shell commands run automatically and unsandboxed, with same-user permissions and inherited environment. They can access or transmit accessible credentials and secrets, and can affect files or systems outside the worktree. Spec Loop cannot guarantee their side effects are isolated or reversible.
- Document non-prescriptive ways users may isolate the entire process using external tools, such as a dedicated operating-system account, a container, a virtual machine, or a platform sandbox. State that isolation effectiveness depends on the user's external configuration and that Spec Loop does not provide or configure these controls.
- Do not add command wrappers, shell hooks, approval prompts, or built-in sandbox behavior.

### Documentation and profiles

- Update README configuration documentation and valid profile examples to state that omitting `agent.max_tool_calls` means no Spec Loop shell-call count ceiling, and that an explicit positive integer sets a per-role limit for the built-in backend only.
- Keep examples valid under the strict profile parser. Show an explicit override and document that Pi rejects the field.
- Update CLI/configuration help if it describes the old default. Remove or correct every claim that the default is 50 calls per role.

## Acceptance criteria

1. With `agent.max_tool_calls` omitted, a deterministic fake-provider test completes a built-in role that requests more than 50 successful shell calls. The test requires no network, live model, credentials, or developer worktree state.
2. A configured positive limit is enforced per role, stops before the first over-limit shell invocation, preserves prior command effects, and does not replay completed commands.
3. Configuration tests cover omitted, positive, zero, unknown, and Pi-backend cases. Zero and backend-incompatible settings fail before agent invocation.
4. `agent.max_tool_output_bytes`, timeout, cancellation, provider and role retries, structured role validation, and logs retain existing behavior. Tests verify removing the default count ceiling does not bypass these controls.
5. README, CLI/configuration help where applicable, and valid profile examples describe the default, override, backend restriction, and external-isolation guidance accurately. No documentation promises unlimited provider usage or execution resources, or claims Spec Loop provides a sandbox.
6. Run `make check` after implementation. Tests require no network, live model, credentials, or Git state.

## Non-goals

- Removing the optional per-role limit when explicitly configured.
- Changing shell output limits, timeouts, cancellation, provider access, or general execution permissions.
- Implementing sandboxing, credential isolation, command approval, rollback, or external side-effect reversal.
