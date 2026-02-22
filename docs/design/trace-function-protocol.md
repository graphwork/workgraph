# Trace-to-Function Extraction Protocol: Design Document

> **Note:** The CLI for function management has moved from `wg trace` to `wg func`. Commands like `wg trace extract`, `wg trace instantiate`, `wg trace list-functions`, `wg trace show-function`, `wg trace bootstrap`, and `wg trace make-adaptive` are now `wg func extract`, `wg func apply`, `wg func list`, `wg func show`, `wg func bootstrap`, and `wg func make-adaptive` respectively. The old `wg trace` names still work as hidden aliases with a deprecation warning. This document predates the rename and uses the original command names throughout.

## 1. Current State Analysis

### 1.1 What Exists Today

The workgraph codebase has a working first-generation trace function system:

**Core data model** (`src/trace_function.rs`): `TraceFunction`, `TaskTemplate`, `FunctionInput`, `FunctionOutput`, `LoopEdgeTemplate`, `ExtractionSource`. Functions stored as YAML in `.workgraph/functions/<id>.yaml`. The `kind` field is always `"trace-function"` with `version: 1`.

**Extraction** (`src/commands/trace_extract.rs`): `wg trace extract <task-id>` extracts a trace function from a completed (Done) task. Supports `--subgraph` to capture the full descendant DAG, `--generalize` (stubbed, prints a warning), `--name`, `--output`, `--force`. Parameter detection is heuristic: scans task text for file paths, URLs, commands, and numbers.

**Instantiation** (`src/commands/trace_instantiate.rs`): `wg trace instantiate <function-id>` creates real tasks from a function definition. Supports `--input key=value`, `--input-file`, `--prefix`, `--dry-run`, `--after`, `--model`, `--from` (peer or file path). Template substitution uses `{{input.<name>}}` placeholders via `str::replace()`.

**Listing and display** (`src/commands/trace_function_cmd.rs`): `wg trace list-functions` and `wg trace show-function <id>` with `--include-peers` for federation-aware discovery.

**Trace viewing** (`src/commands/trace.rs`): `wg trace show <id>` with modes: Summary, Full, Json, OpsOnly. Also supports `--recursive` (execution tree), `--timeline` (parallel lanes), `--graph` (2D box layout), `--animate` (terminal TUI replay).

**Trace export/import** (`src/commands/trace_export.rs`, `trace_import.rs`): Export produces a JSON bundle containing tasks, evaluations, and operations filtered by visibility level (internal/public/peer). Import namespaces tasks under `imported/<source>/`.

**Replay** (`src/commands/replay.rs`): `wg replay` resets completed/failed tasks to Open, optionally filtered by `--failed-only`, `--below-score`, `--subgraph`, `--tasks`. Creates a snapshot in `.workgraph/runs/` before resetting.

**Provenance** (`src/provenance.rs`): Every state change (add_task, claim, done, fail, retry, edit, etc.) recorded in `operations.jsonl` with timestamp, actor, operation type, task_id, and detail.

**Graph model** (`src/graph.rs`): Tasks have `after` (dependencies), `before` (reverse edges), `cycle_config` (for structural loops), `visibility`, `skills`, `agent`, `tags`, `artifacts`, `deliverables`, `verify`, `model`, `log` entries, timestamps, retry state, and loop iteration tracking.

**Cycle detection** (`src/cycle.rs`): Tarjan's SCC (iterative), Havlak's loop nesting forest, incremental cycle detection, cycle metadata extraction. `CycleConfig` with `max_iterations`, `LoopGuard`, and `delay`.

**Agency** (`src/agency.rs`): Roles, Motivations, Agents (role+motivation pairing), Evaluations, Performance records, Lineage (evolutionary history), Skill resolution. Content-addressed by hash.

**Federation** (`src/federation.rs`): Named remotes and peers in `federation.yaml`. Transfer of agency entities between stores. Peer resolution for cross-repo function discovery.

### 1.2 What the Current System Can and Cannot Do

