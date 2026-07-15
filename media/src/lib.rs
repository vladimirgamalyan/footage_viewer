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

/// A forward scrub (Scrub/Play) whose target is within this many seconds ahead of
/// the current decoded position decodes forward in place instead of seeking back
/// to a keyframe and re-decoding the GOP. Beyond it a keyframe seek reaches the
/// target with fewer decodes, so we fall back to that. Sized to cover A/D's
/// half-second steps and small drags while never decoding across more than a
/// short gap.
const FORWARD_SCRUB_LIMIT_S: f64 = 2.0;

/// Log a one-line timing breakdown for each live seek (Scrub/Play), to diagnose
/// scrub latency on a tester's footage. Flip to `false` to silence it.
const LOG_SCRUB_TIMING: bool = true;

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

/// Frames larger than this decode faster on the GPU than on the CPU, so both the
/// grid and playback ask for a D3D11VA decoder above it and stay on the CPU at or
/// below it.
///
/// The crossover is real and measured (mean of 10 precise seeks, release build,
/// see `docs/adr/0009`): at 4K the GPU lands a seek in 30 ms against 145 ms on the
/// CPU, but at 1080p-class sizes the CPU wins (19–35 ms vs 39–64 ms) because the
/// GPU's fixed per-frame cost stops being amortized by the pixels it saves. The
/// threshold therefore sits just above 1080p-class frames (1920×1080 = 2.07 MP),
/// the largest size measured where the CPU is still ahead.
const HW_MIN_PIXELS: u32 = 2_100_000;

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
    /// Media time of the most recently decoded frame, so a forward scrub can tell
    /// how far ahead its target is and skip in place rather than re-seek.
    current_s: f64,
    /// When the in-flight Scrub/Play began; the decode loop takes it to time that
    /// move's landing latency (see [`LOG_SCRUB_TIMING`]).
    move_start: Option<Instant>,
}

impl PlayState {
    /// Fold a command into the state. Returns `false` on `Stop` (end playback).
    fn apply(&mut self, cmd: PlayCommand) -> bool {
        match cmd {
            PlayCommand::Scrub(t) => self.start_move(t, true),
            PlayCommand::Play(t) => self.start_move(t, false),
            PlayCommand::Pause => self.paused = true,
            PlayCommand::Resume => {
                self.paused = false;
                self.anchor = None;
            }
            PlayCommand::Stop => return false,
        }
        true
    }

    /// Begin a precise move to `t`, holding on the landed frame when `hold`. A
    /// short hop forward of the current position decodes forward in place (no
    /// seek/flush), so we don't re-decode the GOP behind us; anything else seeks
    /// to the keyframe before the target. Resets the pacing anchor either way so
    /// the hop isn't paced out in real time.
    ///
    /// Each move fully replaces any previous one — both fields are set on every
    /// path — so folding a queue of moves (see [`play_stream`]) leaves only the
    /// last one's intent, not a seek from one and a skip target from another.
    fn start_move(&mut self, t: f64, hold: bool) {
        if t >= self.current_s && t - self.current_s <= FORWARD_SCRUB_LIMIT_S {
            self.skip_until = Some(t);
            self.pending_seek = None;
        } else {
            self.pending_seek = Some((t, true));
            self.skip_until = None;
        }
        self.hold_after = hold;
        self.paused = false;
        self.anchor = None;
        self.move_start = Some(Instant::now());
    }
}

/// The process-wide D3D11VA device, opened on first use. `None` once we know this
/// machine can't provide one.
///
/// Shared rather than made per clip because opening one costs **~100 ms**
/// (measured), which would otherwise land on every play start — the very latency
/// the GPU is here to remove. libav refcounts the device and serializes access to
/// it, and each decoder takes its own reference, so one device serves them all.
/// Deliberately never released: it lives as long as the process.
static HW_DEVICE: std::sync::OnceLock<Option<HwDevice>> = std::sync::OnceLock::new();

/// The shared D3D11VA device, opening it on the first call. `None` when this
/// machine has none, in which case callers decode on the CPU.
fn hw_device() -> Option<&'static HwDevice> {
    HW_DEVICE.get_or_init(HwDevice::new).as_ref()
}

