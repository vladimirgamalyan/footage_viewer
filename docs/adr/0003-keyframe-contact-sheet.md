# 0003. Keyframe contact sheet

- Status: Accepted
- Date: 2026-07-13

## Context

ADR-0002 built the grid as a fixed 16 cells, each produced by seeking to the
keyframe at or before an evenly-spaced target time and decoding forward to that
target. Per-cell cost was bounded to one GOP, but on the real material this tool
targets that bound is still expensive. The working archive is uniform: 4K H.264,
29.97 fps, ~100 Mbit/s, 10–23 s clips, with a **rock-constant GOP of 29 frames
(0.968 s)** — every file, no variance. Decoding forward from a keyframe to a
mid-GOP target means decoding ~half a GOP of 4K frames per cell, which measured
**3.8 s** for a 16 s clip — the exact slow case ADR-0002 flagged.

Two further observations about this material:

- Keyframes already sit ~1 s apart, so an evenly-spaced 16-cell grid re-derives
  points that essentially coincide with keyframes anyway.
- The shortest clips have **fewer than 16 keyframes** (as few as 11), so a fixed
  16-cell grid cannot be filled with distinct keyframes at all.

## Decision

Replace the fixed-N, seek-per-cell grid with a **keyframe contact sheet**:

- **Send only keyframe packets to the decoder.** In a single demux pass, skip
  every non-key packet (`packet.is_key()`); each keyframe is intra-coded and
  decodes on its own, so the P/B frames between them are never decoded. There is
  no seeking. Per-thumbnail cost is one intra frame; total cost scales with the
  number of keyframes, not with `duration × fps`.
- **Thin to about one thumbnail per `spacing_s`** (currently 1 s) with an integer
  skip factor `N = round(spacing_s / gop)` derived from the first keyframe
  interval (`KeyframeSampler`). On this material `N = 1` — every keyframe is
  kept, ~0.968 s apart. On denser footage (e.g. all-intra) it thins to roughly
  one per `spacing_s`.
- **The thumbnail count is dynamic** (= kept keyframes, not a fixed 16). The UI
  grid is a vertical scroll area with a **fixed column count**; rows grow as
  thumbnails stream in, so already-shown cells never reflow.

Note on a dead end: setting `skip_frame = AVDISCARD_NONKEY` on the decoder did
**not** reduce decode cost here — the decoder still decoded every frame (timing
scaled with full frame count, before and after the option). Gating at the *send*
stage — never handing non-key packets to the decoder — is what actually avoids
the work. `skip_frame` was removed.

## Consequences

- Grid fill is dramatically faster on the real material. Measured (release):
  - 15.4 s 4K, 16 keyframes: **3.8 s → 0.25 s**
  - 23.4 s 4K, 25 keyframes: **0.42 s**
  - 10.4 s 4K, 11 keyframes: **0.20 s**
- Thumbnails are no longer evenly spaced by exact time; they land on keyframes
  (~1 s apart here). This is the accepted "sacrifice interval precision for
  speed" tradeoff, and for a footage overview keyframes are a natural — arguably
  better — sampling grid. They are therefore not byte-identical to ADR-0002's
  output.
- Correctness rests on keyframes decoding standalone. Verified on the archive:
  single-keyframe decode yields clean, full-color frames with no open-GOP
  artifacts (Panasonic AVC uses IDR keyframes).
- The skip factor assumes a near-constant GOP (derived from the first interval),
  which holds for camera files. Highly variable-GOP sources could be thinned
  unevenly; not a concern for this material.
- The thumbnail count grows with clip length (~one per second). For the 10–23 s
  archive this is fine; coarsening `spacing_s` for very long clips is deferred
  until such material exists.
- Supersedes ADR-0002: the grid no longer seeks per cell, and the fixed 16-cell
  model is gone.
