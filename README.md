# wlshare

A VNC server for wlroots-based Wayland compositors, built for the
[remotex](https://github.com/andrewtheguy/remotex) gateway. It captures one
output through wlr-screencopy, serves it over RFB 3.8 with Raw or ZRLE — or, to
a client that asks, as remotex does for its browsers, as one VP9 stream at a
quality that follows the link — injects input through the virtual keyboard and pointer protocols,
shares the clipboard through wlr-data-control, carries the desktop's sound from
PipeWire over the connection itself — and tells the client what pixel density the
framebuffer is drawn at, which standard RFB cannot, so a `scale 2` output is
shown sharp at 2x and a client's own density becomes the output's. A desktop with
more than one monitor sends the client the list, so the one being shared is the
client's to choose. The compositor pointer is excluded from captured frames, so
wlshare captures the cursor image on its own — whatever shape the application
under it chose, at the output's pixel density — and sends it through the RFB
Cursor pseudo-encodings, with its alpha to a client that lists Cursor With
Alpha. The client moves it without waiting for a framebuffer update.

It is compositor-independent within that protocol surface: any wlroots-based
compositor exposing the required protocols is the same kind of peer.
`wlr-screencopy` version 2 or later is required, and so is `ext-image-copy-capture`
with output capture sources, which the cursor image comes from. Output resizing
and rescaling additionally need `wlr-output-management`; input and clipboard use
the virtual keyboard, virtual pointer, and `wlr-data-control` protocols when the
compositor offers them. Keeping the cursor separate on a headless output, and
capturing it, requires wlroots 0.19 or newer.

At the framebuffer layer, any RFB 3.8 client that advertises the standard Cursor
pseudo-encoding and can draw Raw rectangles can connect. ZRLE is used when
listed unless the client also asks for wlshare's private VP9 stream. A configured
login additionally requires RSA-AES. Cursor support is required because wlshare
never puts the pointer in framebuffer pixels. The density and outputs extensions
are asked for by the client and stay silent otherwise; remotex asks for both,
and for the VP9 stream, on a `vnc` target with `subtype = "wlshare"`, and reads
wlshare as any VNC server on a target without it. Audio is a private extension
that remotex asks for when such a target has `audio = true`: while it listens wlshare's own sink is the default, so every
stream that follows the default plays there rather than on the host's speakers,
and that sink's monitor is sent encoded as lossless FLAC. An application pinned
to a sink of the host's stays there and is heard on the host. A client
that does not list it hears nothing.
Audio, the camera and the microphone are each off until the configuration sets
`audio = true`, `camera = true` or `microphone = true`. A client that lists the camera extension —
remotex does, for a target with `camera = true` — can lend the desktop its camera:
the H.264 it sends is decoded with the system's libavcodec into a PipeWire video
source, "wlshare remote camera", which applications reaching cameras through
PipeWire can open, and the client is asked for frames only while one has it open.
Chrome is one only with `chrome://flags/#enable-webrtc-pipewire-camera` enabled
and an `xdg-desktop-portal` backend that implements Access, such as
`xdg-desktop-portal-gtk` — `xdg-desktop-portal-wlr` alone does not, and without
one the portal offers no Camera interface; otherwise Chrome looks only at
`/dev/video*` and lists no camera. A
client that lists the microphone extension can lend the desktop its microphone
the same way: a PipeWire audio source, "wlshare remote microphone", fed with the
16-bit PCM the client sends while an application records from it.
One client is on the desktop at a time: a
connection that finishes the handshake takes it from whoever holds it, the way
Windows Remote Desktop does, and the RFB shared flag changes nothing. See
[`docs/architecture.md`](docs/architecture.md) for how it works and what it
deliberately leaves out.

## Running

wlshare runs inside the Wayland session it captures, as the user who owns it:

```sh
WAYLAND_DISPLAY=wayland-1 wlshare --config ~/.config/wlshare/config.toml
```

or as the systemd user unit the package installs, started with the graphical
session after its environment contains `WAYLAND_DISPLAY`:

```sh
systemctl --user enable --now wlshare.service
```

Configuration is one TOML file, `$XDG_CONFIG_HOME/wlshare/config.toml` by
default. An empty file uses defaults, and
[`packaging/config.example.toml`](packaging/config.example.toml) lists every
setting.

Who may connect is one setting, and there are three answers. Nothing set: anyone
who reaches the port is in, and the session is in the clear, so `listen` stays on
loopback or behind a VPN or an SSH tunnel. `[pam]`: RSA-AES, RealVNC's security
type that TigerVNC and remotex speak — the client sends the username and
password of the account wlshare runs as, PAM checks them under the service
`wlshare` (the package installs `/etc/pam.d/wlshare`), and everything after the
key exchange is encrypted. No other account is accepted, since the desktop
behind the port is that one user's. `[password]`: the same RSA-AES, asking for a
password alone, checked against an Argon2 hash in the configuration file —
for a host where no system account's password should be the way in. Print the
hash with

```sh
wlshare hash-password
```

and paste it into the table; the password itself is never written down.

Classic VncAuth is deliberately not offered in any of the three: it names
nobody, truncates the password to eight characters, proves only knowledge of a
machine's secret, and leaves the session in the clear.
The server's RSA key is generated on first start into `rsa_key_file` and its
fingerprint logged, so it can be compared with the one the client shows.
Under `[pam]`, because the password PAM verifies is the account's, the stack can
pass it on — a `pam_exec ... expose_authtok` line there is how a headless
session gets its keyring unlocked at VNC login.

A custom keyboard layout — for a modifier remap the session's only keyboard has
to carry — goes under `[xkb]`, with `XKB_CONFIG_EXTRA_PATH` in the unit's
environment naming the directory that holds it. Each key must keep its keysym:
the server resolves the client's keysyms through the same keymap it uploads.

## Building

The workspace has two crates: `wlshare-rfb`, the protocol, which builds and
tests anywhere, and `wlshare`, the daemon, which needs libwayland,
libxkbcommon, libpipewire and libavcodec and only runs under a wlroots-based
Wayland compositor. The VP9 coding under them is
[desktop-vp9](https://github.com/andrewtheguy/desktop-vp9), shared with the
remotex gateway and pinned by its release tag. A bare `cargo test` covers the
first; build the daemon with
`cargo build --release -p wlshare` on a Linux host with `libwayland-dev`,
`libxkbcommon-dev`, `libpam0g-dev`, `libpipewire-0.3-dev`, `libspa-0.2-dev`,
`libavcodec-dev`, `libclang-dev` and `pkg-config`. libclang links nothing:
PipeWire's and FFmpeg's `-sys` crates generate their bindings with bindgen, which
loads it at build time.

Packages for Debian trixie on amd64 and arm64 are built in Docker by
`scripts/build-debs.sh`, into `dist/<arch>/wlshare-trixie-<arch>.deb`. The
**Release wlshare** workflow builds the same and publishes them as the GitHub
release `v<version>`, the version being the workspace's in `Cargo.toml`; bump it
before running the workflow. The package version is the crate's, and the
distribution is in the file name only. The package depends on
`libwlroots-0.19 (>= 0.19.0)`, the first wlroots that keeps the cursor out of a
headless capture, and on `pipewire`, `wireplumber` and `pipewire-pulse`: the
PipeWire server the speaker, the camera and the microphone live in, the session
manager that makes the speaker the default and links streams to it, and the
PulseAudio server most applications play through. Without them the desktop's
sound never reaches the speaker and the client hears nothing.

## Sway, labwc and wlroots for Debian trixie

Trixie ships Sway 1.10 and labwc 0.8.3 on wlroots 0.18, whose headless backend
paints the cursor into captures. GitHub Pages serves a signed APT repository with
Sway 1.11, labwc 0.9.7 and wlroots 0.19 rebuilt for trixie against its own
libraries. wlshare itself is not in it.

```sh
sudo mkdir -p /etc/apt/keyrings
sudo curl -fsSL -o /etc/apt/keyrings/wlshare.gpg https://andrewtheguy.github.io/wlshare/wlshare.gpg
sudo tee /etc/apt/sources.list.d/wlshare.sources <<'EOF'
Types: deb
URIs: https://andrewtheguy.github.io/wlshare
Suites: trixie
Components: main
Signed-By: /etc/apt/keyrings/wlshare.gpg
EOF
sudo apt update && sudo apt install sway  # or labwc
```

The packages are Debian's own source packages, pinned by their `.dsc` on
snapshot.debian.org in `packaging/apt/sources.env`, with the series in
`packaging/apt/patches/<source>/` applied after Debian's patches. wlroots stays
on 0.19, the newest series trixie's libdrm and wayland-protocols can build, and
labwc on 0.9, its last series built against wlroots 0.19. They
are versioned `<upstream>+<YYYYMMDD>-<N>~trixie`, above both trixie's packages
and Debian's builds of the same release. `scripts/build-sway-debs.sh` builds them
in Docker into `dist/sway/<arch>/`.

The **Release Sway packages** workflow builds both architectures and publishes
them as the prerelease `sway-<YYYYMMDD>-<N>`, which only stores them. **Publish
APT repository** runs after it: it indexes the three most recent `sway-*`
releases, signs the index with the key in `packaging/apt/pubkey.asc`, installs
Sway and labwc from the result in a trixie container, and deploys it as the whole Pages
site. It needs the private key as the `GPG_PRIVATE_KEY` secret. That key is the
one podman-package's repository uses; its private half stays in the gitignored
`keys/`. To run the assembly locally, import that key first:

```sh
gpg --import keys/apt-signing-key.private.asc
./scripts/apt-repo-build.sh dist/sway site https://andrewtheguy.github.io/wlshare
./scripts/apt-repo-smoke.sh site
```
