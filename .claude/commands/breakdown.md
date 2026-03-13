---
description: Break down an issue into ready-to-execute tasks (fully automated)
argument-hint: <issue description>
---

Fully break down the following issue into implementation tasks, scout files, draft prompts, and mark everything ready for execution.

**Issue:** $ARGUMENTS

## Instructions — do ALL of these steps automatically, do NOT stop to ask the user

### Step 1: Understand context
- Read the CLAUDE.md in the current working directory
- Read `tasks.jsonl` in the current directory (create if it doesn't exist)
- Find the next available task ID

### Step 2: Break down into tasks
- Break the issue into 2-8 concrete, implementable tasks
- Identify dependencies between tasks (which must finish before others can start)
- Each task should be small enough for a single agent to complete

### Step 3: Scout files (use haiku agents in parallel)
- For each task, launch a haiku Agent to search the codebase for relevant files
- Use Glob and Grep to find files that need to be read or modified
- Prefix with `[r]` for read-context, `[w]` for write-targets
- Max 10 files per task

### Step 4: Draft prompts
- For each task, read the scouted files and write a detailed implementation prompt
- Prompts must be self-contained — an agent should execute without other context
- Include exact file paths, what to change, and success criteria (e.g., "cargo check must pass")
- Keep prompts under 2000 characters

### Step 5: Check dependencies and mark ready
- Tasks with no deps or all deps already "done" → status "ready"
- Tasks with unfinished deps → status "blocked"

### Step 6: Write everything to tasks.jsonl
Append each task as one JSON line:
```
{"id": N, "title": "...", "status": "ready", "deps": [M], "files": ["[r] path", "[w] path"], "prompt": "...", "notes": "..."}
```

### Step 7: Show summary
Display a table of all created tasks with their status and deps.

## Line limit
If tasks.jsonl exceeds 50 lines after adding, warn: "⚠ Consider running /split to archive done tasks"

## Task statuses
- `ready` — can be executed now
- `blocked` — waiting on deps
- `running` — being executed
- `done` — completed
- `failed` — execution failed
