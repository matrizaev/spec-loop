# Spec 015: Hierarchical Agent Instructions and Native Compaction

**Status:** Active

## Goal

Ensure workflow agents follow directory-scoped repository instructions for the workspace paths they inspect or modify, including when using the builtin harness, which does not automatically discover project instruction files. Also support opt-in native Responses API compaction for long built-in conversations without replaying workspace side effects.

## Current behavior

The profile can provide role-specific instruction files, and generated prompts tell agents to read those configured paths. This does not explicitly require agents to discover additional `AGENTS.md` files scoped to nested directories containing relevant files. The built-in backend resends its conversation input for each Responses API request and currently has no native compaction path.

## Requirements

### 1. Discover applicable instructions

- Before inspecting the contents of or modifying a workspace file, an agent must establish the applicable instructions from every existing `AGENTS.md` from the workspace root through the file's parent directory, in root-to-leaf order. Reading or reusing full contents already available in current context satisfies this requirement; knowing only a path or summary does not. Directory listings and path-only searches needed to locate relevant files and instruction files may precede this reading; they must not become recursive scans of unrelated directories.
- For a file that does not exist yet, use its intended parent directory when determining which instructions apply.
- When a task touches files under different directory trees, apply this process to each relevant tree. Within one agent invocation, check each relevant directory for `AGENTS.md` once and read each applicable instruction file once, subject to the refresh rules below. Reuse shared ancestors and instructions already supplied in full by the backend or read through configured paths; no duplicate tool read is required.
- For subsequent files, check only previously unchecked directory segments. Remember directories already checked even when no `AGENTS.md` was found. For example, after establishing instructions for `src/auth/login.rs`, inspecting `src/auth/logout.rs` requires no repeated discovery or reads; moving to `src/config/profile.rs` requires checking only the previously unchecked `src/config` directory.
- Refresh affected discovery or contents when there is reason to believe instructions were created, changed, moved, or deleted during the invocation, including changes made by the agent. If full instruction contents are no longer available in current context, reread them before affected work. Do not repeatedly poll unchanged directories or reread instructions merely because another file shares their scope.
- Reuse is limited to the current invocation and its retained context. A fresh role invocation must establish its own applicable instructions; do not assume another role's reads remain available or current. A continuation retaining the same context may reuse it under the same refresh rules.
- Agents that inspect or review workspace files must also read the applicable instruction hierarchy for the files in scope, even when they will not modify those files.
- Do not discover or load `AGENTS.md` files from sibling or unrelated directories, except instruction files explicitly configured in the profile, which remain required regardless of location. Do not recursively scan unrelated parts of the workspace.
- Follow all applicable instructions. Within a file's scope, a deeper `AGENTS.md` overrides conflicting ancestor instructions; nonconflicting ancestor instructions still apply. More-specific instructions do not change instructions outside their scope. Profile configuration requires reading a file but does not expand the directory scope of an `AGENTS.md` or give it precedence over a deeper applicable `AGENTS.md`. Other configured role instructions remain required; if they conflict with applicable repository instructions and existing workflow rules do not resolve the conflict, report the conflict rather than silently choosing one.
- Repository and configured instructions must not override workflow safety constraints, the selected specification, the typed role contract, or existing tool policies.
- Distinguish confirmed absence from failed discovery. Permission errors, failed commands, or incomplete output do not establish that `AGENTS.md` is absent. Resolve uncertainty with available tools; if applicable instructions cannot be established, stop affected work and report failure through the existing role contract.
- If an applicable instruction file exists but cannot be read, do not silently skip it or proceed with affected inspection or modification. Report the path and failure through the role's existing blocked-response contract. This does not require an instruction file to exist in every ancestor directory.

### 2. Apply the rule through built-in system instructions

