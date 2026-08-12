use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use image::RgbImage;

use ffmpeg_next as ffmpeg;

use crate::hw;

#[allow(dead_code)]
pub(crate) enum EncoderPref {
    Auto,
    Specific(hw::HwCodec),
    Software,
}

pub(crate) struct StageTimings {
    scale_us: u128,
    upload_us: u128,
    send_us: u128,
    write_us: u128,
    pub(crate) wait_us: u128,
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
    pub(crate) timings: StageTimings,
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
            encoder.open().map_err(|e| {
                anyhow!(
                    "打开 {} 编码器失败: {e:?}",
                    hw_codec.unwrap().encoder_name()
                )
            })?
        };
        let encoder_time_base = encoder.time_base();

        Ok((encoder, encoder_time_base, hw_setup))
    }

    pub(crate) fn encode(&mut self, image: &RgbImage, timestamp_secs: f64) -> Result<()> {
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

    pub(crate) fn finish(&mut self) -> Result<()> {
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

pub(crate) fn same_specifications(
    decoder: &crate::decoder::VideoDecoder,
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
