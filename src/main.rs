use anyhow::{anyhow, bail, Context};
use clap::Parser;
use image::Rgb;
use quick_xml::{
    events::{attributes::Attribute, Event},
    XmlVersion,
};
use std::{borrow::Cow, ffi::OsString, fs, path::PathBuf, process::exit, str, time::Duration};

mod utils;

#[derive(Debug, Parser)]
#[command(version, author, about)]
struct Args {
    #[arg(long, help = "视频源文件路径")]
    path: PathBuf,

    #[arg(long, short, help = "输出视频路径（默认在源文件名前添加 bili_add_on_ 前缀）")]
    output: Option<PathBuf>,

    #[command(flatten)]
    source: DanmakuSource,

    #[arg(long, default_value_t = 0.5, help = "弹幕不透明度，取值范围 0~1")]
    opacity: f64,

    #[arg(long, short, default_value_t = 0.0, help = "弹幕显示区域上界与画面高度的比值，0 为顶端")]
    upper_limit: f64,

    #[arg(long, short, default_value_t = 1.0, help = "弹幕显示区域下界与画面高度的比值，1 为底端")]
    lower_limit: f64,

    #[arg(long, default_value_t = 20, help = "弹幕字号（像素）")]
    font_size: u32,

    #[arg(long, default_value_t = 3, help = "弹幕滚动速度（像素每帧）")]
    speed: u32,
}

impl Args {
    fn check(&self) -> anyhow::Result<()> {
        if !self.path.exists() {
            bail!("视频源不存在: {}", self.path.display());
        }

        if self.path.is_dir() {
            bail!("不能输入目录（视频源）: {}", self.path.display());
        }

        if let Some(p) = &self.source.danmaku_file {
            if !p.exists() {
                bail!("弹幕文件不存在: {}", p.display());
            }

            if p.extension().unwrap() != "xml" {
                bail!("仅支持xml弹幕文件，当前文件: {}", p.display());
            }

            if p.is_dir() {
                bail!("不能输入目录（弹幕源）: {}", p.display());
            }
        }

        Ok(())
    }

    fn check_output(&mut self) -> anyhow::Result<()> {
        if self.output.is_none() {
            let mut from = self.path.clone();
            let mut prefix = OsString::from("bili_add_on");
            prefix.push(
                from.file_name()
                    .with_context(|| format!("无法从路径获取文件名: {}", self.path.display()))?,
            );
            from.set_file_name(prefix);

            self.output = Some(from);
        }

        Ok(())
    }
}

#[derive(clap::Args, Debug)]
#[group(required = true, multiple = false)]
struct DanmakuSource {
    #[arg(long, short, help = "B站视频 ID（如 BV1fRNH6kEra），将自动拉取对应弹幕")]
    bili_id: Option<String>,

    #[arg(long, help = "本地弹幕 XML 文件路径")]
    danmaku_file: Option<PathBuf>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e:#}");
        exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = Args::parse();
    args.check().context("参数错误")?;
    args.check_output().context("生成输出路径失败")?;
    let args = args;

    let danmakus = if let Some(id) = &args.source.bili_id {
        let page = fetch_bili_vedio_page(id)
            .with_context(|| format!("获取B站视频页面失败 (id: {id})"))?;
        let cid = get_cid(&page).context("解析cid失败")?;
        let xml = get_danmaku_xml(cid)
            .with_context(|| format!("获取弹幕xml失败 (cid: {cid})"))?;
        parse_danmakus(xml).context("解析弹幕xml失败")?
    } else {
        let file = args
            .source
            .danmaku_file
            .as_ref()
            .expect("danmaku_file应为Some（由clap确保）");

        let bytes = fs::read(file)
            .with_context(|| format!("读取弹幕文件失败: {}", file.display()))?;
        let xml = decode_bytes(bytes, "")
            .context("解码弹幕文件编码失败")?;
        parse_danmakus(xml).context("解析弹幕xml失败")?
    }; 

    Ok(())
}

