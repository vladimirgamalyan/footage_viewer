# 0013. A clip-stats dataset alongside the log

- Status: Accepted
- Date: 2026-07-16

## Context

Almost every performance decision in `media` is a bet on what the footage looks
like:

- `HW_MIN_PIXELS` (ADR-0009) sends frames above ~2.1 MP to the GPU, betting on
  resolution.
- `FORWARD_SCRUB_LIMIT_S` decides when a scrub decodes forward instead of seeking
  back, betting on GOP length.
- `SCRUB_CACHE_BYTES` (ADR-0012) sizes the cache in seconds of stepping, betting
  on frame size.
- The grid's keyframe sampler (ADR-0003) derives its skip factor from the *first*
  keyframe interval, betting the GOP is near-constant.

The evidence behind those bets is thin and known to be unrepresentative. The dev
fixtures are synthetic: `camera_8s_4k.mp4` stands in for the target archive at a
0.48 s GOP, while the tester's camera writes ~1 s — 2.5× wider, which changes how
many frames a seek decodes and therefore whether the numbers in ADR-0009 transfer
at all. Everything else we know came from reading one tester's log by hand.

The log (ADR-0008) cannot become that evidence. It is prose meant to be read
after a problem, it holds one session, and it rotates away — by design. What is
missing is the opposite: a small, durable, machine-readable record of *what the
material is*, accumulated from the machines the tool actually runs on.

Separately, five kept runs (ADR-0008) proved too few in practice. The app is
launched per clip rather than left running, so a run worth reporting had usually
rotated out before the tester got round to describing it.

## Decision

**Collect the stats inside the grid pass.** The pass already opens the clip,
demuxes it end to end, and looks at every packet's keyframe flag, size, and
timestamp — a clip's format and its whole keyframe layout are a by-product of
work being done anyway. No probe, no second read, nothing added to the disk this
tool is slow on. `extract_grid_streaming` returns a `ClipStats`.

**Record what the material is, plus what it cost.** Container, codec, profile,
level, resolution, pixel format, frame rate, duration, measured video bitrate,
whether the stream has B-frames; then the keyframe layout — count, and spacing as
min/mean/max in both frames and seconds; then the pass's own timings and whether
libav decoded on the GPU or quietly fell back to the CPU. Format and cost live in
one record on purpose: each line is then a *pair* — this material cost that on
this machine — which is what a constant can be set from. Split across two files
they would have to be re-joined by hand.

Spacing is kept as min/mean/max, not a mean, because the sampler's constant-GOP
assumption is exactly what a spread would disprove. It is kept in both frames and
seconds because the two only agree at a constant frame rate, and they bound
different costs: frames bound the decode to reach a target, seconds bound how far
back a seek lands.

**One JSON line per pass, appended next to the executable, never rotated.**
`footage_viewer_clips.jsonl` sits beside `footage_viewer.log`, so a tester
collects from one folder. It is a dataset, not a log: it survives every run and
grows ~400 bytes per clip opened, which is tens of KB over hundreds of clips.

JSON Lines rather than CSV so a field can be added later without stranding the
records already collected, and so the nested spread reads naturally. It is
hand-formatted with one `format!`, for the same reason ADR-0008 hand-rolled its
timestamp: neither `serde` nor `serde_json` is in the active dependency tree, and
the maintainer's offline local build must not start needing them.

**Every pass writes a line, including read-aheads and re-opens.** Duplicates are
not deduplicated. The same clip read twice is a feature: the difference between
the two records is the OS file cache, which is precisely how a slow disk is told
apart from a slow decode.

**A cancelled pass writes nothing.** Its counts describe the fragment it read,
not the clip, and would enter the dataset indistinguishable from real footage
that is short and sparsely keyframed.

**Keep twenty runs of the log, up from five.** This revises the retention decided
in ADR-0008; the rest of that ADR stands.

## Consequences

- The constants above can be revisited against the footage the tool is pointed
  at, rather than against fixtures known to differ from it. `pandas.read_json(...,
  lines=True)` or `jq` reads the file directly.
- Collection is free at runtime: no extra open, seek, read, or decode.
- The dataset carries no folder layout — file names only, not paths — so a tester
  can send it on without shipping their directory tree.
- Two files to ask a tester for instead of one. They are next to each other and
  next to the exe, which is the reason ADR-0008 put the log there.
- The record describes only clips whose grid was read to the end. A clip the user
  abandons mid-pass never appears, so the dataset skews toward footage that was
  actually looked at — which is the footage worth optimizing for.
- Nothing is escaped when writing JSON, which holds only because the text fields
  are libav identifiers and Windows file names (Windows forbids `"`, `\`, and
  control characters in a name). A field carrying free text — a path, an error —
  would need a real encoder first.
- Log folders now hold up to 20 files. Still tens of KB a run, so the cost is
  nil, and "it worked last week" survives long enough to be reported.
