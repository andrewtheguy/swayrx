#!/usr/bin/env bash
# Make the VP9 streams a decoder is tested and timed on from the captures
# scripts/capture-frames.sh left: each capture played into the encoder as it is
# pinned now (crates/wlshare-rfb/examples/vp9cap.rs), so that a change to how
# a desktop is coded — a pin bump of screen-vp9-native — is one run of this and no
# desktop has to be played again.
#
#   ./scripts/vp9-samples.sh [--captures DIR] [--out DIR] [--streams DIR] [--clean]
#                            [--frames N] [--threads N] [--quiet NAME] [--busy NAME]
#
# For every size the captures in `--captures` (dist/captures) have, `--out`
# (dist/vp9-bench) gets a quiet and a busy sample, `--frames` (120) frames
# each: the quiet one the start of the `--quiet` scenario's capture
# (terminal), the busy one the frames in a row of the `--busy` scenario's
# (flood) that said the most pixels changed. They are named as vp9-wasm's
# benchmark names them, `<quiet|busy>-<WxH>-<tile columns>col-<lf|nolf>`,
# the last two read from the stream itself. With `--streams DIR`, every
# capture is also coded whole, one stream per size it ran at for thirty
# frames or more, `<scenario>-<WxH>-444.ivf` with its `.ivf.csv`.
#
# Beside every `.ivf` is its `.ivf.framemd5`, the MD5 of each decoded frame
# as libvpx decodes it, written by ffmpeg, and only where ffmpeg's own VP9
# decoder gives the same: a stream the two disagree on stops the run. A
# README.md in each directory says what this run put there. The directories
# are made where they are not there, and nothing in them is removed: a stream
# of the same name is written over, and one this run does not make — of a
# size no longer captured, or under the name an older encoder's tile columns
# gave it — stays, unless `--clean` is given, which first empties both of the
# streams they hold. Needs cargo, zstd and an
# ffmpeg with libvpx on the path. `--threads` (4) is the encoder's, which the
# tile columns follow: keep it for samples that are to compare with the last.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
captures="${here}/dist/captures"
out="${here}/dist/vp9-bench"
streams=""
clean=""
frames=120
threads=4
quiet=terminal
busy=flood
while [[ $# -gt 0 ]]; do
	case "$1" in
		--captures) captures="$2"; shift 2 ;;
		--out) out="$2"; shift 2 ;;
		--streams) streams="$2"; shift 2 ;;
		--clean) clean=1; shift ;;
		--frames) frames="$2"; shift 2 ;;
		--threads) threads="$2"; shift 2 ;;
		--quiet) quiet="$2"; shift 2 ;;
		--busy) busy="$2"; shift 2 ;;
		*) echo "$0: $1: not a flag" >&2; exit 2 ;;
	esac
done

log() { printf '%s %s\n' "$(date +%T)" "$*" >&2; }

for tool in zstd ffmpeg; do
	command -v "$tool" >/dev/null || { echo "$0: $tool is not on the path" >&2; exit 1; }
done
shopt -s nullglob
quiet_captures=("$captures/$quiet"-*.wlcap.zst)
all_captures=("$captures"/*.wlcap.zst)
shopt -u nullglob
[[ ${#quiet_captures[@]} -gt 0 ]] || { echo "$0: no $quiet-*.wlcap.zst in $captures: run scripts/capture-frames.sh" >&2; exit 1; }

cargo build --release --manifest-path "${here}/Cargo.toml" -p wlshare-rfb --example vp9cap
vp9cap="${here}/target/release/examples/vp9cap"
coded_by="$(cargo metadata --manifest-path "${here}/Cargo.toml" --format-version 1 --locked | tr '{' '\n' | sed -n 's/.*"name":"screen-vp9-native","version":"\([^"]*\)".*/screen-vp9-native \1/p' | head -n 1)"

# The MD5 of every frame of the streams named, as libvpx decodes them, and
# the same from ffmpeg's own decoder, which shares no code with it.
framemd5() {
	local ivf
	for ivf in "$@"; do
		ffmpeg -nostdin -hide_banner -loglevel error -y -c:v libvpx-vp9 -i "$ivf" -fps_mode passthrough -f framemd5 "$ivf.framemd5"
		if ! ffmpeg -nostdin -hide_banner -loglevel error -c:v vp9 -i "$ivf" -fps_mode passthrough -f framemd5 - | cmp -s - "$ivf.framemd5"; then
			echo "$0: libvpx and ffmpeg's VP9 decoder make different pictures of $ivf" >&2
			exit 1
		fi
	done
}

# The label of a capture, `WxH` or `WxH@2`, from its path.
label() {
	local name
	name="$(basename "$1" .wlcap.zst)"
	echo "${name#*-}"
}

# ── The samples ──────────────────────────────────────────────────────────────

mkdir -p "$out"
[[ -z "$clean" ]] || rm -f "$out"/*.ivf "$out"/*.ivf.csv "$out"/*.ivf.framemd5
rows=""
# One sample: its kind, its capture and any further flags.
sample() {
	local kind=$1 capture=$2 ivf said
	shift 2
	said="$(mktemp)"
	ivf="$("$vp9cap" sample "$capture" --dir "$out" --name "$kind" --frames "$frames" --threads "$threads" "$@" 2>"$said")" || { cat "$said" >&2; rm -f "$said"; exit 1; }
	# `vp9cap: <kind>: frames A..B of N`
	local window
	window="$(sed -n 's/.*: frames \([0-9]*\)\.\.\([0-9]*\) of .*/\1–\2/p' "$said")"
	rm -f "$said" "$ivf.csv"
	framemd5 "$ivf"
	local name shape
	name="$(basename "$ivf" .ivf)"
	shape="${name#*-*-}"
	rows+="| \`$name\` | \`$(basename "$capture")\` | $window | ${shape%%col-*} | $([[ "$shape" == *-nolf ]] && echo off || echo on) | $(du -h "$ivf" | cut -f1) |"$'\n'
	log "$name"
}
for capture in "${quiet_captures[@]}"; do
	sample quiet "$capture"
