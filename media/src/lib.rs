//! Decode core for footage_viewer.
//!
//! One public operation for now: [`extract_grid`] opens a clip and returns an
//! evenly-spaced grid of thumbnails, built in a single linear decode pass — see
//! `spike/README.md` for why a linear pass beats seeking per cell.

use std::path::Path;

use ffmpeg_next as ffmpeg;
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::frame::video::Video;

/// libav's internal time base: timestamps from the format context are in
/// units of 1/1_000_000 second.
const AV_TIME_BASE: f64 = 1_000_000.0;

/// One grid cell: a downscaled RGBA frame and the time it was sampled at.
pub struct Thumbnail {
    pub time_s: f64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
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

/// Extract `cells` thumbnails evenly spaced across the clip.
///
/// Decodes the video stream once, top to bottom, emitting the first frame at or
/// after each target time. Thumbnails fit into a box whose long side is
/// `thumb_long_side`, preserving aspect.
pub fn extract_grid(path: &Path, cells: usize, thumb_long_side: u32) -> anyhow::Result<Grid> {
    let mut ictx = input(path)?;

    let stream = ictx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let stream_index = stream.index();
    let tb = stream.time_base();
    let tb_secs = tb.numerator() as f64 / tb.denominator() as f64;

    let decoder_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
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
    // Sample at the center of each of the N equal time slices.
    let target_pts: Vec<i64> = (0..cells)
        .map(|i| {
            let t = duration_s * (i as f64 + 0.5) / cells as f64;
            (t / tb_secs) as i64
        })
        .collect();

    let mut thumbs = Vec::with_capacity(cells);
    let mut next = 0usize;
    let mut last: Option<Video> = None;

    'outer: for (s, packet) in ictx.packets() {
        if s.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).ok();
        loop {
            let mut frame = Video::empty();
            if decoder.receive_frame(&mut frame).is_err() {
                break;
            }
            let pts = frame.pts().unwrap_or(i64::MIN);
            // One frame may satisfy several targets if slices are short.
            while next < cells && pts >= target_pts[next] {
                thumbs.push(scale_thumb(&mut scaler, &frame, out_w, out_h, pts as f64 * tb_secs)?);
                next += 1;
            }
            if next >= cells {
                break 'outer;
            }
            last = Some(frame);
        }
    }

    // Tail of the clip: any target past the last frame gets that last frame.
    if next < cells {
        if let Some(frame) = last.as_ref() {
            let t = frame.pts().unwrap_or(0) as f64 * tb_secs;
            while next < cells {
                thumbs.push(scale_thumb(&mut scaler, frame, out_w, out_h, t)?);
                next += 1;
            }
        }
    }

    Ok(Grid {
        duration_s,
        src_w,
        src_h,
        thumbs,
    })
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
