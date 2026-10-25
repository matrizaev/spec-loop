# Spec Loop

Spec Loop is a Rust CLI for implementing numbered repository specifications through guarded agent runs, configured checks, independent review, finding audit, repair, and optional completion actions. Repository-specific spec conventions, prompts, commands, archive policy, and acceptance gates live in `.spec-loop.toml` and referenced files.

**Status:** CLI and core workflow implemented. Pi's effective configuration and inherited environment are preserved for every role. GitHub release workflow is configured; no binaries are published yet. `specs/implemented/001-spec-loop.md` is authoritative; `.spec-loop/plans/001.md` is proposed guidance and gitignored.

## Requirements

- Git repository with committed root `.spec-loop.toml` and tracked active specifications.
- Pi CLI available as configured by `agent.executable` (default `pi`).
- Each configured check executable available in its configured working directory.

Install/build using Rust toolchain pinned in `rust-toolchain.toml`:

```sh
cargo build --release
cargo run -- --help
make check
```

For an initial profile, see [`examples/profiles/basic.toml`](examples/profiles/basic.toml) and [`examples/profiles/planned-archive.toml`](examples/profiles/planned-archive.toml). Examples are policy templates: adjust paths, prompts, checks, and lifecycle settings for target repository before use.

## Self-hosting

Root `.spec-loop.toml` selects numbered specs from `specs/`; its single check runs `make check` for formatting, compilation, Clippy, tests, and docs. The checked-in `specs/implemented/001-spec-loop.md` is archived by this profile. Before using these commands, add an active spec at a path matching `.spec-loop.toml`'s `specs.active_globs` and `specs.id_regex`. Spec Loop also requires a valid `HEAD`, so create an initial Git commit before first run. Set `SPEC_ID` to the new active spec's ID:

```sh
cargo build --release
SPEC_ID=123 # Replace 123 with the ID of your active spec.
./target/release/spec-loop --spec "$SPEC_ID" --plan
./target/release/spec-loop --spec "$SPEC_ID"
```

The configured plan lives under `.spec-loop/`, which Git ignores; each clean checkout needs its own approved plan. `--review` is only for resuming a non-empty Git-visible patch when approved plan already exists locally:

```sh
./target/release/spec-loop --spec "$SPEC_ID" --review
```

Do not use `--review` as initial bootstrap: it skips writer and rejects missing plans or empty patches. Self-hosting profile sets retry policy only; `--commit` and `--continue` remain explicit CLI authorizations.

## Workflow guarantees

1. Resolve Git root, load and validate `.spec-loop.toml` and startup state, then select eligible specification.
2. Plan creation/review is separate from implementation. Author, review, ground reported findings against spec and repository rules, fix supported findings, and repeat until no actionable findings remain.
3. Once spec and required plan exist, run implementation, checks, review, grounding, and supported repairs without per-turn user input. Failed checks return directly to implementation; unsupported or unproven review suggestions do not block progress.
4. Review complete Git-visible patch before staging. Stage complete non-ignored patch only after review has no actionable findings. A dedicated status auditor normalizes noncanonical review statuses; malformed role responses retry with format feedback. Retries and timeouts are unlimited unless configured otherwise.
5. `--commit` authorizes standard Git commit; `--continue` authorizes moving to next eligible spec after commit. Profiles provide no permission gates.

All Pi roles preserve Pi-configured tools, MCP, extensions, skills, project trust, and inherited environment. Review/grounding prompts request no mutations; checks and review cover complete Git-visible changes. Ignored files do not block progress. Extensions and MCPs are not sandboxed and may cause external effects. Spec Loop never resets, cleans, or stashes user state.

## Usage

```text
spec-loop [--spec ID] [--review] [--plan] [--dry-run]
           [--commit] [--continue]
```

