# Integrated Multi-User Implementation Roadmap

**Task:** mu-plan-integration
**Date:** 2026-03-25
**Inputs:**
- [Server-Side Multi-User Work Plan](server-side-multi-user-workplan.md) — 12 tasks
- [Web & Mobile Client Implementation Plan](../design/client-implementation-plan.md) — 12 tasks
- [Federation & Distributed Sync](federation-and-distributed-sync.md) — 12 tasks
- [TUI Liveness & Monitoring UX](tui-liveness-monitoring-workplan.md) — 8 tasks

---

## Executive Summary

**44 total tasks** across 4 work plans, organized into **4 delivery phases**. Each phase ships standalone value.

| Phase | Goal | Tasks | Effort (task-agent units) | Wall-clock (4 agents) |
|-------|------|-------|---------------------------|----------------------|
| **1: Multi-User MVP** | 2-7 users, shared VPS, safe concurrency, live TUI | 20 | 20-28 | ~2-3 weeks |
| **2: Web & Mobile Access** | Access from any device via browser, phone, tablet | 12 | 10-14 | ~1-2 weeks |
| **3: Observable Federation** | See peer workgraph state from CLI and TUI | 6 | 5-7 | ~1 week |
| **4: Interactive Federation + Polish** | Cross-repo dispatch, event-driven liveness, multi-machine | 6 | 12-18 | ~2-3 weeks |

**Total: 44 tasks, ~47-67 task-agent units, ~6-9 weeks wall-clock with 4 parallel agents.**

---

## Cross-Plan Dependency Map

Key dependencies resolved between plans:

```
SERVER                     LIVENESS                  CLIENTS                FEDERATION
──────                     ────────                  ───────                ──────────
S-T1: modify_graph P1 ─┐
S-T2: modify_graph P2 ─┤
                        └→ S-T9: fs watcher ──────→ (validates multi-TUI)
S-T3: registry lock     (independent)
S-T4: WG_USER ─────────┬→ S-T5: per-user coord
                        ├→ S-T8: chat user field
                        ├→ S-T10: server init ────→ C-T12: deploy script
                        └→ L-T7: presence (P2)

S-T10: server init ────→ S-T11: tmux mgmt ───────→ (enables C-T8 dispatcher)

S-T12: liveness HUD ───┤  (overlaps with L-T1)
                        └→ (feed into liveness MVP)

L-T1-5: liveness MVP    (NO server deps — can start immediately)
L-T6: event bus client ←── requires server event-bus infrastructure
L-T7: presence ←─────────── requires server presence-protocol
L-T8: surveillance ←──────── requires L-T6 + L-T7

C-T1: responsive TUI ──→ C-T2: single-panel nav
C-T3: ttyd guide ───────→ C-T4: PWA → C-T5: xterm.js validation
C-T8: connect dispatcher ─→ C-T11: resilience testing, C-T12: deploy script

F-T1-6: observable fed   (NO blocking server deps for Phase 1)
F-T7-9: interactive fed ←── requires stable coordinator tick + event bus
F-T10: TCP+TLS ←─────────── requires daemon architecture changes
```

### Resolved Conflicts

1. **Server T12 (Liveness HUD) overlaps with Liveness T1 (HUD Vitals Bar)** — Merge into single task. Liveness T1 is more detailed; use it, drop Server T12.
2. **Server T10 (server init) vs Client T12 (unified deploy script)** — Sequential: server init first (core), then deploy script wraps it with ttyd/mosh/Caddy.
3. **Federation Phase 2 needs server event bus** — which is Liveness T6's dependency too. Server event-bus becomes a shared prerequisite for Phase 4.
4. **Client responsive TUI benefits all platforms** — Pull into Phase 1 since it enables mobile testing early.

---

## Phase 1: Multi-User MVP

**Goal:** 2-7 users on a shared VPS, each running their own `wg tui`, safe concurrent graph access, live monitoring dashboard.

**Entry criteria:** None — can start immediately.
**Exit criteria:** Multiple users can `ssh` to a VPS, run `wg tui`, see each other's effects in <100ms, with a live HUD showing system health.

### Phase 1 Tasks (20 tasks)

Tasks are grouped into parallel streams. Within a stream, tasks are sequential. Streams can run concurrently.

#### Stream A: Concurrent Safety (Critical Path)

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-s-modify-graph-p1` | Complete modify_graph() migration — Phase 1 (core commands) | Server T1 | L | — | ~15 cmd files |
| `mu-s-modify-graph-p2` | Complete modify_graph() migration — Phase 2 (remaining) | Server T2 | L | mu-s-modify-graph-p1 | ~35 files |
| `mu-s-fs-watcher-validation` | Validate fs watcher for multi-user | Server T9 | S | mu-s-modify-graph-p2 | 2 files |

#### Stream B: Identity & User Infrastructure

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-s-registry-lock` | Agent registry universal load_locked() | Server T3 | S | — | 3 files |
| `mu-s-wg-user` | WG_USER identity system | Server T4 | M | — | 6 files |
| `mu-s-per-user-coord` | Per-user coordinator creation | Server T5 | S | mu-s-wg-user | 3 files |
| `mu-s-per-coord-state` | Per-coordinator state files | Server T6 | S | mu-s-per-user-coord | 3 files |
| `mu-s-chat-flock` | Chat inbox flock protection | Server T7 | S | — | 1 file |
| `mu-s-chat-user` | Chat message user field | Server T8 | S | mu-s-wg-user | 2 files |

#### Stream C: Liveness MVP (No Server Dependency)

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-l-hud-vitals` | HUD vitals bar | Liveness T1 | S | — | 3 files (render.rs, state.rs) |
| `mu-l-activity-feed` | Activity feed panel | Liveness T2 | M | — | 3-4 files |
| `mu-l-agent-dashboard` | Agent dashboard tab | Liveness T3 | M | — | 4 files |
| `mu-l-toasts` | Enhanced toast notifications | Liveness T4 | S | — | 4 files |
| `mu-l-drilldown` | Drill-down navigation | Liveness T5 | M | mu-l-agent-dashboard | 3 files |

#### Stream D: Responsive TUI

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-c-responsive-breakpoints` | Responsive TUI breakpoints | Client T1 | M | — | 2 files (render.rs, state.rs) |
| `mu-c-single-panel-nav` | Single-panel navigation mode | Client T2 | M | mu-c-responsive-breakpoints | 2 files |

