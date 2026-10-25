# Spec Loop

Spec Loop is a repository-agnostic Rust CLI for implementing filesystem-selected specifications through agent runs, configured checks, independent review, finding audits, supported repairs, and optional lifecycle commands. Repository-specific spec conventions, prompts, review context, checks, archive policy, and actions live in `.spec-loop.toml` and referenced files.

## Requirements

- Run Spec Loop from project root; `.spec-loop.toml` must be directly in current working directory.
- Pi CLI available as configured by `agent.executable` (default `pi`).
- Each configured check/action executable available when used.
- Git is not required by Spec Loop. Default review context is `git diff`; projects without Git should configure a different command.

Build with pinned Rust toolchain:

```sh
cargo build --release
cargo run -- --help
make check
```

See [`examples/profiles/basic.toml`](examples/profiles/basic.toml) and [`examples/profiles/planned-archive.toml`](examples/profiles/planned-archive.toml). These profiles demonstrate optional Git stage/commit commands as ordinary user-configured actions; Spec Loop itself has no Git integration for status, diffs, staging, or commits.

## Workflow

1. Current working directory is project root. Spec Loop requires `.spec-loop.toml` there and never searches parent directories.
2. Active specs are discovered from filesystem paths matching `specs.active_globs`, regardless of Git tracking or ignore rules. Duplicate IDs are rejected. Profile rules control eligibility, ID parsing, ordering, and requested-spec selection.
3. Plan authoring is separate from implementation. Only `--plan` author role may create or revise selected plan. Implementation and review roles treat selected plan as read-only.
4. Normal implementation runs implementation, every configured check in order, review, finding audit, optional evidence clarification, and repair loops. Any failed check skips review for that pass and returns diagnostics to implementation.
5. `--review` skips initial implementation and runs checks/review on current workspace, including workspaces with no Git metadata or no changes.
6. A valid typed `blocked` response stops workflow. Malformed or inconsistent typed JSON retries same role; exhausted retries fail closed. Each prompt receives a generated role-specific JSON Schema 2020-12.
7. After successful checks and review with no actionable findings, optional archival moves selected spec unchanged. Ordered `actions.on_review_success` run after archive; `actions.on_finish` run after those actions. Action commands are explicit repository-owner authorization and can have irreversible effects.

### Finding audits

Auditors act as skeptical gatekeepers, not second reviewers: they assess supplied findings and do not search for additional improvements. Typed audit results separate factual validity (`supported`, `unsupported`, or `ambiguous`) from actionability (`required`, `not_required`, or `ambiguous`). A true observation can be supported but require no change; only findings marked both supported and required enter repair. An excessive proposed fix does not invalidate a real, actionable defect: auditors identify the smallest sufficient correct fix, and repairs follow that rationale. Reject speculative hardening, hypothetical extensibility, duplicated validation, and architectural expansion without demonstrated need. Do not dismiss a proven in-scope defect because its smallest correct fix is difficult or substantial; simplicity limits unnecessary complexity, not necessary work.

### Writing specifications

Keep each spec small, coherent, and bounded around one independently reviewable outcome with observable acceptance criteria. Split broad “implement everything” requests and unrelated work into focused specs; state dependencies and intended order explicitly. Narrow specs make planning, implementation, review, and repair easier to verify.

Pi owns its effective tools, extensions, MCP servers, skills, project trust, context, and inherited environment. Spec Loop does not sandbox Pi resources. Review/grounding roles are asked not to mutate files. Only selected spec and plan contents are hash-protected; changes to other files do not trigger snapshots, rollback, or re-review. Run logs are retained outside project directory.

## Usage

```text
spec-loop [--spec ID] [--review] [--plan] [--dry-run] [--continue]
```

