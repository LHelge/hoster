# Bears CLI Fallback Reference

Use these commands when the Bears MCP tools (`mcp__bears__*`) are not available. Always pass `--json` for structured output.

## Task Creation

```bash
# Create a task
bea create "Task title" --priority P1 --json

# Create an epic
bea create "Epic title" --epic --priority P1 --json

# Create with parent and tags
bea create "Sub-task" --priority P1 --parent <epic-id> --tag backend,api --json

# Create with dependencies
bea create "Next step" --priority P1 --depends-on <id1>,<id2> --json

# Create with body
bea create "Task" --priority P1 --body "Description here" --json
```

## Dependencies

```bash
# Add a dependency (task <id> depends on <dep-id>)
bea dep add <id> <dep-id> --json

# Remove a dependency
bea dep rm <id> <dep-id> --json
```

## Querying

```bash
# List all open tasks
bea list --json

# List with filters
bea list --status open --priority P1 --tag backend --json

# List tasks under an epic
bea list --epic <epic-id> --json

# Show ready tasks (open, all deps done)
bea ready --json
bea ready --epic <epic-id> --limit 5 --json

# List epics with progress
bea epics --json

# Show a single task
bea show <id> --json

# Search tasks
bea search "query" --json

# Show dependency graph
bea graph --json
```

## Status Changes

```bash
# Start a task (set in_progress), optionally claiming it
bea start <id> --json
bea start <id> --assignee <name> --json

# Release an in-progress task back to the pool (open, assignee cleared)
bea release <id> --json

# Complete a task (set done)
bea done <id> --json

# Submit finished work for review, or list the review queue
bea review <id> --json
bea review --json

# Send a task under review back for changes
bea reject <id> --json

# Propose work without adding it to the backlog, and accept a proposal
bea propose <id> --json
bea accept <id> --json

# Cancel a task
bea cancel <id> --json
```

## Updates

```bash
# Update priority
bea update <id> --priority P0 --json

# Update tags
bea update <id> --tag tag1,tag2 --json

# Update body
bea update <id> --body "New description" --json

# Update status
bea update <id> --status blocked --json
```

## Deletion

```bash
# Delete a task permanently
bea delete <id> --json

# Prune cancelled tasks
bea prune --json

# Prune cancelled and done tasks
bea prune --done --json
```
