//! Self-update from GitHub Releases.
//!
//! On release-build startup we ask the project's public repo for its latest
//! release. If the tag is newer than this build, we download the Windows asset,
//! replace this executable in place, and relaunch the new one. The FFmpeg DLLs
//! sitting next to the exe are left untouched — only the exe changes between
//! feature builds, so a fresh clip viewer is a ~14 MB download, not the ~120 MB
//! bundle.
//!
//! Best-effort: any failure (offline, GitHub unreachable, rate-limited) is
//! swallowed so the user simply keeps running the current version.

const REPO_OWNER: &str = "vladimirgamalyan";
const REPO_NAME: &str = "footage_viewer";

/// Host triple the release binaries are built for. Release assets are named
/// `footage_viewer-<version>-<TARGET>.zip`; self_update matches an asset by
/// finding this string in its name. Keep in sync with `scripts/publish.ps1`.
const TARGET: &str = "x86_64-pc-windows-msvc";

/// Check for a newer release and, if found, apply it and relaunch. Returns
/// normally when already up to date or when the check fails for any reason.
pub fn run() {
    match try_update() {
        Ok(Some(version)) => relaunch(&version),
        Ok(None) => {}
        // The release GUI build has no console, so there is nowhere useful to
        // report this. Staying on the current version is the safe outcome.
        Err(_) => {}
    }
}

/// `Ok(Some(new_version))` if this executable was replaced, `Ok(None)` if it was
/// already the latest.
fn try_update() -> Result<Option<String>, Box<dyn std::error::Error>> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("footage_viewer")
        .target(TARGET)
        .current_version(self_update::cargo_crate_version!())
        // No console in the GUI build: a stdin confirmation prompt would hang,
        // and a stdout progress bar would go nowhere.
        .no_confirm(true)
        .show_download_progress(false)
        .build()?
        .update()?;
    Ok(status.updated().then(|| status.version().to_string()))
}

/// Let the user know we updated, then start the freshly written executable with
/// the same arguments and exit, so they end up on the new code.
fn relaunch(version: &str) -> ! {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title("footage_viewer updated")
        .set_description(format!("Updated to version {version}. The app will now restart."))
        .set_buttons(rfd::MessageButtons::Ok)
        .show();

    if let Ok(exe) = std::env::current_exe() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let _ = std::process::Command::new(exe).args(args).spawn();
    }
    std::process::exit(0);
}
