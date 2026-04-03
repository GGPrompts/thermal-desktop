---
description: Rebuild and install all thermal components to ~/.cargo/bin, then restart running daemons
argument-hint: [crate name, "all", "nuke", or add "fast" to skip tests — e.g. "fast" or "all fast"]
---

# Rebuild Thermal Components

Rebuild affected thermal binaries, install to `~/.cargo/bin/`, kill stale processes, and restart daemons.

IMPORTANT: `CARGO_TARGET_DIR=~/.cargo-target` — `cargo build` alone does NOT update `~/.cargo/bin/`. You must use `cargo install --path`.

## All installable crates

| Crate | Path | Daemon? | How to restart |
|-------|------|---------|----------------|
| thermal-audio | crates/thermal-audio | Yes (pidfile: audio.pid, socket: audio.sock) | `thermal-audio` |
| thermal-commander | crates/thermal-commander | No (stdio MCP server) | N/A |
| thermal-conductor | crates/thermal-conductor | Optional daemon mode (socket: conductor.sock) | `thc daemon &` (if was running) |
| thermal-dispatcher | crates/thermal-dispatcher | Yes (pidfile: dispatcher.pid) | `thermal-dispatcher &` |
| thermal-launch | crates/thermal-launch | No (on-demand overlay) | N/A |
| thermal-lock | crates/thermal-lock | No (on-demand) | N/A |

Note: thermal-core and thermal-terminal are libraries (no binary).
Note: thermal-bar, thermal-hud, thermal-messages, thermal-voice, thermal-monitor, thermal-notify, thermal-screensaver, thermal-wallpaper have been removed.

## Strategy

Arguments are parsed for modifiers first: "fast" anywhere in the argument skips the test step. The remaining word determines the mode.

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
   rm -f /run/user/$UID/thermal/*.sock
   ```

4. **Run workspace tests** (unless "fast" modifier was passed):
   ```bash
   cargo test --workspace --lib
   ```
   If tests fail, abort the rebuild and show the failure output. Do NOT install broken binaries. Hint the user to use `/rebuild nuke fast` to skip tests if they know what they're doing.

5. **Rebuild ALL binary crates** (same as "all" mode)

6. **Restart all standard daemons** in dependency order (don't wait for "was it running?" — start everything):
   - thermal-audio, thermal-dispatcher

7. **Skip** interactive components (TUI, monitor, conductor window, lock, launch)

Then skip to Step 6 (verify).

### Step 1: Detect what needs rebuilding

Use `git diff --name-only HEAD~1` (or the range of recent commits) to find changed files. Map changed files to crates:
- `crates/thermal-core/` changes affect ALL binary crate consumers (rebuild everything)
- `crates/thermal-terminal/` changes affect thermal-conductor
- `crates/<crate>/` changes affect just that crate

### Step 1.5: Run workspace tests (unless "fast" modifier)

If "fast" was NOT passed as an argument, run lib tests before installing:

```bash
cargo test --workspace --lib
```

If tests fail:
- **Abort the rebuild** — do NOT proceed to install
- Show the test failure output clearly
- Hint: "Tests failed. Use `/rebuild fast` to skip tests if you're iterating."

If tests pass, proceed normally.

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
- Pidfiles live under `/run/user/$UID/thermal/*.pid`
- Sockets live under `/run/user/$UID/thermal/*.sock`
- Only remove pidfiles/sockets for processes you just killed, unless you are in `nuke` mode
- Prefer `thc doctor` after restart to detect any stale runtime artifacts you missed

Wait 1-2 seconds after killing to let sockets close.

### Step 5: Restart daemons

Restart ONLY the daemons that were running before (from Step 2). Use nohup + background:

```bash
nohup thermal-<name> [args] > /tmp/thermal-<name>.log 2>&1 &
```

Restart order matters — dependencies first:
1. **thermal-audio** (TTS — dispatcher may depend on it)
2. **thermal-dispatcher**

Do NOT restart:
- thermal-conductor TUI (interactive, user manages it)
- thermal-commander (stdio, launched by MCP host)
- thermal-launch (on-demand)
- thermal-lock (on-demand)

### Step 6: Verify and repair

Wait 2 seconds for daemons to initialize, then run the doctor verify/repair loop:

1. `pgrep -a 'thermal-'` — confirm all previously-running daemons are back
2. `thc doctor` — read-only health check (pid/socket liveness)
3. If doctor reports issues (stale sockets, dead pids), automatically run `thc doctor --fix`
4. After --fix, run `thc doctor` one final time to confirm clean state
5. If still broken after fix, flag for manual investigation — do NOT loop forever

Include doctor status in the final summary.

## Output

Show a summary table:

```
| Component | Rebuilt | Restarted | PID |
|-----------|---------|-----------|-----|
| thermal-audio | Yes | Yes | 12345 |
| thermal-dispatcher | Yes | Yes | 12346 |
| thermal-conductor | No | No | 3448168 (unchanged) |

Doctor: CLEAN (or list issues found/fixed)
Tests: PASSED (or SKIPPED if fast mode)
```
