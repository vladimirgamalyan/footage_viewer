# 0006. Distribute to the tester via GitHub Release self-update

- Status: Accepted
- Date: 2026-07-13

## Context

The app is handed to a single non-technical tester after every feature. Each
build is a ~14 MB `footage_viewer.exe` plus ~120 MB of FFmpeg runtime DLLs that
almost never change (ADR-0001). Re-zipping and re-sending the whole ~52 MB bundle
per feature, then walking the tester through download/extract/run, is friction on
both sides: the tester can extract to the wrong place, run the exe away from its
DLLs, or simply not bother.

We want the tester's per-feature cost to be zero — launch the app, get the latest
build — while keeping the maintainer's cost to one command.

## Decision

**Self-update from GitHub Releases on startup.** On release-build launch,
`app/src/update.rs` asks the public repo for its latest release via the
`self_update` crate. If the release tag is newer than the compiled
`CARGO_PKG_VERSION`, it downloads the Windows asset, replaces the running exe in
place (`self_replace`), and relaunches. It is gated to
`#[cfg(not(debug_assertions))]` so dev builds never hit the network. Every failure
(offline, GitHub unreachable, rate-limited) is swallowed — the tester keeps
running the current version.

**The check runs behind an in-window overlay, not a pre-window block.** The check
and download run on a background thread while the window shows a spinner and a
phase label ("Checking for updates…" → "Downloading update…"), so the tester
always sees motion instead of a silent multi-second hang before the window
appears. When an update lands, the overlay reads "Updated to vX. Restarting…" for
a beat and the app relaunches itself automatically — no dialog and no click. The
normal UI (and any file passed on the command line) is held until the check
resolves. `self_update` exposes no numeric download progress outside its terminal
progress bar, so the indicator is an indeterminate spinner, not a percentage.

**Only the exe ships per feature.** The DLLs live next to the exe and are left
untouched, so a feature update is the ~14 MB exe (7.5 MB zipped), not the full
bundle. The tester gets the DLLs once, in a one-time full bundle.

**The source repo is public.** `self_update` reads public releases with no auth,
so no token is embedded in the distributed binary. The repo was scanned (working
tree + full history) for secrets before flipping visibility; it was clean, only
carrying the author's normal git identity.

**Release layout, produced by `scripts/publish.ps1`:**
- Tag `vX.Y.Z`, where `X.Y.Z` is `app/Cargo.toml`'s version (bump it to release).
- `footage_viewer-X.Y.Z-x86_64-pc-windows-msvc.zip` — the exe-only update asset.
  `self_update` matches an asset by finding the host **target triple** in its
  name, so the triple must be present and must equal `TARGET` in
  `app/src/update.rs`. The zip holds `footage_viewer.exe` at its root, because the
  crate always unpacks the asset as an archive (a bare exe is not accepted).
- `footage_viewer-X.Y.Z-win64-full.zip` (only with `publish.ps1 -Full`) — the
  one-time exe + DLLs bundle. Its name deliberately omits the target triple so the
  updater never selects it instead of the exe-only asset.

## Consequences

- The tester launches once per feature and is on the newest build; the maintainer
  runs `./scripts/publish.ps1` (plus `-Full` on first release or when DLLs
  change). The full ~68 MB bundle is downloaded by hand only once.
- The test binary must avoid the words install/setup/update/patch in its filename:
  Windows installer detection forces a UAC elevation prompt on such names. The
  shipping `footage_viewer.exe` is unaffected; a throwaway verifier named
  `selfupdate_check` was, and had to be renamed.
- The exe is unsigned, so SmartScreen shows "Windows protected your PC" on first
  run of each new hash. For one tester this is a one-time "More info → Run anyway".
  Code signing (e.g. Azure Trusted Signing) would remove it but is out of scope.
- Adds `self_update` → `reqwest`/`tokio` to the dependency graph (bigger release
  build, native-tls via Windows SChannel, no OpenSSL). Acceptable next to FFmpeg
  and eframe.
- The update check runs off the UI thread, so it never blocks the window from
  opening; the tester waits behind the spinner overlay only when an update is
  actually downloading, and offline it fails fast and drops straight to the app.
- Distribution now depends on the repo staying public and on GitHub Releases.
  Reversing to a private repo would require an embedded token or a different host.
