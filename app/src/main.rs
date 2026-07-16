//! footage_viewer — open a clip and see a grid of frames sampled across it.
//!
//! Extraction runs on a background thread and streams thumbnails back; the grid
//! fills in progressively so the window never blocks on open. Grids of clips
//! just visited are kept, and the next sibling is read ahead while the current
//! one is studied, so stepping through a folder rarely waits on the disk.

// In release, build as a Windows GUI app so launching from Explorer doesn't flash a console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use eframe::egui;
use footage_viewer_media as media;

mod logging;
mod stats;

const THUMB_SPACING_S: f64 = 1.0;
const THUMB_LONG: u32 = 320;

/// Grid column count: the value the app starts at and the inclusive range the
/// `-`/`+` keys clamp it to.
const GRID_COLS_DEFAULT: usize = 4;
const GRID_COLS_MIN: usize = 1;
const GRID_COLS_MAX: usize = 12;

/// Long side of decoded playback frames. Caps per-frame scaling and texture
/// upload cost; large enough for the video to fill a typical window crisply.
const PLAYBACK_LONG: u32 = 1600;

/// Total thumbnails the recently-viewed grids may hold before the oldest is
/// evicted. Stepping back to a clip just visited then costs nothing instead of
/// re-reading it — which on the external HDD the target archive lives on is
/// seconds, not milliseconds.
///
/// The budget counts thumbnails rather than clips because a grid holds roughly
/// one per second of footage: a fixed clip count would mean ~25 thumbnails each
/// for the 10-23 s clips this tool targets, but thousands for a long recording.
/// At ~0.23 MB per 320-wide RGBA thumbnail this is ~46 MB of texture memory, and
/// it holds about eight of the target clips. A single grid larger than the whole
/// budget is simply never cached, which is the right way to give up on it.
const RECENT_MAX_THUMBS: usize = 200;

/// How many thumbnails an extraction worker may queue ahead of whoever polls it.
///
/// Only the read-ahead ever reaches this bound. The foreground grid is drained
/// every frame, but a prefetch is polled by nobody until it is opened, so with an
/// unbounded queue its worker reads the whole sibling regardless of length —
/// ~0.23 MB per thumbnail and roughly one per second of footage, so an hour-long
/// neighbour is ~800 MB of buffered thumbnails and minutes of the one disk head,
/// spent on a clip nobody asked for. That is the same "thousands for a long
/// recording" that [`RECENT_MAX_THUMBS`] exists to bound, on the one path that
/// was left unbounded.
///
/// A full queue blocks the worker inside its `on_thumb` callback, which is called
/// from the demux loop — so the read itself stops, keeping its position, decoder
/// and file handle. Opening the clip drains the queue and the read resumes from
/// where it parked, which is why this bounds the cost without the restart-from-
/// zero that made cancelling a read-ahead on play unworkable (see docs/adr/0011).
/// Dropping the clip releases a parked worker too: the receiver goes with it, the
/// blocked send fails, and the next cancel check stops the pass.
///
/// Sized well clear of the 11-25 thumbnails of the clips this tool targets, so
/// reading one of those ahead still completes in full and this never bites.
const GRID_QUEUE_THUMBS: usize = 64;

/// Smallest cursor move (in media seconds) that triggers a new live seek while
/// dragging. Holding within this of the current position keeps the frozen frame
/// instead of restarting the decoder on the same spot.
const SCRUB_MIN_STEP_S: f64 = 0.05;

/// How far A/D nudge the playback position, in media seconds.
const SEEK_STEP_S: f64 = 0.5;

/// Hold delay before A/D key-repeat kicks in, in seconds.
const SEEK_REPEAT_DELAY_S: f64 = 0.35;

/// Interval between repeated A/D seeks while the key is held, in seconds.
const SEEK_REPEAT_INTERVAL_S: f64 = 0.12;

/// How far playback may be zoomed in, as a multiple of the fit-to-window scale.
///
/// Frames arrive capped at [`PLAYBACK_LONG`], so past roughly 2x on a filled
/// window this magnifies the decoded frame rather than uncovering anything: on
/// the 4K footage this tool targets the source's detail was resampled away
/// before the zoom could ever reach it. So this bounds how far a useful
/// magnifier goes, and is not the point where the picture stops being sharp —
/// it stopped before here. Raising [`PLAYBACK_LONG`] is what would buy real
/// detail, at a scaling and upload cost paid on every frame of every clip
/// whether it is zoomed or not.
const ZOOM_MAX: f32 = 8.0;

/// What one press of `+`/`-` multiplies (or divides) the playback zoom by.
const ZOOM_KEY_STEP: f32 = 1.25;

/// Scroll points to zoom multiplier: `exp(points * this)`. Matches egui's own
/// ctrl-scroll speed, which puts one wheel notch (40 points) at ~1.22x — near
/// enough to [`ZOOM_KEY_STEP`] that wheel and keys feel like one control.
const ZOOM_WHEEL_SPEED: f32 = 1.0 / 200.0;

/// JPEG quality for stills saved with the "I" key (1–100). 92 keeps 4:4:4 chroma
/// subsampling and near-lossless detail at a reasonable file size.
const STILL_JPEG_QUALITY: u8 = 92;

/// How long a [`Flash`] stays at full opacity, and how long it then takes to
/// fade, in seconds. Long enough to register, short enough to be gone before it
/// is in the way of the clip underneath.
const FLASH_HOLD_S: f64 = 0.35;
const FLASH_FADE_S: f64 = 0.35;

/// How long the help plate takes to fade once H is let go, in seconds. Shorter
/// than a [`Flash`]'s: that one has to be noticed by someone who was not
/// expecting it, while this one is dismissed by the hand that summoned it and
/// should be out of the way as fast as it can be without simply blinking out.
const HELP_FADE_S: f64 = 0.2;

/// Font family holding the [`Flash`] icons, installed by [`install_icon_font`].
const ICON_FONT: &str = "icons";

/// Reported when the thread writing a still goes away without an outcome, which
/// means it panicked. Said rather than left pending forever.
const WRITER_GONE: &str = "the writer stopped unexpectedly";

/// Extensions we treat as video, for both the open dialog and prev/next navigation.
const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "mov", "webm", "avi", "m4v"];

/// What the help plate lists: a section heading and the keys under it.
///
/// Keys, and the one mouse gesture that hides. Clicking a thumbnail or dragging
/// the scrubber is found by simply using the mouse, whereas nothing on screen
/// announces any of these — which is what the plate exists to fix. Dragging the
/// frame belongs with the keys because it does nothing at all until the frame is
/// zoomed, so anyone who tries it first and pans later has already learnt it
/// doesn't work.
///
/// The sections earn their place: several keys mean different things depending
/// on the view, so a flat list would contradict itself on Enter and A/D.
/// H itself is left out — it is being held down to read this.
const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Anywhere",
        &[
            ("Left / Right", "Previous / next clip in the folder"),
            ("I", "Save the current frame as a JPEG beside the clip"),
            ("N", "Rename the clip"),
            ("Del", "Send the clip to the Recycle Bin and open the next"),
            ("F12", "Toggle fullscreen"),
            ("Esc", "Close the app"),
        ],
    ),
    (
        "Grid",
        &[
            ("W A S D", "Move the frame cursor"),
            ("Enter", "Play from the selected frame"),
            ("+ / -", "Larger / smaller thumbnails"),
        ],
    ),
    (
        "Playback",
        &[
            ("Space", "Pause or resume"),
            ("A / D", "Step half a second back / forward, and pause"),
            ("+ / - / Wheel", "Zoom into the frame"),
            ("Drag / 0", "Move the zoomed frame / fit it again"),
            ("Enter", "Back to the grid"),
        ],
    ),
];

/// True if `path` has a recognized video extension (case-insensitive).
fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTS.iter().any(|v| v.eq_ignore_ascii_case(e)))
}

/// A file's name for a message to the user. The full path is noise in a label
/// that already sits over the very clip it names, and the target's paths are
/// long. Falls back to the whole path for the odd one with no final component.
fn display_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Character index the rename dialog opens its caret at: right before the
/// extension's dot, so the name is what gets typed over and the `.mp4` is left
/// alone. A name with no dot puts it at the end, there being no extension to
/// keep clear of.
///
/// Counts characters rather than bytes because that is what a text caret is
/// indexed by, and the archive's names are not all ASCII — a byte offset would
/// land the caret mid-name on any of them, or inside a character.
fn caret_before_ext(name: &str) -> usize {
    let stem = name.rfind('.').unwrap_or(name.len());
    name[..stem].chars().count()
}

/// Whether `candidate` names a file that is already there and is not `current`
/// itself.
///
/// This is what stands between a typo and a lost clip: `fs::rename` overwrites
/// its destination without a word, so renaming onto a sibling would silently
/// bin footage that was never asked about.
///
/// Compared by canonical path rather than by name, because Windows matches file
/// names case-insensitively: `Clip.mp4` "exists" when `clip.mp4` does, and a
/// plain `exists()` would refuse to fix a clip's case — a rename onto itself,
/// which the OS is perfectly happy to perform. Anything that cannot be resolved
/// counts as taken: not knowing is a reason to leave a file alone, not to write
/// over it.
fn is_taken(candidate: &Path, current: &Path) -> bool {
    if !candidate.exists() {
        return false;
    }
    match (candidate.canonicalize(), current.canonicalize()) {
        (Ok(a), Ok(b)) => a != b,
        _ => true,
    }
}

/// Sibling video files sharing `current`'s directory, sorted by file name.
/// Read fresh from disk on every call so files added while the app is running
/// are picked up.
fn sibling_videos(current: &Path) -> Vec<PathBuf> {
    let dir = match current.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut vids: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && is_video(p))
        .collect();
    vids.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    vids
}

/// Path of the sibling `delta` positions from `current` in its folder
/// (−1 previous, +1 next), or `None` at the ends. Re-scans the directory,
/// so files added while the app is open are included.
fn neighbor_of(current: &Path, delta: i32) -> Option<PathBuf> {
    let name = current.file_name()?;
    let vids = sibling_videos(current);
    let idx = vids.iter().position(|p| p.file_name() == Some(name))?;
    let target = idx as i32 + delta;
    usize::try_from(target).ok().and_then(|t| vids.get(t)).cloned()
}

fn main() -> eframe::Result<()> {
    let log_path = logging::init();
    log::info!(
        "footage_viewer {} starting on {} ({}); log: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        log_path.display()
    );

    media::init().expect("failed to initialize ffmpeg");

    let mut pending = std::env::args().nth(1).map(PathBuf::from);
    if let Some(p) = &pending {
        if is_video(p) {
            log::info!("initial clip from command line: {}", p.display());
        } else {
            // Same as a dropped non-video: start empty rather than on a grid
            // that can never fill.
            log::warn!("ignoring unsupported file from command line: {}", p.display());
            pending = None;
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("footage_viewer")
            .with_inner_size([1000.0, 720.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../../icon.png"))
                    .expect("bundled icon.png is a valid PNG"),
            ),
        ..Default::default()
    };
    let result = eframe::run_native(
        "footage_viewer",
        options,
        Box::new(|cc| {
            install_icon_font(&cc.egui_ctx);
            Ok(Box::new(App::new(pending)))
        }),
    );
    match &result {
        Ok(()) => log::info!("exiting normally"),
        Err(e) => log::error!("eframe exited with error: {e}"),
    }
    result
}

/// Messages from the extraction worker to the UI.
enum Msg {
    Meta(media::GridMeta),
    Thumb(usize, media::Thumbnail),
    Done,
    Err(String),
}

/// One grid cell: waiting to be decoded, or an uploaded texture together with
/// the clip time it was sampled at (used to start playback there).
enum Cell {
    Pending,
    Ready {
        tex: egui::TextureHandle,
        time_s: f64,
    },
}

/// Messages from the playback decode thread to the UI.
enum PlayMsg {
    Frame(media::PlaybackFrame),
    Err(String),
}

/// What a [`Flash`] is confirming.
#[derive(Clone, Copy)]
enum FlashKind {
    StillSaved,
    Deleted,
}

impl FlashKind {
    /// The glyph to show, drawn in the [`ICON_FONT`] family. Both are monochrome
    /// there, so they take whatever colour the fade asks for.
    fn icon(self) -> &'static str {
        match self {
            Self::StillSaved => "📷",
            Self::Deleted => "🗑",
        }
    }
}

/// Pin the [`Flash`] icons to a single font.
///
/// egui's default families chain across two emoji fonts, and the two glyphs land
/// in different ones — the camera in NotoEmoji, the wastebasket in
/// emoji-icon-font. They are drawn in different styles and their fonts carry
/// different `FontTweak` scales, so one size request yields two visibly
/// mismatched icons. emoji-icon-font has both, so a family pinned to it alone
/// keeps the two confirmations looking like a pair.
fn install_icon_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.families.insert(
        egui::FontFamily::Name(ICON_FONT.into()),
        vec!["emoji-icon-font".to_owned()],
    );
    ctx.set_fonts(fonts);
}