**Can do today:**
- Extract a static function from a single completed task or its subgraph
- Detect parameters heuristically from task text (file paths, URLs, commands, numbers)
- Instantiate functions with typed input validation (string, text, file_list, file_content, number, url, enum, json)
- Template substitution in titles, descriptions, skills, deliverables, verify
- Preserve `after` dependency structure and loop edges across extraction/instantiation
- Carry `role_hint` from agency into templates and back via tags
- Discover and instantiate functions from federated peers
- Export/import traces with visibility filtering

**Cannot do today:**
- No generative functions: extracted topology is fixed at extraction time
- No adaptive functions: no trace memory or feedback loop from past instantiations
- No planning node: agents cannot dynamically decide how many tasks to create
- No structural constraints: no way to express "at least 2 implementation tasks, at most 5"
- No self-bootstrapping: the extraction process is a CLI command, not itself a workgraph workflow
- No function composition: functions cannot nest or reference other functions
- No conditional task inclusion: all templates are always instantiated
- No visibility field on functions themselves (tasks have visibility, functions do not)
- The `--generalize` flag is stubbed and not wired to an LLM

## 2. The Three-Layer Function Taxonomy

Each layer subsumes the capabilities of the previous one.

### Layer 1: Static Functions (Fixed Topology)

A parameterized DAG template with a fixed number of tasks, fixed dependency structure, and fixed loop edges. This is what exists today.

**Invariant:** The number of tasks and their dependency edges are determined entirely at extraction time. Instantiation only fills in parameter values. The graph shape is identical across all instantiations.

**When to use:** Routine, well-understood workflows where the structure never varies. Examples: "implement a feature" (plan → implement → validate → refine), "fix a bug" (reproduce → diagnose → fix → verify), "write a design doc" (research → draft → review).

### Layer 2: Generative Functions (Planning Node + Structural Constraints)

A function template where one or more tasks are designated as "planning nodes." When the function is instantiated, the planning node runs first and its output determines the actual task graph. The function definition provides structural constraints (minimum/maximum task counts, required skill coverage, dependency patterns) that the planning node must satisfy.

**Key difference from Layer 1:** The topology is not fixed. The planning node can create 3 tasks or 7 tasks, fan-out or chain, depending on the specific inputs. But it must do so within the declared constraints.

**When to use:** Workflows where the structure depends on the inputs. Examples: "implement an API" (planning node reads the OpenAPI spec and creates one task per endpoint), "refactor a module" (planning node analyzes the codebase and creates tasks per file), "run a test matrix" (planning node generates one task per test configuration).

### Layer 3: Adaptive Functions (Planning Node + Trace Memory)

Generative functions that also have access to the execution traces of all previous instantiations of the same function. The planning node can consult this trace memory to adjust its strategy. The function's trace memory accumulates over time, making the function increasingly effective.

**Key difference from Layer 2:** The planning node receives not just the current inputs but also a summary of what happened in past runs: which tasks succeeded, which failed and why, how long they took, what human interventions occurred, what evaluation scores were achieved.

**When to use:** Workflows that benefit from learning. Examples: "implement a feature" (the function has seen 20 past features and knows that a validation loop with max_iterations=3 is usually sufficient), "deploy to production" (the function has learned that a specific pre-flight check catches 80% of failures).

## 3. Data Model for Extracted Functions

### 3.1 Extended TraceFunction (All Three Layers)

New fields marked with their layer requirement:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceFunction {
    // === Identity (unchanged) ===
    pub kind: String,                    // "trace-function"
    pub version: u32,                    // 1 (static), 2 (generative), 3 (adaptive)
    pub id: String,
    pub name: String,
    pub description: String,

    // === Provenance (unchanged) ===
    pub extracted_from: Vec<ExtractionSource>,
    pub extracted_by: Option<String>,
    pub extracted_at: Option<String>,
    pub tags: Vec<String>,

    // === Layer 1: Static topology (unchanged) ===
    pub inputs: Vec<FunctionInput>,
    pub tasks: Vec<TaskTemplate>,
    pub outputs: Vec<FunctionOutput>,

    // === Layer 2: Generative topology (new) ===
    pub planning: Option<PlanningConfig>,
    pub constraints: Option<StructuralConstraints>,

    // === Layer 3: Adaptive memory (new) ===
    pub memory: Option<TraceMemoryConfig>,

    // === Boundary and visibility (new) ===
    pub visibility: FunctionVisibility,
    pub redacted_fields: Vec<String>,
}
```

### 3.2 Planning Configuration (Layer 2)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanningConfig {
    /// The task template for the planning node itself.
    pub planner_template: TaskTemplate,

    /// Format the planner should output its task graph in.
    pub output_format: String,  // default: "workgraph-yaml"

    /// Use static tasks as fallback if planner fails.
    pub static_fallback: bool,

    /// Validate planner output against constraints before instantiation.
    pub validate_plan: bool,  // default: true
}
```

