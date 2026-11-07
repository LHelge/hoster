# ADR-0040: One active deployment per stack; queue keeps only the newest target

- Status: Accepted (resolves OQ-15)
- Date: 2026-11-07

## Decision

- **Per stack:** at most one active deployment. Different stacks deploy in parallel, with at most
  **2 concurrent image pulls** in total.
- **The queue keeps the newest only.** Each stack has one pending slot holding the newest
  requested target. A replaced pending request is recorded as `Superseded`.
- **When the active deployment ends,** the pending target is checked for idempotency again
  (ADR-0029) and then started, or becomes a `Noop`.
- **Cancelling** an active deployment from the UI is allowed before `Switching`, and leads to
  `Aborting`.

## Alternatives considered

- **Strict FIFO**: deploys every release in order. Slow, and rarely useful.
- **Newest preempts active**: a busy release stream could keep cancelling deploys before they
  finish.

## Consequences

- Intermediate releases may never be deployed. They appear as `Superseded` in the history.