#### Stream E: Server Setup

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-s-server-init` | wg server init automation | Server T10 | M | mu-s-wg-user | 3 new files |
| `mu-s-tmux-mgmt` | tmux session management | Server T11 | S | mu-s-server-init | 1-2 files |
| `mu-c-connect-dispatcher` | Server-side connection dispatcher | Client T8 | S | — | 1 file (script) |
| `mu-c-mosh-guide` | mosh server configuration guide | Client T9 | S | — | 1 file (docs) |

### Phase 1 Parallelism Schedule

```
Week 1:
  Agent 1: mu-s-modify-graph-p1 (L) ─────────────────────────┐
  Agent 2: mu-s-wg-user (M) → mu-s-per-user-coord (S)        │
  Agent 3: mu-l-hud-vitals (S) + mu-l-toasts (S) parallel    │
  Agent 4: mu-c-responsive-breakpoints (M)                    │
                                                               │
Week 2:                                                        │
  Agent 1: mu-s-modify-graph-p2 (L) ←─────────────────────────┘
  Agent 2: mu-s-per-coord-state (S) → mu-s-chat-user (S) → mu-s-server-init (M)
  Agent 3: mu-l-activity-feed (M) → mu-l-agent-dashboard (M)
  Agent 4: mu-c-single-panel-nav (M) → mu-c-connect-dispatcher (S) + mu-c-mosh-guide (S)

Week 3:
  Agent 1: mu-s-fs-watcher-validation (S) ← (after modify_graph done)
  Agent 2: mu-s-tmux-mgmt (S) + mu-s-registry-lock (S) + mu-s-chat-flock (S)
  Agent 3: mu-l-drilldown (M) ← (after dashboard)
  Agent 4: (available for overflow/fixes)
```

### Phase 1 Critical Path

```
mu-s-modify-graph-p1 (L, ~3 units)
  → mu-s-modify-graph-p2 (L, ~3 units)
    → mu-s-fs-watcher-validation (S, ~1 unit)

Total critical path: ~7 task-agent units = ~2.5 weeks with parallel streams
```

---

## Phase 2: Web & Mobile Access

**Goal:** Access workgraph from any device — browser, Android, iOS.
**Entry criteria:** Phase 1 server init and responsive TUI complete.
**Exit criteria:** Users can connect from Chrome, Termux, Blink Shell with reconnection resilience.

### Phase 2 Tasks (12 tasks)

#### Stream F: Web Access

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-c-ttyd-guide` | ttyd deployment guide + configuration | Client T3 | S | mu-s-server-init | docs |
| `mu-c-pwa` | PWA manifest + service worker | Client T4 | S | mu-c-ttyd-guide | assets/pwa/ |
| `mu-c-xterm-validation` | xterm.js TUI rendering validation | Client T5 | S | mu-c-ttyd-guide | tui/ + docs |

#### Stream G: Mobile Access

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-c-termux-setup` | Termux setup script + guide | Client T6 | S | mu-c-responsive-breakpoints | scripts/ + docs |
| `mu-c-blink-guide` | Blink Shell configuration guide (iOS) | Client T7 | S | — | docs |

#### Stream H: Integration & Testing

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-c-distribution` | Distribution & hosting strategy | Client T10 | S | mu-c-ttyd-guide,mu-c-termux-setup,mu-c-blink-guide | docs |
| `mu-c-resilience-testing` | Connection resilience testing suite | Client T11 | M | mu-c-ttyd-guide,mu-c-connect-dispatcher,mu-c-mosh-guide | tests/ + docs |
| `mu-c-deploy-script` | Unified server deployment script | Client T12 | M | mu-c-ttyd-guide,mu-c-connect-dispatcher,mu-c-mosh-guide,mu-s-server-init | scripts/ + docs |

### Phase 2 Parallelism Schedule

```
Week 4:
  Agent 1: mu-c-ttyd-guide (S) → mu-c-pwa (S) → mu-c-xterm-validation (S)
  Agent 2: mu-c-termux-setup (S) → mu-c-blink-guide (S) → mu-c-distribution (S)
  Agent 3: mu-c-resilience-testing (M) ← (after ttyd + dispatcher + mosh guides)
  Agent 4: mu-c-deploy-script (M) ← (after all platform guides)
```

### Phase 2 Critical Path

```
mu-s-server-init (Phase 1)
  → mu-c-ttyd-guide (S)
    → mu-c-deploy-script (M) ← also needs dispatcher + mosh

Total: ~3 task-agent units on critical path (much is parallel docs work)
```

---

## Phase 3: Observable Federation

**Goal:** See peer workgraph state from CLI and TUI. Same-machine. Read-only.
**Entry criteria:** Phase 1 IPC infrastructure stable.
**Exit criteria:** `wg peer scan`, `wg peer tasks`, and TUI Peers tab all work.

### Phase 3 Tasks (6 tasks)

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-f-querygraph-ipc` | QueryGraph IPC request + visibility filtering | Federation T1 | S | — | ipc.rs, graph.rs |
| `mu-f-federation-config` | Federation config in config.toml | Federation T2 | S | — | config.rs |
| `mu-f-peer-scan` | wg peer scan command | Federation T3 | S | — | peer.rs |
| `mu-f-peer-tasks` | wg peer tasks CLI command | Federation T4 | S | mu-f-querygraph-ipc | peer.rs |
| `mu-f-snapshot-cache` | Snapshot caching | Federation T5 | S | mu-f-querygraph-ipc | federation.rs |
| `mu-f-tui-peers-tab` | TUI Peers tab (list + drill-down) | Federation T6 | M | mu-f-querygraph-ipc,mu-f-federation-config,mu-f-snapshot-cache | tui/viz_viewer/ |

### Phase 3 Parallelism Schedule

```
Week 5:
  Agent 1: mu-f-querygraph-ipc (S) → mu-f-peer-tasks (S)
  Agent 2: mu-f-federation-config (S) + mu-f-peer-scan (S) → mu-f-snapshot-cache (S)
  Agent 3: (after deps) mu-f-tui-peers-tab (M)
```

### Phase 3 Critical Path

```
mu-f-querygraph-ipc (S)
  → mu-f-snapshot-cache (S)
    → mu-f-tui-peers-tab (M)

