---
description: Execute ready tasks with agents (parallel when possible)
argument-hint: [task-id or "all"]
---

Execute tasks that are marked as ready, using subagents.

## Instructions

1. Read `tasks.jsonl` in the current directory
2. If `$ARGUMENTS` is a number, execute only that task ID
3. If `$ARGUMENTS` is "all" or empty, execute all tasks with status "ready"
4. Build a dependency graph from the tasks
5. Group tasks into waves:
   - Wave 1: all ready tasks with no unfinished deps
   - Wave 2: tasks whose deps are all in wave 1
   - etc.
6. For each wave:
   - Launch tasks in parallel using Agent tool with sonnet model
   - Each agent gets the task's `prompt` field as its instruction
   - Each agent prompt should end with: "After completing, verify with cargo check. Report what you did."
   - Mark tasks as "running" before launching
   - When agents complete, mark tasks as "done" and record results in notes
   - If an agent fails, mark as "failed" with error in notes
7. After each wave completes, check if the next wave's deps are met
8. Continue until all waves are done
9. Write updated tasks back to `tasks.jsonl`

## Agent model selection
- Use **sonnet** agents for implementation tasks (default)
- Use **opus** agents for complex architecture/integration tasks (if task notes contain "complex" or "architecture")
- Use **haiku** agents for simple file changes or research

## Parallel execution rules
- Tasks in the same wave with NO shared write-files can run in parallel
- Tasks that write to the same file must run sequentially
- Maximum 5 agents in parallel

Show a summary after all waves complete.
