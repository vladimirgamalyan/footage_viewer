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

use eframe::egui;
use footage_viewer_media as media;

mod logging;

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

/// JPEG quality for stills saved with the "I" key (1–100). 92 keeps 4:4:4 chroma
/// subsampling and near-lossless detail at a reasonable file size.
const STILL_JPEG_QUALITY: u8 = 92;

/// Extensions we treat as video, for both the open dialog and prev/next navigation.
const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "mov", "webm", "avi", "m4v"];

/// True if `path` has a recognized video extension (case-insensitive).
fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTS.iter().any(|v| v.eq_ignore_ascii_case(e)))
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

    let pending = std::env::args().nth(1).map(PathBuf::from);
    if let Some(p) = &pending {
        log::info!("initial clip from command line: {}", p.display());
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
        Box::new(|_cc| Ok(Box::new(App::new(pending)))),
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

    /// Save a full-resolution JPEG of the frame at `time_s` next to the loaded
    /// clip as `<clip-stem>.jpg`, overwriting any existing file. A no-op with
    /// nothing loaded; records the reason in `self.error` on failure.
    fn save_still(&mut self, time_s: f64) {
        let Some(l) = &self.loaded else { return };
        let out = l.path.with_extension("jpg");
        if let Err(e) = media::save_frame_jpeg(&l.path, time_s, &out, STILL_JPEG_QUALITY) {
            log::error!("failed to save still {} at {time_s:.3}s: {e:#}", out.display());
            self.error = Some(format!("Failed to save still: {e:#}"));
        } else {
            log::info!("saved still {}", out.display());
        }
    }

    /// Send the currently loaded clip to the recycle bin without confirmation
    /// and return the sibling to open next — the following clip, or the previous
    /// one if the deleted clip was the last in its folder. Returns `None` when
    /// nothing is loaded, the delete failed (reason recorded in `self.error`), or
    /// the folder is now empty. On a successful delete the binned clip's grid is
    /// dropped, so the caller can simply return to the empty view.
    ///
    /// Playback always stops first, since the decoder has to release the file
    /// before it can be binned. A caller that was playing therefore replays the
    /// clip it is handed, and a delete that fails lands back on the grid with the
    /// reason showing rather than silently carrying on playing.
    fn delete_current(&mut self) -> Option<PathBuf> {
        let path = self.loaded.as_ref()?.path.clone();
        // Resolve the neighbor before deleting, while the clip still lists.
        let target = self.neighbor(1).or_else(|| self.neighbor(-1));
        // The clip cannot be binned while the decoder still has it open, and DEL
        // is pressed from playback as often as from the grid.
        self.stop_playback_and_wait();
        if let Err(e) = trash::delete(&path) {
            log::error!("failed to delete {}: {e}", path.display());
            self.error = Some(format!("Failed to delete {}: {e}", path.display()));
            return None;
        }
        log::info!("deleted clip {}", path.display());
        // Drop the binned clip's grid rather than let the next `open` park it in
        // the recent cache: it would hold texture memory for a file that is gone
        // and that prev/next can never reach again, since they re-scan the folder.
        self.loaded = None;
        // Also bin the sidecar still saved by save_still (`<clip-stem>.jpg`), if any.
        let still = path.with_extension("jpg");
        if still.exists() {
            if let Err(e) = trash::delete(&still) {
                log::error!("failed to delete sidecar {}: {e}", still.display());
                self.error = Some(format!("Failed to delete {}: {e}", still.display()));
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

    /// Draw the playing clip filling the window and handle its keys. A click in
    /// the video area, Enter, or a decode error returns to the grid; Escape closes
    /// the app. A scrubber along the bottom shows the position and seeks on drag or
    /// click. Left/Right play the previous/next sibling clip from its start,
    /// staying in playback; Space pauses; A/D nudge the position back/forward
    /// by half a second (auto-repeating on hold) and pause on that frame.
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
            if let Some(next) = self.delete_current() {
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
                self.save_still(t);
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
        // strip. A click in the video area returns to the grid; the scrubber
        // handles its own clicks and drags.
        let (clicked, seek) = egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ui, |ui| {
                let full = ui.max_rect();
                let bar_h = 34.0;
                let split = (full.max.y - bar_h).max(full.min.y);
                let video_rect =
                    egui::Rect::from_min_max(full.min, egui::pos2(full.max.x, split));
                let bar_rect =
                    egui::Rect::from_min_max(egui::pos2(full.min.x, split), full.max);

                let resp = ui.interact(video_rect, ui.id().with("playback"), egui::Sense::click());
                if let Some(p) = &self.player {
                    if let Some(tex) = &p.tex {
                        let target = fit_centered(video_rect, p.frame_size);
                        let uv =
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                        ui.painter().image(tex.id(), target, uv, egui::Color32::WHITE);
                    }
                }

                let mut cmd = None;
                if let Some(p) = &mut self.player {
                    cmd = seek_bar_ui(ui, bar_rect, p, &mut self.scrub_in_flight, duration_s);
                }
                (resp.clicked(), cmd)
            })
            .inner;

        // Seeks go to the live decoder as commands — the player stays put.
        if let Some(cmd) = seek {
            self.send_cmd(cmd);
        }
        if clicked {
            self.stop_playback();
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

    thread::spawn(move || {
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
            Ok(()) => tx.send(Msg::Done),
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
        // Open a dropped file (last one wins).
        if let Some(path) =
            ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).last())
        {
            self.open(&ctx, path);
        }

        self.poll(&ctx);
        self.sync_title(&ctx);
        self.look_ahead(&ctx);

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
            if let Some(next) = self.delete_current() {
                self.open(&ctx, next);
            }
            return;
        }

        // -/+ change how many columns the grid shows, trading thumbnail size for
        // how many frames fit on screen. Accept "=" as "+" too, since on most
        // layouts "+" is Shift+"=". The cursor is an absolute cell index, so it
        // stays valid across a column change with no fix-up needed.
        if self.loaded.is_some() {
            let (shrink, grow) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::Minus),
                    i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                )
            });
            if shrink {
                self.grid_cols = self.grid_cols.saturating_sub(1).max(GRID_COLS_MIN);
            }
            if grow {
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
            self.save_still(t);
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
}
