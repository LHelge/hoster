# Vendored assets

Third-party files embedded in the hoster binary (ADR-0021). Nothing is loaded from a CDN.
Upgrading one means replacing the file deliberately, updating this table and checking the
upstream changelog. The tests in `src/web/assets.rs` fail if a file no longer matches the
SHA-256 recorded here.

| File | Upstream | Version | SHA-256 of the file | License |
|---|---|---|---|---|
| `htmx.min.js` | npm `htmx.org`, `dist/htmx.min.js` | 4.0.1 | `a0abb04f2f45a1f89d1d4844023245537e5e6014c87b144553e0607b4cc32130` | 0BSD (`htmx.LICENSE`) |
| `hx-sse.min.js` | npm `htmx.org`, `dist/ext/hx-sse.min.js` (the SSE extension, registers as `sse`) | 4.0.1 | `18ef2761356ec1fe0bb6d92612315b37517a7c1e9856b661c5ae81fef239d103` | 0BSD (`htmx.LICENSE`) |

## Provenance

- Tarball: `https://registry.npmjs.org/htmx.org/-/htmx.org-4.0.1.tgz`
- npm integrity of the tarball (verified on download):
  `sha512-oF7JgXhDW9UXCy/k1ZYFUQgzu/rm19ORWPSPNL91vPluB2PB9+MV55tyQdH/w7o2EAbTYUG5SDW1R60mdkdqGg==`
- Downloaded 2026-11-07; `hx-sse.min.js` taken from the same tarball (integrity re-verified)
  on 2026-11-08.

## Icons

The icon sprite (`/assets/icons.<hash>.svg`, ADR-0053) is generated at startup from
[Lucide](https://lucide.dev) glyphs in the `icondata` crate (`icondata_lu`, pinned in
`Cargo.lock`), so no SVG file is vendored here. Only the icons `src/web/icons.rs` names are
compiled in. Lucide is ISC-licensed, with the icons derived from Feather under MIT; both texts
are in `lucide.LICENSE`, and the sprite carries the copyright notice in a comment. Upgrading the
glyphs means `cargo update -p icondata_lu`; a glyph Lucide renamed fails the build, since
`icons.rs` names each one as a Rust item.

## CSP notes

The admin UI's CSP allows no inline scripts and no `eval` (`script-src 'self'`), so HTMX 4
features that compile attribute code into functions (`hx-on:*` handlers and similar) do not work
and must not be used. Use server-side logic and plain `hx-*` attributes instead.

`hoster.css` is hoster's own stylesheet, not vendored.
