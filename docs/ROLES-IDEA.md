# Proposal: declarative role definitions for Workgraph


## Problem


Workgraph's current capability system uses simple string tags (`-c coding -c testing`). This is insufficient for:


- Describing complex agent behaviours and constraints
- Encoding domain-specific knowledge into role definitions
- Adapting roles based on performance feedback
- Managing context budget across varied role complexity


## Proposed extension


### 1. Role definitions as markdown files


Roles are defined in markdown files stored in a designated directory (e.g., `.workgraph/roles/`). Each file contains:


- **Objectives**: What the role aims to accomplish
- Skills used: pointers to skills to be loaded before beginning task
- **Constraints**: Boundaries on behaviour, tone, output format
- **Domain knowledge**: Context the agent needs to perform the role
- **Examples**: Reference outputs demonstrating expected quality


```
.workgraph/
  roles/
    technical-writer.md
    code-reviewer.md
    research-analyst.md
```


Agents read the relevant role definition before executing tasks assigned to that role. The definition functions as a detailed prompt prefix.


### 2. Role weight


Each role has a weight: the token cost of instantiating it.


- **Auto-calculated**: Parse the markdown, count tokens using the target model's tokeniser
- **Manual override**: Optional metadata field that supersedes calculated value
- **Staleness detection**: Flag when override differs significantly from calculated value after role edits


Weight affects:
- Model selection (heavier roles require larger context windows)
- Task assignment (context-limited agents cannot take heavy roles)
- Cost estimation


Metadata block at the top of each role file:


```yaml
---
name: technical-writer
weight_override: null
min_context: 8000
---
```


### 3. Task-to-role assignment


Tasks declare which role(s) should execute them:


```
wg add "Write API documentation" --role technical-writer
wg add "Review authentication module" --role code-reviewer --role security-specialist
```


The matching algorithm:
1. Filter agents that have the required role(s) loaded or can load them within context budget
2. Score by fit (existing Workgraph logic) plus role alignment
3. Assign or queue for claim


### 4. Evaluation criteria


Success conditions are defined by the project owner, not hardcoded.


Evaluation criteria are declared per project or per task:


```yaml
# .workgraph/evaluation/default.md
## Success criteria
- Output passes automated tests (if applicable)
- Output approved by human reviewer (if task is verified)
- Downstream tasks unblocked without rework


## Failure indicators
- Task requires more than one revision cycle
- Output rejected by reviewer
- Downstream tasks blocked by quality issues
```


Tasks can override the default:


```
wg add "Write user guide" --role technical-writer --eval-criteria ./evaluation/documentation.md
```


### 5. Autonomous role modification


When a task completes, the system:


1. Evaluates the output against the declared criteria
2. Records the outcome (success, partial success, failure) with context
3. Triggers the modification skill if the outcome indicates the role definition should change


Modifications are appended to a changelog within the role file, then integrated into the main definition. This preserves history and allows rollback.


### 6. Modification logic as a skill


The rules governing how roles are modified are themselves encoded in a markdown skill file:


```
.workgraph/skills/role-modifier.md
```


This skill defines:
- **When to modify**: Threshold of failures, pattern of partial successes, explicit human trigger
- **What to modify**: Which sections of the role definition to adjust
- **How to modify**: Append examples of failure modes, tighten constraints, add clarifying context
- **Limits**: Maximum modification frequency, required human approval for structural changes


Because the modification logic is itself a skill, it can be:
- Customised per project
- Versioned and rolled back
- Evaluated and modified (meta-level recursion)


## Integration with existing Workgraph


| Existing feature | Extension |
|------------------|-----------|
| `-c capability` tags | Replaced or supplemented by `--role` with rich definitions |
| `wg actor add` | Actors declare which roles they can assume and their context limits |
| `wg match` | Matching algorithm incorporates role weight and context budget |
| Skill files | Role definitions and modification logic follow the same pattern |


## File structure


```
.workgraph/
  roles/
    technical-writer.md
    code-reviewer.md
    research-analyst.md
  evaluation/
    default.md
    documentation.md
    code-quality.md
  skills/
    role-modifier.md
  config.toml
```


## Open questions


1. **Role composition**: Can an agent assume multiple roles simultaneously? If so, how are weights combined and conflicts resolved?
2. **Role versioning**: Should role definitions be versioned explicitly, or rely on git history?
3. **Modification approval**: For high-stakes roles, should modifications require human approval despite the autonomous default?
4. **Token counting**: Which tokeniser to use when agents may run on different model families?


## Next steps


1. Define the schema for role metadata
2. Write the initial role-modifier skill
3. Implement weight calculation and context budget checking
4. Extend `wg add` and `wg match` to support `--role`
5. Build evaluation tracking and modification triggers