- Add concise shared hierarchy guidance to the built-in harness system instructions used for every role that inspects or modifies workspace files, including planning, implementation, review, audit, and clarification roles.
- Keep configured instruction paths required, and require discovery of additional applicable `AGENTS.md` files rather than assuming configured paths are exhaustive. Backend-loaded contents may satisfy reading requirements but do not establish that all relevant nested instructions have been discovered.
- Do not change generated role prompts, role-specific prompt guidance, protocol schemas, or Pi prompts/tool guidance. Pi has its own `AGENTS.md` discovery and tool-use guidance; preserve it unchanged.
- Do not instruct agents to repeat discovery or reads before every file when the relevant hierarchy is already established and current. These are agent-guidance requirements, not a guarantee of model compliance.
- Preserve role boundaries: implementation and planning roles may modify only the files their existing contracts allow; review, audit, and clarification roles remain read-only.

### 3. Use existing tools efficiently

- Add concise shared tool-use guidance only to the built-in harness system instructions, using only tools already available to that backend. Do not require a particular search utility or new tool; leave Pi's own tool guidance unchanged.
- Locate relevant paths first, then use scoped content searches and targeted file ranges after establishing applicable instructions. Expand inspection when needed for correctness; targeted reading must not omit required instruction contents, relevant evidence, or diagnostics.
- Where supported, batch independent, bounded read-only operations to reduce tool round trips. Do not batch dependent operations or mutations merely for speed, and preserve each operation's failure diagnostics. Establish applicable instructions before batching workspace-content reads.
- During iteration, prefer quiet, targeted tests and inspect failures. This does not replace repository-required checks, configured workflow checks, or final verification.
- When output is truncated, inspect retained output or narrow subsequent inspection if possible. Never rerun a mutating command solely to recover omitted output. If evidence cannot be recovered safely, report the limitation rather than inventing results or treating incomplete output as proof of success.
- Treat these as system-instruction efficiency guidance, not hard caps on reading, output, or tool calls. Preserve role permissions, required evidence gathering, and failure reporting.

### 4. Preserve existing behavior

- Keep built-in instruction discovery and per-invocation reuse system-instruction-guided. Do not add automatic `AGENTS.md` loading, nested-file enumeration, instruction caches, filesystem watchers, or cross-role instruction state to the engine; do not change Pi's existing instruction behavior.
- Do not require users to list every nested `AGENTS.md` in profile configuration.
- Preserve generated role prompts, Pi prompts and tool guidance, configured prompt paths, role mappings, tool policies, workflow ordering, schemas, and protocol versions. The built-in system-instruction additions in requirements 2–3 and separate `agent.context_compaction` opt-in in requirement 5 are the only changes in scope.

### 5. Compact built-in conversations natively

- Add optional boolean `agent.context_compaction`, default `false` and valid only with the built-in backend. It opts into the documented OpenAI Responses API compaction operation; it does not change Pi invocation, effective settings, or Pi-managed context behavior.
- Do not replace native compaction with model-generated summaries, local summarization, transcript truncation, or dropping tool calls/results. Use the existing stage-resolved `context_window_tokens` as the only proactive limit. When the latest completed response reports input plus output tokens at or above 80% of this user-supplied limit, compact before the next generation request. Use checked arithmetic. If the option is enabled but no applicable limit or trustworthy usage is available, do not infer a model limit, estimate token use, or attempt compaction.
- Compact only at safe request boundaries: after a completed provider response and its shell-call results are incorporated, with no pending tool calls. Do not classify or retry generation requests based on undocumented context-window rejection signals. If a generation request fails, preserve existing failure behavior; compaction is not recovery or retry policy.
- Use the same selected model and existing OAuth path for compaction. Keep system instructions, role schema, and current role task available to each subsequent generation request. Preserve provider-native compacted items as typed conversation state; replace existing state only after validating a completed compaction response. Keep compacted state invocation-local, including correction-only continuation; never persist it across roles or runs.
- If the compaction operation is unsupported or fails, returns malformed/incomplete state, stop with an actionable sanitized error and preserve original conversation state and completed workspace effects. Do not continue the generation request, replay shell calls, or silently fall back to another model, auth method, or weaker output format.
- Include actual compact HTTP requests in provider request-attempt accounting. Do not count compaction as an agent response or shell call; report token usage only when the provider documents and returns valid counts. Do not expose compacted prompts, tool output, credentials, or raw provider bodies in logs.
- During implementation, cite current official provider documentation for the compaction request/response contract and supported usage fields. Do not infer ChatGPT OAuth entitlement or model support from API documentation alone.

