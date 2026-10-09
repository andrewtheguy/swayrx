#!/usr/bin/env bash
# Build and run the capture container (docker/capture.Dockerfile): a headless
# sway, the daemon with `--capture-vp9`, and the scripted desktop of
# docker/capture/run.sh, writing what the VP9 encoder was handed into a
# directory on this host, one `<scenario>-<WxH>.vp9cap.zst` per scenario
# and size, with a README.md beside them.
#
#   ./scripts/capture-vp9.sh [--screen-vp9 DIR] [--out DIR] [--sizes "..."] [--scenarios "..."] [--seconds N] [--quality Q]
#
# `--screen-vp9` names a checkout of screen-vp9 to build the daemon against
# in place of the pinned tag, for a capture format the pinned release does
# not have yet; without it the tag is built. `--out` defaults to
# dist/captures. `--scenarios` takes names from docker/capture/scenarios/
# or the groups `quiet`, `busy` and `all`; the sizes default to run.sh's.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${here}/dist/captures"
screen_vp9=""
env_args=()
while [[ $# -gt 0 ]]; do
	case "$1" in
		--screen-vp9) screen_vp9="$(cd "$2" && pwd)"; shift 2 ;;
		--out) out="$2"; shift 2 ;;
		--sizes) env_args+=(-e "SIZES=$2"); shift 2 ;;
		--scenarios) env_args+=(-e "SCENARIOS=$2"); shift 2 ;;
		--seconds) env_args+=(-e "LENGTH=$2"); shift 2 ;;
		--quality) env_args+=(-e "QUALITY=$2"); shift 2 ;;
		*) echo "$0: $1: not a flag" >&2; exit 2 ;;
	esac
done

# The Dockerfile copies the screen-vp9 context whether or not one is wanted,
# so an empty directory stands in for none; a checkout is copied without its
# build directory and its samples, which are gigabytes the build has no use for.
context="$(mktemp -d)"
trap 'rm -rf "$context"' EXIT
if [[ -n "$screen_vp9" ]]; then
	tar -C "$screen_vp9" --exclude=./target --exclude=./tmp -cf - . | tar -C "$context" -xf -
fi

docker buildx build \
	--file "${here}/docker/capture.Dockerfile" \
	--build-context "screen-vp9=${context}" \
	--load \
	--tag wlshare-capture \
	"${here}"

mkdir -p "$out"
docker run --rm --init \
	--shm-size=1g \
	-v "$(cd "$out" && pwd):/captures" \
	${env_args[@]+"${env_args[@]}"} \
	wlshare-capture
echo "== $out"
cat "$out/README.md"
ls -la "$out"
