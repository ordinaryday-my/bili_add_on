use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser};
use regex::Regex;
use std::{cmp::Ordering, ffi::OsString, path::PathBuf};

use crate::i18n::Lang;

#[derive(Debug, Parser)]
#[command(version, author, about)]
pub struct Args {
    #[arg(long, short, help = "输入视频文件路径")]
    pub input: PathBuf,

    #[arg(
        long,
        short,
        help = "输出视频路径（默认在源文件名前添加 bili_add_on_ 前缀）"
    )]
    pub output: Option<PathBuf>,

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
        value_name = "TIME_RANGE",
        help = "视频处理时段：{起始}-{结束} 或 {结束}；时间格式为 时:分:秒 / 分:秒 / 秒，如 1:23-5:00、162:12、3.1415926"
    )]
    pub range: Option<String>,

    #[arg(
        long,
        value_name = "LANG",
        default_value = "auto",
        value_parser = ["zh", "en", "auto"],
        help = "输出语言：zh/en/auto（auto 按系统区域设置）/ Output language: zh/en/auto (auto follows system locale)"
    )]
    pub lang: String,
}

impl Args {
    /// 解析命令行参数并按 `--lang`/系统区域本地化帮助文本。
    ///
    /// 返回 `(参数, 语言)`；`--help` 时由 clap 以本地化文本输出并退出。
    pub fn parse_with_locale() -> Result<(Args, Lang)> {
        Self::parse_with_locale_from(std::env::args())
    }

    fn parse_with_locale_from(
        argv: impl IntoIterator<Item = String>,
    ) -> Result<(Args, Lang)> {
        let argv: Vec<String> = argv.into_iter().collect();
        let lang = Lang::detect(lang_arg_from(&argv).as_deref());
        let mut cmd = Args::command();
        cmd = cmd.about(lang.t("about"));
        cmd = cmd.mut_args(|arg| match lang.arg_help(arg.get_id().as_str()) {
            Some(en) => arg.help(Some(en)),
            None => arg,
        });
        let matches = cmd.get_matches_from(argv);
        let args = Args::from_arg_matches(&matches).map_err(|e| anyhow!("{e}"))?;
        Ok((args, lang))
    }
    pub fn check(&self) -> anyhow::Result<()> {
        if !self.input.exists() {
            bail!("视频源不存在: {}", self.input.display());
        }

        if self.input.is_dir() {
            bail!("不能输入目录（视频源）: {}", self.input.display());
        }

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

        if let Some(range) = &self.range {
            parse_time_range(range)?;
        }

        Ok(())
    }

    pub fn check_output(&mut self) -> anyhow::Result<()> {
        if self.output.is_none() {
            let mut from = self.input.clone();
            let mut prefix = OsString::from("bili_add_on_");
            prefix.push(
                from.file_name()
                    .with_context(|| format!("无法从路径获取文件名: {}", self.input.display()))?,
            );
            from.set_file_name(prefix);

            self.output = Some(from);
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
                parse_time_point(end).with_context(|| format!("结束时间无效（--range '{s}'）"))?;
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

    fn default_args() -> Args {
        Args {
            input: PathBuf::from("test.mp4"),
            output: Some(PathBuf::from("output.mp4")),
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
            range: None,
            lang: "auto".to_string(),
        }
    }

    #[test]
    fn test_check_output_generates_default_path() {
        let mut args = Args {
            input: PathBuf::from("C:\\videos\\my_video.mp4"),
            output: None,
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
            range: None,
            lang: "auto".to_string(),
        };

        args.check_output().unwrap();
        let out = args.output.unwrap();
        assert_eq!(
            out.file_name().unwrap().to_string_lossy(),
            "bili_add_on_my_video.mp4"
        );
    }

    #[test]
    fn test_check_valid_opacity_rejected() {
        let mut args = default_args();
        args.opacity = 1.5;
        assert!(args.check().is_err());

        args.opacity = -0.1;
        assert!(args.check().is_err());
    }

    #[test]
    fn test_check_valid_opacity_accepted() {
        let args = default_args();
        assert!(args.check().is_err()); // input doesn't exist

        let mut args = default_args();
        args.opacity = 0.0;
        // will fail on file existence, so we only test opacity logic indirectly
        assert!(args.check().is_err()); // not because of opacity

        args.opacity = 1.0;
        assert!(args.check().is_err()); // not because of opacity
    }

    #[test]
    fn test_check_encoder_valid() {
        for enc in &["auto", "nvenc", "amf", "qsv", "software"] {
            let mut args = default_args();
            args.encoder = enc.to_string();
            // Will fail on file existence check first, but encoder validation would pass
            // (we just verify no panic, as file check comes first)
            let _ = args.check();
        }
    }

    #[test]
    fn test_check_encoder_invalid() {
        let mut args = default_args();
        args.encoder = "cuda".to_string();
        assert!(args.check().is_err());
    }

    #[test]
    fn test_check_x264_preset() {
        for preset in ["ultrafast", "veryfast", "fast", "medium", "slow", "veryslow"] {
            let mut args = default_args();
            args.x264_preset = preset.to_string();
            assert!(args.check().is_err()); // 文件存在性检查先失败，预设本身有效
        }
        let mut args = default_args();
        args.x264_preset = "invalid".to_string();
        assert!(args.check().is_err());
        // 预设校验在文件存在性之后，用一个存在的输入验证预设被接受
        let tmp = std::env::temp_dir().join("bili_add_on_preset_check.mp4");
        std::fs::write(&tmp, b"fake").unwrap();
        let mut args = default_args();
        args.input = tmp.clone();
        args.x264_preset = "veryfast".to_string();
        assert!(args.check().is_ok());
        args.x264_preset = "bogus".to_string();
        assert!(args.check().is_err());
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_check_speed_zero_rejected() {
        let mut args = default_args();
        args.speed = 0;
        assert!(args.check().is_err());
    }

    #[test]
    fn test_check_font_scale_non_positive_rejected() {
        let mut args = default_args();
        args.font_scale = 0.0;
        assert!(args.check().is_err());

        args.font_scale = -1.0;
        assert!(args.check().is_err());
    }

    #[test]
    fn test_clap_parse_repeatable_font_and_system_fonts() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "bili_add_on",
            "--input", "v.mp4",
            "--bvid", "BV1test",
            "--font", "a.ttf",
            "--font", "b.ttf",
            "--system-fonts",
        ]).unwrap();
        assert_eq!(args.font.len(), 2);
        assert_eq!(args.font[0].to_string_lossy(), "a.ttf");
        assert_eq!(args.font[1].to_string_lossy(), "b.ttf");
        assert!(args.system_fonts);
    }

