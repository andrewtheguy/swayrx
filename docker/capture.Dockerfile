# check=skip=InvalidDefaultArgInFrom
# The capture container: a headless sway, the daemon started with
# `--capture-frames`, and the scripted desktop `docker/capture/run.sh` plays on
# it, so that what the VP9 encoder is handed by a real session — every frame's
# pixels and rectangles, exact — is written for an encoder to be run again
# on. `scripts/capture-frames.sh` builds and runs it.
ARG BASE_IMAGE=debian:trixie
FROM ${BASE_IMAGE} AS build

ENV DEBIAN_FRONTEND=noninteractive
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl pkg-config git \
        libwayland-dev libxkbcommon-dev libpam0g-dev \
        libpipewire-0.3-dev libspa-0.2-dev libavcodec-dev libclang-dev dpkg-dev && \
    rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable && \
    cargo install cargo-deb --locked

WORKDIR /build
COPY . /build

RUN --mount=type=cache,target=/build/target --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/usr/local/cargo/git \
    set -eux; \
    cargo build --release --locked -p wlshare; \
    cargo build --release --locked -p wlshare-rfb --example vp9-sink; \
    mkdir -p /out; \
    cargo deb -p wlshare --no-build -o /out/wlshare.deb; \
    cp target/release/examples/vp9-sink /out/

FROM ${BASE_IMAGE}

ENV DEBIAN_FRONTEND=noninteractive
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# Sway 1.11 on wlroots 0.19 from the repository's own APT repository (README),
# whose headless backend keeps the cursor out of the capture as a deployment's
# does; the daemon's package depends on that wlroots. foot is the terminal,
# chromium the browser, mpv the video and wlrctl the pointer; the keys are
# typed by vp9-sink, through the daemon. bsdextrautils is `column`, which
# fills the terminals the scenarios show.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    mkdir -p /etc/apt/keyrings && \
    curl -fsSL -o /etc/apt/keyrings/wlshare.gpg https://andrewtheguy.github.io/wlshare/wlshare.gpg && \
    printf 'Types: deb\nURIs: https://andrewtheguy.github.io/wlshare\nSuites: trixie\nComponents: main\nSigned-By: /etc/apt/keyrings/wlshare.gpg\n' > /etc/apt/sources.list.d/wlshare.sources && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        sway foot chromium mpv wlrctl zstd ffmpeg procps bsdextrautils \
        fonts-dejavu fonts-noto-core && \
    rm -rf /var/lib/apt/lists/*
COPY --from=build /out/wlshare.deb /tmp/wlshare.deb
RUN apt-get update && apt-get install -y --no-install-recommends /tmp/wlshare.deb && rm -rf /var/lib/apt/lists/* /tmp/wlshare.deb
COPY --from=build /out/vp9-sink /usr/local/bin/vp9-sink
COPY docker/capture/ /opt/capture/

# Nothing here needs root. scripts/capture-frames.sh runs it as whoever owns
# the directory the captures go to, who may be nobody the image knows: hence
# a home anyone can make.
RUN useradd --create-home --uid 1000 capture && mkdir -p /captures && chown capture /captures
USER capture
ENV HOME=/tmp/home XDG_RUNTIME_DIR=/tmp/runtime WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
VOLUME /captures
ENTRYPOINT ["/opt/capture/run.sh"]
