#!/usr/bin/env bash
# Busy: a long page in Chromium scrolled by wheel, then by key, then back.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
material
browser
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
until_end
close chromium
