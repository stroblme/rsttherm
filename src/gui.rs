// SPDX-License-Identifier: GPL-3.0-or-later

//! Live viewer: the false-colour image with a colour bar, spot meters,
//! min/max/centre markers and digital zoom, and a sidebar for the camera, its
//! shutter, palette, colour range, emissivity correction and noise reduction.
//! Settings and the window geometry are persisted by eframe into the platform
//! config directory.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui::{
    Align2, Color32, ColorImage, FontFamily, FontId, Painter, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Ui, Vec2, pos2, vec2,
};
use serde::{Deserialize, Serialize};

use crate::camera::{self, Device, Frame};
use crate::http;
use crate::live::Live;
use crate::thermal::{Correction, Image};

/// The same flat dark palette as rstptz, plus marker colours.
mod pal {
    use super::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x16, 0x18, 0x1D);
    pub const SURFACE: Color32 = Color32::from_rgb(0x1E, 0x21, 0x28);
    pub const HOVER: Color32 = Color32::from_rgb(0x27, 0x2B, 0x33);
    pub const ACTIVE: Color32 = Color32::from_rgb(0x31, 0x37, 0x42);
    pub const BORDER: Color32 = Color32::from_rgb(0x2E, 0x33, 0x3C);
    pub const TEXT: Color32 = Color32::from_rgb(0xE4, 0xE7, 0xEC);
    pub const MUTED: Color32 = Color32::from_rgb(0x8B, 0x91, 0x9E);
    pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x8D, 0xFF);
    pub const WARN: Color32 = Color32::from_rgb(0xF5, 0xA5, 0x24);
    pub const HOT: Color32 = Color32::from_rgb(0xFF, 0x6B, 0x6B);
    pub const COLD: Color32 = Color32::from_rgb(0x6B, 0xB8, 0xFF);
    pub const TAG: Color32 = Color32::from_black_alpha(0xB0);
}

const GAP: f32 = 6.0;
const PAD: f32 = 12.0;
const ROW: f32 = 28.0;
const SIDEBAR: f32 = 232.0;
const CONTENT: f32 = SIDEBAR - 2.0 * PAD;
const BAR: f32 = 14.0;
/// Room right of the image for the colour bar and its labels.
const LEGEND: f32 = 2.0 * GAP + BAR + 50.0;
/// The auto range never gets narrower than this, so a uniform scene shows as
/// a uniform colour instead of sensor noise stretched over the whole palette.
const MIN_SPAN: f32 = 2.0;
/// How far the auto range moves towards each new frame's, to stop it flickering.
const SMOOTHING: f32 = 0.2;
/// Largest digital zoom; past this a sensor pixel is just a blur.
const MAX_ZOOM: f32 = 8.0;
/// One click of the zoom buttons.
const ZOOM_STEP: f32 = 1.5;
/// How long the zoom buttons stay after the pointer leaves the image.
const LINGER: Duration = Duration::from_millis(1500);
/// The whole image, in texture coordinates.
const FULL: Rect = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
/// A pixel that moves further than this from its running average is taken as
/// motion and shown as is, so moving things don't smear. 32 counts is 0.5 K,
/// about eight times the P3's frame-to-frame noise (σ ≈ 4 counts measured).
const MOTION: f32 = 32.0;

/// Gradient stops from cold to hot, spread evenly over the colour range.
const PALETTES: [(&str, &[[u8; 3]]); 5] = [
    (
        "Ironbow",
        &[
            [0, 0, 0],
            [32, 0, 96],
            [120, 0, 140],
            [190, 30, 110],
            [230, 80, 30],
            [250, 150, 0],
            [255, 210, 40],
            [255, 255, 220],
        ],
    ),
    (
        "Rainbow",
        &[
            [0, 0, 140],
            [0, 0, 255],
            [0, 255, 255],
            [255, 255, 0],
            [255, 0, 0],
            [128, 0, 0],
        ],
    ),
    (
        "Inferno",
        &[
            [0, 0, 4],
            [40, 11, 84],
            [101, 21, 110],
            [159, 42, 99],
            [212, 72, 66],
            [245, 125, 21],
            [250, 193, 39],
            [252, 255, 164],
        ],
    ),
    ("White hot", &[[0, 0, 0], [255, 255, 255]]),
    ("Black hot", &[[255, 255, 255], [0, 0, 0]]),
];