- `--spec ID` selects an eligible active specification. Required with `--review` and `--plan`.
- `--plan` authors/reviews a plan only. `--continue` can process later specs and skips plans already present.
- `--review` resumes at checks and review; it does not require a Git-visible patch.
- Normal mode requires existing plan at `[plan].path_template` (default `.spec-loop/plans/{spec_id}.md`).
- `--continue` advances to later eligible specs after successful review and lifecycle actions. No commit authorization is required.
- `--dry-run` prints resolved spec, plan, checks, review-context argv, and expanded actions without invoking agents or commands.
- `--commit` is not supported. Configure stage/commit behavior explicitly under `[actions]` if desired.
- `completion.max_passes = 0` means unlimited role calls. Agent, check, and action timeouts are optional; omitted means unlimited.

Completed specs remain active if archival is disabled. A later invocation can select them again; repository owners control eligibility through active globs and archive transitions.

## Profile schema v2

Profiles use strict TOML with `schema_version = 2`. Version 1 profiles fail with a migration message; no automatic migration is performed. Unknown fields fail validation. Checks and lifecycle actions use argv arrays, never shell strings, and all configured checks/actions run from current working directory. No command has a configurable `cwd`.

Review context defaults to:

```toml
[review.context]
argv = ["git", "diff"]
```

Spec Loop never executes or captures this command. It supplies exact argv and project-root working directory to implementation review, implementation finding audit, and implementation evidence clarification prompts; those roles run it through configured Pi tools and return `blocked` if it fails. Plan roles do not receive the command.

Lifecycle actions are ordered and optional:

```toml
[[actions.on_review_success]]
name = "stage reviewed changes"
argv = ["git", "add", "--all"]

[[actions.on_finish]]
name = "commit reviewed changes"
argv = ["git", "commit", "-m", "Implement {spec_id}"]
```

Supported action placeholders: `{spec_id}`, `{spec_path}`, `{plan_path}`. They expand within argv elements, not through a shell. `{spec_path}` names archived destination after a move, otherwise original spec path. Actions inherit environment, capture stdout/stderr in run logs, and stop workflow on nonzero status without retry or rollback. Omitted action lists are empty. The self-host profile defines no actions, so it does not stage or commit changes.

Configure checks as ordered `[[checks]]` entries with `name`, non-empty `argv`, and optional `timeout_seconds`. Every check runs from project root. All checks run after a failure; any failure blocks review for that pass. Agent executable, prompts, and checks are validated before agent invocation. All command argv share structural validation (non-empty executable; no NUL bytes). Checks substitute exact `{spec_id}`, `{spec_path}`, and `{plan_path}` tokens; other brace text remains literal. Review-context argv is passed unchanged. Lifecycle actions use strict placeholder validation.

Pi resource availability is controlled only by user's Pi configuration; Spec Loop does not add resource overrides. Optional per-stage model/thinking settings use `[agent.stages.planning]`, `[agent.stages.implementation]`, and `[agent.stages.review]`; omitted values preserve Pi defaults. Thinking levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.

## Self-hosting

The checked-in `.spec-loop.toml` runs `make check`, defaults review context to `git diff`, and archives approved specs under `specs/implemented`. It has no lifecycle actions. Add an active spec matching configured `specs.active_globs` and `specs.id_regex`, then create and approve its plan:

```sh
SPEC_ID=123 # Replace with ID of active spec.
./target/release/spec-loop --spec "$SPEC_ID" --plan
./target/release/spec-loop --spec "$SPEC_ID"
./target/release/spec-loop --spec "$SPEC_ID" --review
```

Plans are stored under `.spec-loop/`; each clean checkout needs its own approved plan. Run from project root.

## Logs and distribution

Run logs are retained outside project tree under a system temporary directory. Prompt text is passed directly to Pi; Spec Loop writes no prompt files. Logs include child output and agent response data and should be handled accordingly.

The crate targets Rust 2024 and pins nightly toolchain for formatting. MIT license. GitHub release workflow builds Linux x86_64 musl binaries; release workflow has not been run as part of local checks.
