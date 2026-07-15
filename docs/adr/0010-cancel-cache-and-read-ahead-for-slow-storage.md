# 0010. Cancel, cache and read ahead the grid for slow storage

- Status: Accepted
- Date: 2026-07-15

## Context

The archive this tool targets lives on a **slow external HDD**. Every decision
below follows from that one fact, and none of it matters on a local SSD — which
is exactly why none of it was noticed until the storage was named.

**The grid reads the whole file.** ADR-0003's contact sheet demuxes the clip end
to end and drops every non-key packet. Keyframes are only **17%** of the stream
bytes (measured, `camera_8s_4k.mp4`), so 83% of the read is thrown away.

**On a warm local disk the pass is decode-bound, which is misleading.** The
`grid done` line added here reports the split; on the 8 s 4K fixture, steady
state:

| stage | cost |
|---|---|
| `input()` — open and probe | 13 ms |
| decoder setup | 1 ms (**108 ms** on a session's first clip: the shared GPU device of ADR-0009) |
| demux — 19.4 MB | **5 ms, at 3.4 GB/s** |
| decode — 17 keyframes | 22 ms |
| convert — download and scale 9 thumbnails | 31 ms |
| total | 75 ms |

Those 3.4 GB/s are the OS page cache, not a disk. The target material is ~100
Mbit/s, so a 23 s clip is **287 MB**; at an external HDD's ~120 MB/s sequential
the same pass is bound by reading, at roughly **2.5 s**. That figure is an
estimate, not a measurement — the machine with the disk has not yet produced a
log. ADR-0003's 0.42 s for a 23.4 s clip was measured warm and is the best case,
not the typical one.

**Abandoned extraction did not stop.** The worker ignored the send error from its
closed channel, and `extract_grid_streaming` had no way to stop, so every clip
skipped past left a thread reading its file to the end. Locally this is
invisible. On one disk head it is not: holding Right through five clips left five
readers fighting the one clip the user was waiting for.

**Stepping back re-read a clip already built**, at full cost.

## Decision

### Stop reading the moment a clip is abandoned

`extract_grid_streaming` takes `cancel: &AtomicBool` and reads it **once per
demuxed packet** — at the read, not at the thumbnail, because reading the file is
what has to stop. `Loaded` owns the flag and raises it in `Drop`, so every way the
UI lets a clip go — navigating away, a failure, a delete — stops the work, and no
call site can forget to.

Cancelling from the `on_thumb` callback instead cannot work, and the reason is
worth recording. Measured: the demux loop finishes having sent all 17 keyframe
packets with **zero thumbnails produced**. The grid decodes on all cores, and
frame threading buffers ~18 frames before releasing the first, so a clip with
fewer keyframes than that yields nothing until `send_eof` — and the target's clips
have 11–25. By the time the first thumbnail exists, the file is already read.

### Keep grids of clips just visited, budgeted in thumbnails

`App::recent` holds finished grids, newest first; `open` parks the outgoing grid
before it looks, so re-opening the clip being left still finds it. Cells hold an
`egui::TextureHandle`, so a cached grid is just a struct kept alive — the texture
is already on the GPU and nothing is re-uploaded.

The budget counts **thumbnails, not clips** (`RECENT_MAX_THUMBS = 200`, ~46 MB).
A grid holds roughly one thumbnail per second of footage, so a fixed clip count
would mean ~25 thumbnails each for the 10–23 s clips this tool targets but
thousands for a long recording: four one-hour clips would be ~3.3 GB. A grid
larger than the whole budget is simply never cached, which is the right way to
give up on it.

**Only finished grids are parked.** An unfinished one is dropped — which cancels
it — because parking it would leave its worker reading a clip nobody is looking
at: the exact stolen head the cancel flag exists to prevent.

### Read the next sibling ahead, forward only

`App::look_ahead` starts the next sibling's extraction into `App::prefetch` while
the user studies the current grid. Forward only: that is how a folder actually
gets worked through, and stepping back is already free via the cache.

A prefetch is the same `spawn_extraction` as an open — the difference is only
which slot holds it — and its thumbnails wait in the channel until someone polls
them. So `open` **takes over** a prefetch already reading the clip being opened
rather than restarting it, keeping whatever it has pulled off the disk;
`self.prefetch.take().filter(|l| l.path == path)` drops a prefetch of any other
clip right there, cancelling it and handing the disk back at once.

Reading ahead must never compete with the foreground, so it:

- **waits for the current grid to finish** — the clip the user is actually
  waiting for gets the disk to itself;
- **never starts while a clip is playing** — playback itself is safe (4K at 100
  Mbit/s needs 12.5 MB/s of a ~100 MB/s disk), but a seek queued behind a read
  nobody asked for is what makes scrubbing drag;
- **is left to finish once started**;
- **decides once per clip**, not once per repaint — `neighbor` re-scans the
  folder, far too much to redo on every frame.

"Left to finish" reverses the intent this work started with, which was to pause
the read while a clip plays. Cancelling on play would restart the read from zero
on every return to the grid: if glances at a grid are shorter than the read (2 s
against ~2.5 s), the prefetch **never completes** — it churns the disk forever and
never delivers a grid, which is worse than not reading ahead at all. A true pause
would need machinery in `media` (a park flag or condvar) that does not exist. Its
cost is bounded and small: a read that outlives the user's Enter finishes within
seconds, and only scrub latency suffers meanwhile.

## Consequences

- Stepping back through a folder no longer re-reads anything, and stepping
  forward — the dominant motion — finds the next clip already read. On the target
  HDD both are the difference between ~2.5 s and nothing; locally they are the
  difference between ~76 ms and nothing, which is why this cannot be judged from
  a developer machine.
- **Cancellation is effectively immediate**: an abandoned pass yields **1
  thumbnail of 120** (all-intra fixture) instead of 120 — it stops at the very
  next packet.
- **The numbers that decide what comes next are now logged.** Every pass reports
  `grid done` or `grid cancelled` with the open/setup/demux/decode/convert split,
  the bytes read and the throughput behind them, and the keyframe share of the
  stream. A log from the machine holding the archive settles two open questions
  that no local measurement can: whether the pass is I/O-bound there, and whether
  the 17% keyframe share is worth reading by seek.
  - Reading only keyframes — seeking to each rather than demuxing through
    everything — is the obvious next candidate if the log says I/O dominates. It
    beats a disk cache on merit: no invalidation, and it helps the *first* view of
    a clip, which no cache can. Deliberately not attempted yet, because on a warm
    disk the demux it would remove costs 5 ms.
  - A **disk cache** stays rejected for now. It alone survives a restart, but it
    is the only option here that needs a cache key, eviction, versioning and
    invalidation — and it helps nothing on a first pass through a folder.
- **The grid does not fill in progressively on this material, and that is not
  fixed here.** The same measurement that shaped the cancellation point says so:
  with 11–25 keyframes against ~18 buffered frames, nothing appears until the file
  is fully read. On the HDD that means a blank grid for the whole read and then
  every thumbnail at once; `Cell::Pending` and the spinner show nothing. Capping
  the grid's frame threads the way `PLAYBACK_THREADS` caps playback's is the
  obvious lever, but it trades against the throughput ADR-0003 and ADR-0009
  measured, so it is its own decision and needs its own numbers.
- Memory is bounded and modest: ~46 MB of cached textures, plus one prefetch's
  thumbnails waiting in a channel (~5.8 MB for a target clip). A single grid is
  still unbounded for a long clip, exactly as before.
- Deleting a clip now drops its grid immediately, so the cache cannot hold
  textures for a file in the recycle bin that prev/next can never reach again.
- A restored grid keeps its cursor, so returning to a clip resumes where it was
  left. This falls out of caching the whole `Loaded` and is worth keeping.
- `test_videos/allintra_4s_240p.mp4` (`-g 1`, 120 keyframes) joins the fixtures.
  It is the only clip with more keyframes than the decoder has frame threads, so
  it is the only one whose thumbnails arrive *during* the read — which is what
  makes a cancelled pass distinguishable from a finished one at all. Without it
  the central property of this ADR is untestable.
- The read-ahead's rules are pinned by tests rather than by comment, because they
  are invisible locally and a well-meaning edit would not feel wrong:
  `look_ahead_does_not_start_while_playing` and
  `look_ahead_waits_for_the_current_grid_to_finish`. So is the subtle ordering in
  `open` — park before looking, or re-opening the clip being left misses its own
  cache (`open_serves_a_cached_grid_instead_of_re_extracting`).
- Not yet seen on the machine that matters: `grid cancelled`, `grid served from
  cache` and `grid taken over from prefetch` are all covered by tests, and
  `reading ahead` was confirmed in a live run, but the first three need navigation
  to trigger and the local read window is 9–128 ms. They will be plain in a
  tester's log, where the window is seconds.
