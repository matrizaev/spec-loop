# Spec Loop

Spec Loop is a repository-agnostic Rust CLI for implementing filesystem-selected specifications through agent runs, configured checks, independent review, finding audits, supported repairs, and optional lifecycle commands. Repository-specific spec conventions, prompts, review context, checks, archive policy, and actions live in `.spec-loop.toml` and referenced files.

## Requirements

- Run Spec Loop from project root; `.spec-loop.toml` must be directly in current working directory.
- Schema-v3 profiles default to built-in OpenAI and require explicit `agent.model` plus ChatGPT authorization. Built-in uses Responses API Structured Outputs with a strict role-specific schema projected from the canonical role protocol; all responses still pass the same local strict schema and semantic validation as Pi. Refusal and incomplete provider responses are not valid handoffs. Live compatibility of the built-in OAuth path, account entitlement, selected model, and Structured Outputs combination has not been established; a provider rejection fails without weaker-format or authentication fallback.
- Each configured check/action executable available when used.
- Git is not required by Spec Loop. Default review context is `git diff`; projects without Git should configure a different command.

Build with pinned Rust toolchain:

```sh
cargo build --release
cargo run -- --help
make check
```

See [`examples/profiles/basic.toml`](examples/profiles/basic.toml), [`examples/profiles/planned-archive.toml`](examples/profiles/planned-archive.toml), and [`examples/profiles/status-update.toml`](examples/profiles/status-update.toml). These profiles demonstrate ordinary user-configured actions; Spec Loop itself has no Git integration for status, diffs, staging, or commits.

## Get Started

Adopt Spec Loop in an existing repository with these steps:

1. Install the `spec-loop` executable and make sure it is on `PATH`. From a checkout of this repository, you can install it with `cargo install --path /path/to/spec-loop`. Confirm it is available with `spec-loop --help`.
2. From the root of the repository where you want to use Spec Loop, copy [`examples/profiles/starter.toml`](examples/profiles/starter.toml) to `.spec-loop.toml`. If you already have a profile, merge the settings rather than replacing it. Spec Loop reads this file only from the current working directory.
3. Configure the profile's agent backend, executable, and model for your environment. The starter uses Pi. Set its `model` to one available in your Pi configuration. Add the repository's actual test, lint, or build commands as `[[checks]]` entries; the starter intentionally defines no checks, so none will run until you add them. The default review context is `git diff`; replace it if the repository does not use Git.
4. Create `specs/` and add a focused specification such as `specs/001-add-feature.md`. The starter profile selects `specs/*.md` files whose names begin with a numeric ID. State the goal, requirements, and observable acceptance criteria. Add or configure repository instruction files under `[prompts]` if the agents need local conventions.
5. From the repository root, author and review the plan, then run implementation and review:

	```sh
	spec-loop --spec 001 --plan
	spec-loop --spec 001
	```

	Replace `001` with your spec ID. The normal run uses the approved plan at `.spec-loop/plans/{spec_id}.md`.

The starter profile does not archive specs or stage, commit, or otherwise alter Git state. Configure lifecycle actions only after reviewing their exact effects. Agent tools and configured commands run with your user's permissions; they are not sandboxed. The built-in backend is available as an alternative, but its workflow execution is currently incomplete; see [Profile schema v3](#profile-schema-v3) before choosing it.

## Workflow

