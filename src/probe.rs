//! Lightweight media probing through `ffprobe` (JSON output).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

/// Metadata extracted from a media file.
#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub duration: f64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub size_bytes: Option<u64>,
    pub bit_rate: Option<u64>,
}

#[derive(Deserialize)]
struct RawStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Deserialize)]
struct RawFormat {
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Deserialize)]
struct RawProbe {
    streams: Option<Vec<RawStream>>,
    format: Option<RawFormat>,
}

/// Probe a media file with `ffprobe`.
pub fn probe(ffprobe: &Path, input: &Path) -> Result<ProbeInfo> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(input)
        .output()
        .with_context(|| format!("failed to run {}", ffprobe.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "ffprobe failed on {}: {}",
            input.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let raw: RawProbe =
        serde_json::from_slice(&output.stdout).context("could not parse ffprobe JSON output")?;

    let streams = raw.streams.unwrap_or_default();
    let video = streams.iter().find(|s| s.codec_type.as_deref() == Some("video"));
    let audio = streams.iter().find(|s| s.codec_type.as_deref() == Some("audio"));
    let format = raw.format.unwrap_or(RawFormat {
        duration: None,
        size: None,
        bit_rate: None,
    });

    Ok(ProbeInfo {
        duration: format
            .duration
            .as_deref()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0.0),
        width: video.and_then(|v| v.width),
        height: video.and_then(|v| v.height),
        video_codec: video.and_then(|v| v.codec_name.clone()),
        audio_codec: audio.and_then(|a| a.codec_name.clone()),
        size_bytes: format.size.as_deref().and_then(|s| s.parse().ok()),
        bit_rate: format.bit_rate.as_deref().and_then(|s| s.parse().ok()),
    })
}