/// A brief confirmation of an action that would otherwise leave no mark on
/// screen: saving a still writes a file the app never shows, and deleting a clip
/// looks the same as stepping to the next one. Shown centered over whatever view
/// is up and faded out after [`FLASH_HOLD_S`] + [`FLASH_FADE_S`].
struct Flash {
    kind: FlashKind,
    /// The file the action acted on. Named because the icon alone is ambiguous:
    /// by the time the wastebasket appears the *next* clip is already open
    /// underneath it, so without this it reads as if that one had been binned.
    name: String,
    /// egui time it was raised, which the fade is measured from.
    started_s: f64,
}

/// The help plate's state: solid for as long as H is held, then fading out from
/// the egui time it was let go.
#[derive(Default, Clone, Copy)]
enum Help {
    #[default]
    Hidden,
    Held,
    Fading(f64),
}

impl Help {
    /// What H being `held` at `now` means for the plate: the state to keep, and
    /// how opaque to draw it — `None` once it is gone entirely.
    ///
    /// Split from the drawing so the fade can be tested without a live window.
    fn advance(self, held: bool, now: f64) -> (Self, Option<f32>) {
        match (self, held) {
            // Taking H back mid-fade is a plain return to solid, which is why
            // this arm ignores the state it came from.
            (_, true) => (Self::Held, Some(1.0)),
            (Self::Held, false) => (Self::Fading(now), Some(1.0)),
            (Self::Fading(let_go_s), false) => {
                let left = 1.0 - (now - let_go_s) / HELP_FADE_S;
                if left <= 0.0 {
                    (Self::Hidden, None)
                } else {
                    (Self::Fading(let_go_s), Some(left as f32))
                }
            }
            (Self::Hidden, false) => (Self::Hidden, None),
        }
    }
}

/// The open rename dialog: the clip's name being edited over whichever view is
/// up. See [`rename_ui`](App::rename_ui).
struct Rename {
    /// The name as typed so far, seeded from the clip's file name.
    name: String,
    /// Whether the field has been handed the focus and the caret. Cleared again
    /// when a rename is refused, which hands both back — a refused name is one
    /// to fix, and Enter has already dropped the focus by then.
    seeded: bool,
    /// Why the last attempt was refused, shown under the field; `None` until one
    /// is. Kept here rather than in [`App::error`] because that one replaces the
    /// whole grid with red text — the right weight for a clip that would not
    /// delete, far too much for a name that needs a character changed.
    error: Option<String>,
}

/// A still being written on a background thread: where it is going, and the
/// channel its outcome arrives on.
///
/// Writing one is not cheap — it opens the clip afresh, seeks, decodes a frame
/// at full source resolution and encodes a JPEG — and on the 4K footage this
/// tool targets that is long enough to be seen as a freeze if it runs on the UI
/// thread. See [`save_still`](App::save_still).
struct Saving {
    out: PathBuf,
    /// The time in the clip being saved, kept for the log a failure writes —
    /// which frame was asked for is what makes one reproducible.
    time_s: f64,
    /// When the write was started, for the log the outcome writes. `save_frame_jpeg`
    /// times its own stages, but only this side can see what the user waited: the
    /// confirmation is raised from the outcome, so this spans the write *plus* the
    /// repaint that delivers it. A gap between the two is the UI being slow to
    /// collect a still that was ready.
    started: Instant,
    /// `Ok` once the still is written, else why it failed.
    rx: Receiver<Result<(), String>>,
}

/// Active playback of the loaded clip, filling the window. One live decoder
/// thread paces frames to real time and reacts to [`media::PlayCommand`]s; the UI
/// sends commands (seek/pause) and displays whatever frame last arrived.
struct Player {
    /// Commands to the live decoder: seek, pause, resume, stop.
    cmds: mpsc::Sender<media::PlayCommand>,
    rx: Receiver<PlayMsg>,
    /// The live decoder thread, which holds the clip's file open for as long as
    /// it runs. Joined by [`stop_playback_and_wait`](App::stop_playback_and_wait)
    /// when the file itself must be free; `None` only while being torn down.
    decoder: Option<thread::JoinHandle<()>>,
    tex: Option<egui::TextureHandle>,
    frame_size: egui::Vec2,
    /// Media time of the last shown frame, for the scrubber handle.
    position_s: f64,
    /// UI-side pause tracking, toggled by Space.
    paused: bool,
    /// While the scrubber is being dragged, the target time under the cursor,
    /// used to position the handle.
    scrub: Option<f64>,
}

/// How the played frame is placed in the video area: `zoom` as a multiple of the
/// fit-to-window scale (1.0 being the whole frame letterboxed), and `offset`
/// shifting it from centered, in screen points.
///
/// Kept on [`App`] rather than [`Player`] so it survives a trip back to the grid
/// and a replay of the same clip — the frame being studied is usually the reason
/// for that trip. [`open`](App::open) resets it, so a new clip always arrives
/// whole.
struct View {
    zoom: f32,
    offset: egui::Vec2,
}

impl Default for View {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            offset: egui::Vec2::ZERO,
        }
    }
}

impl View {
    /// Where the frame lands inside `container`: the fit rect scaled by `zoom`
    /// about the container's center, shifted by `offset`.
    ///
    /// Clamps `offset` on the way, which is what stops a drag from throwing the
    /// frame off into the black: an axis the zoomed frame overflows may be panned
    /// only as far as its own edge, and one it doesn't fill has no slack at all,
    /// so it stays centered exactly as it did before any of this existed. That
    /// also means zoom 1.0 pins the offset to zero and no separate "is it zoomed"
    /// test is needed anywhere.
    ///
    /// Clamping here rather than where the drag is read is what makes that true
    /// for every path into the offset — a drag, a zoom out from a panned-away
    /// corner, or the window being resized under a still frame.
    fn place(&mut self, container: egui::Rect, frame_size: egui::Vec2) -> egui::Rect {
        let size = fit_centered(container, frame_size).size() * self.zoom;
        let slack = ((size - container.size()) * 0.5).max(egui::Vec2::ZERO);
        self.offset = self.offset.clamp(-slack, slack);
        egui::Rect::from_center_size(container.center() + self.offset, size)
    }

    /// Multiply the zoom by `factor`, keeping whatever sits under `anchor` there.
    ///
    /// Anchoring is what makes the wheel land on the thing being looked at rather
    /// than the middle of the window; `+`/`-` pass the center and get plain zoom
    /// about it.
    fn zoom_by(&mut self, factor: f32, anchor: egui::Pos2, container: egui::Rect) {
        let before = self.zoom;
        self.zoom = (self.zoom * factor).clamp(1.0, ZOOM_MAX);
        // Read back rather than reusing `factor`: at either end of the clamp the
        // zoom didn't move the whole way (or at all), and scaling the offset by
        // what was asked for would drift the frame under a wheel that is doing
        // nothing.
        let scale = self.zoom / before;
        // The frame's center is `container.center() + offset`. Scaling that about
        // `anchor` is what holds the anchored point still.
        let center = container.center() + self.offset;
        self.offset = (center - anchor) * scale + anchor.to_vec2() - container.center().to_vec2();
    }
}

/// A clip being (or already) loaded.
struct Loaded {
    path: PathBuf,
    duration_s: f64,
    aspect: f32, // height / width of a thumbnail
    cells: Vec<Cell>,
    ready: usize,
    done: bool,
    rx: Receiver<Msg>,
    /// Selected grid cell, moved with AWSD; Enter plays it.
    cursor: usize,
    /// Stops the extraction worker; raised by `Drop`.
    cancel: Arc<AtomicBool>,
    /// The extraction worker, which holds the clip's file open until it returns.
    /// Joined by [`stop_extraction_and_wait`](App::stop_extraction_and_wait) when
    /// the file itself must be free; `None` once taken, and for a grid that never
    /// had a worker.
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for Loaded {
    /// Stop extracting a clip the moment it is dropped — which is the moment
    /// nothing can display it any more (navigating away, a failure, deleting it).
    ///
    /// The worker does not notice on its own: it ignores the send error from the
    /// closed channel and would read the clip to the end regardless. On a local
    /// disk that is invisible, but the target archive lives on an external HDD
    /// where each abandoned pass holds the single head for seconds — hold a key
    /// to skip through a folder and every clip passed leaves a thread fighting
    /// the one clip the user is actually waiting for.
    ///
    /// Done here rather than at each call site so it cannot be forgotten by one.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct App {
    pending_open: Option<PathBuf>,
    loaded: Option<Loaded>,
    /// Some while a clip is playing back over the grid.
    player: Option<Player>,
    /// Grids of clips viewed earlier, newest first, so stepping back to one is
    /// instant. Bounded by [`RECENT_MAX_THUMBS`]; holds only finished grids.
    recent: VecDeque<Loaded>,
    /// The next sibling's grid, being read in the background so stepping forward
    /// is instant. Its thumbnails stay in the channel until it is opened — only
    /// then does anyone poll them, so its worker parks once the channel is full
    /// ([`GRID_QUEUE_THUMBS`]) and a long sibling can neither fill memory nor hold
    /// the disk to the end. See [`look_ahead`](Self::look_ahead).
    prefetch: Option<Loaded>,
    /// Whether the current clip has had its look-ahead decision made, so the
    /// folder is scanned once per clip rather than once per repaint.
    looked_ahead: bool,
    /// Set while a live seek fired from the scrubber has not produced its frame
    /// yet, so a drag never has more than one seek in flight (see [`seek_bar_ui`]).
    scrub_in_flight: bool,
    /// A/D seek key-repeat state in the player: sign of the held seek key
    /// (−1 A, +1 D, 0 none), the egui time the next repeat is due, and the
    /// running target so fast repeats accumulate even when frames lag behind.
    seek_dir: i32,
    seek_next_fire: f64,
    seek_target: f64,
    /// How many columns the frame grid shows; changed with the `-`/`+` keys and
    /// kept across clips. Clamped to [`GRID_COLS_MIN`]..=[`GRID_COLS_MAX`].
    grid_cols: usize,
    /// Zoom and pan of the played frame. See [`View`].
    view: View,
    /// The confirmation icon currently fading over the view, if any.
    flash: Option<Flash>,
    /// The plate listing the keys, shown for as long as H is held.
    help: Help,
    /// The rename dialog, while one is open. See [`rename_ui`](Self::rename_ui).
    rename: Option<Rename>,
    /// The still being written, if any. At most one runs at a time — see
    /// [`save_still`](App::save_still).
    saving: Option<Saving>,
    /// A still asked for while another was being written: the clip, and the time
    /// in it to save. Started once the running one lands.
    pending_save: Option<(PathBuf, f64)>,
    error: Option<String>,
    /// The clip path currently reflected in the window title, so the title is
    /// only updated when the loaded clip actually changes.
    title_path: Option<PathBuf>,
}

impl App {
    fn new(pending: Option<PathBuf>) -> Self {
        Self {
            pending_open: pending,
            grid_cols: GRID_COLS_DEFAULT,
            ..Default::default()
        }
    }

    /// Sibling `delta` positions from the currently loaded clip, or `None` at
    /// the ends or with nothing loaded.
    fn neighbor(&self, delta: i32) -> Option<PathBuf> {
        neighbor_of(&self.loaded.as_ref()?.path, delta)
    }

    /// Set the outgoing clip aside in [`recent`](Self::recent) so stepping back to
    /// it later is free, evicting the oldest grids to stay inside the thumbnail
    /// budget. Leaves `self.loaded` empty.
    ///
    /// Only a finished grid is kept. An unfinished one is dropped instead — which
    /// cancels it — because parking it would leave its worker reading a clip
    /// nobody is looking at, exactly the stolen disk head that cancellation
    /// exists to prevent.
    fn park_current(&mut self) {
        let Some(l) = self.loaded.take() else { return };
        if !l.done {
            return;
        }
        self.recent.push_front(l);

        let mut total: usize = self.recent.iter().map(|l| l.cells.len()).sum();
        while total > RECENT_MAX_THUMBS {
            match self.recent.pop_back() {
                Some(evicted) => total -= evicted.cells.len(),
                None => break,
            }
        }
    }

    /// Take `path`'s grid out of the recent cache, if it is still there.
    fn take_recent(&mut self, path: &Path) -> Option<Loaded> {
        let i = self.recent.iter().position(|l| l.path == path)?;
        self.recent.remove(i)
    }

    /// Show `path`'s grid, without ever re-reading a clip we already have: from
    /// the recent cache, or from a prefetch already reading it, else by kicking
    /// off a fresh extraction. Returns immediately in every case.
    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        log::info!("opening clip: {}", path.display());
        self.error = None;
        // Leaving any current clip: stop playback and fall back to the grid.
        self.player = None;
        // A new clip deserves its own look ahead.
        self.looked_ahead = false;
        // ...and its own zoom: a crop framed on the last clip means nothing on
        // this one, and stepping through a folder zoomed in would hide that the
        // clip even changed.
        self.view = View::default();

        // Set the outgoing grid aside before looking, so re-opening the very clip
        // being left still finds it.
        self.park_current();

        // Claim a prefetch already reading this clip; one reading anything else is
        // now pointless, and `filter` drops it here — which cancels it and hands
        // the disk back to the clip being opened.
        let prefetched = self.prefetch.take().filter(|l| l.path == path);

        if let Some(l) = self.take_recent(&path) {
            // Logged so a tester's log doesn't show an "opening clip" with no
            // grid timing after it and read as a stall.
            log::info!("grid served from cache: {}", path.display());
            self.loaded = Some(l);
            return;
        }
        if let Some(l) = prefetched {
            log::info!("grid taken over from prefetch: {}", path.display());
            self.loaded = Some(l);
            return;
        }

