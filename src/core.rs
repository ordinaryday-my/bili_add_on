use std::{
    collections::HashSet,
    path::Path,
    time::Duration,
};

use ab_glyph::{FontVec, PxScale};
use anyhow::{anyhow, Context, Result};
use image::{ImageBuffer, Rgb, RgbImage};
use imageproc::drawing::{draw_text_mut, text_size};
use ndarray::Array3;
use ffmpeg_next as ffmpeg;

use crate::{
    core::Direction::{ToLeft, ToRight},
    danmaku::{Danmaku, DanmakuMode},
    decoder::VideoDecoder,
    interaction::Args,
    utils::{GrowableVec, Ignore},
};

pub(crate) struct FfmpegEncoder {
    output: ffmpeg::format::context::Output,
    encoder: ffmpeg::encoder::Video,
    scaler: ffmpeg::software::scaling::context::Context,
    ost_index: usize,
    encoder_time_base: ffmpeg::Rational,
    src_frame: ffmpeg::frame::Video,
    dst_frame: ffmpeg::frame::Video,
    frame_count: u64,
}

impl FfmpegEncoder {
    fn new(path: &Path, width: u32, height: u32, frame_rate: f32) -> Result<Self> {
        let mut octx = ffmpeg::format::output(path)
            .with_context(|| format!("创建视频输出文件失败: {}", path.display()))?;

        let codec = ffmpeg::encoder::find_by_name("libx264")
            .or_else(|| ffmpeg::encoder::find(ffmpeg::codec::Id::H264))
            .context("找不到可用的 H.264 编码器（libx264），请确认 ffmpeg 安装完整")?;

        let global_header = octx.format().flags()
            .contains(ffmpeg::format::flag::Flags::GLOBAL_HEADER);

        let mut ost = octx.add_stream(codec.clone())
            .context("添加视频流到输出文件失败")?;
        let ost_index = ost.index();

        let ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
        let mut encoder = ctx.encoder().video()
            .context("创建视频编码器上下文失败")?;

        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(ffmpeg::util::format::Pixel::YUV420P);
        encoder.set_frame_rate(Some((frame_rate.round() as i32, 1)));
        encoder.set_time_base(ffmpeg::util::mathematics::rescale::TIME_BASE);

        if global_header {
            encoder.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
        }

        let encoder = encoder.open()
            .map_err(|e| anyhow!("打开 H.264 编码器失败: {e:?}"))?;
        let encoder_time_base = encoder.time_base();

        ost.set_parameters(&encoder);

        let scaler = ffmpeg::software::scaling::context::Context::get(
            ffmpeg::util::format::Pixel::RGB24,
            width,
            height,
            ffmpeg::util::format::Pixel::YUV420P,
            width,
            height,
            ffmpeg::software::scaling::flag::Flags::empty(),
        ).context("创建像素格式转换器失败（RGB → YUV420P）")?;

        let src_frame = ffmpeg::frame::Video::new(
            ffmpeg::util::format::Pixel::RGB24,
            width,
            height,
        );

        let mut dst_frame = ffmpeg::frame::Video::new(
            ffmpeg::util::format::Pixel::YUV420P,
            width,
            height,
        );
        unsafe {
            ffmpeg::ffi::av_frame_get_buffer(dst_frame.as_mut_ptr(), 0);
        }

        octx.write_header()
            .context("写入输出文件头失败")?;

        Ok(Self {
            output: octx,
            encoder,
            scaler,
            ost_index,
            encoder_time_base,
            src_frame,
            dst_frame,
            frame_count: 0,
        })
    }

