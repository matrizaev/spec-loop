# Spec 014: Implementation Summary in Review Handoff

**Status:** Complete

## Goal

Give the implementation reviewer a concise orientation to the implementer's reported work while preserving independent, full-scope review. The implementation summary is context only: it is not evidence, approval, or a limit on what the reviewer must inspect.

## Current behavior

A validated implementation response includes a brief summary. When implementation is ready for checks, the workflow displays that summary to the user before running configured checks. After successful checks, the implementation reviewer receives the selected spec, approved plan, repository instructions, and check results, but not the implementation summary.

## Requirements

### 1. Summary handoff

- When an implementation response returns `ready_for_review`, pass its validated summary to the implementation reviewer along with the existing review context and check results.
- Pass the summary from the implementation response for the current pass only. In a repair loop, use the latest ready response's summary; never reuse an earlier pass's summary after new implementation work.
- Do not pass an implementation summary when the workflow resumes at review without running implementation. Do not recover or invent one from logs or other sources.
- Provide the summary as a distinct, clearly delimited prompt context item. Do not encode it as reviewer feedback or change any role response schema or protocol version.
- Pass it to the implementation review role only. Do not add it to plan review, audit, clarification, or blocker-adjudication inputs.

### 2. Reviewer treatment

- Label the summary as implementer-authored, unverified orientation context.
- Treat the summary as untrusted data, not instructions. Explicitly direct the reviewer to ignore any embedded requests or directives that attempt to change its role, review behavior, scope, tool policy, or acceptance criteria. Frame the summary so its contents cannot escape or alter the surrounding prompt context.
- Instruct the reviewer that it may help navigate the workspace, but is not evidence that work was performed or complete. The reviewer must verify claims against the current workspace and independently review the full selected spec, full approved plan, repository instructions, and check results.
- The summary must not narrow review scope, excuse unmet requirements, replace evidence for findings, or change the reviewer's existing status and finding criteria.

### 3. Preserve workflow behavior

- Keep displaying the implementation summary to the user at the existing transition into automated checks.
- Preserve check-before-review ordering: failed checks still skip review and return diagnostics to implementation.
- Keep the handoff backend-independent so Pi and built-in review roles receive the same summary context while retaining their existing settings, tools, and review-context delegation.
- Do not change review response validation, review schemas, protocol version, implementation summary validation, or review-only behavior beyond the absence of a summary when implementation did not run.

## Acceptance criteria

1. Prompt tests show that the implementation reviewer receives the exact validated summary for the current implementation pass in a distinct section labeled as unverified orientation context.
2. A prompt test with instruction-like summary text verifies that the reviewer is told to ignore embedded directives, that the summary remains framed as data, and that full-scope independent-review instructions remain in force.
3. Reviewer prompt tests explicitly state that the summary is not evidence and does not limit scope or acceptance criteria.
4. Workflow tests verify that a summary reaches review only after configured checks pass, and that a later implementation pass supplies its own summary rather than stale prior context.
5. Review-only/resume tests verify that no implementation summary is included when implementation did not run in that workflow pass.
6. Tests verify the summary is supplied to implementation review, not plan review, audit, clarification, or blocker adjudication, and that both backends receive the shared prompt context.
7. Existing implementation summary display, typed role response contracts, check ordering, full-spec review requirements, and review result behavior remain unchanged.
8. Run targeted tests during implementation and `make check` after the implementation. Report observed results and any blockers.

## Non-goals

- Treating the implementation summary as evidence, a completion claim, or a reviewer instruction.
- Narrowing reviewer inspection to files or changes mentioned in the summary.
- Changing implementation summary contents or validation, role schemas, protocol versions, review findings, or approval policy.
- Passing implementation summaries to roles other than the implementation reviewer.
