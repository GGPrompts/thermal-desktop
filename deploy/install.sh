#!/bin/bash
# Install Thermal Desktop deploy configs
# Copies configs from deploy/ into their live locations (no symlinks)
#
# Usage:
#   ./install.sh          Copy all configs (prompts before overwriting changes)
#   ./install.sh --diff   Show what's changed without modifying anything
#   ./install.sh --force  Overwrite without prompting

set -euo pipefail

DEPLOY="$(cd "$(dirname "$0")" && pwd)"
DIFF_ONLY=false
FORCE=false

for arg in "$@"; do
    case "$arg" in
        --diff)  DIFF_ONLY=true ;;
        --force) FORCE=true ;;
        -h|--help)
            echo "Usage: $0 [--diff|--force]"
            echo "  --diff   Show what's changed without modifying anything"
            echo "  --force  Overwrite without prompting"
            exit 0
            ;;
        *) echo "Unknown option: $arg"; exit 1 ;;
    esac
done

CHANGED=0
INSTALLED=0
SKIPPED=0

# Deploy a single file: source -> destination
deploy_file() {
    local src="$1" dest="$2"
    mkdir -p "$(dirname "$dest")"

    # If destination is a symlink, always replace it
    if [ -L "$dest" ]; then
        if $DIFF_ONLY; then
            echo "  SYMLINK $dest -> $(readlink "$dest") (will be replaced with copy)"
            CHANGED=$((CHANGED + 1))
            return
        fi
        rm "$dest"
        cp "$src" "$dest"
        echo "  replace $dest (was symlink)"
        INSTALLED=$((INSTALLED + 1))
        return
    fi

    # New file — just copy
    if [ ! -e "$dest" ]; then
        if $DIFF_ONLY; then
            echo "  NEW $dest"
            CHANGED=$((CHANGED + 1))
            return
        fi
        cp "$src" "$dest"
        echo "  install $dest"
        INSTALLED=$((INSTALLED + 1))
        return
    fi

    # Existing file — check for differences
    if diff -q "$src" "$dest" > /dev/null 2>&1; then
        SKIPPED=$((SKIPPED + 1))
        return
    fi

    if $DIFF_ONLY; then
        echo "  CHANGED $dest"
        diff --color=auto -u "$dest" "$src" | head -30 || true
        echo ""
        CHANGED=$((CHANGED + 1))
        return
    fi

    if $FORCE; then
        cp "$src" "$dest"
        echo "  update $dest"
        INSTALLED=$((INSTALLED + 1))
        return
    fi

    # Interactive: show diff and ask
    echo "  CHANGED $dest"
    diff --color=auto -u "$dest" "$src" | head -20 || true
    printf "  Overwrite? [y/N] "
    read -r answer
    if [[ "$answer" =~ ^[Yy]$ ]]; then
        cp "$src" "$dest"
        echo "  updated $dest"
        INSTALLED=$((INSTALLED + 1))
    else
        echo "  skipped"
        SKIPPED=$((SKIPPED + 1))
    fi
}

# Deploy a directory of files
deploy_dir() {
    local src_dir="$1" dest_dir="$2"
    for f in "$src_dir"/*; do
        [ -f "$f" ] || continue
        deploy_file "$f" "$dest_dir/$(basename "$f")"
    done
}

# ── Kitty ────────────────────────────────────────────────────
echo "Kitty config..."

# If ~/.config/kitty is a directory symlink, replace it
if [ -L "$HOME/.config/kitty" ]; then
    if $DIFF_ONLY; then
        echo "  SYMLINK ~/.config/kitty -> $(readlink "$HOME/.config/kitty") (will be replaced with directory)"
        CHANGED=$((CHANGED + 1))
    else
        rm "$HOME/.config/kitty"
        mkdir -p "$HOME/.config/kitty"
        echo "  replace ~/.config/kitty (was symlink to directory)"
    fi
fi

deploy_dir "$DEPLOY/kitty" "$HOME/.config/kitty"

# ── Hyprland ─────────────────────────────────────────────────
echo "Hyprland config..."
deploy_file "$DEPLOY/hypr/hyprland.conf" "$HOME/.config/hypr/hyprland.conf"
deploy_file "$DEPLOY/hypr/hypridle.conf" "$HOME/.config/hypr/hypridle.conf"

# ── Systemd units ────────────────────────────────────────────
echo "Systemd units..."
SYSTEMD_DIR="$HOME/.config/systemd/user"
for f in "$DEPLOY/systemd"/*.service "$DEPLOY/systemd"/*.target "$DEPLOY/systemd"/*.slice; do
    [ -f "$f" ] || continue
    deploy_file "$f" "$SYSTEMD_DIR/$(basename "$f")"
done

# ── Claude settings + hooks ──────────────────────────────────
echo "Claude settings..."
deploy_file "$DEPLOY/claude/settings.json" "$HOME/.claude/settings.json"
deploy_file "$DEPLOY/claude/statusline.sh" "$HOME/.claude/statusline.sh"

echo "Claude hooks..."
deploy_dir "$DEPLOY/claude/hooks" "$HOME/.claude/hooks"

# Preserve execute bit on scripts
chmod +x "$HOME/.claude/statusline.sh" 2>/dev/null || true
chmod +x "$HOME/.claude/hooks/"*.sh 2>/dev/null || true

# ── Summary ──────────────────────────────────────────────────
echo ""
if $DIFF_ONLY; then
    echo "Diff complete: $CHANGED changed, $SKIPPED unchanged"
    if [ "$CHANGED" -gt 0 ]; then echo "Run without --diff to apply changes."; fi
else
    echo "Done: $INSTALLED installed/updated, $SKIPPED unchanged"

    # Reload systemd if any units were touched
    if [ "$INSTALLED" -gt 0 ]; then
        echo ""
        echo "Reloading systemd user daemon..."
        systemctl --user daemon-reload

        echo ""
        echo "Enabling thermal.target + core services..."
        systemctl --user enable thermal.target 2>/dev/null || true
        systemctl --user enable thermal-conductor.service 2>/dev/null || true
        systemctl --user enable thermal-audio.service 2>/dev/null || true
        systemctl --user enable swww-daemon.service 2>/dev/null || true
    fi
fi