    fn encode(&mut self, ndarray_frame: &Array3<u8>, timestamp_secs: f64) -> Result<()> {
        let (height, width, _channels) = ndarray_frame.dim();
        let layout = ndarray_frame.as_standard_layout();
        let slice = layout
            .as_slice()
            .context("帧数据不连续，无法转换为编码器输入格式")?;

        unsafe {
            ffmpeg::ffi::av_image_fill_arrays(
                (*self.src_frame.as_mut_ptr()).data.as_mut_ptr(),
                (*self.src_frame.as_mut_ptr()).linesize.as_mut_ptr(),
                slice.as_ptr(),
                ffmpeg::util::format::Pixel::RGB24.into(),
                width as i32,
                height as i32,
                1,
            );
        }
        self.src_frame.set_width(width as u32);
        self.src_frame.set_height(height as u32);

        let tb_num = self.encoder_time_base.numerator() as f64;
        let tb_den = self.encoder_time_base.denominator() as f64;
        let pts = (timestamp_secs * tb_den / tb_num).round() as i64;
        self.src_frame.set_pts(Some(pts));

        self.scaler.run(&self.src_frame, &mut self.dst_frame)
            .context("帧像素格式缩放失败")?;
        self.dst_frame.set_pts(Some(pts));

        if self.frame_count % 12 == 0 {
            self.dst_frame.set_kind(ffmpeg::util::picture::Type::I);
        }

        self.frame_count += 1;

        self.encoder.send_frame(&self.dst_frame)
            .context("发送帧到 H.264 编码器失败")?;

        self.receive_and_write()
            .with_context(|| format!("接收并写入编码包失败 (帧 #{})", self.frame_count))?;

        Ok(())
    }

    fn receive_and_write(&mut self) -> Result<()> {
        let ost_time_base = self
            .output
            .stream(self.ost_index)
            .context("获取输出流时基失败")?
            .time_base();

        loop {
            let mut packet = ffmpeg::codec::packet::Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(self.ost_index);
                    packet.set_position(-1);
                    packet.rescale_ts(self.encoder_time_base, ost_time_base);
                    packet
                        .write(&mut self.output)
                        .context("写入编码包到输出文件失败")?;
                }
                Err(ffmpeg::Error::Other { errno })
                    if errno == ffmpeg::util::error::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(anyhow!("编码器接收包错误: {e}")),
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.encoder.send_eof()
            .context("编码器发送 EOF 信号失败")?;

        let ost_time_base = self
            .output
            .stream(self.ost_index)
            .context("获取输出流时基失败")?
            .time_base();

        loop {
            let mut packet = ffmpeg::codec::packet::Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(self.ost_index);
                    packet.set_position(-1);
                    packet.rescale_ts(self.encoder_time_base, ost_time_base);
                    packet
                        .write(&mut self.output)
                        .context("写入最终编码包失败")?;
                }
                Err(ffmpeg::Error::Other { errno })
                    if errno == ffmpeg::util::error::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(anyhow!("编码器最终收包错误: {e}")),
            }
        }

        self.output.write_trailer()
            .context("写入输出文件尾失败")?;

        Ok(())
    }
}