### 3.3 Structural Constraints (Layer 2)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuralConstraints {
    pub min_tasks: Option<u32>,
    pub max_tasks: Option<u32>,
    pub required_skills: Vec<String>,
    pub max_depth: Option<u32>,
    pub allow_cycles: bool,
    pub max_total_iterations: Option<u32>,
    pub required_phases: Vec<String>,
    pub forbidden_patterns: Vec<ForbiddenPattern>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForbiddenPattern {
    pub tags: Vec<String>,
    pub reason: String,
}
```

### 3.4 Trace Memory (Layer 3)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceMemoryConfig {
    /// Maximum past run summaries to include in planning prompt.
    pub max_runs: u32,  // default: 10
    pub include: MemoryInclusions,
    pub storage_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInclusions {
    pub outcomes: bool,       // default: true
    pub scores: bool,         // default: true
    pub interventions: bool,  // default: true
    pub duration: bool,       // default: true
    pub retries: bool,        // default: false
    pub artifacts: bool,      // default: false
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub instantiated_at: String,
    pub inputs: HashMap<String, serde_yaml::Value>,
    pub prefix: String,
    pub task_outcomes: Vec<TaskOutcome>,
    pub interventions: Vec<InterventionSummary>,
    pub wall_clock_secs: Option<i64>,
    pub all_succeeded: bool,
    pub avg_score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub template_id: String,
    pub task_id: String,
    pub status: String,
    pub score: Option<f64>,
    pub duration_secs: Option<i64>,
    pub retry_count: u32,
}
```

### 3.5 Function Visibility

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum FunctionVisibility {
    Internal,  // only within this workgraph
    Peer,      // discoverable by federated peers, redaction applies
    Public,    // fully portable, provenance stripped
}
```

### 3.6 YAML Examples

**Layer 1 (Static):**
```yaml
kind: trace-function
version: 1
id: bug-fix
name: "Bug Fix"
description: "Reproduce, diagnose, fix, verify."
inputs:
  - name: bug_name
    type: string
    required: true
tasks:
  - template_id: reproduce
    title: "Reproduce {{input.bug_name}}"
    skills: [debugging]
  - template_id: diagnose
    title: "Diagnose {{input.bug_name}}"
    after: [reproduce]
    skills: [analysis]
  - template_id: fix
    title: "Fix {{input.bug_name}}"
    after: [diagnose]
    skills: [implementation]
  - template_id: verify
    title: "Verify {{input.bug_name}}"
    after: [fix]
    skills: [testing]
```

**Layer 2 (Generative):**
```yaml
kind: trace-function
version: 2
id: impl-api
name: "Implement API from Spec"
description: "Read an API spec, plan tasks per endpoint, implement, validate."
visibility: peer
inputs:
  - name: api_spec
    type: file_content
    required: true
planning:
  planner_template:
    template_id: plan-api
    title: "Plan API implementation"
    description: |
      Read the API spec below and produce a task plan.
      Create one implementation task per endpoint group.
      Create one test task per implementation task.
      API Spec: {{input.api_spec}}
    skills: [analysis, api-design]
    role_hint: architect
  output_format: workgraph-yaml
  static_fallback: true
  validate_plan: true
constraints:
  min_tasks: 2
  max_tasks: 20
  required_skills: [implementation, testing]
  required_phases: [implement, test]
  max_depth: 4
