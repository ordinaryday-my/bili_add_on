use std::path::Path;

use anyhow::{anyhow, Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::{
    codec::context::Context as AvContext,
    media::Type as MediaType,
    software::scaling::{context::Context as Scaler, flag::Flags as ScalerFlags},
    util::error::EAGAIN,
};
use ndarray::Array3;

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
                rate.numerator() as f32 / rate.denominator() as f32
            } else {
                0.0
            }
        };

        let mut ctx = AvContext::new();
        ctx.set_parameters(stream.parameters())?;

        let decoder = ctx
            .decoder()
            .video()
            .context("创建视频解码器失败")?;

        let width = decoder.width();
        let height = decoder.height();
        let decoder_format = decoder.format();

        if decoder_format == ffmpeg::util::format::pixel::Pixel::None || width == 0 || height == 0
        {
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

    pub fn next_frame(&mut self) -> Result<Option<(f64, Array3<u8>)>> {
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

                    let array3 = avframe_rgb24_to_array3(&rgb)?;

                    return Ok(Some((ts_secs, array3)));
                }
                Err(ffmpeg::Error::Other { errno }) if errno == EAGAIN => {
                    if self.draining {
                        return Ok(None);
                    }
                    let found = self.drain_next_packet()?;
                    if !found {
                        self.decoder
                            .send_eof()
                            .context("解码器发送 EOF 失败")?;
                        self.draining = true;
                    }
                }
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(e) => return Err(anyhow!("解码器接收帧错误: {e}")),
            }
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

fn avframe_rgb24_to_array3(frame: &ffmpeg::util::frame::video::Video) -> Result<Array3<u8>> {
    unsafe {
        let frame_ptr = frame.as_ptr();
        let width = (*frame_ptr).width as usize;
        let height = (*frame_ptr).height as usize;

        let mut array = Array3::default((height, width, 3));

        let ret = ffmpeg::ffi::av_image_copy_to_buffer(
            array.as_mut_ptr(),
            (height * width * 3) as i32,
            (*frame_ptr).data.as_ptr() as *const *const u8,
            (*frame_ptr).linesize.as_ptr(),
            ffmpeg::util::format::pixel::Pixel::RGB24.into(),
            width as i32,
            height as i32,
            1,
        );

        if ret < 0 {
            return Err(anyhow!(
                "AVFrame 到 ndarray 转换失败: 错误码 {ret}"
            ));
        }

        Ok(array)
    }
}
