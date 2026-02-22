# `wg func` Rename & Extraction Quality Spec

**Status:** Draft
**Date:** 2026-02-22
**Task:** func-spec

---

## 1. CLI Rename: `wg trace` → `wg func`

### 1.1 Motivation

The `wg trace` subcommand currently conflates two unrelated concerns:

1. **Trace viewing** — inspecting execution history (`show`, `timeline`, `animate`, `graph`, `export`, `import`)
2. **Function management** — extracting, listing, instantiating, and evolving reusable workflow templates

These are conceptually distinct. "Trace" is about observability; "function" is about reuse and composition. Splitting them improves discoverability and makes the CLI surface clearer.

### 1.2 Command Mapping

| Current Command                  | New Command             | Notes                                  |
|----------------------------------|-------------------------|----------------------------------------|
| `wg trace extract`               | `wg func extract`       | Same args                              |
| `wg trace instantiate`           | `wg func apply`         | **Verb rename**: instantiate → apply   |
| `wg trace list-functions`        | `wg func list`          | Shorter, more natural                  |
| `wg trace show-function`         | `wg func show`          | Disambiguated from `wg trace show`     |
| `wg trace bootstrap`             | `wg func bootstrap`     | Same args                              |
| `wg trace make-adaptive`         | `wg func make-adaptive` | Same args                              |

Commands that **stay** under `wg trace` (trace-viewing only):

| Command                          | Purpose                          |
|----------------------------------|----------------------------------|
| `wg trace show <id>`             | Execution history of a task      |
| `wg trace show --timeline`       | Chronological timeline view      |
| `wg trace show --graph`          | 2D box layout of trace subgraph  |
| `wg trace show --animate`        | Animated trace replay            |
| `wg trace export`                | Export trace data by visibility  |
| `wg trace import`                | Import a trace export file       |

### 1.3 Backward Compatibility

Hidden aliases under `wg trace` for the old names:

```rust
// In TraceCommands enum, add hidden variants:
#[command(name = "extract", hide = true)]
ExtractAlias { /* same fields as FuncCommands::Extract */ },

#[command(name = "instantiate", hide = true)]
InstantiateAlias { /* same fields as FuncCommands::Apply */ },

#[command(name = "list-functions", hide = true)]
ListFunctionsAlias { /* same fields as FuncCommands::List */ },

#[command(name = "show-function", hide = true)]
ShowFunctionAlias { /* same fields as FuncCommands::Show */ },

#[command(name = "bootstrap", hide = true)]
BootstrapAlias { /* same fields as FuncCommands::Bootstrap */ },

#[command(name = "make-adaptive", hide = true)]
MakeAdaptiveAlias { /* same fields as FuncCommands::MakeAdaptive */ },
```

Each alias handler prints a deprecation warning to stderr then delegates to the real implementation:

```
Warning: 'wg trace extract' is deprecated. Use 'wg func extract' instead.
```

### 1.4 New Enum: `FuncCommands`

```rust
#[derive(Subcommand)]
enum FuncCommands {
    /// Extract a function from completed task(s)
    Extract { /* same fields as current TraceCommands::Extract */ },

    /// Create tasks from a function with provided inputs
    Apply {
        /// Function ID (prefix match supported)
        function_id: String,

        /// Load function from a peer workgraph (peer:function-id) or file path
        #[arg(long)]
        from: Option<String>,

        /// Set an input parameter (repeatable, format: key=value)
        #[arg(long = "input", num_args = 1)]
        inputs: Vec<String>,

        /// Read inputs from a YAML/JSON file
        #[arg(long = "input-file")]
        input_file: Option<String>,

        /// Override the task ID prefix (default: from feature_name input)
        #[arg(long)]
        prefix: Option<String>,

        /// Show what tasks would be created without creating them
        #[arg(long)]
        dry_run: bool,

        /// Make all root tasks depend on this task (repeatable)
        #[arg(long = "after", alias = "blocked-by")]
        after: Vec<String>,

        /// Set model for all created tasks
        #[arg(long)]
        model: Option<String>,
    },

    /// List available functions
    List {
        /// Show input parameters and task templates
        #[arg(long)]
        verbose: bool,

        /// Include functions from federated peer workgraphs
        #[arg(long)]
        include_peers: bool,

        /// Filter by visibility level (internal, peer, public)
        #[arg(long)]
        visibility: Option<String>,
    },

    /// Show details of a function
    Show {
        /// Function ID (prefix match supported)
        id: String,
    },

    /// Bootstrap the extract-function meta-function
    Bootstrap {
        /// Overwrite if already exists
        #[arg(long)]
        force: bool,
    },

    /// Upgrade a generative function to adaptive (adds run memory)
    #[command(name = "make-adaptive")]
    MakeAdaptive {
        /// Function ID (prefix match supported)
        function_id: String,

        /// Maximum number of past runs to include in memory
        #[arg(long, default_value = "10")]
        max_runs: u32,
    },
}
```

