---
description: Archive completed tasks to keep tracking files lean
argument-hint: [jsonl-file]
---

Split a tasks.jsonl file by archiving completed tasks.

## Instructions

1. Read the specified `$ARGUMENTS` file (default: `tasks.jsonl` in current directory)
2. Separate tasks into:
   - **Active**: status is NOT "done"
   - **Archived**: status is "done"
3. If there are archived tasks:
   - Write archived tasks to `tasks-archive-YYYY-MM-DD.jsonl` (today's date)
   - Write only active tasks back to the original file
4. Report how many tasks were archived and how many remain active

## Rules
- Never delete tasks — only move to archive
- Keep the original file's active tasks in their original order
- If an archive file for today already exists, append to it
