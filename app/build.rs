//! Windows build steps for the executable:
//!   * embed the application icon (Explorer / taskbar / shortcut), and
//!   * copy the FFmpeg runtime DLLs next to the exe so it runs without the
//!     FFmpeg `bin` directory on PATH — e.g. when launched from Explorer.
//! Both are no-ops off Windows; the DLL copy also no-ops if FFMPEG_DIR is unset.

use std::path::Path;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");

    if !cfg!(windows) {
        return;
    }

    embed_exe_icon();

    let Ok(ffmpeg_dir) = env::var("FFMPEG_DIR") else {
        println!("cargo:warning=FFMPEG_DIR not set; skipping FFmpeg DLL copy (exe will need the DLLs on PATH)");
        return;
    };
    let bin = Path::new(&ffmpeg_dir).join("bin");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR must be set by cargo");
    // OUT_DIR is target/<profile>/build/<pkg>/out; the executable lives in target/<profile>.
    let exe_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR layout");

    let entries = match fs::read_dir(&bin) {
        Ok(entries) => entries,
        Err(e) => {
            println!("cargo:warning=cannot read {}: {e}", bin.display());
            return;
        }
    };

    for entry in entries.flatten() {
        let src = entry.path();
        if src.extension().and_then(|e| e.to_str()) != Some("dll") {
            continue;
        }
        let dst = exe_dir.join(src.file_name().unwrap());
        // Skip when an identical-size copy already exists (avoids re-copying every build
        // and avoids clobbering DLLs a running instance may have locked).
        let up_to_date = fs::metadata(&dst)
            .ok()
            .zip(fs::metadata(&src).ok())
            .map(|(d, s)| d.len() == s.len())
            .unwrap_or(false);
        if up_to_date {
            continue;
        }
        if let Err(e) = fs::copy(&src, &dst) {
            println!("cargo:warning=failed to copy {}: {e}", src.display());
        }
    }
}

/// Embed the application icon into the exe so Explorer, the taskbar, and the
/// shortcut the tester makes all show it. Best-effort: a missing resource
/// compiler just leaves the default icon rather than failing the build.
fn embed_exe_icon() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo");
    let icon = Path::new(&manifest)
        .parent()
        .expect("app crate has a parent directory")
        .join("icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon.to_str().expect("icon path is valid UTF-8"));
    if let Err(e) = res.compile() {
        println!("cargo:warning=failed to embed exe icon: {e}");
    }
}
