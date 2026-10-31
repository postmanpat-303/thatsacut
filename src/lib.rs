//! thatsacut — a fast, lossless video trimming and multi-cut library.
//!
//! A Rust rewrite of the Python/Tkinter "Pastor Video Trimmer".
//! It orchestrates FFmpeg the same way (stream copy, no re-encoding)
//! and adds multi-cut support on top: keep or remove several ranges
//! and join the result in one pass.

pub mod ffmpeg;
pub mod plan;
pub mod probe;
pub mod time;
