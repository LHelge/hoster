# Vendored assets

Third-party files embedded in the hoster binary (ADR-0021). Nothing is loaded from a CDN.
Upgrading one means replacing the file deliberately, updating this table and checking the
upstream changelog. The tests in `src/web/assets.rs` fail if a file no longer matches the
SHA-256 recorded here.

| File | Upstream | Version | SHA-256 of the file | License |
|---|---|---|---|---|
| `htmx.min.js` | npm `htmx.org`, `dist/htmx.min.js` | 4.0.1 | `a0abb04f2f45a1f89d1d4844023245537e5e6014c87b144553e0607b4cc32130` | 0BSD (`htmx.LICENSE`) |

## Provenance

- Tarball: `https://registry.npmjs.org/htmx.org/-/htmx.org-4.0.1.tgz`
- npm integrity of the tarball (verified on download):
  `sha512-oF7JgXhDW9UXCy/k1ZYFUQgzu/rm19ORWPSPNL91vPluB2PB9+MV55tyQdH/w7o2EAbTYUG5SDW1R60mdkdqGg==`
- Downloaded 2026-11-07.

## CSP notes

The admin UI's CSP allows no inline scripts and no `eval` (`script-src 'self'`), so HTMX 4
features that compile attribute code into functions (`hx-on:*` handlers and similar) do not work
and must not be used. Use server-side logic and plain `hx-*` attributes instead.

`hoster.css` is hoster's own stylesheet, not vendored.
