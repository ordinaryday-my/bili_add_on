//! 中英双语支持：运行时消息与 CLI 帮助文本的本地化。

/// 输出语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

/// 双语条目表：(键, 中文, 英文)。
const BILINGUAL: &[(&str, &str, &str)] = &[
    // ---- 运行时消息 ----
    (
        "parsed_danmakus",
        "已解析 {} 条弹幕",
        "Parsed {} danmakus",
    ),
    ("filtered_out", "滤过 {} 个", "Filtered out {}"),
    ("codec_ready", "编解码器已就绪", "Codecs ready"),
    (
        "rendering",
        "正在渲染弹幕到视频帧...",
        "Rendering danmaku onto video frames...",
    ),
    (
        "merging_audio",
        "正在合并音频轨道...",
        "Merging audio tracks...",
    ),
    ("output_file", "输出文件: {}", "Output file: {}"),
    ("done_in", "完成，总用时 {} 秒", "Done in {} seconds"),
    (
        "render_progress",
        "正在渲染弹幕... 已处理 {} 帧",
        "Rendering danmaku... {} frames processed",
    ),
    // ---- CLI 命令简介 ----
    (
        "about",
        "为视频叠加B站弹幕（danmaku）的命令行工具，支持从B站视频ID自动获取弹幕XML或指定本地弹幕文件",
        "CLI tool to overlay Bilibili danmaku (bullet comments) onto videos; fetches danmaku XML by Bilibili video ID or from a local file",
    ),
    // ---- CLI 参数帮助 ----
    ("input", "输入视频文件路径", "Input video file path"),
    (
        "output",
        "输出视频路径（默认在源文件名前添加 bili_add_on_ 前缀）",
        "Output video path (defaults to bili_add_on_<source name>)",
    ),
    (
        "bvid",
        "B站视频 ID（如 BV1fRNH6kEra），将自动拉取对应弹幕",
        "Bilibili video ID (e.g. BV1fRNH6kEra); danmaku is fetched automatically",
    ),
    ("xml", "本地弹幕 XML 文件路径", "Path to a local danmaku XML file"),
    (
        "opacity",
        "弹幕不透明度，取值范围 0~1",
        "Danmaku opacity, in range 0~1",
    ),
    (
        "top_ratio",
        "弹幕显示区域上界与画面高度的比值，0 为顶端",
        "Top edge of the danmaku area as a fraction of video height; 0 = top",
    ),
    (
        "bottom_ratio",
        "弹幕显示区域下界与画面高度的比值，1 为底端",
        "Bottom edge of the danmaku area as a fraction of video height; 1 = bottom",
    ),
    ("font_scale", "弹幕字号缩放比", "Danmaku font size scale"),
    (
        "font",
        "用户字体文件路径（ttf/otf/ttc），可重复传入多个，按传入顺序依次降级；优先级高于系统字体与项目内置字体",
        "User font file path (ttf/otf/ttc); may be repeated, used in the given order; higher priority than system and bundled fonts",
    ),
    (
        "system_fonts",
        "启用系统字体作为回退（开启后优先级：用户字体 > 系统字体 > 项目内置字体）",
        "Enable system fonts as fallback (priority: user fonts > system fonts > bundled fonts)",
    ),
    (
        "speed",
        "弹幕滚动速度（像素每帧）",
        "Danmaku scroll speed (pixels per frame)",
    ),
    ("line_spacing", "弹幕行间距（像素）", "Danmaku line spacing (pixels)"),
    (
        "min_space",
        "同一轨道内前后滚动弹幕的最小水平间距（像素），与字号无关",
        "Minimum horizontal gap between scrolling danmaku in the same rail (pixels)",
    ),
    (
        "fixed_duration",
        "固定弹幕的持续时间（秒）",
        "Fixed danmaku duration (seconds)",
    ),
    (
        "no_audio",
        "不保留输入视频的音频轨道",
        "Do not keep the input video's audio track",
    ),
    (
        "quiet",
        "静默模式，不输出进度提示",
        "Quiet mode; suppress progress output",
    ),
    (
        "encoder",
        "视频编码器: auto/nvenc/amf/qsv/software（auto 自动选择最佳可用编码器）",
        "Video encoder: auto/nvenc/amf/qsv/software (auto selects the best available encoder)",
    ),
    (
        "x264_preset",
        "libx264 编码预设（仅软件编码生效）: ultrafast/superfast/veryfast/faster/fast/medium/slow/slower/veryslow",
        "libx264 encoding preset (software encoding only): ultrafast/superfast/veryfast/faster/fast/medium/slow/slower/veryslow",
    ),
    (
        "longest",
        "若弹幕时间跨度大于视频时长，自动延长输出视频（末尾补黑帧）以完整显示全部弹幕",
        "Extend the output video (black frames at the end) so all danmaku are fully displayed",
    ),
    ("filter", "弹幕过滤条件（regex）", "Danmaku filter (regex)"),
    (
        "range",
        "视频处理时段：{起始}-{结束} 或 {结束}；时间格式为 时:分:秒 / 分:秒 / 秒，如 1:23-5:00、162:12、3.1415926",
        "Video time range: {start}-{end} or {end}; formats: hh:mm:ss / mm:ss / seconds, e.g. 1:23-5:00, 162:12, 3.1415926",
    ),
    (
        "lang",
        "输出语言：zh/en/auto（auto 按系统区域设置）",
        "Output language: zh/en/auto (auto follows system locale)",
    ),
];

