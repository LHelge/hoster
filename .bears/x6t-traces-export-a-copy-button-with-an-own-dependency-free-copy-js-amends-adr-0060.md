---
id: x6t
title: "Traces export: a Copy button with an own dependency-free copy.js (amends ADR-0060)"
status: open
priority: P3
created: "2026-11-18T09:41:43.208942745Z"
updated: "2026-11-18T09:41:43.208942745Z"
tags:
  - observability
  - web
depends_on:
  - "2ty"
parent: byh
---

## Summary
A **Copy** button beside the export's Download and Open as text links that puts the export text (task `2ty`) on the clipboard, written as hoster's second own script under ADR-0060's rules. The owner chose this on 2026-11-18 over a script-free "open as text" only.

## Acceptance Criteria
- [ ] `assets/copy.js`: dependency-free, embedded and served under a content-hashed name like `editor.js` (`src/web/assets.rs`), loaded as `<script type="module">` only on the Traces tab; CSP unchanged (`script-src 'self'`, no inline script, no `style=`)
- [ ] Progressive enhancement: the markup is an `<a href="…/export?…">` (the Open as text link) that the script enhances into a copy button by a `data-` attribute. Without the script, or without `navigator.clipboard`, the link still opens the text
- [ ] It copies with `navigator.clipboard.write([new ClipboardItem({"text/plain": fetchPromise})])` where available, so Safari keeps the user activation across the fetch, and falls back to `fetch` then `writeText`. It says "Copied" or "Copy failed" in an `aria-live` region for a moment, by class and text, with no inline styles
- [ ] ADR-0060 gets an amendment line (status and a short section): own scripts may be added under the same rules, and `copy.js` is the second. The `docs/adr/README.md` row for 0060 mentions it
- [ ] Pure parts, if any, tested with `node --test` as `editor.js` is in CI; the handler-side markup tested in the traces tests
- [ ] `site/content/wiki/tracing.md` "Exporting traces" mentions Copy

## References
- ADR-0060, ADR-0054 (CSP)
- `assets/editor.js`, `src/web/assets.rs`, `templates/telemetry_traces.html`