# Agent Instructions

## Scope

- Read relevant source, tests, configuration, and documentation before changing them. Make the smallest complete change consistent with clean, hexagonal architecture: explicit API and layer boundaries with clear responsibilities, domain-driven design, and strong types that enforce invariants and make invalid states unrepresentable.

## Rust engineering

- Write focused, idiomatic Rust. Prefer unique ownership and borrowing over `Rc` and `Arc`; use shared ownership only when borrowing would be unreasonably complex. For immutable inputs, accept references to underlying data (`&T`, `&[T]`, `&str`, `Option<&T>`), not references to owning wrappers (`&Box<T>`, `&Vec<T>`, `&String`, `&Option<T>`). Avoid needless clones, shared mutable state, and clever abstractions. Keep `main` to argument handling, orchestration, and error reporting; keep domain logic testable. Expose no public API without an in-repository caller.
- Production code must not use `unwrap`, `expect`, panic macros, `unreachable!`, `todo!`, or `unimplemented!`. Define structured error types with `thiserror`; include operation, path, and causal source where applicable. Use `Option` for simple expected absence and `Result` when callers need failure context. Never discard errors or hide failures with fallback values. Do not suppress diagnostics with lint allowances. Tests may use assertions and panic-based failures.
- Keep each `use` tree to a single module. Import type names unqualified unless ambiguous; prefer module-qualified free-function calls and locally scoped trait imports unless file-wide use is clearer.
- Use checked conversions and arithmetic for external values; no lossy casts, unchecked indexing, or assumptions about external input. Model exclusive states with enums/state-specific types, using booleans only for independent facts; use enums instead of unclear boolean parameters. Prefer named fields over tuples except for newtypes and unmistakable local uses. Validate external representations at boundaries; add newtypes only for concrete invariants. Document public items and non-obvious invariants, security boundaries, transitions, and error contracts. Comments explain rationale, not syntax.
- Keep CLI errors actionable and non-zero. Deserialize profiles into strict types and report invalid fields with path/context. Build child processes with argv-safe APIs; capture stdout/stderr without deadlock and preserve diagnostics and exit status. Separate raw backend output from typed results and validate it at the boundary.
- Use standard traits when their semantics fit, derive implementations when possible, and add traits only for real API needs—not code organization. Prefer iterator returns when they avoid allocation or improve composition; reusable functions that only iterate may accept `IntoIterator`. In application code, prefer slices or owned collections when simpler. Avoid complex iterator chains when a loop reads better.
- Mark functions `#[must_use]` when ignoring their result would lose important behavior. Give non-obvious test assertions useful failure context. Follow Rust naming conventions for conversions (`as_`, `to_`, `into_`) and fallible constructors (`try_new`). Do not use unstable Rust features or `panic::catch_unwind`.
- Add dependencies only for demonstrated needs, after reviewing maintenance, features, MSRV, license, transitive dependencies, and supply-chain impact. Disable unused default features, commit and review `Cargo.lock`, and keep declared `rust-version`, CI toolchains, and dependency MSRVs consistent.

## Safety and change discipline

- Preserve user data and changes. Never use destructive Git recovery commands (`reset`, `clean`, checkout, stash, or equivalents); do not stage, commit, alter history, or change `HEAD`. Keep Git policy out of engine code.
- Validate options and configuration before mutation or agent invocation. Reject unknown or invalid fields where required. Never invoke configured commands through a shell; pass validated argv directly.
- Preserve effective user settings and inherited environment. Pi uses its own configured resources; built-in execution uses only its shell tool and is unsandboxed. Commands have same-user access and may expose secrets or affect systems outside the worktree. Do not claim built-in execution is complete or credentials isolated from same-user processes. Logs may contain secrets and cannot guarantee their removal.
- Do not add arbitrary shell hooks, dynamic shell expansion, project-specific defaults, or speculative abstractions unless required by the task.
- Before changing code, trace affected behavior and error paths. Add deterministic tests for success, invalid input, failures, safety, and regressions. Review the diff for scope, unintended changes, brittle assumptions, secret leakage, and untested branches.

## Verification

- During iteration, run targeted tests with quiet output and inspect failure diagnostics. Before handoff for code or configuration changes, run `make check` from the repository root. For documentation-only changes, run relevant lightweight validation such as `git diff --check`.
- Tests must use temporary non-Git projects and fake processes/commands. Do not require network, live models, credentials, Git, or developer worktree state. Keep tests deterministic and isolated. Add process/integration coverage when changing process, packaging, or release behavior; do not add network-dependent default tests.
- Release builds and smoke tests remain CI gates, not local requirements. If a required gate fails or is blocked, report incomplete status and name the gate.
