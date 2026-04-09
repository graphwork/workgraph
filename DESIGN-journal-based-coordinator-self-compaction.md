# Design: Journal-Based Coordinator Self-Compaction

**Date:** 2026-04-09
**Status:** Draft
**Type:** Architecture Enhancement

---

## Problem Statement

The current coordinator compaction system (`run_graph_compaction` in `src/commands/service/mod.rs:1643`) is **externally driven** — the daemon must poll, check token thresholds, and trigger compaction on behalf of the coordinator. This creates coupling and timing issues.

**Goal:** Enable the coordinator to compact **itself** using its own conversation journal, without requiring daemon-mediated token counting or explicit `.compact-*` task lifecycle management.

---

## Background

### Current Compaction Architecture

The existing system has **two independent compaction layers**:

| Layer | Location | Trigger | Token Counting |
|-------|----------|---------|----------------|
| **Native Executor** | `src/executor/native/resume.rs:698–845` | Per-turn pressure check (char-count proxy) | Rough (÷4.0 char estimation) |
| **Coordinator** | `src/commands/service/mod.rs:1643–1857` | Daemon poll + token threshold | Accurate (API-reported) |

Coordinator compaction:
- Runs inside the daemon's main event loop
- Checks `CoordinatorState::accumulated_tokens` against `effective_compaction_threshold()`
- Marks `.compact-0` as InProgress → calls `compactor::run_compaction()` → marks Done
- Resets `accumulated_tokens` to 0 after successful compaction

### The Journal

The native executor already writes a **conversation journal** (`src/executor/native/journal.rs`) to `.workgraph/output/<task-id>/conversation.jsonl`. Each entry is a JSON line with:

```json
{"seq":1,"timestamp":"2026-04-09T10:00:00Z","entry_type":"init",...}
{"seq":2,"timestamp":"2026-04-09T10:00:01Z","entry_type":"message",...}
{"seq":3,"timestamp":"2026-04-09T10:00:05Z","entry_type":"tool_execution",...}
{"seq":4,"timestamp":"2026-04-09T10:00:10Z","entry_type":"compaction",...}
```

**Existing compaction journal entry** (`JournalEntryKind::Compaction`):
```rust
Compaction {
    compacted_through_seq: u64,    // Last seq that was compacted
    summary: String,                // Human-readable summary
    original_message_count: u32,   // How many messages were compacted
    original_token_count: u32,     // Token count in compacted region (always 0)
}
```

The `seq` field provides a monotonically increasing sequence number — ideal for determining how much conversation history remains uncompacted.

---

## Design: Coordinator Self-Compaction via Journal

### Core Idea

The coordinator agent (running as a native executor) already has access to its own conversation journal. Rather than relying on the daemon to count tokens externally, the coordinator can:

1. **Read its own journal** to determine conversation length
2. **Trigger self-compaction** when the conversation exceeds a threshold
3. **Write a `Compaction` journal entry** marking what was summarized
4. **Continue running** with the compacted context

This shifts compaction from an **external daemon action** to an **internal agent action**.

### Advantages

| Aspect | Current (Daemon-Driven) | Proposed (Journal-Based Self-Compaction) |
|--------|------------------------|------------------------------------------|
| Token counting | External (`accumulated_tokens`) | Native (actual API usage per message) |
| Trigger mechanism | Daemon poll interval | Agent pre-turn check |
| Compaction scope | Whole graph state | Single coordinator conversation |
| Lifecycle management | `.compact-*` tasks | No special tasks needed |
| Coupling | Daemon ↔ Coordinator | Coordinator is self-contained |
| Debugging | Scattered across daemon/coordinator | All compaction evidence in journal |