pub fn video_process(
    mut decoder: VideoDecoder,
    mut encoder: FfmpegEncoder,
    mut danmakus: Vec<Danmaku>,
    args: &Args,
    frame_duration_secs: f64,
) -> Result<()> {
    let (video_width, video_height) = decoder.size();
    let gap = args.line_spacing;
    let area_top = (video_height as f64 * args.top_ratio) as u32;
    let area_bottom = (video_height as f64 * args.bottom_ratio) as u32;
    let area_height = area_bottom - area_top;

    let base_font_size = args.font_scale * 25.0;
    let line_height = base_font_size as u32 + gap;
    let rail_cnt = area_height / line_height;

    let mut scroll_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut top_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut bottom_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut reverse_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);

    static SOURCE_FONT: &[u8] = include_bytes!("../fonts/SourceHanSansSC-Regular-2.otf");
    let regular = FontVec::try_from_vec(SOURCE_FONT.to_vec())
        .expect("内置字体加载失败: SourceHanSansSC-Regular-2.otf，字体文件可能已损坏或不存在");

    let mut frame_count = 0u64;
    let total_frames = decoder.frame_count();

    loop {
        let (ts_secs, frame) = match decoder.next_frame()? {
            Some(res) => res,
            None => break,
        };
        let dur = Duration::from_secs_f64(ts_secs);

        let mut image = array3_to_rgb_image(&frame)
            .with_context(|| format!("帧数据转换为RGB图像失败 (时间戳: {ts_secs})"))?;

        let enqueue = danmakus.extract_if(.., |d| d.time <= dur);

        for d in enqueue {
            let scale = PxScale::from(d.font_size as f32);
            let (width, height) = text_size(scale, &regular, &d.text);
            let dead_line = match d.mode {
                DanmakuMode::Scroll | DanmakuMode::Reverse => {
                    let travel_frames = (width + video_width).div_ceil(args.speed);
                    dur + Duration::from_secs_f64(
                        travel_frames as f64 * frame_duration_secs,
                    )
                }
                DanmakuMode::Top | DanmakuMode::Bottom => {
                    dur + Duration::from_secs_f64(args.fixed_duration)
                }
                _ => Duration::from_millis(0),
            };
            match d.mode {
                DanmakuMode::Scroll => scroll_slots
                    .set_first_empty((
                        d,
                        NormalComponent {
                            x: video_width as i64,
                            y: None,
                            width,
                            height,
                            dead_line,
                        },
                    ))
                    .ignore(),
                DanmakuMode::Bottom => bottom_slots
                    .set_first_empty((
                        d,
                        NormalComponent {
                            x: video_width as i64 / 2 - width as i64 / 2,
                            y: None,
                            width,
                            height,
                            dead_line,
                        },
                    ))
                    .ignore(),
                DanmakuMode::Top => top_slots
                    .set_first_empty((
                        d,
                        NormalComponent {
                            x: video_width as i64 / 2 - width as i64 / 2,
                            y: None,
                            width,
                            height,
                            dead_line,
                        },
                    ))
                    .ignore(),
                DanmakuMode::Reverse => reverse_slots
                    .set_first_empty((
                        d,
                        NormalComponent {
                            x: -(width as i64),
                            y: None,
                            width,
                            height,
                            dead_line,
                        },
                    ))
                    .ignore(),
                _ => (),
            };
        }

        del_dead(&mut scroll_slots, dur);
        del_dead(&mut reverse_slots, dur);
        del_dead(&mut top_slots, dur);
        del_dead(&mut bottom_slots, dur);

        draw_scroll_danmukus(
            &mut image,
            &mut scroll_slots,
            &regular,
            line_height,
            rail_cnt,
            args.font_scale,
            area_top as i64,
            ToLeft,
            args.opacity,
        );
        draw_scroll_danmukus(
            &mut image,
            &mut reverse_slots,
            &regular,
            line_height,
            rail_cnt,
            args.font_scale,
            area_top as i64,
            ToRight,
            args.opacity,
        );
        scroll(
            scroll_slots
                .iter_mut()
                .filter_map(|cur| cur.as_mut().map(|(_, c)| c))
                .filter(|c| c.y.is_some()),
            ToLeft,
            args.speed,
        );
        scroll(
            reverse_slots
                .iter_mut()
                .filter_map(|cur| cur.as_mut().map(|(_, c)| c))
                .filter(|c| c.y.is_some()),
            ToRight,
            args.speed,
        );

        draw_fixed_danmukus(
            &mut image,
            &mut top_slots,
            &regular,
            line_height,
            rail_cnt,
            args.font_scale,
            area_top as i64,
            true,
            args.opacity,
        );
        draw_fixed_danmukus(
            &mut image,
            &mut bottom_slots,
            &regular,
            line_height,
            rail_cnt,
            args.font_scale,
            area_top as i64,
            false,
            args.opacity,
        );

        let video_frame = rgb_image_to_array3(&image);
        encoder
            .encode(&video_frame, ts_secs)
            .with_context(|| format!("编码帧失败 (时间戳: {ts_secs})"))?;
        frame_count += 1;
        if !args.quiet {
            render_progress(frame_count, total_frames);
        }
    }

    if !args.quiet {
        eprintln!(); // finalize progress bar line
    }

    encoder.finish().context("编码器完成写入失败（输出文件不可用）")?;

    Ok(())
}

