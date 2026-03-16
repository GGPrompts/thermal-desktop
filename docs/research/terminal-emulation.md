# Terminal Emulation Libraries Research (March 2026)

## Recommendation: alacritty_terminal 0.25 + nix PTY

### alacritty_terminal — Best Choice With Caveats

**Latest version**: 0.25.1 (paired with Alacritty 0.16.1)

The de facto standard for embedding a VT state machine in Rust. Used by Zed editor, Rio terminal, and multiple other projects.

**Core API:**
- `Term<T: EventListener>` — central type
- `renderable_content()` — iterates drawable cells with attributes, cursor, selection
- `scroll_display()`, `selection_to_string()` — standard operations
- Grid-based storage with configurable scrollback
- Full VTE parsing via `vte` crate (latest: 0.15.0)

**Embedding Pain Points:**
- API not designed for third-party consumers — breaking changes on every minor bump (pre-1.0 semver)
- `EventListener` trait requires a bridge layer
- `event_loop` module couples PTY I/O with event dispatch — if you want to own PTY separately, use `Term` + `vte` directly
- No inline image protocol (no Sixel, no Kitty graphics)
- Config types leak into some `Term` methods

**The Zed Integration Pattern (gold standard):**
1. `ZedListener` implements `EventListener`, sends events via `UnboundedSender`
2. `Term` wrapped in `Arc<FairMutex<Term<ZedListener>>>`
3. Background task runs event loop
4. Events batched: first processed immediately, then 4ms window
5. `TerminalElement` reads `renderable_content()` and paints via GPU

### Alternatives Assessed

| Crate | Version | Verdict |
|-------|---------|---------|
| **wezterm-term** | (not published) | Equivalent to alacritty_terminal but not extractable from wezterm monorepo |
| **termwiz** (wezterm) | latest | Escape sequence codec/surfaces — complementary, not a replacement |
| **vte** | 0.15.0 | Low-level parser only, no grid/scrollback — used inside alacritty_terminal |
| **vt100** | 0.16.2 | Simpler API, designed for embedding, good screen diffing. Backup option if alacritty_terminal coupling becomes painful |
| **Copa** (Rio) | fork of vte | Rio-specific extensions, not general-purpose |

### Watch: libghostty-vt

Ghostty extracting their VT parser as a reusable C-ABI library:
- SIMD-optimized fast paths for ASCII
- Kitty graphics protocol support, modern Unicode
- C API coming, tagged release expected 2026
- Someone built a [GPUI + Ghostty terminal](https://xuanwo.io/2026/01-gpui-ghostty/) proving embedding works
- **Revisit in 6-12 months** once C API stabilizes

### PTY: Use nix Directly

| Option | Verdict |
|--------|---------|
| **nix 0.29** (recommended) | Already in workspace with term+signal features. `openpty()` + fork/exec. What Alacritty uses. Zero new deps. |
| portable-pty 0.9.0 | Cross-platform overkill for Linux-only Wayland project |
| rustix | Less proven for PTY work, more deps |

### Action Items for thermal-conductor

1. Keep `alacritty_terminal = "0.25"`, bump `vte` from `"0.14"` to `"0.15"`
2. Follow Zed model: `EventListener` → `Arc<FairMutex<Term>>` → background tokio task → batch events
3. Use `nix::pty::openpty()` directly — add `"process"` to nix features for fork/exec
4. Call `term.renderable_content()` each frame → convert `RenderableCell` to wgpu instance buffer
5. Watch libghostty-vt for future migration path (Kitty graphics, SIMD parsing)