        self.loaded = Some(spawn_extraction(ctx, path));
    }

    /// Read the next sibling's grid in the background while the user studies the
    /// current one, so stepping forward — the way a folder actually gets worked
    /// through — finds it already read. Forward only: that is the dominant
    /// motion, and stepping back is already free via the recent cache.
    ///
    /// Reading ahead must never compete with the foreground, because the archive
    /// this tool targets lives on an external HDD with a single head. So it waits
    /// for the current grid to finish (the clip the user is actually waiting for
    /// gets the disk to itself) and never starts while a clip is playing (its
    /// seeks must not queue behind a read nobody asked for). Once started it is
    /// left to finish: cancelling it on every play would restart the read from
    /// zero each time the user came back, so it would churn the disk forever and
    /// never deliver a grid.
    fn look_ahead(&mut self, ctx: &egui::Context) {
        // Decided once per clip, not once per repaint: `neighbor` re-scans the
        // folder, which is far too much to redo on every frame.
        if self.looked_ahead || self.player.is_some() {
            return;
        }
        let Some(l) = &self.loaded else { return };
        if !l.done {
            return;
        }
        self.looked_ahead = true;

        let Some(next) = self.neighbor(1) else { return };
        if self.recent.iter().any(|l| l.path == next) {
            return;
        }
        log::info!("reading ahead: {}", next.display());
        self.prefetch = Some(spawn_extraction(ctx, next));
    }

