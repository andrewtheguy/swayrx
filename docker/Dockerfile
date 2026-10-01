# check=skip=InvalidDefaultArgInFrom
ARG BASE_IMAGE=debian:trixie
FROM ${BASE_IMAGE} AS build

ENV DEBIAN_FRONTEND=noninteractive
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# The daemon links libwayland-client, libxkbcommon, libpam, libpipewire and
# libavcodec (the camera's H.264 decoder); cargo-deb reads the resulting .so
# dependencies into the package's Depends. libFLAC, the audio encoder, is loaded
# at run time and not linked, so nothing of it is installed here and the
# package's Depends names it by hand. libclang-dev is a build-time need
# with nothing to link: pipewire-sys, libspa-sys and ffmpeg-sys-next generate
# their bindings with bindgen, which loads libclang, and without it a clean
# build fails with "Unable to find libclang".
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl pkg-config \
        libwayland-dev libxkbcommon-dev libpam0g-dev \
        libpipewire-0.3-dev libspa-0.2-dev libavcodec-dev libclang-dev dpkg-dev && \
    rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable && \
    cargo install cargo-deb --locked

WORKDIR /build
COPY . /build

# The package version is the crate's, as cargo-deb writes it (<version>-1); the
# distribution the package was built for is in the file name only, so the
# release asset is wlshare-trixie-<arch>.deb whatever the version.
RUN set -eux; \
    version="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"wlshare","version":"\([^"]*\)".*/\1/p' | head -n 1)"; \
    arch="$(dpkg --print-architecture)"; \
    cargo build --release -p wlshare; \
    [ "$(target/release/wlshare --version)" = "wlshare ${version}" ]; \
    mkdir -p /out; \
    cargo deb -p wlshare --no-build -o "/out/wlshare-trixie-${arch}.deb"; \
    [ "$(dpkg-deb --field "/out/wlshare-trixie-${arch}.deb" Version)" = "${version}-1" ]; \
    cd /out && sha256sum ./*.deb | sed 's#  \./#  #' > SHA256SUMS && cat SHA256SUMS

FROM scratch
COPY --from=build /out /
