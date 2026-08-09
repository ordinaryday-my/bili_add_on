use core::panic;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use bit_set::BitSet;
use crossbeam_channel::bounded;
use image::{Rgb, RgbImage, RgbaImage};

use ffmpeg_next as ffmpeg;

use crate::{
    core::Direction::{ToLeft, ToRight},
    danmaku::{Danmaku, DanmakuMode},
    decoder::VideoDecoder,
    fonts::FontStack,
    hw,
    interaction::Args,
    utils::{blit_cached_text, sprite_ink_bounds, GrowableVec, Ignore},
};

#[allow(dead_code)]
pub(crate) enum EncoderPref {
    Auto,
    Specific(hw::HwCodec),
    Software,
}

struct StageTimings {
    scale_us: u128,
    upload_us: u128,
    send_us: u128,
    write_us: u128,
    wait_us: u128,
    frames: u64,
}

impl StageTimings {
    fn new() -> Self {
        Self {
            scale_us: 0,
            upload_us: 0,
            send_us: 0,
            write_us: 0,
            wait_us: 0,
            frames: 0,
        }
    }
    #[allow(dead_code)]
    fn report(&self) {
        if self.frames == 0 {
            return;
        }
        let total_us = self.scale_us + self.upload_us + self.send_us + self.write_us;
        let pct = |v: u128| -> f32 {
            if total_us > 0 {
                v as f32 / total_us as f32 * 100.0
            } else {
                0.0
            }
        };
        eprintln!(
            "[timing] frames={} avg_total={:.3}ms avg_wait={:.3}ms | scale={:.3}ms({:.0}%) upload={:.3}ms({:.0}%) send={:.3}ms({:.0}%) write={:.3}ms({:.0}%)",
            self.frames,
            total_us as f64 / self.frames as f64 / 1000.0,
            self.wait_us as f64 / self.frames as f64 / 1000.0,
            self.scale_us as f64 / self.frames as f64 / 1000.0,
            pct(self.scale_us),
            self.upload_us as f64 / self.frames as f64 / 1000.0,
            pct(self.upload_us),
            self.send_us as f64 / self.frames as f64 / 1000.0,
            pct(self.send_us),
            self.write_us as f64 / self.frames as f64 / 1000.0,
            pct(self.write_us),
        );
    }
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
    timings: StageTimings,
}

unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    fn new(
        path: &Path,
        width: u32,
        height: u32,
        frame_rate: f32,
        encoder_pref: EncoderPref,
        x264_preset: &str,
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
                x264_preset,
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
                    .context("创建像素格式转换器失败（RGB 转编码像素格式）")?;

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
                        timings: StageTimings::new(),
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
        x264_preset: &str,
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

        let encoder = if hw_codec.is_none() {
            let mut x264_opts = ffmpeg::Dictionary::new();
            x264_opts.set("preset", x264_preset);
            encoder
                .open_with(x264_opts)
                .map_err(|e| anyhow!("打开 libx264 编码器失败: {e:?}"))?
        } else {
            encoder
                .open()
                .map_err(|e| anyhow!("打开 {} 编码器失败: {e:?}", hw_codec.unwrap().encoder_name()))?
        };
        let encoder_time_base = encoder.time_base();

        Ok((encoder, encoder_time_base, hw_setup))
    }

    fn encode(&mut self, image: &RgbImage, timestamp_secs: f64) -> Result<()> {
        let (width, height) = image.dimensions();
        let raw = image.as_raw();

        let t0 = Instant::now();
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
        let t1 = Instant::now();

        let mut upload_done = t1;
        let send_result = if let Some(ref hw_setup) = self.hw_setup {
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
            upload_done = Instant::now();
            hw_frame.set_pts(Some(pts));

            if self.frame_count.is_multiple_of(12) {
                hw_frame.set_kind(ffmpeg::util::picture::Type::I);
            }

            self.frame_count += 1;

            self.encoder
                .send_frame(&hw_frame)
                .context("发送帧到硬件编码器失败")
        } else {
            self.sw_frame_yuv.set_pts(Some(pts));

            if self.frame_count.is_multiple_of(12) {
                self.sw_frame_yuv.set_kind(ffmpeg::util::picture::Type::I);
            }

            self.frame_count += 1;

            self.encoder
                .send_frame(&self.sw_frame_yuv)
                .context("发送帧到 H.264 编码器失败")
        };
        let t2 = Instant::now();
        send_result?;

        self.receive_and_write()
            .with_context(|| format!("接收并写入编码包失败 (帧 #{})", self.frame_count))?;
        let t3 = Instant::now();

        self.timings.scale_us += (t1 - t0).as_micros();
        self.timings.upload_us += (upload_done - t1).as_micros();
        self.timings.send_us += (t2 - upload_done).as_micros();
        self.timings.write_us += (t3 - t2).as_micros();
        self.timings.frames += 1;

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
                    break;
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(anyhow!("编码器接收包错误: {e}")),
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        // self.timings.report();
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
    fonts: &mut FontStack,
    args: &Args,
    video_width: u32,
    frame_duration_secs: f64,
) -> f64 {
    let mut max_deadline = 0.0f64;
    for dan in danmakus {
        let deadline_secs = match dan.mode {
            DanmakuMode::Scroll | DanmakuMode::Reverse => {
                let text_width =
                    fonts.text_width(&dan.text, (dan.font_size as f32) * args.font_scale);
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

const PROGRESS_INTERVAL_MS: u64 = 100;

pub(crate) fn video_process(
    mut decoder: VideoDecoder,
    mut encoder: FfmpegEncoder,
    mut danmakus: Vec<Danmaku>,
    args: &Args,
    frame_duration_secs: f64,
    range: Option<(f64, f64)>,
) -> Result<()> {
    if let Some((start, end)) = range {
        decoder.set_range(start, end);
        danmakus.retain(|dan| {
            let t = dan.time.as_secs_f64();
            t >= start && t < end
        });
        let shift = Duration::from_secs_f64(start);
        for dan in &mut danmakus {
            dan.time = dan.time.saturating_sub(shift);
        }
    }

    danmakus.sort_unstable_by_key(|dan| std::cmp::Reverse(dan.time));

    let (video_width, video_height) = decoder.size();
    let area_top = (video_height as f64 * args.top_ratio) as u32;
    let area_bottom = (video_height as f64 * args.bottom_ratio) as u32;
    let area_height = area_bottom - area_top;

    let mut scroll_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut top_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut bottom_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut reverse_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);

    let mut fonts = FontStack::load(args).context("字体加载失败")?;

    // 标准字号（25 × font_scale）参考墨迹高度：轨道基准间距 = 墨迹高度 + line_spacing，
    // 保证标准字号弹幕的相邻行视觉间隙恰为 line_spacing（轨道间无死区）。
    let sample_img = fonts.render_sprite("字", 25.0 * args.font_scale, Rgb([255, 255, 255]));
    let ink_ref = sprite_ink_bounds(&sample_img)
        .map(|(t, b)| b.saturating_sub(t))
        .unwrap_or(1)
        .max(1);
    let base_pitch = ink_ref.saturating_add(args.line_spacing).max(1);
    let rail_cnt = area_height / base_pitch;

    let mut frame_count = 0u64;
    let mut total_frames = match range {
        Some((start, end)) => ((end - start) / frame_duration_secs).ceil() as u64,
        None => decoder.frame_count(),
    };
    let total_reporter = Arc::new(AtomicU64::new(0));

    if args.longest {
        let video_duration = match range {
            Some((start, end)) => end - start,
            None if decoder.frame_rate() > 0.0 => {
                decoder.frame_count() as f64 / decoder.frame_rate() as f64
            }
            _ => 0.0,
        };
        let max_deadline = compute_max_danmaku_deadline(
            &danmakus,
            &mut fonts,
            args,
            video_width,
            frame_duration_secs,
        );
        if max_deadline > video_duration {
            let ext_stop_orig = match range {
                Some((start, end)) => (max_deadline + start).min(end),
                None => max_deadline,
            };
            decoder.set_extend_to(ext_stop_orig, frame_duration_secs);
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
                let mut last_progress =
                    Instant::now() - Duration::from_millis(PROGRESS_INTERVAL_MS);
                let mut final_shown = false;
                loop {
                    let Ok((ts_secs, dur, mut image)) = decode_r.recv() else {
                        break;
                    };
                    let ready_idx = danmakus.partition_point(|dan| dan.time > dur);
                    let enqueue = danmakus.drain(ready_idx..).rev();

                    for d in enqueue {
                        let scale = (d.font_size as f32) * args.font_scale;
                        let color = d.color;
                        let cached_text = fonts.render_sprite(&d.text, scale, color);
                        let width = cached_text.width();
                        // 虚拟轨道数：该字号墨迹高度需要几个基础轨道（B站虚拟轨道机制）
                        let n_rails = compute_n_rails(ink_ref, base_pitch, d.font_size);
                        let (travel, dead_line) = match d.mode {
                            DanmakuMode::Scroll | DanmakuMode::Reverse => {
                                let travel_frames = (width + video_width).div_ceil(args.speed);
                                let travel = Duration::from_secs_f64(
                                    travel_frames as f64 * frame_duration_secs,
                                );
                                (travel, dur + travel)
                            }
                            DanmakuMode::Top | DanmakuMode::Bottom => {
                                let travel = Duration::from_secs_f64(args.fixed_duration);
                                (travel, dur + travel)
                            }
                            _ => (Duration::ZERO, Duration::from_millis(0)),
                        };
                        match d.mode {
                            DanmakuMode::Scroll => scroll_slots
                                .set_first_empty((
                                    d,
                                    NormalComponent {
                                        x: video_width as i64,
                                        y: None,
                                        width,
                                        n_rails,
                                        travel,
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
                                        n_rails,
                                        travel,
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
                                        n_rails,
                                        travel,
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
                                        n_rails,
                                        travel,
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
                        base_pitch,
                        rail_cnt,
                        area_top: area_top as i64,
                        opacity: args.opacity,
                        min_space: args.min_space as i64,
                        now: dur,
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

                    draw_fixed_danmukus(&mut draw_params, &mut top_slots, false);
                    draw_fixed_danmukus(&mut draw_params, &mut bottom_slots, true);

                    if encode_s.send((image, ts_secs)).is_err() {
                        break;
                    }
                    frame_count += 1;
                    if !args.quiet {
                        let exact = total_reporter.load(Ordering::Relaxed);
                        let is_final = exact > 0 && frame_count >= exact;
                        let now = Instant::now();
                        if is_final
                            || now - last_progress >= Duration::from_millis(PROGRESS_INTERVAL_MS)
                        {
                            render_progress(frame_count, total_frames, exact);
                            last_progress = now;
                            if is_final {
                                final_shown = true;
                            }
                        }
                    }
                }

                // 收尾刷新：解码线程在通道关闭前已写入精确总数，
                // 补打一次确保进度条走到 100%
                if !args.quiet && !final_shown {
                    let exact = total_reporter.load(Ordering::Relaxed);
                    render_progress(frame_count, total_frames, exact);
                    if exact == 0 || frame_count < exact {
                        // 异常路径：收尾行未带换行，补一个避免后续输出粘连
                        eprintln!();
                    }
                }

                Ok(())
            });

        let encode_customer = thread::Builder::new()
            .name("encode".to_string())
            .spawn_scoped(s, move || -> Result<()> {
                loop {
                    let t0 = Instant::now();
                    let Ok((image, ts_secs)) = encode_v.recv() else {
                        break;
                    };
                    encoder.timings.wait_us += t0.elapsed().as_micros();

                    encoder
                        .encode(&image, ts_secs)
                        .with_context(|| format!("编码帧失败 (时间戳: {ts_secs})"))?;
                    let _ = recycle_s.send(image);
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
        handles
            .into_iter()
            .map(|handle| match handle.join() {
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
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(())
    })?;

    Ok(())
}

struct DrawParams<'a> {
    image: &'a mut RgbImage,
    base_pitch: u32,
    rail_cnt: u32,
    area_top: i64,
    opacity: f64,
    min_space: i64,
    now: Duration,
}

const RAIL_OFFSET: usize = 1000;

/// 在轨道占用位图中查找第一条空闲的虚拟轨道（连续 `n_rails` 条基础轨道）。
///
/// `from_bottom == true` 时从底部向上扫描（底部固定弹幕），否则自上而下。
/// 返回基础轨道序号，找不到返回 `None`。
fn find_free_track(
    occupieds: &BitSet,
    n_rails: u32,
    rail_cnt: u32,
    from_bottom: bool,
) -> Option<u32> {
    let last_start = rail_cnt.saturating_sub(n_rails);
    let mut i = if from_bottom { last_start } else { 0 };
    loop {
        let free = (0..n_rails).all(|k| !occupieds.contains(RAIL_OFFSET + (i + k) as usize));
        if free {
            return Some(i);
        }
        if from_bottom {
            if i == 0 {
                break;
            }
            i -= 1;
        } else {
            if i >= last_start {
                break;
            }
            i += 1;
        }
    }
    None
}

/// 计算某字号的虚拟轨道数：该字号墨迹高度需要几个基础轨道（B站虚拟轨道机制）。
///
/// `ink_ref` 为标准字号（25）在 `font_scale` 下的墨迹高度；其余字号按比例缩放。
fn compute_n_rails(ink_ref: u32, base_pitch: u32, font_size: usize) -> u32 {
    let ink_h_font = (ink_ref as u64 * font_size as u64).div_ceil(25) as u32;
    ink_h_font.div_ceil(base_pitch).max(1)
}

fn mark_track_occupied(occupieds: &mut BitSet, track: u32, n_rails: u32) {
    for k in 0..n_rails {
        occupieds.insert(RAIL_OFFSET + (track + k) as usize);
    }
}

fn draw_fixed_danmukus(
    params: &mut DrawParams,
    fixed_slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    from_bottom: bool,
) {
    let cap = fixed_slots.len() + RAIL_OFFSET;
    let mut occupieds = BitSet::with_capacity(cap);
    let mut ensure_y_q = Vec::new();
    let mut pending_y: Vec<(i64, u32)> = Vec::new();
    let mut drop_q = Vec::new();
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
                    Some((comp.y.unwrap(), comp.n_rails))
                } else {
                    None
                }
            })
        });

        for (y, n_rails) in raw_occupieds {
            debug_assert_eq!(y % params.base_pitch as i64, 0);
            let track = (y / params.base_pitch as i64) as u32;
            mark_track_occupied(&mut occupieds, track, n_rails);
        }

        for &(y, n_rails) in &pending_y {
            let track = (y / params.base_pitch as i64) as u32;
            mark_track_occupied(&mut occupieds, track, n_rails);
        }

        // B站式丢弃：延迟放置的弹幕剩余寿命不足，直接不显示
        if comp.dead_line.saturating_sub(params.now) < comp.travel {
            drop_q.push(idx);
            occupieds.reset();
            continue;
        }

        let Some(track) = find_free_track(&occupieds, comp.n_rails, params.rail_cnt, from_bottom)
        else {
            occupieds.reset();
            continue;
        };
        let y = track as i64 * params.base_pitch as i64;

        blit_cached_text(
            params.image,
            comp.cached_text.as_ref().unwrap(),
            comp.x as i32,
            (y + params.area_top) as i32,
            params.opacity,
        );
        pending_y.push((y, comp.n_rails));
        ensure_y_q.push((idx, y));
        occupieds.reset();
    }

    for (idx, y) in ensure_y_q {
        fixed_slots[idx].as_mut().unwrap().1.y = Some(y);
    }
    for idx in drop_q {
        fixed_slots[idx] = None;
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
    let cap = scroll_slots.len() + RAIL_OFFSET;
    let mut occupieds = BitSet::with_capacity(cap);
    let mut ensure_y_q = Vec::new();
    let mut pending_y: Vec<(i64, u32)> = Vec::new();
    let mut drop_q = Vec::new();
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
                    Some((comp.width, comp.x, comp.y.unwrap(), comp.n_rails))
                } else {
                    None
                }
            })
        });

        for (width, x, y, n_rails) in raw_occupieds {
            debug_assert_eq!(y % params.base_pitch as i64, 0);
            let overlap = match dir {
                ToLeft => width as i64 + x + params.min_space > comp.x,
                ToRight => x < comp.x + comp.width as i64 + params.min_space,
            };
            if !overlap {
                continue;
            }
            let track = (y / params.base_pitch as i64) as u32;
            mark_track_occupied(&mut occupieds, track, n_rails);
        }

        for &(y, n_rails) in &pending_y {
            let track = (y / params.base_pitch as i64) as u32;
            mark_track_occupied(&mut occupieds, track, n_rails);
        }

        // B站式丢弃：延迟放置的弹幕剩余寿命不足以完整滚出，直接不显示
        if comp.dead_line.saturating_sub(params.now) < comp.travel {
            drop_q.push(idx);
            occupieds.reset();
            continue;
        }

        let Some(track) = find_free_track(&occupieds, comp.n_rails, params.rail_cnt, false) else {
            occupieds.reset();
            continue;
        };
        let y = track as i64 * params.base_pitch as i64;

        blit_cached_text(
            params.image,
            comp.cached_text.as_ref().unwrap(),
            comp.x as i32,
            (y + params.area_top) as i32,
            params.opacity,
        );
        pending_y.push((y, comp.n_rails));
        ensure_y_q.push((idx, y));
        occupieds.reset();
    }

    for (idx, y) in ensure_y_q {
        scroll_slots[idx].as_mut().unwrap().1.y = Some(y);
    }
    for idx in drop_q {
        scroll_slots[idx] = None;
    }
}