    /// Reflect the loaded clip's path in the window title, resending the command
    /// only when the path changes rather than every frame.
    fn sync_title(&mut self, ctx: &egui::Context) {
        let path = self.loaded.as_ref().map(|l| l.path.clone());
        if path == self.title_path {
            return;
        }
        let title = match &path {
            Some(p) => format!("{} — footage_viewer", p.display()),
            None => "footage_viewer".to_owned(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        self.title_path = path;
    }

    /// Drain whatever the worker has produced since the last frame.
    fn poll(&mut self, ctx: &egui::Context) {
        let mut failed: Option<String> = None;
        if let Some(l) = &mut self.loaded {
            while !l.done {
                match l.rx.try_recv() {
                    Ok(Msg::Meta(m)) => {
                        l.duration_s = m.duration_s;
                        if m.src_w > 0 {
                            l.aspect = m.src_h as f32 / m.src_w as f32;
                        }
                    }
                    Ok(Msg::Thumb(i, t)) => {
                        let img = egui::ColorImage::from_rgba_unmultiplied(
                            [t.width as usize, t.height as usize],
                            &t.rgba,
                        );
                        let tex =
                            ctx.load_texture(format!("thumb_{i}"), img, egui::TextureOptions::default());
                        // Thumbnails stream in order, but grow defensively so an
                        // out-of-order index can never panic on indexing.
                        if i >= l.cells.len() {
                            l.cells.resize_with(i + 1, || Cell::Pending);
                        }
                        l.cells[i] = Cell::Ready {
                            tex,
                            time_s: t.time_s,
                        };
                        l.ready += 1;
                    }
                    Ok(Msg::Done) => l.done = true,
                    Ok(Msg::Err(e)) => {
                        failed = Some(e);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => l.done = true,
                }
            }
        }
        if let Some(e) = failed {
            let clip = self.loaded.as_ref().map(|l| l.path.display().to_string());
            log::error!("extraction failed for {}: {e}", clip.as_deref().unwrap_or("?"));
            self.error = Some(e);
            self.loaded = None;
        }
    }

    /// Start playing the loaded clip from the keyframe before `start_from_s`.
    /// Spawns the live decoder thread (kept open for the whole clip) and wires up
    /// the frame and command channels; seeking afterwards is a `PlayCommand`, not
    /// a new thread. The full-resolution decode and pacing live in `media`.
    fn play(&mut self, ctx: &egui::Context, start_from_s: f64) {
        let Some(l) = &self.loaded else { return };
        let path = l.path.clone();

        let (frame_tx, frame_rx) = mpsc::sync_channel::<PlayMsg>(3);
        let (cmd_tx, cmd_rx) = mpsc::channel::<media::PlayCommand>();
        let ctx_frame = ctx.clone();
        let decoder = thread::spawn(move || {
            let result = media::play_stream(&path, start_from_s, PLAYBACK_LONG, cmd_rx, |f| {
                let delivered = frame_tx.send(PlayMsg::Frame(f)).is_ok();
                ctx_frame.request_repaint();
                delivered
            });
            if let Err(e) = result {
                let _ = frame_tx.send(PlayMsg::Err(format!("{e:#}")));
            }
            // Wake the UI so it notices the closed channel (end-of-stream/error).
            ctx_frame.request_repaint();
        });

        self.player = Some(Player {
            cmds: cmd_tx,
            rx: frame_rx,
            decoder: Some(decoder),
            tex: None,
            frame_size: egui::Vec2::ZERO,
            position_s: start_from_s,
            paused: false,
            scrub: None,
        });
    }

    /// Send a command to the live decoder, if a clip is playing.
    fn send_cmd(&self, cmd: media::PlayCommand) {
        if let Some(p) = &self.player {
            let _ = p.cmds.send(cmd);
        }
    }

    /// Take the latest frame the decoder produced and upload it (it already paces
    /// itself, so whatever arrived is due). Returns `false` when playback is over
    /// — the decoder closed its channel (end-of-stream) or hit an error — so the
    /// caller drops the player and returns to the grid.
    fn advance_player(&mut self, ctx: &egui::Context) -> bool {
        let Some(p) = &mut self.player else { return true };
        let mut latest = None;
        let mut ended = false;
        loop {
            match p.rx.try_recv() {
                Ok(PlayMsg::Frame(f)) => latest = Some(f),
                Ok(PlayMsg::Err(e)) => {
                    log::error!("playback error: {e}");
                    self.error = Some(e);
                    return false;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    ended = true;
                    break;
                }
            }
        }
        if let Some(f) = latest {
            // A frame arriving means any scrub we fired has landed: the decoder
            // emits exactly one frame per `Scrub` and then holds on it.
            self.scrub_in_flight = false;
            p.frame_size = egui::vec2(f.width as f32, f.height as f32);
            p.position_s = f.time_s;
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [f.width as usize, f.height as usize],
                &f.rgba,
            );
            match &mut p.tex {
                Some(tex) => tex.set(img, egui::TextureOptions::default()),
                None => {
                    p.tex =
                        Some(ctx.load_texture("playback", img, egui::TextureOptions::default()))
                }
            }
        }
        !ended
    }

    /// Space toggles pause; the decoder holds or resumes its own clock.
    fn toggle_pause(&mut self) {
        if let Some(p) = &mut self.player {
            p.paused = !p.paused;
            let cmd = if p.paused {
                media::PlayCommand::Pause
            } else {
                media::PlayCommand::Resume
            };
            let _ = p.cmds.send(cmd);
        }
    }

    /// Raise the confirmation icon for `kind` naming `path`, replacing whatever
    /// flash was still fading.
    ///
    /// The repaint request is what animates it: neither view drives repaints of
    /// its own once it is idle, so without this the flash would freeze at
    /// whatever the next incidental frame caught and never fade out.
    fn show_flash(&mut self, ctx: &egui::Context, kind: FlashKind, path: &Path) {
        self.flash = Some(Flash {
            kind,
            name: display_name(path),
            started_s: ctx.input(|i| i.time),
        });
        ctx.request_repaint();
    }

    /// Draw the flash over the view, and drop it once it has faded out.
    ///
    /// It goes in its own foreground layer, so it lands on top whichever view is
    /// up and regardless of when in the frame this runs. That is what lets it run
    /// before either view is built — ahead of the early returns that would
    /// otherwise skip it on exactly the paths that raise it. The cost is that a
    /// flash raised later in the same frame first shows on the next one, which is
    /// the repaint [`show_flash`](Self::show_flash) asks for.
    fn flash_ui(&mut self, ctx: &egui::Context) {
        let Some(f) = &self.flash else { return };
        let age = ctx.input(|i| i.time) - f.started_s;
        if age >= FLASH_HOLD_S + FLASH_FADE_S {
            self.flash = None;
            return;
        }
        let alpha: f32 = if age <= FLASH_HOLD_S {
            1.0
        } else {
            (1.0 - (age - FLASH_HOLD_S) / FLASH_FADE_S) as f32
        };

        // The icon sits on a dark plate because the backdrop is not one thing: a
        // bare white glyph reads over playback's black, but is lost among the
        // thumbnails of the grid.
        egui::Area::new(egui::Id::new("flash"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .interactable(false)
            // The fade is ours; egui's own fade-in would fight it.
            .fade_in(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_black_alpha(180).gamma_multiply(alpha))
                    .corner_radius(16.0)
                    .inner_margin(18.0)
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(f.kind.icon())
                                    .font(egui::FontId::new(
                                        64.0,
                                        egui::FontFamily::Name(ICON_FONT.into()),
                                    ))
                                    .color(egui::Color32::WHITE.gamma_multiply(alpha)),
                            );
                            // Extend, not wrap: the plate is sized by its
                            // content, and a wrapping name would be laid out
                            // against a width that the name itself decides —
                            // so a name broke across two lines depending on
                            // which pass won. Letting it size the plate keeps
                            // one line and one predictable shape.
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(f.name.as_str())
                                        .size(13.0)
                                        .color(egui::Color32::from_gray(190).gamma_multiply(alpha)),
                                )
                                .extend(),
                            );
                        });
                    });
            });
        ctx.request_repaint();
    }

    /// Draw the plate listing the keys ([`HELP`]) for as long as H is held.
    ///
    /// Held rather than toggled, so it is never left standing over the very clip
    /// it explains and needs no key of its own to dismiss. It is drawn like a
    /// [`Flash`] and for the same reasons: its own foreground layer over
    /// whichever view is up, run before either is built since playback returns
    /// early, and not interactable so clicks reach the clip underneath.
    fn help_ui(&mut self, ctx: &egui::Context) {
        let (held, now) = ctx.input(|i| (i.key_down(egui::Key::H), i.time));
        let (state, alpha) = self.help.advance(held, now);
        self.help = state;
        let Some(alpha) = alpha else { return };

        let text = |s: &str, gray: u8| {
            egui::RichText::new(s).color(egui::Color32::from_gray(gray).gamma_multiply(alpha))
        };
        egui::Area::new(egui::Id::new("help"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .interactable(false)
            .fade_in(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_black_alpha(210).gamma_multiply(alpha))
                    .corner_radius(16.0)
                    .inner_margin(18.0)
                    .show(ui, |ui| {
                        egui::Grid::new("help_keys")
                            .num_columns(2)
                            .spacing([18.0, 6.0])
                            .show(ui, |ui| {
                                for (i, (section, keys)) in HELP.iter().enumerate() {
                                    if i > 0 {
                                        ui.end_row(); // blank row between sections
                                    }
                                    // The heading is the dimmest thing on the
                                    // plate: it is a label for the block under
                                    // it, and the keys are what is being read.
                                    ui.label(text(section, 140));
                                    ui.end_row();
                                    for (key, what) in *keys {
                                        ui.label(text(key, 255).monospace());
                                        ui.label(text(what, 190));
                                        ui.end_row();
                                    }
                                }
                            });
                    });
            });

        // The fade is ours to drive: nothing else asks for repaints once a grid
        // is finished, so without this the plate would freeze at whatever
        // opacity the last incidental frame caught and stay there. While H is
        // held the picture is static, so it costs nothing to leave it alone.
        if !held {
            ctx.request_repaint();
        }
    }

    /// Open the rename dialog on the loaded clip, seeded with its file name.
    ///
    /// Playback is held first, and stays held once the dialog is gone: a clip
    /// running on behind a dialog would be somewhere else by the time a name was
    /// typed, and it is the frame that prompted the rename that the user is
    /// looking at. Space resumes it, as it always does.
    fn begin_rename(&mut self) {
        let Some(l) = &self.loaded else { return };
        self.rename = Some(Rename {
            name: display_name(&l.path),
            seeded: false,
            error: None,
        });
        if let Some(p) = &mut self.player {
            if !p.paused {
                p.paused = true;
                let _ = p.cmds.send(media::PlayCommand::Pause);
            }
        }
    }

    /// Draw the rename dialog and act on it, while one is open. Enter renames,
    /// Escape or a click outside walks away, and a refused name stays up with
    /// the reason under it.
    ///
    /// Drawn from its own layer before either view, like [`Flash`] and [`Help`]
    /// and for the same reason: playback returns early, so a dialog built inside
    /// a view could not serve both. Running first is also what lets it take the
    /// keyboard — see the strip at the end.
    fn rename_ui(&mut self, ctx: &egui::Context) {
        // Taken rather than borrowed: the dialog is closed unless something
        // below puts it back, so every way out of here — renamed, cancelled, or
        // the clip going away underneath — leaves it shut on its own.
        let Some(mut r) = self.rename.take() else { return };
        // The clip can go out from under an open dialog: `poll` drops it when
        // its extraction fails. There is then nothing left to rename.
        if self.loaded.is_none() {
            return;
        }

        let mut confirm = false;
        let modal = egui::Modal::new(egui::Id::new("rename")).show(ctx, |ui| {
            // Wide enough for the target archive's long names without the field
            // scrolling, and the dialog is sized by it rather than by the name —
            // so it doesn't resize itself under every keystroke.
            ui.set_min_width(460.0);
            ui.label("Rename clip");
            ui.add_space(6.0);
            let out = egui::TextEdit::singleline(&mut r.name)
                .desired_width(f32::INFINITY)
                .show(ui);
            if !r.seeded {
                // The field has no focus on the frame it first appears, so what
                // is stored here is what it picks up when the focus lands next
                // frame — ahead of the end-of-text caret TextEdit would
                // otherwise start from.
                out.response.request_focus();
                let caret = egui::text::CCursor::new(caret_before_ext(&r.name));
                let mut state = out.state;
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(caret)));
                state.store(ctx, out.response.id);
                r.seeded = true;
            }
            // Enter confirms. Read through `lost_focus` because that is what a
            // single-line field does with Enter — it is the press that ends the
            // edit, not any press while it runs.
            confirm = out.response.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter));
            if let Some(e) = &r.error {
                ui.add_space(6.0);
                ui.colored_label(egui::Color32::from_rgb(255, 120, 120), e);
            }
        });
        // Escape, or a click on the backdrop.
        let cancel = modal.should_close();

        // The dialog owns the keyboard for the rest of the frame. Both views
        // read keys straight off the input state rather than through the focus,
        // so without this every letter of a name would drive them too: "a" and
        // "d" would seek, "i" would write a still, "h" would raise the help
        // plate. It has to come after the field has read its own events, which
        // is why it sits here and not at the top of the frame. The pointer needs
        // no such help — the modal's backdrop already blocks what is under it.
        ctx.input_mut(|i| {
            i.events
                .retain(|e| !matches!(e, egui::Event::Key { .. } | egui::Event::Text(_)));
            i.keys_down.clear();
        });

        if cancel {
            return;
        }
        if !confirm {
            self.rename = Some(r);
            return;
        }
        if let Err(e) = self.commit_rename(ctx, &r.name) {
            // Stay up with the reason: a name that collides or lost its
            // extension is a character to fix, not a dialog to start over.
            r.error = Some(e);
            r.seeded = false;
            self.rename = Some(r);
        }
    }

    /// Rename the loaded clip to `name`, taking its sidecar still along, and
    /// leave the app on the renamed clip — the grid intact where it can be, and
    /// playback held where it was. Returns why it was refused, for the dialog to
    /// show under the field; `Ok` means the dialog is done.
    ///
    /// The name is checked over before anything is stopped, because the checks
    /// are what a typo hits: a collision or a dropped extension then costs
    /// nothing and leaves the clip exactly as it was. Only a name that passes is
    /// worth tearing the readers down for — Windows refuses to rename a file
    /// that anything holds open, which is the same reason
    /// [`delete_current`](Self::delete_current) stops all three of them.
    fn commit_rename(&mut self, ctx: &egui::Context, name: &str) -> Result<(), String> {
        let Some(l) = &self.loaded else { return Ok(()) };
        let path = l.path.clone();
        let name = name.trim();
        if name.is_empty() {
            return Err("The name cannot be empty.".to_owned());
        }
        // A name, not a path: this renames a clip where it lies, and a separator
        // would move it somewhere else entirely.
        if name.contains(['/', '\\']) {
            return Err("The name cannot contain \\ or /.".to_owned());
        }
        let new_path = path.with_file_name(name);
        if new_path == path {
            return Ok(());
        }
        // A clip renamed out of `VIDEO_EXTS` would still be the one on screen,
        // but the folder scan behind prev/next would no longer list it — so it
        // could be neither stepped away from nor found again. Refused rather
        // than left in a view with no way on.
        if !is_video(&new_path) {
            return Err(format!(
                "The name has to keep a video extension ({}).",
                VIDEO_EXTS.join(", ")
            ));
        }
        if is_taken(&new_path, &path) {
            return Err(format!("\"{name}\" is already in this folder."));
        }

        // Nothing of ours may hold the clip open, or Windows turns the rename
        // down the same way it turns a delete down: the decoder for as long as
        // playback lasts, the grid worker until its pass ends, and a still until
        // its frame is written.
        let playing_at = self.player.as_ref().map(|p| p.position_s);
        self.stop_playback_and_wait();
        let was_reading = self.stop_extraction_and_wait();
        self.finish_save_and_wait(ctx);

        let result = std::fs::rename(&path, &new_path);
        // Whichever way it went, the readers that were stopped go back on the
        // clip that is actually there now — the new name if the rename landed,
        // the old one if it didn't.
        let clip = if result.is_ok() { &new_path } else { &path };
        if was_reading {
            // The partial grid went with the worker, so this one starts over.
            // The alternative was leaving the user on an error with no grid,
            // which is what prev/next and DEL work from.
            self.loaded = Some(spawn_extraction(ctx, clip.clone()));
        } else if let Some(l) = &mut self.loaded {
            // A finished grid is simply relabelled: the thumbnails are of frames
            // the rename didn't touch, so re-reading the clip would spend the
            // disk to arrive back where it already is.
            l.path = clip.clone();
        }
        if let Some(pos) = playing_at {
            self.play(ctx, pos);
            // Land back on the exact frame that was showing and hold there: the
            // dialog paused it, and `play` would otherwise run on from the
            // keyframe before it.
            if let Some(p) = &mut self.player {
                p.paused = true;
            }
            self.send_cmd(media::PlayCommand::Scrub(pos));
        }

        if let Err(e) = result {
            log::error!(
                "failed to rename {} to {}: {e}",
                path.display(),
                new_path.display()
            );
            return Err(format!("Could not rename it: {e}"));
        }
        log::info!("renamed {} to {}", path.display(), new_path.display());

        // The sidecar still written by `save_still` is named after the clip, so
        // it follows the clip — the same reason a delete bins it. The clip is
        // already renamed by here, so a still that won't move is reported and
        // lived with rather than rolled back.
        let still = path.with_extension("jpg");
        if still.exists() {
            let new_still = new_path.with_extension("jpg");
            if let Err(e) = std::fs::rename(&still, &new_still) {
                log::error!("failed to rename sidecar {}: {e}", still.display());
                self.error = Some(format!(
                    "Renamed the clip, but its still \"{}\" stayed — it may still be in use.",
                    display_name(&still)
                ));
            }
        }
        Ok(())
    }

    /// Ask for a full-resolution JPEG of the frame at `time_s`, written next to
    /// the loaded clip as `<clip-stem>.jpg` and overwriting any existing file.
    /// A no-op with nothing loaded. The write happens on a background thread, so
    /// this returns at once and the outcome lands later in
    /// [`poll_save`](Self::poll_save).
    ///
    /// Only one runs at a time, and a request arriving while one is in flight is
    /// held rather than started. Two saves of the same clip write the same
    /// `<clip-stem>.jpg`, so running both would leave two threads racing on one
    /// file; holding the later one and letting it win when the running save
    /// lands is also what the synchronous save did, since it simply overwrote
    /// whatever the earlier press had put there.
    fn save_still(&mut self, ctx: &egui::Context, time_s: f64) {
        let Some(l) = &self.loaded else { return };
        let request = (l.path.clone(), time_s);
        if self.saving.is_some() {
            // The wait this press turns into is its own write plus all of the one
            // already running, which no single timing below shows.
            log::info!("still at {time_s:.3}s held behind one being written");
            self.pending_save = Some(request);
            return;
        }
        self.start_save(ctx, request);
    }

    /// Start writing the frame at `time_s` of `path` on a background thread.
    fn start_save(&mut self, ctx: &egui::Context, (path, time_s): (PathBuf, f64)) {
        let out = path.with_extension("jpg");
        let (tx, rx) = mpsc::channel();
        let (ctx_end, worker_out) = (ctx.clone(), out.clone());
        thread::spawn(move || {
            let result = media::save_frame_jpeg(&path, time_s, &worker_out, STILL_JPEG_QUALITY)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            // Wake the UI so it delivers the outcome; nothing else is driving
            // repaints while the app sits on a finished grid.
            ctx_end.request_repaint();
        });
        self.saving = Some(Saving {
            out,
            time_s,
            started: Instant::now(),
            rx,
        });
    }

    /// Take the outcome of a save that has landed: confirm it, or record why it
    /// failed. Then start whatever request was held behind it.
    fn settle_save(&mut self, ctx: &egui::Context, s: Saving, result: Result<(), String>) {
        let out = s.out;
        let waited_ms = s.started.elapsed().as_secs_f64() * 1000.0;
        match result {
            Ok(()) => {
                log::info!("saved still {} | 📷 shown after {waited_ms:.0}ms", out.display());
                // Names the still rather than the clip: the file is written next
                // to the clip and never shown, so where it went is the one thing
                // the confirmation can usefully say.
                self.show_flash(ctx, FlashKind::StillSaved, &out);
            }
            Err(e) => {
                log::error!(
                    "failed to save still {} at {:.3}s: {e}",
                    out.display(),
                    s.time_s
                );
                self.error = Some(format!(
                    "Could not save the still \"{}\": {e}",
                    display_name(&out)
                ));
            }
        }
        if let Some(request) = self.pending_save.take() {
            self.start_save(ctx, request);
        }
    }

    /// Deliver the outcome of a still being written, once it has one.
    fn poll_save(&mut self, ctx: &egui::Context) {
        let Some(s) = &self.saving else { return };
        let result = match s.rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(WRITER_GONE.to_owned()),
        };
        let Some(s) = self.saving.take() else { return };
        self.settle_save(ctx, s, result);
    }

    /// Wait for a still being written to land, so the clip it reads is closed by
    /// the time this returns. A no-op with no save in flight.
    ///
    /// A save holds its clip open while it decodes, and Windows will not bin a
    /// file that is open. Unlike the grid worker there is nothing to cancel —
    /// `save_frame_jpeg` takes no cancel flag, and half a JPEG would be worth
    /// less than the wait saves — so the delete waits it out.
    ///
    /// Receiving is enough to know the file is closed: the worker sends only
    /// after `save_frame_jpeg` has returned and dropped the input it opened,
    /// which is the same reasoning [`stop_extraction_and_wait`] leans on to
    /// leave a finished grid alone.
    ///
    /// [`stop_extraction_and_wait`]: Self::stop_extraction_and_wait
    fn finish_save_and_wait(&mut self, ctx: &egui::Context) {
        let Some(s) = self.saving.take() else { return };
        let result = s.rx.recv().unwrap_or_else(|_| Err(WRITER_GONE.to_owned()));
        // Drop a request held behind it before settling, or `settle_save` would
        // start it and hand the clip straight back to a reader — the one thing
        // the caller waited to be rid of.
        self.pending_save = None;
        self.settle_save(ctx, s, result);
    }

    /// Send the currently loaded clip to the recycle bin without confirmation
    /// and return the sibling to open next — the following clip, or the previous
    /// one if the deleted clip was the last in its folder. Returns `None` when
    /// nothing is loaded, the delete failed (reason recorded in `self.error`), or
    /// the folder is now empty. On a successful delete the binned clip's grid is
    /// dropped, so the caller can simply return to the empty view.
    ///
    /// All three of the clip's readers stop first — playback, a grid still being
    /// extracted, and a still being written — since each holds the file open and
    /// Windows will not bin it until they let go. A caller that was playing
    /// therefore replays the clip it is handed rather than carrying on. A delete
    /// that fails lands back on the grid with the reason showing, and an
    /// interrupted read is started again so that grid is whole.
    fn delete_current(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        let path = self.loaded.as_ref()?.path.clone();
        // Resolve the neighbor before deleting, while the clip still lists.
        let target = self.neighbor(1).or_else(|| self.neighbor(-1));
        // Nothing of ours may still hold the clip open, or Windows refuses to bin
        // it. All three readers do: the decoder for as long as playback lasts,
        // the grid worker until its pass ends, and a still being written until
        // its frame is decoded.
        self.stop_playback_and_wait();
        let was_reading = self.stop_extraction_and_wait();
        self.finish_save_and_wait(ctx);
        if let Err(e) = trash::delete(&path) {
            log::error!("failed to delete {}: {e}", path.display());
            // The read was stopped to free the file and the bin turned it down
            // anyway, so start it again. The grid is what prev/next and a second
            // DEL work from; without it the user is left at an error with no way
            // out but the open dialog.
            if was_reading {
                self.loaded = Some(spawn_extraction(ctx, path.clone()));
            }
            self.error = Some(format!(
                "Could not send \"{}\" to the Recycle Bin — it may still be in use.",
                display_name(&path)
            ));
            return None;
        }
        log::info!("deleted clip {}", path.display());
        self.show_flash(ctx, FlashKind::Deleted, &path);
        // Drop the binned clip's grid rather than let the next `open` park it in
        // the recent cache: it would hold texture memory for a file that is gone
        // and that prev/next can never reach again, since they re-scan the folder.
        self.loaded = None;
        // Also bin the sidecar still saved by save_still (`<clip-stem>.jpg`), if any.
        let still = path.with_extension("jpg");
        if still.exists() {
            if let Err(e) = trash::delete(&still) {
                log::error!("failed to delete sidecar {}: {e}", still.display());
                self.error = Some(format!(
                    "Deleted the clip, but its still \"{}\" stayed — it may still be in use.",
                    display_name(&still)
                ));
            }
        }
        target
    }

    /// Leave playback and fall back to the grid. A no-op when nothing is playing.
    fn stop_playback(&mut self) {
        self.player = None;
    }

    /// Leave playback and wait for the decoder to let go of the clip, so its file
    /// is closed by the time this returns. A no-op when nothing is playing.
    ///
    /// Dropping the `Player` only closes the channels; the decoder notices that
    /// and drops its open input some time later, on its own thread. Windows will
    /// not bin a file that is still open, so a delete racing that shutdown fails
    /// with a sharing violation — which is why the delete path waits and the
    /// other exits from playback (which do not touch the file) do not.
    ///
    /// The channels must close before the join: a decoder parked on a full frame
    /// channel only wakes because the send fails once the UI drops the receiver.
    fn stop_playback_and_wait(&mut self) {
        let Some(mut p) = self.player.take() else { return };
        let decoder = p.decoder.take();
        drop(p);
        if let Some(decoder) = decoder {
            let _ = decoder.join();
        }
    }

    /// Stop the loaded clip's grid worker and wait for it to let go of the file,
    /// dropping the partial grid with it. Returns whether it was still reading, so
    /// a caller that only wanted the file free can put the grid back.
    ///
    /// A finished grid is left alone, and that is not a shortcut: the worker sends
    /// `Done` only after `extract_grid_streaming` has returned and its input is
    /// dropped, so `done` already means the file is closed. It is also the common
    /// case — a clip is usually studied before it is judged.
    ///
    /// Like the decoder, the worker holds the clip open until it returns, and it
    /// cannot merely be flagged and joined: one parked on a full queue waits on a
    /// send that only fails once the receiver is gone, so the join would hang on
    /// exactly the worker that ignores the flag. Dropping `Loaded` does both —
    /// `Drop` raises the flag and the receiver goes with it.
    fn stop_extraction_and_wait(&mut self) -> bool {
        let Some(l) = &mut self.loaded else { return false };
        if l.done {
            return false;
        }
        let worker = l.worker.take();
        self.loaded = None;
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        true
    }

    /// Draw the playing clip filling the window and handle its keys. Enter or a
    /// decode error returns to the grid; Escape closes the app. A scrubber along
    /// the bottom shows the position and seeks on drag or click. Left/Right play
    /// the previous/next sibling clip from its start, staying in playback; Space
    /// pauses; A/D nudge the position back/forward by half a second
    /// (auto-repeating on hold) and pause on that frame. `+`/`-`/wheel zoom the
    /// frame and dragging it pans (see [`View`]).
    fn playback_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Left/Right switch to the previous/next sibling clip and keep playing:
        // open() loads the new clip (and kicks off its background grid), then we
        // start playback from the start instead of dropping back to the grid.
        let (prev, next) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
            )
        });
        let neighbor = if prev {
            self.neighbor(-1)
        } else if next {
            self.neighbor(1)
        } else {
            None
        };
        if let Some(path) = neighbor {
            self.open(ctx, path);
            self.play(ctx, 0.0);
            return;
        }

        // DEL sends the current clip to the recycle bin and moves to the next,
        // staying in playback (or dropping to the empty grid if it was the last).
        if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            if let Some(next) = self.delete_current(ctx) {
                self.open(ctx, next);
                self.play(ctx, 0.0);
            }
            return;
        }

        // Enter returns to the grid. Consume it so egui doesn't also use it to
        // activate a focused widget.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
            self.stop_playback();
            return;
        }

        // Escape closes the app, matching the grid view.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.toggle_pause();
        }
        if !self.advance_player(ctx) {
            self.stop_playback();
            return;
        }

        // "I" saves the currently shown frame as a still next to the clip.
        if ctx.input(|i| i.key_pressed(egui::Key::I)) {
            if let Some(t) = self.player.as_ref().map(|p| p.position_s) {
                self.save_still(ctx, t);
            }
        }

        // No self-driven repaint loop: the decoder paces itself and requests a
        // repaint per frame, and mouse motion drives repaints while dragging.
        let now = ctx.input(|i| i.time);
        let duration_s = self.loaded.as_ref().map(|l| l.duration_s).unwrap_or(0.0);

        // A/D nudge the live position back/forward by half a second and pause
        // on the landed frame. Repeat is driven off a timer (egui key events
        // don't reliably auto-repeat here), and we request repaints while a key
        // is held so the timer keeps ticking even though playback is paused.
        let dir = ctx.input(|i| {
            i.key_down(egui::Key::D) as i32 - i.key_down(egui::Key::A) as i32
        });
        if dir == 0 {
            self.seek_dir = 0;
        } else {
            if let Some(p) = &mut self.player {
                let start = dir != self.seek_dir;
                // Only fire the next step once the previous one has landed (the
                // shown frame reached the target). Each Scrub is a keyframe seek
                // plus decode, slower than the repeat interval, so firing blindly
                // backs up the command channel and stalls; gating on arrival
                // paces us to the decoder and keeps at most one seek in flight.
                let landed = (p.position_s - self.seek_target).abs() < SEEK_STEP_S * 0.5;
                if start || (now >= self.seek_next_fire && landed) {
                    let base = if start { p.position_s } else { self.seek_target };
                    let target = (base + dir as f64 * SEEK_STEP_S).clamp(0.0, duration_s);
                    self.seek_target = target;
                    p.paused = true;
                    let _ = p.cmds.send(media::PlayCommand::Scrub(target));
                    self.seek_dir = dir;
                    self.seek_next_fire =
                        now + if start { SEEK_REPEAT_DELAY_S } else { SEEK_REPEAT_INTERVAL_S };
                }
            }
            ctx.request_repaint();
        }

        // Full-window black backdrop: the frame is letterboxed above a scrubber
        // strip. Dragging the video area pans it and the wheel zooms it; the
        // scrubber handles its own clicks and drags.
        let seek = egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ui, |ui| {
                let full = ui.max_rect();
                let bar_h = 34.0;
                let split = (full.max.y - bar_h).max(full.min.y);
                let video_rect =
                    egui::Rect::from_min_max(full.min, egui::pos2(full.max.x, split));
                let bar_rect =
                    egui::Rect::from_min_max(egui::pos2(full.min.x, split), full.max);

                let resp = ui.interact(video_rect, ui.id().with("playback"), egui::Sense::drag());

                // "0" drops back to the whole frame — the way out of a zoom that
                // "-" can also walk back but only a step at a time.
                let (zoom_in, zoom_out, reset) = ctx.input(|i| {
                    (
                        i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                        i.key_pressed(egui::Key::Minus),
                        i.key_pressed(egui::Key::Num0),
                    )
                });
                if reset {
                    self.view = View::default();
                }
                if zoom_in {
                    self.view.zoom_by(ZOOM_KEY_STEP, video_rect.center(), video_rect);
                }
                if zoom_out {
                    self.view
                        .zoom_by(1.0 / ZOOM_KEY_STEP, video_rect.center(), video_rect);
                }

                // The wheel zooms about the cursor. Exponential so each notch is
                // the same proportional step wherever the zoom already is, and
                // read from the smoothed delta so a spun wheel ramps rather than
                // jumping in notch-sized leaps. egui repaints while that smoothing
                // has more to give, so the tail arrives even with playback paused.
                let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
                if resp.hovered() && scroll != 0.0 {
                    if let Some(at) = ctx.input(|i| i.pointer.latest_pos()) {
                        self.view
                            .zoom_by((scroll * ZOOM_WHEEL_SPEED).exp(), at, video_rect);
                    }
                }

                // Dragging moves the frame with the cursor. `place` clamps the
                // offset, so at fit scale this is inert rather than special-cased.
                if resp.dragged_by(egui::PointerButton::Primary) {
                    self.view.offset += resp.drag_delta();
                }

                // Taken by value so `place` can borrow the view mutably below.
                let shown = self
                    .player
                    .as_ref()
                    .and_then(|p| p.tex.as_ref().map(|t| (t.id(), p.frame_size)));
                if let Some((tex, frame_size)) = shown {
                    let target = self.view.place(video_rect, frame_size);
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    // Clipped: a zoomed frame is larger than the area it plays in
                    // and would otherwise paint straight over the scrubber.
                    ui.painter()
                        .with_clip_rect(video_rect)
                        .image(tex, target, uv, egui::Color32::WHITE);
                }

                let mut cmd = None;
                if let Some(p) = &mut self.player {
                    cmd = seek_bar_ui(ui, bar_rect, p, &mut self.scrub_in_flight, duration_s);
                }
                cmd
            })
            .inner;

        // Seeks go to the live decoder as commands — the player stays put.
        if let Some(cmd) = seek {
            self.send_cmd(cmd);
        }
    }
}

