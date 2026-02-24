# Trace Functions: Parameterized Workflow Templates from Completed Traces

## Problem

A completed task's trace captures *how* work was done — the full agent conversation, tool calls, file changes, outcome. Today, traces are opaque blobs viewable via `wg trace <id>`. The replay system (`wg replay`) can re-run tasks but only by resetting them in place — it doesn't create new tasks from a pattern.

We want to extract a trace into a reusable, parameterized function that can be instantiated with different inputs to create new tasks following the same workflow pattern.

## Key Distinction from Existing Systems

| System | What it does | Creates new tasks? |
|--------|--------------|--------------------|
| **Trace** (`wg trace`) | View what happened during a task | No |
| **Replay** (`wg replay`) | Reset completed tasks to re-run them | No (resets existing) |
| **Canon** (proposed) | Distilled knowledge injected into retries | No (enriches existing) |
| **Trace Functions** (this design) | Parameterized templates that mint new tasks | **Yes** |

Trace functions are complementary to all three. A trace function could reference a canon for knowledge injection. Replay re-runs tasks; trace functions create fresh task graphs from a pattern.

## Design

### What Is a Trace Function?

A trace function is a parameterized task template extracted from one or more completed task traces. It captures:

1. **The workflow structure** — a DAG of task templates (possibly a single task, possibly a fan-out/fan-in pattern with loops)
2. **Input parameters** — typed slots that are substituted when instantiating
3. **Agent requirements** — which roles/skills are needed (optional; enables agency matching)
4. **Extraction provenance** — which traces it was derived from

A trace function is NOT a recorded macro or conversation replay. It captures the *workflow pattern* (what tasks to create, what prompts to use, what structure to follow), not the specific tool calls or conversation turns.

### Data Structure

```yaml
# .workgraph/functions/impl-feature.yaml
kind: trace-function
version: 1

# Metadata
id: impl-feature                    # unique identifier (kebab-case)
name: "Implement Feature"
description: "Plan, implement, test, and commit a new feature"
extracted_from:                      # provenance
  - task_id: impl-global-config
    run_id: run-003
    timestamp: "2026-02-18T14:30:00Z"
  - task_id: impl-agency-federation
    run_id: run-005
    timestamp: "2026-02-19T10:15:00Z"
extracted_by: scout                  # agent or user who created it
extracted_at: "2026-02-19T12:00:00Z"
tags: [implementation, feature]

# Input parameters
inputs:
  - name: feature_name
    type: string
    description: "Short name for the feature (used as task ID prefix)"
    required: true
    example: "global-config"

  - name: feature_description
    type: text
    description: "Full description of what to implement"
    required: true
    example: "Add global config at ~/.workgraph/config.toml with merge semantics"

  - name: source_files
    type: file_list
    description: "Key source files to modify"
    required: false
    default: []
    example: ["src/config.rs", "src/main.rs"]

  - name: test_command
    type: string
    description: "Command to verify the implementation"
    required: false
    default: "cargo test"

  - name: model
    type: string
    description: "Model to use for agent tasks"
    required: false
    default: null

# Task templates — the workflow structure
# Each template becomes a task when instantiated.
# Templates can reference inputs via {{input.name}} syntax.
tasks:
  - template_id: plan
    title: "Plan {{input.feature_name}}"
    description: |
      Analyze the codebase and design an implementation plan for:

      {{input.feature_description}}

      Key files to consider: {{input.source_files}}

      Produce a brief implementation plan listing files to change
      and the approach for each.
    skills: [analysis, "{{input.primary_language}}"]
    role_hint: analyst
    deliverables: ["docs/design/{{input.feature_name}}.md"]

  - template_id: implement
    title: "Implement {{input.feature_name}}"
    description: |
      Implement the following feature based on the plan from the
      planning task:

      {{input.feature_description}}

      Files to modify: {{input.source_files}}

      After implementation, run: {{input.test_command}}
    blocked_by: [plan]
    skills: [implementation, "{{input.primary_language}}"]
    role_hint: programmer

  - template_id: validate
    title: "Validate {{input.feature_name}}"
    description: |
      Review and validate the implementation of:

      {{input.feature_description}}

      Run the test suite: {{input.test_command}}
      Check for edge cases, error handling, and code quality.
    blocked_by: [implement]
    skills: [review, testing]
    role_hint: analyst

  - template_id: refine
    title: "Refine {{input.feature_name}}"
    description: |
      Address any issues found during validation.
      If everything passes, mark as converged.
    blocked_by: [validate]
    skills: [implementation]
    role_hint: programmer
    loops_to:
      - target: validate
        max_iterations: 3

# Optional: expected outputs
outputs:
  - name: modified_files
    description: "Files changed by the implementation"
    from_task: implement
    field: artifacts

  - name: test_results
    description: "Whether tests passed"
    from_task: validate
    field: status
```

