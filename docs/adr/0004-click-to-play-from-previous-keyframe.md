# 0004. Click-to-play from the previous keyframe

- Status: Accepted
- Date: 2026-07-13

## Context

The grid (ADR-0003) is a keyframe contact sheet for scanning a clip. The natural
next step is to watch: clicking a cell should play the clip. Two forces shape the
design.

- **Where to start.** A cell sits exactly on a keyframe. Starting playback at
  that keyframe is jarring — the moment of interest is already on screen. Opening
  a little earlier gives the eye lead-in.
- **How to pace frames.** Playback must run at real time without a frame-timing
  index, an audio clock, or a full presentation-order scan (all of which the grid
  path deliberately avoids).

## Decision

**Seek to the keyframe *before* the picked frame.** `media::play_stream` aims a
backward seek (`avformat_seek_file` with `max_ts = target`) at the picked
thumbnail time minus a 1 ms epsilon. The picked time is itself a keyframe PTS, so
the epsilon steps the landing one keyframe back to the immediately-previous
keyframe. Playback then decodes the *full* P/B stream forward from there — unlike
the grid, which decodes keyframes only. On the archive's ~1 s GOP this opens
playback ~1 s early; on all-intra footage it is one frame early.

**Pace frames by a wall clock with channel backpressure, decode off-thread.** The
decode thread streams RGBA frames over a bounded `sync_channel` (capacity 3). The
UI shows each frame when its presentation time comes due against egui's monotonic
clock (`ctx.input(|i| i.time)`), anchored to the first frame. The bounded channel
blocks the decoder once the UI stops draining, which:

- paces the decoder to real time (no unbounded read-ahead), and
- makes pause fall out for free — a paused UI stops draining, the channel fills,
  the decoder blocks; resume slides the clock anchor past the paused span.

Stopping is drop-based: leaving playback drops the receiver, so the decoder's next
`send` fails and `play_stream` returns. Playback fills the window over a black
frame; Space pauses, Escape and end-of-stream return to the grid.

**Video only, for now.** No audio. This keeps the media crate free of
`software-resampling` (kept off for startup DLL cost — see the startup-delay
notes) and avoids an audio device and A/V sync. Frames are scaled to a 1600 px
long side on the decode thread to bound per-frame swscale and texture-upload cost.

## Consequences

- Playback opens with useful lead-in and lands the picked moment in context, at
  the cost of decoding a GOP prefix before the first shown frame (one backward
  seek plus a short forward decode — negligible on this material).
- No frame index or seek table is needed: timing is purely the frame's own PTS
  versus the wall clock. If decode can't sustain real time (heavy 4K), playback
  slows rather than dropping frames — acceptable for a review tool; frame-drop
  logic is deferred.
- Pause/stop need no explicit decoder signalling; backpressure and receiver-drop
  handle both. The decode thread can linger for at most one frame's work after
  the UI leaves, which is harmless.
- Audio is a clean later addition: enable the `software-resampling` feature, add
  an output crate, and drive the clock from the audio timeline instead of the
  wall clock. Until then the startup-DLL optimization stands.
- `media` grew a second decode path (`play_stream`) alongside `extract_grid_streaming`;
  they share the scaler/sizing helpers but differ in that playback decodes every
  packet, not just keyframes.
