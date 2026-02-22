# Task ID Namespacing

## Problem

Task IDs in a workgraph are global. When multiple tools or specs add tasks to the same graph, ID collisions are likely — common names like `build`, `test`, `deploy` will clash.

This is especially relevant when:
- A compiler (like Attracty) generates tasks from a spec into an existing workgraph
- Multiple specs or subgraphs are composed into one graph
- Agents add tasks with generic names

## Proposal: Slash-Delimited Namespaces

Use `/` as a namespace separator in task IDs:

```
deploy-pipeline/build
deploy-pipeline/test
deploy-pipeline/deploy
my-other-spec/build       # no collision
```

This is natural (filesystem-like), readable, and avoids ambiguity.

### Benefits

- **Collision avoidance** — different specs/subgraphs coexist cleanly
- **Grouping** — `wg list` could filter by prefix (`wg list deploy-pipeline/*`)
- **Hierarchy** — supports nesting if needed (`org/project/phase`)
- **Convention, not enforcement** — flat IDs still work; namespacing is opt-in

### Considerations

- `wg` CLI commands that accept task IDs should handle `/` in IDs without issues (shell quoting, path confusion)
- Tab completion and display formatting should account for longer IDs
- A `--namespace` or `--prefix` flag on `wg add` could make this ergonomic:
  ```
  wg add "build" --namespace deploy-pipeline
  # creates task: deploy-pipeline/build
  ```

## Agent Guidance

Agents working in shared workgraphs should be aware that task ID collisions can occur. When generating tasks programmatically or from specs, always namespace task IDs to avoid stomping on existing tasks.
