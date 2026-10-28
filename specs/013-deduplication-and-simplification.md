# Spec 013: Deduplication and Simplification

**Status:** Active

## Goal

Reduce duplicated policy, redundant wrappers, and unnecessary internal plumbing in the existing harness without weakening safety guarantees. Preserve observable behavior except for the narrowly scoped non-auditor blocker-adjudication contract specified below. Prefer small shared functions and existing types over new frameworks, traits, or layers. This is a bounded internal refactor with one explicit workflow correction, not a claim that built-in execution is complete.

## Current behavior and scope

The code review identified these concrete targets. Paths and symbols describe the starting implementation; verify them against source and tests before refactoring.

| Area | Existing duplication or complexity | Starting locations |
| --- | --- | --- |
| Semantic validation | Shared parsing validates intrinsic response constraints, then workflow callbacks repeat them; evidence helpers repeat citation and uniqueness checks. | `src/protocol/mod.rs`: `parse`, `ResponseFields`, `validate_evidence_result`, `validate_evidence`, `validate_audit`, `validate_classifications`; `src/workflow/execution.rs` |
| Audit and clarification | Plan and implementation branches repeat validation, blocked handling, and extraction of the same result enums. | `src/workflow/execution.rs`: `request_evidence`, `invoke_audit` |
| Backend invocation | Both backends implement forwarding paths for `run` and `run_with_continuation`; built-in `RoleTask` and `RoleInput` carry nearly identical data. | `src/backend/mod.rs`, `src/backend/builtin/mod.rs`, `src/backend/pi/mod.rs` |
| Schema handling | Prompt and provider paths repeat the eight-role response-type dispatch; generated schemas are serialized and immediately parsed back into JSON values. | `src/prompts/mod.rs`: `schema_for_role`; `src/backend/builtin/mod.rs`: `provider_schema`; `src/protocol/mod.rs`; `src/protocol/provider/mod.rs` |
| Process timing | Supervision carries relative timeout, absolute deadline, and start time even though the production duration entry point already converts to a deadline. | `src/commands/mod.rs`: `ProcessOptions`, `ReaderWait`, `deadline_reached`, process entry points |
| Auth persistence | Four callback type parameters and their bounds propagate through commit, activation, and rollback helpers that use only subsets of those operations. | `src/auth/mod.rs`: `RegistrationPersistence`, `persist_registration_with`, commit/activation/rollback helpers |
| Provider test fixtures | Tests repeat listener setup, request capture, response formatting, and thread joining; provider tests import shared fixtures from another test module. | `src/backend/builtin/tests.rs`, `src/backend/builtin/provider/tests.rs` |

Smaller cleanup targets are also in scope:

- Identical `add_counters` and `add_options`, plus forwarding-only `display_option`, in `src/backend/builtin/usage/mod.rs`.
- Forwarding-only `resolve_optional` and duplicate metadata lookup across `resolve_existing` and `validate_regular_file` in `src/paths/mod.rs`.
- Token-scope extraction in `credential_from_token` duplicating existing `token_scopes` in `src/auth/mod.rs`.
- Repeated enumeration of the same eight prompt-file collections in `src/config/mod.rs` and `src/workflow/mod.rs`.

## Requirements

### 1. Validation ownership

- Keep one shared cross-backend implementation of intrinsic role-response validation: exact shape, required and unknown fields, role/version/status, non-empty text, summary constraints, finding structure, audit consistency, and evidence structure.
- Remove workflow callbacks that merely repeat validation already performed by the shared typed parser. Keep input-dependent checks in the same-role validation/retry path, including requested evidence IDs, one assessment per finding, and finding order. Do not move these checks after successful handoff in a way that turns a correction retry into a workflow failure or acceptance of invalid data.
- Share intrinsic evidence/citation and audit-assessment validation rather than maintaining duplicate implementations. Standalone validators that accept unvalidated data must retain their existing guarantees.
- Preserve validation after audit reports are merged. Revalidation at a distinct boundary is not redundant merely because its component inputs were previously validated; deduplicate the validation implementation, not necessary safety checks.
- Preserve deterministic missing/unknown-field diagnostics, including nested paths and existing asserted diagnostic text. Every valid `blocked` response from a non-auditor agent role enters the corresponding auditor adjudication flow in requirement 2. A valid `blocked` response from an auditor stops immediately without recursive auditing. Malformed or inconsistent responses still retry the same role.

### 2. Audit and clarification branching