/// Draw the playback scrubber into `rect` and handle interaction, VLC-style: a
/// click jumps playback there, and dragging anywhere on the bar seeks live (the
/// decoder shows the exact frame at the cursor and holds); releasing resumes
/// playback from that spot, or holds there if playback was paused. The handle
/// just reflects the position. Returns the `PlayCommand` to send this frame — a
/// held `Scrub` while dragging, or a `Play`/`Scrub` (per pause state) on
/// click/release — else `None`.
fn seek_bar_ui(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    player: &mut Player,
    in_flight: &mut bool,
    duration_s: f64,
) -> Option<media::PlayCommand> {
    let resp = ui.interact(rect, ui.id().with("seek"), egui::Sense::click_and_drag());

    let margin = 12.0;
    let left = rect.left() + margin;
    let right = rect.right() - margin;
    let track_w = (right - left).max(1.0);
    let y = rect.center().y;
    let time_at_x = |x: f32| ((x - left) / track_w).clamp(0.0, 1.0) as f64 * duration_s;

    // A finished seek resumes playback, unless it was paused before the seek —
    // then hold on the landed frame (Scrub) so a mouse seek doesn't silently
    // start playing and desync the Space pause toggle.
    let paused = player.paused;
    let seek_cmd = |t: f64| {
        if paused {
            media::PlayCommand::Scrub(t)
        } else {
            media::PlayCommand::Play(t)
        }
    };

    let mut cmd = None;
    if resp.dragged() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let target = time_at_x(pos.x);
            // Handle follows the cursor exactly. Fire a held live seek only once
            // the cursor has moved off the shown frame, and only when the previous
            // seek has landed: a drag outruns the decoder, and seeks fired on a
            // fixed clock instead pile up in the command queue, each discarded by
            // the next, so the picture stops tracking the cursor at all. Waiting
            // for the landing paces us to the decoder — as fast as it can go, and
            // never faster. The same gate paces the A/D keys in `playback_ui`.
            player.scrub = Some(target);
            let moved = (target - player.position_s).abs() >= SCRUB_MIN_STEP_S;
            if moved && !*in_flight {
                *in_flight = true;
                cmd = Some(media::PlayCommand::Scrub(target));
            }
        }
    } else if let Some(target) = player.scrub.take() {
        // Drag released: seek to the exact spot; resume or hold per the pause state.
        cmd = Some(seek_cmd(target));
    }
    if resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            cmd = Some(seek_cmd(time_at_x(pos.x)));
        }
    }

    let pos_s = player.scrub.unwrap_or(player.position_s);
    let frac = if duration_s > 0.0 {
        (pos_s / duration_s).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let handle_x = left + frac * track_w;

    let painter = ui.painter();
    let track = egui::Rect::from_min_max(egui::pos2(left, y - 2.0), egui::pos2(right, y + 2.0));
    let filled =
        egui::Rect::from_min_max(egui::pos2(left, y - 2.0), egui::pos2(handle_x, y + 2.0));
    painter.rect_filled(track, 2.0, egui::Color32::from_gray(70));
    painter.rect_filled(filled, 2.0, egui::Color32::from_gray(200));
    painter.circle_filled(egui::pos2(handle_x, y), 7.0, egui::Color32::WHITE);

    cmd
}

/// Start reading `path`'s grid on a background thread, returning the clip state
/// its thumbnails stream into. The thumbnails wait in the channel until someone
/// polls them, so this is equally the way a clip is opened and the way one is
/// read ahead — the difference is only which slot holds the result.
///
/// The channel is bounded ([`GRID_QUEUE_THUMBS`]): a worker whose thumbnails
/// nobody is taking parks instead of reading a whole clip into memory. That only
/// ever happens to a read-ahead, since the foreground grid is drained every frame.
fn spawn_extraction(ctx: &egui::Context, path: PathBuf) -> Loaded {
    let (tx, rx) = mpsc::sync_channel(GRID_QUEUE_THUMBS);
    let (tx_meta, tx_thumb) = (tx.clone(), tx.clone());
    let (ctx_meta, ctx_thumb, ctx_end) = (ctx.clone(), ctx.clone(), ctx.clone());
    let worker_path = path.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    let worker = thread::spawn(move || {
        let result = media::extract_grid_streaming(
            &worker_path,
            THUMB_SPACING_S,
            THUMB_LONG,
            &worker_cancel,
            move |m| {
                let _ = tx_meta.send(Msg::Meta(m));
                ctx_meta.request_repaint();
            },
            move |i, t| {
                let _ = tx_thumb.send(Msg::Thumb(i, t));
                ctx_thumb.request_repaint();
            },
        );
        let _ = match result {
            Ok(s) => {
                // Every whole pass — a clip opened or read ahead — adds a line to
                // the dataset. A cancelled one reports `None` and adds nothing.
                if let Some(s) = s {
                    stats::record(&s);
                }
                tx.send(Msg::Done)
            }
            Err(e) => tx.send(Msg::Err(format!("{e:#}"))),
        };
        ctx_end.request_repaint();
    });

    Loaded {
        path,
        duration_s: 0.0,
        aspect: 9.0 / 16.0,
        cells: Vec::new(),
        ready: 0,
        done: false,
        rx,
        cursor: 0,
        cancel,
        worker: Some(worker),
    }
}