Total: ~4 task-agent units
```

---

## Phase 4: Interactive Federation + Advanced Liveness

**Goal:** Cross-repo task dispatch, event-driven updates, multi-machine federation.
**Entry criteria:** Phase 3 complete. Server event-bus infrastructure built.

### Phase 4 Tasks (6 tasks — most impactful subset)

The remaining tasks from federation and liveness are large and independent enough to be sequenced based on value. The full set from the source plans includes 12 more tasks (Federation T7-12, Liveness T6-8). The highest-value subset for Phase 4:

| ID | Title | Source | Complexity | After | Files |
|----|-------|--------|------------|-------|-------|
| `mu-f-cross-repo-dispatch` | Cross-repo task dispatch (--repo flag) | Federation T7 | M | mu-f-tui-peers-tab | add.rs, ipc.rs |
| `mu-f-cross-repo-deps` | Cross-repo dependencies (peer:task-id) | Federation T8 | M | mu-f-cross-repo-dispatch | graph.rs, coordinator.rs |
| `mu-f-push-notify` | Push notifications between peers | Federation T9 | S | mu-f-tui-peers-tab | ipc.rs, federation.rs |
| `mu-l-event-bus-client` | Event bus TUI client | Liveness T6 | M | mu-l-agent-dashboard | tui state.rs, event.rs |
| `mu-l-presence` | Presence indicators | Liveness T7 | S | mu-l-event-bus-client,mu-s-wg-user | tui render.rs |
| `mu-l-surveillance` | Surveillance view | Liveness T8 | M | mu-l-event-bus-client,mu-l-presence | tui render.rs, event.rs |

### Deferred to Phase 5+ (when demand justifies)

| ID | Title | Source | Complexity | Notes |
|----|-------|--------|------------|-------|
| `mu-f-tcp-tls` | TCP IPC transport with TLS | Federation T10 | L | Enables multi-machine federation |
| `mu-f-git-sync` | Git merge driver + wg sync | Federation T11 | L | Async collaboration via git |
| `mu-f-service-discovery` | Service announcement (mDNS/registry) | Federation T12 | M | Auto-discovery for TCP peers |

---

## Global Critical Path

The end-to-end critical path through all 4 phases:

```
mu-s-modify-graph-p1 (L, ~3 units)
  → mu-s-modify-graph-p2 (L, ~3 units)
    → mu-s-fs-watcher-validation (S, ~1 unit)
      ─── Phase 1 complete ───
        → mu-c-ttyd-guide (S, ~1 unit)
          → mu-c-deploy-script (M, ~2 units)
            ─── Phase 2 complete ───
              → mu-f-querygraph-ipc (S, ~1 unit)
                → mu-f-snapshot-cache (S, ~1 unit)
                  → mu-f-tui-peers-tab (M, ~2 units)
                    ─── Phase 3 complete ───
                      → mu-f-cross-repo-dispatch (M, ~2 units)
                        → mu-f-cross-repo-deps (M, ~2 units)
                          ─── Phase 4 complete ───

Critical path total: ~18 task-agent units
```

However, most work runs **off the critical path** in parallel streams. With 4 agents, the modify_graph migration (Streams A) runs alongside identity work (Stream B), liveness (Stream C), and responsive TUI (Stream D).

---

## Total Scope Estimate

| Phase | Tasks | Effort Range | Wall-Clock (4 agents) |
|-------|-------|-------------|----------------------|
| Phase 1 | 20 | 20-28 units | 2-3 weeks |
| Phase 2 | 12 | 10-14 units | 1-2 weeks |
| Phase 3 | 6 | 5-7 units | ~1 week |
| Phase 4 | 6 | 8-12 units | 1-2 weeks |
| **Total** | **44** | **43-61 units** | **5-8 weeks** |

**Deferred (Phase 5+):** 3 tasks, 10-14 additional units.

### Complexity Distribution

| Complexity | Count | % |
|-----------|-------|---|
| S (Small) | 25 | 57% |
| M (Medium) | 15 | 34% |
| L (Large) | 2 | 5% |
| Docs-only | 5 | 11% |

---

## Phase 1: Ready-to-Create Task Descriptions

Below are concrete `wg add` commands for every Phase 1 task. Copy-paste ready.

### Stream A: Concurrent Safety

```bash
wg add "Complete modify_graph() migration — Phase 1 (core commands)" \
  --id mu-s-modify-graph-p1 \
  --verify "cargo test passes; no save_graph() calls remain in: log.rs, artifact.rs, claim.rs, add.rs, edit.rs, link.rs, abandon.rs, reject.rs, approve.rs, assign.rs, msg.rs" \
  -d "## Description
Migrate the highest-traffic mutation paths from load_graph()/save_graph() to modify_graph() for safe concurrent access. This is the critical foundation for multi-user.

Target files (~15 command files):
- src/commands/log.rs — agents call wg log constantly
- src/commands/artifact.rs — agents register artifacts frequently
- src/commands/claim.rs — concurrent agent task claims
- src/commands/add.rs — concurrent task creation
- src/commands/edit.rs — task description edits
- src/commands/link.rs — dependency modifications
- src/commands/abandon.rs — task abandonment
- src/commands/reject.rs, approve.rs — validation workflow
- src/commands/assign.rs — agent assignment
- src/commands/msg.rs — message sending

Each command has unique mutation logic; no one-size-fits-all conversion. Some commands do multiple loads/saves — determine whether they should be a single modify_graph() or multiple.

## Validation
- [ ] All listed files migrated from save_graph() to modify_graph()
- [ ] Unit test: modify_graph() closure correctly applies each mutation type
- [ ] Integration test: two concurrent mutations don't lose updates
- [ ] Stress test: 5 parallel wg log invocations, verify no lost entries after 100 writes
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Complete modify_graph() migration — Phase 2 (remaining commands)" \
  --id mu-s-modify-graph-p2 \
  --after mu-s-modify-graph-p1 \
  --verify "cargo test passes; grep -r 'save_graph' src/ shows only test helpers, no production mutation paths" \
  -d "## Description
Migrate ALL remaining production save_graph() call sites to modify_graph(). This completes the concurrent safety foundation.

Target files (~35 files):
- src/commands/exec.rs (8 call sites — complex, shell execution lifecycle)
- src/commands/status.rs (10 occurrences — batch status changes)
- src/commands/agent.rs (11 occurrences — agent CRUD lifecycle)
- src/commands/service/ipc.rs (9 occurrences — daemon-side mutations)
- src/commands/service/coordinator.rs (verify completeness)
- src/commands/service/triage.rs, service/coordinator_agent.rs
- src/commands/spawn/execution.rs, spawn/mod.rs
- src/tui/viz_viewer/event.rs, state.rs (TUI-initiated mutations)
- src/executor/native/tools/wg.rs (native executor tool calls)
- src/federation.rs (federation-initiated writes)
- src/matrix_commands.rs (Matrix bot mutations)
- All remaining: sweep, gc, reclaim, retry, reschedule, checkpoint, plan, coordinate, etc.

Risk: IPC handler mutations happen inside daemon — verify flock doesn't deadlock. TUI mutations in async context — modify_graph() must not block event loop.

