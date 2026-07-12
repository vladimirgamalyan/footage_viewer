# Footage Viewer — Concept

## 1. Purpose

A specialized desktop video player for **fast triage of large volumes of short
footage**. Instead of scrubbing every clip manually, the viewer opens a file and
immediately shows a **grid of frames sampled across the clip**, so the user can
grasp what the clip contains at a glance.

Target material: individual clips, typically **10–30 seconds** each.

The tool optimizes for one thing above all: *how quickly can the user understand
what a clip is about and move to the next one.*

## 2. Goals and Non-Goals

### Goals (v1)

- Open a single video file (drag & drop, "Open with…", or file dialog).
- Show a **uniform-by-time frame grid** for the whole clip on open.
- **Click a frame to start video playback from that moment** (video only, no
  audio in v1).
- Basic transport controls (play/pause, seek, playback speed).
- **Disk cache** of generated previews so reopening a file is instant.
- Feel instant: cache hit → grid appears immediately; cache miss for a 10–30 s
  clip → grid within ~1–2 s.

### Non-Goals (v1) — explicitly deferred

- **Audio playback** (video-only in v1; audio is a future addition).
- **Folder / gallery browser** (v1 is a single-file player; the architecture
  should not preclude a gallery later).
- **Scene-detection based grids** (v1 uses uniform time sampling only).
- **Culling workflow** — keep/reject flags, ratings, tagging, and file
  operations (copy/move/delete, list export) are future work.
- Keyboard-driven cell navigation is optional/nice-to-have, not required for v1.
- Editing, trimming, transcoding, or any export of media.

## 3. Core Concept: the Frame Grid

The central data structure is the **frame grid** — N thumbnails sampled at evenly
spaced timestamps across the whole clip, decoded once and cached. It serves two
needs:

1. **Display** — the NxM grid the user reads on open.
2. **Cache** — the same N thumbnails are persisted to disk, so reopening the file
   is instant.

```
clip timeline  ├───────────────────────────────────────────────┤  (e.g. 20 s)
grid frames     ▮       ▮       ▮       ▮   ...  (N evenly spaced samples)
                └ t0     └ t1     └ t2     └ t3
```

### Parameters (defaults, all configurable later)

- **Grid size N**: default 16 (4×4).
- **Thumbnail size**: long side ~320 px, preserving aspect ratio.

Memory budget example: 16 thumbnails at 320×180 RGBA ≈ 3.7 MB per clip —
negligible for a single-file player.

## 4. Interaction Model

- **On open**: grid is shown immediately (from cache) or progressively as the
  thumbnails are extracted (cache miss).
- **Click a frame**: playback starts from that timestamp; the view switches to
  (or overlays) the main video surface.
- **Transport**: play/pause, seek bar, playback speed (e.g. 0.5×–4×). No audio.
- **Back to grid**: a key/gesture returns to the grid view.

## 5. Architecture

```
        ┌──────────────────────────────────────────────────────────┐
        │                    UI thread (egui/eframe)                │
        │  grid view · transport · video surface                    │
        └───▲───────────────▲───────────────────────▲──────────────┘
            │ grid frames    │ ready notifications    │ decoded frames
            │ (textures)     │                        │ (video texture)
   ┌────────┴───────┐  ┌─────┴───────────┐   ┌────────┴────────────┐
   │   Thumbnail    │  │   Disk cache    │   │  Playback decoder   │
   │   extractor    │◄─┤  (read/write)   │   │  (seek + decode +   │
   │ (seek+decode+  │  │  atlas + meta   │   │   frame pacing)     │
   │  scale)        │  └─────────────────┘   └─────────────────────┘
   └───────┬────────┘                                  │
           └──────────────► FFmpeg (ffmpeg-next) ◄─────┘
                     demux · decode · swscale (rescale)
```

### 5.1 Decode layer — FFmpeg via `ffmpeg-next`

- All demuxing/decoding/scaling goes through `ffmpeg-next` (Rust bindings to
  FFmpeg). Chosen for broad format coverage (H.264/H.265, ProRes, DNxHD, common
  camera containers) and hardware-decode potential.
- **Thumbnail extraction** uses fast (keyframe-accurate) seeking to each target
  timestamp and decodes the nearest frame — frame-exact accuracy is unnecessary
  for previews, and keyframe seeks are much faster.
- **swscale** downscales decoded frames to thumbnail size and converts pixel
  format to RGBA for texture upload.

### 5.2 Threading model

- Decoding never runs on the UI thread.
- **Thumbnail extractor thread**: on cache miss, walks the target timestamps,
  decodes + scales, streams thumbnails back to the UI as they are ready
  (progressive grid fill), and writes the completed grid to the cache.
- **Playback decoder thread**: owns a decoder positioned at the play point,
  decodes full-resolution frames, and paces them against a wall-clock
  presentation timer (scaled by playback speed). Since there is no audio in v1,
  the presentation clock is simply monotonic wall time — no A/V sync needed.
