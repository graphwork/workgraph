#set text(font: "New Computer Modern", size: 11pt)
#set page(margin: 1in)
#set heading(numbering: "1.1")
#set par(justify: true)

#align(center)[
  #text(size: 18pt, weight: "bold")[Arena Evaluation for Workgraph]
  #v(0.5em)
  #text(size: 12pt)[Integrating FLIP Backward-Inference Scoring into Agent Evaluation, Selection, and Evolution]
  #v(1em)
  #text(size: 10pt, style: "italic")[Synthesized research report -- February 2026]
]

#v(1em)

= Abstract

FLIP (FLipped Inference for Prompt reconstruction) converts evaluation from judgment --- hard for small models --- into generation, measuring response quality via word-level F1 between original instructions and backward-inferred instructions (Wang et al., 2025). This report analyzes four integration points where FLIP can improve workgraph: task evaluation, model selection, context/prompt selection, and agent evolution. Together these form a closed-loop system: cheap FLIP scores on every task feed dense signal into evolution, which produces better agent configurations, which are validated through arena comparisons before deployment. The result is a self-improving agency system that replaces sparse, expensive LLM-as-Judge evaluations with continuous, adversarially-robust measurement.

= Paper Summary

== Problem and Motivation

LLM-as-a-Judge evaluation --- the approach used by workgraph's current `wg evaluate` command --- relies on the evaluator model having strong reasoning and judgment capabilities. Small models perform approximately 41% worse than large models at direct judgment (FLIP paper, \u{00A7}1, p.2), making cheap, scalable evaluation unreliable. This "validation-generation gap" (\u{00A7}5, p.8) means that even GPT-4 achieves only 76% consistency between generating and validating answers; small models exhibit an even larger gap, being relatively better at generation than judgment.

== FLIP Method

FLIP exploits this asymmetry through backward inference (\u{00A7}3, p.3--4):

+ Given a response $y$, ask a model to infer the instruction $x'$ that would produce it: $x' tilde p_phi(x' | y)$
+ Compute word-level F1 between the inferred instruction $x'$ and the actual instruction $x$
+ Use $r = "F1"(x, x')$ as the quality score

The scoring formula (\u{00A7}3, p.4):

#align(center)[
  $"Precision" = (|"tokens"(x) inter "tokens"(x')|) / (|"tokens"(x')|)$
  #h(2em)
  $"Recall" = (|"tokens"(x) inter "tokens"(x')|) / (|"tokens"(x)|)$
]
#align(center)[
  $"F1" = (2 dot "Precision" dot "Recall") / ("Precision" + "Recall")$
]

No learned parameters, no training data, no fine-tuning. The method requires only a generation call plus string matching.

== Key Results

- Outperforms LLM-as-Judge by +99.4% average on RewardBench2 across 13 small models (Table 1, \u{00A7}4.1)
- A 12B FLIP model matches 72.9--76% accuracy of large commercial LLM-as-Judge (Table 2, \u{00A7}4.1)
- Performance gap increases as model size decreases --- 75% improvement for 1B models (\u{00A7}4.1)
- Robust against adversarial attacks and reward hacking (\u{00A7}5, Figure 6)
- Effective for Best-of-N selection (\u{00A7}4.2) and GRPO RL training (\u{00A7}4.3)

== Limitations

- Responses that repeat the instruction verbatim inflate scores (rare, detectable --- \u{00A7}7)
- Cross-language instruction/response pairs break F1 scoring; requires LLM-judge fallback (\u{00A7}7)
- Longer responses yield higher FLIP scores (\u{00A7}5, Figure 5) --- relative ranking mitigates this within arena comparisons, but absolute scores may need normalization

= Integration Architecture

FLIP integrates into workgraph at four points that form a feedback loop:

#align(center)[
  #block(stroke: 0.5pt, inset: 1em, radius: 4pt)[
    #text(size: 10pt)[
    ```
    Task completes
      │
      ├─► [1] FLIP Evaluation ──► source:"flip" Evaluation record
      │       (auto, every task)    │
      │                             ▼
      │                      PerformanceRecord (agent, role, motivation)
      │                             │
      ├─► [2] Model Arena ────────► Win-rate stats ──► Model registry
      │       (on demand)           │
      │                             ▼
      ├─► [3] Context Arena ──────► Best prompt config per task type
      │       (offline batch)       │
      │                             ▼
      └──────────────────────────► [4] Evolution
                                    wg evolve reads dense FLIP scores +
                                    arena win-rates → proposes mutations →
                                    arena-validates before promoting
                                          │
                                          ▼
                                    Better agents (loop back to task dispatch)
    ```
    ]
  ]
]

