//! `vc-gui` — friendly graphical frontend for thatsacut.
//!
//! Drag a video in, scrub the filmstrip, drag the handles to pick what to
//! keep (or remove), hit Export. Everything the `vc` CLI can do — trims,
//! multi-range keep/cut, frame grabs, re-encode options — no timestamps
//! required.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use thatsacut::ffmpeg::{CutOptions, Ffmpeg};
use thatsacut::plan::{self, Range};
use thatsacut::probe::{self, ProbeInfo};

const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "mov", "avi", "webm", "m4v", "ts", "mpg", "mpeg", "wmv", "flv",
];
const PRESETS: &[&str] = &[
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
];
const THUMBS: usize = 10;
const STRIP_H: f32 = 64.0;
const BAR_H: f32 = 10.0;
const HANDLE_W: f32 = 7.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Trim,
    Keep,
    Cut,
}

impl Mode {
    fn tag(self) -> &'static str {
        match self {
            Mode::Trim => "trimmed",
            Mode::Keep => "keep",
            Mode::Cut => "cut",
        }
    }
}

#[derive(Clone, Copy)]
struct RangeRow {
    start: f64,
    end: f64,
}

/// Messages sent from worker threads back to the UI.
enum Msg {
    Thumb(usize, egui::ColorImage),
    Done(Result<PathBuf, String>),
}

struct GuiApp {
    ff: Option<Ffmpeg>,
    ff_error: Option<String>,

    input: Option<PathBuf>,
    info: Option<ProbeInfo>,
    probe_error: Option<String>,
    thumbs: Vec<Option<egui::TextureHandle>>,

    mode: Mode,
    ranges: Vec<RangeRow>,
    output: String,

    reencode: bool,
    crf: u8,
    preset: String,
    jobs: usize,

    snapshot_at: f64,

    log: Vec<String>,
    busy: bool,
    rx: Option<Receiver<Msg>>,
    last_output: Option<PathBuf>,
}

impl GuiApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (ff, ff_error) = match Ffmpeg::locate() {
            Ok(ff) => (Some(ff), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let mut app = Self {
            ff,
            ff_error,
            input: None,
            info: None,
            probe_error: None,
            thumbs: vec![None; THUMBS],
            mode: Mode::Trim,
            ranges: vec![],
            output: String::new(),
            reencode: false,
            crf: 18,
            preset: "veryfast".into(),
            jobs: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            snapshot_at: 0.0,
            log: Vec::new(),
            busy: false,
            rx: None,
            last_output: None,
        };
        // `vc-gui some/video.mp4` opens it straight away.
        if let Some(arg) = std::env::args_os().nth(1) {
            let p = PathBuf::from(arg);
            if p.is_file() {
                app.set_input(p);
            }
        }
        app
    }

    fn log(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
    }

    fn open_video(&mut self) {
        let file = rfd::FileDialog::new()
            .add_filter("Video files", VIDEO_EXTS)
            .add_filter("All files", &["*"])
            .pick_file();
        if let Some(path) = file {
            self.set_input(path);
        }
    }

    fn duration(&self) -> f64 {
        self.info.as_ref().map(|i| i.duration).unwrap_or(0.0)
    }

    fn reset_ranges(&mut self) {
        let d = self.duration();
        self.ranges = match self.mode {
            Mode::Trim | Mode::Keep => vec![RangeRow { start: 0.0, end: d }],
            Mode::Cut => vec![RangeRow {
                start: d * 0.25,
                end: d * 0.5,
            }],
        };
    }

