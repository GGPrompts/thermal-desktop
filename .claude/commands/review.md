---
description: Review completed work for quality and correctness
argument-hint: [task-id or "recent"]
---

Review completed tasks for code quality, correctness, and thermal aesthetic consistency.

## Instructions

1. Read `tasks.jsonl` in the current directory
2. If `$ARGUMENTS` is a number, review that task ID
3. If `$ARGUMENTS` is "recent" or empty, review all tasks completed since last review
4. For each task to review:
   - Read all files in the task's `files` array (the write targets)
   - Check for:
     - **Correctness**: Does the code compile? Does it do what the task asked?
     - **Quality**: Is it clean, idiomatic Rust? No unnecessary complexity?
     - **Consistency**: Does it use thermal-core palette colors? Follow project patterns?
     - **Security**: Any obvious vulnerabilities?
     - **Integration**: Does it work with the rest of the codebase?
   - Run `cargo check` to verify compilation
   - Note any issues found
5. Report findings with severity: **critical** (must fix), **warning** (should fix), **nit** (optional)

If issues are found, suggest follow-up tasks to fix them (but don't create them automatically — let the user decide).
