# Vendored assets

Third-party files embedded in the hoster binary (ADR-0021). Nothing is loaded from a CDN.
Upgrading one means replacing the file deliberately, updating this table and checking the
upstream changelog. The tests in `src/web/assets.rs` fail if a file no longer matches the
SHA-256 recorded here.

| File | Upstream | Version | SHA-256 of the file | License |
|---|---|---|---|---|
| `htmx.min.js` | npm `htmx.org`, `dist/htmx.min.js` | 4.0.1 | `a0abb04f2f45a1f89d1d4844023245537e5e6014c87b144553e0607b4cc32130` | 0BSD (`htmx.LICENSE`) |
| `hx-sse.min.js` | npm `htmx.org`, `dist/ext/hx-sse.min.js` (the SSE extension, registers as `sse`) | 4.0.1 | `18ef2761356ec1fe0bb6d92612315b37517a7c1e9856b661c5ae81fef239d103` | 0BSD (`htmx.LICENSE`) |
| `jetbrains-mono-400.woff2` | GitHub `JetBrains/JetBrainsMono` release v2.304, `fonts/webfonts/JetBrainsMono-Regular.woff2`, Latin subset | 2.304 | `9498a2e109cae16f2dec519485504fc439ed7ae9ec36504ec0a53994df1fd968` | OFL-1.1 (`jetbrains-mono.LICENSE`) |
| `jetbrains-mono-500.woff2` | the same release, `JetBrainsMono-Medium.woff2`, Latin subset | 2.304 | `17b79a17c3dc15a643f8f931853a69b1b0d4b5dea3d3a775aa907e2b9f990a25` | OFL-1.1 (`jetbrains-mono.LICENSE`) |
| `jetbrains-mono-600.woff2` | the same release, `JetBrainsMono-SemiBold.woff2`, Latin subset | 2.304 | `5f07c783517142d9243f3c110be369c7c048336e93c76a4f2afe039284dc6dea` | OFL-1.1 (`jetbrains-mono.LICENSE`) |

## Provenance

- Tarball: `https://registry.npmjs.org/htmx.org/-/htmx.org-4.0.1.tgz`
- npm integrity of the tarball (verified on download):
  `sha512-oF7JgXhDW9UXCy/k1ZYFUQgzu/rm19ORWPSPNL91vPluB2PB9+MV55tyQdH/w7o2EAbTYUG5SDW1R60mdkdqGg==`
- Downloaded 2026-11-07; `hx-sse.min.js` taken from the same tarball (integrity re-verified)
  on 2026-11-08.

## JetBrains Mono

The admin UI's monospace face (ADR-0054), at weights 400, 500 and 600, served as hashed
`font/woff2` assets that `hoster.css` loads with `font-display: swap`.

- Archive: `https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip`,
  SHA-256 `6f6376c6ed2960ea8a963cd7387ec9d76e3f629125bc33d1fdcd7eb7012f7bbf`, downloaded
  2026-11-08. The SHA-256 of the unsubset release files: Regular
  `a9cb1cd82332b23a47e3a1239d25d13c86d16c4220695e34b243effa999f45f2`, Medium
  `086c48dfbea9ddaff1320f7e09399b8e2924e88ce67453721255db3bdbb5a353`, SemiBold
  `918edad542a1da608fd2ba8daebaff9ac802309103fe760eed465b8b4e47faf1`.
- Subset with fonttools 4.68.0 (`pyftsubset`, with `brotli` for woff2), keeping every layout
  feature, to Google Fonts' "latin" range plus arrows and `≤ ≥`; text outside it falls back to
  the system monospace font:

  ```sh
  pyftsubset JetBrainsMono-Regular.woff2 --flavor=woff2 --layout-features='*' \
    --unicodes='U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,U+02DA,U+02DC,U+0304,U+0308,U+0329,U+2000-206F,U+20AC,U+2122,U+2190-2199,U+2212,U+2215,U+2264-2265,U+FEFF,U+FFFD' \
    --output-file=jetbrains-mono-400.woff2
  ```

  and the same for Medium (500) and SemiBold (600).
- Licence: SIL Open Font License 1.1, the release's `OFL.txt`, copied as
  `jetbrains-mono.LICENSE`. The font declares no Reserved Font Name, so the subset keeps its
  name.

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
