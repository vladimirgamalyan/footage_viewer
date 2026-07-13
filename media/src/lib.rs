//! Decode core for footage_viewer.
//!
//! One public operation for now: [`extract_grid`] opens a clip and returns a
//! contact sheet of thumbnails sampled at (or near) every keyframe, roughly one
//! per `spacing_s` seconds. Only keyframe packets are sent to the decoder, so a
//! single demux pass decodes ~1/GOP of the frames and never seeks — see
//! `docs/adr/0003-keyframe-contact-sheet.md` for why this replaced the per-cell
//! seek approach.

use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use image::ImageEncoder;
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

/// Tolerance when landing a precise seek: a decoded frame within this of the
/// target counts as "the frame at the target", absorbing PTS rounding.
const FRAME_EPS_S: f64 = 1e-3;

/// Cap on frame-decoding threads for playback. Frame threading delays the first
/// output by roughly the thread count: the pipeline must fill before frame 0 is
/// released, so the all-cores default (e.g. 32) buffers ~18 frames before the
/// first is shown — a black gap at play start that grows with per-frame decode
/// cost on heavy footage. Frame-threading throughput saturates well before that
/// many threads for H.264/HEVC, so capping low keeps real-time playback while
/// cutting the warmup (measured ~18 buffered frames at 32 threads down to ~8 at
/// 6). The grid path intentionally keeps all cores: it is a batch decode where
/// startup latency does not matter.
const PLAYBACK_THREADS: usize = 6;

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

/// A command to the live playback decoder (see [`play_stream`]). One decoder
/// thread stays open for the whole session and reacts to these, so seeking is a
/// flush rather than a fresh decode.
pub enum PlayCommand {
    /// Seek to `time_s`, emit exactly that frame, then hold (scrub preview).
    Scrub(f64),
    /// Seek to `time_s` and play forward from there in real time.
    Play(f64),
    /// Hold on the current frame.
    Pause,
    /// Resume playing from the current position.
    Resume,
    /// End playback; `play_stream` returns `Ok(())`.
    Stop,
}

/// Mutable state of the playback state machine, mutated by [`PlayCommand`]s.
struct PlayState {
    /// Holding on the current frame (paused, or scrub between moves).
    paused: bool,
    /// Skip decoded frames earlier than this media time (precise-seek landing).
    skip_until: Option<f64>,
    /// After emitting the next frame, hold on it (used by `Scrub`).
    hold_after: bool,
    /// Wall/media clock anchor for real-time pacing; reset on every seek.
    anchor: Option<(Instant, f64)>,
    /// A seek to apply before decoding resumes: `(time_s, precise)`. `precise`
    /// skips forward to the exact frame; otherwise playback opens at the keyframe.
    pending_seek: Option<(f64, bool)>,
}

