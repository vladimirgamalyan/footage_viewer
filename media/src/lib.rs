//! Decode core for footage_viewer.
//!
//! One public operation for now: [`extract_grid`] opens a clip and returns a
//! contact sheet of thumbnails sampled at (or near) every keyframe, roughly one
//! per `spacing_s` seconds. Only keyframe packets are sent to the decoder, so a
//! single demux pass decodes ~1/GOP of the frames and never seeks — see
//! `docs/adr/0003-keyframe-contact-sheet.md` for why this replaced the per-cell
//! seek approach.

use std::path::Path;

use ffmpeg_next as ffmpeg;
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::frame::video::Video;

/// libav's internal time base: timestamps from the format context are in
/// units of 1/1_000_000 second.
const AV_TIME_BASE: f64 = 1_000_000.0;

/// How far before the requested time to aim the playback seek, in microseconds.
/// A picked thumbnail sits exactly on a keyframe, so nudging the seek target a
/// hair earlier makes the backward seek land on the *previous* keyframe instead
/// of that same one. 1 ms is far below any real GOP length yet well above PTS
/// rounding, so it steps back exactly one keyframe.
const SEEK_BACK_US: i64 = 1_000;

/// One grid cell: a downscaled RGBA frame and the time it was sampled at.
pub struct Thumbnail {
    pub time_s: f64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// One decoded frame for playback: an RGBA image scaled to fit the playback box
/// and its presentation time relative to the stream start (same timeline as a
/// [`Thumbnail`]'s `time_s`).
pub struct PlaybackFrame {
    pub time_s: f64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Clip metadata, reported once before any thumbnail.
pub struct GridMeta {
    pub duration_s: f64,
    pub src_w: u32,
    pub src_h: u32,
}

/// The whole grid plus source metadata.
pub struct Grid {
    pub duration_s: f64,
    pub src_w: u32,
    pub src_h: u32,
    pub thumbs: Vec<Thumbnail>,
}

/// Initialize libav. Call once at startup.
pub fn init() -> anyhow::Result<()> {
    ffmpeg::init()?;
    Ok(())
}

/// Decides which keyframes to keep so kept thumbnails land about `spacing_s`
/// apart. From the first keyframe interval it derives an integer skip factor
/// `N = round(spacing_s / gop)` and keeps every N-th keyframe: on footage whose
/// GOP is ~`spacing_s` this keeps them all (N = 1), and on denser footage it
/// thins to roughly one per `spacing_s`. Assumes a near-constant GOP, which holds
/// for camera files.
struct KeyframeSampler {
    spacing_s: f64,
    first_t: Option<f64>,
    step: usize,
    index: usize,
}

impl KeyframeSampler {
    fn new(spacing_s: f64) -> Self {
        Self {
            spacing_s,
            first_t: None,
            step: 0,
            index: 0,
        }
    }

