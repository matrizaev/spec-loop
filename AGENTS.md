# Agent Instructions

## Purpose and authority

Build a reliable, secure, repository-agnostic Rust CLI for the workflow described by the currently selected active repository specification.

- Resolve the active specification using repository instructions and profile policy. Never hard-code a numbered spec ID or spec path in these instructions or generic engine policy.
- Treat the selected active specification as authoritative. Preserve its safety guarantees and acceptance criteria; do not weaken them to simplify implementation.
- The selected active specification is read-only workflow input. Spec Loop and its agents must never edit, rewrite, or change its contents or requirements; only the user may revise it. A configured archive transition may move the unchanged file after approved completion. If a finding appears to require changing the active spec, surface the conflict to the user instead of editing the spec.
- Approved plans are read-only in implementation and review modes. Only the PlanAuthor in an explicit `--plan` run may create or revise a plan; implementation, review, audit, and engine completion steps must not edit or replace it.
- Treat implementation plans as proposed guidance, not binding requirements. Verify plan claims; plans never override the active specification or repository instructions.
- If requirements conflict or leave a consequential behavior ambiguous, surface the conflict rather than inventing policy. Keep repository-specific behavior in profiles and referenced files, not engine code.
- Read relevant source, tests, configuration, and documentation before changing them. Make the smallest complete change that satisfies the requirement.

## Rust engineering standards

Every rule in this section is mandatory. No exceptions, suppressions, or local opt-outs.

- Write idiomatic, readable Rust with explicit control flow, cohesive modules, and the simplest design that fully satisfies requirements. Reject cleverness, speculative abstraction, and duplicated policy.
- Keep the binary entry point limited to argument handling, top-level orchestration, and error reporting. Put domain logic in testable modules. Expose no public API without a concrete in-repository caller.
- Use ownership and borrowing to encode invariants. Eliminate unnecessary cloning, shared mutable state, global state, and interior mutability.
- Prohibit `unsafe` throughout all first-party Rust code. Apply `#![forbid(unsafe_code)]` to every crate root.
- Prohibit production `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!`, and `unimplemented!`. Return typed errors with operation, path, and causal context. Never discard errors or use fallback values to hide failure. Tests may use assertions and panic-based failure reporting.
- Eliminate every compiler warning and Clippy warning. Deny all warnings, `clippy::pedantic`, and `clippy::nursery` across all targets and features. Do not add lint allowances, warning suppressions, or lint exceptions. Fix the underlying issue.
- Format only with nightly `rustfmt`. Maintain `rustfmt.toml` with `group_imports = "StdExternalCrate"` and `reorder_imports = true`; stable rustfmt silently ignores `group_imports`. Pin exact nightly date in CI and formatting scripts. Commit no formatting deviations.
- Group imports in standard-library, external-crate, and crate-local groups. Use explicit imports; prohibit glob imports in production. Put module imports first, child-module declarations next, and remaining items after them. Use `self::` for sibling-module imports.
- Use checked conversions and arithmetic for values influenced by files, configuration, processes, or users. Do not use lossy casts, unchecked indexing, or assumptions about external data.
- Keep functions focused, types explicit, and state transitions represented with enums rather than boolean combinations or stringly typed values. Validate data at boundaries and maintain validated types after parsing.
- Document every public item and every non-obvious invariant, security boundary, state transition, and error contract. Write comments that explain rationale and constraints, not syntax.
- Add no dependency without a demonstrated requirement and a review of maintenance status, feature surface, MSRV, license, transitive dependencies, and supply-chain impact. Disable unused default features. Commit `Cargo.lock` and review every lockfile change.
- Keep declared `rust-version`, CI toolchains, and dependency MSRVs exactly consistent. Reject dependencies that exceed the declared MSRV. Never infer finalized versions or MSRV from proposed plans.

## Safety and workflow invariants

These rules apply to every implementation and refactor:

