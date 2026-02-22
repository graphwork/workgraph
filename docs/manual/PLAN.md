# Workgraph + Agency Manual — Document Plan

This plan governs the structure, terminology, tone, and content of a conceptual manual for humans who want to understand and use workgraph and its agency system for their own projects. This is not an API reference or CLI cheat-sheet. It is a book — tight, precise prose that builds understanding from first principles.

**Target audience:** Humans who manage projects (solo or team), coordinate AI agents, or both. They may be software engineers, researchers, or project leads. They want to understand what workgraph does, why it works the way it does, and how to use it effectively.

**Tone:** Concise, lyrical technical prose. Not bullet-point documentation. Each section should read as a short essay that builds a mental model. Analogies are welcome where they clarify; jargon is defined before use. Diagrams (ASCII or typst-native) are encouraged where they compress paragraphs into a glance.

**Format:** Typst (`.typ` files). Each section is a standalone file that can be compiled independently or composed into the full manual. Cross-references use typst `@label` syntax.

---

## Glossary

Every writer must use these terms with these exact meanings. If a term is not in the glossary, do not invent new terminology — describe the concept in plain language and flag it for glossary review.

| Term | Definition |
|------|-----------|
| **task** | The fundamental unit of work in a workgraph. Has an ID, title, status, and may have dependencies, skills, inputs, deliverables, and other metadata. Tasks are nodes in the graph. |
| **status** | The current lifecycle state of a task. One of: *open* (available for work), *in-progress* (claimed by an agent), *done* (completed successfully), *failed* (attempted and failed; retryable), *abandoned* (permanently dropped), or *blocked* (explicitly marked; rarely used since blocking is usually derived). The three *terminal* statuses are done, failed, and abandoned — a terminal task no longer blocks its dependents. |
| **dependency** | A directed edge between tasks expressed via `after`. Task B depends on task A means B cannot be ready until A reaches a terminal status. Dependencies form the forward structure of the graph. |
| **after** | The authoritative dependency list on a task. `task.after = ["dep"]` means the task comes after `dep` — i.e., `dep` must complete before this task can be ready. Renamed from `blocked_by`; the old name is accepted as a serde alias for backward compatibility. |
| **before** | The computed inverse of `after`, maintained for bidirectional traversal. If B is after A, then A.before includes B. Not checked by the scheduler — purely a convenience index. Renamed from `blocks`. |
| **ready** | A task is *ready* when it is open, not paused, past any time constraints (`not_before`, `ready_after`), and every task in its `after` list is terminal. For cycle headers, back-edge predecessors within the same cycle are exempt from the readiness check (see *back-edge exemption*). Ready tasks are candidates for dispatch. |
| **structural cycle** | A cycle formed by `after` edges in the task graph. Detected automatically by Tarjan's SCC algorithm. Each structural cycle has a header (entry point), members, and back-edges. Cycles iterate when all members complete and the header's `CycleConfig` allows it. Structural cycles replace the old `loops_to` edge system. |
| **CycleConfig** | Configuration for structural cycle iteration, stored on the cycle header task. Fields: `max_iterations` (hard cap), `guard` (optional firing condition), `delay` (optional time delay between iterations). A cycle without a `CycleConfig` on its header is unconfigured and treated as a deadlock. |
| **cycle header** | The entry point of a structural cycle — the task that has a `CycleConfig` and controls iteration. Identified automatically as the node with external predecessors (or the lexicographically smallest ID in isolated cycles). The header receives back-edge exemption in readiness checks and carries the `"converged"` tag and `loop_iteration` counter. |
| **back-edge exemption** | In cycle-aware dispatch, the cycle header's predecessors that form back-edges within the cycle are exempt from the readiness check. This allows the header to become ready on the first iteration (when back-edge predecessors haven't completed yet) and on re-iteration (after all members are re-opened). Only applies when the header has a `CycleConfig`. |
| **guard** | A condition on a cycle's `CycleConfig` that must be true for the cycle to iterate. Three kinds: *Always* (unconditional), *TaskStatus* (iterate if a named task has a specific status), and *IterationLessThan* (iterate if the header's iteration count is below a threshold). |
| **loop iteration** | A counter on each task tracking how many times it has been re-activated by cycle iteration. Starts at zero. On the cycle header, compared against `CycleConfig.max_iterations` to enforce cycle bounds. Set to the same value on all cycle members during re-opening. |
| **loop edge (historical)** | The former `loops_to` mechanism: a conditional back-edge that fired on task completion to re-open a target upstream. Replaced by structural cycles. Legacy `loops_to` edges can be migrated with `wg migrate-loops`. |
| **resource** | A non-task node in the graph representing a consumable or limited asset (budget, compute, etc.). Tasks may `require` resources. Currently informational — not enforced by the scheduler. |
| **role** | An agency entity defining *what* an agent does. Contains a description, a desired outcome, and a list of skills. Identified by a content-hash of its identity-defining fields. |
| **motivation** | An agency entity defining *why* an agent acts the way it does. Contains a description, acceptable trade-offs (compromises the agent may make), and unacceptable trade-offs (hard constraints it must never violate). Identified by a content-hash of its identity-defining fields. |
| **agent** | The unified identity in the agency system — a named pairing of a role and a motivation. Can represent a human or an AI. For AI agents, the role and motivation are injected into the prompt. For human agents, role and motivation are optional. Identified by a content-hash of `(role_id, motivation_id)`. |
| **agency** | The collective system of roles, motivations, and agents. Also refers to the storage directory (`.workgraph/agency/`) and configuration namespace (`[agency]` in config.toml). |
| **content-hash ID** | A SHA-256 hash of an entity's identity-defining fields. Deterministic (same content = same ID), deduplicating (cannot create two identical entities), and immutable (changing identity-defining fields produces a *new* entity). Displayed as 8-character hex prefixes. All commands accept unique prefixes. |
| **capability** | A flat string tag on an agent (e.g., `"rust"`, `"testing"`) used for task-to-agent matching at dispatch time. Distinct from role skills: capabilities are for *routing*, skills are for *prompt injection*. |
| **skill** | A capability reference attached to a role. Four types: *Name* (tag-only label), *File* (path to a document), *Url* (HTTP resource), *Inline* (embedded content). Skills are resolved when an agent is spawned and their content is injected into the prompt. |
| **skill resolution** | The process of converting skill references into content. Name skills pass through as labels. File skills read from disk. Url skills fetch via HTTP. Inline skills use their embedded text. Failed resolutions produce warnings but do not block execution. |
| **trust level** | A classification on an agent: *verified* (fully trusted), *provisional* (default for new agents), or *unknown* (external, needs verification). Verified agents receive a small scoring bonus in task matching. |
| **executor** | The backend that runs an agent's work. Built-in executors: *claude* (pipes a prompt into the Claude CLI), *shell* (runs a bash command from the task's `exec` field). Custom executors can be defined as TOML files. The executor determines whether an agent is AI or human: `claude` = AI; `matrix`, `email`, `shell` = human. |
| **coordinator** | The scheduling brain inside the service daemon. Runs a tick loop that: cleans up dead agents, counts available slots, optionally creates auto-assign and auto-evaluate meta-tasks, finds ready tasks, and spawns agents for them. |
| **service** (or **service daemon**) | The background process started by `wg service start`. Hosts the coordinator, listens on a Unix socket for IPC, and manages agent lifecycle. Agents are detached via `setsid()` and survive daemon restarts. |
| **tick** | One iteration of the coordinator loop. Triggered by IPC (`graph_changed`) or a safety-net poll timer. |
| **dispatch** | The act of selecting a ready task and spawning an agent for it. Involves claiming the task, resolving the executor and model, rendering the prompt, generating a wrapper script, and forking a detached process. |
| **claim** | Marking a task as *in-progress* and recording who is working on it. The coordinator claims tasks atomically before spawning agents to prevent double-dispatch. |
| **assignment** | Binding an agency agent identity to a task (via `wg assign`). When the task is dispatched, the agent's role and motivation are rendered into the prompt. Distinct from *claiming* — assignment sets identity, claiming sets execution state. |
| **auto-assign** | A coordinator feature that automatically creates `assign-{task-id}` meta-tasks for unassigned ready work. An assigner agent (itself an agency entity) evaluates available agents and picks the best fit. |
| **auto-evaluate** | A coordinator feature that automatically creates `evaluate-{task-id}` meta-tasks for completed work. The evaluation task runs `wg evaluate`, which spawns an evaluator agent to score the work. |
| **evaluation** | A scored assessment of an agent's work on a task. Four dimensions: *correctness* (40%), *completeness* (30%), *efficiency* (15%), *style adherence* (15%). Produces an overall weighted score (0.0–1.0). Scores propagate to three levels: the agent, its role, and its motivation. |
| **performance record** | A running tally on each agent, role, and motivation: task count, average score, and a list of evaluation references. Role evaluations carry `context_id = motivation_id` and vice versa, enabling synergy analysis. |
| **evolution** | The process of improving agency entities based on evaluation data. `wg evolve` spawns an evolver agent that proposes structured operations (create, modify, retire) on roles and motivations. |
| **strategy** | An evolution approach: *mutation* (modify one entity), *crossover* (combine two), *gap-analysis* (create for unmet needs), *retirement* (remove underperformers), *motivation-tuning* (adjust trade-offs), or *all* (use all strategies). |
| **lineage** | Evolutionary history tracked on every role, motivation, and agent. Records parent IDs (empty for manual creation, one for mutation, two for crossover), generation number, creator identity, and timestamp. |
| **generation** | The number of evolutionary steps from a manually-created ancestor. Generation 0 = human-created. Each mutation or crossover increments the generation by one from the highest parent. |
| **synergy matrix** | A performance cross-reference showing how each (role, motivation) pair performs together. Computed from the `context_id` fields in evaluation references. Surfaced by `wg agency stats`. |
| **meta-task** | A task created automatically by the coordinator to manage the agency loop. Assignment tasks (`assign-{id}`), evaluation tasks (`evaluate-{id}`), and evolution review tasks are meta-tasks. Tagged to prevent recursive meta-task creation. |
| **map/reduce pattern** | An emergent workflow pattern in the task graph. *Fan-out* (map): one task precedes several children that run in parallel. *Fan-in* (reduce): several tasks precede a single aggregator that runs only when all are terminal. Not a formal primitive — arises naturally from dependency edges. |
| **triage** | An optional LLM-based assessment of dead agents. When `auto_triage` is enabled, the coordinator reads the dead agent's output log and classifies the result as *done*, *continue* (with recovery context), or *restart*. |
| **wrapper script** | The `run.sh` file generated for each spawned agent. Runs the executor command, captures output, and handles post-exit logic: if the agent didn't self-report completion, the wrapper checks task status and calls `wg done` or `wg fail` accordingly. |
| **visibility** | A field on each task controlling what information crosses organizational boundaries during trace exports. Three values: *internal* (default, org-only — all details included), *public* (sanitized sharing — task structure without agent output or logs), *peer* (richer detail for credentialed peers — includes evaluations and agent info but strips notes and detailed logs). Set via `wg add --visibility <zone>` or `wg edit`. |
| **convergence** | An agent-driven signal (`wg done --converged`) indicating that a cycle's iterative work has reached a stable state. Adds a `"converged"` tag to the **cycle header** (regardless of which cycle member the agent completes). When the header carries the converged tag, the cycle does not iterate — even if iterations remain and guard conditions are met. Cleared on retry. Any cycle member can signal convergence for the entire cycle. |
| **trace** | The operations log (`operations.jsonl`) recording every mutation to the graph. The project's organizational memory — queryable via `wg trace`, exportable with visibility filtering via `wg trace export`, and importable from peers via `wg trace import`. Also the high-level concept referring to the provenance system and `wg trace` command family. |
| **trace export** | A filtered, shareable snapshot of the trace. Visibility filtering controls what is included: *internal* exports everything, *public* sanitizes (strips logs, evaluations, agent details), *peer* provides richer detail for trusted peers. The interchange format for cross-boundary sharing. Produced by `wg trace export --visibility <zone>`. |
| **trace function** | A parameterized workflow template extracted from completed traces via `wg func extract`. Captures task structure, dependencies, structural cycles, and input parameters. Input types include string, text, file list, file content, number, URL, enum, and JSON. Applied via `wg func apply` to create new task graphs following the same pattern. Stored as YAML in `.workgraph/functions/`. |
| **replay** | Re-execution of previously completed or failed work. `wg replay` creates an immutable snapshot of the current graph state (stored in `.workgraph/runs/`), then selectively resets tasks based on criteria: failed-only, below-score threshold, explicit task list, or subgraph. Dependents in the transitive closure are also reset. Supports `--plan-only` for previewing. |
| **federation** | The system for sharing agency entities across workgraph projects. Operations: *scan* (discover entities in a remote store), *pull* (import from remote to local), *push* (export from local to remote). Named remotes are stored in `.workgraph/federation.yaml`. Performance records are merged during transfer with deduplication by task ID and timestamp. Content-hash IDs make federation natural — identical entities deduplicate automatically. |
| **remote** | A named reference to another workgraph project's agency store, used for federation. Stores a path, optional description, and last-sync timestamp. Managed via `wg agency remote add/list/remove`. Stored in `.workgraph/federation.yaml`. |
| **evaluation source** | A freeform string tag on each evaluation identifying its origin. Default: `"llm"` (internal auto-evaluator). Conventions: `"manual"` (human-reviewed), `"outcome:<metric>"` (external outcome data, e.g., `"outcome:sharpe"`), `"ci:<suite>"` (CI/test results), `"vx:<peer-id>"` (Veracity Exchange peer evaluation), `"import:<source>"` (imported via trace import). The evolver reads all evaluations regardless of source. |
| **event stream** | A real-time feed of graph mutations produced by `wg watch --json`. Events are typed: `task.created`, `task.started`, `task.completed`, `task.failed`, `task.retried`, `evaluation.recorded`, `agent.spawned`, `agent.completed`. Filterable by category (`task_state`, `evaluation`, `agent`) or by task ID prefix. Enables external adapters to observe and react without polling. |
| **adapter** | An external tool or service that translates between an external system's vocabulary and workgraph's ingestion points. The generic pattern: observe (via `wg watch`) → translate → ingest (via `wg` CLI commands) → react. Not a formal type in the codebase — a conceptual pattern for integration. Examples: CI bots, Slack integrations, portfolio management tools. |
| | |
| **— Pattern Vocabulary —** | The following terms come from the [Canonical Pattern Vocabulary](../design/spec-patterns-vocab.md), organized into four categories: structure, agency, control, and shorthands. |
| | |
| *Structure patterns* | |
| **pipeline** | A sequential chain of specialized stages with handoffs. Each stage transforms its predecessor's output. Graph shape: `A → B → C → D`. Throughput is limited by the slowest stage (Theory of Constraints). |
| **diamond** (fork-join) | Split work into parallel independent tasks, then synchronize at an integrator. The "diamond" shape: one node fans out, another fans in. Graph shape: `planner → [worker-1, worker-2, worker-3] → synthesizer`. **Critical rule: never parallelize tasks that modify the same files.** |
| **scatter-gather** | Fan out to heterogeneous specialists who each examine the same artifact from their own perspective, then collect their views. Unlike diamond, workers have different roles and the aggregator may accept partial results. |
| **loop** (structural cycle) | Iterate a chain of tasks until convergence. Formed by `after` edges creating a cycle, detected by Tarjan's SCC algorithm. The cycle header carries a `CycleConfig` with `max_iterations`, optional guard, and optional delay. Any cycle member can signal `--converged` to stop iteration early. |
| | |
| *Agency patterns* | |
| **planner-workers-synthesizer** | The canonical staffing for a diamond: one thinker decomposes the problem, many doers execute in parallel, one integrator combines results. Roles: architect/planner, implementer/worker, integrator/synthesizer. |
| **specialist** | One role owns one domain. Tasks requiring that domain are routed to agents with that role. Maps to Team Topologies' "complicated-subsystem team." Ashby's Law check: ensure at least as many specialist roles as distinct task types requiring specialized knowledge. |
| **stream-aligned** | One role follows one thread of work end-to-end, from inception to delivery. The default role type — most roles should be this. Aligned to a work stream (feature, product area), not to a function. Maps to Team Topologies' "stream-aligned team." |
| **platform** | One role produces shared infrastructure that other roles depend on. Platform tasks appear as `after` dependencies for stream-aligned tasks. Maps to Team Topologies' "platform team." See also: **scaffold** shorthand. |
| | |
| *Control patterns* | |
| **stigmergic** | Agents read the graph, not each other. The graph is the communication channel. All coordination happens through task state — no agent-to-agent messages. Every `wg done`, `wg log`, `wg artifact` call modifies the shared graph, stimulating downstream agents. This is the fundamental operating principle of workgraph, not an optional pattern. |
| **requisite variety** | Match the number of distinct roles to the number of distinct task types. Ashby's Law: **R ≥ V** (roles ≥ variety of tasks). Diagnosed via `wg agency stats`. Too few roles → low scores on specialized tasks; too many → roles with zero assignments. Fixed via `wg evolve`. |
| **evolve** | Evaluate completed work, then mutate roles and motivations to produce better agents. The execute → evaluate → evolve cycle. Autopoietic: the system produces the components (agent definitions) that produce the system (task completions). Strategies: mutation, crossover, gap-analysis, retirement, motivation-tuning. |
| **double-loop** | Don't just retry a failed task with a different agent (single-loop). Change the role definition itself (double-loop). Escalate from single to double loop when the same task type fails repeatedly, evaluation scores plateau, or the role definition doesn't match actual work. From Argyris & Schön (1978). |
| | |
| *Shorthands* | |
| **map-reduce** | Data-parallel diamond variant: planner decomposes → N workers → reducer aggregates. |
| **scaffold** | Platform role produces infrastructure that stream-aligned roles depend on. Graph shape: `setup-ci → [feature-A, feature-B, feature-C]`. |
| **"diamond with specialists"** | The most common compound pattern: the structure is a diamond, the staffing is planner-workers-synthesizer, and the workers are specialists matched to their slice of work. |

