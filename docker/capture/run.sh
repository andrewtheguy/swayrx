#!/usr/bin/env bash
# The scripted desktop the capture container plays: a headless sway, the
# daemon started with `--capture-vp9`, and vp9-sink connected to it once per
# scenario and size, so each capture in /captures is one session's worth of
# what the VP9 encoder was handed — the frames of one kind of content at one
# size — named `<scenario>-<WxH>[@2].vp9cap.zst`. Settings come from the
# environment: SIZES (WxH, with `@2` for a desktop drawn at scale 2),
# SCENARIOS, QUALITY (the dial, 90 by default) and LENGTH (each scenario's
# seconds, 20 by default).
#
# The scenarios, each at every size:
#   terminal  a resize into the size, then a terminal typed into and scrolled
#   busy      a terminal flooded with coloured random lines, every line new
#   browser   a long page scrolled by wheel and by key
#   drag      a floating terminal dragged across the desktop
#   uncover   a window over a page moved away, then closed
#   video     a video playing in a window, with a keyframe asked midway
#   walk      the video while the client reads late, so the dial walks down
#             and back up, then stopped, so the desktop settles
set -euo pipefail

SIZES="${SIZES:-1280x800 1920x1080 2560x1440 3840x2160 2048x1536@2 2880x1800@2 3456x2168@2}"
SCENARIOS="${SCENARIOS:-terminal busy browser drag uncover video walk}"
QUALITY="${QUALITY:-90}"
LENGTH="${LENGTH:-20}"
OUT=/captures
RAW="$OUT/raw"
ADDR=127.0.0.1:5900

mkdir -p "$XDG_RUNTIME_DIR" "$RAW"
chmod 700 "$XDG_RUNTIME_DIR"

log() { printf '%s %s\n' "$(date +%T)" "$*" >&2; }

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

wlshare --config /opt/capture/wlshare.toml --capture-vp9 "$RAW" >"$OUT/wlshare.log" 2>&1 &
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

# ── Material ─────────────────────────────────────────────────────────────────

# Twenty seconds of a moving test pattern, for the video scenarios.
ffmpeg -loglevel error -y -f lavfi -i testsrc2=size=1280x720:rate=30 -t 20 -c:v libx264 -preset veryfast -pix_fmt yuv420p /tmp/video.mp4

# A long page: headings, paragraphs, a table and coloured boxes, as a
# document a browser scrolls.
{
	echo '<!doctype html><html><head><meta charset="utf-8"><title>page</title><style>body{font-family:"DejaVu Serif",serif;font-size:17px;line-height:1.5;max-width:70%;margin:40px auto;color:#222;background:#fff}h2{font-family:"DejaVu Sans",sans-serif}table{border-collapse:collapse;margin:1em 0}td,th{border:1px solid #999;padding:4px 10px}.box{display:inline-block;width:120px;height:60px;margin:4px}</style></head><body>'
	for i in $(seq 1 60); do
		echo "<h2>Section $i: the quick brown fox jumps over the lazy dog</h2>"
		for _ in 1 2 3; do
			echo "<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum. <code>fn main() { println!(\"$i\"); }</code></p>"
		done
		echo "<table><tr><th>name</th><th>bytes</th><th>ms</th><th>dB</th></tr>"
		for r in 1 2 3 4; do echo "<tr><td>sample-$i-$r</td><td>$((i * 7919 + r * 131))</td><td>$((i % 23)).$r</td><td>4$r.$i</td></tr>"; done
		echo "</table>"
		for c in 1 2 3 4 5 6; do echo "<div class=box style=\"background:hsl($(((i * 37 + c * 61) % 360)),70%,55%)\"></div>"; done
	done
	echo '</body></html>'
} >/tmp/page.html

# The flooded terminal: every line a random colour and random characters, as
# fast as the terminal draws them.
cat >/tmp/busy.sh <<'BUSY'
#!/usr/bin/env bash
while true; do
  cols=$(tput cols)
  color=$((RANDOM%256))
  printf '\033[38;5;%sm' "$color"
  LC_ALL=C tr -dc 'A-Za-z0-9@#$%&*+=:;!?' </dev/urandom | head -c "$((cols-2))"
  printf '\033[0m\n'
