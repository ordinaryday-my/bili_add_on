use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use regex::Regex;
use std::{
    cmp::Ordering,
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::i18n::Lang;

/// `--input` 特殊值：从标准输入读取视频。
pub const STDIN: &str = ":STDIN:";
/// `--output` 特殊值：将视频写入标准输出。
pub const STDOUT: &str = ":STDOUT:";

#[derive(Debug, Parser)]
#[command(
    version,
    author,
    about,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// 为本地视频叠加B站弹幕（支持文件或标准输入）
    Overlay(OverlayArgs),
    /// 从采集设备（摄像头、屏幕捕获等）实时输入并叠加弹幕
    Capture(CaptureArgs),
    /// 列出采集格式的可用设备后退出（dshow/avfoundation）
    ListDevices(ListDevicesArgs),
}

#[derive(clap::Args, Debug)]
pub struct OverlayArgs {
    #[arg(
        long,
        short,
        help = "输入视频文件路径，或 :STDIN: 从标准输入读取"
    )]
    pub input: String,

    #[arg(
        long,
        short,
        help = "输出视频路径，或 :STDOUT: 输出到标准输出（默认在源文件名前添加 bili_add_on_ 前缀）"
    )]
    pub output: Option<String>,

    #[arg(
        long,
        value_name = "TIME_RANGE",
        help = "视频处理时段：{起始}-{结束} 或 {结束}；时间格式为 时:分:秒 / 分:秒 / 秒，如 1:23-5:00、162:12、3.1415926"
    )]
    pub range: Option<String>,

    #[command(flatten)]
    pub render: RenderOptions,
}

#[derive(clap::Args, Debug)]
pub struct CaptureArgs {
    #[arg(
        long,
        value_name = "SPEC",
        help = "采集设备规格：{格式}:{URL}，如 dshow:video=USB Camera、gdigrab:desktop、v4l2:/dev/video0、avfoundation:0:none；或直接写 desktop/screen 使用平台默认屏幕捕获"
    )]
    pub capture: String,

    #[arg(
        long,
        value_name = "TIME_RANGE",
        value_parser = parse_capture_range,
        help = "录制时长（仅结束时间，如 30、1:23）。采集源没有尽头，必须指定且不允许起始时间"
    )]
    pub range: String,

    #[arg(
        long,
        short,
        help = "输出视频路径，或 :STDOUT: 输出到标准输出（采集设备输入无源文件名，必填）"
    )]
    pub output: String,

    #[command(flatten)]
    pub render: RenderOptions,
}

#[derive(clap::Args, Debug)]
pub struct ListDevicesArgs {
    /// 采集格式名：dshow / avfoundation（gdigrab、v4l2 等无设备列表）
    pub format: String,
}

/// 共享渲染参数（overlay / capture 均可用）。
#[derive(clap::Args, Debug)]
pub struct RenderOptions {
    #[command(flatten)]
    pub source: DanmakuSource,

    #[arg(long, default_value_t = 0.93, help = "弹幕不透明度，取值范围 0~1")]
    pub opacity: f64,

