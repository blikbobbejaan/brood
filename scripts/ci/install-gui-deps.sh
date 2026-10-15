#!/usr/bin/env bash
# The system libraries the `gui`/`gui-gpu`/`audio` features link against on Ubuntu: winit,
# softbuffer, glutin+glow and arboard (X11 + Wayland + xkbcommon + OpenGL), and ALSA. A
# default build needs none of them. One list for every CI job that builds those features.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  pkg-config \
  libasound2-dev \
  libxkbcommon-dev \
  libwayland-dev \
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
  libgl1-mesa-dev