tasks:  # fallback
  - template_id: implement
    title: "Implement API"
    skills: [implementation]
  - template_id: test
    title: "Test API"
    after: [implement]
    skills: [testing]
```

**Layer 3 (Adaptive):**
```yaml
kind: trace-function
version: 3
id: deploy-production
name: "Deploy to Production"
description: "Build, test, stage, deploy with memory of past deploys."
inputs:
  - name: version
    type: string
    required: true
  - name: environment
    type: enum
    values: [staging, production]
    required: true
planning:
  planner_template:
    template_id: plan-deploy
    title: "Plan deployment of {{input.version}}"
    description: |
      Plan the deployment to {{input.environment}}.
      Past deployment history:
      {{memory.run_summaries}}
    skills: [devops, planning]
    role_hint: architect
  validate_plan: true
constraints:
  required_phases: [build, test, deploy]
  required_skills: [devops]
memory:
  max_runs: 10
  include:
    outcomes: true
    scores: true
    interventions: true
    duration: true
tasks:  # fallback
  - template_id: build
    title: "Build {{input.version}}"
  - template_id: test
    title: "Test {{input.version}}"
    after: [build]
  - template_id: deploy
    title: "Deploy {{input.version}} to {{input.environment}}"
    after: [test]
```

## 4. The Extraction Protocol

### 4.1 Static Extraction (Layer 1, Implemented)

```
STATIC_EXTRACT(task_id, graph):
  1. task = graph.get_task(task_id); REQUIRE: task.status == Done
  2. IF --subgraph: tasks = collect_subgraph(task_id)
  3. FOR each task: build TaskTemplate (strip prefix, preserve after edges, role_hint)
  4. Detect parameters heuristically (file paths, URLs, commands, numbers)
  5. Build outputs from artifacts
  6. Validate (after references, no circular deps, no duplicate IDs)
  7. Save to .workgraph/functions/<id>.yaml
```

### 4.2 Generative Extraction (Layer 2, Proposed)

Requires analyzing multiple completed instances of similar workflows:

```
GENERATIVE_EXTRACT(task_ids[]):
  1. Load recursive traces for each task_id
  2. Align traces: identify shared vs variable task topology
  3. If all identical topology → fall back to static extraction
  4. Synthesize planning prompt from common phases and variable parts
  5. Infer constraints from observed traces (min/max tasks, required skills)
  6. Extract median topology as static fallback
  7. Save as version 2 function
```

### 4.3 Adaptive Extraction (Layer 3, Proposed)

```
MAKE_ADAPTIVE(function_id):
  1. Load function; REQUIRE: version >= 2
  2. Scan provenance for past instantiations of this function
  3. Build RunSummary for each past instantiation
  4. Save summaries to .workgraph/functions/<id>.memory/
  5. Add TraceMemoryConfig to function
  6. Append {{memory.run_summaries}} to planner template
  7. Bump version to 3; save
```

## 5. Visibility and Boundary Semantics

### 5.1 What Crosses the Export Boundary

| Field | Internal | Peer | Public |
|-------|----------|------|--------|
| id, name, description | Full | Full | Full |
| inputs (schema) | Full | Full | Full |
| inputs (examples/defaults) | Full | Full | Stripped if paths |
| tasks (templates) | Full | Full | Full |
| outputs | Full | Full | Full |
| extracted_from | Full | task_id + timestamp | task_id only |
| extracted_by | Full | Generalized | Omitted |
| planning (prompt) | Full | Full | Sanitized |
| constraints | Full | Full | Full |
| memory (config) | Full | Schema only | Omitted |
| memory (run data) | Full | Omitted | Omitted |

### 5.2 Boundary Crossing Protocol

```
EXPORT_FUNCTION(func, target_visibility):
  1. IF func.visibility < target_visibility: ERROR
  2. Clone function
  3. Apply redaction rules per target visibility level
  4. Strip trace memory run data for peer/public
  5. Strip path-specific defaults for public
  6. Return sanitized function