    #[arg(
        long,
        short,
        default_value_t = 0.0,
        help = "弹幕显示区域上界与画面高度的比值，0 为顶端"
    )]
    pub top_ratio: f64,

    #[arg(
        long,
        short,
        default_value_t = 1.0,
        help = "弹幕显示区域下界与画面高度的比值，1 为底端"
    )]
    pub bottom_ratio: f64,

    #[arg(long, default_value_t = 1.0, help = "弹幕字号缩放比")]
    pub font_scale: f32,

    #[arg(
        long,
        value_name = "FONT_FILE",
        help = "用户字体文件路径（ttf/otf/ttc），可重复传入多个，按传入顺序依次降级；优先级高于系统字体与项目内置字体"
    )]
    pub font: Vec<PathBuf>,

    #[arg(
        long,
        default_value_t = false,
        help = "启用系统字体作为回退（开启后优先级：用户字体 > 系统字体 > 项目内置字体）"
    )]
    pub system_fonts: bool,

    #[arg(long, short, default_value_t = 3, help = "弹幕滚动速度（像素每帧）")]
    pub speed: u32,

    #[arg(long, default_value_t = 4, help = "弹幕行间距（像素）")]
    pub line_spacing: u32,

    #[arg(
        long,
        default_value_t = 20,
        help = "同一轨道内前后滚动弹幕的最小水平间距（像素），与字号无关"
    )]
    pub min_space: u32,

    #[arg(long, default_value_t = 5.0, help = "固定弹幕的持续时间（秒）")]
    pub fixed_duration: f64,

    #[arg(long, default_value_t = false, help = "不保留输入视频的音频轨道")]
    pub no_audio: bool,

    #[arg(
        long,
        short,
        default_value_t = false,
        help = "静默模式，不输出进度提示"
    )]
    pub quiet: bool,

    #[arg(
        long,
        default_value = "auto",
        help = "视频编码器: auto/nvenc/amf/qsv/software（auto 自动选择最佳可用编码器）"
    )]
    pub encoder: String,

    #[arg(
        long,
        default_value = "medium",
        help = "libx264 编码预设（仅软件编码生效）: ultrafast/superfast/veryfast/faster/fast/medium/slow/slower/veryslow"
    )]
    pub x264_preset: String,

    #[arg(
        long,
        help = "若弹幕时间跨度大于视频时长，自动延长输出视频（末尾补黑帧）以完整显示全部弹幕"
    )]
    pub longest: bool,

    #[arg(long, help = "弹幕过滤条件（regex）", value_delimiter = ',')]
    pub filter: Option<Vec<String>>,

    #[arg(
        long,
        value_name = "AUDIO_FILE",
        help = "音频源文件路径，覆盖视频自带音频（stdin/采集设备输入时可用此参数保留音频）"
    )]
    pub audio: Option<PathBuf>,

    #[arg(
        long,
        value_name = "TIME_RANGE",
        help = "音频裁剪时段：{起始}-{结束} 或 {结束}；先按此裁剪音频并对齐视频开头，再随视频一起按 --range 裁剪"
    )]
    pub audio_range: Option<String>,

    #[arg(
        long,
        value_name = "LANG",
        default_value = "auto",
        value_parser = ["zh", "en", "auto"],
        help = "输出语言：zh/en/auto（auto 按系统区域设置）/ Output language: zh/en/auto (auto follows system locale)"
    )]
    pub lang: String,
}

impl OverlayArgs {
    pub fn check(&self) -> anyhow::Result<()> {
        if self.input != STDIN {
            if !Path::new(&self.input).exists() {
                bail!("视频源不存在: {}", self.input);
            }

            if Path::new(&self.input).is_dir() {
                bail!("不能输入目录（视频源）: {}", self.input);
            }
        }

        self.render.check()?;

        if let Some(range) = &self.range {
            parse_time_range(range)?;
        }

        Ok(())
    }

    pub fn check_output(&mut self) -> anyhow::Result<()> {
        if self.output.is_none() {
            if self.input == STDIN {
                bail!("使用 stdin 输入时必须显式指定 --output（文件路径或 :STDOUT:）");
            }
            let mut from = PathBuf::from(&self.input);
            let mut prefix = OsString::from("bili_add_on_");
            prefix.push(
                from.file_name()
                    .with_context(|| format!("无法从路径获取文件名: {}", self.input))?,
            );
            from.set_file_name(prefix);

            self.output = Some(from.to_string_lossy().into_owned());
        }

        Ok(())
    }
}

impl CaptureArgs {
    pub fn check(&self) -> anyhow::Result<()> {
        if let Some((_, url)) = self.capture.split_once(':')
            && url.is_empty()
        {
            bail!(
                "--capture 规格无效: '{}'（{{格式}}:{{URL}} 中 URL 不能为空）",
                self.capture
            );
        }
        self.render.check()
    }
}