---

## Section Plan

### Section 1: System Overview
**File:** `docs/manual/01-overview.typ`

**Purpose:** Establish what workgraph is, what the agency system is, how they relate, and why this combination exists. The reader should finish this section with a clear mental model of the whole before diving into parts.

**Key points to cover:**

1. **What workgraph is.** A task coordination system for humans and AI agents. Work is modeled as a directed graph of tasks connected by dependency edges. The graph is the single source of truth — all state lives in a JSONL file under version control.

2. **The core loop.** Add tasks with dependencies → the scheduler finds ready work → agents (human or AI) are dispatched → completed work unblocks downstream tasks → repeat. This is the heartbeat of any workgraph project.

3. **What the agency system adds.** Without the agency, every AI agent is a generic assistant. The agency gives agents *composable identities* — a role (what they do) paired with a motivation (why they do it that way). Different pairings produce different agents. These identities are immutable (content-hashed), evaluated (scored after task completion), and evolved (improved by an LLM evolver based on performance data).

4. **The full agency loop.** Assign identity → execute task → evaluate results → evolve agency. Each step can be manual or automated. The system is designed to run as a self-improving cycle.

5. **Human and AI agents in the same model.** The identity system is uniform. The only difference is the executor: AI agents receive prompts; human agents receive notifications. Both are tracked, evaluated, and evolved using the same mechanisms. (Human evaluations are excluded from the evolution signal.)

