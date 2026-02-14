# Research: Cyclic & Repetitive Processes in Work Graphs

**Date**: 2026-02-14
**Task**: research-cyclic-processes
**Audience**: Small team / solo developer modeling recurring workflows

---

## Executive Summary

Workgraph currently assumes a DAG (Directed Acyclic Graph) for task dependencies. Many real workflows are cyclic: sprint cycles, review-revise loops, CI/CD pipelines, monitoring→alert→fix→verify, recurring standups. This report surveys how other systems handle cycles, reviews formal approaches, and proposes a minimal extension for workgraph.

**Key finding**: The most pragmatic path is *not* to abandon the DAG model, but to layer cycle semantics on top of it. The three patterns that emerge across all systems are:

1. **Iteration edges** — explicit back-edges with guards and bounds (keep the DAG for scheduling, add annotated cycle edges for re-activation)
2. **Template instantiation** — cycles create new instances of task subgraphs rather than mutating existing ones
3. **Event-driven re-activation** — completion/failure events trigger status resets on upstream tasks

The recommended minimal change for workgraph: add a `loop` edge type with a guard condition and max iteration count, let `Done` tasks be re-opened to `Open` via these edges, and use the existing `wg check` infrastructure to verify cycle safety.

---

## Table of Contents

1. [How Other Systems Handle Cycles](#1-how-other-systems-handle-cycles)
2. [Formal Graph Theory Approaches](#2-formal-graph-theory-approaches)
3. [Task Status in Cycles](#3-task-status-in-cycles)
4. [Preventing Infinite Loops](#4-preventing-infinite-loops)
5. [Minimal Design for Workgraph](#5-minimal-design-for-workgraph)
6. [Appendix: Comparative Tables](#6-appendix-comparative-tables)

---

## 1. How Other Systems Handle Cycles

### 1.1 Temporal

**Model**: Imperative code with durable event history. No static graph — the workflow *is* the code.

**Cycle mechanism**: Native language loops (`for`, `while`). Each loop iteration appends events to the workflow's history. The hard limit is **51,200 history events** (50 MB). After ~100 iterations, you're supposed to call **`ContinueAsNew`**, which atomically completes the current execution and starts a fresh one with the same Workflow ID, carrying forward only explicitly passed state.

**Key insight for workgraph**: Temporal separates the *logical identity* (Workflow ID stays constant) from the *physical execution* (Run ID changes on ContinueAsNew). This is directly applicable — a recurring task in workgraph could keep its task ID but create new "iterations" or "runs."

**Other primitives**:
- **Cron Schedules / Schedules API**: Server-side recurring triggers (recommended for periodic workflows)
- **Signals**: External messages that wake up a blocked workflow
- **Timers**: Durable sleeps that survive process restarts
- **Child Workflows**: Parent spawns children; child's ContinueAsNew is invisible to parent

**Loop prevention**: History event limit (51,200). No built-in max-iterations — developer responsibility.

### 1.2 Apache Airflow

**Model**: Strict DAG. The `DAG` object **rejects** circular dependencies at parse time.

**Cycle mechanism**: None within a single DAG run. Instead, repetition is handled through orthogonal mechanisms:
- **Scheduling**: Each DAG has a `schedule` (cron, `@daily`, `@continuous`). The scheduler creates independent **DAG Runs** for each interval.
- **Retries**: Per-task `retries` count with `retry_delay` and exponential backoff. Failed tasks enter `up_for_retry` state.
- **Sensors**: Operators that block until an external condition is met. In reschedule mode, they release the worker between checks.
- **Dynamic Task Mapping** (`expand()`): Fan-out at runtime based on upstream output.

**Key insight for workgraph**: Airflow's approach of "loops live outside the graph" via scheduling is simple and proven. A workgraph equivalent would be a `wg schedule` command that periodically re-instantiates a task subgraph.

**Task states**: `none → scheduled → queued → running → success/failed/up_for_retry/skipped/upstream_failed`

### 1.3 Argo Workflows (Kubernetes)

**Model**: DAG or Steps templates. No cycles in the graph.

**Cycle mechanism**: Explicit loop primitives:
- **`withItems`**: Iterate a step over a hardcoded list
- **`withParam`**: Iterate over a dynamically generated JSON array
- **`withSequence`**: Iterate over a numeric range
- **CronWorkflow**: Scheduled recurring execution with concurrency policies (`Allow`, `Replace`, `Forbid`)

**Retry strategy**: Per-template with `limit`, `retryPolicy` (OnFailure/OnError/Always), backoff duration/factor/maxDuration, and CEL expressions for conditional retry.

**Key insight for workgraph**: Each loop iteration creates a **separate node** in the workflow tree, named `step(0)`, `step(1)`, etc. This "unroll the loop into instances" pattern keeps the execution history clean.

### 1.4 n8n

**Model**: DAG with **explicit cycle support**. This is the most relevant precedent for workgraph.

**Cycle mechanism**: n8n allows connecting a node's output back to a previous node's input, creating actual cycles in the graph. It handles this using **Tarjan's strongly connected components (SCC) algorithm** for cycle detection in O(V + E).

- **SplitInBatches node**: Takes an input list, outputs items in batches, has a "loop" output that connects back to the processing chain and a "done" output for termination
- **Manual loops**: Connect any output back to a previous input, use an IF node as termination condition

**Partial execution rule**: If any node within a cycle is "dirty" (needs re-execution), the **entire cycle** must restart from its entry point.

**Key insight for workgraph**: n8n proves that a practical workflow tool can support cycles without abandoning the graph model. The SCC-based approach means cycles are first-class citizens that the engine understands and handles safely.

**Loop prevention**: **None built-in.** Documentation warns that manual loops without termination conditions will run forever.

### 1.5 Prefect

**Model**: Imperative Python code (no static graph). Like Temporal but Python-native.

**Cycle mechanism**:
- Standard Python `for`/`while` loops (v2/v3) — each task call in a loop creates a separate task run
- `LOOP` signal (v1 only) — task raises `LOOP(result=state)` to trigger another iteration, accumulating `Looped` states in a single task run's history
- Subflows for compositional repetition
- Deployments with cron/interval/RRule schedules for recurring execution

**Key insight for workgraph**: Prefect v1's `LOOP` signal is interesting — it's an explicit "re-run me" primitive at the task level, with iteration state visible in the task's history. This maps well to a `loop_count` field on workgraph tasks.

### 1.6 BPMN / Camunda

**Model**: Process graphs that **natively support cycles**. The only mainstream standard with first-class loop semantics.

**Loop types**:

1. **Standard Loop** (loop marker ↻ on activity):
   - Condition-based repetition (evaluated before or after each iteration via `testBefore`)
   - Optional `loopMaximum` to cap iterations
   - Always sequential

2. **Multi-Instance Loop** (three bars marker):
   - Collection-based fan-out (like `forEach`)
   - Parallel (|||) or Sequential (≡) execution
   - `completionCondition` for early termination
   - Per-instance `loopCounter` variable

3. **Sequence Flow Cycles**: Explicit back-edges using XOR gateways as while-loop conditions

**Key insight for workgraph**: BPMN distinguishes between **structured loops** (well-nested, with clear entry/exit) and **arbitrary cycles** (spaghetti back-edges). Workgraph should follow this — structured loops are safe and analyzable; arbitrary cycles need explicit opt-in.

### 1.7 Cylc

**Model**: Unique **cycling model** purpose-built for repetitive workflows (weather forecasting, climate modeling).

**Cycle mechanism**: Tasks repeat on different intervals within the same workflow. Cylc "unrolls the cycle loop" to create a single non-cycling workflow of repeating tasks, each with its own **cycle point** (timestamp or integer). Inter-cycle dependencies use notation like `[-P1]` (previous cycle point) or `[-P2]` (two cycles back).

**Key insight for workgraph**: Cylc's approach of parameterizing tasks by cycle point — `task_A[cycle=3]` depends on `task_A[cycle=2]` — is elegant. It keeps the graph acyclic by making each iteration a distinct node, but the *definition* is cyclic. This "template + instantiation" pattern is powerful.

### 1.8 AWS Step Functions

**Model**: State machine (Amazon States Language). Allows cycles through `Choice` states pointing backward.

**Cycle mechanism**: A `Choice` state evaluates conditions and either transitions forward (exit) or backward (loop). A `Map` state iterates over arrays with `MaxConcurrency` control.

**Loop prevention**: Hard limit of **25,000 events** and 1-year maximum duration. Each loop iteration consumes ~3 state transitions. Cost per transition is a natural limiter.

---

## 2. Formal Graph Theory Approaches

### 2.1 Back-Edges in Directed Graphs

The simplest extension: classify edges using DFS into **tree edges**, **forward edges**, **cross edges**, and **back-edges**. A back-edge points from a descendant to an ancestor in the DFS tree — it's what creates cycles. A directed graph is cyclic iff DFS produces at least one back-edge.

**For workgraph**: Add a `loop_to` or `iterates_to` edge type that is explicitly marked as a back-edge. The topological sort ignores these edges (so scheduling still works), but the executor respects them for re-activation. Attach `max_iterations` and a `guard` condition.

**Complexity**: O(V + E) for detection and classification.

**Assessment**: Lowest implementation effort. Already 80% implemented — workgraph's `check.rs` does DFS-based cycle detection and `loops.rs` classifies cycles by tag.

### 2.2 Petri Nets

**Model**: A bipartite graph of **places** (circles, hold tokens) and **transitions** (rectangles, fire when all input places have tokens). Transitions consume input tokens and produce output tokens.

**Cycles**: Arise naturally when a transition's output place is also (transitively) an input place. A review cycle:

```
[draft] --> |submit| --> [under_review] --> |review_pass| --> [approved]
                                        --> |review_fail| --> [needs_revision]
[needs_revision] --> |revise| --> [draft]  ← closes the cycle
```

**Key properties**:
- **Liveness**: Every transition can eventually fire (no permanent stalls)
- **Boundedness**: Token counts stay within limits (finite state space, analyzable)
- **Soundness** (for Workflow Nets): From any reachable state, completion is achievable; completion means exactly one token in the end place

**Assessment**: Most formally rigorous. Directly applicable — workgraph tasks map to transitions, dependency edges to places, resources to resource-places. The `wg check` command could perform soundness analysis instead of just cycle detection. But EXPSPACE-hard for general soundness checking, and adds conceptual overhead.

### 2.3 Statecharts (Harel)

**Model**: Hierarchical state machines with concurrent regions. States contain sub-states (OR-decomposition) or parallel sub-machines (AND-decomposition).

**Cycles**: Simply transitions that return to a previously visited state. Key innovation: **history states** (H / H*) that remember which sub-state was last active when re-entering a composite state. Deep history (H*) is particularly relevant — re-entering a review phase resumes from the specific step that flagged the issue, not from scratch.

**Assessment**: Good fit for modeling individual task lifecycles (the Open→InProgress→Done state machine already in workgraph). Less natural for modeling the inter-task dependency graph. Best used as a complementary conceptual model.

### 2.4 Workflow Nets (van der Aalst)

**Model**: A Petri net subclass specifically designed for business processes. Has a source place (start), sink place (end), and every node lies on a path between them.

**Soundness criterion**: (1) completion is always reachable, (2) completion means all other places are empty, (3) no dead transitions. This is exactly the property workgraph needs for safe cycles.

**Extended WF-net**: Add a feedback transition from sink to source to model "run the workflow again." If the extended net is live and bounded, the original WF-net is sound.

**Assessment**: Most directly applicable formalism. Your workgraph is essentially already a WF-net. The soundness criterion gives formal backing to the Intentional/Warning/Info cycle classification already in `loops.rs`.

### 2.5 Hypergraphs

**Model**: Edges connect sets of nodes to sets of nodes (many-to-many). A single hyperarc can express "if both deploy-staging AND deploy-prod fail, re-open the design task."

**Assessment**: Theoretically elegant but practically too exotic. Few tools, hard to visualize, unfamiliar to developers. Not recommended for workgraph.

### 2.6 Process Algebras (CSP, Pi-Calculus)

**Model**: Behavioral specifications using recursion and parallel composition. Cycles are expressed as recursive process definitions.

**Assessment**: Powerful for reasoning about concurrent agent behavior (relevant to `wg service`), but too abstract for task dependency management. Academic tooling only.

### 2.7 Event-Driven / Reactive

**Model**: Workflows as compositions of event handlers connected by streams. Cycles are feedback loops where completion/failure events trigger upstream re-activation.

**Key patterns**:
- **Dead letter queues**: Events retried beyond a threshold are moved aside, breaking cycles
- **Circuit breakers**: After N failures, the retry stream is cut
- **TTL/hop count**: Events carry a counter decremented on each cycle traversal

**Assessment**: Complementary to the graph model. Workgraph's service daemon already handles task lifecycle events — adding explicit retry/revision events with backpressure gives safe cycle execution without changing the graph representation.

---

## 3. Task Status in Cycles

### 3.1 The Core Question: Can a Done Task Become Open Again?

Every system answers this differently:

| System | Can Done → Active? | Mechanism |
|--------|-------------------|-----------|
| Temporal | No — new execution | ContinueAsNew creates fresh Run ID |
| Airflow | No — new DAG Run | Each scheduling tick creates independent instances |
| Argo | No — new node | Loop iterations are separate nodes `step(N)` |
| n8n | Yes (implicitly) | Back-edge re-executes nodes in the cycle |
| Prefect v1 | Yes (same task run) | LOOP signal adds Looped states to history |
| BPMN | Yes (natively) | Loop marker re-activates the activity |
| Step Functions | Yes (natively) | Choice state loops back to previous state |

**Two philosophies emerge**:

**A) Immutable runs, new instances** (Temporal, Airflow, Argo, Cylc): Each iteration creates a new execution/run/node. The original remains Done. History is clean and append-only. The "loop" is visible as a series of instances.

**B) Mutable status, re-activation** (n8n, BPMN, Step Functions): The same task transitions back from Done to an active state. Simpler model but muddier history.

### 3.2 Recommendation for Workgraph

**Hybrid approach**: Use **both** philosophies depending on the use case.

**For retry/revision loops** (short cycles, same work unit): Allow `Done → Open` re-activation on the same task. Workgraph already supports `Failed → Open` via `wg retry` and `PendingReview → Open` via `wg reject`. Extending this to `Done → Open` via a loop edge is natural.

**For recurring processes** (sprint cycles, periodic reviews): Create **new task instances** from a template. `sprint-review[2026-W07]` is a distinct task from `sprint-review[2026-W08]`. Keep the template as a definition, instantiate per cycle.

### 3.3 What Triggers Re-Activation?

Three trigger models from the survey:

1. **Condition-based** (BPMN, Step Functions): A guard expression on the back-edge evaluates to true. "If integration tests fail, re-open implementation."

2. **Event-based** (Temporal signals, reactive): An external or internal event triggers re-activation. "When a monitoring alert fires, re-open the investigation task."

3. **Schedule-based** (Airflow, Argo CronWorkflow): Time triggers a new cycle. "Every Monday, create a new sprint planning task."

For workgraph, condition-based triggers are the most natural fit for inline cycles. Schedule-based triggers are best for recurring processes. Event-based triggers could be added later via the service daemon.

---

## 4. Preventing Infinite Loops

### 4.1 Strategies Across Systems

| Strategy | Used By | Mechanism |
|----------|---------|-----------|
| **Hard event/history limits** | Temporal (51K), Step Functions (25K) | Execution terminated at limit |
| **Max iteration count** | BPMN (loopMaximum), custom | Counter on the loop edge |
| **Guard conditions** | BPMN, Step Functions Choice | Boolean expression must be true to loop |
| **Concurrency policies** | Argo (Forbid/Replace) | Prevent overlapping executions |
| **Cost/billing** | Step Functions | Economic pressure to bound loops |
| **Structural: no cycles** | Airflow, Luigi | Reject cycles at parse time |
| **Developer responsibility** | n8n, Prefect | Documentation warns, no enforcement |
| **Backpressure** | Reactive systems | Bounded queues prevent runaway |
| **SCC-based restart** | n8n | Entire cycle restarts from entry, bounded by input |

### 4.2 Recommendation for Workgraph

Use **defense in depth** — multiple layers:

1. **Required `max_iterations` on loop edges** (mandatory, no default of "unlimited"). Forces the developer to think about bounds. Small default (e.g., 10) with explicit override.

2. **Guard conditions** (optional). A simple expression: "re-open if status of downstream task X is Failed" or "re-open if iteration_count < N."

3. **`wg check` validation**. Extend the existing cycle checker to:
   - Verify all cycles have at least one bounded edge (max_iterations or guard)
   - Classify unbounded cycles as errors, not just warnings
   - Optionally: lightweight soundness check (can all tasks still reach Done?)

4. **Runtime iteration counter**. Track `loop_iteration` per task. The executor refuses to re-open a task beyond its bound.

---

## 5. Minimal Design for Workgraph

### 5.1 Design Principles

1. **Don't break the DAG** — scheduling, topological sort, critical path, and `wg ready` should keep working. Loop edges are ignored for scheduling purposes.
2. **Explicit over implicit** — cycles require opt-in. No accidental infinite loops.
3. **Observable** — every re-activation is logged, with iteration counts visible.
4. **Bounded by default** — unbounded loops are errors.
5. **Incremental** — implement in stages; each stage is useful on its own.

### 5.2 Stage 1: Loop Edges (Graph Model Change)

Add a new edge type to the task model:

```yaml
# In graph.jsonl
{
  "id": "fix-bug",
  "title": "Fix the bug",
  "status": "open",
  "blocked_by": ["investigate-bug"],
  "loops_to": [
    {
      "target": "investigate-bug",
      "guard": "status_of(verify-fix) == 'failed'",
      "max_iterations": 3
    }
  ]
}
```

**New fields on Task**:
- `loops_to: Vec<LoopEdge>` — back-edges that can re-activate upstream tasks
- `loop_iteration: u32` — current iteration count (0 = first run)

**LoopEdge struct**:
```rust
struct LoopEdge {
    target: String,          // task ID to re-activate
    guard: Option<String>,   // condition expression (evaluated at runtime)
    max_iterations: u32,     // hard cap (required, no default unlimited)
}
```

**Scheduling behavior**: `loops_to` edges are **ignored** by topological sort and `wg ready`. They only matter when a task completes.

**Completion behavior**: When a task with `loops_to` edges transitions to `Done`:
1. Evaluate each loop edge's guard condition
2. If guard is true and `target.loop_iteration < max_iterations`:
   - Set target task status to `Open`
   - Increment target's `loop_iteration`
   - Log: "Re-activated by loop from {source} (iteration {n}/{max})"
3. If guard is false or iterations exhausted: proceed normally (task stays Done)

### 5.3 Stage 2: Cycle Validation (Safety)

Extend `wg check` and `wg loops`:

- **Bounded check**: Every cycle in the graph must pass through at least one `loops_to` edge with a finite `max_iterations`. Cycles without bounds are errors.
- **Reachability check**: From any state reachable by loop re-activation, can all tasks still reach Done? (Simplified soundness — doesn't need full Petri net analysis.)
- **Integration with existing classification**: `loops_to` edges with valid bounds → Intentional. Missing bounds → Error (not Warning).

### 5.4 Stage 3: Recurring Templates (For Sprint Cycles, Standups)

For processes that repeat on a schedule (not condition-based loops):

```toml
# .workgraph/templates/sprint-review.toml
[template]
id = "sprint-review"
schedule = "0 10 * * 1"  # Every Monday at 10am
instance_id_format = "sprint-review-{date}"

[[template.tasks]]
id = "plan-{date}"
title = "Sprint planning"

[[template.tasks]]
id = "review-{date}"
title = "Sprint review"
blocked_by = ["plan-{date}"]
```

A `wg template instantiate sprint-review` command (or automatic via service daemon on schedule) creates concrete tasks with date-parameterized IDs. Each instantiation is a fresh subgraph — no mutation of completed tasks.

This is the Cylc/Airflow pattern: the *definition* is cyclic, but each *instance* is a DAG.

### 5.5 Stage 4: Event-Driven Re-Activation (Service Integration)

Extend the service daemon to handle loop edge evaluation:

1. When an agent completes a task, the daemon checks for `loops_to` edges
2. Evaluates guard conditions against current graph state
3. If re-activation is triggered, resets target task and dispatches a new agent
4. Backpressure: if multiple re-activations fire simultaneously, queue them with configurable concurrency

### 5.6 What This Looks Like in Practice

**Example 1: Review-Revise Loop**
```bash
wg add "Write draft" --id write-draft
wg add "Review draft" --id review-draft --blocked-by write-draft
wg add "Revise draft" --id revise-draft --blocked-by review-draft \
  --loops-to "write-draft:guard=status_of(review-draft)==failed:max=5"
```

The graph: `write-draft → review-draft → revise-draft --loop→ write-draft`

When `revise-draft` completes, if review failed, `write-draft` is re-opened (up to 5 times).

**Example 2: CI/CD Pipeline with Retry**
```bash
wg add "Build" --id build
wg add "Test" --id test --blocked-by build
wg add "Deploy" --id deploy --blocked-by test \
  --loops-to "build:guard=status_of(test)==failed:max=3"
```

If tests fail after deploy attempt, the whole pipeline restarts from build, up to 3 times.

**Example 3: Monitoring Loop**
```bash
wg add "Monitor" --id monitor
wg add "Alert" --id alert --blocked-by monitor
wg add "Investigate" --id investigate --blocked-by alert
wg add "Fix" --id fix --blocked-by investigate
wg add "Verify" --id verify --blocked-by fix \
  --loops-to "monitor:max=10"
```

The monitor→alert→investigate→fix→verify cycle runs up to 10 times.

**Example 4: Recurring Standup (Template)**
```bash
wg template create standup --schedule "0 9 * * 1-5"
wg template add-task standup "Daily standup" --id "standup-{date}"
# Service instantiates fresh tasks each weekday at 9am
```

### 5.7 Implementation Effort Estimate

| Stage | Scope | Touches |
|-------|-------|---------|
| Stage 1: Loop edges | Add `loops_to` field, completion hook | `graph.rs`, `parser.rs`, `done.rs` |
| Stage 2: Validation | Extend cycle checker | `check.rs`, `loops.rs` |
| Stage 3: Templates | New template system | New `template.rs`, new CLI commands |
| Stage 4: Service integration | Daemon handles re-activation | `service/coordinator.rs` |

Stages 1-2 are the minimal viable change. Stage 3 is independently useful. Stage 4 ties it all together.

---

## 6. Appendix: Comparative Tables

### System Survey Summary

| System | Base Model | Native Cycles? | Loop Mechanism | Infinite Loop Prevention |
|--------|-----------|---------------|----------------|-------------------------|
| **Temporal** | Imperative code + event history | Yes (code loops) | ContinueAsNew + Schedules API | 51,200 event limit |
| **Airflow** | Strict DAG | No | Scheduling (DAG Runs), retries, sensors | DAG validation rejects cycles |
| **Argo** | DAG/Steps templates | No | withItems/Param/Sequence, CronWorkflow | etcd limits, parallelism caps |
| **n8n** | DAG with cycle support | Yes | SplitInBatches, manual back-edges | None (developer responsibility) |
| **Prefect** | Imperative Python | Yes (code loops) | Python loops, LOOP signal (v1) | None (developer responsibility) |
| **BPMN/Camunda** | Process graph | Yes (native) | Standard loop, multi-instance, flow cycles | loopMaximum, completionCondition |
| **Cylc** | Cycling model | Yes (parameterized) | Cycle points, inter-cycle deps | Finite cycle range |
| **Step Functions** | State machine | Yes (Choice loops) | Choice + backward transition, Map | 25,000 event limit |
| **Luigi** | Implicit DAG (targets) | No | External cron | Relies on finite dependency tree |

### Formal Approach Summary

| Approach | Implementation Effort | Cycle Expressiveness | Verification Power | Recommendation |
|----------|----------------------|---------------------|-------------------|----------------|
| **Back-edges** | Low | Structured only | Termination bounds | **Yes — Stage 1** |
| **Petri Nets** | Medium | Full | Liveness, boundedness, soundness | Inspire validation |
| **Statecharts** | Medium | Full | Model checking | Conceptual model only |
| **Workflow Nets** | Medium | Full | Soundness criterion | **Yes — Stage 2 validation** |
| **Hypergraphs** | High | Full + conjunctive | Hyperpath analysis | No — too exotic |
| **Process Algebras** | High | Full | Bisimulation | No — wrong abstraction |
| **Event-Driven** | Low-Medium | Full | Backpressure + TTL | **Yes — Stage 4** |

### Sources

- [Temporal Fundamentals - Workflows](https://keithtenzer.com/temporal/Temporal_Fundamentals_Workflows/)
- [Guide to ContinueAsNew in Temporal](https://medium.com/@qlong/guide-to-continueasnew-in-cadence-temporal-workflow-using-iwf-as-an-example-part-1-c24ae5266f07)
- [Airflow Tasks Documentation](https://airflow.apache.org/docs/apache-airflow/stable/core-concepts/tasks.html)
- [Argo Workflows Loops](https://argo-workflows.readthedocs.io/en/latest/walk-through/loops/)
- [Argo CronWorkflows](https://argo-workflows.readthedocs.io/en/latest/cron-workflows/)
- [n8n Loop Over Items](https://docs.n8n.io/integrations/builtin/core-nodes/n8n-nodes-base.splitinbatches/)
- [n8n Partial Execution and Graph Traversal](https://zread.ai/n8n-io/n8n/11-partial-execution-and-graph-traversal-algorithms)
- [Prefect Task Looping](https://docs.prefect.io/core/advanced_tutorials/task-looping.html)
- [Repeating Activities in BPMN](https://www.trisotech.com/repeating-activities-in-bpmn/)
- [Camunda Workflow Patterns](https://docs.camunda.io/docs/components/concepts/workflow-patterns/)
- [Cylc Workflow Engine](https://cylc.github.io/)
- [Cylc Basic Cycling Tutorial](https://cylc.github.io/cylc-doc/latest/html/tutorial/scheduling/integer-cycling.html)
- [AWS Step Functions Map State](https://docs.aws.amazon.com/step-functions/latest/dg/state-map.html)
- [van der Aalst — Application of Petri Nets to Workflow Management (1998)](https://users.cs.northwestern.edu/~robby/courses/395-495-2017-winter/Van%20Der%20Aalst%201998%20The%20Application%20of%20Petri%20Nets%20to%20Workflow%20Management.pdf)
- [Petri Nets for Workflow Modeling](http://www.project-open.com/en/workflow-petri-nets)
- [Statecharts — Harel 1987](https://www.sciencedirect.com/science/article/pii/0167642387900359)
- [Workflow Patterns — Structured Loop (WCP-21)](http://www.workflowpatterns.com/patterns/control/new/wcp21.php)
- [Executing Cyclic Scientific Workflows in the Cloud](https://link.springer.com/article/10.1186/s13677-021-00229-7)
- [Cylc: A Workflow Engine for Cycling Systems (paper)](https://www.researchgate.net/publication/326554854_Cylc_A_Workflow_Engine_for_Cycling_Systems)
- [State of Open Source Workflow Orchestration Systems 2025](https://www.pracdata.io/p/state-of-workflow-orchestration-ecosystem-2025)
