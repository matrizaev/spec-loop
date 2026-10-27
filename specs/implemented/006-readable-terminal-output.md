# Spec 006: Readable Terminal Output

**Status:** Complete

## Goal

Make Spec Loop's workflow CLI output easy to scan in terminals and plain-text logs. Replace compact raw agent-response JSON dumps with readable, role-aware output and use consistent semantic styling without relying on color to communicate meaning. Authentication command output remains unchanged.

Do not change workflow decisions, agent protocols, JSON schemas, or retained raw logs.

## Existing behavior

- Workflow progress, agent details, check/action results, and errors are printed through scattered plain-text output paths.
- `invoke_and_log_agent` prints each raw final role response directly to the terminal. Responses use compact JSON and appear before typed validation, so malformed data can also be dumped as if it were a result.
- The code currently emits no ANSI color styling or color-control option.
- Raw provider transcripts and final response text are retained in external run logs for diagnosis.

## Requirements

### Human-readable role results

- Render each successfully validated typed role response as readable, deterministic text rather than printing its compact JSON to the terminal.
- Show the role and status prominently. The concise default view is:
  - implementation: status and summary preview; include blocked-reason preview when blocked;
  - plan author: readiness or blocked status and blocked-reason preview;
  - review: status and finding count, or blocked-reason preview; for each finding, show ID, location, and requirement/violation previews;
  - audit: status and blocked-reason preview, or each finding ID, classification, and actionability;
  - clarification: status and blocked-reason preview, or each finding ID with citation count and citation paths/locations.
- Show explicit “no findings” or “no citations” text for empty collections. Limit each concise text preview to 160 Unicode scalar values; append `… [truncated; use --verbose]` when more text remains.
- Add workflow `--verbose` to show every field of a validated typed result in the same structured layout, without preview truncation. In review output include evidence and proposed correction; in audit output include rationale; in clarification output include excerpt and relevance. `--verbose` must not print raw JSON or provider transcripts.
- Render a response only after typed parsing and semantic validation succeed. For invalid responses, show concise validation and retry diagnostics plus the exact final-response log path; do not print malformed raw JSON to the terminal.
- Keep the exact raw provider transcript and final response in their existing external log files. Terminal formatting must not rewrite or replace those logs.

### Progress and outcomes

- Give workflow stages, agent roles/attempts, checks, review outcomes, archive transitions, and lifecycle actions consistent visual hierarchy and whitespace.
- Use explicit labels such as `[RUNNING]`, `[PASS]`, `[BLOCKED]`, `[RETRY]`, `[WARN]`, `[FAIL]`, and `[ERROR]` as appropriate. Color must never be the only status signal.
- Keep check/action summaries and bounded output excerpts readable, with stdout and stderr labeled and log paths easy to find. Preserve current execution order, failure behavior, exit status, and log retention.
- Sanitize agent-supplied text and displayed child-output excerpts before writing to the terminal: strip terminal-control sequences, preserve line feeds and tabs, and visibly escape other control characters. Keep captured child output and logs unchanged.
- Keep dry-run output human-readable and observational. Do not add spinners, cursor manipulation, or full-screen terminal behavior.

### Color behavior

- Add `--color <auto|always|never>` to workflow invocations only. Default to `auto`; authentication command behavior and formatting remain unchanged.
- In `auto`, emit ANSI styling only when the relevant output stream is a terminal, `TERM` is not `dumb`, and `NO_COLOR` is not set to a non-empty value. Detect stdout and stderr independently.
- `never` emits no ANSI escape sequences. `always` forces Spec Loop styling even when output is redirected or `NO_COLOR` is set.
- Use a consistent semantic palette: bold cyan for stages, green for `[PASS]`, yellow for `[BLOCKED]`, `[RETRY]`, and `[WARN]`, red for `[FAIL]` and `[ERROR]`, and dim styling for secondary metadata. Keep explicit text labels in every mode.
- Apply styles only to formatter-owned terminal presentation, never to logs, command arguments, agent prompts, or captured child output. Sanitize untrusted control sequences before formatting.

### Compatibility and safety

- Do not change role JSON schemas, protocol validation, retry behavior, workflow state transitions, check/action execution, or the raw log format.
- Keep stdout/stderr suitable for redirection. Redirected output must contain no Spec Loop ANSI styling in `auto` mode; untrusted text must not inject terminal-control sequences in any color mode.
- Add no output/color dependency unless required and reviewed under repository dependency policy.
- Update CLI help and README with the concise/verbose result format and color behavior. Document that `--verbose` shows validated typed fields, not raw JSON.

## Acceptance criteria

1. Tests cover concise and verbose rendering for every role response family, including ready, blocked, no-issues, findings, audit classifications, and clarification citations. Concise previews obey the 160-character limit and visibly direct users to `--verbose`; verbose output includes all substantive typed fields without truncation.
2. Workflow tests prove invalid typed responses show validation/retry diagnostics and the exact final-response log path without printing raw JSON or presenting it as a successful result. Raw response bytes remain unchanged in the existing run log.
3. Progress, check, review, archive, and action output uses consistent headings/spacing and explicit status labels. Check/action ordering, captured output, failure handling, exit status, and log retention remain unchanged.
4. Color tests cover `auto`, `always`, and `never`, terminal versus redirected streams, `NO_COLOR`, and `TERM=dumb`; stdout and stderr detection are independent. `never` and redirected `auto` output contain no Spec Loop ANSI styling; `always` output does, even with `NO_COLOR` or `TERM=dumb`.
5. Sanitization tests cover agent text and child-output excerpts containing ANSI/terminal-control sequences and other control characters. Display output cannot inject terminal controls; line feeds and tabs remain readable; captured bytes and raw logs remain unchanged.
6. Color-disabled output remains understandable using explicit text labels alone. Formatter styling and untrusted terminal controls do not leak into logs, child processes, prompts, or argv.
7. Dry-run and authentication commands retain their behavior; `--color` and `--verbose` apply to workflow invocations only. CLI help documents both options accurately.
8. README documents concise and verbose role-result presentation, color detection/overrides, sanitization, and plain-text redirection behavior.
9. Tests use captured/fake output and require no live model, network, credentials, or terminal-specific manual step.
10. Run `make check` after implementation.

## Non-goals

- Changing agent response schemas or adding a machine-readable stdout mode.
- Changing authentication command formatting, workflow policy, role prompts, retries, or check/action semantics.
- Adding an interactive TUI, progress animation, log viewer, or new log files.
- Truncating substantive validated result fields in verbose mode or replacing retained raw response logs.