6. **Storage and simplicity.** Everything is files: JSONL for the graph, YAML for agency entities, TOML for configuration. No database, no server dependency. The service daemon is optional — you can run workgraph purely from the CLI.

7. **Trace and organizational memory.** The operations log records every mutation to the graph. This trace is the project's memory — queryable via `wg trace`, exportable for sharing, and extractable into reusable workflow templates (trace functions). Mention briefly; detailed treatment in §2 (trace functions) and §4 (event stream).

8. **External integration.** External systems can observe workgraph via the event stream (`wg watch --json`), translate events, and inject information through five ingestion points: evaluations, tasks, context (trace import), state changes, and observations. This adapter pattern enables CI bots, portfolio tools, and peer collaboration without tight coupling. Mention briefly; detailed treatment in §4.

9. **Task visibility.** Tasks carry a visibility field (`internal`, `public`, or `peer`) that controls what information crosses organizational boundaries during trace exports. This is the boundary mechanism for sharing — relevant to both the task graph (§2) and federation (§3).

10. **Federation.** Agency entities can be shared across projects via federation — pulling proven roles and motivations from one project into another. Content-hash IDs make deduplication automatic. Mention briefly; detailed treatment in §3 and §5.

**Cross-references:** Forward-references to Section 2 (task graph, trace functions), Section 3 (agency model, federation), Section 4 (coordination, event stream), Section 5 (external evaluation sources).