    /// Whether the keyframe at time `t` (seconds) should become a thumbnail.
    /// Keyframes must be passed in decode order.
    fn keep(&mut self, t: f64) -> bool {
        let i = self.index;
        self.index += 1;
        match self.first_t {
            None => {
                self.first_t = Some(t);
                true
            }
            Some(t0) => {
                if self.step == 0 {
                    let gop = (t - t0).max(1e-6);
                    self.step = ((self.spacing_s / gop).round() as usize).max(1);
                }
                i % self.step == 0
            }
        }
    }
}

/// Extract a contact sheet of thumbnails sampled across the clip, streaming them out.
///
/// Only keyframe packets are sent to the decoder — each is intra-coded and decodes
/// on its own, so the P/B frames in between are never decoded and the pass costs
/// one frame per keyframe rather than a whole GOP. Keyframes are then thinned to
/// about one per `spacing_s` (see [`KeyframeSampler`]). `on_meta` is called once
/// with clip metadata, then `on_thumb(index, thumbnail)` is called for each kept
/// frame in order (index `0, 1, 2, …`) as it becomes ready — the total count is
/// not known up front. Thumbnails fit into a box whose long side is
/// `thumb_long_side`, preserving aspect.
pub fn extract_grid_streaming(
    path: &Path,
    spacing_s: f64,
    thumb_long_side: u32,
    mut on_meta: impl FnMut(GridMeta),
    mut on_thumb: impl FnMut(usize, Thumbnail),
) -> anyhow::Result<()> {
    let mut ictx = input(path)?;

    let stream = ictx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let stream_index = stream.index();
    let tb = stream.time_base();
    let tb_secs = tb.numerator() as f64 / tb.denominator() as f64;
    // Timeline origin: many camera files start at a non-zero PTS. Reported cell
    // times are measured relative to it.
    let start_s = match stream.start_time() {
        ts if ts != ffmpeg::sys::AV_NOPTS_VALUE => ts as f64 * tb_secs,
        _ => 0.0,
    };

    let mut decoder_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
    // Decode across all cores. Frame-level threading is the biggest speedup for
    // H.264/HEVC (count 0 auto-detects the logical CPU count); the few frames of
    // pipeline latency it adds are drained after each packet and at end-of-stream.
    decoder_ctx.set_threading(ffmpeg::codec::threading::Config::kind(
        ffmpeg::codec::threading::Type::Frame,
    ));
    let mut decoder = decoder_ctx.decoder().video()?;
    let (src_w, src_h) = (decoder.width(), decoder.height());
    let (out_w, out_h) = thumb_size(src_w, src_h, thumb_long_side);

    let mut scaler = Scaler::get(
        decoder.format(),
        src_w,
        src_h,
        Pixel::RGBA,
        out_w,
        out_h,
        Flags::BILINEAR,
    )?;

    let duration_s = ictx.duration() as f64 / AV_TIME_BASE;
    on_meta(GridMeta {
        duration_s,
        src_w,
        src_h,
    });

    let mut next_index = 0usize;
    let mut sampler = KeyframeSampler::new(spacing_s);

    for (s, packet) in ictx.packets() {
        if s.index() != stream_index {
            continue;
        }
        // Send only keyframe packets; everything between them is skipped entirely.
        if !packet.is_key() {
            continue;
        }
        decoder.send_packet(&packet).ok();
        drain(
            &mut decoder,
            &mut scaler,
            tb_secs,
            start_s,
            out_w,
            out_h,
            &mut sampler,
            &mut next_index,
            &mut on_thumb,
        )?;
    }

    // Flush frames still buffered by the threaded decoder.
    decoder.send_eof().ok();
    drain(
        &mut decoder,
        &mut scaler,
        tb_secs,
        start_s,
        out_w,
        out_h,
        &mut sampler,
        &mut next_index,
        &mut on_thumb,
    )?;

    Ok(())
}

/// Drain every frame currently available from the decoder (all keyframes, since
/// only keyframe packets are sent), emitting a thumbnail for each one the sampler
/// decides to keep.
#[allow(clippy::too_many_arguments)]
fn drain(
    decoder: &mut ffmpeg::decoder::Video,
    scaler: &mut Scaler,
    tb_secs: f64,
    start_s: f64,
    out_w: u32,
    out_h: u32,
    sampler: &mut KeyframeSampler,
    next_index: &mut usize,
    on_thumb: &mut impl FnMut(usize, Thumbnail),
) -> anyhow::Result<()> {
    loop {
        let mut frame = Video::empty();
        if decoder.receive_frame(&mut frame).is_err() {
            break;
        }
        let time_s = (frame_pts(&frame) as f64 * tb_secs - start_s).max(0.0);
        if sampler.keep(time_s) {
            let thumb = scale_thumb(scaler, &frame, out_w, out_h, time_s)?;
            on_thumb(*next_index, thumb);
            *next_index += 1;
        }
    }
    Ok(())
}

/// Frame PTS in stream time_base, falling back to the best-effort timestamp. A
/// frame with neither is treated as very early so ordering stays monotonic.
fn frame_pts(frame: &Video) -> i64 {
    frame.pts().or_else(|| frame.timestamp()).unwrap_or(i64::MIN)
}

/// Extract the whole grid at once (convenience wrapper over [`extract_grid_streaming`]).
pub fn extract_grid(path: &Path, spacing_s: f64, thumb_long_side: u32) -> anyhow::Result<Grid> {
    let mut meta: Option<GridMeta> = None;
    let mut thumbs = Vec::new();
    extract_grid_streaming(
        path,
        spacing_s,
        thumb_long_side,
        |m| meta = Some(m),
        |_, t| thumbs.push(t),
    )?;
    let meta = meta.ok_or_else(|| anyhow::anyhow!("no metadata produced"))?;
    Ok(Grid {
        duration_s: meta.duration_s,
        src_w: meta.src_w,
        src_h: meta.src_h,
        thumbs,
    })
}

/// Play `path` from the keyframe just before `start_from_s`, decoding every
/// frame forward and handing each to `on_frame` in presentation order.
///
/// Unlike [`extract_grid_streaming`] this decodes the full P/B stream, not just
/// keyframes, so it produces continuous motion. It first seeks backward to the
/// keyframe preceding `start_from_s` (a thumbnail time from the grid) so
/// playback opens a little earlier than the picked frame — see [`SEEK_BACK_US`].
/// Frames are scaled to fit a box whose long side is `long_side`. `on_frame`
/// returns `false` to stop early (e.g. the UI closed the player); decoding then
/// ends and the call returns `Ok(())`.
pub fn play_stream(
    path: &Path,
    start_from_s: f64,
    long_side: u32,
    mut on_frame: impl FnMut(PlaybackFrame) -> bool,
) -> anyhow::Result<()> {
    let mut ictx = input(path)?;

    let stream = ictx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let stream_index = stream.index();
    let tb = stream.time_base();
    let tb_secs = tb.numerator() as f64 / tb.denominator() as f64;
    let start_s = match stream.start_time() {
        ts if ts != ffmpeg::sys::AV_NOPTS_VALUE => ts as f64 * tb_secs,
        _ => 0.0,
    };

    let mut decoder_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
    decoder_ctx.set_threading(ffmpeg::codec::threading::Config::kind(
        ffmpeg::codec::threading::Type::Frame,
    ));
    let mut decoder = decoder_ctx.decoder().video()?;
    let (src_w, src_h) = (decoder.width(), decoder.height());
    let (out_w, out_h) = thumb_size(src_w, src_h, long_side);

    let mut scaler = Scaler::get(
        decoder.format(),
        src_w,
        src_h,
        Pixel::RGBA,
        out_w,
        out_h,
        Flags::BILINEAR,
    )?;

    // Seek to the keyframe at or before (target − ε). avformat_seek_file with a
    // max of `target_us` lands on the newest keyframe not after it; ε steps past
    // the picked frame's own keyframe onto the previous one. Absolute stream
    // microseconds, so add back `start_s` (files often start at a non-zero PTS).
    let target_us = (((start_from_s + start_s) * AV_TIME_BASE) as i64 - SEEK_BACK_US).max(0);
    ictx.seek(target_us, ..target_us)?;
    decoder.flush();

    for (s, packet) in ictx.packets() {
        if s.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).ok();
        if !drain_playback(
            &mut decoder,
            &mut scaler,
            tb_secs,
            start_s,
            out_w,
            out_h,
            &mut on_frame,
        )? {
            return Ok(());
        }
    }

