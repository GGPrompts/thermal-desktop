# thermal-bar

GPU-rendered Wayland layer-shell status bar.

## What it does
Renders a FLIR instrument panel anchored to the top of the screen via
wlr-layer-shell. Displays real-time system metrics (CPU, GPU temp, memory,
network), Hyprland workspace map, agent session badges, and a voice level
meter. Supports mouse clicks for workspace switching, voice mute toggle,
and session focus.

## Socket / Pidfile
- Pidfile: `/run/user/$UID/thermal/bar.pid`
- No socket (renders directly via Wayland)

## CLI Usage
```
thermal-bar
```
No flags. Configure via `~/.config/thermal/settings.toml`.

## Dependencies
- **Needs**: Wayland compositor with wlr-layer-shell (Hyprland), wgpu-compatible GPU
- **Needed by**: nothing (standalone visual component)

## Troubleshooting
- If the bar does not appear, check that wlr-layer-shell is supported by your compositor
- On NVIDIA, set `NVD_BACKEND=direct` if rendering is unstable after DPMS resume
- Agent badges require `/tmp/claude-code-state/` to be populated (run Claude sessions)
