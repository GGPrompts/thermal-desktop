---
description: Rebuild and install all thermal components to ~/.cargo/bin, then restart running daemons
argument-hint: [crate name or "all" — default: changed crates only]
---

# Rebuild Thermal Components

Rebuild affected thermal binaries, install to `~/.cargo/bin/`, kill stale processes, and restart daemons.

IMPORTANT: `CARGO_TARGET_DIR=/tmp/cargo-target` — `cargo build` alone does NOT update `~/.cargo/bin/`. You must use `cargo install --path`.

## All installable crates

| Crate | Path | Daemon? | How to restart |
|-------|------|---------|----------------|
| thermal-audio | crates/thermal-audio | Yes (pidfile: audio.pid) | `thermal-audio` |
| thermal-bar | crates/thermal-bar | Yes (long-running, no pidfile) | `thermal-bar &` |
| thermal-commander | crates/thermal-commander | No (stdio MCP server) | N/A |
| thermal-conductor | crates/thermal-conductor | Optional daemon mode | `thc daemon &` (if was running) |
| thermal-dispatcher | crates/thermal-dispatcher | Yes (no pidfile) | `thermal-dispatcher &` |
| thermal-face | crates/thermal-face | Yes (long-running, no pidfile) | `thermal-face &` |
| thermal-hud | crates/thermal-hud | Yes (long-running, no pidfile) | `thermal-hud &` |
| thermal-launch | crates/thermal-launch | No (on-demand overlay) | N/A |
| thermal-lock | crates/thermal-lock | No (on-demand) | N/A |
| thermal-messages | crates/thermal-messages | Yes (pidfile: messages.pid) | `thermal-messages &` or `thermal-messages --persist &` |
| thermal-monitor | crates/thermal-monitor | No (interactive TUI) | N/A |
| thermal-notify | crates/thermal-notify | Yes (long-running, no pidfile) | `thermal-notify &` |
| thermal-screensaver | crates/thermal-screensaver | Yes (long-running, no pidfile) | `thermal-screensaver &` |
| thermal-voice | crates/thermal-voice | Yes (pidfile: voice.pid) | `thermal-voice listen &` |
| thermal-wallpaper | crates/thermal-wallpaper | Yes (long-running, no pidfile) | `thermal-wallpaper &` |

Note: thermal-core and thermal-terminal are libraries (no binary).

## Strategy

1. If argument is a specific crate name: rebuild just that crate
2. If argument is "all": rebuild every binary crate
3. If argument is "nuke": full reset — kill ALL thermal processes (including phantoms from debug builds and cargo run), rebuild everything, clean start all daemons
4. If no argument: detect which crates have changes since the last install and rebuild only those

### Step 0: Nuke mode (only if argument is "nuke")

If the argument is "nuke", perform a full scorched-earth reset:

1. **Kill ALL thermal processes** — not just daemons, everything:
   ```bash
   # Kill all installed thermal binaries
   pkill -f 'thermal-' || true
   # Kill phantom debug builds
   pgrep -af 'target/debug/thermal-' | while read pid rest; do kill "$pid" 2>/dev/null; done
   # Kill cargo run processes building thermal crates
   pgrep -af 'cargo.*thermal-' | while read pid rest; do kill "$pid" 2>/dev/null; done
   ```

2. **Wait for processes to die** (2 seconds)

3. **Clean up ALL pidfiles and sockets**:
   ```bash
   rm -f /run/user/$UID/thermal/*.pid
   rm -f /run/user/$UID/thermal/conductor.sock
   ```

4. **Rebuild ALL binary crates** (same as "all" mode)

5. **Restart all standard daemons** in dependency order (don't wait for "was it running?" — start everything):
   - thermal-messages, thermal-audio, thermal-voice listen, thermal-dispatcher
   - thermal-bar, thermal-hud, thermal-notify, thermal-wallpaper, thermal-screensaver

6. **Skip** interactive components (TUI, monitor, conductor window, lock, launch)

Then skip to Step 6 (verify).

### Step 1: Detect what needs rebuilding

Use `git diff --name-only HEAD~1` (or the range of recent commits) to find changed files. Map changed files to crates:
- `crates/thermal-core/` changes affect ALL binary crate consumers (rebuild everything)
- `crates/thermal-terminal/` changes affect thermal-conductor
- `crates/<crate>/` changes affect just that crate

### Step 2: Check what's currently running

Run `pgrep -a 'thermal-'` to see which thermal processes are active. Save this list — you'll need it to know what to restart.

### Step 3: Rebuild

Run `cargo install --path crates/<crate>` for each affected crate. Run up to 4 installs in parallel to avoid thrashing.

### Step 4: Kill stale and phantom processes

For each rebuilt daemon that was running (from Step 2):

```bash
# Kill by process name
pkill -f 'thermal-<name>'
```

**IMPORTANT: Also kill phantom debug-build processes.** These are leftover processes from `cargo run` or old `target/debug/` binaries that linger and waste CPU/memory:

```bash
# Find and kill any thermal processes running from target/debug/ or via cargo run
pgrep -af 'target/debug/thermal-' | while read pid rest; do kill "$pid"; done
pgrep -af 'cargo.*thermal-' | while read pid rest; do kill "$pid"; done
```

Also clean up stale pidfiles and sockets for killed daemons:
- Pidfiles: `/run/user/$UID/thermal/{audio,messages,voice}.pid`
- Sockets: `/run/user/$UID/thermal/conductor.sock` (if no conductor process is running)
- Only remove pidfiles for processes you just killed

Wait 1-2 seconds after killing to let sockets close.

### Step 5: Restart daemons

Restart ONLY the daemons that were running before (from Step 2). Use nohup + background:

```bash
nohup thermal-<name> [args] > /tmp/thermal-<name>.log 2>&1 &
```

Restart order matters — dependencies first:
1. **thermal-messages** (message bus — others depend on it)
2. **thermal-audio** (TTS — dispatcher depends on it)
3. **thermal-voice**, **thermal-dispatcher** (voice pipeline)
4. **thermal-bar**, **thermal-hud**, **thermal-notify**, **thermal-wallpaper**, **thermal-screensaver**, **thermal-face** (UI components, independent)

Preserve original arguments: if `thermal-voice` was running with `listen`, restart as `thermal-voice listen`. If `thermal-messages` was running with `--persist`, include that flag. Check the original `pgrep -a` output for the full command line.

Do NOT restart:
- thermal-conductor TUI (interactive, user manages it)
- thermal-monitor (interactive TUI)
- thermal-commander (stdio, launched by MCP host)
- thermal-launch (on-demand)
- thermal-lock (on-demand)

### Step 6: Verify

After a couple seconds, run `pgrep -a 'thermal-'` again and confirm all previously-running daemons are back. Report the results.

## Output

Show a summary table:

```
| Component | Rebuilt | Restarted | PID |
|-----------|---------|-----------|-----|
| thermal-bar | Yes | Yes | 12345 |
| thermal-hud | Yes | Yes | 12346 |
| thermal-audio | No | No | 3448168 (unchanged) |
```