done
for capture in "${quiet_captures[@]}"; do
	other="$captures/$busy-$(label "$capture").wlcap.zst"
	[[ -f "$other" ]] || { echo "$0: $(basename "$capture") has no $(basename "$other") beside it" >&2; exit 1; }
	sample busy "$other" --busiest
done
{
	echo "# Benchmark samples"
	echo
	echo "A quiet and a busy 4:4:4 stream of each size captured, $frames frames each, for vp9-wasm's routine benchmark (\`bench/run.sh\` in that repository). Each is cut from a lossless capture of what a wlshare session handed its VP9 encoder, played into the encoder again by \`scripts/vp9-samples.sh\` in the wlshare repository on $(date -u +%Y-%m-%d): the frames named, the first a keyframe of the picture as it stood and the rest as the session coded them, with their rectangles and their dial, by $coded_by on $threads threads."
	echo
	echo "| Sample | From capture | Frames | Tile columns | Loop filter | Size |"
	echo "|---|---|---|---|---|---|"
	printf '%s' "$rows"
	echo
	echo "The quiet ones are the start of the \`$quiet\` capture at its size; the busy ones are the $frames frames in a row of the \`$busy\` capture whose frames said the most pixels changed, the earliest where several tie. What the scenarios are is in the README.md beside the captures."
	echo
	echo "Each \`.ivf\` has its \`.ivf.framemd5\`: the MD5 of each decoded frame in ffmpeg's \`framemd5\` format, a frame hashed as its Y plane, then U, then V, each row at the plane's width. They were written from libvpx, and ffmpeg's own VP9 decoder gave the same for every stream. The time of each frame in the IVF is when the session's encoder was handed it, in milliseconds from the sample's first."
} >"$out/README.md"
log "== $out"

# ── Every capture whole ──────────────────────────────────────────────────────

if [[ -n "$streams" ]]; then
	mkdir -p "$streams"
	[[ -z "$clean" ]] || rm -f "$streams"/*.ivf "$streams"/*.ivf.csv "$streams"/*.ivf.framemd5
	rows=""
	for capture in "${all_captures[@]}"; do
		scenario="$(basename "$capture" .wlcap.zst)"
		scenario="${scenario%%-*}"
		# Before any of them is read, so that a capture it failed on stops the run.
		made="$("$vp9cap" streams "$capture" --dir "$streams" --name "$scenario" --least 30 --threads "$threads")"
		while read -r ivf; do
			[[ -n "$ivf" ]] || continue
			framemd5 "$ivf"
			count=$(($(wc -l <"$ivf.csv") - 1))
			keyframes=$(awk -F, 'NR > 1 && $4 == 1' "$ivf.csv" | wc -l)
			dial="$(awk -F, 'NR > 1 { if (low == "" || $5 < low) low = $5; if ($5 > high) high = $5 } END { print (low == high ? low : low "–" high) }' "$ivf.csv")"
			seconds="$(awk -F, 'END { printf "%.1f", $2 / 1000 }' "$ivf.csv")"
			rows+="| \`$(basename "$ivf" .ivf)\` | \`$(basename "$capture")\` | $("$vp9cap" shape "$ivf") | $count | $keyframes | $dial | $seconds | $(du -h "$ivf" | cut -f1) |"$'\n'
			log "$(basename "$ivf" .ivf)"
		done <<<"$made"
	done
	{
		echo "# VP9 streams"
		echo
		echo "Every capture of what a wlshare session handed its VP9 encoder, coded whole and as the session coded it — each frame's rectangles, its dial, a keyframe where one was asked — by \`scripts/vp9-samples.sh\` in the wlshare repository on $(date -u +%Y-%m-%d), with $coded_by on $threads threads. One 4:4:4 stream per size a capture ran at for thirty frames or more, so each is one size throughout and starts with a keyframe. What the scenarios are is in the README.md beside the captures."
		echo
		echo "| Stream | From capture | Shape | Frames | Keyframes | Quality | Seconds | Size |"
		echo "|---|---|---|---|---|---|---|---|"
		printf '%s' "$rows"
		echo
		echo "- \`<stream>.ivf\`: the stream, in IVF: a 32-byte header, then for each frame a \`u32\` length and a \`u64\` timestamp in milliseconds, little-endian, and the frame."
		echo "- \`<stream>.ivf.csv\`: \`frame,ms,bytes,keyframe,quality\`: when the session's encoder was handed the frame, counted from the stream's first, its bytes, whether it is a keyframe, and the 1–100 dial it was coded at."
		echo "- \`<stream>.ivf.framemd5\`: the MD5 of each decoded frame in ffmpeg's \`framemd5\` format, a frame hashed as its Y plane, then U, then V, each row at the plane's width. Written from libvpx; ffmpeg's own VP9 decoder gave the same for every stream."
	} >"$streams/README.md"
	log "== $streams"
fi