### 1.5 Top-Level Registration

In the `Commands` enum (`src/main.rs`):

```rust
/// Function management: extract, apply, list, show, bootstrap
Func {
    #[command(subcommand)]
    command: FuncCommands,
},
```

---

## 2. Extraction Quality Improvements

### 2.1 Filter `evaluate-*` Tasks

**Problem:** When extracting with `--subgraph` or `--generative`, the coordinator-generated `evaluate-*` tasks (LLM-based evaluation tasks) pollute the extracted function. These are internal coordinator infrastructure, not part of the user's workflow pattern.

**Solution:** In `trace_extract.rs::collect_subgraph()`, filter out tasks whose ID matches the pattern `evaluate-*` or that have the tag `coordinator:evaluation`:

```rust
fn collect_subgraph<'a>(root_id: &str, graph: &'a WorkGraph) -> Vec<&'a Task> {
    // ... existing BFS traversal ...
    // After collecting, filter:
    result.retain(|t| !is_coordinator_noise(t));
    result
}

fn is_coordinator_noise(task: &Task) -> bool {
    // Filter evaluate-* tasks (coordinator-generated evaluations)
    if task.id.starts_with("evaluate-") {
        return true;
    }
    // Filter by tag
    if task.tags.iter().any(|t| t == "coordinator:evaluation") {
        return true;
    }
    // Filter assign-* tasks (coordinator-generated agent assignment tasks)
    if task.id.starts_with("assign-") {
        return true;
    }
    false
}
```

Also filter in `run_generative()` where traces are collected.

### 2.2 Smarter Parameter Detection

**Problem:** `detect_parameters()` in `trace_extract.rs` extracts every standalone number as `threshold`, `value_2`, `value_3`, etc. Most of these are noise (line numbers, iteration counts, max_retries values, etc.), not meaningful parameters.

**Current behavior (lines 920-944):**
```rust
let numbers = extract_numbers(&all_text);
for (i, num) in numbers.iter().enumerate() {
    let param_name = if i == 0 { "threshold".to_string() } else { format!("value_{}", i + 1) };
    // ...blindly creates a parameter for every number
}
```

**Solution:** Replace the naive number extraction with contextual analysis:

1. **Require semantic context.** Only extract a number as a parameter if it appears near a keyword that suggests it's a meaningful threshold/limit:
   ```rust
   fn extract_numbers_with_context(text: &str) -> Vec<(String, f64)> {
       let threshold_keywords = [
           "threshold", "limit", "max", "min", "count", "retries",
           "timeout", "interval", "batch", "size", "depth", "iterations",
       ];
       // For each number found, check the surrounding 3 words for a keyword.
       // Use the keyword as the parameter name instead of value_N.
       // Skip numbers with no recognizable context.
   }
   ```

2. **Cap at 3 numeric parameters.** If more than 3 contextual numbers are found, only keep the 3 most confident matches.

3. **Skip common noise values:** Filter numbers that match common structural values (line numbers > 100, port numbers like 3000/8080, common constants like 10/100/1000 when not next to a keyword).

### 2.3 Better `--generalize`: Multi-Pass LLM Generalization

**Problem:** The current `--generalize` flag does a single LLM pass that often misses instance-specific details or creates malformed YAML.

**Current behavior (`generalize_with_executor`, lines 364-427):**
- Single prompt asking the LLM to replace instance-specific values
- No validation of the LLM's output beyond YAML parsing
- No understanding of task roles or workgraph patterns

**Solution:** Multi-pass generalization pipeline:

#### Pass 1: Role Classification
```
Classify each task in this function by its role in the workflow:
- planning: task analyzes requirements and creates a plan
- implementation: task writes code or creates artifacts
- review: task validates or reviews work
- integration: task combines outputs from multiple tasks
- testing: task runs tests or verification

Output a JSON mapping: { template_id: role }
```