- Preserve user data. Never run `git reset`, `git clean`, checkout, stash, or other destructive recovery commands. Never silently overwrite or discard user changes; review the complete combined patch before engine staging, then stage only after no actionable findings remain.
- Never invoke configured commands through a shell. Pass validated argument vectors directly to child processes; keep untrusted spec/config text out of shell syntax.
- Validate configuration and options before mutation or agent invocation. Reject unknown configuration fields and invalid, ambiguous, or unsafe inputs where required by the active specification rather than guessing. This generic rule does not expand scope explicitly excluded by the active specification. For Spec Loop, parent-traversal rejection, symlink-containment checks, and archive-destination collision guards are explicitly out of scope; do not implement them unless the user deliberately amends the active specification.
- Keep Git mutation owned by workflow code. Agents must not commit, stage, alter history, or change `HEAD`. Preserve unexpected changes; route ordinary Git-visible file/index edits through checks and review rather than rollback or ownership-based stops. Assume one workflow operator; do not snapshot, poll, lock, or otherwise detect concurrent user/agent Git mutations. After review approval, use standard Git staging/commit behavior without rechecking worktree/index.
- Preserve the user's effective Pi settings and inherited environment for every role. Pi configuration alone controls tools, extensions, MCP, skills, project trust, and context files; do not add Spec Loop profile fields or invocation flags that alter them. Run every role from repository root. Prompts ask reviewers/grounders not to mutate; Git-visible checks validate workflow inputs before approval, ignore Git-ignored artifacts, and do not sandbox extensions/MCP or reverse external side effects.
- A failed, malformed, incomplete, or unknown-marker response cannot satisfy a completion marker; retry by default. A noncanonical review status uses the dedicated status auditor to normalize the raw response without adding findings. If status remains genuinely ambiguous after reviewer clarification, pause for user decision. A completion marker is not proof of correctness.
- Run every configured check even after a failure. If any check fails, immediately return concise failure diagnostics to implementation and skip review for that pass. Review only after all configured checks succeed; stage only after review has no actionable findings. A failed check blocks completion.
- Do not claim a check, review, build, smoke test, or release passed unless it was actually run and its result observed.

## Implementation and change discipline

1. Identify applicable requirements in the selected active specification, existing behavior, and tests before coding.
2. Trace the full affected flow, including error handling, retries, interruption, and filesystem/Git side effects.
3. Implement behavior at the narrowest responsible layer. Keep workflow policy independent of Pi-specific invocation and parsing.
4. Add or update tests in the same change. Cover success, invalid input, failure paths, safety guards, and regression cases—not only the happy path.
5. Update user-facing help, examples, and documentation when behavior or configuration changes. Keep examples valid against the actual parser.
6. Review the diff for scope, unintended mutations, brittle assumptions, secret leakage, and untested branches before declaring done.

Do not add arbitrary shell hooks, dynamic shell expansion, project-specific defaults, or speculative framework/plugin abstractions unless the active specification is deliberately amended to require them.

## Testing and quality gates

Use focused tests while iterating. Before handoff for code or configuration changes, run `make check` from the repository root after the final change; this gate is mandatory. For documentation-only changes, skip `make check` and run relevant lightweight validation such as `git diff --check`. If an external blocker prevents a passing required check, report the task as incomplete and name the exact blocker and failing gate. Once the Cargo project and scripts exist, expected Rust checks are:

```text
cargo +nightly-<pinned-date> fmt --all -- --check
cargo check --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings -D clippy::pedantic -D clippy::nursery
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
```

Also run relevant integration tests when changing process, packaging, or release behavior. Release-target builds and smoke tests are CI gates, not local validation requirements. For this project:

- Use temporary Git repositories and fake agent/backend processes for automated tests. Tests must not require a network, live model, credentials, or the developer's worktree state.
- Test process supervision, malformed/partial output, non-zero exits, check ordering, and child termination where relevant.
- Test duplicate/ambiguous specs, staged-change preservation, reviewer edits routed through repair, ignored-file tolerance, and check/review gates before staging/commit.
- Assert that failures do not trigger staging/commit or destructive cleanup. Inspect resulting Git state, not only returned errors.
- Keep tests deterministic, isolated, and explicit about platform-specific assumptions. Do not make network-dependent tests part of default CI.
- Run the full configured check list in order; capture each result and continue after failures as required by the workflow.

The selected active specification's acceptance criteria define required coverage. Do not mark unimplemented or untested criteria complete.

## CLI, process, and configuration quality

- Keep CLI errors actionable and non-zero on invalid options, configuration, prerequisites, or failed gates. `--help` should remain usable without a repository profile if required by the spec.
- Use structured types for profile data, strict deserialization, and validation that reports the offending field/path. Avoid loosely typed maps and scattered stringly typed policy.
- Build child-process arguments with `std::process::Command` or an equivalent argv-safe API. Capture stdout and stderr without deadlock; preserve useful diagnostics and exit status.
- Separate raw backend output from parsed final responses. Validate event sequence, settlement, completion state, and exact expected marker per attempt; malformed attempts feed recovery retries and never count as complete.
- Log enough context to recover and diagnose failures while protecting credentials. Keep retained logs outside the worktree with restrictive permissions where supported.
- Make dry-run observational: resolve and report intended work without invoking agents or mutating Git/worktree state.

## Completion report

When finishing a task, state what changed, list files, and report tests/checks actually run. Mention failures, skipped gates, unresolved ambiguity, or required user action. Keep claims precise; do not imply approval, release readiness, or successful behavior beyond evidence.