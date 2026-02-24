# Manual & README Update Specification

**Date:** 2026-02-21
**Purpose:** Structured spec for writers to update each manual section, the README, and supporting docs against recent features.

---

## Recent Features to Audit Against

These features were implemented in the most recent commits and are not yet documented in the manual or README:

| Feature | Design Doc | Implementation |
|---------|-----------|----------------|
| `Evaluation.source` field | `docs/design/vx-integration-response.md` §8 | `src/agency.rs:229-234` |
| `Task.visibility` field (`internal`/`public`/`peer`) | `docs/design/vx-integration-response.md` §6 | `src/graph.rs:233-241` |
| `wg watch --json` (event stream) | `docs/design/vx-integration-response.md` §1 | `src/commands/watch.rs` |
| `wg trace export --visibility <zone>` | `docs/design/vx-integration-response.md` §6 | `src/commands/trace_export.rs` |
| `wg trace import` | `docs/design/vx-integration-response.md` §6 | `src/commands/trace_import.rs` |
| `wg trace show --animate` | — | `src/commands/trace_animate.rs` |
| `wg done --converged` (loop convergence) | `docs/design/loop-convergence.md` | `src/commands/done.rs`, `src/graph.rs` |
| `wg trace extract` / `wg trace instantiate` (trace functions) | `docs/design/trace-functions.md` | `src/commands/func_extract.rs`, `src/commands/func_apply.rs` |
| `wg viz --graph` (2D spatial layout) | — | `src/commands/viz.rs:952+` |
| Agency federation (`wg agency scan/pull/push/remote/merge`) | `docs/AGENCY.md` (not covered there either) | `src/federation.rs` |
| External information flows / adapter pattern | `docs/design/vx-integration-response.md` §1-4 | various |

---

## Section 1: System Overview (`01-overview.typ`)

### Currently Covered
- What workgraph is (graph-based task coordination)
- The core loop (define → dispatch → execute → complete)
- The agency loop (assign → execute → evaluate → evolve)
- How graph and agency relate (coordinator at intersection)
- Human and AI agents in the same model
- Storage model (JSONL, YAML, TOML, files-only)

### Missing or Outdated
1. **No mention of external information flows.** The overview doesn't mention how external systems integrate with workgraph (the five ingestion points: evaluation, task, context, state, observation).
2. **No mention of `wg watch`.** The observation/event-stream capability is a significant addition that enables external adapters — it belongs in the overview as part of "how workgraph communicates outward."
3. **No mention of trace system.** The trace/provenance system (`wg trace`, `wg trace export`, `wg trace import`, trace functions) is a major capability for organizational memory and workflow reuse. The overview should at least mention it exists.
4. **No mention of federation.** Agency federation (sharing roles, motivations, agents across projects) is a new cross-cutting capability.
5. **No mention of task visibility.** The three-zone visibility model (`internal`/`public`/`peer`) is architecturally significant — it defines what crosses organizational boundaries.

### Specific Additions Needed
1. **Add a paragraph or subsection after "How They Relate" on external integration.** Briefly describe the five ingestion points pattern: external systems observe via `wg watch --json`, translate, and inject via evaluations, tasks, context (trace import), or state changes. Keep it brief — this is an overview, not the full architecture.
2. **Add a brief mention of trace and organizational memory** in the "Storage and simplicity" concept area. One or two sentences: "The operations log (`operations.jsonl`) records every mutation to the graph. This trace is the project's memory — queryable via `wg trace`, exportable for sharing, and extractable into reusable workflow templates."
3. **Add a sentence about federation** in the agency section: "Agency entities can be shared across projects via federation — pulling proven roles and motivations from one project into another."
4. **Add a sentence about task visibility** where appropriate: "Tasks carry a visibility field (`internal`, `public`, or `peer`) that controls what information crosses organizational boundaries during trace exports."

---

## Section 2: The Task Graph (`02-task-graph.typ`)