```

## 6. Self-Bootstrapping

### 6.1 The Meta-Function

The extraction process itself is expressible as a trace function called `extract-function`:

```
analyze trace → identify invariant structure → identify parameter points →
draft function template → validate against original trace → export
```

### 6.2 Bootstrapping Sequence

1. **Manual first extraction:** Human runs `wg trace extract` on completed workflows. Produces Layer 1 functions.
2. **Extract the extractor:** `wg trace extract --generative` across multiple extraction traces → Layer 2 meta-function.
3. **Make it adaptive:** `wg trace make-adaptive extract-function` → Layer 3 meta-function with trace memory.
4. **Use the meta-function:** `wg trace instantiate extract-function --input source_task_id=my-task` → extraction workflow.
5. **The loop closes:** The extraction workflow completes, producing a new function. The meta-function's trace memory records the run.

### 6.3 CLI

```bash
# Step 1: Extract manually
wg trace extract impl-auth --name impl-feature --subgraph

# Step 2: Bootstrap meta-function
wg trace extract --generative extract-run-1 extract-run-2 --name extract-function

# Step 3: Make adaptive
wg trace make-adaptive extract-function

# Step 4: Use it
wg trace instantiate extract-function \
    --input source_task_id=impl-caching \
    --input function_name=impl-feature-v2
```

## 7. Integration Points

### 7.1 Agency

- Extraction already looks up `role_hint` from agency
- Planning nodes need agent assignment via coordinator
- Generative constraints can specify skill requirements
- Adaptive functions consult evaluation scores for trace memory

### 7.2 Federation

- `wg trace list-functions --include-peers` already discovers peer functions
- Function visibility controls peer discoverability
- Trace memory is never shared across peers (too sensitive)
- Constraints are always shared (structural, not sensitive)

### 7.3 Cycles

- `TaskTemplate` already has loop edge support
- Structural constraints include `allow_cycles` and `max_total_iterations`
- Plan validator uses existing `CycleAnalysis::from_graph()` to verify cycle constraints

### 7.4 Provenance

- Extraction and instantiation both record provenance entries
- Trace memory is built by querying provenance for past instantiations
- Plan validation results recorded as provenance entries

### 7.5 Replay

- `wg replay --subgraph <prefix>` can reset an instantiated function's tasks
- Adaptive functions detect replays from provenance and incorporate into trace memory

## 8. Implementation Roadmap

### Phase 1: Harden Layer 1 (2-3 weeks)

1. Wire `--generalize` to LLM for generalization pass
2. Add `visibility: FunctionVisibility` to `TraceFunction`
3. Cycle extraction: capture `CycleConfig` into `LoopEdgeTemplate`
4. Run tracking: record function_id + prefix → created task IDs in `.workgraph/functions/<id>.runs.jsonl`
5. Test coverage for edge cases

**Files:** `trace_function.rs`, `trace_extract.rs`, `trace_function_cmd.rs`, `trace_instantiate.rs`

### Phase 2: Generative Functions (3-4 weeks)

1. Add `PlanningConfig`, `StructuralConstraints` to data model
2. Plan execution protocol: create planner task → parse output → validate → create tasks or fallback
3. New `src/plan_validator.rs` module
4. Multi-trace extraction command: `wg trace extract --generative`
5. CLI updates

**New files:** `plan_validator.rs`, `tests/integration_generative_functions.rs`

### Phase 3: Trace Memory and Adaptive Functions (2-3 weeks)

1. New `src/trace_memory.rs` module
2. Memory injection during instantiation: `{{memory.run_summaries}}`
3. Automatic post-run recording of `RunSummary`
4. `wg trace make-adaptive` command

**New files:** `trace_memory.rs`, `tests/integration_adaptive_functions.rs`

### Phase 4: Self-Bootstrapping (1-2 weeks)

1. Ship `extract-function` as built-in meta-function
2. `wg trace bootstrap` convenience command
3. Full round-trip integration test

### Phase 5: Function Composition (future)

- Functions referencing other functions as sub-steps
- Nested prefix handling
- Deferred pending design work

---

The key architectural insight: each layer is a strict superset of the previous one. A Layer 1 function is a valid Layer 2 function with no planning node, and a Layer 2 function is a valid Layer 3 function with no trace memory. The data model is additive and backward-compatible.

---

## 9. Implementation Notes

Notes on how the implementation diverges from or extends the spec above.

### 9.1 Trace Memory: Dual Storage Strategy

The spec (§3.4) describes a per-run JSON directory at `.workgraph/functions/<id>.memory/`. The implementation adds a second, parallel strategy: `.workgraph/functions/<id>.runs.jsonl` (one JSON line per run summary). The JSONL strategy is what `trace_instantiate` and `trace_make_adaptive` actually use for reading/writing run history. The per-run JSON directory (`trace_memory::memory_dir`, `save_run_summary`, `load_recent_summaries`) exists alongside it but is used by `build_run_summary` for spec-compliant individual run storage. Both strategies coexist; consumers should prefer the JSONL path for operational use.

### 9.2 Generative Extraction: Trace Alignment Heuristic

The spec (§4.2) describes an abstract "align traces: identify shared vs variable task topology" step. The implementation in `trace_extract::run_generative()` implements this as:
1. Collect subgraphs for each trace.
2. Compare task counts and ordered skill-set tuples across all traces.
3. If all traces have identical topology (same count AND same skills in order), fall back to static extraction.
4. Otherwise: compute `min_tasks`/`max_tasks` from trace sizes, `common_skills` (intersection), `all_skills` (union), and `max_depth`. Pick the median-size trace as the static fallback.
5. Synthesize a planning prompt describing the observed pattern variation.

This is more heuristic than a formal structural alignment algorithm but works well for typical extraction scenarios.

### 9.3 Planner Output Capture

The spec (§4.2) doesn't detail how the planning node's output is captured. The implementation in `trace_instantiate::execute_plan_or_fallback()` uses a two-step search:
1. Check the planner task's artifacts for files ending in `.yaml` or `.yml`, parse the first one found.
2. If no artifact, scan the task's log entries for ` ```yaml ` fenced code blocks.
3. Parse the YAML as `Vec<TaskTemplate>`.
4. If `validate_plan` is true, run `plan_validator::validate_plan()` against the function's constraints.
5. On validation failure: use static fallback tasks if `static_fallback` is true; otherwise error.