**Tone notes:** This section should feel like an invitation. Brief, confident, no hedging. Establish the *why* before the *how*.

---

### Section 2: The Task Graph
**File:** `docs/manual/02-task-graph.typ`

**Purpose:** Deep understanding of the graph model — tasks, statuses, dependencies, structural cycles, readiness, and emergent patterns. The reader should finish this section able to design a workgraph for any project.

**Key points to cover:**

1. **Tasks as nodes.** The anatomy of a task: ID (auto-generated slug from title), title, description, status, estimates, tags, skills, inputs, deliverables, artifacts, and visibility. Explain what each field is *for* — not just what it contains. Include the `visibility` field (`internal`/`public`/`peer`) in the field table — it controls what crosses organizational boundaries during trace exports. Emphasize that tasks are the atoms of work; everything else is structure around them.

2. **Status and lifecycle.** The six statuses and their transitions. Open → InProgress → Done is the happy path. Failed → retry → Open is the recovery path. Abandoned is the escape hatch. Blocked is rarely used explicitly because blocking is derived from dependencies. Draw the state machine clearly.

3. **Terminal statuses and their meaning.** Done, Failed, and Abandoned are all *terminal* — they all unblock dependents. This is a deliberate design choice: a failed dependency doesn't freeze the entire graph. The downstream task gets dispatched and can decide what to do with a failed upstream.

4. **Dependencies: after and before.** The `after` list is authoritative. `before` is its inverse, maintained for convenience. A task is blocked (derived) when any of its `after` entries is non-terminal. Transitivity works naturally: if A is after B, and B is after C, then B is not ready while C is not terminal, so A is not ready either.

5. **Readiness.** The four conditions: open status, not paused, past time constraints, all predecessors terminal. Explain `not_before` (scheduling for the future) and `ready_after` (set by cycle delays). Non-existent predecessors are treated as resolved (fail-open).

6. **Structural cycles: intentional repetition.** Why workgraph is a directed graph, not a DAG. Cycles are formed naturally by `after` edges — if A is after C, C is after B, and B is after A, the system detects the cycle automatically using Tarjan's SCC algorithm. The cycle header (entry point) carries a `CycleConfig` with `max_iterations`, an optional guard condition, and an optional delay. Without a `CycleConfig`, cycles are flagged as unconfigured deadlocks. Back-edge exemption allows the header to become ready despite its cycle predecessors not being terminal. Walk through the review-revise cycle example step by step.

7. **Early convergence.** Any agent working on a cycle member can signal `wg done <task> --converged` to stop the cycle, even if iterations remain and guards are satisfied. The `"converged"` tag is placed on the cycle header (regardless of which member the agent completes). The cycle evaluator checks this tag and skips iteration. Use case: a refine agent determines work has converged and doesn't need another iteration. `wg retry` clears the convergence tag. This complements bounded iteration — `max_iterations` is the safety cap, convergence is the intelligent early exit.