### Currently Covered
- Tasks as nodes (full field table including `model`, `verify`, `agent`, `exec`)
- Status and lifecycle (state machine diagram, all six statuses)
- Terminal statuses unblock design choice
- Dependencies (`blocked_by` and `blocks`)
- Readiness (four conditions)
- Loop edges (`loops_to`, guards, max_iterations, delays)
- Intermediate task re-opening
- Bounded iteration
- Pause and resume
- Emergent patterns (fan-out, fan-in, pipelines, review loops)
- Graph analysis tools (critical path, bottlenecks, impact, cost, forecast)
- Storage format (JSONL)

### Missing or Outdated
1. **`visibility` field missing from the task field table.** The `visibility` field (`internal`/`public`/`peer`) is not listed in the task anatomy table at the top of the section.
2. **`--converged` flag not mentioned.** The section on "Bounded Iteration" discusses `max_iterations` but does not mention the `wg done --converged` mechanism that lets agents break loops early. This is significant because it was a real pain point (loops running to max even after convergence).
3. **Trace functions not mentioned.** The "Emergent Patterns" subsection describes map/reduce, pipelines, and review loops — but doesn't mention that patterns can now be extracted into reusable trace functions via `wg trace extract` and re-instantiated via `wg trace instantiate`.
4. **`wg viz --graph` not mentioned.** The graph analysis section mentions `wg viz` but doesn't describe the new 2D spatial layout format.

### Specific Additions Needed
1. **Add `visibility` to the task field table:**
   ```
   [`visibility`], [Controls what information crosses organizational boundaries during trace exports. One of `internal` (default, org-only), `public` (sanitized sharing), or `peer` (richer detail for trusted peers).]
   ```
2. **Add a subsection or paragraph on loop convergence** after "Bounded Iteration" (around line 221). Title: "Early Convergence." Content: Explain that an agent can signal `wg done <task> --converged` to prevent loop edges from firing, even if iterations remain and guards are satisfied. The task gets a `"converged"` tag. The loop evaluator checks this tag and skips firing. Use case: a refine agent determines work has converged and doesn't need another iteration. Mention that `wg retry` clears the convergence tag.
3. **Add a brief mention of trace functions** at the end of "Emergent Patterns." One paragraph: "When a workflow pattern proves useful, it can be extracted from a completed trace into a reusable template — a _trace function_. Trace functions capture the task structure, dependencies, and loop edges of a proven workflow. They are parameterized (feature name, description, files become input variables) and can be instantiated to create new task graphs following the same pattern. See `wg trace extract` and `wg trace instantiate`."
4. **Mention `wg viz --graph` in the graph analysis section.** Add to the existing `wg viz` reference: "The `--graph` format produces a 2D spatial layout using Unicode box-drawing characters, showing tasks positioned by their dependency depth."

---

## Section 3: The Agency Model (`03-agency.typ`)

### Currently Covered
- Composable identities (role × motivation)
- Roles (description, skills, desired outcome)
- Motivations (description, acceptable/unacceptable trade-offs)
- Agents: the pairing (operational fields: capabilities, rate, capacity, trust, contact, executor)
- Content-hash IDs (deterministic, deduplicating, immutable)
- The skill system (Name, File, Url, Inline; resolution)
- Trust levels (Verified, Provisional, Unknown)
- Human vs. AI agents (executor distinction)
- Composition in practice (`wg agency init`, starter roles/motivations)
- Lineage and deduplication

### Missing or Outdated
1. **No mention of federation.** The agency model section doesn't discuss sharing agency entities across projects. Federation is a significant capability — `wg agency scan`, `wg agency pull`, `wg agency push`, named remotes, performance merge.
2. **No mention of `Evaluation.source` field.** The section references evaluation (forward ref to §5) but doesn't mention that evaluations now carry a `source` field distinguishing internal LLM evaluations from external signals.

