---
description: Break down issues into ready-to-execute tasks (fully automated)
argument-hint: [issue description — or omit to process existing planning tasks]
---

## Two modes

**Mode 1 — With argument:** Break down a new issue into tasks, scout, draft, mark ready.
**Mode 2 — No argument:** Find all tasks with status "planning" in tasks.jsonl and run scout → draft → ready on them.

**Issue (if provided):** $ARGUMENTS

## Instructions — do ALL of these steps automatically, do NOT stop to ask the user

### Step 1: Understand context
- Read the CLAUDE.md in the current working directory
- Read `tasks.jsonl` in the current directory (create if it doesn't exist)
- Find the next available task ID

### Step 2: Get or create tasks
**If an issue was provided ($ARGUMENTS is not empty):**
- Break the issue into 2-8 concrete, implementable tasks
- Identify dependencies between tasks (which must finish before others can start)
- Each task should be small enough for a single agent to complete
- Append them to tasks.jsonl with status "planning"

**If no argument was provided:**
- Read tasks.jsonl and find all tasks with status "planning"
- These are the tasks to process in the following steps

If there are no tasks to process, say so and stop.

### Step 3: Scout files (launch haiku agents in parallel — one per task)
- For each task, launch a haiku Agent to search the codebase for relevant files
- Agents should use Glob and Grep to find files that need to be read or modified
- Prefix with `[r]` for read-context, `[w]` for write-targets
- Max 10 files per task
- Update each task's `files` array and change status to "scouted"

### Step 4: Draft prompts
- For each scouted task, read the discovered files and write a detailed implementation prompt
- Prompts must be self-contained — an agent should execute without other context
- Include exact file paths, what to change, and success criteria (e.g., "cargo check must pass")
- Keep prompts under 2000 characters
- Update each task's `prompt` field and change status to "drafted"

### Step 5: Check dependencies and mark ready
- Tasks with no deps or all deps already "done" → status "ready"
- Tasks with unfinished deps → status "blocked"

### Step 6: Write everything to tasks.jsonl
Each task is one JSON line:
```
{"id": N, "title": "...", "status": "ready", "deps": [M], "files": ["[r] path", "[w] path"], "prompt": "...", "notes": "..."}
```

### Step 7: Show summary
Display a table of all created/updated tasks with their status and deps.

## Line limit
If tasks.jsonl exceeds 50 lines after adding, warn: "⚠ Consider running /split to archive done tasks"

## Task statuses
- `planning` — just created, needs scout/draft
- `scouted` — files identified (intermediate, auto-advances)
- `drafted` — prompt written (intermediate, auto-advances)
- `ready` — can be executed now
- `blocked` — waiting on deps
- `running` — being executed
- `done` — completed
- `failed` — execution failed