### 9.4 The `--generalize` Flag Is Wired

The spec (§1.2) notes that `--generalize` is "stubbed and not wired to an LLM." As of the current implementation, it is wired: `trace_extract::generalize_with_executor()` calls `claude --print` via subprocess, sending the raw function YAML with a prompt to replace instance-specific values with `{{input.<name>}}` placeholders. Requires the coordinator executor to be `claude`.

### 9.5 Intervention Detection

The spec (§3.4) defines `InterventionSummary` but doesn't specify which provenance operations constitute interventions. The implementation (`trace_memory::build_run_summary()`) detects four operation types as interventions: `retry`, `edit`, `reassign`, and `manual_override`.

### 9.6 TaskTemplate YAML Aliases

The `TaskTemplate` struct accepts both `after` and `blocked_by` (via `#[serde(alias = "blocked_by")]`) as the field name for dependency lists in YAML. This mirrors the graph model's edge rename from `blocked_by` to `after`.

### 9.7 Plan Validation: Depth Calculation

The spec (§3.3) mentions `max_depth` as a constraint but doesn't detail the algorithm. The implementation (`plan_validator::validate_plan()`) computes depth via BFS from root nodes (tasks with zero in-degree), tracking the longest path. This is the standard topological-order longest-path computation.

### 9.8 Phase Detection via Tags

The spec (§3.3) mentions `required_phases` but doesn't define what constitutes a "phase." The implementation checks the union of all task `tags` across the plan for the presence of each required phase name. This means planners must tag tasks with phase names (e.g., `analyze`, `implement`, `test`) for phase validation to work.

### 9.9 Function Composition (Phase 5)

Function composition (functions referencing other functions as sub-steps) remains unimplemented as noted in the roadmap. This is deferred to a future phase.