### Specific Additions Needed
1. **Add a subsection on agency federation** after "Lineage and Deduplication" (or as a new final subsection before "Cross-References"). Title: "Federation: Sharing Across Projects." Content:
   - Agency entities can be shared between workgraph projects via federation.
   - `wg agency scan <path>` discovers what roles, motivations, and agents exist in another project's agency store.
   - `wg agency pull <remote>` imports entities (roles, motivations, agents, and their evaluations) from a remote store into the local project.
   - `wg agency push <remote>` exports local entities to a remote store.
   - Named remotes are stored in `.workgraph/federation.yaml` and managed via `wg agency remote add/list/remove`.
   - Performance records are merged during transfer: evaluations are deduplicated by task ID + timestamp, and average scores are recalculated.
   - Content-hash IDs make federation natural: the same entity has the same ID everywhere, so deduplication is automatic.
   - Mention that lineage is preserved across federation — you can trace an entity's ancestry even when it was pulled from another project.
2. **Mention `Evaluation.source`** briefly in the context of how evaluations connect to the agency model. One sentence in the cross-references or in the evaluation forward reference: "Evaluations carry a `source` field that distinguishes internal assessments (`\"llm\"`) from external signals (`\"outcome:sharpe\"`, `\"vx:peer-id\"`), enabling the evolver to weigh different kinds of performance data."

---

## Section 4: Coordination & Execution (`04-coordination.typ`)

### Currently Covered
- The service daemon (background process, Unix socket, PID, detached agents)
- The coordinator tick (six phases, diagram)
- The dispatch cycle (executor resolution, model resolution, context building, prompt rendering, wrapper script, claim-before-spawn, fork, register)
- The wrapper script (safety net for agents that die without reporting)
- Parallelism control (`max_agents`, live reconfiguration)
- Map/reduce patterns
- Auto-assign (two-phase dispatch via `assign-{task-id}` meta-tasks)
- Auto-evaluate (evaluation meta-tasks, shell executor, exclusions)
- Dead agent detection and triage (three verdicts: done, continue, restart)
- IPC protocol (full command table)
- Custom executors (TOML files)
- Pause, resume, and manual control
- End-to-end walkthrough

### Missing or Outdated
1. **No mention of `wg watch --json`.** This is a significant new capability for observing the coordinator's actions from external systems. It reads from the operations log and streams events as JSON.
2. **No mention of convergence signal in the coordinator.** The coordinator injects a note about `--converged` into prompts for tasks that have `loops_to` edges. This belongs in the dispatch cycle or wrapper script discussion.
3. **No mention of `Evaluation.source` in auto-evaluate.** Auto-evaluate creates evaluations with source `"llm"`. External evaluations can be recorded via `wg evaluate record --source <tag>`. This is relevant to how the evaluation pipeline works.

### Specific Additions Needed
1. **Add a subsection on `wg watch` (event stream)** after "IPC Protocol" or as a new subsection. Title: "Observing the System: `wg watch`." Content:
   - `wg watch --json` streams a real-time event feed of graph mutations.
   - Events are typed: `task.created`, `task.started`, `task.completed`, `task.failed`, `task.retried`, `evaluation.recorded`, `agent.spawned`, `agent.completed`.
   - Events can be filtered by type (`--filter task_state`) or by task ID.
   - The event stream reads from the same operations log that records all graph mutations.
   - This enables external adapters: a CI integration, a Slack bot, or a portfolio management tool can observe workgraph events and react without polling.
   - Mention the generic adapter pattern: observe → translate → ingest → react.
2. **Add a mention of convergence in the dispatch section.** In the prompt rendering step: "For tasks that are the source of loop edges, the rendered prompt includes a note about the `--converged` flag, informing the agent that it can break the loop early if the work has reached a stable state."
3. **Mention `Evaluation.source`** in the auto-evaluate discussion: "Evaluations created by auto-evaluate carry `source: \"llm\"`. External evaluations can be recorded via `wg evaluate record --source <tag>`, allowing the evolver to consider both internal quality assessments and external outcome data."

---

## Section 5: Evolution & Improvement (`05-evolution.typ`)