1. Current working directory is project root. Spec Loop requires `.spec-loop.toml` there and never searches parent directories.
2. Active specs are discovered from filesystem paths matching `specs.active_globs`, regardless of Git tracking or ignore rules. Duplicate IDs are rejected. Profile rules control eligibility, ID parsing, ordering, and requested-spec selection.
3. Plan authoring is separate from implementation. Only `--plan` author role may create or revise selected plan. Plan approval is an implementation handoff: author researches relevant code, tests, references, and APIs; records evidence and applicable baselines or benchmarks; and gives concrete implementation and test steps. Required research is the PlanAuthor's responsibility, never a blocker by itself; plan review rejects unresolved research or decisions that can be completed during planning. If a required external prerequisite remains unavailable after investigation, planning returns `blocked` instead of approving a plan that leaves implementation blocked. Implementation and review roles treat selected plan as read-only; implementation uses its research, verifies consequential claims, and does not treat plan wording alone as authority over the spec.
4. Normal implementation runs implementation, every configured check in order, review, finding audit, optional evidence clarification, supported repairs, and repair loops. Any failed check skips review for that pass and returns diagnostics to implementation. Each implementation response includes a concise summary of work completed so far; Spec Loop prints it as workflow enters automated checks. Each `ready_for_review` response starts another check/review pass; it means current work is reviewable, not that the full spec is complete. Repair passes resume from the current workspace and preserve prior work. Remaining in-scope work alone is not a blocker.
5. `--review` skips initial implementation and runs checks/review on current workspace, including workspaces with no Git metadata or no changes. Supported actionable findings then enter the same implementation/check/review repair loop.
6. Every valid typed non-auditor `blocked` response goes to its corresponding auditor for independent blocker adjudication. Confirmed or ambiguous blockers stop; unsupported blockers return to the same originating role for a corrected typed response, never automatic completion or approval. Auditor `blocked` responses stop immediately without recursion. Malformed or inconsistent typed JSON retries the same role; feedback identifies exact missing/unknown fields, preserves the prior response, and requests a corrected handoff without repeating workspace work solely for JSON formatting. Exhausted retries fail closed. Each attempt receives a generated role-specific JSON Schema 2020-12.
7. After successful checks and review with no actionable findings, optional archival moves selected spec unchanged. Ordered `actions.on_review_success` run after archive; `actions.on_finish` run after those actions. Action commands are explicit repository-owner authorization and can have irreversible effects.

### Agent blockers

A completed plan or implementation review with no actionable gaps and no genuine external blocker returns `no_issues` with empty findings. A genuine external prerequisite can still require `blocked` even when there are no findings; an empty finding set alone is not a blocker.

Blockers from `plan_author`, `plan_review`, and `plan_review_clarification` route to `plan_audit`; blockers from `implementation`, `review`, and `review_clarification` route to `review_audit`. The auditor receives the original typed response and exact reason, without synthetic findings. It assesses only that claim and returns a strict typed outcome:

- `confirmed`: stop with the unmet requirement, unavailable prerequisite, why tools cannot resolve it, and required user input or approval.
- `ambiguous`: stop with the precise unresolved question for the user.
- `unsupported`: send audit feedback to the same originating role, retaining its original task, repair findings, check diagnostics, or clarification request. Tools remain available for unfinished required work. The role must return its own corrected response; any renewed blocker is audited again. This is not automatic completion or approval.

Correcting a blocker preserves completed effects and does not rerun unrelated completed roles, checks, or actions. Malformed-response correction remains separate, including built-in correction-only retries without tools. Existing role-attempt limits, timeouts, interruption, integrity checks, and completion gates still apply. Wrong-kind or malformed auditor responses retry that auditor. Auditor blockers stop immediately; auditors are not recursively audited. Both backends share this workflow, with implementation auditors retaining delegated review context and plan auditors receiving none.

### Finding audits

Auditors act as skeptical gatekeepers, not second reviewers: they assess supplied findings and do not search for additional improvements. Typed audit results separate factual validity (`supported`, `unsupported`, or `ambiguous`) from actionability (`required`, `not_required`, or `ambiguous`). A true observation can be supported but require no change; only findings marked both supported and required enter repair. An excessive proposed fix does not invalidate a real, actionable defect: auditors identify the smallest sufficient correct fix, and repairs follow that rationale. Reject speculative hardening, hypothetical extensibility, duplicated validation, and architectural expansion without demonstrated need. Do not dismiss a proven in-scope defect because its smallest correct fix is difficult or substantial; simplicity limits unnecessary complexity, not necessary work.

