# Workgraph

This project uses `wg` (workgraph) to coordinate work. The binary is at `./target/debug/workgraph`.

## Quick Reference

```bash
wg ready          # What can I work on?
wg list           # All tasks
wg add "title"    # Add task
wg done <id>      # Complete task
wg check          # Verify graph health
```

## Agent Protocol

1. Check `wg ready` before starting work
2. Claim tasks: `wg claim <id> --actor agent-N`
3. When done: `wg done <id>`
4. If you discover new work, add it: `wg add "..." --blocked-by X`

## Analysis Commands

```bash
wg why-blocked <id>   # Why is this stuck?
wg impact <id>        # What depends on this?
wg bottlenecks        # What's blocking the most work?
wg structure          # Entry points, dead ends
wg aging              # How old are tasks?
wg velocity           # Completion rate
wg forecast           # When will we finish?
```

All commands support `--json` for scripting.