impl RenderOptions {
    pub fn check(&self) -> anyhow::Result<()> {
        if let Some(p) = &self.source.xml {
            if !p.exists() {
                bail!("弹幕文件不存在: {}", p.display());
            }

            let ext = p.extension().unwrap_or_default();
            if ext != "xml" {
                bail!(
                    "弹幕文件扩展名必须为 .xml，当前文件: {} (扩展名: {:?})",
                    p.display(),
                    ext
                );
            }

            if p.is_dir() {
                bail!("不能输入目录（弹幕源）: {}", p.display());
            }
        }

        if !(0.0..=1.0).contains(&self.opacity) {
            bail!("opacity 必须在 0.0 到 1.0 之间，当前值: {}", self.opacity);
        }

        if self
            .bottom_ratio
            .partial_cmp(&self.top_ratio)
            .ok_or_else(|| {
                anyhow!(
                    "top_ratio ({}) 或 bottom_ratio ({}) 不是有效数值（可能为 NaN 或 Infinity）",
                    self.top_ratio,
                    self.bottom_ratio
                )
            })?
            != Ordering::Greater
        {
            bail!(
                "bottom_ratio ({}) 必须大于 top_ratio ({})",
                self.bottom_ratio,
                self.top_ratio
            );
        }

        if self.font_scale <= 0.0 {
            bail!("font_scale 必须大于 0，当前值: {}", self.font_scale);
        }

        for path in &self.font {
            if !path.exists() {
                bail!("字体文件不存在: {}", path.display());
            }
            if path.is_dir() {
                bail!("不能输入目录（字体源）: {}", path.display());
            }
        }

        if self.speed == 0 {
            bail!("speed 必须大于 0，当前值: {}", self.speed);
        }

        let valid_encoders = ["auto", "nvenc", "amf", "qsv", "software"];
        if !valid_encoders.contains(&self.encoder.as_str()) {
            bail!(
                "encoder 必须是 {} 之一，当前值: {}",
                valid_encoders.join("/"),
                self.encoder
            );
        }

        let valid_presets = [
            "ultrafast",
            "superfast",
            "veryfast",
            "faster",
            "fast",
            "medium",
            "slow",
            "slower",
            "veryslow",
        ];
        if !valid_presets.contains(&self.x264_preset.as_str()) {
            bail!(
                "x264_preset 必须是 {} 之一，当前值: {}",
                valid_presets.join("/"),
                self.x264_preset
            );
        }

        if self.fixed_duration <= 0.0 {
            bail!("fixed_duration 必须大于 0，当前值: {}", self.fixed_duration);
        }

        if self.font_scale as f64 * 25.0 + self.line_spacing as f64 <= 0.0 {
            bail!(
                "font_scale ({}) * 25 + line_spacing ({}) 必须大于 0",
                self.font_scale,
                self.line_spacing
            );
        }

        if let Some(path) = &self.audio {
            if !path.exists() {
                bail!("音频文件不存在: {}", path.display());
            }
            if path.is_dir() {
                bail!("不能输入目录（音频源）: {}", path.display());
            }
        }

        if self.audio.is_some() && self.no_audio {
            bail!("--audio 与 --no-audio 冲突，请二选一");
        }

        if let Some(audio_range) = &self.audio_range {
            parse_time_range(audio_range)?;
            if self.no_audio {
                bail!("--audio-range 与 --no-audio 冲突，无法使用");
            }
        }

        Ok(())
    }

    pub fn parse_filters(&self) -> Option<Result<Vec<Regex>, regex::Error>> {
        let Some(filters) = &self.filter else {
            return None;
        };

        let res: Result<Vec<_>, _> = filters.iter().map(|s| Regex::new(s.as_str())).collect();

        Some(res)
    }
}

impl Cli {
    /// 解析命令行参数并按 `--lang`/系统区域本地化帮助文本。
    ///
    /// 返回 `(参数, 语言)`；`--help` 时由 clap 以本地化文本输出并退出。
    pub fn parse_with_locale() -> Result<(Cli, Lang)> {
        Self::parse_with_locale_from(std::env::args())
    }

