use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use crate::utils::decode_bytes;

pub fn get_cid_from_api(bvid: &str) -> anyhow::Result<u64> {
    let url = format!("https://api.bilibili.com/x/web-interface/view?bvid={bvid}");
    let text = client()?
        .get(&url)
        .send()
        .with_context(|| format!("HTTP 请求 B站 API 失败: {url}"))?
        .text()
        .with_context(|| format!("读取 B站 API 响应失败 (bvid: {bvid})"))?;
    let resp: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("解析 B站 API 响应 JSON 失败 (bvid: {bvid})"))?;

    let code = resp["code"]
        .as_i64()
        .ok_or_else(|| anyhow!("B站 API 响应中缺少 code 字段 (bvid: {bvid})"))?;

    if code != 0 {
        let msg = resp["message"].as_str().unwrap_or("未知错误");
        bail!("B站 API 返回错误 (bvid: {bvid}, code: {code}, message: {msg})");
    }

    resp["data"]["cid"]
        .as_u64()
        .ok_or_else(|| anyhow!("B站 API 响应中未找到 data.cid 字段 (bvid: {bvid})"))
}

pub fn get_danmaku_xml(cid: u64) -> anyhow::Result<String> {
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

    if data.is_empty() {
        return Ok(data.to_vec());
    }

    if data[0] == 0x78 {
        let mut decoder = flate2::read::ZlibDecoder::new(data);
        let mut decompressed = Vec::new();
        match decoder.read_to_end(&mut decompressed) {
            Ok(_) if !decompressed.is_empty() => return Ok(decompressed),
            _ => {}
        }
    }

    let mut decoder = DeflateDecoder::new(data);
    let mut decompressed = Vec::new();
    match decoder.read_to_end(&mut decompressed) {
        Ok(_) if !decompressed.is_empty() => return Ok(decompressed),
        _ => {}
    }

    Ok(data.to_vec())
}

fn client() -> anyhow::Result<&'static reqwest::blocking::Client> {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    Ok(CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36 Edg/150.0.0.0")
            .timeout(Duration::from_secs(30))
            .build()
            .expect("创建HTTP客户端失败，请检查系统网络环境")
    }))
}