# 0015. Seek to the keyframes instead of reading past them

- Status: Accepted
- Date: 2026-07-17

## Context

ADR-0010 named the measurement that would decide this and declined to guess
without it:

> A log from the machine holding the archive settles two open questions that no
> local measurement can: whether the pass is I/O-bound there, and whether the 17%
> keyframe share is worth reading by seek.

Two logs from the tester's machine now answer both. They are close to a
controlled pair: the same seven clips, opened first from an internal disk and
then from the external drive the archive lives on. (Which log is which came from
the tester in chat, not from the logs — both read `E:\Hong Kong Original\`. That
gap is ADR-0016's problem, not this one's.)

**The pass is I/O-bound on the target storage, decisively.** Every `grid done`
line from the external drive, steady state:

| clip | total | demux | read | throughput | keyframes |
|---|---|---|---|---|---|
| P1900101 | 4111 ms | **3192 ms** | 135.4 MB | 42 MB/s | 35.4 MB (26%) |
| P1900102 | 6126 ms | **4624 ms** | 186.6 MB | 40 MB/s | 58.7 MB (31%) |
| P1900103 | 7065 ms | **5949 ms** | 235.7 MB | 40 MB/s | 72.8 MB (31%) |
| P1900104 | 6860 ms | **5926 ms** | 240.4 MB | 41 MB/s | 71.8 MB (30%) |
| P1900105 | 5784 ms | **4844 ms** | 202.4 MB | 42 MB/s | 54.8 MB (27%) |
| P1900106 | 5053 ms | **4360 ms** | 173.5 MB | 40 MB/s | 48.6 MB (28%) |
| P1900107 | 6463 ms | **5508 ms** | 225.6 MB | 41 MB/s | 62.9 MB (28%) |

Demux is 75–84% of the pass. Against it, decode is 33–91 ms and convert is
85–217 ms. The work this tool does is noise; the file it reads is everything.

**ADR-0010's estimate was 3× optimistic, and said so.** It assumed "an external
HDD's ~120 MB/s sequential ... at roughly 2.5 s", flagged as an estimate with no
log behind it. Measured: 40–46 MB/s, flat across every clip, and 5–7 s per grid.
The same clips off the internal disk ran at 141–163 MB/s — `P1900101` demuxed in
833 ms there against 3192 ms here, same file, same 24 keyframes. The target
storage is roughly a quarter of the speed the design assumed.

**The keyframe share is larger than the fixture said.** ADR-0010 measured 17% on
`camera_8s_4k.mp4`. On the tester's footage it is 26–31%. So the waste is ~3.3×,
not ~6× — the ceiling on this change is lower than the fixture suggested, and it
is still most of the read.

**The tester compared us to VLC, and the comparison is diagnostic rather than
unfair.** Her report: VLC does not slow down on the external drive the way this
tool does. The logs say why. `P1900104` is 240.4 MB over ~42 s of footage —
~5.7 MB/s of stream. VLC needs that 5.7 MB/s of the 40 MB/s available, a
four-fold margin, so the slow drive barely touches it. Opening the same clip here
reads all 240.4 MB before a single thumbnail exists: 6 s, entirely in the
critical path. The gap she feels is not decode and not the GPU. It is that we
read the whole file to show 30% of it.

## Decision

Acquire the keyframe packets by **seeking to each one**, rather than demuxing
every packet and dropping the non-key ones at the send stage.

- **Enumerate keyframes from the container index, not by reading.** An MP4's
  `moov` lists every sample's timestamp, size and keyframe flag, and the demuxer
  has already parsed it into an index by the time `input()` returns — that is the
  `open` stage the log reports at 330–1226 ms. The targets are known before a
  byte of media is read. (Measured, not assumed — see the index table below.)
- **Seek per kept keyframe, decode it alone.** For each target, seek and read
  forward to the first key packet at or past it, and send that packet by itself.
- **Search forward, not backward.** This is the one part that did not survive
  contact with the implementation, and it is worth recording because it is
  invisible and it fails quietly. An index entry's timestamp is the sample's
  *decode* time, but libav matches a seek target against its *presentation* time,
  shifting the target by the stream's reorder offset before searching the index.
  So `AVSEEK_FLAG_BACKWARD` on a keyframe's own index timestamp asks for a moment
  that falls just short of that keyframe and returns **the one before it**. On the
  4K fixture — keyframes 6144 ticks apart, a 1024-tick offset — every seek came
  back exactly one GOP early. Searching forward from the same timestamp lands on
  the keyframe itself, because a sample's decode time never runs ahead of its
  presentation time; this holds while the reorder offset is shorter than a GOP,
  which is the assumption ADR-0003 already makes by sampling on keyframes at all.
  The failure is silent by nature: a mis-landed seek still yields a full sheet of
  real frames, each labelled with its true time — just of the wrong moments. Only
  the fixture's known ~0.96 s cell spacing caught it.
- **ADR-0003's thinning is unchanged in effect.** `KeyframeSampler`'s skip factor
  now selects which enumerated keyframes to fetch, rather than which packets of a
  stream being walked to keep. On this material `N = 1` either way.

**This is not a return to ADR-0002, and the distinction is the whole point.**
ADR-0002 seeked to arbitrary evenly-spaced targets and then decoded forward
through ~half a GOP of 4K to reach each one; that forward decode is what cost
3.8 s on a 16 s clip and what ADR-0003 removed. This seeks to the keyframes
themselves and decodes nothing but them. The two ADRs each measured a real cost
on a warm local disk, where reading is free: ADR-0002 was right about the bytes
and wrong about the decode, ADR-0003 was right about the decode and never saw the
bytes. Neither was wrong on its own evidence. This takes the seek from one and
the keyframe-only decode from the other.

## Consequences

- **The read drops to the keyframe bytes** — 26–31% here. On `P1900103` that is
  235.7 MB down to 72.8 MB.
- **Thinned keyframes stop being decoded, not just dropped.** A side effect of the
  sampler moving ahead of the fetch: where `N > 1`, the keyframes it skips are now
  never read and never sent to the decoder, where before every one was decoded and
  the surplus discarded at the drain. Worth nothing on the tester's footage, where
  `N = 1`; on the 4K fixture it halves the decode, 17 keyframes down to the 9 the
  sheet shows. It changes no output.
- **The byte ratio is a bound, not a promise, and the shortfall is HDD head
  time.** The pass stops being a sequential stream and becomes a skip-scan:
  ~1.5 MB read, ~4 MB skipped, 21–42 times per clip. The gaps are far wider than
  any readahead window, so the bytes are genuinely not read; but each seek costs
  head movement the current pass never pays. Estimating ~12 ms a seek, P1900103's
  demux lands near 1800–2500 ms rather than the 1800 ms the ratio alone implies —
  still a 2.5–3× win, and still an estimate. This ADR is proposed on measured
  waste, not on a measured gain; the gain needs the tester's log, exactly as this
  ADR's own premise did.
- **ADR-0013's stats survive whole, and stop needing the read at all.** The
  worry was that its record is a by-product of walking every packet: `gop_frames`
  is counted as packets between key packets, and a keyframe-only read has no
  packets in between. That field bounds `FORWARD_SCRUB_LIMIT_S`, and ADR-0013
  keeps spacing in frames *and* seconds precisely because the two "only agree at
  a constant frame rate" — the assumption the dataset exists to test — so
  deriving it as `gop_s × fps` would have made the dataset assume what it checks.

  Measured instead of assumed: the container index holds **every sample**, not
  just the keyframes, and it is built by the demuxer at header time — before a
  byte of media is read.

  | fixture | index entries | keyframe entries | Σ entry sizes | file |
  |---|---|---|---|---|
  | `camera_8s_4k.mp4` | 200 | 17 | 19.4 MB | 19.4 MB |
  | `allintra_4s_240p.mp4` | 120 | 120 | 0.6 MB | 0.6 MB |
  | `scenes_18s_720p.mp4` | 540 | 3 | 23.2 MB | 23.2 MB |

  Each `AVIndexEntry` carries `timestamp`, `pos`, `size`, and an
  `AVINDEX_KEYFRAME` flag, so the whole record comes off the index: `gop_s` from
  the timestamps, `gop_frames` from the entries between flagged ones, the
  keyframe count from the flags, and both stream bytes and keyframe bytes from
  the sizes — which sum to the file to within a rounding step in all three
  fixtures. Nothing is lost, and the collection gets strictly cheaper: today it
  is free only because a 5-second read is happening anyway; from the index it
  costs nothing at all, and the keyframe *share* is known before the first seek
  rather than after the last packet.

  Reachability, the thing that had to be checked first, is settled.
  `ffmpeg-next`'s safe wrapper exposes no index access whatsoever, but
  `ffmpeg-sys-next` 8.1 binds all three of `avformat_index_get_entries_count`,
  `avformat_index_get_entry` and `avformat_index_get_entry_from_timestamp`, and
  `ffmpeg::sys` is already how this crate reaches `AV_NOPTS_VALUE`. The cost is
  that the index walk is `unsafe` and hand-written against raw `AVStream`
  pointers.
- **It needs a container with an index, and the index shape is only
  fixture-verified.** MP4 has one and the archive is all MP4, so the table above
  should hold on the tester's Panasonic files: it is the demuxer that builds the
  index out of `stsz`/`stco`/`stss`, and that path does not care what wrote the
  file. But ADR-0013 exists because fixtures mislead, and these three are
  synthetic — the count is worth confirming once against real footage. The stats
  are moved onto the index now rather than after that confirmation, on one piece
  of evidence the fixtures can supply: the index puts `camera_8s_4k.mp4`'s
  keyframes at **17.0%** of the stream, which is the same 17% ADR-0010 measured by
  weighing every packet it read. The sizes in the index are the sizes on the disk.
  A fragmented MP4 would index per fragment, and a source with no index at all
  (raw MPEG-TS) would make ffmpeg scan to build one, which is the read this
  removes; such material would have to fall back to today's linear pass, and
  nothing in the tree needs that yet — so a clip whose index is empty is reported
  as an error rather than drawn as an empty sheet, the two being indistinguishable
  from the outside.
- **ADR-0003 is not superseded.** The contact sheet stands: keyframes as the
  sampling grid, a dynamic thumbnail count, thinning by skip factor, and the dead
  end that `skip_frame = AVDISCARD_NONKEY` does not avoid the work. Only how the
  keyframe packets are obtained changes.
- **ADR-0010's cancellation still works and gets cheaper.** The flag is read once
  per demuxed packet today; per seek it is read at least as often against far less
  outstanding I/O, so an abandoned clip lets the head go sooner than the "very next
  packet" that ADR already achieves.
- **The blank grid stays blank.** ADR-0010 recorded that nothing appears until the
  file is read, because frame threading buffers ~18 frames and the clips have
  11–25 keyframes. Seeking makes that wait 2.5–3× shorter without changing its
  shape: still nothing, then every thumbnail at once. Capping the grid's frame
  threads remains its own decision.
- **`grid done`'s throughput figure changes meaning.** Today `demux N ms (X MB, Y
  MB/s)` describes a sequential read. After this it describes a skip-scan, where
  MB/s is bytes delivered per second of head time and is no longer comparable to a
  disk's sequential rating — nor to the numbers in the table above. The line should
  say which it is, or the next person to read a log will compare the two and
  conclude the disk got slower. It now reads `skip-scan N ms (X MB, Y MB/s)`.
- **ADR-0013's dataset renames the two fields this changes.** The log is rotated
  after twenty runs; the dataset is kept for good, so it carries the same hazard
  for longer. `demux_ms`/`read_mb_s` become `scan_ms`/`scan_mb_s`. Records written
  before this keep the old names *and* the old meaning, which is the point: a
  reader gets two sparse columns rather than one dense column that silently
  averages a sequential read together with a skip-scan. This is the only safe way
  to change what a field measures, and the format was chosen to tolerate it —
  ADR-0013 picked JSON Lines so the schema could move without stranding records.
