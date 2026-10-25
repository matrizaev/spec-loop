# Spec 002: Decouple Spec Loop from Git

**Status:** Active

## Goal

Remove Git as a built-in workflow dependency. Spec Loop must discover its project
from `.spec-loop.toml`, select specs from the filesystem, and run checks and
review without built-in Git inspection. Repository owners may configure a
review-context command and commands to run after successful review or when a
workflow finishes. The default review-context command is `git diff`; users may
replace it. These commands may invoke Git tools such as `git diff`, `git add`,
and `git commit`; Spec Loop itself has no Git-specific integration for state,
staging, or commits.

This spec supersedes conflicting Git-specific requirements in SPEC-001. Other
SPEC-001 requirements remain in force unless changed here.

## Scope

- Remove `src/git.rs` and all engine calls to Git for root discovery, `HEAD`
  validation, file discovery, change identity, ignore checks, staging, and
  commit.
- Do not require a Git worktree, committed profile, or valid `HEAD`. The default
  review-context command is `git diff` and runs through the review agent; users
  without Git configure another command.
- Keep Git usable as an ordinary executable in configured checks and lifecycle
  actions. Such commands are repository-owner policy, not built-in engine
  behavior. All configured commands run in the Spec Loop process's current
  working directory; no command supports a configurable `cwd`.
- The selected spec and selected plan are the only integrity-protected inputs.
  Detect their content changes with hashes and stop while preserving edits. Do
  not detect changes to other files, snapshot the whole tree, roll back changes,
  or add concurrent-operator detection.
- Replace marker- and heading-based agent handoffs with strict, typed JSON
  contracts. Do not infer workflow state from prose or line parsing.
- Remove the `acceptance.manual_gates` feature, including profile parsing,
  reporting, and workflow blocking.
- Keep Pi as owner of its tools, extensions, MCP servers, skills, trust, context,
  and inherited environment. Do not sandbox agents.

## Project root and spec discovery

1. The process current working directory is the project root. Require
   `.spec-loop.toml` directly in that directory and fail with an actionable
   error if it is missing. Do not search parent directories. Git metadata has
   no role in root discovery.
2. Resolve profile paths relative to that root. Configured checks and lifecycle
   commands run from the process current working directory, which is this root;
   do not accept per-command working directories.
3. Discover active specs from filesystem paths matching `specs.active_globs`.
   Git tracking and ignore rules do not affect discovery. Repository owners
   control spec eligibility through active globs and ID rules. The engine does
   not filter agent file access by Git state, ignore rules, or path allowlists;
   agents choose which repository files are relevant to inspect.
4. Reject duplicate active spec IDs. Do not discover archived files unless they
   also match an active glob; archive configuration is not a separate discovery
   or review policy.
5. Check selected spec and plan hashes at existing integrity checkpoints.
   Changes to any other file never trigger change detection, restart checks, or
   re-review, regardless of which process made them or when they occur.

## Review behavior

- Normal implementation still runs implementation, configured checks, review,
  finding audit, and supported repairs.
- `--review` skips the implementation writer and runs checks and review against
  the current workspace. It no longer requires a Git-visible patch or checks
  whether the workspace is dirty.
- Review agents receive the selected spec, approved plan, repository
  instructions, check summaries, and paths needed for inspection. They inspect
  relevant repository state using configured Pi resources and decide which
  files are needed. The engine imposes no file-read allowlist or Git-ignore
  filter and does not construct or promise a complete Git diff.
- Every effective profile has one review-context command under
  `[review.context]`. When omitted, default to:

  ```toml
  [review.context]
  argv = ["git", "diff"]
  ```

  Users may replace the command with any argv-based command. The command runs
  in the Spec Loop process's current working directory (the project root); no
  `cwd` override is configurable. Review-context argv is passed through unchanged
  and is never placeholder-expanded; braces are ordinary argument text. Include
  exact argv and that working directory
  in prompts for implementation review, review evidence
  clarification, and implementation finding audit; do not include command
  output. These agents run the command using configured Pi tools and inspect its
  tool result. Plan-review roles do not receive this command. If an implementation
  reviewer, implementation evidence-clarification role, or implementation
  finding auditor cannot run it or reports command failure, that role returns
  typed `blocked`; Spec Loop stops immediately and preserves changes.
  The engine never executes or captures the command. It imposes no scope
  restrictions or re-review after success actions or later `--continue`
  iterations; configured commands own their effects.
