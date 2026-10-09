#!/usr/bin/env bash
# Busy: a floating terminal full of text dragged around the desktop,
# bouncing off its edges.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
foot -- bash -c 'seq 1 2000 | column -c 100; sleep 1000' >/dev/null 2>&1 &
await_window foot
swaymsg -q '[app_id="foot"] floating enable, resize set 800 600, move position 50 50'
sleep 1
x=50 y=50 dx=11 dy=7
for _ in $(seq $((LENGTH * 20))); do
	x=$((x + dx)); y=$((y + dy))
	if ((x < 0 || x + 800 > W)); then dx=$((-dx)); x=$((x + 2 * dx)); fi
	if ((y < 0 || y + 600 > H)); then dy=$((-dy)); y=$((y + 2 * dy)); fi
	swaymsg -q "[app_id=\"foot\"] move position $x $y"
	sleep 0.04
done
until_end
close foot
