# 0008. File logging next to the executable for tester reports

- Status: Accepted
- Date: 2026-07-14

## Context

The release build is a Windows GUI app (`windows_subsystem = "windows"`), so it
has no console. Combined with the manual-download-per-feature distribution from
ADR-0007, the tester runs the bundled exe directly and sees nothing when it
misbehaves: panics (e.g. `media::init().expect(...)`) abort silently, decode and
save/delete errors surface only as a transient in-app label, and the GPU/driver
context behind startup and render problems is invisible. There was no way to get
a problem report richer than "it didn't work."

We want a log the tester can attach to a report, capturing panics, errors, and
enough environment context to diagnose issues remotely.

## Decision

**Write a plain-text log next to the executable.** The file lives beside the exe
(`footage_viewer.log`), which the tester can find and send without hunting
through `%LOCALAPPDATA%`. The portable zip is extracted to a writable location
(Downloads/Desktop), so co-locating the log there works; if the folder is
read-only, logging is skipped and the app runs unchanged.

**Keep the last five runs by rotation.** On start, existing files shift up by one
(`footage_viewer.log` -> `footage_viewer.1.log` -> ... -> `footage_viewer.4.log`)
and the oldest is dropped. This preserves the "it worked last time" run without
letting the folder grow without bound.

**Roll our own `log::Log` sink rather than add a logger crate.** The `log` facade
is already in the tree (eframe/wgpu depend on it), so declaring it directly adds
nothing to download — important because the maintainer builds locally and an
offline build must not break. A file-writing sink, five-file rotation, a panic
hook, and a UTC timestamp are together small enough (`app/src/logging.rs`) that
pulling in `flexi_logger`/`simplelog` (and, for timestamps, `chrono` — which is
not in the active dependency tree) would cost more in dependencies and offline
risk than it saves in code. Timestamps are UTC, hand-formatted, so no date crate
is needed and times are unambiguous across machines.

**Capture panics with a backtrace.** A panic hook logs the message, location, and
`Backtrace::force_capture()` (no `RUST_BACKTRACE` needed on the tester's machine),
flushes, then chains the previous hook so debug builds still print to the console.

**Route eframe/wgpu diagnostics into the same file at `Info`.** Because those
crates log through the `log` facade, setting the max level to `Info` records the
selected GPU adapter, backend, and driver versions — directly relevant to the
known startup-delay / GPU-init behavior — while dropping wgpu's chatty
debug/trace stream. The app itself logs sparingly: start (version/OS/arch),
clip opens, extraction/playback errors, and still-save/delete outcomes.

Writes are unbuffered. The app logs infrequently, and the global logger is leaked
as `'static` (`set_boxed_logger`) so it never drops — a buffer would neither
flush on exit nor survive a hard crash, defeating the purpose.

## Consequences

- The tester can attach `footage_viewer.log` (plus the numbered previous runs)
  to a report; panics that used to vanish now leave a message, location, and
  backtrace on disk.
- GPU adapter and driver details are captured on every run, aiding remote
  diagnosis of render/startup issues without a special build.
- No new dependency is downloaded and no date/time crate is added; the offline
  local build stays intact.
- Release backtraces carry function names and source lines: the workspace release
  profile sets `debug = "line-tables-only"`, so the tester's log shows a readable
  stack (e.g. `footage_viewer::main at app\src\main.rs`), not bare addresses. The
  cost is a larger exe (line tables only, no full variable debuginfo).
- Logging never fails the app: an unopenable log file is silently skipped.