The four integration points share a common FLIP scoring primitive (`flip_score()`) and differ only in what they vary (model, prompt, agent configuration) and when they run (automatic vs. on-demand vs. offline).

All results flow through the existing `Evaluation` struct (`agency.rs:204`), differentiated by the `source` field: `"flip"` for automatic evaluation, `"arena"` for comparative results. The `record_evaluation()` function (`agency.rs:1203`) propagates scores to agent, role, and motivation `PerformanceRecord` objects unchanged --- no structural modifications to the agency data model are required for Phase 1.

= Evaluation Integration

== Current System

`wg evaluate run <task-id>` (`evaluate.rs:36`) uses LLM-as-Judge: it assembles an `EvaluatorInput` from the task's agent identity, artifacts, and logs; renders an evaluator prompt via `render_evaluator_prompt()` (`agency.rs:398`); spawns a Claude instance; and parses the returned JSON `{score, dimensions, notes}` into an `Evaluation` with `source: "llm"`. An alternative entry point, `wg evaluate record` (`evaluate.rs:327`), accepts externally-sourced scores.

Current limitations: each evaluation requires a full LLM API call (\u{007E}\$0.01--0.10), making evaluation opt-in and sparse. Single-evaluator bias. No adversarial robustness. No relative comparison mechanism.

== FLIP as Evaluation Component

FLIP maps directly onto workgraph concepts:

#table(
  columns: (1fr, 1fr),
  inset: 8pt,
  [*FLIP concept*], [*Workgraph equivalent*],
  [Instruction $x$], [Task description + title],
  [Response $y$], [Agent log entries + artifact contents],
  [Model $phi$], [Any small LM (1B--12B), configurable],
  [Score $r$], [`Evaluation.score` with `source: "flip"`],
)

The backward-inference prompt: _"Infer a single instruction that would most plausibly generate the given response. Output ONLY the instruction, nothing else."_ The response $y$ is assembled from task logs and artifacts, reusing the same data extraction that `render_evaluator_prompt()` already performs.

== Complement, Not Replace

FLIP measures *instruction adherence* --- did the agent do what was asked? This maps to the `correctness` and `completeness` evaluation dimensions. It does not measure code quality, stylistic concerns, or nuanced trade-offs (the `efficiency` and `style_adherence` dimensions), which still require LLM-as-Judge.

The recommended architecture runs both in parallel:

#table(
  columns: (auto, 1fr, 1fr),
  inset: 8pt,
  [], [*FLIP (automatic)*], [*LLM-as-Judge (on-demand)*],
  [Trigger], [Every completed task], [Explicit request or high-stakes tasks],
  [Cost], [\u{007E}\$0 (local model)], [\u{007E}\$0.01--0.10 per eval],
  [Measures], [Instruction adherence], [Quality, style, nuance],
  [Source tag], [`"flip"`], [`"llm"`],
  [Adversarial robustness], [High (\u{00A7}5, Figure 6)], [Low (\u{00A7}5)],
)

== Data Flow

+ Agent completes task (`wg done`)
+ Coordinator post-completion hook triggers FLIP evaluation automatically
+ FLIP: extract instruction $x$ from task title + description; extract response $y$ from logs + artifacts; run backward inference through small model; compute F1
+ Build `Evaluation` with `source: "flip"`, `evaluator: "flip:<model>"`, dimensions including `instruction_adherence`, `precision`, `recall`, and `notes` containing the inferred instruction
+ `record_evaluation()` saves to `.workgraph/agency/evaluations/` and updates `PerformanceRecord` on agent, role, and motivation

