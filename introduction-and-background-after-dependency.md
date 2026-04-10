# Introduction and Background: Enforcing --after Dependencies in Coordinator Prompts

## 1. Introduction

In modern task coordination systems, the proper sequencing of tasks is critical to prevent race conditions, file conflicts, and execution order issues. The workgraph system utilizes `--after` dependencies to explicitly define the execution order between tasks, ensuring that tasks which modify shared resources are executed in a safe and predictable sequence.

However, current coordinator prompts often create tasks without proper dependency wiring, leading to potential conflicts and system inefficiencies. This document outlines the design for enforcing proper `--after` dependencies in coordinator prompts to ensure reliable and predictable task execution.

## 2. Background

### 2.1 The `--after` Dependency Mechanism

The `--after` dependency mechanism in workgraph provides explicit control over task execution order. When a task B is created with `--after task-a`, it ensures that task B will not execute until task A has successfully completed. This is particularly important when multiple tasks may modify the same files or shared resources.

### 2.2 Current Challenges

Coordinator agents are responsible for creating and orchestrating tasks based on user requests and system needs. However, the complexity of analyzing which tasks might conflict with each other and the dynamic nature of task creation make it challenging for coordinators to automatically wire appropriate `--after` dependencies.

Common scenarios that lead to missing dependencies include:
- Sequential tasks that modify the same file not being properly sequenced
- Parallel tasks that require integration not having proper merge points
- Task decomposition that doesn't consider potential resource conflicts

### 2.3 Impact of Missing Dependencies

Without proper `--after` dependencies:
- Race conditions can occur when multiple tasks attempt to modify the same file simultaneously
- File conflicts may result in corrupted or incomplete work
- Execution order issues can lead to tasks running before their prerequisites are complete
- System reliability and predictability are compromised

## 3. Goal

The goal of this design is to ensure that coordinator prompts properly enforce `--after` dependencies by:
1. Providing clear guidance to coordinators on when and how to use `--after` dependencies
2. Implementing mechanisms to detect potential conflicts and suggest appropriate dependencies
3. Creating patterns and examples for common sequential task workflows
4. Reducing file conflict errors and improving overall system reliability
