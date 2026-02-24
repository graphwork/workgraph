# Arena-Based Context Selection for Workgraph

How FLIP backward-inference scoring (Wang et al., 2025, arXiv:2602.13551) can drive empirical selection of prompts, contexts, and agent configurations in workgraph.

## 1. Current Context Construction

When the coordinator spawns an agent, it constructs a prompt via `TemplateVars` (`src/service/executor.rs:17-90`). The prompt is assembled from these components:

| Component | Source | Template var |
|-----------|--------|-------------|
| Skills preamble | `.claude/skills/using-superpowers/SKILL.md` | `{{skills_preamble}}` |
| Agent identity | Role + Motivation + resolved skills via `render_identity_prompt()` (`src/agency.rs:328-366`) | `{{task_identity}}` |
| Task metadata | Task ID, title, description | `{{task_id}}`, `{{task_title}}`, `{{task_description}}` |
| Dependency context | Artifacts/logs from upstream tasks | `{{task_context}}` |
| Loop info | Cycle iteration count + convergence instructions | `{{task_loop_info}}` |
| Workflow boilerplate | `wg log`, `wg done`, graph patterns, func references | (hardcoded in template) |

The full prompt template is a ~60-line Markdown document (`executor.rs:372-444`). The identity section renders the agent's role name, description, skills with inline content, desired outcome, acceptable trade-offs, and non-negotiable constraints.

**Key observation:** Every part of this prompt is currently static per-task. The role, motivation, skill descriptions, and workflow instructions are fixed once the agent is assigned. There is no mechanism to evaluate whether a different framing would produce better output.

## 2. Arena for Context Selection

**Idea:** Given a task, generate N prompt variants by varying the context configuration, run each through the same model, FLIP-score the outputs, and select the best prompt configuration.

### What can vary

1. **Role description wording** — same role, different phrasing of `description` and `desired_outcome`
2. **Motivation framing** — emphasize speed vs. quality vs. thoroughness
3. **Skills preamble** — include all skills, only relevant skills, or no skills
4. **Task description detail** — full description vs. summary vs. description + exemplar output
5. **Dependency context window** — all upstream artifacts vs. summaries vs. most-recent-only
6. **Workflow instruction verbosity** — full boilerplate vs. minimal vs. none

### Protocol

```
for each prompt variant P_i:
    response_i = model(P_i)          # same model, same task
    x'_i = FLIP_model(response_i)    # backward inference: infer instruction from response
    score_i = F1(task_description, x'_i)

select P_best = argmax(score_i)
```

The FLIP score measures how faithfully the response tracks the original task instruction (spec, Table 1). A prompt that causes the agent to drift from the task objective scores lower.

### Practical considerations

- **Cost:** N model calls per task. Run offline as a batch experiment, not on every dispatch. The FLIP scoring itself uses a small model (1B-12B), so scoring is cheap (spec §4.1).
- **Stationarity:** Prompt quality depends on task type. Cache results keyed by `(role, skill_set, task_tags)` to amortize across similar tasks.
- **Response length bias:** FLIP scores correlate with response length (spec §5, Figure 5). Normalize by comparing variants against each other (relative ranking), not absolute scores.

## 3. Application to `--generalize`

`wg func extract --generalize` (`src/commands/func_extract.rs:473-548`) runs three LLM passes to convert a raw trace into a reusable function template:

1. **Pass 1:** Identify task roles from descriptions and graph position
2. **Pass 2:** Rewrite titles/descriptions to be generic (replace concrete values with `{{input.*}}` placeholders)
3. **Pass 3:** Extract placeholder parameters with types and defaults

Each pass sends a distinct prompt to the claude executor and parses JSON output. The quality of generalization directly impacts whether the function template produces good instantiations later.

### Arena application

For each generalization pass, generate N prompt variants and FLIP-score:

```
# For pass 2 (description rewriting):
for each prompt variant P_i:
    rewrite_i = model(P_i + raw_function)
    # Apply rewrite to get a generalized function
    func_i = merge(raw_function, rewrite_i)
    # Instantiate with test inputs
    instance_i = substitute(func_i, test_inputs)
    # Score: does the instantiated description recover the original task intent?
    x'_i = FLIP_model(instance_i.description)
    score_i = F1(original_task.description, x'_i)
```