#### Pass 2: Description Generalization
```
For each task, generalize the description:
- Replace specific file paths with {{input.source_files}} references
- Replace specific feature names with {{input.feature_name}}
- Replace specific test commands with {{input.test_command}}
- Replace instance-specific nouns (e.g., "FunctionVisibility enum") with
  role-based descriptions (e.g., "the data model")
- Keep structural verbs intact (design, implement, test, review, integrate)

Output ONLY the updated YAML tasks section.
```

#### Pass 3: Validation & Merge
- Parse Pass 2 output
- Validate all `{{input.*}}` placeholders have matching FunctionInput entries (auto-add missing ones)
- Validate YAML structure against TraceFunction schema
- Merge back with the original function's metadata

**Implementation:** Modify `generalize_with_executor()` to call the LLM twice (Pass 1 and 2), then do Pass 3 in Rust. Add a `--generalize-model` flag to control which model is used.

### 2.4 Extract the PATTERN, Not the FOSSIL

**Problem:** Extracted function task descriptions are verbatim copies of the original task, containing instance-specific details:

```yaml
# BAD: fossil
- template_id: add-enum
  title: "Add FunctionVisibility enum to src/trace_function.rs"
  description: "Add FunctionVisibility enum with Internal, Peer, Public variants..."

# GOOD: pattern
- template_id: define-model
  title: "Define the data model for {{input.feature_name}}"
  description: "Create or extend data structures needed for the feature..."
```

**Solution:** This is addressed by Pass 2 of the multi-pass generalization (Section 2.3), but also needs a **structural** change to extraction:

1. **Role-based template IDs.** Instead of deriving `template_id` from the task ID (which is instance-specific), derive it from the task's role:
   ```rust
   fn derive_role_based_template_id(task: &Task, index: usize) -> String {
       // Use skills to infer role
       if task.skills.contains(&"analysis".into()) { return "analyze".into(); }
       if task.skills.contains(&"implementation".into()) { return "implement".into(); }
       if task.skills.contains(&"review".into()) { return "review".into(); }
       if task.skills.contains(&"testing".into()) { return "test".into(); }
       // Fallback: step-N
       format!("step-{}", index + 1)
   }
   ```

2. **Default generalization without LLM.** Even without `--generalize`, apply basic heuristic generalization:
   - Replace the root task ID prefix in all descriptions with `{{input.feature_name}}`
   - Replace detected file paths with `{{input.source_files}}` references
   - Strip concrete function/struct/enum names from titles (keep the verb + generic noun)

3. **Add `--raw` flag** to preserve the current behavior (verbatim fossil extraction) for users who want it.

---

## 3. File Mapping

### 3.1 Files That Must Change

| File | Change | Scope |
|------|--------|-------|
| `src/main.rs` | Add `FuncCommands` enum, `Func` variant in `Commands`, match arm routing, hidden aliases in `TraceCommands` | Large — enum definitions + match routing (~150 lines) |
| `src/commands/mod.rs` | No new modules needed (existing `trace_*.rs` modules are reused) | Tiny — no changes needed if we reuse existing modules |
| `src/commands/trace_extract.rs` | Add `is_coordinator_noise()` filter, improve `detect_parameters()`, improve `generalize_with_executor()` with multi-pass, add role-based template IDs, add `--raw` flag support | Medium-large (~100 lines changed) |
| `src/commands/trace_instantiate.rs` | No logic changes — just called from a new routing path | Tiny — no internal changes |
| `src/commands/trace_function_cmd.rs` | Update help text hint (`wg trace extract` → `wg func extract`) | Tiny — string change on line 58 |
| `src/commands/trace_bootstrap.rs` | Update help text hints (`wg trace instantiate` → `wg func apply`, `wg trace make-adaptive` → `wg func make-adaptive`) | Tiny — string changes on lines 199-204 |
| `src/commands/trace_make_adaptive.rs` | No logic changes | None |
| `src/commands/trace_export.rs` | No changes | None |
| `src/commands/trace_import.rs` | No changes | None |
| `src/commands/trace_animate.rs` | No changes | None |
| `src/commands/trace.rs` | No changes | None |
| `src/trace_function.rs` | No changes (library code, called from commands) | None |
| `docs/AGENT-GUIDE.md` | Update CLI examples | Small |
| `CLAUDE.md` | No changes needed | None |

### 3.2 Parallel-Safe Edit Groups

Tasks that edit **non-overlapping files** can run in parallel. Tasks that touch the same file must be sequential.

