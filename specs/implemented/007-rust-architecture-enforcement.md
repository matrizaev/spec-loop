# Spec 007: Rust Architecture Enforcement

**Status:** Complete

## Goal

Make Rust architecture rules explicit, mechanically checked, and practical to maintain. Keep modules cohesive and model meaningful domain invariants with types, without introducing abstractions for appearance.

## Existing behavior

- Rust source contains oversized modules and inline unit-test modules.
- Import wildcards and module layout are not checked by a dedicated architecture test.
- Existing code uses booleans and primitive values for both independent options and semantic data; architectural boundaries are not consistently checked.

## Requirements

### Module size and layout

- Enforce source-file limits for all first-party Rust files in the repository, including `src/`, `tests/`, `examples/`, `benches/`, and build scripts. Exclude generated, vendored, and build-output files.
- Production module files must contain fewer than 1,599 lines (maximum 1,598). Test module files must contain fewer than 2,099 lines (maximum 2,098). Test module files are files named `tests.rs` and Rust files under `tests/`.
- Count physical lines, including blank lines, comments, and a final line without a terminating newline. Report offending path, measured count, and limit. Do not remove tests, comments, documentation, or whitespace, or use formatting changes as a size workaround.
- Bring existing files into compliance by splitting along cohesive responsibilities.
- Define modules in filesystem-backed Rust files. Inline module bodies (`mod name { ... }`) are forbidden everywhere, including test code. Use the directory form `module/mod.rs` as the module root whenever that module declares child modules, including an out-of-line `tests.rs`; place child files in that module directory. A leaf module with no child modules may use `module.rs`. Do not combine `module.rs` and `module/` as alternate roots for the same module. Do not use `#[path]` to relocate or rename modules.
- In each module file, put imports first, out-of-line child-module declarations next, and remaining items after them. A `macro_rules!` item required by a child module may precede that module declaration. Keep imports grouped by standard library, external crates, and crate-local paths; use `self::` for sibling-module imports.
- Prohibit glob imports (`use path::*`) in production and test code. Import referenced names explicitly.

### Domain types and state transitions

- Convert external representations into validated, semantic values at boundaries. Use enums for finite semantic choices and newtypes when they prevent mixing distinct concepts or encode a real invariant. Validate values at construction or conversion; internal policy and workflow logic must not repeatedly interpret unvalidated raw strings or primitives.
- Represent mutually exclusive lifecycle or workflow states with enums or state-specific types instead of independent booleans that permit contradictory combinations. Encode valid transitions explicitly and use consuming transitions when retaining prior state would violate an invariant.
- Keep booleans for genuinely independent facts and options. Do not add types, typestate, traits, or layers without a concrete invariant, capability, or caller.
- Keep fields private when uncontrolled access could violate an invariant. Add getters, setters, constructors, conversions, or transition methods only when a concrete caller needs them and they preserve the invariant. Do not add boilerplate accessors or expose mutation that bypasses it.
- Preserve existing observable behavior and safety guarantees. Add or update tests for affected transitions, construction, boundary validation, and invalid values.

### Architecture enforcement and documentation

- Add deterministic architecture tests, run by `make check`, that inspect Rust syntax and filesystem paths rather than relying only on source-text or regular-expression matching. Tests must detect over-limit files, inline module bodies, noncanonical module roots, `#[path]` module relocation, misplaced module declarations/imports, and glob imports in production and test source.
- Test the architecture checker with valid and invalid source fixtures. Diagnostics must identify the offending file and rule. Count-limit tests cover threshold boundaries and a final unterminated line; specialized newline-format cases are not required.
- Enforce semantic type and state rules through focused behavior tests and review. Do not use identifier-based heuristics to guess whether a boolean or primitive represents a domain state.
- Update `AGENTS.md` to state these enforceable module, import, type-modeling, and state-transition rules.
- Keep the existing nightly rustfmt import-grouping configuration effective. Do not add lint suppressions or weaken compiler/Clippy gates. Review any dependency needed for syntax-aware checks under repository dependency policy.

## Acceptance criteria

1. Architecture tests enforce production and test file limits across first-party Rust files: maximum 1,598 and 2,098 physical lines respectively. Boundary-count and final-unterminated-line cases pass; failures report path, measured count, and limit.
2. Existing oversized modules are split into cohesive filesystem modules without deleting or reducing tests, comments, or documentation.
3. No first-party source or test file contains an inline module body or `#[path]` module relocation. Modules with children, including out-of-line unit-test modules, use `module/mod.rs` roots with child files in that directory; only childless leaf modules may use `module.rs`. Unit tests use separate `tests.rs` files and out-of-line declarations. Architecture tests reject alternate or mismatched module roots.
4. Imports precede module declarations, declarations precede other module-level items except required macros, and no production or test source uses glob imports. Architecture tests cover violations and accepted standard, external, crate-local, and sibling-module imports.
5. Mutually exclusive lifecycle/workflow states use enums or state-specific types where booleans could permit contradictory combinations; independent booleans remain valid. Newtypes and encapsulation protect concrete invariants or prevent meaningful type confusion, rather than adding wrappers for appearance.
6. Tests cover representative valid transitions, invalid transitions or construction, and malformed external values without requiring live services.
7. `AGENTS.md` and architecture tests accurately describe and enforce these rules. Existing runtime behavior and safety guarantees remain unchanged.
8. Run `make check` after implementation. Tests require no network, live model, credentials, or developer worktree state.

## Non-goals

- Reworking product behavior or introducing workflow states unrelated to existing requirements.
- Banning independent boolean options or requiring a newtype for every primitive value.
- Adding layers, traits, or dependencies solely to satisfy an architectural pattern.
- Implementing process sandboxing or changing agent execution permissions.
