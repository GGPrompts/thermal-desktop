#!/usr/bin/env bash
set -euo pipefail

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cargo-target}"
RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/thermal"
REPO_ROOT="${REPO_ROOT:-$HOME/projects/thermal-desktop}"

kill_matching_pgrep() {
    local pattern="$1"
    local matches

    matches="$(pgrep -af "$pattern" || true)"
    if [[ -z "$matches" ]]; then
        return 0
    fi

    while read -r pid _; do
        [[ -n "$pid" ]] || continue
        kill "$pid" 2>/dev/null || true
    done <<<"$matches"
}

cd "$REPO_ROOT"

echo "== Kill thermal processes =="
pkill -f 'thermal-' || true
kill_matching_pgrep 'target/debug/thermal-'
kill_matching_pgrep 'cargo.*thermal-'

# Wait for graceful shutdown, then SIGKILL any stragglers
echo "   waiting for processes to exit..."
for i in 1 2 3 4 5; do
    if ! pgrep -f 'thermal-' >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
# Force-kill anything still alive
if pgrep -f 'thermal-' >/dev/null 2>&1; then
    echo "   force-killing remaining processes"
    pkill -9 -f 'thermal-' || true
    sleep 1
fi

echo "== Clean runtime state =="
mkdir -p "$RUNTIME_DIR"
rm -f "$RUNTIME_DIR"/*.pid "$RUNTIME_DIR"/*.sock || true

echo "== Reinstall binaries =="
cargo install --path crates/thermal-audio
cargo install --path crates/thermal-bar
cargo install --path crates/thermal-commander
cargo install --path crates/thermal-conductor
cargo install --path crates/thermal-dispatcher
cargo install --path crates/thermal-dispatch-cli
cargo install --path crates/thermal-hud
cargo install --path crates/thermal-launch
cargo install --path crates/thermal-lock
cargo install --path crates/thermal-messages
cargo install --path crates/thermal-monitor
cargo install --path crates/thermal-notify
cargo install --path crates/thermal-screensaver
cargo install --path crates/thermal-voice
cargo install --path crates/thermal-wallpaper

echo "== Start daemons =="
nohup thermal-messages >/tmp/thermal-messages.log 2>&1 &
nohup thermal-audio >/tmp/thermal-audio.log 2>&1 &
nohup thermal-voice listen >/tmp/thermal-voice.log 2>&1 &
nohup thermal-dispatcher >/tmp/thermal-dispatcher.log 2>&1 &
nohup thermal-bar >/tmp/thermal-bar.log 2>&1 &
nohup thermal-hud >/tmp/thermal-hud.log 2>&1 &
nohup thermal-notify >/tmp/thermal-notify.log 2>&1 &
nohup thermal-wallpaper >/tmp/thermal-wallpaper.log 2>&1 &
nohup thermal-screensaver >/tmp/thermal-screensaver.log 2>&1 &

sleep 3

echo "== Verify =="
pgrep -a 'thermal-' || true
thc doctor || true

echo "== Repair stale runtime artifacts =="
thc doctor --fix || true

echo "== Final verification =="
thc doctor || true
