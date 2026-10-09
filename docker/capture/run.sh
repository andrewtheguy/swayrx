#!/usr/bin/env bash
# The capture container's run: a headless sway, the daemon started with
# `--capture-frames`, and vp9-sink connected to it once per scenario and size,
# so each capture in /captures is one session's worth of what the VP9 encoder
# was handed — the frames of one kind of content at one size — named
# `<scenario>-<WxH>[@2].wlcap.zst`. Settings come from the environment:
# SIZES (WxH, with `@2` for a desktop drawn at scale 2), SCENARIOS (names,
# or the groups `quiet`, `busy` and `all`), QUALITY (the dial, 90 by
# default), LENGTH (each scenario's seconds, 20 by default) and FRAMES (the
# least frames the flood is played for, 150 by default).
#
# The content is the scripts in scenarios/, each of which can be run by
# hand on any sway desktop a daemon is capturing; what this adds is the
# session around each — the sink's flags, which make the first one resize
# into the size, the video ask a keyframe midway, and the walk read late so
# the dial moves — and the file each session leaves.
#
#   quiet  terminal  a resize into the size, then a terminal typed into and scrolled
#          uncover   a window over a page moved aside, then closed
#   busy   flood     a terminal flooded with coloured random lines, every line new
#          browser   a long page scrolled by wheel and by key
#          drag      a floating terminal dragged across the desktop
#          video     a video playing in a window, with a keyframe asked midway
#          walk      the video while the client reads late, so the dial walks
#                    down and back up, then stopped, so the desktop settles
set -euo pipefail

# The sizes are vp9-wasm's: the nine its benchmark has a quiet and a busy
# sample of, whose names (`bench/run.sh` there) are what
# scripts/vp9-samples.sh makes of these. The five from 2048 wide are drawn at
# scale 2, as the desktops those sizes were first captured from were.
SIZES="${SIZES:-1280x800 1440x900 1600x1000 1920x1080 2048x1536@2 2560x1600@2 2880x1800@2 3456x2168@2 3840x2160@2}"
SCENARIOS="${SCENARIOS:-all}"
QUALITY="${QUALITY:-90}"
LENGTH="${LENGTH:-20}"
FRAMES="${FRAMES:-150}"
OUT=/captures
RAW="$OUT/raw"
ADDR=127.0.0.1:5900
SCENARIOS_DIR=/opt/capture/scenarios
QUIET="terminal uncover"
BUSY="flood browser drag video walk"

export CAPTURE_MATERIAL=/tmp/capture-material
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$RAW"
chmod 700 "$XDG_RUNTIME_DIR"
# What the scenarios type goes through each session's sink, which reads it
# here (scenarios/lib.sh, `press`); held open both ways, so that neither a
# sink ending nor a key written finds nobody at the other end.
export CAPTURE_KEYS="$XDG_RUNTIME_DIR/capture-keys"
mkfifo "$CAPTURE_KEYS"
exec 3<>"$CAPTURE_KEYS"

log() { printf '%s %s\n' "$(date +%T)" "$*" >&2; }

# The groups spelled out, in order, each name once.
expand() {
	local name out=""
	for name in $1; do
		case "$name" in
			all) out="$out $QUIET $BUSY" ;;
			quiet) out="$out $QUIET" ;;
			busy) out="$out $BUSY" ;;
			*) out="$out $name" ;;
		esac
	done
	echo "$out" | tr ' ' '\n' | awk 'NF && !seen[$0]++' | tr '\n' ' '
}
SCENARIOS=$(expand "$SCENARIOS")

# ── The desktop ──────────────────────────────────────────────────────────────

sway -c /opt/capture/sway.config >"$OUT/sway.log" 2>&1 &
SWAY=$!
for _ in $(seq 100); do
	sock=$(ls "$XDG_RUNTIME_DIR"/wayland-? 2>/dev/null | head -n 1 || true)
	[[ -n "$sock" ]] && break
	sleep 0.2
done
[[ -n "${sock:-}" ]] || { log "sway did not come up"; cat "$OUT/sway.log" >&2; exit 1; }
export WAYLAND_DISPLAY
WAYLAND_DISPLAY=$(basename "$sock")
export SWAYSOCK
SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.* | head -n 1)
log "sway on $WAYLAND_DISPLAY"

wlshare --config /opt/capture/wlshare.toml --capture-frames "$RAW" >"$OUT/wlshare.log" 2>&1 &
WLSHARE=$!
for _ in $(seq 100); do
	grep -q "listening on" "$OUT/wlshare.log" 2>/dev/null && break
	sleep 0.2
done
log "wlshare listening"

cleanup() {
	kill "$WLSHARE" "$SWAY" 2>/dev/null || true
}
trap cleanup EXIT

# ── The session around each scenario ─────────────────────────────────────────

# Connect vp9-sink for the scenario's length at WxH, with any further flags,
# in the background; `finish` waits for it and names the capture it made.
sink() {
	vp9-sink "$ADDR" --size "$W"x"$H" --quality "$QUALITY" --seconds "$LENGTH" --keys "$@" <&3 2>>"$OUT/sink.log" &
	SINK=$!
	# Its first frame, which opens the capture: a keyframe, then the content.
	sleep 1.5
}

