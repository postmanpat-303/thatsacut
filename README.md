# thatsacut 🦀✂️

A blazing fast **Rust** video trimming & multi-cut tool — a rewrite of the
Python/Tkinter [Pastor Video Trimmer](https://github.com/olexandr-klymenko/VideoCutter)
with the same lossless stream-copy trimming, **plus multi-cut support**: cut
several ranges *out* of a video (or keep only the ones you want) and join
everything in one pass, with parallel segment extraction.

It drives **FFmpeg** under the hood and does **no re-encoding** by default, so
cuts are near-instant and quality loss is zero.

## Why Rust?

* **Single static binary** — no Python, no venv, no PyInstaller.
* **Instant startup** and minimal memory footprint.
* **Parallel segment extraction** — multi-cuts scale with your CPU cores.
* **One-pass re-encode mode** (`trim`/`concat` filter graph) for frame-accurate
  cuts when you need them.

## Install

Requires [Rust](https://rustup.rs/) 1.74+ and FFmpeg on your `PATH`
(or set `FFMPEG_BIN` to the ffmpeg binary).

```bash
git clone https://github.com/postmanpat-303/thatsacut.git
cd thatsacut
cargo install --path .
```

## Usage

```
vc info  <video>                                 # duration, resolution, codecs, size
vc trim  <video> --from 01:30 --to 02:45         # classic single trim (lossless)
vc keep  <video> --ranges "0-30, 01:00-01:30"    # keep only these ranges, joined
vc cut   <video> --remove "10-20, 01:00-01:15"   # cut these ranges OUT
vc frame <video> --at 01:30 -o thumb.jpg         # grab a preview frame
```

Timestamps are flexible: `90`, `90.5`, `01:30`, `00:01:30.500`, `1m30s`, `500ms`.

### Multi-cut examples

Remove two ad breaks from a recording, keeping everything else:

```bash
vc cut movie.mp4 --remove "05:10-06:00, 22:30-23:15" -o movie_clean.mp4
```

Build a highlights reel from the best moments:

```bash
vc keep game.mkv --ranges "12:00-12:40, 31:05-32:00, 58:10-59:00" -o highlights.mkv
```

Frame-accurate cutting (lossless mode snaps to keyframes; add `--reencode`
when precision matters):

```bash
vc cut movie.mp4 --remove "10-20" --reencode --crf 18 --preset slow
```

Limit parallelism during lossless multi-segment extraction:

```bash
vc keep big.mp4 --ranges "0-60, 120-180" --jobs 2
```

## How it works

| Mode | What happens |
| --- | --- |
| Single trim | `ffmpeg -ss <t> -t <d> -i in -c copy out` (identical to the original app) |
| Multi-cut (lossless) | each kept segment is stream-copied **in parallel** into a temp dir, then joined with FFmpeg's concat demuxer — still no re-encoding |
| `--reencode` | one FFmpeg pass using a `trim`/`atrim` + `concat` filter graph, encoded with x264/AAC |

## Development

```bash
cargo test          # unit tests (time parsing, cut planning)
cargo clippy        # lints
cargo build --release
```

## License

MIT. Inspired by [VideoCutter](https://github.com/olexandr-klymenko/VideoCutter)
by Olexandr Klymenko aka Pastor.
