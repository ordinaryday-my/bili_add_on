use core::panic;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

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
    hw,
    interaction::Args,
    utils::{blit_cached_text, rail_hs, GrowableVec, Ignore},
};

#[allow(dead_code)]
pub(crate) enum EncoderPref {
    Auto,
    Specific(hw::HwCodec),
    Software,
}

pub(crate) struct FfmpegEncoder {
    output: ffmpeg::format::context::Output,
    encoder: ffmpeg::encoder::Video,
    ost_index: usize,
    encoder_time_base: ffmpeg::Rational,
    frame_count: u64,
    sw_scaler: ffmpeg::software::scaling::context::Context,
    sw_frame_rgb: ffmpeg::frame::Video,
    sw_frame_yuv: ffmpeg::frame::Video,
    hw_setup: Option<hw::HwSetup>,
}

unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    fn new(
        path: &Path,
        width: u32,
        height: u32,
        frame_rate: f32,
        encoder_pref: EncoderPref,
    ) -> Result<Self> {
        let mut octx = ffmpeg::format::output(path)
            .with_context(|| format!("创建视频输出文件失败: {}", path.display()))?;

        let global_header = octx
            .format()
            .flags()
            .contains(ffmpeg::format::flag::Flags::GLOBAL_HEADER);

        let candidates: Vec<Option<hw::HwCodec>> = match encoder_pref {
            EncoderPref::Auto => {
                let mut v: Vec<_> = hw::HwCodec::all().into_iter().map(Some).collect();
                v.push(None);
                v
            }
            EncoderPref::Specific(c) => vec![Some(c), None],
            EncoderPref::Software => vec![None],
        };

        let mut last_err: Option<anyhow::Error> = None;

        for candidate in &candidates {
            let codec = match candidate {
                Some(hw_codec) => match ffmpeg::encoder::find_by_name(hw_codec.encoder_name()) {
                    Some(c) => c,
                    None => {
                        last_err = Some(anyhow!("找不到编码器: {}", hw_codec.encoder_name()));
                        continue;
                    }
                },
                None => {
                    match ffmpeg::encoder::find_by_name("libx264")
                        .or_else(|| ffmpeg::encoder::find(ffmpeg::codec::Id::H264))
                    {
                        Some(c) => c,
                        None => {
                            last_err = Some(anyhow!(
                                "找不到可用的 H.264 编码器（libx264），请确认 ffmpeg 安装完整"
                            ));
                            continue;
                        }
                    }
                }
            };

            match Self::try_open_encoder(
                &codec,
                *candidate,
                width,
                height,
                frame_rate,
                global_header,
            ) {
                Ok((encoder, encoder_time_base, hw_setup)) => {
                    let mut ost = octx.add_stream(codec).context("添加视频流到输出文件失败")?;
                    let ost_index = ost.index();
                    ost.set_parameters(&encoder);

                    let (sw_dst_pix, sw_dst_buffer) = if hw_setup.is_some() {
                        (ffmpeg::util::format::Pixel::NV12, false)
                    } else {
                        (ffmpeg::util::format::Pixel::YUV420P, true)
                    };

                    let sw_scaler = ffmpeg::software::scaling::context::Context::get(
                        ffmpeg::util::format::Pixel::RGB24,
                        width,
                        height,
                        sw_dst_pix,
                        width,
                        height,
                        ffmpeg::software::scaling::flag::Flags::empty(),
                    )
                    .context("创建像素格式转换器失败（RGB → 编码像素格式）")?;

                    let sw_frame_rgb = ffmpeg::frame::Video::new(
                        ffmpeg::util::format::Pixel::RGB24,
                        width,
                        height,
                    );

                    let mut sw_frame_yuv = ffmpeg::frame::Video::new(sw_dst_pix, width, height);
                    if sw_dst_buffer {
                        unsafe {
                            ffmpeg::ffi::av_frame_get_buffer(sw_frame_yuv.as_mut_ptr(), 0);
                        }
                    }

                    octx.write_header().context("写入输出文件头失败")?;

                    return Ok(Self {
                        output: octx,
                        encoder,
                        ost_index,
                        encoder_time_base,
                        frame_count: 0,
                        sw_scaler,
                        sw_frame_rgb,
                        sw_frame_yuv,
                        hw_setup,
                    });
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("未能找到任何可用的视频编码器")))
    }

    fn try_open_encoder(
        codec: &ffmpeg::Codec,
        hw_codec: Option<hw::HwCodec>,
        width: u32,
        height: u32,
        frame_rate: f32,
        global_header: bool,
    ) -> Result<(
        ffmpeg::encoder::Video,
        ffmpeg::Rational,
        Option<hw::HwSetup>,
    )> {
        let ctx = ffmpeg::codec::context::Context::new_with_codec(*codec);
        let mut encoder = ctx.encoder().video().context("创建视频编码器上下文失败")?;

        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_frame_rate(Some((frame_rate.round() as i32, 1)));
        encoder.set_time_base(ffmpeg::util::mathematics::rescale::TIME_BASE);
        if global_header {
            encoder.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
        }

        encoder.set_max_b_frames(0);

        let hw_setup = if let Some(hwc) = hw_codec {
            encoder.set_format(hwc.hw_pixel_rust());
            let setup = unsafe { hw::try_create_hardware_setup(hwc, width as i32, height as i32) }
                .with_context(|| format!("创建 {} 硬件帧上下文失败", hwc.encoder_name()))?;
            unsafe {
                (*encoder.as_mut_ptr()).hw_frames_ctx =
                    ffmpeg::ffi::av_buffer_ref(setup.frames_ref);
            }
            Some(setup)
        } else {
            encoder.set_format(ffmpeg::util::format::Pixel::YUV420P);
            None
        };

        let encoder = encoder.open().map_err(|e| {
            anyhow!(
                "打开 {} 编码器失败: {e:?}",
                hw_codec.map_or("libx264", |c| c.encoder_name())
            )
        })?;
        let encoder_time_base = encoder.time_base();

        Ok((encoder, encoder_time_base, hw_setup))
    }

    fn encode(&mut self, image: &RgbImage, timestamp_secs: f64) -> Result<()> {
        let (width, height) = image.dimensions();
        let raw = image.as_raw();

        unsafe {
            ffmpeg::ffi::av_image_fill_arrays(
                (*self.sw_frame_rgb.as_mut_ptr()).data.as_mut_ptr(),
                (*self.sw_frame_rgb.as_mut_ptr()).linesize.as_mut_ptr(),
                raw.as_ptr(),
                ffmpeg::util::format::Pixel::RGB24.into(),
                width as i32,
                height as i32,
                1,
            );
        }
        self.sw_frame_rgb.set_width(width);
        self.sw_frame_rgb.set_height(height);

        let tb_num = self.encoder_time_base.numerator() as f64;
        let tb_den = self.encoder_time_base.denominator() as f64;
        let pts = (timestamp_secs * tb_den / tb_num).round() as i64;
        self.sw_frame_rgb.set_pts(Some(pts));

        self.sw_scaler
            .run(&self.sw_frame_rgb, &mut self.sw_frame_yuv)
            .context("帧像素格式缩放失败")?;

        if let Some(ref hw_setup) = self.hw_setup {
            let mut hw_frame = ffmpeg::frame::Video::empty();
            unsafe {
                let ret = ffmpeg::ffi::av_hwframe_get_buffer(
                    hw_setup.frames_ref,
                    hw_frame.as_mut_ptr(),
                    0,
                );
                if ret < 0 {
                    return Err(anyhow!("从硬件帧池分配帧失败: 错误码 {}", ret));
                }

                let ret = ffmpeg::ffi::av_hwframe_transfer_data(
                    hw_frame.as_mut_ptr(),
                    self.sw_frame_yuv.as_ptr(),
                    0,
                );
                if ret < 0 {
                    return Err(anyhow!("帧数据上传到 GPU 失败: 错误码 {}", ret));
                }
            }
            hw_frame.set_pts(Some(pts));

            if self.frame_count.is_multiple_of(12) {
                hw_frame.set_kind(ffmpeg::util::picture::Type::I);
            }

            self.frame_count += 1;

            self.encoder
                .send_frame(&hw_frame)
                .context("发送帧到硬件编码器失败")?;
        } else {
            self.sw_frame_yuv.set_pts(Some(pts));

            if self.frame_count.is_multiple_of(12) {
                self.sw_frame_yuv.set_kind(ffmpeg::util::picture::Type::I);
            }

            self.frame_count += 1;

            self.encoder
                .send_frame(&self.sw_frame_yuv)
                .context("发送帧到 H.264 编码器失败")?;
        }

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

        let mut drain_retries = 0u32;
        loop {
            let mut packet = ffmpeg::codec::packet::Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    drain_retries = 0;
                    packet.set_stream(self.ost_index);
                    packet.set_position(-1);
                    packet.rescale_ts(self.encoder_time_base, ost_time_base);
                    packet
                        .write(&mut self.output)
                        .context("写入最终编码包失败")?;
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    drain_retries += 1;
                    if drain_retries >= 200 {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(anyhow!("编码器最终收包错误: {e}")),
            }
        }

        self.output.write_trailer().context("写入输出文件尾失败")?;

        Ok(())
    }
}