#### Group A: CLI Routing (sequential — all touch `src/main.rs`)
1. Add `FuncCommands` enum to `src/main.rs`
2. Add `Func` variant to `Commands` enum in `src/main.rs`
3. Add match arm routing for `Commands::Func` in `src/main.rs`
4. Add hidden alias variants to `TraceCommands` in `src/main.rs`
5. Add match arm routing for alias variants in `src/main.rs`

These are all in `src/main.rs` — **must be one sequential task**.

#### Group B: Extraction Quality (sequential — all in `trace_extract.rs`)
1. Add `is_coordinator_noise()` filter
2. Improve `detect_parameters()` with contextual number extraction
3. Multi-pass `generalize_with_executor()`
4. Role-based template IDs
5. Add `--raw` flag

These are all in `src/commands/trace_extract.rs` — **must be one sequential task**.

#### Group C: Help Text Updates (parallel-safe)
These files are independent of each other and of Groups A/B:
- `src/commands/trace_function_cmd.rs` — update hint text
- `src/commands/trace_bootstrap.rs` — update hint text
- `docs/AGENT-GUIDE.md` — update CLI examples

**Each can be a separate parallel task**, or bundled into one small task.

### 3.3 Dependency Graph

```
              ┌──────────────┐
              │  func-spec   │  (this task — you are here)
              └──────┬───────┘
                     │
         ┌───────────┼───────────┐
         ▼           ▼           ▼
   ┌───────────┐ ┌──────────┐ ┌──────────────┐
   │  Group A  │ │ Group B  │ │   Group C    │
   │ CLI route │ │ Extract  │ │  Help text   │
   │(main.rs)  │ │ quality  │ │  updates     │
   └─────┬─────┘ └────┬─────┘ └──────┬───────┘
         │            │              │
         └────────────┼──────────────┘
                      ▼
              ┌──────────────┐
              │  Integration │  (cargo build, cargo test, cargo install)
              │  & Validate  │
              └──────────────┘
```

- **Group A, B, C** can all run in **parallel** (no file conflicts)
- **Integration** task runs **after all three**, compiles and runs tests

### 3.4 Recommended Task Breakdown

| Task ID | Title | Files | After |
|---------|-------|-------|-------|
| `func-cli-routing` | Add `wg func` CLI routing with hidden aliases | `src/main.rs` | `func-spec` |
| `func-extract-quality` | Improve extraction quality (filter noise, smarter params, multi-pass generalize) | `src/commands/trace_extract.rs` | `func-spec` |
| `func-help-text` | Update help text and docs for func rename | `src/commands/trace_function_cmd.rs`, `src/commands/trace_bootstrap.rs`, `docs/AGENT-GUIDE.md` | `func-spec` |
| `func-integrate` | Build, test, install — verify everything works | all (read-only verification) | `func-cli-routing`, `func-extract-quality`, `func-help-text` |

---

## 4. Implementation Notes

### 4.1 `src/main.rs` — Detailed Changes

The match arm for `Commands::Func` should delegate to the same underlying functions as the current `TraceCommands`:

```rust
Commands::Func { command } => match command {
    FuncCommands::Extract { task_ids, name, subgraph, recursive, generalize, generative, output, force } => {
        // Same logic as TraceCommands::Extract
        if generative {
            commands::trace_extract::run_generative(...)
        } else if recursive {
            commands::trace_extract::run_recursive(...)
        } else {
            commands::trace_extract::run(...)
        }
    }
    FuncCommands::Apply { function_id, from, inputs, input_file, prefix, dry_run, after, model } => {
        commands::trace_instantiate::run(...)
    }
    FuncCommands::List { verbose, include_peers, visibility } => {
        commands::trace_function_cmd::run_list(...)
    }
    FuncCommands::Show { id } => {
        commands::trace_function_cmd::run_show(...)
    }
    FuncCommands::Bootstrap { force } => {
        commands::trace_bootstrap::run(...)
    }
    FuncCommands::MakeAdaptive { function_id, max_runs } => {
        commands::trace_make_adaptive::run(...)
    }
},
```

The hidden aliases in `TraceCommands` match arm print a deprecation warning, then call the same functions.

### 4.2 Extraction Quality — `detect_parameters()` Rewrite

The key insight: the current `extract_numbers()` function (lines 1034-1062 in `trace_extract.rs`) scans all whitespace-delimited tokens for parseable numbers. This is far too aggressive.

