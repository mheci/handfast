# Handfast

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
[![Latest release](https://img.shields.io/github/v/release/mheci/handfast)](https://github.com/mheci/handfast/releases/latest)

Connect your Android phone to your Linux desktop over your local network. No cloud, no account, no phone vendor needed. Handfast speaks the KDE Connect protocol, so it pairs with the standard KDE Connect Android app.

Built Wayland-first. Ships as three binaries: `handfastd` (the daemon), `hfctl` (terminal UI), and `handfast-gui` (desktop app).

> Personal project, experimental. Anything can change or break without notice.

## What you can do

- See and dismiss phone notifications on your desktop
- Check your phone's battery level
- Control everything from a terminal UI (`hfctl tui`) or a desktop app
- Run quick remote commands (ping today, more arriving)

_File sharing, link and text sharing, phone input control, two-way clipboard sync, and notification replies are under active development (Phases 3-4)._

## Install

### Arch Linux — from a prebuilt package

Grab `handfast-<version>-x86_64.pkg.tar.zst` from the
[latest release](https://github.com/mheci/handfast/releases/latest), then:

```sh
pacman -U handfast-*-x86_64.pkg.tar.zst
```

### Arch Linux — build it yourself

Use the shipped `PKGBUILD` (also attached to every release):

```sh
curl -LO https://github.com/mheci/handfast/releases/latest/download/PKGBUILD
makepkg -si
```

### Any distro — portable binaries

Download `handfast-*.tar.zst` from the
[latest release](https://github.com/mheci/handfast/releases/latest), unpack,
and run `handfastd` / `hfctl` / `handfast-gui` from wherever you like.

### From source

You need a Rust toolchain plus the development headers for Wayland, xkbcommon, and SQLite (Debian/Ubuntu: `libwayland-dev libxkbcommon-dev libsqlite3-dev`).

```sh
git clone https://github.com/mheci/handfast && cd handfast
cargo build --release --locked --all-features \
  -p handfastd -p hfctl -p handfast-gui
```

Binaries end up in `target/release/`.

## Getting started

1. Install **KDE Connect** on your Android phone.
2. Start the daemon:

   ```sh
   systemctl --user enable --now handfast.service   # packaged install
   # or directly: ./handfastd &
   ```

3. Make sure both devices are on the same Wi-Fi network.
4. Pair from either side:

   ```sh
   hfctl devices        # your phone shows up here
   hfctl pair <device-id>
   ```

5. Open the terminal UI any time with `hfctl tui`, or launch `handfast-gui`.

Need help reading logs? Set `RUST_LOG=debug` before starting the daemon.

## Contributing

PRs welcome. Conventional commits, `cargo fmt` + `clippy -D warnings` clean.
Found a bug or want a feature? Open an [issue](https://github.com/mheci/handfast/issues).

## License

MIT — see [LICENSE-MIT](LICENSE-MIT). Handfast speaks the KDE Connect protocol
but contains no upstream code and is not affiliated with KDE e.V.
