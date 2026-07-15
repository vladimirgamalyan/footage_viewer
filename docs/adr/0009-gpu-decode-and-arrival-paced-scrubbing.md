# 0009. GPU decode for large frames, and arrival-paced scrubbing

- Status: Accepted
- Date: 2026-07-15

## Context

Scrubbing the archive this tool targets (4K H.264, 25 fps, ~94 Mbit/s, 12-frame
GOP) was unusable. Two independent faults, both measured on `video/P2010646.MP4`
in a release build:

**1. A precise seek cost 145 ms, and almost none of it was seeking.** ADR-0005's
live seek flushes the decoder and decodes forward to the exact frame. Timing its
parts (the `LOG_SCRUB_TIMING` breakdown added in `3fa5823`) showed:

| stage | cost |
|---|---|
| `ictx.seek` — the actual seek | **0.2 ms** |
| `decoder.flush()` | **36–57 ms** |
| decode forward to the target (2–12 frames) | 63–177 ms |

The seek is free. The flush is not: `avcodec_flush_buffers` on a frame-threaded
decoder drains the whole worker pipeline, and its cost scales with the thread
count — measured 0.5 ms at 1 thread rising to 50 ms at 12. Frame threading also
never reaches steady state in the 2–12 frame burst a seek decodes, so it pays
full pipeline latency every time (19.6 ms/frame, against 3.6 ms/frame in a long
linear decode). Tuning `PLAYBACK_THREADS` cannot win: fewer threads cut the flush
but slow the decode, and the total stays at 130–145 ms at every setting.

**2. The UI queued seeks faster than the decoder could serve them.** The scrubber
fired a `Scrub` every 80 ms (`SCRUB_INTERVAL_S`) while each took ~145 ms. The
queue grew at roughly twice the rate it drained; the decoder abandoned each
target for the next queued one and so almost never emitted a frame at all. During
a 2-second drag the picture trailed the cursor by a mean of **3.24 s** (max
7.90 s) on a 12.5-second clip — it effectively froze. Worse, `play_stream` ran a
stale command's seek *and flush* before checking the queue for a newer one, so
each backed-up command cost a full flush that the next command discarded.

## Decision

**Decode on the GPU when frames are large.** `play_stream` attaches a D3D11VA
device (`HwDevice`) to the decoder when the source exceeds `HW_MIN_PIXELS`, and
downloads *only the frame it emits* — the frames decoded past on the way to a
seek target never leave the GPU. `ffmpeg-next` exposes no hardware-decode API, so
this calls libav directly; libav's default `get_format` selects the hardware
pixel format once a device is attached, and quietly keeps decoding on the CPU if
the codec has no matching hardware config, so the FFI only ever adds a fast path.
The scaler is built from the first emitted frame rather than up front, because a
hardware decoder's real pixel format is only known once a frame exists.

**Open the device once per process, not once per clip.** `av_hwdevice_ctx_create`
costs **~100 ms** — as much as two scrubs. Paid per clip it would land squarely on
play start (measured: first frame 217 → 318 ms) and hand back much of what the GPU
wins. A `OnceLock` holds one device for the process; libav refcounts it and
serializes access, so every decoder just takes a reference. The session's first
clip pays the 100 ms on the grid's worker thread; nothing after it does.

