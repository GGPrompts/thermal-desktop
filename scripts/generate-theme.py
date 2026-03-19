#!/usr/bin/env python3
"""
generate-theme.py — Propagate colors from colors/thermal.toml to all config files.

Reads the single-source-of-truth TOML file and patches every config that has
THERMAL-COLORS-START / THERMAL-COLORS-END marker comments.

Usage:
    python3 scripts/generate-theme.py          # from thermal-desktop repo root
    python3 scripts/generate-theme.py --check  # exit 1 if any file would change
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# Locate repos
# ---------------------------------------------------------------------------

SCRIPT_DIR = Path(__file__).resolve().parent
THERMAL_DESKTOP = SCRIPT_DIR.parent
DOTFILES = Path(os.environ.get(
    "THERMAL_DOTFILES",
    THERMAL_DESKTOP.parent / "thermal-os-dotfiles",
))

TOML_PATH = THERMAL_DESKTOP / "colors" / "thermal.toml"

# Target files (path, marker_start, marker_end)
TARGETS = {
    "palette.rs": (
        THERMAL_DESKTOP / "crates" / "thermal-core" / "src" / "palette.rs",
        "// THERMAL-COLORS-START",
        "// THERMAL-COLORS-END",
    ),
    "palette_legacy": (
        THERMAL_DESKTOP / "crates" / "thermal-core" / "src" / "palette.rs",
        "// THERMAL-PALETTE-COLORS-START",
        "// THERMAL-PALETTE-COLORS-END",
    ),
    "kitty.conf": (
        DOTFILES / "config" / "kitty" / "kitty.conf",
        "# THERMAL-COLORS-START",
        "# THERMAL-COLORS-END",
    ),
    "hyprland.conf": (
        DOTFILES / "config" / "hypr" / "hyprland.conf",
        "# THERMAL-COLORS-START",
        "# THERMAL-COLORS-END",
    ),
    "starship.toml": (
        DOTFILES / "config" / "starship.toml",
        "# THERMAL-COLORS-START",
        "# THERMAL-COLORS-END",
    ),
}

# ---------------------------------------------------------------------------
# Minimal TOML parser (stdlib only — no tomllib on Python <3.11)
# ---------------------------------------------------------------------------

def parse_toml(path: Path) -> dict[str, str]:
    """Parse a flat-ish TOML file into {color_name: "#RRGGBB"} dict.

    Supports [section] headers and key = "value" lines.  Flattens into a
    single namespace (e.g. bg, freezing, accent_cold).
    """
    colors: dict[str, str] = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or line.startswith("["):
                continue
            m = re.match(r'^(\w+)\s*=\s*"(#[0-9a-fA-F]{6})"', line)
            if m:
                colors[m.group(1)] = m.group(2).lower()
    return colors


# ---------------------------------------------------------------------------
# Generators — produce the inner content for each marker block
# ---------------------------------------------------------------------------

def hex_to_0x(hex_str: str) -> str:
    """'#0a0010' -> '0x0a0010'"""
    return "0x" + hex_str.lstrip("#")


def hex_to_components(hex_str: str) -> tuple[str, str, str]:
    """'#0a0010' -> ('0x0a', '0x00', '0x10')"""
    h = hex_str.lstrip("#")
    return (f"0x{h[0:2]}", f"0x{h[2:4]}", f"0x{h[4:6]}")


def hex_to_rgba(hex_str: str) -> str:
    """'#ef4444' -> 'ef4444ff' (with full alpha)"""
    return hex_str.lstrip("#") + "ff"


# Ordered list of (name, group_comment) to preserve the existing layout
COLOR_ORDER = [
    # group, name
    ("Void / Background", "bg"),
    (None, "bg_light"),
    (None, "bg_surface"),
    ("Cold spectrum", "freezing"),
    (None, "cold"),
    (None, "cool"),
    ("Neutral", "mild"),
    (None, "warm"),
    ("Hot spectrum", "hot"),
    (None, "hotter"),
    (None, "searing"),
    (None, "critical"),
    ("White-hot", "white_hot"),
    ("Text", "text"),
    (None, "text_bright"),
    (None, "text_muted"),
    ("Accents", "accent_cold"),
    (None, "accent_cool"),
    (None, "accent_neutral"),
    (None, "accent_warm"),
    (None, "accent_hot"),
]


def gen_color_impl(colors: dict[str, str]) -> str:
    """Generate the Color impl block content."""
    lines = ["impl Color {"]
    for group, name in COLOR_ORDER:
        if group:
            lines.append(f"    // {group}")
        upper = name.upper()
        hex_val = hex_to_0x(colors[name])
        lines.append(f"    pub const {upper}: Color = Color::from_hex({hex_val});")
        # Blank line between groups
        next_idx = COLOR_ORDER.index((group, name)) + 1
        if next_idx < len(COLOR_ORDER) and COLOR_ORDER[next_idx][0] is not None:
            lines.append("")
    lines.append("}")
    return "\n".join(lines)


def gen_palette_impl(colors: dict[str, str]) -> str:
    """Generate the ThermalPalette impl block content."""
    lines = ["impl ThermalPalette {"]
    for group, name in COLOR_ORDER:
        if group:
            lines.append(f"    // {group}")
        upper = name.upper()
        r, g, b = hex_to_components(colors[name])
        lines.append(f"    pub const {upper}: [f32; 4] = Self::hex({r}, {g}, {b});")
        next_idx = COLOR_ORDER.index((group, name)) + 1
        if next_idx < len(COLOR_ORDER) and COLOR_ORDER[next_idx][0] is not None:
            lines.append("")
    return "\n".join(lines)


def gen_kitty_colors(colors: dict[str, str]) -> str:
    """Generate kitty color scheme block."""
    c = colors
    lines = [
        "# ── Thermal Color Scheme ──────────────────────────────────",
        "",
        "# Background / Foreground",
        f"background {c['bg']}",
        f"foreground {c['text']}",
        f"selection_background {c['freezing']}",
        f"selection_foreground {c['text_bright']}",
        "",
        "# Black (cold void)",
        f"color0  {c['bg']}",
        f"color8  {c['cold']}",
        "",
        "# Red (searing / critical)",
        f"color1  {c['searing']}",
        f"color9  {c['critical']}",
        "",
        "# Green (readout / warm)",
        f"color2  {c['warm']}",
        f"color10 {c['mild']}",
        "",
        "# Yellow (hot)",
        f"color3  {c['hot']}",
        f"color11 {c['hotter']}",
        "",
        "# Blue (cool)",
        f"color4  {c['accent_cool']}",
        f"color12 {c['accent_cold']}",
        "",
        "# Magenta (cold spectrum)",
        f"color5  {c['freezing']}",
        f"color13 {c['text']}",
        "",
        "# Cyan (neutral / teal)",
        f"color6  {c['accent_neutral']}",
        f"color14 {c['mild']}",
        "",
        "# White (white-hot)",
        f"color7  {c['text_bright']}",
        f"color15 {c['white_hot']}",
    ]
    return "\n".join(lines)


def gen_hyprland_colors(colors: dict[str, str]) -> str:
    """Generate hyprland border color block."""
    searing = hex_to_rgba(colors["searing"])
    hotter = hex_to_rgba(colors["hotter"])
    cold = hex_to_rgba(colors["cold"])
    lines = [
        "    # Thermal border colors: active = searing red, inactive = cold purple",
        f"    col.active_border = rgba({searing}) rgba({hotter}) 45deg",
        f"    col.inactive_border = rgba({cold})",
    ]
    return "\n".join(lines)


def gen_starship_colors(colors: dict[str, str]) -> str:
    """Generate starship color block."""
    c = colors
    lines = [
        '# ── Custom: THERMAL reticle prefix ──────────────────────────────',
        '[custom.thermal]',
        'command = "echo \'◉ THERMAL\'"',
        'when = "true"',
        f'style = "bold {c["searing"]}"',
        'format = "[$output]($style) "',
        '',
        '# ── Directory ────────────────────────────────────────────────────',
        '[directory]',
        f'style = "bold {c["hot"]}"',
        'format = "[⊕ $path]($style) "',
        'truncation_length = 4',
        'truncate_to_repo = true',
        'read_only = " 🔒"',
        f'read_only_style = "{c["searing"]}"',
        '',
        '# ── Git branch ───────────────────────────────────────────────────',
        '[git_branch]',
        'symbol = "◉ "',
        f'style = "{c["warm"]}"',
        'format = "[$symbol$branch]($style) "',
        '',
        '# ── Git status ───────────────────────────────────────────────────',
        '[git_status]',
        "format = '([$all_status$ahead_behind]($style) )'",
        f'style = "{c["hotter"]}"',
        'conflicted = "⚡"',
        'ahead = "▲${count}"',
        'behind = "▽${count}"',
        'diverged = "⟫${ahead_count}⟪${behind_count}"',
        'untracked = "?${count}"',
        'stashed = "≡"',
        'modified = "~${count}"',
        f'staged = "[+${{count}}]({c["warm"]})"',
        f'deleted = "[-${{count}}]({c["searing"]})"',
        '',
        '# ── Node.js ──────────────────────────────────────────────────────',
        '[nodejs]',
        'symbol = "⬡ "',
        f'style = "{c["accent_neutral"]}"',
        'format = "[$symbol$version]($style) "',
        '',
        '# ── Python ───────────────────────────────────────────────────────',
        '[python]',
        'symbol = "▲ "',
        f'style = "{c["accent_neutral"]}"',
        'format = "[$symbol$version]($style) "',
        'pyenv_version_name = true',
        '',
        '# ── Rust ─────────────────────────────────────────────────────────',
        '[rust]',
        'symbol = "⚙ "',
        f'style = "{c["accent_neutral"]}"',
        'format = "[$symbol$version]($style) "',
        '',
        '# ── Go ───────────────────────────────────────────────────────────',
        '[golang]',
        'symbol = "█ "',
        f'style = "{c["accent_neutral"]}"',
        'format = "[$symbol$version]($style) "',
        '',
        '# ── Command duration ─────────────────────────────────────────────',
        '[cmd_duration]',
        'min_time = 500',
        f'style = "{c["text"]}"',
        'format = "[╍ ${duration}]($style) "',
        'show_notifications = false',
        '',
        '# Override to red when slow (>5s)  — Starship uses the same field;',
        '# we threshold via min_time and the style is handled by the format.',
        '# For >5s we use a separate approach: the style changes automatically',
        '# based on the threshold being crossed — we set a second profile below.',
        '',
        '# ── Time ─────────────────────────────────────────────────────────',
        '[time]',
        'disabled = false',
        f'style = "{c["text_muted"]}"',
        'format = "[$time]($style) "',
        'time_format = "%H:%M:%S"',
        '',
        '# ── Prompt character ─────────────────────────────────────────────',
        '[character]',
        f'success_symbol = "[⟫]({c["warm"]})"',
        f'error_symbol   = "[⟫]({c["searing"]})"',
        f'vimcmd_symbol  = "[◉]({c["hot"]})"',
        '',
        '# ── Misc: disable modules we don\'t need cluttering the prompt ────',
        '[package]',
        'disabled = true',
        '',
        '[docker_context]',
        'disabled = true',
        '',
        '[aws]',
        'disabled = true',
        '',
        '[gcloud]',
        'disabled = true',
        '',
        '# [env_var] — left unconfigured; disabled via omission from format string',
        '',
        '[jobs]',
        'disabled = false',
        'symbol = "⊕"',
        f'style = "{c["hotter"]}"',
        'format = "[$symbol$number]($style) "',
    ]
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Patcher — replace content between markers
# ---------------------------------------------------------------------------

def patch_between_markers(
    content: str,
    marker_start: str,
    marker_end: str,
    new_inner: str,
) -> str:
    """Replace everything between marker_start and marker_end (exclusive)."""
    # Find the markers
    start_idx = content.find(marker_start)
    end_idx = content.find(marker_end)
    if start_idx == -1:
        raise ValueError(f"Marker not found: {marker_start}")
    if end_idx == -1:
        raise ValueError(f"Marker not found: {marker_end}")
    if end_idx <= start_idx:
        raise ValueError(f"End marker appears before start marker")

    # Find end of start marker line
    after_start = content.index("\n", start_idx) + 1
    # Find start of end marker line
    before_end_line = content.rfind("\n", start_idx, end_idx) + 1

    before = content[:after_start]
    after = content[before_end_line:]

    return before + new_inner + "\n" + after


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description="Generate thermal theme files from colors/thermal.toml")
    parser.add_argument("--check", action="store_true", help="Check mode: exit 1 if any file would change")
    args = parser.parse_args()

    if not TOML_PATH.exists():
        print(f"ERROR: {TOML_PATH} not found", file=sys.stderr)
        return 1

    colors = parse_toml(TOML_PATH)
    if not colors:
        print("ERROR: no colors parsed from TOML", file=sys.stderr)
        return 1

    print(f"Loaded {len(colors)} colors from {TOML_PATH.name}")

    # Map target name -> generator function
    generators = {
        "palette.rs": gen_color_impl,
        "palette_legacy": gen_palette_impl,
        "kitty.conf": gen_kitty_colors,
        "hyprland.conf": gen_hyprland_colors,
        "starship.toml": gen_starship_colors,
    }

    changed_files: list[str] = []
    skipped_files: list[str] = []

    for name, (path, marker_start, marker_end) in TARGETS.items():
        if not path.exists():
            print(f"  SKIP {name}: {path} not found")
            skipped_files.append(name)
            continue

        original = path.read_text()
        new_inner = generators[name](colors)

        try:
            patched = patch_between_markers(original, marker_start, marker_end, new_inner)
        except ValueError as e:
            print(f"  ERROR {name}: {e}", file=sys.stderr)
            return 1

        if patched == original:
            print(f"  OK   {name}: no changes")
        else:
            changed_files.append(name)
            if args.check:
                print(f"  DIFF {name}: would change")
            else:
                path.write_text(patched)
                print(f"  WRITE {name}: updated")

    # Summary
    print()
    if changed_files:
        if args.check:
            print(f"CHECK FAILED: {len(changed_files)} file(s) would change: {', '.join(changed_files)}")
            return 1
        else:
            print(f"Updated {len(changed_files)} file(s): {', '.join(changed_files)}")
    else:
        print("All files up to date.")

    if skipped_files:
        print(f"Skipped {len(skipped_files)} file(s): {', '.join(skipped_files)}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