done
BUSY
chmod +x /tmp/busy.sh

# ── Helpers ──────────────────────────────────────────────────────────────────

# Connect vp9-sink for the scenario's length at WxH, with any further flags,
# in the background; `finish` waits for it and names the capture it made.
sink() {
	vp9-sink "$ADDR" --size "$W"x"$H" --quality "$QUALITY" --seconds "$LENGTH" "$@" 2>>"$OUT/sink.log" &
	SINK=$!
	# Its first frame, which opens the capture: a keyframe, then the content.
	sleep 1.5
}

# Wait for the sink, close every window, and move the session's capture to
# the scenario's name.
finish() {
	local name=$1
	wait "$SINK" || log "vp9-sink ended with $?"
	swaymsg -q '[app_id=".*"] kill' || true
	pkill -f /tmp/busy.sh || true
	sleep 1
	local newest
	newest=$(ls -t "$RAW"/*.vp9cap 2>/dev/null | head -n 1 || true)
	if [[ -z "$newest" ]]; then
		log "$name-$LABEL: no capture was written"
		return
	fi
	mv "$newest" "$OUT/$name-$LABEL.vp9cap"
	log "$name-$LABEL: $(du -h "$OUT/$name-$LABEL.vp9cap" | cut -f1)"
}

# Type text a character at a time, as a person does.
type_slowly() {
	local text=$1 i
	for ((i = 0; i < ${#text}; i++)); do
		local c=${text:i:1}
		if [[ "$c" == $'\n' ]]; then wtype -k Return; else wtype -- "$c"; fi
		sleep 0.05
	done
}

# A window matching the criteria, within a few seconds.
await_window() {
	for _ in $(seq 50); do
		swaymsg -t get_tree | grep -q "\"app_id\": \"$1" && return 0
		sleep 0.2
	done
	log "no window $1 appeared"
	return 1
}

# ── Scenarios ────────────────────────────────────────────────────────────────

scenario_terminal() {
	# Starts at the size before this one and resizes into it, so the resize
	# and the keyframe it brings are captured too.
	sink --size "$PREV" --resize 2:"$W"x"$H"
	foot >/dev/null 2>&1 &
	await_window foot
	swaymsg -q '[app_id="foot"] fullscreen enable'
	sleep 1
	type_slowly $'ls -la /usr/share/fonts/truetype/dejavu\n'
	sleep 0.5
	type_slowly $'for i in $(seq 1 40); do printf "%4d  %s\\n" "$i" "the quick brown fox jumps over the lazy dog"; done\n'
	sleep 0.5
	type_slowly $'cat /tmp/page.html | head -c 20000\n'
	sleep 1
	type_slowly $'echo "a sentence typed one key at a time, with a few corrections and pauses"\n'
	sleep 1
	type_slowly $'seq 1 500 | column -c $(tput cols)\n'
	finish terminal
}

scenario_busy() {
	sink
	foot -- /tmp/busy.sh >/dev/null 2>&1 &
	await_window foot
	swaymsg -q '[app_id="foot"] fullscreen enable'
	finish busy
}

scenario_browser() {
	sink
	chromium --ozone-platform=wayland --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --password-store=basic --kiosk file:///tmp/page.html >/dev/null 2>&1 &
	await_window chromium
	sleep 4
	wlrctl pointer move $((W / 2)) $((H / 2))
	for _ in $(seq 40); do
		wlrctl pointer scroll 15 0
		sleep 0.1
	done
	sleep 1
	for _ in $(seq 10); do
		wtype -k Page_Down
		sleep 0.3
	done
	sleep 1
	for _ in $(seq 40); do
		wlrctl pointer scroll -15 0
		sleep 0.05
	done
	finish browser
}

scenario_drag() {
	sink
	foot -- bash -c 'seq 1 2000 | column -c 100; sleep 1000' >/dev/null 2>&1 &
	await_window foot
	swaymsg -q '[app_id="foot"] floating enable, resize set 800 600, move position 50 50'
	sleep 1
	local x=50 y=50 dx=11 dy=7
	for _ in $(seq 300); do
		x=$((x + dx)); y=$((y + dy))
		if ((x < 0 || x + 800 > W)); then dx=$((-dx)); x=$((x + 2 * dx)); fi
		if ((y < 0 || y + 600 > H)); then dy=$((-dy)); y=$((y + 2 * dy)); fi
		swaymsg -q "[app_id=\"foot\"] move position $x $y"
		sleep 0.04
	done
	finish drag
}

scenario_uncover() {
	sink
	chromium --ozone-platform=wayland --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --password-store=basic --kiosk file:///tmp/page.html >/dev/null 2>&1 &
	await_window chromium
	sleep 4
	foot -- bash -c 'seq 1 2000 | column -c 100; sleep 1000' >/dev/null 2>&1 &
	await_window foot
	swaymsg -q '[app_id="foot"] floating enable, resize set 900 700, move position 100 100'
	sleep 2
	local x
	for x in $(seq 100 20 1000); do
		swaymsg -q "[app_id=\"foot\"] move position $x 100"
		sleep 0.1
	done
	sleep 2
	swaymsg -q '[app_id="foot"] kill'
	sleep 2
	finish uncover
}

scenario_video() {
	sink --keyframe 10
	mpv --vo=wlshm --hwdec=no --no-audio --loop=inf --no-border /tmp/video.mp4 >/dev/null 2>&1 &
	await_window mpv
	swaymsg -q '[app_id="mpv"] floating enable, resize set 1280 720, move position 100 100'
	finish video
}

scenario_walk() {
	# The sink reads late for six seconds, a step down the dial a second,
	# then promptly, for the steps back up; the video stops five seconds
	# before the end, so the desktop goes quiet below the ceiling and the
	# settle is captured. Best with LENGTH at 20 or more.
	sink --slow 4:6:200
	mpv --vo=wlshm --hwdec=no --no-audio --loop=inf --no-border /tmp/video.mp4 >/dev/null 2>&1 &
	await_window mpv
	swaymsg -q '[app_id="mpv"] floating enable, resize set 1280 720, move position 100 100'
	sleep $((LENGTH > 11 ? LENGTH - 6 : 5))
	swaymsg -q '[app_id="mpv"] kill'
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
captures=("$OUT"/*.vp9cap)
shopt -u nullglob
if [[ ${#captures[@]} -eq 0 ]]; then
	log "no captures were written"
	exit 1
fi
zstd -T0 -q --rm "${captures[@]}"
rmdir "$RAW" 2>/dev/null || true
{
	echo "# VP9 captures"
	echo
	echo "What a wlshare session handed its VP9 encoder, exact, written by \`wlshare --capture-vp9\` in the capture container (\`docker/capture/\`) on $(date -u +%Y-%m-%d): screen-vp9's capture format, \`zstd\` compressed. One file per scenario and size, \`<scenario>-<WxH>[@2]\`, where \`@2\` is a desktop drawn at scale 2. Quality $QUALITY, $LENGTH seconds each."
	echo
	echo "- terminal: a resize into the size from the size before, then a terminal typed into one key at a time and scrolled"
	echo "- busy: a terminal flooded with coloured random lines, every line new"
	echo "- browser: a long page in Chromium scrolled by wheel, by key, and back"
	echo "- drag: a floating terminal dragged around the desktop"
	echo "- uncover: a terminal over a page moved aside, then closed"
	echo "- video: a test pattern playing in a 1280×720 window, with a keyframe asked at 10 s"
	echo "- walk: the video while the client reads 200 ms late from 4 s to 10 s, so the dial walks down and back up, then stopped 5 s before the end so the desktop settles"
	echo
	echo "$(sway --version), $(wlshare --version)"
} >"$OUT/README.md"
log "done"
ls -la "$OUT" >&2
