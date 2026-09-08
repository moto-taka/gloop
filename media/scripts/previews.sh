#!/bin/sh
set -eu
ffmpeg -hide_banner -loglevel warning -i ../assets/videos/gloop-terminal.mp4 \
  -vf 'fps=6,scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3' \
  -loop 0 -y ../assets/videos/gloop-terminal.gif
ffmpeg -hide_banner -loglevel warning -i ../assets/videos/gloop-parallel.mp4 \
  -vf 'fps=12,scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3' \
  -loop 0 -y ../assets/videos/gloop-parallel.gif