== Implementation: evaluate.rs Changes

Add an `--method flip` flag to `wg evaluate run`. The FLIP path constructs the backward-inference prompt from task data, calls a configurable small model, computes word-level F1, and records with `source: "flip"`. The F1 function is a pure \u{007E}15-line Rust function with no external dependencies. The `Evaluation` struct requires no changes --- `source`, `dimensions`, and `evaluator` fields already accommodate FLIP data. Add `auto_flip_eval: bool` and `flip_model: String` to the agency config.

= Model Selection

== Current System

Model resolution follows a static hierarchy (`spawn.rs:209--213`): `task.model` > `executor.model` > `coordinator.model` > default. The model registry (`src/models.rs`, `.workgraph/models.yaml`) catalogs models with cost, tier (frontier/mid/budget), capabilities, and context window metadata, but the coordinator does not query it when spawning.

== Arena for Model Selection

Best-of-N selection using FLIP (\u{00A7}4.2):

+ For a task with description $x$, run through $N$ candidate models producing responses ${y_1, ..., y_N}$
+ FLIP-score each: $r_i = "F1"(x, "FLIP"(y_i))$
+ Select the highest-scoring response

This is model-agnostic and training-free. The FLIP evaluator can be any small model; it does not need to understand code quality, only generate plausible instructions.

== When to Use Arena Selection

*Use when:* high-value task routing (probe cost small relative to full task), evolution validation, unfamiliar task types, model onboarding.

*Skip when:* proven model track record (high `avg_score`), budget-constrained runs, latency-sensitive tasks.

== Cost Analysis

For a 10k-token description and 50k-token response with 3 candidate models:

#table(
  columns: (1fr, auto),
  inset: 8pt,
  [*Component*], [*Cost*],
  [3 model runs (avg \$3/1M in, \$15/1M out)], [\u{007E}\$2.40],
  [3 FLIP evaluations (budget model)], [\u{007E}\$0.05],
  [Normal single run], [\u{007E}\$0.80],
  [*Arena overhead*], [*\u{007E}\$1.65 (\u{007E}2\u{00D7} normal)*],
)

Arena is cost-effective when quality gains prevent downstream retries. A single failed attempt (\$0.80) plus retry (\$0.80) equals \$1.60 --- comparable to arena's upfront cost.

== Probe Strategy

For cost efficiency, don't run the full task. Ask each candidate model to produce a plan or outline (first 500 tokens). FLIP scoring works on partial responses --- if the plan captures the task intent, FLIP will reconstruct the instruction. This reduces arena cost by \u{007E}10\u{00D7}.

== Win-Rate Tracking

Store arena results in `.workgraph/arena-stats.yaml` keyed by model ID, tracking `tasks_entered`, `wins`, and `avg_flip_score`. This data feeds into automatic model recommendation --- over time, the system learns which model tier works for which task types.

== Recommended Approach

Start with an explicit `wg arena-select` command (no coordinator changes, user opt-in, easy to validate). Graduate to automatic arena in the coordinator once win-rate data demonstrates meaningful quality differences.

= Context Selection

== Current System

The coordinator constructs agent prompts via `TemplateVars` (`executor.rs:17--90`), assembling: skills preamble, agent identity (role + motivation + resolved skills via `render_identity_prompt()`), task metadata, dependency context, loop info, and workflow boilerplate. Every component is static per-task --- no mechanism exists to evaluate whether a different framing would produce better output.

== Arena for Context Selection

Given a task, generate $N$ prompt variants by varying the context configuration, run each through the same model, FLIP-score, and select the best:

