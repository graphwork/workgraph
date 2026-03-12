# Real-Time Agent Chat: Streaming Render and Interrupt-Aware Messaging

## Current Architecture Analysis

### Streaming Pipeline

The streaming pipeline for Claude-executor agents follows this path:

```
Claude CLI (--print --output-format stream-json)
    |
    v
stdout (JSONL events)
    |
    +--> tee -a raw_stream.jsonl   (raw Claude CLI events)
    +--> >> output.log             (same content, for TUI)
```

**Source:** `src/commands/spawn/execution.rs:698-703`

The wrapper script captures stdout via:
```bash
{command} > >(tee -a raw_stream.jsonl >> "$OUTPUT_FILE") 2>> "$OUTPUT_FILE"
```

#### Event Types in raw_stream.jsonl

Claude CLI's `stream-json` format emits **per-turn** events, not token-level deltas:

| Event Type | Frequency | Contains |
|---|---|---|
| `system` | 1 per session | session_id, model, tools list |
| `assistant` | 1 per turn | Full message content (text + tool_use blocks), usage |
| `user` | 1 per turn | Tool results returned to the model |
| `rate_limit_event` | ~1 per turn | Rate limit status |
| `result` | 1 per session | Final usage, cost, duration |

**Critical finding:** The `stream-json` format does NOT include `content_block_delta` events (token-level streaming). Each `assistant` event arrives as a complete turn with all text and tool calls already assembled. Analysis of 150+ raw_stream.jsonl files confirms this: the largest files have ~800 `assistant` events (one per tool-use cycle), but zero `content_block_delta` events.

This means **the raw data is NOT actually streaming token-by-token** as the task description assumed. The problem is architectural — `--output-format stream-json` gives us turn-level granularity, not token-level.

#### How the TUI Reads Agent Output

**Firehose view** (`src/tui/viz_viewer/state.rs:4534-4630`):
- Polls `output.log` files for all active agents on each tick (~250ms)
- Uses byte-offset tracking for incremental reads
- Displays raw JSONL lines as plain text with color-coded agent prefixes
- Auto-scrolls to bottom in tail mode

**Inspector output view** (`src/tui/viz_viewer/state.rs:6440-6523`):
- Reads entire `output.log` on each refresh (no incremental reads)
- Calls `extract_assistant_text_from_log()` to parse JSON and extract text
- Renders the extracted text through the markdown renderer (`src/tui/markdown.rs`)
- The extraction is batch-mode: slurps entire file, processes all lines, concatenates

The inspector already uses `pulldown-cmark` + `syntect` for markdown rendering
(`src/tui/markdown.rs:53-63`), but it only runs after the full text is assembled.

### Messaging Pipeline

#### Storage

Messages are stored as JSONL in `.workgraph/messages/{task-id}.jsonl`
(`src/messages.rs:68-69`). Each message has:
- `id`: Monotonic counter per task
- `timestamp`: RFC 3339
- `sender`: "user", "coordinator", agent-id
- `body`: Free-form text
- `priority`: "normal" or "urgent"
- `status`: Sent -> Delivered -> Read -> Acknowledged

Read cursors are stored in `.workgraph/messages/.cursors/{agent-id}.{task-id}`
— a plain file containing the last-read message ID.

#### Message Delivery

All three executor adapters (Claude, Amplifier, Shell) return `supports_realtime() = false`
(`src/messages.rs:554, 583, 607`).

The `ClaudeMessageAdapter` documents the constraint explicitly:
> "Claude agents run with `claude --print` which reads stdin once and processes a single turn.
> Mid-session injection is not supported in v1." (`src/messages.rs:541-542`)

When a message is sent:
1. Appended to the task's JSONL queue
2. A human-readable line is appended to `agents/{agent-id}/pending_messages.txt`
3. The agent must self-poll via `wg msg read` or `wg msg poll`

#### When Agents Check Messages

The agent prompt instructs agents to check messages at task start and before marking done.
There is no mechanism to force an agent to check messages mid-turn. The agent runs in
`--print` mode, which reads stdin once at launch and then runs autonomously until exit.

#### TUI Chat vs Task Messages

There are two separate messaging systems:
1. **`wg msg`** — task-scoped message queues (JSONL files per task)
2. **`wg chat`** — coordinator chat via IPC socket (inbox/outbox JSONL files + daemon)

