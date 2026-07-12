# Test clips

Synthetic clips for developing and testing the frame grid. Each is designed so
that frames sampled evenly across time are **clearly distinct**, and each burns
in a **timecode** so a grid cell can be verified against its true timestamp.

All are H.264 / yuv420p / 30 fps, generated with the ffmpeg CLI — run
`./generate.sh` (needs `ffmpeg` on PATH) to (re)create them.

| File | Res | Dur | What it exercises |
|------|-----|-----|-------------------|
| `hue_sweep_20s_720p.mp4` | 1280×720 | 20 s | Continuous hue sweep — every cell a distinct color (smooth change). |
| `scenes_18s_720p.mp4` | 1280×720 | 18 s | 6 hard-cut scenes ×3 s (bars, rgb, testsrc2, fractal, life, gradient) — discrete scene changes. |
| `mandelbrot_15s_1080p.mp4` | 1920×1080 | 15 s | Continuous fractal zoom + 1080p decode cost. |
| `counter_25s_vertical.mp4` | 1080×1920 | 25 s | Vertical aspect + longer duration; testsrc counter for ground-truth times. |

These are build artifacts, not source — regenerate with the script rather than
committing them to version control.
