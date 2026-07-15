# 0011. Bound the read-ahead queue

- Status: Accepted
- Date: 2026-07-15

## Context

ADR-0010 budgeted the recent-grid cache in **thumbnails rather than clips**, for
a stated reason: a grid holds roughly one thumbnail per second of footage, so a
clip count would mean ~25 thumbnails each for the 10-23 s clips this tool
targets but thousands for a long recording.

The read-ahead it introduced in the same pass was left unbounded, and it is the
one path that reads a clip **nobody asked for**. `spawn_extraction` used
`mpsc::channel()`, and `poll` drains only `self.loaded` — never `self.prefetch`,
by design: a prefetch's thumbnails are meant to wait in the channel until it is
opened. Nobody polls it, nothing bounds it, so its worker reads the whole sibling
whatever its length.

At 0.23 MB per 320-wide RGBA thumbnail and ~1 per second of footage, an hour-long
neighbour is **~800 MB** of buffered thumbnails. Worse than the memory: at the
target's ~100 Mbit/s that clip is ~45 GB, so reading it is roughly **6 minutes**
of an external HDD's single head — spent on a clip nobody opened, while the user
plays the current one. ADR-0010's "left to finish once started" is what makes it
bite: `open` cancels a prefetch by dropping it, but **playback does not**. The
same "thousands for a long recording" that shaped the cache budget applies here
and was not applied.

None of this touches the material this tool targets, where a prefetch is ~25
thumbnails and ~5.8 MB. It is a tail risk, not an everyday one — but the tail is
exactly the read-ahead stealing the disk from the clip in front of the user.

## Decision

Bound the extraction channel: `mpsc::sync_channel(GRID_QUEUE_THUMBS)`, with
`GRID_QUEUE_THUMBS = 64`.

**A full queue stops the read, not just the buffering.** `on_thumb` is called from
`drain`, which is called from inside the demux loop — so a worker blocked on
`send` is a worker that has stopped pulling packets off the disk, holding its
position, decoder and file handle. This is what makes a bound worth having: it
costs the disk, not only the memory.

**It engages only where it is needed, and this is not a coincidence.** ADR-0010
measured that frame threading buffers ~18 frames before releasing the first, so a
clip with fewer keyframes than that yields **nothing** until `send_eof` — the file
is already read by the time the first thumbnail exists. The target's clips have
11-25 keyframes, so their read-ahead never reaches the bound and behaves exactly
as before. A clip long enough to be a runaway has thousands of keyframes, so its
thumbnails arrive *during* the read, and it parks. The property that made
cancellation useless on this material is the same one that makes backpressure
harmless on it.

`GRID_QUEUE_THUMBS = 64` sits well clear of those 11-25 so a target read-ahead
completes in full, and caps a runaway at ~64 s of footage: ~15 MB buffered and
~800 MB read (~7 s of the head) instead of the whole clip.

**Parking, not cancelling.** ADR-0010 rejected stopping a read-ahead on play
because cancelling restarts the read from zero, so a prefetch whose read is longer
than a glance at a grid would never complete. Parking has no such cost: the worker
resumes where it stopped when the queue drains. That ADR noted a true pause "would
need machinery in `media` (a park flag or condvar) that does not exist" — a
bounded channel is that machinery and it is already in `std` (`play` has used
`sync_channel(3)` for the same reason all along). It is **not** a general pause,
though: it parks on a full queue, not on play, so ADR-0010's decision to let a
read-ahead of a *target* clip finish during playback stands untouched.

A parked worker is still released when the clip is dropped: `Loaded` owns the
receiver, so dropping it fails the blocked `send`, which the worker ignores, and
the next cancel check ends the pass.

## Consequences

- A long sibling can no longer fill memory or hold the disk to the end. The
  read-ahead's worst case falls from ~800 MB and minutes of the head to ~15 MB and
  seconds of it.
- **Nothing changes for the material this tool targets** — its grids never reach
  the bound. That is the intent, and it is also the limit of what this buys: on
  the target archive this ADR is insurance, not a speedup.
- The foreground grid gets the same bound, which is a no-op: `poll` drains it every
  frame, and its thumbnails become textures on arrival, so a single grid stays
  unbounded in memory for a long clip exactly as ADR-0010 left it. The bound would
  only ever pace a worker whose UI has stopped repainting.
- **This is not pinned by a test, unlike ADR-0010's read-ahead rules.** The bound
  is a resource property, and the end state it produces is identical to the
  unbounded one: a parked prefetch resumes when opened and delivers the same grid.
  No functional assertion separates them — `mpsc` exposes no queue length, and
  draining the channel to count it unblocks the worker being measured. A test that
  slept and inferred parking from timing would pin the clock, not the decision. The
  bound is instead argued from the code it composes: `std`'s guarantee that a full
  `sync_channel` blocks the sender, and `on_thumb` being called from the demux
  loop, which `cancel_stops_extraction_early` already depends on.
- Two exposures found alongside this one are deliberately left alone:
  - **The shared GPU device.** ADR-0010 reasons only about the disk head, but the
    grid and playback both attach the process-wide D3D11VA device for frames over
    `HW_MIN_PIXELS`, and libav serializes access to it. The target material is 4K,
    so a read-ahead left to finish during playback queues on the GPU as well as the
    disk. Likely small — a grid's whole decode measured 22 ms, spread across an
    I/O-bound read — but it is asserted nowhere and measured nowhere, and a
    tester's log will not show it. Its own decision if it ever bites.
  - **`look_ahead` scans the folder on the UI thread.** `neighbor` re-reads and
    sorts the directory once per clip, synchronously in `ui`. The pattern predates
    this work (prev/next do the same on keypress), but the read-ahead now makes it
    automatic for every clip, including ones the user never steps away from.
