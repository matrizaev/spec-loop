# Spec 009: Coherent Plan Slices

**Status:** Complete

## Goal

Help PlanAuthor and Implementation manage context by preferring plans organized into coherent, bounded slices and having each implementation invocation complete the next slice rather than attempting the entire plan at once. Preserve the selected specification's full scope and existing review/repair workflow.

## Existing behavior

- PlanAuthor maps acceptance criteria to ordered changes and tests but is not asked to group work into coherent slices.
- Implementation starts with the plan's first concrete step and is asked to make progress, but is not explicitly told to stop after the next coherent slice.
- Reviews compare the workspace with the selected specification and approved plan; slice sequencing must not narrow their review scope.

## Requirements

### PlanAuthor guidance

- Prefer organizing implementation plans as an ordered sequence of coherent, bounded slices suitable for separate implementation and review passes.
- For each slice, identify its intended outcome, relevant work/tests, dependencies on earlier slices, and the acceptance criteria it advances or completes. Together, all slices must cover every acceptance criterion and required repository instruction.
- Keep slices proportionate to the selected specification. A small or naturally indivisible change may use one slice; do not invent extra slices or split work into incomplete placeholder behavior solely to follow a format.
- Slice organization is guidance, not a new plan schema or permission to weaken, defer, or omit requirements. Preserve the existing evidence-backed handoff contract and read-only rules.

### Implementation guidance

- On each implementation invocation, identify and complete the next uncompleted coherent slice in plan order, including prerequisites necessary for that slice. Do not implement all remaining slices in one invocation or jump ahead to later independent slices.
- If review or repair feedback identifies defects in the current slice, address those before moving to a later slice, subject to existing audit and repair rules.
- Treat slices as sequencing and context guidance only. The selected specification remains the full scope; later slices are not optional and incomplete work is not complete merely because the current slice is done.
- Return `ready_for_review` when current changes are ready for checks and review, even if later slices remain. This status does not claim that the full specification is complete. Preserve prior workspace changes and keep the approved plan read-only.
- Keep existing typed responses, retry behavior, check ordering, review/repair transitions, and completion conditions unchanged.

### Review safeguard

- Review the full selected specification and approved plan, not only the current slice. Treat slice boundaries as implementation order, not as narrowed acceptance scope. Report actionable unmet requirements so the normal repair loop can continue with the next slice.

## Acceptance criteria

1. Generated PlanAuthor prompts tell the role to prefer an ordered set of coherent, bounded slices, cover all acceptance criteria across the plan, state slice dependencies, and avoid artificial splitting or incomplete placeholder behavior.
2. Generated Implementation prompts direct each invocation to the next uncompleted slice in order, include only its necessary prerequisites, avoid implementing all remaining slices at once, and prioritize repairs to the current slice before advancing.
3. Implementation prompt text makes clear that later slices remain required, `ready_for_review` means only that current work is reviewable, the selected specification remains authoritative, and the approved plan remains read-only.
4. Generated Review prompts explicitly retain full-spec and full-plan review scope; slice boundaries do not excuse unmet acceptance criteria.
5. Prompt unit tests assert the PlanAuthor, Implementation, and Review guidance is present in the appropriate role prompts. Other role prompts, generated JSON schemas, typed response contracts, and workflow transitions remain unchanged.
6. Tests require no network, live model, credentials, or developer worktree state. Run `make check` after implementation.

## Non-goals

- Changing plan file format, response schemas, workflow state, retry policy, or review/repair mechanics.
- Requiring multiple slices when one coherent slice is sufficient.
- Treating plan slices as authority to defer or weaken any specification requirement.
- Changing which files an agent may access or how much context a backend provides.
