# 0007. Remove in-app self-update; distribute via manual GitHub download

- Status: Accepted
- Date: 2026-07-13

## Context

ADR-0006 added startup self-update from GitHub Releases so the tester's
per-feature cost would be zero. It shipped the exe alone per feature and left the
FFmpeg DLLs untouched, on the assumption that they almost never change.

That split has a fragile edge. The exe and its FFmpeg DLLs must stay ABI-compatible
(`ffmpeg-next` links at load time, so the DLLs are mapped before `main()`). An
exe-only update against a changed DLL set would leave a new exe paired with old
DLLs — a loader error before `main()` or a crash on first FFmpeg call — and the
broken exe can no longer self-update to recover. Avoiding that requires either the
maintainer manually deciding when to ship the full bundle (`publish.ps1 -Full`) and
telling the tester to reinstall by hand, or a cross-process helper that swaps the
locked DLLs. Both add coordination or machinery we don't want to carry right now.

Download bandwidth is not a constraint here (one tester, fast connection), which
removes the original reason to ship the exe alone.

## Decision

**Remove the in-app self-updater and hand distribution back to the tester.** Delete
`app/src/update.rs`, its wiring in `app/src/main.rs` (the startup overlay, progress
drain, and relaunch), and the `self_update` dependency (which also drops
`reqwest`/`tokio` from the release build). The app no longer touches the network on
startup.

**Publish one full bundle per release.** `scripts/publish.ps1` is simplified to
always build and attach a single `footage_viewer-<version>-win64-full.zip`
(exe + FFmpeg DLLs). The exe-only asset, the target-triple in the asset name, and
the `-Full` switch existed only for the self-updater's asset matching and are gone.

**The tester updates by hand.** They download the latest full bundle from the
Releases page, extract it, and run the exe — exe and DLLs always move together, so
the compatibility hole is closed by construction.

This is deliberately a step back to a simpler manual flow ("пока" — for now). If the
per-feature manual download becomes friction again, the path forward is the
single-path helper swap explored while deciding this (stage the full bundle, relaunch
the new exe from staging with an `--apply-update` flag so it can overwrite the locked
DLLs of the exited process), not a return to exe-only self-update.

## Consequences

- No exe/DLL mismatch can brick the install, and the maintainer no longer decides
  per release whether DLLs changed.
- The release build is smaller and has no TLS/HTTP stack; startup does no network I/O.
- The tester pays a manual download + extract per feature again — the cost ADR-0006
  set out to remove. Accepted as a temporary tradeoff.
- `scripts/publish.ps1` now produces exactly one asset; `dist/` stays git-ignored.
- The notes in ADR-0006 about unsigned-binary SmartScreen prompts and avoiding
  install/setup/update/patch in asset filenames still apply to the manual bundle.
