#!/usr/bin/env bash
# Busy: a full-screen terminal flooded with coloured random lines
# (busy-lines.sh), every frame the whole screen new.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
foot -- "$HERE/busy-lines.sh" >/dev/null 2>&1 &
await_window foot
swaymsg -q '[app_id="foot"] fullscreen enable'
until_end
close foot
