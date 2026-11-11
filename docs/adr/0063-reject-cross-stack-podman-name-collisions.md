# ADR-0063: Reject a stack whose podman container or volume names collide with another stack's

- Status: Accepted (resolves OQ-35; amends ADR-0030)
- Date: 2026-11-11

## Context

Stack, service and volume names may contain `-` (`[a-z][a-z0-9-]{0,30}`), and podman names join
them with `-` (architecture §8): containers `hoster-<stack>-<service>-g<n>`, volumes
`hoster-<stack>-<volume>`. Different stacks can therefore map to the same podman name: stack `a-b`
volume `c` and stack `a` volume `b-c` are both `hoster-a-b-c`, and likewise for services.
Networks (`hoster-<stack>`) carry only the stack name and cannot collide.

By the time this was decided, the worst outcome was already prevented. Volumes are labelled
`hoster.stack` at creation, and a deploy refuses to mount a volume labelled for another stack, so
one stack can never mount another's data. Containers are only ever looked up by labels (engine,
reconciler, removal), never by name, so a stack never touches another's container. What remained
was a confusing failure. The second stack's deploy fails late, either because the volume belongs
to another stack or, only when the generation numbers also match, with a container create
"already exists". It depends on the timing and looks random.

## Decision

- **Ownership of podman names.** Within the stack, the string `<stack>-<service>` (for containers)
  and `<stack>-<volume>` (for volumes) is owned by exactly one stack. Validation of a stack apply
  rejects a stack whose `<stack>-<service>` or `<stack>-<volume>` equals another stack's, before
  anything is created. It works like route collisions (ADR-0012). The error names both stacks and
  the podman name, e.g. `volume b-c would be podman volume hoster-a-b-c, which stack a-b's volume
  c already uses`. Re-applying a stack never collides with itself.
- Services and volumes are checked separately, because containers and volumes are separate podman
  namespaces. A service `x` and a volume `x` never collide.
- A removed stack's **kept volumes** (ADR-0030) still own their names. A podman volume labelled
  `hoster.stack=<s>` claims `<s>-<volume>` until it is deleted, so a new stack cannot apply its way
  onto a removed stack's data either.
- **Defence in depth stays.** The deploy-time label check on volumes (refusing to mount a volume
  labelled for another stack) is kept as the last line of defence, as `RouteTable::build` is for
  routes.
- The naming scheme does not change.

## Alternatives considered

- **An unambiguous separator** (`hoster-<stack>_<volume>`, `hoster-<stack>_<service>-g<n>`).
  Collisions become impossible by construction. Rejected. Podman cannot rename a volume, so every
  existing host's volumes would need a data-copying migration or a stored per-volume name, and
  the label↔name check on containers would have to accept both formats during the upgrade. That
  is a high price for a collision that needs contrived names on a single-admin host.
- **Forbid `-` in stack names.** Rejected, since it breaks existing stacks with hyphenated names.
- **Keep the ownership label check only.** It prevents the data leak but leaves the late,
  timing-dependent failure. Rejected.

## Consequences

- Whichever stack is applied first keeps a contested name. Moving a service or volume name
  between stacks takes two steps, as moving a route does: remove it from one stack (and delete its
  kept volume), then add it to the other.
- Apply needs the other stacks' committed revisions (it has them for route collisions) and the
  labelled volumes that podman holds for removed stacks.
- Architecture §8, ADR-0030 and the stack-format guide describe the rule.