## Validation
- [ ] Each migrated command gets a test verifying idempotent behavior under modify_graph()
- [ ] Full cargo test passes
- [ ] Concurrent stress test: extended version covering all mutation paths
- [ ] grep -r 'save_graph' src/ — only test helpers remain
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Validate fs watcher for multi-user" \
  --id mu-s-fs-watcher-validation \
  --after mu-s-modify-graph-p2 \
  --verify "cargo test test_multi_user_watcher passes OR validation document exists at docs/plans/fs-watcher-validation.md" \
  -d "## Description
Validate that the existing fs watcher (50ms debounce, inotify-based) works correctly with multiple concurrent TUI instances. Architecture doc states <100ms propagation for <=7 users.

This is a validation/hardening task, not new feature work:
- Verify multiple TUI instances all receive IN_MOVED_TO events from atomic renames
- Verify debounce doesn't cause missed updates under burst writes
- Verify inotify watch limit is sufficient (default 8192, each TUI adds ~5 watches)
- Document any edge cases or tuning parameters
- Fix any issues found

## Validation
- [ ] Automated test: spawn N file watchers, perform M atomic renames, verify all N receive all M events
- [ ] Manual test documented: 5 TUI instances, rapid wg log from CLI, all TUIs update within 100ms
- [ ] Any issues found are documented and/or fixed
- [ ] cargo build + cargo test pass with no regressions"
```

### Stream B: Identity & User Infrastructure

```bash
wg add "Agent registry universal load_locked()" \
  --id mu-s-registry-lock \
  --verify "cargo test passes; grep for unlocked registry save patterns in spawn/execution.rs and spawn/mod.rs returns zero matches" \
  -d "## Description
The agent registry has both locked and unlocked access patterns. Under multiple coordinators spawning agents concurrently, unlocked access races. Migrate all registry write paths to use load_locked().

Currently load_locked() is used in 11 files. Migrate unlocked load()/save() in:
- src/commands/spawn/execution.rs — agent spawn registration
- src/commands/spawn/mod.rs — spawn orchestration
- Any other registry writers

Audit for nested locks — potential deadlock if a codepath holds registry lock while acquiring graph lock. Document lock hierarchy: graph before registry, always.

## Validation
- [ ] All registry write paths use load_locked()
- [ ] Test: two concurrent LockedRegistry acquisitions — second blocks until first dropped
- [ ] Test: spawn two agents concurrently — both appear in registry
- [ ] Existing dead_agents, kill, heartbeat tests still pass
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "WG_USER identity system" \
  --id mu-s-wg-user \
  --verify "cargo test test_current_user passes; WG_USER env var is read in provenance, chat, and log paths" \
  -d "## Description
Implement WG_USER environment variable support. Fallback chain: WG_USER -> \$USER -> 'unknown'.

Create shared utility: fn current_user() -> String in src/lib.rs or new src/identity.rs.

Wire WG_USER into:
1. Provenance log (src/provenance.rs): user field in provenance entries
2. Task logs (src/commands/log.rs): user in log entry metadata
3. Chat messages (src/chat.rs): user field, display in TUI
4. Coordinator labels (src/commands/service/coordinator.rs): auto-label with creating user
5. Graph mutations (via modify_graph()): record mutating user in provenance

Schema evolution: adding user field must be backward-compatible (missing field -> None/default). Verify WG_USER propagates to spawned agent processes.

## Validation
- [ ] Unit test: current_user() returns WG_USER when set, \$USER when not, 'unknown' when neither
- [ ] Unit test: provenance entry includes user field
- [ ] Unit test: chat message serialization includes user field
- [ ] Integration test: set WG_USER=alice, run wg log, verify log entry attributes to alice
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Per-user coordinator creation" \
  --id mu-s-per-user-coord \
  --after mu-s-wg-user \
  --verify "cargo test test_per_user_coord passes OR per-user coordinator logic exists in coordinator.rs" \
  -d "## Description
When a user launches wg tui, auto-create a coordinator labeled with their WG_USER identity. Each user gets their own coordinator managing their own agent budget.

Changes:
- On TUI startup, if no coordinator exists for current WG_USER, prompt or auto-create one
- Coordinator state files: coordinator-state-{id}.json instead of shared coordinator-state.json
- Service daemon manages multiple coordinators with independent agent budgets

Backward compatibility: existing single coordinator-state.json must be migrated or handled as fallback. TUI tab bar already supports multiple coordinators — verify auto-creation integrates cleanly.

## Validation
- [ ] Test: TUI startup with WG_USER=alice creates coordinator labeled 'alice'
- [ ] Test: two coordinators (alice, bob) run simultaneously without state file conflicts
- [ ] Test: per-ID state files correctly read/written
- [ ] Backward compat: old single coordinator-state.json handled gracefully
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Per-coordinator state files" \
  --id mu-s-per-coord-state \
  --after mu-s-per-user-coord \
  --verify "cargo test passes; coordinator state uses per-ID files" \
  -d "## Description
Split coordinator-state.json into per-coordinator files (coordinator-state-{id}.json) to eliminate write contention between coordinators updating their own state.

Files: src/commands/service/coordinator.rs, src/commands/service/mod.rs, any code reading coordinator state.

Handle migration path: first run after upgrade converts old file to new format.

## Validation
- [ ] Test: two coordinators write state simultaneously without corruption
- [ ] Test: wg service status reads all per-coordinator state files correctly
- [ ] Test: migration from old single file to per-ID files
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Chat inbox flock protection" \
  --id mu-s-chat-flock \
  --verify "cargo test passes; flock used in chat.rs write operations" \
  -d "## Description
Add flock-based locking to chat inbox file operations (src/chat.rs). Two users sending messages simultaneously can race on the inbox file. Use flock (consistent with rest of system). Inbox files are small and contention is low.

## Validation
- [ ] Test: two concurrent chat message sends don't lose either message
- [ ] Test: reading inbox while another process writes doesn't see partial data
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Chat message user field" \
  --id mu-s-chat-user \
  --after mu-s-wg-user \
  --verify "cargo test passes; chat messages include user field" \
  -d "## Description
Add user field to chat message structs, populated from current_user(). Display user attribution in TUI chat panel. Additive schema change with serde defaults.

Files: src/chat.rs (message struct), TUI chat rendering (src/tui/viz_viewer/render.rs).

## Validation
- [ ] Test: chat message serialization roundtrips with user field
- [ ] Test: backward compat — old messages without user field deserialize to user: None
- [ ] Visual: TUI displays 'alice: message' format
- [ ] cargo build + cargo test pass with no regressions"
```

### Stream C: Liveness MVP

```bash
wg add "HUD vitals bar" \
  --id mu-l-hud-vitals \
  --verify "cargo test passes; TUI renders vitals bar with agent count, task counts, and time-since-last-event" \
  -d "## Description