- Only selected spec and plan hashes trigger integrity stops. Changes to any
  other files—before, during, or after checks/review—do not trigger workflow
  restarts or re-review. Each check and reviewer sees workspace state when it
  runs; no change snapshots or concurrent-operator detection are performed.
- Archive moves remain filesystem operations. An enabled archive uses a
  destination directory and preserves the selected spec's filename. Git ignore
  rules do not block archival.

## Structured stage protocol

Use JSON for every machine-consumed Pi role response and handoff. Do not support
YAML. Define role-specific Rust types as canonical schemas, derive JSON Schema
2020-12 with `schemars`, serialize validated handoffs as JSON, and include the
relevant generated schema in each role prompt. Do not maintain separate
hand-written schema copies.

- Every response is exactly one JSON object with required `protocol_version`
  (integer `1`) and role-specific `role` string. No Markdown fences, preamble,
  trailing prose, unknown fields, or missing fields. Generated schemas encode
  required fields, enums, non-empty strings, statically defined array bounds,
  and `additionalProperties: false`. Do not impose fixed maximum array sizes.
  Validate role/version and input-dependent array cardinality, ID, and
  cross-field constraints in Rust after deserialization.
- Deserialize the complete final assistant content into the expected role type;
  reject the response if it is not one valid JSON object matching that type. Do
  not extract JSON from prose, parse status markers, split headings, use regex,
  or infer status from natural language. Pi's outer JSONL transport remains
  unchanged.
- Rust structs/enums are the source of truth for handoff shapes. Generate JSON
  Schema from those types; do not maintain a second hand-written schema. Pass
  downstream stages validated typed values serialized as canonical JSON, never
  raw free-form responses. Raw responses may remain in run logs for diagnosis.
- A valid `blocked` result stops workflow immediately, preserves changes, and
  does not retry that role. Retry malformed or semantically inconsistent JSON
  from the same role using the existing role retry limit. Malformed output never
  means approval or `NO_ISSUES`; fail closed when retries exhaust.

Required role schemas:

1. Plan author and implementation return a readiness or blocked result:

   ```json
   {"protocol_version":1,"role":"plan_author","status":"ready_for_review"}
   {"protocol_version":1,"role":"plan_author","status":"blocked","reason":"..."}
   {"protocol_version":1,"role":"implementation","status":"ready_for_review"}
   {"protocol_version":1,"role":"implementation","status":"blocked","reason":"..."}
   ```

   `role` must match the invoked role exactly. `status` is `ready_for_review` or
   `blocked`. `reason` is a required non-empty string only for `blocked` and is
   forbidden otherwise.

2. Plan reviewer and implementation reviewer return:

   ```json
   {
     "protocol_version": 1,
     "role": "review",
     "status": "issues",
     "findings": [{
       "id": 1,
       "requirement": "...",
       "location": "...",
       "evidence": "...",
       "violation": "...",
       "proposed_correction": "..."
     }]
   }
   ```

   `role` is `plan_review` or `review`. `status` is `no_issues`, `issues`, or
   `blocked`. `no_issues` requires an empty `findings` array; `issues` requires a
   non-empty `findings` array; `blocked` requires a non-empty `reason` and omits
   `findings`. The schema sets `minItems: 0` and `maxItems: 0` for `no_issues`,
   and `minItems: 1` with no `maxItems` for `issues`. `reason` is forbidden for
   other statuses. Finding fields
   are all required non-empty strings except `id`, which is a positive integer.
   IDs are sequential from 1 in array order. Finding text is stored once as
   typed data and cannot be rewritten by later stages.

3. Plan and implementation auditors return one classification per input finding:

   ```json
   {
     "protocol_version": 1,
     "role": "review_audit",
     "status": "complete",
     "classifications": [{
       "finding_id": 1,
       "classification": "supported",
       "rationale": "..."
     }]
   }
   ```

   `role` is `plan_audit` or `review_audit`. A completed audit has
   `status: "complete"` and one classification per input finding. `blocked`
   instead has a non-empty `reason` and omits `classifications`.
   `classification` is exactly `supported`, `unsupported`, or `ambiguous`;
   `rationale` is required and non-empty. Audits are invoked only for non-empty
   finding lists, so the schema sets `classifications.minItems` to 1 and has no
   `maxItems`. Runtime validation requires exactly one classification per input
   finding, in input order; unknown, missing, or duplicate IDs fail validation.
   This input-dependent exact bound is not encoded in the standalone schema. The
   engine derives workflow outcome from classifications.

