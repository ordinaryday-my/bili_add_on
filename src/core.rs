use std::{panic, path::Path, thread, time::Duration};

use ab_glyph::{FontVec, PxScale};
use anyhow::{anyhow, Context, Result};
use bit_set::BitSet;
use crossbeam_channel::bounded;
use image::{RgbImage, Rgba, RgbaImage};
use imageproc::drawing::{draw_text_mut, text_size};

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

        let global_header = octx
            .format()
            .flags()
            .contains(ffmpeg::format::flag::Flags::GLOBAL_HEADER);

        let mut ost = octx.add_stream(codec).context("添加视频流到输出文件失败")?;
        let ost_index = ost.index();

        let ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
        let mut encoder = ctx.encoder().video().context("创建视频编码器上下文失败")?;

        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(ffmpeg::util::format::Pixel::YUV420P);
        encoder.set_frame_rate(Some((frame_rate.round() as i32, 1)));
        encoder.set_time_base(ffmpeg::util::mathematics::rescale::TIME_BASE);

        if global_header {
            encoder.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
        }

        let encoder = encoder
            .open()
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
        )
        .context("创建像素格式转换器失败（RGB → YUV420P）")?;

        let src_frame =
            ffmpeg::frame::Video::new(ffmpeg::util::format::Pixel::RGB24, width, height);

        let mut dst_frame =
            ffmpeg::frame::Video::new(ffmpeg::util::format::Pixel::YUV420P, width, height);
        unsafe {
            ffmpeg::ffi::av_frame_get_buffer(dst_frame.as_mut_ptr(), 0);
        }

        octx.write_header().context("写入输出文件头失败")?;

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

    fn encode(&mut self, image: &RgbImage, timestamp_secs: f64) -> Result<()> {
        let (width, height) = image.dimensions();
        let raw = image.as_raw();

        unsafe {
            ffmpeg::ffi::av_image_fill_arrays(
                (*self.src_frame.as_mut_ptr()).data.as_mut_ptr(),
                (*self.src_frame.as_mut_ptr()).linesize.as_mut_ptr(),
                raw.as_ptr(),
                ffmpeg::util::format::Pixel::RGB24.into(),
                width as i32,
                height as i32,
                1,
            );
        }
        self.src_frame.set_width(width);
        self.src_frame.set_height(height);

        let tb_num = self.encoder_time_base.numerator() as f64;
        let tb_den = self.encoder_time_base.denominator() as f64;
        let pts = (timestamp_secs * tb_den / tb_num).round() as i64;
        self.src_frame.set_pts(Some(pts));

        self.scaler
            .run(&self.src_frame, &mut self.dst_frame)
            .context("帧像素格式缩放失败")?;
        self.dst_frame.set_pts(Some(pts));

        if self.frame_count.is_multiple_of(12) {
            self.dst_frame.set_kind(ffmpeg::util::picture::Type::I);
        }

        self.frame_count += 1;

        self.encoder
            .send_frame(&self.dst_frame)
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
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    break
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(anyhow!("编码器接收包错误: {e}")),
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.encoder.send_eof().context("编码器发送 EOF 信号失败")?;

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
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    break
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(anyhow!("编码器最终收包错误: {e}")),
            }
        }

        self.output.write_trailer().context("写入输出文件尾失败")?;

        Ok(())
    }
}

unsafe impl Send for FfmpegEncoder {}

