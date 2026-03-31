# Thermal Runtime Audit

Date: 2026-03-31

## Scope

This is a blunt architecture review of the current Thermal stack after the recent terminal window synchronization fixes and follow-up design work around daemon-owned agent state.

The goal is not to grade polish. The goal is to answer:

- what is already strong
- what is transitional
- what is likely to become core
- what should stop expanding until the authority model is clearer

## Executive Summary

Thermal is already more than a collection of terminal hacks. It has recognizable system boundaries:

- `thermal-conductor` as terminal/session runtime
- `thermal-messages` as local bus plus routing layer
- multiple consumers (`thermal-hud`, TUI, audio, dispatcher)
- persistence/replay and trust-gated execution paths

The main architectural problem is split authority.

Today, at least three layers overlap:

- conductor daemon owns PTYs and window attach state
- messages daemon owns semantic message routing
- file/hook watchers still carry too much agent semantic truth

That makes the system powerful, but also muddy.

My blunt assessment is:

- the conductor daemon and terminal stack are the most important long-term asset
- the messages daemon is useful and should stay, but should not become the source of truth for session state
- file-watcher and hook-driven semantic state should be treated as compatibility, not architecture
- the project should optimize for "best managed-agent terminal/runtime" rather than "best general-purpose terminal emulator"

## What Is Strong

### 1. The Session Runtime Direction Is Correct

The strongest long-term bet in the repo is the conductor daemon plus terminal ownership model.

Why:

- PTY ownership gives control over lifecycle, exit, resize, modes, titles, and input
- the daemon can become the canonical source of truth for managed sessions
- attach/detach/reconnect can be designed intentionally instead of inferred through sidecars
- UI, audio, and orchestration can subscribe to real state instead of reconstructing it

This matters much more for Thermal's goals than generic terminal feature parity.

### 2. `thermal-messages` Is Already a Real System

The message bus is not a toy. It already has:

- Unix-socket transport
- replayable ring buffer
- live subscriber fanout
- optional persistence
- typed message model
- lightweight routing backends

That is a legitimate local event bus. It is a good subsystem. It just should not be overloaded into being the place where PTY/session truth is reconstructed.

### 3. The Product Thesis Is Real

A full custom Arch/Hyprland setup is niche.

A terminal/runtime built for AI-agent workflows is not niche in the same way.

The adoptable product is not "use my entire desktop." It is "use the terminal/runtime layer that makes agent workflows dramatically better."

That is a much stronger distribution path.

## What Is Transitional

### 1. File-Watcher Semantic State

The current `ClaudeStatePoller` pattern is useful, but it is transitional.

It duplicates work across:

- `thermal-hud`
- conductor TUI
- conductor window overlays
- `thermal-audio`
- other session-status consumers

That means:

- duplicated watchers
- duplicated interpretation logic
- possible drift in semantics
- hooks/scripts staying in the critical path longer than they should

This should move behind daemon-owned semantic state for managed sessions.

### 2. `thermal-messages` Routing as the Center of Orchestration

The routing layer is useful, especially for:

- `@claude`
- `@codex`
- `@dispatcher`
- `@system`

But it is still basically a semantic bus plus backend launcher. It is not yet a full control plane.

That is fine. It just means the bus should sit above session/runtime truth, not replace it.

### 3. The Current Terminal Is in the "Authority Layer Stabilization" Phase

The recent fixes were not cosmetic:

- attach ordering
- lag/resync behavior
- terminal mode propagation
- mouse behavior
- clean PTY exit propagation

Those are all authority-layer bugs. They are normal at this stage. They do not mean the terminal direction is wrong. They mean the terminal has reached the hard part.

## What To Double Down On

### 1. Daemon-Owned Semantic Session State

This is the highest-value architecture move.

The daemon should own:

- runtime family (`claude`, `codex`, `copilot`, etc.)
- lifecycle state
- prompt/response state
- tool activity
- context saturation
- exit metadata
- session relationships / continuation links

That unlocks a clean subscriber model for HUD, TUI, audio, and future automation.

### 2. Structured Semantic Event Streams

The daemon protocol should grow from framebuffer attach plus ad hoc polling into:

- session snapshots
- semantic event stream subscriptions
- explicit lag/resync behavior
- per-session sequence numbers

This is the modern replacement for "send keys and hope."

### 3. Narrowly-Scoped Terminal Excellence

Do not try to beat Kitty at everything.

Do try to become the best terminal/runtime for managed agent sessions.

That means prioritizing:

- lifecycle boringness
- attach/detach/reconnect correctness
- mouse/scroll/paste/focus correctness
- resize boringness
- daemon-owned truth

If those are excellent, the terminal already justifies itself even if Kitty remains better for some generic use cases.

## What To Stop Expanding

### 1. UI Consumers Re-Deriving Truth

HUD, TUI, audio, and similar surfaces should not keep growing independent state interpretation logic if the daemon is going to own truth anyway.

Every new watcher-based or hook-based semantic path increases migration cost later.

### 2. Desktop-Specific Coupling as Product Center