#table(
  columns: (auto, 1fr),
  inset: 8pt,
  [*Dimension*], [*What varies*],
  [Role description], [Same role, different phrasing of description and desired outcome],
  [Motivation framing], [Emphasize speed vs. quality vs. thoroughness],
  [Skills preamble], [All skills, relevant-only, or none],
  [Task description detail], [Full, summary, or description + exemplar],
  [Dependency context], [All upstream artifacts vs. summaries vs. most-recent-only],
  [Workflow verbosity], [Full boilerplate vs. minimal vs. none],
)

Context arena runs offline as a batch experiment, not per-dispatch. Cache results keyed by `(role, skill_set, task_tags)` to amortize across similar tasks. Use relative ranking to mitigate FLIP's response-length bias (\u{00A7}5, Figure 5).

== Application to Function Generalization

`wg func extract --generalize` (`func_extract.rs:473--548`) runs three LLM passes to convert raw traces into reusable templates. For each pass, arena can compare prompt variants --- e.g., different abstraction levels, example counts, or output formats. FLIP scores whether the generalized template, when re-instantiated, produces descriptions that recover the original task intent. A bad generalization that strips too much specificity scores low.

== Application to Func Planning

Layer 2 generative functions have `PlanningConfig` with a planner template. Different planning prompts produce different graph topologies. Arena can score each planned task's description against the function's high-level intent, testing whether the planner decomposes goals into demonstrably-related subtasks. The `static_fallback` mechanism enables direct comparison between planner output and static templates.

== Agency System Integration

Arena enables empirical selection of role/motivation pairings. Given a task type, test which motivation produces the best results by running all candidates through the same model and FLIP-scoring. Similarly, when multiple agents skill-match for a task, arena can select the best agent. Results stored as `source: "arena"` evaluations feed directly into evolution.

= Evolution Input

== Current System

`wg evolve` (`evolve.rs`) loads all roles, motivations, and evaluations; builds a performance summary via `build_performance_summary()` with per-role scores, dimension breakdowns, and a synergy matrix; invokes an evolver LLM with strategy-specific prompts; and applies structured operations (`create_role`, `modify_role`, `retire_role`, etc.). Each evolved entity tracks lineage: `parent_ids`, `generation`, `created_by` (`agency.rs:52`).

*Key limitation:* evolution quality is bounded by evaluation density. With sparse, expensive LLM-as-Judge evaluations, the evolver makes noisy decisions and the synergy matrix is often incomplete.

== Arena as Evolution Signal

Arena rankings provide *relative* signal --- which agent variant performs better on the same task --- rather than absolute scores that are noisy across task types. Both variants face identical tasks, normalizing for difficulty. FLIP's adversarial robustness (\u{00A7}5, Figure 6) means agents cannot evolve to exploit the evaluator at the expense of genuine capability.

Concrete change: extend `build_performance_summary()` to report arena win-rates alongside `avg_score`, giving the evolver strictly better signal for mutation and retirement decisions.

== Arena-Gated Evolution

After `wg evolve` proposes a mutation (e.g., `analyst` \u{2192} `analyst-v2`):

+ Apply mutation provisionally (with lineage tracking)
+ Run arena: both parent and child on $N$ sampled tasks
+ FLIP-score each pair; record win-rate
+ Promote if child win-rate exceeds threshold; keep both for diversity if roughly equal; discard if child loses

This mirrors the paper's Best-of-N selection (\u{00A7}4.2) with $N = 2$.

== Lineage Integration

Add an `ArenaResult` struct recording opponent ID, task IDs, wins, losses, draws, per-task FLIP scores, and timestamp. Store in `PerformanceRecord` alongside existing `evaluations`. This makes the evolutionary trajectory auditable: lineage queries show arena results at each generation.

== Maintaining Diversity

Arena risks convergence to a single strategy. Mitigations:

- *Task-type arenas:* run within categories (e.g., "code review" tasks). A role that loses globally may win its niche
- *Elo ratings:* handle transitive relationships and prevent single-winner domination
- *Retirement protection:* don't retire a role that wins its niche arena, even if its global `avg_score` is low. A specialist with 0.6 global but 0.9 on its task type should survive
- *Diversity pressure:* include coverage metrics in the evolver prompt; instruct it to maintain coverage across all task types

