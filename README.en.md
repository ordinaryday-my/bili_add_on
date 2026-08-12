# bili_add_on

[English](README.en.md) | 中文

A command-line tool to overlay Bilibili danmaku (bullet comments) onto videos. It can fetch danmaku XML automatically from a Bilibili video ID, or use a local danmaku XML file.

## Dependencies

- [ffmpeg](https://ffmpeg.org/) — required for video encoding/decoding. Make sure it is installed and available in PATH.

## Installation

```bash
cargo build --release
```

The binary is at `target/release/bili_add_on` (`bili_add_on.exe` on Windows).

## Usage

### Basic usage

```bash
# Fetch danmaku from a Bilibili video ID and overlay it onto a local video
bili_add_on --input input.mp4 --bvid BV1xxxxxxxxxx

# Use a local danmaku XML file
bili_add_on --input input.mp4 --xml danmaku.xml
```

Exactly one of `--bvid` and `--xml` is required.

### Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--input` | `-i` | **required** | Input video file path |
| `--output` | `-o` | `bili_add_on_<source name>` | Output video path |
| `--bvid` | | | Bilibili video ID (e.g. `BV1xxxxxxxxxx`), danmaku is fetched automatically |
| `--xml` | `-x` | | Path to a local danmaku XML file |
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
bili_add_on --input input.mp4 --bvid BV1xxxxxxxxxx --encoder software --x264-preset veryfast
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
bili_add_on --input input.mp4 --bvid BV1xxxxxxxxxx --top-ratio 0.0 --bottom-ratio 0.5
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
bili_add_on --input input.mp4 --bvid BV1xxxxxxxxxx \
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
bili_add_on --input input.mp4 --bvid BV1xxxxxxxxxx \
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
- All audio tracks of the input are preserved by default (stream copy, no quality loss)
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
