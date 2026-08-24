# Handfast

Connect your Android phone and your Linux desktop. See notifications, share
files and links, use your desktop keyboard/mouse on the phone, check your
phone's battery — over your local network, no cloud involved.

Works on Wayland (and is built for it first). Compatible with the KDE Connect
Android app — install that on your phone and pair.

## What you can do

- Get phone notifications on your desktop and dismiss them (reply UI lands in Phase 3)
- See your phone's battery level
- Browse and control everything from a terminal UI (`hfctl tui`) or a desktop app
- Run quick remote commands (ping today; more landing soon)

_File sharing, link/text sharing, phone input control, two-way clipboard sync
and notification replies are under active development and arrive in Phases 3-4._

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

## Documentation

See [`docs/perf.md`](docs/perf.md) and [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Contributing

PRs welcome! Conventional commits, `cargo fmt` + `clippy -D warnings` clean.
See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE-MIT](LICENSE-MIT). Handfast speaks the KDE Connect protocol
but contains no upstream code and is not affiliated with KDE e.V.
