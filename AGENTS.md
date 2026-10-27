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
- Enforce architecture limits for every first-party Rust file, including `src/`, `tests/`, `examples/`, `benches/`, build scripts, and other repository locations: production files have at most 1,598 physical lines; files named `tests.rs` and Rust files under `tests/` have at most 2,098. Count blank lines, comments, and an unterminated final line. Exclude only root `.git/` metadata, root `target/` build output, and exact generated/vendored files listed with provenance in `tests/architecture-exclusions.txt`; never infer exclusions from directory names or generated markers.
- Group imports in standard-library (`std`, `core`, `alloc`), external-crate, and crate-local groups, in that order. Keep each category in a separate `use` tree/group; use explicit imports and prohibit glob imports in production and test code. Put imports first, out-of-line child-module declarations next, and remaining module-level items after them. A `macro_rules!` declaration may precede a child declaration only when that child or its descendant invokes it. Use `self::` for sibling-module imports.
- Give every module its own filesystem-backed source file. A module with child modules, including an out-of-line test module, uses `module/mod.rs` as its root and stores children in that directory; a childless leaf may use `module.rs`. Do not combine both root forms for one module. Unit tests are out-of-line child modules in `module/tests.rs`. Inline module bodies are forbidden everywhere, and `#[path]` relocation is forbidden.
- Use checked conversions and arithmetic for values influenced by files, configuration, processes, or users. Do not use lossy casts, unchecked indexing, or assumptions about external data.
- Keep functions focused, types explicit, and mutually exclusive lifecycle/workflow choices represented with enums or state-specific types rather than independent booleans. Keep booleans for genuinely independent options and facts. Convert external representations into validated semantic values at boundaries; add newtypes and encapsulation only for concrete invariants or prevention of meaningful type confusion, not for appearance.
- Document every public item and every non-obvious invariant, security boundary, state transition, and error contract. Write comments that explain rationale and constraints, not syntax.
- Add no dependency without a demonstrated requirement and a review of maintenance status, feature surface, MSRV, license, transitive dependencies, and supply-chain impact. Disable unused default features. Commit `Cargo.lock` and review every lockfile change.
- Keep declared `rust-version`, CI toolchains, and dependency MSRVs exactly consistent. Reject dependencies that exceed the declared MSRV. Never infer finalized versions or MSRV from proposed plans.

## Safety and workflow invariants

These rules apply to every implementation and refactor:

- Preserve user data. Never run `git reset`, `git clean`, checkout, stash, or other destructive recovery commands. Never silently overwrite or discard user changes. Spec Loop has no built-in Git state, staging, or commit integration; configured lifecycle commands are explicit user authorization and may have irreversible effects.
- Never invoke configured commands through a shell. Pass validated argument vectors directly to child processes; keep untrusted spec/config text out of shell syntax.
- Validate configuration and options before mutation or agent invocation. Reject unknown configuration fields and invalid, ambiguous, or unsafe inputs where required by the active specification rather than guessing. This generic rule does not expand scope explicitly excluded by the active specification. For Spec Loop, parent-traversal rejection, symlink-containment checks, and archive-destination collision guards are explicitly out of scope; do not implement them unless the user deliberately amends the active specification.
- Keep Git-specific policy out of engine code. Agents must not commit, stage, alter history, or change `HEAD`. Configured checks and lifecycle actions may invoke Git as ordinary argv-based commands. Preserve unexpected changes; do not snapshot, poll, lock, or detect concurrent mutations. Only selected spec and plan hashes trigger integrity stops.
- Preserve the user's effective settings and inherited environment for every role. Pi roles use Pi configuration for tools, extensions, MCP, skills, project trust, and context files; do not add Spec Loop overrides. Built-in auth commands use documented OpenAI ChatGPT OAuth; built-in role execution remains incomplete and must not be claimed to work. Built-in roles use only the built-in shell tool, with no Pi extensions/MCP/skills/trust/context integration. Run every role from current project root. Implementation review, review clarification, and finding audit receive configured review-context argv and delegate execution to selected backend tools; engine never executes or captures output. Built-in shell commands must run automatically without per-command approval and remain unsandboxed. Commands run with same-user permissions and inherited environment; they may read or transmit accessible credentials and secrets, and may change files or systems outside the worktree. OAuth storage must not be described as isolated from same-user shell access. Pi resources and external side effects also cannot be sandboxed or reversed.
- Every machine-consumed role response is strict typed JSON matching its generated role schema. A malformed or semantically inconsistent response retries the same role; valid `blocked` stops immediately. Never infer workflow state from prose, markers, or headings. Typed completion is not proof of correctness. Built-in tool transcripts may contain sensitive workspace data, credentials, or inherited secrets; logs cannot guarantee secret removal.
- Run every configured check even after a failure. If any check fails, return concise diagnostics to implementation and skip review for that pass. Review only after all configured checks succeed. Configured success/finish actions run only at their specified workflow points; action failure stops without retry or rollback.
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