/// A D3D11VA device. `ffmpeg-next` exposes no hardware-decode API, so this wraps
/// the libav calls directly.
struct HwDevice(*mut ffmpeg::sys::AVBufferRef);

// Safety: an AVHWDeviceContext is refcounted with atomics and is designed to be
// shared between decoders on different threads; the D3D11VA device libav builds
// carries the lock callbacks that serialize access to the underlying video
// context. We only ever hand out new references to it (`attach`).
unsafe impl Send for HwDevice {}
unsafe impl Sync for HwDevice {}

impl HwDevice {
    /// Open the default D3D11VA device, or `None` when this machine can't provide
    /// one (no adapter, no driver support) — the caller then decodes on the CPU.
    fn new() -> Option<Self> {
        let mut ptr = std::ptr::null_mut();
        let ret = unsafe {
            ffmpeg::sys::av_hwdevice_ctx_create(
                &mut ptr,
                ffmpeg::sys::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ret < 0 {
            log::info!("no D3D11VA device ({ret}); decoding on the CPU");
            return None;
        }
        Some(Self(ptr))
    }

    /// Offer the device to `ctx`, which must not be open yet. libav's default
    /// `get_format` picks the hardware pixel format on its own once the device is
    /// attached, and silently keeps decoding on the CPU if this codec has no
    /// matching hardware config — so this only ever adds a fast path.
    ///
    /// # Safety
    /// `ctx` must be a valid, not-yet-opened decoder context.
    unsafe fn attach(&self, ctx: *mut ffmpeg::sys::AVCodecContext) {
        (*ctx).hw_device_ctx = ffmpeg::sys::av_buffer_ref(self.0);
    }
}

/// Timing for one live seek, accumulated across the decode loop and logged when
/// the landing frame is emitted (see [`LOG_SCRUB_TIMING`]). Separates the two
/// costs that make scrubbing slow: the keyframe seek/flush, and the number of
/// frames decoded forward to reach the exact target.
struct SeekProfile {
    start: Instant,
    target_s: f64,
    seek_ms: f64,
    decoded: u32,
    decode_ms: f64,
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
    // Frame size before the decoder is opened, to decide on hardware decode.
    let (src_w, src_h) = unsafe {
        let c = decoder_ctx.as_mut_ptr();
        ((*c).width as u32, (*c).height as u32)
    };
    // Decode across all cores. Frame-level threading is the biggest speedup for
    // H.264/HEVC (count 0 auto-detects the logical CPU count); the few frames of
    // pipeline latency it adds are drained after each packet and at end-of-stream.
    // Sending only keyframes makes every frame independent, so this parallelizes
    // near-perfectly — which is why the grid keeps all cores where playback caps
    // them (see PLAYBACK_THREADS), and why the GPU has a much smaller edge here.
    decoder_ctx.set_threading(ffmpeg::codec::threading::Config::kind(
        ffmpeg::codec::threading::Type::Frame,
    ));
    // Big frames decode faster on the GPU — see HW_MIN_PIXELS.
    if src_w * src_h > HW_MIN_PIXELS {
        if let Some(hw) = hw_device() {
            // Safety: the context is not opened until below.
            unsafe { hw.attach(decoder_ctx.as_mut_ptr()) };
        }
    }
    let mut decoder = decoder_ctx.decoder().video()?;
    let (out_w, out_h) = thumb_size(src_w, src_h, thumb_long_side);

    // Built on the first thumbnail, not here: with a hardware decoder the frames
    // arrive in GPU memory and their real pixel format is only known once one has
    // been decoded.
    let mut scaler: Option<Scaler> = None;

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
            src_w,
            src_h,
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
        src_w,
        src_h,
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
    scaler: &mut Option<Scaler>,
    src_w: u32,
    src_h: u32,
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
            // Unlike playback, every decoded frame here becomes a thumbnail, so a
            // hardware decode downloads all of them rather than just one.
            let from_gpu = frame.format() == Pixel::D3D11;
            let frame = to_sw_frame(frame)?;
            let s = match scaler.as_mut() {
                Some(s) => s,
                None => {
                    // First thumbnail: report which decoder actually took it (see
                    // the matching line in play_stream).
                    log::info!(
                        "grid: {}x{} ({:.1} MP), decoding on the {}, frames as {:?}",
                        src_w,
                        src_h,
                        (src_w as f64 * src_h as f64) / 1e6,
                        if from_gpu { "GPU" } else { "CPU" },
                        frame.format(),
                    );
                    scaler.insert(Scaler::get(
                        frame.format(),
                        src_w,
                        src_h,
                        Pixel::RGBA,
                        out_w,
                        out_h,
                        Flags::BILINEAR,
                    )?)
                }
            };
            let thumb = scale_thumb(s, &frame, out_w, out_h, time_s)?;
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
    // Frame size before the decoder is opened, to decide on hardware decode.
    // `from_parameters` has already copied it into the context.
    let (src_w, src_h) = unsafe {
        let c = decoder_ctx.as_mut_ptr();
        ((*c).width as u32, (*c).height as u32)
    };
    // Cap frame threads to keep the play-start warmup short — see PLAYBACK_THREADS.
    // Set even when a hardware device is attached: threading is then unused, but it
    // still applies if this codec turns out to have no hardware config and libav
    // quietly falls back to decoding on the CPU.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(PLAYBACK_THREADS))
        .unwrap_or(1);
    decoder_ctx.set_threading(ffmpeg::codec::threading::Config {
        kind: ffmpeg::codec::threading::Type::Frame,
        count: threads,
    });
    // Big frames decode faster on the GPU — see HW_MIN_PIXELS.
    if src_w * src_h > HW_MIN_PIXELS {
        if let Some(hw) = hw_device() {
            // Safety: the context is not opened until below.
            unsafe { hw.attach(decoder_ctx.as_mut_ptr()) };
        }
    }
    let mut decoder = decoder_ctx.decoder().video()?;
    let (out_w, out_h) = thumb_size(src_w, src_h, long_side);

    // Built on the first emitted frame, not here: with a hardware decoder the
    // frames arrive in GPU memory and their real pixel format (and the format they
    // download to) is only known once one has been decoded.
    let mut scaler: Option<Scaler> = None;

    let mut st = PlayState {
        paused: false,
        skip_until: None,
        hold_after: false,
        anchor: None,
        // Initial start: keyframe-before, matching a grid-cell click.
        pending_seek: Some((start_from_s, false)),
        current_s: start_from_s,
        move_start: None,
    };
    // Demux/decoder end-of-stream flags, reset on every seek.
    let mut demux_eof = false;
    let mut eof_sent = false;
    // Most recent frame skipped while precise-seeking; emitted if the seek target
    // lies past the last frame, so stepping/scrubbing to the end lands on it.
    let mut last_skipped: Option<Video> = None;
    // Timing for the in-flight live seek, logged when its frame lands.
    let mut profile: Option<SeekProfile> = None;

    loop {
        // Fold in every command already queued before acting on any of them. A drag
        // outruns the decoder easily, and a stale Scrub left in the queue would
        // otherwise cost a full seek and flush that the very next command discards.
        // `apply` only sets state, so folding in order leaves the newest move's
        // intent and we pay for one seek instead of one per queued command.
        loop {
            match commands.try_recv() {
                Ok(cmd) => {
                    if !st.apply(cmd) {
                        return Ok(()); // Stop
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // A precise move (Scrub/Play) just started: open a timing profile for it,
        // whether it will seek to a keyframe or hop forward in place.
        if let Some(start) = st.move_start.take() {
            if LOG_SCRUB_TIMING {
                let target_s = st
                    .skip_until
                    .or(st.pending_seek.map(|(t, _)| t))
                    .unwrap_or(st.current_s);
                profile = Some(SeekProfile {
                    start,
                    target_s,
                    seek_ms: 0.0,
                    decoded: 0,
                    decode_ms: 0.0,
                });
            }
        }

        // Apply a pending seek: land on the keyframe at or before (target − ε) and
        // flush, so the decoder resumes cleanly from the new position.
        if let Some((t, precise)) = st.pending_seek.take() {
            let seek_start = Instant::now();
            let target_us = (((t + start_s) * AV_TIME_BASE) as i64 - SEEK_BACK_US).max(0);
            ictx.seek(target_us, ..target_us)?;
            decoder.flush();
            if let Some(p) = &mut profile {
                p.seek_ms = millis(seek_start.elapsed());
            }
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

        // Decode the next frame.
        let decode_start = Instant::now();
        let decoded = next_frame(
            &mut ictx,
            &mut decoder,
            stream_index,
            &mut demux_eof,
            &mut eof_sent,
        )?;
        if let Some(p) = &mut profile {
            p.decode_ms += millis(decode_start.elapsed());
            if decoded.is_some() {
                p.decoded += 1;
            }
        }
        let frame = match decoded {
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
        st.current_s = time_s;

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

        // Emit the frame. Only this one is brought back from the GPU — the frames
        // decoded past on the way to a seek target never leave it.
        let from_gpu = frame.format() == Pixel::D3D11;
        let frame = to_sw_frame(frame)?;
        let scaler = match scaler.as_mut() {
            Some(s) => s,
            None => {
                // First frame of the clip: report which decoder actually took it.
                // Attaching a device is only a request — libav decodes on the CPU
                // regardless when the codec has no matching hardware config, and
                // this line is the only way to tell the two apart in a tester's log
                // (see docs/adr/0008). Pairs with the per-seek timings below to
                // show what the decode path costs on that machine.
                log::info!(
                    "playback: {}x{} ({:.1} MP), decoding on the {}, frames as {:?}",
                    src_w,
                    src_h,
                    (src_w as f64 * src_h as f64) / 1e6,
                    if from_gpu { "GPU" } else { "CPU" },
                    frame.format(),
                );
                scaler.insert(Scaler::get(
                    frame.format(),
                    src_w,
                    src_h,
                    Pixel::RGBA,
                    out_w,
                    out_h,
                    Flags::BILINEAR,
                )?)
            }
        };
        let t = scale_thumb(scaler, &frame, out_w, out_h, time_s)?;
        if !on_frame(PlaybackFrame {
            time_s: t.time_s,
            width: t.width,
            height: t.height,
            rgba: t.rgba,
        }) {
            return Ok(());
        }
        // A live seek's frame just landed: log its timing breakdown and close the
        // profile so later frames of a forward Play aren't logged.
        if let Some(p) = profile.take() {
            let per = if p.decoded > 0 {
                p.decode_ms / p.decoded as f64
            } else {
                0.0
            };
            log::info!(
                "scrub -> {:.3}s | total {:.1}ms | seek {:.1}ms | decoded {} frames {:.1}ms ({:.1}ms/f) | landed {:.3}s",
                p.target_s,
                millis(p.start.elapsed()),
                p.seek_ms,
                p.decoded,
                p.decode_ms,
                per,
                time_s,
            );
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

/// Bring a decoded frame into system memory, downloading it when it is a hardware
/// frame and passing software frames straight through. Timestamps are read off the
/// frame before this point, so the download's dropped side data doesn't matter.
fn to_sw_frame(frame: Video) -> anyhow::Result<Video> {
    if frame.format() != Pixel::D3D11 {
        return Ok(frame);
    }
    let mut sw = Video::empty();
    let ret = unsafe { ffmpeg::sys::av_hwframe_transfer_data(sw.as_mut_ptr(), frame.as_ptr(), 0) };
    if ret < 0 {
        anyhow::bail!("failed to download frame from the GPU: {ret}");
    }
    Ok(sw)
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

/// A `Duration` as fractional milliseconds, for timing logs.
fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
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

    /// A 4K clip is big enough to take the hardware decode path ([`HW_MIN_PIXELS`]),
    /// which must land a scrub on exactly the frame the CPU path would: the GPU
    /// changes where pixels are decoded, not which frame comes back. On a machine
    /// with no D3D11VA device this silently decodes on the CPU and still holds.
    #[test]
    fn scrub_lands_on_the_target_frame_in_4k() {
        init().unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::channel::<(f64, usize)>();
        let path = test_video("camera_8s_4k.mp4");

        let worker = std::thread::spawn(move || {
            play_stream(&path, 0.0, 1600, cmd_rx, |f| {
                frame_tx.send((f.time_s, f.rgba.len())).is_ok()
            })
        });

        let (_, first_len) = frame_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("no first frame");
        // A hardware frame is downloaded and scaled like any other: a full RGBA
        // buffer, not an empty or GPU-side one.
        assert!(first_len > 0, "first frame has no pixels");

        // 5.3 s sits mid-GOP (keyframes are 0.48 s apart), so landing there proves
        // the decoder decoded forward past the keyframe rather than stopping on it.
        cmd_tx.send(PlayCommand::Scrub(5.3)).unwrap();
        let mut landed = None;
        while let Ok((t, len)) = frame_rx.recv_timeout(Duration::from_secs(30)) {
            if (t - 5.3).abs() < 0.05 {
                landed = Some((t, len));
                break;
            }
        }
        cmd_tx.send(PlayCommand::Stop).unwrap();
        worker.join().unwrap().unwrap();

        let (t, len) = landed.expect("4K scrub did not land near 5.3s");
        assert_eq!(len, first_len, "landed frame is a different size");
        assert!((t - 5.3).abs() < 0.05, "landed at {t}, not 5.3s");
    }

    /// Two short forward scrubs each land on the exact target frame. These stay
    /// within `FORWARD_SCRUB_LIMIT_S`, so they take the in-place forward-decode
    /// path (no re-seek) — the second hop decodes on from where the first landed.
    #[test]
    fn short_forward_scrubs_land_on_target() {
        init().unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::channel::<f64>();
        let path = test_video("counter_25s_vertical.mp4");

        let worker = std::thread::spawn(move || {
            play_stream(&path, 0.0, 320, cmd_rx, |f| frame_tx.send(f.time_s).is_ok())
        });

        // Wait for the first frame so the decoder is warm and near t=0, keeping
        // the following scrubs short hops forward rather than backward seeks.
        frame_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("no first frame");

        for target in [0.6_f64, 1.1] {
            cmd_tx.send(PlayCommand::Scrub(target)).unwrap();
            let mut landed = None;
            while let Ok(t) = frame_rx.recv_timeout(Duration::from_secs(10)) {
                if (t - target).abs() < 0.06 {
                    landed = Some(t);
                    break;
                }
            }
            assert!(landed.is_some(), "forward scrub did not land near {target}s");
        }

        cmd_tx.send(PlayCommand::Stop).unwrap();
        worker.join().unwrap().unwrap();
    }

    /// A 4K clip's grid is big enough to take the hardware decode path
    /// ([`HW_MIN_PIXELS`]). Every kept keyframe must still turn into a thumbnail:
    /// a hardware decoder draws frames from a fixed surface pool, and if one is
    /// ever starved the packet is dropped and the thumbnail silently disappears —
    /// so this pins the count, not just that some output appeared.
    #[test]
    fn grid_of_4k_keeps_every_kept_keyframe() {
        init().unwrap();
        // The fixture is 8 s at 25 fps with `-g 12`: a keyframe every 0.48 s, so
        // ~17 of them. At 1 s spacing the sampler keeps every 2nd (round(1/0.48)),
        // which is 9 thumbnails ~0.96 s apart.
        let grid = extract_grid(&test_video("camera_8s_4k.mp4"), 1.0, 320).unwrap();
        assert_eq!(grid.src_w, 3840);
        assert_eq!(grid.src_h, 2160);
        assert_eq!(grid.thumbs.len(), 9, "lost thumbnails on the way");

        for (i, t) in grid.thumbs.iter().enumerate() {
            assert_eq!(
                t.rgba.len(),
                t.width as usize * t.height as usize * 4,
                "thumb {i} buffer does not match its dimensions"
            );
            assert!(t.rgba.iter().any(|&b| b != 0), "thumb {i} is blank");
            if i > 0 {
                let step = t.time_s - grid.thumbs[i - 1].time_s;
                assert!(
                    (step - 0.96).abs() < 0.05,
                    "thumb {i} is {step:.3}s after the last, expected ~0.96s"
                );
            }
        }
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