pub fn video_process(
    mut decoder: VideoDecoder,
    mut encoder: FfmpegEncoder,
    mut danmakus: Vec<Danmaku>,
    args: &Args,
    frame_duration_secs: f64,
) -> Result<()> {
    danmakus.sort_unstable_by_key(|dan| std::cmp::Reverse(dan.time));

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

    thread::scope(move |s| -> Result<()> {
        let (recycle_s, recycle_r) = bounded::<RgbImage>(3);
        for _ in 0..3 {
            recycle_s
                .send(RgbImage::new(video_width, video_height))
                .expect("初始化帧缓冲池失败");
        }

        let (decode_s, decode_r) = bounded(800);
        let decode_producer = s.spawn(move || -> Result<()> {
            loop {
                let mut image = recycle_r
                    .recv()
                    .expect("帧回收通道异常关闭，编码线程可能已崩溃");
                let ts_secs = match decoder.next_frame_into(&mut image)? {
                    Some(ts) => ts,
                    None => break,
                };
                let dur = Duration::from_secs_f64(ts_secs);
                decode_s
                    .send((ts_secs, dur, image))
                    .expect("接收端不应提早关闭");
            }
            Ok(())
        });

        let (encode_s, encode_v) = bounded(800);
        let process_pipeline = s.spawn(move || -> Result<()> {
            loop {
                let Ok((ts_secs, dur, mut image)) = decode_r.recv() else {
                    break;
                };
                let ready_idx = danmakus.partition_point(|dan| dan.time > dur);
                let enqueue = danmakus.drain(ready_idx..).rev();

                for d in enqueue {
                    let scale = PxScale::from((d.font_size as f32) * args.font_scale);
                    let (width, height) = text_size(scale, &regular, &d.text);
                    let color = d.color;
                    let mut cached_text = RgbaImage::new(width, height);
                    draw_text_mut(
                        &mut cached_text,
                        Rgba([color[0], color[1], color[2], 255]),
                        0,
                        0,
                        scale,
                        &regular,
                        &d.text,
                    );
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
                                    cached_text: Some(cached_text),
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
                                    cached_text: Some(cached_text),
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
                                    cached_text: Some(cached_text),
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
                                    cached_text: Some(cached_text),
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

                let mut draw_params = DrawParams {
                    image: &mut image,
                    line_height,
                    rail_cnt,
                    area_top: area_top as i64,
                    opacity: args.opacity,
                };

                draw_scroll_danmukus(&mut draw_params, &mut scroll_slots, ToLeft);
                draw_scroll_danmukus(&mut draw_params, &mut reverse_slots, ToRight);
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

                draw_fixed_danmukus(&mut draw_params, &mut top_slots, true);
                draw_fixed_danmukus(&mut draw_params, &mut bottom_slots, false);

                encode_s
                    .send((image, ts_secs))
                    .expect("接收端不得早于发送端关闭");
                frame_count += 1;
                if !args.quiet {
                    render_progress(frame_count, total_frames);
                }
            }

            Ok(())
        });

        let encode_customer = s.spawn(move || -> Result<()> {
            loop {
                let Ok((image, ts_secs)) = encode_v.recv() else {
                    break;
                };

                encoder
                    .encode(&image, ts_secs)
                    .with_context(|| format!("编码帧失败 (时间戳: {ts_secs})"))?;
                let _ = recycle_s.try_send(image);
            }

            if !args.quiet {
                eprintln!(); // finalize progress bar line
            }

            encoder
                .finish()
                .context("编码器完成写入失败（输出文件不可用）")?;
            Ok(())
        });

        let handles = [decode_producer, process_pipeline, encode_customer];
        for handle in handles {
            match handle.join() {
                Ok(res) => res?,
                Err(e) => {
                    if let Some(msg) = e.downcast_ref::<&'static str>() {
                        panic!("{}", msg);
                    } else if let Some(msg) = e.downcast_ref::<String>() {
                        panic!("{}", msg);
                    } else {
                        panic!("panic with unknown payload");
                    }
                }
            }
        }

        Ok(())
    })?;

    Ok(())
}

struct DrawParams<'a> {
    image: &'a mut RgbImage,
    line_height: u32,
    rail_cnt: u32,
    area_top: i64,
    opacity: f64,
}

