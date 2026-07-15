//! Spike: precise-seek latency with a D3D11VA hardware decoder.
//!
//! Mirrors the app's live-scrub move (seek to the keyframe before the target,
//! decode forward to the exact frame, hand back a CPU frame) and times each part,
//! so the result is directly comparable to the CPU numbers from
//! `media/examples/scrub_bench.rs`.
//!
//! Usage: cargo run --release -- <video>

use std::time::{Duration, Instant};

use ffmpeg::format::input;
use ffmpeg::media::Type;
use ffmpeg::util::frame::video::Video;
use ffmpeg_next as ffmpeg;

const AV_TIME_BASE: f64 = 1_000_000.0;
const SEEK_BACK_US: i64 = 1_000;
const FRAME_EPS_S: f64 = 1e-3;

fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: hwaccel_spike <video>");
    ffmpeg::init()?;

    let mut ictx = input(&path)?;
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
    let params = stream.parameters();
    let mut decoder_ctx = ffmpeg::codec::context::Context::from_parameters(params)?;

    // Attach a D3D11VA device. avcodec_default_get_format picks the matching hw
    // pixel format on its own once hw_device_ctx is set, so no get_format hook.
    let mut hw_dev: *mut ffmpeg::sys::AVBufferRef = std::ptr::null_mut();
    unsafe {
        let r = ffmpeg::sys::av_hwdevice_ctx_create(
            &mut hw_dev,
            ffmpeg::sys::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        );
        if r < 0 {
            anyhow::bail!("av_hwdevice_ctx_create(D3D11VA) failed: {r}");
        }
        let ctx = decoder_ctx.as_mut_ptr();
        (*ctx).hw_device_ctx = ffmpeg::sys::av_buffer_ref(hw_dev);
    }

    let mut decoder = decoder_ctx.decoder().video()?;
    println!(
        "opened {path}: {}x{}, decoder pix_fmt {:?}",
        decoder.width(),
        decoder.height(),
        decoder.format()
    );

    let targets = [8.0_f64, 2.0, 9.5, 1.0, 6.0, 3.0, 11.0, 0.5, 7.0, 4.0];
    let mut totals = Vec::new();
    let mut hw_confirmed = false;

    for &target in &targets {
        let t0 = Instant::now();

        // Seek to the keyframe at or before the target, then flush.
        let target_us = (((target + start_s) * AV_TIME_BASE) as i64 - SEEK_BACK_US).max(0);
        ictx.seek(target_us, ..target_us)?;
        let seek_ms = millis(t0.elapsed());
        let f0 = Instant::now();
        decoder.flush();
        let flush_ms = millis(f0.elapsed());

        // Decode forward to the exact target frame.
        let d0 = Instant::now();
        let mut demux_eof = false;
        let mut eof_sent = false;
        let mut decoded = 0u32;
        let mut landed: Option<(Video, f64)> = None;
        while let Some(frame) = next_frame(
            &mut ictx,
            &mut decoder,
            stream_index,
            &mut demux_eof,
            &mut eof_sent,
        )? {
            decoded += 1;
            let t = (frame.pts().or_else(|| frame.timestamp()).unwrap_or(0) as f64 * tb_secs
                - start_s)
                .max(0.0);
            if t + FRAME_EPS_S >= target {
                landed = Some((frame, t));
                break;
            }
        }
        let decode_ms = millis(d0.elapsed());

        let (frame, landed_s) = landed.ok_or_else(|| anyhow::anyhow!("no frame at {target}"))?;
        if !hw_confirmed {
            // AV_PIX_FMT_D3D11 means the hwaccel really engaged (not a sw fallback).
            println!("  landed frame format: {:?}", frame.format());
            hw_confirmed = true;
        }

        // Download the landing frame to system memory — only this one frame, not
        // the ones decoded past on the way.
        let x0 = Instant::now();
        let mut sw = Video::empty();
        unsafe {
            let r = ffmpeg::sys::av_hwframe_transfer_data(sw.as_mut_ptr(), frame.as_ptr(), 0);
            if r < 0 {
                anyhow::bail!("av_hwframe_transfer_data failed: {r}");
            }
        }
        let xfer_ms = millis(x0.elapsed());

        // Download the same frame again. The GPU decode is already synced by the
        // first transfer, so this second one times the pure PCIe readback — if it
        // is much cheaper, the first transfer was mostly waiting on GPU decode.
        let x1 = Instant::now();
        let mut sw2 = Video::empty();
        unsafe {
            let r = ffmpeg::sys::av_hwframe_transfer_data(sw2.as_mut_ptr(), frame.as_ptr(), 0);
            if r < 0 {
                anyhow::bail!("second av_hwframe_transfer_data failed: {r}");
            }
        }
        let xfer2_ms = millis(x1.elapsed());

        let total = millis(t0.elapsed());
        totals.push(total);
        println!(
            "scrub -> {target:5.2}s | total {total:6.1}ms | seek {seek_ms:4.1} flush {flush_ms:4.1} | decoded {decoded:2} frames {decode_ms:6.1}ms ({:.1}ms/f) | download {xfer_ms:4.1}ms (again {xfer2_ms:4.1}ms) -> {:?} | landed {landed_s:.3}s",
            if decoded > 0 { decode_ms / decoded as f64 } else { 0.0 },
            sw.format(),
        );
    }

    totals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "\nn={} min {:.0}ms  median {:.0}ms  max {:.0}ms  mean {:.0}ms",
        totals.len(),
        totals[0],
        totals[totals.len() / 2],
        totals[totals.len() - 1],
        totals.iter().sum::<f64>() / totals.len() as f64,
    );
    Ok(())
}

/// Decode the next frame, pulling packets on demand (mirrors media's next_frame).
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
            return Ok(None);
        }
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