### Currently Covered
- Evaluation (four dimensions, weights, evaluator context)
- Three-level propagation (agent, role with context, motivation with context)
- What gets evaluated (done and failed, human exclusion)
- Performance records and aggregation
- Synergy matrix
- Trend indicators
- Evolution (manual trigger, evolver agent, six strategies)
- Mechanics (how `wg evolve` runs)
- How modified entities are born (new content-hash, lineage)
- Safety guardrails (last entity protection, retired preservation, dry-run, budget, self-mutation deferral)
- Lineage (generation tracking, audit trail)
- The autopoietic loop (full system diagram)
- Practical guidance

### Missing or Outdated
1. **No mention of `Evaluation.source` and external signals.** The section on evaluation doesn't mention that evaluations can come from external sources (not just the internal LLM evaluator). The `source` field is architecturally significant — it's how VX portfolio scores, CI results, or user feedback enter the system.
2. **No mention of federation's role in evolution.** Federation can pull evaluation data from other projects, enriching the performance summary the evolver sees.
3. **No mention of the five ingestion points pattern.** The evolution section should mention that the evolver can consider diverse signal sources — not just internal auto-evaluations.

### Specific Additions Needed
1. **Add a subsection or paragraph on external evaluation sources** after "What gets evaluated" or within the evaluation subsection. Title: "External Evaluation Sources." Content:
   - Every evaluation carries a `source` field. Internal auto-evaluations have `source: "llm"`. External evaluations can be recorded via `wg evaluate record --task <id> --source <tag> --score <0.0-1.0>`.
   - Source tags are freeform strings: `"outcome:sharpe"`, `"ci:test-suite"`, `"user:feedback"`, `"vx:peer-123"`.
   - The evolver reads all evaluations regardless of source. It sees both "the LLM evaluator thought the code was good" and "the market said the portfolio was mediocre."
   - This enables a richer signal for evolution: internal quality + external outcomes.
   - Walk through an example: an agent scores 0.91 on internal evaluation (clean code) but 0.72 on outcome evaluation (poor market performance). The evolver sees the gap and proposes a domain-specific improvement.
2. **Mention federation as a data source for evolution.** One paragraph: "Agency federation (`wg agency pull`) can import evaluation data from other projects. When evaluations are transferred, they merge with local performance records — deduplicating by task ID and timestamp, recalculating averages. This means the evolver can consider performance data from the broader organizational context, not just the current project."

---

## Glossary (in `PLAN.md` and `workgraph-manual.typ`)

### Terms Currently Defined
The glossary has 30 terms covering: task, status, dependency, blocked_by, blocks, ready, loop edge, guard, loop iteration, resource, role, motivation, agent, agency, content-hash ID, capability, skill, skill resolution, trust level, executor, coordinator, service/service daemon, tick, dispatch, claim, assignment, auto-assign, auto-evaluate, evaluation, performance record, evolution, strategy, lineage, generation, synergy matrix, meta-task, map/reduce pattern, triage, wrapper script.

### New Terms Needed

