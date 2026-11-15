# Branding

Handfast has one mark, one wordmark and one palette. They are original work,
drawn from the geometry in `scripts/branding.py`, and carry the repository
license.

## The mark

Two interlocking rings on a -18 degree axis. Teal is the desktop, copper is the
phone, and the teal ring sits in front so the pair reads as a chain link rather
than two separate circles. The name comes from handfasting, the binding of two
hands, so the mark is a binding and not a phone or a plug.

The mark has no text, works at 16 px and survives a single-color print.

## Palette

| Token | Hex | Use |
| --- | --- | --- |
| Teal | `#128C7E` | Desktop ring, primary accent |
| Copper | `#E8833A` | Phone ring, pairing and transfer accents |
| Ink | `#0B1220` | Wordmark text, dark surfaces |

Teal on white is 4.1:1. Copper on white is 2.7:1, so copper is an accent, never
body text. On the ink surface, teal is 4.5:1 and copper is 6.9:1.

## Files

`assets/branding/` holds the sources and the exports:

- `handfast-mark.svg`: color mark, transparent background.
- `handfast-mark-mono.svg`: one-color mark using `currentColor`.
- `handfast-wordmark.svg`: mark plus the `Handfast` wordmark.
- `handfast-{16,24,32,48,64,128,256,512,1024}.png`: square RGBA exports.
- `handfast.ico`: 16 to 256 px, uncompressed 32-bit entries for the MSI.
- `handfast-128.rgba`: raw RGBA for the window icon, compiled into the binary.

Regenerate them with:

```sh
python scripts/branding.py
```

The script needs only the standard library. It rasterizes the same geometry as
the SVG with 4x4 supersampling, so the PNGs and the SVG agree.

## Where it is used

- Window icon: `crates/handfast-daemon/src/gui.rs` embeds `handfast-128.rgba`.
- MSI: `packaging/windows/handfast.wxs` embeds `handfast.ico` as the product
  icon and the Start Menu shortcut icon.
- deb and Arch: `packaging/linux/make-deb.sh` and `packaging/arch/PKGBUILD`
  install the PNGs under `hicolor` and add `packaging/linux/handfast.desktop`
  with `Icon=handfast`.
- README: `handfast-mark.svg`.

## Rules

- Keep clear space of one ring stroke (50/512 of the width) on every side.
- Do not recolor the rings, rotate the mark, add a glow, or place it on a busy
  photo.
- Below 24 px use the mono mark.
- Forks that change the product name must replace the mark and wordmark.
