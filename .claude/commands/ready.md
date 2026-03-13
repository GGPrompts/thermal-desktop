---
description: Mark drafted tasks as ready for execution
argument-hint: [task-id or "all"]
---

Mark tasks as ready for execution after reviewing their prompts.

## Instructions

1. Read `tasks.jsonl` in the current directory
2. If `$ARGUMENTS` is a number, mark only that task ID as ready
3. If `$ARGUMENTS` is "all", mark ALL drafted tasks as ready
4. For each task:
   - Verify status is "drafted"
   - Verify the prompt field is non-empty
   - Verify deps are either empty or all deps have status "done"
   - Change status to "ready" (or "blocked" if deps aren't met)
5. Write the updated tasks back to `tasks.jsonl`

Show which tasks are now ready and which are blocked.