impl PlayState {
    /// Fold a command into the state. Returns `false` on `Stop` (end playback).
    fn apply(&mut self, cmd: PlayCommand) -> bool {
        match cmd {
            PlayCommand::Scrub(t) => {
                self.pending_seek = Some((t, true));
                self.hold_after = true;
                self.paused = false;
            }
            PlayCommand::Play(t) => {
                self.pending_seek = Some((t, true));
                self.hold_after = false;
                self.paused = false;
            }
            PlayCommand::Pause => self.paused = true,
            PlayCommand::Resume => {
                self.paused = false;
                self.anchor = None;
            }
            PlayCommand::Stop => return false,
        }
        true
    }
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

/// Play `path` with one long-lived decoder driven by [`PlayCommand`]s over
/// `commands`, handing each due frame to `on_frame` in presentation order.
///
/// Unlike [`extract_grid_streaming`] this decodes the full P/B stream, not just
/// keyframes, so it produces continuous motion, and it paces frames to real time
/// itself (the UI just displays what arrives). Playback opens at the keyframe
/// before `start_from_s` — see [`SEEK_BACK_US`]. Thereafter a `Scrub`/`Play`
/// command seeks by flushing this same decoder and skipping forward to the exact
/// target frame, so scrubbing shows precise frames without reopening the file.
/// Frames are scaled to fit a box whose long side is `long_side`. `on_frame`
/// returns `false` to stop (the UI closed the player); `Stop`, a closed command
/// channel, or end-of-stream during playback also end the call with `Ok(())`.
pub fn play_stream(
    path: &Path,
    start_from_s: f64,
    long_side: u32,
    commands: Receiver<PlayCommand>,
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
    // Cap frame threads to keep the play-start warmup short — see PLAYBACK_THREADS.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(PLAYBACK_THREADS))
        .unwrap_or(1);
    decoder_ctx.set_threading(ffmpeg::codec::threading::Config {
        kind: ffmpeg::codec::threading::Type::Frame,
        count: threads,
    });
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

    let mut st = PlayState {
        paused: false,
        skip_until: None,
        hold_after: false,
        anchor: None,
        // Initial start: keyframe-before, matching a grid-cell click.
        pending_seek: Some((start_from_s, false)),
    };
    // Demux/decoder end-of-stream flags, reset on every seek.
    let mut demux_eof = false;
    let mut eof_sent = false;
    // Most recent frame skipped while precise-seeking; emitted if the seek target
    // lies past the last frame, so stepping/scrubbing to the end lands on it.
    let mut last_skipped: Option<Video> = None;

    loop {
        // Apply a pending seek: land on the keyframe at or before (target − ε) and
        // flush, so the decoder resumes cleanly from the new position.
        if let Some((t, precise)) = st.pending_seek.take() {
            let target_us = (((t + start_s) * AV_TIME_BASE) as i64 - SEEK_BACK_US).max(0);
            ictx.seek(target_us, ..target_us)?;
            decoder.flush();
            st.skip_until = precise.then_some(t);
            st.anchor = None;
            demux_eof = false;
            eof_sent = false;
            last_skipped = None;
        }

        // Paused (or scrub-holding): block until a command wakes us.
        if st.paused {
            match commands.recv() {
                Ok(cmd) => {
                    if !st.apply(cmd) {
                        return Ok(()); // Stop
                    }
                    continue;
                }
                Err(_) => return Ok(()), // channel closed
            }
        }

        // Drain any queued command without blocking; a command restarts the loop.
        match commands.try_recv() {
            Ok(cmd) => {
                if !st.apply(cmd) {
                    return Ok(());
                }
                continue;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(()),
        }

        // Decode the next frame.
        let frame = match next_frame(
            &mut ictx,
            &mut decoder,
            stream_index,
            &mut demux_eof,
            &mut eof_sent,
        )? {
            Some(f) => f,
            None => match last_skipped.take() {
                // End of stream while still skipping toward a precise seek target
                // past the last frame: land on the final frame we buffered so a
                // step/scrub to the very end shows it instead of nothing.
                Some(f) if st.skip_until.is_some() => {
                    st.skip_until = None;
                    st.hold_after = true;
                    f
                }
                _ => {
                    // Reached the end (played through to it, or sought past it):
                    // hold on the last frame instead of ending, so playback stays
                    // up rather than kicking back to the grid. Escape/Stop from the
                    // UI still exits, and scrubbing back seeks and resumes.
                    st.skip_until = None;
                    st.paused = true;
                    continue;
                }
            },
        };
        let time_s = (frame_pts(&frame) as f64 * tb_secs - start_s).max(0.0);

        // Skip decoded frames before a precise seek target so scrubbing lands on
        // the exact frame rather than the keyframe. Buffer the last skipped frame
        // so end-of-stream can fall back to it (see above).
        if let Some(u) = st.skip_until {
            if time_s + FRAME_EPS_S < u {
                last_skipped = Some(frame);
                continue;
            }
            st.skip_until = None;
            last_skipped = None;
        }

        // Pace to real time, but let a command preempt the wait (drop this frame).
        match st.anchor {
            None => st.anchor = Some((Instant::now(), time_s)),
            Some((wall0, media0)) => {
                let due = wall0 + Duration::from_secs_f64((time_s - media0).max(0.0));
                // Wait until the frame is due, but let a command preempt the wait
                // (dropping this frame — the command seeks or pauses anyway).
                let mut preempted = false;
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    match commands.recv_timeout(wait) {
                        Ok(cmd) => {
                            if !st.apply(cmd) {
                                return Ok(());
                            }
                            preempted = true;
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
                if preempted {
                    continue;
                }
            }
        }

        // Emit the frame.
        let t = scale_thumb(&mut scaler, &frame, out_w, out_h, time_s)?;
        if !on_frame(PlaybackFrame {
            time_s: t.time_s,
            width: t.width,
            height: t.height,
            rgba: t.rgba,
        }) {
            return Ok(());
        }
        // Scrub shows one frame at the target, then holds on it.
        if st.hold_after {
            st.hold_after = false;
            st.paused = true;
        }
    }
}

/// Decode the frame at `time_s` at full source resolution and write it to `out`
/// as a JPEG at `quality` (1–100, higher is better). Seeks to the keyframe at or
/// before the target and decodes forward to the first frame at/after it — the
/// same precise landing a scrub uses — so the saved still matches what playback
/// shows at that time. Overwrites `out` if it already exists.
pub fn save_frame_jpeg(path: &Path, time_s: f64, out: &Path, quality: u8) -> anyhow::Result<()> {
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
    let (w, h) = (decoder.width(), decoder.height());

    // Full source resolution, RGB (no alpha) — JPEG has no alpha channel.
    let mut scaler = Scaler::get(decoder.format(), w, h, Pixel::RGB24, w, h, Flags::BILINEAR)?;

    // Land on the keyframe at or before the target, then decode forward to it.
    let target_us = (((time_s + start_s) * AV_TIME_BASE) as i64 - SEEK_BACK_US).max(0);
    ictx.seek(target_us, ..target_us)?;
    decoder.flush();

    let mut demux_eof = false;
    let mut eof_sent = false;
    // Keep the newest decoded frame so end-of-stream (target past the last frame)
    // falls back to the final frame instead of failing.
    let mut chosen: Option<Video> = None;
    loop {
        match next_frame(&mut ictx, &mut decoder, stream_index, &mut demux_eof, &mut eof_sent)? {
            Some(frame) => {
                let t = (frame_pts(&frame) as f64 * tb_secs - start_s).max(0.0);
                let reached = t + FRAME_EPS_S >= time_s;
                chosen = Some(frame);
                if reached {
                    break;
                }
            }
            None => break,
        }
    }
    let frame = chosen.ok_or_else(|| anyhow::anyhow!("no frame decoded at {time_s:.3}s"))?;

    let mut rgb = Video::empty();
    scaler.run(&frame, &mut rgb)?;

    // RGB24 rows are padded to stride(0); copy the visible width per row.
    let stride = rgb.stride(0);
    let data = rgb.data(0);
    let row = w as usize * 3;
    let mut buf = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        let start = y * stride;
        buf.extend_from_slice(&data[start..start + row]);
    }

    let file = std::fs::File::create(out)?;
    let writer = std::io::BufWriter::new(file);
    image::codecs::jpeg::JpegEncoder::new_with_quality(writer, quality)
        .write_image(&buf, w, h, image::ExtendedColorType::Rgb8)?;
    Ok(())
}

/// Decode the next frame, pulling packets on demand. Returns `Ok(None)` only when
/// the stream is fully drained. `demux_eof`/`eof_sent` track end-of-stream across
/// calls and must be reset after a seek/flush.
fn next_frame(
    ictx: &mut ffmpeg::format::context::Input,
    decoder: &mut ffmpeg::decoder::Video,
    stream_index: usize,
    demux_eof: &mut bool,
    eof_sent: &mut bool,
) -> anyhow::Result<Option<Video>> {
    loop {
        let mut frame = Video::empty();
        match decoder.receive_frame(&mut frame) {
            Ok(()) => return Ok(Some(frame)),
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => {}
            Err(ffmpeg::Error::Eof) => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        if *demux_eof {
            if !*eof_sent {
                decoder.send_eof().ok();
                *eof_sent = true;
                continue;
            }
            return Ok(None); // fully drained
        }

        // Read one video packet; the iterator is dropped each call so `ictx` is
        // free to seek between frames.
        let packet = ictx
            .packets()
            .find_map(|(s, p)| (s.index() == stream_index).then_some(p));
        match packet {
            Some(p) => {
                decoder.send_packet(&p).ok();
            }
            None => *demux_eof = true,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::mpsc;

    fn test_video(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test_videos")
            .join(name)
    }

    /// A `Scrub` command lands the decoder on the exact frame at the target time,
    /// not the keyframe before it — the whole point of the live seek.
    #[test]
    fn scrub_lands_on_the_target_frame() {
        init().unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::channel::<f64>();
        let path = test_video("counter_25s_vertical.mp4");

        let worker = std::thread::spawn(move || {
            play_stream(&path, 0.0, 320, cmd_rx, |f| frame_tx.send(f.time_s).is_ok())
        });

        // Wait for playback to produce a first frame, then scrub to 12s.
        frame_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("no first frame");
        cmd_tx.send(PlayCommand::Scrub(12.0)).unwrap();

        // The scrubbed frame should arrive within a frame of the target.
        let mut landed = None;
        while let Ok(t) = frame_rx.recv_timeout(Duration::from_secs(10)) {
            if (t - 12.0).abs() < 0.05 {
                landed = Some(t);
                break;
            }
        }
        cmd_tx.send(PlayCommand::Stop).unwrap();
        worker.join().unwrap().unwrap();

        assert!(landed.is_some(), "scrub did not land near 12s: {landed:?}");
    }

    /// `save_frame_jpeg` writes a valid JPEG at full source resolution for the
    /// frame at the requested time.
    #[test]
    fn save_frame_jpeg_writes_full_res_still() {
        init().unwrap();
        let path = test_video("counter_25s_vertical.mp4");

        // Source dimensions come from the grid metadata for the same clip.
        let grid = extract_grid(&path, 5.0, 320).unwrap();
        assert!(grid.src_w > 0 && grid.src_h > 0);

        let out = std::env::temp_dir().join("footage_viewer_still_test.jpg");
        save_frame_jpeg(&path, 12.0, &out, 92).unwrap();

        let img = image::open(&out).expect("saved file is a readable image");
        assert_eq!(img.width(), grid.src_w);
        assert_eq!(img.height(), grid.src_h);

        std::fs::remove_file(&out).ok();
    }

    /// `Stop` (and a dropped command channel) ends `play_stream` cleanly.
    #[test]
    fn stop_ends_playback() {
        init().unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::channel::<f64>();
        let path = test_video("counter_25s_vertical.mp4");

        let worker = std::thread::spawn(move || {
            play_stream(&path, 0.0, 320, cmd_rx, |f| frame_tx.send(f.time_s).is_ok())
        });

        frame_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("no first frame");
        cmd_tx.send(PlayCommand::Stop).unwrap();
        worker.join().unwrap().unwrap();
    }
}
