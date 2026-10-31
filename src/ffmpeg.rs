//! FFmpeg orchestration: lossless trims, multi-segment cuts and concatenation.

use crate::plan::Range;
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use which::which;

/// A located FFmpeg toolchain.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    pub bin: PathBuf,
    pub probe_bin: PathBuf,
}

fn exe_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

impl Ffmpeg {
    /// Locate `ffmpeg` and `ffprobe`.
    ///
    /// Search order:
    /// 1. `FFMPEG_BIN` environment variable (full path to the ffmpeg binary);
    /// 2. a `bin/` folder next to the executable;
    /// 3. the system `PATH`.
    pub fn locate() -> Result<Self> {
        if let Ok(custom) = std::env::var("FFMPEG_BIN") {
            let bin = PathBuf::from(&custom);
            anyhow::ensure!(
                bin.exists(),
                "FFMPEG_BIN points to a missing file: {custom}"
            );
            let probe_bin = Self::probe_for(&bin);
            return Ok(Self { bin, probe_bin });
        }

        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                for candidate in [
                    dir.join("bin").join(exe_name("ffmpeg")),
                    dir.join(exe_name("ffmpeg")),
                ] {
                    if candidate.exists() {
                        let probe_bin = Self::probe_for(&candidate);
                        return Ok(Self {
                            bin: candidate,
                            probe_bin,
                        });
                    }
                }
            }
        }

        let bin = which("ffmpeg").context(
            "ffmpeg was not found — install FFmpeg, bundle it in ./bin, or set FFMPEG_BIN",
        )?;
        let probe_bin = Self::probe_for(&bin);
        Ok(Self { bin, probe_bin })
    }

    fn probe_for(ffmpeg: &Path) -> PathBuf {
        let sibling = ffmpeg.with_file_name(exe_name("ffprobe"));
        if sibling.exists() {
            sibling
        } else {
            which("ffprobe").unwrap_or(sibling)
        }
    }

    /// Run ffmpeg with the given arguments, failing on a non-zero exit code.
    fn run(&self, args: &[String]) -> Result<()> {
        let status = Command::new(&self.bin)
            .args(args)
            .status()
            .with_context(|| format!("failed to launch {}", self.bin.display()))?;
        anyhow::ensure!(status.success(), "ffmpeg exited with {status}");
        Ok(())
    }

    /// Extract a single `[start, end)` segment.
    ///
    /// Stream-copy mode seeks with `-ss` *before* `-i` (fast keyframe seek),
    /// exactly like the original Python tool.
    pub fn extract_segment(
        &self,
        input: &Path,
        output: &Path,
        seg: &Range,
        opts: &CutOptions,
    ) -> Result<()> {
        let mut args = vec![
            "-y".into(),
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-ss".into(),
            format!("{:.3}", seg.start),
            "-t".into(),
            format!("{:.3}", seg.duration()),
            "-i".into(),
            input.to_string_lossy().into_owned(),
        ];
        args.extend(opts.codec_args());
        args.push(output.to_string_lossy().into_owned());
        self.run(&args)
    }

    /// Cut a video down to `segments` and write the joined result to `output`.
    ///
    /// * Lossless path: segments are extracted **in parallel** (stream copy,
    ///   keyframe seek) into a temp dir, then joined with the concat demuxer.
    /// * Re-encode path: a single ffmpeg pass with trim/concat filters, which
    ///   is frame accurate.
    pub fn cut(
        &self,
        input: &Path,
        output: &Path,
        segments: &[Range],
        opts: &CutOptions,
        jobs: usize,
    ) -> Result<()> {
        anyhow::ensure!(!segments.is_empty(), "cut plan is empty — nothing to keep");
        if segments.len() == 1 {
            return self.extract_segment(input, output, &segments[0], opts);
        }
        if opts.reencode {
            self.cut_reencode(input, output, segments, opts)
        } else {
            self.cut_lossless(input, output, segments, opts, jobs.max(1))
        }
    }

    fn cut_lossless(
        &self,
        input: &Path,
        output: &Path,
        segments: &[Range],
        opts: &CutOptions,
        jobs: usize,
    ) -> Result<()> {
        let ext = output.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        let tmp = tempfile::tempdir().context("could not create temp directory")?;

        // Intermediate segments are MPEG-TS regardless of the output format:
        // TS has no edit lists or global timestamps, which lets the concat
        // demuxer build a clean, gapless timeline on stream copy.
        let paths: Vec<PathBuf> = (0..segments.len())
            .map(|i| tmp.path().join(format!("seg_{i:04}.ts")))
            .collect();

        // Extract segments in parallel, `jobs` at a time.
        for (seg_chunk, path_chunk) in segments.chunks(jobs).zip(paths.chunks(jobs)) {
            std::thread::scope(|scope| -> Result<()> {
                let handles: Vec<_> = seg_chunk
                    .iter()
                    .zip(path_chunk)
                    .map(|(seg, path)| {
                        scope.spawn(move || self.extract_segment(input, path, seg, opts))
                    })
                    .collect();
                let mut first_err: Option<anyhow::Error> = None;
                for h in handles {
                    match h.join() {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                        }
                        Err(_) => {
                            if first_err.is_none() {
                                first_err = Some(anyhow::anyhow!("a worker thread panicked"));
                            }
                        }
                    }
                }
                match first_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            })?;
        }

        // Join with the concat demuxer (still stream copy).
        let list_path = tmp.path().join("concat.txt");
        let mut list = String::new();
        for p in &paths {
            let escaped = p
                .to_string_lossy()
                .replace('\\', "/")
                .replace('\'', "'\\''");
            let _ = writeln!(list, "file '{escaped}'");
        }
        fs::write(&list_path, list)?;

        let mut args = vec![
            "-y".into(),
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "concat".into(),
            "-safe".into(),
            "0".into(),
            "-i".into(),
            list_path.to_string_lossy().into_owned(),
            "-c".into(),
            "copy".into(),
        ];
        if matches!(ext, "mp4" | "m4v" | "mov") {
            args.push("-movflags".into());
            args.push("+faststart".into());
        }
        args.push(output.to_string_lossy().into_owned());
        self.run(&args)
    }

    fn cut_reencode(
        &self,
        input: &Path,
        output: &Path,
        segments: &[Range],
        opts: &CutOptions,
    ) -> Result<()> {
        let mut filter = String::new();
        for (i, seg) in segments.iter().enumerate() {
            let _ = write!(
                filter,
                "[0:v]trim=start={:.3}:end={:.3},setpts=PTS-STARTPTS[v{i}];",
                seg.start, seg.end
            );
            let _ = write!(
                filter,
                "[0:a]atrim=start={:.3}:end={:.3},asetpts=PTS-STARTPTS[a{i}];",
                seg.start, seg.end
            );
        }
        for i in 0..segments.len() {
            let _ = write!(filter, "[v{i}][a{i}]");
        }
        let _ = write!(filter, "concat=n={}:v=1:a=1[v][a]", segments.len());

        let mut args = vec![
            "-y".into(),
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-i".into(),
            input.to_string_lossy().into_owned(),
            "-filter_complex".into(),
            filter,
            "-map".into(),
            "[v]".into(),
            "-map".into(),
            "[a]".into(),
        ];
        args.extend(opts.codec_args());
        args.push(output.to_string_lossy().into_owned());
        self.run(&args)
    }

    /// Grab a single frame as a JPEG (preview / thumbnails).
    pub fn frame(&self, input: &Path, t: f64, output: &Path) -> Result<()> {
        let args = vec![
            "-y".into(),
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-ss".into(),
            format!("{:.3}", t),
            "-i".into(),
            input.to_string_lossy().into_owned(),
            "-vframes".into(),
            "1".into(),
            "-q:v".into(),
            "3".into(),
            output.to_string_lossy().into_owned(),
        ];
        self.run(&args)
    }
}

/// Options shared by every cut operation.
#[derive(Debug, Clone)]
pub struct CutOptions {
    /// Re-encode instead of stream-copy (frame accurate, much slower).
    pub reencode: bool,
    /// x264 CRF for the re-encode path.
    pub crf: u8,
    /// x264 preset for the re-encode path.
    pub preset: String,
}

impl Default for CutOptions {
    fn default() -> Self {
        Self {
            reencode: false,
            crf: 18,
            preset: "veryfast".into(),
        }
    }
}

impl CutOptions {
    fn codec_args(&self) -> Vec<String> {
        if self.reencode {
            vec![
                "-c:v".into(),
                "libx264".into(),
                "-preset".into(),
                self.preset.clone(),
                "-crf".into(),
                self.crf.to_string(),
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                "192k".into(),
            ]
        } else {
            vec![
                "-c".into(),
                "copy".into(),
                "-avoid_negative_ts".into(),
                "make_zero".into(),
            ]
        }
    }
}