`wg chat` sends a `UserChat` IPC request to the running coordinator service, which processes
it in its agent loop. The coordinator has access to all graph state and can act on chat
messages, but individual task agents do not receive these messages.

---

## Problem Analysis

### Problem 1: No Token-Level Streaming

**Root cause:** Claude CLI's `--output-format stream-json` aggregates content internally
and emits complete per-turn `assistant` events. The TUI never sees individual tokens.

**Why this matters:**
- Agent response time is 10-60s per turn; user sees nothing until the turn completes
- No way to "read along" with the agent's reasoning
- Can't detect early if the agent is going wrong

### Problem 2: No Mid-Turn Message Delivery

**Root cause:** Claude CLI's `--print` mode reads stdin once at launch, then runs
its tool-use loop autonomously. There is no API to inject messages mid-conversation.
Agents poll for messages only when their prompt tells them to (typically start + end).

**Why this matters:**
- Corrections arrive too late (agent may have done minutes of wasted work)
- No "stop" button that actually stops the agent's current reasoning
- Multiple queued messages lose conversational flow

---

## Streaming Render Design

### Option A: Enable `--include-partial-messages` (Recommended)

Claude CLI supports `--include-partial-messages` (seen in `--help` output):
> "Include partial message chunks as they arrive (only works with --print and --output-format=stream-json)"

This flag would emit `content_block_delta`-style events (or partial `assistant` events)
in the stream-json output. This is the simplest path to token-level streaming.

**Implementation:**
1. Add `--include-partial-messages` to the command construction in
   `src/commands/spawn/execution.rs` (all claude mode variants)
2. Update `translate_claude_event()` in `src/stream_event.rs` to handle the new event
   types (likely `content_block_start`, `content_block_delta`, `content_block_stop`)
3. Add a new `StreamEvent::TextDelta { text: String, timestamp_ms: i64 }` variant
4. Update the TUI to consume text deltas incrementally

**TUI rendering strategy:**

For the inspector output view:
- Maintain a `StreamingTextBuffer` per agent that accumulates text deltas
- On each TUI tick, check for new deltas and append to the buffer
- Re-render markdown only when needed (not on every delta):
  - **Append-only fast path:** If the delta doesn't complete a block boundary
    (heading, code block, list), just append raw text to the last line
  - **Re-render on block boundaries:** When a newline followed by `#`, `` ` ``,
    `-`, `*`, or `|` arrives, re-render the current block
  - **Full re-render on completion:** When the turn finishes, do a complete
    markdown render of the accumulated text

This handles the "partial markdown" problem: incomplete code blocks or headings
are rendered as plain text until the block boundary closes, then re-rendered
with proper styling.

**Complexity:** Low. This is a CLI flag change + event parsing + incremental
TUI buffer.

### Option B: Use Anthropic API Directly (Streaming SSE)

Bypass the Claude CLI entirely and call the Anthropic Messages API with
`stream: true`. This gives us Server-Sent Events with `content_block_delta`
events containing individual text tokens.

**Pros:** Full control over streaming, can inject system messages between turns.
**Cons:** Loses all Claude CLI features (tool execution, file access, permission
system, context management, agent features). Would require reimplementing the
entire agent loop. Not recommended.

### Option C: Parse stdout Line-by-Line (Current Architecture)

The firehose already reads output.log incrementally. But since the events are
per-turn, this only gives us turn-level granularity. Not useful for streaming.

**Verdict:** Options A >> B > C. Option A is the clear winner — it uses an
existing CLI flag to enable token-level streaming with minimal code changes.

### Incremental Markdown Rendering

The existing `markdown_to_lines()` function (`src/tui/markdown.rs:53-63`) uses
`pulldown-cmark` which parses the full markdown string. For streaming, we need
an approach that handles partial markdown gracefully.

**Recommended strategy: Dual-buffer rendering**

```
                   +-----------------+
  text deltas ---> | raw text buffer | ---> render on block boundaries
                   +-----------------+          |
                                               v
                                    +------------------+
                                    | rendered lines[] | ---> TUI display
                                    +------------------+
