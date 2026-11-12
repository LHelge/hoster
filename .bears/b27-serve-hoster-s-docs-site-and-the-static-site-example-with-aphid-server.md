---
id: b27
title: Serve hoster's docs site and the static-site example with aphid-server
status: done
priority: P2
created: "2026-11-12T13:14:07.297564618Z"
updated: "2026-11-12T22:41:27.784571069Z"
tags:
  - docs
  - site
attempts: 1
---

## Summary
The documentation site (https://docs.hoster.lhelge.se) and `examples/static-site/` pack the built site into `nginx-unprivileged` with a `default.conf`. aphid 0.4.1 ships `aphid-server` (`ghcr.io/lhelge/aphid-server`), a small static-file server that listens on `$PORT`, answers `/healthz`, serves `404.html` and logs JSON on request. Move both to it, as aphid's own docs site already is (`docs/Dockerfile` and `docs/hoster/aphid-docs.stack.toml` in LHelge/aphid).

Work outside an epic: its own short-lived branch and PR.

## Acceptance Criteria
- [ ] `site/Dockerfile` is `FROM ghcr.io/lhelge/aphid-server:<version>` plus `COPY dist /site`; `site/default.conf` is deleted
- [ ] `https://docs.hoster.lhelge.se/stack.schema.json` still answers: stack files name that URL in `#:schema`, and the nginx image copied `dist/static/stack.schema.json` to the site root. Keep an equivalent `COPY`
- [ ] `site/docs.stack.toml` sets `APHID_LOG_FORMAT = "json"`, and its nginx comments are updated
- [ ] `examples/static-site/` uses aphid-server (two-line Dockerfile, no `default.conf`), and `release.yml` there still works
- [ ] `site/content/wiki/static-sites.md` describes the new setup: the table of files, the `PORT` paragraph (aphid-server honours it), and "Things to know" (redirects, `www` to apex, which had an nginx `server` block as one option). Say that any static server works and keep nginx as a one-line alternative
- [ ] The aphid action and the aphid-server image use the same pinned version in `.github/workflows/release.yml` and the Dockerfile
- [ ] `aphid build` in `site/` passes; the image is built and run locally and checked for: `/`, a wiki page, a directory without trailing slash (relative redirect), a missing page (404 with the site's 404 page), `/healthz`, `/stack.schema.json`
- [ ] CLAUDE.md and README mentions of the nginx image are updated

## Implementation Notes
- nginx's `try_files $uri $uri/ $uri.html` served `/page` from `page.html`. Check what aphid emits (directories with `index.html` or `.html` files) and that every existing URL still resolves on aphid-server; a changed URL shape is a finding to raise, not to paper over
- nginx gzipped responses; check whether aphid-server compresses, and note it in the PR if not (hoster's proxy does not compress)
- The stack file change has to be applied by hand in the admin UI (ADR-0027); say so in the PR description. Applying it redeploys (ADR-0049)
- The docs image is only built on a release tag (`release.yml`), so the first real deploy is the next release; verify locally before that

## References
- ADR-0003, ADR-0018, ADR-0027, ADR-0051
- `site/Dockerfile`, `site/default.conf`, `site/docs.stack.toml`, `examples/static-site/`, `site/content/wiki/static-sites.md`, `.github/workflows/release.yml`
- https://github.com/LHelge/aphid (`examples/aphid-server-site/`, `docs/Dockerfile`)