    fn parse_with_locale_from(argv: impl IntoIterator<Item = String>) -> Result<(Cli, Lang)> {
        let argv: Vec<String> = argv.into_iter().collect();
        let lang = Lang::detect(lang_arg_from(&argv).as_deref());
        let mut cmd = Cli::command();
        cmd = cmd.about(lang.t("about"));
        cmd = cmd.mut_subcommands(|sc| {
            let name = sc.get_name().to_string();
            match lang.arg_help(&name) {
                Some(en) => sc.about(Some(en)),
                None => sc,
            }
        });
        cmd = cmd.mut_args(|arg| match lang.arg_help(arg.get_id().as_str()) {
            Some(en) => arg.help(Some(en)),
            None => arg,
        });
        let matches = cmd.get_matches_from(argv);
        let cli = Cli::from_arg_matches(&matches).map_err(|e| anyhow!("{e}"))?;
        Ok((cli, lang))
    }
}

/// 从原始命令行参数中提取 `--lang` 值（在 clap 完整解析前用于帮助文本本地化）。
fn lang_arg_from(argv: &[String]) -> Option<String> {
    let mut iter = argv.iter().skip(1);
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--lang=") {
            return Some(value.to_string());
        }
        if arg == "--lang" {
            return iter.next().cloned();
        }
    }
    None
}

/// 解析单个时间点：`时:分:秒` / `分:秒` / `秒`。
///
/// 时、分必须为非负整数，秒必须为非负浮点数（拒绝 NaN/Infinity/负数）。
pub fn parse_time_point(s: &str) -> Result<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        bail!("时间格式无效: '{s}'（应为 时:分:秒 / 分:秒 / 秒）");
    }
    if parts.iter().any(|p| p.is_empty()) {
        bail!("时间格式无效: '{s}'（存在空的时间分量）");
    }

    let parse_component = |p: &str, name: &str| -> Result<u64> {
        p.parse::<u64>()
            .with_context(|| format!("时间分量 '{name}' 不是非负整数: '{p}'（位于 '{s}'）"))
    };
    let parse_secs = |p: &str| -> Result<f64> {
        let v: f64 = p
            .parse()
            .with_context(|| format!("秒分量不是有效数字: '{p}'（位于 '{s}'）"))?;
        if !v.is_finite() || v < 0.0 {
            bail!("秒分量必须为非负有限数值: '{p}'（位于 '{s}'）");
        }
        Ok(v)
    };

    match parts.as_slice() {
        [secs] => parse_secs(secs),
        [mins, secs] => {
            let m = parse_component(mins, "分")?;
            let s = parse_secs(secs)?;
            Ok(m as f64 * 60.0 + s)
        }
        [hours, mins, secs] => {
            let h = parse_component(hours, "时")?;
            let m = parse_component(mins, "分")?;
            let s = parse_secs(secs)?;
            Ok(h as f64 * 3600.0 + m as f64 * 60.0 + s)
        }
        _ => unreachable!(),
    }
}

/// 解析视频处理时段：`{起始}-{结束}` 或 `{结束}`（起始为 0）。
///
/// 返回 `(start, end)`（秒），保证 `start < end` 且均非负。
pub fn parse_time_range(s: &str) -> Result<(f64, f64)> {
    if s.is_empty() {
        bail!("--range 参数为空");
    }

    let (start, end) = match s.split_once('-') {
        Some((start, end)) => {
            let start = parse_time_point(start)
                .with_context(|| format!("起始时间无效（--range '{s}'）"))?;
            let end =
                parse_time_point(end).with_context(|| format!("结束时间无效（--range '{s}'"))?;
            (start, end)
        }
        None => {
            let end =
                parse_time_point(s).with_context(|| format!("结束时间无效（--range '{s}'）"))?;
            (0.0, end)
        }
    };

    if start >= end {
        bail!("--range 的起始时间 ({start} 秒) 必须小于结束时间 ({end} 秒)");
    }
    Ok((start, end))
}

