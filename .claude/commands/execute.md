---
description: Execute ready tasks with agents until all done
argument-hint: [task-id or "all"]
---

Execute tasks using subagents. Run everything until all tasks are done.

## Instructions — fully automated, do not stop to ask the user

1. Read `tasks.jsonl` in the current directory
2. If `$ARGUMENTS` is a number, execute only that task ID
3. If `$ARGUMENTS` is "all" or empty, execute ALL tasks with status "ready"

### Build waves from dependency graph
- **Wave 1**: all ready tasks with no unfinished deps
- **Wave 2**: tasks whose deps are all in wave 1
- Continue until all tasks are assigned to waves

### Execute each wave
For each wave:
1. Check which tasks can run in parallel (no shared `[w]` files)
2. Launch agents in parallel (max 5 at once):
   - **sonnet** agents for implementation (default)
   - **opus** agents if task notes contain "complex" or "architecture"
   - **haiku** agents if task notes contain "simple" or "research"
3. Each agent receives the task's `prompt` field as its full instruction
4. Mark tasks as "running" before launch
5. When agents return:
   - If successful → mark "done", record summary in notes
   - If failed → mark "failed", record error in notes
6. After wave completes, unblock the next wave's tasks (change "blocked" → "ready")
7. Continue to next wave

### After all waves
1. Write updated tasks back to `tasks.jsonl`
2. Run `cargo check` to verify the workspace still compiles
3. If cargo check fails, create a fix task and execute it
4. Show final summary of what was done

### Git
After all tasks complete successfully:
- `git add -A && git commit` with a summary of changes
- `git push`

## Important
- Do NOT ask for confirmation between waves — just keep going
- If a task fails, continue with other tasks that don't depend on it
- Log everything in task notes for later review
