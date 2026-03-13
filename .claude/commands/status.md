---
description: Show current task status across all tracking files
argument-hint: [filter: planning|scouted|drafted|ready|running|done|failed|all]
---

Show the current status of all tracked tasks.

## Instructions

1. Find all `tasks.jsonl` files in the current project:
   - `./tasks.jsonl` (root)
   - `./crates/*/tasks.jsonl` (per-crate)
   - Any `tasks-*.jsonl` split files
2. Parse each file and collect all tasks
3. If `$ARGUMENTS` is provided, filter by that status (or "all" for everything)
4. If no arguments, show a summary + any active tasks

## Display format

Show a summary table:
```
◉ THERMAL TASK STATUS
╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍
planning: N | scouted: N | drafted: N
ready: N    | running: N | done: N
failed: N   | blocked: N
╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍╍
```

Then list active tasks (anything not "done"):
```
[status] #ID title (deps: #X, #Y) — file.jsonl
```

## Line limit check
If any tasks.jsonl exceeds 50 lines, warn:
"⚠ tasks.jsonl has N lines — consider running /split to archive done tasks"
