# Archive Review: Human-Agent Interaction, Interface Design, and Integration Work

**Date:** 2026-03-05
**Task:** review-archive-workgraph
**Sources:** `.workgraph/archive.jsonl` (213 tasks), `.workgraph/graph-archive-20260305.jsonl` (1545 tasks), 11 design/research documents

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Design Documents Catalog](#2-design-documents-catalog)
3. [Archived Task Clusters](#3-archived-task-clusters)
4. [Key Design Decisions](#4-key-design-decisions)
5. [Cross-Document Connections](#5-cross-document-connections)
6. [Open Questions and Unfinished Threads](#6-open-questions-and-unfinished-threads)
7. [Relevance Assessment](#7-relevance-assessment)

---

## 1. Executive Summary

Workgraph has accumulated substantial design work across three major domains of human-agent interaction:

1. **Messaging Infrastructure** (implemented): A file-based message queue (`messages/{task-id}.jsonl`), a coordinator chat protocol (`chat/inbox.jsonl` + `chat/outbox.jsonl`), message discipline (unread messages block `wg done`), and a TUI design for node-specific message threads.

2. **Agent Lifecycle Management** (partially implemented): A unified lifecycle state machine with 11 states, `wg wait` for agent-initiated parking, checkpointing for long-running tasks, message-triggered resurrection of completed tasks, and liveness detection for stuck agents.

3. **Integration and External Communication** (mostly design-phase): A `NotificationChannel` trait for multi-platform notifications (Telegram, Slack, Discord, email, SMS, etc.), cross-repo federation and task dispatch, Amplifier executor integration, and the "smooth integration" strategy for the nikete/VX exchange fork.

Across both archive files, **~80 tasks** are directly relevant to human-agent interaction design, with an additional **~250 tasks** tangentially related (touching messaging, lifecycle, or integration code). The most intensive design effort was the agent lifecycle committee (12+ tasks, 6 researchers across 2 rounds) and the human-in-the-loop notification system (15+ implementation tasks across 4 phases).

---

## 2. Design Documents Catalog

### 2.1 Human-in-the-Loop Channels

**Source:** `docs/research/human-in-the-loop-channels.md`
**Type:** Research report
**Status:** Complete
**Task:** `research-human-in` (done)

**Summary:** Comprehensive evaluation of 13 communication channels for bidirectional human-agent communication. Includes channel comparison matrix, voice capability matrix, Rust crate availability assessment, existing Matrix code audit, and a `NotificationChannel` trait design.

**Key decisions:**
- Telegram is the #1 priority channel (easy setup, `teloxide` crate, inline keyboards)
- Webhooks are universal glue (trivial to implement)
- Existing Matrix lite implementation is the stronger foundation; full SDK version likely broken (pins non-existent matrix-sdk 0.16)
- Signal and WhatsApp are not recommended
- 4-phase implementation roadmap: Trait + Telegram + Webhooks + Matrix refactor -> Email + Slack -> Discord + SMS -> Voice + Push

**Proposed architecture:** `NotificationChannel` trait with `send_text`, `send_rich`, `send_with_actions`, `listen` methods. `NotificationRouter` for event-type routing. Escalation chains (Telegram -> SMS -> Phone call).

**Implementation status:** Tasks were created for all 4 phases:
- Phase 1: `phase-1-implement-notificationchannel`, `phase-1-implement-telegram`, `phase-1-implement-webhook`, `phase-1-refactor-matrix` (all done in archive)
- Phase 2: `phase-2-email-slack` (done)
- Phase 3: `phase-3-discord-sms` (done)
- Phase 4: `phase-4-voice-calls` (done)

**Note:** These tasks are marked "done" in the archive but it's unclear how much was actually implemented vs. designed. The notification system code may need verification.

---

### 2.2 Agent Message Queue

**Source:** `docs/design/agent-message-queue.md`
**Type:** Design specification
**Status:** Implemented
**Tasks:** `research-agent-message` (done), `build-base-message` (done), `validate-agent-messaging` (done)

**Summary:** Complete design for a per-task message queue system. Three layers: producer API (`wg msg send`), message store (`.workgraph/messages/{task-id}.jsonl`), and consumer layer (executor adapters for Claude, Amplifier, Shell).

**Key decisions:**
- JSONL files per task, not inside the graph YAML (avoids contention, semantic mismatch)
- Per-agent read cursors in `.cursors/{agent-id}.{task-id}`
- Monotonic message IDs with `O_APPEND` for atomic writes
- Agent self-poll pattern as the universal consumption mechanism (works with ALL executors)
- Two-phase delivery: v1 = file-based polling, v2 = stream-json + named pipe for real-time injection
- `IpcRequest::SendMessage` for programmatic sending

**Implementation status:** Core messaging is implemented (`src/messages.rs`, `src/commands/msg.rs`). Agent prompt templates instruct agents to poll messages. The real-time injection (v2) with named pipes has not been implemented.

---

### 2.3 Coordinator Chat Protocol

**Source:** `docs/design/coordinator-chat-protocol.md`
**Type:** Design specification
**Status:** Implemented
**Tasks:** `sh-chat-protocol-design` (done), `sh-chat-storage` (done), `sh-instant-wakeup` (done), `sh-impl-chat-cli` (done)

**Summary:** IPC extension for user <-> coordinator communication. Separate from task messages - this is a dedicated conversational channel to the coordinator agent.

**Key decisions:**
- Separate `UserChat` IPC request type (not `SendMessage` - different semantics)
- Separate inbox/outbox JSONL files (`chat/inbox.jsonl`, `chat/outbox.jsonl`)
- `request_id` correlation for matching responses to requests (client-side generation: `chat-{unix_millis}-{random_suffix}`)
- Urgent wake mechanism bypasses settling delay for instant chat response
- Chat bypasses service pause (user-facing, shouldn't be ignored)
- Phase 1: stub coordinator response. Phase 2: persistent LLM agent processes chat

**Implementation status:** The `wg chat` command, chat storage, instant wake-up, and IPC types are implemented. Phase 1 (stub responses) is complete. Phase 2 (persistent coordinator agent processing chat) is a major open thread.

---

### 2.4 Message Discipline

**Source:** `docs/design/message-discipline-design.md`
**Type:** Design specification
**Status:** Implemented
**Task:** `design-message-discipline` (done)

**Summary:** Unread messages block task completion. When an agent calls `wg done`, the system checks for unread messages and rejects completion if any exist.

**Key decisions:**
- `wg done` blocks on unread messages (inserted between blocker check and converged check)
- `--force` flag for emergency bypass
- `wg fail` warns but does NOT block (failing agent is already in trouble)
- Humans calling `wg done` manually skip the check (no agent_id to determine unread)
- `message_discipline` config option (default: true) to disable per-project
- `count_unread()` helper in `messages.rs`

**Implementation status:** Implemented. Agents see this in their prompt instructions. The message check is in `src/commands/done.rs`.

---

### 2.5 Node-Specific Chat (TUI Per-Task Messaging)

**Source:** `docs/design/node-specific-chat-design.md`
**Type:** Design specification
**Status:** Design only (not implemented)
**Task:** `design-node-specific` (done)

**Summary:** Extends the TUI chat panel to be context-sensitive: when a task is selected, the chat panel shows that task's message thread instead of the coordinator chat.

**Key decisions:**
- Same physical panel, same keybindings - context changes based on task selection
- No new "Messages" tab - would fragment attention
- Tab header changes from "Chat" to task ID when selected
- In-memory cursor per task (TUI is a viewer, not a consumer - doesn't advance agent cursors)
- Completed tasks retain message history as read-only threads
- Coordinator does NOT automatically see task-level messages (separation of concerns)
- Color-coded sender labels: user=yellow, coordinator=cyan, agent=green

**Implementation status:** Design document complete. State model (`ChatPanelState`, `ChatContext` enum) and rendering logic specified. Not yet implemented in the TUI code.

---

### 2.6 Agent Lifecycle (Unified Design)

**Source:** `docs/design/agent-lifecycle.md`
**Type:** Committee consensus document (Round 2, 6 researchers)
**Status:** Partially implemented
**Tasks:** Committee tasks `committee-host-v2`, `committee-v2-researcher-{1..6}`, implementation phases `phase-4a` through `phase-6`

**Summary:** Unified model where agents can stop and restart without losing context, regardless of why they stopped. Three triggers (agent-initiated wait, coordinator-initiated kill, message on done task) all flow through one checkpoint->resume pipeline.

**Key decisions:**
- **Core insight (D1):** All lifecycle transitions through a stopped state use the same checkpoint->resume pipeline. The trigger differs; the resume path is identical.
- Executor-agnostic design: every mechanism must work across Claude CLI, native executor, Amplifier, and shell
- Stuck is a field (`stuck_since`), not a status
- Paused remains an orthogonal boolean flag
- Message-triggered resurrection: conditional reopen (if safe) vs. child task (if downstream running)
- Resurrection guards: max 5 per task, 60s cooldown, sender whitelist
- Hybrid checkpointing: agent-driven (explicit `wg checkpoint`) + coordinator-driven (auto, based on turn count/time)
- `SessionResume` trait for executor-specific session resume implementations
- Native executor is primary target (manages own conversation history, making resume first-class)

**Implementation status:**
- Phase 4 (`wg wait`, Waiting status, condition evaluation): Implemented via tasks `phase-4a-remove`, `phase-4b-implement`, `phase-4c-coordinator`
- Phase 5 (checkpointing): Implemented via `phase-5a-implement`, `phase-5b-coordinator`
- Phase 6 (message resurrection): Task `phase-6-message-triggered` done in archive
- The `SessionResume` trait and executor-specific implementations may need verification

---

### 2.7 Unified Lifecycle State Machine

**Source:** `docs/design/unified-lifecycle-state-machine.md`
**Type:** Research document (Committee researcher C1)
**Status:** Partially implemented

**Summary:** Formal state machine with 11 states, all valid transitions, and invalid/impossible transitions mapped. Detailed comparison with existing `Status` enum.

**Key decisions:**
- Split `Open` into `Ready` + `Draft` (eliminate overloaded semantics)
- Add `Waiting` status for agent-parked tasks
- Stuck as field, not status (task is logically InProgress from agent's perspective)
- `Resuming` as transient coordinator-internal state (not persisted)
- `Paused` stays as orthogonal flag
- `Blocked` (structural, automatic) vs `Waiting` (agent-initiated, voluntary) distinction
- AgentStatus gains `Parked` and `Stuck` variants
- Migration path: deserialize `"open"` as `Ready` for backward compatibility

**Implementation status:** The `Waiting` status and some transitions are implemented. The full `Open` -> `Ready`/`Draft` split has open questions about whether `Ready` should be computed or stored.

---

### 2.8 Amplifier Integration Proposal

**Source:** `docs/research/amplifier-integration-proposal.md`
**Type:** Research synthesis (3 research documents)
**Status:** Partially implemented

**Summary:** Analysis of three integration options: Amplifier as wg executor (Option A), wg as Amplifier bundle (Option B, status quo), or full bidirectional (Option C = A + B for free).

**Key decisions:**
- Core changes are small (~200 LOC): add `prompt_mode` to decouple stdin piping from executor type, add `{{model}}` template variable, always write `prompt.txt`
- Don't adopt Amplifier's bundle model for executor configs (over-engineering)
- If you do Option A at all, you get Option C for free
- Recommended: Do PRs 1-5 (small, backward-compatible executor model improvements) regardless of Amplifier commitment
- Risks: maintenance coupling with Microsoft's Amplifier, recursion depth control, user confusion with two orchestrators

**Implementation status:** The `amplifier` executor type exists (`wg config --coordinator-executor amplifier`). The prompt_mode decoupling and template variable improvements were partially addressed. The amplifier bundle (`amplifier-bundle-workgraph`) works externally.

---

### 2.9 Amplifier Context Transfer

**Source:** `docs/research/amplifier-context-transfer.md`
**Type:** Research analysis
**Status:** Partially addressed

**Summary:** Deep analysis of how wg constructs prompts vs. how Amplifier's bundle passes context. Identifies 7 gaps in wg's `build_task_context()`.

**Key recommendations:**
- R1: Include dependency titles and descriptions in context (not just artifacts and logs)
- R2: Validate `inputs` against upstream `artifacts` at spawn time
- R3: Allow artifact annotations (`--description`)
- R4: Make log entry count configurable (hardcoded `take(5)`)
- R5: Support artifact content inlining for small files
- R6: Include `verify` field in prompt (agents don't know what "done" means)
- R7: Add `{{task_inputs}}` and `{{task_deliverables}}` template variables

**Implementation status:** R6 (verify field) has been addressed. R1 (dependency metadata) may have been partially addressed. Others appear to be open.

---

### 2.10 Cross-Repo Communication

**Source:** `docs/design/cross-repo-communication.md`
**Type:** Design specification
**Status:** Partially implemented

**Summary:** Enable workgraph instances across repositories to dispatch tasks, share dependencies, observe state, and share trace functions.

**Key decisions:**
- Extend `federation.yaml` with `peers` section (alongside existing agency `remotes`)
- `wg peer add|remove|list|show|status` commands
- `--repo` flag on `wg add` for cross-repo task dispatch
- `peer:task-id` namespace syntax for cross-repo references
- `AddTask` and `QueryTask` IPC request types
- Polling-based dependency resolution (not push-based) for simplicity
- Graceful degradation: use IPC if peer service running, fall back to direct file access
- `--from` flag for `wg trace instantiate` to use functions from peers

**Implementation status:** Task `cross-repo-dispatch` (done) suggests Phase 2 (cross-repo task dispatch) was implemented. Federation config exists. Full cross-repo dependency resolution (Phase 3) status is unclear.

---

### 2.11 Smooth Integration (nikete/VX)

**Source:** `docs/design/smooth-integration.md`
**Type:** Strategic design document
**Status:** Design only (mostly)
**Tasks:** `design-smooth-integration` (done), `write-integration-roadmap` (done), `compare-nikete-fork-feb20` (done), plus several nikete research tasks

**Summary:** Strategy for integrating nikete's workgraph fork and supporting VX (Veracity Exchange) integration. Core principle: design workgraph as a platform, not a monolith. External systems should observe, react to, and extend workgraph through well-defined surfaces.

**Key decisions:**
- CLI as the integration surface (`wg <cmd> --json` is the API contract)
- Tier 1 stable JSON contracts for 8 key commands
- `wg watch --json` for event streaming (Phase 1, before webhook callbacks)
- `wg capabilities --json` for capability discovery
- Canon schema as interchange format (materialized, sanitized view of work product)
- Three zones of sharing: Internal (full), Public (sanitized), Credentialed (richer for verified peers)
- `wg veracity` subcommand namespace for outcome scoring, attribution, sensitivity
- Bridging vocabulary: serde aliases (`value`<->`score`, `mean_reward`<->`avg_score`) instead of renames
- Thin VX adapter pattern (translates between VX protocol and `wg` CLI)
- No plugin system yet - current extension points (executors, evaluator agents, federation trait, CLI) are sufficient
- 5-phase implementation: Foundation -> Observability -> Canon -> Outcome Scoring -> Exchange

**Implementation status:** Foundation (serde aliases, `Evaluation.source`) may be partially done. `wg watch --json` and `wg capabilities --json` not yet implemented. Canon system and veracity scoring are design-only.

---

## 3. Archived Task Clusters

### 3.1 Agent Message Queue Cluster (15+ tasks)

| Task ID | Title | Status |
|---------|-------|--------|
| `research-agent-message` | Research: agent message queue design | done |
| `build-base-message` | Build base message queue platform | done |
| `implement-amplifier-message` | Implement Amplifier message adapter | done |
| `validate-agent-messaging` | Validate agent messaging end-to-end | done |
| `test-and-fix` | Test and fix agent message responsiveness | done |
| `design-message-discipline` | Design: message discipline | done |
| `research-messaging-impl` | Research: current message queue implementation | done |
| `commit-all-uncommitted` | Commit all uncommitted agent changes | done |

### 3.2 Coordinator Chat Cluster (8+ tasks)

| Task ID | Title | Status |
|---------|-------|--------|
| `sh-chat-protocol-design` | Design coordinator chat protocol | done |
| `sh-chat-storage` | Implement chat storage | done |
| `sh-instant-wakeup` | Implement instant wake-up | done |
| `sh-impl-chat-cli` | Implement wg chat CLI command | done |
| `design-node-specific` | Design: node-specific chat | done |
| `tui-chat-subtle` | TUI chat: subtle background tint | done |
| `research-group-chat` | Research: group chat implications | done |

### 3.3 Agent Lifecycle Cluster (25+ tasks)

| Task ID | Title | Status |
|---------|-------|--------|
| `committee-host-v2` | Committee host v2: unified lifecycle design | done |
| `committee-v2-researcher-{1..6}` | 6 parallel researchers (A1, A2, B1, B2, C1, D1) | all done |
| `committee-researcher-d` | Round 1: wg wait design | done |
| `phase-4a-remove` | Remove --no-session-persistence | done |
| `phase-4b-implement` | Implement Waiting status and wg wait | done |
| `phase-4c-coordinator` | Coordinator condition evaluation/resume | done |
| `phase-5a-implement` | Implement wg checkpoint CLI | done |
| `phase-5b-coordinator` | Coordinator auto-checkpoint + triage | done |
| `phase-6-message-triggered` | Message-triggered resurrection | done |
| `verify-flip-committee-host-v2` | FLIP verification of lifecycle design | open |
| `verify-flip-committee-v2-researcher-5` | FLIP verification of state machine | open |

### 3.4 Notification Channels Cluster (20+ tasks)

| Task ID | Title | Status |
|---------|-------|--------|
| `research-human-in` | Research: HITL channels | done |
| `phase-1-implement-notificationchannel` | NotificationChannel trait + router | done |
| `phase-1-implement-telegram` | Telegram channel | done |
| `phase-1-implement-webhook` | Webhook channel | done |
| `phase-1-refactor-matrix` | Matrix refactor behind trait | done |
| `phase-2-email-slack` | Email + Slack channels | done |
| `phase-3-discord-sms` | Discord + SMS channels | done |
| `phase-4-voice-calls` | Voice calls + Push | done |

### 3.5 Integration / VX Cluster (15+ tasks)

| Task ID | Title | Status |
|---------|-------|--------|
| `design-smooth-integration` | Smooth integration design | done |
| `write-integration-roadmap` | nikete integration roadmap | done |
| `compare-nikete-fork-feb20` | Compare nikete fork | done |
| `review-nikete-fork` | Review nikete fork code | done |
| `review-nikete-logging` | Review nikete logging | done |
| `research-veracity-exchange` | Research VX system | done |
| `research-veracity-deep` | Deep VX research | done |
| `update-veracity-deep-dive` | Update VX deep dive | done |
| `eval-logging-vs` | Evaluate logging vs veracity | done |
| `design-agency-federation` | Agency federation design | done |
| `cross-repo-dispatch` | Cross-repo task dispatch | done |

### 3.6 Persistent Session / Agent Communication Research (5+ tasks)

| Task ID | Title | Status |
|---------|-------|--------|
| `research-persistent-session` | Research: persistent/session agents | done |
| `research-agent-askuser` | Research: agent AskUser/JSON template behavior | done |
| `research-group-chat` | Research: group chat implications | done |
| `bootstrap-self-hosting` | Bootstrap self-hosting workgraph | done |
| `design-pan-executor` | Pan-executor bidirectional streaming | done |

---

## 4. Key Design Decisions

### 4.1 Architecture Decisions (Settled)

| Decision | Choice | Rationale | Source |
|----------|--------|-----------|--------|
| Message storage | JSONL per task, not in graph YAML | Avoids contention, semantic clarity, O(1) append | agent-message-queue.md |
| Chat vs task messages | Separate systems (chat/ vs messages/) | Different semantics: conversational vs directive | coordinator-chat-protocol.md |
| Urgent wake for chat | Bypass settling delay via separate flag | Chat needs sub-second response | coordinator-chat-protocol.md |
| Message discipline | Unread messages block `wg done` | Messages are structural, not optional | message-discipline-design.md |
| Stuck detection | Field on InProgress, not separate status | Agent doesn't know it's stuck | agent-lifecycle.md |
| Paused | Orthogonal boolean flag, not status | Any status can be paused | unified-lifecycle-state-machine.md |
| Resume pipeline | One pipeline for all triggers | Trigger differs; resume path identical | agent-lifecycle.md |
| NotificationChannel | Trait-based abstraction | Clean multi-platform support | human-in-the-loop-channels.md |
| Primary notification | Telegram first | Best ROI: easy setup, excellent Rust crate | human-in-the-loop-channels.md |
| Integration surface | CLI with `--json` as API | Process-level plugin, composable | smooth-integration.md |
| Executor generalization | `prompt_mode` decoupling | Eliminates `type = "claude"` hack | amplifier-integration-proposal.md |
| Cross-repo naming | `peer:task-id` syntax | Unambiguous, no colons in local IDs | cross-repo-communication.md |
| Canon format | YAML, zone-based sanitization | Shareable materialized view of work | smooth-integration.md |
| VX adapter | Thin adapter, fat CLI | Don't duplicate wg logic | smooth-integration.md |

### 4.2 Architecture Decisions (Open/Debated)

| Decision | Options | Current Status | Source |
|----------|---------|----------------|--------|
| Ready: computed vs stored | Computed (simpler) vs stored (faster) | Defer to implementation, start computed | unified-lifecycle-state-machine.md |
| Real-time message injection | Named pipe + stream-json (v2) | Designed but not implemented | agent-message-queue.md |
| Phase 2 coordinator chat | Persistent LLM agent processes chat | Designed, stub in place | coordinator-chat-protocol.md |
| Plugin system | None (CLI is sufficient) vs formal plugins | Not yet, revisit when 3+ integrations | smooth-integration.md |

---

## 5. Cross-Document Connections

### 5.1 Document Dependency Graph

```
human-in-the-loop-channels.md
  └─> NotificationChannel trait design
      └─> Referenced by smooth-integration.md (extension points)

agent-message-queue.md
  └─> Feeds into coordinator-chat-protocol.md (separate but parallel storage pattern)
  └─> Feeds into message-discipline-design.md (depends on message infrastructure)
  └─> Feeds into node-specific-chat-design.md (TUI reads same message files)
  └─> Referenced by agent-lifecycle.md (messages trigger resurrection)

coordinator-chat-protocol.md
  └─> Depends on agent-message-queue.md (same JSONL pattern)
  └─> Feeds into node-specific-chat-design.md (TUI shows both chat and task messages)

message-discipline-design.md
  └─> Depends on agent-message-queue.md (poll_messages, cursors)
  └─> Referenced by node-specific-chat-design.md (compatible design)

agent-lifecycle.md
  └─> Extends docs/design/liveness-detection.md (Round 1)
  └─> References docs/design/wg-wait-design.md
  └─> References docs/research/message-triggered-resurrection.md
  └─> References docs/research/checkpointing-systems-analysis.md
  └─> Feeds into unified-lifecycle-state-machine.md

unified-lifecycle-state-machine.md
  └─> Formalizes agent-lifecycle.md into state transitions
  └─> Depends on liveness-detection.md (Round 1 detection)

amplifier-integration-proposal.md
  └─> Depends on amplifier-context-transfer.md (context analysis)
  └─> Referenced by smooth-integration.md (executor extension point)

amplifier-context-transfer.md
  └─> Depends on amplifier-architecture.md (Amplifier internals)
  └─> Feeds into amplifier-integration-proposal.md

cross-repo-communication.md
  └─> Extends existing federation.rs (agency federation)
  └─> Feeds into smooth-integration.md (peer concept)

smooth-integration.md
  └─> Depends on all of the above (synthesis document)
  └─> References nikete-integration-roadmap.md, nikete-fork-deep-review.md, veracity-exchange-deep-dive.md
```

### 5.2 Shared Concepts Across Documents

| Concept | Appears In |
|---------|-----------|
| JSONL append-only storage | agent-message-queue, coordinator-chat-protocol, message-discipline |
| Per-agent cursors | agent-message-queue, node-specific-chat (in-memory variant) |
| Executor-agnostic design | agent-lifecycle, amplifier-integration, agent-message-queue |
| `request_id` correlation | coordinator-chat-protocol |
| Checkpoint/resume pipeline | agent-lifecycle, unified-lifecycle-state-machine |
| NotificationChannel trait | human-in-the-loop-channels, smooth-integration |
| Federation/peers | cross-repo-communication, smooth-integration |
| Canon interchange format | smooth-integration |
| Zone-based sanitization | smooth-integration |

---

## 6. Open Questions and Unfinished Threads

### 6.1 High Priority (Directly Relevant to Current Integration)

1. **Persistent coordinator agent (Phase 2 of chat)**: The coordinator chat protocol has a stub response in Phase 1. Phase 2 requires a persistent LLM agent that processes chat messages, calls wg tools, and writes intelligent responses. This is the key missing piece for natural human-coordinator interaction.

2. **Node-specific chat TUI implementation**: The design is complete (`node-specific-chat-design.md`) but not implemented. This would give users a natural way to communicate with specific tasks.

3. **`wg watch --json` event streaming**: Designed in `smooth-integration.md` but not implemented. Critical for any external integration (VX adapter, webhooks, monitoring).

4. **Real-time message injection (v2)**: Named pipes + `--input-format stream-json` for injecting messages into running Claude agents. Currently agents must poll with `wg msg read`.

5. **FLIP verification tasks still open**: `verify-flip-committee-host-v2` and `verify-flip-committee-v2-researcher-5` are still in `open` status, suggesting the lifecycle design hasn't been fully verified.

### 6.2 Medium Priority

6. **NotificationChannel implementation verification**: All 4 phases of HITL notification channels are marked "done" in the archive, but the actual code (src/notify/) needs verification that it compiled and works.

7. **Context transfer improvements**: R1-R5 from `amplifier-context-transfer.md` (dependency metadata, artifact annotations, content inlining) are largely unaddressed.

8. **Canon system**: Fully designed in `smooth-integration.md` but no implementation tasks created. The canon schema, `wg canon export/import`, and `{{task_canon}}` template variable are all design-only.

9. **Ready vs Open status split**: The `unified-lifecycle-state-machine.md` proposes splitting `Open` into `Ready` and `Draft`. This is a significant breaking change that hasn't been decided.

10. **Cross-repo dependency resolution**: `cross-repo-dispatch` is done (Phase 2) but it's unclear if Phase 3 (cross-repo `blocked_by` resolution in coordinator tick) was implemented.

### 6.3 Lower Priority

11. **VX/Veracity subcommand namespace**: `wg veracity outcome|attribute|scores|sensitivity|check|challenge|suggest` -- entirely design-phase.

12. **Group chat**: `research-group-chat` explored implications but no design or implementation followed.

13. **Agent-to-agent direct channels**: Currently agents message each other's tasks. No dedicated inter-agent communication channel.

14. **Message threading**: `reply_to` field mentioned in agent-message-queue.md Phase 4 (advanced features) but never implemented.

15. **Escalation chains**: Designed in HITL channels (Telegram -> SMS -> Phone) but likely not implemented beyond the trait.

---

## 7. Relevance Assessment

### 7.1 Still Relevant (Build On)

| Work | Why Relevant | Action |
|------|-------------|--------|
| Agent message queue design + implementation | Foundation for all agent communication | Maintain and extend |
| Coordinator chat protocol + implementation | Foundation for user-coordinator interaction | Implement Phase 2 (persistent agent) |
| Message discipline | Already enforced; agents read messages | Maintain |
| Agent lifecycle (wait, checkpoint, resurrection) | Partially implemented; resume pipeline is key | Verify implementation, complete gaps |
| NotificationChannel trait design | Clean abstraction for multi-platform | Verify implementation status |
| Cross-repo communication design | Federation peers concept is clean | Complete Phase 3 (dependency resolution) |
| Node-specific chat design | Ready-to-implement TUI feature | Implement |
| Smooth integration strategy | Platform-not-monolith principle applies broadly | Follow when integrating |

### 7.2 Partially Superseded

| Work | What Changed | What to Keep |
|------|-------------|-------------|
| Amplifier integration proposal | Amplifier executor already works (with hack) | prompt_mode decoupling still valuable |
| Amplifier context transfer | Some recommendations addressed (verify field) | Remaining R1-R5 still valuable |
| Matrix code assessment | Matrix may have been refactored behind trait | Assessment methodology is useful template |

### 7.3 Potentially Superseded

| Work | Why Potentially Superseded | Evaluate Before Using |
|------|---------------------------|---------------------|
| VX/Veracity integration | Depends on nikete collaboration status | Check if nikete integration is still planned |
| Canon interchange format | Depends on VX integration direction | Check if cross-org sharing is still a goal |
| Unified state machine (full 11-state) | May be over-engineered for current needs | Only implement states that are needed |
| Phase 4 voice/push notifications | Very niche use case | Defer until demanded |