### Interaction with Existing Systems

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Coordinator Self-Compaction                       │
├─────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  Coordinator Agent (Native Executor)                                 │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │  Pre-turn check:                                            │   │
│  │    1. Read journal entries                                  │   │
│  │    2. Sum actual token counts (from Usage in Message entries)│   │
│  │    3. If tokens > threshold → call emergency_compact()      │   │
│  │    4. Write Compaction journal entry                        │   │
│  └──────────────────────────────────────────────────────────┘   │
│                              │                                     │
│                              ▼                                     │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │  Conversation Journal (conversation.jsonl)                    │   │
│  │    - Init / Message / ToolExecution / Compaction / End        │   │
│  │    - seq is monotonically increasing                         │   │
│  │    - Compaction entries track compacted_through_seq          │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                              │                                     │
│                              ▼                                     │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │  Daemon Compaction (existing system, optional)               │   │
│  │    - Summarizes graph state → context.md                    │   │
│  │    - Can read journal for additional context                 │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                      │
└─────────────────────────────────────────────────────────────────────┘
```

**Key insight:** The two compaction systems operate at different levels:
- **Agent-level** (proposed): Compacts the coordinator's conversation context
- **Graph-level** (existing): Summarizes the full workgraph state into `context.md`

They are complementary and can coexist.

---

## Implementation Plan

### Phase 1: Journal-Enabled Token Counting

Enhance the journal to capture actual API token counts, not just estimate them.

**Changes to `JournalEntryKind::Message`:**
```rust
Message {
    role: Role,
    content: Vec<ContentBlock>,
    usage: Option<Usage>,           // Already present
    response_id: Option<String>,
    stop_reason: Option<StopReason>,
    // ADD: Tokens consumed by this turn (input + output)
    turn_token_count: Option<u32>,  // NEW FIELD
}
```

**Changes to `Journal::append` for compaction:**
```rust
Compaction {
    compacted_through_seq: u64,
    summary: String,
    original_message_count: u32,
    original_token_count: u32,  // NOW POPULATED from sum of turn_token_count
}
```

### Phase 2: Self-Compaction in Coordinator Agent

Add a pre-turn pressure check in the coordinator agent loop that:

1. **Reads the journal** to count tokens since last compaction
2. **Sums `turn_token_count`** from Message entries after the last `Compaction` entry
3. **If sum > threshold:** calls `emergency_compact()` with a summary prompt
4. **Writes a `Compaction` journal entry** with accurate token counts

**Trigger site:** Similar to `agent.rs:1000–1070` but in the coordinator agent.

**Compaction algorithm:** Same `ContextBudget::emergency_compact()` used by native executor.

### Phase 3: Daemon Compaction (Optional Enhancement)

The daemon's `run_graph_compaction()` can optionally read the coordinator's journal to:
- Get accurate token usage statistics
- Include per-coordinator conversation summaries in `context.md`
- Remove the need for external `accumulated_tokens` tracking

---

## Detailed Design

### Token Counting

Current: `ContextBudget::estimate_tokens()` uses char-count proxy (÷4.0).

Proposed: Use actual API-reported token counts from journal entries.

```rust
fn journal_token_count(path: &Path, since_seq: u64) -> Result<u64> {
    let entries = Journal::read_all(path)?;
    let mut total = 0u64;
    for entry in entries {
        match &entry.kind {
            JournalEntryKind::Message { usage, .. } => {
                if entry.seq > since_seq {
                    if let Some(u) = usage {
                        total += u.input_tokens as u64;
                        total += u.output_tokens as u64;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(total)
}
```

### Compaction Trigger

```rust
fn check_coordinator_pressure(messages: &[Message], journal_path: &Path) -> ContextPressureAction {
    let last_compaction_seq = find_last_compaction_seq(journal_path);
    let tokens_since_compaction = journal_token_count(journal_path, last_compaction_seq)
        .unwrap_or(0);

    let threshold = Config::load_or_default(std::path::Path::new("."))
        .effective_compaction_threshold();

    let ratio = tokens_since_compaction as f64 / threshold as f64;
    match ratio {
        r if r < 0.80 => ContextPressureAction::Ok,
        r if r < 0.90 => ContextPressureAction::Warning,
        r if r < 0.95 => ContextPressureAction::EmergencyCompaction,
        _ => ContextPressureAction::CleanExit,
    }
}
```

### Self-Compaction Execution

```rust
fn self_compact(messages: &mut Vec<Message>, journal: &mut Journal, keep_recent: usize) -> Result<()> {
    let pre_compact_count = messages.len();
    let pre_compact_tokens = estimate_tokens(messages); // or read from journal

    // Compact messages (same algorithm as native executor)
    let summary = summarize_messages(&messages[..messages.len() - keep_recent]);
    *messages = ContextBudget::emergency_compact(messages, keep_recent);

    // Write compaction journal entry
    journal.append(JournalEntryKind::Compaction {
        compacted_through_seq: journal.seq(),
        summary: format!(
            "Compacted {} messages (est. {} tokens). Summary: {}",
            pre_compact_count, pre_compact_tokens, summary
        ),
        original_message_count: pre_compact_count as u32,
        original_token_count: pre_compact_tokens as u32,
    })?;

    Ok(())
}
```

---

## File Changes

| File | Lines | Change |
|------|-------|--------|
| `src/executor/native/journal.rs` | 50–94 | Add `turn_token_count` to `Message`; populate `original_token_count` in `Compaction` |
| `src/executor/native/resume.rs` | 698–845 | `ContextBudget` is already usable; no changes needed |
| `src/executor/native/agent.rs` | 1000–1070 | Add self-compaction trigger in coordinator agent loop |
| `src/service/compactor.rs` | 127–176 | Optionally read journal for accurate token stats |

---

## Verification

1. `cargo test` passes
2. Compaction journal entries are written with accurate `original_token_count`
3. Coordinator agent continues running after self-compaction
4. Daemon compaction (if enabled) reads journal for context

---

## Related Documents

- `design/coordinator-compaction.md` — Existing coordinator compaction lifecycle analysis
- `research-findings.md` — Comparison of native executor vs coordinator compaction systems
- `src/executor/native/journal.rs` — Journal implementation
- `src/service/compactor.rs` — Graph-level compaction
- `src/commands/service/mod.rs:1643` — Daemon compaction trigger

---

## Open Questions

1. **Interaction with daemon compaction:** Should self-compaction disable or supplement daemon compaction?
2. **Threshold coordination:** Should self-compaction and daemon compaction share the same threshold?
3. **Summary quality:** The current `emergency_compact()` uses placeholder summaries. Should we call an LLM for better summaries?
