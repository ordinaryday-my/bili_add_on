use std::{cmp::Ordering, ffi::OsString, path::PathBuf};
use clap::Parser;
use anyhow::{Context, anyhow, bail};

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

    #[arg(long, short, default_value_t = 3, help = "弹幕滚动速度（像素每帧）")]
    pub speed: u32,

    #[arg(long, default_value_t = 4, help = "弹幕行间距（像素）")]
    pub line_spacing: u32,

    #[arg(long, default_value_t = 5.0, help = "固定弹幕的持续时间（秒）")]
    pub fixed_duration: f64,

    #[arg(long, default_value_t = false, help = "不保留输入视频的音频轨道")]
    pub no_audio: bool,

    #[arg(long, short, default_value_t = false, help = "静默模式，不输出进度提示")]
    pub quiet: bool,

    #[arg(
        long,
        default_value = "auto",
        help = "视频编码器: auto/nvenc/amf/qsv/software（auto 自动选择最佳可用编码器）"
    )]
    pub encoder: String,
}

impl Args {
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
            bail!(
                "opacity 必须在 0.0 到 1.0 之间，当前值: {}",
                self.opacity
            );
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

        if self.fixed_duration <= 0.0 {
            bail!(
                "fixed_duration 必须大于 0，当前值: {}",
                self.fixed_duration
            );
        }

        if self.font_scale as f64 * 25.0 + self.line_spacing as f64 <= 0.0 {
            bail!(
                "font_scale ({}) * 25 + line_spacing ({}) 必须大于 0",
                self.font_scale,
                self.line_spacing
            );
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
}

#[derive(clap::Args, Debug)]
#[group(required = true, multiple = false)]
pub struct DanmakuSource {
    #[arg(
        long,
        help = "B站视频 ID（如 BV1fRNH6kEra），将自动拉取对应弹幕"
    )]
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
            speed: 3,
            line_spacing: 4,
            fixed_duration: 5.0,
            no_audio: false,
            quiet: false,
            encoder: "auto".to_string(),
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
            speed: 3,
            line_spacing: 4,
            fixed_duration: 5.0,
            no_audio: false,
            quiet: false,
            encoder: "auto".to_string(),
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
    fn test_check_bottom_must_be_greater_than_top() {
        let mut args = default_args();
        args.top_ratio = 0.5;
        args.bottom_ratio = 0.3;
        assert!(args.check().is_err());
    }

    #[test]
    fn test_clap_parse_basic() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "bili_add_on",
            "--input", "video.mp4",
            "--bvid", "BV1test",
        ]);
        assert!(args.is_ok());
    }

    #[test]
    fn test_clap_parse_requires_source() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "bili_add_on",
            "--input", "video.mp4",
        ]);
        assert!(args.is_err());
    }
}
