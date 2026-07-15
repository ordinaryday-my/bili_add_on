use anyhow::{Context, anyhow, bail};
use crate::utils::decode_bytes;

pub fn get_cid(page: &str) -> anyhow::Result<&str> {
    let re = regex::Regex::new(r#""cid":(\d+)"#).context("编译cid提取用正则表达式失败")?;
    let capss = re
        .captures_iter(page);

    for caps in capss {
        match caps.get(1) {
            Some(m) => {
                let s = m.as_str();
                if s.parse::<i64>().is_err() {
                    continue;
                }
                return Ok(s)
            },
            None => bail!("cid正则已匹配但无法提取数字捕获组，正则执行结果异常"),
        };
    } 

    Err(anyhow!("B站视频页面中未找到cid信息（可能页面结构已变更）"))
}

pub fn get_danmaku_xml(cid: &str) -> anyhow::Result<String> {
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

    let bytes = maybe_decompress(&bytes).context("弹幕XML数据解压（deflate）失败，数据可能损坏或使用了不支持的压缩格式")?;

    decode_bytes(bytes, &content_type)
}

pub fn maybe_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> {
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

pub fn fetch_bili_vedio_page(id: &str) -> anyhow::Result<String> {
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
            .expect("创建HTTP客户端失败，请检查系统网络环境")
    }))
}