**What this tests:** Whether the generalized template, when re-instantiated, produces task descriptions that faithfully represent the original intent. A bad generalization that strips too much specificity will score low because the FLIP model can't recover the original instruction from the vague output.

### Variant dimensions for generalization prompts

- **Abstraction level:** "Generalize completely" vs. "Keep domain-specific terms" vs. "Generalize structure, keep vocabulary"
- **Example inclusion:** Show 0, 1, or 2 examples of good generalizations in the prompt
- **Output format:** Free-form vs. structured JSON vs. YAML-in-markdown

## 4. Application to Trace Functions (Layer 2 Planning)

Layer 2 generative functions (`src/function.rs:209-225`) have a `PlanningConfig` with a `planner_template` — a task that generates the execution plan at apply-time rather than using static tasks.

The planner prompt determines the quality of the generated task graph. Different planning prompts produce different graph topologies, task granularities, and skill assignments.

### Arena application

```
for each planner prompt variant P_i:
    plan_i = model(P_i + function_inputs)
    # Validate against structural constraints
    if !valid(plan_i, function.constraints):
        score_i = 0
        continue
    # Score each task in the plan via FLIP
    task_scores = []
    for task in plan_i.tasks:
        x' = FLIP_model(task.description)
        task_scores.append(F1(function.description, x'))
    score_i = mean(task_scores)
```

**What this tests:** Whether the planner decomposes the high-level function intent into tasks that each demonstrably relate to the original goal. A planner that generates tangential or vague tasks scores low.

### Variant dimensions for planning prompts

- **Decomposition strategy:** "Break into phases" vs. "Break by component" vs. "Break by skill"
- **Constraint emphasis:** Lead with constraints vs. mention at end vs. embed inline
- **Memory inclusion** (Layer 3): Include 0, 3, or all past run summaries from `TraceMemoryConfig`
- **Output structure:** `workgraph-yaml` vs. free-form with parsing

### Integration with `static_fallback`

If `PlanningConfig.static_fallback = true`, the function has both a planner and static tasks. Arena can compare the planner output against the static fallback:

```
score_planner = FLIP_score(planner_generated_plan)
score_static  = FLIP_score(static_tasks)
if score_static > score_planner:
    use static_tasks  # planner didn't improve on the template
```

## 5. Integration with the Agency System

The agency system (`src/agency.rs`) pairs Roles with Motivations to form Agents. `render_identity_prompt()` (line 328) injects the role's description, skills, desired outcome, and the motivation's trade-off constraints into the agent prompt.

Different role/motivation pairings produce different behavioral constraints. The evolution system (`src/commands/evolve.rs`) already mutates these, but currently relies on sparse LLM-as-Judge evaluations to measure fitness.

### Arena for motivation selection

Given a task type (identified by skills + tags), test which motivation produces the best results:

```
for each motivation M_i in candidate_motivations:
    identity_i = render_identity_prompt(role, M_i, skills)
    prompt_i = build_prompt(task, identity_i)
    response_i = model(prompt_i)
    score_i = FLIP(task.description, response_i)

best_motivation = argmax(score_i)
```

### Arena for role selection

Similarly, when the coordinator has multiple agents with different roles that all skill-match for a task:

```
for each agent A_i with matching skills:
    identity_i = render_identity_prompt(A_i.role, A_i.motivation, A_i.skills)
    prompt_i = build_prompt(task, identity_i)
    response_i = model(prompt_i)
    score_i = FLIP(task.description, response_i)

assign task to argmax(score_i)
```

### Feeding arena results into evolution

Arena scores provide cheap, high-volume signal for the evolution system:

- **Per-task-type leaderboard:** Track which `(role, motivation)` pairs win arena comparisons for each task type (keyed by skill set + tags)
- **Evolution input:** `build_performance_summary()` in `evolve.rs` currently uses sparse `Evaluation` records. Arena scores stored as `source: "arena"` evaluations provide dense signal
- **Retirement signal:** A role/motivation that consistently loses arena comparisons is a candidate for retirement or mutation

## 6. Implementation Sketch

### Phase 1: FLIP scoring primitive

Add a `flip_score(instruction: &str, response: &str, model: &str) -> f64` function:

```rust
// src/arena.rs (new module)

pub struct FlipResult {
    pub inferred_instruction: String,
    pub f1_score: f64,
    pub precision: f64,
    pub recall: f64,
}

/// Run FLIP backward inference and score.
/// `model` is a small model identifier (e.g., "haiku").
pub fn flip_score(instruction: &str, response: &str, model: &str) -> Result<FlipResult> {
    let prompt = format!(
        "Infer a single instruction that would most plausibly \
         generate the following response. Output ONLY the instruction, \
         nothing else.\n\nResponse:\n{}", response
    );
    let inferred = call_model(model, &prompt)?;
    let (precision, recall, f1) = token_f1(instruction, &inferred);
    Ok(FlipResult { inferred_instruction: inferred, f1_score: f1, precision, recall })
}

/// Word-level F1 between two strings (per spec §2).
fn token_f1(reference: &str, candidate: &str) -> (f64, f64, f64) {
    let ref_tokens: HashSet<&str> = reference.split_whitespace().collect();
    let cand_tokens: HashSet<&str> = candidate.split_whitespace().collect();
    let overlap = ref_tokens.intersection(&cand_tokens).count() as f64;
    let precision = if cand_tokens.is_empty() { 0.0 } else { overlap / cand_tokens.len() as f64 };
    let recall = if ref_tokens.is_empty() { 0.0 } else { overlap / ref_tokens.len() as f64 };
    let f1 = if precision + recall == 0.0 { 0.0 } else { 2.0 * precision * recall / (precision + recall) };
    (precision, recall, f1)
}
```

### Phase 2: Arena runner

```rust
// src/arena.rs

pub struct ArenaResult {
    pub variants: Vec<VariantResult>,
    pub winner_index: usize,
}

pub struct VariantResult {
    pub label: String,
    pub response: String,
    pub flip_score: f64,
}

/// Run an arena comparison: N prompt variants, same model, FLIP-scored.
pub fn run_arena(
    variants: Vec<(String, String)>,  // (label, prompt)
    model: &str,
    flip_model: &str,
    instruction: &str,  // original task description for FLIP scoring
) -> Result<ArenaResult> {
    let mut results = Vec::new();
    for (label, prompt) in &variants {
        let response = call_model(model, prompt)?;
        let flip = flip_score(instruction, &response, flip_model)?;
        results.push(VariantResult {
            label: label.clone(),
            response,
            flip_score: flip.f1_score,
        });
    }
    let winner = results.iter().enumerate()
        .max_by(|a, b| a.1.flip_score.partial_cmp(&b.1.flip_score).unwrap())
        .map(|(i, _)| i).unwrap_or(0);
    Ok(ArenaResult { variants: results, winner_index: winner })
}
```

### Phase 3: CLI integration

```
wg arena context <task-id>              # compare prompt variants for a task
wg arena generalize <function-id>       # compare generalization prompts
wg arena motivation <task-id>           # compare motivations for a task type
wg arena planner <function-id>          # compare planning prompts (Layer 2)
```

Each subcommand would:
1. Generate variants from the relevant dimension
2. Call `run_arena()`
3. Print ranked results with scores
4. Optionally record the winner as an evaluation with `source: "arena"`

### Phase 4: Coordinator integration

In `src/commands/service.rs`, before dispatching a task:

```rust
if config.arena.auto_select_context {
    let variants = generate_context_variants(&task, &agency_dir);
    let arena_result = run_arena(variants, &model, &flip_model, &task.description);
    // Use winning prompt configuration
    apply_winning_config(&mut task, &arena_result);
}
```

This should be opt-in (`wg config arena.auto_select_context true`) since it multiplies API calls by N per task.

### Cost model

| Operation | Calls | Model size | Cost estimate |
|-----------|-------|------------|--------------|
| FLIP scoring (per variant) | 1 | 1B-12B (local or haiku) | ~$0.001 |
| Response generation (per variant) | 1 | Task model (sonnet/opus) | ~$0.01-0.10 |
| Arena with 3 variants | 6 total | Mixed | ~$0.03-0.30 |
| Arena with 5 variants | 10 total | Mixed | ~$0.05-0.50 |

For offline batch experiments (generalization, motivation tuning), cost is one-time. For per-task context selection, the N-multiplier makes it suitable only for high-value tasks or as an occasional calibration run.