Add an always-visible vitals strip to the TUI bottom status bar showing system health at a glance:

Wireframe:
\`\`\`
| ● 2 agents | 8 open · 3 running · 45 done | last event 4s ago | coord ● 3s |
\`\`\`

Indicators:
- Agent count (running): from AgentRegistry (already loaded)
- Task status counts: from Graph stats (already computed)
- Time since last event: operations.jsonl mtime (cheap syscall), relative display
- Coordinator heartbeat: coordinator-state.json last tick time

Color coding for time-since-last-event: green (<30s), yellow (30s-5m), red (>5m), or warning if daemon not running.

Files: src/tui/viz_viewer/state.rs (add last_event_time, vitals fields), src/tui/viz_viewer/render.rs (new render_vitals_bar(), reserve 1 row at bottom).

## Validation
- [ ] Unit test: vitals formatting for various durations
- [ ] Unit test: vitals bar renders correctly with 0, 1, N agents
- [ ] Integration test: TUI screen dump includes vitals bar content
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Activity feed panel" \
  --id mu-l-activity-feed \
  --verify "cargo test passes; activity feed parses operations.jsonl into typed ActivityEvent structs" \
  -d "## Description
Transform the CoordLog tab from raw daemon log lines into a semantic activity feed. Parse operations.jsonl into typed ActivityEvent structs. Color-coded, icon-prefixed stream.

Event types: task created (+/blue), status change (→/yellow), agent spawned (▶/green), agent completed (✓/green bold), agent failed (✗/red bold), coordinator tick (⟳/dim), verification result, user action.

Implementation: On fs watcher trigger, read new lines appended since last read position. Format and append to ring buffer (500 entries max). Auto-tail with scroll-up to pause.

Files: src/tui/viz_viewer/state.rs (ActivityEvent struct, activity_feed VecDeque, tail position), src/tui/viz_viewer/render.rs (render_activity_feed).

## Validation
- [ ] Unit test: ActivityEvent parsing from provenance log lines (all event types)
- [ ] Unit test: ring buffer behavior (overflow, auto-tail, scroll pause)
- [ ] Unit test: activity feed rendering (each event type produces expected styled line)
- [ ] Integration test: create task via CLI → activity feed shows it
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Agent dashboard tab" \
  --id mu-l-agent-dashboard \
  --verify "cargo test passes; Dashboard tab shows coordinator cards, agent table, graph summary" \
  -d "## Description
Dedicated dashboard view: all running agents, their tasks, elapsed time, token usage, status. The operational nerve center.

Layout: Coordinator cards (state from coordinator-state.json), Agent table (from AgentRegistry + per-agent output mtime), Graph summary (from Graph), Activity sparkline.

Agent status logic: active (output <30s), slow (30s-5m), stuck (>5m), exited.

Add RightPanelTab::Dashboard variant. Keybindings: Enter (drill-down), k (kill), t (task detail), b (back).

Files: src/tui/viz_viewer/state.rs, render.rs, event.rs.

## Validation
- [ ] Unit test: agent status classification (active/slow/stuck thresholds)
- [ ] Unit test: dashboard rendering with 0, 1, many agents and coordinators
- [ ] Unit test: sparkline computation from event timestamps
- [ ] Integration test: screen dump with dashboard tab active
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Enhanced toast notifications" \
  --id mu-l-toasts \
  --verify "cargo test passes; toast system supports info/warning/error severity with auto-dismiss" \
  -d "## Description
Upgrade notification system to severity-leveled toasts. Info (green, 5s auto-dismiss), Warning (yellow, 10s auto-dismiss), Error (red, until Esc dismissed).

Phase 1 triggers (on graph reload diff):
- Task done → Info, Task failed → Error, Agent exited → Info with duration, Agent stuck (>5m) → Warning (deduplicated), New message → Info.

Replace notification: Option<(String, Instant)> with toasts: Vec<Toast>. Render stacked in top-right corner. Max 4 visible.

Files: src/tui/viz_viewer/state.rs, render.rs, event.rs.

## Validation
- [ ] Unit test: toast lifecycle (creation, auto-expiry by severity, manual dismissal)
- [ ] Unit test: toast deduplication
- [ ] Unit test: toast rendering (multiple toasts stack, color per severity)
- [ ] Integration test: fail a task → error toast appears
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Drill-down navigation" \
  --id mu-l-drilldown \
  --after mu-l-agent-dashboard \
  --verify "cargo test passes; NavStack push/pop works through Dashboard → Agent → Task → Log chain" \
  -d "## Description
Navigation chain: Dashboard → select agent → agent output → task detail → task logs. Each level more detail, Esc/b to go back.

NavStack model: Vec<NavEntry> where NavEntry is Dashboard/AgentDetail/TaskDetail/TaskLog. Push on Enter, pop on Esc/b.

This is purely TUI-side navigation — no server interaction. Ties dashboard to existing detail views.

Files: src/tui/viz_viewer/state.rs (NavStack), event.rs (Enter/Esc bindings), event.rs (set agent filter/selected task on drill).

## Validation
- [ ] Unit test: NavStack push/pop behavior, empty stack Esc does nothing
- [ ] Unit test: drill-down from dashboard agent row sets correct tab + filter
- [ ] Integration test: screen dump sequence through drill-down chain
- [ ] cargo build + cargo test pass with no regressions"
```

### Stream D: Responsive TUI

```bash
wg add "Responsive TUI breakpoints" \
  --id mu-c-responsive-breakpoints \
  --verify "cargo test test_responsive_ passes; TUI renders without panic at 40-col width" \
  -d "## Description
Implement responsive layout breakpoints in the TUI renderer:
- < 50 cols: Single-panel mode — show graph OR detail, not both. Tab/key to switch.
- 50-80 cols: Narrow split — graph list (left), compact detail (right). Hide non-essential columns.
- > 80 cols: Current full layout (no change).

Detect terminal resize events (SIGWINCH) and switch layouts dynamically. Critical for mobile (Termux, Blink Shell).

Files: src/tui/viz_viewer/render.rs, src/tui/viz_viewer/state.rs.

## Validation
- [ ] Unit tests: render to virtual terminal at various widths (40, 60, 100 cols), assert layout invariants
- [ ] Manual: wg tui in terminals resized to phone-like dimensions (40x25, 50x30)
- [ ] TUI renders without panic at 40-col width
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Single-panel navigation mode" \
  --id mu-c-single-panel-nav \
  --after mu-c-responsive-breakpoints \
  --verify "cargo test test_single_panel_ passes" \
  -d "## Description
When in single-panel mode (< 50 cols), implement panel-switching navigation:
- Tab or ]/[ to cycle between: graph list → task detail → log/output → back
- Breadcrumb or header indicator showing current panel
- All existing keybindings work within each panel
- Panel state persists across switches (cursor position, scroll offset)

Files: src/tui/viz_viewer/state.rs, src/tui/viz_viewer/render.rs.

## Validation
- [ ] Unit tests: simulate key events in single-panel mode, verify panel transitions
- [ ] Manual: use in 40-col terminal, verify all panels reachable and functional
- [ ] Panel state persists across switches
- [ ] cargo build + cargo test pass with no regressions"
```