Replace with:
```rust
fn extract_contextual_numbers(text: &str) -> Vec<(String, f64)> {
    let mut results = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();

    let keywords = ["threshold", "limit", "max", "min", "count", "retries",
                     "timeout", "interval", "batch", "size", "depth", "iterations",
                     "workers", "agents", "concurrency", "attempts"];

    for (i, word) in words.iter().enumerate() {
        let cleaned = word.trim_matches(|c: char| !c.is_ascii_digit() && c != '.' && c != '-');
        if let Ok(n) = cleaned.parse::<f64>() {
            if n == 0.0 || n == 1.0 || !n.is_finite() { continue; }
            // Look at surrounding words for context
            let context_window = &words[i.saturating_sub(3)..=(i + 3).min(words.len() - 1)];
            let context = context_window.join(" ").to_lowercase();
            if let Some(keyword) = keywords.iter().find(|&&kw| context.contains(kw)) {
                results.push((keyword.to_string(), n));
            }
            // Otherwise: skip this number (no semantic context)
        }
    }

    // Deduplicate and cap at 3
    results.truncate(3);
    results
}
```

### 4.3 `is_coordinator_noise()` — Tasks to Filter

Based on the coordinator's behavior (see `src/service.rs` and `src/commands/evaluate.rs`), the following task patterns are coordinator-internal noise that should never appear in extracted functions:

- `evaluate-*` — LLM-based evaluation tasks
- `assign-*` — agent assignment tasks (coordinator workflow)

These are infrastructure tasks, not user workflow steps.

### 4.4 Role-Based Template IDs — Heuristic

When `--raw` is NOT specified, derive template IDs from task role:

```rust
fn role_based_id(task: &Task, index: usize, seen: &mut HashSet<String>) -> String {
    let base = if task.skills.iter().any(|s| s == "analysis" || s == "planning") {
        "analyze"
    } else if task.skills.iter().any(|s| s == "implementation" || s == "rust" || s == "coding") {
        "implement"
    } else if task.skills.iter().any(|s| s == "review" || s == "audit") {
        "review"
    } else if task.skills.iter().any(|s| s == "testing" || s == "test") {
        "test"
    } else {
        "step"
    };

    let mut id = base.to_string();
    let mut n = 1;
    while !seen.insert(id.clone()) {
        n += 1;
        id = format!("{}-{}", base, n);
    }
    id
}
```

---

## 5. Testing Strategy

### 5.1 CLI Routing Tests

- `wg func list` produces the same output as `wg trace list-functions`
- `wg func show <id>` produces the same output as `wg trace show-function <id>`
- `wg func apply <id> --input ...` creates the same tasks as `wg trace instantiate <id> --input ...`
- `wg func extract <id>` produces the same function as `wg trace extract <id>`
- Hidden aliases: `wg trace extract <id>` still works but prints deprecation warning
- Hidden aliases: `wg trace instantiate <id>` still works but prints deprecation warning

### 5.2 Extraction Quality Tests

- `is_coordinator_noise()` filters `evaluate-*` and `assign-*` tasks
- `detect_parameters()` with contextual extraction does NOT produce `value_2`..`value_8` for random numbers
- `detect_parameters()` with contextual extraction DOES produce named parameters like `max_retries=3` when "retries" appears near "3"
- Multi-pass generalization replaces instance-specific file paths with `{{input.source_files}}`
- Multi-pass generalization replaces instance-specific feature names with `{{input.feature_name}}`
- `--raw` flag preserves verbatim extraction (backward compat)
- Extracted functions with `--generalize` have valid YAML and pass `validate_function()`

### 5.3 Existing Test Preservation

All existing tests in `trace_extract.rs`, `trace_instantiate.rs`, `trace_function_cmd.rs`, `trace_bootstrap.rs`, and `trace_make_adaptive.rs` must continue to pass unchanged (they test the underlying logic, not the CLI routing).

---

## 6. Migration & Documentation

### 6.1 User-Facing Changes

- New top-level command: `wg func`
- Old `wg trace` function commands still work (with deprecation warning)
- `wg trace instantiate` becomes `wg func apply` (verb change for clarity)
- Extraction produces cleaner functions by default
- `--raw` flag available for verbatim extraction

### 6.2 Agent Guide Updates

The `docs/AGENT-GUIDE.md` should be updated to reference `wg func` instead of `wg trace` for function operations. The `wg trace` section should only cover trace viewing.

### 6.3 Bootstrap Help Text

`wg func bootstrap` output should reference `wg func apply` and `wg func make-adaptive` instead of the old commands.