### Input Parameter Types

Parameters are typed to enable validation and appropriate substitution:

| Type | Description | Substitution behavior |
|------|-------------|----------------------|
| `string` | Short text (names, commands, identifiers) | Direct text replacement |
| `text` | Long-form text (descriptions, prompts) | Direct text replacement (multiline OK) |
| `file_list` | List of file paths | Rendered as newline-separated or comma-separated list |
| `file_content` | Content read from a file path at instantiation time | File is read; content replaces the placeholder |
| `number` | Numeric value (thresholds, counts, scores) | Rendered as string for substitution |
| `url` | URL string | Direct text replacement |
| `enum` | One of a set of allowed values | Validated against `values` list |
| `json` | Arbitrary structured data | Serialized as JSON string |

Parameter declaration:

```yaml
inputs:
  - name: threshold
    type: number
    description: "Minimum passing score"
    required: false
    default: 0.8
    min: 0.0      # optional validation
    max: 1.0

  - name: language
    type: enum
    description: "Primary programming language"
    values: [rust, python, typescript, go]
    default: rust

  - name: api_spec
    type: file_content
    description: "OpenAPI spec to implement against"
    required: true
    # At instantiation, the user provides a path.
    # The file is read and its content replaces {{input.api_spec}}.
```

### Template Substitution

Within task templates, input references use the `{{input.<name>}}` syntax. This reuses and extends the existing `TemplateVars` mechanism from `executor.rs`.

Substitution rules:
- `{{input.name}}` — replaced with the parameter value
- `{{input.name | default: "fallback"}}` — replaced with default if not provided (beyond the schema default)
- Undefined or missing required inputs → error at instantiation time
- `file_list` values are joined with newlines for `{{input.files}}`, or can be iterated with `{{#each input.files}}{{.}}\n{{/each}}` (future, if needed)

For v1, simple `str.replace()` substitution (matching the existing `TemplateVars::apply()` pattern) is sufficient. No need for a full template engine.

### How Extraction Works

Extraction transforms a completed task's trace into a trace function. This is a hybrid process: static analysis of the trace data provides structure, and (optionally) an LLM pass cleans up and generalizes.

#### Phase 1: Static Analysis (always runs)

Given a task ID (or set of task IDs), extract:

1. **Task structure** — from the graph: the task itself, its `blocked_by`/`blocks` edges, `loops_to` edges, skills, role assignments. If the task is part of a subgraph (e.g., a fan-out from a parent), capture the full subgraph structure.

2. **Agent identity** — from agency: which role/motivation/skills were used. These become `role_hint` and `skills` in the template.

3. **Artifacts** — from `task.artifacts`: what files were produced. These inform the `outputs` section.

4. **Provenance operations** — from `operations.jsonl`: the sequence of operations (add_task, claim, done, fail, retry). This reveals the actual execution flow including retries.

5. **Conversation statistics** — from agent archives: turn count, tool calls, duration. Useful metadata but not directly templated.

The static extraction produces a "raw" trace function with the original task descriptions as-is (not yet parameterized).

#### Phase 2: Parameterization (semi-automated)

Identify values in the task descriptions that should become parameters:

1. **Exact-match detection**: The task title and description are scanned for values that appear to be instance-specific:
   - Task IDs and titles → `{{input.feature_name}}`
   - File paths mentioned → `{{input.source_files}}`
   - URLs mentioned → candidate for `{{input.api_url}}`
   - Numeric thresholds → candidate for `{{input.threshold}}`
   - Commands like `cargo test` → `{{input.test_command}}`

2. **User review**: The extracted function is presented to the user with suggested parameters highlighted. The user confirms, renames, or removes parameters.

3. **LLM-assisted generalization** (optional, `--generalize` flag): An LLM reads the trace function and the original task description, then rewrites the template descriptions to be more generic — removing instance-specific details while preserving the workflow intent.

#### Phase 3: Validation

