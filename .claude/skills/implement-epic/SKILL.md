---
name: implement-epic
description: Implement a whole Bears epic unattended, from its planned tasks to a pull request with green CI, by orchestrating one task-implementer subagent per task and a task-reviewer for each.
argument-hint: <epic-id>
disable-model-invocation: true
---

Implement the Bears epic `$ARGUMENTS` from start to a pull request with green CI, without
the user watching. They read one thing at the end: the pull request and your final report.

You are the orchestrator. You write no feature code and read no full diffs: each task goes
to a `task-implementer` subagent and each diff to a `task-reviewer` subagent, so that your
context lasts for the whole epic. What only you can do is the part between the tasks:
choosing the order, telling each agent what the others built, deciding on review findings,
integrating the branches, and judging whether the epic as a whole is done.

The rules of `CLAUDE.md` bind you and every agent you spawn: one branch per epic, **exactly
one Conventional Commit per task** with `Bears: <task-id>` in its body and the task's
`.bears/` file in it, review fixes squashed into the task's commit, a rebase-merged PR.

## Before starting

Stop and tell the user, instead of working around it, if any of these fails:

- `bea show <epic> --json` is an epic with planned sub-tasks (`bea list --epic <epic> -a --json`).
- Every epic or task in the epic's own `depends_on` is done. `bea ready --epic` does not
  check this, so check it yourself.
- The working tree is clean, and `main` is up to date with `origin/main`.
- No sub-task has a high `attempts` count without a body that says what changed since the
  last try; repeating a failed approach is not a plan.

Then read, as `CLAUDE.md` "What to read" says: `docs/adr/README.md`, the epic's body, every
task's body and dependencies (`bea show <id> --json` for each; the list output leaves the
dependencies out), and the ADRs and `docs/architecture.md` sections that the epic's and the
tasks' *References* name, individually and by section. You need to know the epic well
enough to tell a correct seam from a wrong one. Do not read the code the tasks will change.

Create the epic's branch from `main`: `epic/<epic-id>-<slug>`, for example
`epic/tbf-release-install-update`. If the branch already exists, this is a resumed run:
check it out, see which tasks have their commit (`git log --format='%h %s%n%b' main..`, the
`Bears:` lines), `bea release` any task left in progress with no commit, and continue with
the loop.

## The loop

Repeat until `bea ready --epic <epic> --json` returns nothing and no agent is running.

A task tagged `manual` is the user's to do, and a `decision` task is the owner's (CLAUDE.md:
architecture choices are the owner's): never give either to an implementer. Tasks of this
epic can depend on tasks of other epics, and those you do not implement either. So the loop
can end with tasks still open: blocked by a manual or decision task, by a task outside the
epic, or by a task you released. That is a partial run; see "Finishing the epic".

### 1. Choose the batch

Take the ready tasks. Run them one at a time unless running some together is safe:

- **At most one task that touches Rust at a time** (`src/`, `tests/`, `migrations/`, `migrations_logs/`,
  `.sqlx/`, `templates/`, `Cargo.toml`). hoster is one crate: such tasks share the
  migrations' order, `.sqlx/`, `Cargo.lock` and a `target/` of well over 100 GB that a
  second checkout would rebuild from nothing.
- A task that touches no Rust (only `docs/`, `site/`, `actions/`, `scripts/`, `deploy/`,
  `assets/`) can run beside a Rust task, and two such tasks beside each other, when their
  bodies make it clear that they touch different files. If you cannot tell, run them in
  sequence.
- No more than three agents at once.

A single task, and the Rust task of a parallel batch, works in the main checkout on the
epic's branch. Every other task of a batch gets its own worktree: spawn it with
`isolation: "worktree"`. The project's settings base such a worktree on the current HEAD,
so the main checkout must be clean and committed when you spawn it.

### 2. Spawn the implementers

One `task-implementer` per task, all of a batch in the same message. Its prompt holds:

- the task id and the epic id (it reads the bodies itself; do not paste them);
- where it works: the main checkout on the epic's branch, or its worktree;
- the seams it builds on, from earlier implementers' reports: the types, traits, modules,
  store methods, routes, templates or stack-format fields that exist now and where they are;
- decisions already made in this epic that bind it;
- for a batch: which other tasks run beside it and what they own, so that it keeps out.

### 3. When an implementer reports

Check the report against the repository, briefly: exactly one new commit
(`git log --format='%h %s%n%b' <base>..<head>`) with the right `Bears:` line, the tree is
clean, `git diff --stat <base>..<head>` touches what the task is about and includes the
task's `.bears/` file. Do not read the diff.

If it stopped without finishing, decide: answer it with SendMessage if you can resolve what
blocked it from the epic, the ADRs or the architecture; otherwise `bea release <id>`, note
it for the final report, and go on with the tasks that do not depend on it. If what blocked
it is an architecture choice no ADR covers, follow "Deciding without the user".

Otherwise spawn a `task-reviewer` with the task id, the directory and the commit range, and
nothing else. It must not see the implementer's report: a reviewer handed the author's
account tends to confirm it.

### 4. When a reviewer reports

Decide on every finding yourself; the reviewer advises.

- Blocking and should-fix findings you agree with go back to the same implementer with
  SendMessage, so that it fixes them with its context intact. Send the findings as the
  reviewer wrote them, with the `file:line` and the failing case. It folds the fixes into
  the task's one commit.
- A finding you judge wrong, or outside the task, you drop, with the reason kept for the
  final report. A finding that is real but belongs to another task becomes a Bears task
  (`bea create … --parent <epic>`, with `bea dep add` where it blocks something); one you
  are unsure is worth doing becomes a proposal (`bea propose <id>` after creating it).
- After fixes, send the reviewer the task's new commit with SendMessage for a second look
  at just those findings. Two rounds at most: what is still open after the second becomes a
  Bears task under the epic, and you move on.

Start the next task only when this one's commit is final, so that fixes never have to be
squashed under a later task's commit.

### 5. Integrate

A task done in the main checkout is already on the epic's branch. For each worktree task,
in dependency order:

1. `git -C <worktree> rebase <epic-branch>`. A conflict only in `.bears/` files is resolved
   by keeping both sides' changes. Any other conflict: abort the rebase and send it to the
   task's implementer, which knows both the code and its worktree.
2. `git merge --ff-only <task-branch>` in the main checkout. Never a merge commit.
3. `git worktree remove <worktree>` and delete the task's branch.

After a batch with a worktree task is integrated, run the checks of every part the batch
touched on the epic's branch (the list is in `task-implementer`, "Checks"). Send the output
to a file in your scratchpad and read only the end of it. A failure goes to the implementer
whose task it concerns.

Discovered work from the implementers' reports becomes Bears tasks now: under the epic if
the epic is not done without it, otherwise on its own with the right priority, and as a
proposal when it is uncertain. Commit new and changed `.bears/` files that no implementer
committed: `chore(bears): track work found in the <name> epic`, with `Bears: <epic>` in the
body.

## Finishing the epic

When the loop has ended, with every task done or with the rest blocked (a partial run):

1. Run every check for every part the epic touched, on the epic's branch, output to files
   as above, including `TEST_HOSTER_PODMAN=1` end-to-end tests if any task touched the
   runtime, orchestrator or proxy. Fix failures through the implementer concerned.
2. Spawn a `task-reviewer` for an epic review of `main..HEAD`. Handle its findings as in
   step 4, sending each to the implementer of the task it concerns; it squashes the fix
   into that task's commit (`git commit --fixup=<sha>`, then
   `git rebase --autosquash <sha>~1`).