# Run a scenario script for the session's length. One that fails did not
# play what its capture would be named for.
BROKEN=""
FAILED=""
play() {
	"$SCENARIOS_DIR/$1.sh" "$LENGTH" 2>>"$OUT/scenarios.log" || BROKEN="the scenario ended with $?"
}

# Wait for the sink, close every window, and move the session's capture to
# the scenario's name. A session whose scenario or sink failed leaves no
# capture, and the run ends in failure once the others are done.
finish() {
	local name=$1
	wait "$SINK" || BROKEN="vp9-sink ended with $?"
	swaymsg -q '[app_id=".*"] kill' || true
	sleep 1
	local newest
	newest=$(ls -t "$RAW"/*.wlcap 2>/dev/null | head -n 1 || true)
	[[ -n "$newest" ]] || BROKEN="${BROKEN:-no capture was written}"
	if [[ -n "$BROKEN" ]]; then
		log "$name-$LABEL: $BROKEN"
		FAILED="$FAILED $name-$LABEL"
		BROKEN=""
		[[ -z "$newest" ]] || rm -f "$newest"
		return
	fi
	mv "$newest" "$OUT/$name-$LABEL.wlcap"
	log "$name-$LABEL: $(du -h "$OUT/$name-$LABEL.wlcap" | cut -f1)"
}

scenario_terminal() {
	# Starts at the size before this one and resizes into it, so the resize
	# and the keyframe it brings are captured too.
	sink --size "$PREV" --resize 2:"$W"x"$H"
	play terminal
	finish terminal
}

scenario_uncover() {
	sink
	play uncover
	finish uncover
}

scenario_flood() {
	# Every frame of it is the whole picture new, the slowest there is to
	# code, and a large desktop gets a few a second: the session runs on
	# past its length until FRAMES have come, so that there are enough of
	# them to cut a sample from, and the flood until the session ends.
	sink --frames "$FRAMES"
	"$SCENARIOS_DIR/flood.sh" $((LENGTH * 10)) 2>>"$OUT/scenarios.log" &
	local flood=$!
	finish flood
	pkill -P "$flood" 2>/dev/null || true
	kill "$flood" 2>/dev/null || true
	wait "$flood" 2>/dev/null || true
}

scenario_browser() {
	sink
	play browser
	finish browser
}

scenario_drag() {
	sink
	play drag
	finish drag
}

scenario_video() {
	sink --keyframe 10
	play video
	finish video
}

scenario_walk() {
	# The sink reads late for six seconds, a step down the dial a second,
	# then promptly, for the steps back up; the video stops five seconds
	# before the end, so the desktop goes quiet below the ceiling and the
	# settle is captured. Best with LENGTH at 20 or more.
	sink --slow 4:6:200
	STOP_BEFORE=5 play video
	finish walk
}

# ── The run ──────────────────────────────────────────────────────────────────

PREV=1280x720
for size in $SIZES; do
	LABEL=$size
	scale=1
	if [[ "$size" == *@2 ]]; then
		scale=2
		size=${size%@2}
	fi
	W=${size%x*}
	H=${size#*x}
	swaymsg -q "output HEADLESS-1 scale $scale"
	log "== $LABEL"
	for scenario in $SCENARIOS; do
		"scenario_$scenario"
	done
	PREV=$size
done

log "compressing"
shopt -s nullglob
captures=("$OUT"/*.wlcap)
shopt -u nullglob
cp "$CAPTURE_MATERIAL"/*.log "$OUT/" 2>/dev/null || true
if [[ ${#captures[@]} -eq 0 ]]; then
	log "no captures were written"
	exit 1
fi
# Over a capture of the same name a run before left.
zstd -T0 -q -f --rm "${captures[@]}"
rmdir "$RAW" 2>/dev/null || true
{
	echo "# Frame captures"
	echo
	echo "What a wlshare session handed its VP9 encoder, exact, written by \`wlshare --capture-frames\` in the capture container (\`docker/capture/\`) on $(date -u +%Y-%m-%d): the capture file of \`crates/wlshare-rfb/src/capture.rs\`, \`zstd\` compressed. One file per scenario and size, \`<scenario>-<WxH>[@2]\`, where \`@2\` is a desktop drawn at scale 2. Quality $QUALITY, $LENGTH seconds each; the content is \`docker/capture/scenarios/\`."
	echo
	echo "Quiet:"
	echo "- terminal: a resize into the size from the size before, then a terminal typed into one key at a time and scrolled"
	echo "- uncover: a terminal over a page moved aside, then closed"
	echo
	echo "Busy:"
	echo "- flood: a terminal flooded with coloured random lines, every line new, for as long past the $LENGTH seconds as $FRAMES frames took"
	echo "- browser: a long page in Chromium scrolled by wheel, by key, and back"
	echo "- drag: a floating terminal dragged around the desktop"
	echo "- video: a test pattern playing in a 1280×720 window, with a keyframe asked at 10 s"
	echo "- walk: the video while the client reads 200 ms late from 4 s to 10 s, so the dial walks down and back up, then stopped 5 s before the end so the desktop settles"
	echo
	echo "$(sway --version), $(wlshare --version)"
} >"$OUT/README.md"
ls -la "$OUT" >&2
if [[ -n "$FAILED" ]]; then
	log "failed, and left no capture:$FAILED"
	exit 1
fi
log "done"
