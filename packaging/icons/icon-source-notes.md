# Icon source notes — `handfast.svg`

## Motif

Two interlocking rounded rings, drawn as a single alternating over-under
weave (a "handfast knot"). The left teal ring represents the desktop; the
right amber ring represents the phone. Their two crossings read as clasped
hands: bound, but separable — pairing is reversible by design.

Construction details (for future edits):

- viewBox `0 0 64 64`, transparent background.
- Ring radius 15, stroke width 7, centers at (24,32) and (40,32).
- The base rings are painted first; two short arc segments then repaint the
  crossings in alternating order (amber over teal on top, teal over amber on
  bottom) to produce the woven look.

## Brand colors

| Token       | Hex       | Use                          |
|-------------|-----------|------------------------------|
| deep teal   | `#0F766E` | desktop ring, weave bottom   |
| warm amber  | `#F59E0B` | phone ring, weave top        |

The amber ring is painted at opacity 0.92 so the overlap region shows a
subtle blend before the weave arcs reassert solid color.

## Bitmap export targets

Export from the SVG (never redraw by hand) for bitmap contexts:

| Size    | File name                        | Context                     |
|---------|----------------------------------|-----------------------------|
| 128 px  | `handfast-128.png`               | store listings, README      |
| 256 px  | `handfast-256.png`               | app grids, hi-DPI fallbacks |
