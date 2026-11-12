# UI design reference

> **Reference material only.** These are the owner's Claude Design mockups for the admin UI
> design pass (epic 8bf), kept so later work can read them. They are not hoster's UI and nothing
> here is served. What hoster adopted from them, and what it deliberately did not (secret
> reveal, `exec`, editing env or settings in the UI, git-repo sources, Inter, inline styles and
> more), is recorded in [ADR-0054](../adr/0054-admin-ui-design-system.md); where this README and
> the ADR disagree, the ADR wins. The `.dc.html` files load a `./support.js` that is not
> included and fonts from Google Fonts, so they do not render as intended when opened directly;
> read them as source. Their sample data is illustrative, not hoster's API.

Design mockups for the stack/deploy manager UI. These are reference designs, not production code: re-implement them in the app's own frontend stack.

## Files

One file per screen, linked together by `<a href="X.dc.html">`:

| File | Screen |
|---|---|
| Main.dc.html | Dashboard: traffic by status class, p50/p95, stacks table, certificates, recent events |
| Stacks.dc.html | Stack list as cards, filter/sort, card/table toggle |
| StackDetail.dc.html | Stack overview: KPIs, charts, services (containers), routes, volumes, env & secrets, deployments, danger zone |
| StackLogs.dc.html | Stack logs tab: service filter, grep, level, follow, source switch |
| Certificates.dc.html | Certificate list, renew, DNS check panel (resolvers + HTTP-01 probe) |
| Settings.dc.html | Grouped settings sub-nav; General, Source control, Reverse proxy, ACME drawn |
| System.dc.html | Version/rollback, update flow, host info, images & storage (replaces a Containers menu) |
| NewStack.dc.html | Create stack: repo/branch/compose file, services→domains, env, plan + pre-flight |
| Telemetry.dc.html | OTel traces and logs. Traces: duration scatter, trace list, span waterfall (collapse, critical path, inline span details with attributes, events, resource, span logs). Logs: severity histogram, filters, expandable records. Traces and logs link to each other via trace_id/span_id |

Sidebar order: Dashboard, Stacks, Telemetry, Certificates, Settings, System.

### Telemetry notes

Added 2026-11-12 (planned in epics `psq`, `qnh` and `xv9`). Where the mockup disagrees with a
decision, the decision wins: the receiver speaks OTLP/HTTP protobuf only, not gRPC on :4317
(OQ-42, question 4); spans are kept 3 days and logs 7 days in a 1 GiB `logs.db` (ADR-0067,
OQ-42); the query box takes no query language yet, only grep beside filter controls (OQ-43);
hoster's own records appear under the service `hoster` (ADR-0068), where the mockup says
`proxy`.

- Service colours are categorical and must not overlap the status colours: proxy `#b4befe`, then `#94e2d5`, `#cba6f7`, `#89dceb`, `#74c7ec`, `#f5c2e7`, `#f9e2af`. Error spans keep their service colour and get a red ring (`#f38ba8`).
- The proxy emits its own server + upstream client spans for every request, so apps without OTel still get 2-span traces ("app not instrumented").
- Waterfall math is in `renderVals()`: DFS order by start time, depth from parent chain, critical path = recursively the child with the latest end, self time = duration minus the union of child intervals.
- Attribute keys follow OTel semantic conventions (`http.route`, `db.query.text`, `server.address`, ...).

## Reading the .dc.html format

- Markup is inside `<x-dc>`; `<helmet>` holds fonts and a few shared CSS rules.
- `{{name}}` is a value from `renderVals()` in the `<script type="text/x-dc">` block at the bottom of each file.
- `<sc-for list="{{items}}" as="item">` repeats its body; `<sc-if value="{{cond}}">` is conditional.
- The sample data (stacks, certs, log lines, env vars) lives in `renderVals()`. Use it as fixture data and as a guide to the API shapes the UI needs.
- Styles are inline. Lift the tokens below into CSS variables / theme config rather than copying inline styles.

## Design tokens (Catppuccin Mocha)

- Backgrounds: page `#1e1e2e` (base), cards and sidebar `#181825` (mantle), code and inputs `#11111b` (crust)
- Borders: `#313244` (surface0), inner dividers `#26263a`, input borders `#6c7086` (overlay0; Latte `#7c7f93` overlay2), for 3:1 against the field and the card
- Text: `#cdd6f4` primary; `#bac2de` / `#a6adc8` secondary; `#9399b2` muted. Do not go dimmer than `#9399b2` for text on base, mantle or crust: overlay0/1 fail 4.5:1 contrast on base. On surface0 (chips, tags, hovered rows) `#9399b2` only reaches 4.45:1, so use `#a6adc8` or brighter there (ADR-0054).
- Accent: mauve `#cba6f7` (primary buttons with `#11111b` text, active nav, focus rings)
- Status colours:
  - 2xx / healthy / valid: green `#a6e3a1`
  - 3xx / in progress / renewing: blue `#89b4fa`
  - 4xx / degraded / warning: peach `#fab387`
  - 5xx / failed: red `#f38ba8`
  - p95 line: yellow `#f9e2af`; p50 line and resource meters: teal `#94e2d5`
- Pills: status colour as text, with the same colour at ~10% alpha as background (e.g. `#a6e3a11a`).
- Type: Inter for UI prose; JetBrains Mono for anything machine-derived (names, SHAs, numbers, paths, labels, table headers).
  - Table headers: 11px uppercase, letter-spacing .06em.
- Radii: cards 10–12px, controls 8px, pills 10px.

## Layout

- Fluid layout with a 232px sidebar. Content takes the rest; the sidebar stacks on top at narrow widths.
- Wide tables scroll horizontally inside their card.
- Interactive targets are at least 44px where they're primary actions.
