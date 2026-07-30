use std::{borrow::Cow, fs, path::Path, time::Duration};

use anyhow::{anyhow, Context};
use image::Rgb;
use quick_xml::{
    XmlVersion, escape::unescape, events::{Event, attributes::Attribute},
};

use crate::{
    utils::{cow_u8_to_str, decode_bytes, decode_rgb},
    web::{get_cid_from_api, get_danmaku_xml},
};

pub fn parse_danmakus(xml: String) -> anyhow::Result<Vec<Danmaku>> {
    let xml = fix_bili_xml(&xml);
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut danmakus = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .with_context(|| "XML 解析失败（非法的 XML 标签或属性）")?
        {
            Event::Start(ref s) if s.name().as_ref() == b"d" => {
                let p_attr = s
                    .attributes()
                    .flatten()
                    .find(|attr: &Attribute<'_>| attr.key.as_ref() == b"p")
                    .map(|attr| attr.normalized_value(XmlVersion::Explicit1_1))
                    .transpose()
                    .with_context(|| "XML 属性规范化失败，p 属性值无法解码")?
                    .unwrap_or_default();

                let content = reader
                    .read_text(s.name())
                    .with_context(|| "读取弹幕标签文本内容失败")?
                    .into_inner();

                let text_preview = {
                    let s = String::from_utf8_lossy(&content);
                    if s.len() > 60 {
                        let truncated: String = s.chars().take(60).collect();
                        format!("{truncated}...")
                    } else {
                        s.into_owned()
                    }
                };

                if let Some(d) = Danmaku::new(p_attr, content).with_context(|| {
                    let index = danmakus.len() + 1;
                    format!("第{index}条弹幕解析失败 (文本预览: {text_preview})")
                })? {
                    danmakus.push(d);
                }
            }
            Event::Eof => break,
            _ => (),
        }
        buf.clear();
    }

    Ok(danmakus)
}