4. Evidence clarification returns citations only:

   ```json
   {
     "protocol_version": 1,
     "role": "review_clarification",
     "status": "complete",
     "evidence": [{
       "finding_id": 1,
       "citations": [{
         "path": "src/example.rs",
         "location": "function name or line range",
         "excerpt": "...",
         "relevance": "..."
       }]
     }]
   }
   ```

   `role` is `plan_review_clarification` or `review_clarification`. A completed
   response has `status: "complete"` and `evidence`; `blocked` has a non-empty
   `reason` and omits `evidence`. The schema permits empty `evidence` and
   `citations` arrays (`minItems: 0`) and imposes no fixed maximum. Runtime
   validation permits at most one evidence entry per requested finding ID and
   requires every entry to refer to a requested ID; these input-dependent bounds
   are not encoded in the standalone schema. Citation fields are required
   non-empty strings. This
   response cannot add, remove, or rewrite findings; omitted IDs provide no
   evidence. The engine retains original findings and passes citations as typed
   data to grounding.

- Remove plan-review and implementation-review status-auditor roles and their
  profile prompt fields. Reviewer status is a required typed enum; contradictory
  fields fail validation and retry the reviewer rather than invoking a text
  normalizer. Implementation finding auditors receive the review-context command;
  plan auditors do not. Any auditor unable to complete its work returns typed
  `blocked` and stops workflow.
- Grounding audit ambiguity remains represented by per-finding
  `classification: "ambiguous"`; existing one-time evidence clarification and
  unsupported-finding behavior remain unchanged.

## Lifecycle actions

Add ordered, argv-based action lists under `[actions]`:

```toml
[[actions.on_review_success]]
name = "stage reviewed changes"
argv = ["git", "add", "--all"]

[[actions.on_finish]]
name = "commit reviewed changes"
argv = ["git", "commit", "-m", "Implement {spec_id}"]
```

`[actions]` and either action list may be omitted; omitted lists are empty and
execute no commands.

- `on_review_success` runs once after each spec's review has no actionable
  findings and required automated checks pass. It runs after any configured
  archive move. In `--continue` mode, it runs before selecting the next spec.
- `on_finish` runs once for each spec after its checks and review succeed and
  its `on_review_success` actions complete. In `--continue` mode, it runs before
  selecting the next spec. It does not run for a spec that fails before this
  point, in dry-run, or in plan-only mode. Actions already run for a spec or an
  earlier completed spec remain if a later action or spec fails or is
  interrupted.
- Action commands run sequentially from the process current working directory,
  inherit the process environment, and use argv arrays rather than shell-string
  parsing. Existing `{spec_id}`, `{spec_path}`, and `{plan_path}` placeholders
  are supported in lifecycle action argv. Unknown, unmatched, or unclosed
  placeholder syntax is a configuration error, not literal argv content. At
  action time, `{spec_path}` resolves to the archived destination if the spec
  was moved, otherwise its original path; `{plan_path}` resolves to the
  configured plan path. Each action uses the spec associated with that
  completion; in `--continue` mode, `on_finish` placeholders refer to the spec
  just completed before the next spec is selected.
- Print action name and displayed argv before execution. Retain stdout and
  stderr in run logs and report nonzero exit status.
- On action failure, stop immediately, preserve all workspace changes and prior
  action side effects, and do not run later actions or continue to another spec.
  Do not retry actions or attempt rollback; action commands may have irreversible
  effects. Rerunning a workflow may execute the same actions again; do not add
  idempotency detection or cross-invocation deduplication.
- Configured actions are explicit user authorization for their commands,
  including Git staging or commit. Spec Loop provides no built-in `--commit`
  option or commit message policy.
- Keep `--continue` as the explicit CLI authorization to process later specs;
  it does not require a commit. `completion.max_passes` remains profile-only.
- `--dry-run` prints resolved actions and commands but never executes them.

## Configuration and migration

- Require the current working directory to contain `.spec-loop.toml`; do not
  search ancestor directories. Add strict profile support for effective
  `[review.context]` with non-empty `argv` (default `["git", "diff"]`). The
  command runs in the process current working directory; do not support a
  configurable `cwd`. Pass command argv and that working directory to
  implementation review, implementation-review clarification, and
  implementation finding-audit prompts; never execute the command or pass its
  output. Add ordered
  `actions.on_review_success` and `actions.on_finish` entries, each with `name`,
  non-empty `argv`, and optional timeout. Omitted timeout means unlimited. All
  configured checks and actions run from the process current working directory;
  remove per-command `cwd` fields from profile schema v2. Add strict typed JSON
  handoffs and generated JSON Schemas.