    fn set_input(&mut self, path: PathBuf) {
        self.info = None;
        self.probe_error = None;
        self.last_output = None;
        self.thumbs = vec![None; THUMBS];
        let mut duration = 0.0;
        if let Some(ff) = &self.ff {
            match probe::probe(&ff.probe_bin, &path) {
                Ok(info) => {
                    duration = info.duration;
                    self.log(format!(
                        "opened {} — {} long",
                        path.display(),
                        fmt_secs(info.duration)
                    ));
                    self.info = Some(info);
                }
                Err(e) => {
                    self.probe_error = Some(format!(
                        "That file doesn't look like a readable video ({e:#})"
                    ));
                }
            }
        }
        self.output = default_output(&path, self.mode.tag())
            .to_string_lossy()
            .into_owned();
        self.input = Some(path);
        self.reset_ranges();
        self.spawn_thumbs(duration);
    }

    /// Grab filmstrip thumbnails in the background via ffmpeg.
    fn spawn_thumbs(&mut self, duration: f64) {
        if duration <= 0.0 {
            return;
        }
        let (Some(ff), Some(input)) = (self.ff.clone(), self.input.clone()) else {
            return;
        };
        let (tx, rx) = channel();
        // Merge with the shared receiver if a job is running; normally not.
        if self.rx.is_none() {
            self.rx = Some(rx);
        } else {
            // A cut is running — thumbnails are cosmetic, skip them.
            return;
        }
        std::thread::spawn(move || {
            let Ok(tmp) = tempfile::tempdir() else { return };
            for i in 0..THUMBS {
                let t = duration * (i as f64 + 0.5) / THUMBS as f64;
                let out = tmp.path().join(format!("t{i}.jpg"));
                let ok = Command::new(&ff.bin)
                    .args([
                        "-y",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-ss",
                        &format!("{t:.3}"),
                        "-i",
                        &input.to_string_lossy(),
                        "-vframes",
                        "1",
                        "-vf",
                        "scale=320:-2",
                    ])
                    .arg(&out)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&out) else {
                    continue;
                };
                let Ok(img) = image::load_from_memory(&bytes) else {
                    continue;
                };
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let _ = tx.send(Msg::Thumb(
                    i,
                    egui::ColorImage::from_rgba_unmultiplied(size, &rgba),
                ));
            }
        });
    }

    /// Final kept-segment plan for the current mode.
    fn build_plan(&self) -> Result<Vec<Range>, String> {
        if self.info.is_none() {
            return Err("Open a video first.".into());
        }
        for (i, r) in self.ranges.iter().enumerate() {
            if r.end <= r.start {
                return Err(format!(
                    "Part {} ends before it starts — drag the handles apart.",
                    i + 1
                ));
            }
        }
        let rows: Vec<Range> = self
            .ranges
            .iter()
            .map(|r| Range {
                start: r.start,
                end: r.end.min(self.duration()),
            })
            .collect();
        match self.mode {
            Mode::Trim => Ok(vec![rows[0]]),
            Mode::Keep => {
                plan::build_plan(self.duration(), &rows, false).map_err(|e| format!("{e:#}"))
            }
            Mode::Cut => {
                plan::build_plan(self.duration(), &rows, true).map_err(|e| format!("{e:#}"))
            }
        }
    }

    fn start_job(&mut self) {
        if self.busy {
            return;
        }
        let Some(ff) = self.ff.clone() else {
            self.log("error: ffmpeg is not available");
            return;
        };
        let Some(input) = self.input.clone() else {
            self.log("error: open a video first");
            return;
        };
        let segments = match self.build_plan() {
            Ok(s) => s,
            Err(e) => {
                self.log(format!("error: {e}"));
                return;
            }
        };
        if segments.is_empty() {
            self.log("error: nothing would be left — widen the green parts");
            return;
        }
        let mut output = PathBuf::from(self.output.trim());
        if output.as_os_str().is_empty() {
            output = default_output(&input, self.mode.tag());
        }

        let opts = CutOptions {
            reencode: self.reencode,
            crf: self.crf,
            preset: self.preset.clone(),
        };
        let jobs = self.jobs;

        self.log(format!(
            "exporting {} of video → {}",
            fmt_secs(segments.iter().map(Range::duration).sum()),
            output.display()
        ));
        if opts.reencode {
            self.log(format!(
                "re-encoding (crf={} preset={}) — this can take a while…",
                opts.crf, opts.preset
            ));
        }

        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        std::thread::spawn(move || {
            let result = ff
                .cut(&input, &output, &segments, &opts, jobs)
                .map(|()| output);
            let _ = tx.send(Msg::Done(result.map_err(|e| format!("{e:#}"))));
        });
    }

    fn grab_frame(&mut self) {
        if self.busy {
            return;
        }
        let Some(ff) = self.ff.clone() else { return };
        let Some(input) = self.input.clone() else {
            self.log("error: open a video first");
            return;
        };
        let t = self.snapshot_at.min(self.duration());
        let stem = input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "frame".into());
        let output = input.with_file_name(format!("{stem}_{t:.3}s.jpg"));
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        let out_clone = output.clone();
        std::thread::spawn(move || {
            let result = ff.frame(&input, t, &out_clone).map(|()| out_clone);
            let _ = tx.send(Msg::Done(result.map_err(|e| format!("{e:#}"))));
        });
    }

    fn drain_worker(&mut self, ctx: &egui::Context) {
        let mut pending = Vec::new();
        let mut channel_closed = false;
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                pending.push(msg);
            }
        }
        for msg in pending {
            match msg {
                Msg::Thumb(i, img) => {
                    if i < self.thumbs.len() {
                        self.thumbs[i] = Some(ctx.load_texture(
                            format!("thumb{i}"),
                            img,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
                Msg::Done(Ok(path)) => {
                    self.log(format!("✔ saved {}", path.display()));
                    self.last_output = Some(path);
                    self.busy = false;
                    channel_closed = true;
                }
                Msg::Done(Err(e)) => {
                    self.log(format!("✖ failed: {e}"));
                    self.busy = false;
                    channel_closed = true;
                }
            }
        }
        if channel_closed {
            self.rx = None;
        }
        // Keep repainting while thumbnails stream in or a job runs.
        let waiting_thumbs = self.info.is_some() && self.thumbs.iter().any(|t| t.is_none());
        if self.busy || waiting_thumbs {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
    }

    // ---------- UI pieces ----------

    fn ui_drop_zone(&mut self, ui: &mut egui::Ui) {
        let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let frame = egui::Frame::group(ui.style()).fill(if hovering {
            Color32::from_rgb(40, 70, 50)
        } else {
            ui.visuals().extreme_bg_color
        });
        frame.show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.label(RichText::new("🎬 Drop a video here").size(22.0).strong());
                ui.add_space(4.0);
                ui.label(RichText::new("or").weak());
                ui.add_space(4.0);
                if ui
                    .button(RichText::new("Choose a video…").size(16.0))
                    .clicked()
                {
                    self.open_video();
                }
                ui.add_space(24.0);
            });
        });
    }

    fn ui_video_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("📂 Change video").clicked() {
                self.open_video();
            }
            if let Some(p) = &self.input {
                ui.label(
                    RichText::new(p.file_name().unwrap_or_default().to_string_lossy()).strong(),
                );
            }
        });
        if let Some(info) = &self.info {
            let mut chips = vec![format!("⏱ {}", fmt_secs(info.duration))];
            if let (Some(w), Some(h)) = (info.width, info.height) {
                chips.push(format!("🖼 {w}×{h}"));
            }
            if let Some(c) = &info.video_codec {
                chips.push(c.clone());
            }
            if let Some(s) = info.size_bytes {
                chips.push(format!("{:.0} MB", s as f64 / (1024.0 * 1024.0)));
            }
            ui.label(RichText::new(chips.join("   ·   ")).small().weak());
        }
        if let Some(e) = &self.probe_error {
            ui.label(RichText::new(e).color(Color32::from_rgb(230, 120, 120)));
        }
    }

    fn ui_mode(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("What do you want to do?").strong());
        ui.horizontal(|ui| {
            let old = self.mode;
            for (mode, title, hint) in [
                (
                    Mode::Trim,
                    "✂️  Trim to one clip",
                    "keep one part of the video",
                ),
                (
                    Mode::Keep,
                    "⭐  Keep these parts",
                    "join the good bits together",
                ),
                (Mode::Cut, "🗑  Remove these parts", "cut out ads, mistakes…"),
            ] {
                let selected = self.mode == mode;
                let text = if selected {
                    RichText::new(title).strong()
                } else {
                    RichText::new(title)
                };
                if ui.selectable_label(selected, text).clicked() {
                    self.mode = mode;
                }
                ui.label(RichText::new(hint).small().weak());
                ui.add_space(8.0);
            }
            if self.mode != old && self.info.is_some() {
                self.reset_ranges();
                if let Some(input) = &self.input {
                    self.output = default_output(input, self.mode.tag())
                        .to_string_lossy()
                        .into_owned();
                }
            }
        });
    }

    /// Filmstrip + kept/removed bar + draggable handles.
    fn ui_timeline(&mut self, ui: &mut egui::Ui) {
        let duration = self.duration();
        if duration <= 0.0 {
            return;
        }
        let plan = self.build_plan().unwrap_or_default();
        let total_h = STRIP_H + BAR_H + HANDLE_W + 8.0;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), total_h),
            egui::Sense::hover(),
        );
        let strip = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), STRIP_H));
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left(), strip.bottom() + 2.0),
            egui::vec2(rect.width(), BAR_H),
        );
        let painter = ui.painter_at(rect);

        let x_of = |t: f64| rect.left() + (t / duration).clamp(0.0, 1.0) as f32 * rect.width();

        // Thumbnails, centre-cropped to their cell.
        let cell_w = rect.width() / THUMBS as f32;
        for i in 0..THUMBS {
            let cell = egui::Rect::from_min_size(
                egui::pos2(rect.left() + i as f32 * cell_w, strip.top()),
                egui::vec2(cell_w, STRIP_H),
            );
            painter.rect_filled(cell, 0.0, Color32::from_rgb(28, 28, 32));
            if let Some(tex) = &self.thumbs[i] {
                let img = tex.size_vec2();
                let ir = img.x / img.y.max(1.0);
                let cr = cell.width() / cell.height();
                let uv = if ir > cr {
                    let keep = cr / ir;
                    egui::Rect::from_min_max(
                        egui::pos2((1.0 - keep) / 2.0, 0.0),
                        egui::pos2(1.0 - (1.0 - keep) / 2.0, 1.0),
                    )
                } else {
                    let keep = ir / cr;
                    egui::Rect::from_min_max(
                        egui::pos2(0.0, (1.0 - keep) / 2.0),
                        egui::pos2(1.0, 1.0 - (1.0 - keep) / 2.0),
                    )
                };
                painter.image(tex.id(), cell, uv, Color32::WHITE);
            }
        }

        // Dim cells whose midpoint is outside the kept plan.
        if !plan.is_empty() {
            for i in 0..THUMBS {
                let mid = duration * (i as f64 + 0.5) / THUMBS as f64;
                let kept = plan.iter().any(|s| mid >= s.start && mid < s.end);
                if !kept {
                    let cell = egui::Rect::from_min_size(
                        egui::pos2(rect.left() + i as f32 * cell_w, strip.top()),
                        egui::vec2(cell_w, STRIP_H),
                    );
                    painter.rect_filled(cell, 0.0, Color32::from_black_alpha(170));
                }
            }
        }
        painter.rect_stroke(
            strip,
            0.0,
            egui::Stroke::new(1.0_f32, Color32::from_rgb(80, 80, 90)),
        );

        // Kept-segments bar.
        painter.rect_filled(bar, 3.0, Color32::from_rgb(64, 34, 38));
        for seg in &plan {
            let seg_rect = egui::Rect::from_min_max(
                egui::pos2(x_of(seg.start), bar.top()),
                egui::pos2(x_of(seg.end).max(x_of(seg.start) + 2.0), bar.bottom()),
            );
            painter.rect_filled(seg_rect, 3.0, Color32::from_rgb(80, 185, 105));
        }

        // Draggable handles at every range boundary.
        for i in 0..self.ranges.len() {
            for side in 0..2 {
                let t = if side == 0 {
                    self.ranges[i].start
                } else {
                    self.ranges[i].end
                };
                let x = x_of(t);
                let handle_rect = egui::Rect::from_min_max(
                    egui::pos2(x - HANDLE_W / 2.0, strip.top()),
                    egui::pos2(x + HANDLE_W / 2.0, bar.bottom() + HANDLE_W),
                );
                let id = ui.id().with(("handle", i, side));
                let response = ui.interact(handle_rect, id, egui::Sense::drag());
                let hovered = response.hovered() || response.dragged();
                let color = if hovered {
                    Color32::from_rgb(255, 210, 90)
                } else {
                    Color32::WHITE
                };
                painter.rect_filled(handle_rect, 2.0, color);
                if hovered {
                    let label = format!(
                        "{} part {} — {}",
                        if side == 0 { "start of" } else { "end of" },
                        i + 1,
                        fmt_secs(t)
                    );
                    response
                        .clone()
                        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
                        .on_hover_text(label);
                }
                if response.dragged() {
                    let dx = response.drag_delta().x;
                    let dt = dx as f64 / rect.width() as f64 * duration;
                    let row = &mut self.ranges[i];
                    if side == 0 {
                        row.start = (row.start + dt).clamp(0.0, row.end - 0.05);
                    } else {
                        row.end = (row.end + dt).clamp(row.start + 0.05, duration);
                    }
                }
            }
        }

        let kept_total: f64 = plan.iter().map(Range::duration).sum();
        ui.label(
            RichText::new(format!(
                "Exporting {} of {} — drag the white handles to adjust",
                fmt_secs(kept_total),
                fmt_secs(duration)
            ))
            .small()
            .weak(),
        );
    }

    fn ui_ranges(&mut self, ui: &mut egui::Ui) {
        let duration = self.duration();
        let rows = if self.mode == Mode::Trim {
            self.ranges.len().min(1)
        } else {
            self.ranges.len()
        };
        let mut remove_idx = None;
        for i in 0..rows {
            ui.horizontal(|ui| {
                let verb = match self.mode {
                    Mode::Trim => "Keep from",
                    Mode::Keep => "Part",
                    Mode::Cut => "Remove from",
                };
                if self.mode == Mode::Cut && i > 0 {
                    ui.label("and from");
                } else if self.mode == Mode::Keep {
                    ui.label(format!("{verb} {}", i + 1));
                } else {
                    ui.label(verb);
                }
                let row = &mut self.ranges[i];
                ui.add(
                    egui::DragValue::new(&mut row.start)
                        .speed(0.2)
                        .range(0.0..=(row.end - 0.05).max(0.0))
                        .custom_formatter(|v, _| fmt_secs(v)),
                );
                ui.label("to");
                ui.add(
                    egui::DragValue::new(&mut row.end)
                        .speed(0.2)
                        .range((row.start + 0.05).min(duration)..=duration)
                        .custom_formatter(|v, _| fmt_secs(v)),
                );
                ui.label(
                    RichText::new(format!("({})", fmt_secs(row.end - row.start)))
                        .small()
                        .weak(),
                );
                if self.mode != Mode::Trim && self.ranges.len() > 1 && ui.button("✕").clicked() {
                    remove_idx = Some(i);
                }
            });
        }
        if let Some(i) = remove_idx {
            self.ranges.remove(i);
        }
        if self.mode != Mode::Trim && ui.button("+ Add another part").clicked() {
            let d = self.duration();
            self.ranges.push(RangeRow {
                start: (d * 0.5).min(d),
                end: d,
            });
        }
    }

    fn ui_export(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Save as").strong());
            ui.add(
                egui::TextEdit::singleline(&mut self.output)
                    .desired_width((ui.available_width() - 90.0).max(120.0)),
            );
            if ui.button("Browse…").clicked() {
                let mut dlg = rfd::FileDialog::new()
                    .add_filter("Video files", VIDEO_EXTS)
                    .add_filter("All files", &["*"]);
                if !self.output.is_empty() {
                    let p = PathBuf::from(&self.output);
                    if let Some(dir) = p.parent() {
                        dlg = dlg.set_directory(dir);
                    }
                    if let Some(name) = p.file_name() {
                        dlg = dlg.set_file_name(name.to_string_lossy());
                    }
                }
                if let Some(path) = dlg.save_file() {
                    self.output = path.to_string_lossy().into_owned();
                }
            }
        });

        ui.add_space(6.0);
        let valid = self.build_plan().map(|p| !p.is_empty()).unwrap_or(false);
        ui.horizontal(|ui| {
            let button = ui.add_enabled(
                valid,
                egui::Button::new(RichText::new("🚀  Export video").size(17.0).strong())
                    .min_size(egui::vec2(170.0, 38.0)),
            );
            if button.clicked() {
                self.start_job();
            }
            if let Err(e) = self.build_plan() {
                ui.label(
                    RichText::new(e)
                        .color(Color32::from_rgb(230, 160, 60))
                        .small(),
                );
            } else if !self.reencode {
                ui.label(
                    RichText::new(
                        "Tip: lossless mode snaps to the nearest keyframe — \
                         turn on re-encoding in Advanced for frame-exact cuts.",
                    )
                    .small()
                    .weak(),
                );
            }
        });
    }

    fn ui_success(&mut self, ui: &mut egui::Ui) {
        let Some(out) = self.last_output.clone() else {
            return;
        };
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(30, 55, 38))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("✔ Done!")
                            .strong()
                            .color(Color32::from_rgb(130, 220, 150)),
                    );
                    ui.label(out.display().to_string());
                    if ui.button("Show in folder").clicked() {
                        reveal_in_folder(&out);
                    }
                    if ui.button("▶ Play").clicked() {
                        open_file(&out);
                    }
                });
            });
    }

    fn ui_advanced(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("⚙ Advanced options").small())
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.reencode, "Re-encode (frame-accurate, slower)");
                    ui.add_enabled_ui(self.reencode, |ui| {
                        ui.label("Quality (CRF)");
                        ui.add(egui::Slider::new(&mut self.crf, 0..=51));
                        ui.label("Speed preset");
                        egui::ComboBox::from_id_salt("preset")
                            .selected_text(&self.preset)
                            .show_ui(ui, |ui| {
                                for p in PRESETS {
                                    ui.selectable_value(&mut self.preset, (*p).to_string(), *p);
                                }
                            });
                    });
                });
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!self.reencode, |ui| {
                        ui.label("Parallel jobs (lossless multi-cut)");
                        ui.add(egui::Slider::new(&mut self.jobs, 1..=16));
                    });
                });
                ui.horizontal(|ui| {
                    ui.label("📷 Save a snapshot at");
                    let duration = self.duration();
                    ui.add(
                        egui::DragValue::new(&mut self.snapshot_at)
                            .speed(0.2)
                            .range(0.0..=duration)
                            .custom_formatter(|v, _| fmt_secs(v)),
                    );
                    if ui.button("Save JPEG").clicked() {
                        self.grab_frame();
                    }
                });
                match &self.ff {
                    Some(ff) => {
                        ui.label(
                            RichText::new(format!("ffmpeg: {}", ff.bin.display()))
                                .small()
                                .weak(),
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new(format!(
                                "ffmpeg not found — {}",
                                self.ff_error.as_deref().unwrap_or("")
                            ))
                            .color(Color32::from_rgb(230, 120, 120))
                            .small(),
                        );
                        if ui.button("Locate ffmpeg…").clicked() {
                            self.locate_ffmpeg();
                        }
                    }
                }
            });
    }

    fn locate_ffmpeg(&mut self) {
        if let Some(bin) = rfd::FileDialog::new()
            .add_filter("ffmpeg", &["exe"])
            .pick_file()
        {
            let sibling = bin.with_file_name(if cfg!(windows) {
                "ffprobe.exe"
            } else {
                "ffprobe"
            });
            let probe_bin = if sibling.exists() {
                sibling
            } else {
                which::which("ffprobe").unwrap_or(sibling)
            };
            self.ff = Some(Ffmpeg {
                bin: bin.clone(),
                probe_bin,
            });
            self.ff_error = None;
            self.log(format!("using ffmpeg at {}", bin.display()));
            if let Some(input) = self.input.clone() {
                self.set_input(input);
            }
        }
    }
}