pub fn fix_bili_xml(xml: &str) -> String {
    let tag_re = regex::Regex::new(r"</?([a-zA-Z]\w*)(\s[^<>]*)?/?>|<\?[^>]*\?>").unwrap();
    let known: &[&str] = &[
        "i",
        "chatserver",
        "chatid",
        "mission",
        "maxlimit",
        "state",
        "real_name",
        "source",
        "ds",
        "d",
    ];

    let mut result = String::with_capacity(xml.len() + 2048);
    let mut pos = 0;
    let mut depth: usize = 0;

    for caps in tag_re.captures_iter(xml) {
        let m = caps.get(0).unwrap();
        let tag_str = m.as_str();
        let is_known = caps
            .get(1)
            .map(|g| known.contains(&g.as_str()))
            .unwrap_or(true);

        if m.start() < pos {
            continue;
        }

        if is_known {
            let text = &xml[pos..m.start()];
            if depth == 0 {
                result.push_str(&escape_text(text));
            } else {
                result.push_str(text);
            }
            result.push_str(tag_str);
            pos = m.end();

            if tag_str.starts_with("</") {
                depth = depth.saturating_sub(1);
            } else if !tag_str.ends_with("/>") {
                depth += 1;
            }
        }
    }

    if pos < xml.len() {
        if depth == 0 {
            result.push_str(&escape_text(&xml[pos..]));
        } else {
            result.push_str(&xml[pos..]);
        }
    }

    result
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

#[derive(Debug, Clone)]
pub struct Danmaku {
    pub time: Duration,
    pub mode: DanmakuMode,
    pub font_size: usize,
    pub color: Rgb<u8>,
    pub text: String,
}

impl Danmaku {
    pub fn new(styles: Cow<'_, str>, content: Cow<'_, [u8]>) -> anyhow::Result<Option<Self>> {
        let mut styles_it = styles.split(',');

        let time_str = styles_it
            .next()
            .context("p属性缺少第1字段（弹幕出现时间）")?;
        let time = Duration::from_secs_f64(
            time_str
                .parse::<f64>()
                .with_context(|| format!("时间字段解析为f64失败: '{time_str}'"))?,
        );

        let mode_str = styles_it.next().context("p属性缺少第2字段（弹幕模式）")?;
        let mode_id = mode_str
            .parse::<usize>()
            .with_context(|| format!("模式字段解析为整数失败: '{mode_str}'"))?;

        let font_size_str = styles_it.next().context("p属性缺少第3字段（字体大小）")?;
        let font_size = font_size_str
            .parse::<usize>()
            .with_context(|| format!("字号字段解析为整数失败: '{font_size_str}'"))?;

        let color_str = styles_it.next().context("p属性缺少第4字段（弹幕颜色）")?;
        let color = decode_rgb(
            color_str
                .parse::<u32>()
                .with_context(|| format!("颜色字段解析为整数失败: '{color_str}'"))?,
        );

        // 忽略字段5~9: 发送时间戳、弹幕池、发送者哈希、弹幕ID、屏蔽权重
        for _ in 0..5 {
            styles_it.next();
        }

        let content_str = cow_u8_to_str(content)
            .context("弹幕文本内容UTF-8解码失败，内容可能包含非法字节序列")?;

        if mode_id == 7 {
            let json: Vec<serde_json::Value> =
                serde_json::from_str(&content_str).with_context(|| {
                    format!("高级弹幕 (mode=7) JSON 解析失败，前80字符: {content_str:.80}")
                })?;

            let x = json.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = json.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);

            let gradient = json
                .get(2)
                .and_then(|v| v.as_str())
                .and_then(|s| {
                    let parts: Vec<&str> = s.split('-').collect();
                    if parts.len() == 2 {
                        Some(OpacityGradient::new(
                            parts[0].parse::<f64>().ok()?,
                            parts[1].parse::<f64>().ok()?,
                        ))
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| OpacityGradient::new(1.0, 1.0));

            let duration = json
                .get(3)
                .and_then(|v| v.as_f64())
                .map(Duration::from_secs_f64)
                .unwrap_or(Duration::ZERO);

            let text = unescape(json.get(4).and_then(|v| v.as_str()).unwrap_or(""))
                .map_err(|e| anyhow!("弹幕文本XML实体解码失败: {e}"))?
                .to_string();

            let z_rotate = json.get(5).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y_rotate = json.get(6).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let end_x = json.get(7).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let end_y = json.get(8).and_then(|v| v.as_f64()).unwrap_or(0.0);

            let move_duration = json
                .get(9)
                .and_then(|v| v.as_f64())
                .map(|ms| Duration::from_millis(ms as u64))
                .unwrap_or(Duration::ZERO);

            let move_delay = json
                .get(10)
                .and_then(|v| v.as_f64())
                .map(|ms| Duration::from_millis(ms as u64))
                .unwrap_or(Duration::ZERO);

            let stroke = json
                .get(11)
                .and_then(|v| v.as_u64())
                .map(|v| v != 0)
                .unwrap_or(false);
            let font_family = json
                .get(12)
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_default();
            let linear_speed_up = json
                .get(13)
                .and_then(|v| v.as_u64())
                .map(|v| v != 0)
                .unwrap_or(false);

            Ok(Some(Self {
                time,
                mode: DanmakuMode::Advance {
                    x,
                    y,
                    gradient,
                    duration,
                    end_x,
                    end_y,
                    z_rotate,
                    y_rotate,
                    move_duration,
                    move_delay,
                    stroke,
                    font_family,
                    linear_speed_up,
                },
                font_size,
                color,
                text,
            }))
        } else {
            let text = unescape(&content_str)
                .map_err(|e| anyhow!("弹幕实体解码失败: {e}"))?
                .into_owned();
            match DanmakuMode::from_id(mode_id) {
                Some(mode) => Ok(Some(Self {
                    time,
                    mode,
                    font_size,
                    color,
                    text,
                })),
                None => Ok(None),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum DanmakuMode {
    Scroll,
    Bottom,
    Top,
    Reverse,
    #[allow(dead_code)]
    Advance {
        x: f64,
        y: f64,
        gradient: OpacityGradient,
        duration: Duration,
        end_x: f64,
        end_y: f64,
        z_rotate: f64,
        y_rotate: f64,
        move_duration: Duration,
        move_delay: Duration,
        stroke: bool,
        font_family: String,
        linear_speed_up: bool,
    },
    Bas,
}

impl DanmakuMode {
    pub fn from_id(id: usize) -> Option<Self> {
        match id {
            1..=3 => Some(Self::Scroll),
            4 => Some(Self::Bottom),
            5 => Some(Self::Top),
            6 => Some(Self::Reverse),
            9 => Some(Self::Bas),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OpacityGradient {
    pub start: f64,
    pub end: f64,
}

impl OpacityGradient {
    pub fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }
}

pub fn get_danmuku_xml_by_bili_id(id: &str) -> anyhow::Result<String> {
    let cid = get_cid_from_api(id).with_context(|| format!("获取B站视频cid失败 (bvid: {id})"))?;
    let xml = get_danmaku_xml(cid).with_context(|| format!("获取弹幕xml失败 (cid: {cid})"))?;

    Ok(xml)
}

pub fn get_danmuku_xml_from_file(file: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(file).with_context(|| format!("读取弹幕文件失败: {}", file.display()))?;
    let xml = decode_bytes(bytes, "").context("解码弹幕文件编码失败")?;
    Ok(xml)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_danmaku_mode_from_id_scroll_1() {
        assert!(matches!(
            DanmakuMode::from_id(1),
            Some(DanmakuMode::Scroll)
        ));
    }

    #[test]
    fn test_danmaku_mode_from_id_scroll_2() {
        assert!(matches!(
            DanmakuMode::from_id(2),
            Some(DanmakuMode::Scroll)
        ));
    }

    #[test]
    fn test_danmaku_mode_from_id_scroll_3() {
        assert!(matches!(
            DanmakuMode::from_id(3),
            Some(DanmakuMode::Scroll)
        ));
    }

    #[test]
    fn test_danmaku_mode_from_id_bottom() {
        assert!(matches!(
            DanmakuMode::from_id(4),
            Some(DanmakuMode::Bottom)
        ));
    }

    #[test]
    fn test_danmaku_mode_from_id_top() {
        assert!(matches!(
            DanmakuMode::from_id(5),
            Some(DanmakuMode::Top)
        ));
    }

    #[test]
    fn test_danmaku_mode_from_id_reverse() {
        assert!(matches!(
            DanmakuMode::from_id(6),
            Some(DanmakuMode::Reverse)
        ));
    }

    #[test]
    fn test_danmaku_mode_from_id_bas() {
        assert!(matches!(DanmakuMode::from_id(9), Some(DanmakuMode::Bas)));
    }

    #[test]
    fn test_danmaku_mode_from_id_unknown() {
        assert!(DanmakuMode::from_id(0).is_none());
        assert!(DanmakuMode::from_id(8).is_none());
        assert!(DanmakuMode::from_id(99).is_none());
    }

    #[test]
    fn test_escape_text_no_special_chars() {
        assert_eq!(escape_text("hello"), "hello");
    }

    #[test]
    fn test_escape_text_ampersand() {
        assert_eq!(escape_text("a&b"), "a&amp;b");
    }

    #[test]
    fn test_escape_text_lt() {
        assert_eq!(escape_text("a<b"), "a&lt;b");
    }

    #[test]
    fn test_escape_text_mixed() {
        assert_eq!(escape_text("a&b<c"), "a&amp;b&lt;c");
    }

    #[test]
    fn test_escape_text_multiple() {
        assert_eq!(escape_text("&&"), "&amp;&amp;");
    }

    #[test]
    fn test_fix_bili_xml_simple_valid() {
        let xml = "<i><d p=\"1.0,1,25,16777215\">hello</d></i>";
        let result = fix_bili_xml(xml);
        assert!(result.contains("<i>"));
        assert!(result.contains("<d p=\"1.0,1,25,16777215\">"));
    }

    #[test]
    fn test_fix_bili_xml_escapes_unknown_text() {
        let xml = "raw text & special < chars";
        let result = fix_bili_xml(xml);
        assert!(result.contains("&amp;"));
        assert!(result.contains("&lt;"));
    }

    #[test]
    fn test_fix_bili_xml_preserves_text_inside_tags() {
        let xml = "<i>hello & world</i>";
        let result = fix_bili_xml(xml);
        assert_eq!(result, "<i>hello & world</i>");
    }

    #[test]
    fn test_fix_bili_xml_known_tags_pass_through() {
        let xml = "<i><chatserver>chat.bilibili.com</chatserver></i>";
        let result = fix_bili_xml(xml);
        assert!(result.contains("<chatserver>"));
        assert!(result.contains("chat.bilibili.com"));
    }

    #[test]
    fn test_danmaku_new_scroll() {
        let dan = Danmaku::new(
            Cow::Borrowed("1.0,1,25,16777215"),
            Cow::Borrowed("Hello World".as_bytes()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dan.time, Duration::from_secs_f64(1.0));
        assert!(matches!(dan.mode, DanmakuMode::Scroll));
        assert_eq!(dan.font_size, 25);
        assert_eq!(dan.color, Rgb([255, 255, 255]));
        assert_eq!(dan.text, "Hello World");
    }

    #[test]
    fn test_danmaku_new_bottom() {
        let dan = Danmaku::new(
            Cow::Borrowed("5.0,4,18,255"),
            Cow::Borrowed("bottom text".as_bytes()),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(dan.mode, DanmakuMode::Bottom));
        assert_eq!(dan.text, "bottom text");
        assert_eq!(dan.color, Rgb([0, 0, 255]));
    }

    #[test]
    fn test_danmaku_new_top() {
        let dan = Danmaku::new(
            Cow::Borrowed("3.5,5,30,16711680"),
            Cow::Borrowed("top text".as_bytes()),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(dan.mode, DanmakuMode::Top));
        assert_eq!(dan.text, "top text");
    }

    #[test]
    fn test_danmaku_new_unsupported_mode_returns_none() {
        let dan = Danmaku::new(
            Cow::Borrowed("1.0,8,25,16777215"),
            Cow::Borrowed("unknown".as_bytes()),
        )
        .unwrap();
        assert!(dan.is_none());
    }

    #[test]
    fn test_danmaku_new_missing_fields() {
        let result = Danmaku::new(
            Cow::Borrowed("1.0"),
            Cow::Borrowed("text".as_bytes()),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_danmakus_basic() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<i>
    <d p="1.0,1,25,16777215,1234567890,0,a1b2c3,1001,0">Hello</d>
    <d p="2.0,4,18,255,1234567891,0,d4e5f6,1002,0">Bottom</d>
</i>"#
            .to_string();
        let danmakus = parse_danmakus(xml).unwrap();
        assert_eq!(danmakus.len(), 2);
        assert_eq!(danmakus[0].text, "Hello");
        assert!(matches!(danmakus[0].mode, DanmakuMode::Scroll));
        assert_eq!(danmakus[1].text, "Bottom");
        assert!(matches!(danmakus[1].mode, DanmakuMode::Bottom));
    }

    #[test]
    fn test_parse_danmakus_empty() {
        let xml = "<i></i>".to_string();
        let danmakus = parse_danmakus(xml).unwrap();
        assert!(danmakus.is_empty());
    }

    #[test]
    fn test_parse_danmakus_skips_unsupported_modes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<i>
    <d p="1.0,1,25,16777215">scroll</d>
    <d p="2.0,8,18,255">unknown</d>
    <d p="3.0,5,30,16711680">top</d>
</i>"#
            .to_string();
        let danmakus = parse_danmakus(xml).unwrap();
        assert_eq!(danmakus.len(), 2);
        assert_eq!(danmakus[0].text, "scroll");
        assert_eq!(danmakus[1].text, "top");
    }

    #[test]
    fn test_parse_danmakus_xml_entities() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<i>
    <d p="1.0,1,25,16777215">Hello &amp; World &lt;3</d>
</i>"#
            .to_string();
        let danmakus = parse_danmakus(xml).unwrap();
        assert_eq!(danmakus.len(), 1);
        assert_eq!(danmakus[0].text, "Hello & World <3");
    }
}
