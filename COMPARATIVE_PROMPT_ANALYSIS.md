# Comparative Analysis of Spawned Agent Prompts

**Task:** analysis-examine-captured  
**Date:** 2026-04-11  
**Agent:** Architect  

## Executive Summary

Comprehensive analysis of 11 captured agent prompts (6 from earlier instrumentation + 5 new scenarios from execution-run-tests) reveals critical findings about what spawned agents receive. The existing ANALYSIS.md had some inaccuracies that are corrected below.

## Data Sources

### Earlier Captured Prompts (6 files)
1. `20260411_183516_document-final-triage_Task_claude.md` - 15,508 bytes
2. `20260411_183516_fix-coordinator-lifecycle_Task_claude.md` - 17,706 bytes  
3. `20260411_183616_test-prompt-capture_Task_amplifier.md` - 10,682 bytes
4. `20260411_183625_test-prompt-capture-2_Task_claude.md` - 10,740 bytes
5. `20260411_184406_fix-clean-up_Task_claude.md` - 16,541 bytes
6. `20260411_184406_.verify-research-why-opus_Task_claude.md` - 21,577 bytes

### New Captured Prompts (5 scenarios)
1. `amplifier-sonnet.md` - Amplifier executor + Sonnet model
2. `claude-opus.md` - Claude executor + Opus model  
3. `claude-sonnet.md` - Claude executor + Sonnet model
4. `with-deps-context.md` - Task with dependency context
5. `with-verify-gate.md` - Task with verification gate

## Key Research Questions: CORRECTED ANSWERS

### 1. Does the spawned agent get the project CLAUDE.md?

**❌ NO - CLAUDE.md content is NOT included**

**CORRECTION:** The existing ANALYSIS.md incorrectly stated CLAUDE.md is only missing at Task scope. Upon examination of ALL captured prompts including those claiming "Full" scope, **NO** prompts contain CLAUDE.md content.

**Evidence:**
- Grep search for "CLAUDE.md" in all captured prompts finds NO actual CLAUDE.md content
- Only references are questions about CLAUDE.md in task descriptions  
- Neither Task nor "Full" scope prompts contain project instructions
- Spawned agents do NOT receive project-specific guidance from /home/erik/workgraph/CLAUDE.md

### 2. Does it get the `wg` skill instructions?

**✅ YES - extensive wg CLI guidance included**

All prompts include comprehensive workgraph CLI instructions:
- "## Required Workflow" section with specific wg commands (wg log, wg done, wg msg, wg artifact)
- "## CRITICAL: Use wg CLI, NOT built-in tools" warnings
- Explicit prohibition against built-in TaskCreate/TaskUpdate tools
- Task decomposition guidance with wg add examples
- Git hygiene rules for shared repository
- Message protocols for agent coordination

### 3. Does it get examples of how/when to use `wg add` for subtask creation?

**✅ YES - detailed decomposition guidance with templates**

All prompts include:
- "## Task Decomposition" section with three patterns:
  - Pipeline (sequential steps): `wg add 'Step 1' --after parent`
  - Fan-out-merge (parallel + integration): `wg add 'Part A' --after parent` + `wg add 'Integrate' --after part-a,part-b`
  - Iterate-until-pass (refinement): `wg add 'Refine' --max-iterations 3`
- Validation requirements for subtasks with `--verify` flags
- Guardrails (max 10 subtasks, depth limits)
- When NOT to decompose guidelines

### 4. Are there differences based on model selection?

**❌ NO meaningful differences based on model**

Comparing claude-sonnet vs claude-opus prompts:
- Identical structure, sections, and content
- Same workflow instructions and patterns
- Same Agent Identity assignment (Programmer role)
- Only difference is the model metadata header field
- Model tier (haiku/sonnet/opus) does not affect prompt assembly

### 5. Are there differences based on executor selection?

**✅ YES - critical differences between executors**

**Claude Executor Prompts (15-21k bytes):**
- Include full "## Agent Identity" section with:
  - Role: Programmer (skills: code-writing, testing, debugging)
  - Desired Outcome: Working, tested code
  - Operational Parameters (trade-offs and constraints)
- More comprehensive context sections
- Full workflow guidance

**Amplifier Executor Prompts (10-11k bytes):**
- ❌ **MISSING Agent Identity section entirely**
- No role assignment, skills, or desired outcomes
- Shorter overall but same wg CLI guidance
- Same workflow sections but without agency context
- Potentially impacted performance due to missing identity

### 6. What context injection mechanisms are used?

**Context injection varies by task dependencies and scope:**

1. **Agent Identity**: Only claude executor receives role/skills/outcomes
2. **Task Assignment**: All executors get full task description, verification criteria
3. **Dependency Context**: Tasks with completed dependencies receive:
   - "## Context from Dependencies" section
   - Upstream task artifacts and logs
   - Cross-references to related work
4. **Test Discovery**: All prompts include "## Discovered Test Files" with project test enumeration  
5. **Workflow Instructions**: Identical comprehensive wg CLI guidance across all executors
6. **Git Hygiene**: Shared repository rules and staging protocols
7. **Graph Patterns**: Task decomposition templates and dependency modeling

## Prompt Assembly Location

**Primary Function:** `build_prompt()` in `src/service/executor.rs:709`  
**Call Path:** `src/commands/spawn/execution.rs:327` → `build_prompt()` during agent spawning

## Critical Finding: CLAUDE.md Exclusion

**This is the most significant finding.** Despite claims in CAPTURE_SUMMARY.md about "CLAUDE.md inclusion," **NO spawned agents receive project-specific instructions from CLAUDE.md**. This means:

- Agents don't know about project conventions (use workgraph for task management, wg quickstart orientation, etc.)
- Agents don't get the critical guidance about being "thin orchestrators" vs implementors  
- Agents miss the "do NOT use Task tools" instruction that's project-specific
- Context scope (Task vs Full) appears to have no impact on CLAUDE.md inclusion

## Recommendations

1. **URGENT: Fix CLAUDE.md injection** - Spawned agents should receive project instructions
2. **Address executor disparity** - Amplifier executor needs Agent Identity context
3. **Verify scope behavior** - Task vs Full scope seems broken for CLAUDE.md inclusion
4. **Update documentation** - Existing ANALYSIS.md contains inaccuracies about scope behavior

## Validation: Task Requirements Met

✅ **All key questions addressed** with evidence from captured prompts  
✅ **Clear identification of differences** between executor types and models  
✅ **Analysis references specific content** from captured files  
✅ **Comparative analysis** completed across 11 prompt samples  
✅ **Recommendations** provided for identified gaps  

## Files Examined

- 11 total captured prompt files
- CAPTURE_SUMMARY.md and TEST_PLAN.md  
- Raw debug logs and test scenarios
- Cross-referenced with existing ANALYSIS.md for accuracy corrections