fn lut(stops: &[[u8; 3]]) -> [Color32; 256] {
    std::array::from_fn(|i| {
        let pos = i as f32 / 255.0 * (stops.len() - 1) as f32;
        let k = (pos as usize).min(stops.len() - 2);
        let t = pos - k as f32;
        let mix = |c: usize| {
            let (a, b) = (stops[k][c] as f32, stops[k + 1][c] as f32);
            (a + (b - a) * t).round() as u8
        };
        Color32::from_rgb(mix(0), mix(1), mix(2))
    })
}

/// A quarter turn clockwise.
fn rotated(image: &Image) -> Image {
    let (w, h) = (image.width, image.height);
    Image {
        width: h,
        height: w,
        temps: (0..w * h)
            .map(|i| {
                let (x, y) = (i % h, i / h);
                image.temps[(h - 1 - x) * w + y]
            })
            .collect(),
    }
}

/// Flipped left to right.
fn mirrored(image: &Image) -> Image {
    Image {
        temps: image
            .temps
            .chunks(image.width)
            .flat_map(|row| row.iter().rev())
            .copied()
            .collect(),
        ..*image
    }
}

/// Colour range that ignores the coldest and hottest 1 % of pixels, so one dead
/// or glinting pixel cannot squash the rest of the scene into a single colour.
fn auto_range(temps: &[f32]) -> [f32; 2] {
    let mut sorted = temps.to_vec();
    let cut = sorted.len() / 100;
    let last = sorted.len() - 1 - cut;
    let lo = *sorted.select_nth_unstable_by(cut, f32::total_cmp).1;
    let hi = *sorted.select_nth_unstable_by(last, f32::total_cmp).1;
    [lo, hi.max(lo + MIN_SPAN)]
}

/// Temporal noise reduction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Denoise {
    Off,
    Low,
    High,
}

impl Denoise {
    /// The weight a new frame gets in the running average.
    fn weight(self) -> Option<f32> {
        match self {
            Denoise::Off => None,
            Denoise::Low => Some(0.5),
            Denoise::High => Some(0.2),
        }
    }
}

/// Motion-adaptive running average of the raw counts, updated in place. Starts
/// over from `raw` when the frame size changes.
fn denoise(average: &mut Vec<f32>, raw: &[u16], weight: f32) {
    if average.len() != raw.len() {
        *average = raw.iter().map(|&r| r as f32).collect();
        return;
    }
    for (avg, &r) in average.iter_mut().zip(raw) {
        let r = r as f32;
        *avg = if (r - *avg).abs() > MOTION {
            r
        } else {
            *avg + (r - *avg) * weight
        };
    }
}

/// `view` zoomed by `factor` around `anchor`, both in texture coordinates.
fn zoomed(view: Rect, factor: f32, anchor: Pos2) -> Rect {
    let size = (view.width() / factor).clamp(1.0 / MAX_ZOOM, 1.0);
    let k = size / view.width();
    clamped(Rect::from_min_size(
        anchor + (view.min - anchor) * k,
        Vec2::splat(size),
    ))
}

/// `view` moved back inside the image.
fn clamped(view: Rect) -> Rect {
    let max = pos2(1.0, 1.0) - view.size();
    Rect::from_min_size(view.min.clamp(Pos2::ZERO, max), view.size())
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    palette: usize,
    correction: Correction,
    /// `None` follows the scene; otherwise the colour range is pinned, in °C.
    range: Option<[f32; 2]>,
    /// Quarter turns clockwise.
    turns: u8,
    mirror: bool,
    center: bool,
    extremes: bool,
    denoise: Denoise,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            palette: 0,
            correction: Correction::DEFAULT,
            range: None,
            turns: 0,
            mirror: false,
            center: true,
            extremes: true,
            denoise: Denoise::Low,
        }
    }
}

