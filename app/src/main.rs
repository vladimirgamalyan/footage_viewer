//! footage_viewer — open a clip and see a grid of frames sampled across it.
//!
//! Extraction runs on a background thread and streams thumbnails back; the grid
//! fills in progressively so the window never blocks on open.

// In release, build as a Windows GUI app so launching from Explorer doesn't flash a console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui;
use footage_viewer_media as media;

const GRID_N: usize = 16;
const THUMB_LONG: u32 = 320;

fn main() -> eframe::Result<()> {
    media::init().expect("failed to initialize ffmpeg");

    let pending = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("footage_viewer")
            .with_inner_size([1000.0, 720.0]),
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

/// One grid cell: waiting to be decoded, or an uploaded texture.
enum Cell {
    Pending,
    Ready(egui::TextureHandle),
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
}

#[derive(Default)]
struct App {
    pending_open: Option<PathBuf>,
    loaded: Option<Loaded>,
    error: Option<String>,
}

impl App {
    fn new(pending: Option<PathBuf>) -> Self {
        Self {
            pending_open: pending,
            ..Default::default()
        }
    }

    /// Kick off background extraction; returns immediately.
    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.error = None;

        let (tx, rx) = mpsc::channel();
        let (tx_meta, tx_thumb) = (tx.clone(), tx.clone());
        let (ctx_meta, ctx_thumb, ctx_end) = (ctx.clone(), ctx.clone(), ctx.clone());
        let worker_path = path.clone();

        thread::spawn(move || {
            let result = media::extract_grid_streaming(
                &worker_path,
                GRID_N,
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
            cells: (0..GRID_N).map(|_| Cell::Pending).collect(),
            ready: 0,
            done: false,
            rx,
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
                        if let Some(cell) = l.cells.get_mut(i) {
                            *cell = Cell::Ready(tex);
                            l.ready += 1;
                        }
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

        egui::Panel::top("bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open video…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Video", &["mp4", "mkv", "mov", "webm", "avi", "m4v"])
                        .pick_file()
                    {
                        self.open(&ctx, path);
                    }
                }
                if let Some(l) = &self.loaded {
                    ui.separator();
                    let name = l
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.label(format!("{name}  ·  {:.1}s", l.duration_s));
                    if !l.done {
                        ui.separator();
                        ui.add(egui::Spinner::new());
                        ui.label(format!("{}/{}", l.ready, l.cells.len()));
                    }
                }
            });
        });

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

            let cols = (l.cells.len() as f64).sqrt().ceil() as usize;
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
                            match cell {
                                Cell::Ready(tex) => {
                                    let sized = egui::load::SizedTexture::new(tex.id(), size);
                                    ui.add(egui::Image::from_texture(sized));
                                }
                                Cell::Pending => {
                                    let (rect, _) =
                                        ui.allocate_exact_size(size, egui::Sense::hover());
                                    ui.painter().rect_filled(rect, 2.0, egui::Color32::from_gray(35));
                                }
                            }
                            if (i + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    });
            });
        });
    }
}
