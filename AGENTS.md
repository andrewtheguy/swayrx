# Repository instructions

- Strict no backward-compatibility or legacy paths no matter what.

- Two crates: `wlshare-rfb` (protocol), platform-independent and tested by a
  bare `cargo test`, and `wlshare` (the daemon, Linux + wlroots-based Wayland
  only). Its one client is the remotex gateway; there is no native client. The
  VP9 coding is not here: it is the `screen-vp9` repository, shared
  with the remotex gateway and pinned by release tag, and a libvpx setting or
  the quality walk changes there and arrives as a pin bump. After Rust
  changes run `cargo test` and `cargo clippy --all-targets -- -D warnings`.
  The daemon has no cross-target check to run: nothing in it is
  architecture-specific, and the release builds each architecture in Docker on
  its own native runner.
  Linking the daemon's tests needs `libpam0g-dev`, and building it needs
  `libpipewire-0.3-dev`, `libspa-0.2-dev` and `libclang-dev` for the audio
  capture and the camera — the last for bindgen, which PipeWire's and FFmpeg's
  `-sys` crates run — and `libavcodec-dev` for the camera's H.264 decoder, the
  system's libavcodec linked dynamically.
  The audio extension's FLAC coding is not here either: it is the
  `sound-flac` repository, shared with the remotex gateway and pinned by
  release tag the same way, with libFLAC linked from a prebuilt static archive:
  nothing is needed to build or to run.
  Its Opus coding is the `sound-opus` repository, shared and pinned the same
  way, with libopus linked from a prebuilt static archive too: nothing is needed
  to build or to run, and `ffmpeg` on the path to run the protocol crate's Opus
  tests, which decode with it.
- Every protocol byte comes from `wlshare-rfb`; the daemon writes none itself.
  Every encoder gets an independent decoder in its tests.
- Build and run the daemon on a Linux host inside the wlroots-based Wayland
  session it shares; packages are built only in Docker, by
  `scripts/build-debs.sh` for wlshare and `scripts/build-sway-debs.sh` for the
  wlroots and Sway the APT repository serves, never on the host.
- Do not run `cargo fmt`. Use `anyhow` for application errors and `thiserror` for
  typed protocol errors.
- Design and wire details live in `docs/architecture.md`, not here.