This cost is easy to mistake for slow decoding. It first made the GPU look like a
*regression* for the grid (468 ms against the CPU's 381 ms) and only turned out to
be one-time setup when the same clip was measured twice in one process.

**Use the GPU for the grid too, above the same threshold.** The contact sheet
(ADR-0003) sends only keyframes, so every frame is intra and independent — the CPU
parallelizes it near-perfectly across all cores, and there is no seek and so no
flush. That is the CPU's best case and the GPU's worst (every frame is downloaded
here, not just one), so the margin is thin rather than dramatic, and on cheap
content it is a tie. It still wins, and it widens on any machine with fewer cores.

**Keep the CPU for small frames.** The GPU is not uniformly faster: its fixed
per-frame cost stops being amortized as frames shrink. Measured mean of 10
precise seeks:

| clip | CPU (frame×6, before) | GPU (D3D11VA) |
|---|---|---|
| 4K camera, 8.3 MP | 145 ms | **30 ms** |
| mandelbrot, 2.07 MP | 85 ms | **75 ms** |
| counter vertical, 2.07 MP | **35 ms** | 64 ms |
| scenes 720p, 0.92 MP | **19 ms** | 39 ms |

The crossover is not resolution alone — `counter` and `mandelbrot` have identical
pixel counts and land on opposite sides, because content and GOP length also set
the per-frame cost. But 1080p-class is the largest size measured where the CPU
still wins, so `HW_MIN_PIXELS` sits just above it (2.1 MP). Below the threshold
nothing changes; above it, the target material gets the GPU.

**Pace scrubs by arrival, not by a clock.** The scrubber fires the next live seek
only once the previous one has landed (`App::scrub_in_flight`, cleared when a
frame arrives — a `Scrub` emits exactly one frame and then holds). At most one
seek is ever in flight, so no queue can build, and the UI tracks the cursor as
fast as the decoder can go and never faster. This mirrors the gate ADR-0005's
A/D key repeat already used. `SCRUB_INTERVAL_S` is gone.

**Fold the command queue in the decoder.** `play_stream` now drains every queued
command before acting on any of them, so a burst costs one seek instead of one
per command. `PlayState::start_move` clears both `skip_until` and `pending_seek`,
so folding leaves exactly the last move's intent rather than a seek from one
command and a skip target from another.

## Consequences

- Scrubbing the target material is responsive. Measured after (mean of 8 seeks;
  drag = how far the picture trails the cursor over a 2 s sweep):

  | clip | seek before → after | drag before → after |
  |---|---|---|
  | 4K camera (`P2010646`) | 145 → **49 ms** | 3.24 → **0.06 s** |
  | mandelbrot 1080p | 85 → **75 ms** | 0.22 → **0.11 s** |
  | scenes 720p | 19 → **12 ms** | — → **0.02 s** |
  | counter vertical | 35 → **32 ms** | — → **0.04 s** |

  Nothing regressed: clips below the threshold stay on the CPU and still gained,
  from the queue folding alone. Reproduce with
  `cargo run -p footage-viewer-media --release --example scrub_bench -- <clip>`.
- **GPU output is bit-identical to CPU output.** 23,040,000 bytes of RGBA across
  four 4K timestamps compared byte for byte: zero differences. H.264 decoding is
  exact by specification, and NV12→RGBA agrees with YUV420P→RGBA.
- The decode path now has `unsafe`: three libav calls (device create, buffer ref,
  frame download). `HwDevice` owns the device and releases it on drop, which
  matters because `play_stream` returns from many places.
- **`HW_MIN_PIXELS` is calibrated on one machine** (Ryzen 9 9950X3D, 16C/32T +
  RTX 5070 Ti) and the real crossover moves with the CPU/GPU balance. The error is
  one-directional and safe: the CPU side of the measurement is about as fast as
  CPUs get, so on a weaker machine the GPU wins at *smaller* frames than here and
  the threshold is merely conservative — clips below it decode exactly as they do
  today, so no machine can regress there. The reverse (GPU losing at 4K) would need
  a fast CPU paired with a slow GPU, which a weaker machine is not: hardware
  decoders are fixed-function and handle 4K H.264 even on old parts, while a
  few-core CPU does not.
- The residual gap is a machine with no usable D3D11VA for the codec: it falls back
  to CPU frame threading and gains nothing on 4K. Slice threading is the ready
  answer there (measured 145 → 46 ms on the target material, no `unsafe`), held
  back only because it costs 3–4× on the long-GOP material the other fixtures use
  and would need the GOP length to choose safely. Decide it with a log, not a
  guess.
- Both paths log which decoder took the clip, once each (`grid: 3840x2160 (8.3 MP),
  decoding on the GPU, frames as NV12`). Attaching a device is only a *request* —
  libav silently decodes on the CPU when the codec has no matching hardware config
  — so this line, taken from the first emitted frame, is the only thing that
  distinguishes "GPU asked for" from "GPU used". With the per-seek timings it makes
  a tester's log (ADR-0008) enough to settle the threshold on any machine.
- Playback holds a GPU device for the clip's lifetime and uses ~14× less CPU
  (measured `utime` 0.6 s vs 8.7 s for a full linear 4K decode), leaving cores for
  the UI. A machine with no D3D11VA device logs the fact once and decodes on the
  CPU exactly as before.
- The download is not worth optimizing away: a 4K NV12 readback is **1.7 ms**
  (re-downloading an already-synced frame). The 10–33 ms the first download
  appears to cost is waiting on the asynchronous GPU decode — real work, not
  transfer. GPU-side scaling would therefore buy nothing, which is just as well:
  it would need `avfilter`, which `media/Cargo.toml` deliberately does not link
  (~34 MB of DLLs the Windows loader would map before `main`).
- This supersedes ADR-0005's scrubber pacing — a drag no longer throttles seeks on
  a wall clock, it waits for the previous one to land. The rest of ADR-0005 (one
  long-lived command-driven decoder, seek as flush-and-skip, decoder-side frame
  pacing) stands unchanged.
- The floor is now the decode itself: a precise seek must decode from the previous
  keyframe, ~6 frames of 4K on this material. Below that only an imprecise preview
  would help, and at 0.06 s of drag lag there is nothing left to fix.
- `test_videos/camera_8s_4k.mp4` (4K, 25 fps, `-g 12`) joins the fixtures. The
  existing clips are 720p/1080p with x264's 250-frame GOP and resemble the target
  archive in no way; this one is the only fixture that takes the GPU path or
  decodes a short GOP on a seek.
- The grid (ADR-0003) gains less than playback, as expected from an all-intra,
  no-seek, download-everything workload. Steady state, once the device is open
  (`grid_bench` reports a second run for exactly this reason):

  | clip | fill before → after | first thumbnail before → after |
  |---|---|---|
  | `P2010646` 4K, 13 thumbs | 382 → **350 ms** | 254 → **221 ms** |
  | `P2010649` 4K, 18 thumbs | 464 → **441 ms** | 287 → **242 ms** |
  | `camera_8s_4k`, 9 thumbs | 84 → 89 ms | 55 → **46 ms** |
  | 1080p/720p fixtures | unchanged (CPU) | unchanged |

  The margin grows as cores shrink, which is the point: with the CPU decoder
  pinned to stand in for a weaker machine, the same 4K grid takes 473 ms at 8
  threads, 649 ms at 4 and 1043 ms at 2, against the GPU's ~350 ms at any of them.
  This machine's 32 threads are the *least* favourable case for the GPU here.
- Batching sends to give the GPU more frames in flight was tried and rejected: it
  did not help (a 4K grid took 216–232 ms at every depth from 1 to 64, so the GPU
  decode is genuinely ~24 ms per 4K intra frame, not latency waiting to be hidden)
  **and it silently lost thumbnails** — 13 became 9 — because a starved surface
  pool makes `send_packet` return EAGAIN, which the grid discards with `.ok()`.
  `grid_of_4k_keeps_every_kept_keyframe` pins the count so this cannot return
  unnoticed.
- What now dominates the grid is not decoding at all: `input()` —
  `avformat_open_input` plus `avformat_find_stream_info` — takes **175–197 ms** on
  the real camera files (against 14 ms on a synthetic 4K clip of the same size),
  which is half the total fill and most of the time before the first thumbnail
  appears. It is reducible — `-probesize 500k -analyzeduration 0` cuts a probe of
  `P2010646` from 212 ms to 84 ms with the duration still read correctly — but that
  trades against stream detection on less well-formed files, so it is its own
  decision and not folded in here.
- `spike/hwaccel/` holds the standalone spike that de-risked this, in the manner
  ADR-0001 was de-risked; it is where the flush/download/decode split was measured
  before any of it touched `media`.
