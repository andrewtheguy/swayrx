#!/usr/bin/env bash
# Flood the terminal this runs in: every line a random colour and random
# characters to its width, as fast as the terminal draws them, until killed.
# The busy end of what a desktop can be: every frame the whole window new.
while true; do
  cols=$(tput cols)
  color=$((RANDOM%256))
  printf '\033[38;5;%sm' "$color"
  LC_ALL=C tr -dc 'A-Za-z0-9@#$%&*+=:;!?' </dev/urandom | head -c "$((cols-2))"
  printf '\033[0m\n'
done
