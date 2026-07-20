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