impl Lang {
    /// 依据 `--lang` 参数值与系统区域设置检测语言。
    ///
    /// `zh`/`en` 直接采用；`auto` 或缺省时按系统区域（sys-locale）判定：
    /// 区域以 `zh` 开头 → 中文，否则英文。
    pub fn detect(lang_arg: Option<&str>) -> Self {
        match lang_arg {
            Some("zh") => Lang::Zh,
            Some("en") => Lang::En,
            _ => {
                let locale = sys_locale::get_locale()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if locale.starts_with("zh") {
                    Lang::Zh
                } else {
                    Lang::En
                }
            }
        }
    }

    /// 取本地化文本模板；未知键原样返回。
    pub fn t<'a>(&self, key: &'a str) -> &'a str {
        for (k, zh, en) in BILINGUAL {
            if *k == key {
                return match self {
                    Lang::Zh => zh,
                    Lang::En => en,
                };
            }
        }
        key
    }

    /// 取本地化模板并将唯一的 `{}` 占位符替换为给定值。
    pub fn t_fmt(&self, key: &str, value: impl std::fmt::Display) -> String {
        self.t(key).replace("{}", &value.to_string())
    }

    /// 取某 CLI 参数在指定语言下的帮助文本（用于 clap 帮助本地化）。
    pub fn arg_help(&self, arg_id: &str) -> Option<&'static str> {
        if *self == Lang::Zh {
            return None;
        }
        for (k, _, en) in BILINGUAL {
            if *k == arg_id {
                return Some(en);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_explicit() {
        assert_eq!(Lang::detect(Some("zh")), Lang::Zh);
        assert_eq!(Lang::detect(Some("en")), Lang::En);
    }

    #[test]
    fn test_detect_auto_falls_back_to_system_locale() {
        // auto/None 走系统区域检测（测试环境不假定区域，只验证不 panic 且合法）
        let lang = Lang::detect(Some("auto"));
        assert!(matches!(lang, Lang::Zh | Lang::En));
        let lang = Lang::detect(None);
        assert!(matches!(lang, Lang::Zh | Lang::En));
    }

    #[test]
    fn test_runtime_message_bilingual() {
        let zh = Lang::Zh.t_fmt("parsed_danmakus", 42);
        let en = Lang::En.t_fmt("parsed_danmakus", 42);
        assert!(zh.contains("42"));
        assert!(en.contains("42"));
        assert!(zh.contains('弹'));
        assert!(en.contains("danmakus"));
    }

    #[test]
    fn test_unknown_key_passthrough() {
        assert_eq!(Lang::En.t("no_such_key"), "no_such_key");
    }

    #[test]
    fn test_arg_help_english_only() {
        // 中文帮助直接来自 clap derive，无需替换
        assert_eq!(Lang::Zh.arg_help("input"), None);
        let en = Lang::En.arg_help("input").unwrap();
        assert!(en.contains("Input video file path"));
        assert_eq!(Lang::En.arg_help("no_such_arg"), None);
    }
}