| Term | Proposed Definition |
|------|-------------------|
| **visibility** | A field on each task controlling what information crosses organizational boundaries during trace exports. Three values: _internal_ (default, org-only — nothing is shared), _public_ (sanitized sharing — task structure without agent output or logs), _peer_ (richer detail for credentialed peers — includes evaluations and patterns). |
| **convergence** | An agent-driven signal (`wg done --converged`) indicating that a loop's iterative work has reached a stable state. When the source task carries the `"converged"` tag, loop edges do not fire — even if iterations remain and guard conditions are met. Cleared on retry. |
| **trace** | The operations log recording every mutation to the graph. The project's organizational memory, queryable via `wg trace`, exportable with visibility filtering via `wg trace export`, and importable from peers via `wg trace import`. |
| **trace export** | A filtered, shareable snapshot of the trace. Visibility filtering controls what is included: `internal` exports everything, `public` sanitizes (no agent output, no logs), `peer` provides richer detail for trusted peers. The interchange format for cross-boundary sharing. |
| **trace function** | A parameterized workflow template extracted from completed traces via `wg trace extract`. Captures task structure, dependencies, loop edges, and input parameters. Instantiated via `wg trace instantiate` to create new task graphs following the same pattern. Stored as YAML in `.workgraph/functions/`. |
| **federation** | The system for sharing agency entities across workgraph projects. Operations: _scan_ (discover entities in a remote store), _pull_ (import from remote to local), _push_ (export from local to remote). Named remotes are stored in `.workgraph/federation.yaml`. Performance records are merged during transfer with deduplication. |
| **remote** | A named reference to another workgraph project's agency store. Managed via `wg agency remote add/list/remove`. Stored in `.workgraph/federation.yaml`. |
| **evaluation source** | A freeform string tag on each evaluation identifying its origin. Default: `"llm"` (internal auto-evaluator). External sources use structured tags: `"outcome:sharpe"`, `"ci:test-suite"`, `"vx:peer-id"`. The evolver reads all sources. |
| **watch** | A real-time event stream (`wg watch --json`) that emits typed events (task.created, task.completed, agent.spawned, etc.) as the graph mutates. Enables external adapters to observe and react without polling. |
| **adapter** | An external tool that translates between an external system's vocabulary and workgraph's ingestion points. The generic pattern: observe (via `wg watch`) → translate → ingest (via `wg` CLI) → react. |

---

## README.md

### Currently Covered
- What workgraph is
- Installation
- Setup (init, add tasks, edit, agent creation, start working)
- Verification workflow
- Using with AI coding assistants (Claude Code, OpenCode, Codex)
- Agentic workflows (4 patterns)
- Service (quickstart, configuration, managing, agents, dead agents, triage, model selection, TUI, troubleshooting)
- Agency system (quickstart, what it does, automation, evolution)
- Graph locking
- Loop edges (how it works, creating, inspecting)
- Key concepts
- Query and analysis commands
- Utilities
- Storage format
- More docs links

### Missing or Outdated

1. **No mention of `wg watch --json`.** Should be listed in the "Query and analysis" section or as a new subsection for external integration.
2. **No mention of trace export/import.** `wg trace export --visibility <zone>` and `wg trace import` are not mentioned anywhere in the README.
3. **No mention of trace functions.** `wg trace extract` and `wg trace instantiate` are not mentioned.
4. **No mention of trace animation.** `wg trace show --animate` is not mentioned.
5. **No mention of `wg viz --graph`.** The README mentions `wg viz` but not the new 2D graph layout format.
6. **No mention of `Task.visibility` field.** Not mentioned in Setup, Key Concepts, or anywhere.
7. **No mention of `Evaluation.source` field.** Not mentioned in Agency section.
8. **No mention of `wg done --converged`.** The loop edges section doesn't mention the convergence mechanism.
9. **No mention of agency federation.** `wg agency scan/pull/push/remote` are not mentioned anywhere.
10. **`docs/AGENCY.md` link** is listed but AGENCY.md itself doesn't cover federation.

### Specific Additions Needed

1. **Add `--converged` to the Loop Edges section.** After "How it works" point 5 (Delay), add:
   ```markdown
   6. **Convergence** — an agent can signal `wg done <task> --converged` to prevent the loop
      from firing, even if iterations remain. Useful when the work has reached a stable state
      before hitting `max_iterations`.
   ```

2. **Add `visibility` to task creation.** In the "Add some tasks" section:
   ```bash
   # Task with visibility for sharing
   wg add "Public API design" --visibility public
   ```