3. Read the seams yourself, and only those: the new migrations and their `.down.sql`, the
   changes to `docs/adr/` and `docs/architecture.md`, and to the stack format
   (`site/content/wiki/stack-format.md`). Check them against the epic's body and its
   "done when".
4. `bea done <epic>`, and fold the epic's `.bears/` file into the last task's commit
   (`git commit --fixup=HEAD && git rebase --autosquash HEAD~2` when that commit is HEAD),
   so that no status needs a commit of its own. In a partial run, leave the epic open and
   skip this step; the pull request's title and description say that it is a part, and
   list each open task with what blocks it.
5. Rebase on `origin/main` if it moved, rerun the checks if it did, push, and open the pull
   request with `gh pr create`. Its description, written for a reader who has not followed
   the run: what the epic delivers, in the user's terms; the tasks with their ids; **decisions
   made**: every place where an implementer or you chose between readings, with the reason,
   so the user can overrule one cheaply; findings you dropped and why; tasks you created;
   open questions and `decision` tasks you added; anything not done. Note that it is to be
   merged with a rebase merge.
6. Watch CI (`gh pr checks <number> --watch`). A failing job is fixed through the
   implementer concerned, squashed into its task's commit, and force-pushed with
   `--force-with-lease`, until every check is green.

Do not merge the pull request, never bump the version, and never create a release tag.

## Deciding without the user

The user is not there, so do not stop to ask what you can decide. Where a task is open to
two readings, take the one that fits `spec.md`, the ADRs and the architecture, and record
it under "decisions made". Prefer the reading that is easier to change later.

An **architecture choice that no ADR covers is not yours or an implementer's to make.** Add
it to `docs/open-questions.md` with the options and a recommendation, create a `decision`
task that blocks the affected task, commit both as `docs: raise <question>` with the
epic's id in a `Bears:` line, release the affected task, and go on with what does not
depend on it.

Stop and report, with the epic's branch left as it is, only when going on would mean one
of these: a breaking change to the stack format, the CLI or the admin API; an edit to a
merged migration; deleting or rewriting work outside the epic; a task that contradicts an
ADR; or the same task failing after two implementers have tried it and nothing else is
ready.

## The final report

A few lines: the pull request's link and whether CI is green; the tasks done and any
released; the decisions the user should look at first, and the open questions raised; the
Bears tasks you created. If the run stopped early, lead with what stopped it and what the
user needs to decide.
