use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, Context, Result};
use ffmpeg::{
    codec::context::Context as AvContext,
    media::Type as MediaType,
    software::scaling::{context::Context as Scaler, flag::Flags as ScalerFlags},
    util::error::EAGAIN,
};
use ffmpeg_next as ffmpeg;
use image::RgbImage;

pub struct VideoDecoder {
    input: ffmpeg::format::context::input::Input,
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    scaler: Scaler,
    width: u32,
    height: u32,
    frame_rate: f32,
    draining: bool,
    last_ts: Option<f64>,
    frame_interval: f64,
    extend_max_deadline: Option<f64>,
    extend_frame_duration: f64,
    real_frame_count: u64,
    extended_count: u64,
    total_reporter: Option<Arc<AtomicU64>>,
    total_reported: bool,
    range: Option<(f64, f64)>,
    seeked: bool,
}

impl VideoDecoder {
    pub fn new(path: &Path) -> Result<Self> {
        let input = ffmpeg::format::input(path)
            .with_context(|| format!("无法打开视频文件: {}", path.display()))?;

        let stream = input
            .streams()
            .best(MediaType::Video)
            .context("找不到可用的视频流")?;
        let stream_index = stream.index();
        let stream_time_base = stream.time_base();

        let frame_rate = {
            let rate = stream.rate();
            if rate.denominator() > 0 {
                let r_frame_rate = rate.numerator() as f32 / rate.denominator() as f32;

                let frames = stream.frames();
                let raw_duration = stream.duration();
                if frames > 0 && raw_duration > 0 {
                    let dur_secs = raw_duration as f64 * stream_time_base.numerator() as f64
                        / stream_time_base.denominator() as f64;
                    if dur_secs > 0.0 {
                        frames as f32 / dur_secs as f32
                    } else {
                        r_frame_rate
                    }
                } else {
                    r_frame_rate
                }
            } else {
                0.0
            }
        };

        let mut ctx = AvContext::new();
        ctx.set_parameters(stream.parameters())?;

        let decoder = ctx.decoder().video().context("创建视频解码器失败")?;

        let width = decoder.width();
        let height = decoder.height();
        let decoder_format = decoder.format();

        if decoder_format == ffmpeg::util::format::pixel::Pixel::None || width == 0 || height == 0 {
            return Err(anyhow!(
                "视频流参数无效: 格式={decoder_format:?}, 尺寸={width}x{height}"
            ));
        }

        let scaler = Scaler::get(
            decoder_format,
            width,
            height,
            ffmpeg::util::format::pixel::Pixel::RGB24,
            width,
            height,
            ScalerFlags::empty(),
        )
        .context("创建解码器像素格式转换器失败")?;

        Ok(Self {
            input,
            decoder,
            stream_index,
            stream_time_base,
            scaler,
            width,
            height,
            frame_rate,
            draining: false,
            last_ts: None,
            frame_interval: 0.0,
            extend_max_deadline: None,
            extend_frame_duration: 0.0,
            real_frame_count: 0,
            extended_count: 0,
            total_reporter: None,
            total_reported: false,
            range: None,
            seeked: false,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn frame_rate(&self) -> f32 {
        self.frame_rate
    }

    pub fn frame_count(&self) -> u64 {
        self.input
            .stream(self.stream_index)
            .map(|s| s.frames().max(0) as u64)
            .unwrap_or(0)
    }

    pub fn set_extend_to(&mut self, max_deadline_secs: f64, frame_duration_secs: f64) {
        self.extend_max_deadline = Some(max_deadline_secs);
        self.extend_frame_duration = frame_duration_secs;
    }

    pub fn set_total_reporter(&mut self, reporter: Arc<AtomicU64>) {
        self.total_reporter = Some(reporter);
    }

    /// 设置处理时段 `[start, end)`（原始时间轴，秒）。
    ///
    /// 首次读取时跳转到起始附近并丢弃之前的帧；读取到达 `end` 后停止；
    /// 返回的时间戳统一减去 `start`（输出时间轴从 0 开始）。
    pub fn set_range(&mut self, start: f64, end: f64) {
        debug_assert!(end > start);
        self.range = Some((start, end));
    }

    fn report_total_once(&mut self) {
        if !self.total_reported {
            self.report_total();
            self.total_reported = true;
        }
    }

    fn seek_to_range_start(&mut self) -> Result<()> {
        let (start, _) = self.range.unwrap();
        if start > 0.0 {
            let target = (start * ffmpeg::ffi::AV_TIME_BASE as f64) as i64;
            self.input
                .seek(target, ..target.saturating_add(1))
                .context("跳转到起始时间失败")?;
            self.decoder.flush();
        }
        self.draining = false;
        self.last_ts = None;
        self.seeked = true;
        Ok(())
    }

    pub fn next_frame_into(&mut self, reuse: &mut RgbImage) -> Result<Option<f64>> {
        assert_eq!(reuse.dimensions(), (self.width, self.height));
        self.read_into(reuse)
    }

    fn read_into(&mut self, image: &mut RgbImage) -> Result<Option<f64>> {
        if self.range.is_some() && !self.seeked {
            self.seek_to_range_start()?;
        }
        let range = self.range;

        let mut decoded = ffmpeg::util::frame::video::Video::empty();

        loop {
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    let mut rgb = ffmpeg::util::frame::video::Video::empty();
                    self.scaler
                        .run(&decoded, &mut rgb)
                        .context("解码帧 RGB 转换失败")?;

                    let ts_secs = decoded
                        .pts()
                        .map(|pts| {
                            pts as f64 * self.stream_time_base.numerator() as f64
                                / self.stream_time_base.denominator() as f64
                        })
                        .unwrap_or(0.0);

                    if let Some((start, end)) = range {
                        if ts_secs < start {
                            continue;
                        }
                        if ts_secs >= end {
                            // 提前停止路径上扩展帧不可达（未到 EOF），
                            // 清除扩展上限避免总数虚报扩展帧。
                            self.extend_max_deadline = None;
                            self.report_total_once();
                            return Ok(None);
                        }
                    }

                    if let Some(prev) = self.last_ts {
                        let interval = ts_secs - prev;
                        if interval > 0.0 {
                            self.frame_interval = interval;
                        }
                    }
                    self.last_ts = Some(ts_secs);
                    self.real_frame_count += 1;

                    avframe_rgb24_to_image(&rgb, image)?;

                    let shifted = match range {
                        Some((start, _)) => ts_secs - start,
                        None => ts_secs,
                    };
                    return Ok(Some(shifted));
                }
                Err(ffmpeg::Error::Other { errno }) if errno == EAGAIN => {
                    if self.draining {
                        return self.maybe_extend(image);
                    }
                    let found = self.drain_next_packet()?;
                    if !found {
                        self.decoder.send_eof().context("解码器发送 EOF 失败")?;
                        self.draining = true;
                    }
                }
                Err(ffmpeg::Error::Eof) => return self.maybe_extend(image),
                Err(e) => return Err(anyhow!("解码器接收帧错误: {e}")),
            }
        }
    }

    fn maybe_extend(&mut self, image: &mut RgbImage) -> Result<Option<f64>> {
        self.report_total_once();

        if let Some(max_deadline) = self.extend_max_deadline {
            let step = self.extension_step();
            let stop_at = max_deadline + step.max(self.extend_frame_duration);
            let next_ts = self.last_ts.map_or(0.0, |t| t + step);
            if next_ts < stop_at {
                if let Some((_, end)) = self.range {
                    if next_ts >= end {
                        return Ok(None);
                    }
                }
                self.last_ts = Some(next_ts);
                self.extended_count += 1;
                image.fill(0);
                return Ok(Some(next_ts));
            }
        }
        Ok(None)
    }

    fn extension_step(&self) -> f64 {
        if self.frame_interval > 0.0 {
            self.frame_interval
        } else if self.frame_rate > 0.0 {
            1.0 / self.frame_rate as f64
        } else {
            self.extend_frame_duration
        }
    }

    fn report_total(&mut self) {
        if let Some(reporter) = &self.total_reporter {
            let total = if let Some(max_deadline) = self.extend_max_deadline {
                let step = self.extension_step();
                let stop_at = max_deadline + step.max(self.extend_frame_duration);
                let mut extended = 0u64;
                let mut ts = self.last_ts.map_or(0.0, |t| t);
                while ts + step < stop_at {
                    ts += step;
                    extended += 1;
                }
                self.real_frame_count + extended
            } else {
                self.real_frame_count
            };
            reporter.store(total, Ordering::Relaxed);
        }
    }

    fn drain_next_packet(&mut self) -> Result<bool> {
        for (stream, packet) in self.input.packets() {
            if stream.index() == self.stream_index {
                self.decoder
                    .send_packet(&packet)
                    .context("发送数据包到解码器失败")?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

unsafe impl Send for VideoDecoder {}

fn avframe_rgb24_to_image(
    frame: &ffmpeg::util::frame::video::Video,
    image: &mut RgbImage,
) -> Result<()> {
    unsafe {
        let frame_ptr = frame.as_ptr();
        let width = (*frame_ptr).width as u32;
        let height = (*frame_ptr).height as u32;

        let ret = ffmpeg::ffi::av_image_copy_to_buffer(
            image.as_mut_ptr(),
            (width * height * 3) as i32,
            (*frame_ptr).data.as_ptr() as *const *const u8,
            (*frame_ptr).linesize.as_ptr(),
            ffmpeg::util::format::pixel::Pixel::RGB24.into(),
            width as i32,
            height as i32,
            1,
        );

        if ret < 0 {
            return Err(anyhow!("AVFrame 到 RGB 图像转换失败: 错误码 {ret}"));
        }

        Ok(())
    }
}
