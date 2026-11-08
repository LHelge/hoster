---
id: qde
title: ADR-0054 and the design reference in docs/design
status: done
priority: P2
created: "2026-11-08T18:19:01.770890071Z"
updated: "2026-11-08T18:32:02.339582642Z"
tags:
  - docs
  - web
  - ui
parent: "8bf"
attempts: 1
---

Record the owner's design-pass decisions and keep the mockups in the repo, so later tasks can read them without the owner's Downloads folder.

- Unzip `~/Downloads/design-ref.zip` into `docs/design/` (8 `*.dc.html` screens, `README.md`; drop `canvas.json`). Add a short note at the top of its README that this is reference material only, with a pointer to ADR-0054 for what hoster adopted and what it didn't.
- Write `docs/adr/0054-admin-ui-design-system.md` (Status Accepted, 2026-11-08) from the epic's "Owner decisions" and "Not adopted" sections: Mocha/Latte via `prefers-color-scheme`; vendored JetBrains Mono only, with prose in system-ui; sidebar shell; Containers folded into stack pages; stack tabs as URLs; CSP unchanged, so meters and charts are server-drawn SVG and there are no inline styles or scripts. Alternatives: Mocha only, Mocha with a Latte toggle, vendoring Inter too, system fonts only, single-page stack detail with anchors, keeping Containers in the nav, relaxing CSP for `style-src 'unsafe-inline'`.
- Add the row to `docs/adr/README.md`. ADR-0053's "fits the current look" stays as it is (ADRs are immutable).

References: epic 8bf body, `docs/adr/README.md`, ADR-0021, ADR-0053.