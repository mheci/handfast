# Releasing

Releases are cut from a tag. The tag must match the workspace version in
`Cargo.toml`, for example `v0.2.0`.

## Assets

| Platform | Asset | Build host |
| --- | --- | --- |
| Debian 12, amd64 | `handfastd_<version>_debian12_amd64.deb` | `rust:1.90-bookworm` |
| Ubuntu 24.04, amd64 | `handfastd_<version>_ubuntu24.04_amd64.deb` | `ubuntu-24.04` |
| Arch Linux, x86_64 | `handfastd-bin-<pkgver>-1-x86_64.pkg.tar.zst` | `archlinux:latest` |
| Windows 11, x64 | `handfast-<msi-version>-x64.msi` | `windows-latest` |

The Debian package version replaces `-` with `~`. The Arch package version
replaces `-` with `_`. The MSI version drops any pre-release suffix. For
`0.2.0` all four assets use `0.2.0`.

## Signing secrets

The Windows job signs both `handfastd.exe` and the MSI when both secrets are
present, then runs `signtool verify /pa /all`. Without them it builds an
unsigned MSI and the release still includes it. Set both secrets for a signed
release. The 0.2.0 release is unsigned.

| Secret | Value |
| --- | --- |
| `WINDOWS_CERT_PFX_BASE64` | Base64 of a code-signing PFX with the private key and certificate chain |
| `WINDOWS_CERT_PASSWORD` | Password for the PFX |

The certificate must chain to an authority that Windows trusts. A self-signed
certificate will fail verification in CI.

Create the base64 value on Windows:

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes("handfast.pfx"))
```

Or on Linux:

```sh
base64 -w0 handfast.pfx
```

Store the single-line result in `WINDOWS_CERT_PFX_BASE64`. The PFX is written
only to the runner's temporary directory and is removed by the `always()` step.

## Cut a release

1. Update `[workspace.package] version` in `Cargo.toml`.
2. Refresh `Cargo.lock`, then run the normal local checks.
3. Commit and push. Wait for CI on `main` to pass.
4. Tag the commit and push the tag:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

5. The Release workflow checks the tag against `Cargo.toml`, builds the four
   packages, signs the Windows files when the secrets are present and attaches
   every asset to the GitHub release.

A manual `workflow_dispatch` run builds and uploads run artifacts. Set
`sign_windows` to false to build an unsigned MSI without the signing secrets.
A tag push is also unsigned when the secrets are absent. The publish job only
runs on a tag push, so a manual run never publishes.

## Install

Debian and Ubuntu:

```sh
sudo apt install ./handfastd_0.2.0_debian12_amd64.deb
```

Arch Linux:

```sh
sudo pacman -U handfastd-bin-0.2.0-1-x86_64.pkg.tar.zst
```

Windows 11: open the MSI. It installs `handfastd.exe` to
`C:\Program Files\Handfast`. The daemon writes state to
`%LOCALAPPDATA%\handfast`.

The daemon is still P0. The package installs the binary; there is no service
unit or Windows service yet.