= Unified Design

The four integration points form a coherent closed-loop system. The central insight is that FLIP provides a single, cheap, robust scoring primitive that unifies evaluation, selection, and evolution under one measurement framework.

== The FLIP Scoring Primitive

All four components share a single function:

```rust
fn flip_score(instruction: &str, response: &str, model: &str) -> FlipResult
```

This lives in a new `src/arena.rs` module, producing `FlipResult { inferred_instruction, f1_score, precision, recall }`. The function is called by: (1) the evaluator for automatic task scoring, (2) the model arena for comparing model outputs, (3) the context arena for comparing prompt variants, and (4) the evolution system for validating mutations.

== Flow: Evaluation \u{2192} Evolution \u{2192} Selection \u{2192} Better Agents

*Dense evaluation enables informed evolution.* Currently, `wg evolve` operates on sparse data. With automatic FLIP evaluation on every task, `build_performance_summary()` receives dense signal: per-task instruction-adherence scores, precision/recall breakdowns, and coverage across task types. The evolver LLM can make better-informed mutation decisions.

*Arena-gated evolution ensures mutations help.* Instead of applying mutations blindly, arena comparison validates that a child role actually outperforms its parent on representative tasks. This prevents evolutionary drift and wasted cycles.

*Evolution improves selection inputs.* Better roles and motivations (from evolution) produce better prompt configurations (for context selection) and better agent assignments (for model selection). Arena results from context and model selection feed back as `source: "arena"` evaluations, further enriching evolution data.

*The loop closes.* Better agents produce better task outputs, which receive higher FLIP scores, which reinforce the evolutionary direction. Poor configurations receive low scores, triggering mutation or retirement. The system converges toward configurations that demonstrably match task requirements.

== Consistent Data Model

All four components write to the same `Evaluation` struct, differentiated only by `source`:

#table(
  columns: (auto, auto, 1fr),
  inset: 8pt,
  [*Source*], [*Component*], [*When*],
  [`"flip"`], [Automatic evaluation], [Every completed task],
  [`"flip-manual"`], [Explicit FLIP eval], [User runs `wg evaluate --method flip`],
  [`"arena-model"`], [Model arena], [On-demand comparison],
  [`"arena-context"`], [Context arena], [Offline batch experiment],
  [`"arena-evolution"`], [Evolution validation], [After `wg evolve --arena-validate`],
  [`"llm"`], [LLM-as-Judge], [Explicit request (unchanged)],
)

Downstream consumers (`evolve.rs`, `stats`, `show`) can filter or weight by source. The `PerformanceRecord` aggregation works unchanged for Phase 1; source-aware weighting (e.g., FLIP at 0.5\u{00D7}, LLM at 1.0\u{00D7}, manual at 1.5\u{00D7}) is a Phase 3 enhancement.

= Implementation Roadmap

== Phase 1: FLIP Scoring Primitive and Automatic Evaluation

*Goal:* Dense evaluation data on every completed task.

+ Add `src/arena.rs` with `flip_score()` and `token_f1()` functions
+ Add `--method flip` to `wg evaluate run` in `evaluate.rs`
+ Add `auto_flip_eval` and `flip_model` to agency config
+ Hook into coordinator post-completion in `service.rs` to auto-trigger FLIP evaluation
+ Record with `source: "flip"`, `evaluator: "flip:<model>"`

*Files:* `src/arena.rs` (new, \u{007E}60 lines), `evaluate.rs` (+\u{007E}80 lines), `service.rs` (+\u{007E}10 lines), `config.rs` (+2 fields).

*Prerequisite:* None. Immediately useful for denser evolution signal.

== Phase 2: Model Arena

*Goal:* Empirical model selection via Best-of-N comparison.