- Retain role-specific typed response dispatch, but consolidate shared result validation, blocked handling, and extraction for plan and implementation audit/clarification calls. Handle common `AuditResult` and `EvidenceResult` values once where behavior is identical.
- A completed plan or implementation review with zero findings and no genuine external blocker must return `no_issues`, not `blocked`. A genuine unavailable external prerequisite may require `blocked` even when there are zero findings; an empty finding set by itself is not a blocker. More generally, when review finds no actionable gaps and no genuine external blocker, the reviewer must return `no_issues`.
- Treat any valid `blocked` response from a non-auditor agent as a claim for the corresponding auditor to assess, not as proof that progress is impossible. Route `plan_author`, `plan_review`, and `plan_review_clarification` blockers to `plan_audit`; route `implementation`, `review`, and `review_clarification` blockers to `review_audit`. Preserve the original typed response and exact reason in the audit input and run logs. The auditor assesses only that blocker against the active spec, repository instructions, and available evidence; it must not search for unrelated findings, approve the plan or implementation, change repository files, or synthesize findings from prose. Preserve role-specific review-context delegation and selected-backend tool policy.
- Require a strict typed blocker adjudication with an explicit outcome (`confirmed`, `unsupported`, or `ambiguous`) and evidence-backed rationale. A confirmed blocker must identify the unmet requirement, genuinely unavailable prerequisite, why available tools cannot resolve it, and the input or approval needed to unblock progress. An absence of actionable findings is not itself a blocker; do not invent findings or missing prerequisites. A confirmed blocker stops with actionable diagnostics; an ambiguous adjudication also stops and identifies the unresolved question for the user.
- Distinguish finding-classification audits from non-auditor blocker adjudications in the typed auditor response. Validate the response kind against the requested audit kind inside the same auditor retry loop, alongside input-dependent finding-ID/count/order checks. A finding-classification response cannot satisfy a blocker-adjudication request, and a blocker-adjudication response cannot satisfy a finding-classification request; either mismatch is invalid for that attempt and retries the same auditor. A valid auditor `blocked` response remains an immediate stop, not a kind mismatch to be retried.
- An `unsupported` blocker returns audit feedback to the same originating non-auditor role for a corrected typed response; it never completes or approves the workflow automatically. Any renewed `blocked` response from that role is adjudicated again. Preserve attempt limits, timeouts, cancellation, and completed effects; do not replay completed side effects or rerun unrelated completed roles, checks, or actions merely to correct the handoff. Malformed adjudications retry the auditor. A valid `blocked` response from either auditor stops immediately; do not recursively audit auditors. Integrity violations, interruption, fatal transport/provider failures, configured check failures, and lifecycle-action failures are not blocker claims and retain their existing handling without auditor approval.
- Preserve requested-finding selection, omission of uncited evidence, follow-up auditing, classification order and merging, and repair handoff contents.
- Preserve differences between plan and implementation policy, including review-context delegation and handling of unresolved ambiguity. Keep outer plan and implementation workflow loops separate: configured checks, archival, and lifecycle actions are not interchangeable with plan approval.
- Do not introduce a generic workflow engine or a role plugin framework to remove these branches.

### 3. Backend invocation plumbing

- Use one backend invocation entry point carrying the existing optional continuation and reporting information instead of requiring parallel ordinary/continued forwarding implementations in each backend and fake backend.
- Retain the backend trait and deterministic fake-backend injection. Keep continuation state backend-owned and opaque to workflow policy; do not expose built-in conversation mechanics to the workflow.
- Consolidate equivalent built-in task/input structs and forwarding layers. Preserve injectable credentials, endpoints, reporting, and other concrete test seams without retaining duplicate production argument bundles solely for test convenience.
- Preserve preflight-before-credentials/provider-access behavior, fatal versus retryable failures, correction-only continuation, and rejection of tools during correction. Completed shell effects must never be replayed to repair response formatting.
- Preserve role/spec-scoped usage accumulation and observation boundaries, including reports on failure, interruption, and correction. Pi must not gain built-in usage reporting or built-in tool behavior.

### 4. Canonical schema handling

- Centralize the role-to-canonical-response-schema dispatch used by prompts and provider request construction. Keep generated schemas as JSON values internally; serialize at text boundaries rather than immediately serializing and reparsing them within schema generation, validation, or projection.
- Preserve one canonical typed schema source per role and every existing role wire form, status variant, and required/unknown-field constraint. Add the blocker-adjudication outcome as a strict typed branch of the corresponding existing auditor response contract; preserve all existing response forms and the protocol version.
- Keep the provider schema a mechanical projection of the canonical schema, including the adjudication branch. Preserve strict Structured Outputs, exact transport-envelope normalization, fail-closed unsupported-schema handling, and local semantic validation. Do not substitute a handwritten provider contract or a weaker format.
- Keep provider-specific projection at the transport boundary and shared protocol policy backend-independent. Do not add global schema caches, a schema registry framework, dependencies, or a protocol-version change for this cleanup.

