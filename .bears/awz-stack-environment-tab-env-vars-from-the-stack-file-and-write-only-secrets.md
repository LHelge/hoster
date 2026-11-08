---
id: awz
title: "Stack environment tab: env vars from the stack file and write-only secrets"
status: open
priority: P2
created: "2026-11-08T18:20:40.506086332Z"
updated: "2026-11-08T18:20:40.506086332Z"
tags:
  - web
  - ui
  - secrets
depends_on:
  - "7wg"
parent: "8bf"
---

Restyle the stack secrets page as the **environment** tab, after the "Environment & secrets" section of `docs/design/StackDetail.dc.html`.

- One table: **key** (mono), **value**, **service**, **type** pill. Plain env vars come from the active revision's stack file and show their values (they're in the stack file anyway). Secrets show `stack/<name>` as the value, never the secret value (write-only, ADR-0029), with the type pill in accent and a warn "not set" pill when missing. No reveal button, and no edit for plain vars (they change through the stack file). A help line explains both.
- Keep **set a secret** (name with datalist, value, review step with the redeploy confirm from ADR-0049) and **delete** (with the type-the-name confirm when referenced) in the new form and button styles.
- The tab is reached from the stack tab bar. If the route is renamed (for example `/stacks/{name}/environment`), the old `/stacks/{name}/secrets` URLs redirect, and the form endpoints and CSRF keep working.

References: ADR-0029, ADR-0049, `templates/stack_secrets.html`, `templates/secret_confirm.html`, `src/web/stacks/secrets.rs`, `docs/design/StackDetail.dc.html`.