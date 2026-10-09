#!/usr/bin/env bash
# What the scenario scripts share: the desktop's size, the material they
# show, and the few actions they are made of. Sourced, not run.
#
# A scenario script plays one kind of content on the sway desktop it is run
# in, for LENGTH seconds (its one argument, 20 by default), and closes what
# it opened. Run one by hand on any sway desktop a daemon is capturing —
# `wlshare --capture-frames DIR` — or let docker/capture/run.sh run them all.
# They need swaymsg, wlrctl (the pointer), foot, chromium, mpv and ffmpeg,
# and for the keys either CAPTURE_KEYS or wtype (`press`, below).
#
#   quiet: terminal.sh, uncover.sh    little changes a frame, most of it still
#   busy:  flood.sh, browser.sh, drag.sh, video.sh    most of the picture new
#
# busy-lines.sh is the flood itself, for any terminal.

set -euo pipefail

LENGTH="${1:-20}"
# Where the page and the video are made, once.
MATERIAL="${CAPTURE_MATERIAL:-${XDG_RUNTIME_DIR:-/tmp}/capture-material}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log() { printf '%s %s\n' "$(date +%T)" "$*" >&2; }

# The desktop's size as sway lays it out, from the output's rectangle: the
# units windows are placed and the pointer moved in, which at scale 2 are
# half the mode's pixels.
desktop_size() {
	local rect
	rect=$(swaymsg -t get_outputs | tr -d ' \n' | sed -n 's/.*"rect":{"x":-\{0,1\}[0-9]*,"y":-\{0,1\}[0-9]*,"width":\([0-9]*\),"height":\([0-9]*\).*/\1 \2/p')
	W=${rect% *}
	H=${rect#* }
	[[ -n "$W" && -n "$H" ]] || { log "no output size from swaymsg"; exit 1; }
}

# The page and the video, made where MATERIAL says if they are not there.
material() {
	mkdir -p "$MATERIAL"
	if [[ ! -s "$MATERIAL/video.mp4" ]]; then
		# Twenty seconds of a moving test pattern.
		ffmpeg -loglevel error -y -f lavfi -i testsrc2=size=1280x720:rate=30 -t 20 -c:v libx264 -preset veryfast -pix_fmt yuv420p "$MATERIAL/video.mp4"
	fi
	if [[ ! -s "$MATERIAL/page.html" ]]; then
		# A long page: headings, paragraphs, a table and coloured boxes.
		{
			echo '<!doctype html><html><head><meta charset="utf-8"><title>page</title><style>body{font-family:"DejaVu Serif",serif;font-size:17px;line-height:1.5;max-width:70%;margin:40px auto;color:#222;background:#fff}h2{font-family:"DejaVu Sans",sans-serif}table{border-collapse:collapse;margin:1em 0}td,th{border:1px solid #999;padding:4px 10px}.box{display:inline-block;width:120px;height:60px;margin:4px}</style></head><body>'
			local i r c
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
		} >"$MATERIAL/page.html"
	fi
}

# Press and let go of one key: a character, or `-k` and Return or Page_Down.
# Through the session's client where CAPTURE_KEYS names what it reads keys
# from (`vp9-sink --keys`: a keysym in hexadecimal a line), so that they come
# as a person's do, on the daemon's keyboard; by wtype otherwise. Not by both
# on one desktop: wtype's keyboard is gone when it exits, and with the
# daemon's beside it, which has pressed nothing, sway is left with a seat
# that has a keyboard and no keymap, which Chromium does not survive starting
# on.
press() {
	if [[ -z "${CAPTURE_KEYS:-}" ]]; then
		if [[ "$1" == -k ]]; then wtype -k "$2"; else wtype -- "$1"; fi
		return
	fi
	local keysym
	case "$1 ${2:-}" in
		"-k Return") keysym=ff0d ;;
		"-k Page_Down") keysym=ff56 ;;
		-k*) log "press: no keysym for $2"; return 1 ;;
		*) keysym=$(printf '%x' "'$1") ;;
	esac
	echo "$keysym" >"$CAPTURE_KEYS"
}

# Type text a character at a time, as a person does.
type_slowly() {
	local text=$1 i c
	for ((i = 0; i < ${#text}; i++)); do
		c=${text:i:1}
		if [[ "$c" == $'\n' ]]; then press -k Return; else press "$c"; fi
		sleep 0.05
	done
}

# Wait for a window whose app_id starts with the argument.
await_window() {
	local _
	for _ in $(seq 50); do
		swaymsg -t get_tree | grep -q "\"app_id\": \"$1" && return 0
		sleep 0.2
	done
	log "no window $1 appeared"
	return 1
}

# The page in a kiosk Chromium, software rendered. Its first start in a
# fresh profile sometimes dies at once, so it is started again when no
# window comes; what it says goes to chromium.log beside the material.
browser() {
	local _
	for _ in 1 2 3; do
		chromium --ozone-platform=wayland --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --password-store=basic --kiosk "file://$MATERIAL/page.html" >>"$MATERIAL/chromium.log" 2>&1 &
		if await_window chromium; then
			sleep 4
			return 0
		fi
	done
	return 1
}

# The video in a floating 1280×720 window, software rendered.
video() {
	mpv --vo=wlshm --hwdec=no --no-audio --loop=inf --no-border "$MATERIAL/video.mp4" >/dev/null 2>&1 &
	await_window mpv
	swaymsg -q '[app_id="mpv"] floating enable, resize set 1280 720, move position 100 100'
}

# Close the windows a scenario opened: the app ids it names.
close() {
	local app
	for app in "$@"; do
		swaymsg -q "[app_id=\"$app\"] kill" 2>/dev/null || true
	done
	pkill -f busy-lines.sh 2>/dev/null || true
}

# The scenario's clock: sleep until its LENGTH is up, from its start.
STARTED=$(date +%s)
until_end() {
	local now
	now=$(date +%s)
	if ((STARTED + LENGTH > now)); then
		sleep $((STARTED + LENGTH - now))
	fi
}

desktop_size