```

1. **Raw text buffer:** Accumulates text from deltas. Append-only.
2. **Block boundary detection:** Track state: are we in a code block? A list?
   When a delta crosses a block boundary (e.g., closes a code fence), trigger
   re-render of the current block.
3. **Partial render:** For text that isn't in a special block, render as plain
   paragraphs. When the block completes, re-render with proper styling.
4. **Final render:** On turn completion, do full `markdown_to_lines()` to ensure
   correctness.

This avoids the cost of full re-render on every token while handling the edge
cases of partial markdown (unclosed code blocks, incomplete headings, etc.).

---

## Message Delivery Options

### Approach 1: Bidirectional Stream-JSON (Recommended for Phase 2)

Claude CLI supports `--input-format stream-json` which enables **streaming
input**. Combined with `--output-format stream-json`, this creates a
bidirectional JSONL pipe:

```
stdin (JSONL)  ---->  Claude CLI  ----> stdout (JSONL)
                        ^   |
                        |   v
                      tool loop
```

With `--input-format stream-json`, the CLI reads from stdin continuously rather
than reading once and closing. This means we could:

1. Keep the stdin pipe open to the Claude CLI process
2. When a user sends a message via `wg msg send`, write it to the agent's stdin
   as a stream-json event
3. Claude CLI would receive it as a new user message

**Implementation:**
1. Change `cmd.stdin(Stdio::null())` to `cmd.stdin(Stdio::piped())` in
   `execution.rs:260`
2. Store the stdin handle in the `AgentRegistry`
3. When `wg msg send` is called for a running agent, write the message to the
   agent's stdin pipe as a stream-json user message
4. Update `ClaudeMessageAdapter.deliver()` to write to the pipe
5. Update `supports_realtime()` to return `true`

**Constraints:**
- We need to verify the exact stream-json input format (likely
  `{"type":"user","content":"..."}`)
- The agent may be mid-response when the message arrives — need to understand
  how Claude CLI handles mid-stream input
- The `--replay-user-messages` flag might be needed for the agent to echo back
  the injected message

**Feasibility:** High, but needs CLI behavior verification. The `--input-format
stream-json` flag exists and is documented.

### Approach 2: Signal-Based Poll Trigger (Simple, Near-Term)

Use a file-watch or signal mechanism to wake the agent's polling:

1. When `wg msg send` is called, write to `pending_messages.txt` (already done)
2. Also touch a sentinel file: `agents/{agent-id}/.msg_notify`
3. Modify the agent prompt to include a periodic polling instruction:
   "Check `wg msg poll` between tool calls if you've been working for more
   than 2 minutes"
4. Add a Claude Code hook or MCP tool that checks for new messages between
   tool calls

**Pros:** Works with current architecture, no CLI changes needed.
**Cons:** Agent compliance is voluntary; still can't interrupt mid-response;
polling cadence is agent-dependent.

**A more robust variant:** Create a custom MCP server that:
- The agent connects to at session start
- Exposes a `check_messages` tool
- Is registered as a notification handler that fires between tool calls
- When a notification fires, the MCP server returns any pending messages

This leverages MCP's notification mechanism for proactive delivery.

### Approach 3: Session Resume with Message Injection

When a message needs to reach a running agent:

1. Send SIGTERM to the Claude CLI process (graceful stop)
2. The CLI saves its session state (it does this automatically)
3. Resume the session with `--resume <session_id>` and pipe the new message
   as a follow-up (this pattern already exists in `execution.rs:448-473`)
4. The agent continues where it left off but now has the new message

**Pros:** Uses existing resume infrastructure; message definitely reaches agent.
**Cons:** Disruptive — kills the current response; loses in-progress work on
the current turn; adds ~5-10s latency for session resume; may confuse the
agent's state.

**Best for:** Emergency "stop" messages or major course corrections. Not
suitable for casual conversation.

### Approach 4: Checkpoint-and-Redirect

Combine approaches 2 and 3:

1. Agent periodically checks `wg msg poll` between tool calls (Approach 2)
2. For urgent messages: SIGTERM + resume (Approach 3)
3. Use message priority to determine which path:
   - `normal`: Wait for agent's next poll
   - `urgent`: Trigger session restart with message injection

**Implementation:**
- Add a `--signal` flag to `wg msg send` that sends SIGUSR1 to the agent's PID
- The wrapper script traps SIGUSR1 and writes a "check messages" instruction
  to a file the agent's prompt tells it to watch
- For urgent: terminate + resume

### Approach 5: Parallel Monitor Agent

Spawn a lightweight "monitor" agent alongside the main agent:

1. Main agent works on the task
2. Monitor agent watches `pending_messages.txt` (via `wg msg poll --watch`)
3. When a message arrives, monitor can:
   - For corrections: Create a blocking prerequisite task, causing the
     coordinator to eventually kill the main agent and restart
   - For "stop": Kill the main agent's PID directly
   - For info: Just acknowledge the message

**Pros:** Doesn't require any CLI changes.
**Cons:** Resource overhead of a second agent; complex coordination.

---

## Claude CLI / API Constraints

### Confirmed Constraints

1. **`--print` mode reads stdin once:** The process reads the initial prompt from
   stdin, then runs autonomously. No way to inject content mid-run without
   `--input-format stream-json`.

2. **`stream-json` output is per-turn:** Without `--include-partial-messages`,
   events are complete turns, not token-level deltas.

3. **No mid-response interruption via API:** Once the model starts generating a
   response, it completes the full response before processing the next input.
   There is no "cancel current generation" mechanism in the Messages API.

4. **Session resume is available:** `--resume <session-id>` continues a
   conversation, preserving full context. This is already used by workgraph.

5. **Process signals work:** The Claude CLI process can be killed with signals.
   It saves session state on graceful termination.

### Capabilities to Leverage

1. **`--include-partial-messages`:** Enables token-level streaming in the
   `stream-json` output. This is the key unlock for Problem 1.

2. **`--input-format stream-json`:** Enables streaming input, potentially
   allowing message injection during the tool-use loop (between turns, not
   mid-generation). This is the key unlock for Problem 2.

3. **`--replay-user-messages`:** Re-emits user messages from stdin back on
   stdout. Useful for acknowledgment when using stream-json input.

4. **MCP servers:** The CLI connects to configured MCP servers. A custom MCP
   server could provide a message-delivery channel.

### Unknown / Needs Verification

1. Does `--input-format stream-json` keep the stdin pipe open throughout the
   session, or only for the initial prompt?
2. What happens if a user message arrives via stdin while the model is
   generating a response? Is it queued for the next turn?
3. What is the exact format for stream-json input messages?
4. Does `--include-partial-messages` emit standard Anthropic API delta events,
   or a Claude CLI-specific format?

---

## Recommended Implementation Plan

### Phase 1: Streaming Output (Weeks 1-2)

**Goal:** User sees agent text appearing token-by-token in the TUI.

1. **Add `--include-partial-messages` to spawn commands**
   - Modify `build_inner_command()` in `execution.rs` for all claude modes
   - Files: `src/commands/spawn/execution.rs`

2. **Extend StreamEvent for text deltas**
   - Add `StreamEvent::TextDelta { text: String, timestamp_ms: i64 }`
   - Update `translate_claude_event()` to handle `content_block_delta` events
   - Files: `src/stream_event.rs`

3. **Add streaming text buffer to TUI state**
   - New `StreamingTextState` per agent in `VizApp`
   - Accumulates text deltas, tracks block boundaries
   - Files: `src/tui/viz_viewer/state.rs`

4. **Render streaming text in inspector**
   - When viewing a working agent's task, show live streaming text
   - Use append-only rendering with periodic markdown re-render
   - Files: `src/tui/viz_viewer/render.rs`, `src/tui/markdown.rs`

5. **Update firehose for streaming**
   - Parse `content_block_delta` events and show text fragments
   - Files: `src/tui/viz_viewer/state.rs` (update_firehose)

### Phase 2: Message Delivery (Weeks 3-4)

**Goal:** Messages reach the agent between tool calls.

1. **Verify `--input-format stream-json` behavior**
   - Test: Does stdin stay open? What format? Mid-generation behavior?
   - Create a small test script to verify

2. **Keep stdin pipe open**
   - Change `cmd.stdin(Stdio::null())` to `cmd.stdin(Stdio::piped())`
   - Store `ChildStdin` handle in the agent registry
   - Files: `src/commands/spawn/execution.rs`, `src/service/registry.rs`

3. **Implement realtime message delivery for Claude adapter**
   - Write message to agent's stdin pipe as stream-json event
   - Update `ClaudeMessageAdapter` to use the pipe
   - Update `supports_realtime()` to return `true`
   - Files: `src/messages.rs`

4. **Fallback: signal-based polling**
   - If stream-json input doesn't work as expected, implement SIGUSR1-based
     polling trigger as a fallback
   - Agent prompt includes "check for messages when signaled"

### Phase 3: Chat UX (Weeks 5-6)

**Goal:** Integrated chat experience in the TUI.

1. **Streaming output in chat view**
   - Split pane: streaming output (top) + input box (bottom)
   - Streaming output shows markdown-rendered tokens as they arrive
   - Visual indicator: "typing..." when deltas are arriving

2. **Message status indicators**
   - Show message delivery status: sent -> delivered -> read -> acknowledged
   - Visual states: gray (sent), yellow (delivered), green (read/ack)
   - "Message pending" indicator when sent but not yet read

3. **Interrupt button**
   - Keyboard shortcut (e.g., Ctrl-C in chat mode) sends urgent message
   - If stream-json input is available: injects message between turns
   - If not: shows confirmation dialog, then SIGTERM + resume with message

4. **TUI chat input**
   - Allow composing messages directly in the TUI chat panel
   - Tab to switch between streaming output and input
   - Enter to send, visual feedback for delivery status

---

## Ideal Chat UX Description

```
+--[ Task: implement-auth ]--[ Chat ]--[ Output ]--[ Files ]--+
|                                                                |
|  ## Chat with agent-7162                                       |
|                                                                |
|  [you, 2m ago]                                                 |
|  Focus on the JWT validation first, not the OAuth flow.        |
|                                                                |
|  [agent-7162, 1m ago]                                          |
|  Acknowledged -- switching to JWT validation. I'll implement   |
|  the token parsing and expiry checks first.                    |
|                                                                |
|  [agent-7162, streaming...]                      <- live       |
|  I've added the JWT validation middleware. Now writing tests   |
|  for expired tokens. The test structure looks like...           |
|  |                                               <- cursor     |
|                                                                |
|  ┌─ Bash ────                                    <- tool call  |
|  | cargo test test_jwt_expired                                 |
|  └─                                                            |
|                                                                |
+----------------------------------------------------------------+
|  > Type message... (Enter to send, Ctrl-C to interrupt)    |   |
+----------------------------------------------------------------+
```

Key elements:
- **Streaming text** appears in real-time with a cursor indicator
- **Tool calls** shown inline as they execute
- **Message status** shown via sender label color (gray=sent, green=read)
- **Input box** at bottom, always available
- **Interrupt** via Ctrl-C sends an urgent message
- **History** scrollable, with relative timestamps

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| `--include-partial-messages` format unknown | Blocks Phase 1 | Test with a manual claude --print invocation first |
| `--input-format stream-json` doesn't stay open | Blocks Phase 2 | Fall back to signal-based polling (Approach 2) |
| Partial markdown rendering glitches | UX annoyance | Use plain-text fallback for incomplete blocks |
| Stdin pipe write fails (broken pipe) | Message lost | Write to JSONL queue as primary, pipe as secondary |
| Performance: re-rendering markdown on every delta | TUI lag | Batch deltas (render at most every 50ms) |
| Agent ignores injected messages | Message ineffective | Make message appear as tool result, not user message |

---

## Summary

The two problems have different root causes and different solutions:

1. **Streaming render** is solvable with `--include-partial-messages` flag + incremental
   TUI rendering. Low risk, high impact. Should be Phase 1.

2. **Message delivery** requires `--input-format stream-json` or a signal/resume mechanism.
   Medium risk (CLI behavior needs verification), high impact. Should be Phase 2.

Both require changes to:
- `src/commands/spawn/execution.rs` (CLI flags, stdin piping)
- `src/stream_event.rs` (new event types)
- `src/tui/viz_viewer/state.rs` (streaming buffers, incremental reads)
- `src/tui/viz_viewer/render.rs` (live streaming display)
- `src/messages.rs` (realtime delivery)

The combined effort is ~4-6 weeks for a full implementation. Phase 1 (streaming render)
can be delivered independently in ~2 weeks and provides immediate value.
