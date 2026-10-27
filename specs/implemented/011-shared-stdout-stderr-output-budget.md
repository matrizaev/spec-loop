# Spec 011: Shared Stdout/Stderr Output Budget

**Status:** Complete

## Goal

Use `agent.max_tool_output_bytes` as one combined stdout/stderr payload budget per built-in shell invocation. Let either stream use unused capacity from the other while enforcing the same combined bound during process capture and output rendering.

## Existing behavior

- `agent.max_tool_output_bytes` defaults to 1,048,576 bytes and limits captured stdout/stderr for each built-in shell invocation.
- Process supervision currently reserves half the limit for stdout and half for stderr. Rendering applies another per-stream half allocation.
- A stream that produces less than its reserved share cannot donate unused capacity. For example, 800 KiB stdout and empty stderr retain only 512 KiB of stdout under a 1 MiB limit.
- The process supervisor reads both pipes concurrently and records each stream's truncation state. The shell result includes termination and exit status separately from output truncation.

## Requirements

### Combined capture and rendering budget

- Treat `agent.max_tool_output_bytes` as one maximum for the combined retained stdout and stderr payload of a single built-in shell invocation. Preserve its default, valid range, backend restriction, and per-call scope.
- Do not reserve a fixed portion for either stream. Bytes from one stream may use the full budget when the other stream is empty or produces less output.
- Enforce the combined limit in the process capture layer itself, not only in shell-output rendering. Across retained raw stdout and stderr bytes, process capture must never exceed the configured maximum.
- Continue draining both child pipes after the shared budget is exhausted, discarding excess bytes. Do not stop the command, block either pipe, or deadlock the child because output was truncated.
- Apply a combined limit during rendering too. The rendered stdout and stderr payloads together must not exceed the configured maximum, including when lossy UTF-8 conversion expands invalid input bytes. Do not independently allocate half the rendering budget to each stream.
- When stdout and stderr produce output concurrently, consume the shared capture budget as bytes are read. Do not promise a fixed split, proportional distribution, or stable priority between streams. Tests must verify the combined bound and single-stream capacity, not a particular concurrent split.
- Mark truncation whenever any bytes are discarded during capture or omitted during rendering. Keep truncation explicit in the tool result and run log. Do not mark output that fits exactly within the limit as truncated.
- Preserve the process termination and actual child exit status when output is truncated. Truncation alone must not change the exit status, convert success into command failure, or suppress an actual nonzero status.
- The limit applies to stdout/stderr payload bytes, not the fixed tool-result labels, exit-status metadata, failure marker, or explicit truncation marker.
- Leave uncapped process invocations and unrelated process, timeout, cancellation, logging, and shell behavior unchanged.

### Documentation

- Update README configuration documentation and any applicable CLI/configuration help to describe `agent.max_tool_output_bytes` as a shared combined stdout/stderr byte limit with no per-stream reservation.
- Keep valid examples and the default value accurate. Do not describe the limit as a per-stream allowance.

## Acceptance criteria

1. With a 1 MiB limit and 800 KiB stdout with empty stderr, process capture and rendered output retain all 800 KiB stdout, report no truncation, with no capacity held back for empty stderr. Mirror test with stderr-only output.
2. Process capture retains no more than the configured combined raw-byte limit across both streams, marks streams whose data was discarded, continues draining both pipes, and allows the child to exit without output-induced deadlock.
3. Rendering retains no more than the combined configured byte limit across stdout and stderr; tests cover invalid UTF-8 input whose replacement rendering would otherwise expand beyond the limit.
4. Tests cover below-limit, exactly-at-limit, and over-limit output for each stream and both streams together. Truncation markers appear only when data was omitted.
5. A shell command that exceeds the output limit and exits nonzero still reports its actual exit status and unsuccessful-command diagnostic. Truncation does not replace or hide either fact.
6. Existing default value, validation, built-in-only restriction, per-call reset, uncapped process behavior, timeout, cancellation, and transcript logging remain unchanged.
7. README, applicable help, and examples accurately describe the combined budget. Run `make check` after implementation. Tests require no network, live model, credentials, or developer worktree state.

## Non-goals

- Changing `agent.max_tool_output_bytes`, its default, configuration validity, or which backend accepts it.
- Limiting total formatted tool-result metadata or transcript-log size with this payload budget.
- Prioritizing stdout over stderr or guaranteeing a deterministic split when both streams exceed the shared budget concurrently.
- Changing command execution, retry policy, timeout/cancellation policy, process exit handling, or output content beyond the combined-cap correction.
