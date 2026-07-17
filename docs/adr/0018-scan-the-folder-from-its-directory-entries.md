# 0018. Scan the folder from its directory entries, not a stat per file

- Status: Accepted
- Date: 2026-07-17

## Context

The tester works a folder of ~2000 clips on the external drive and reports that
everything about it is slow: "оооочень медленно всё на жестком, потому что в
папке 2000 файлов". Every performance decision so far has been about reading
clips, and the log could neither confirm nor deny her — the folder scan was the
one path with no measurement on it at all.

`sibling_videos` re-scans that directory on every navigation and every
look-ahead (ADR-0010 already cut it from once per repaint to once per clip, which
is as far as it went). Its filter was `p.is_file() && is_video(p)`, and
`Path::is_file` is a fresh metadata call per entry — a full open/query/close
against the file system. In a 2000-file folder that is 2000 round trips, each one
its own request to a USB disk, to answer a question the directory enumeration had
already answered.

Measured on a warm 2000-file folder on NVMe:

| pass | metadata call per entry | attributes from the enumeration | ratio |
|---|---|---|---|
| 1 | 26.9 ms | 0.6 ms | 45× |
| 2 | 27.1 ms | 0.5 ms | 58× |
| 3 | 26.1 ms | 0.5 ms | 56× |

That is the *warm, local, best case*. The cost scales with per-call latency, and
on a cold USB disk each of those 2000 calls is a round trip rather than a memory
hit — which is the shape of a complaint about a big folder being slow.

## Decision

**Take the file type from the `DirEntry`.** On Windows `DirEntry::file_type` is
free — the attributes come back with the directory enumeration already being
walked — while `Path::is_file` re-asks the file system per entry.

**Check the extension first**, so even that free question is only asked about
plausible clips.

**Test for "not a directory" rather than "is a file".** `is_file` returned true
for a symlinked clip because it follows the link; `file_type` does not, and would
have reported a symlink and dropped the clip. Excluding directories keeps the old
answer for every input that matters and changes it only for a symlink pointing at
a directory named like a clip, which then fails to open instead of being filtered
out.

**Log what the scan cost**: `folder scan: 1987 clips of 2000 entries in 412ms`.
The count and the cost together are what say whether a big folder is a problem;
neither alone does.

## Consequences

- The scan drops from ~2000 file-system calls to one enumeration. Locally that is
  27 ms → 0.5 ms per navigation; on the tester's disk it is the difference between
  a stat storm and a directory read, and her log will now put a number on it.
- Her report becomes checkable. If the scan is still slow with the storm gone, the
  next lever is caching the listing per folder — but ADR-0010's reason for
  re-reading it (clips appear while the app is open) would have to be answered
  first, so that is its own decision and wants this measurement before it.
- The scan is logged on every navigation, roughly two lines per clip. That is the
  point: the pattern over a session is what shows whether it is the first scan that
  is slow (cold directory) or all of them.
- One behaviour change, deliberate and narrow: a symlink to a directory named
  `*.MP4` is now offered to the decoder rather than filtered out, and fails there.
  No such thing exists in a camera folder.
