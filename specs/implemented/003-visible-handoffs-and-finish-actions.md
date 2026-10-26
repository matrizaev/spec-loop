# Spec 003: Visible Handoffs and Finish-Action Updates

**Status:** Implemented

## Goal

Make implementation progress visible, require PlanAuthor to complete research
itself, and allow an explicitly configured `on_finish` action to update the
selected spec's completion status after the final selected-spec integrity check.

## Existing work to preserve

The working tree already contains user-requested implementation-summary
protocol/display changes and stronger PlanAuthor research instructions. Verify
and preserve these behaviors; complete any missing validation, regression tests,
or documentation rather than reverting them:

- Implementation responses carry a required, short factual summary, and the
  workflow displays it when transitioning into automated checks.
- PlanAuthor is instructed to perform required research and produce an
  actionable implementation handoff instead of deferring research to the
  implementer.

## PlanAuthor research and handoff

- Make PlanAuthor responsible for researching the selected spec and repository
  instructions before drafting: inspect relevant source, tests, configuration,
  documentation, APIs, and available references; resolve consequential design
  decisions; and run safe baselines or probes when their results affect design
  or acceptance.
- Do not return `blocked` because research, inspection, lookup, benchmarking,
  or design work is needed, incomplete, difficult, or time-consuming. Use
  available Pi tools to do that work. Research itself is never a blocker.
- Return `blocked` only when compliant planning or implementation depends on a
  genuinely unavailable external input, access, or approval that available
  tools cannot obtain and no compliant alternative exists. State the exact
  prerequisite, why it is necessary, what was investigated, and what user input
  would unblock it.
- A ready plan maps every acceptance criterion to ordered implementation work
  and tests; identifies affected files and APIs; resolves consequential data,
  error, and state-transition decisions; and records evidence, exact checks,
  and relevant baselines. Do not defer executable in-scope research or decisions
  to implementation.

## Implementation summary protocol

- Require every implementation response, including `blocked`, to contain a
  non-empty top-level `summary` string in its generated typed schema. Limit it
  to 500 characters, reject whitespace-only or control-character-containing
  values, and retry semantically invalid responses through the existing role
  retry behavior.
- Prompt the implementation role for a concise, factual, single-line summary
  of work completed so far, in one or two sentences. It must not claim work that
  was not done.
- When `ready_for_review` advances workflow to automated checks, print the
  summary alongside that stage transition, before checks start. A `--review`
  run that skips implementation has no implementation summary to print.
- Preserve the existing readiness and blocked status semantics. A summary does
  not imply completion or approval.

## `on_finish` selected-spec hash timing

- Preserve selected-spec hash checks through implementation, checks, review,
  archival, and all `on_review_success` actions. Continue to move an archived
  spec unchanged and verify its content when updating the tracked archive path.
- Immediately before the first configured `on_finish` action, verify selected
  spec and selected plan hashes. This is the last selected-spec hash check for
  that spec during this workflow pass.
- After `on_finish` starts, do not verify the selected spec hash after an action
  or between successive `on_finish` actions. This permits an explicitly
  configured, user-authorized finish action to update a completion/status line
  in the selected spec. With archival enabled, `{spec_path}` still resolves to
  the archived destination; otherwise it resolves to the original path.
- Continue verifying the selected plan hash before and after every configured
  action, including each `on_finish` action. A plan mutation remains an
  integrity failure. `on_review_success` actions remain unable to change the
  selected spec without triggering an integrity failure.
- Preserve action ordering and failure behavior: a failed finish action stops
  later actions and continuation without retry or rollback; preserve all
  already-applied changes and side effects. A successful finish action that
  changes only the selected spec must not fail due to a post-action spec hash
  mismatch.

## Example configuration

Add a valid profile example showing an argv-based `on_finish` action that uses
`{spec_path}` to replace the literal line `**Status:** Active` with
`**Status:** Complete`. This status wording is an example convention only; the
engine does not interpret it. Identify any platform assumption of the chosen
`sed` invocation. Keep the example consistent with archive placeholder behavior
and the actual strict profile parser; do not invoke commands through a shell.

## Typed-response retry feedback

- Keep Rust role models as the single schema source of truth. Continue generating
  each role's JSON Schema from those types and include it in every attempt's
  prompt; do not add a separate hand-maintained schema file for agents to read.
- When typed validation rejects missing or unknown object fields, report the
  exact missing and unknown field names, deterministically, with the role
  context. Preserve other actionable validation details for semantic failures.
- Retry feedback must identify the specific defect and request one corrected
  JSON object only. Tell the role to repair the handoff, not repeat or alter
  workspace work solely to fix JSON formatting. It must follow its normal role
  contract if substantive work remains and base any status or summary on actual
  current workspace state; never infer completion from the malformed response.
- Preserve the previous response and include the generated role schema on
  retries. Invalid attempts remain incomplete and retry the same role.

## Acceptance criteria

1. PlanAuthor prompts explicitly assign required research to PlanAuthor, state
   that incomplete research is not a reason to return `blocked`, and permit
   `blocked` only for genuinely unavailable external prerequisites. Prompt tests
   cover these instructions.
2. PlanAuthor prompt still requests an evidence-backed, actionable plan with
   ordered work and tests for every acceptance criterion. Existing plan review
   and implementation handoff behavior remains intact.
3. The generated implementation schema requires `summary` for both readiness
   and blocked responses. Runtime validation rejects missing, blank, multiline
   or control-character-containing, and over-500-character summaries. Tests
   cover valid readiness and blocked responses and each invalid case.
4. A workflow integration test observes the implementation summary printed
   with the automated-check stage before check execution. Review-only mode does
   not print an implementation summary.
5. Fake-action integration tests prove selected-spec changes by
   `on_review_success` still fail and are preserved; a successful `on_finish`
   action may change the selected spec and the workflow proceeds; and multiple
   finish actions retain order without selected-spec hash checks between them.
6. Selected-plan changes by an `on_finish` action still fail integrity checks
   and remain preserved. Nonzero finish actions still stop later actions and
   continuation without rollback.
7. Tests cover both archival-enabled and archival-disabled `{spec_path}` values
   for the status update, and verify the archived move itself preserves spec
   bytes before `on_finish` runs.
8. At least one example profile demonstrates the supported status-update
   action with argv (no shell string) and validates against the actual profile
   parser.
9. Run `make check` after implementation.
10. Typed retry diagnostics identify exact missing and unknown fields. Tests
    cover both sets and verify retry feedback asks for corrected JSON only,
    preserves the prior response/schema context, and prevents workspace edits
    made solely to repair the handoff. Existing same-role retry/fail-closed
    behavior remains intact.

## Non-goals

- Allowing agents, the Spec Loop engine, checks, or `on_review_success` actions
  to edit the selected spec.
- Changing selected-plan integrity protection.
- Adding shell hooks, status-line parsing to spec discovery, or built-in
  completion-status policy.
- Changing archive naming, selection, or continuation policy.
