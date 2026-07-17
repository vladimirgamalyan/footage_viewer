# 0017. Report the open as header plus probe

- Status: Accepted
- Date: 2026-07-17

## Context

Opening a clip costs the tester 332–1226 ms, median ~650 ms. After ADR-0015 cuts
the read, that is a quarter of the whole pass — and it is 100% of the time before
anything can appear on screen, since nothing else starts until it returns.

The obvious explanation was that `avformat_find_stream_info` reads up to its
default 5 MB `probesize` and 5 s `analyzeduration` to re-derive what an MP4's
`moov` already declares, and that capping those would take the read off the
critical path. **Both halves of that are measured false.**

Capping does nothing. On the 4K fixture:

| | default | probesize 500 KB | probesize 100 KB |
|---|---|---|---|
| `avformat_open_input` | 0.9 ms | 0.3 ms | 0.3 ms |
| `avformat_find_stream_info` | 13.6 ms | 12.8 ms | 12.7 ms |

FFmpeg stops early on its own; it never reaches the cap, so lowering the cap
moves nothing.

And the cost is not the disk at all. Comparing the two logs of the same seven
clips:

| | open, median | range |
|---|---|---|
| internal drive (~150 MB/s) | ~630 ms | 334–737 ms |
| external drive (~40 MB/s) | ~690 ms | 332–1226 ms |

A stage that reads would be ~4× slower on the slow disk. This one is flat. So
whatever the tester's ~650 ms is, it is not reading — and locally the whole call
is 14 ms, 46× less, which no amount of staring closes.

One thing the measurement did settle: `avformat_open_input` alone already yields
resolution, frame rate, codec, `avcC` extradata, and the **entire sample index**
(200 entries on the 8 s fixture). `find_stream_info` adds only `profile`,
`level`, `pix_fmt` and `duration`. So skipping it outright is a real candidate —
but it is a candidate for a decision that needs to know where the 650 ms sits,
and right now nothing does.

## Decision

**Report the two halves rather than their sum.** `grid done` and `still ->` now
carry `open 690ms (header 30ms + probe 660ms)`: `open` keeps its old meaning so
the logs already collected stay comparable, and the split says which call it was.

`open_timed` in `media` mirrors what `ffmpeg::format::input` does — the two libav
calls back to back — because `input()` reports only their sum and there is no way
to time the halves through it.

**Nothing is capped and nothing is skipped.** Capping is measured useless.
Skipping `find_stream_info` would trade away four fields on a guess about which
stage is slow, and the guess is exactly what this ADR exists to stop making.

## Consequences

- The next log from the tester says whether the ~650 ms is the header read or the
  probe, and the fix follows from the answer rather than preceding it. If it is
  the probe, skipping `find_stream_info` becomes a real proposal with the four
  lost fields as its known price. If it is the header, the moov is being read the
  hard way and that is a different problem entirely.
- This is the same move ADR-0010 made for demux: it shipped the `grid done` split
  precisely because "a log from the machine holding the archive settles two open
  questions that no local measurement can". That worked; the split answered them.
- The log line grows. `open` still leads it, so a reader who does not care about
  the split can skip the parenthesis.
- `media` now opens the input itself, through `ffmpeg::sys`, rather than through
  the safe wrapper. It is the same two calls in the same order — the wrapper's own
  source, with timers between — but it is `unsafe`, and a future ffmpeg-next that
  changed what `input()` does would not change this.
- Playback keeps calling `input()`: it reports no open timing, so splitting it
  there would add unsafe code for a number nobody reads.
