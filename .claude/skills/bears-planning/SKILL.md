---
name: bears-planning
description: "Plan work using Bears task tracker. Use when: breaking down features into epics and sub-tasks, creating implementation plans, organizing work with dependencies and priorities, scoping new functionality, decomposing user stories into trackable tasks."
---

# Bears Planning

Create structured plans using the Bears task tracker — epics with ordered, dependency-linked sub-tasks stored as Markdown files in `.bears/`.

## When to Use

- Breaking a feature request into implementable tasks
- Creating an epic with ordered sub-tasks
- Organizing work with priorities and dependency chains
- Scoping a refactor, bug fix, or new module

## Tool Selection

**Prefer Bears MCP tools** (`mcp__bears__*`) when available. They are structured and avoid shell parsing issues.

If MCP tools are not loaded or fail, **fall back to the `bea` CLI** with `--json` for structured output. See [CLI reference](./references/cli-fallback.md) for all commands.

### Quick tool mapping

| Action | MCP tool | CLI fallback |
|--------|----------|--------------|
| Create epic | `create_task(title, type="epic", priority)` | `bea create "Title" --epic --priority P1 --json` |
| Create task | `create_task(title, priority, parent, tags)` | `bea create "Title" --priority P1 --parent <id> --json` |
| Add dependency | `add_dependency(id, depends_on)` | `bea dep add <id> <dep-id> --json` |
| List ready | `list_ready(limit, tag, epic)` | `bea ready --json` |
| List all tasks | `list_all_tasks(status, priority, tag, epic)` | `bea list --json` |
| List epics | `list_epics()` | `bea epics --json` |
| Get task | `get_task(id)` | `bea show <id> --json` |
| Search tasks | `search_tasks(query)` | `bea search "query" --json` |

## Planning Procedure

### 1. Understand the request

Before creating any tasks, gather enough context:

- Clarify scope, constraints, and must-haves vs nice-to-haves with the user
- Read relevant source files to understand existing patterns and where new code fits
- Check existing tasks (`list_all_tasks` / `bea list --json`) to avoid duplicates
- Check existing epics (`list_epics` / `bea epics --json`) for related work

### 2. Design the task breakdown

Structure the plan mentally before creating anything:

- **One concern per task**: each task should touch one layer or one logical unit
- **Core before frontends**: model → storage → graph → service → CLI → MCP → TUI
- **Tests are part of each task**, not separate tasks (unless purely adding coverage)
- **Cross-cutting changes** (error types, docs) get their own task when substantial

### 3. Create the epic

Create an epic to group the work:

```
create_task(
  title="Feature name",
  type="epic",
  priority="P1",
  body="## Scope\n\nWhat this epic covers.\n\n## Acceptance Criteria\n\n- [ ] Criterion 1\n- [ ] Criterion 2"
)
```

### 4. Create sub-tasks with dependencies

Create tasks in dependency order. Link each to the epic via `parent` and to predecessor tasks via `depends_on`:

```
create_task(title="Step 1: Data model", priority="P1", parent="<epic-id>", tags=["core"])
  → returns id "abc"

create_task(title="Step 2: Service logic", priority="P1", parent="<epic-id>", depends_on=["abc"])
  → returns id "def"

create_task(title="Step 3: CLI command", priority="P2", parent="<epic-id>", depends_on=["def"])
```

### 5. Enrich task bodies

Each task body should include enough detail for an implementer to start without asking questions.

**Task body template:**

```markdown
## Summary
One-paragraph description of what this task accomplishes and why.

## Acceptance Criteria
- [ ] Criterion 1 — specific, testable
- [ ] Criterion 2

## Implementation Notes
- Files to modify (with paths)
- Patterns to follow from existing code
- Key structs, functions, or types involved

## Edge Cases
- Error scenarios to handle
- Validation rules

## Testing
- Unit tests to add or update
- cargo fmt && cargo clippy && cargo test must pass
```

After creating a task, update its body with detailed content using `update_task(id, body=...)` or by editing the `.bears/*.md` file directly.

> **Direct file editing**: Task files are plain Markdown with YAML frontmatter at `.bears/{id}-{slug}.md`. You can read and edit them with normal file tools — this is often easier than passing long body text through MCP or CLI arguments. Use file editing to add multi-line descriptions, acceptance criteria, and implementation notes. The frontmatter fields (`status`, `priority`, `tags`, `depends_on`, `parent`, etc.) can also be edited directly.

### 6. Verify and present the plan

- Use `list_all_tasks(epic="<id>")` or `bea list --epic <id> --json` to review all tasks
- Confirm dependency ordering makes sense — `list_ready(epic="<id>")` should show only the first unblocked task(s)
- Present a summary table to the user:

```
Epic: <title> (<id>)

| # | Task | ID | Priority | Depends On |
|---|------|----|----------|------------|
| 1 | Data model changes | abc | P1 | — |
| 2 | Service logic | def | P1 | abc |
| 3 | CLI command | ghi | P2 | def |
```

## Priority Guidelines

| Priority | When to use |
|----------|-------------|
| **P0** | Blocking other work or breaking existing functionality |
| **P1** | Core functionality on the critical path |
| **P2** | Standard work, part of the epic but not blocking |
| **P3** | Polish, nice-to-have, follow-up improvements |

## Tips

- **Keep tasks small** — completable in a single focused session
- **Use tags** to categorize by area: `core`, `cli`, `mcp`, `tui`, `docs`
- **Dependencies encode ordering**, not just tracking — use them to ensure correct execution order
- **Epics auto-close** when all children complete — no need to manually complete them
- **Check for cycles** — Bears rejects dependency edges that would create circular dependencies
- **Edit files directly** — `.bears/*.md` files are plain Markdown; for long bodies or bulk edits, reading and editing the files is faster than repeated MCP/CLI calls
