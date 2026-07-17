# 0016. Log the storage bus behind a clip

- Status: Accepted
- Date: 2026-07-17

## Context

The tester sent two logs of the same seven clips. One ran at 141–163 MB/s, the
other at 40–46 MB/s — a 4× difference that decided ADR-0015, and the single most
important fact about either log.

Neither log says which drive it read. Both open `E:\Hong Kong Original\`, clip
for clip, same names, same bytes. That the first was an internal disk and the
second an external one is known only because she said so in chat, two hours after
the fact. Had she not, the pair would have read as one machine mysteriously
getting 4× slower between breakfast and mid-morning, and ADR-0015's central
measurement would have been an anomaly to explain away rather than a controlled
comparison.

This is not a one-off. ADR-0010 turned on storage being slow and could not be
judged from a developer machine; ADR-0013 exists to collect what the material
costs "on this machine". The bus is exactly the variable those decisions turn on,
and it is the one thing the log never recorded.

**`GetDriveTypeW` does not answer this.** The obvious call reports `DRIVE_FIXED`
for a USB hard disk, the same as for an internal one — its removable bit
describes removable *media* (a card reader with no card), not a removable drive.
Confirmed locally: both NVMe volumes on the dev machine report `Fixed`, and an
external HDD would join them. A drive type would have made the two logs look
identical all over again.

## Decision

**Log the storage bus** — `USB`, `SATA`, `NVMe` — queried with
`IOCTL_STORAGE_QUERY_PROPERTY` against the volume device (`\\.\E:`) and read from
`STORAGE_DEVICE_DESCRIPTOR::BusType`. This is the field that separates an
external drive from an internal one, and the only one that does.

The volume opens with `dwDesiredAccess = 0`: device metadata and nothing else,
which a standard user is granted. Any read access would demand administrator
rights, which a tester running a portable zip does not have.

**Once per drive per run, not once per clip.** Every line that matters already
carries the path, so one `storage: E: is on a USB bus` makes every `opening
clip: E:\...` after it attributable. Per clip it would be repetition.

**A drive that cannot be identified is passed over silently.** A UNC path, a
volume that will not open, a bus Windows does not name — the line is skipped and
the app is unchanged. This follows ADR-0008's rule that logging never fails the
app.

**`windows-sys` is declared directly, at the version eframe already resolves
(0.61).** It is in the tree via eframe exactly as `log` was, so declaring it adds
nothing to download and the maintainer's offline local build stays intact — the
reasoning ADR-0008 used for the logging facade, and the property it insisted on.
The dependency is `cfg(windows)`-gated, so the crate still builds off Windows,
where the query is a `None`-returning stub — matching `build.rs`, which no-ops
off Windows rather than assuming the platform.

## Consequences

- The two logs that prompted this would have been self-describing, and the next
  pair will be. A tester's report no longer depends on remembering which disk was
  plugged in.
- ADR-0015 can be judged from a log alone: its whole case rests on separating a
  40 MB/s read from a 150 MB/s one, which until now was an out-of-band fact.
- The bus does not distinguish USB 2.0 from USB 3.0, and the 40 MB/s in these
  logs is a USB 2.0 ceiling — a USB 3 external HDD reads at 100–130 MB/s. So the
  line will say `USB` for a drive that may simply be in the wrong port. Naming
  the port would need USB descriptors and a device-tree walk, far past what a log
  line is worth; `USB` plus the throughput the log already prints is enough to
  raise the question with the tester.
- **The USB answer is unverified locally.** The dev machine has only NVMe, where
  the query returns `NVMe`, cross-checked against `Get-PhysicalDisk`. The branch
  that matters most will first run on the tester's machine.
- The line reports the bus and stops there, rather than concluding "external".
  eSATA and Thunderbolt would make that label wrong, and a wrong label is worse
  than a name the reader maps themselves.
- **The dataset does not carry it.** `footage_viewer_clips.jsonl` (ADR-0013) pairs
  material with cost per clip, and the bus is precisely the confounder that made
  two records of the same clip differ 4× — so a `bus` field there is the obvious
  next candidate. Deliberately not taken here: this ADR is about making a log
  readable, and a dataset field is a schema decision that should be made with the
  rest of ADR-0015's stats question, not bolted on ahead of it.
- One more Windows-only surface in the app crate (`app/src/drive.rs`), and the
  first FFI in it. Contained: two calls, one struct field read, no state.