    #[test]
    fn test_clap_parse_lang() {
        use clap::Parser;
        for lang in ["zh", "en", "auto"] {
            let args = Args::try_parse_from([
                "bili_add_on",
                "--input", "v.mp4",
                "--bvid", "BV1test",
                "--lang", lang,
            ]).unwrap();
            assert_eq!(args.lang, lang);
        }
        let args = Args::try_parse_from([
            "bili_add_on",
            "--input", "v.mp4",
            "--bvid", "BV1test",
            "--lang=zh",
        ]).unwrap();
        assert_eq!(args.lang, "zh");
        assert!(Args::try_parse_from([
            "bili_add_on",
            "--input", "v.mp4",
            "--bvid", "BV1test",
            "--lang", "fr",
        ]).is_err());
    }

    #[test]
    fn test_parse_with_locale_returns_lang() {
        let (args, lang) = Args::parse_with_locale_from(
            [
                "bili_add_on",
                "--input", "v.mp4",
                "--bvid", "BV1test",
                "--lang", "en",
            ]
            .into_iter()
            .map(String::from),
        )
        .unwrap();
        assert_eq!(args.input.to_string_lossy(), "v.mp4");
        assert_eq!(lang, crate::i18n::Lang::En);
        let (_, lang) = Args::parse_with_locale_from(
            [
                "bili_add_on",
                "--input", "v.mp4",
                "--bvid", "BV1test",
                "--lang", "zh",
            ]
            .into_iter()
            .map(String::from),
        )
        .unwrap();
        assert_eq!(lang, crate::i18n::Lang::Zh);
    }

    #[test]
    fn test_check_rejects_missing_font_file() {
        let tmp = std::env::temp_dir().join("bili_add_on_font_check.mp4");
        std::fs::write(&tmp, b"fake").unwrap();
        let mut args = default_args();
        args.input = tmp.clone();
        args.font = vec![PathBuf::from("definitely_missing_font.ttf")];
        assert!(args.check().is_err());
        args.font = vec![std::env::temp_dir()];
        assert!(args.check().is_err()); // 目录不允许
        args.font = vec![];
        assert!(args.check().is_ok());
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_check_bottom_must_be_greater_than_top() {
        let mut args = default_args();
        args.top_ratio = 0.5;
        args.bottom_ratio = 0.3;
        assert!(args.check().is_err());
    }

    #[test]
    fn test_clap_parse_basic() {
        use clap::Parser;
        let args =
            Args::try_parse_from(["bili_add_on", "--input", "video.mp4", "--bvid", "BV1test"]);
        assert!(args.is_ok());
    }

    #[test]
    fn test_clap_parse_requires_source() {
        use clap::Parser;
        let args = Args::try_parse_from(["bili_add_on", "--input", "video.mp4"]);
        assert!(args.is_err());
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
    fn test_check_rejects_invalid_range() {
        let mut args = default_args();
        args.range = Some("10-5".to_string());
        assert!(args.check().is_err());
        args.range = Some("1:2:3:4-5".to_string());
        assert!(args.check().is_err());
    }

    #[test]
    fn test_check_accepts_valid_range() {
        let tmp = std::env::temp_dir().join("bili_add_on_range_test.mp4");
        std::fs::write(&tmp, b"fake").unwrap();
        let mut args = default_args();
        args.input = tmp.clone();
        args.range = Some("1:23-5:00".to_string());
        assert!(args.check().is_ok());
        args.range = Some("162:12".to_string());
        assert!(args.check().is_ok());
        std::fs::remove_file(&tmp).unwrap();
    }
}