The extracted function is validated:
- All `blocked_by` references resolve to template IDs within the function
- All `loops_to` targets resolve to template IDs within the function
- All required inputs have no default (or vice versa)
- No circular `blocked_by` dependencies (loops are only via `loops_to`)
- Template substitution with example values produces valid task descriptions

### Extraction Algorithm (Concrete)

```
extract(task_id, graph, agency_dir, log_dir) -> TraceFunction:

    1. task = graph.get_task(task_id)
    2. subgraph = collect_subgraph(task_id, graph)
       // If task is a root with subtasks, collect the full DAG.
       // If task is standalone, subgraph = [task].

    3. For each task t in subgraph:
       a. template = TaskTemplate {
            template_id: sanitize(t.id),     // strip instance prefix
            title: t.title,
            description: t.description,
            skills: t.skills,
            blocked_by: [sanitize(dep) for dep in t.blocked_by if dep in subgraph],
            loops_to: t.loops_to (remapped to template IDs),
            role_hint: lookup_role_name(t.agent, agency_dir),
            deliverables: t.deliverables,
          }
       b. Add template to function.tasks

    4. inputs = detect_parameters(subgraph)
       // Scan all descriptions for extractable values
       // Present as suggested inputs

    5. Return TraceFunction { tasks, inputs, extracted_from: [task_id], ... }
```

### Instantiation

Instantiation takes a trace function + input values and creates real tasks in the graph.

```
instantiate(function_id, inputs, graph) -> Vec<TaskId>:

    1. func = load_function(function_id)
    2. validate_inputs(func.inputs, inputs)
       // Check required, types, enum values, ranges

    3. prefix = inputs["feature_name"] or generate_prefix()
       // All created task IDs get a prefix to avoid collisions

    4. id_map = {}  // template_id -> real task_id
    5. For each template in func.tasks:
       a. task_id = prefix + "-" + template.template_id
       b. title = substitute(template.title, inputs)
       c. description = substitute(template.description, inputs)
       d. blocked_by = [id_map[dep] for dep in template.blocked_by]
       e. loops_to = remap(template.loops_to, id_map)
       f. skills = [substitute(s, inputs) for s in template.skills]
       g. Create task in graph via wg add
       h. id_map[template.template_id] = task_id

    6. Record provenance: "instantiate" operation with function_id, inputs, created task IDs
    7. Return list of created task IDs
```

### Example: Full Lifecycle

#### 1. A task completes

```
$ wg show impl-global-config
impl-global-config (Done)
  Title: Implement global config
  Duration: 12m 34s
  Agent: scout (programmer, improve-quality)
  Artifacts: src/config.rs, src/main.rs, tests/integration_config.rs
```

#### 2. Extract a function from the trace

```
$ wg trace extract impl-global-config --name impl-feature

Extracted trace function 'impl-feature' from task 'impl-global-config'

Tasks: 1 (standalone task, no subgraph)

Suggested parameters:
  feature_name (string, required): "global-config" → extracted from task ID
  feature_description (text, required): "Add global config at ~/.workgraph/..." → from description
  source_files (file_list): ["src/config.rs", "src/main.rs"] → from artifacts
  test_command (string): "cargo test" → detected in conversation

Saved to: .workgraph/functions/impl-feature.yaml

Review and edit the function file to adjust parameters and descriptions.
```

#### 3. Optionally generalize with LLM

```
$ wg trace extract impl-global-config --name impl-feature --generalize

[LLM rewrites descriptions to be generic...]

Saved to: .workgraph/functions/impl-feature.yaml
```

#### 4. Extract from a multi-task subgraph

```
$ wg trace extract design-agency-federation --name design-and-implement

Extracted trace function 'design-and-implement' from subgraph rooted at 'design-agency-federation'

Tasks: 5 (design → impl-scan → impl-pull → impl-push → validate)
  design: Plan the feature
  impl-scan: Implement scanning
  impl-pull: Implement pull
  impl-push: Implement push
  validate: Run tests and validate (loops_to: design, max 3)

Suggested parameters:
  feature_name (string): extracted from task IDs
  feature_description (text): from root task description
  ...
```

#### 5. Instantiate with new inputs