### 5. Process deadlines

- Convert configured relative durations into checked absolute deadlines at process entry boundaries. Use one absolute-deadline representation throughout child waiting and output-reader supervision, including the test-only OS-argument entry point.
- Remove redundant relative-timeout/start-time plumbing from internal supervision. Preserve existing production deadline boundaries and timeout/cancellation outcomes, including cancellation before spawn and an already-expired deadline.
- Preserve process-group termination, descendant cleanup, and deadline enforcement while descendants hold output pipes after the direct child exits. Continue draining both streams without deadlock under the shared output budget.
- Preserve spawn-observer ordering, exit status, captured output, and truncation reporting. This does not authorize changing output budgets, timeout configuration, shell semantics, or process isolation.

### 6. Auth persistence plumbing

- Reduce repeated callback type/bound declarations and argument propagation around `RegistrationPersistence`. Prefer cohesive methods on the existing persistence object, or an equally small design, over a new storage framework.
- Retain injectable storage operations and failure testing. Preserve identity-bound registration checks, metadata staging before credential writes, activation only after successful persistence, file fallback eligibility, and owner-only storage protections.
- Preserve existing authentication transaction restoration: restore prior credentials and registry metadata where currently required, retain existing credentials on failure, and report both the original failure and any restoration failures. This requirement concerns existing auth persistence only; it does not authorize workspace, command, or workflow rollback.
- Do not change OAuth scopes, refresh policy, account selection, registry format/location, or credential-store fallback policy. Treat this as higher-risk cleanup and retain its failure-path coverage, not merely successful login coverage.

### 7. Shared provider test support

- Extract common fake-provider HTTP/SSE fixture mechanics into a filesystem-backed test-support module shared by built-in and provider tests. Tests must not import reusable fixture infrastructure from a sibling module's test cases.
- Reuse small helpers for scripted responses, request capture, and server cleanup. Preserve request inspection and scenarios involving multiple turns, retries, usage observations, malformed/partial output, refusal, incomplete responses, correction, timeout, and interruption.
- Keep timing-sensitive synchronization and deliberately malformed transport scenarios explicit where their mechanics are part of the test. Ensure failed expectations cannot leave fixture servers waiting indefinitely.
- Keep tests isolated and deterministic, with temporary non-Git project directories, fake credentials, and local fixtures only. No live model, real credentials, external network, Git installation, or developer-worktree state may be required. Loopback fake HTTP servers remain permitted.
- Do not add a mock-server dependency or a general-purpose HTTP testing framework. Preserve scenario coverage and assertions; reducing test count is not a goal.

### 8. Small shared helpers and redundant wrappers

- Replace identical optional checked-add implementations in usage reporting with one helper and remove the forwarding-only display wrapper. Preserve overflow-to-unknown behavior, unavailable versus zero distinctions, and displayed output.
- Remove forwarding-only optional path resolution and avoid performing metadata lookup twice for the same regular-file validation. Retain actionable operation/path/causal errors and rejection of non-regular files; do not add new path-security policy.
- Reuse existing token-scope extraction for initial credential conversion and refresh, preserving scope order and missing-scope behavior.
- Provide one enumeration of configured prompt-file collections used by both structural path validation and filesystem resource validation. Preserve coverage of all eight roles and each validator's separate responsibility; do not merge configuration validation with filesystem effects.

### 9. Scope and safety

- Preserve CLI/profile formats and defaults, retry eligibility and limits, timeout behavior, check ordering, review gates, action ordering, selected-input integrity protection, and external run-log handling. The only intentional user-facing behavior change is non-auditor blocker adjudication as specified in requirement 2. Preserve existing diagnostic contracts and useful causal context outside that change; this is not a general output redesign.
- Preserve Pi's effective settings, tools, extensions/MCP/skills/trust/context, inherited environment, and invocation behavior. Built-in roles retain their existing automatic unsandboxed shell behavior and credential security boundary. No additional approval, shell expansion for configured commands, sandbox, Git integration, snapshotting, or concurrent-mutation detection is introduced.
- Preserve user files and completed side effects on workflow failures. Only selected spec/plan hashes trigger integrity stops. Do not weaken guards to reduce code size.
- Add no dependencies, speculative public API, generic workflow/storage/plugin frameworks, or new configuration. Do not fold unrelated cleanup into this change. No arbitrary line-reduction quota applies; success means less duplicated policy and plumbing with equally explicit control flow.
- Organize implementation into coherent, independently testable slices, beginning with low-risk helper/validation cleanup before higher-risk backend, process, and auth changes. Slices sequence the work; all listed targets remain required.

