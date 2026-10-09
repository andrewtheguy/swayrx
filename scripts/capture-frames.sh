#!/usr/bin/env bash
# Build and run the capture container (docker/capture.Dockerfile): a headless
# sway, the daemon with `--capture-frames`, and the scripted desktop of
# docker/capture/run.sh, writing what the VP9 encoder was handed into a
# directory on this host, one `<scenario>-<WxH>.wlcap.zst` per scenario
# and size, with a README.md beside them.
#
#   ./scripts/capture-frames.sh [--out DIR] [--sizes "..."] [--scenarios "..."] [--seconds N] [--frames N] [--quality Q]
#
# `--out` defaults to dist/captures. `--scenarios` takes names from
# docker/capture/scenarios/ or the groups `quiet`, `busy` and `all`; the
# sizes default to run.sh's. The captures are the source: the streams a
# decoder is tested on are made of them by scripts/vp9-samples.sh, again
# whenever the encoder changes.
#
# The container is docker's, or podman's where there is no docker or where
# `docker` is podman under that name. A rootless podman maps the container's
# users into a range of its own, so the user the desktop runs as is kept as
# the one running this, who can then write the captures into `--out`.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${here}/dist/captures"
env_args=()
while [[ $# -gt 0 ]]; do
	case "$1" in
		--out) out="$2"; shift 2 ;;
		--sizes) env_args+=(-e "SIZES=$2"); shift 2 ;;
		--scenarios) env_args+=(-e "SCENARIOS=$2"); shift 2 ;;
		--seconds) env_args+=(-e "LENGTH=$2"); shift 2 ;;
		--frames) env_args+=(-e "FRAMES=$2"); shift 2 ;;
		--quality) env_args+=(-e "QUALITY=$2"); shift 2 ;;
		*) echo "$0: $1: not a flag" >&2; exit 2 ;;
	esac
done

engine=docker
command -v docker >/dev/null || engine=podman
command -v "$engine" >/dev/null || { echo "$0: neither docker nor podman is on the path" >&2; exit 1; }
run_args=()
if "$engine" --version | grep -qi podman && [[ "$("$engine" info --format '{{.Host.Security.Rootless}}')" == true ]]; then
	run_args+=(--userns=keep-id)
fi

"$engine" buildx build \
	--file "${here}/docker/capture.Dockerfile" \
	--load \
	--tag wlshare-capture \
	"${here}"

mkdir -p "$out"
"$engine" run --rm --init \
	--shm-size=1g \
	${run_args[@]+"${run_args[@]}"} \
	-v "$(cd "$out" && pwd):/captures" \
	${env_args[@]+"${env_args[@]}"} \
	wlshare-capture
echo "== $out"
cat "$out/README.md"
ls -la "$out"