    // Flush frames still buffered by the threaded decoder at end-of-stream.
    decoder.send_eof().ok();
    drain_playback(
        &mut decoder,
        &mut scaler,
        tb_secs,
        start_s,
        out_w,
        out_h,
        &mut on_frame,
    )?;

    Ok(())
}

/// Drain every frame currently available from the decoder, emitting a
/// [`PlaybackFrame`] for each. Returns `Ok(false)` as soon as `on_frame` asks to
/// stop, `Ok(true)` when the decoder is drained.
#[allow(clippy::too_many_arguments)]
fn drain_playback(
    decoder: &mut ffmpeg::decoder::Video,
    scaler: &mut Scaler,
    tb_secs: f64,
    start_s: f64,
    out_w: u32,
    out_h: u32,
    on_frame: &mut impl FnMut(PlaybackFrame) -> bool,
) -> anyhow::Result<bool> {
    loop {
        let mut frame = Video::empty();
        if decoder.receive_frame(&mut frame).is_err() {
            break;
        }
        let time_s = (frame_pts(&frame) as f64 * tb_secs - start_s).max(0.0);
        let t = scale_thumb(scaler, &frame, out_w, out_h, time_s)?;
        let ok = on_frame(PlaybackFrame {
            time_s: t.time_s,
            width: t.width,
            height: t.height,
            rgba: t.rgba,
        });
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Fit into a box whose long side is `long`, preserving aspect, even dimensions.
fn thumb_size(w: u32, h: u32, long: u32) -> (u32, u32) {
    let (mut ow, mut oh) = if w >= h {
        (long, (long as u64 * h as u64 / w as u64) as u32)
    } else {
        ((long as u64 * w as u64 / h as u64) as u32, long)
    };
    ow &= !1;
    oh &= !1;
    (ow.max(2), oh.max(2))
}

fn scale_thumb(
    scaler: &mut Scaler,
    frame: &Video,
    w: u32,
    h: u32,
    time_s: f64,
) -> anyhow::Result<Thumbnail> {
    let mut rgba = Video::empty();
    scaler.run(frame, &mut rgba)?;

    // RGBA rows are padded to stride(0); copy the visible width per row.
    let stride = rgba.stride(0);
    let data = rgba.data(0);
    let row = w as usize * 4;
    let mut buf = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        let start = y * stride;
        buf.extend_from_slice(&data[start..start + row]);
    }

    Ok(Thumbnail {
        time_s,
        width: w,
        height: h,
        rgba: buf,
    })
}