```
$ wg trace instantiate impl-feature \
    --input feature_name=agency-prune \
    --input feature_description="Add wg agency prune command to remove unused roles" \
    --input source_files=src/agency.rs,src/commands/agency_crud.rs \
    --input test_command="cargo test agency"

Created 4 tasks from function 'impl-feature':
  agency-prune-plan (Open)
  agency-prune-implement (Open, blocked by agency-prune-plan)
  agency-prune-validate (Open, blocked by agency-prune-implement)
  agency-prune-refine (Open, blocked by agency-prune-validate, loops to agency-prune-validate)

Start the service to begin execution: wg service start
```

#### 6. List available functions

```
$ wg trace list-functions

Functions:
  impl-feature       "Implement Feature"                     4 tasks, 4 inputs
  design-and-impl    "Design and Implement with Validation"  5 tasks, 3 inputs
  bug-fix            "Investigate and Fix Bug"               3 tasks, 2 inputs
```

### Storage

Trace functions are stored as YAML files in `.workgraph/functions/`:

```
.workgraph/
  functions/
    impl-feature.yaml
    design-and-impl.yaml
    bug-fix.yaml
```

Why YAML:
- Consistent with agency storage (roles, motivations, agents)
- Human-readable and editable
- Supports multiline strings for descriptions
- Existing `serde_yaml` dependency in the project

The `kind: trace-function` field distinguishes from other YAML files and enables future polymorphism if other kinds of functions are added.

### CLI

#### `wg trace extract <task-id>`

Extract a trace function from a completed task.

```
wg trace extract <task-id> [OPTIONS]

OPTIONS:
  --name <id>          Function name/ID (default: derived from task ID)
  --subgraph           Include all subtasks (tasks blocked by this one) in the function
  --generalize         Use LLM to generalize descriptions (removes instance-specific details)
  --output <path>      Write to specific path instead of .workgraph/functions/<name>.yaml
  --force              Overwrite existing function with same name
```

Constraints:
- Task must be in Done status (extracting from failed/in-progress tasks is not meaningful)
- If `--subgraph` is used and the task has no subtasks, behaves as single-task extraction

#### `wg trace instantiate <function-id>`

Create tasks from a trace function with provided inputs.

```
wg trace instantiate <function-id> [OPTIONS]

OPTIONS:
  --input <key=value>  Set an input parameter (repeatable)
  --input-file <path>  Read inputs from a YAML/JSON file
  --prefix <string>    Override the task ID prefix (default: from feature_name input)
  --dry-run            Show what tasks would be created without creating them
  --blocked-by <id>    Make all root tasks (those with no internal blocked_by) depend on this task
  --model <model>      Set model for all created tasks
```

Input file format:

```yaml
# inputs.yaml
feature_name: agency-prune
feature_description: |
  Add a wg agency prune command that removes roles, motivations,
  and agents that have no evaluations and are not assigned to any task.
source_files:
  - src/agency.rs
  - src/commands/agency_crud.rs
test_command: "cargo test agency"
```

```
$ wg trace instantiate impl-feature --input-file inputs.yaml
```

#### `wg trace list-functions`

List available trace functions.

```
wg trace list-functions [OPTIONS]

OPTIONS:
  --json               Output as JSON
  --verbose            Show input parameters and task templates
```

#### `wg trace show-function <function-id>`

Show details of a trace function.

```
wg trace show-function <function-id> [OPTIONS]

OPTIONS:
  --json               Output as JSON
```

### Relationship to Other Systems

#### Replay

Replay resets existing tasks; trace functions create new tasks. They compose naturally:

1. Extract a function from a successful run
2. Instantiate it for a new feature
3. Run it (service dispatches agents)
4. If the result is unsatisfactory, `wg replay` resets the instantiated tasks for another attempt
5. If the result is good, extract a refined function from this run too

#### Canon

Canon distills *knowledge* (what to do, what to avoid) from traces. Trace functions distill *structure* (what tasks to create, in what order). They compose:

- A trace function template could include a `{{task_canon}}` reference in its descriptions, injecting distilled knowledge from a prior canon
- Extraction could optionally create a canon alongside the function
- When instantiating, if a canon exists for the function, it can be injected into the created tasks

#### Agency

Trace functions reference roles via `role_hint` — a suggested role name (not a hash). At instantiation time:

- If auto-assign is enabled, the coordinator matches agents to tasks based on skills (existing behavior)
- `role_hint` is stored as a tag on the created task (e.g., `role:analyst`) for the auto-assigner to consider
- The function does NOT hard-code agent hashes — this keeps functions portable across projects with different agency setups

#### Skills