/// New cursor index after moving `(dx, dy)` cells within a `cols`-wide grid of
/// `n` cells. Clamps at the edges (no wrap) and to the last, possibly partial,
/// row so the cursor never lands past the streamed thumbnails.
fn move_cursor(cursor: usize, n: usize, cols: usize, dx: i32, dy: i32) -> usize {
    if n == 0 {
        return 0;
    }
    let rows = n.div_ceil(cols);
    let row = (cursor / cols) as i32;
    let col = (cursor % cols) as i32;
    let new_col = (col + dx).clamp(0, cols as i32 - 1);
    let new_row = (row + dy).clamp(0, rows as i32 - 1);
    (new_row as usize * cols + new_col as usize).min(n - 1)
}

/// Largest rect with `size`'s aspect ratio that fits centered inside `container`.
fn fit_centered(container: egui::Rect, size: egui::Vec2) -> egui::Rect {
    if size.x <= 0.0 || size.y <= 0.0 {
        return container;
    }
    let scale = (container.width() / size.x).min(container.height() / size.y);
    egui::Rect::from_center_size(container.center(), size * scale)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Open from the command line on the first frame.
        if let Some(path) = self.pending_open.take() {
            self.open(&ctx, path);
        }
        // Open a dropped file (last video dropped wins). Anything else is
        // ignored: extraction finds no video stream, so the app would sit on a
        // named-but-empty grid where every key does nothing. A drop is ignored
        // outright while the rename dialog is up — it arrives from the OS rather
        // than through the modal's backdrop, and swapping the clip out from
        // under an open dialog would have it rename the newcomer to the name of
        // the clip that was there when it opened.
        if let Some(path) = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .filter(|p| is_video(p))
                .last()
        }) {
            if self.rename.is_none() {
                self.open(&ctx, path);
            }
        }

        self.poll(&ctx);
        self.poll_save(&ctx);
        self.sync_title(&ctx);
        self.look_ahead(&ctx);
        // Before the views, so the early returns below cannot skip it; it draws
        // over them from its own layer regardless. The dialog goes first of the
        // three: it takes the keyboard for the rest of the frame, and the help
        // plate's H is one of the keys a name being typed must not reach.
        self.rename_ui(&ctx);
        self.flash_ui(&ctx);
        self.help_ui(&ctx);

        // F12 toggles borderless fullscreen. Handled here, above the early
        // return below, so the one handler serves both views. What it toggles
        // away from is read back from the viewport rather than kept in a field
        // of ours: the window can leave fullscreen without us (the window
        // manager, or the OS), and a field would have the next press turn
        // fullscreen back on when the user meant to turn it off.
        if ctx.input(|i| i.key_pressed(egui::Key::F12)) {
            let full = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
        }

        // "N" opens the rename dialog. Also above the early return, so the one
        // handler serves the grid and playback alike; a dialog already up has
        // taken the keyboard by here, so this cannot reopen on the "n" of a name.
        if self.loaded.is_some() && ctx.input(|i| i.key_pressed(egui::Key::N)) {
            self.begin_rename();
        }

        // While a clip is playing it fills the window; the grid and its keys are
        // hidden until playback ends or Escape returns here.
        if self.player.is_some() {
            self.playback_ui(ui, &ctx);
            return;
        }

        // ESC quits.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // DEL sends the current clip to the recycle bin and opens the next one.
        if self.loaded.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            if let Some(next) = self.delete_current(&ctx) {
                self.open(&ctx, next);
            }
            return;
        }

        // -/+ change how many columns the grid shows, trading thumbnail size for
        // how many frames fit on screen. Accept "=" as "+" too, since on most
        // layouts "+" is Shift+"=". The cursor is an absolute cell index, so it
        // stays valid across a column change with no fix-up needed.
        if self.loaded.is_some() {
            let (zoom_in, zoom_out) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                    i.key_pressed(egui::Key::Minus),
                )
            });
            if zoom_in {
                self.grid_cols = self.grid_cols.saturating_sub(1).max(GRID_COLS_MIN);
            }
            if zoom_out {
                self.grid_cols = (self.grid_cols + 1).min(GRID_COLS_MAX);
            }
        }

        // A clip to open this frame, from a button, an arrow key, or the dialog.
        let mut nav_to: Option<PathBuf> = None;
        if self.loaded.is_some() {
            let (prev, next) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::ArrowRight),
                )
            });
            if prev {
                nav_to = self.neighbor(-1);
            } else if next {
                nav_to = self.neighbor(1);
            }
        }

        if let Some(l) = &self.loaded {
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    let name = l
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.label(format!("{name}  ·  {:.1}s", l.duration_s));
                    if !l.done {
                        ui.separator();
                        ui.add(egui::Spinner::new());
                        ui.label(format!("{} frames", l.ready));
                    }
                });
            });
        }

        if let Some(path) = nav_to {
            self.open(&ctx, path);
        }

        // A frame clicked (or picked with Enter) this pass: its clip time, to
        // start playback there.
        let mut play_from: Option<f64> = None;
        // Set when "I" is pressed on a ready cell: its clip time, to save a still.
        let mut save_still_at: Option<f64> = None;
        // Set when AWSD moved the cursor this pass, so we only auto-scroll then
        // and don't fight the user's mouse-wheel scrolling.
        let mut cursor_moved = false;

        // AWSD moves the frame cursor; Enter plays the frame under it; "I" saves it.
        let grid_cols = self.grid_cols;
        if let Some(l) = &mut self.loaded {
            let (left, right, up, down, enter, save) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::A),
                    i.key_pressed(egui::Key::D),
                    i.key_pressed(egui::Key::W),
                    i.key_pressed(egui::Key::S),
                    i.key_pressed(egui::Key::Enter),
                    i.key_pressed(egui::Key::I),
                )
            });
            let n = l.cells.len();
            if n > 0 {
                let dx = right as i32 - left as i32;
                let dy = down as i32 - up as i32;
                if dx != 0 || dy != 0 {
                    l.cursor = move_cursor(l.cursor, n, grid_cols, dx, dy);
                    cursor_moved = true;
                }
                if let Some(Cell::Ready { time_s, .. }) = l.cells.get(l.cursor) {
                    if enter {
                        play_from = Some(*time_s);
                    }
                    if save {
                        save_still_at = Some(*time_s);
                    }
                }
            }
        }

        let grid_frame = egui::Frame::NONE.fill(ui.visuals().panel_fill);
        egui::CentralPanel::default().frame(grid_frame).show(ui, |ui| {
            if let Some(err) = &self.error {
                ui.colored_label(egui::Color32::RED, err);
                return;
            }
            let Some(l) = &self.loaded else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open a video or drop one here to see its frame grid.");
                });
                return;
            };

            let cols = self.grid_cols;
            let spacing = 2.0f32;
            let avail = ui.available_width();
            let cell_w = ((avail - spacing * (cols as f32 + 1.0)) / cols as f32).max(80.0);
            let cell_h = cell_w * l.aspect;
            let size = egui::vec2(cell_w, cell_h);

            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("frame_grid")
                    .spacing([spacing, spacing])
                    .show(ui, |ui| {
                        for (i, cell) in l.cells.iter().enumerate() {
                            let cell_rect = match cell {
                                Cell::Ready { tex, time_s } => {
                                    let sized = egui::load::SizedTexture::new(tex.id(), size);
                                    let resp = ui.add(
                                        egui::Image::from_texture(sized)
                                            .sense(egui::Sense::click()),
                                    );
                                    if resp.clicked() {
                                        play_from = Some(*time_s);
                                    }
                                    resp.rect
                                }
                                Cell::Pending => {
                                    let (rect, _) =
                                        ui.allocate_exact_size(size, egui::Sense::hover());
                                    ui.painter().rect_filled(rect, 2.0, egui::Color32::from_gray(35));
                                    rect
                                }
                            };
                            if i == l.cursor {
                                ui.painter().rect_stroke(
                                    cell_rect,
                                    0.0,
                                    egui::Stroke::new(3.0, egui::Color32::from_rgb(77, 166, 255)),
                                    egui::StrokeKind::Inside,
                                );
                                if cursor_moved {
                                    // Keep the selected cell visible as it moves
                                    // through the scrolled grid.
                                    ui.scroll_to_rect(cell_rect, None);
                                }
                            }
                            if (i + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    });
            });
        });

        // Start playback / save a still after the panel closure releases `self`.
        if let Some(t) = play_from {
            self.play(&ctx, t);
        }
        if let Some(t) = save_still_at {
            self.save_still(&ctx, t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"x").unwrap();
        p
    }

    /// A stand-in grid. `Cell::Pending` needs no texture, so the cache's
    /// bookkeeping is testable without a live egui context.
    fn fake_loaded(path: &str, thumbs: usize, done: bool, cancel: Arc<AtomicBool>) -> Loaded {
        let (_tx, rx) = mpsc::channel();
        Loaded {
            path: PathBuf::from(path),
            duration_s: 0.0,
            aspect: 1.0,
            cells: (0..thumbs).map(|_| Cell::Pending).collect(),
            ready: thumbs,
            done,
            rx,
            cursor: 0,
            cancel,
            worker: None,
        }
    }

    /// Dropping a clip stops its extraction worker. `media` honours the flag (it
    /// has its own test for that); this pins the other half of the chain — that
    /// the flag is actually raised when the UI lets a clip go.
    #[test]
    fn dropping_a_loaded_clip_cancels_its_extraction() {
        let cancel = Arc::new(AtomicBool::new(false));
        let loaded = fake_loaded("clip.mp4", 0, false, Arc::clone(&cancel));

        assert!(!cancel.load(Ordering::Relaxed), "not cancelled while held");
        drop(loaded);
        assert!(
            cancel.load(Ordering::Relaxed),
            "dropping a clip left its worker reading the file"
        );
    }

    /// A finished grid survives being stepped away from, so coming back to it
    /// costs no re-read.
    #[test]
    fn a_finished_grid_comes_back_from_the_cache() {
        let mut app = App::new(None);
        app.loaded = Some(fake_loaded("a.mp4", 5, true, Arc::new(AtomicBool::new(false))));

        app.park_current();
        assert!(app.loaded.is_none(), "parking should empty the current slot");

        let restored = app.take_recent(Path::new("a.mp4"));
        assert!(restored.is_some(), "a finished grid should come back");
        assert!(
            app.take_recent(Path::new("a.mp4")).is_none(),
            "taking a grid should remove it from the cache"
        );
    }

    /// An unfinished grid is dropped rather than cached — and dropping it stops
    /// its worker, so no reading continues for a clip nobody is looking at.
    #[test]
    fn an_unfinished_grid_is_cancelled_not_cached() {
        let mut app = App::new(None);
        let cancel = Arc::new(AtomicBool::new(false));
        app.loaded = Some(fake_loaded("half.mp4", 3, false, Arc::clone(&cancel)));

        app.park_current();

        assert!(app.recent.is_empty(), "an unfinished grid must not be cached");
        assert!(
            cancel.load(Ordering::Relaxed),
            "an unfinished grid left its worker reading"
        );
    }

    /// The cache is bounded by thumbnails, not by clip count: enough grids evict
    /// the oldest. A clip count would let long clips blow past any memory budget.
    #[test]
    fn the_cache_evicts_oldest_to_stay_within_the_thumbnail_budget() {
        let mut app = App::new(None);
        // 25 thumbs each, the size of a target 23 s clip; 20 clips overruns 200.
        for i in 0..20 {
            let cancel = Arc::new(AtomicBool::new(false));
            app.loaded = Some(fake_loaded(&format!("c{i}.mp4"), 25, true, cancel));
            app.park_current();
        }

        let total: usize = app.recent.iter().map(|l| l.cells.len()).sum();
        assert!(
            total <= RECENT_MAX_THUMBS,
            "cache holds {total} thumbs, over the {RECENT_MAX_THUMBS} budget"
        );
        assert!(
            app.take_recent(Path::new("c19.mp4")).is_some(),
            "the newest grid should still be cached"
        );
        assert!(
            app.take_recent(Path::new("c0.mp4")).is_none(),
            "the oldest grid should have been evicted"
        );
    }

    /// `open` serves a cached grid instead of re-extracting it. Re-opening the
    /// very clip being left is the sharp case: it only works because `open` parks
    /// the outgoing grid *before* it looks in the cache.
    #[test]
    fn open_serves_a_cached_grid_instead_of_re_extracting() {
        let ctx = egui::Context::default();
        let mut app = App::new(None);
        let cancel = Arc::new(AtomicBool::new(false));
        app.loaded = Some(fake_loaded("a.mp4", 5, true, Arc::clone(&cancel)));

        app.open(&ctx, PathBuf::from("a.mp4"));

        let l = app.loaded.as_ref().expect("a grid should be loaded");
        assert_eq!(l.path, PathBuf::from("a.mp4"));
        // A fresh extraction would have started empty and unfinished; only the
        // cached grid comes back already done and with its thumbnails.
        assert!(l.done, "open re-extracted a clip it already had cached");
        assert_eq!(l.cells.len(), 5, "cached thumbnails should have come back");
        assert!(
            !cancel.load(Ordering::Relaxed),
            "a cached grid was dropped instead of reused"
        );
    }

    /// A stand-in player. Nothing reads its channels; it only marks the app as
    /// playing.
    fn fake_player() -> Player {
        let (cmds, _cmd_rx) = mpsc::channel();
        let (_frame_tx, rx) = mpsc::channel();
        Player {
            cmds,
            rx,
            decoder: None,
            tex: None,
            frame_size: egui::Vec2::ZERO,
            position_s: 0.0,
            paused: false,
            scrub: None,
        }
    }

    /// A player whose decoder parks on its command channel exactly as a paused
    /// `play_stream` does, standing in for one holding the clip's file open. The
    /// flag rises only once the thread is really gone — the moment a real decoder
    /// would drop its input and let go of the file.
    fn parked_player() -> (Player, Arc<AtomicBool>) {
        let (cmds, cmd_rx) = mpsc::channel::<media::PlayCommand>();
        let (_frame_tx, rx) = mpsc::channel();
        let exited = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&exited);
        let decoder = thread::spawn(move || {
            while cmd_rx.recv().is_ok() {}
            // A real decoder takes a moment to wind down after the channel closes;
            // without it a missing join could still observe the flag set and pass.
            thread::sleep(std::time::Duration::from_millis(50));
            flag.store(true, Ordering::Relaxed);
        });
        (
            Player {
                cmds,
                rx,
                decoder: Some(decoder),
                tex: None,
                frame_size: egui::Vec2::ZERO,
                position_s: 0.0,
                paused: false,
                scrub: None,
            },
            exited,
        )
    }

    /// A grid whose worker is parked on a full queue, as a read-ahead's always is
    /// and the foreground's can be for a frame. This is the case that dictates the
    /// order inside `stop_extraction_and_wait`: the flag alone never reaches a
    /// worker blocked in a send. The flag rises only once the thread is gone — the
    /// moment a real worker would drop its input and let go of the file.
    fn parked_extraction() -> (Loaded, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::sync_channel::<Msg>(1);
        let exited = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&exited);
        let worker = thread::spawn(move || {
            // Fills the queue, then blocks in a send exactly as `on_thumb` does,
            // and is freed only by the receiver going away.
            while tx.send(Msg::Done).is_ok() {}
            thread::sleep(std::time::Duration::from_millis(50));
            flag.store(true, Ordering::Relaxed);
        });
        let mut l = fake_loaded("a.mp4", 0, false, Arc::new(AtomicBool::new(false)));
        l.rx = rx;
        l.worker = Some(worker);
        (l, exited)
    }

    /// Stopping a grid that is still reading waits for its worker to exit, and a
    /// worker parked on a full queue must not turn that wait into a hang.
    #[test]
    fn stopping_a_reading_grid_waits_for_a_parked_worker_to_exit() {
        let mut app = App::new(None);
        let (loaded, exited) = parked_extraction();
        app.loaded = Some(loaded);

        let was_reading = app.stop_extraction_and_wait();

        assert!(was_reading, "a grid mid-read should report that it was stopped");
        assert!(app.loaded.is_none(), "the partial grid should be gone");
        assert!(
            exited.load(Ordering::Relaxed),
            "returned while the worker still had the clip open"
        );
    }

    /// A finished grid is left alone: its worker already dropped the clip's file
    /// before reporting `Done`, so there is nothing to wait for and a grid worth
    /// keeping. This is what spares the common delete a needless re-read.
    #[test]
    fn stopping_a_finished_grid_keeps_it() {
        let mut app = App::new(None);
        app.loaded = Some(fake_loaded(
            "a.mp4",
            5,
            true,
            Arc::new(AtomicBool::new(false)),
        ));

        let was_reading = app.stop_extraction_and_wait();

        assert!(!was_reading, "a finished grid was not reading");
        assert!(app.loaded.is_some(), "a finished grid should have been kept");
    }

    /// Leaving playback for a delete waits for the decoder to actually exit.
    /// Windows refuses to bin a file that is still open, so returning while the
    /// decoder holds it is what made DEL fail during playback.
    #[test]
    fn stopping_playback_for_a_delete_waits_for_the_decoder_to_exit() {
        let mut app = App::new(None);
        let (player, exited) = parked_player();
        app.player = Some(player);

        app.stop_playback_and_wait();

        assert!(app.player.is_none(), "playback should be over");
        assert!(
            exited.load(Ordering::Relaxed),
            "returned while the decoder still had the clip open"
        );
    }

    /// A stand-in for a still being written. Like a real save it holds its clip
    /// for a while and only then reports, so a wait that did not actually wait
    /// is caught rather than passing on timing luck.
    fn parked_save() -> (Saving, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel();
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(50));
            // Ordered as the real worker is: the clip is closed when
            // `save_frame_jpeg` returns, which is before the outcome is sent.
            flag.store(true, Ordering::Relaxed);
            let _ = tx.send(Ok(()));
        });
        (
            Saving {
                out: PathBuf::from("clip.jpg"),
                time_s: 0.0,
                started: Instant::now(),
                rx,
            },
            closed,
        )
    }

    /// Waiting for a still hands back a closed clip. A save reads the clip it is
    /// taken from, and Windows refuses to bin a file that is still open, so a
    /// delete racing a save would fail exactly as it did racing the decoder.
    #[test]
    fn waiting_for_a_still_returns_only_once_the_writer_let_go() {
        let ctx = egui::Context::default();
        let mut app = App::new(None);
        let (saving, closed) = parked_save();
        app.saving = Some(saving);

        app.finish_save_and_wait(&ctx);

        assert!(app.saving.is_none(), "the save should have been settled");
        assert!(
            closed.load(Ordering::Relaxed),
            "returned while the writer still had the clip open"
        );
    }

    /// A second still asked for while one is being written waits its turn.
    /// Both write the same `<clip-stem>.jpg`, so starting the second would leave
    /// two threads racing on the one file.
    #[test]
    fn a_still_asked_for_while_one_is_writing_is_held() {
        let ctx = egui::Context::default();
        let mut app = App::new(None);
        app.loaded = Some(fake_loaded(
            "clip.mp4",
            1,
            true,
            Arc::new(AtomicBool::new(false)),
        ));
        let (saving, _closed) = parked_save();
        app.saving = Some(saving);

        app.save_still(&ctx, 3.0);

        assert!(
            matches!(&app.pending_save, Some((p, t)) if p == Path::new("clip.mp4") && *t == 3.0),
            "the request should have been held behind the running save"
        );
    }

    /// Waiting for a still drops a request held behind it. Settling the wait
    /// starts a held request, and the one caller that waits is a delete — which
    /// would then bin a clip it had just handed back to a fresh reader.
    #[test]
    fn waiting_for_a_still_drops_the_request_held_behind_it() {
        let ctx = egui::Context::default();
        let mut app = App::new(None);
        let (saving, _closed) = parked_save();
        app.saving = Some(saving);
        app.pending_save = Some((PathBuf::from("clip.mp4"), 3.0));

        app.finish_save_and_wait(&ctx);

        assert!(app.pending_save.is_none(), "the held request should be gone");
        assert!(app.saving.is_none(), "no writer should have been started");
    }

    /// An app looking at a finished grid of `a.mp4`, with `b.mp4` next to it —
    /// the state a look-ahead acts on. Returns the folder, which must outlive the
    /// app, and both paths.
    fn app_on_a_folder() -> (tempfile::TempDir, App, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "a.mp4");
        let b = touch(dir.path(), "b.mp4");
        let mut app = App::new(None);
        app.loaded = Some(fake_loaded(
            a.to_str().unwrap(),
            5,
            true,
            Arc::new(AtomicBool::new(false)),
        ));
        (dir, app, a, b)
    }

    /// With the grid finished and nothing playing, the next sibling starts being
    /// read — so stepping forward finds it already done.
    #[test]
    fn look_ahead_starts_reading_the_next_sibling() {
        let (_dir, mut app, _a, b) = app_on_a_folder();

        app.look_ahead(&egui::Context::default());

        let p = app.prefetch.as_ref().expect("next sibling should be reading");
        assert_eq!(p.path, b);
    }

    /// Nothing is read ahead while a clip plays: the archive's HDD has one head,
    /// and the seeks of the clip being watched must not queue behind a read
    /// nobody asked for.
    #[test]
    fn look_ahead_does_not_start_while_playing() {
        let (_dir, mut app, _a, _b) = app_on_a_folder();
        app.player = Some(fake_player());

        app.look_ahead(&egui::Context::default());

        assert!(app.prefetch.is_none(), "read ahead while a clip was playing");
    }

    /// Nothing is read ahead until the current grid is finished: the clip the
    /// user is waiting for gets the disk to itself first.
    #[test]
    fn look_ahead_waits_for_the_current_grid_to_finish() {
        let (_dir, mut app, a, _b) = app_on_a_folder();
        app.loaded = Some(fake_loaded(
            a.to_str().unwrap(),
            2,
            false, // still being read
            Arc::new(AtomicBool::new(false)),
        ));

        app.look_ahead(&egui::Context::default());

        assert!(
            app.prefetch.is_none(),
            "read ahead while the current grid was still being read"
        );
    }

    /// A clip already in the recent cache is not read a second time.
    #[test]
    fn look_ahead_skips_a_clip_already_cached() {
        let (_dir, mut app, _a, b) = app_on_a_folder();
        app.recent.push_front(fake_loaded(
            b.to_str().unwrap(),
            3,
            true,
            Arc::new(AtomicBool::new(false)),
        ));

        app.look_ahead(&egui::Context::default());

        assert!(app.prefetch.is_none(), "read ahead a clip already cached");
    }

    /// Opening the clip being read ahead takes that read over rather than
    /// restarting it — whatever it has already pulled off the disk is kept.
    #[test]
    fn open_takes_over_a_matching_prefetch() {
        let ctx = egui::Context::default();
        let mut app = App::new(None);
        let cancel = Arc::new(AtomicBool::new(false));
        app.prefetch = Some(fake_loaded("b.mp4", 0, false, Arc::clone(&cancel)));

        app.open(&ctx, PathBuf::from("b.mp4"));

        assert!(app.prefetch.is_none(), "the prefetch slot should be empty");
        assert_eq!(app.loaded.as_ref().expect("a grid").path, PathBuf::from("b.mp4"));
        // A fresh extraction would have dropped the prefetch, cancelling it.
        assert!(
            !cancel.load(Ordering::Relaxed),
            "the prefetch was restarted instead of taken over"
        );
    }

    /// Opening some other clip drops the read-ahead, which cancels it and hands
    /// the disk straight back to the clip the user asked for.
    #[test]
    fn open_cancels_a_prefetch_of_another_clip() {
        let ctx = egui::Context::default();
        let mut app = App::new(None);
        let cancel = Arc::new(AtomicBool::new(false));
        app.prefetch = Some(fake_loaded("b.mp4", 0, false, Arc::clone(&cancel)));
        // Park a.mp4 so opening it hits the cache and spawns no real extraction.
        app.loaded = Some(fake_loaded("a.mp4", 5, true, Arc::new(AtomicBool::new(false))));
        app.park_current();

        app.open(&ctx, PathBuf::from("a.mp4"));

        assert!(app.prefetch.is_none(), "the prefetch slot should be empty");
        assert!(
            cancel.load(Ordering::Relaxed),
            "a read-ahead for a clip we left was not cancelled"
        );
    }

    /// A grid bigger than the whole budget is never cached: it would evict
    /// everything else and still overrun.
    #[test]
    fn a_grid_larger_than_the_budget_is_not_cached() {
        let mut app = App::new(None);
        let thumbs = RECENT_MAX_THUMBS + 1;
        app.loaded = Some(fake_loaded(
            "long.mp4",
            thumbs,
            true,
            Arc::new(AtomicBool::new(false)),
        ));

        app.park_current();

        assert!(
            app.recent.is_empty(),
            "a grid over the budget should not be cached"
        );
    }

    /// The plate is solid while H is held, fades from the moment it is let go,
    /// and is gone once the fade runs out.
    #[test]
    fn the_help_plate_holds_then_fades_out() {
        let (held, alpha) = Help::default().advance(true, 0.0);
        assert!(matches!(held, Help::Held), "holding H should show the plate");
        assert_eq!(alpha, Some(1.0), "a held plate is solid");

        let (fading, alpha) = held.advance(false, 10.0);
        assert!(matches!(fading, Help::Fading(_)), "letting go should start the fade");
        assert_eq!(alpha, Some(1.0), "the fade starts from solid");

        let (fading, alpha) = fading.advance(false, 10.0 + HELP_FADE_S / 2.0);
        let alpha = alpha.expect("half way through, the plate is still on screen");
        assert!((alpha - 0.5).abs() < 0.01, "half way through the fade alpha was {alpha}");

        // Past the end of the fade rather than exactly on it: the boundary lands
        // on a float that rounds to a hair above zero, and no real frame ever
        // arrives on it anyway.
        let (gone, alpha) = fading.advance(false, 10.0 + HELP_FADE_S * 2.0);
        assert!(matches!(gone, Help::Hidden), "the fade should have ended");
        assert_eq!(alpha, None, "a faded-out plate is not drawn at all");
    }

    /// Taking H back mid-fade brings the plate straight back to solid, rather
    /// than carrying on fading out from under the key that is asking for it.
    #[test]
    fn holding_h_again_mid_fade_brings_the_plate_back() {
        let (state, _) = Help::Fading(10.0).advance(false, 10.0 + HELP_FADE_S / 2.0);

        let (state, alpha) = state.advance(true, 10.0 + HELP_FADE_S / 2.0 + 0.01);

        assert!(matches!(state, Help::Held), "H should have taken the plate back");
        assert_eq!(alpha, Some(1.0), "the plate should be solid again, not still fading");
    }

    /// A 400x200 view showing a 2:1 frame, so the fit rect is the whole view and
    /// the arithmetic below stays readable.
    fn view_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 200.0))
    }
    const FRAME: egui::Vec2 = egui::vec2(2000.0, 1000.0);

    /// Unzoomed playback is exactly the letterboxed fit it was before zoom
    /// existed, and no drag can shift it: there is nothing off screen to pan to.
    #[test]
    fn an_unzoomed_frame_is_centered_and_cannot_be_panned() {
        // As if dragged.
        let mut view = View { offset: egui::vec2(50.0, 50.0), ..Default::default() };

        let target = view.place(view_rect(), FRAME);

        assert_eq!(target, fit_centered(view_rect(), FRAME), "fit, ignoring the drag");
        assert_eq!(view.offset, egui::Vec2::ZERO, "the drag should have been clamped away");
    }

    /// Panning stops at the frame's edge instead of letting it drift off into the
    /// black. At 2x the 400x200 view holds an 800x400 frame, so 200 points of it
    /// hang off horizontally and 100 vertically — exactly the slack to pan into.
    #[test]
    fn panning_stops_at_the_edge_of_the_zoomed_frame() {
        let mut view = View { zoom: 2.0, offset: egui::vec2(5000.0, -5000.0) };

        let target = view.place(view_rect(), FRAME);

        assert_eq!(view.offset, egui::vec2(200.0, -100.0), "clamped to the frame's edges");
        assert_eq!(target.min.x, 0.0, "the frame's left edge should sit on the view's");
        assert_eq!(target.max.y, 200.0, "and its bottom edge on the view's bottom");
    }

    /// The wheel zooms about the cursor: whatever pixel sits under it stays there,
    /// which is what makes zooming land on the thing being looked at.
    #[test]
    fn zooming_holds_the_point_under_the_cursor() {
        let rect = view_rect();
        let at = egui::pos2(100.0, 50.0); // off-center, so an unanchored zoom would move it
        let mut view = View::default();
        let before = view.place(rect, FRAME);
        // Where the cursor sits within the frame, as a fraction of it.
        let frac = (at - before.min) / before.size();

        view.zoom_by(2.0, at, rect);
        let after = view.place(rect, FRAME);

        let moved_to = after.min + (frac * after.size());
        assert!(
            (moved_to - at).length() < 0.01,
            "the anchored point moved from {at:?} to {moved_to:?}",
        );
    }

    /// Zooming out never goes past the whole frame, and coming back to fit leaves
    /// no leftover pan — a zoom out from a panned-away corner re-centers.
    #[test]
    fn zooming_out_bottoms_out_at_fit_with_no_leftover_pan() {
        let rect = view_rect();
        let mut view = View { zoom: 2.0, offset: egui::vec2(100.0, 50.0) };

        // Far more zoom-out than it takes to get back to 1.0.
        for _ in 0..20 {
            view.zoom_by(1.0 / ZOOM_KEY_STEP, egui::pos2(0.0, 0.0), rect);
        }
        let target = view.place(rect, FRAME);

        assert_eq!(view.zoom, 1.0, "zoom should have stopped at the whole frame");
        assert_eq!(target, fit_centered(rect, FRAME), "and left it centered");
    }

    /// Zoom in stops at [`ZOOM_MAX`] rather than running away into a texture that
    /// has no more detail to give.
    #[test]
    fn zooming_in_stops_at_the_cap() {
        let rect = view_rect();
        let mut view = View::default();

        for _ in 0..100 {
            view.zoom_by(ZOOM_KEY_STEP, rect.center(), rect);
        }

        assert_eq!(view.zoom, ZOOM_MAX);
    }

    #[test]
    fn siblings_are_sorted_videos_only() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "b.mp4");
        touch(dir.path(), "a.mov");
        touch(dir.path(), "notes.txt"); // non-video, must be skipped
        fs::create_dir(dir.path().join("sub.mp4")).unwrap(); // dir, not a file

        let names: Vec<_> = sibling_videos(&dir.path().join("a.mov"))
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.mov", "b.mp4"]);
    }

    #[test]
    fn cursor_moves_and_clamps_at_edges() {
        // 4-wide grid, 10 cells (last row is 8,9 — partial).
        let cols = 4;
        let n = 10;
        // From top-left: left/up clamp in place, right/down step.
        assert_eq!(move_cursor(0, n, cols, -1, 0), 0); // left edge
        assert_eq!(move_cursor(0, n, cols, 0, -1), 0); // top edge
        assert_eq!(move_cursor(0, n, cols, 1, 0), 1); // right
        assert_eq!(move_cursor(0, n, cols, 0, 1), 4); // down a row
        // Right edge of a full row clamps horizontally.
        assert_eq!(move_cursor(3, n, cols, 1, 0), 3);
        // Down into the partial last row snaps back to the last cell.
        assert_eq!(move_cursor(6, n, cols, 0, 1), 9); // col 2 row 2 -> would be 10, clamped to 9
        assert_eq!(move_cursor(7, n, cols, 0, 1), 9); // col 3 row 2 -> would be 11, clamped to 9
        // An empty grid stays put.
        assert_eq!(move_cursor(0, 0, cols, 1, 1), 0);
    }

    /// The gate on both ways a clip gets in from outside — a drop and the
    /// command line. A still saved by "I" lands right next to the clip it came
    /// from, which is exactly how a jpg ends up dropped on the window.
    #[test]
    fn is_video_spots_a_clip_whatever_the_case_and_nothing_else() {
        assert!(is_video(Path::new("a.mp4")));
        assert!(is_video(Path::new("a.MOV")));
        assert!(!is_video(Path::new("a.jpg")));
        assert!(!is_video(Path::new("a"))); // no extension at all
    }

    #[test]
    fn neighbor_walks_and_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "a.mp4");
        let b = touch(dir.path(), "b.mp4");
        let c = touch(dir.path(), "c.mp4");

        assert_eq!(neighbor_of(&b, 1), Some(c.clone()));
        assert_eq!(neighbor_of(&b, -1), Some(a.clone()));
        assert_eq!(neighbor_of(&a, -1), None); // clamp at the first
        assert_eq!(neighbor_of(&c, 1), None); // clamp at the last
    }

    #[test]
    fn neighbor_picks_up_files_added_after_open() {
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "a.mp4");
        touch(dir.path(), "c.mp4");
        // At open time the next sibling is c.mp4.
        assert_eq!(
            neighbor_of(&a, 1).unwrap().file_name().unwrap(),
            "c.mp4"
        );
        // A file dropped into the folder mid-session is seen on the next scan.
        let b = touch(dir.path(), "b.mp4");
        assert_eq!(neighbor_of(&a, 1), Some(b));
    }

    /// Where the rename dialog opens its caret — the whole point of the key.
    #[test]
    fn the_caret_opens_before_the_extension() {
        assert_eq!(caret_before_ext("clip.mp4"), 4);
        // The last dot: everything ahead of it is the name being edited.
        assert_eq!(caret_before_ext("a.b.mp4"), 3);
        // Nothing to stay clear of, so the end.
        assert_eq!(caret_before_ext("clip"), 4);
        // Characters, not bytes. A byte offset would put the caret eight
        // characters along a name that is only four long.
        assert_eq!(caret_before_ext("клип.mp4"), 4);
    }

    /// An `App` sitting on a finished grid for a real file, which is what
    /// `commit_rename` needs: nothing holds the clip open, so it renames it
    /// where it lies.
    fn app_on(clip: &Path) -> App {
        let mut app = App::new(None);
        app.loaded = Some(fake_loaded(
            &clip.to_string_lossy(),
            5,
            true,
            Arc::new(AtomicBool::new(false)),
        ));
        app
    }

    /// A name already in the folder is refused rather than renamed onto.
    /// `fs::rename` overwrites its destination without a word, so without the
    /// check a typo would bin the clip that name belongs to.
    #[test]
    fn renaming_onto_a_sibling_is_refused_and_leaves_it_alone() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "a.mp4");
        let b = dir.path().join("b.mp4");
        fs::write(&b, b"keep me").unwrap();
        let mut app = app_on(&a);

        assert!(
            app.commit_rename(&ctx, "b.mp4").is_err(),
            "renaming onto a sibling should be refused"
        );
        assert_eq!(fs::read(&b).unwrap(), b"keep me", "the sibling was overwritten");
        assert!(a.exists(), "the clip should still be where it was");
    }

    /// The ordinary case: the file moves and the grid follows it, unread.
    #[test]
    fn renaming_moves_the_clip_and_keeps_the_grid() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "a.mp4");
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = App::new(None);
        app.loaded = Some(fake_loaded(&a.to_string_lossy(), 5, true, Arc::clone(&cancel)));

        app.commit_rename(&ctx, "b.mp4").expect("the rename should land");

        let b = dir.path().join("b.mp4");
        assert!(b.exists() && !a.exists(), "the clip should have moved");
        let l = app.loaded.as_ref().expect("the clip should still be loaded");
        assert_eq!(l.path, b, "the grid should follow the clip's new name");
        assert_eq!(l.cells.len(), 5, "a finished grid should not be re-read");
        assert!(
            !cancel.load(Ordering::Relaxed),
            "the grid was dropped instead of relabelled"
        );
    }

    /// Enter on a name left as it was closes the dialog and does nothing else.
    #[test]
    fn renaming_to_the_same_name_is_a_no_op() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "a.mp4");
        let mut app = app_on(&a);

        app.commit_rename(&ctx, "a.mp4").expect("an unchanged name should just close");

        assert!(a.exists(), "the clip should be untouched");
    }

    /// Fixing a clip's case is a rename, not a collision: Windows matches names
    /// case-insensitively, so the file "already there" is the clip itself.
    #[test]
    fn a_clip_can_be_renamed_to_fix_its_case() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "clip.mp4");
        let mut app = app_on(&a);

        app.commit_rename(&ctx, "Clip.mp4")
            .expect("a change of case should be allowed");

        let listed: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(listed, ["Clip.mp4"], "the clip should have taken the new case");
    }

    /// The still saved beside a clip is named after it, so it goes where the
    /// clip goes — the same reason a delete bins it.
    #[test]
    fn renaming_takes_the_sidecar_still_along() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "a.mp4");
        touch(dir.path(), "a.jpg");
        let mut app = app_on(&a);

        app.commit_rename(&ctx, "b.mp4").expect("the rename should land");

        assert!(dir.path().join("b.jpg").exists(), "the still should have followed");
        assert!(!dir.path().join("a.jpg").exists(), "the old still should be gone");
        assert!(app.error.is_none(), "a still that moved should be reported as nothing");
    }

    /// Changing only the extension leaves the still's own name unchanged, so the
    /// sidecar is renamed onto itself. That has to pass quietly: reporting it
    /// would put a red "the still stayed" where the grid should be, over a still
    /// that is exactly where it belongs.
    #[test]
    fn renaming_only_the_extension_leaves_the_still_alone() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "a.mp4");
        touch(dir.path(), "a.jpg");
        let mut app = app_on(&a);

        app.commit_rename(&ctx, "a.mkv").expect("the rename should land");

        assert!(dir.path().join("a.mkv").exists(), "the clip should have moved");
        assert!(dir.path().join("a.jpg").exists(), "the still should still be there");
        assert!(app.error.is_none(), "a still that never had to move was reported as stuck");
    }

    /// Names that are not names, or that would strand the view on a clip the
    /// folder scan no longer lists. Each is turned down before anything is
    /// stopped, so the clip is left exactly as it was.
    #[test]
    fn malformed_names_are_refused() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let a = touch(dir.path(), "a.mp4");
        let mut app = app_on(&a);

        assert!(
            app.commit_rename(&ctx, "a.txt").is_err(),
            "a name that drops the video extension should be refused"
        );
        assert!(
            app.commit_rename(&ctx, "   ").is_err(),
            "an empty name should be refused"
        );
        assert!(
            app.commit_rename(&ctx, "sub\\a.mp4").is_err(),
            "a path rather than a name should be refused"
        );
        assert!(a.exists(), "a refused name should leave the clip where it is");
    }
}
