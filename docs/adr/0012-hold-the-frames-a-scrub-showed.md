# 0012. Hold the frames a scrub showed

- Status: Accepted
- Date: 2026-07-15

## Context

ADR-0009 closed with "at 0.06 s of drag lag there is nothing left to fix". A
tester's log (ADR-0008) from real work on real footage says otherwise: scrub
steps there take **102 ms forward and 167 ms backward**, not the 49 ms measured
here. Two assumptions behind that closing line do not survive contact with the
tester's machine.

**The fixture's GOP is not the archive's.** ADR-0009 calibrated on
`camera_8s_4k.mp4` — 25 fps, `-g 12` — and concluded "a precise seek must decode
from the previous keyframe, ~6 frames of 4K on this material". The tester's
Lumix files are 4K **29.97 fps with a ~30-frame GOP** (1 s, the camera's
default). A backward step there decodes a mean of **16 frames and a maximum of
27**, and each costs ~9 ms on their GTX 1070 rather than ~5 ms on this machine's
RTX 5070 Ti. Both the fixture and the dev machine flatter the design; the
combination flatters it twice.

**Nothing was decoded twice in the benchmark, and everything is decoded twice in
use.** `scrub_bench`'s seek phase visits eight distinct targets and its drag
sweeps one way. What the log shows is a person holding A/D: stepping out over a
stretch, back over the same stretch, then out again. Measured over the tester's
111 scrubs:

| | |
|---|---|
| frames decoded to show 120 frames | **1833** (15.3× waste) |
| scrubs asking for a frame decoded seconds earlier | **31%** |
| scrub time that was decoding | **77%** |

The backward step is what makes this expensive, and it cannot decode in place:
it seeks to the keyframe before the target and decodes the GOP forward, and the
next step back seeks to the *same* keyframe and decodes almost the same frames
again — 26 frames, then 12, then 26, alternating as the target crosses each
keyframe. The frames it needs were decoded, scaled, shown, and dropped moments
before.

A third fault surfaced while testing this. Scrubbing twice to one position
stepped the picture a frame *forward*: the decoder already sits on the frame it
emitted, so `start_move` sees the target as ahead of `current_s`, decodes in
place, and lands on the next frame at or after it. The log shows it as
`decoded 1 frames | landed <target + one frame>`. It also shifts the whole return
pass one frame off the outward pass's grid, which is why a naive cache keyed on
the target would have missed most of what it held.

## Decision

**Keep the frames a scrub shows, and answer repeat targets from them.**
`ScrubCache` holds the scaled RGBA a `Scrub` emitted, keyed by presentation time.
The decode loop checks it before the seek, because that seek *is* the cost: a hit
answers in a memcpy instead of a keyframe seek and a GOP of decoding.

**A hit must be the frame a decode would have produced, or the cache is a bug.**
The loop emits the first frame at or after `target − FRAME_EPS_S`; `get` takes
the earliest held frame at or after that same bound and trusts it only within one
frame duration of it. On a constant frame rate a nearer frame would then have to
lie before the bound, where the loop would not have emitted it either. A wider
gap means the frames between are simply not held and one of *them* is the answer,
so the cache misses and the decoder runs. The frame duration comes from the
stream's `avg_frame_rate`; a stream that declares none disables the cache rather
than guess how far a held frame reaches.

**Leave the decoder where it stands on a hit.** Nothing was decoded, so
`current_s` keeps meaning "where the decoder is" — which is what `start_move`
needs to choose between decoding forward in place and seeking back to a keyframe.
The frame on screen and the decoder's position are allowed to differ while a
scrub walks over held ground; the first miss seeks from wherever the decoder
actually is, which is correct by construction.

**Scrubs only.** A `Play` streams frames continuously and would evict the whole
cache in a second of footage for frames nobody asks for again — and it has to
move the decoder regardless.

**Budget in bytes, not frames** (`SCRUB_CACHE_BYTES`, 128 MB). A frame's cost is
set by the playback box, not the source: 4K at the UI's 1600 px box is ~5.8 MB,
so this holds ~22 frames — about 11 s of A/D stepping — while smaller footage
gets proportionally more frames for the same memory, which is what we want, since
it is scrubbed the same way.

## Consequences

- Stepping back over ground already covered is now free. Measured with
  `scrub_bench`'s new A/D sweep (0.5 s steps — the UI's `SEEK_STEP_S` — out to
  7 s and back over the same positions), release build, this machine:

  | clip | back over same ground | full sweep | drag lag |
  |---|---|---|---|
  | `camera_8s_4k` (GPU) | 27.0 → **5.9 ms** | 0.68 → **0.62 s** | 0.04 → **0.01 s** |
  | `mandelbrot_15s_1080p` (CPU) | 93.9 → **0.7 ms** | 1.40 → **0.18 s** | 0.11 → **0.02 s** |
  | `scenes_18s_720p` (CPU) | 15.0 → **0.7 ms** | 0.26 → **0.08 s** | 0.02 → 0.02 s |
  | `counter_25s_vertical` (CPU) | 32.3 → **0.7 ms** | 0.51 → **0.10 s** | 0.04 → **0.02 s** |

  The outward pass is unchanged, as expected — it decodes ground nobody has seen.
- **The long-GOP CPU clips gain most, not the 4K the work was aimed at.**
  `mandelbrot` is 1080p, below `HW_MIN_PIXELS`, and carries x264's 250-frame GOP:
  a backward step there decoded ~125 frames at 94 ms. That is the worst case in
  the fixture set and it is now 0.7 ms. The 4K path gains less in relative terms
  because ADR-0009 already made its seek cheap.
- **A drag gains without revisiting anything.** A drag fires targets far faster
  than frames change — three UI ticks land inside one 40 ms frame — so most of
  its scrubs now hit the frame just shown. The 4K drag went from 128 to 180
  scrubs served in 2 s, and `mandelbrot` from 69 to 468.
- **Scrubbing twice to a position now shows the same frame**, since the repeat is
  a hit. `re_scrubbing_a_position_shows_the_same_frame` pins it and fails without
  the cache (it lands a frame late), so the forward-step defect above cannot
  return unnoticed. The underlying `start_move` asymmetry is untouched — the
  cache masks it for scrubs, which is where it was reachable.
- Memory rises by up to `SCRUB_CACHE_BYTES` per open clip, freed when the clip
  closes with its decoder. Only the played clip has one; the grid cache is
  separate and unchanged.
- The cache is per clip and per `play_stream` call, so it cannot serve the grid
  or survive a reopen. Both would need frames to outlive the decoder that made
  them, which is a different decision than this one.
- ADR-0009's conclusion stands where it was measured and is wrong where it was
  extrapolated: the seek is as fast as it said, and a scrub was still decoding
  most of what it showed. The residual floor is the outward pass — genuinely new
  frames, one GOP each — which no cache can help.
- What this does not fix, from the same log: the grid's `convert` (175 ms mean,
  28% of a fill, single-threaded swscale on frames the GPU already holds), its
  `open` probe (109 ms), and a demux that reads 100% of a file to use 15% of it —
  2003 MB read across the session for 306 MB of keyframes. Each is its own
  decision.
