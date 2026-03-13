---
description: Scout filepaths for tasks using haiku agents
argument-hint: [task-id or "all"]
model: haiku
---

Scout relevant files for tasks in the current project.

## Instructions

1. Read `tasks.jsonl` in the current directory
2. If `$ARGUMENTS` is a number, scout only that task ID
3. If `$ARGUMENTS` is "all" or empty, scout all tasks with status "planning"
4. For each task to scout:
   - Read the task title and notes
   - Search the codebase for relevant files using Glob and Grep
   - Find files that would need to be read or modified
   - Update the task's `files` array with the discovered paths
   - Change status from "planning" to "scouted"
5. Write the updated tasks back to `tasks.jsonl`

## Important
- Use Glob and Grep to actually search — don't guess at file paths
- Include files that need to be READ for context, not just modified
- Keep file lists concise (max 10 files per task)
- Prefix files with `[r]` for read-only context or `[w]` for write targets:
  e.g., `["[r] src/main.rs", "[w] src/tmux.rs"]`

Show a summary of what files were found for each task.
