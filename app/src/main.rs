//! footage_viewer — open a clip and see a grid of frames sampled across it.
//!
//! Extraction runs on a background thread and streams thumbnails back; the grid
//! fills in progressively so the window never blocks on open.

// In release, build as a Windows GUI app so launching from Explorer doesn't flash a console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui;
use footage_viewer_media as media;

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

/// Minimum wall-clock gap between live seeks while dragging the scrubber. Each
/// seek restarts the decoder, so throttling caps how many we fire during a drag
/// while still tracking the cursor closely (~12/s).
const SCRUB_INTERVAL_S: f64 = 0.08;

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
    media::init().expect("failed to initialize ffmpeg");

    let pending = std::env::args().nth(1).map(PathBuf::from);
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
    eframe::run_native(
        "footage_viewer",
        options,
        Box::new(|_cc| Ok(Box::new(App::new(pending)))),
    )
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
}

#[derive(Default)]
struct App {
    pending_open: Option<PathBuf>,
    loaded: Option<Loaded>,
    /// Some while a clip is playing back over the grid.
    player: Option<Player>,
    /// egui time of the last live seek fired while dragging the scrubber, for
    /// throttling (see [`SCRUB_INTERVAL_S`]).
    scrub_last_seek: f64,
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

    /// Kick off background extraction; returns immediately.
    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.error = None;
        // Leaving any current clip: stop playback and fall back to the grid.
        self.player = None;

        let (tx, rx) = mpsc::channel();
        let (tx_meta, tx_thumb) = (tx.clone(), tx.clone());
        let (ctx_meta, ctx_thumb, ctx_end) = (ctx.clone(), ctx.clone(), ctx.clone());
        let worker_path = path.clone();

        thread::spawn(move || {
            let result = media::extract_grid_streaming(
                &worker_path,
                THUMB_SPACING_S,
                THUMB_LONG,
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

        self.loaded = Some(Loaded {
            path,
            duration_s: 0.0,
            aspect: 9.0 / 16.0,
            cells: Vec::new(),
            ready: 0,
            done: false,
            rx,
            cursor: 0,
        });
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
        thread::spawn(move || {
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
            self.error = Some(format!("Failed to save still: {e:#}"));
        }
    }

    /// Send the currently loaded clip to the recycle bin without confirmation
    /// and return the sibling to open next — the following clip, or the previous
    /// one if the deleted clip was the last in its folder. Returns `None` when
    /// nothing is loaded, the delete failed (reason recorded in `self.error`), or
    /// the folder is now empty; in the empty case the loaded/playing state is
    /// cleared here so the caller can simply return to the empty view.
    fn delete_current(&mut self) -> Option<PathBuf> {
        let path = self.loaded.as_ref()?.path.clone();
        // Resolve the neighbor before deleting, while the clip still lists.
        let target = self.neighbor(1).or_else(|| self.neighbor(-1));
        if let Err(e) = trash::delete(&path) {
            self.error = Some(format!("Failed to delete {}: {e}", path.display()));
            return None;
        }
        // Also bin the sidecar still saved by save_still (`<clip-stem>.jpg`), if any.
        let still = path.with_extension("jpg");
        if still.exists() {
            if let Err(e) = trash::delete(&still) {
                self.error = Some(format!("Failed to delete {}: {e}", still.display()));
            }
        }
        if target.is_none() {
            // Deleted the only clip in the folder — nothing left to show.
            self.player = None;
            self.loaded = None;
        }
        target
    }

    /// Leave playback and fall back to the grid. A no-op when nothing is playing.
    fn stop_playback(&mut self) {
        self.player = None;
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
                    cmd = seek_bar_ui(
                        ui,
                        bar_rect,
                        p,
                        &mut self.scrub_last_seek,
                        duration_s,
                        now,
                    );
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
/// held `Scrub` while dragging (throttled), or a `Play`/`Scrub` (per pause state)
/// on click/release — else `None`.
fn seek_bar_ui(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    player: &mut Player,
    last_seek: &mut f64,
    duration_s: f64,
    now: f64,
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
            // the cursor has moved off the shown frame, throttled so a fast drag
            // doesn't flood the decoder with seeks.
            player.scrub = Some(target);
            let moved = (target - player.position_s).abs() >= SCRUB_MIN_STEP_S;
            if moved && now - *last_seek >= SCRUB_INTERVAL_S {
                *last_seek = now;
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

        egui::CentralPanel::default().show(ui, |ui| {
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
            let spacing = 6.0f32;
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
                                    2.0,
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
