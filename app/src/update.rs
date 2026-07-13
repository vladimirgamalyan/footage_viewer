//! Self-update from GitHub Releases, with UI-visible progress.
//!
//! [`start`] spawns a background thread that asks the project's public repo for
//! its latest release and, if it is newer than this build, downloads the Windows
//! asset and replaces this executable in place. It reports its phase over a
//! channel so the UI can show a spinner instead of a silent multi-second hang.
//! [`relaunch`] is called by the UI once an update has landed: it starts the
//! freshly written exe with the same arguments and exits this process.
//!
//! Best-effort: any failure (offline, GitHub unreachable, rate-limited) ends as
//! [`Msg::Failed`], and the app simply keeps running the current version.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;

const REPO_OWNER: &str = "vladimirgamalyan";
const REPO_NAME: &str = "footage_viewer";

/// Host triple the release binaries are built for. Release assets are named
/// `footage_viewer-<version>-<TARGET>.zip`; self_update matches an asset by
/// finding this string in its name. Keep in sync with `scripts/publish.ps1`.
const TARGET: &str = "x86_64-pc-windows-msvc";

/// Messages from the background update thread to the UI.
pub enum Msg {
    /// New phase text to show in the overlay (e.g. once the download starts).
    Status(&'static str),
    /// Already on the latest release: dismiss the overlay and carry on.
    UpToDate,
    /// The exe was replaced; the UI should call [`relaunch`] with this version.
    Updated(String),
    /// The check or download failed: dismiss the overlay and carry on.
    Failed,
}

/// Spawn the update check/download on a background thread, repainting the UI on
/// each phase change. Returns the receiver the UI drains each frame.
pub fn start(ctx: egui::Context) -> Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let outcome = run(&tx, &ctx);
        let _ = tx.send(outcome);
        ctx.request_repaint();
    });
    rx
}

/// Do the work and return the terminal message; phase updates go through `tx`.
fn run(tx: &Sender<Msg>, ctx: &egui::Context) -> Msg {
    let updater = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("footage_viewer")
        .target(TARGET)
        .current_version(self_update::cargo_crate_version!())
        // No console in the GUI build: a stdin confirmation would hang and a
        // stdout progress bar would go nowhere -- progress is shown in the UI.
        .no_confirm(true)
        .show_download_progress(false)
        .build();
    let updater = match updater {
        Ok(u) => u,
        Err(_) => return Msg::Failed,
    };

    // Peek at the latest release only to pick the overlay label -- the real
    // update decision is delegated to update() below, which handles the tag
    // format and compatibility itself, so a wrong guess here never blocks a
    // genuine update.
    if let Ok(latest) = updater.get_latest_release() {
        let current = self_update::cargo_crate_version!();
        let newer = self_update::version::bump_is_greater(
            current,
            latest.version.trim_start_matches('v'),
        )
        .unwrap_or(true);
        if newer {
            let _ = tx.send(Msg::Status("Downloading update\u{2026}"));
            ctx.request_repaint();
        }
    }

    match updater.update() {
        Ok(status) if status.updated() => Msg::Updated(status.version().to_string()),
        Ok(_) => Msg::UpToDate,
        Err(_) => Msg::Failed,
    }
}

/// Start the freshly written executable with the same arguments and exit, so the
/// user ends up on the new code. The UI shows the "restarting" notice before
/// calling this, so no dialog is needed here.
pub fn relaunch() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let _ = std::process::Command::new(exe).args(args).spawn();
    }
    std::process::exit(0);
}
