//! Eyeball `extract_grid`: dump the grid of a clip as PNGs.
//!
//! Usage: cargo run -p footage-viewer-media --example dump_grid -- <video> [out_dir]

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().expect("usage: dump_grid <video> [out_dir]"));
    let out_dir = PathBuf::from(args.next().unwrap_or_else(|| "out".to_string()));
    std::fs::create_dir_all(&out_dir)?;

    footage_viewer_media::init()?;
    let grid = footage_viewer_media::extract_grid(&path, 1.0, 320)?;
    println!(
        "duration {:.2}s  src {}x{}  thumbs {}",
        grid.duration_s,
        grid.src_w,
        grid.src_h,
        grid.thumbs.len()
    );

    for (i, t) in grid.thumbs.iter().enumerate() {
        let img = image::RgbaImage::from_raw(t.width, t.height, t.rgba.clone())
            .expect("thumbnail buffer matches its dimensions");
        img.save(out_dir.join(format!("cell_{i:02}_t{:06.2}.png", t.time_s)))?;
    }
    println!("wrote {} PNGs to {}", grid.thumbs.len(), out_dir.display());
    Ok(())
}
