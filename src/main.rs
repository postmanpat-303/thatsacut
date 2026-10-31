//! `vc` — command line frontend for thatsacut.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use thatsacut::ffmpeg::{CutOptions, Ffmpeg};
use thatsacut::plan::{self, Range};
use thatsacut::probe;
use thatsacut::time;

#[derive(Parser)]
#[command(
    name = "vc",
    version,
    about = "Blazing fast lossless video trimming & multi-cut tool (FFmpeg frontend)"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show duration, resolution, codecs and size of a video.
    Info {
        /// Video file to inspect.
        input: PathBuf,
    },

    /// Extract a single segment between two timestamps.
    Trim {
        /// Video file to cut.
        input: PathBuf,
        /// Start time — e.g. 90, 01:30, 1m30s, 00:01:30.500
        #[arg(long)]
        from: String,
        /// End time (same formats as --from).
        #[arg(long)]
        to: String,
        /// Output file (default: <name>_trimmed.<ext>).
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        opts: Opts,
    },

    /// Keep ONLY the given ranges and join them into one file.
    ///
    /// Example: vc keep movie.mp4 --ranges "0-30, 01:00-01:30"
    Keep {
        /// Video file to cut.
        input: PathBuf,
        /// Comma-separated ranges to keep.
        #[arg(long)]
        ranges: String,
        /// Output file (default: <name>_keep.<ext>).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Parallel segment extractions (lossless mode).
        #[arg(short, long)]
        jobs: Option<usize>,
        #[command(flatten)]
        opts: Opts,
    },

    /// Cut the given ranges OUT of the video, keeping everything else.
    ///
    /// Example: vc cut movie.mp4 --remove "10-20, 01:00-01:15"
    Cut {
        /// Video file to cut.
        input: PathBuf,
        /// Comma-separated ranges to remove.
        #[arg(long)]
        remove: String,
        /// Output file (default: <name>_cut.<ext>).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Parallel segment extractions (lossless mode).
        #[arg(short, long)]
        jobs: Option<usize>,
        #[command(flatten)]
        opts: Opts,
    },

    /// Grab a single frame as a JPEG.
    Frame {
        /// Video file.
        input: PathBuf,
        /// Timestamp of the frame.
        #[arg(long)]
        at: String,
        /// Output image (default: <name>_<t>s.jpg).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Args, Clone)]
struct Opts {
    /// Re-encode for frame-accurate cuts (much slower than stream copy).
    #[arg(long)]
    reencode: bool,
    /// x264 CRF used with --reencode.
    #[arg(long, default_value_t = 18)]
    crf: u8,
    /// x264 preset used with --reencode.
    #[arg(long, default_value = "veryfast")]
    preset: String,
}

impl From<&Opts> for CutOptions {
    fn from(o: &Opts) -> Self {
        CutOptions {
            reencode: o.reencode,
            crf: o.crf,
            preset: o.preset.clone(),
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

fn default_jobs(jobs: Option<usize>) -> usize {
    jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    })
}

fn print_plan(segments: &[Range]) {
    println!("plan ({} segment{}):", segments.len(), if segments.len() == 1 { "" } else { "s" });
    let mut total = 0.0;
    for (i, s) in segments.iter().enumerate() {
        println!(
            "  {}. {} → {}  ({:.2}s)",
            i + 1,
            time::format_time(s.start),
            time::format_time(s.end),
            s.duration()
        );
        total += s.duration();
    }
    println!("  total kept: {} ({:.2}s)", time::format_time(total), total);
}

fn run_multi(
    ff: &Ffmpeg,
    input: &Path,
    ranges_raw: &str,
    remove: bool,
    output: Option<PathBuf>,
    jobs: Option<usize>,
    opts: &Opts,
    tag: &str,
) -> Result<()> {
    let info = probe::probe(&ff.probe_bin, input)?;
    let ranges = plan::parse_ranges(ranges_raw)?;
    let segments = plan::build_plan(info.duration, &ranges, remove)?;
    print_plan(&segments);

    let output = output.unwrap_or_else(|| default_output(input, tag));
    let cut_opts = CutOptions::from(opts);
    ff.cut(input, &output, &segments, &cut_opts, default_jobs(jobs))?;
    println!("wrote {}", output.display());
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let ff = Ffmpeg::locate()?;

    match cli.command {
        Cmd::Info { input } => {
            let info = probe::probe(&ff.probe_bin, &input)?;
            println!("file      : {}", input.display());
            println!(
                "duration  : {} ({:.3}s)",
                time::format_time(info.duration),
                info.duration
            );
            if let (Some(w), Some(h)) = (info.width, info.height) {
                println!("resolution: {w}x{h}");
            }
            if let Some(c) = &info.video_codec {
                println!("video     : {c}");
            }
            if let Some(c) = &info.audio_codec {
                println!("audio     : {c}");
            }
            if let Some(s) = info.size_bytes {
                println!("size      : {:.1} MB", s as f64 / (1024.0 * 1024.0));
            }
            if let Some(b) = info.bit_rate {
                println!("bitrate   : {:.2} Mbit/s", b as f64 / 1_000_000.0);
            }
        }

        Cmd::Trim {
            input,
            from,
            to,
            output,
            opts,
        } => {
            let start = time::parse_time(&from)?;
            let end = time::parse_time(&to)?;
            anyhow::ensure!(end > start, "--to must be later than --from");
            let output = output.unwrap_or_else(|| default_output(&input, "trimmed"));
            let seg = Range { start, end };
            print_plan(&[seg]);
            ff.cut(&input, &output, &[seg], &CutOptions::from(&opts), 1)?;
            println!("wrote {}", output.display());
        }

        Cmd::Keep {
            input,
            ranges,
            output,
            jobs,
            opts,
        } => {
            run_multi(&ff, &input, &ranges, false, output, jobs, &opts, "keep")?;
        }

        Cmd::Cut {
            input,
            remove,
            output,
            jobs,
            opts,
        } => {
            run_multi(&ff, &input, &remove, true, output, jobs, &opts, "cut")?;
        }

        Cmd::Frame { input, at, output } => {
            let t = time::parse_time(&at)?;
            let stem = input
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "frame".into());
            let output =
                output.unwrap_or_else(|| input.with_file_name(format!("{stem}_{t:.3}s.jpg")));
            ff.frame(&input, t, &output)?;
            println!("wrote {}", output.display());
        }
    }
    Ok(())
}