fn compute_max_danmaku_deadline(
    danmakus: &[Danmaku],
    regular: &FontVec,
    args: &Args,
    video_width: u32,
    frame_duration_secs: f64,
) -> f64 {
    let mut max_deadline = 0.0f64;
    for dan in danmakus {
        let deadline_secs = match dan.mode {
            DanmakuMode::Scroll | DanmakuMode::Reverse => {
                let scale = PxScale::from((dan.font_size as f32) * args.font_scale);
                let (text_width, _) = text_size(scale, regular, &dan.text);
                let travel_frames = (text_width + video_width).div_ceil(args.speed);
                dan.time.as_secs_f64() + travel_frames as f64 * frame_duration_secs
            }
            DanmakuMode::Top | DanmakuMode::Bottom => dan.time.as_secs_f64() + args.fixed_duration,
            _ => dan.time.as_secs_f64(),
        };
        if deadline_secs > max_deadline {
            max_deadline = deadline_secs;
        }
    }
    max_deadline
}

pub(crate) fn video_process(
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
    let mut total_frames = decoder.frame_count();
    let total_reporter = Arc::new(AtomicU64::new(0));

    if args.longest {
        let video_duration = if decoder.frame_rate() > 0.0 {
            decoder.frame_count() as f64 / decoder.frame_rate() as f64
        } else {
            0.0
        };
        let max_deadline = compute_max_danmaku_deadline(
            &danmakus,
            &regular,
            args,
            video_width,
            frame_duration_secs,
        );
        if max_deadline > video_duration {
            decoder.set_extend_to(max_deadline, frame_duration_secs);
            total_frames =
                ((max_deadline + frame_duration_secs) / frame_duration_secs).ceil() as u64;
        }
    }
    decoder.set_total_reporter(total_reporter.clone());

    thread::scope(move |s| -> Result<()> {
        const RECYCLE_LIM: usize = 8;
        let (recycle_s, recycle_r) = bounded::<RgbImage>(RECYCLE_LIM);
        for _ in 0..RECYCLE_LIM {
            recycle_s
                .send(RgbImage::new(video_width, video_height))
                .expect("初始化帧缓冲池失败");
        }

        let (decode_s, decode_r) = bounded(RECYCLE_LIM);
        let decode_producer = thread::Builder::new()
            .name("decode".to_string())
            .spawn_scoped(s, move || -> Result<()> {
                loop {
                    let Ok(mut image) = recycle_r.recv() else {
                        break;
                    };
                    let ts_secs = match decoder.next_frame_into(&mut image)? {
                        Some(ts) => ts,
                        None => break,
                    };
                    let dur = Duration::from_secs_f64(ts_secs);
                    if decode_s.send((ts_secs, dur, image)).is_err() {
                        break;
                    }
                }
                Ok(())
            });

        let (encode_s, encode_v) = bounded(RECYCLE_LIM);
        let process_pipeline = thread::Builder::new()
            .name("render".to_string())
            .spawn_scoped(s, move || -> Result<()> {
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

                    if encode_s.send((image, ts_secs)).is_err() {
                        break;
                    }
                    frame_count += 1;
                    if !args.quiet {
                        render_progress(
                            frame_count,
                            total_frames,
                            total_reporter.load(Ordering::Relaxed),
                        );
                    }
                }

                Ok(())
            });

        let encode_customer = thread::Builder::new()
            .name("encode".to_string())
            .spawn_scoped(s, move || -> Result<()> {
                loop {
                    let Ok((image, ts_secs)) = encode_v.recv() else {
                        break;
                    };

                    encoder
                        .encode(&image, ts_secs)
                        .with_context(|| format!("编码帧失败 (时间戳: {ts_secs})"))?;
                    let _ = recycle_s.send(image);
                }

                if !args.quiet {
                    eprintln!(); // finalize progress bar line
                }

                encoder
                    .finish()
                    .context("编码器完成写入失败（输出文件不可用）")?;
                Ok(())
            });

        let res: Result<Vec<_>, _> = [decode_producer, process_pipeline, encode_customer]
            .into_iter()
            .collect();
        let handles = res.context("系统错误: 无法创建线程")?;
        handles.into_iter().map(|handle| {
            match handle.join() {
                Ok(res) => res,
                Err(e) => {
                    if let Some(msg) = e.downcast_ref::<&'static str>() {
                        panic!("线程 panic: {msg}")
                    } else if let Some(msg) = e.downcast_ref::<String>() {
                        panic!("线程 panic: {msg}")
                    } else {
                        panic!("线程 panic: 未知错误")
                    }
                }
            }
        }).collect::<Result<Vec<_>>>()?;

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

#[derive(Debug, Clone)]
struct NormalComponent {
    x: i64,
    y: Option<i64>,
    width: u32,
    height: u32,
    dead_line: Duration,
    cached_text: Option<RgbaImage>,
}

pub(crate) fn same_specifications(
    decoder: &VideoDecoder,
    path: impl AsRef<Path>,
    encoder_pref: EncoderPref,
) -> anyhow::Result<(FfmpegEncoder, f64)> {
    let path = path.as_ref();

    let (width, height) = decoder.size();
    let frame_rate = decoder.frame_rate();
    if frame_rate <= 0.0 {
        return Err(anyhow!("视频帧率无效: {frame_rate}，无法确定每帧持续时间"));
    }
    let encoder = FfmpegEncoder::new(path, width, height, frame_rate, encoder_pref)?;

    let frame_duration_secs = 1.0 / frame_rate as f64;

    Ok((encoder, frame_duration_secs))
}

fn render_progress(current: u64, estimate: u64, exact_total: u64) {
    const BAR_WIDTH: usize = 30;

    let total = if exact_total > 0 {
        exact_total
    } else {
        estimate.max(current)
    };
    if total == 0 {
        eprint!("\r正在渲染弹幕... 已处理 {current} 帧");
    } else {
        let pct = (current as f64 / total as f64 * 100.0) as u32;
        let filled = ((BAR_WIDTH as f64 * current as f64 / total as f64) as usize).min(BAR_WIDTH);
        let empty = BAR_WIDTH - filled;
        if exact_total > 0 && current >= exact_total {
            eprintln!(
                "\r[{}{}] {:>3}% ({current}/{total})",
                "█".repeat(filled),
                "░".repeat(empty),
                pct,
            );
        } else {
            eprint!(
                "\r[{}{}] {:>3}% ({current}/{total})",
                "█".repeat(filled),
                "░".repeat(empty),
                pct,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::{blit_cached_text, rail_hs};
    use image::{Rgb, RgbImage, RgbaImage};

    #[test]
    fn test_rail_hs_basic() {
        let positions: Vec<i64> = rail_hs(30, 3).collect();
        assert_eq!(positions, vec![0, 30, 60]);
    }

    #[test]
    fn test_rail_hs_single() {
        let positions: Vec<i64> = rail_hs(10, 1).collect();
        assert_eq!(positions, vec![0]);
    }

    #[test]
    fn test_rail_hs_zero_rails() {
        let positions: Vec<i64> = rail_hs(10, 0).collect();
        assert_eq!(positions, vec![0]);
    }

    #[test]
    fn test_rail_hs_large() {
        let positions: Vec<i64> = rail_hs(5, 4).collect();
        assert_eq!(positions, vec![0, 5, 10, 15]);
    }

    #[test]
    fn test_blit_cached_text_full_opacity() {
        let mut frame = RgbImage::new(4, 4);
        frame.fill(0);

        let mut sprite = RgbaImage::new(2, 2);
        sprite.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        sprite.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        sprite.put_pixel(0, 1, image::Rgba([0, 0, 255, 255]));
        sprite.put_pixel(1, 1, image::Rgba([128, 128, 128, 255]));

        blit_cached_text(&mut frame, &sprite, 0, 0, 1.0);

        let p0 = frame.get_pixel(0, 0);
        assert!(p0.0[0] >= 254, "red near 255");
        assert_eq!(p0.0[1], 0);
        assert_eq!(p0.0[2], 0);

        let p1 = frame.get_pixel(1, 0);
        assert_eq!(p1.0[0], 0);
        assert!(p1.0[1] >= 254, "green near 255");
        assert_eq!(p1.0[2], 0);

        assert_eq!(frame.get_pixel(0, 1), &image::Rgb([0, 0, 254]));
        assert_eq!(frame.get_pixel(1, 1), &image::Rgb([127, 127, 127]));
    }

    #[test]
    fn test_blit_cached_text_zero_opacity() {
        let mut frame = RgbImage::new(2, 2);
        frame.put_pixel(0, 0, image::Rgb([100, 100, 100]));

        let mut sprite = RgbaImage::new(2, 2);
        sprite.put_pixel(0, 0, image::Rgba([255, 255, 255, 255]));

        blit_cached_text(&mut frame, &sprite, 0, 0, 0.0);

        assert_eq!(frame.get_pixel(0, 0), &image::Rgb([100, 100, 100]));
    }

    #[test]
    fn test_blit_cached_text_partial_opacity() {
        let mut frame = RgbImage::new(1, 1);
        frame.put_pixel(0, 0, image::Rgb([255, 255, 255]));

        let mut sprite = RgbaImage::new(1, 1);
        sprite.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));

        blit_cached_text(&mut frame, &sprite, 0, 0, 0.5);

        let pixel = frame.get_pixel(0, 0);
        let v = pixel.0[0] as u32;
        assert!(v > 120 && v < 135);
    }

    #[test]
    fn test_blit_cached_text_transparent_sprite_pixels_are_skipped() {
        let mut frame = RgbImage::new(2, 2);
        frame.put_pixel(0, 0, image::Rgb([10, 20, 30]));

        let mut sprite = RgbaImage::new(1, 1);
        sprite.put_pixel(0, 0, image::Rgba([255, 0, 0, 0])); // fully transparent

        blit_cached_text(&mut frame, &sprite, 0, 0, 1.0);

        assert_eq!(frame.get_pixel(0, 0), &image::Rgb([10, 20, 30]));
    }

    #[test]
    fn test_blit_cached_text_clipping_outside_frame() {
        let mut frame = RgbImage::new(2, 2);
        frame.fill(0);

        let mut sprite = RgbaImage::new(2, 2);
        sprite.fill(255);

        blit_cached_text(&mut frame, &sprite, -2, -2, 1.0);

        assert_eq!(frame.get_pixel(0, 0), &image::Rgb([0, 0, 0]));
    }

    #[test]
    fn test_blit_cached_text_partial_clipping() {
        let mut frame = RgbImage::new(2, 2);
        frame.fill(0);

        let mut sprite = RgbaImage::new(2, 2);
        sprite.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        sprite.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        sprite.put_pixel(0, 1, image::Rgba([0, 0, 255, 255]));
        sprite.put_pixel(1, 1, image::Rgba([128, 128, 128, 255]));

        blit_cached_text(&mut frame, &sprite, -1, -1, 1.0);

        let p = frame.get_pixel(0, 0);
        assert!(
            p.0[0] >= 127 && p.0[0] <= 128,
            "gray near 128, got {:?}",
            p.0
        );
        assert!(p.0[1] >= 127 && p.0[1] <= 128);
        assert!(p.0[2] >= 127 && p.0[2] <= 128);
        assert_eq!(frame.get_pixel(1, 0), &image::Rgb([0, 0, 0]));
    }

    #[test]
    fn test_scroll_to_left() {
        let mut comp = NormalComponent {
            x: 100,
            y: Some(0),
            width: 10,
            height: 10,
            dead_line: Duration::ZERO,
            cached_text: None,
        };
        scroll(std::iter::once(&mut comp), Direction::ToLeft, 3);
        assert_eq!(comp.x, 97);
    }

    #[test]
    fn test_scroll_to_right() {
        let mut comp = NormalComponent {
            x: 100,
            y: Some(0),
            width: 10,
            height: 10,
            dead_line: Duration::ZERO,
            cached_text: None,
        };
        scroll(std::iter::once(&mut comp), Direction::ToRight, 5);
        assert_eq!(comp.x, 105);
    }

    #[test]
    fn test_scroll_only_active() {
        let mut comp1 = NormalComponent {
            x: 100,
            y: Some(10),
            width: 10,
            height: 10,
            dead_line: Duration::ZERO,
            cached_text: None,
        };
        let mut comp2 = NormalComponent {
            x: 200,
            y: None,
            width: 10,
            height: 10,
            dead_line: Duration::ZERO,
            cached_text: None,
        };

        let comps: Vec<&mut NormalComponent> = vec![&mut comp1, &mut comp2];
        let active = comps.into_iter().filter(|c| c.y.is_some());
        scroll(active, Direction::ToLeft, 2);

        assert_eq!(comp1.x, 98);
        assert_eq!(comp2.x, 200); // y is None, so it shouldn't be iterated
    }

    #[test]
    fn test_del_dead_removes_expired() {
        let dan = Danmaku {
            time: Duration::from_secs(0),
            mode: DanmakuMode::Scroll,
            font_size: 25,
            color: Rgb([255, 0, 0]),
            text: "test".into(),
        };
        let comp = NormalComponent {
            x: 0,
            y: None,
            width: 10,
            height: 10,
            dead_line: Duration::from_secs(10),
            cached_text: None,
        };

        let mut slots = GrowableVec::new(None);
        slots[0] = Some((dan.clone(), comp.clone()));

        del_dead(&mut slots, Duration::from_secs(11));
        assert!(slots[0].is_none());
    }

    #[test]
    fn test_del_dead_keeps_active() {
        let dan = Danmaku {
            time: Duration::from_secs(0),
            mode: DanmakuMode::Scroll,
            font_size: 25,
            color: Rgb([255, 0, 0]),
            text: "test".into(),
        };
        let comp = NormalComponent {
            x: 0,
            y: None,
            width: 10,
            height: 10,
            dead_line: Duration::from_secs(10),
            cached_text: None,
        };

        let mut slots = GrowableVec::new(None);
        slots[0] = Some((dan.clone(), comp));

        del_dead(&mut slots, Duration::from_secs(9));
        assert!(slots[0].is_some());
    }

    #[test]
    fn test_del_dead_exact_deadline() {
        let dan = Danmaku {
            time: Duration::from_secs(0),
            mode: DanmakuMode::Scroll,
            font_size: 25,
            color: Rgb([255, 0, 0]),
            text: "test".into(),
        };
        let comp = NormalComponent {
            x: 0,
            y: None,
            width: 10,
            height: 10,
            dead_line: Duration::from_secs(10),
            cached_text: None,
        };

        let mut slots = GrowableVec::new(None);
        slots[0] = Some((dan, comp));

        del_dead(&mut slots, Duration::from_secs(10));
        assert!(slots[0].is_none());
    }
}
