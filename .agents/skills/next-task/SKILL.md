---
description: Results in a committable unit of work. Learnings accumulate in AGENTS.md. The checklist in IMPLEMENTATION.md tracks overall progress.
name: 'next-task'
---

Choose either the single most appropriate task, based on its `[P.E.T]` identifier, or most appropriate group of tasks that are tightly coupled and optimally done together, to work on next. Do NOT simply pick the first uncompleted task. Think about what enables the most progress. Complete the chosen task(s) fully.

Selection criteria:

- **Dependencies satisfied** - All prerequisite tasks are complete `[x]`. Infer dependencies from task descriptions and phase structure.
- **Blocking others** - Prioritize tasks that unblock many downstream tasks
- **Phase progression** - Earlier phases before later phases
- **Logical grouping** - Complete related tasks together when it makes sense

**Consistency Check**: Before implementing, analyze the task details for potential conflicts:

1. Cross-reference the task requirements against `README.md` functional requirements
2. Verify alignment with `IMPLEMENTATION.md` architecture decisions and algorithms
3. Check for contradictions with other tasks in the same phase or dependent phases
4. If you find inconsistencies:

- Stop and report the conflict to the user with specific references (e.g., "Task 2.3.1 assumes X, but README FR-2.V.3 specifies Y")
- Do NOT attempt to resolve spec conflicts autonomously—these require human decision
- Propose alternatives if you have insights, but wait for user direction

**Dependency Discovery**: If during implementation you discover a task cannot be completed until another task is done first:

1. Add a dependency notation to the blocked task: `depends: [P.E.T]` (using the task ID)
2. Switch to work on the blocking task instead, or report to the user if it's outside current scope
3. Document the discovered dependency in the task description for future cycles

**Create a Todos list to tackle the task**, including:

1. Mark the task as in progress in `IMPLEMENTATION.md`:

- Change `[ ]` to `[~]`
- Checkout a new git branch named after the task ID, pattern: `git checkout -b task/P.E.T main`
  - if marking more than one task as in progress, use the first task ID for the branch name
  - **DO NOT git add or git commit**, I will do this manually after reviewing the changes

2. all steps you plan to take to complete the task

3. Mark the task as complete in `IMPLEMENTATION.md`:

- Change `[~]` to `[x]`
- If other tasks have been completed in the process of completing the picked task, mark those as complete as well
- Update ### Progress Summary task counters, for both the current phase and the total
- Update ### Current Sprint section if appropriate
- **DO NOT git add or git commit**, I will do this manually after reviewing the changes

4. Summarize:

- What task was completed
- Any issues encountered and how they were resolved

**If during implementation, a task proves impossible or significantly diverges from the original expectation:**

- Stop work immediately
- Report the blocker to the user (do not attempt to resolve architectural issues autonomously)
- Ask the user to choose between:
  - keep the changes and continue attempts with the original plan
  - undo the changes, and alter the README.md and IMPLEMENTATION.md for alternatives or adaptations for a future cycle
