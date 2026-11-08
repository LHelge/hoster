# ADR-0053: Admin UI icons are Lucide glyphs from `icondata`, served as one cached SVG sprite

- Status: Accepted
- Date: 2026-11-08

## Context

The admin UI (ADR-0021) has no icons. A design pass is planned, and icons on the navigation,
status badges and main actions should exist before it. Assets are embedded in the binary and
served from `/assets/` under content-hashed names with a one-year `immutable` cache, and the CSP
allows only same-origin resources. The UI themes itself with CSS custom properties, including a
dark mode, so icons must take the text colour.

## Decision

- **Source.** Icons come from the `icondata` crate with only its `lucide` feature: Lucide's
  24 px stroke icons, which draw in `currentColor`. Only icons the code names are compiled in.
- **Naming.** `src/web/icons.rs` defines `Icon`, an enum named by meaning in hoster
  (`Stacks`, `Warning`, `Redeploy`, …), and maps each variant to a Lucide glyph in one place.
  It renders its own markup in Askama templates (`HtmlSafe`); the markup is decorative
  (`aria-hidden`), and the visible text carries the meaning.
- **Delivery.** At startup every `Icon` is written as a `<symbol>` into one SVG sprite, served as
  an ordinary asset at `/assets/icons.<hash>.svg` (`image/svg+xml`, `public, max-age=31536000,
  immutable`; unhashed names are not found). Pages reference it with
  `<svg class="icon"><use href="/assets/icons.<hash>.svg#redeploy"/></svg>`, which inherits
  `currentColor`. A changed icon set changes the hash, and with it the URL.
- **Licence.** Lucide is ISC (Feather-derived icons MIT). The texts are in
  `assets/lucide.LICENSE`, and the sprite carries the copyright notice in a comment.

## Alternatives considered

- **Other sets in `icondata`** (Tabler, Heroicons): similar quality; Lucide's neutral stroke
  style fits the current look and keeps the choice small.
- **One file per icon** at its own hashed URL, coloured with a CSS `mask-image`: one request per
  icon and one CSS rule per icon.
- **`<img src="….svg">`**: an SVG loaded as an image cannot inherit `currentColor`, so icons
  would not follow dark mode or link and button colours.
- **Inline SVG in every page**: no extra request, but every page repeats the path data and
  nothing is cached across pages.
- **Vendoring Lucide's SVG files** into `assets/`: the same result with more files to update by
  hand; `icondata` gives typed names, so a renamed glyph fails the build.

## Consequences

- One more dependency (`icondata`, `icondata_core`, `icondata_lu`), compile-time data only.
- Adding an icon is a variant, a glyph and an id in `icons.rs`; the sprite and its hash follow.
- The design pass can replace glyphs, or the whole set, without touching templates.
