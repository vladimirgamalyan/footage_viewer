//! Measure live scrub responsiveness — the harness behind `docs/adr/0009`.
//!
//! Reports the two numbers that decide how a scrub feels: how long one seek takes
//! to land, and how far the picture trails the cursor during a drag. The drag is
//! paced the way the UI paces it (one seek in flight, the next fired when the
//! previous lands), so the result reflects what a user would see.
//!
//! Usage: cargo run -p footage-viewer-media --release --example scrub_bench -- <video>

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use footage_viewer_media::{init, play_stream, PlayCommand};

/// Where the drag sweeps, in media seconds, and how long it takes — a brisk drag
/// across a short clip.
const DRAG_FROM_S: f64 = 1.0;
const DRAG_TO_S: f64 = 7.0;
const DRAG_SECS: f64 = 2.0;

/// A frame counts as the one asked for when it lands within this of the target.
const HIT_EPS_S: f64 = 0.06;

/// The A/D sweep: the UI's own step size (`SEEK_STEP_S`), walked out to the end of
/// the clip and back over the same positions.
const STEP_S: f64 = 0.5;
const STEP_FROM_S: f64 = 1.0;
const STEP_TO_S: f64 = 7.0;

/// Print the media crate's own per-seek timing breakdown.
struct Stdout;
impl log::Log for Stdout {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        println!("    {}", r.args());
    }
    fn flush(&self) {}
}

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(std::env::args().nth(1).expect("usage: scrub_bench <video>"));
    if std::env::var("FV_QUIET").is_err() {
        log::set_logger(&Stdout).ok();
        log::set_max_level(log::LevelFilter::Info);
    }
    init()?;

    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (frame_tx, frame_rx) = mpsc::channel::<f64>();
    let p = path.clone();
    let worker = std::thread::spawn(move || {
        play_stream(&p, 0.0, 1600, cmd_rx, |f| frame_tx.send(f.time_s).is_ok())
    });

    let t0 = Instant::now();
    frame_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("no first frame");
    println!("first frame after {:.0}ms\n", t0.elapsed().as_secs_f64() * 1000.0);

    // Jumps far enough apart to force a keyframe seek rather than a forward hop,
    // alternating direction — the worst case, and what a click on the bar does.
    println!("--- single seeks");
    let mut lat = Vec::new();
    for target in [5.0_f64, 1.0, 6.5, 2.0, 4.0, 0.5, 7.0, 3.0] {
        cmd_tx.send(PlayCommand::Scrub(target))?;
        let start = Instant::now();
        while let Ok(t) = frame_rx.recv_timeout(Duration::from_secs(30)) {
            if (t - target).abs() < HIT_EPS_S {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                println!("scrub -> {target:5.2}s  landed in {ms:6.1}ms");
                lat.push(ms);
                break;
            }
        }
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "n={} min {:.0}ms  median {:.0}ms  max {:.0}ms  mean {:.0}ms\n",
        lat.len(),
        lat[0],
        lat[lat.len() / 2],
        lat[lat.len() - 1],
        lat.iter().sum::<f64>() / lat.len() as f64,
    );

    // Park on the start of the sweep so the drag below measures steady-state
    // tracking rather than the initial catch-up.
    println!("--- drag ({DRAG_FROM_S}s -> {DRAG_TO_S}s over {DRAG_SECS}s, paced as the UI paces it)");
    cmd_tx.send(PlayCommand::Scrub(DRAG_FROM_S))?;
    let mut shown = DRAG_FROM_S;
    while let Ok(t) = frame_rx.recv_timeout(Duration::from_secs(30)) {
        if (t - DRAG_FROM_S).abs() < HIT_EPS_S {
            break;
        }
    }

    let drag = Instant::now();
    let (mut sent, mut lag) = (0, Vec::new());
    let mut in_flight = false;
    while drag.elapsed().as_secs_f64() < DRAG_SECS {
        let frac = drag.elapsed().as_secs_f64() / DRAG_SECS;
        let cursor = DRAG_FROM_S + frac * (DRAG_TO_S - DRAG_FROM_S);
        if !in_flight {
            cmd_tx.send(PlayCommand::Scrub(cursor))?;
            sent += 1;
            in_flight = true;
        }
        std::thread::sleep(Duration::from_millis(4)); // a UI repaint tick
        while let Ok(t) = frame_rx.try_recv() {
            shown = t;
            in_flight = false; // the seek landed, so chase the cursor again
        }
        // How far the picture trails the cursor right now: the perceived lag.
        lag.push((cursor - shown).abs());
    }
    println!(
        "sent {sent} scrubs in {DRAG_SECS}s | picture trails cursor: mean {:.2}s max {:.2}s\n",
        lag.iter().sum::<f64>() / lag.len() as f64,
        lag.iter().cloned().fold(0.0, f64::max),
    );

    // Step through with A/D and back again — what a tester's log actually shows
    // someone doing (docs/adr/0012), and the pattern a drag cannot measure: every
    // step of the return walks over frames the outward pass just showed. Each step
    // waits for the previous to land, exactly as the UI's key-repeat gate does.
    println!("--- A/D sweep ({STEP_S}s steps, {STEP_FROM_S}s -> {STEP_TO_S}s -> {STEP_FROM_S}s)");
    let steps: Vec<f64> = {
        let n = ((STEP_TO_S - STEP_FROM_S) / STEP_S) as usize;
        let out: Vec<f64> = (0..=n).map(|i| STEP_FROM_S + i as f64 * STEP_S).collect();
        out.iter().chain(out.iter().rev().skip(1)).cloned().collect()
    };
    let mut out_ms = Vec::new();
    let mut back_ms = Vec::new();
    let turn = steps.len() / 2;
    for (i, target) in steps.iter().enumerate() {
        cmd_tx.send(PlayCommand::Scrub(*target))?;
        let start = Instant::now();
        while let Ok(t) = frame_rx.recv_timeout(Duration::from_secs(30)) {
            if (t - target).abs() < HIT_EPS_S {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                if i < turn { &mut out_ms } else { &mut back_ms }.push(ms);
                break;
            }
        }
    }
    cmd_tx.send(PlayCommand::Stop)?;
    worker.join().unwrap()?;

    let mean = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len() as f64;
    println!(
        "outward n={} mean {:.1}ms | back over the same ground n={} mean {:.1}ms | sweep {:.2}s",
        out_ms.len(),
        mean(&out_ms),
        back_ms.len(),
        mean(&back_ms),
        (out_ms.iter().sum::<f64>() + back_ms.iter().sum::<f64>()) / 1000.0,
    );
    Ok(())
}
