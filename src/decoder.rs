use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
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
    seekable: bool,
    normalize_start: bool,
    base_ts: Option<f64>,
}

impl VideoDecoder {
    /// 以可选输入格式提示、选项字典、帧率兜底与时间线归一化创建解码器。
    ///
    /// - `format`: 输入格式名（如 `dshow`、`gdigrab`），用于采集设备等无法探测的输入；
    ///   `None` 时按路径探测（与 `new` 一致）
    /// - `options`: 传递给输入格式的选项字典
    /// - `fps_fallback`: 帧率无法从流信息推断时的兜底值（部分采集设备不报帧率）
    /// - `normalize_start`: 将首帧时间戳归一化为 0（用于 PTS 不从 0 起的实时输入）
    pub fn new_with_format(
        path: &Path,
        format: Option<&str>,
        options: ffmpeg::Dictionary,
        fps_fallback: Option<f32>,
        normalize_start: bool,
    ) -> Result<Self> {
        let input = match format {
            Some(name) => {
                let c_name = std::ffi::CString::new(name)
                    .map_err(|_| anyhow!("输入格式名包含非法字符: {name}"))?;
                let fmt_ptr = unsafe { ffmpeg::ffi::av_find_input_format(c_name.as_ptr()) };
                if fmt_ptr.is_null() {
                    bail!("找不到输入格式: {name}（请确认 ffmpeg 编译包含该格式）");
                }
                let input_fmt = unsafe { ffmpeg::format::format::Input::wrap(fmt_ptr as *mut _) };
                match ffmpeg::format::open_with(path, &ffmpeg::Format::Input(input_fmt), options)
                    .with_context(|| format!("无法打开采集设备: {}", path.display()))?
                {
                    ffmpeg::format::Context::Input(i) => i,
                    _ => unreachable!(),
                }
            }
            None => ffmpeg::format::input(path)
                .with_context(|| format!("无法打开视频文件: {}", path.display()))?,
        };

        let stream = input
            .streams()
            .best(MediaType::Video)
            .context("找不到可用的视频流")?;
        let stream_index = stream.index();
        let stream_time_base = stream.time_base();

        let frame_rate = {
            let rate = stream.rate();
            let r_frame_rate = if rate.denominator() > 0 {
                rate.numerator() as f32 / rate.denominator() as f32
            } else {
                0.0
            };

            let mut fr = if r_frame_rate > 0.0 {
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
            };

            // 非可寻址输入（如 stdin 管道）可能无法推断 r_frame_rate，
            // 回退到 avg_frame_rate，保证时间线（帧间隔/PTS）可用。
            if fr <= 0.0 {
                let avg = stream.avg_frame_rate();
                if avg.denominator() > 0 {
                    fr = avg.numerator() as f32 / avg.denominator() as f32;
                }
            }
            // 采集设备等实时输入可能仍无帧率信息，使用调用方兜底值。
            if fr <= 0.0 {
                fr = fps_fallback.unwrap_or(0.0);
            }
            fr
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

        // 判断底层协议是否可随机寻址（普通文件可寻址，stdin 管道/网络流不可寻址）。
        // 不可寻址时 --range 起始时间只能靠逐帧丢弃到达，无法 seek。
        let seekable = unsafe {
            const AVIO_SEEKABLE_NORMAL: i32 = 1;
            let fmt = input.as_ptr();
            !(*fmt).pb.is_null() && (*(*fmt).pb).seekable & AVIO_SEEKABLE_NORMAL != 0
        };

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
            seekable,
            normalize_start,
            base_ts: None,
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
        if start > 0.0 && self.seekable {
            let target = (start * ffmpeg::ffi::AV_TIME_BASE as f64) as i64;
            self.input
                .seek(target, ..target.saturating_add(1))
                .context("跳转到起始时间失败")?;
            self.decoder.flush();
        }
        // 不可寻址输入（stdin 管道）无法 seek，由 read_into 中的逐帧丢弃
        // （ts < start 时 continue）到达起始时间。
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

                    let mut ts_secs = decoded
                        .pts()
                        .map(|pts| {
                            pts as f64 * self.stream_time_base.numerator() as f64
                                / self.stream_time_base.denominator() as f64
                        })
                        .unwrap_or(0.0);

                    // 实时输入（采集设备）的 PTS 可能不从 0 起，
                    // 将首帧时间戳归一化为 0，保证 --range 结束判断与弹幕时间轴对齐。
                    if self.normalize_start {
                        let base = *self.base_ts.get_or_insert(ts_secs);
                        ts_secs -= base;
                    }

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
                if let Some((_, end)) = self.range
                    && next_ts >= end
                {
                    return Ok(None);
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

/// 列出指定采集格式的可用设备（模拟 `ffmpeg -list_devices true -f <格式> -i dummy`）。
///
/// 支持 `dshow` / `avfoundation`：设备列表由解复用器通过 `av_log`（INFO 级）打印到 stderr，
/// 打开动作预期失败，列表打印后正常返回。`gdigrab` / `x11grab` / `v4l2` 无设备列表，
/// 打印使用提示。
pub fn list_devices(format: &str) -> Result<()> {
    match format {
        "gdigrab" | "x11grab" => {
            eprintln!(
                "{format} 无设备列表：屏幕捕获直接指定 URL，如 gdigrab:desktop（窗口捕获可用 gdigrab:窗口标题）"
            );
            return Ok(());
        }
        "v4l2" => {
            eprintln!(
                "v4l2 无设备列表：请使用 v4l2-ctl --list-devices，或直接指定设备路径（如 /dev/video0）"
            );
            return Ok(());
        }
        "dshow" | "avfoundation" => {}
        other => bail!("不支持的格式: {other}（--list-devices 支持 dshow/avfoundation）"),
    }

    // 设备解复用器（dshow/gdigrab/avfoundation 等）由 avdevice_register_all 注册，
    // 必须先 init 才能被 av_find_input_format 找到。
    ffmpeg_next::init()
        .map_err(|e| anyhow!("{e}"))
        .context("视频编解码器初始化失败，请确认 ffmpeg 已正确安装且版本兼容")?;

    let c_name =
        std::ffi::CString::new(format).map_err(|_| anyhow!("输入格式名包含非法字符: {format}"))?;
    let fmt_ptr = unsafe { ffmpeg::ffi::av_find_input_format(c_name.as_ptr()) };
    if fmt_ptr.is_null() {
        bail!("找不到输入格式: {format}（请确认 ffmpeg 编译包含该格式）");
    }
    let input_fmt = unsafe { ffmpeg::format::format::Input::wrap(fmt_ptr as *mut _) };

    let url = if format == "avfoundation" {
        ""
    } else {
        "dummy"
    };
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("list_devices", "true");

    unsafe {
        // 设备列表通过 av_log(INFO) 输出，临时提高日志级别，结束后恢复。
        let prev = ffmpeg::ffi::av_log_get_level();
        ffmpeg::ffi::av_log_set_level(ffmpeg::ffi::AV_LOG_INFO);
        // 该打开动作预期失败（列表已打印），结果忽略。
        let _ = ffmpeg::format::open_with(Path::new(url), &ffmpeg::Format::Input(input_fmt), opts);
        ffmpeg::ffi::av_log_set_level(prev);
    }
    Ok(())
}

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
