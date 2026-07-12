# 0001. FFmpeg integration and build strategy

- Status: Accepted
- Date: 2026-07-12

## Context

The viewer decodes video through `ffmpeg-next`, Rust bindings to the native
FFmpeg C libraries (`avformat`, `avcodec`, `avutil`, `swscale`, `swresample`).
These libraries must be available at both build time (headers + import
libraries) and run time (shared libraries).

The primary development and target platform is **Windows 11**, where obtaining and
linking native FFmpeg is the main source of friction: MSVC-compatible import
libraries, matching DLLs, an ABI that matches the `ffmpeg-next` version, and
FFmpeg's licensing split (LGPL core vs. GPL-only components).

Requirements that constrain the choice:

- **In-process, frame-level decode** is required — the player seeks and paces
  individual frames for playback, and the grid extracts individual frames at
  chosen timestamps. A process-per-frame model is not acceptable for playback.
- **Broad format coverage** (H.264/H.265, ProRes, DNxHD, common camera
  containers) — all decode-only.
- **No encoding** anywhere in the product (v1 or planned).
- Contributors and CI must be able to reproduce the build from a documented
  setup.

### Alternatives considered

1. **`ffmpeg-next` + prebuilt shared FFmpeg dev libraries**, located via the
   `FFMPEG_DIR` environment variable; ship the matching DLLs next to the
   executable.
2. **`ffmpeg-next` + FFmpeg managed by `vcpkg`** — reproducible, but a heavier
   first build and slower CV/CI.
3. **`ffmpeg-sidecar` (bundle `ffmpeg.exe`, shell out)** — trivial to set up and
   no linking, but only exposes the CLI. Real-time seeking + frame pacing for
   playback is impractical over a subprocess, and we want a single decode path
   shared by the grid and the player.

## Decision

Use **`ffmpeg-next` linked against a prebuilt shared, LGPL-licensed FFmpeg
build**, discovered at build time via the `FFMPEG_DIR` environment variable, and
distribute the matching FFmpeg shared DLLs alongside the application binary.

Specifics:

- **Licensing: LGPL, not GPL.** We only decode, so we do not need the GPL-only
  encoders (x264, x265). Use an LGPL FFmpeg build (e.g. the LGPL variant of the
  BtbN `FFmpeg-Builds` shared packages, or an LGPL `vcpkg` build). Keep FFmpeg
  **dynamically linked** so it remains replaceable by the end user, as LGPL
  requires, and ship FFmpeg's license text with the application.
- **Version pinning.** Pin the FFmpeg major/minor version to the range supported
  by the chosen `ffmpeg-next` release to avoid ABI mismatches. Record both
  versions together whenever either is bumped.
- **Developer setup.** Document in `README.md`: download the LGPL shared dev
  package, extract it, set `FFMPEG_DIR` to its root, and ensure the `bin` DLLs
  are on `PATH` (or copied next to the built binary) for run time.
- **CI.** Cache the same FFmpeg dev package as a CI artifact and export
  `FFMPEG_DIR` before building.
- **`vcpkg` is the documented fallback** for reproducible/from-source builds; it
  is not the default because of first-build cost.
- **`ffmpeg-sidecar` is rejected** as the primary path because it cannot serve
  in-process playback; it may still be reconsidered only if a future feature
  needs CLI-only batch processing.

## Consequences

**Easier:**

- Full decode format coverage and a single in-process decode path shared by the
  grid extractor and the playback decoder.
- A clear route to hardware-accelerated decode later (native API access).
- Simple licensing story: decode-only + LGPL + dynamic linking.

**Harder / obligations:**

- The build environment must have `FFMPEG_DIR` set and the correct DLLs present
  at run time; this is an extra, documented setup step for every contributor and
  for CI.
- Distribution must **bundle version-matched FFmpeg DLLs**; a mismatch between
  the DLLs and the `ffmpeg-next` build causes load-time or ABI failures.
- We must stay **LGPL-clean**: no GPL components pulled in, FFmpeg kept
  dynamically linked and replaceable, and license texts shipped.
- Bumping `ffmpeg-next` or FFmpeg requires re-validating the version pairing.

A follow-up ADR may cover hardware-decode backend selection (e.g. D3D11VA /
DXVA2 on Windows) once basic decode is in place.

## Validation (spike + sibling project, 2026-07-12)

A spike (`spike/`) confirmed the decode path end to end: `ffmpeg-next` opens H.264
clips and extracts a correct, evenly-spaced grid of thumbnails on this Windows
machine. The sibling project `D:\projects\vievo` — a working frame-accurate player
— independently uses the same stack (`ffmpeg-next 8.1`, `eframe`, `image`),
corroborating the choice. Refinements (they sharpen, not reverse, this decision):

- **Match the crate's `bindgen` to the installed `libclang`, not just FFmpeg.**
  `ffmpeg-next 7.1` (→ `bindgen ^0.70`) fails against libclang 22 by emitting
  opaque structs and breaking layout assertions; `ffmpeg-next 8.1` (→ `bindgen
  ^0.72`) builds cleanly. Pin the `ffmpeg-next` line to one whose bindgen supports
  the target libclang (or install a matching LLVM).
- **Build and run from a native Windows shell (PowerShell), not Git Bash.** Git
  Bash shadows the MSVC linker with its own `/usr/bin/link.exe` and mis-resolves
  the UCRT api-set DLLs at runtime. From PowerShell, rustc auto-detects the MSVC
  linker — no "Developer Shell" needed.
- **Wire the build via `.cargo/config.toml` `[env]`** (git-ignored, with a
  committed `.example`): `FFMPEG_DIR` → an extracted shared/dev build kept in a
  git-ignored `vendor/`, `LIBCLANG_PATH` → the LLVM `bin`. Then a plain `cargo
  build` works. Put the FFmpeg `bin` DLLs on `PATH` to run (or have `build.rs` copy
  them next to the exe, as vievo does).
- **Thumbnail extraction must decode forward to the target timestamp** (a bare
  backward seek snaps to sparse keyframes). Per-cell seek+forward-decode is slow
  for continuous 1080p content (~7 s for 16 frames); build the initial grid with a
  single linear decode pass instead.

Note: vievo vendors a **GPL** FFmpeg build; footage_viewer stays **LGPL** (it is
decode-only), so the two projects differ only in which BtbN variant they fetch.

See `spike/README.md` for detail, timings, and reproduction steps.
