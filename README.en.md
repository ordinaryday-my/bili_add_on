# bili_add_on

[English](README.en.md) | 中文

A command-line tool to overlay Bilibili danmaku (bullet comments) onto videos. It can fetch danmaku XML automatically from a Bilibili video ID, or use a local danmaku XML file.

## Dependencies

- [ffmpeg](https://ffmpeg.org/) — required for video encoding/decoding. Make sure it is installed and available in PATH.

> The `-with-ffmpeg` release artifacts from [GitHub Releases](https://github.com/ordinaryday-my/bili_add_on/releases) bundle the FFmpeg shared libraries, so **no separate FFmpeg installation is needed**.

## Installation

### Download from GitHub Releases

Each release ships two kinds of artifacts:

| Artifact | Description |
|----------|-------------|
| `bili_add_on-<platform>` | Standalone binary; requires FFmpeg installed and on PATH |
| `bili_add_on-<platform>-with-ffmpeg.zip` | Bundles the FFmpeg shared libraries (DLLs on Windows, `libs/` directory with .so/.dylib on Linux/macOS); run directly after extraction |

### Build from source

```bash
cargo build --release
```

The binary is at `target/release/bili_add_on` (`bili_add_on.exe` on Windows).

## Usage

The CLI is organized into subcommands: `overlay` (local file / stdin), `capture` (capture device input), and `list-devices`. `overlay` and `capture` share all rendering options.

### overlay: basic usage

```bash
# Fetch danmaku from a Bilibili video ID and overlay it onto a local video
bili_add_on overlay --input input.mp4 --bvid BV1xxxxxxxxxx

# Use a local danmaku XML file
bili_add_on overlay --input input.mp4 --xml danmaku.xml
```

Exactly one of `--bvid` and `--xml` is required.

### overlay: stdin / stdout

`--input :STDIN:` reads the video from stdin; `--output :STDOUT:` writes the result to stdout (progress and logs always go to stderr). Either can be used alone, or combined to form a pipeline:

```bash
# Produce a streamable fMP4, process it through a pipe, and write to stdout
ffmpeg -i input.mp4 -c copy -movflags frag_keyframe+empty_moov -f mp4 pipe:1 \
    | bili_add_on overlay --input :STDIN: --output :STDOUT: --bvid BV1xxxxxxxxxx > output.mp4
```

Limitations and notes:

- stdin input must be a **streamable** container (e.g. fMP4 / MPEG-TS / WebM). A regular MP4 (moov at the end of the file) cannot be parsed from a pipe; convert it to faststart/fMP4 first.
- The video's built-in audio cannot be remuxed from stdin (the stream is consumed by the decoder); a warning is printed and the output is video-only. Pass `--audio` to supply an external audio file instead.
- stdin input cannot be seeked; `--range` with a start time > 0 is reached by dropping frames. When the duration is unknown, the out-of-range validation for the `--range` start time is skipped.
- With stdin input, `--output` is required (no default filename can be generated).
- Output to stdout is written after processing completes (temp file first), not streamed incrementally.

### capture: capture device input (camera / screen capture)

`capture` grabs from a device in real time. Use `--capture` to specify the device spec (`{format}:{URL}`, matching ffmpeg CLI's `-f {format} -i {URL}`) and always provide the `--range` end time:

```bash
# Windows: camera (dshow)
bili_add_on capture --capture "dshow:video=USB Camera" \
    --output out.mp4 --bvid BV1xxxxxxxxxx --range 30

# Screen capture on Windows / Linux / macOS
bili_add_on capture --capture gdigrab:desktop --output out.mp4 --xml danmaku.xml --range 30
bili_add_on capture --capture x11grab::0.0   --output out.mp4 --xml danmaku.xml --range 30
bili_add_on capture --capture "avfoundation:1:none" --output out.mp4 --xml danmaku.xml --range 30

# A bare desktop/screen uses the platform default screen capture
# (Windows→gdigrab, Linux→x11grab, macOS→avfoundation)
bili_add_on capture --capture desktop --output out.mp4 --xml danmaku.xml --range 30
```

Limitations and notes:

- A capture source has no end, so `--range` is **required** and **end-only** (e.g. `--range 30`, `--range 1:23`); a start time (`5-30`) is rejected at the CLI level.
- Camera/mic audio cannot be remuxed from a capture device (the live stream cannot be re-read); a warning is printed and the output is video-only. Pass `--audio` to supply an external audio file.
- The first frame's timestamp is normalized to 0 so the danmaku timeline aligns with the capture start.
- The frame rate is determined by the device; if unavailable, 25 fps is assumed.
- The FFmpeg build must include the required capture format (dshow / gdigrab / v4l2 / x11grab / avfoundation).

### list-devices: list capture devices

```bash
# List available devices (cameras, mics, screens) for dshow / avfoundation
bili_add_on list-devices dshow
bili_add_on list-devices avfoundation

# gdigrab / v4l2 have no device list; usage hints are printed instead
bili_add_on list-devices gdigrab
```

When opening a capture device fails (wrong device name, device busy, etc.), the tool automatically prints the device list of the corresponding format to help troubleshooting.

### External audio source and audio trimming

`--audio` overrides the video's built-in audio with an external file; `--audio-range` first trims the audio on the audio-source timeline, aligns the trimmed start with the video start, then cuts it together with the video by `--range`:

```bash
# Example: audio source A=[0:00:00-0:30:00]; --audio-range 5-10 trims B=[0:00:05-0:00:10]
#          aligned to the video start; --range 3 then takes the first 3 s of B
#          → output audio = A[5s, 8s)
bili_add_on overlay --input input.mp4 --xml danmaku.xml \
    --audio audio.m4a --audio-range 5-10 --range 3
```

Full example with stdin input and an external audio source:

```bash
ffmpeg -i input.mp4 -c copy -movflags frag_keyframe+empty_moov -f mp4 pipe:1 \
    | bili_add_on overlay --input :STDIN: --output out.mp4 --xml danmaku.xml \
        --audio audio.m4a --audio-range 5-10
```

### Options

**overlay-specific**

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--input` | `-i` | **required** | Input video file path, or `:STDIN:` to read from stdin |
| `--output` | `-o` | `bili_add_on_<source name>` | Output video path, or `:STDOUT:` to write to stdout |
| `--range` | | | Video time range: `{start}-{end}` or `{end}`, e.g. `1:23-5:00`, `162:12` |

**capture-specific**

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--capture` | | **required** | Capture device spec: `{format}:{URL}`, e.g. `dshow:video=USB Camera`, `gdigrab:desktop`, `v4l2:/dev/video0`, `avfoundation:0:none`; or a bare `desktop`/`screen` for the platform default screen capture |
| `--range` | | **required** | Recording duration (end time only, e.g. `30`, `1:23`); a start time is not allowed |
| `--output` | `-o` | **required** | Output video path, or `:STDOUT:` to write to stdout (no default name can be generated) |

**shared rendering options (overlay / capture)**

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--bvid` | | | Bilibili video ID (e.g. `BV1xxxxxxxxxx`), danmaku is fetched automatically |
| `--xml` | `-x` | | Path to a local danmaku XML file |
| `--audio` | | | Path to an audio source file, overriding the video's built-in audio (useful with stdin or capture device input) |
| `--audio-range` | | | Audio trim range: `{start}-{end}` or `{end}`; audio is trimmed and aligned to the video start, then cut together with the video by `--range` |
| `--opacity` | | `0.93` | Danmaku opacity, in range 0~1 |
| `--top-ratio` | `-t` | `0.0` | Top edge of the danmaku area as a fraction of video height; 0 = top |
| `--bottom-ratio` | `-b` | `1.0` | Bottom edge of the danmaku area as a fraction of video height; 1 = bottom |
| `--font-scale` | | `1.0` | Danmaku font size scale |
| `--font` | | | User font file path (ttf/otf/ttc); may be repeated, used in the given order |
| `--system-fonts` | | `false` | Enable system fonts as fallback |
| `--speed` | `-s` | `3` | Danmaku scroll speed (pixels per frame) |
| `--line-spacing` | | `4` | Danmaku line spacing (pixels) |
| `--fixed-duration` | | `5.0` | Fixed danmaku (top/bottom) duration (seconds) |
| `--no-audio` | | `false` | Do not keep the input video's audio track (video-only output) |
| `--quiet` | `-q` | `false` | Quiet mode; suppress progress output |
| `--encoder` | | `auto` | Video encoder: `auto` / `nvenc` / `amf` / `qsv` / `software` |
| `--x264-preset` | | `medium` | libx264 encoding preset (software encoding only) |
| `--longest` | | false | Extend the output video to display all danmaku |
| `--lang` | | `auto` | Output language: `zh` / `en` / `auto` (auto follows system locale) |

### Hardware-accelerated encoding

Three major GPU hardware encoders are supported:

| Option | Encoder | Platform |
|--------|---------|----------|
| `auto` | Automatically picks the best available encoder (hardware preferred) | All |
| `nvenc` | NVIDIA NVENC | NVIDIA GPU |
| `amf` | AMD AMF | AMD GPU |
| `qsv` | Intel QuickSync | Intel GPU |
| `software` | libx264 (pure software encoding) | All |

If the requested hardware encoder is unavailable, the tool falls back to `software`.

Software encoding uses libx264 with the default preset `medium`. Use a faster preset (e.g. `veryfast`, typically several times faster at the cost of slightly lower compression) when encoding speed matters:

```bash
bili_add_on overlay --input input.mp4 --bvid BV1xxxxxxxxxx --encoder software --x264-preset veryfast
```

### Danmaku area control

```
┌─────────────────────┐  ← top (top-ratio=0.0)
│                     │
│   danmaku area      │
│                     │
└─────────────────────┘  ← bottom (bottom-ratio=1.0)
```

Adjust `--top-ratio` and `--bottom-ratio` to control the vertical range of the danmaku area. For example, restrict danmaku to the upper half of the frame:

```bash
bili_add_on overlay --input input.mp4 --bvid BV1xxxxxxxxxx --top-ratio 0.0 --bottom-ratio 0.5
```

## Fonts and character coverage

Text rendering is powered by cosmic-text with glyph-level font fallback: when a character is missing, the font chain is searched automatically.

### Priority

| `--font` | `--system-fonts` | Fallback order |
|----------|------------------|----------------|
| not provided | off (default) | bundled fonts |
| provided | off | user fonts → bundled fonts |
| provided | on | user fonts → system fonts → bundled fonts |
| not provided | on | system fonts → bundled fonts |

Multiple `--font` options are used in the given order:

```bash
bili_add_on overlay --input input.mp4 --bvid BV1xxxxxxxxxx \
    --font noto-emoji.ttf --font noto-symbols.ttf
```

### Bundled fonts

| Font | Coverage |
|------|----------|
| Source Han Sans SC | CJK, Latin and other major scripts |
| Noto Sans Symbols 2 | Currency, math and miscellaneous symbols (e.g. ☑) |
| Noto Sans Symbols (9 weights) | Arrows and dingbats (e.g. ✟✝) |

Bundled fonts use the OpenType full-Unicode cmap subtable (format 12), sharing the same fallback logic as system fonts.

### Recommendations

- **Symbols (✟ etc.)**: on Windows, enable `--system-fonts` so Segoe UI Symbol covers them; on other platforms pass Noto Sans Symbols via `--font`
- **Emoji**: pass Noto Color Emoji via `--font` (system fonts such as Segoe UI Emoji also work with `--system-fonts`)
- Combine fonts to cover multiple gaps:

```bash
bili_add_on overlay --input input.mp4 --bvid BV1xxxxxxxxxx \
    --font NotoSansSymbols-Regular.ttf --font NotoColorEmoji.ttf
```

## Supported danmaku modes

| Mode | ID | Description |
|------|----|-------------|
| Scroll | 1/2/3 | Scrolls from right to left |
| Bottom | 4 | Fixed at the bottom |
| Top | 5 | Fixed at the top |
| Reverse | 6 | Scrolls from left to right |
| BAS | 9 | Special bottom danmaku |

Advanced danmaku (mode 7) is parsed but not rendered.

## Output format

- Video encoding: H.264 YUV420P (NVENC / AMF / QSV hardware acceleration supported)
- All audio tracks of the input are preserved by default (stream copy, no quality loss); `--audio` can override them with an external audio source
- Default output naming: `bili_add_on_<source file name>`
- Use `--no-audio` for a video-only output

## Architecture overview

The tool uses a three-stage pipeline with lock-free channels for concurrency:

1. **Decode thread** — decodes the source video frame by frame into RGB images
2. **Render thread** — overlays danmaku onto RGB frames (scheduling, collision detection, pixel blending)
3. **Encode thread** — feeds the rendered frames into the encoder, writing to a temporary video file

After processing, audio tracks are muxed from the source into the output file (skipped with `--no-audio`).

## License

MIT
