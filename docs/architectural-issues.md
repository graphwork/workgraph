# Workgraph Architectural Issues

## The Core Question

How do autonomous agents coordinate work through workgraph without a human orchestrator?

Currently, I (Claude) act as the coordinator - checking ready tasks, dispatching subagents, monitoring completion. But for true autonomy, agents need to self-organize.

---

## Issue 1: Agent Identity Model

**Current state**: Agents are just strings ("agent-1", "worker-1"). No persistence, no capabilities, no history.

**What's missing**:
- Agents don't know what they're good at
- Tasks don't know what skills they need
- No way to match tasks to capable agents
- No way to verify an agent is who it claims to be

**Questions**:
- Should agents have persistent profiles with capabilities?
- Should capabilities be declarative ("I know Rust") or demonstrated (track record)?
- How do agents authenticate? (matters for distributed systems)

**Proposed model**:
```
Actor {
  id, name, role,
  capabilities: ["rust", "design", "testing"],  // NEW
  context_limit: 100000,  // tokens/chars they can handle
  trust_level: "verified" | "provisional",
}
```

---

## Issue 2: Task-Agent Matching

**Current state**: Any agent can claim any task. No skill matching.

**What's missing**:
- Tasks don't specify required skills
- No algorithm to suggest which agent should take which task
- Agents waste time claiming tasks they can't do

**Questions**:
- Hard requirements ("must know Rust") vs soft preferences ("Rust preferred")?
- Who decides if an agent is capable - the agent itself, or a coordinator?
- What if no capable agent is available?

**Proposed model**:
```
Task {
  ...existing fields...
  requires_skills: ["rust", "cli"],  // hard requirement
  prefers_skills: ["testing"],       // nice to have
  complexity: "simple" | "moderate" | "complex",
}
```

---

## Issue 3: Context Inheritance

**Current state**: Each agent starts fresh. No context flows between tasks.

**What's missing**:
- When task B depends on task A, B's agent doesn't know what A produced
- No "handoff" mechanism
- Agents re-discover context that previous agents already found

**This is critical**: An agent working on `task-skills` (blocked by `task-description`) needs to know what the description field looks like before adding skills to it.

**Questions**:
- Should tasks have explicit input/output specifications?
- Should there be a "task artifact" storage?
- How much context should flow automatically vs explicitly?

**Proposed model**:
```
Task {
  ...existing fields...
  inputs: ["task-description:output"],  // depends on another task's output
  output: "src/graph.rs",               // what this task produces
  context_files: ["docs/spec.md"],      // what to read before starting
}
```

---

## Issue 4: Execution Model

**Current state**: Workgraph tracks tasks. Something external (me, Claude Code) actually does the work.

**What's missing**:
- No standard way to "execute" a task
- No definition of what "doing the work" means
- No connection between workgraph and the execution environment

**Questions**:
- Is workgraph just coordination, or should it trigger execution?
- Should tasks have executable definitions (scripts, prompts)?
- How do agents get permissions/tools they need?

**Options**:
1. **Coordination only**: Workgraph tracks state, external system does work
2. **Execution included**: `wg run <id>` spawns an agent with the right context
3. **Hybrid**: Workgraph provides context, agent decides how to execute

**Proposed model** (hybrid):
```
Task {
  ...existing fields...
  prompt: "Implement X by doing Y",     // instruction for agent
  tools_required: ["bash", "edit"],     // what permissions needed
  working_dir: "/home/erik/workgraph",  // where to work
}
```

---

## Issue 5: Failure Handling

**Current state**: Tasks are open → in-progress → done. No failure state.

**What's missing**:
- What if an agent crashes mid-task?
- What if the work fails (tests don't pass)?
- No way to record why something failed
- No retry logic

**Questions**:
- Should there be a "failed" status?
- Automatic retry or human intervention?
- How long before an in-progress task is considered abandoned?

**Proposed model**:
```
Status: open | in-progress | done | failed | abandoned

Task {
  ...existing fields...
  failure_reason: "Tests failed: 3 errors",
  attempts: 2,
  max_attempts: 3,
  timeout_hours: 24,  // abandon if in-progress longer than this
}
```

---

## Issue 6: Coordination Pattern

**Current state**: Decentralized - agents independently check `ready`, race to claim.

**What's missing**:
- Race conditions cause wasted work
- No way to batch related tasks
- No load balancing

**Options**:

### A. Pure Decentralized (current)
```
while true:
  tasks = wg ready
  for task in tasks:
    if wg claim task:
      do_work(task)
      wg done task
      break
  sleep(interval)
```
**Pro**: Simple, no single point of failure
**Con**: Race conditions, no optimization

### B. Centralized Coordinator
```
# Coordinator
while true:
  ready = wg ready
  agents = available_agents()
  assignments = match(ready, agents)  # skill matching
  for (task, agent) in assignments:
    dispatch(agent, task)
```
**Pro**: Optimal matching, no races
**Con**: Single point of failure, bottleneck

### C. Trajectory-Based
```
# Agent claims a path through the graph
trajectory = find_context_coherent_path(ready_tasks)
wg claim-trajectory trajectory  # atomic claim of multiple tasks
for task in trajectory:
  do_work(task)
  wg done task
```
**Pro**: Context efficient, fewer handoffs
**Con**: Harder to parallelize, complex algorithm

### D. Bidding/Auction
```
# Coordinator announces task
# Agents bid based on capability + availability
# Highest bidder wins
```
**Pro**: Market efficiency
**Con**: Complex, overhead

**Recommendation**: Start with A (current), add trajectory-based claiming as optimization.

---

## Issue 7: Progress Visibility

**Current state**: Binary - in-progress or done. No intermediate state.

**What's missing**:
- Can't see what an agent is actually doing
- Can't estimate time remaining
- No way to intervene if stuck

**Proposed model**:
```
Task {
  ...existing fields...
  progress_log: [
    {timestamp, agent, message: "Reading existing code..."},
    {timestamp, agent, message: "Found 3 files to modify"},
    {timestamp, agent, message: "50% complete - 2/4 functions done"},
  ],
  progress_percent: 50,  // optional numeric progress
}
```

---

## Summary: What's Actually Architectural?

| Issue | Type | Priority |
|-------|------|----------|
| Agent identity model | Architectural | High |
| Task-agent matching | Architectural | High |
| Context inheritance | Architectural | Critical |
| Execution model | Architectural | Medium |
| Failure handling | Design | High |
| Coordination pattern | Architectural | Medium |
| Progress visibility | Feature | Medium |

**The critical path**: Context inheritance is the biggest gap. Without it, agents can't build on each other's work effectively.

---

## Next Steps

1. **Decide on execution model**: Is workgraph coordination-only or does it trigger execution?
2. **Design context inheritance**: How do task outputs become inputs for dependent tasks?
3. **Add agent capabilities**: Extend Actor with skills
4. **Add task requirements**: Extend Task with required skills, context files, deliverables
5. **Implement trajectory claiming**: Algorithm to find context-coherent task paths
