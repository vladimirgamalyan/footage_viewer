# 0005. Command-driven live playback decoder with a scrubber

- Status: Accepted
- Date: 2026-07-13

## Context

ADR-0004 gave playback a decode thread that streamed frames over a bounded
channel, paced by the UI against egui's wall clock, with pause and stop falling
out of channel backpressure and receiver-drop. Each `play_stream` call opened the
file, seeked once, and decoded forward until the UI left.

Adding a scrubber (a seek bar with a draggable handle) exposed the limits of that
design. VLC-style scrubbing wants the video to track the cursor live and land on
the *exact* frame under it. With a one-shot `play_stream`, every seek meant
tearing down the player and spawning a fresh decode thread — reopening the file,
re-seeking, re-warming the threaded decoder. That is expensive per drag step,
flashes black while the new position decodes, and can only cheaply show the
keyframe near the target, not the precise frame.

## Decision

**One long-lived decoder thread per clip, driven by commands.** `play_stream`
takes a `Receiver<PlayCommand>` and runs a state machine over a single open
decoder. Commands are `Scrub(t)`, `Play(t)`, `Pause`, `Resume`, `Stop`. The UI
holds the `Sender` and seeks by sending a command — no thread is recreated.

**Pacing moves into the decoder.** The thread times frames itself with
`Instant`-based anchors and waits on `recv_timeout`, so a command preempts the
wait instead of a frame blocking on a full channel. The UI no longer keeps a
clock anchor; `advance_player` just uploads the latest frame that arrived (the
decoder already released it when due) and reads its `time_s` for the scrubber.
The frame `sync_channel` (capacity 3) remains, but only as transport — pause is
now an explicit `Pause`/`Resume`, not a side effect of a stalled channel.

**Seek is a flush-and-skip on the same decoder.** A `Scrub`/`Play` command seeks
to the keyframe before the target, flushes, then decodes forward discarding
frames earlier than the target (within `FRAME_EPS_S`). That lands the exact
frame. `Scrub` emits it and holds (paused); `Play` emits it and runs on. The
initial start keeps ADR-0004's keyframe-before behavior (a grid-cell click opens
with lead-in); explicit seeks from the bar are precise.

**Scrubber interaction, VLC-style.** A click on the bar sends `Play(t)`; a drag
sends `Scrub(t)` (throttled by time and deduplicated against the shown position,
since a seek is cheap but not free); releasing sends `Play(t)` from the exact
spot. The handle is a pure indicator — it never grabs; the whole bar seeks.

## Consequences

- Scrubbing shows precise frames without reopening the file, and the last frame
  stays on screen across a seek (the texture is only replaced when the new frame
  arrives), so there is no black flash.
- This supersedes ADR-0004's pacing model (UI wall clock + backpressure-driven
  pause). The keyframe-before start, video-only scope, and 1600 px decode box
  from ADR-0004 still stand.
- Timing lives in one place (`media`), and the UI player shrank to transport plus
  display state. Pause/stop are now explicit signals rather than emergent
  behavior; stop still also works by dropping the channels (the decoder's next
  send or `recv` fails and `play_stream` returns).
- The decoder reads commands between decodes and during the pace wait, so a
  command is serviced within at most one frame's decode latency — smooth enough
  for live scrubbing on this material; heavy 4K may lag a frame behind the cursor.
- End-of-stream holds on the last frame (paused) instead of returning to the
  grid: playing through to the end and scrubbing past it behave the same way, so
  reaching the end never kicks back to the grid. The decoder thread stays open,
  blocked on the command channel; Escape/Stop from the UI exits, and scrubbing
  back seeks and resumes.
