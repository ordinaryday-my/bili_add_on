use std::{
    fs, io,
    path::{Path, PathBuf},
    process::exit,
    time::Instant,
};

use anyhow::{Context, anyhow, bail};
#[cfg(not(feature = "dhat-heap"))]
use mimalloc::MiMalloc;
use ffmpeg_next as ffmpeg;

use crate::{
    core::video_process,
    danmaku::{
        filter_danmakus, get_danmaku_xml_by_bili_id, get_danmaku_xml_from_file, parse_danmakus,
    },
    decoder::VideoDecoder,
    encoder::{EncoderPref, same_specifications},
    interaction::{Args, DEVICE, STDIN, STDOUT},
};

mod audio;
mod core;
mod danmaku;
mod decoder;
mod encoder;
mod fonts;
mod hw;
mod i18n;
mod interaction;
mod layout;
mod utils;
mod web;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static ALLOC: MiMalloc = MiMalloc;

fn main() {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    env_logger::init();
    if let Err(e) = run() {
        eprintln!("{e:#}");
        exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let start_time = Instant::now();
    let (mut args, lang) = Args::parse_with_locale()?;
    args.check()
        .context("参数校验失败，请检查输入参数是否正确")?;
    args.check_output()
        .context("生成默认输出路径失败（源文件名无效，无法自动拼接输出文件名）")?;
    let args = args;

    let filters = args
        .parse_filters()
        .transpose()
        .context("--filter参数转换失败")?;

    let range = args
        .range
        .as_deref()
        .map(interaction::parse_time_range)
        .transpose()
        .context("--range 参数解析失败")?;

    let audio_range = args
        .audio_range
        .as_deref()
        .map(interaction::parse_time_range)
        .transpose()
        .context("--audio-range 参数解析失败")?;

    let xml = if let Some(id) = &args.source.bvid {
        get_danmaku_xml_by_bili_id(id)
            .with_context(|| format!("获取B站弹幕数据失败 (bvid: {id})"))?
    } else {
        let file = args
            .source
            .xml
            .as_ref()
            .ok_or_else(|| anyhow!("弹幕来源为空，请通过 --bvid 或 --xml 指定弹幕来源"))?;
        get_danmaku_xml_from_file(file)
            .with_context(|| format!("读取本地弹幕文件失败: {}", file.display()))?
    };
    let danmakus =
        parse_danmakus(xml).context("解析弹幕XML失败，请检查弹幕文件格式是否符合B站XML规范")?;
    if !args.quiet {
        eprintln!("{}", lang.t_fmt("parsed_danmakus", danmakus.len()));
    }

    let danmakus = if let Some(filters) = filters {
        let original_len = danmakus.len();
        let after = filter_danmakus(danmakus, &filters);
        eprintln!("{}", lang.t_fmt("filtered_out", original_len - after.len()));
        after
    } else {
        danmakus
    };

    ffmpeg_next::init()
        .map_err(|e| anyhow!("{e}"))
        .context("视频编解码器初始化失败，请确认 ffmpeg 已正确安装且版本兼容")?;
    #[cfg(not(feature = "ffmpeg-log"))]
    unsafe {
        ffmpeg_next::ffi::av_log_set_level(ffmpeg_next::ffi::AV_LOG_FATAL);
    }
    #[cfg(feature = "ffmpeg-log")]
    unsafe {
        ffmpeg_next::ffi::av_log_set_level(ffmpeg_next::ffi::AV_LOG_INFO);
    }
    if !args.quiet {
        eprintln!("{}", lang.t("codec_ready"));
    }

    let input_is_stdin = args.input == STDIN;
    let input_is_device = args.input == DEVICE;
    let (input_url, input_format): (String, Option<String>) = if input_is_device {
        let (url, fmt) = device_input_spec(args.capture.as_deref().unwrap())?;
        (url, Some(fmt))
    } else if input_is_stdin {
        ("pipe:0".to_string(), None)
    } else {
        (args.input.clone(), None)
    };

    let output_path: Option<PathBuf> = match args.output.as_deref() {
        Some(STDOUT) => None,
        Some(dest) => Some(PathBuf::from(dest)),
        None => unreachable!("check_output 已保证输出路径存在"),
    };
    let output_is_stdout = output_path.is_none();

    let temp_dir = match output_path.as_deref() {
        Some(p) => p
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf(),
        None => std::env::temp_dir(),
    };

    let temp_file = tempfile::Builder::new()
        .prefix(".bili_add_on_")
        .suffix(".mp4")
        .tempfile_in(&temp_dir)
        .context("无法在输出目录创建临时文件")?;
    let temp_path = temp_file.into_temp_path();

    let decoder = VideoDecoder::new_with_format(
        Path::new(&input_url),
        input_format.as_deref(),
        ffmpeg::Dictionary::new(),
        input_is_device.then_some(25.0),
        input_is_device,
    )
    .with_context(|| {
        format!(
            "视频解码器创建失败，无法解码源: {}",
            if input_is_device {
                args.capture.as_deref().unwrap_or_default()
            } else if input_is_stdin {
                "stdin (pipe:0)"
            } else {
                &args.input
            }
        )
    })?;

    if let Some((start, _)) = range {
        let frame_count = decoder.frame_count();
        let frame_rate = decoder.frame_rate();
        if frame_count > 0 && frame_rate > 0.0 {
            let video_duration = frame_count as f64 / frame_rate as f64;
            if start >= video_duration {
                bail!("--range 起始时间 ({start} 秒) 超出视频时长 ({video_duration:.3} 秒)");
            }
        } else if !args.quiet && !input_is_device {
            // 管道/流式输入（如 stdin）或部分容器无法预知时长，跳过校验。
            // 采集设备输入 --range 强制且必填结束时间，无需提示。
            eprintln!("{}", lang.t("range_unknown_duration"));
        }
    }

    let encoder_pref = match args.encoder.as_str() {
        "auto" => EncoderPref::Auto,
        "software" => EncoderPref::Software,
        name => match hw::HwCodec::from_cli(name) {
            Some(c) => EncoderPref::Specific(c),
            None => unreachable!("encoder 校验已保证值有效"),
        },
    };

    let (encoder, frame_duration) =
        same_specifications(&decoder, &temp_path, encoder_pref, &args.x264_preset).with_context(
            || {
                format!(
                    "视频编码器创建失败，无法写入临时文件: {}",
                    temp_path.display()
                )
            },
        )?;

    if !args.quiet {
        eprintln!("{}", lang.t("rendering"));
    }
    video_process(
        decoder,
        encoder,
        danmakus,
        &args,
        frame_duration,
        range,
        lang,
    )
    .context("视频处理流程失败（弹幕渲染到视频帧时出错）")?;

    // 音频源解析：--audio 指定文件 > 视频自带音频（文件输入）> 无（stdin/采集设备输入时警告）
    let audio_source: Option<PathBuf> = if args.no_audio {
        None
    } else if let Some(p) = &args.audio {
        if !audio::has_audio(p)
            .with_context(|| format!("无法检测音频源文件: {}", p.display()))?
        {
            bail!("音频源文件中没有音频流: {}", p.display());
        }
        Some(p.clone())
    } else if !input_is_stdin
        && !input_is_device
        && audio::has_audio(Path::new(&args.input)).unwrap_or(false)
    {
        Some(PathBuf::from(&args.input))
    } else {
        None
    };

    if audio_source.is_none() && !args.no_audio && !args.quiet {
        if input_is_stdin {
            eprintln!("{}", lang.t("stdin_audio_skipped"));
        } else if input_is_device {
            eprintln!("{}", lang.t("device_audio_skipped"));
        } else if args.audio_range.is_some() {
            eprintln!("{}", lang.t("audio_range_ignored"));
        }
    }

    // stdout 输出且需要混流时，先混入第二个临时文件再流式写出
    let second_temp = if output_is_stdout && audio_source.is_some() {
        Some(
            tempfile::Builder::new()
                .prefix(".bili_add_on_")
                .suffix(".mp4")
                .tempfile_in(&temp_dir)
                .context("无法创建音频混流临时文件")?
                .into_temp_path(),
        )
    } else {
        None
    };

    // 处理完成后得到最终视频文件位置
    let final_path: PathBuf = match (&output_path, &audio_source) {
        (Some(out), Some(src)) => {
            if !args.quiet {
                eprintln!("{}", lang.t("merging_audio"));
            }
            audio::remux_audio(&temp_path, src, out, audio_range, range).context("音频混流失败")?;
            out.clone()
        }
        (None, Some(src)) => {
            if !args.quiet {
                eprintln!("{}", lang.t("merging_audio"));
            }
            let target = second_temp.as_ref().unwrap();
            audio::remux_audio(&temp_path, src, target, audio_range, range)
                .context("音频混流失败")?;
            target.to_path_buf()
        }
        (Some(out), None) => {
            fs::rename(&*temp_path, out).with_context(|| {
                format!(
                    "无法将临时视频移动到输出路径: {} -> {}",
                    temp_path.display(),
                    out.display()
                )
            })?;
            out.clone()
        }
        (None, None) => temp_path.to_path_buf(),
    };

    if output_is_stdout {
        stream_to_stdout(&final_path)?;
        if !args.quiet {
            eprintln!("{}", lang.t("output_stdout"));
        }
    } else if !args.quiet {
        eprintln!("{}", lang.t_fmt("output_file", final_path.display()));
    }

    if !args.quiet {
        eprintln!(
            "{}",
            lang.t_fmt(
                "done_in",
                format!("{:.1}", (Instant::now() - start_time).as_secs_f32())
            )
        );
    }
    Ok(())
}

fn stream_to_stdout(path: &Path) -> anyhow::Result<()> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("无法打开临时文件以输出到标准输出: {}", path.display()))?;
    let mut stdout = io::stdout().lock();
    io::copy(&mut file, &mut stdout).context("写入标准输出失败")?;
    Ok(())
}

