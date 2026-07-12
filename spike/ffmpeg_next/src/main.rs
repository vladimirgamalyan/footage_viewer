//! Spike for ADR-0001: prove that `ffmpeg-next` can, on Windows, open a clip and
//! extract an evenly-spaced grid of thumbnails via seek + decode + swscale.
//!
//! Usage: ffmpeg_next_spike [INPUT] [OUT_DIR]
//!   INPUT   defaults to test_videos/scenes_18s_720p.mp4
//!   OUT_DIR defaults to spike/ffmpeg_next/out

use std::env;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ffmpeg_next as ffmpeg;
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::frame::video::Video;

const AV_TIME_BASE: f64 = 1_000_000.0;
const GRID_N: usize = 16;
const THUMB_LONG_SIDE: u32 = 320;

fn main() -> anyhow::Result<()> {
    let mut args = env::args().skip(1);
    let input_path = args
        .next()
        .unwrap_or_else(|| "test_videos/scenes_18s_720p.mp4".to_string());
    let out_dir = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "spike/ffmpeg_next/out".to_string()),
    );
    std::fs::create_dir_all(&out_dir)?;

    ffmpeg::init()?;

    let mut ictx = input(&input_path)?;

    let stream = ictx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| anyhow::anyhow!("no video stream in {input_path}"))?;
    let video_stream_index = stream.index();
    let tb = stream.time_base();
    let tb_secs = tb.numerator() as f64 / tb.denominator() as f64;

    let decoder_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
    let mut decoder = decoder_ctx.decoder().video()?;

    let (src_w, src_h) = (decoder.width(), decoder.height());
    let (out_w, out_h) = thumb_size(src_w, src_h, THUMB_LONG_SIDE);
    let duration_s = ictx.duration() as f64 / AV_TIME_BASE;

    println!("input : {input_path}");
    println!("stream: {src_w}x{src_h}  {duration_s:.3}s  codec={:?}", decoder.id());
    println!("output: {GRID_N} thumbs @ {out_w}x{out_h} -> {}", out_dir.display());

    let mut scaler = Scaler::get(
        decoder.format(),
        src_w,
        src_h,
        Pixel::RGB24,
        out_w,
        out_h,
        Flags::BILINEAR,
    )?;

    let start = Instant::now();
    let mut ok = 0usize;
    for i in 0..GRID_N {
        // Sample at the center of each of the N equal time slices.
        let t = duration_s * (i as f64 + 0.5) / GRID_N as f64;
        let seek_ts = (t * AV_TIME_BASE) as i64;

        // Seek to the keyframe at or before t, then flush decoder buffers.
        ictx.seek(seek_ts, ..seek_ts)?;
        decoder.flush();

        // Decode forward from the keyframe until we reach the requested time.
        // Taking the first post-seek frame would snap every cell to the nearest
        // (sparse) keyframe and produce duplicates, so we walk to target_pts.
        let target_pts = (t / tb_secs) as i64;
        let mut saved = false;
        let mut last: Option<Video> = None;
        'packets: for (s, packet) in ictx.packets() {
            if s.index() != video_stream_index {
                continue;
            }
            decoder.send_packet(&packet).ok();
            loop {
                let mut frame = Video::empty();
                if decoder.receive_frame(&mut frame).is_err() {
                    break;
                }
                if frame.pts().unwrap_or(i64::MIN) >= target_pts {
                    save_thumb(&mut scaler, &frame, out_w, out_h, &out_dir, i, t)?;
                    saved = true;
                    break 'packets;
                }
                last = Some(frame);
            }
        }

        // Fallback for the final slice: target may sit past the last frame.
        if !saved {
            if let Some(frame) = last.as_ref() {
                save_thumb(&mut scaler, frame, out_w, out_h, &out_dir, i, t)?;
                saved = true;
            }
        }

        if saved {
            ok += 1;
        } else {
            eprintln!("  [warn] no frame for cell {i} (t={t:.3}s)");
        }
    }

    println!("extracted {ok}/{GRID_N} thumbs in {:?}", start.elapsed());
    Ok(())
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

fn save_thumb(
    scaler: &mut Scaler,
    frame: &Video,
    out_w: u32,
    out_h: u32,
    out_dir: &Path,
    idx: usize,
    t: f64,
) -> anyhow::Result<()> {
    let mut rgb = Video::empty();
    scaler.run(frame, &mut rgb)?;

    // RGB24 rows are padded to stride(0); copy the visible width per row.
    let stride = rgb.stride(0);
    let data = rgb.data(0);
    let mut img = image::RgbImage::new(out_w, out_h);
    for y in 0..out_h as usize {
        let row = &data[y * stride..y * stride + out_w as usize * 3];
        for x in 0..out_w as usize {
            let p = x * 3;
            img.put_pixel(x as u32, y as u32, image::Rgb([row[p], row[p + 1], row[p + 2]]));
        }
    }

    let path = out_dir.join(format!("cell_{idx:02}_t{t:06.2}.png"));
    img.save(&path)?;
    Ok(())
}