Task templates include `skills` lists that can reference inputs (e.g., `"{{input.primary_language}}"`). These are substituted at instantiation time and stored on the created tasks, enabling the existing skill-based matching to work.

#### Loop Edges

Task templates can include `loops_to` edges. These are remapped from template IDs to real task IDs at instantiation time. The existing loop evaluation system (`evaluate_loop_edges`, `--converged`) works unchanged.

### Rust Data Structures

```rust
/// A parameterized workflow template extracted from completed traces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceFunction {
    pub kind: String,              // "trace-function"
    pub version: u32,              // schema version (1)
    pub id: String,                // unique identifier
    pub name: String,              // human-readable name
    pub description: String,
    pub extracted_from: Vec<ExtractionSource>,
    pub extracted_by: Option<String>,
    pub extracted_at: String,
    pub tags: Vec<String>,
    pub inputs: Vec<FunctionInput>,
    pub tasks: Vec<TaskTemplate>,
    pub outputs: Vec<FunctionOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionSource {
    pub task_id: String,
    pub run_id: Option<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionInput {
    pub name: String,
    pub input_type: InputType,
    pub description: String,
    pub required: bool,
    pub default: Option<serde_yaml::Value>,
    pub example: Option<serde_yaml::Value>,
    // Type-specific validation
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub values: Option<Vec<String>>,   // for enum type
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    String,
    Text,
    FileList,
    FileContent,
    Number,
    Url,
    Enum,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTemplate {
    pub template_id: String,
    pub title: String,
    pub description: String,
    pub skills: Vec<String>,
    pub blocked_by: Vec<String>,        // references to other template_ids
    pub loops_to: Vec<LoopEdgeTemplate>,
    pub role_hint: Option<String>,
    pub deliverables: Vec<String>,
    pub verify: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopEdgeTemplate {
    pub target: String,                 // template_id
    pub max_iterations: u32,
    pub guard: Option<String>,          // serialized guard condition
    pub delay: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionOutput {
    pub name: String,
    pub description: String,
    pub from_task: String,              // template_id
    pub field: String,                  // "artifacts", "status", etc.
}
```

### Implementation Plan

The implementation breaks into five independent modules plus tests:

1. **Core data structures and storage** (`src/function.rs`)
   - `TraceFunction`, `FunctionInput`, `TaskTemplate` structs
   - YAML serialization/deserialization
   - Load/save to `.workgraph/functions/`
   - Input validation
   - Template substitution

2. **Extraction** (`src/commands/func_extract.rs`)
   - Static extraction from task + graph + agency
   - Subgraph collection
   - Parameter detection heuristics
   - Optional LLM generalization pass

3. **Instantiation** (`src/commands/func_apply.rs`)
   - Input parsing and validation
   - Task creation with ID remapping
   - Dependency wiring (blocked_by, loops_to)
   - Provenance recording

4. **CLI commands** (additions to `src/main.rs`)
   - `wg trace extract` subcommand
   - `wg trace instantiate` subcommand
   - `wg trace list-functions` subcommand
   - `wg trace show-function` subcommand

5. **Tests** (`tests/integration_trace_functions.rs`)
   - Extraction from single task
   - Extraction from subgraph
   - Parameterization and substitution
   - Instantiation creates correct tasks and dependencies
   - Instantiation with loop edges
   - Input validation (missing required, wrong type, out of range)
   - Round-trip: extract → instantiate → verify graph structure
   - Edge cases: duplicate function names, empty subgraphs, circular deps in templates

### Open Questions and Future Extensions

1. **Function composition**: Can trace functions reference other trace functions? (e.g., a "release" function that instantiates a "test" function as a sub-step). Deferred to v2 — keep v1 flat.

2. **Function versioning**: Should functions track versions as they're edited? v1 uses simple overwrite. Versioning can be added later via content-hashing (like agency entities).

3. **Conditional tasks**: Some workflows have optional steps (e.g., "if language is rust, run clippy"). v1 does not support conditional inclusion of tasks. All templates are always instantiated.

4. **Function federation**: Like agency federation, trace functions could be shared across projects. The YAML format is already portable. `wg trace push/pull` can follow the same pattern as `wg agency push/pull`.

5. **Auto-extraction**: The coordinator could automatically extract a function when a "prototype" task completes successfully (e.g., tasks tagged `prototype`). Deferred.

6. **Function evaluation**: Track how well instantiated tasks perform vs. the original. Feed back into function refinement. Deferred.
