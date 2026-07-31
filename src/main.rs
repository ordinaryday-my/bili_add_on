use std::{fs, process::exit, time::Instant};

use anyhow::{anyhow, Context};
use clap::Parser;
#[cfg(not(feature = "dhat-heap"))]
use mimalloc::MiMalloc;

use crate::{
    core::{same_specifications, video_process, EncoderPref},
    danmaku::{get_danmuku_xml_by_bili_id, get_danmuku_xml_from_file, parse_danmakus},
    decoder::VideoDecoder,
    interaction::Args,
};

mod audio;
mod core;
mod danmaku;
mod decoder;
mod hw;
mod interaction;
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
    let mut args = Args::parse();
    args.check().context("参数校验失败，请检查输入参数是否正确")?;
    args.check_output().context("生成默认输出路径失败（源文件名无效，无法自动拼接输出文件名）")?;
    let args = args;

    let xml = if let Some(id) = &args.source.bvid {
        get_danmuku_xml_by_bili_id(id)
            .with_context(|| format!("获取B站弹幕数据失败 (bvid: {id})"))?
    } else {
        let file = args.source.xml.as_ref().ok_or_else(|| {
            anyhow!("弹幕来源为空，请通过 --bvid 或 --xml 指定弹幕来源")
        })?;
        get_danmuku_xml_from_file(file)
            .with_context(|| format!("读取本地弹幕文件失败: {}", file.display()))?
    };
    let danmakus = parse_danmakus(xml).context("解析弹幕XML失败，请检查弹幕文件格式是否符合B站XML规范")?;
    if !args.quiet {
        eprintln!("已解析 {} 条弹幕", danmakus.len());
    }

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
        eprintln!("编解码器已就绪");
    }

    let output_path = args.output.clone().unwrap();

    let temp_file = tempfile::Builder::new()
        .prefix(".bili_add_on_")
        .suffix(".mp4")
        .tempfile_in(
            output_path
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )
        .context("无法在输出目录创建临时文件")?;
    let temp_path = temp_file.into_temp_path();

    let decoder = VideoDecoder::new(&args.input)
        .with_context(|| format!("视频解码器创建失败，无法解码源文件: {}", args.input.display()))?;

    let encoder_pref = match args.encoder.as_str() {
        "auto" => EncoderPref::Auto,
        "software" => EncoderPref::Software,
        name => match hw::HwCodec::from_cli(name) {
            Some(c) => EncoderPref::Specific(c),
            None => unreachable!("encoder 校验已保证值有效"),
        },
    };

    let (encoder, frame_duration) =
        same_specifications(&decoder, &temp_path, encoder_pref)
            .with_context(|| format!("视频编码器创建失败，无法写入临时文件: {}", temp_path.display()))?;

    if !args.quiet {
        eprintln!("正在渲染弹幕到视频帧...");
    }
    video_process(decoder, encoder, danmakus, &args, frame_duration)
        .context("视频处理流程失败（弹幕渲染到视频帧时出错）")?;

    if args.no_audio || !audio::has_audio(&args.input).unwrap_or(false) {
        fs::rename(&*temp_path, &output_path).with_context(|| {
            format!(
                "无法将临时视频移动到输出路径: {} -> {}",
                temp_path.display(),
                output_path.display()
            )
        })?;
    } else {
        if !args.quiet {
            eprintln!("正在合并音频轨道...");
        }
        audio::remux_audio(&temp_path, &args.input, &output_path)
            .context("音频混流失败")?;
    }

    if !args.quiet {
        eprintln!("输出文件: {}", output_path.display());
        eprintln!("完成，总用时 {:.1}s", (Instant::now() - start_time).as_secs_f32());
    }
    Ok(())
}

