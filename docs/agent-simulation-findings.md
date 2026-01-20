# Agent Simulation Findings

## Overview

Ran 3 parallel agents (worker-1, worker-2, worker-3) to test workgraph coordination. Each agent:
1. Checked `wg ready` for available tasks
2. Attempted to claim a task
3. Reported their experience

## Results

| Agent | Saw Ready | Claimed | Result |
|-------|-----------|---------|--------|
| worker-1 | 2 tasks | agent-architecture | Success |
| worker-2 | 2 tasks | (both) | Failed - already claimed |
| worker-3 | 2 tasks | agent-identity | Success |

worker-2 experienced a race condition: tasks appeared in `ready` but were claimed by others before they could act.

## Gaps Identified

### 1. No `wg show <id>` command
Agents wanted detailed task information but only `list` exists. Need a dedicated command to show all fields for a single task.

### 2. Tasks lack rich metadata
Current task fields are minimal. Agents need:
- **Description/body**: Detailed requirements, acceptance criteria
- **Skills required**: What capabilities does an agent need? (rust, design, docs)
- **Context files**: What should the agent read first?
- **Deliverable**: Where does output go? What format?

### 3. Claim failures lack context
When a claim fails, the message just says "already in progress". Should show:
- Who claimed it
- When they claimed it
- Suggestion for what else to do

### 4. No progress tracking
No way to record partial progress or notes. If an agent is interrupted, all context is lost.

### 5. Race conditions are inevitable
The `ready` → `claim` gap means agents will sometimes fail to claim. This is fine, but:
- Need graceful retry logic
- Could add optimistic locking / compare-and-swap
- Could show "claiming..." state

## Architectural Questions Raised

### Agent Identity
How does an agent know who it is? Currently passed via `--actor` flag, but:
- Should identity persist across sessions?
- Should agents have profiles with capabilities?
- How to prevent impersonation?

### Context Adoption
When an agent wakes up:
1. How does it know what skills it has?
2. How does it select appropriate tasks?
3. How does it load necessary context (files, history)?

### Trajectory Claiming
Related tasks could be claimed together for context efficiency:
- `task-description` → `task-skills` → `task-deliverable` form a logical unit
- An agent working on data model changes could claim all three
- Need algorithm to find "context-coherent" task clusters

## Recommendations

### Immediate (fix the gaps)
1. Add `wg show <id>` command
2. Add description field to Task
3. Improve claim failure messages
4. Add progress log

### Medium-term (agent support)
1. Add skills/context metadata to tasks
2. Add agent profiles with capabilities
3. Implement trajectory-based claiming

### Long-term (autonomous agents)
1. Executive agent that manages worker pool
2. Wake/check/work/sleep cycle implementation
3. Context inheritance between related tasks
