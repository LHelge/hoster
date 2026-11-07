# ADR-0050: The orchestrator is a shared handle with async methods, not a command-channel actor

- Status: Accepted (resolves OQ-33; fixes the vocabulary used by ADR-0035 #1 and ADR-0036)
- Date: 2026-11-07

## Context

architecture.md described the orchestrator as an actor: one task reading a bounded
`mpsc<Command>`, answering through `oneshot`s, with deploy tasks reporting back over a second
channel. Every operation then exists three times (enum variant, reply type, handler arm) and every
caller is message plumbing. The property the actor was there to give — a single writer of desired
state and of the `RouteTable` — does not need message passing to hold. ADR-0008 fixes channels and
`ArcSwap` for the *proxy* boundary only, and ADR-0035 #1 fixes only that the orchestrator, not a
deploy task, performs the swap. CLAUDE.md asks for behaviour on types and traits.

## Decision

- **`Orchestrator` is a struct behind `Arc`**, constructed once by the supervisor with the store,
  the `ContainerRuntime`, the registry client, the `ArcSwap<RouteTable>` writer, the ACME command
  sender and the `ui_events` broadcast sender.
- **Callers invoke async methods and get typed results:** `apply(stack_file)`,
  `deploy(stack, images, source)`, `remove(stack, delete_volumes)`, `rollback(stack,
  deployment_id)`, `status()`, and so on. Axum handlers, the webhook, the poller and the
  reconciler all hold the same `Arc<Orchestrator>`.
- **Single writer by construction.** Desired state and the per-stack deploy slots live in one
  `Mutex<State>` inside the struct. Critical sections only read or update in-memory state and
  publish the `RouteTable`; they never await I/O. Nothing outside `Orchestrator`'s methods can
  touch desired state or store a `RouteTable`.
- **Deploy tasks are spawned by the handle** and drive the pure transition function. They call
  back through `orchestrator.transition(deployment_id, event)`, which applies the pure function
  under the lock, persists the transition, performs the swap on `Switching`, fans out
  `ui_events`, and returns the next effects to execute.
- **Channels remain where a boundary needs them:** the Pingora shutdown watch and execution
  phase (ADR-0008), `acme_tx`/`acme_events` to and from the ACME task, and the `ui_events`
  broadcast to SSE handlers. The `Command`, `DeployEvent` and per-request `oneshot` channels are
  gone.
- **Tests** construct `Orchestrator` with a `FakeRuntime` and an in-memory store and call its
  methods directly.

## Alternatives considered

- **Actor + `Command` enum** (as documented): an explicit serialization point and free
  back-pressure, at the cost of a variant, a reply type and a handler arm per operation.
  Back-pressure is not needed: callers are a single admin and a webhook that is itself
  rate-limited by GitHub Actions.
- **Hybrid** (methods for requests, a channel for deploy-task events): adds a second mechanism for
  one direction; `transition()` does the same job as a method.

## Consequences

- "Orchestrator actor" in the documentation becomes "orchestrator". `Command`, `DeployEvent`
  and `cmd_tx` disappear from architecture §3.3.
- Every method is a natural tracing span, so the audit trail the actor's command log would have
  given is kept.
- The lock discipline must be followed: hold the mutex only for in-memory work. A clippy-friendly
  way is to keep `State` private and expose it only through methods that take `&mut State` and
  return before any `.await`.
