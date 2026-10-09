#!/usr/bin/env bash
# Busy: a test pattern playing in a 1280×720 window. With STOP_BEFORE set,
# the video is stopped that many seconds before the end and the desktop
# left still, so a dial the walk lowered gets its settle.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
material
video
if [[ -n "${STOP_BEFORE:-}" ]]; then
	sleep $((LENGTH > STOP_BEFORE + 2 ? LENGTH - STOP_BEFORE : 2))
	close mpv
fi
until_end
close mpv
