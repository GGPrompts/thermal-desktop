# GPU Text Rendering Research (March 2026)

## Recommendation: Stay on glyphon + cosmic-text, Upgrade Versions

### Current Stack Validated

Every production Rust terminal/editor doing GPU text uses cosmic-text or OS-native rasterization feeding into a glyph atlas. The pattern: CPU rasterize → GPU atlas → blit. Nobody does per-frame GPU path rasterization for terminal grids.

- **Zed**: wgpu + cosmic-text + texture atlas
- **Warp**: wgpu + cosmic-text + custom glyph atlas (~200 lines of shaders)
- **Rio**: wgpu + custom Sugarloaf engine
- **Alacritty**: OpenGL + crossfont (locked to OpenGL)

### Version Upgrade Path

| Dep | Current | Target (safe) | Target (aggressive) |
|-----|---------|---------------|---------------------|
| wgpu | 23 | **25** | 28 |
| glyphon | 0.7 | **0.9** | 0.10 |
| cosmic-text | 0.12 | **0.14** | 0.15 |

**Glyphon version ↔ dependency mapping:**

| glyphon | wgpu | cosmic-text | Date |
|---------|------|-------------|------|
| 0.7 (current) | 23 | 0.12 | Nov 2024 |
| 0.8 | 24 | 0.12 | Jan 2025 |
| 0.9 | 25 | 0.14 | Apr 2025 |
| 0.10 | 28 | 0.15 | Dec 2025 |

**Recommended: glyphon 0.9 / wgpu 25 / cosmic-text 0.14**
- 2-version wgpu jump (manageable API churn)
- cosmic-text 0.14: font hinting, configurable fallback, ASCII fast-path optimization
- ASCII fast-path directly benefits terminal rendering (overwhelmingly ASCII monospace glyphs)

### cosmic-text Evolution (0.12 → 0.18)

| Version | Key Features |
|---------|-------------|
| 0.14 | Configurable font fallback, `Shaping` enum, `PhysicalGlyph` |
| 0.15 | Variable fonts, pixel-based scrolling, **ASCII fast-path** |
| 0.16 | `Renderer` trait (abstracted rendering), configurable `Hinting` enum |
| 0.17 | Variable font fixes, improved ligatures, MSRV 1.89 |
| 0.18 | Ellipsizing support |

Note: glyphon 0.10 pins cosmic-text at 0.15, not 0.18.

### Alternatives — Don't Switch

| Library | Verdict |
|---------|---------|
| **Vello 0.6** | Wrong architecture — renders 2D scenes, owns pipeline. Can't inject as middleware into wgpu render pass like glyphon. No emoji. |
| **Parley 0.7** (Linebender) | Most advanced text layout in Rust, but no wgpu atlas renderer. Would rebuild glyphon from scratch. |
| **fontdue 0.9** | Fastest rasterizer but no shaping, no complex scripts, no Unicode. Games only. |
| **ab_glyph** | Too low-level — no layout, no shaping, no fallback. Would rebuild cosmic-text. |
| **swash 0.2** | What cosmic-text uses internally. No reason to use directly. |

### Glyph Atlas Techniques

- **CPU rasterize → GPU atlas** (current, optimal for terminals): cosmic-text/swash rasterize on CPU, upload to texture atlas packed by etagere, GPU samples from atlas. LRU eviction.
- **Subpixel grid snapping**: Glyphs snapped to 1/3 pixel offsets (0.0, 0.33, 0.66). 3x atlas entries per glyph max.
- **GPU outline rasterization** (Slug/Evan Wallace): Higher quality at arbitrary sizes but overkill for monospace grids with ~100 repeated glyphs.

### Action Items

1. Upgrade to glyphon 0.9 / wgpu 25 / cosmic-text 0.14 before or during Phase 1
2. Changes contained to `thermal-core/src/text.rs` (ThermalTextRenderer) + Cargo.toml + surface/device creation in each crate
3. wgpu surface configuration API changes will ripple into thermal-bar, thermal-lock, thermal-launch, thermal-notify
