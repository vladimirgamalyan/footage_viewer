# Spike: FFmpeg decode via `ffmpeg-next` on Windows

Goal: de-risk **ADR-0001** by proving that `ffmpeg-next` can, on this Windows
machine, perform the app's core operation — open a clip and extract an
evenly-spaced grid of thumbnails (seek + decode + swscale + write).

**Result: SUCCESS.** All four test clips yield 16/16 correct, evenly-spaced,
visually distinct thumbnails. See `ffmpeg_next/out/*_montage.png`.

## Layout

- `cli_contact_sheets/` — baseline: 4×4 contact sheets built with the **ffmpeg
  CLI** (`fps` filter + `tile`). Confirms the sampling idea and gives reference
  output. This single linear pass is effectively instant.
- `ffmpeg_next/` — the real spike: a Rust program using `ffmpeg-next` that
  reproduces the grid extraction in-process. `out/` holds per-clip thumbnails and
  a montage per clip.
- `hwaccel/` — a **later, separate spike** (for **ADR-0009**, not ADR-0001): times
  a precise seek on a D3D11VA decoder and splits the cost into seek, flush, decode
  and GPU readback. It established that the flush, not the seek, was what made
  scrubbing slow, and that the readback is only ~1.7 ms. Everything below in this
  file concerns the ADR-0001 spike only.

## Environment (as tested, 2026-07-12)

| Tool | Version |
|------|---------|
| ffmpeg CLI (test-clip generation only) | 8.1.1 gyan.dev (GPL) via choco |
| rustc / cargo | 1.96.1, host `x86_64-pc-windows-msvc` |
| LLVM / libclang | 22.1.8 (`C:\Program Files\LLVM`) |
| MSVC | Visual Studio Community 2026 (v18), VC Tools |
| FFmpeg dev libs (linked) | BtbN `ffmpeg-n8.1-latest-win64-lgpl-shared-8.1` (**LGPL**, per ADR-0001) |

## Findings

### 1. Crate/toolchain version alignment is mandatory (build blocker)

`ffmpeg-next = "7.1"` pulls `ffmpeg-sys-next 7.1.3`, which pins **`bindgen
^0.70`**. bindgen 0.70 is **incompatible with libclang 22**: it reads the correct
struct *sizes* from the headers but emits every record as an **opaque** (empty)
type, so the generated layout assertions fail to compile:

```
error[E0080]: attempt to compute `1_usize - 472_usize`, which would overflow
  ["Size of AVFormatContext"][size_of::<AVFormatContext>() - 472usize];
```

Passing MSVC/SDK include paths (`BINDGEN_EXTRA_CLANG_ARGS`, `-isystem`) does **not**
help — the headers were already found; the bug is in bindgen's codegen against a
too-new libclang.

**Fix:** move to `ffmpeg-next = "8.1"` → `ffmpeg-sys-next 8.1.0` → **`bindgen
^0.72`**, which is libclang-22-compatible. Builds cleanly, no extra clang args.

> Lesson for ADR-0001: pinning FFmpeg to the crate's version range is not enough —
> the crate's **bindgen** must also match the **installed libclang**. Alternative:
> install an older LLVM whose libclang matches the crate's bindgen.

### 2. Build/run environment requirements

- **Build:** run inside the **VS Developer environment** so the real MSVC
  `link.exe` is used. In plain Git Bash, `/usr/bin/link.exe` (an MSYS coreutil)
  shadows the linker. `Enter-VsDevShell` fixes this.
- **Build:** `FFMPEG_DIR` → extracted dev build root (`include/`, `lib/`);
  `LIBCLANG_PATH` → `C:\Program Files\LLVM\bin`.
- **Run:** the FFmpeg `bin/*.dll` must be on `PATH` (or copied next to the exe).

### 3. Sampling algorithm: decode forward, and prefer a single pass

- Taking the **first frame after a backward seek** snaps every cell to the
  nearest (sparse) keyframe → duplicate thumbnails. The burned-in timecodes made
  this obvious (five cells all read `00:00:00.000`). The extractor must **decode
  forward to the target PTS**.
- **Per-cell seek + forward-decode re-decodes overlapping GOP regions** and is
  slow for continuous content. Timings for 16 thumbnails (after the fix):

  | Clip | Res | Time |
  |------|-----|------|
  | hue_sweep_20s | 1280×720 | 0.81 s |
  | scenes_18s | 1280×720 | 1.75 s |
  | mandelbrot_15s | 1920×1080 | **6.96 s** |
  | counter_25s | 1080×1920 | 2.09 s |

  The CLI baseline (`fps` filter = one linear pass) produced the same grids
  effectively instantly.

  **Recommendation for the app:** build the initial grid with a **single linear
  decode pass** — decode sequentially and keep the frame nearest each of the N
  target times as it goes by. Reserve seek + short forward-decode for on-demand
  single frames (e.g. click-to-play). This keeps the concept's "grid within
  ~1–2 s" target reachable even at 1080p.

## Reproduce

```bash
# 1. Get the LGPL shared dev build (once)
gh release download latest -R BtbN/FFmpeg-Builds \
  -p 'ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip' -D <dir>
#    ...extract it; note the root folder (has include/, lib/, bin/)
```

```powershell
# 2. Build + run (PowerShell) inside the VS dev environment
$vs = "C:\Program Files\Microsoft Visual Studio\18\Community"
Import-Module (Join-Path $vs "Common7\Tools\Microsoft.VisualStudio.DevShell.dll")
Enter-VsDevShell -VsInstallPath $vs -SkipAutomaticLocation -DevCmdArguments "-arch=x64 -host_arch=x64"
$env:FFMPEG_DIR   = "<extracted>\ffmpeg-n8.1-latest-win64-lgpl-shared-8.1"
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
cargo build --release --manifest-path spike\ffmpeg_next\Cargo.toml

$env:PATH = "$env:FFMPEG_DIR\bin;$env:PATH"
.\spike\ffmpeg_next\target\release\ffmpeg_next_spike.exe test_videos\scenes_18s_720p.mp4 spike\ffmpeg_next\out\scenes
```

The downloaded dev build and `target/`/`out/` are throwaway; keep them out of
version control.