fn draw_fixed_danmukus(
    params: &mut DrawParams,
    fixed_slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    to_bottom: bool,
) {
    const OFFSET: i64 = 1000;
    let cap = fixed_slots.len() + OFFSET as usize;
    let mut occupieds = BitSet::with_capacity(cap);
    let mut ensure_y_q = Vec::new();
    let mut pending_y: Vec<(i64, u32)> = Vec::new();
    for (idx, opt) in fixed_slots
        .iter()
        .enumerate()
        .filter(|(_, opt)| opt.is_some())
    {
        let (_dan, comp) = opt.as_ref().unwrap();
        if let Some(y) = comp.y {
            blit_cached_text(
                params.image,
                comp.cached_text.as_ref().unwrap(),
                comp.x as i32,
                (y + params.area_top) as i32,
                params.opacity,
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

        for (height, y) in raw_occupieds {
            occupieds.insert((y + OFFSET) as usize);
            let extra_rails = height.div_ceil(params.line_height);
            for i in 1..extra_rails {
                occupieds.insert(((y + i as i64 * params.line_height as i64) + OFFSET) as usize);
            }
        }

        for &(y, height) in &pending_y {
            occupieds.insert((y + OFFSET) as usize);
            let extra_rails = height.div_ceil(params.line_height);
            for i in 1..extra_rails {
                occupieds.insert(((y + i as i64 * params.line_height as i64) + OFFSET) as usize);
            }
        }

        let rail_hs = rail_hs(params.line_height, params.rail_cnt);
        let free = rail_hs.filter(|h| !occupieds.contains((h + OFFSET) as usize));
        let mut free: Box<dyn Iterator<Item = _>> = if to_bottom {
            Box::new(free)
        } else {
            Box::new(free.collect::<Vec<_>>().into_iter().rev())
        };

        let y = match free.next() {
            None => {
                drop(free);
                occupieds.reset();
                continue;
            }
            Some(y) => y,
        };

        blit_cached_text(
            params.image,
            comp.cached_text.as_ref().unwrap(),
            comp.x as i32,
            (y + params.area_top) as i32,
            params.opacity,
        );
        pending_y.push((y, comp.height));
        ensure_y_q.push((idx, y));
        drop(free);
        occupieds.reset();
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
    params: &mut DrawParams,
    scroll_slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    dir: Direction,
) {
    const OFFSET: i64 = 1000;
    let cap = scroll_slots.len() + OFFSET as usize;
    let mut occupieds = BitSet::with_capacity(cap);
    let mut ensure_y_q = Vec::new();
    let mut pending_y: Vec<(i64, u32)> = Vec::new();
    for (idx, opt) in scroll_slots
        .iter()
        .enumerate()
        .filter(|(_, opt)| opt.is_some())
    {
        let (_dan, comp) = opt.as_ref().unwrap();
        if let Some(y) = comp.y {
            blit_cached_text(
                params.image,
                comp.cached_text.as_ref().unwrap(),
                comp.x as i32,
                (y + params.area_top) as i32,
                params.opacity,
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

        for (width, height, x, y) in raw_occupieds {
            debug_assert_eq!(y % params.line_height as i64, 0);
            match dir {
                ToLeft => {
                    if width as i64 + x > comp.x {
                        occupieds.insert((y + OFFSET) as usize);
                    } else {
                        continue;
                    }
                }
                ToRight => {
                    if x < comp.x + comp.width as i64 {
                        occupieds.insert((y + OFFSET) as usize);
                    } else {
                        continue;
                    }
                }
            }
            let extra_rails = height.div_ceil(params.line_height);
            for i in 1..extra_rails {
                occupieds.insert(((y + i as i64 * params.line_height as i64) + OFFSET) as usize);
            }
        }

        for &(y, height) in &pending_y {
            occupieds.insert((y + OFFSET) as usize);
            let extra_rails = height.div_ceil(params.line_height);
            for i in 1..extra_rails {
                occupieds.insert(((y + i as i64 * params.line_height as i64) + OFFSET) as usize);
            }
        }

        let rail_hs = rail_hs(params.line_height, params.rail_cnt);
        let mut free = rail_hs.filter(|h| !occupieds.contains((h + OFFSET) as usize));

        let y = match free.next() {
            None => {
                drop(free);
                occupieds.reset();
                continue;
            }
            Some(y) => y,
        };

        blit_cached_text(
            params.image,
            comp.cached_text.as_ref().unwrap(),
            comp.x as i32,
            (y + params.area_top) as i32,
            params.opacity,
        );
        pending_y.push((y, comp.height));
        ensure_y_q.push((idx, y));
        drop(free);
        occupieds.reset();
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

fn blit_cached_text(frame: &mut RgbImage, sprite: &RgbaImage, x: i32, y: i32, opacity: f64) {
    let o256 = (opacity * 256.0).round() as u32;
    if o256 == 0 {
        return;
    }
    let (sw, sh) = sprite.dimensions();
    let (fw, fh) = frame.dimensions();
    let clip_x1 = x.max(0) as u32;
    let clip_y1 = y.max(0) as u32;
    let clip_x2 = ((x + sw as i32).max(0) as u32).min(fw);
    let clip_y2 = ((y + sh as i32).max(0) as u32).min(fh);
    if clip_x1 >= clip_x2 || clip_y1 >= clip_y2 {
        return;
    }
    let frame_stride = fw as usize * 3;
    let sprite_stride = sw as usize * 4;
    let frame_buf = frame.as_mut();
    let sprite_buf = sprite.as_raw();
    for fy in clip_y1..clip_y2 {
        let sy = (fy as i32 - y) as u32;
        let frame_row_start = fy as usize * frame_stride;
        let sprite_row_start = sy as usize * sprite_stride;
        for fx in clip_x1..clip_x2 {
            let sx = (fx as i32 - x) as u32;
            let si = sprite_row_start + sx as usize * 4;
            let sa = sprite_buf[si + 3] as u32;
            if sa == 0 {
                continue;
            }
            let ea = sa * o256 / 256;
            let inv_ea = 256 - ea;
            let fi = frame_row_start + fx as usize * 3;
            frame_buf[fi] = ((sprite_buf[si] as u32 * ea + frame_buf[fi] as u32 * inv_ea) / 256) as u8;
            frame_buf[fi + 1] =
                ((sprite_buf[si + 1] as u32 * ea + frame_buf[fi + 1] as u32 * inv_ea) / 256) as u8;
            frame_buf[fi + 2] =
                ((sprite_buf[si + 2] as u32 * ea + frame_buf[fi + 2] as u32 * inv_ea) / 256) as u8;
        }
    }
}

#[derive(Debug, Clone)]
struct NormalComponent {
    x: i64,
    y: Option<i64>,
    width: u32,
    height: u32,
    dead_line: Duration,
    cached_text: Option<RgbaImage>,
}

pub fn same_specifications(
    decoder: &VideoDecoder,
    path: impl AsRef<Path>,
) -> anyhow::Result<(FfmpegEncoder, f64)> {
    let path = path.as_ref();

    let (width, height) = decoder.size();
    let frame_rate = decoder.frame_rate();
    if frame_rate <= 0.0 {
        return Err(anyhow!("视频帧率无效: {frame_rate}，无法确定每帧持续时间"));
    }
    let encoder = FfmpegEncoder::new(path, width, height, frame_rate)?;

    let frame_duration_secs = 1.0 / frame_rate as f64;

    Ok((encoder, frame_duration_secs))
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
