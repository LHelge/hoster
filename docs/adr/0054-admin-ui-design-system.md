# ADR-0054: Admin UI design system: Catppuccin Mocha/Latte, vendored JetBrains Mono, sidebar shell, stack tabs as URLs, CSP unchanged

- Status: Accepted (amended by ADR-0062, which adds traffic history, its charts and range picker, and by ADR-0066, which adds the sidebar host meters and the services table's CPU and memory columns)
- Date: 2026-11-08

## Context

The admin UI (ADR-0021) grew page by page with a plain stylesheet, and ADR-0053 gave it icons.
The owner had a set of screens drawn in Claude Design, kept as reference in `docs/design/`
(dashboard, stacks, stack overview and logs, certificates, settings, system, new stack). They
are mockups: inline styles, Google Fonts, a script runtime, and sample data for features hoster
does not have or has ruled out.

The UI must keep its constraints while adopting the look. Everything is server-rendered with
Askama and HTMX, assets are embedded and served from `/assets/` under content-hashed names, and
the web server sends a CSP of `default-src 'self'` with no `unsafe-*` (`src/web/mod.rs`),
which ADR-0021's "nothing from a CDN" relies on. Observability lives in the UI only (ADR-0045).
Stack files and `hoster.toml` are the sources of truth (ADR-0024, ADR-0027), and secret values
are never shown (ADR-0029).

## Decision

The owner made these choices on 2026-11-08 (epic 8bf):

- **Theme.** Catppuccin **Mocha** in dark mode and **Latte** in light mode, chosen by
  `prefers-color-scheme`; there is no toggle. Tokens are CSS custom properties in
  `assets/hoster.css`. Every text colour, status colours included, meets 4.5:1 against the
  surface it actually sits on, in both themes, so the dimmest allowed text depends on the
  background. On base, mantle and crust it is `#9399b2` (overlay2) in Mocha, the README's
  contrast rule, and `#5c5f77` (subtext1) in Latte, whose overlay2 and subtext0 fail there. On
  surface0 (chips, tags, hovered rows) those two fall short (4.45:1 and 4.05:1), so dim text
  there uses `#a6adc8` (subtext0) in Mocha and `#4c4f69` (text) in Latte. Text on surface1 or
  lighter is checked case by case; neither of those clears 4.5:1 on it.
- **Fonts.** **JetBrains Mono** (OFL) is vendored as a Latin-subset woff2, served as a hashed,
  `immutable` asset like the others, with its licence text alongside. It is used for everything
  machine-derived: names, SHAs, digests, numbers, paths, labels and table headers. Prose uses
  `system-ui`. Inter, which the mockups use for prose, is not vendored.
- **Shell.** Every page sits in one layout: a 232 px sidebar with the brand, version and
  hostname, the navigation with counts, and the signed-in user and session. At narrow widths the
  sidebar stacks above the content. Each page has a `~/path` breadcrumb and an h1 header with a
  slot for its actions. Wide tables scroll inside their card; the page itself never scrolls
  sideways, down to 375 px.
- **Containers** leaves the main navigation. A stack's containers and their logs are shown on
  that stack's pages, and `/containers` redirects to `/stacks`.
- **Stack pages are separate URLs** under one shared header and tab bar: overview, logs,
  environment and deployments. The danger zone is the last section of the overview.
- **CSP stays as it is.** There are no `style=` attributes and no inline scripts. Meters, bars
  and charts are SVG drawn on the server, with sizes as attributes, or `<meter>`. Filters and
  toggles are query parameters or htmx requests.
- **Only real data is drawn.** Where a mockup shows data hoster does not record (traffic
  history, request rates, sparklines, recent events, CPU and memory), the page leaves that part
  out until a backend exists for it; there are no placeholder charts.
- **Not adopted**, because an ADR or hoster's model rules it out: `exec` into containers;
  revealing secret values (ADR-0029); editing env, domains or settings in the UI (ADR-0024,
  ADR-0027); git-repo or compose stack sources and the repo/branch/compose picker (ADR-0003,
  ADR-0027); a GitHub App and an HMAC webhook secret (ADR-0047); users and tokens (one admin,
  ADR-0021, ADR-0031); DNS-01 (ADR-0016); a rollback button in the UI (CLI only, ADR-0052);
  release channels and a background "update available" badge (ADR-0052); an auto-deploy toggle;
  "changes not deployed" (ADR-0049); memory limits (ADR-0038).

## Alternatives considered

- **Mocha only**: one palette to maintain, but it ignores an operator's light-mode preference,
  and the UI already follows `prefers-color-scheme`.
- **Mocha with a Latte toggle**: needs a stored preference (cookie or script) and a flash of the
  wrong theme on load, for a choice the browser already makes.
- **Vendoring Inter too**: closer to the mockups, but a second font family to ship and update
  for prose that `system-ui` sets well.
- **System fonts only**: no vendored file, but monospace stacks differ widely between systems,
  and the mono face carries most of the UI's data.
- **A single-page stack detail with anchors**: one long page that loads logs and deployments
  every time, and whose sections cannot be linked or refreshed independently.
- **Keeping Containers in the navigation**: a second place to look for the same containers,
  with no stack context.
- **Relaxing the CSP to `style-src 'unsafe-inline'`**: would allow the mockups' inline widths
  and colours, but weakens a protection the UI has always had, for a styling shortcut.

## Consequences

- `docs/design/` holds the mockups as reference; this ADR, not the mockups, says what is
  adopted.
- One more vendored asset (the woff2 subset) and its licence; text outside the Latin subset
  falls back to the system monospace font.
- Server-drawn SVG means bar and meter geometry is computed in Rust and covered by tests, not
  styled in CSS.
- Bookmarks to `/containers` keep working through the redirect.
- Each part of the mockups that needs new backend (traffic history, events, resource usage,
  notifications and others) is its own follow-up and adopts the matching screen when it lands.