- `--spec ID` selects an eligible spec according to profile ID syntax and ordering. It is required with `--review` and `--plan`.
- `--review` resumes at checks without invoking implementation first. Requires non-empty combined Git-visible changes; staged, unstaged, and untracked changes are accepted. Failed checks enter implementation repair.
- Normal `--spec ID` runs implementation even when changes are pre-staged; it does not infer readiness for review.
- Every implementation requires existing plan at configured `[plan].path_template` (default `.spec-loop/plans/{spec_id}.md`). `--plan` runs plan author/review/grounding. If plan roles change other Git-visible files, Spec Loop runs configured checks and implementation review for those edits; failed checks or actionable findings stop with changes preserved and without implementation, staging, or commit. `--continue` starts at requested spec, skips existing plans, then advances through later eligible specs missing plans.
- `--dry-run` prints resolved spec, paths, checks, and completion policy without invoking agents or changing Git.
- During execution, Spec Loop prints each agent/check stage and child command argv before launch, plus stage/command details for staging and commits. Pi prompts are redacted from displayed argv; full outputs remain in retained logs and final role responses print only after calls finish.
- Retry limit is profile-only: `completion.max_passes = 0` means unlimited and applies to every Pi role, including reviewers and auditors. Agent and check timeouts are optional profile settings; omitted means unlimited.
- `--commit` authorizes standard Git commit after review approval. Without it, approved changes remain staged and uncommitted.
- `--continue` requires `--commit` in implementation mode and advances after commit. In `--plan` mode, it plans successive specs without committing.

When archiving is disabled, completed specs remain active. Spec Loop skips them for the rest of current invocation, but a later invocation can select them again; user owns archiving completed specs to prevent reimplementation.

Existing staged, unstaged, and untracked changes are preserved and included in checks/review; startup does not require a clean worktree. `--review` requires a non-empty combined patch. Before approval, Git-visible changes during checks/review trigger updated checks and review. After approval, staging and commit use standard Git behavior without rechecking worktree/index. Repository-relative paths resolve from repository root; traversal, symlink-containment, and collision guards are out of scope. The selected active spec is read-only; only the user may change its contents. Configured archival may move it unchanged after approved completion. Only explicit `--plan` runs may create or revise plans; implementation and review treat the selected plan as read-only.

## Profile and checks

Profiles use strict TOML with `schema_version = 1`; unknown fields fail validation. Check commands are argv arrays, never shell strings. Supported placeholders are `{spec_id}`, `{spec_path}`, and `{plan_path}`. Archive configuration names destination directory; Spec Loop preserves selected spec's filename. The `specs.selection` policy supports `next` (requested active spec must be next) and `any` (any eligible active spec). After review has no actionable findings, engine stages complete non-ignored patch. Configure per-stage model/thinking overrides with `[agent.stages.planning]`, `[agent.stages.implementation]`, and `[agent.stages.review]`; each falls back to global agent setting. See example profiles for supported settings.

Pi configuration is sole authority for available tools, extensions, MCP servers, skills, project trust, and context files. Spec Loop has no profile fields or invocation flags to override those resources; every role runs from repository root with inherited process environment. Repo-aware roles use configured Pi tools to read repository files and inspect the complete diff; prompts identify paths instead of embedding source or generating diff payloads. Optional profile model/thinking settings are passed as per-stage Pi overrides; thinking levels must be one of `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`. Omitted values preserve Pi defaults. Review/grounding prompts request no mutations, but configured resources are not sandboxed; Git-visible checks verify approved patch only where needed.

Configure user-owned acceptance requirements in `acceptance.manual_gates` only when an explicit manual pause is intended. Spec Loop reports pending gates honestly and never claims they passed.

## Logs

Run logs are retained outside the repository under a system temporary directory. Prompt text is passed directly to Pi; Spec Loop writes no prompt files. Logs may contain command output and model responses. Agent output and check failures are logged for recovery and diagnosis.

## Build and distribution

The crate targets Rust 2024 and pins a nightly toolchain for formatting. No Rust MSRV is declared. MIT license. Tag pushes matching `v*` build and test Linux x86_64 musl binary, then publish binary and SHA-256 checksum through GitHub Releases. Workflow has not yet run against a configured upstream repository.