fn parse_danmakus(xml: String) -> anyhow::Result<Vec<Danmaku>> {
    let xml = fix_bili_xml(&xml); 
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut danmakus = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref s) if s.name().as_ref() == b"d" => {
                let p_attr = s
                    .attributes()
                    .flatten()
                    .find(|attr: &Attribute<'_>| attr.key.as_ref() == b"p")
                    .map(|attr| attr.normalized_value(XmlVersion::Explicit1_1))
                    .transpose()?
                    .unwrap_or_default();

                let content = reader.read_text(s.name())?.into_inner();

                if let Some(d) = Danmaku::new(p_attr, content).with_context(|| {
                    let index = danmakus.len() + 1;
                    format!("第{index}条弹幕解析失败")
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

fn fix_bili_xml(xml: &str) -> String {
    let tag_re = regex::Regex::new(r"</?([a-zA-Z]\w*)(\s[^<>]*)?/?>|<\?[^>]*\?>").unwrap();
    let known: &[&str] = &[
        "i", "chatserver", "chatid", "mission", "maxlimit",
        "state", "real_name", "source", "ds", "d",
    ];

    let mut result = String::with_capacity(xml.len() + 2048);
    let mut pos = 0;

    for caps in tag_re.captures_iter(xml) {
        let m = caps.get(0).unwrap();
        let is_known = caps
            .get(1)
            .map(|g| known.contains(&g.as_str()))
            .unwrap_or(true); // <?...?> is always preserved

        if m.start() < pos {
            continue;
        }

        if is_known {
            let text = &xml[pos..m.start()];
            result.push_str(&escape_text(text));
            result.push_str(m.as_str());
            pos = m.end();
        }
    }

    if pos < xml.len() {
        result.push_str(&escape_text(&xml[pos..]));
    }

    result
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

#[derive(Debug, Clone)]
struct Danmaku {
    time: Duration,
    mode: DanmakuMode,
    font_size: usize,
    color: Rgb<u8>,
    text: String,
}

impl Danmaku {
    fn new(styles: Cow<'_, str>, content: Cow<'_, [u8]>) -> anyhow::Result<Option<Self>> {
        let mut styles_it = styles.split(',');

        let time_str = styles_it.next().context("p属性缺少时间字段")?;
        let time = Duration::from_secs_f64(
            time_str
                .parse::<f64>()
                .with_context(|| format!("时间字段解析为f64失败: '{time_str}'"))?,
        );

        let mode_str = styles_it.next().context("p属性缺少模式字段")?;
        let mode_id = mode_str
            .parse::<usize>()
            .with_context(|| format!("模式字段解析为整数失败: '{mode_str}'"))?;

        let font_size_str = styles_it.next().context("p属性缺少字号字段")?;
        let font_size = font_size_str
            .parse::<usize>()
            .with_context(|| format!("字号字段解析为整数失败: '{font_size_str}'"))?;

        let color_str = styles_it.next().context("p属性缺少颜色字段")?;
        let color = decode_rgb(
            color_str
                .parse::<u32>()
                .with_context(|| format!("颜色字段解析为整数失败: '{color_str}'"))?,
        );

        // 忽略字段5~9: 发送时间戳、弹幕池、发送者哈希、弹幕ID、屏蔽权重
        for _ in 0..5 {
            styles_it.next();
        }

        let content_str = cow_u8_to_str(content).context("弹幕内容UTF-8解码失败")?;

        if mode_id == 7 {
            let json: Vec<serde_json::Value> = serde_json::from_str(&content_str)
                .with_context(|| format!("高级弹幕JSON解析失败，内容: {content_str:.80}"))?;

            let x = json.get(0).and_then(|v| v.as_f64()).unwrap_or(0.0);
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

            let text = json.get(4).and_then(|v| v.as_str()).unwrap_or("").to_string();

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

            let stroke = json.get(11).and_then(|v| v.as_u64()).map(|v| v != 0).unwrap_or(false);
            let font_family = json.get(12).and_then(|v| v.as_str()).map(String::from).unwrap_or_default();
            let linear_speed_up = json.get(13).and_then(|v| v.as_u64()).map(|v| v != 0).unwrap_or(false);

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
            let text = content_str.into_owned();
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
enum DanmakuMode {
    Scroll,
    Bottom,
    Top,
    Reverse,
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
    fn from_id(id: usize) -> Option<Self> {
        match id {
            1 | 2 | 3 => Some(Self::Scroll),
            4 => Some(Self::Bottom),
            5 => Some(Self::Top),
            6 => Some(Self::Reverse),
            9 => Some(Self::Bas),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct OpacityGradient {
    start: f64,
    end: f64,
}

impl OpacityGradient {
    fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }
}

fn get_cid<'a>(page: &'a str) -> anyhow::Result<&'a str> {
    let re = regex::Regex::new(r#""cid":(\d+)"#).context("编译cid正则表达式失败")?;
    let caps = re
        .captures(page)
        .or_else(|| {
            regex::Regex::new(r#""last_play_cid":(\d+)"#)
                .ok()?
                .captures(page)
        })
        .ok_or_else(|| anyhow!("B站视频页面中未找到cid信息（可能页面结构已变更）"))?;
    match caps.get(1) {
        Some(m) => Ok(m.as_str()),
        None => bail!("cid正则匹配成功但无法提取捕获组1"),
    }
}

fn get_danmaku_xml(cid: &str) -> anyhow::Result<String> {
    let url = format!("https://comment.bilibili.com/{cid}.xml");
    let response = client()?
        .get(&url)
        .send()
        .with_context(|| format!("HTTP请求弹幕xml失败: {url}"))?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response
        .bytes()
        .with_context(|| format!("读取弹幕xml响应体失败 (cid: {cid})"))?;

    let bytes = maybe_decompress(&bytes).context("deflate解压失败")?;

    decode_bytes(bytes, &content_type)
}

fn maybe_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use flate2::read::DeflateDecoder;
    use std::io::Read;

    // Try raw deflate first (RFC 1951)
    let mut decoder = DeflateDecoder::new(data);
    let mut decompressed = Vec::new();
    if decoder.read_to_end(&mut decompressed).is_ok() && !decompressed.is_empty() {
        return Ok(decompressed);
    }

    // Try zlib-wrapped deflate (RFC 1950)
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    decompressed.clear();
    if decoder.read_to_end(&mut decompressed).is_ok() && !decompressed.is_empty() {
        return Ok(decompressed);
    }

    // Not compressed, return as-is
    Ok(data.to_vec())
}

fn fetch_bili_vedio_page(id: &str) -> anyhow::Result<String> {
    let url = format!("https://www.bilibili.com/video/{id}");
    let text = client()?
        .get(&url)
        .send()
        .with_context(|| format!("HTTP请求B站视频页面失败: {url}"))?
        .text()
        .with_context(|| format!("读取B站视频页面响应失败 (id: {id})"))?;
    Ok(text)
}

fn client() -> anyhow::Result<&'static reqwest::blocking::Client> {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    Ok(CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36 Edg/150.0.0.0")
            .build()
            .expect("failed to build HTTP client")
    }))
}

fn decode_bytes(bytes: impl AsRef<[u8]>, content_type: &str) -> anyhow::Result<String> {
    let bytes = bytes.as_ref();

    // 如果 Content-Type 明确声明了 GB 系列编码，优先用 GBK
    let ct_lower = content_type.to_lowercase();
    if ct_lower.contains("gbk") || ct_lower.contains("gb2312") || ct_lower.contains("gb18030") {
        let (text, _, _) = encoding_rs::GBK.decode(bytes);
        return Ok(text.into_owned());
    }

    // 否则尝试 UTF-8，失败则回退到 GBK
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_string());
    }

    let (text, _, _) = encoding_rs::GBK.decode(bytes);
    Ok(text.into_owned())
}

fn cow_u8_to_str(data: Cow<'_, [u8]>) -> anyhow::Result<Cow<'_, str>> {
    match data {
        Cow::Borrowed(bytes) => Ok(Cow::Borrowed(str::from_utf8(bytes)?)),
        Cow::Owned(vec) => Ok(Cow::Owned(String::from_utf8(vec)?)),
    }
}

fn decode_rgb(decimal_color: u32) -> Rgb<u8> {
    // 1. 提取RGB分量
    let r = ((decimal_color >> 16) & 0xFF) as u8;
    let g = ((decimal_color >> 8) & 0xFF) as u8;
    let b = (decimal_color & 0xFF) as u8;

    // 2. 创建 Rgb<u8> 像素
    let pixel: Rgb<u8> = Rgb([r, g, b]);

    pixel
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_fetch() {
        let res = fetch_bili_vedio_page("BV1fRNH6kEra");
        assert!(res.is_ok());
        println!("{}", res.unwrap());
    }
}