### Stream E: Server Setup

```bash
wg add "wg server init automation" \
  --id mu-s-server-init \
  --after mu-s-wg-user \
  --verify "cargo test passes; wg server init --dry-run prints expected commands without executing" \
  -d "## Description
Create wg server init command that automates multi-user server setup:

1. Check/install prerequisites: tmux, ttyd (optional), caddy (optional)
2. Create Unix group for project (e.g., wg-<project>)
3. Set directory permissions: .workgraph/ owned by project group, 0770
4. Set file permissions: graph.jsonl 0660, daemon.sock 0660
5. Generate per-user shell profile snippet: export WG_USER=<name>
6. Generate tmux launch command
7. Optionally generate ttyd + Caddy config for web access
8. Print summary of what was configured

Use --dry-run by default, require --apply for actual changes. Handle both fresh installs and upgrades.

Files: new src/commands/server.rs, src/commands/mod.rs, src/cli.rs.

## Validation
- [ ] wg server init --dry-run prints expected commands without executing
- [ ] Validates prerequisites are installed
- [ ] Generated shell snippet sets correct env vars
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "tmux session management" \
  --id mu-s-tmux-mgmt \
  --after mu-s-server-init \
  --verify "cargo test passes; wg server connect creates or attaches to tmux session" \
  -d "## Description
Integrate tmux session management:
- wg server connect [user] — creates or attaches to user's tmux session (\${WG_USER}-wg)
- Verify TUI works correctly inside tmux
- Ensure wg tui detects it's inside tmux and adjusts behavior if needed

Graceful error with install instructions if tmux not installed.

Files: new subcommand in src/commands/server.rs.

## Validation
- [ ] wg server connect creates tmux session with correct name
- [ ] Re-running reattaches to existing session
- [ ] WG_USER correctly propagated inside tmux session
- [ ] cargo build + cargo test pass with no regressions"
```

```bash
wg add "Server-side connection dispatcher script" \
  --id mu-c-connect-dispatcher \
  --verify "scripts/wg-connect.sh exists and is executable" \
  -d "## Description
Create wg-connect.sh dispatcher script that all transports use:
- Determines WG_USER from SSH user / ttyd auth / env var
- Runs: tmux new-session -A -s \"\${WG_USER:-\$USER}-wg\" \"wg tui\"
- Ensures consistent session naming across all platforms
- Handles first-run: if wg binary not found, prints setup instructions
- Used by: ttyd launch command, SSH ForceCommand, mosh connection command

Files: scripts/wg-connect.sh, referenced in all platform guides.

## Validation
- [ ] Script is executable and handles WG_USER correctly
- [ ] Idempotent: run twice → attaches to existing session
- [ ] Missing wg binary → helpful error message"
```

```bash
wg add "mosh server configuration guide" \
  --id mu-c-mosh-guide \
  --verify "docs/guides/server-setup.md exists with mosh configuration section" \
  -d "## Description
Document server-side mosh setup:
- Install mosh-server on shared VPS
- Firewall: open UDP 60000-61000
- Systemd configuration
- Performance tuning: MOSH_PREDICTION_DISPLAY settings
- Security model documentation (AES-128-OCB)
- Integration with wg-connect.sh

Files: docs/guides/server-setup.md (new or addendum).

## Validation
- [ ] Guide covers installation, firewall, systemd, performance tuning
- [ ] Security model documented
- [ ] All commands in guide are correct and tested"
```

### Phase 2 Task Descriptions

