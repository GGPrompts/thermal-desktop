# AI Terminal Landscape Research (March 2026)

## The Gap: Nobody is Building the Terminal FOR the Agents

Everyone is building AI agents that run IN terminals. Nobody is building a terminal purpose-built for orchestrating and visualizing AI agents. This is thermal-conductor's niche.

## Existing AI Terminal Features

### Warp Terminal
- **"Blocks" concept**: Each command+output is a discrete, selectable, copyable unit — not a scroll buffer. Semantic structure enables AI to reference individual blocks.
- **Oz Platform**: Cloud-based parallel agent orchestration. "Spin up unlimited parallel coding agents that are programmable, auditable, and fully steerable."
- **Multi-agent**: Integrates Claude Code, OpenAI Codex, Gemini CLI as first-class citizens.
- Built in Rust (Alacritty terminal rendering, Tokio async). 144+ FPS, 1.9ms average redraw.

### Cursor
- **Agent sandbox execution**: Three modes — sandbox (restricted), ask-every-time, run-everything.
- **Agent Client Protocol (ACP)**: Standardization layer similar to LSP but for AI agents.
- **`CURSOR_AGENT` env detection**: Warns about shell customization interfering with agent output parsing. Purpose-built terminal eliminates this friction.
- **Cloud agents** (v2.6, March 2026): Always-on agents triggered by events from Slack, Linear, GitHub, PagerDuty.

### Claude Code Agent Teams (State of the Art Multi-Agent)
- **Two display modes**: In-process (single terminal, Shift+Down to cycle) or split-pane (tmux/iTerm2).
- **Shared task list**: pending/in-progress/completed states with dependency tracking. File-locking prevents races.
- **Mailbox system**: Direct messaging between agents, broadcast to teammates.
- **Plan approval flow**: Teammates submit plans, lead reviews/approves.
- **Competing hypotheses**: Spawn multiple agents to investigate different theories and debate.
- **Storage**: `~/.claude/teams/{team-name}/config.json` and `~/.claude/tasks/{team-name}/`

### Other Notable Projects
- **Gemini CLI**: Conversation checkpointing, `--output-format stream-json` for real-time events, MCP support.
- **Cline** (VS Code): Checkpoint system — workspace snapshots after each agent step, diff comparison, rollback. Token/cost tracking per task loop.
- **AIChat**: Multi-provider CLI (20+ LLMs), REPL/CMD/Shell modes, session persistence, embedded LLM Playground.

## Context Window Visualization (Primitive Everywhere)

| Tool | Approach |
|------|----------|
| Aider | `/tokens` command prints text-based usage |
| Cline | Running counter of tokens and API cost |
| code2prompt | Token counting, smart filtering, source tree viz |
| Claude Code | Auto-compaction at ~95% capacity, `preTokens` metadata |

**Nobody is doing**: Real-time visual heatmaps, animated fill gauges, per-file token contribution breakdowns, compaction visualization.

## Protocols to Support

### OSC 633 (VS Code Shell Integration)
Marks command boundaries in terminal output:
- `A` — prompt start
- `B` — prompt end
- `C` — pre-execution
- `D;exitcode` — execution finished
- `E;commandline` — explicit command line

Enables semantic understanding of command boundaries. Treat each command+output as a structured block.

### OSC 133 (iTerm2/FinalTerm Shell Integration)
Similar command boundary marking. "Particularly useful for AI tool output parsing."

### Model Context Protocol (MCP)
- Open standard by Anthropic. JSON-RPC 2.0 based.
- Transports: stdio (local) and Streamable HTTP (remote, SSE streaming).
- Primitives: Tools, Resources, Prompts.
- **For thermal-conductor**: Act as MCP host. Each agent pane gets own MCP client connections.

### Claude Code Team Protocol
- Inter-agent communication: shared tasks, mailboxes, dependency graphs.
- File-locking for task claiming.
- Worth reading directly from the filesystem for visualization.

### Kitty Graphics Protocol
- APC escape sequences for inline images.
- Pixel-perfect rendering, Z-index layering, alpha blending, animation.
- Supported by: Kitty, WezTerm, Konsole, Ghostty (partial).
- Since we control the renderer, can implement richer protocol for agent visualizations.

## What Thermal-Conductor Should Build (Nobody Has These)

1. **GPU-rendered context heatmaps** — thermal gradient visualization of token usage per agent, per file
2. **Native multi-agent dashboard** — not tmux splits, purpose-built GPU renderer that understands agent state
3. **Semantic agent output parsing** — tool calls, code blocks, thinking sections, diffs → rich GPU widgets
4. **MCP host per pane** — each agent gets own MCP server connections
5. **Audio-per-agent TTS** — unique voice per agent (thermal-audio, already built!)
6. **Agent timeline scrubbing** — visual checkpoint system with diff-based history
7. **Context compaction visualization** — show what was dropped, what remains, capacity trajectory
8. **OSC 633 support** — parse command boundaries, render as Warp-style blocks
9. **Inter-agent dependency graph** — visual representation of blocked/unblocked agents
10. **Zero shell-customization friction** — no `CURSOR_AGENT` env hacks needed, we own the renderer

## Streamdown (Streaming Markdown Renderer)

A streaming markdown renderer purpose-built for LLM output. Forward-only, character-by-character parsing.

**Rust port**: `streamdown-rs` (fed-stew/streamdown-rs), v0.1.4, ~8k lines, 7 modular crates, MIT license.

**Relevant crates for thermal-conductor:**
- `streamdown-parser` — decoupled from ANSI renderer, yields structured markdown events
- `streamdown-syntax` — syntect-based syntax highlighting

**Architecture for integration:**
```
streamdown-parser  →  structured markdown events (headings, code blocks, lists)
streamdown-syntax  →  syntax highlighting data
        ↓
thermal-conductor  →  wgpu/glyphon rendering of those events
```

Useful for Phase 4 semantic scrollback and rich agent output rendering. The `ParseState` design for incremental state tracking across streaming chunks is a well-tested pattern.
