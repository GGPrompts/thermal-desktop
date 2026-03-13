---
description: Draft implementation prompts for scouted tasks
argument-hint: [task-id or "all"]
---

Draft detailed implementation prompts for tasks that have been scouted.

## Instructions

1. Read `tasks.jsonl` in the current directory
2. Read the CLAUDE.md for project context
3. If `$ARGUMENTS` is a number, draft only that task ID
4. If `$ARGUMENTS` is "all" or empty, draft all tasks with status "scouted"
5. For each task to draft:
   - Read all files listed in the task's `files` array
   - Write a detailed implementation prompt that:
     - States exactly what to build/change
     - References specific files and line numbers
     - Includes the thermal palette colors if visual work
     - Specifies what "done" looks like (success criteria)
     - Is self-contained (an agent should be able to execute it without other context)
   - Store the prompt in the task's `prompt` field
   - Change status from "scouted" to "drafted"
6. Write the updated tasks back to `tasks.jsonl`

## Prompt quality guidelines
- Be specific: "Add a `resize()` method to TmuxSession in src/tmux.rs" not "update tmux"
- Include file paths: "Edit /home/builder/projects/thermal-desktop/crates/thermal-conductor/src/tmux.rs"
- Include success criteria: "cargo check -p thermal-conductor must pass with no errors"
- Keep prompts under 2000 characters

Show each drafted prompt for review.