- Remove `completion.commit_message`; commit message belongs in the configured
  command's argv. Remove the built-in `--commit` switch. Remove
  `acceptance.manual_gates` and its reporting/blocking
  behavior; remove `[acceptance]` when no fields remain.
- Bump profile schema to version 2. Version 1 profiles fail with a clear
  migration message; no automatic migration or compatibility aliases.
- Validate configured command argv consistently: each checks, review-context, and
  lifecycle-action argv must contain a non-whitespace executable at index 0 and
  no argument may contain NUL. Template handling is separate from this shared
  structural validation. Checks retain existing exact substitutions for
  `{spec_id}`, `{spec_path}`, and `{plan_path}`; all other brace text, including
  unmatched braces, remains literal check argv data. Review-context argv is never
  expanded and reaches implementation review, clarification, and audit prompts
  unchanged. Lifecycle actions expand the three documented placeholders and
  reject unknown or malformed placeholder syntax.
- Action command failure is a workflow failure, not a reviewer finding. Preserve
  check ordering and failure/repair behavior, but remove per-check `cwd` from
  profile schema v2; checks run from process current working directory.

## Acceptance criteria

1. A complete workflow runs without `.git` or Git when configured with a
   non-Git review-context command. The default review-context command is
   `git diff`.
2. The current working directory is root. A profile there is required; missing
   `.spec-loop.toml` fails before agent invocation, without searching parents.
3. Filesystem spec discovery honors active globs, excludes paths outside them,
   rejects duplicate IDs, and does not consult Git tracking or ignore rules.
4. `--review` runs checks and supplies the effective review-context command in
   prompts for implementation review and finding audit. The default is
   `git diff`; the engine does not execute it or capture its output. Plan-review
   roles do not receive the command. If implementation review, implementation
   evidence clarification, or implementation finding audit cannot run the
   command or reports failure, that role returns typed `blocked` and workflow
   stops.
5. `on_review_success` runs once per approved spec, in configured order, before
   continuation. `on_finish` runs once per spec after its checks, review, and
   `on_review_success` actions succeed, before continuation. Neither runs in
   plan-only or dry-run. If a later action or spec fails, effects from actions
   already run remain.
6. Action failure preserves prior changes and side effects, skips remaining
   actions, prevents continuation, and returns failure without retry or rollback.
7. Review-context argv reaches implementation review, clarification, and
   finding-audit roles unchanged; its working directory is the Spec Loop
   process's current working directory (project root). The engine never
   executes or captures that command. Checks and lifecycle actions also run from
   that directory; no configured command accepts `cwd`. All command argv use
   common structural validation. Check placeholders retain existing exact-token
   expansion while unrelated brace text remains literal; review-context argv is
   unchanged; action placeholders use strict syntax validation. Action argv,
   placeholders, inherited environment, timeout, logging, and dry-run behavior are
   tested. Git
   stage/commit examples use a fake executable or temporary repository, without
   making Git a required runtime dependency. Test `{spec_path}` as archive path
   after a move and original path when archival is disabled; reject unknown or
   malformed action placeholders.
8. Every Pi role has a strict role-specific Rust model and generated JSON
   Schema. Test valid and blocked responses, missing/extra fields, wrong
   role/version, invalid enums, duplicate or missing finding IDs, contradictory
   status/findings, retry behavior, and fail-closed exhaustion. Verify static
   schema bounds: `no_issues` findings exactly empty, `issues` findings
   non-empty, classifications non-empty, and evidence/citations may be empty;
   impose no arbitrary maxima. Test input-dependent exact classification and
   evidence ID constraints in Rust. Verify schemas are generated from the Rust
   types, prompts receive the role schema, and downstream roles receive
   canonical typed JSON rather than raw prose. No
   marker/heading parsers or status-auditor roles remain.
9. Selected spec and plan hash protections remain effective around agent calls,
   checks, archival, and actions.
10. Existing checks, plan workflow, typed review/finding semantics, and
    continuation selection continue to work unless explicitly changed above.
11. `acceptance.manual_gates`, related reporting, and blocking are removed from
    config, CLI help, docs, examples, and tests. No `[acceptance]` table remains.

## Explicit non-goals

- Built-in Git status, diff, ignore, stage, commit, branch, or history support.
- Automatic complete-patch construction or whole-filesystem change snapshots.
- Rollback of agent, check, archive, or action changes.
- Shell-string command configuration, command sandboxing, or permission
  restriction.
- Concurrency detection, symlink/path-containment checks, filesystem-collision
  guards, or merge-conflict guards.