- Communication via channels (`crossbeam-channel`); the UI calls
  `ctx.request_repaint()` when new frames arrive.

### 5.3 Rendering — egui + wgpu (`eframe`)

- `eframe` bundles egui + winit + wgpu; the tool is a single native window.
- Thumbnails and video frames are uploaded as GPU textures (egui
  `TextureHandle`); the grid is a layout of textured cells, the player is a
  single large textured surface.
- Uploading a new texture per video frame is fine at these resolutions.

### 5.4 Disk cache

- **Location**: platform cache dir via `dirs::cache_dir()` →
  `%LOCALAPPDATA%/footage_viewer/cache/` on Windows.
- **Key**: hash of `(absolute_path, file_size, mtime, grid_size_N,
  thumb_dimensions)`. Using size + mtime (not a content hash) keeps opens cheap;
  moving/renaming a file simply produces a new cache entry — acceptable.
- **Format**: one **atlas image** per clip (all grid thumbnails packed into a
  single JPEG/WebP) plus a small **sidecar** (timestamps, per-thumb geometry,
  clip metadata) serialized with `serde` + `bincode`. One file open + one decode
  to restore the whole grid.
- **Invalidation**: automatic via the key (changed size/mtime/params → miss).
- **Eviction**: simple size-capped LRU (future refinement; not required for a
  correct v1).

## 6. Tech Stack (proposed)

| Concern            | Crate(s)                                  |
|--------------------|-------------------------------------------|
| Window + GUI + GPU | `eframe` (`egui` + `winit` + `wgpu`)      |
| Video decode/scale | `ffmpeg-next`                             |
| File dialog        | `rfd`                                     |
| Cache image codec  | `image` (JPEG) or `webp`                  |
| Cache metadata     | `serde`, `bincode`                        |
| Cache key hashing  | `blake3` (or `twox-hash`)                 |
| Cache path         | `dirs`                                     |
| Threading channels | `crossbeam-channel`                       |
| Errors             | `anyhow`, `thiserror`                     |
| Logging            | `tracing`, `tracing-subscriber`           |

Exact versions are pinned at implementation time (latest stable). Per project
rules, all code, comments, logs, and errors are in English.

## 7. Data Flow — opening a file

```
open file
   │
   ▼
compute cache key ──► cache HIT ──► load atlas + sidecar ──► show grid instantly
   │
   └► cache MISS ──► spawn extractor ──► progressive grid fill
                          │
                          ▼
                     write atlas + sidecar to cache
```

Playback is orthogonal: clicking a frame spawns/redirects the playback decoder to
that timestamp and switches the UI to the video surface.

## 8. Performance Targets

- Cache hit: grid visible in well under 100 ms.
- Cache miss (10–30 s clip, N≈16 thumbnails): usable grid within ~1–2 s, filled
  progressively.
- Playback start after click: < ~300 ms to first frame (keyframe seek + decode).

## 9. Windows / FFmpeg Build Notes

`ffmpeg-next` links against native FFmpeg libraries, which need to be present at
build/run time on Windows. Options (to be finalized as an ADR):

- Prebuilt shared FFmpeg dev libraries + `FFMPEG_DIR` pointing at them.
- `vcpkg`-managed FFmpeg.

An alternative pragmatic path for *thumbnailing only* is shelling out to a
bundled `ffmpeg.exe` (e.g. via `ffmpeg-sidecar`), but real-time in-process
playback needs the native bindings, so `ffmpeg-next` is the primary choice. The
build/distribution strategy for the FFmpeg dependency should be captured in an
ADR before implementation.

## 10. Open Questions / Future Work

- **Audio playback** and A/V sync (deferred from v1).
- **Folder / gallery browser** with a grid per clip for bulk triage — the most
  likely next major feature; keep the core decoupled from "single file" state.
- **Scene-detection grids** (shot-change aware sampling) as an alternative to
  uniform time sampling.
- **Culling workflow**: keep/reject flags, ratings, tags, and resulting file
  operations (copy/move/delete, export a shortlist).
- **Keyboard-driven navigation** across cells for mouse-free review.
- **Cache eviction policy** (size-capped LRU) and cache management UI.
- **Hardware-accelerated decode** path selection.

## 11. Rough Roadmap

1. **Skeleton**: `eframe` window, open-file dialog / drag & drop. → *verify: a
   file path is received and displayed.*
2. **Decode + thumbnail extraction**: `ffmpeg-next` seek/decode/scale one frame
   at a timestamp. → *verify: a single thumbnail renders in the window.*
3. **Frame grid**: extract N thumbnails at evenly spaced timestamps, render the
   NxM grid with progressive fill. → *verify: grid populates for a real 10–30 s
   clip.*
4. **Disk cache**: atlas + sidecar write/read, keyed invalidation. → *verify:
   second open is instant; touching the file forces a re-extract.*
5. **Playback (video-only)**: click-to-play from timestamp, transport + speed. →
   *verify: playback starts from the clicked frame and paces correctly.*