fn draw_fixed_danmukus(
    image: &mut RgbImage,
    fixed_slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    font: &FontVec,
    line_height: u32,
    rail_cnt: u32,
    font_size_ratio: f32,
    area_top: i64,
    to_bottom: bool,
    opacity: f64,
) {
    let mut ensure_y_q = Vec::new();
    for (idx, opt) in fixed_slots
        .iter()
        .enumerate()
        .filter(|(_, opt)| opt.is_some())
    {
        let (dan, comp) = opt.as_ref().unwrap();
        let scale = PxScale::from(dan.font_size as f32 * font_size_ratio);
        if let Some(y) = comp.y {
            let canvas_pos = (comp.x as i32, (y + area_top) as i32);
            draw_text_mut(
                image,
                apply_opacity(dan.color, opacity),
                canvas_pos.0,
                canvas_pos.1,
                scale,
                font,
                &dan.text,
            );
            continue;
        }

        let raw_occupieds = fixed_slots.iter().filter_map(|opt| {
            opt.as_ref().map(|(_, c)| c).and_then(|comp| {
                if comp.y.is_some() {
                    Some((comp.height, comp.y.unwrap()))
                } else {
                    None
                }
            })
        });
        let mut occupieds = HashSet::with_capacity(fixed_slots.len());
        for ( height, y) in raw_occupieds {
            occupieds.insert(y);
            if height > line_height {
                occupieds.insert(y + (line_height as i64) * (height / line_height) as i64);
            }
        }

        let rail_hs = rail_hs(line_height, rail_cnt);
        let free = rail_hs.filter(|h| !occupieds.contains(h));
        let mut free: Box<dyn Iterator<Item = _>> = if to_bottom {
            Box::new(free)
        } else {
            Box::new(free.collect::<Vec<_>>().into_iter().rev())
        };

        let y = match free.next() {
            None => {
                continue;
            }
            Some(y) => y,
        };

        let canvas_pos = (comp.x as i32, (y + area_top) as i32);
        draw_text_mut(
            image,
            apply_opacity(dan.color, opacity),
            canvas_pos.0,
            canvas_pos.1,
            scale,
            font,
            &dan.text,
        );
        ensure_y_q.push((idx, y));
    }

    for (idx, y) in ensure_y_q {
        fixed_slots[idx].as_mut().unwrap().1.y = Some(y);
    }
}

fn del_dead(slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>, dur: Duration) {
    for opt in slots.iter_mut() {
        let Some((_, comp)) = &opt else {
            continue;
        };

        if comp.dead_line <= dur {
            let _ = comp;
            *opt = None;
        }
    }
}

fn scroll<'a>(
    comps: impl Iterator<Item = &'a mut NormalComponent>,
    direction: Direction,
    speed: u32,
) {
    for comp in comps {
        match direction {
            ToLeft => comp.x -= speed as i64,
            ToRight => comp.x += speed as i64,
        };
    }
}

enum Direction {
    ToLeft,
    ToRight,
}

fn draw_scroll_danmukus(
    big_image: &mut RgbImage,
    scroll_slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    font: &FontVec,
    line_height: u32,
    rail_cnt: u32,
    font_size_ratio: f32,
    area_top: i64,
    dir: Direction,
    opacity: f64,
) {
    let mut ensure_y_q = Vec::new();
    for (idx, opt) in scroll_slots
        .iter()
        .enumerate()
        .filter(|(_, opt)| opt.is_some())
    {
        let (dan, comp) = opt.as_ref().unwrap();
        let scale = PxScale::from(dan.font_size as f32 * font_size_ratio);
        if let Some(y) = comp.y {
            let canvas_pos = (comp.x as i32, (y + area_top) as i32);
            draw_text_mut(
                big_image,
                apply_opacity(dan.color, opacity),
                canvas_pos.0,
                canvas_pos.1,
                scale,
                font,
                &dan.text,
            );
            continue;
        }

        let raw_occupieds = scroll_slots.iter().filter_map(|opt| {
            opt.as_ref().map(|(_, c)| c).and_then(|comp| {
                if comp.y.is_some() {
                    Some((comp.width, comp.height, comp.x, comp.y.unwrap()))
                } else {
                    None
                }
            })
        });
        let mut occupieds = HashSet::with_capacity(scroll_slots.len());
        for (width, height, x, y) in raw_occupieds {
            debug_assert_eq!(y % line_height as i64, 0);
            match dir {
                ToLeft => {
                    if width as i64 + x > comp.x {
                        occupieds.insert(y);
                    } else {
                        continue;
                    }
                }
                ToRight => {
                    if x < comp.x + comp.width as i64 {
                        occupieds.insert(y);
                    } else {
                        continue;
                    }
                }
            }
            if height > line_height {
                occupieds.insert(y + (line_height as i64) * (height / line_height) as i64);
            }
        }

        let rail_hs = rail_hs(line_height, rail_cnt);
        let mut free = rail_hs.filter(|h| !occupieds.contains(h));

        let y = match free.next() {
            None => {
                continue;
            }
            Some(y) => y,
        };

        let canvas_pos = (comp.x as i32, (y + area_top) as i32);
        draw_text_mut(
            big_image,
            apply_opacity(dan.color, opacity),
            canvas_pos.0,
            canvas_pos.1,
            scale,
            font,
            &dan.text,
        );
        ensure_y_q.push((idx, y));
    }

    for (idx, y) in ensure_y_q {
        scroll_slots[idx].as_mut().unwrap().1.y = Some(y);
    }
}

