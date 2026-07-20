use anyhow::{anyhow, Context};
use clap::Parser;

use std::{process::exit, time::Instant};

use crate::{
    core::{same_specifications, video_process}, danmaku::{get_danmuku_xml_by_bili_id, get_danmuku_xml_from_file, parse_danmakus}, interaction::Args,
};

mod core;
mod danmaku;
mod interaction;
mod utils;
mod web;

fn main() {
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
        get_danmuku_xml_by_bili_id(id)?
    } else {
        let file = args.source.xml.as_ref().ok_or_else(|| {
            anyhow!("弹幕来源为空，请通过 --bvid 或 --xml 指定弹幕来源")
        })?;
        get_danmuku_xml_from_file(file)?
    };
    let danmakus = parse_danmakus(xml).context("解析弹幕XML失败，请检查弹幕文件格式是否符合B站XML规范")?;

    video_rs::init()
        .map_err(|e| anyhow!("{e}"))
        .context("视频编解码器初始化失败，请确认 ffmpeg 已正确安装且版本兼容")?;

    let decoder = video_rs::Decoder::new(args.input.clone())
        .with_context(|| format!("视频解码器创建失败，无法解码源文件: {}", args.input.display()))?;
    let output_path = args.output.clone().unwrap();
    let (encoder, frame_duration) =
        same_specifications(&decoder, &output_path)
            .with_context(|| format!("视频编码器创建失败，无法写入输出文件: {}", output_path.display()))?;

    video_process(decoder, encoder, danmakus, &args, frame_duration)?;

    eprintln!("用时: {}s", (Instant::now() - start_time).as_secs_f32());
    Ok(())
}