8. **Cycle member re-opening.** When all cycle members reach `done` and the cycle hasn't converged or hit `max_iterations`, all members are re-opened with `loop_iteration` incremented. The entire cycle becomes available for re-execution. This replaces the old intermediate-task BFS with precise cycle membership from the SCC analysis.

9. **Emergent patterns.** Fan-out (map): one parent precedes several children. Fan-in (reduce): several tasks precede one aggregator. Pipelines: linear chains. Review loops: structural cycles. These are not built-in primitives — they arise naturally from the dependency graph. Mention that proven patterns can be captured as reusable trace functions (see below).

10. **Trace functions: reusable workflow templates.** When a workflow pattern proves useful, it can be extracted from a completed trace into a reusable template — a trace function. Trace functions capture the task structure, dependencies, and structural cycles of a proven workflow. They are parameterized (feature name, description, files become input variables with typed schemas) and can be applied to create new task graphs following the same pattern. See `wg func extract` and `wg func apply`. Stored in `.workgraph/functions/`. Connect back to emergent patterns — trace functions formalize what starts as ad-hoc structure.

11. **Graph analysis and visualization tools.** Critical path (longest dependency chain), bottlenecks (tasks blocking the most downstream work), impact (what depends on a task), cost (total including dependencies), forecast (projected completion). Include `wg viz --graph` which produces a 2D spatial layout using Unicode box-drawing characters, showing tasks positioned by dependency depth. Brief mentions — these are tools, not concepts.

12. **Storage format.** JSONL: one JSON node per line, human-readable, version-control-friendly. Atomic writes with file locking for concurrent safety. The graph file is the canonical state — everything reads from and writes to it.

**Cross-references:** Back-reference to Section 1 (overview). Forward-reference to Section 3 (federation uses visibility for cross-boundary sharing). Forward-reference to Section 4 (how the coordinator uses readiness; convergence in dispatch prompts; event stream). Forward-reference to Section 5 (structural cycles as evaluation points; trace functions connect to replay).

**Tone notes:** This section is the most technical. Be precise but not pedantic. Use the review-revise-loop as a running example to make the abstract concrete.

---

### Section 3: The Agency Model
**File:** `docs/manual/03-agency.typ`

**Purpose:** Explain the identity system — roles, motivations, agents, content-hash IDs, the skill system, capabilities, trust, and how human and AI agents coexist. The reader should finish this section able to design an agency for their own project.

**Key points to cover:**

1. **Why composable identities.** A generic AI assistant is a blank slate — it has no persistent personality, no declared priorities, no way to improve. The agency system solves this by giving agents a *what* (role) and a *why* (motivation). Same role + different motivation = different agent. This combinatorial identity space is the key insight.

2. **Roles.** What an agent does. Fields: name, description, skills (what it can do), desired outcome (what good output looks like). The description and skills and desired outcome are *identity-defining* — they determine the content-hash ID. Name and performance are mutable metadata.

3. **Motivations.** Why an agent acts the way it does. Fields: name, description, acceptable trade-offs (what it may sacrifice), unacceptable trade-offs (hard constraints). Description and trade-offs are identity-defining. Motivations shape *how* the agent approaches work — a "Careful" motivation produces different behavior than a "Fast" motivation on the same role.

4. **Agents: the pairing.** An AI agent is `(role_id, motivation_id)` — its ID is the content-hash of that pair. A human agent may omit role and motivation. The agent struct also carries operational fields: capabilities (for routing), rate (for cost), capacity (for parallelism), trust level (for priority), contact info, and executor (for dispatch).