## Acceptance criteria

1. All seven main targets and all smaller cleanup targets are addressed. Final implementation report maps each target to changed files and observed validation; it does not substitute file moves or renamed duplicates for shared implementation.
2. Protocol tests preserve valid and invalid responses for every existing role/status and the new typed blocker-adjudication outcomes, including unknown/missing/nested fields, summary constraints, non-sequential findings, conflicting classifications, duplicate/unrequested evidence IDs, invalid citations, and blocked reasons. Fake-backend workflow tests show intrinsic and input-dependent failures still retry the same role; `blocked` results from every non-auditor role enter the corresponding auditor adjudication, while a valid `blocked` result from either auditor stops without recursive auditing.
3. Plan and implementation audit/clarification tests preserve supported-required repair, supported-not-required non-repair, unsupported/ambiguous clarification, empty/omitted citations, follow-up merge validation, finding order, and their distinct ambiguity/review-context behavior.
4. Normal and correction backend invocations use the consolidated interface. Fake backend/provider tests preserve no-replay behavior, preflight before credential/provider access, fatal/retryable classification, role/spec-scoped usage, and Pi's unchanged effective invocation and lack of built-in telemetry.
5. For every role/status, tests compare canonical schema semantics and provider projection with the existing contract, inspect strict provider requests, and verify exact envelope normalization and shared local rejection of malformed or semantically inconsistent responses. Unsupported projection, refusal, and incomplete transport still fail closed without fallback or shell replay.
6. Process tests cover relative-duration entry points and absolute deadlines, expired deadlines, cancellation before/after spawn, nonzero exits, successful-spawn observation, concurrent stdout/stderr capture and shared-budget truncation, and termination of pipe-holding descendants both before and after direct-child exit. Process/platform assumptions remain explicit.
7. Auth persistence tests cover successful keyring and file activation, existing/new registration failures, failed credential writes, failed registry activation, restoration of prior credentials/metadata, and restoration failures retaining original diagnostics. Tests remain independent of the real credential store and live OAuth services.
8. Shared fake-provider fixtures serve both affected test modules without sibling-test infrastructure imports, preserve existing transport/retry/usage/correction scenarios and assertions, and terminate on failure instead of hanging indefinitely.
9. Focused tests retain usage overflow/unknown/zero behavior, regular-file path errors, token-scope extraction behavior, and validation of every prompt-file collection. Existing workflow checks/review/action ordering and selected-input integrity regressions continue to pass.
10. No protocol/profile migration, new dependency, unsupported completion claim, weakened safety check, or reduced scenario coverage is introduced. Update the README to explain the non-auditor blocker-adjudication flow, the `no_issues` rule when there is no genuine blocker, and the outcomes that stop or return the originating role for correction; keep documentation limited to that user-visible behavior change. Run targeted tests during iteration and final `make check` after implementation. Report actual results and any blockers; no live compatibility claim follows from offline tests.
11. Deterministic fixtures cover every non-auditor role under both backends, route each blocker to its corresponding auditor, and cover confirmed, unsupported, and ambiguous adjudications; a role returning `blocked` while its reason reports no actionable gaps and no genuine external blocker; a genuine external blocker with zero findings remaining `blocked` and entering adjudication; both audit-kind mismatches and malformed adjudications retrying the same auditor; corrected responses from the same originating role without automatic completion; renewed blockers; attempt exhaustion; and preservation of files and completed effects. Verify that auditor `blocked` stops without recursion and that integrity violations, interruption, fatal transport/provider failures, configured check failures, and lifecycle-action failures do not enter auditor adjudication. An unsupported blocker must never directly trigger approval or lifecycle actions.

## Non-goals

- Completing or extending built-in execution, changing provider/auth support, adding tools, or establishing live OAuth/model compatibility.
- Unifying outer plan/implementation workflows or replacing backend testability with backend-specific engine policy.
- Redesigning existing role protocols, provider projection, retry policy, telemetry presentation, command execution, or credential transaction semantics. The additive typed blocker-adjudication branch defined above is the sole role-contract exception.
- Adding path traversal/symlink-containment/archive-collision policy, workspace rollback, Git state tracking, or process sandboxing.
- Broad performance optimization, new caching, benchmarks without a demonstrated requirement, or minimizing source lines at the expense of readability and safety.
