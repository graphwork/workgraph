# Agency System

The agency system gives workgraph agents composable identities. Instead of every agent being a generic assistant, you define **roles** (what an agent does), **tradeoffs** (why it acts that way), and pair them into **agents** that are assigned to tasks, evaluated, and evolved over time.

Agents can be **human or AI**. The difference is the executor: AI agents use `claude` (or similar), human agents use `matrix`, `email`, or `shell`. Both share the same identity model — roles, tradeoffs, capabilities, trust levels, and performance tracking all work uniformly regardless of who (or what) is doing the work.

## Core Concepts

### Role

A role defines **what** an agent does.

| Field | Description | Identity-defining? |
|-------|-------------|--------------------|
| `name` | Human-readable label (e.g. "Programmer") | No |
| `description` | What this role is about | Yes |
| `skills` | List of skill references (see [Skill System](#skill-system)) | Yes |
| `desired_outcome` | What good output looks like | Yes |
| `performance` | Aggregated evaluation scores | No (mutable) |
| `lineage` | Evolutionary history | No (mutable) |
| `default_context_scope` | Default context scope for tasks dispatched with this role (`clean`, `task`, `graph`, `full`) | No (mutable) |

### Tradeoff

A tradeoff defines **why** an agent acts the way it does.

| Field | Description | Identity-defining? |
|-------|-------------|--------------------|
| `name` | Human-readable label (e.g. "Careful") | No |
| `description` | What this tradeoff prioritizes | Yes |
| `acceptable_tradeoffs` | Compromises the agent may make | Yes |
| `unacceptable_tradeoffs` | Hard constraints the agent must never violate | Yes |
| `performance` | Aggregated evaluation scores | No (mutable) |
| `lineage` | Evolutionary history | No (mutable) |

### Agent

An agent is the **unified identity** in workgraph — it can represent a human or an AI. For AI agents, it is a named pairing of a role and a tradeoff. For human agents, role and tradeoff are optional.

| Field | Description |
|-------|-------------|
| `name` | Human-readable label |
| `role_id` | Content-hash ID of the role (required for AI, optional for human) |
| `tradeoff_id` | Content-hash ID of the tradeoff (required for AI, optional for human) |
| `capabilities` | Skills/capabilities for task matching (e.g., `rust`, `testing`) |
| `rate` | Hourly rate for cost forecasting |
| `capacity` | Maximum concurrent task capacity |
| `trust_level` | `Verified`, `Provisional` (default), or `Unknown` |
| `contact` | Contact info — email, Matrix ID, etc. (primarily for human agents) |
| `executor` | How this agent receives work: `claude` (default), `matrix`, `email`, `shell` |
| `performance` | Agent-level aggregated evaluation scores |
| `lineage` | Evolutionary history |

The same role paired with different tradeoffs produces different agents. A "Programmer" role with a "Careful" tradeoff produces a different agent than with a "Fast" tradeoff.

Human agents are distinguished by their executor. Agents with a human executor (`matrix`, `email`, `shell`) don't need a role or tradeoff — they're real people who bring their own judgment. AI agents (executor `claude`) require both, because the role and tradeoff are injected into the prompt to shape behavior.

## Content-Hash IDs

Every role, tradeoff, and agent is identified by a **SHA-256 content hash** of its identity-defining fields.

- **Deterministic**: Same content → same ID
- **Deduplication**: Can't create two entities with identical content
- **Immutable identity**: Changing an identity-defining field produces a *new* entity. The old one stays.

| Entity | Hashed fields |
|--------|---------------|
| Role | `skills` + `desired_outcome` + `description` |
| Tradeoff | `acceptable_tradeoffs` + `unacceptable_tradeoffs` + `description` |
| Agent | `role_id` + `tradeoff_id` |

For display, IDs are shown as 8-character prefixes (e.g. `a3f7c21d`). All commands accept unique prefixes.

## The Full Agency Loop

The agency system runs as a loop: assign identity → execute task → evaluate → evolve. Each step can be manual or automated.

```
┌─────────────┐     ┌───────────┐     ┌───────────┐     ┌──────────┐
│  1. Assign  │────>│ 2. Execute│────>│3. Evaluate│────>│ 4. Evolve│
│  identity   │     │   task    │     │  results  │     │  agency  │
│  to task    │     │  (agent   │     │  (score   │     │  (create │
│             │     │   runs)   │     │   agent)  │     │   new    │
│  wg assign  │     │  wg spawn │     │ wg evaluate│    │  roles)  │
└─────────────┘     └───────────┘     └───────────┘     └──────────┘
       ▲                                                      │
       └──────────────────────────────────────────────────────┘
                    performance data feeds back
```

### Manual loop

```bash
# 1. Assign
wg assign my-task a3f7c21d

# 2. Execute (service handles this)
wg service start

# 3. Evaluate
wg evaluate run my-task

# 4. Evolve
wg evolve run
```

### Automated loop

```bash
# Enable auto-assign and auto-evaluate
wg config --auto-assign true --auto-evaluate true

# The coordinator creates assign-{task} and evaluate-{task} meta-tasks automatically
# Just start the service and add work:
wg service start
wg add "Implement feature X" --skill rust

# Evolution is still manual (run when you have enough evaluations):
wg evolve run
```

## Lifecycle

### 1. Create roles and tradeoffs

```bash
# Create a role
wg role add "Programmer" \
  --outcome "Working, tested code" \
  --skill code-writing \
  --skill testing \
  --description "Writes, tests, and debugs code"

# Create a tradeoff
wg tradeoff add "Careful" \
  --accept "Slow" \
  --accept "Verbose" \
  --reject "Unreliable" \
  --reject "Untested" \
  --description "Prioritizes reliability and correctness above speed"
```

Or seed the built-in starters:

```bash
wg agency init
```

This creates four starter roles (Programmer, Reviewer, Documenter, Architect) and four starter tradeoffs (Careful, Fast, Thorough, Balanced).

### 2. Pair into agents

```bash
# AI agent (role + tradeoff required)
wg agent create "Careful Programmer" --role <role-hash> --tradeoff <tradeoff-hash>

# AI agent with operational fields
wg agent create "Careful Programmer" \
  --role <role-hash> \
  --tradeoff <tradeoff-hash> \
  --capabilities rust,testing \
  --rate 50.0

# Human agent (role + tradeoff optional)
wg agent create "Erik" \
  --executor matrix \
  --contact "@erik:server" \
  --capabilities rust,python,architecture \
  --trust-level verified
```

### 3. Assign to tasks

```bash
wg assign <task-id> <agent-hash>
```

When the service spawns that task, the agent's role and tradeoff are rendered into the prompt as an identity section:

```markdown
# Task Assignment

## Agent Identity

### Role: Programmer
Writes, tests, and debugs code

#### Skills
- code-writing
- testing

#### Desired Outcome
Working, tested code

### Operational Parameters
#### Acceptable Trade-offs
- Slow
- Verbose

#### Non-negotiable Constraints
- Unreliable
- Untested
```

### 4. Evaluate

After a task completes, evaluate the agent's work:

```bash
wg evaluate run <task-id>
wg evaluate run <task-id> --evaluator-model opus
wg evaluate run <task-id> --dry-run    # preview the evaluator prompt
```

You can also record evaluations from external sources (outcome metrics, peer reviews, manual scoring):

```bash
wg evaluate record --task <task-id> --score 0.9 --source "manual"
wg evaluate record --task <task-id> --score 0.85 --source "outcome:sharpe" \
  --dim correctness=0.9 --dim completeness=0.8 --notes "Strong on accuracy"
```

And view evaluation history with filters:

```bash
wg evaluate show                          # all evaluations
wg evaluate show --task <task-id>         # filter by task (prefix match)
wg evaluate show --agent <agent-id>       # filter by agent (prefix match)
wg evaluate show --source "outcome:*"     # filter by source (glob)
wg evaluate show --limit 10               # most recent N
```

The evaluator scores across four dimensions:

| Dimension | Weight | Description |
|-----------|--------|-------------|
| `correctness` | 40% | Does the output match the desired outcome? |
| `completeness` | 30% | Were all aspects of the task addressed? |
| `efficiency` | 15% | Was work done without unnecessary steps? |
| `style_adherence` | 15% | Were project conventions and tradeoff constraints followed? |

The evaluator receives:
- The task definition (title, description, deliverables)
- The agent's identity (role + tradeoff)
- Task artifacts and log entries
- The evaluation rubric

It outputs a JSON evaluation:
```json
{
  "score": 0.85,
  "dimensions": {
    "correctness": 0.9,
    "completeness": 0.85,
    "efficiency": 0.8,
    "style_adherence": 0.75
  },
  "notes": "Implementation is correct and complete. Minor efficiency issue..."
}
```

Scores propagate to three levels:
1. The **agent's** performance record
2. The **role's** performance record (with `tradeoff_id` as context)
3. The **tradeoff's** performance record (with `role_id` as context)

### 4b. FLIP — Fidelity via Latent Intent Probing

FLIP is a **roundtrip intent fidelity** metric that complements standard evaluation. While evaluation judges *quality* (was the approach good?), FLIP judges *fidelity* (did the output match what was asked?).

#### How it works

FLIP runs in two phases:

1. **Inference phase** (sonnet): An LLM reads *only* the agent's output (logs, artifacts, diffs) and reconstructs what the original task prompt must have been — without seeing the actual task description.
2. **Comparison phase** (haiku): A second LLM compares the inferred prompt to the actual task description, scoring similarity across four dimensions.

The resulting `flip_score` (0.0–1.0) measures whether the agent's output faithfully reflects the task spec. High FLIP = output clearly reflects the task. Low FLIP = agent went off-track.

#### FLIP dimensions

| Dimension | Description |
|-----------|-------------|
| `semantic_match` | How closely the inferred intent matches the actual task |
| `requirement_coverage` | What fraction of requirements are reflected in the output |
| `specificity_match` | Whether the output addresses task-specific details vs. generic work |
| `hallucination_rate` | How much of the output addresses things *not* in the task spec |

#### FLIP vs evaluation

FLIP and evaluation are independent — they measure different things and should not be merged into a single score:

- **High quality + low fidelity** = well-crafted code that doesn't match the spec
- **Low quality + high fidelity** = sloppy code that does what was asked
- **Both high** = ideal
- **Both low** = needs rework

#### Low-FLIP verification

When a FLIP score falls below the configured threshold (default: 0.70), the coordinator automatically creates a `.verify-flip-{task-id}` task. This task uses a high-capability model (default: opus) to independently verify whether the work was actually completed correctly.

The verification task receives the original task description, the agent's output, and the low FLIP score, then makes a pass/fail determination.

#### Running FLIP

FLIP runs automatically as part of the `.evaluate-*` task when enabled:

```bash
# Enable globally
wg config --flip-enabled true

# Or tag individual tasks for FLIP evaluation
wg add "My task" --tag flip-eval

# Run manually
wg evaluate run <task-id> --flip
wg evaluate run <task-id> --flip --dry-run    # preview the prompts
```

FLIP evaluations are stored with `source: "flip"` in the evaluations directory, separate from standard evaluations.

#### Per-role model routing

FLIP uses different models for each phase, configured via the model routing system:

| Role | Default model | Rationale |
|------|---------------|-----------|
| `flip_inference` | sonnet | Creative reconstruction requires mid-tier capability |
| `flip_comparison` | haiku | Comparison/scoring is simpler, cost-effective |
| `verification` | opus | Independent verification needs highest capability |

Configure via `[models]` in config.toml:

```toml
[models.flip_inference]
model = "sonnet"

[models.flip_comparison]
model = "haiku"

[models.verification]
model = "opus"
```

#### Pipeline integration

FLIP fits into the coordinator tick as follows:

```
Phase 4:   Task completes -> .evaluate-* created
           Eval script runs standard eval, then FLIP (non-fatal)
Phase 4.5: If FLIP score < threshold -> .verify-flip-* created
Phase 4.6: Auto-evolve (if enabled)
```

### 5. Evolve

Use performance data to improve the agency:

```bash
wg evolve run                                     # full cycle, all strategies
wg evolve run --strategy mutation --budget 3      # targeted changes
wg evolve run --model opus                        # use specific model
wg evolve run --dry-run                           # preview without applying
```

## CLI Reference

### `wg role`

| Command | Description |
|---------|-------------|
| `wg role add <name> --outcome <text> [--skill <spec>] [-d <text>]` | Create a new role |
| `wg role list` | List all roles |
| `wg role show <id>` | Show details |
| `wg role edit <id>` | Edit in `$EDITOR` (re-hashes on save) |
| `wg role rm <id>` | Delete a role |
| `wg role lineage <id>` | Show evolutionary ancestry |

### `wg tradeoff`

| Command | Description |
|---------|-------------|
| `wg tradeoff add <name> --accept <text> --reject <text> [-d <text>]` | Create a new tradeoff |
| `wg tradeoff list` | List all tradeoffs |
| `wg tradeoff show <id>` | Show details |
| `wg tradeoff edit <id>` | Edit in `$EDITOR` (re-hashes on save) |
| `wg tradeoff rm <id>` | Delete a tradeoff |
| `wg tradeoff lineage <id>` | Show evolutionary ancestry |

### `wg agent`

| Command | Description |
|---------|-------------|
| `wg agent create <name> [OPTIONS]` | Create an agent (see options below) |
| `wg agent list` | List all agents |
| `wg agent show <id>` | Show details with resolved role/tradeoff |
| `wg agent rm <id>` | Remove an agent |
| `wg agent lineage <id>` | Show agent + role + tradeoff ancestry |
| `wg agent performance <id>` | Show evaluation history |

**`wg agent create` options:**

| Option | Description |
|--------|-------------|
| `--role <ID>` | Role ID or prefix (required for AI agents) |
| `--tradeoff <ID>` | Tradeoff ID or prefix (required for AI agents) |
| `--capabilities <SKILLS>` | Comma-separated skills for task matching |
| `--rate <FLOAT>` | Hourly rate for cost tracking |
| `--capacity <FLOAT>` | Maximum concurrent task capacity |
| `--trust-level <LEVEL>` | `verified`, `provisional`, or `unknown` |
| `--contact <STRING>` | Contact info (email, Matrix ID, etc.) |
| `--executor <NAME>` | Executor backend: `claude` (default), `matrix`, `email`, `shell` |

### `wg assign`

```bash
wg assign <task-id> <agent-hash>    # assign agent to task
wg assign <task-id> --clear         # remove assignment
```

### `wg evaluate`

```bash
wg evaluate run <task-id> [--evaluator-model <model>] [--dry-run]
wg evaluate record --task <id> --score <0.0-1.0> --source <tag> [--notes <text>] [--dim <dim>=<score>]...
wg evaluate show [--task <id>] [--agent <id>] [--source <glob>] [--limit <N>]
```

| Subcommand | Description |
|------------|-------------|
| `run` | Trigger LLM-based evaluation of a completed task |
| `record` | Record an evaluation from an external source (outcome metrics, peer reviews, manual scores) |
| `show` | View evaluation history with optional filters (task, agent, source, limit) |

### `wg evolve`

```bash
wg evolve run [--strategy <name>] [--budget <N>] [--model <model>] [--dry-run]
```

### `wg agency stats`

```bash
wg agency stats [--min-evals <N>] [--by-model]
```

Shows: role leaderboard, tradeoff leaderboard, synergy matrix, tag breakdown, under-explored combinations.

| Flag | Description |
|------|-------------|
| `--min-evals <N>` | Minimum evaluations to consider a pair "explored" (default: 3) |
| `--by-model` | Group stats by model, showing per-model score breakdown |

## Skill System

Skills define capabilities attached to a role. Four types of skill references:

### Name (tag-only)

Simple string label. No content, just matching and display.

```bash
wg role add "Coder" --skill rust --skill testing --outcome "Working code"
```

### File

Path to a file containing skill instructions. Supports absolute paths, relative paths, and `~` expansion.

```bash
wg role add "Coder" --skill "coding:file:///home/user/skills/rust-style.md" --outcome "Idiomatic Rust"
```

### Url

URL to fetch skill content from.

```bash
wg role add "Reviewer" --skill "review:https://example.com/checklist.md" --outcome "Review report"
```

### Inline

Skill content embedded directly.

```bash
wg role add "Writer" --skill "tone:inline:Write in a clear, technical style" --outcome "Documentation"
```

### Resolution

When a task is dispatched with an agent identity, all skill references on the role are resolved:
- `Name` → passes through as-is
- `File` → reads file content
- `Url` → fetches URL content
- `Inline` → uses content directly

Skills that fail to resolve produce a warning but don't block execution.

## Evolution

The evolution system improves agency performance by analyzing evaluation data and proposing changes. It spawns an LLM-powered "evolver agent" that reads performance summaries and proposes structured operations.

### Strategies

| Strategy | Description |
|----------|-------------|
| `mutation` | Modify a single existing role to improve weak dimensions |
| `crossover` | Combine traits from two high-performing roles into a new one |
| `gap-analysis` | Create entirely new roles/tradeoffs for unmet needs |
| `retirement` | Remove consistently poor-performing roles/tradeoffs |
| `tradeoff-tuning` | Adjust trade-offs and constraints on existing tradeoffs |
| `component-mutation` | Mutate individual components (skills, outcomes, tradeoffs) at the primitive level |
| `randomisation` | Randomly compose new roles or agents from existing primitives |
| `bizarre-ideation` | Generate novel primitives via creative/divergent prompting |
| `all` | Use all strategies as appropriate (default) |

### Operations

The evolver outputs structured JSON operations. These span three levels of the agency hierarchy:

**Legacy (role/motivation level):**

| Operation | Effect |
|-----------|--------|
| `create_role` | Creates a new role (typically from gap-analysis) |
| `modify_role` | Mutates or crosses over an existing role into a new one |
| `create_motivation` | Creates a new tradeoff/motivation |
| `modify_motivation` | Tunes an existing tradeoff into a new variant |
| `retire_role` | Retires a poor-performing role (renamed to `.yaml.retired`) |
| `retire_motivation` | Retires a poor-performing tradeoff |

**Primitive-level mutations:**

| Operation | Effect |
|-----------|--------|
| `wording_mutation` | Rewrites description/content of a component, tradeoff, or outcome |
| `component_substitution` | Swaps one component for another in a role |
| `config_add_component` | Adds a component to an existing role |
| `config_remove_component` | Removes a component from an existing role |
| `config_swap_outcome` | Changes which outcome a role targets (deferred for approval) |
| `config_swap_tradeoff` | Changes which tradeoff an agent uses |
| `random_compose_role` | Randomly assembles a role from existing components |
| `random_compose_agent` | Randomly assembles an agent from existing role + tradeoff |
| `bizarre_ideation` | Generates a novel primitive via creative divergent prompting |

**Meta-agent operations (coordinator-level):**

| Operation | Effect |
|-----------|--------|
| `meta_swap_role` | Change which role a meta-agent (assigner/evaluator/evolver) uses |
| `meta_swap_tradeoff` | Change which tradeoff a meta-agent uses |
| `meta_compose_agent` | Compose a new agent for a meta-agent slot from scratch |
| `modify_coordinator_prompt` | Modify mutable coordinator prompt files (`evolved-amendments.md`, `common-patterns.md`) |

### Safety guardrails

- The last remaining role or tradeoff cannot be retired
- Retired entities are preserved as `.yaml.retired` files, not deleted
- `--dry-run` shows the full evolver prompt without making changes
- `--budget` limits the number of operations applied

### Deferred operations

Some operations are too impactful to apply immediately. The evolver automatically defers operations that require human approval:

- **Outcome swaps** (`config_swap_outcome`) — changing a role's target outcome changes what "success" means
- **Self-mutation** — operations targeting the evolver's own role or tradeoff

Deferred operations are saved to `.workgraph/agency/deferred-ops/` and can be managed with:

```bash
wg evolve review list              # view pending deferred operations
wg evolve review approve <id>      # approve and apply a deferred operation
wg evolve review reject <id>       # reject and discard
```

### Coordinator prompt evolution

The evolver can modify the coordinator agent's behavior by writing to mutable prompt files in `.workgraph/agency/coordinator-prompt/`:

| File | Mutability | Purpose |
|------|-----------|---------|
| `base-system-prompt.md` | Immutable | Core coordinator instructions |
| `behavioral-rules.md` | Immutable | Safety and behavioral constraints |
| `evolved-amendments.md` | **Mutable** | Evolver-written rules and heuristics |
| `common-patterns.md` | **Mutable** | Evolver-written examples and patterns |

The `evolved-amendments.md` file is the primary output of coordinator prompt evolution — the evolver appends learned rules and heuristics based on evaluation data.

### Auto-evolve infrastructure

The coordinator can automatically trigger evolution cycles when sufficient evaluation data accumulates. This is opt-in:

```bash
wg config --auto-evolve true
```

When enabled, the coordinator's Phase 4.6 checks two triggers:

1. **Threshold trigger**: New evaluations since last evolution exceed `evolution_threshold` (default: 10)
2. **Reactive trigger**: Performance has dropped below `evolution_reactive_threshold`

The coordinator creates a `.evolve-*` meta-task that runs `wg evolve run` with the configured budget. A minimum interval (`evolution_interval`, default: 7200 seconds / 2 hours) prevents evolution from running too frequently.

Configuration:

```toml
[agency]
auto_evolve = false              # enable auto-evolution (default: false)
evolution_interval = 7200        # minimum seconds between cycles (default: 2h)
evolution_threshold = 10         # new evals needed to trigger (default: 10)
evolution_budget = 5             # max operations per cycle (default: 5)
```

### Evolver identity and meta-agent configuration

The evolver itself can have an agent identity. Configure meta-agents in config.toml:

```toml
[agency]
evolver_model = "opus"           # model for the evolver agent
evolver_agent = "abc123..."      # content-hash of evolver agent identity
assigner_model = "haiku"         # model for assigner agents
assigner_agent = "def456..."     # content-hash of assigner agent identity
evaluator_model = "haiku"        # model for evaluator agents
evaluator_agent = "ghi789..."    # content-hash of evaluator agent identity
retention_heuristics = "Retire roles scoring below 0.3 after 10 evaluations"
```

Or via CLI:

```bash
wg config --evolver-model opus --evolver-agent abc123
wg config --assigner-model haiku --assigner-agent def456
wg config --evaluator-model opus --evaluator-agent ghi789
wg config --retention-heuristics "Retire roles scoring below 0.3 after 10 evaluations"
```

The evolver prompt includes:
- Performance summaries for all roles and tradeoffs
- Strategy-specific skill documents from `.workgraph/agency/evolver-skills/`
- The evolver's own identity (if configured)
- References to the assigner, evaluator, and evolver agent hashes
- Retention heuristics (if configured)

### Evolver skills

Strategy-specific guidance documents live in `.workgraph/agency/evolver-skills/`:

- `role-mutation.md` — procedures for improving a single role
- `role-crossover.md` — procedures for combining two roles
- `gap-analysis.md` — procedures for identifying missing capabilities
- `retirement.md` — procedures for removing underperformers
- `motivation-tuning.md` — procedures for adjusting trade-offs
- `component-mutation.md` — procedures for mutating individual primitives
- `randomisation.md` — procedures for random composition
- `bizarre-ideation.md` — procedures for divergent creative generation

## Performance Tracking

### Evaluation flow

1. Task completes → evaluation is created (4 dimensions + overall score)
2. Evaluation saved as YAML in `.workgraph/agency/evaluations/`
3. **Agent's** performance record updated (task count, avg score, eval history)
4. **Role's** performance record updated (with tradeoff_id as `context_id`)
5. **Tradeoff's** performance record updated (with role_id as `context_id`)

### Performance records

Each entity maintains a `PerformanceRecord`:

```yaml
performance:
  task_count: 5
  avg_score: 0.82
  evaluations:
    - score: 0.85
      task_id: "implement-feature-x"
      timestamp: "2026-01-15T10:30:00Z"
      context_id: "<tradeoff_id>"  # on roles; role_id on tradeoffs
```

The `context_id` cross-references create a performance matrix: how a role performs with different tradeoffs, and vice versa. `wg agency stats` uses this to build a synergy matrix.

### Trend indicators

`wg agency stats` computes trends by comparing first and second halves of recent scores:

- **up** — second half averages >0.03 higher
- **down** — second half averages >0.03 lower
- **flat** — difference within 0.03

## Lineage

Every role, tradeoff, and agent tracks evolutionary history:

```yaml
lineage:
  parent_ids:
    - "a1b2c3d4..."   # single parent for mutation, two for crossover
  generation: 2
  created_by: "evolver-run-20260115-143022"
  created_at: "2026-01-15T14:30:22Z"
```

| Field | Description |
|-------|-------------|
| `parent_ids` | Empty for manual, single for mutation, multiple for crossover |
| `generation` | 0 for manual, incrementing for evolved |
| `created_by` | `"human"` for manual, `"evolver-{run_id}"` for evolved |
| `created_at` | Timestamp |

### Viewing lineage

```bash
wg role lineage <id>
wg tradeoff lineage <id>
wg agent lineage <id>        # shows agent + role + tradeoff ancestry
```

## Storage Layout

```
.workgraph/agency/
├── primitives/
│   ├── components/              # Skill components (atomic capabilities)
│   │   └── <sha256>.yaml
│   ├── outcomes/                # Desired outcomes
│   │   └── <sha256>.yaml
│   └── tradeoffs/               # Tradeoff definitions
│       ├── <sha256>.yaml
│       └── <sha256>.yaml.retired
├── cache/
│   ├── roles/                   # Composed roles (component_ids + outcome_id)
│   │   ├── <sha256>.yaml
│   │   └── <sha256>.yaml.retired
│   └── agents/                  # Agent definitions (role + tradeoff pairs)
│       └── <sha256>.yaml
├── evaluations/
│   ├── eval-<task-id>-<timestamp>.json   # Standard evaluations (source: "llm")
│   └── flip-<task-id>-<timestamp>.json   # FLIP evaluations (source: "flip")
├── evolver-skills/              # Strategy-specific guidance documents
│   ├── role-mutation.md
│   ├── role-crossover.md
│   ├── gap-analysis.md
│   ├── retirement.md
│   ├── motivation-tuning.md
│   ├── component-mutation.md
│   ├── randomisation.md
│   └── bizarre-ideation.md
├── coordinator-prompt/          # Coordinator prompt files
│   ├── base-system-prompt.md    # (immutable)
│   ├── behavioral-rules.md      # (immutable)
│   ├── evolved-amendments.md    # (mutable — evolver-written rules)
│   └── common-patterns.md       # (mutable — evolver-written examples)
└── deferred-ops/                # Deferred evolution operations awaiting approval
    └── <op-id>.json
```

Roles, tradeoffs, and agents are stored as YAML. Evaluations are stored as JSON. All filenames are based on the entity's content-hash ID.

## Federation

Federation lets you share agency entities (roles, tradeoffs, agents) and their performance data across workgraph projects. Because entities use content-hash IDs, the same role in two projects has the same ID — pull/push merges performance records automatically.

### Remotes

Named references to other agency stores:

```bash
wg agency remote add <name> <path>       # add a named remote
wg agency remote list                     # list all configured remotes
wg agency remote show <name>             # show remote details and entity counts
wg agency remote remove <name>           # remove a named remote
```

### Discovering stores

Scan a directory tree for workgraph agency stores:

```bash
wg agency scan <root-dir>                 # find all .workgraph/agency/ stores
wg agency scan <root-dir> --max-depth 5   # limit recursion depth (default: 10)
```

### Pull, push, and merge

```bash
# Pull entities from another store into local
wg agency pull <source>                              # pull all from path or named remote
wg agency pull <source> --type role                  # only roles
wg agency pull <source> --entity <id-prefix>         # specific entities
wg agency pull <source> --dry-run                    # preview without writing
wg agency pull <source> --no-performance             # definitions only, skip scores
wg agency pull <source> --no-evaluations             # skip evaluation JSON files
wg agency pull <source> --global                     # pull into ~/.workgraph/agency/

# Push local entities to another store
wg agency push <target>                              # push all to path or named remote
wg agency push <target> --type tradeoff            # only tradeoffs
wg agency push <target> --entity <id-prefix>         # specific entities
wg agency push <target> --dry-run                    # preview without writing
wg agency push <target> --global                     # push from ~/.workgraph/agency/

# Merge multiple stores
wg agency merge <source1> <source2> ...              # merge into local project
wg agency merge <source1> <source2> --into <path>    # merge into specific target
wg agency merge <source1> <source2> --dry-run        # preview
```

For the full federation design (conflict resolution, global store, trust propagation), see `docs/design/agency-federation.md`.

## Configuration Reference

```toml
[agency]
auto_evaluate = false              # auto-create evaluation tasks on completion
auto_assign = false                # auto-create assignment tasks for ready work
auto_triage = false                # auto-triage dead agents before respawning
assigner_model = "haiku"           # model for assigner agents
evaluator_model = "haiku"          # model for evaluator agents
evolver_model = "opus"             # model for evolver agents
creator_model = ""                 # model for agent-creator meta-tasks
triage_model = "haiku"             # model for triage (default: haiku)
assigner_agent = ""                # content-hash of assigner agent
evaluator_agent = ""               # content-hash of evaluator agent
evolver_agent = ""                 # content-hash of evolver agent
creator_agent = ""                 # content-hash of agent-creator agent
retention_heuristics = ""          # prose policy for retirement decisions
triage_timeout = 30                # timeout in seconds for triage calls
triage_max_log_bytes = 50000       # max bytes of agent log to read for triage

# FLIP settings
flip_enabled = false               # enable FLIP fidelity evaluation (default: false)
flip_verification_threshold = 0.7  # FLIP score below this triggers Opus verification

# Auto-evolve settings
auto_evolve = false                # enable automatic evolution cycles
evolution_interval = 7200          # minimum seconds between cycles (default: 2h)
evolution_threshold = 10           # new evals needed to trigger (default: 10)
evolution_budget = 5               # max operations per auto-evolve cycle

# Per-role model routing (alternative to legacy model fields above)
[models.flip_inference]
model = "sonnet"                   # model for FLIP inference phase

[models.flip_comparison]
model = "haiku"                    # model for FLIP comparison phase

[models.verification]
model = "opus"                     # model for FLIP-triggered verification
```

```bash
# CLI equivalents
wg config --auto-evaluate true
wg config --auto-assign true
wg config --auto-triage true
wg config --assigner-model haiku
wg config --evaluator-model opus
wg config --evolver-model opus
wg config --creator-model haiku
wg config --triage-model haiku
wg config --assigner-agent abc123
wg config --evaluator-agent def456
wg config --evolver-agent ghi789
wg config --creator-agent abc123
wg config --retention-heuristics "Retire roles scoring below 0.3 after 10 evaluations"
wg config --triage-timeout 30
wg config --triage-max-log-bytes 50000
wg config --flip-enabled true
wg config --auto-evolve true
```
