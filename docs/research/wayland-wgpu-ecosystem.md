# Wayland + wgpu Ecosystem Research (March 2026)

## Current vs Latest Versions

| Crate | Current | Latest | Gap |
|-------|---------|--------|-----|
| wgpu | 23.0.1 | **28.0.1** | 5 major behind |
| winit | 0.30.13 | **0.30.13** | Current! |
| smithay-client-toolkit | 0.19.2 | **0.20.0** | 1 minor behind |
| glyphon | 0.7.0 | **0.9.0** | 2 minor behind |
| cosmic-text | 0.12.1 | **0.18.2** | 6 minor behind |

## Key Findings

### winit — You're Current
0.30.13 is latest stable. Critical Wayland detail: **must call `pre_present_notify()` before every `surface.present()`** or the app freezes when window is hidden/occluded. Verify all crates do this.

### wgpu — Constrained by glyphon
glyphon tracks wgpu 1:1. No glyphon release exists for wgpu 26, 27, or 28. Max clean upgrade: **wgpu 25 + glyphon 0.9**.

Going past 25 means forking glyphon or building your own text pipeline.

Notable in wgpu 28: **fixed NVIDIA crash when presenting from another thread** — relevant if you move rendering off main thread.

### smithay-client-toolkit 0.20.0
Non-breaking upgrade (same wayland-client 0.31). Incremental changes. Upgrade when convenient.

### iced — Consider for thermal-conductor Only
- Powers entire COSMIC desktop, production-quality
- `iced::widget::Shader` lets you embed custom wgpu shaders as widgets
- No native layer-shell support (thermal-bar needs pop-os/iced fork)
- Good fit for thermal-conductor Phase 2 (complex pane UI), bad fit for thermal-bar/thermal-lock

### Slint — Skip
No native wgpu renderer, immature layer-shell support, GPL licensing.

### COSMIC Toolkit
- **cosmic-text**: Upgrade independently (standalone crate, no COSMIC dependency)
- **cosmic-term**: Uses alacritty_terminal + iced. Study their PTY patterns for Phase 2.
- **libcosmic**: Too heavy, brings COSMIC design language. Skip.

## Missing Wayland Protocols for Terminal

| Protocol | Why | Priority |
|----------|-----|----------|
| **keyboard-shortcuts-inhibit-v1** | Pass Ctrl+Alt+etc through to terminal | High (Phase 1) |
| **wp-primary-selection** | Middle-click paste (Linux terminal UX expectation) | High (Phase 1) |
| **text-input-v3** | IME/CJK input methods | Medium (Phase 2+) |

## Frame Pacing on Wayland + NVIDIA

1. **Always use `PresentMode::Fifo`** (VSync). Mailbox/Immediate cause NVIDIA issues.
2. **Always call `pre_present_notify()`** before `present()` — hooks Wayland frame callbacks.
3. Pattern: `RedrawRequested → render → pre_present_notify() → present()`
4. For infrequent renderers (thermal-bar, thermal-lock): render on events only, don't run continuous loop.
5. NVIDIA driver 590.48+ fixes Vulkan swapchain recreation and Wayland crashes.

## GPU Terminal Projects to Study

| Project | Renderer | Why |
|---------|----------|-----|
| **Rio** | Rust + wgpu | Most architecturally similar to thermal-conductor target |
| **cosmic-term** | Rust + iced/wgpu | Uses alacritty_terminal, study PTY patterns |
| **Ghostty** | Zig, Metal/OpenGL | 45K stars, libghostty-vt being extracted |
| **Foot** | C, CPU (pixman) | Benchmark for latency comparison |

## Recommended Upgrade Path

### Step 1 (Before Phase 1)
- Verify `pre_present_notify()` called in all crates
- Update NVIDIA driver to 590.48+ if needed

### Step 2 (During Phase 1)
- wgpu 23 → **25** + glyphon 0.7 → **0.9** + cosmic-text 0.12 → **0.14**
- smithay-client-toolkit 0.19 → **0.20**

### Step 3 (Phase 2 — Multi-Pane)
- Add keyboard-shortcuts-inhibit protocol
- Add primary-selection protocol
- Evaluate iced for thermal-conductor UI
- Study Rio + cosmic-term rendering pipelines
