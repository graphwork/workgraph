# Design Document: Enforcing --after Dependencies in Coordinator Prompts

## 1. Introduction and Background

In modern task coordination systems, the proper sequencing of tasks is critical to prevent race conditions, file conflicts, and execution order issues. The workgraph system utilizes `--after` dependencies to explicitly define the execution order between tasks, ensuring that tasks which modify shared resources are executed in a safe and predictable sequence.

However, current coordinator prompts often create tasks without proper dependency wiring, leading to potential conflicts and system inefficiencies. This document outlines the design for enforcing proper `--after` dependencies in coordinator prompts to ensure reliable and predictable task execution.

### 1.1 The `--after` Dependency Mechanism

The `--after` dependency mechanism in workgraph provides explicit control over task execution order. When a task B is created with `--after task-a`, it ensures that task B will not execute until task A has successfully completed. This is particularly important when multiple tasks may modify the same files or shared resources.

### 1.2 Current Challenges

Coordinator agents are responsible for creating and orchestrating tasks based on user requests and system needs. However, the complexity of analyzing which tasks might conflict with each other and the dynamic nature of task creation make it challenging for coordinators to automatically wire appropriate `--after` dependencies.

Common scenarios that lead to missing dependencies include:
- Sequential tasks that modify the same file not being properly sequenced
- Parallel tasks that require integration not having proper merge points
- Task decomposition that doesn't consider potential resource conflicts

### 1.3 Impact of Missing Dependencies

Without proper `--after` dependencies:
- Race conditions can occur when multiple tasks attempt to modify the same file simultaneously
- File conflicts may result in corrupted or incomplete work
- Execution order issues can lead to tasks running before their prerequisites are complete
- System reliability and predictability are compromised

### 1.4 Goal

The goal of this design is to ensure that coordinator prompts properly enforce `--after` dependencies by:
1. Providing clear guidance to coordinators on when and how to use `--after` dependencies
2. Implementing mechanisms to detect potential conflicts and suggest appropriate dependencies
3. Creating patterns and examples for common sequential task workflows
4. Reducing file conflict errors and improving overall system reliability

## 2. Current State Analysis
- How coordinators currently handle task dependencies
- Common patterns that lead to missing dependencies
- Examples of conflicts that occur without proper dependency enforcement

## 3. Requirements Specification
- All tasks modifying the same files must be sequenced properly
- Parallel tasks must have integration tasks with proper dependencies
- Coordinator prompts must automatically suggest --after dependencies when appropriate

## 4. Solution Approaches
### 4.1 Prompt Enhancement
- Update coordinator system prompts with dependency guidance
- Include examples of proper --after usage patterns

### 4.2 Pattern Recognition
- Identify common sequential task patterns
- Automatically suggest --after dependencies based on file conflicts

### 4.3 File Conflict Detection
- Track which files each task modifies
- Detect potential conflicts between concurrent tasks

## 5. Implementation Plan
### 5.1 Phase 1: Prompt Enhancement
- Update coordinator system prompts
- Add dependency guidance and examples

### 5.2 Phase 2: Pattern Recognition
- Implement pattern matching for sequential task creation
- Add file conflict detection logic

### 5.3 Phase 3: Validation and Feedback
- Validate suggested dependencies
- Implement user feedback mechanisms

## 6. Success Metrics
- Reduction in file conflict errors
- Percentage of related tasks properly wired with --after dependencies
- User satisfaction metrics

## 7. Testing Strategy
- Unit tests for pattern recognition algorithms
- Integration tests for dependency suggestion system
- End-to-end tests for coordinator prompt scenarios

## 8. Rollout Plan
- Gradual deployment to test environments
- Monitoring and feedback collection
- Iterative improvements based on usage data