pub fn run(device: Option<String>) -> camera::Result<()> {
    let live = Live::start(device, Correction::DEFAULT);
    {
        // One process covers both the viewer and HTTP clients.
        let live = Arc::clone(&live);
        thread::spawn(move || {
            if let Err(e) = http::serve(&live, http::DEFAULT_BIND, http::DEFAULT_PORT) {
                eprintln!("rsttherm: http server: {e}");
            }
        });
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1110.0, 640.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "rsttherm",
        options,
        Box::new(move |cc| {
            theme(&cc.egui_ctx);
            Ok(Box::new(Viewer::new(cc, live)))
        }),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Inter and the flat dark palette, as in rstptz.
fn theme(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "inter".into(),
        egui::FontData::from_static(include_bytes!("../assets/Inter-Regular.ttf")).into(),
    );
    fonts.font_data.insert(
        "inter-semibold".into(),
        egui::FontData::from_static(include_bytes!("../assets/Inter-SemiBold.ttf")).into(),
    );
    // Inserted at the front rather than replacing the list, so egui's own fonts
    // stay as fallbacks for anything Inter does not cover.
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "inter".into());
    fonts
        .families
        .insert(semibold(), vec!["inter-semibold".into()]);
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|style| {
        use egui::TextStyle;

        style.spacing.item_spacing = vec2(GAP, GAP);
        style.spacing.button_padding = vec2(10.0, 5.0);
        style.spacing.interact_size.y = 24.0;

        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Body, FontId::proportional(13.0)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Monospace, FontId::monospace(12.0)),
            (TextStyle::Heading, FontId::proportional(16.0)),
        ]
        .into();

        let visuals = &mut style.visuals;
        visuals.panel_fill = pal::BG;
        visuals.extreme_bg_color = pal::BG;
        visuals.window_fill = pal::SURFACE;
        visuals.window_stroke = Stroke::new(1.0, pal::BORDER);
        visuals.selection.bg_fill = pal::ACCENT;
        visuals.selection.stroke = Stroke::new(1.0, pal::TEXT);

        for (widget, fill, text) in [
            (
                &mut visuals.widgets.noninteractive,
                pal::SURFACE,
                pal::MUTED,
            ),
            (&mut visuals.widgets.inactive, pal::SURFACE, pal::TEXT),
            (&mut visuals.widgets.hovered, pal::HOVER, pal::TEXT),
            (&mut visuals.widgets.active, pal::ACTIVE, pal::TEXT),
            (&mut visuals.widgets.open, pal::HOVER, pal::TEXT),
        ] {
            widget.corner_radius = egui::CornerRadius::same(6);
            widget.bg_fill = fill;
            widget.weak_bg_fill = fill;
            widget.bg_stroke = Stroke::new(1.0, pal::BORDER);
            widget.fg_stroke = Stroke::new(1.0, text);
            widget.expansion = 0.0;
        }
    });
}

/// The bold family registered by `theme`.
fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

struct Viewer {
    live: Arc<Live>,
    s: Settings,
    texture: Option<TextureHandle>,
    /// The frame on screen, to tell a new one from a repaint.
    frame: Option<Arc<Frame>>,
    /// That frame, corrected and turned the way it is shown.
    image: Option<Image>,
    /// The auto range, smoothed.
    auto: [f32; 2],
    /// Pixels in `image` coordinates.
    spots: Vec<(usize, usize)>,
    fps: f32,
    last_frame: Instant,
    /// Running average of the raw counts, empty while noise reduction is off.
    average: Vec<f32>,
    /// Visible part of the image in texture coordinates, [`FULL`] unzoomed.
    view: Rect,
    /// Last time the pointer was over the image, for the zoom buttons.
    pointer_seen: Option<Instant>,
    /// Cameras for the picker, enumerated when it opens.
    devices: Vec<Device>,
    /// Screen area a snapshot keeps: the image and its colour bar.
    capture: Rect,
    /// When a snapshot was asked for; the zoom buttons stay out of it.
    snapshot: Option<Instant>,
    note: String,
}

