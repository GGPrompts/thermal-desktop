# Thermal Runtime Roadmap

Date: 2026-03-31

Companion to:
- [2026-03-31-thermal-runtime-audit.md](/home/builder/projects/thermal-desktop/docs/audits/2026-03-31-thermal-runtime-audit.md)

## Goal

Turn Thermal from a promising collection of runtime pieces into a system with one clear authority model:

- `thermal-conductor` daemon owns managed session truth
- `thermal-messages` handles messaging/routing above that truth
- HUD, TUI, audio, and other surfaces subscribe instead of re-deriving state

## Priorities

### P0: Keep the Terminal Trustworthy

This work stays ahead of everything else. If the managed terminal is not boring, the rest of the architecture does not matter.

Current bar:

- attach/detach/reconnect are stable
- clean shell exit propagates reliably
- mouse, scroll, paste, focus, and mode sync are correct
- resize/render corruption stays fixed

Practical rule:

- correctness first
- performance second
- polish third

## Phase 1: Lock the Authority Model

### 1. Review and finalize the daemon semantic event model

Target issue:
- `therm-4w00`

Outcome:

- reviewed canonical session snapshot model
- reviewed semantic event categories
- reviewed ordering and resync semantics
- reviewed migration assumptions for HUD/TUI/audio

This is the architectural gate. Do not start broad migrations before this is settled.

### 2. Add semantic protocol support to conductor

Target issue:
- `therm-ldhp`

Outcome:

- semantic snapshot types in protocol
- semantic event stream subscription flow
- per-session sequence numbers
- explicit lag/resync rules

Success means the daemon has a real semantic transport, not just framebuffer attach.

### 3. Move canonical managed-session state into the daemon

Target issue:
- `therm-6yqa`

Outcome:

- daemon-owned per-session semantic state
- PTY/inference path updates that mutate canonical state
- lifecycle, runtime, tool, and context truth emitted from daemon-owned state

Success means the conductor daemon becomes the real source of truth for managed sessions.

## Phase 2: Migrate Consumers Off File-Watcher Truth

### 4. Migrate HUD and conductor UI consumers

Target issue:
- `therm-s88k`

Outcome:

- `thermal-hud` stops depending on direct state-file watching for normal managed sessions
- conductor TUI/window overlays consume daemon semantic state for managed sessions
- file-based fallback, if retained, is explicitly compatibility-only

Success means UI surfaces are views over daemon truth, not state interpreters.

### 5. Migrate audio announcement logic

Target issue:
- `therm-wcwo`

Outcome:

- `thermal-audio` consumes daemon semantic events for managed sessions
- speech triggers are event-driven instead of poll-and-diff driven
- voice assistant UI state stays separate from session truth

Success means audio no longer depends on parallel session-state inference.

## Phase 3: Reduce Transitional Architecture

### 6. Demote file/hook state to compatibility mode

This is not necessarily one issue yet, but it should happen only after Phases 1 and 2.

Outcome:

- `/tmp/*-state` remains optional compatibility input
- hook/script paths are not required for normal managed workflows
- daemon-owned state is the default path

### 7. Narrow cleanup work to evidence-based issues

Target issue:
- `therm-jfvf`

Outcome:

- dead-code allowances are re-audited
- ownership-retained fields are documented more clearly
- only truly unused code is removed

This should happen after the authority model is clearer, not before.

## Parallel Work That Still Makes Sense

### Voice pipeline quality

Target issue:
- `therm-sc0w`

Why it can proceed in parallel:

- mostly orthogonal to conductor authority work
- directly improves voice reliability
- does not depend on the semantic session-event migration

This is still worth doing even before the daemon-state migration is complete.

## Human Decision Track

### Prototype crate decision

Target issue:
- `therm-yiqu`

This is not critical-path architecture work. It is useful to decide, but it should not distract from the session/runtime authority model.

## What Not To Do Yet

Do not spend major effort on these before Phase 1 is settled:

- broad conductor decomposition/refactor work
- large cleanup passes based on stale assumptions
- compositor-specific productization
- performance tuning aimed at future local-model workloads

These may all matter later, but they are lower leverage than fixing authority.

## Success Criteria

Thermal reaches the next meaningful milestone when all of the following are true:

1. The managed terminal is boring to use.
2. The conductor daemon is the canonical source of truth for managed sessions.
3. UI and audio consumers subscribe to daemon truth instead of reconstructing it.
4. `thermal-messages` remains useful as a bus/router without becoming a shadow state authority.
5. Hook/file state is compatibility, not core architecture.

## Recommended Order

1. `therm-4w00`
2. `therm-ldhp`
3. `therm-6yqa`
4. `therm-s88k`
5. `therm-wcwo`
6. `therm-sc0w` in parallel where convenient
7. `therm-jfvf`
8. `therm-yiqu` whenever you want the workspace-scope cleanup decision

## Bottom Line

The next high-leverage move is not adding more orchestration features.

It is making one subsystem authoritative:

- conductor daemon owns managed session truth
- everything else consumes that truth

That is the move that makes the rest of the system simpler instead of more elaborate.