## Acceptance criteria

1. The built-in harness system instructions used by all eight roles (`PlanAuthor`, `PlanReview`, `PlanAudit`, `PlanClarification`, `Implementation`, `Review`, `ReviewAudit`, and `ReviewClarification`) explicitly require establishing applicable `AGENTS.md` instructions from workspace root through each relevant file's parent directory before inspecting contents or modifying files, by reading or reusing full contents already available in current context. Path-only discovery may precede instruction reading.
2. Built-in system-instruction guidance distinguishes applicable ancestor instructions from sibling or unrelated instruction files, covers intended parent directories for new files, and states that deeper instructions override conflicting ancestor instructions only within their scope while retaining nonconflicting rules.
3. Configured instruction paths remain required regardless of location, while agents are told they may not be the complete hierarchy. Guidance distinguishes mandatory reading from directory scope and precedence, and requires reporting unresolved conflicts or unreadable applicable instruction files rather than silently proceeding.
4. The built-in system instructions receive hierarchy-discovery requirements without relying on native automatic discovery. Generated role prompts and Pi prompts/tool guidance remain unchanged.
5. Tests verify built-in system-instruction guidance across all eight roles for timing, discovery, scope, precedence, configured-path exception, and failure handling, including that permission errors, failed commands, and incomplete output do not establish instruction-file absence and unresolved discovery failures stop affected work under the existing role contract. Verify review, audit, and clarification roles remain read-only and plan authors retain their existing selected-plan-only write boundary.
6. Tests verify explicit per-invocation reuse guidance: shared ancestors and already-available full contents are not reread, checked directories without instruction files are not repeatedly checked, new trees require only previously unchecked segments, changes or lost contents require refreshing affected instructions, and fresh invocations establish their own hierarchy. Test generated guidance, not live-model compliance; no live agent or engine-side instruction tracker is required.
7. Built-in system-instruction tests verify concise guidance for scoped searches and targeted reads, safe batching of independent read-only operations, quiet targeted tests without weakening required verification, and safe handling of truncated output without replaying mutations. Guidance must not assume tools unavailable to the built-in backend or impose hard inspection caps. Pi prompts and tool guidance remain unchanged.
8. Existing instruction discovery does not add engine-side recursive loading or instruction caches. System-instruction guidance adds no tools and changes no output limits or storage behavior.
9. Configuration tests verify `agent.context_compaction` defaults off, is accepted only for the built-in backend, and leaves existing profiles valid. Deterministic fake-provider tests cover the 80% trigger boundary, no compaction below it, and no compaction when applicable limit or trustworthy usage is absent.
10. Compaction tests verify request ordering and typed compacted-state reuse, including malformed/incomplete results and unsupported/failing compaction responses. Compaction failure stops the role without a generation retry, preserves existing conversation state and completed effects, and never replays shell calls. Request-attempt accounting includes compact requests; Pi arguments, settings, and invocation remain unchanged.
11. Tests and implementation require no undocumented Responses context-rejection code or field, and do not retry generation requests based on such signals. Documentation cites the official provider compaction contract, explains opt-in and the 80% trigger, and avoids claiming unsupported OAuth/model compatibility. No live provider, credentials, network, Git, or developer worktree state required by default tests.
12. Run focused prompt and compaction tests plus `make check`; report observed results and any blockers.

## Non-goals

- Automatically reading or injecting instruction files in the engine, or adding instruction caches, watchers, or cross-role instruction state.
- Changing harness tools, tool-output limits or storage, context-budget configuration, cross-invocation session persistence, or telemetry beyond accurate accounting for native compaction requests.
- Loading instructions from unrelated directories or recursively indexing the workspace.
- Changing the precedence of workflow safety constraints, specifications, typed role contracts, or existing tool policies.
- Changing role permissions or protocol behavior. Profile schema changes are limited to the `agent.context_compaction` opt-in defined above.