5. **Content-hash IDs.** SHA-256 of identity-defining fields. Three properties: deterministic (same content → same ID), deduplicating (can't create duplicates), immutable (changing identity creates a *new* entity). Displayed as 8-character prefixes. Why this matters: it makes identity a mathematical fact, not an administrative convention. You can verify that two agents are using the same role by comparing hashes.

6. **The skill system.** Four reference types: Name (label), File (read from disk), Url (fetch via HTTP), Inline (embedded). Skills are resolved at dispatch time and injected into the prompt as markdown sections. Resolution failures warn but don't block. The difference between role skills (prompt injection: *instructions*) and agent capabilities (routing: *matching tags*).

7. **Trust levels.** Verified, Provisional (default), Unknown. Verified agents get a small priority bonus in task matching. Trust is set on agent creation and can be changed.

8. **Human vs. AI agents.** The executor field distinguishes them: `claude` = AI, `matrix`/`email`/`shell` = human. Human agents don't need roles or motivations — they bring their own judgment. Both types are tracked and evaluated uniformly. Human agent evaluations are excluded from evolution to prevent the system from trying to "improve" humans.

9. **Creating an agency from scratch.** The `wg agency init` command seeds four starter roles (Programmer, Reviewer, Documenter, Architect) and four starter motivations (Careful, Fast, Thorough, Balanced). Then pair them into agents. Walk through the setup process.

10. **Task-agent matching.** `wg match` compares a task's required skills against agents' capabilities. Scores by match count + trust bonus. This is used by the auto-assign system and can be used manually.

11. **Federation: sharing across projects.** Agency entities can be shared between workgraph projects via federation. `wg agency scan <remote>` discovers entities in another project's agency store. `wg agency pull <remote>` imports entities (roles, motivations, agents, and their evaluations) from a remote store. `wg agency push <remote>` exports local entities to a remote store. Named remotes are stored in `.workgraph/federation.yaml` and managed via `wg agency remote add/list/remove`. Performance records are merged during transfer: evaluations are deduplicated by task ID + timestamp, and average scores are recalculated. Content-hash IDs make federation natural — the same entity has the same ID everywhere, so deduplication is automatic. Lineage is preserved across federation — ancestry chains remain intact. Mention that task visibility (§2) controls what trace data crosses boundaries, while federation controls what agency data crosses boundaries.

12. **Evaluation source and external signals.** Evaluations carry a `source` field that distinguishes internal assessments (`"llm"`) from external signals (`"outcome:sharpe"`, `"ci:test-suite"`, `"vx:peer-id"`). This is architecturally significant — it connects the agency model to external systems that produce outcome data. Brief mention here with forward reference to §5 for full treatment of how the evolver weighs different sources.

**Cross-references:** Back-reference to Section 1 (agency overview). Back-reference to Section 2 (task visibility as the task-level boundary mechanism). Forward-reference to Section 4 (how agents are dispatched). Forward-reference to Section 5 (how agents are evaluated and evolved; federation as data source for evolution; evaluation source weighting).

**Tone notes:** Emphasize the design philosophy — why content-hashing, why immutability, why the role/motivation split. These are choices, not accidents. Federation should feel like a natural extension of content-hashing — if identity is a mathematical fact, sharing it is trivial.

---

### Section 4: Coordination & Execution
**File:** `docs/manual/04-coordination.typ`

**Purpose:** Explain how work gets dispatched and monitored — the service daemon, the coordinator loop, agent spawning, parallelism, the full dispatch cycle, auto-assign, auto-evaluate, and dead agent handling. The reader should finish this section able to run a multi-agent project.

**Key points to cover:**

1. **The service daemon.** A background process started by `wg service start`. It hosts the coordinator, listens on a Unix socket for IPC, and manages agent lifecycle. Optional — workgraph works without it, but the daemon automates dispatch. Agents are detached from the daemon (via `setsid()`) and survive restarts.

2. **The coordinator tick.** The six-phase heartbeat: (1) reap zombie processes, (2) clean up dead agents and count alive slots, (3) create auto-assign meta-tasks if enabled, (4) create auto-evaluate meta-tasks if enabled, (5) save graph if modified and find ready tasks, (6) spawn agents for ready tasks up to available slots. Two triggers: IPC-driven (immediate, reactive) and safety-net poll (background timer, catches manual edits).

3. **The dispatch cycle in detail.** For each ready task: resolve executor (shell for `exec` tasks, agent's executor otherwise, fallback to config default) → resolve model (task.model > coordinator.model > agent.model) → build context from completed dependencies (their artifacts + recent logs) → render prompt template with identity and skills → generate wrapper script → claim task atomically → fork detached process → register in agent registry. Emphasize the claim-before-spawn ordering that prevents double-dispatch. Mention that for tasks that are members of structural cycles, the rendered prompt includes a note about the `--converged` flag, informing the agent that it can break the cycle early if the work has reached a stable state.

4. **The wrapper script.** What `run.sh` does: unsets environment variables for nested sessions, runs the executor command with output capture, checks task status after exit (the agent may have already self-reported), and marks done or failed if the agent didn't. This is the safety net that ensures tasks don't get stuck in-progress after agent death.

5. **Parallelism control.** `max_agents` caps concurrent agents. The coordinator counts truly alive agents (PID check, not just registry status) and only spawns into available slots. Live reconfiguration via `wg service reload --max-agents N`.

6. **Auto-assign.** When enabled, the coordinator creates blocking `assign-{task-id}` meta-tasks for unassigned ready work. An assigner agent (configurable model and identity) evaluates available agents and picks the best fit. Meta-tasks are tagged to prevent recursive auto-assignment.

7. **Auto-evaluate.** When enabled, the coordinator creates `evaluate-{task-id}` meta-tasks blocked by each work task. When the work task completes (or fails), the evaluation task becomes ready. Evaluation tasks use the shell executor to run `wg evaluate`. Human-agent tasks are skipped. Meta-tasks are tagged to prevent recursive evaluation. Evaluations created by auto-evaluate carry `source: "llm"`. External evaluations can be recorded via `wg evaluate record --source <tag>`, allowing the evolver to consider both internal quality assessments and external outcome data (see §5).

8. **Dead agent detection and triage.** Every tick checks PIDs. Dead agents have their tasks unclaimed. With `auto_triage` enabled, an LLM reads the agent's output log and classifies the result: *done* (task was actually completed), *continue* (partial progress, inject recovery context), or *restart* (no meaningful progress). Max-retries is respected.

9. **IPC protocol.** The Unix socket accepts JSON-line commands: `graph_changed`, `spawn`, `agents`, `kill`, `heartbeat`, `status`, `shutdown`, `pause`, `resume`, `reconfigure`. Graph-modifying CLI commands (`wg done`, `wg add`, etc.) automatically send `graph_changed` to wake the coordinator.

10. **Custom executors.** Executors are defined as TOML files in `.workgraph/executors/`. Each specifies command, args, environment, prompt template, working directory, and timeout. The default `claude` executor pipes a prompt file into the Claude CLI. Custom executors enable integration with any tool.

11. **Pause, resume, and manual control.** The coordinator can be paused (no new spawns, running agents continue) and resumed. `wg service tick` runs a single coordinator tick for debugging. `wg spawn` dispatches a single task manually without the daemon.

12. **Observing the system: `wg watch` (event stream).** `wg watch --json` streams a real-time event feed of graph mutations. Events are typed: `task.created`, `task.started`, `task.completed`, `task.failed`, `task.retried`, `evaluation.recorded`, `agent.spawned`, `agent.completed`. Events can be filtered by category (`--filter task_state`) or by task ID prefix. The event stream reads from the same operations log that records all graph mutations. This enables the adapter pattern: a CI integration, a Slack bot, or a portfolio management tool can observe workgraph events and react without polling. Describe the generic adapter cycle: observe → translate → ingest → react. Connect back to the five ingestion points mentioned in §1.

13. **Replay.** `wg replay` re-executes previously completed or failed work by creating an immutable snapshot of the current graph state and selectively resetting tasks. Criteria for reset: `--failed-only`, `--below-score <threshold>`, explicit task list, or `--subgraph <root>`. Dependents in the transitive closure are also reset. `--plan-only` previews what would be reset. Snapshots are stored in `.workgraph/runs/`. Connect to trace functions (§2) — replay re-executes existing work, trace functions template new work from proven patterns.

**Cross-references:** Back-reference to Section 2 (readiness; convergence and structural cycles). Back-reference to Section 3 (agent identity injection; federation). Forward-reference to Section 5 (evaluation feeding evolution; evaluation source). Forward-reference to Section 1 (event stream enables external integration pattern).

**Tone notes:** This is operational prose. Walk through the dispatch cycle as a narrative, not a bullet list. Make the reader *see* what happens when `wg service start` runs and a task becomes ready.

---

### Section 5: Evolution & Improvement
**File:** `docs/manual/05-evolution.typ`

**Purpose:** Explain how the agency improves over time — evaluation, evolution strategies, lineage, performance aggregation, the synergy matrix, and the autopoietic nature of the system. The reader should finish this section understanding how to run the improvement cycle and what to expect from it.

**Key points to cover:**

1. **The agency as a living system.** The full loop: assign → execute → evaluate → evolve → assign (improved). Each step feeds the next. The system is designed to learn from its own performance data and produce better agent identities over time. This is not magic — it is a structured feedback loop with human oversight at the evolution step.

2. **Evaluation in depth.** What the evaluator sees: task definition, agent identity, artifacts, log entries, timing. What it scores: correctness (40%), completeness (30%), efficiency (15%), style adherence (15%). How scores are computed: weighted average to a single 0.0–1.0 score. How scores propagate: to the agent, the role (with motivation as context), and the motivation (with role as context). This three-level propagation creates the data needed for synergy analysis. Every evaluation carries a `source` field (see below).

3. **The performance record.** Each entity maintains: task count, average score, and a list of evaluation references. The `context_id` on each evaluation reference enables cross-cutting analysis: "how does this role perform with different motivations?" and vice versa.

4. **The synergy matrix.** A performance cross-reference computed from context IDs. For every (role, motivation) pair with evaluations, the matrix shows average score and count. High-synergy pairs (score >= 0.8) and low-synergy pairs (score <= 0.4) are flagged. Under-explored combinations (fewer than N evaluations) are surfaced for experimentation.

5. **Trend indicators.** `wg agency stats` computes trends by comparing first and second halves of recent scores: improving, declining, or flat. These trends guide evolution decisions.

6. **Evolution strategies.** Six approaches the evolver can use: *Mutation* (modify one existing role to improve weak dimensions), *Crossover* (combine traits from two high-performing roles), *Gap analysis* (create entirely new roles/motivations for unmet needs), *Retirement* (remove consistently poor performers), *Motivation tuning* (adjust trade-offs on existing motivations), and *All* (use all strategies as appropriate). Each strategy has optional guidance documents in `.workgraph/agency/evolver-skills/`.

7. **How evolution works mechanically.** The evolver agent receives: performance summaries, strategy instructions, budget constraints, retention heuristics, and its own identity (if configured). It outputs structured JSON operations: create_role, modify_role, create_motivation, modify_motivation, retire_role, retire_motivation. Operations are applied to the agency storage. Modified entities get new content-hash IDs with lineage metadata linking to their parents.

8. **Safety guardrails.** The last remaining role or motivation cannot be retired. Retired entities are preserved as `.yaml.retired` files, not deleted. `--dry-run` shows the evolver's plan without applying. `--budget` limits operations per run. Self-mutations (evolver changing its own role/motivation) are deferred to human review via a workgraph task with verification requirements.

9. **Lineage tracking.** Every role, motivation, and agent records: parent IDs (none for manual creation, one for mutation, two+ for crossover), generation number (0 for manual, incrementing), creator identity (`"human"` or `"evolver-{run_id}"`), and timestamp. Lineage can be walked with `wg role lineage`, `wg motivation lineage`, `wg agent lineage`. Content-hash IDs make lineage unfalsifiable — the original entity is never modified, only new children are created.

10. **The autopoietic dimension.** The meta-agents (assigner, evaluator, evolver) are themselves agency entities with roles and motivations. They can be evaluated and evolved. The evolver can propose changes to the assigner or evaluator. Self-mutations of the evolver itself require human approval. This creates a system that can improve not just its workers but its coordination mechanisms — subject to human oversight at every evolution step. Evolution is intentionally kept as a manual trigger (`wg evolve`), not automated, because the human decides when there is enough evaluation data to act on.

11. **External evaluation sources.** Every evaluation carries a `source` field. Internal auto-evaluations have `source: "llm"`. External evaluations can be recorded via `wg evaluate record --task <id> --source <tag> --score <0.0-1.0>`. Source tags are freeform strings: `"outcome:sharpe"`, `"ci:test-suite"`, `"user:feedback"`, `"vx:peer-123"`. The evolver reads all evaluations regardless of source — it sees both "the LLM evaluator thought the code was good" and "the market said the portfolio was mediocre." This enables richer evolution signals: internal quality + external outcomes. Walk through an example: an agent scores 0.91 on internal evaluation (clean code) but 0.72 on outcome evaluation (poor market performance). The evolver sees the gap and proposes a domain-specific improvement.

12. **Federation as a data source for evolution.** Agency federation (`wg agency pull`) can import evaluation data from other projects. When evaluations are transferred, they merge with local performance records — deduplicating by task ID and timestamp, recalculating averages. This means the evolver can consider performance data from the broader organizational context, not just the current project. Connect to §3 for federation mechanics.

13. **Practical guidance.** When to evolve: after accumulating enough evaluations (at least 5-10 per role). How to use `--budget`: start small (2-3 operations), review results, iterate. How to use `--dry-run`: always preview first. How to seed: `wg agency init` for starters, then evolve. How to experiment: use the under-explored combinations from `wg agency stats` as hypotheses.

**Cross-references:** Back-reference to Section 3 (roles, motivations, content-hash IDs; federation mechanics). Back-reference to Section 4 (auto-evaluate, auto-assign creating the data pipeline; evaluation source in auto-evaluate; event stream enabling external evaluations). Back-reference to Section 2 (structural cycles as natural evaluation points for iterative tasks).

**Tone notes:** This section should feel like watching a system learn. The prose should build from the concrete (one evaluation) to the systemic (the full improvement cycle). End with the philosophical point: this is a system that can describe and improve itself, but always with a human hand on the wheel.

---

## Cross-Reference Map

| From | To | Nature |
|------|----|--------|
| 01-overview | 02-task-graph | "The graph model is detailed in §2" |
| 01-overview | 03-agency | "The agency system is detailed in §3" |
| 01-overview | 04-coordination | "Coordination and dispatch are detailed in §4" |
| 01-overview | 05-evolution | "External evaluation sources are detailed in §5" |
| 02-task-graph | 03-agency | "Task visibility controls what federation shares — §3" |
| 02-task-graph | 04-coordination | "How the coordinator uses readiness — §4; convergence in dispatch prompts — §4" |
| 02-task-graph | 05-evolution | "Structural cycles as natural evaluation points — §5; trace functions connect to replay — §4" |
| 03-agency | 04-coordination | "How agents are dispatched — §4" |
| 03-agency | 05-evolution | "How agents are evaluated and evolved — §5; federation data enriches evolution — §5" |
| 03-agency | 02-task-graph | "Task visibility as the task-level boundary mechanism — §2" |
| 04-coordination | 01-overview | "Event stream enables external integration pattern — §1" |
| 04-coordination | 02-task-graph | "Readiness calculation — §2; convergence and structural cycles — §2" |
| 04-coordination | 03-agency | "Agent identity injection — §3; federation — §3" |
| 04-coordination | 05-evolution | "Auto-evaluate creates the data pipeline for §5; evaluation source — §5" |
| 05-evolution | 03-agency | "Roles, motivations, content-hash IDs — §3; federation mechanics — §3" |
| 05-evolution | 04-coordination | "Auto-evaluate and auto-assign — §4; event stream enabling external evaluations — §4" |
| 05-evolution | 02-task-graph | "Structural cycles as evaluation points — §2" |

---

## Terminology Consistency Rules

These rules apply to ALL writers across all five sections:

1. **Never say "DAG."** Workgraph is a directed graph, not necessarily acyclic. Loop edges create intentional cycles. Use "directed graph" or "task graph."

2. **"Ready" has a precise meaning.** A task is ready if and only if it satisfies the four readiness conditions (see glossary). Do not use "ready" loosely to mean "available" or "waiting."

3. **"Blocked" is derived, not assigned.** A task is blocked when its dependencies are non-terminal. Do not conflate with the rarely-used explicit `Blocked` status.

4. **"Terminal" means done, failed, or abandoned.** All three unblock dependents. Do not say "completed" when you mean "terminal" — failed tasks are terminal but not completed.

5. **"Agent" means the agency entity, not the running process.** The running process is an "agent process" or simply "process." The identity is the "agent." When in doubt, say "agent identity" for the entity and "spawned agent" or "running agent" for the process.

6. **"Assign" vs. "claim."** Assigning binds an identity to a task. Claiming marks a task as in-progress. These are different operations. The coordinator claims; the agency system (or user) assigns.

7. **"Dispatch" means the full cycle.** Claim + spawn + register. Do not use "dispatch" for just one step.

8. **Content-hash, not content hash.** Hyphenated when used as a compound modifier.

9. **Use "role" and "motivation" as nouns, never as adjectives.** Say "the agent's role" not "the role agent." Say "the motivation's trade-offs" not "the motivation trade-offs."

10. **"Evolution" is the process; "evolve" is the command.** Do not use "evolution" as a verb. Say "run evolution" or "run `wg evolve`."

11. **"Structural cycle" for the current system; "loop edge" only in historical context.** The codebase now uses `CycleConfig` and `CycleAnalysis`. Use "structural cycle" for the current model. "Loop edge" (`loops_to`) refers to the old system and should only appear in historical context or migration documentation. "Back-edge" is acceptable when referring to the specific edges within a cycle that create the cycle structure.

12. **"Meta-task" for auto-created coordinator tasks.** Assignment tasks, evaluation tasks, and evolution review tasks are meta-tasks. Regular work is just "tasks."

---

## File Inventory

| File | Section | Primary Writer Responsibility |
|------|---------|-------------------------------|
| `docs/manual/PLAN.md` | — | This document (plan). Not part of the manual. |
| `docs/manual/01-overview.typ` | System Overview | Establish the whole before the parts. ~1500-2000 words. |
| `docs/manual/02-task-graph.typ` | The Task Graph | Deep graph model. ~3000-4000 words. |
| `docs/manual/03-agency.typ` | The Agency Model | Identity system. ~2500-3500 words. |
| `docs/manual/04-coordination.typ` | Coordination & Execution | Operational mechanics. ~3000-4000 words. |
| `docs/manual/05-evolution.typ` | Evolution & Improvement | Self-improvement cycle. ~2500-3500 words. |

---

## Notes for Writers

- **Read this entire plan before writing.** Your section will reference and be referenced by others. Understand the whole.
- **Use the glossary terms exactly.** If you need a term that isn't in the glossary, use plain language and flag it for review.
- **Write prose, not documentation.** Each section is an essay. Bullet lists are for summaries and tables, not for primary exposition. Use paragraphs.
- **Include diagrams.** ASCII art or typst-native diagrams. A good diagram replaces three paragraphs. Include at least one per major concept.
- **Example-driven.** Walk through concrete examples. The review-revise loop, the CI retry pipeline, the four-role agency — these should recur across sections to build familiarity.
- **No CLI reference.** This is a conceptual manual. Mention commands where they illustrate concepts (`wg evolve --dry-run` shows the safety mindset), but do not enumerate all flags. The existing `docs/COMMANDS.md` serves as the CLI reference.
- **Target length is guidance, not constraint.** Write what the section needs. If the overview is tight at 1200 words, that's fine. If the task graph section needs 4500, that's fine too.
- **Typst format.** Use `= Heading`, `== Subheading`, `#figure`, `#table`, `@label` cross-references. Each file should compile standalone with `typst compile docs/manual/0N-section.typ`.