+ Add `wg arena-select <task-id>` command in `src/commands/arena.rs`
+ Support `--candidates N`, `--models list`, `--tier filter`, `--dry-run`
+ Use probe strategy (plan-only, first 500 tokens) for cost efficiency
+ Store win-rate stats in `.workgraph/arena-stats.yaml`
+ Integrate with model registry for candidate filtering by tier and capability

*Files:* `src/commands/arena.rs` (new, \u{007E}200 lines), `models.rs` (minor registry queries).

*Prerequisite:* Phase 1 (uses `flip_score()`).

== Phase 3: Context Arena and Evolution Integration

*Goal:* Empirical prompt optimization and arena-gated evolution.

+ Add `wg arena context <task-id>` for prompt variant comparison
+ Add `wg arena motivation <task-id>` for motivation selection
+ Add `--arena-validate` to `wg evolve` for mutation validation
+ Extend `build_performance_summary()` with arena win-rates
+ Add `ArenaResult` struct to `PerformanceRecord`
+ Implement provisional mutation status in evolution pipeline

*Files:* `arena.rs` (+\u{007E}150 lines), `evolve.rs` (+\u{007E}100 lines), `agency.rs` (+`ArenaResult` struct).

*Prerequisite:* Phase 2 (uses arena runner infrastructure).

== Phase 4: Advanced Optimization

*Goal:* Elo ratings, diversity protection, automatic coordinator arena.

+ Compute Elo ratings from accumulated arena results
+ Add diversity metrics (skill coverage, niche win-rates) to evolver prompt
+ Protect niche specialists from retirement via arena-aware thresholds
+ Optional: automatic arena in coordinator for unassigned-model tasks
+ Optional: arena for `--generalize` prompts and Layer 2 planner templates
+ Source-aware weighting in `PerformanceRecord` aggregation

*Files:* `agency.rs` (Elo computation), `evolve.rs` (diversity metrics), `service.rs` (coordinator arena).

*Prerequisite:* Phase 3 plus accumulated arena data.

= Open Questions

+ *FLIP model selection:* Which small model works best for backward inference on workgraph tasks? The paper tests 1B--12B models (\u{00A7}4.1); we need empirical validation on code-heavy task outputs. Running a small benchmark across 2--3 candidate FLIP models would determine the default `flip_model` config value.

+ *Response length normalization:* FLIP scores correlate with response length (\u{00A7}5, Figure 5). Within-arena relative ranking handles this, but automatic evaluation scores on individual tasks may need normalization --- e.g., dividing by `log(response_length)` or bucketing by response size.

+ *Task description quality:* FLIP measures instruction recovery from the response. If a task description is vague or poorly written, even a perfect response will score low because the description itself is hard to reconstruct. Should FLIP scores penalize vague task descriptions (useful signal) or should we normalize for description specificity?

+ *Probe fidelity for model arena:* The 500-token probe strategy reduces cost by \u{007E}10\u{00D7}, but does plan quality predict full-task quality? This needs empirical validation --- run full and probe arenas on the same tasks and measure rank correlation.

+ *Local vs. API FLIP model:* Using a local model (\u{007E}\$0 per eval) is ideal for automatic evaluation, but requires infrastructure. An API-based budget model (e.g., Haiku at \u{007E}\$0.001/eval) is simpler to deploy. The initial implementation should support both via the `flip_model` config.

+ *Arena sample size:* How many tasks are needed for reliable arena comparison during evolution validation? The paper doesn't address sample efficiency for Best-of-N in a task-diverse setting. A minimum of 5--10 tasks per comparison seems reasonable, but this should be tunable.

+ *Interaction with `verify` criteria:* Tasks can have explicit verification criteria (the `verify` field). Should FLIP evaluation incorporate these criteria into the instruction text $x$, or score against the base task description only? Including `verify` criteria would make FLIP scores stricter and more aligned with task-specific quality requirements.

+ *Evolution convergence criteria:* When should arena-gated evolution stop? If the child consistently ties the parent (win-rate \u{007E}50%), further mutation may not help. Define a convergence threshold and a diversity floor below which evolution preserves the current population.
