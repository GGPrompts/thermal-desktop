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