#[derive(Debug, Clone)]
struct NormalComponent {
    x: i64,
    y: Option<i64>,
    width: u32,
    n_rails: u32,
    travel: Duration,
    dead_line: Duration,
    cached_text: Option<RgbaImage>,
}

pub(crate) fn same_specifications(
    decoder: &VideoDecoder,
    path: impl AsRef<Path>,
    encoder_pref: EncoderPref,
    x264_preset: &str,
) -> anyhow::Result<(FfmpegEncoder, f64)> {
    let path = path.as_ref();

    let (width, height) = decoder.size();
    let frame_rate = decoder.frame_rate();
    if frame_rate <= 0.0 {
        return Err(anyhow!("视频帧率无效: {frame_rate}，无法确定每帧持续时间"));
    }
    let encoder = FfmpegEncoder::new(path, width, height, frame_rate, encoder_pref, x264_preset)?;

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
    use crate::utils::blit_cached_text;
    use image::{Rgb, RgbImage, RgbaImage};

    #[test]
    fn test_find_free_track_basic() {
        let mut occ = BitSet::new();
        mark_track_occupied(&mut occ, 1, 2);
        assert_eq!(find_free_track(&occ, 1, 10, false), Some(0));
        assert_eq!(find_free_track(&occ, 2, 10, false), Some(3));
        assert_eq!(find_free_track(&occ, 3, 10, false), Some(3));
    }

    #[test]
    fn test_find_free_track_from_bottom() {
        let mut occ = BitSet::new();
        mark_track_occupied(&mut occ, 7, 1);
        assert_eq!(find_free_track(&occ, 1, 10, true), Some(9));
        assert_eq!(find_free_track(&occ, 3, 10, true), Some(4));
    }

    #[test]
    fn test_find_free_track_none() {
        let mut occ = BitSet::new();
        for t in 0..10 {
            mark_track_occupied(&mut occ, t, 1);
        }
        assert_eq!(find_free_track(&occ, 1, 10, false), None);
        assert_eq!(find_free_track(&occ, 1, 10, true), None);
    }

    #[test]
    fn test_compute_n_rails_bilibili_table() {
        // ink_ref=32（scale 2 实测）、base_pitch=36：18→1, 25→1, 36→2, 45→2, 64→3
        let n = |s: usize| compute_n_rails(32, 36, s);
        assert_eq!(n(18), 1);
        assert_eq!(n(25), 1);
        assert_eq!(n(36), 2);
        assert_eq!(n(45), 2);
        assert_eq!(n(64), 3);
    }

    #[test]
    fn test_compute_n_rails_min_one() {
        assert_eq!(compute_n_rails(32, 36, 1), 1);
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
            n_rails: 1,
            travel: Duration::ZERO,
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
            n_rails: 1,
            travel: Duration::ZERO,
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
            n_rails: 1,
            travel: Duration::ZERO,
            dead_line: Duration::ZERO,
            cached_text: None,
        };
        let mut comp2 = NormalComponent {
            x: 200,
            y: None,
            width: 10,
            n_rails: 1,
            travel: Duration::ZERO,
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
            n_rails: 1,
            travel: Duration::ZERO,
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
            n_rails: 1,
            travel: Duration::ZERO,
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
            n_rails: 1,
            travel: Duration::ZERO,
            dead_line: Duration::from_secs(10),
            cached_text: None,
        };

        let mut slots = GrowableVec::new(None);
        slots[0] = Some((dan, comp));

        del_dead(&mut slots, Duration::from_secs(10));
        assert!(slots[0].is_none());
    }

    fn danmaku_comp(
        dead_line: Duration,
        travel: Duration,
        mode: DanmakuMode,
    ) -> (Danmaku, NormalComponent) {
        let dan = Danmaku {
            time: Duration::from_secs(0),
            mode,
            font_size: 25,
            color: Rgb([255, 255, 255]),
            text: "测试".into(),
        };
        let comp = NormalComponent {
            x: 100,
            y: None,
            width: 10,
            n_rails: 1,
            travel,
            dead_line,
            cached_text: Some(RgbaImage::new(10, 10)),
        };
        (dan, comp)
    }

    fn test_draw_params(image: &mut RgbImage, now: Duration) -> DrawParams<'_> {
        DrawParams {
            image,
            base_pitch: 10,
            rail_cnt: 4,
            area_top: 0,
            opacity: 1.0,
            min_space: 0,
            now,
        }
    }

    #[test]
    fn test_scroll_placed_when_remaining_equals_travel() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        slots[0] = Some(danmaku_comp(
            Duration::from_secs(10),
            Duration::from_secs(5),
            DanmakuMode::Scroll,
        ));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_scroll_danmukus(&mut params, &mut slots, ToLeft);
        let y = slots[0].as_ref().unwrap().1.y;
        assert_eq!(y, Some(0), "剩余寿命足够时应放置");
    }

    #[test]
    fn test_scroll_dropped_when_remaining_less_than_travel() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        slots[0] = Some(danmaku_comp(
            Duration::from_secs(8),
            Duration::from_secs(5),
            DanmakuMode::Scroll,
        ));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_scroll_danmukus(&mut params, &mut slots, ToLeft);
        assert!(slots[0].is_none(), "剩余寿命(3s) < travel(5s) 时应丢弃");
    }

    #[test]
    fn test_scroll_kept_when_placed_immediately() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        // 第一帧即放置：now == 入队时刻
        let (dan, comp) = danmaku_comp(
            Duration::from_secs(10),
            Duration::from_secs(5),
            DanmakuMode::Scroll,
        );
        slots[0] = Some((dan, comp));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_scroll_danmukus(&mut params, &mut slots, ToLeft);
        assert!(slots[0].is_some());
        assert_eq!(slots[0].as_ref().unwrap().1.y, Some(0));
    }

    #[test]
    fn test_fixed_dropped_when_remaining_less_than_travel() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        slots[0] = Some(danmaku_comp(
            Duration::from_secs(8),
            Duration::from_secs(5),
            DanmakuMode::Top,
        ));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_fixed_danmukus(&mut params, &mut slots, false);
        assert!(slots[0].is_none(), "固定弹幕延迟放置同样丢弃");
    }

    #[test]
    fn test_fixed_kept_when_remaining_equals_travel() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        slots[0] = Some(danmaku_comp(
            Duration::from_secs(10),
            Duration::from_secs(5),
            DanmakuMode::Top,
        ));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_fixed_danmukus(&mut params, &mut slots, false);
        assert_eq!(slots[0].as_ref().unwrap().1.y, Some(0));
    }
}
