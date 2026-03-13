---
description: Break down an issue into tasks with dependencies
argument-hint: <issue description>
---

Break down the following issue into implementation tasks:

**Issue:** $ARGUMENTS

## Instructions

1. Read the CLAUDE.md in the current working directory for project context
2. Read the `tasks.jsonl` file in the current directory (create it if it doesn't exist)
3. Find the next available task ID (max existing ID + 1)
4. Break the issue into concrete, implementable tasks (aim for 2-8 tasks)
5. For each task, identify:
   - **deps**: IDs of tasks that must complete first (empty array if none)
   - **files**: leave as empty array (use /scout to fill these in)
   - **prompt**: leave as empty string (use /draft to fill these in)
6. Append each task as a JSONL line to `tasks.jsonl`

## Task Format (one JSON object per line in tasks.jsonl)

```
{"id": N, "title": "...", "status": "planning", "deps": [], "files": [], "prompt": "", "notes": "..."}
```

## Status values
- `planning` — just created, needs scouting/drafting
- `scouted` — files identified
- `drafted` — prompt written, ready for review
- `ready` — approved for execution
- `running` — currently being executed
- `done` — completed
- `blocked` — waiting on dependency
- `failed` — execution failed

## Line limit rule
If tasks.jsonl exceeds 50 lines, suggest splitting into a sub-file (e.g., `tasks-feature-name.jsonl`) and keeping only active/recent tasks in the main file.

After creating tasks, show a summary table of what was planned.
