# 0002. Seek-based grid extraction

- Status: Superseded by ADR-0003
- Date: 2026-07-13

## Context

The grid is built by sampling N evenly-spaced frames across a clip. The FFmpeg
spike (`spike/README.md`) compared two strategies and recommended a **single
linear decode pass** — decode every frame from the start and keep the ones
nearest each target time — because per-cell seeking re-decoded overlapping GOP
regions on the short (15–25 s) test clips and lost to a linear pass there.

That recommendation was measured only on short clips. A linear pass decodes
every frame up to the last target (~97% of the file), so its cost scales with
clip length: `duration × fps`. On real footage (minutes to hours) the grid took
many seconds to fill — a 10-minute 720p clip measured **10.5 s** — which is the
"previews are very slow" symptom. Other players (e.g. the sibling `vievo`
project) open and scrub instantly because they seek.

## Decision

Extract each grid cell by **seeking to the keyframe at or before its target time
and decoding forward to the target**, instead of one linear pass over the whole
file. This mirrors the frame-accurate seek proven in the `vievo` decode core.

Per-cell cost is bounded to one GOP and is independent of clip length. Timeline
origin (non-zero stream `start_time`, common in camera files) is accounted for
when computing targets and reported cell times. Progressive streaming of cells
to the UI is unchanged.

The decoder is also configured for **frame-level multithreading** (auto CPU
count). Seeking bounds *how many* frames are decoded; on high-resolution footage
the remaining cost is raw per-frame decode throughput, which single-threaded
decode leaves on one core. On a short 4K clip — where cells sit ~1 GOP apart and
seeking alone helps little — threading is the dominant win.

This supersedes the linear-pass recommendation in `spike/README.md` §3 for the
grid; that section's other findings (decode *forward* past the keyframe to avoid
duplicate thumbnails) still hold and are preserved.

## Consequences

- Grid fill time is now roughly constant regardless of clip length. Measured
  (release), thumbnails byte-identical to the linear pass:
  - 10-minute 720p: **10.5 s → 0.19 s**
  - 16-second **4K** (H.264, ~1 s GOP): **14.2 s → 3.8 s** (seek alone: 9.3 s;
    frame threading takes it the rest of the way)
- Very short clips with sparse keyframes may re-decode overlapping GOP regions
  across adjacent cells (the case the spike flagged), but threading keeps them
  well under the linear pass — no user-visible regression.
- The decoder now seeks and flushes per cell rather than running one continuous
  pass; correctness depends on decode-forward-to-target and end-of-stream flush
  handling for B-frame reorder.