/// 解析 `--capture` 规格为 `(URL, 输入格式名)`。
///
/// `{格式}:{URL}` 直接解析（如 `dshow:video=USB Camera`、`gdigrab:desktop`）；
/// 裸名 `desktop`/`screen` 按平台映射默认屏幕捕获，其他裸名按平台映射默认摄像头。
fn device_input_spec(spec: &str) -> anyhow::Result<(String, String)> {
    if let Some((fmt, url)) = spec.split_once(':') {
        if fmt.is_empty() || url.is_empty() {
            bail!(
                "--capture 规格无效: '{spec}'（应为 {{格式}}:{{URL}}，如 dshow:video=USB Camera）"
            );
        }
        return Ok((url.to_string(), fmt.to_string()));
    }
    let os = std::env::consts::OS;
    match spec {
        "desktop" | "screen" => Ok(match os {
            "windows" => ("desktop".to_string(), "gdigrab".to_string()),
            "linux" => (":0.0".to_string(), "x11grab".to_string()),
            "macos" => ("1:none".to_string(), "avfoundation".to_string()),
            _ => bail!(
                "当前平台 ({os}) 不支持默认屏幕捕获，请显式指定 --capture {{格式}}:{{URL}}"
            ),
        }),
        name => Ok(match os {
            "windows" => (format!("video={name}"), "dshow".to_string()),
            "linux" => (name.to_string(), "v4l2".to_string()),
            "macos" => (name.to_string(), "avfoundation".to_string()),
            _ => bail!(
                "当前平台 ({os}) 不支持默认摄像头捕获，请显式指定 --capture {{格式}}:{{URL}}"
            ),
        }),
    }
}
