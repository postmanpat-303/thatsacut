//! `vc-gui` — graphical frontend for thatsacut.
//!
//! A native egui app exposing everything the `vc` CLI can do:
//! probing, single trims, multi-range keep/cut, frame grabs and the
//! re-encode options — with a visual timeline of the cut plan.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use thatsacut::ffmpeg::{CutOptions, Ffmpeg};
use thatsacut::plan::{self, Range};
use thatsacut::probe::{self, ProbeInfo};
use thatsacut::time;

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

#[derive(Clone)]
struct RangeRow {
    start: String,
    end: String,
}

/// Messages sent from the worker thread back to the UI.
enum Msg {
    Done(Result<PathBuf, String>),
}

struct GuiApp {
    ff: Option<Ffmpeg>,
    ff_error: Option<String>,

    input: Option<PathBuf>,
    info: Option<ProbeInfo>,
    probe_error: Option<String>,

    mode: Mode,
    ranges: Vec<RangeRow>,
    output: String,

    reencode: bool,
    crf: u8,
    preset: String,
    jobs: usize,

    frame_at: String,

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
        Self {
            ff,
            ff_error,
            input: None,
            info: None,
            probe_error: None,
            mode: Mode::Trim,
            ranges: vec![RangeRow {
                start: "0".into(),
                end: "10".into(),
            }],
            output: String::new(),
            reencode: false,
            crf: 18,
            preset: "veryfast".into(),
            jobs: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            frame_at: "0".into(),
            log: Vec::new(),
            busy: false,
            rx: None,
            last_output: None,
        }
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

    fn set_input(&mut self, path: PathBuf) {
        self.info = None;
        self.probe_error = None;
        self.last_output = None;
        if let Some(ff) = &self.ff {
            match probe::probe(&ff.probe_bin, &path) {
                Ok(info) => {
                    self.log(format!(
                        "probed {} — {} ({:.3}s)",
                        path.display(),
                        time::format_time(info.duration),
                        info.duration
                    ));
                    self.info = Some(info);
                }
                Err(e) => {
                    self.probe_error = Some(format!("{e:#}"));
                }
            }
        }
        self.output = default_output(&path, self.mode.tag())
            .to_string_lossy()
            .into_owned();
        self.input = Some(path);
    }

    /// Parse the range rows into concrete ranges, mirroring the CLI rules.
    fn parse_rows(&self) -> Result<Vec<Range>, String> {
        let mut out = Vec::new();
        for (i, row) in self.ranges.iter().enumerate() {
            let start = time::parse_time(row.start.trim())
                .map_err(|e| format!("range {} start: {e:#}", i + 1))?;
            let end = time::parse_time(row.end.trim())
                .map_err(|e| format!("range {} end: {e:#}", i + 1))?;
            if end <= start {
                return Err(format!("range {}: end must be later than start", i + 1));
            }
            out.push(Range { start, end });
        }
        Ok(out)
    }