During iteration, run only targeted tests, using quiet output such as `cargo test -q <test-filter>` (add package/target selectors as needed). Do not run the full test suite until the final required `make check`. Quiet flags reduce successful-test output; they must not hide failures or replace inspecting failure diagnostics. Before handoff for code or configuration changes, run `make check` from the repository root after the final change; this gate is mandatory. For documentation-only changes, skip `make check` and run relevant lightweight validation such as `git diff --check`. If an external blocker prevents a passing required check, report the task as incomplete and name the exact blocker and failing gate. Once the Cargo project and scripts exist, expected Rust checks are:

```text
cargo +nightly-<pinned-date> fmt --all -- --check
cargo check --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings -D clippy::pedantic -D clippy::nursery
cargo test -q --locked --all-targets --all-features
cargo test -q --locked --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
```

Also run relevant integration tests when changing process, packaging, or release behavior. Release-target builds and smoke tests are CI gates, not local validation requirements. For this project:

- Use temporary non-Git project directories, fake agent/backend processes, and fake command executables. Tests must not require a network, live model, credentials, Git installation, or developer worktree state.
- Test process supervision, malformed/partial output, non-zero exits, check ordering, action ordering, and child termination where relevant.
- Test duplicate/ambiguous specs, hash protection for selected inputs, unrelated-file tolerance, reviewer repairs, archive paths, and check/review/action gates.
- Assert failures preserve workspace changes and prior action effects, skip later actions/specs as required, and never trigger rollback.
- Keep tests deterministic, isolated, and explicit about platform-specific assumptions. Do not make network-dependent tests part of default CI.
- Run the full configured check list in order; capture each result and continue after failures as required by the workflow.

The selected active specification's acceptance criteria define required coverage. Do not mark unimplemented or untested criteria complete.

## CLI, process, and configuration quality

- Keep CLI errors actionable and non-zero on invalid options, configuration, prerequisites, or failed gates. `--help` should remain usable without a repository profile if required by the spec.
- Use structured types for profile data, strict deserialization, and validation that reports the offending field/path. Avoid loosely typed maps and scattered stringly typed policy.
- Build child-process arguments with `std::process::Command` or an equivalent argv-safe API. Capture stdout and stderr without deadlock; preserve useful diagnostics and exit status.
- Separate raw backend output from typed final responses. Validate Pi event sequence, settlement, completion state, exact JSON shape, role/version, and input-dependent constraints per attempt; malformed attempts retry and never count as complete.
- Log enough context to recover and diagnose failures while protecting credentials. Keep retained logs outside the worktree with restrictive permissions where supported.
- Make dry-run observational: resolve and report intended work without invoking agents, checks, lifecycle actions, or mutating workspace state.

## Completion report

When finishing a task, state what changed, list files, and report tests/checks actually run. Mention failures, skipped gates, unresolved ambiguity, or required user action. Keep claims precise; do not imply approval, release readiness, or successful behavior beyond evidence.
