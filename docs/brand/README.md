# InPhase brand

| File | Use |
| --- | --- |
| `inphase-icon.svg` | App icon: favicon, installer, touch icon |
| `inphase-logo.svg` | Logo with icon tile, for light backgrounds |
| `inphase-logo-dark-bg.svg` | Logo with icon tile, for dark backgrounds (the web UI's `lockup.svg`) |
| `inphase-logo-no-tile.svg` | Logo without the tile, for light backgrounds |
| `inphase-logo-stacked.svg` | Icon above the wordmark, for light backgrounds |
| `inphase-mark-transparent.svg` | Mark alone, for light backgrounds |
| `inphase-mark-transparent-light.svg` | Mark alone, for dark backgrounds (the web UI's `symbol.svg`, source of the tray masks) |
| `preview.png` | All of the above at a glance |

Colors: wave gradient `#22D3EE` → `#8B5CF6`, tile `#0E1220`, light strokes
`#F4F6FB`. The wordmark is outlined paths, so no font is needed.

Derived assets: `web/public/favicon*`, `web/public/brand/`, `installer/inphase.ico`
and `crates/host/src/platform/windows/tray_mask.rs`. Render the SVGs with a
browser when regenerating them: they use gradients and masks that
ImageMagick's built-in SVG renderer draws wrong.