fn default_output(input: &Path, tag: &str) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".into());
    let ext = input
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mp4".into());
    input.with_file_name(format!("{stem}_{tag}.{ext}"))
}

/// `1:05.0`, `0:12.5`, `1:02:03.4`
fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0);
    let h = (s / 3600.0) as u64;
    let m = ((s % 3600.0) / 60.0) as u64;
    let sec = s % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{sec:04.1}")
    } else {
        format!("{m}:{sec:04.1}")
    }
}

fn reveal_in_folder(path: &Path) {
    if cfg!(target_os = "windows") {
        let _ = Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    } else if cfg!(target_os = "macos") {
        let _ = Command::new("open").arg("-R").arg(path).spawn();
    } else if let Some(parent) = path.parent() {
        let _ = Command::new("xdg-open").arg(parent).spawn();
    }
}

fn open_file(path: &Path) {
    if cfg!(target_os = "windows") {
        let _ = Command::new("cmd")
            .args(["/C", "start", "", &path.to_string_lossy()])
            .spawn();
    } else if cfg!(target_os = "macos") {
        let _ = Command::new("open").arg(path).spawn();
    } else {
        let _ = Command::new("xdg-open").arg(path).spawn();
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_worker(ctx);

        // Drag-and-drop a video anywhere onto the window.
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if let Some(f) = dropped.into_iter().next() {
            if let Some(path) = f.path {
                self.set_input(path);
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_enabled_ui(!self.busy, |ui| {
                    ui.add_space(4.0);

                    if self.ff.is_none() {
                        egui::Frame::group(ui.style())
                            .fill(Color32::from_rgb(70, 40, 40))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("⚠ FFmpeg wasn't found on this computer.");
                                    if ui.button("Find it manually…").clicked() {
                                        self.locate_ffmpeg();
                                    }
                                });
                            });
                        ui.add_space(6.0);
                    }

                    if self.input.is_none() {
                        self.ui_drop_zone(ui);
                    } else {
                        self.ui_video_header(ui);
                        ui.add_space(8.0);
                        self.ui_mode(ui);
                        ui.add_space(8.0);
                        self.ui_timeline(ui);
                        ui.add_space(4.0);
                        self.ui_ranges(ui);
                        ui.add_space(10.0);
                        ui.separator();
                        self.ui_export(ui);
                    }
                    ui.add_space(6.0);
                    self.ui_advanced(ui);
                });

                if self.busy {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Working on it…");
                    });
                }
                self.ui_success(ui);

                if !self.log.is_empty() {
                    ui.add_space(6.0);
                    egui::CollapsingHeader::new(RichText::new("Activity log").small())
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(160.0)
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    for line in &self.log {
                                        ui.label(RichText::new(line).monospace().small());
                                    }
                                });
                        });
                }
            });
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("thatsacut")
            .with_inner_size([820.0, 680.0])
            .with_min_inner_size([660.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "thatsacut",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)))),
    )
}