### Writing specifications

Keep each spec small, coherent, and bounded around one independently reviewable outcome with observable acceptance criteria. Split broad “implement everything” requests and unrelated work into focused specs; state dependencies and intended order explicitly. Narrow specs make planning, implementation, review, and repair easier to verify.

Pi owns its effective tools, extensions, MCP servers, skills, project trust, context, and inherited environment. Spec Loop does not sandbox Pi resources. Review/grounding roles are asked not to mutate files. Only selected spec and plan contents are hash-protected; changes to other files do not trigger snapshots, rollback, or re-review. Run logs are retained outside project directory.

## Usage

```text
spec-loop [--spec ID] [--review] [--plan] [--dry-run] [--continue] [--verbose] [--color <auto|always|never>]
```

- `--spec ID` selects an eligible active specification. Required with `--review` and `--plan`.
- `--plan` authors/reviews a plan only. `--continue` can process later specs and skips plans already present.
- `--review` resumes at checks and review; it does not require a Git-visible patch.
- Normal mode requires existing plan at `[plan].path_template` (default `.spec-loop/plans/{spec_id}.md`).
- `--continue` advances to later eligible specs after successful review and lifecycle actions. No commit authorization is required.
- `--dry-run` prints resolved spec, plan, checks, review-context argv, and expanded actions without invoking agents or commands.
- `--verbose` prints every field of each successfully validated typed role result; it does not print raw JSON or provider transcripts.
- `--color auto|always|never` controls workflow styling and does not affect authentication commands. The default `auto` mode styles stdout and stderr independently only when that stream is a terminal, `TERM` is not `dumb`, and `NO_COLOR` is absent or empty. `always` forces Spec Loop styling; `never` disables it. Explicit status labels remain in all modes.
- `--commit` is not supported. Configure stage/commit behavior explicitly under `[actions]` if desired.
- `completion.max_passes = 0` disables the engine's generic role-attempt cap. This does not promise unlimited provider access or usage, runtime, output, or external resources. Built-in provider retries are separately capped at three retries per request; agent, check, and action timeouts are optional unless configured.

Completed specs remain active if archival is disabled. A later invocation can select them again; repository owners control eligibility through active globs and archive transitions.

### Terminal output

Workflow role results use readable, deterministic summaries instead of compact response JSON. The concise view identifies each role and status: implementation shows its summary (and a blocked reason when blocked); plan author shows readiness or its blocked reason; reviews show finding count and each finding's ID, location, requirement, and violation; audits show finding ID, classification, and actionability; clarifications show finding IDs, citation counts, and citation paths/locations. Blocker adjudications show the outcome, rationale, and stop or same-role correction details; verbose output also includes evidence. Empty review and citation collections say `No findings` or `No citations`. Free-text previews are limited to 160 Unicode scalar values and append `… [truncated; use --verbose]` when shortened.

Use `--verbose` for all substantive fields from validated typed results, including review evidence and proposed corrections, audit rationale, and clarification excerpts and relevance. This is still structured human-readable output, not raw JSON or provider transcripts. Invalid responses are not presented as results; diagnostics identify the validation retry and exact retained final-response log path.

Built-in role runs print a compact, UTF-8 icon-labeled summary: role/model/time and execution counts, latest response/context snapshot, then cumulative **observed completed-response usage**. On terminals, each role's report refreshes in place. Redirected output retains snapshots but prints role/model heading only once per role. Token counts use thousands separators. Cached-input and reasoning-output values are optional breakdowns of their parent totals, not additional tokens. Missing, null, malformed, inconsistent, or unavailable provider counts display as `unknown`, not zero; failed requests are not assumed to have consumed no resources. These observations are not billing totals and do not estimate cost, quota, remaining access, or failed-request usage. Pi runs do not display built-in provider telemetry.

