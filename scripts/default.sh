#!/usr/bin/env bash
set -euo pipefail

xdg-mime default stitchviewer.desktop image/tiff
xdg-mime query default image/tiff