```bash
# Stream F: Web Access
wg add "ttyd deployment guide + configuration" \
  --id mu-c-ttyd-guide \
  --after mu-s-server-init \
  --verify "docs/guides/web-access.md exists with minimal, production, and OAuth2 setup sections" \
  -d "## Description
Write deployment documentation for web access:
- Minimal setup (LAN, no auth): single ttyd command
- Production setup: Caddy reverse proxy + TLS + basic auth
- OAuth2 setup: Caddy + OAuth2 Proxy → GitHub/Google provider
- Multi-user: ttyd session management per authenticated user
- Systemd unit files for ttyd and Caddy
- Troubleshooting: xterm.js rendering quirks, WebSocket timeout tuning

## Validation
- [ ] Guide tested on fresh Ubuntu 24.04 VPS
- [ ] wg tui loads in Chrome, Firefox, Safari
- [ ] Tab close + reopen → tmux session reattaches
- [ ] Unauthenticated requests rejected (production setup)"

wg add "PWA manifest + service worker" \
  --id mu-c-pwa \
  --after mu-c-ttyd-guide \
  --verify "assets/pwa/manifest.json exists with standalone display mode" \
  -d "## Description
Create PWA assets for 'Add to Home Screen' on mobile:
- manifest.json with display: standalone, app name, theme color, icons
- App icons at 192x192 and 512x512
- Minimal service worker: cache shell, show 'Reconnecting...' offline
- Instructions for hosting alongside ttyd

## Validation
- [ ] Add to Home Screen works on Android Chrome and iOS Safari
- [ ] Standalone mode launches without browser chrome
- [ ] Offline screen appears when server unreachable"

wg add "xterm.js TUI rendering validation" \
  --id mu-c-xterm-validation \
  --after mu-c-ttyd-guide \
  --verify "docs/guides/web-access.md has known-issues section for xterm.js" \
  -d "## Description
Systematically test TUI in xterm.js (ttyd's terminal emulator):
- Color rendering, box-drawing characters, Unicode
- Mouse support, keyboard shortcuts
- Resize behavior
- Document TERM/COLORTERM env var settings needed
- Fix any rendering differences vs native terminal

## Validation
- [ ] Visual comparison documented: native terminal vs xterm.js
- [ ] Tested on Chrome, Firefox, Safari
- [ ] Any rendering fixes committed"

# Stream G: Mobile Access
wg add "Termux setup script + guide" \
  --id mu-c-termux-setup \
  --after mu-c-responsive-breakpoints \
  --verify "scripts/wg-termux-setup.sh exists and docs/guides/android-access.md exists" \
  -d "## Description
Android onboarding:
- wg-termux-setup.sh: installs mosh, tmux, openssh; creates ~/.shortcuts/workgraph
- Guide: F-Droid install instructions (NOT Google Play)
- Termux:Widget setup for home screen shortcut
- Connection template: mosh user@server -- tmux new-session -A -s \$USER-wg 'wg tui'
- Troubleshooting: storage permissions, battery optimization, SSH key generation

## Validation
- [ ] Setup script runs on fresh Termux install
- [ ] Shortcut launches and connects
- [ ] mosh reconnection works (airplane mode toggle)"

wg add "Blink Shell configuration guide (iOS)" \
  --id mu-c-blink-guide \
  --verify "docs/guides/ios-access.md exists" \
  -d "## Description
iOS onboarding:
- Step-by-step Blink Shell host configuration (mosh + tmux command)
- SSH key setup
- Recommended Blink settings for TUI
- iOS background limitations and mosh reconnection behavior
- Alternative: web access via Safari for free
- Brief mention of iSH (free but slower)

## Validation
- [ ] Guide is complete and accurate
- [ ] Alternative (web fallback) documented"

# Stream H: Integration & Testing
wg add "Distribution & hosting strategy" \
  --id mu-c-distribution \
  --after mu-c-ttyd-guide,mu-c-termux-setup,mu-c-blink-guide \
  --verify "docs/guides/getting-started.md exists with cross-platform quickstart" \
  -d "## Description
Document how users get access on each platform. Cross-platform quickstart with platform detection. Link to all platform-specific guides. Include distribution table (server, desktop, web, Android, iOS).

## Validation
- [ ] Each platform path documented end-to-end
- [ ] All links and commands correct"

wg add "Connection resilience testing suite" \
  --id mu-c-resilience-testing \
  --after mu-c-ttyd-guide,mu-c-connect-dispatcher,mu-c-mosh-guide \
  --verify "tests/connection/ directory exists with test scripts" \
  -d "## Description
Systematic test suite for connection resilience:
- Script simulating: network drop, high latency, Wi-Fi→cellular switch
- Test matrix: [SSH, mosh, ttyd] × [network drop, high latency, IP change]
- Measure: reconnection time, state preservation
- Document results in connection resilience matrix

## Validation
- [ ] Test scripts exist and are runnable
- [ ] Results documented
- [ ] Any TUI state issues filed as bugs"

wg add "Unified server deployment script" \
  --id mu-c-deploy-script \
  --after mu-c-ttyd-guide,mu-c-connect-dispatcher,mu-c-mosh-guide,mu-s-server-init \
  --verify "scripts/wg-server-setup.sh exists and is executable" \
  -d "## Description
One-command server setup: installs workgraph binary, tmux, mosh-server, ttyd, Caddy. Generates Caddyfile, systemd units. Opens firewall ports. Creates wg-connect.sh. Prints per-platform connection instructions. Supports Ubuntu 22.04/24.04, Debian 12.

## Validation
- [ ] Runs on fresh VPS from supported OS
- [ ] All platforms can connect after setup
- [ ] Idempotent: run twice, no breakage"
```

### Phase 3 Task Descriptions

```bash
wg add "QueryGraph IPC request + visibility filtering" \
  --id mu-f-querygraph-ipc \
  --verify "cargo test test_query_graph passes; visibility filtering excludes internal tasks" \
  -d "## Description
Add QueryGraph IPC request type to daemon. Handler: loads graph, filters by visibility (peer/public), builds PeerGraphSnapshot with aggregate counts + filtered PeerTaskSummary list.

PeerTaskSummary strips fields per visibility matrix:
- peer: title, status, tags, description summary, role, cross-deps, verification, timestamps
- public: title, status, tags, first line of description, verification, timestamps

INVARIANT: Visibility filtering MUST happen server-side (in IPC handler), never client-side.

Files: src/commands/service/ipc.rs, src/graph.rs.

## Validation
- [ ] Unit test: visibility filtering excludes internal tasks
- [ ] Unit test: peer vs public produce different field sets
- [ ] Integration test: send QueryGraph over IPC, verify response structure
- [ ] Edge case: empty graph, all-internal graph
- [ ] cargo build + cargo test pass"

wg add "Federation config in config.toml" \
  --id mu-f-federation-config \
  --verify "cargo test passes; FederationConfig struct parsed from config.toml [federation] section" \
  -d "## Description
Add [federation] section to config.toml: name, owner, poll_interval_secs, snapshot_ttl_secs. Parse into FederationConfig struct. Sensible defaults for all fields.

Files: src/config.rs.

## Validation
- [ ] Unit test: parse with all fields, partial fields, and missing section
- [ ] Unit test: defaults correct when section missing
- [ ] cargo build + cargo test pass"

wg add "wg peer scan command" \
  --id mu-f-peer-scan \
  --verify "cargo test test_peer_scan passes" \
  -d "## Description
Walk directory tree looking for .workgraph/ directories. For each: report path, task count, service status. Offer wg peer add command to register.

Pattern: identical to existing wg agency scan implementation. Default scan root: current directory. Optional argument: wg peer scan ~/projects.

Files: src/commands/peer.rs.

## Validation
- [ ] Integration test: create temp dirs with .workgraph/graph.jsonl, verify discovery
- [ ] Edge case: nested .workgraph/ directories
- [ ] cargo build + cargo test pass"

wg add "wg peer tasks CLI command" \
  --id mu-f-peer-tasks \
  --after mu-f-querygraph-ipc \
  --verify "cargo test test_peer_tasks passes" \
  -d "## Description
New subcommand: wg peer tasks <peer-name>.

Resolution: peer name → path (via federation.yaml), check service running → QueryGraph IPC if yes, file-read fallback if no. Output: table with task ID, title, status, tags.

Files: src/commands/peer.rs.

## Validation
- [ ] Integration test: start peer service, run wg peer tasks, verify output
- [ ] Integration test: peer service not running, verify file-read fallback
- [ ] Edge case: peer path doesn't exist, no visible tasks
- [ ] cargo build + cargo test pass"

wg add "Snapshot caching" \
  --id mu-f-snapshot-cache \
  --after mu-f-querygraph-ipc \
  --verify "cargo test test_snapshot_cache passes" \
  -d "## Description
Cache PeerGraphSnapshot responses with configurable TTL (default 5s from config). Prevents hammering peers when TUI auto-refreshes.

Implementation: HashMap<String, (Instant, PeerGraphSnapshot)> with TTL eviction. Thread-safe via Arc<Mutex<>> or channel.

Files: src/federation.rs or new src/federation/cache.rs.

## Validation
- [ ] Unit test: cache hit within TTL returns cached snapshot
- [ ] Unit test: cache miss after TTL fetches fresh
- [ ] Unit test: cache respects configurable TTL
- [ ] cargo build + cargo test pass"

wg add "TUI Peers tab (list + drill-down)" \
  --id mu-f-tui-peers-tab \
  --after mu-f-querygraph-ipc,mu-f-federation-config,mu-f-snapshot-cache \
  --verify "cargo test passes; RightPanelTab::Peers variant exists" \
  -d "## Description
Add RightPanelTab::Peers to TUI. Two sub-views:

List view: all configured peers with aggregate counts, service status (●/○), last refresh timestamp. Auto-refreshes on poll interval. Only polls when Peers tab active.

Drill-down: Enter on peer shows visible tasks. Esc returns. r forces refresh.

Follow existing coordinator tab pattern. Background async task polls peers.

Files: src/tui/viz_viewer/state.rs, render.rs.

## Validation
- [ ] Manual: configure 2-3 peers, verify list view
- [ ] Manual: drill down, verify task list matches wg peer tasks output
- [ ] Polling stops when tab not active
- [ ] Unit test: render functions with mock PeerGraphSnapshot
- [ ] cargo build + cargo test pass"
```

