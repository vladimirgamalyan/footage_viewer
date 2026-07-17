# 0019. Cap the grid's frame threads on the GPU

- Status: Accepted
- Date: 2026-07-17

## Context

ADR-0010 recorded the symptom and declined to fix it:

> **The grid does not fill in progressively on this material, and that is not
> fixed here.** [...] with 11–25 keyframes against ~18 buffered frames, nothing
> appears until the file is fully read. On the HDD that means a blank grid for the
> whole read and then every thumbnail at once [...] Capping the grid's frame
> threads the way `PLAYBACK_THREADS` caps playback's is the obvious lever, but it
> trades against the throughput ADR-0003 and ADR-0009 measured, so it is its own
> decision and needs its own numbers.

Here are the numbers. Decoding the 4K fixture's 17 keyframes, release build:

| threads | on the GPU | on the CPU |
|---|---|---|
| all cores (16) | **50 ms, first frame after 18 packets** | 55 ms, after 18 |
| 1 | 25 ms, first frame after **3** | 222 ms, after 3 |
| 2 | **15 ms**, after 4 | 113 ms, after 4 |
| 4 | 16 ms, after 6 | 65 ms, after 6 |
| 6 | 18 ms, after 8 | **49 ms**, after 8 |

Two things fall out, and they point the same way.

**The warmup is the thread count, exactly.** On the all-intra fixture the first
frame comes out after precisely N packets at N threads (1→1, 2→2, 6→6). The 4K
column sits two higher because the stream's B-frame reorder delay adds a constant
floor of 3 — that floor is the stream's, not the threads'. So with all cores
against 17 keyframes, the first frame lands at packet 18: after the last one.
This is ADR-0010's observation reproduced exactly, and its diagnosis confirmed —
frame threading, not reorder.

**On the GPU the threads buy nothing.** They are the *slowest* setting there,
50 ms against 15 ms at two, because the decode belongs to the GPU and the threads
only add coordination in front of it. All cores on the GPU is the worst of both
worlds: slowest, and nothing released until the file is read.

The trade ADR-0010 worried about is real, but only on the CPU, where threading
still earns its keep — 222 ms single-threaded against 49 ms at six.

**Why this went unnoticed is worth recording.** ADR-0003 chose all cores when the
grid decoded on the CPU, where the choice was right and measured. ADR-0009 then
moved frames above `HW_MIN_PIXELS` to the GPU and did not revisit the thread
count. The setting has been the wrong one for the target's 4K material ever
since, and it stayed invisible because on a warm local disk the whole pass is
75 ms and nobody waits for a thumbnail.

## Decision

**Cap the grid's frame threads to one when it decodes on the GPU**
(`GRID_GPU_THREADS`). **Leave the CPU path on all cores** (`count: 0`), where the
measurement says they still pay.

One rather than two: the 10 ms two threads save is noise against a read measured
in seconds, and each thread costs a whole keyframe of warmup — which on the
target's external disk is a seek. Three packets is the floor either way, set by
the stream's reorder delay.

## Consequences

- **The grid fills as it reads.** On the target's 4K material the first thumbnail
  now lands after 3 keyframes instead of after the file. With ADR-0015's seek that
  is roughly 150 ms against the ~7 s of blank grid the tester sees today — the
  change that makes opening a clip feel like VLC opening one, which is what she
  actually asked for.
- **The GPU path also gets faster**, which is not the point but is not nothing:
  50 ms → 25 ms on the fixture's 17 keyframes. It was a straight regression sitting
  unnoticed since ADR-0009.
- **Cancellation gets teeth on 4K.** ADR-0010 built the cancel flag to stop a read
  the moment a clip is abandoned, but on this material the first thumbnail — the
  natural place to notice — arrived only after the read was done. Now a 4K pass can
  be abandoned mid-read like any other. `the_4k_grid_streams_thumbnails_before_it_finishes`
  pins it, and fails with `kept 9 of 9` if the count goes back to `0`.
- **`cancel_stops_extraction_early`'s premise is now half-wrong and its comment is
  corrected.** It said every clip but the all-intra fixture holds fewer keyframes
  than the decoder has threads, so none streams. That was true of all of them; it
  is now true only of the ones that decode on the CPU. The test still uses the
  all-intra fixture, which is 240p and therefore below `HW_MIN_PIXELS`, so it keeps
  testing what it always tested.
- **The split is by decode device, not by path.** A machine with no D3D11VA gets
  all cores and the warmup with them, by decision rather than by defect — there the
  threads are doing real work. The new test skips itself on such a machine, and
  says why.
- `PLAYBACK_THREADS`'s doc no longer claims the grid does not care about startup
  latency. It did not care when it was a batch decode behind a warm disk; ADR-0010
  measured that it is neither.