impl Viewer {
    fn new(cc: &eframe::CreationContext, live: Arc<Live>) -> Self {
        let s: Settings = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, eframe::APP_KEY))
            .unwrap_or_default();
        Self {
            live,
            s,
            texture: None,
            frame: None,
            image: None,
            auto: [0.0, MIN_SPAN],
            spots: Vec::new(),
            fps: 0.0,
            last_frame: Instant::now(),
            average: Vec::new(),
            view: FULL,
            pointer_seen: None,
            devices: camera::list_devices().unwrap_or_default(),
            capture: Rect::NOTHING,
            snapshot: None,
            note: String::new(),
        }
    }

    fn palette(&self) -> [Color32; 256] {
        lut(PALETTES[self.s.palette.min(PALETTES.len() - 1)].1)
    }

    fn range(&self) -> [f32; 2] {
        self.s.range.unwrap_or(self.auto)
    }

    /// Rebuilt on every pass rather than only on new frames, so palette and
    /// correction changes show at once; it is a few hundred microseconds.
    fn refresh(&mut self, ctx: &egui::Context) {
        *self.live.correction.lock().unwrap() = self.s.correction;
        let Some(frame) = self.live.frame() else {
            // Reconnecting, or switching cameras: start the averages afresh.
            self.image = None;
            self.frame = None;
            self.average.clear();
            return;
        };

        let new = self.frame.as_ref().is_none_or(|f| !Arc::ptr_eq(f, &frame));
        if new {
            match self.s.denoise.weight() {
                Some(weight) => denoise(&mut self.average, &frame.raw, weight),
                None => self.average.clear(),
            }
        }
        let denoised;
        let source = if self.average.is_empty() {
            &*frame
        } else {
            denoised = Frame {
                width: frame.width,
                height: frame.height,
                raw: self.average.iter().map(|&a| a.round() as u16).collect(),
            };
            &denoised
        };

        let mut image = Image::new(source, &self.s.correction);
        for _ in 0..self.s.turns % 4 {
            image = rotated(&image);
        }
        if self.s.mirror {
            image = mirrored(&image);
        }

        if new {
            let [lo, hi] = auto_range(&image.temps);
            self.auto = match self.frame {
                None => [lo, hi],
                Some(_) => [
                    self.auto[0] + (lo - self.auto[0]) * SMOOTHING,
                    self.auto[1] + (hi - self.auto[1]) * SMOOTHING,
                ],
            };
            let dt = self.last_frame.elapsed().as_secs_f32();
            self.fps += (1.0 / dt.max(1e-3) - self.fps) * 0.1;
            self.last_frame = Instant::now();
            self.frame = Some(frame);
        }

        let [lo, hi] = self.range();
        let lut = self.palette();
        let pixels = image
            .temps
            .iter()
            .map(|&t| lut[((t - lo) / (hi - lo).max(0.1) * 255.0).clamp(0.0, 255.0) as usize])
            .collect();
        let colours = ColorImage::new([image.width, image.height], pixels);
        match &mut self.texture {
            Some(texture) => texture.set(colours, TextureOptions::LINEAR),
            None => {
                self.texture = Some(ctx.load_texture("thermal", colours, TextureOptions::LINEAR))
            }
        }
        self.image = Some(image);
    }

    fn controls(&mut self, ui: &mut Ui) {
        heading(ui, "Camera");
        self.camera_picker(ui);
        if ui
            .add_sized(vec2(CONTENT, ROW), egui::Button::new("Shutter / NUC"))
            .on_hover_text("flat-field calibration; the camera also runs one about every 90 s")
            .clicked()
        {
            self.live.shutter();
        }

        heading(ui, "Palette");
        egui::ComboBox::from_id_salt("palette")
            .selected_text(PALETTES[self.s.palette.min(PALETTES.len() - 1)].0)
            .width(CONTENT)
            .show_ui(ui, |ui| {
                for (i, (name, _)) in PALETTES.iter().enumerate() {
                    ui.selectable_value(&mut self.s.palette, i, *name);
                }
            });

        heading(ui, "Range");
        ui.horizontal(|ui| {
            let half = vec2((CONTENT - GAP) / 2.0, ROW);
            let auto = self.s.range.is_none();
            if ui
                .add_sized(half, egui::Button::selectable(auto, "Auto"))
                .clicked()
            {
                self.s.range = None;
            }
            if ui
                .add_sized(half, egui::Button::selectable(!auto, "Manual"))
                .on_hover_text("pin the colour range where it is now")
                .clicked()
                && auto
            {
                self.s.range = Some(self.auto);
            }
        });
        if let Some([lo, hi]) = &mut self.s.range {
            ui.horizontal(|ui| {
                ui.add(degrees(lo).range(-40.0..=*hi - 0.1));
                ui.label("to");
                ui.add(degrees(hi).range(*lo + 0.1..=600.0));
            });
        }

        heading(ui, "Correction");
        let c = &mut self.s.correction;
        egui::Grid::new("correction")
            .num_columns(2)
            .spacing(vec2(2.0 * GAP, GAP))
            .show(ui, |ui| {
                ui.label("Emissivity").on_hover_text(
                    "0.95 for skin, paint, wood and plastics; bare metal reads wrong at any setting",
                );
                ui.add(
                    egui::DragValue::new(&mut c.emissivity)
                        .speed(0.01)
                        .range(0.1..=1.0)
                        .fixed_decimals(2),
                );
                ui.end_row();
                ui.label("Reflected")
                    .on_hover_text("temperature of what the surface reflects, usually the room");
                ui.add(degrees(&mut c.reflected));
                ui.end_row();
                ui.label("Offset").on_hover_text(
                    "added to every reading: aim at something of known temperature and dial out the difference",
                );
                ui.add(degrees(&mut c.offset).speed(0.05));
                ui.end_row();
            });

        heading(ui, "Noise reduction");
        ui.horizontal(|ui| {
            let third = vec2((CONTENT - 2.0 * GAP) / 3.0, ROW);
            for (level, name) in [
                (Denoise::Off, "Off"),
                (Denoise::Low, "Low"),
                (Denoise::High, "High"),
            ] {
                if ui
                    .add_sized(
                        third,
                        egui::Button::selectable(self.s.denoise == level, name),
                    )
                    .on_hover_text("averages over frames; anything moving is shown as is")
                    .clicked()
                {
                    self.s.denoise = level;
                }
            }
        });

        heading(ui, "Markers");
        ui.checkbox(&mut self.s.center, "Centre");
        ui.checkbox(&mut self.s.extremes, "Min / max");
        ui.horizontal(|ui| {
            ui.label(format!("{} spots", self.spots.len()));
            if ui.button("Clear").clicked() {
                self.spots.clear();
            }
        });
        ui.label(
            RichText::new("Click the image to add a spot, right-click to remove one.")
                .small()
                .color(pal::MUTED),
        );

        heading(ui, "View");
        ui.horizontal(|ui| {
            if ui.button("Rotate").clicked() {
                self.s.turns = (self.s.turns + 1) % 4;
                self.spots.clear();
                self.view = FULL;
            }
            if ui.checkbox(&mut self.s.mirror, "Mirror").changed() {
                self.spots.clear();
                self.view = FULL;
            }
        });
        ui.add_space(GAP);
        if ui
            .add_sized(vec2(CONTENT, ROW), egui::Button::new("Snapshot"))
            .on_hover_text("save what is shown as PNG, and the temperatures as CSV")
            .clicked()
        {
            self.snapshot = Some(Instant::now());
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
    }

    fn camera_picker(&mut self, ui: &mut Ui) {
        let current = self.live.device();
        let name = |d: &Device| format!("{} · {}", d.name, d.id);
        let selected = match &current {
            Some(id) => self
                .devices
                .iter()
                .find(|d| &d.id == id)
                .map_or_else(|| id.clone(), name),
            None => "no camera".to_string(),
        };
        let mut picked = None;
        let combo = egui::ComboBox::from_id_salt("camera")
            .selected_text(selected)
            .width(CONTENT)
            .truncate()
            .show_ui(ui, |ui| {
                for d in &self.devices {
                    if ui
                        .selectable_label(current.as_ref() == Some(&d.id), name(d))
                        .clicked()
                    {
                        picked = Some(d.id.clone());
                    }
                }
                if self.devices.is_empty() {
                    ui.label(RichText::new("no cameras found").color(pal::MUTED));
                }
            });
        // Enumerating is a sysfs walk, so only when the list is opened.
        if combo.response.clicked() {
            self.devices = camera::list_devices().unwrap_or_default();
        }
        if let Some(id) = picked {
            self.live.select(id);
        }
    }

    fn status_bar(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let status = self.live.status();
            if !status.is_empty() {
                ui.label(RichText::new(status).small().color(pal::WARN));
            } else if let Some(image) = &self.image {
                let text = format!("{:.1} fps · {}×{}", self.fps, image.width, image.height);
                ui.label(RichText::new(text).small().color(pal::MUTED));
            }
            if !self.note.is_empty() {
                ui.label(RichText::new(&self.note).small().color(pal::MUTED));
            }
        });
    }

    fn view(&mut self, ui: &mut Ui) {
        let (Some(image), Some(texture)) = (&self.image, &self.texture) else {
            let status = self.live.status();
            ui.centered_and_justified(|ui| ui.label(RichText::new(status).color(pal::MUTED)));
            return;
        };
        let (w, h) = (image.width, image.height);

        // Fit the image and its colour bar, keeping the aspect ratio, centred.
        let area = ui.available_rect_before_wrap();
        let room = vec2(area.width() - LEGEND, area.height()).max(Vec2::splat(1.0));
        let aspect = w as f32 / h as f32;
        let size = if room.x / room.y > aspect {
            vec2(room.y * aspect, room.y)
        } else {
            vec2(room.x, room.x / aspect)
        };
        let rect = Rect::from_min_size(area.min + (room - size) / 2.0, size);
        let response = ui.allocate_rect(rect, Sense::click_and_drag());

        // Wheel or pinch zooms around the pointer, dragging pans.
        if let Some(p) = response.hover_pos() {
            let factor = ui.input(|i| i.zoom_delta() * (i.smooth_scroll_delta.y / 200.0).exp());
            if factor != 1.0 {
                let anchor = self.view.min + (p - rect.min) / rect.size() * self.view.size();
                self.view = zoomed(self.view, factor, anchor);
            }
        }
        if response.dragged() {
            let delta = response.drag_delta() / rect.size() * self.view.size();
            self.view = clamped(self.view.translate(-delta));
        }
        let view = self.view;

        let painter = ui.painter();
        painter.image(texture.id(), rect, view, Color32::WHITE);

        // Screen points per texture coordinate.
        let scale = rect.size() / view.size();
        let to_screen = |(x, y): (usize, usize)| {
            let uv = pos2((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
            rect.min + (uv - view.min) * scale
        };
        let to_pixel = |p: Pos2| {
            let uv = view.min + (p - rect.min) / scale;
            (
                ((uv.x * w as f32).max(0.0) as usize).min(w - 1),
                ((uv.y * h as f32).max(0.0) as usize).min(h - 1),
            )
        };
        // Markers zoomed out of view are skipped rather than pinned to the edge.
        let mark_visible = |at: Pos2, text: String, colour: Color32| {
            if rect.contains(at) {
                mark(painter, rect, at, text, colour);
            }
        };

        if let Some(p) = response.interact_pointer_pos() {
            if response.clicked() {
                self.spots.push(to_pixel(p));
            } else if response.secondary_clicked() {
                let nearest = self
                    .spots
                    .iter()
                    .map(|&s| to_screen(s).distance(p))
                    .enumerate()
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                if let Some((i, d)) = nearest
                    && d < 20.0
                {
                    self.spots.remove(i);
                }
            }
        }

        let reading = |t: Option<f32>| t.map_or("–".to_string(), |t| format!("{t:.1}"));
        if self.s.extremes {
            let stats = image.stats();
            for ((t, i), name, colour) in
                [(stats.max, "max", pal::HOT), (stats.min, "min", pal::COLD)]
            {
                mark_visible(to_screen((i % w, i / w)), format!("{name} {t:.1}"), colour);
            }
        }
        if self.s.center {
            let text = reading(Some(image.center()));
            mark_visible(to_screen((w / 2, h / 2)), text, pal::TEXT);
        }
        for (n, &(x, y)) in self.spots.iter().enumerate() {
            let text = format!("{} {}", n + 1, reading(image.at(x, y)));
            mark_visible(to_screen((x, y)), text, pal::TEXT);
        }
        if let Some(p) = response.hover_pos() {
            let (x, y) = to_pixel(p);
            let text = format!("{} °C", reading(image.at(x, y)));
            tag(painter, rect, p + vec2(14.0, 14.0), text, pal::TEXT);
        }

        let legend = self.legend(painter, rect);
        self.capture = rect.union(legend);
        // The zoom buttons show while the pointer is over the image and for a
        // moment after, fading out; never in a snapshot.
        // Not `response.contains_pointer()`: that is false while the pointer is
        // on a zoom button, which would fade the button out under it.
        if ui.rect_contains_pointer(rect) {
            self.pointer_seen = Some(Instant::now());
        }
        let shown = self.pointer_seen.is_some_and(|t| t.elapsed() < LINGER)
            && self
                .snapshot
                .is_none_or(|asked| asked.elapsed() > Duration::from_secs(1));
        let opacity = ui.ctx().animate_bool(ui.id().with("zoom"), shown);
        if opacity > 0.0 {
            self.zoom_buttons(ui, rect, shown, opacity);
        }
    }

    /// Zoom out, zoom in and fit, over the image's top-left corner, with the
    /// current factor. Clickable only while `shown`, not while fading out. A
    /// click takes effect on the next pass.
    fn zoom_buttons(&mut self, ui: &Ui, image: Rect, shown: bool, opacity: f32) {
        let mut painter = ui.painter().clone();
        painter.multiply_opacity(opacity);
        let mut at = image.min + Vec2::splat(GAP);
        for (label, factor) in [
            ("−", Some(1.0 / ZOOM_STEP)),
            ("+", Some(ZOOM_STEP)),
            ("Fit", None),
        ] {
            let width = if factor.is_some() { 24.0 } else { 36.0 };
            let rect = Rect::from_min_size(at, vec2(width, 22.0));
            let sense = if shown {
                Sense::click()
            } else {
                Sense::hover()
            };
            let response = ui.interact(rect, ui.id().with(("zoom", label)), sense);
            let fill = if response.hovered() {
                pal::ACTIVE
            } else {
                pal::TAG
            };
            painter.rect_filled(rect, 4.0, fill);
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(13.0),
                pal::TEXT,
            );
            if response.clicked() {
                self.view = factor.map_or(FULL, |f| zoomed(self.view, f, self.view.center()));
            }
            at.x = rect.right() + 4.0;
        }
        let zoom = format!("×{:.1}", 1.0 / self.view.width());
        tag(&painter, image, at + vec2(3.0, 4.0), zoom, pal::TEXT);
    }

    /// Colour bar with five evenly spaced labels; returns the area it used.
    fn legend(&self, painter: &Painter, image: Rect) -> Rect {
        let bar = Rect::from_min_size(
            pos2(image.right() + 2.0 * GAP, image.top()),
            vec2(BAR, image.height()),
        );
        let lut = self.palette();
        let slices = 64;
        let step = bar.height() / slices as f32;
        for k in 0..slices {
            let top = bar.top() + k as f32 * step;
            let slice =
                Rect::from_min_max(pos2(bar.left(), top), pos2(bar.right(), top + step + 0.5));
            let hot = 1.0 - (k as f32 + 0.5) / slices as f32;
            painter.rect_filled(slice, 0.0, lut[(hot * 255.0) as usize]);
        }
        painter.rect_stroke(bar, 0.0, Stroke::new(1.0, pal::BORDER), StrokeKind::Outside);

        let [lo, hi] = self.range();
        for k in 0..=4 {
            let f = k as f32 / 4.0;
            painter.text(
                pos2(bar.right() + GAP, bar.bottom() - f * bar.height()),
                Align2::LEFT_CENTER,
                format!("{:.1}", lo + (hi - lo) * f),
                FontId::proportional(11.0),
                pal::MUTED,
            );
        }
        bar.with_max_x(image.right() + LEGEND).expand(GAP)
    }

    /// Writes the snapshot once the screenshot asked for by the button arrives,
    /// a frame or two later.
    fn save_snapshot(&mut self, ui: &Ui) {
        let shot = ui.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        let (Some(shot), Some(image)) = (shot, &self.image) else {
            return;
        };
        let ppp = ui.ctx().pixels_per_point();
        let screen = Rect::from_min_size(
            Pos2::ZERO,
            vec2(shot.width() as f32, shot.height() as f32) / ppp,
        );
        self.snapshot = None;
        let region = shot.region(&self.capture.intersect(screen), Some(ppp));
        self.note = match save(&region, image) {
            Ok(base) => format!("saved {base}.png and .csv"),
            Err(e) => format!("snapshot failed: {e}"),
        };
    }
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.refresh(ui.ctx());
        self.save_snapshot(ui);
        // Every row is sized to CONTENT, which the margin makes exactly the
        // panel's inner width: anything wider would push the panel off-screen.
        egui::Panel::right("controls")
            .exact_size(SIDEBAR)
            .resizable(false)
            .frame(egui::Frame::new().fill(pal::BG).inner_margin(PAD))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.controls(ui));
            });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.view(ui));
        // Frames arrive at 25 Hz; poll a little faster than that.
        ui.ctx().request_repaint_after(Duration::from_millis(20));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.s);
    }
}

fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(GAP);
    ui.label(RichText::new(text).family(semibold()).color(pal::MUTED));
}

fn degrees(value: &mut f32) -> egui::DragValue<'_> {
    egui::DragValue::new(value)
        .speed(0.1)
        .fixed_decimals(1)
        .suffix(" °C")
}

/// A crosshair that reads on any palette, with a labelled reading.
fn mark(painter: &Painter, bounds: Rect, at: Pos2, text: String, colour: Color32) {
    for stroke in [
        Stroke::new(3.0, Color32::BLACK),
        Stroke::new(1.0, Color32::WHITE),
    ] {
        for d in [
            vec2(1.0, 0.0),
            vec2(-1.0, 0.0),
            vec2(0.0, 1.0),
            vec2(0.0, -1.0),
        ] {
            painter.line_segment([at + d * 3.0, at + d * 8.0], stroke);
        }
    }
    tag(painter, bounds, at + vec2(9.0, -22.0), text, colour);
}

/// Text on a dark pill, kept inside `bounds`.
fn tag(painter: &Painter, bounds: Rect, at: Pos2, text: String, colour: Color32) {
    let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), colour);
    let r = Rect::from_min_size(at, galley.size()).expand(3.0);
    let shift = vec2(
        (bounds.right() - r.right()).min(0.0) + (bounds.left() - r.left()).max(0.0),
        (bounds.bottom() - r.bottom()).min(0.0) + (bounds.top() - r.top()).max(0.0),
    );
    let r = r.translate(shift);
    painter.rect_filled(r, 4.0, pal::TAG);
    painter.galley(r.min + vec2(3.0, 3.0), galley, colour);
}

