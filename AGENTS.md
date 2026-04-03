# Repository Guidelines

## Project Structure & Module Organization
This repository is a Rust workspace for Thermal Desktop. Most code lives in `crates/`, with shared logic in `crates/thermal-core` and `crates/thermal-terminal`, and app binaries such as `thermal-conductor`, `thermal-bar`, `thermal-audio`, and `thermal-dispatcher` alongside them. Runtime config lives in `config/`, helper scripts in `scripts/`, docs in `docs/`, and visual assets in `shaders/` and `colors/`.

## Build, Test, and Development Commands
Use Cargo from the workspace root:

- `cargo build` builds the full workspace.
- `cargo run -p thermal-conductor` runs the main TUI hub (`thc`).
- `cargo run -p thermal-conductor -- window` runs the GPU terminal window.
- `cargo test --workspace` runs all unit and async tests.
- `cargo test -p thermal-conductor` runs a focused crate test pass.
- `cargo fmt --all` formats the workspace.
- `cargo clippy --workspace --all-targets -- -D warnings` catches lint regressions.

For daemons or installed commands, finish with `cargo install --path crates/<crate-name>` so `~/.cargo/bin` picks up the new binary.

## Coding Style & Naming Conventions
The workspace uses Rust 2024 edition and standard Rust formatting: 4-space indentation, `snake_case` for modules/functions, `CamelCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Prefer shared abstractions in `thermal-core` over duplicated palette, state, or rendering code. Edit `crates/thermal-core/schemas/thermal-protocol.ggl` for protocol changes, not generated output.

## Testing Guidelines
Tests are primarily inline unit tests under `mod tests` within the edited module. Add regression tests next to the behavior you change, especially in parser, state inference, routing, and TUI state code. Run `cargo test --workspace` before opening a PR; for faster iteration, run the affected crate first, then the full workspace.

## Commit & Pull Request Guidelines
Recent history favors short imperative subjects such as `Fix thermal-terminal ANSI color mapping` or scoped summaries like `Wave 1+2: ...`. Keep commits focused by crate or behavior. PRs should list affected crates, describe user-visible changes, include verification commands, and attach screenshots or clips for TUI/Wayland/UI changes.

## Cross-Repo Sync Notes
Keep sibling repos aligned when changes affect setup or generated artifacts. Sync `~/projects/thermal-os-dotfiles` for Hyprland autostart/keybinds, kitty integration, and Claude hooks (`config/hypr/hyprland.conf`, `config/kitty/kitty.conf`, `config/claude/settings.json`, `config/claude/hooks/state-tracker.sh`). Sync `~/projects/ggl` when changing `.ggl` schemas or codegen expectations; `crates/thermal-core/Cargo.toml` uses a path dependency on `~/projects/ggl/proto/ggl-build`, and local agent tooling expects the `ggl` MCP server configured in `.claude/settings.local.json`.

## Configuration & Runtime Notes
Profiles and trust settings live in `config/profiles.toml` and `config/trust-tiers.toml`. Many services exchange state through `/run/user/$UID/thermal/` and `/tmp/*-state/`; changes touching IPC, watchers, or daemon startup should call out those paths in review notes. Local Claude-only additions live under `.claude/`; keep `.claude/settings.local.json` valid if you change MCP-backed workflows.

<!-- BEGIN BEADS INTEGRATION -->
## Issue Tracking with ggbd (beads)

**IMPORTANT**: This project uses **ggbd (beads)** for ALL issue tracking. Do NOT use markdown TODOs, task lists, or other tracking methods.

### Why ggbd?

- Dependency-aware: Track blockers and relationships between issues
- Git-friendly: Postgres/Supabase-backed storage with native sync
- Agent-optimized: JSON output, ready work detection, discovered-from links
- Prevents duplicate tracking systems and confusion

### Quick Start

**Check for ready work:**

```bash
ggbd ready --json
```

**Create new issues:**

```bash
ggbd create "Issue title" --description="Detailed context" -t bug|feature|task -p 0-4 --json
ggbd create "Issue title" --description="What this issue is about" -p 1 --deps discovered-from:bd-123 --json
```

**Claim and update:**

```bash
ggbd update <id> --claim --json
ggbd update bd-42 --priority 1 --json
```

**Complete work:**

```bash
ggbd close bd-42 --reason "Completed" --json
```

### Issue Types

- `bug` - Something broken
- `feature` - New functionality
- `task` - Work item (tests, docs, refactoring)
- `epic` - Large feature with subtasks
- `chore` - Maintenance (dependencies, tooling)

### Priorities

- `0` - Critical (security, data loss, broken builds)
- `1` - High (major features, important bugs)
- `2` - Medium (default, nice-to-have)
- `3` - Low (polish, optimization)
- `4` - Backlog (future ideas)

### Workflow for AI Agents

1. **Check ready work**: `ggbd ready` shows unblocked issues
2. **Claim your task atomically**: `ggbd update <id> --claim`
3. **Work on it**: Implement, test, document
4. **Discover new work?** Create linked issue:
   - `ggbd create "Found bug" --description="Details about what was found" -p 1 --deps discovered-from:<parent-id>`
5. **Complete**: `ggbd close <id> --reason "Done"`

### Auto-Sync

ggbd automatically syncs via Postgres/Supabase:

- Each write is stored directly in the Postgres database
- Use `ggbd sync` to sync changes
- No manual export/import needed!

### Important Rules

- Use ggbd for ALL task tracking
- Always use `--json` flag for programmatic use
- Link discovered work with `discovered-from` dependencies
- Check `ggbd ready` before asking "what should I work on?"
- ❌ Do NOT create markdown TODO lists
- ❌ Do NOT use external issue trackers
- ❌ Do NOT duplicate tracking systems

For more details, see README.md and docs/QUICKSTART.md.

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   ggbd sync
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds

<!-- END BEADS INTEGRATION -->
