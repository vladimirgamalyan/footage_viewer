//! Measure contact-sheet extraction: how fast the first thumbnail shows up and
//! how long the whole grid takes to fill.
//!
//! Usage: cargo run -p footage-viewer-media --release --example grid_bench -- <video>

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use footage_viewer_media::{extract_grid_streaming, init};

/// Same values the app uses, so the numbers mean what the user sees.
const THUMB_SPACING_S: f64 = 1.0;
const THUMB_LONG: u32 = 320;

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
    let path = PathBuf::from(std::env::args().nth(1).expect("usage: grid_bench <video>"));
    if std::env::var("FV_QUIET").is_err() {
        log::set_logger(&Stdout).ok();
        log::set_max_level(log::LevelFilter::Info);
    }
    init()?;

    // Run twice: the first open in a process also pays for one-time setup (opening
    // the shared GPU device costs ~100 ms), which the app pays once per session,
    // not per clip. The second run is the steady state a user actually sees.
    for run in 1..=2 {
        let t0 = Instant::now();
        let mut first_ms = None;
        let mut times = Vec::new();
        let mut meta_ms = 0.0;
        extract_grid_streaming(
            &path,
            THUMB_SPACING_S,
            THUMB_LONG,
            // Nothing to abandon: the bench always measures a whole grid.
            &AtomicBool::new(false),
            |_| meta_ms = t0.elapsed().as_secs_f64() * 1000.0,
            |_, _| {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                first_ms.get_or_insert(ms);
                times.push(ms);
            },
        )?;
        let total = t0.elapsed().as_secs_f64() * 1000.0;

        println!(
            "run {run}: thumbs {} | meta at {meta_ms:.0}ms | first at {:.0}ms | all filled at {total:.0}ms | {:.1}ms per thumb",
            times.len(),
            first_ms.unwrap_or(f64::NAN),
            total / times.len().max(1) as f64,
        );
    }
    Ok(())
}
