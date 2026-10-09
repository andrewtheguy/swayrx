#!/usr/bin/env bash
# Quiet: a full-screen terminal typed into one key at a time, with a few
# commands whose output scrolls it.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
material
foot >/dev/null 2>&1 &
await_window foot
swaymsg -q '[app_id="foot"] fullscreen enable'
sleep 1
type_slowly $'ls -la /usr/share/fonts/truetype/dejavu\n'
sleep 0.5
type_slowly $'for i in $(seq 1 40); do printf "%4d  %s\\n" "$i" "the quick brown fox jumps over the lazy dog"; done\n'
sleep 0.5
type_slowly "head -c 20000 $MATERIAL/page.html"$'\n'
sleep 1
type_slowly $'echo "a sentence typed one key at a time, with a few corrections and pauses"\n'
sleep 1
type_slowly $'seq 1 500 | column -c $(tput cols)\n'
until_end
close foot
