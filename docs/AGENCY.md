# Agency System

The agency system gives workgraph agents composable identities. Instead of every agent being a generic assistant, you define **roles** (what an agent does), **motivations** (why it acts that way), and pair them into **agents** that are assigned to tasks, evaluated, and evolved over time.

Agents can be **human or AI**. The difference is the executor: AI agents use `claude` (or similar), human agents use `matrix`, `email`, or `shell`. Both share the same identity model — roles, motivations, capabilities, trust levels, and performance tracking all work uniformly regardless of who (or what) is doing the work.

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

### Motivation

A motivation defines **why** an agent acts the way it does.

| Field | Description | Identity-defining? |
|-------|-------------|--------------------|
| `name` | Human-readable label (e.g. "Careful") | No |
| `description` | What this motivation prioritizes | Yes |
| `acceptable_tradeoffs` | Compromises the agent may make | Yes |
| `unacceptable_tradeoffs` | Hard constraints the agent must never violate | Yes |
| `performance` | Aggregated evaluation scores | No (mutable) |
| `lineage` | Evolutionary history | No (mutable) |

### Agent

An agent is the **unified identity** in workgraph — it can represent a human or an AI. For AI agents, it is a named pairing of a role and a motivation. For human agents, role and motivation are optional.

| Field | Description |
|-------|-------------|
| `name` | Human-readable label |
| `role_id` | Content-hash ID of the role (required for AI, optional for human) |
| `motivation_id` | Content-hash ID of the motivation (required for AI, optional for human) |
| `capabilities` | Skills/capabilities for task matching (e.g., `rust`, `testing`) |
| `rate` | Hourly rate for cost forecasting |
| `capacity` | Maximum concurrent task capacity |
| `trust_level` | `Verified`, `Provisional` (default), or `Unknown` |
| `contact` | Contact info — email, Matrix ID, etc. (primarily for human agents) |
| `executor` | How this agent receives work: `claude` (default), `matrix`, `email`, `shell` |
| `performance` | Agent-level aggregated evaluation scores |
| `lineage` | Evolutionary history |

The same role paired with different motivations produces different agents. A "Programmer" role with a "Careful" motivation produces a different agent than with a "Fast" motivation.

Human agents are distinguished by their executor. Agents with a human executor (`matrix`, `email`, `shell`) don't need a role or motivation — they're real people who bring their own judgment. AI agents (executor `claude`) require both, because the role and motivation are injected into the prompt to shape behavior.

## Content-Hash IDs

Every role, motivation, and agent is identified by a **SHA-256 content hash** of its identity-defining fields.

- **Deterministic**: Same content → same ID
- **Deduplication**: Can't create two entities with identical content
- **Immutable identity**: Changing an identity-defining field produces a *new* entity. The old one stays.

| Entity | Hashed fields |
|--------|---------------|
| Role | `skills` + `desired_outcome` + `description` |
| Motivation | `acceptable_tradeoffs` + `unacceptable_tradeoffs` + `description` |
| Agent | `role_id` + `motivation_id` |

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
wg evaluate my-task

# 4. Evolve
wg evolve
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
wg evolve
```

## Lifecycle

### 1. Create roles and motivations

```bash
# Create a role
wg role add "Programmer" \
  --outcome "Working, tested code" \
  --skill code-writing \
  --skill testing \
  --description "Writes, tests, and debugs code"

# Create a motivation
wg motivation add "Careful" \
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

This creates four starter roles (Programmer, Reviewer, Documenter, Architect) and four starter motivations (Careful, Fast, Thorough, Balanced).

### 2. Pair into agents

```bash
# AI agent (role + motivation required)
wg agent create "Careful Programmer" --role <role-hash> --motivation <motivation-hash>

# AI agent with operational fields
wg agent create "Careful Programmer" \
  --role <role-hash> \
  --motivation <motivation-hash> \
  --capabilities rust,testing \
  --rate 50.0

# Human agent (role + motivation optional)
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

When the service spawns that task, the agent's role and motivation are rendered into the prompt as an identity section:

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
wg evaluate <task-id>
wg evaluate <task-id> --evaluator-model opus
wg evaluate <task-id> --dry-run    # preview the evaluator prompt
```

The evaluator scores across four dimensions:

| Dimension | Weight | Description |
|-----------|--------|-------------|
| `correctness` | 40% | Does the output match the desired outcome? |
| `completeness` | 30% | Were all aspects of the task addressed? |
| `efficiency` | 15% | Was work done without unnecessary steps? |
| `style_adherence` | 15% | Were project conventions and motivation constraints followed? |

The evaluator receives:
- The task definition (title, description, deliverables)
- The agent's identity (role + motivation)
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
2. The **role's** performance record (with `motivation_id` as context)
3. The **motivation's** performance record (with `role_id` as context)

### 5. Evolve

Use performance data to improve the agency:

