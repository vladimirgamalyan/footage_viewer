//! footage_viewer — open a clip and see a grid of frames sampled across it.
//!
//! Skeleton: synchronous extract-on-open + grid render. Background extraction and
//! progressive fill (see docs/concept.md) come next.

use std::path::PathBuf;

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

/// A loaded clip: its grid uploaded as GPU textures.
struct Loaded {
    path: PathBuf,
    duration_s: f64,
    textures: Vec<egui::TextureHandle>,
    aspect: f32, // height / width of a thumbnail
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

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        match media::extract_grid(&path, GRID_N, THUMB_LONG) {
            Ok(grid) => {
                let textures = grid
                    .thumbs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let img = egui::ColorImage::from_rgba_unmultiplied(
                            [t.width as usize, t.height as usize],
                            &t.rgba,
                        );
                        ctx.load_texture(format!("thumb_{i}"), img, egui::TextureOptions::default())
                    })
                    .collect();
                let aspect = grid
                    .thumbs
                    .first()
                    .map(|t| t.height as f32 / t.width as f32)
                    .unwrap_or(9.0 / 16.0);
                self.loaded = Some(Loaded {
                    path,
                    duration_s: grid.duration_s,
                    textures,
                    aspect,
                });
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
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

            let cols = (l.textures.len() as f64).sqrt().ceil() as usize;
            let spacing = 6.0f32;
            let avail = ui.available_width();
            let cell_w = ((avail - spacing * (cols as f32 + 1.0)) / cols as f32).max(80.0);
            let cell_h = cell_w * l.aspect;

            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("frame_grid")
                    .spacing([spacing, spacing])
                    .show(ui, |ui| {
                        for (i, tex) in l.textures.iter().enumerate() {
                            let sized =
                                egui::load::SizedTexture::new(tex.id(), egui::vec2(cell_w, cell_h));
                            ui.add(egui::Image::from_texture(sized));
                            if (i + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    });
            });
        });
    }
}
