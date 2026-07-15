#!/usr/bin/env bash
# Generate the test clips (see README.md). Deliberately distinct frames across
# time; each clip burns in a timecode for grid verification. Requires the ffmpeg
# CLI on PATH. Run from anywhere; outputs into this directory.
set -euo pipefail
cd "$(dirname "$0")"

FONT="C\\:/Windows/Fonts/arial.ttf"
X264="-c:v libx264 -preset veryfast -pix_fmt yuv420p -movflags +faststart"
TC="drawtext=fontfile='${FONT}':fontsize=56:fontcolor=white:box=1:boxcolor=black@0.55:boxborderw=12"

echo "[1/5] hue_sweep_20s_720p.mp4 (continuous rainbow, 1280x720, 20s)"
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "color=c=red:s=1280x720:r=30:d=20" \
  -vf "hue=h=360*t/20,${TC}:text='HUE  %{pts\\:hms}':x=(w-tw)/2:y=h-th-40" \
  ${X264} hue_sweep_20s_720p.mp4

echo "[2/5] scenes_18s_720p.mp4 (6 hard-cut scenes x 3s, 1280x720, 18s)"
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -t 3 -i "smptebars=s=1280x720:r=30" \
  -f lavfi -t 3 -i "rgbtestsrc=s=1280x720:r=30" \
  -f lavfi -t 3 -i "testsrc2=s=1280x720:r=30" \
  -f lavfi -t 3 -i "mandelbrot=s=1280x720:r=30" \
  -f lavfi -t 3 -i "life=s=1280x720:r=30:ratio=0.3:life_color=0x33ff33:death_color=black:mold=10" \
  -f lavfi -t 3 -i "gradients=s=1280x720:r=30" \
  -filter_complex "\
[0:v]format=yuv420p,setsar=1[v0];\
[1:v]format=yuv420p,setsar=1[v1];\
[2:v]format=yuv420p,setsar=1[v2];\
[3:v]format=yuv420p,setsar=1[v3];\
[4:v]format=yuv420p,setsar=1[v4];\
[5:v]format=yuv420p,setsar=1[v5];\
[v0][v1][v2][v3][v4][v5]concat=n=6:v=1:a=0[cat];\
[cat]${TC}:text='SCENE  %{pts\\:hms}':x=20:y=20[out]" \
  -map "[out]" ${X264} scenes_18s_720p.mp4

echo "[3/5] mandelbrot_15s_1080p.mp4 (continuous fractal zoom, 1920x1080, 15s)"
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -t 15 -i "mandelbrot=s=1920x1080:r=30" \
  -vf "${TC}:text='FRACTAL  %{pts\\:hms}':x=(w-tw)/2:y=40" \
  ${X264} mandelbrot_15s_1080p.mp4

echo "[4/5] counter_25s_vertical.mp4 (testsrc counter, vertical 1080x1920, 25s)"
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -t 25 -i "testsrc=s=1080x1920:r=30" \
  -vf "${TC}:text='%{pts\\:hms}   n=%{n}':x=(w-tw)/2:y=60" \
  ${X264} counter_25s_vertical.mp4

# Stands in for the archive this tool targets (4K, 25 fps, ~0.5 s GOP), which the
# others do not resemble: they are 720p/1080p with x264's default 250-frame GOP.
# It is the only clip big enough to take the hardware decode path (HW_MIN_PIXELS)
# and the only one whose seeks decode a short GOP rather than a long one.
echo "[5/5] camera_8s_4k.mp4 (4K short-GOP, mimics the target camera footage, 3840x2160, 8s)"
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -t 8 -i "testsrc2=s=3840x2160:r=25" \
  -vf "${TC}:text='4K  %{pts\\:hms}   n=%{n}':x=(w-tw)/2:y=60" \
  -g 12 ${X264} camera_8s_4k.mp4

echo "DONE"