Hyprland integration is valuable for your own environment, but it is not the best center of gravity for adoption.

The more the system's value depends on a full custom desktop stack, the smaller the audience becomes.

Keep compositor-specific work in the "nice for this environment" bucket, not the product thesis bucket.

### 3. Broad Cleanup Issues That Assume Live Code Is Dead

Recent cleanup issues showed a risk of inaccurate "dead code" assumptions. Cleanup work should be narrower and evidence-based, especially around renderer and terminal-adjacent code.

## Kitty vs Thermal Terminal

### Kitty Is Stronger Today On

- terminal correctness
- renderer maturity
- selection and scrollback boringness
- mouse/input reliability
- general-purpose shell trust
- "I do not want to think about the terminal" stability

### Thermal Is Stronger Today On

- daemon ownership of managed sessions
- ability to unify terminal and semantic state
- orchestration-aware lifecycle design
- attach/detach/reconnect under your control
- custom session metadata and future event streams
- becoming a real runtime, not just a terminal emulator

### The Correct Strategic Framing

Thermal should not be judged first as a universal replacement for Kitty.

It should be judged first as a managed-agent terminal/runtime.

That means the right question is:

"Would I choose Thermal over Kitty for managed agent sessions?"

Not:

"Would I choose Thermal over Kitty for every shell task on the machine?"

## Milestones That Flip the Default Choice

Thermal becomes the default for managed sessions when:

1. Lifecycle is boring
   - attach, detach, reconnect, and exit are dependable

2. Input is boring
   - keyboard, mouse, wheel, paste, and focus reporting are dependable

3. Resize/render is boring
   - no corruption, stale rows, wrap glitches, or viewport mismatches

4. Session truth is clearly better than Kitty-based workflows
   - daemon-owned semantic state
   - minimal dependence on fragile hooks for managed sessions

5. It solves a workflow problem Kitty cannot solve as cleanly
   - continuation/session orchestration
   - semantic subscriptions
   - better multi-agent visibility

## Cloud Now, Local Later

Right now, cloud-hosted models flatten the hardware difference between your phone and your Arch box more than people expect. In that world, the value is mostly:

- workflow quality
- persistence
- orchestration
- visibility
- low-friction control

Not raw local inference speed.

That means Thermal is valuable today primarily as a cloud-era orchestration runtime.

But if local models keep improving, the same architecture becomes even more valuable:

- local GPU scheduling matters
- many concurrent agents matter
- session isolation matters
- terminal/runtime responsiveness under local load matters
- a daemon-owned orchestration substrate matters more

So the strong position is:

- build a great cloud-era orchestration terminal now
- do not block the future where the machine becomes the execution substrate

That makes Thermal useful in both futures:

- useful now because orchestration is bad everywhere
- more valuable later if local models become strong enough for daily use

## Product and Adoption Thesis

The scalable product is probably not "my full Linux desktop setup."

It is:

- an AI-native terminal/runtime
- with daemon-owned session truth
- strong persistence and attach semantics
- semantic event streams
- orchestration-aware UX

That is much easier to ask people to adopt than:

- Arch Linux
- Hyprland
- a full bespoke workstation philosophy

So the desktop stack is a differentiator for development and dogfooding.

The terminal/runtime is the likely adoptable layer.

## Subsystem Assessment

### Most Future-Proof

`thermal-conductor` daemon plus terminal ownership.

Reason:

- it sits at the real execution boundary
- it can become the authority layer
- it can emit state instead of reconstructing it

### Valuable but Should Stay Layered Above

`thermal-messages`.

Reason:

- good bus
- good local replay/fanout/routing story
- useful for user/agent/system messages

But it should consume or coordinate with canonical session truth, not define it.

### Most Likely to Become Compatibility Rather Than Core

Direct file-watcher semantic state as the primary path.

Reason:

- duplicated watchers
- duplicated interpretation logic
- too much drift risk
- hard to make authoritative

### Highest Current Risk

Too many partially-correct authority paths staying alive at once.

That is the failure mode to avoid more than "bad rendering" or "one more daemon."

## Recommended Direction

### Keep Investing In

- conductor daemon protocol
- terminal correctness for managed sessions
- daemon-owned semantic session state
- subscriber migration away from direct file-poller truth

### Treat As Transitional

- direct `ClaudeStatePoller`-based truth in UI/audio consumers
- hook/script-based state for managed sessions
- ad hoc orchestration semantics spread across multiple daemons

### Avoid Turning Into the Main Story

- compositor-specific integration as the product thesis
- cleanup efforts that do not respect current live wiring
- broad feature expansion before authority is consolidated

## Bottom Line

Thermal is already a serious local runtime experiment, not just a custom terminal toy.

The core strategic bet looks good:

- own the session/runtime layer
- make semantic state first-class
- let UI, audio, and orchestration subscribe to that truth

The main thing to guard against is split authority.

If the daemon becomes the single source of truth for managed sessions, Thermal can become meaningfully better than a Kitty-based orchestration stack even if Kitty remains more mature as a generic terminal.

That is the right battle to pick.