3. **Add a new subsection "Trace & Sharing"** (after "Loop edges" or after "Agency system"):
   ```markdown
   ## Trace & Sharing

   Workgraph records every operation in a trace log. Use it for introspection, sharing, and workflow reuse.

   ### Watching events
   ```bash
   wg watch --json              # stream events as JSON (for external adapters)
   wg watch --json --filter task_state  # only task state changes
   ```

   ### Exporting and importing traces
   ```bash
   wg trace export --visibility public   # sanitized for open sharing
   wg trace export --visibility peer     # richer for trusted peers
   wg trace import peer-export.json      # import a peer's trace
   ```

   ### Trace functions (workflow templates)
   ```bash
   wg trace extract impl-feature --name impl-feature  # extract pattern from completed work
   wg trace instantiate impl-feature --input feature_name=new-thing  # create tasks from pattern
   wg trace list-functions   # list available templates
   ```

   ### Animated trace visualization
   ```bash
   wg trace show <task-id> --animate  # watch the task's execution unfold over time
   ```
   ```

4. **Add `--graph` to viz mentions.** In the "Utilities" section, update:
   ```bash
   wg viz --graph            # 2D spatial layout with Unicode box-drawing
   ```

5. **Add federation to Agency system section.** After the Evolution subsection:
   ```markdown
   ### Federation

   Share agency entities across projects:

   ```bash
   wg agency remote add partner /path/to/other/project/.workgraph/agency
   wg agency scan partner              # see what they have
   wg agency pull partner              # import their roles, motivations, agents
   wg agency push partner              # export yours to them
   ```

   Performance records merge during transfer — evaluations are deduplicated and averages recalculated.
   ```

6. **Add `Evaluation.source` mention.** In the Agency "What it does" list, update item 5:
   ```
   5. **Evaluation** scores completed tasks across four dimensions, with a `source` field
      distinguishing internal LLM assessments from external signals (CI results, outcome data, peer reviews)
   ```

7. **Update "Query and analysis" section.** Add:
   ```bash
   wg watch --json           # real-time event stream for external adapters
   wg trace export           # export trace data for sharing
   wg trace extract <id>     # extract workflow pattern from completed task
   ```

---

## docs/AGENCY.md

### Currently Covered
- Core concepts (Role, Motivation, Agent tables)
- Content-hash IDs
- The full agency loop (manual and automated)
- Lifecycle (create roles/motivations, pair into agents, assign, evaluate, evolve)
- CLI reference (role, motivation, agent, assign, evaluate, evolve, stats)
- Skill system (Name, File, Url, Inline, resolution)
- Evolution (strategies, operations, safety, evolver identity, evolver skills)
- Performance tracking (evaluation flow, records, trends)
- Lineage
- Storage layout
- Configuration reference

### Missing or Outdated

1. **No mention of `Evaluation.source` field.** The evaluation flow section doesn't mention the source field.
2. **No mention of federation.** `wg agency scan`, `wg agency pull`, `wg agency push`, `wg agency remote`, and the `.workgraph/federation.yaml` config are not documented.
3. **No mention of `wg agency merge`.** Merge semantics for performance records during federation.
4. **Storage layout incomplete.** Missing `.workgraph/federation.yaml` and `.workgraph/functions/` directories.

### Specific Additions Needed

1. **Add `Evaluation.source` to the evaluation section.** After the evaluation JSON example:
   ```markdown
   ### Evaluation Sources

   Every evaluation carries a `source` field identifying where the score came from:

   | Source | Meaning |
   |--------|---------|
   | `"llm"` | Internal auto-evaluator (default) |
   | `"outcome:<metric>"` | External outcome data (e.g., `"outcome:sharpe"`) |
   | `"ci:<suite>"` | CI/test results |
   | `"vx:<peer>"` | VX peer evaluation |
   | Custom string | Any freeform source tag |

   Record external evaluations:
   ```bash
   wg evaluate record --task my-task --source "outcome:sharpe" --score 0.72
   ```

   The evolver reads all evaluations regardless of source.
   ```