```bash
wg evolve                                     # full cycle, all strategies
wg evolve --strategy mutation --budget 3      # targeted changes
wg evolve --model opus                        # use specific model
wg evolve --dry-run                           # preview without applying
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

### `wg motivation`

Also aliased as `wg mot`.

| Command | Description |
|---------|-------------|
| `wg motivation add <name> --accept <text> --reject <text> [-d <text>]` | Create a new motivation |
| `wg motivation list` | List all motivations |
| `wg motivation show <id>` | Show details |
| `wg motivation edit <id>` | Edit in `$EDITOR` (re-hashes on save) |
| `wg motivation rm <id>` | Delete a motivation |
| `wg motivation lineage <id>` | Show evolutionary ancestry |

### `wg agent`

| Command | Description |
|---------|-------------|
| `wg agent create <name> [OPTIONS]` | Create an agent (see options below) |
| `wg agent list` | List all agents |
| `wg agent show <id>` | Show details with resolved role/motivation |
| `wg agent rm <id>` | Remove an agent |
| `wg agent lineage <id>` | Show agent + role + motivation ancestry |
| `wg agent performance <id>` | Show evaluation history |

**`wg agent create` options:**

| Option | Description |
|--------|-------------|
| `--role <ID>` | Role ID or prefix (required for AI agents) |
| `--motivation <ID>` | Motivation ID or prefix (required for AI agents) |
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
wg evaluate <task-id> [--evaluator-model <model>] [--dry-run]
```

### `wg evolve`

```bash
wg evolve [--strategy <name>] [--budget <N>] [--model <model>] [--dry-run]
```

### `wg agency stats`

```bash
wg agency stats [--min-evals <N>]
```

Shows: role leaderboard, motivation leaderboard, synergy matrix, tag breakdown, under-explored combinations.

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
| `gap-analysis` | Create entirely new roles/motivations for unmet needs |
| `retirement` | Remove consistently poor-performing roles/motivations |
| `motivation-tuning` | Adjust trade-offs and constraints on existing motivations |
| `all` | Use all strategies as appropriate (default) |

### Operations

The evolver outputs structured JSON operations:

| Operation | Effect |
|-----------|--------|
| `create_role` | Creates a new role (typically from gap-analysis) |
| `modify_role` | Mutates or crosses over an existing role into a new one |
| `create_motivation` | Creates a new motivation |
| `modify_motivation` | Tunes an existing motivation into a new variant |
| `retire_role` | Retires a poor-performing role (renamed to `.yaml.retired`) |
| `retire_motivation` | Retires a poor-performing motivation |

### Safety guardrails

- The last remaining role or motivation cannot be retired
- Retired entities are preserved as `.yaml.retired` files, not deleted
- `--dry-run` shows the full evolver prompt without making changes
- `--budget` limits the number of operations applied

### Evolver identity and meta-agent configuration

The evolver itself can have an agent identity. Configure meta-agents in config.toml:

```toml
[agency]
evolver_model = "opus"           # model for the evolver agent
evolver_agent = "abc123..."      # content-hash of evolver agent identity
assigner_model = "haiku"         # model for assigner agents
assigner_agent = "def456..."     # content-hash of assigner agent identity
evaluator_model = "opus"         # model for evaluator agents
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
- Performance summaries for all roles and motivations
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

## Performance Tracking

### Evaluation flow

1. Task completes → evaluation is created (4 dimensions + overall score)
2. Evaluation saved as YAML in `.workgraph/agency/evaluations/`
3. **Agent's** performance record updated (task count, avg score, eval history)
4. **Role's** performance record updated (with motivation_id as `context_id`)
5. **Motivation's** performance record updated (with role_id as `context_id`)

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
      context_id: "<motivation_id>"  # on roles; role_id on motivations
```

The `context_id` cross-references create a performance matrix: how a role performs with different motivations, and vice versa. `wg agency stats` uses this to build a synergy matrix.

### Trend indicators

`wg agency stats` computes trends by comparing first and second halves of recent scores:

- **up** — second half averages >0.03 higher
- **down** — second half averages >0.03 lower
- **flat** — difference within 0.03

## Lineage

Every role, motivation, and agent tracks evolutionary history:

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
wg motivation lineage <id>
wg agent lineage <id>        # shows agent + role + motivation ancestry
```

## Storage Layout

```
.workgraph/agency/
├── roles/
│   ├── <sha256>.yaml            # Role definitions
│   └── <sha256>.yaml.retired    # Retired roles
├── motivations/
│   ├── <sha256>.yaml            # Motivation definitions
│   └── <sha256>.yaml.retired    # Retired motivations
├── agents/
│   └── <sha256>.yaml            # Agent definitions (role+motivation pairs)
├── evaluations/
│   └── eval-<task-id>-<timestamp>.yaml  # Evaluation records
└── evolver-skills/
    ├── role-mutation.md
    ├── role-crossover.md
    ├── gap-analysis.md
    ├── retirement.md
    └── motivation-tuning.md
```

Roles, motivations, and agents are stored as YAML. Evaluations are stored as YAML. All filenames are based on the entity's content-hash ID.

## Configuration Reference

```toml
[agency]
auto_evaluate = false              # auto-create evaluation tasks on completion
auto_assign = false                # auto-create assignment tasks for ready work
assigner_model = "haiku"           # model for assigner agents
evaluator_model = "opus"           # model for evaluator agents
evolver_model = "opus"             # model for evolver agents
assigner_agent = ""                # content-hash of assigner agent
evaluator_agent = ""               # content-hash of evaluator agent
evolver_agent = ""                 # content-hash of evolver agent
retention_heuristics = ""          # prose policy for retirement decisions
```

```bash
# CLI equivalents
wg config --auto-evaluate true
wg config --auto-assign true
wg config --assigner-model haiku
wg config --evaluator-model opus
wg config --evolver-model opus
wg config --assigner-agent abc123
wg config --evaluator-agent def456
wg config --evolver-agent ghi789
wg config --retention-heuristics "Retire roles scoring below 0.3 after 10 evaluations"
```