/// 采集设备输入 `--range` 的 clap 值解析器：仅允许结束时间（`{结束}`），拒绝起始时间。
fn parse_capture_range(s: &str) -> Result<String, String> {
    if s.contains('-') {
        return Err("采集设备输入不支持起始时间，请只写结束时间（如 --range 30）".to_string());
    }
    parse_time_range(s).map_err(|e| e.to_string())?;
    Ok(s.to_string())
}

#[derive(clap::Args, Debug)]
#[group(required = true, multiple = false)]
pub struct DanmakuSource {
    #[arg(long, help = "B站视频 ID（如 BV1fRNH6kEra），将自动拉取对应弹幕")]
    pub bvid: Option<String>,

    #[arg(long, short, help = "本地弹幕 XML 文件路径")]
    pub xml: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_render() -> RenderOptions {
        RenderOptions {
            source: DanmakuSource {
                bvid: Some("BV1test".to_string()),
                xml: None,
            },
            opacity: 0.93,
            top_ratio: 0.0,
            bottom_ratio: 1.0,
            font_scale: 1.0,
            font: vec![],
            system_fonts: false,
            speed: 3,
            line_spacing: 4,
            min_space: 20,
            fixed_duration: 5.0,
            no_audio: false,
            quiet: false,
            encoder: "auto".to_string(),
            x264_preset: "medium".to_string(),
            longest: false,
            filter: Some(vec![]),
            audio: None,
            audio_range: None,
            lang: "auto".to_string(),
        }
    }

    fn default_overlay() -> OverlayArgs {
        OverlayArgs {
            input: "test.mp4".to_string(),
            output: Some("output.mp4".to_string()),
            range: None,
            render: default_render(),
        }
    }

    fn parse_overlay(argv: &[&str]) -> OverlayArgs {
        let mut full = vec!["bili_add_on", "overlay"];
        full.extend_from_slice(argv);
        let cli = Cli::try_parse_from(full).unwrap();
        match cli.command {
            Commands::Overlay(args) => args,
            _ => panic!("expected overlay"),
        }
    }

    fn parse_capture(argv: &[&str]) -> CaptureArgs {
        let mut full = vec!["bili_add_on", "capture"];
        full.extend_from_slice(argv);
        let cli = Cli::try_parse_from(full).unwrap();
        match cli.command {
            Commands::Capture(args) => args,
            _ => panic!("expected capture"),
        }
    }

    #[test]
    fn test_clap_parse_overlay_full_args() {
        let args = parse_overlay(&[
            "--input",
            "video.mp4",
            "--output",
            "out.mp4",
            "--bvid",
            "BV1fRNH6kEra",
            "--opacity",
            "0.5",
            "--top-ratio",
            "0.1",
            "--bottom-ratio",
            "0.9",
            "--font-scale",
            "1.5",
            "--speed",
            "5",
            "--line-spacing",
            "3",
            "--fixed-duration",
            "10.0",
            "--encoder",
            "software",
            "--quiet",
        ]);
        assert_eq!(args.input, "video.mp4");
        assert_eq!(args.output.as_deref(), Some("out.mp4"));
        assert_eq!(args.render.source.bvid.unwrap(), "BV1fRNH6kEra");
        assert!(args.render.source.xml.is_none());
        assert!((args.render.opacity - 0.5).abs() < f64::EPSILON);
        assert_eq!(args.render.speed, 5);
        assert_eq!(args.render.encoder, "software");
        assert!(args.render.quiet);
    }

    #[test]
    fn test_clap_parse_overlay_default_values() {
        let args = parse_overlay(&["--input", "video.mp4", "--bvid", "BV1test"]);
        assert!((args.render.opacity - 0.93).abs() < f64::EPSILON);
        assert_eq!(args.render.speed, 3);
        assert_eq!(args.render.encoder, "auto");
        assert!(args.output.is_none());
    }