2. **Add a "Federation" section** after "Lineage" or before "Storage Layout":
   ```markdown
   ## Federation

   Share agency entities across workgraph projects.

   ### Named remotes

   ```bash
   wg agency remote add partner /path/to/other/.workgraph/agency
   wg agency remote list
   wg agency remote remove partner
   ```

   Remotes are stored in `.workgraph/federation.yaml`.

   ### Scanning

   ```bash
   wg agency scan partner    # list entities in remote store
   ```

   ### Pull (import from remote)

   ```bash
   wg agency pull partner                    # pull all entities
   wg agency pull partner --roles-only       # only roles
   wg agency pull partner --dry-run          # preview
   ```

   Pulls roles, motivations, agents, and their evaluations. Deduplicates by content-hash — identical entities are skipped. Performance records are merged: evaluations are deduplicated by task_id + timestamp, and averages are recalculated.

   ### Push (export to remote)

   ```bash
   wg agency push partner                    # push all entities
   wg agency push partner --dry-run          # preview
   ```

   ### How merging works

   - Entities with matching content-hash IDs are deduplicated (same identity = same entity)
   - Performance records merge: evaluations are unioned, duplicates removed by task_id + timestamp
   - Average scores are recalculated from the merged evaluation set
   - Lineage is preserved — ancestry chains remain intact across federation
   ```

3. **Update storage layout** to include federation.yaml:
   ```
   .workgraph/
   ├── federation.yaml              # Named remotes for federation
   ├── agency/
   │   ├── roles/
   │   ├── motivations/
   │   ├── agents/
   │   ├── evaluations/
   │   └── evolver-skills/
   └── functions/                   # Trace functions (workflow templates)
       └── <name>.yaml
   ```

4. **Add CLI reference entries:**
   ```markdown
   ### `wg agency remote`

   | Command | Description |
   |---------|-------------|
   | `wg agency remote add <name> <path>` | Add a named remote |
   | `wg agency remote list` | List remotes |
   | `wg agency remote remove <name>` | Remove a remote |

   ### `wg agency scan`

   ```bash
   wg agency scan <remote>   # list entities in remote store
   ```

   ### `wg agency pull`

   ```bash
   wg agency pull <remote> [--roles-only] [--motivations-only] [--dry-run]
   ```

   ### `wg agency push`

   ```bash
   wg agency push <remote> [--roles-only] [--motivations-only] [--dry-run]
   ```
   ```

---

## Unified Manual (`workgraph-manual.typ`)

The unified manual concatenates all sections. Changes needed:

1. **Update glossary** with the new terms listed above (visibility, convergence, trace, trace export, trace function, federation, remote, evaluation source, watch, adapter).
2. **Sections auto-update** when individual section files are updated — the unified manual `#include`s them. Verify that the include mechanism works after section updates.
3. **No structural changes needed** to the unified manual itself beyond the glossary additions.

---

## Cross-cutting Concerns

### Terminology Consistency
- **"Trace"** is used in two senses: the operations log (provenance) and the `wg trace` command family. Writers should use "operations log" or "trace log" for the raw data, and "trace" for the high-level concept/commands.
- **"Federation"** should always refer to agency federation (sharing entities). Don't confuse with cross-repo task dispatch, which is a separate concept.
- **"Visibility"** is a task field, not a system-wide setting. Each task has its own visibility.
- **"Source"** on evaluations is a freeform string. Don't imply it's an enum or restricted set.

### Diagrams Needed
- The five ingestion points diagram from `vx-integration-response.md` §1 should be adapted for the manual (either §1 overview or §4 coordination).
- A trace export flow diagram: graph → visibility filter → trace export JSON → peer imports.
- A federation flow: project A agency store ←→ federation ←→ project B agency store.

### Order of Operations for Writers
Sections can be updated independently. Recommended order:
1. **Glossary first** (both PLAN.md and workgraph-manual.typ) — new terms need to be defined before writers use them.
2. **Section 2** (task graph) — `visibility` field and `--converged` are foundational.
3. **Section 3** (agency) — federation additions.
4. **Section 4** (coordination) — `wg watch`, convergence in prompts, evaluation source.
5. **Section 5** (evolution) — evaluation source, federation data.
6. **Section 1** (overview) — synthesize after details are settled.
7. **README.md** — update last, drawing from manual text.
8. **docs/AGENCY.md** — update alongside section 3.
