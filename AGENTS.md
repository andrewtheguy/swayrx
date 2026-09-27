# Repository instructions

- Strict no backward-compatibility or legacy paths no matter what.

- Three crates: `wlshare-rfb` (protocol) and `wlshare-client` (the session the
  macOS and Windows apps are built on), both platform-independent and tested by
  a bare `cargo test`, and `wlshare` (the daemon, Linux + wlroots-based Wayland
  only). The VP9 coding is not here: it is the `desktop-vp9` repository, shared
  with the remotex gateway and pinned by release tag, and a libvpx setting or
  the quality walk changes there and arrives as a pin bump. After Rust
  changes run `cargo test` and `cargo clippy --all-targets -- -D warnings`.
  The daemon has no cross-target check to run: nothing in it is
  architecture-specific, and the release builds each architecture in Docker on
  its own native runner. `wlshare-client` builds for the apps' targets, so after
  changing it also run `cargo clippy -p wlshare-client --all-targets --target <t>
  -- -D warnings` for `x86_64-pc-windows-msvc` and `aarch64-apple-darwin`.
  Linking the daemon's tests needs `libpam0g-dev`, and building it needs
  `libpipewire-0.3-dev`, `libspa-0.2-dev` and `libclang-dev` for the audio
  capture and the camera — the last for bindgen, which PipeWire's and FFmpeg's
  `-sys` crates run — and `libavcodec-dev` for the camera's H.264 decoder, the
  system's libavcodec linked dynamically.
- Every protocol byte comes from `wlshare-rfb`; neither the daemon nor
  `wlshare-client` writes one itself.
  Every encoder gets an independent decoder in its tests.
- Build and run the daemon on a Linux host inside the wlroots-based Wayland
  session it shares; packages are built only in Docker, by
  `scripts/build-debs.sh` for wlshare and `scripts/build-sway-debs.sh` for the
  wlroots and Sway the APT repository serves, never on the host.
- Do not run `cargo fmt`. Use `anyhow` for application errors and `thiserror` for
  typed protocol errors.
- Design and wire details live in `docs/architecture.md`, not here.