---

## Appendix: Full Task Index

| # | ID | Title | Phase | Source | Complexity | After |
|---|-----|-------|-------|--------|------------|-------|
| 1 | mu-s-modify-graph-p1 | modify_graph() migration P1 | 1 | Server T1 | L | — |
| 2 | mu-s-modify-graph-p2 | modify_graph() migration P2 | 1 | Server T2 | L | #1 |
| 3 | mu-s-fs-watcher-validation | fs watcher multi-user validation | 1 | Server T9 | S | #2 |
| 4 | mu-s-registry-lock | Registry universal load_locked() | 1 | Server T3 | S | — |
| 5 | mu-s-wg-user | WG_USER identity system | 1 | Server T4 | M | — |
| 6 | mu-s-per-user-coord | Per-user coordinator creation | 1 | Server T5 | S | #5 |
| 7 | mu-s-per-coord-state | Per-coordinator state files | 1 | Server T6 | S | #6 |
| 8 | mu-s-chat-flock | Chat inbox flock protection | 1 | Server T7 | S | — |
| 9 | mu-s-chat-user | Chat message user field | 1 | Server T8 | S | #5 |
| 10 | mu-l-hud-vitals | HUD vitals bar | 1 | Liveness T1 | S | — |
| 11 | mu-l-activity-feed | Activity feed panel | 1 | Liveness T2 | M | — |
| 12 | mu-l-agent-dashboard | Agent dashboard tab | 1 | Liveness T3 | M | — |
| 13 | mu-l-toasts | Enhanced toast notifications | 1 | Liveness T4 | S | — |
| 14 | mu-l-drilldown | Drill-down navigation | 1 | Liveness T5 | M | #12 |
| 15 | mu-c-responsive-breakpoints | Responsive TUI breakpoints | 1 | Client T1 | M | — |
| 16 | mu-c-single-panel-nav | Single-panel navigation mode | 1 | Client T2 | M | #15 |
| 17 | mu-s-server-init | wg server init automation | 1 | Server T10 | M | #5 |
| 18 | mu-s-tmux-mgmt | tmux session management | 1 | Server T11 | S | #17 |
| 19 | mu-c-connect-dispatcher | Connection dispatcher script | 1 | Client T8 | S | — |
| 20 | mu-c-mosh-guide | mosh server config guide | 1 | Client T9 | S | — |
| 21 | mu-c-ttyd-guide | ttyd deployment guide | 2 | Client T3 | S | #17 |
| 22 | mu-c-pwa | PWA manifest + service worker | 2 | Client T4 | S | #21 |
| 23 | mu-c-xterm-validation | xterm.js TUI rendering validation | 2 | Client T5 | S | #21 |
| 24 | mu-c-termux-setup | Termux setup script + guide | 2 | Client T6 | S | #15 |
| 25 | mu-c-blink-guide | Blink Shell config guide (iOS) | 2 | Client T7 | S | — |
| 26 | mu-c-distribution | Distribution & hosting strategy | 2 | Client T10 | S | #21,#24,#25 |
| 27 | mu-c-resilience-testing | Connection resilience testing | 2 | Client T11 | M | #21,#19,#20 |
| 28 | mu-c-deploy-script | Unified server deployment script | 2 | Client T12 | M | #21,#19,#20,#17 |
| 29 | mu-f-querygraph-ipc | QueryGraph IPC + visibility filter | 3 | Fed T1 | S | — |
| 30 | mu-f-federation-config | Federation config in config.toml | 3 | Fed T2 | S | — |
| 31 | mu-f-peer-scan | wg peer scan command | 3 | Fed T3 | S | — |
| 32 | mu-f-peer-tasks | wg peer tasks CLI command | 3 | Fed T4 | S | #29 |
| 33 | mu-f-snapshot-cache | Snapshot caching | 3 | Fed T5 | S | #29 |
| 34 | mu-f-tui-peers-tab | TUI Peers tab | 3 | Fed T6 | M | #29,#30,#33 |
| 35 | mu-f-cross-repo-dispatch | Cross-repo task dispatch (--repo) | 4 | Fed T7 | M | #34 |
| 36 | mu-f-cross-repo-deps | Cross-repo dependencies | 4 | Fed T8 | M | #35 |
| 37 | mu-f-push-notify | Push notifications between peers | 4 | Fed T9 | S | #34 |
| 38 | mu-l-event-bus-client | Event bus TUI client | 4 | Liveness T6 | M | #12 |
| 39 | mu-l-presence | Presence indicators | 4 | Liveness T7 | S | #38,#5 |
| 40 | mu-l-surveillance | Surveillance view | 4 | Liveness T8 | M | #38,#39 |
| 41 | mu-f-tcp-tls | TCP IPC transport with TLS | 5+ | Fed T10 | L | #34 |
| 42 | mu-f-git-sync | Git merge driver + wg sync | 5+ | Fed T11 | L | — |
| 43 | mu-f-service-discovery | Service announcement (mDNS) | 5+ | Fed T12 | M | #41 |
| 44 | — | (Server T12 merged into mu-l-hud-vitals) | — | — | — | — |