The optional built-in-only `agent.context_window_tokens` and `[agent.stages.<stage>].context_window_tokens` values are positive **user-provided limits**, not model facts. A stage limit overrides the global limit. If a stage overrides the global model, its context limit is unknown unless that stage also supplies its own limit; the global limit applies only when the stage inherits the global model. No limit is inferred from a model name, fetched from a catalog, or looked up over the network. Counts remain visible without a limit. A displayed percentage describes only input plus output tokens from the last completed response against the configured limit; it is not a prediction of next-request capacity. A later request, output reservation, a distinct model input limit, and provider-side accounting can change available capacity. Pi rejects these built-in-only settings.

Workflow headings and outcomes use explicit labels such as `[RUNNING]`, `[PASS]`, `[BLOCKED]`, `[RETRY]`, `[WARN]`, `[FAIL]`, and `[ERROR]`. Styling uses a consistent semantic palette: cyan stages, green pass, yellow blocked/retry/warning, red failure/error, and dim secondary metadata. `--color auto` (the default) detects stdout and stderr separately and styles a stream only if it is a terminal, `TERM` is not `dumb`, and `NO_COLOR` is unset or empty. `--color always` forces styling; `--color never` disables it. Redirected output in auto mode is plain text, and labels always communicate status without color. These workflow options do not change authentication command output.

Displayed agent-supplied text and child-output excerpts have terminal-control sequences stripped; line feeds and tabs remain readable and other control characters are visibly escaped. Child output captured and retained in logs, provider transcripts, and raw final-response files are not rewritten by terminal formatting.

## Profile schema v3

Profiles use strict TOML with `schema_version = 3`. Omitted `agent.backend` selects `builtin`; set `backend = "pi"` explicitly to retain Pi. Built-in profiles require non-empty global `agent.model`; stage-specific model and thinking settings override global values. Optional positive `agent.context_window_tokens` and per-stage `context_window_tokens` configure user-provided built-in context limits; stage values take precedence, and a global limit is inherited only when that stage inherits the global model. A stage model override without a stage limit has an unknown context limit. Pi rejects both context-window settings. Omitting `agent.max_tool_calls` imposes no Spec Loop shell-call count ceiling. An explicit positive integer sets a per-role shell-call limit for the built-in backend only; it resets for each role and prevents invocation of the first over-limit command without replaying completed commands or rolling back their effects. Zero is rejected, and Pi rejects the field. `agent.max_tool_output_bytes` defaults to a shared combined stdout/stderr payload limit of 1,048,576 bytes per built-in shell call; there is no per-stream reservation, so either stream may use the other’s unused capacity. The field is valid only for the built-in backend. No Spec Loop call-count ceiling does not mean unlimited provider/subscription access or usage, execution time, output bytes, or external resources. Unknown fields fail validation. See the security section for external process-isolation options.

Version 1 and 2 profiles require manual migration to `schema_version = 3`; no automatic migration is performed. Add an `[agent]` table with `backend = "pi"` to preserve prior backend behavior, or configure built-in model and authentication. Checks and lifecycle actions use argv arrays, never shell strings, and all configured checks/actions run from current working directory. No command has a configurable `cwd`.

**Implementation status:** schema-v3 selection, profile-independent auth commands, and an initial built-in Responses/shell backend are present. The implementation remains incomplete against the full active-spec acceptance criteria; do not treat built-in workflow execution as complete. Authentication commands do not load or modify project profiles.

### Built-in authentication and security boundary

Built-in authentication uses the official OpenAI Sign in with ChatGPT flow for eligible ChatGPT-plan use and is intended for the Responses API. It does not accept API keys, use `OPENAI_API_KEY`, or fall back to API-key billing or another provider. Login requires `chatgpt.tokens.use.direct`; missing permission stops authorization. Subscription access and usage limits are provider-controlled; ChatGPT authorization does not promise unlimited usage.