    #[test]
    fn test_clap_parse_requires_source() {
        assert!(
            Cli::try_parse_from(["bili_add_on", "overlay", "--input", "video.mp4"]).is_err()
        );
        assert!(Cli::try_parse_from([
            "bili_add_on",
            "capture",
            "--capture",
            "gdigrab:desktop",
            "--range",
            "30",
            "--output",
            "out.mp4"
        ])
        .is_err());
    }

    #[test]
    fn test_clap_parse_overlay_font_and_lang() {
        let args = parse_overlay(&[
            "--input",
            "v.mp4",
            "--bvid",
            "BV1test",
            "--font",
            "a.ttf",
            "--font",
            "b.ttf",
            "--system-fonts",
            "--lang",
            "zh",
        ]);
        assert_eq!(args.render.font.len(), 2);
        assert!(args.render.system_fonts);
        assert_eq!(args.render.lang, "zh");

        assert!(Cli::try_parse_from([
            "bili_add_on",
            "overlay",
            "--input",
            "v.mp4",
            "--bvid",
            "BV1test",
            "--lang",
            "fr"
        ])
        .is_err());
    }

    #[test]
    fn test_cli_parse_capture_required() {
        // 缺少 --range
        assert!(Cli::try_parse_from([
            "bili_add_on",
            "capture",
            "--capture",
            "gdigrab:desktop",
            "--output",
            "out.mp4",
            "--bvid",
            "BV1test"
        ])
        .is_err());
        // 缺少 --capture
        assert!(Cli::try_parse_from([
            "bili_add_on",
            "capture",
            "--range",
            "30",
            "--output",
            "out.mp4",
            "--bvid",
            "BV1test"
        ])
        .is_err());
        // 缺少 --output
        assert!(Cli::try_parse_from([
            "bili_add_on",
            "capture",
            "--capture",
            "gdigrab:desktop",
            "--range",
            "30",
            "--bvid",
            "BV1test"
        ])
        .is_err());
        // --range 不允许起始时间
        assert!(Cli::try_parse_from([
            "bili_add_on",
            "capture",
            "--capture",
            "gdigrab:desktop",
            "--range",
            "5-30",
            "--output",
            "out.mp4",
            "--bvid",
            "BV1test"
        ])
        .is_err());
        // --range 语法错误
        assert!(Cli::try_parse_from([
            "bili_add_on",
            "capture",
            "--capture",
            "gdigrab:desktop",
            "--range",
            "abc",
            "--output",
            "out.mp4",
            "--bvid",
            "BV1test"
        ])
        .is_err());
    }

    #[test]
    fn test_cli_parse_capture_ok() {
        let args = parse_capture(&[
            "--capture",
            "dshow:video=USB Camera",
            "--range",
            "30",
            "--output",
            "out.mp4",
            "--bvid",
            "BV1test",
        ]);
        assert_eq!(args.capture, "dshow:video=USB Camera");
        assert_eq!(args.range, "30");
        assert_eq!(args.output, "out.mp4");
    }

    #[test]
    fn test_cli_parse_list_devices_without_input_or_source() {
        let cli = Cli::try_parse_from(["bili_add_on", "list-devices", "dshow"]).unwrap();
        let Commands::ListDevices(args) = cli.command else {
            panic!("expected list-devices");
        };
        assert_eq!(args.format, "dshow");
    }