    /// Build the final kept-segment plan for the current mode.
    fn build_plan(&self) -> Result<Vec<Range>, String> {
        let duration = self
            .info
            .as_ref()
            .map(|i| i.duration)
            .ok_or("load a video first")?;
        let rows = self.parse_rows()?;
        match self.mode {
            Mode::Trim => Ok(vec![rows[0]]),
            Mode::Keep => plan::build_plan(duration, &rows, false).map_err(|e| format!("{e:#}")),
            Mode::Cut => plan::build_plan(duration, &rows, true).map_err(|e| format!("{e:#}")),
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
            self.log("error: choose a video first");
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
            self.log("error: cut plan is empty — nothing to keep");
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
            "plan: {} segment(s), {:.2}s kept → {}",
            segments.len(),
            segments.iter().map(Range::duration).sum::<f64>(),
            output.display()
        ));
        self.log(if opts.reencode {
            format!(
                "re-encoding (libx264 crf={} preset={}) — this may take a while…",
                opts.crf, opts.preset
            )
        } else {
            "stream copy (lossless, keyframe snapped)…".to_string()
        });

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
            self.log("error: choose a video first");
            return;
        };
        let t = match time::parse_time(self.frame_at.trim()) {
            Ok(t) => t,
            Err(e) => {
                self.log(format!("error: frame time: {e:#}"));
                return;
            }
        };
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
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                pending.push(msg);
            }
        }
        for msg in pending {
            match msg {
                Msg::Done(Ok(path)) => {
                    self.log(format!("✔ wrote {}", path.display()));
                    self.last_output = Some(path);
                    self.busy = false;
                    self.rx = None;
                }
                Msg::Done(Err(e)) => {
                    self.log(format!("✖ failed: {e}"));
                    self.busy = false;
                    self.rx = None;
                }
            }
        }
        if self.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
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

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_worker(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(!self.busy, |ui| {
                ui.heading("thatsacut — video trimmer");
                ui.add_space(6.0);

                // ---------- FFmpeg status ----------
                match &self.ff {
                    Some(ff) => {
                        ui.label(
                            RichText::new(format!("ffmpeg: {}", ff.bin.display()))
                                .color(Color32::from_rgb(120, 200, 120))
                                .small(),
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
                }
                ui.add_space(4.0);

                // ---------- Input ----------
                ui.horizontal(|ui| {
                    if ui.button("Open video…").clicked() {
                        self.open_video();
                    }
                    match &self.input {
                        Some(p) => {
                            ui.label(p.display().to_string());
                        }
                        None => {
                            ui.label(RichText::new("no file selected").italics());
                        }
                    }
                });

                // ---------- Probe info ----------
                if let Some(info) = &self.info {
                    ui.add_space(4.0);
                    egui::Grid::new("info_grid")
                        .num_columns(4)
                        .spacing([16.0, 2.0])
                        .show(ui, |ui| {
                            ui.label(RichText::new("duration").strong());
                            ui.label(format!(
                                "{} ({:.3}s)",
                                time::format_time(info.duration),
                                info.duration
                            ));
                            ui.label(RichText::new("resolution").strong());
                            ui.label(match (info.width, info.height) {
                                (Some(w), Some(h)) => format!("{w}×{h}"),
                                _ => "—".into(),
                            });
                            ui.end_row();
                            ui.label(RichText::new("video").strong());
                            ui.label(info.video_codec.clone().unwrap_or("—".into()));
                            ui.label(RichText::new("audio").strong());
                            ui.label(info.audio_codec.clone().unwrap_or("—".into()));
                            ui.end_row();
                            ui.label(RichText::new("size").strong());
                            ui.label(match info.size_bytes {
                                Some(s) => format!("{:.1} MB", s as f64 / (1024.0 * 1024.0)),
                                None => "—".into(),
                            });
                            ui.label(RichText::new("bitrate").strong());
                            ui.label(match info.bit_rate {
                                Some(b) => format!("{:.2} Mbit/s", b as f64 / 1_000_000.0),
                                None => "—".into(),
                            });
                            ui.end_row();
                        });
                }
                if let Some(e) = &self.probe_error {
                    ui.label(RichText::new(format!("probe failed: {e}")).color(Color32::from_rgb(230, 120, 120)));
                }

                ui.add_space(8.0);
                ui.separator();

                // ---------- Mode ----------
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Mode").strong());
                    let old = self.mode;
                    ui.radio_value(&mut self.mode, Mode::Trim, "Trim (single range)");
                    ui.radio_value(&mut self.mode, Mode::Keep, "Keep ranges");
                    ui.radio_value(&mut self.mode, Mode::Cut, "Cut ranges out");
                    if self.mode != old {
                        if self.mode == Mode::Trim && self.ranges.is_empty() {
                            self.ranges.push(RangeRow {
                                start: "0".into(),
                                end: "10".into(),
                            });
                        }
                        if let Some(input) = &self.input {
                            self.output = default_output(input, self.mode.tag())
                                .to_string_lossy()
                                .into_owned();
                        }
                    }
                });

                // ---------- Ranges ----------
                let row_count = if self.mode == Mode::Trim {
                    1
                } else {
                    self.ranges.len()
                };
                let mut remove_idx = None;
                egui::Grid::new("ranges_grid")
                    .num_columns(4)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("start").small());
                        ui.label(RichText::new("end").small());
                        ui.label("");
                        ui.label(RichText::new("formats: 90 · 01:30 · 1m30s · 00:01:30.500").small().italics());
                        ui.end_row();
                        for i in 0..row_count {
                            let row = &mut self.ranges[i];
                            ui.add(egui::TextEdit::singleline(&mut row.start).desired_width(110.0));
                            ui.add(egui::TextEdit::singleline(&mut row.end).desired_width(110.0));
                            let parsed = time::parse_time(row.start.trim())
                                .and_then(|s| time::parse_time(row.end.trim()).map(|e| (s, e)));
                            match parsed {
                                Ok((s, e)) if e > s => {
                                    ui.label(RichText::new(format!(
                                        "{:.2}s → {:.2}s ({:.2}s)",
                                        s,
                                        e,
                                        e - s
                                    ))
                                    .small());
                                }
                                Ok(_) => {
                                    ui.label(RichText::new("end ≤ start").color(Color32::from_rgb(230, 160, 60)).small());
                                }
                                Err(_) => {
                                    ui.label(RichText::new("invalid time").color(Color32::from_rgb(230, 120, 120)).small());
                                }
                            }
                            if self.mode != Mode::Trim
                                && self.ranges.len() > 1
                                && ui.button("✕").clicked()
                            {
                                remove_idx = Some(i);
                            }
                            ui.end_row();
                        }
                    });
                if let Some(i) = remove_idx {
                    self.ranges.remove(i);
                }
                if self.mode != Mode::Trim && ui.button("+ Add range").clicked() {
                    self.ranges.push(RangeRow {
                        start: "0".into(),
                        end: "10".into(),
                    });
                }

                // ---------- Timeline ----------
                if let Some(info) = &self.info {
                    let duration = info.duration.max(f64::EPSILON);
                    match self.build_plan() {
                        Ok(segments) if !segments.is_empty() => {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 30.0),
                                egui::Sense::hover(),
                            );
                            let painter = ui.painter_at(rect);
                            painter.rect_filled(rect, 4.0, Color32::from_rgb(50, 30, 34));
                            for seg in &segments {
                                let x0 = rect.left() + (seg.start / duration) as f32 * rect.width();
                                let x1 = rect.left() + (seg.end.min(duration) / duration) as f32 * rect.width();
                                painter.rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(x0, rect.top()),
                                        egui::pos2(x1.max(x0 + 1.5), rect.bottom()),
                                    ),
                                    4.0,
                                    Color32::from_rgb(70, 160, 90),
                                );
                            }
                            painter.rect_stroke(
                                rect,
                                4.0,
                                egui::Stroke::new(1.0_f32, Color32::from_rgb(90, 90, 90)),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "timeline: green = kept · {} segment(s) · {:.2}s of {:.2}s kept",
                                    segments.len(),
                                    segments.iter().map(Range::duration).sum::<f64>(),
                                    duration
                                ))
                                .small(),
                            );
                        }
                        Ok(_) => {
                            ui.label(RichText::new("plan is empty — nothing would be kept").color(Color32::from_rgb(230, 160, 60)).small());
                        }
                        Err(e) => {
                            ui.label(RichText::new(e).color(Color32::from_rgb(230, 120, 120)).small());
                        }
                    }
                }

                ui.add_space(8.0);
                ui.separator();

                // ---------- Options ----------
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.reencode, "Re-encode (frame accurate, slower)");
                    ui.add_enabled_ui(self.reencode, |ui| {
                        ui.label("CRF");
                        ui.add(egui::Slider::new(&mut self.crf, 0..=51));
                        ui.label("Preset");
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

                // ---------- Output ----------
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Output").strong());
                    ui.add(
                        egui::TextEdit::singleline(&mut self.output)
                            .desired_width(ui.available_width() - 180.0),
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
                ui.horizontal(|ui| {
                    let run = ui.add(
                        egui::Button::new(RichText::new("▶ Run cut").strong())
                            .min_size(egui::vec2(110.0, 30.0)),
                    );
                    if run.clicked() {
                        self.start_job();
                    }
                    ui.separator();
                    ui.label("Frame at");
                    ui.add(egui::TextEdit::singleline(&mut self.frame_at).desired_width(90.0));
                    if ui.button("Grab JPEG").clicked() {
                        self.grab_frame();
                    }
                });
            });

            if self.busy {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("working…");
                });
            }
            if let Some(out) = &self.last_output {
                ui.label(
                    RichText::new(format!("last output: {}", out.display()))
                        .color(Color32::from_rgb(120, 200, 120)),
                );
            }

            // ---------- Log ----------
            ui.add_space(6.0);
            ui.separator();
            ui.label(RichText::new("Log").strong());
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.label(RichText::new(line).monospace().small());
                    }
                });
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("thatsacut")
            .with_inner_size([760.0, 640.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "thatsacut",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)))),
    )
}