Run `spec-loop auth login`, then configure a profile with non-empty global `agent.model` (stage-specific model settings override it). Login attempts to open authorization URL and always prints it for manual use. Flow requires reachable localhost callback; remote/SSH users must arrange loopback reachability, for example port forwarding. No device-code fallback exists. With multiple saved registrations, use `auth login --account REGISTRATION_ID` to reauthorize one, or `--new-account` to create distinct registration. `auth status` does not display tokens. `auth logout [--account REGISTRATION_ID]` removes only Spec Loop's local credentials; it does not revoke upstream grant.

Before each new built-in Responses request, Spec Loop checks stored access-token expiry and refreshes it when five minutes or less remain, saving any rotated refresh token. So an overnight pause alone should not require login. A rejected or revoked refresh token, or lost direct-use permission, still requires `spec-loop auth login`. Refresh happens between model requests, not per streamed event; existing 401/403 responses remain fatal without replay.

OAuth credentials must stay outside the active project worktree and run logs, in an operating-system credential store where available or an owner-only platform credential file otherwise. Set `SPEC_LOOP_AUTH_DIR` to choose the directory that directly contains `auth.json` (no additional `spec-loop` or `Spec Loop` component is appended). It applies on every supported platform and takes precedence over platform defaults and `XDG_CONFIG_HOME`; the value must be a non-empty absolute path outside the active project. Empty and relative values are errors, and a selected path failure never falls back to another location. Changing the setting does not migrate, copy, or delete registry or credential-store data.

When `SPEC_LOOP_AUTH_DIR` is unset, Linux and other non-macOS/non-Windows platforms use `$XDG_CONFIG_HOME/spec-loop/auth.json`, or `$HOME/.config/spec-loop/auth.json` if `XDG_CONFIG_HOME` is unset; macOS uses `~/Library/Application Support/Spec Loop/auth.json`; Windows uses `%APPDATA%\Spec Loop\auth.json`. File and directory permissions are restricted to the owner where supported. If the operating-system credential store is unavailable, `auth.json` may contain plaintext credentials. A custom location does not encrypt credentials or isolate them from same-user processes, and is not a credential sandbox. Authentication commands do not load repository profiles.

Built-in shell commands execute automatically without per-command approval via POSIX `/bin/sh -c`, from project root, with the inherited environment and same-user operating-system permissions. This is not a sandbox: a requested command can access or transmit credentials and secrets available to that user and can affect files or systems outside the worktree. Spec Loop cannot guarantee that command effects are isolated or reversible. Users who need isolation may run the entire process under a dedicated operating-system account, in a container or virtual machine, or inside a platform sandbox. Isolation effectiveness depends on the user's external configuration; Spec Loop does not provide or configure these controls. When `agent.max_tool_calls` is omitted, there is no Spec Loop shell-call count ceiling; an explicit positive integer applies per role for the built-in backend only (see Profile schema v3). `agent.max_tool_output_bytes` defaults to a shared combined stdout/stderr payload cap of 1,048,576 bytes per built-in shell call, with no per-stream reservation; either stream may use the other’s unused capacity. Configured role timeout and cancellation apply to provider requests and shell execution. Provider and operating-system limits still apply. Unsupported platforms must fail rather than silently change shell semantics.

Shell commands and bounded outputs are retained in external run logs for diagnosis. Tool output can contain workspace data, credentials, or inherited secrets; transcript logging cannot reliably remove secrets. Commands and external side effects may alter files or systems outside the worktree. The implementation remains incomplete against full acceptance criteria; these descriptions do not assert completion or approval.

Review context defaults to:

```toml
[review.context]
argv = ["git", "diff"]
```

Spec Loop never executes or captures this command. It supplies exact argv and project-root working directory to implementation review, implementation finding audit, and implementation evidence clarification prompts; those roles delegate it to the selected backend's available tools and return `blocked` if it fails. The engine never executes or captures the command. Plan roles do not receive it.