/// `rsttherm-<unix time>.png` and `.csv` in the working directory.
// ponytail: unix time and the working directory; add a save dialog when someone
// wants their snapshots elsewhere.
fn save(shot: &ColorImage, image: &Image) -> camera::Result<String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let base = format!("rsttherm-{stamp}");

    let file = BufWriter::new(File::create(format!("{base}.png"))?);
    let mut png = png::Encoder::new(file, shot.width() as u32, shot.height() as u32);
    png.set_color(png::ColorType::Rgba);
    png.set_depth(png::BitDepth::Eight);
    let rgba: Vec<u8> = shot.pixels.iter().flat_map(|c| c.to_array()).collect();
    png.write_header()?.write_image_data(&rgba)?;

    let mut csv = BufWriter::new(File::create(format!("{base}.csv"))?);
    for row in image.temps.chunks(image.width) {
        let cells: Vec<String> = row.iter().map(|t| format!("{t:.2}")).collect();
        writeln!(csv, "{}", cells.join(","))?;
    }
    csv.flush()?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_run_from_first_to_last_stop() {
        for (name, stops) in PALETTES {
            let lut = lut(stops);
            let [r, g, b] = stops[0];
            assert_eq!(lut[0], Color32::from_rgb(r, g, b), "{name}");
            let [r, g, b] = stops[stops.len() - 1];
            assert_eq!(lut[255], Color32::from_rgb(r, g, b), "{name}");
        }
    }

    #[test]
    fn denoise_averages_noise_and_follows_motion() {
        let mut average = Vec::new();
        denoise(&mut average, &[1000, 1000], 0.5);
        assert_eq!(average, [1000.0, 1000.0]);
        // Noise is averaged; a jump beyond MOTION is taken as is.
        denoise(&mut average, &[1010, 1000 + MOTION as u16 + 1], 0.5);
        assert_eq!(average, [1005.0, 1000.0 + MOTION + 1.0]);
        // A different frame size starts over.
        denoise(&mut average, &[7], 0.5);
        assert_eq!(average, [7.0]);
    }

    #[test]
    fn zooms_around_the_anchor_and_stays_inside_the_image() {
        let close = |a: Rect, b: Rect| (a.min - b.min).length() + (a.max - b.max).length() < 1e-5;
        let half = Rect::from_min_max(pos2(0.25, 0.25), pos2(0.75, 0.75));
        assert!(close(zoomed(FULL, 2.0, pos2(0.5, 0.5)), half));
        let corner = Rect::from_min_max(Pos2::ZERO, pos2(0.5, 0.5));
        assert!(close(zoomed(FULL, 2.0, Pos2::ZERO), corner));
        // Clamped at both ends, and never outside the image.
        assert!(close(zoomed(half, 0.1, pos2(0.5, 0.5)), FULL));
        assert!((zoomed(FULL, 100.0, pos2(0.5, 0.5)).width() - 1.0 / MAX_ZOOM).abs() < 1e-6);
        assert!(close(
            clamped(half.translate(vec2(0.5, -0.5))),
            Rect::from_min_max(pos2(0.5, 0.0), pos2(1.0, 0.5))
        ));
    }

    #[test]
    fn rotates_and_mirrors() {
        // 0 1 2
        // 3 4 5
        let image = Image {
            width: 3,
            height: 2,
            temps: (0..6).map(|i| i as f32).collect(),
        };
        // 3 0
        // 4 1
        // 5 2
        let r = rotated(&image);
        assert_eq!((r.width, r.height), (2, 3));
        assert_eq!(r.temps, [3.0, 0.0, 4.0, 1.0, 5.0, 2.0]);
        assert_eq!(rotated(&rotated(&rotated(&r))), image);
        assert_eq!(mirrored(&image).temps, [2.0, 1.0, 0.0, 5.0, 4.0, 3.0]);
    }

    #[test]
    fn auto_range_ignores_outliers_and_keeps_a_minimum_span() {
        let mut temps = vec![20.0; 1000];
        temps[0] = -40.0;
        temps[1] = 500.0;
        assert_eq!(auto_range(&temps), [20.0, 20.0 + MIN_SPAN]);

        let ramp: Vec<f32> = (0..1000).map(|i| i as f32 / 10.0).collect();
        assert_eq!(auto_range(&ramp), [1.0, 98.9]);
    }
}