    #[test]
    fn test_cli_requires_subcommand() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["bili_add_on"]).is_err());
        assert!(Cli::try_parse_from(["bili_add_on", "unknown-cmd"]).is_err());
    }

    #[test]
    fn test_check_output_generates_default_path() {
        let mut args = default_overlay();
        args.input = "videos/my_video.mp4".to_string();
        args.output = None;

        args.check_output().unwrap();
        let out = args.output.unwrap();
        assert_eq!(
            PathBuf::from(out).file_name().unwrap().to_string_lossy(),
            "bili_add_on_my_video.mp4"
        );
    }

    #[test]
    fn test_check_output_requires_explicit_output_for_stdin() {
        let mut args = default_overlay();
        args.input = STDIN.to_string();
        args.output = None;
        assert!(args.check_output().is_err());
        args.output = Some(STDOUT.to_string());
        assert!(args.check_output().is_ok());
    }

    #[test]
    fn test_check_stdin_skips_file_checks() {
        let mut args = default_overlay();
        args.input = STDIN.to_string();
        assert!(args.check().is_ok());
    }

    #[test]
    fn test_check_valid_opacity_rejected() {
        let mut args = default_overlay();
        args.render.opacity = 1.5;
        assert!(args.check().is_err());
        args.render.opacity = -0.1;
        assert!(args.check().is_err());
    }

    #[test]
    fn test_check_encoder_valid() {
        for enc in &["auto", "nvenc", "amf", "qsv", "software"] {
            let mut args = default_overlay();
            args.render.encoder = enc.to_string();
            let _ = args.check();
        }
        let mut args = default_overlay();
        args.render.encoder = "cuda".to_string();
        assert!(args.check().is_err());
    }

    #[test]
    fn test_check_x264_preset() {
        for preset in [
            "ultrafast",
            "veryfast",
            "fast",
            "medium",
            "slow",
            "veryslow",
        ] {
            let mut args = default_overlay();
            args.render.x264_preset = preset.to_string();
            assert!(args.check().is_err()); // 文件存在性检查先失败，预设本身有效
        }
        let tmp = std::env::temp_dir().join("bili_add_on_preset_check.mp4");
        std::fs::write(&tmp, b"fake").unwrap();
        let mut args = default_overlay();
        args.input = tmp.display().to_string();
        args.render.x264_preset = "veryfast".to_string();
        assert!(args.check().is_ok());
        args.render.x264_preset = "bogus".to_string();
        assert!(args.check().is_err());
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_check_speed_zero_rejected() {
        let mut args = default_overlay();
        args.render.speed = 0;
        assert!(args.check().is_err());
    }

    #[test]
    fn test_check_font_scale_non_positive_rejected() {
        let mut args = default_overlay();
        args.render.font_scale = 0.0;
        assert!(args.check().is_err());
        args.render.font_scale = -1.0;
        assert!(args.check().is_err());
    }

    #[test]
    fn test_check_rejects_missing_font_file() {
        let tmp = std::env::temp_dir().join("bili_add_on_font_check.mp4");
        std::fs::write(&tmp, b"fake").unwrap();
        let mut args = default_overlay();
        args.input = tmp.display().to_string();
        args.render.font = vec![PathBuf::from("definitely_missing_font.ttf")];
        assert!(args.check().is_err());
        args.render.font = vec![std::env::temp_dir()];
        assert!(args.check().is_err()); // 目录不允许
        args.render.font = vec![];
        assert!(args.check().is_ok());
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_check_bottom_must_be_greater_than_top() {
        let mut args = default_overlay();
        args.render.top_ratio = 0.5;
        args.render.bottom_ratio = 0.3;
        assert!(args.check().is_err());
    }

    #[test]
    fn test_check_range() {
        let tmp = std::env::temp_dir().join("bili_add_on_range_test.mp4");
        std::fs::write(&tmp, b"fake").unwrap();
        let mut args = default_overlay();
        args.input = tmp.display().to_string();
        args.range = Some("1:23-5:00".to_string());
        assert!(args.check().is_ok());
        args.range = Some("162:12".to_string());
        assert!(args.check().is_ok());
        args.range = Some("10-5".to_string());
        assert!(args.check().is_err());
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_check_audio_conflicts() {
        let tmp = std::env::temp_dir().join("bili_add_on_audio_conflict.mp4");
        std::fs::write(&tmp, b"fake").unwrap();
        let mut args = default_overlay();
        args.input = tmp.display().to_string();
        args.render.audio = Some(PathBuf::from("audio.m4a"));
        args.render.no_audio = true;
        assert!(args.check().is_err());
        args.render.audio = None;
        args.render.audio_range = Some("5-10".to_string());
        args.render.no_audio = true;
        assert!(args.check().is_err());
        args.render.no_audio = false;
        assert!(args.check().is_ok());
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_check_capture_spec() {
        let mut args = CaptureArgs {
            capture: "gdigrab:".to_string(),
            range: "30".to_string(),
            output: "out.mp4".to_string(),
            render: default_render(),
        };
        assert!(args.check().is_err()); // URL 为空
        args.capture = "gdigrab:desktop".to_string();
        assert!(args.check().is_ok());
    }

    #[test]
    fn test_parse_with_locale_returns_lang() {
        let (cli, lang) = Cli::parse_with_locale_from(
            [
                "bili_add_on",
                "overlay",
                "--input",
                "v.mp4",
                "--bvid",
                "BV1test",
                "--lang",
                "en",
            ]
            .into_iter()
            .map(String::from),
        )
        .unwrap();
        let Commands::Overlay(args) = cli.command else {
            panic!("expected overlay");
        };
        assert_eq!(args.input, "v.mp4");
        assert_eq!(lang, crate::i18n::Lang::En);
        let (_, lang) = Cli::parse_with_locale_from(
            [
                "bili_add_on",
                "overlay",
                "--input",
                "v.mp4",
                "--bvid",
                "BV1test",
                "--lang",
                "zh",
            ]
            .into_iter()
            .map(String::from),
        )
        .unwrap();
        assert_eq!(lang, crate::i18n::Lang::Zh);
    }

    #[test]
    #[allow(clippy::approx_constant)] // 特意用圆周率近似值验证小数解析
    fn test_parse_time_point_seconds() {
        assert!((parse_time_point("3.1415926").unwrap() - 3.1415926).abs() < 1e-9);
        assert!((parse_time_point("0").unwrap() - 0.0).abs() < 1e-9);
        assert!((parse_time_point("0.5").unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_parse_time_point_minutes_seconds() {
        assert!((parse_time_point("162:12").unwrap() - 9732.0).abs() < 1e-9);
        assert!((parse_time_point("0:30").unwrap() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_time_point_hours_minutes_seconds() {
        assert!((parse_time_point("1:23:2.21").unwrap() - 4982.21).abs() < 1e-9);
        assert!((parse_time_point("0:0:0.5").unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_parse_time_point_invalid() {
        assert!(parse_time_point("abc").is_err());
        assert!(parse_time_point("1:2:3:4").is_err());
        assert!(parse_time_point("1:2:").is_err());
        assert!(parse_time_point(":2").is_err());
        assert!(parse_time_point("-5").is_err());
        assert!(parse_time_point("1:-2:3").is_err());
        assert!(parse_time_point("1.5:2").is_err());
        assert!(parse_time_point("NaN").is_err());
        assert!(parse_time_point("Infinity").is_err());
        assert!(parse_time_point("").is_err());
    }

    #[test]
    fn test_parse_time_range_end_only() {
        let (start, end) = parse_time_range("162:12").unwrap();
        assert!((start - 0.0).abs() < 1e-9);
        assert!((end - 9732.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_time_range_full() {
        let (start, end) = parse_time_range("1:23-5:00").unwrap();
        assert!((start - 83.0).abs() < 1e-9);
        assert!((end - 300.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_time_range_mixed_formats() {
        let (start, end) = parse_time_range("0:30-1:23:2.21").unwrap();
        assert!((start - 30.0).abs() < 1e-9);
        assert!((end - 4982.21).abs() < 1e-9);
    }

    #[test]
    fn test_parse_time_range_invalid() {
        assert!(parse_time_range("").is_err());
        assert!(parse_time_range("10-5").is_err());
        assert!(parse_time_range("5-5").is_err());
        assert!(parse_time_range("1:23-").is_err());
        assert!(parse_time_range("-5:00").is_err());
        assert!(parse_time_range("abc-def").is_err());
    }

    #[test]
    fn test_parse_capture_range_rejects_start() {
        assert!(parse_capture_range("30").is_ok());
        assert!(parse_capture_range("1:23").is_ok());
        assert!(parse_capture_range("5-30").is_err());
        assert!(parse_capture_range("abc").is_err());
    }
}