Lifecycle actions are ordered and optional:

```toml
[[actions.on_review_success]]
name = "stage reviewed changes"
argv = ["git", "add", "--all"]

[[actions.on_finish]]
name = "commit reviewed changes"
argv = ["git", "commit", "-m", "Implement {spec_id}"]
```

Supported action placeholders: `{spec_id}`, `{spec_path}`, `{plan_path}`. They expand within argv elements, not through a shell. `{spec_path}` names archived destination after a move, otherwise original spec path. Actions inherit environment, capture stdout/stderr in run logs, and stop workflow on nonzero status without retry or rollback. Before the first `on_finish` action, Spec Loop verifies selected spec and plan hashes; after finish actions begin, it no longer checks selected-spec hash, allowing an explicitly configured action to update that spec. Selected-plan hash remains checked before and after every action, and `on_review_success` actions remain protected for both inputs. The status-update profile demonstrates GNU `sed -i` changing `**Status:** Active` to `**Status:** Complete`; these labels are example convention only, not engine policy. Omitted action lists are empty. The self-host profile updates the archived spec's status using a finish action; it does not stage or commit changes.

Configure checks as ordered `[[checks]]` entries with `name`, non-empty `argv`, and optional `timeout_seconds`. Every check runs from project root. All checks run after a failure; any failure blocks review for that pass. The Pi executable is validated only when Pi is selected; prompts and checks are validated before agent invocation. All command argv share structural validation (non-empty executable; no NUL bytes). Checks substitute exact `{spec_id}`, `{spec_path}`, and `{plan_path}` tokens; other brace text remains literal. Review-context argv is passed unchanged. Lifecycle actions use strict placeholder validation.

Pi resource availability is controlled only by user's Pi configuration; Spec Loop does not add resource overrides. The built-in backend uses only OpenAI ChatGPT OAuth and its shell tool, with no Pi extensions, MCP, skills, trust, or context integration. For built-in role responses, Spec Loop requests provider-native Responses API Structured Outputs (`text.format` with strict `json_schema`) from the shared role contract, then retains local exact-shape, role/version/status, and input-dependent validation. Provider documentation does not establish compatibility for this OAuth path, entitlement, or every selected model; there is no fallback to weaker formatting, another provider, or API-key authentication. Optional per-stage model/thinking settings use `[agent.stages.planning]`, `[agent.stages.implementation]`, and `[agent.stages.review]`; omitted values inherit global values. Thinking levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; the intended built-in mapping is `off` to `none` and remaining values directly to reasoning effort.

## Self-hosting

Spec Loop is built with Spec Loop itself: this repository's specifications are planned, implemented, checked, and reviewed through the CLI.

The checked-in `.spec-loop.toml` runs `make check`, defaults review context to `git diff`, and archives approved specs under `specs/implemented`. Its finish action updates the archived spec's status; it does not stage or commit changes. Add an active spec matching configured `specs.active_globs` and `specs.id_regex`, then create and approve its plan:

```sh
SPEC_ID=123 # Replace with ID of active spec.
./target/release/spec-loop --spec "$SPEC_ID" --plan
./target/release/spec-loop --spec "$SPEC_ID"
./target/release/spec-loop --spec "$SPEC_ID" --review
```

Plans are stored under `.spec-loop/`; each clean checkout needs its own approved plan. Run from project root.

## Logs and distribution

Run logs are retained outside project tree under a system temporary directory. Role prompts are passed directly to the selected backend; Spec Loop writes no prompt files. Logs include child output, built-in shell transcripts, and agent response data and should be handled accordingly because tool output may contain accessible secrets.

The crate targets Rust 2024 and pins nightly toolchain for formatting. MIT license. GitHub release workflow builds Linux x86_64 musl binaries; release workflow has not been run as part of local checks.
