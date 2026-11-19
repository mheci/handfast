# Temporary SSH/SFTP dependency patches

These are local copies of the published crates.io sources, patched solely to
remove the unmaintained `chrono` dependency (RUSTSEC-2026-0543). Both crates
are Apache-2.0 licensed; their licenses are included in the respective folders.

- `russh-sftp` 3.0.1, crates.io SHA-256
  `9bb17f72aba4314656b359ffba72c68dcbd70ed9e97497898b75498fe7198042`,
  upstream `AspectUnk/russh-sftp`, source revision
  `e8de8f903cb019f32beb26cc1195c7c71bef121f`: replace UTC timestamp conversion
  and longname formatting with `std::time` and `time`.
- `russh-util` 0.52.0, crates.io SHA-256
  `668424a5dde0bcb45b55ba7de8476b93831b4aa2fa6947e145f3b053e22c60b6`,
  upstream `warp-tech/russh`; the package identifies revision
  `3d09c20c52a542ffa327723d3a44cb1edd9b0ca4` as dirty, so the crates.io archive
  checksum above, not the git revision alone, identifies the original. The
  wasm32 wall clock uses `js-sys` instead of `chrono`; native code is unchanged.
  The registry archive did not include a license file; the included Apache-2.0
  text matches its package license declaration.

Only crate sources and manifest/license/readme files needed for builds are
carried. Review both forks for upstream security fixes and replace them with
reviewed maintained upstream releases as soon as both stop depending on chrono.
Never silently update these copies; record the source checksum and changes.
Issue: https://github.com/mheci/handfast/issues/274