fn rail_hs(line_height: u32, rail_cnt: u32) -> impl Iterator<Item = i64> {
    std::iter::successors(Some(0i64), move |prev| {
        let next = prev + line_height as i64;
        if next < (rail_cnt * line_height) as i64 {
            Some(next)
        } else {
            None
        }
    })
}

fn apply_opacity(color: Rgb<u8>, opacity: f64) -> Rgb<u8> {
    Rgb([
        (color[0] as f64 * opacity) as u8,
        (color[1] as f64 * opacity) as u8,
        (color[2] as f64 * opacity) as u8,
    ])
}

#[derive(Debug, Clone)]
struct NormalComponent {
    x: i64,
    y: Option<i64>,
    width: u32,
    height: u32,
    dead_line: Duration,
}

pub fn same_specifications(
    decoder: &VideoDecoder,
    path: impl AsRef<Path>,
) -> anyhow::Result<(FfmpegEncoder, f64)> {
    let path = path.as_ref();

    let (width, height) = decoder.size();
    let frame_rate = decoder.frame_rate();
    let encoder = FfmpegEncoder::new(path, width, height, frame_rate)?;

    let frame_duration_secs = 1.0 / frame_rate as f64;

    Ok((encoder, frame_duration_secs))
}

pub fn array3_to_rgb_image(array: &Array3<u8>) -> Result<RgbImage> {
    let (height, width, channels) = array.dim();
    if channels != 3 {
        return Err(anyhow!(
            "视频帧通道数异常：期望 RGB 3 通道，实际得到 {channels} 通道"
        ));
    }

    let layout = array.as_standard_layout();
    let slice = layout
        .as_slice()
        .ok_or_else(|| anyhow!("视频帧数据不连续或使用了非标准内存布局，无法转换为图像"))?;

    let img = ImageBuffer::from_vec(width as u32, height as u32, slice.to_vec())
        .ok_or_else(|| {
            anyhow!(
                "图像缓冲区创建失败 ({width}x{height}x3)，像素数据长度与尺寸不匹配"
            )
        })?;

    Ok(img)
}

pub fn rgb_image_to_array3(img: &RgbImage) -> Array3<u8> {
    let (width, height) = img.dimensions();
    let raw_pixels = img.as_raw();

    Array3::from_shape_vec((height as usize, width as usize, 3), raw_pixels.to_vec())
        .expect("图像转视频帧失败：尺寸不匹配，像素数据长度与维度 (height×width×3) 不一致")
}

fn render_progress(current: u64, total: u64) {
    const BAR_WIDTH: usize = 30;

    if total == 0 {
        eprint!("\r正在渲染弹幕... 已处理 {current} 帧");
    } else {
        let pct = (current as f64 / total as f64 * 100.0) as u32;
        let filled = ((BAR_WIDTH as f64 * current as f64 / total as f64) as usize).min(BAR_WIDTH);
        let empty = BAR_WIDTH - filled;
        eprint!(
            "\r[{}{}] {:>3}% ({current}/{total})",
            "█".repeat(filled),
            "░".repeat(empty),
            pct,
        );
    }
}
