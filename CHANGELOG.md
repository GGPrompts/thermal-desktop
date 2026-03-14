# Changelog — Thermal Desktop

All completed work from initial development through bare-metal deployment.

## Phase 1 Complete — 2025

### thermal-core (shared library)
- ThermalPalette [f32; 4] RGBA color constants
- WgpuContext helpers (Instance/Adapter/Device/Queue bundle)
- Text renderer abstraction wrapping glyphon 0.7
- Common geometry types: Point, Size, Rect with grid/split utilities
- Thermal gradient generator: LUT, f32 conversion, heat_label

### thermal-conductor (agent dashboard)
- Wayland window with wgpu rendering
- Glyphon text rendering with ANSI color mapping
- tmux session management (start/shutdown/persistence)
- Single-pane and multi-pane capture from tmux
- Terminal text rendering with thermal color palette
- Pane grid layout engine (Grid/Sidebar/Stack)
- Input routing: keyboard, mouse, escape sequences
- Click-to-focus sidebar with hit-testing
- Agent state detection via hook watching
- Audio notifications on state transitions
- D-Bus service: org.thermal.Conductor
- thermal-ctl CLI (list/capture/send/kill/state)
- Git diff awareness with notify crate file watching
- Thermal HUD overlay: FPS, agent count, CPU temp, changed files

### thermal-bar (status bar)
- Wayland layer-shell surface (TOP anchor, 32px exclusive zone)
- wgpu rendering pipeline with glyphon text and sparklines
- System metrics reader: CPU, memory, network, GPU from /proc and /sys
- Left/center/right module layout with 1px separator
- D-Bus client for thermal-conductor status
- Clock + date module with 500ms refresh cache
- Network and GPU metrics (AMD/NVIDIA detection)
- Thermal gradient sparklines with 30-sample rolling history

### thermal-lock (lock screen)
- ext-session-lock-v1 Wayland protocol implementation
- wgpu surface setup with Vulkan/GL backends
- Heat-map WGSL shader: Voronoi noise, 6-stop thermal gradient
- Auth input UI: masked password, blinking cursor
- PAM authentication via direct FFI (no bindgen)
- Clock display: green HH:MM:SS, muted YYYY-MM-DD
- Failed auth feedback: shake animation, CRITICAL flash overlay

### thermal-launch (app launcher)
- Wayland layer-shell overlay (700x500, centered)
- wgpu rendering pipeline with glyphon text
- Desktop file parser from XDG_DATA_DIRS
- Fuzzy search engine with substring matching and scoring
- Search input handling (keyboard, arrows, enter/escape)
- Targeting reticle UI with L-bracket corners
- App launch with field code stripping, --hidden CLI flag

### thermal-notify (notification daemon)
- D-Bus server: org.freedesktop.Notifications interface
- Wayland layer-shell popup (TOP|RIGHT, margin 16px)
- wgpu notification card rendering with urgency accent bar
- Urgency heat mapping: Low=COOL, Normal=WARM, Critical=SEARING
- Auto-dismiss timer with 300ms fade animation
- Notification queue with vertical stacking
- PipeWire audio cues via rodio (frequency-mapped tones)

### Bug Fixes & Security (code review + Codex audit)
- Password zeroing with zeroize crate (thermal-lock)
- Wayland surface pointer type safety
- PAM account management (pam_acct_mgmt)
- Conductor capture fallback index fix
- State detector Complete timeout logic
- Audio thread leak fix (Sink::sleep_until_end)
- Error pattern detection word-boundary fix
- D-Bus mutex poison recovery
- Escape key binding → Ctrl-Q
- Hardcoded BG colors → ThermalPalette
- WgpuContext async→sync conversion
- Unused dependency cleanup
- AgentState missing Hash derive
- Rect::grid divide-by-zero guard
- Launch buffer allocation optimization
- Lock shader hardcoded resolution fix

### Deployment & Dotfiles
- Arch dual-boot install guide with microcode + staged approach
- greetd, brightnessctl, wallpaper setup
- Rofi power menu fix
- polkit agent + rofi-wayland
- SSH setup for remote Claude Code access

## Bare-Metal Fixes — 2026-03-14
- Fix thermal-bar infinite configure loop
- Wire up async render loop with wgpu renderer
- Merge sparkline rects into single render pass (fix flicker)
- Boost thermal gradient floor for readable text on dark background
- Increase bar font size from 13px to 16px
- Hyprland 0.54 window rule block syntax migration
- Dual monitor config: ultrawide DP-1 + Dell portrait 240Hz
