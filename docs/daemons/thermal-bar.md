# thermal-bar (built into thermal-conductor)

> **Note**: thermal-bar is no longer a standalone daemon. It runs as a managed layer-shell surface inside thermal-conductor. This doc describes the bar's behavior.

GPU-rendered Wayland layer-shell status bar.

## What it does
Renders a FLIR instrument panel anchored to the top of the screen via
wlr-layer-shell. Displays real-time system metrics (CPU, GPU temp, memory,
network), Hyprland workspace map, agent session badges, and a voice level
meter. Supports mouse clicks for workspace switching, voice mute toggle,
and session focus.

## How it runs
The bar surface is spawned automatically when thermal-conductor starts in daemon mode.
It shares the conductor's wgpu device/queue and reads agent state directly from
the SemanticEventBus (in-process, replaces former D-Bus queries).

## Dependencies
- **Needs**: Wayland compositor with wlr-layer-shell (Hyprland), wgpu-compatible GPU
- On NVIDIA, set `NVD_BACKEND=direct` if rendering is unstable after DPMS resume
- Agent badges require `/tmp/claude-code-state/` to be populated (run Claude sessions)
