use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;

struct AudioStreamData {
    ist_index: usize,
    time_base: ffmpeg::Rational,
    params: ffmpeg::codec::Parameters,
}

pub fn has_audio(path: &Path) -> Result<bool> {
    let ictx = ffmpeg::format::input(path)
        .with_context(|| format!("无法打开文件以检测音频流: {}", path.display()))?;
    let found = ictx
        .streams()
        .any(|s| s.parameters().medium() == ffmpeg::media::Type::Audio);
    Ok(found)
}

/// 将临时视频与 `audio_source` 的音频流合并到输出文件。
///
/// 音频处理分两级（时间均为秒）：
/// 1. 若 `audio_range` 为 `Some((ar_s, ar_e))`，先按音频源时间轴裁剪为 B，
///    裁剪后 B 的起点与视频开头（时间轴 0）对齐；
/// 2. 随后与视频一起按 `video_range`（输出时间轴）裁剪得到 C：
///    保留 B 在 `[vr_s, vr_e)` 内的部分，并将其时间戳减去 `vr_s`。
///
/// 两级裁剪可合并为：有效窗口起点 `ar_s+vr_s`、终点 `min(ar_e, ar_s+vr_e)`，
/// 整体平移 `ar_s+vr_s`。
/// 例如：A=[0:00-0:30] 经 `--audio-range 5-10` 得 B=[0-5]（对齐视频起点），
/// 再经 `--range 3` 得 C = A[5s, 8s)，位于输出时间轴 [0, 3)。
pub fn remux_audio(
    video_temp: &Path,
    audio_source: &Path,
    output: &Path,
    audio_range: Option<(f64, f64)>,
    video_range: Option<(f64, f64)>,
) -> Result<()> {
    let mut audio_ictx = ffmpeg::format::input(audio_source)
        .with_context(|| format!("无法打开音频源文件: {}", audio_source.display()))?;

    let mut audio_streams: Vec<AudioStreamData> = vec![];
    let mut audio_packets: Vec<(usize, ffmpeg::Rational, ffmpeg::Packet)> = vec![];

    for ist in audio_ictx.streams() {
        if ist.parameters().medium() == ffmpeg::media::Type::Audio {
            audio_streams.push(AudioStreamData {
                ist_index: ist.index(),
                time_base: ist.time_base(),
                params: ist.parameters(),
            });
        }
    }

    if audio_streams.is_empty() {
        drop(audio_ictx);
        std::fs::copy(video_temp, output)
            .with_context(|| format!("无法复制视频文件到: {}", output.display()))?;
        return Ok(());
    }

    let (ar_s, ar_e) = audio_range.unwrap_or((0.0, f64::INFINITY));
    let (vr_s, vr_e) = video_range.unwrap_or((0.0, f64::INFINITY));
    // 合并后的有效窗口与平移量（音频源时间轴）。
    let win_s = ar_s + vr_s;
    let win_e = match (audio_range, video_range) {
        (Some(_), Some(_)) => ar_e.min(ar_s + vr_e),
        (Some(_), None) => ar_e,
        (None, Some(_)) => vr_e,
        (None, None) => f64::INFINITY,
    };
    let shift = win_s;

    for (stream, packet) in audio_ictx.packets() {
        if stream.parameters().medium() == ffmpeg::media::Type::Audio {
            let tb = stream.time_base();
            let to_secs = |v: i64| v as f64 * tb.numerator() as f64 / tb.denominator() as f64;
            let Some(pts) = packet.pts() else {
                continue;
            };
            let ts = to_secs(pts);
            if ts < win_s || ts >= win_e {
                continue;
            }
            let mut packet = packet;
            let shift_units = (shift * tb.denominator() as f64 / tb.numerator() as f64) as i64;
            packet.set_pts(packet.pts().map(|p| p - shift_units));
            packet.set_dts(packet.dts().map(|d| d - shift_units));
            audio_packets.push((stream.index(), tb, packet));
        }
    }
    drop(audio_ictx);

    let mut video_ictx = ffmpeg::format::input(video_temp)
        .with_context(|| format!("无法打开临时视频文件: {}", video_temp.display()))?;
    let mut octx = ffmpeg::format::output(output)
        .with_context(|| format!("无法创建输出文件: {}", output.display()))?;

    let mut video_mapping: HashMap<usize, usize> = HashMap::new();
    let mut audio_mapping: HashMap<usize, usize> = HashMap::new();
    let mut ost_count = 0usize;

    for ist in video_ictx.streams() {
        let codec_id = ist.parameters().id();
        let mut ost = octx
            .add_stream(
                ffmpeg::encoder::find(codec_id)
                    .with_context(|| format!("无法找到编码器 (id: {codec_id:?})"))?,
            )
            .context("无法添加视频输出流")?;
        ost.set_parameters(ist.parameters());
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
        }
        ost.set_time_base(ist.time_base());
        video_mapping.insert(ist.index(), ost.index());
        ost_count += 1;
    }

    for data in audio_streams.into_iter() {
        let codec_id = data.params.id();
        let mut ost = octx
            .add_stream(
                ffmpeg::encoder::find(codec_id)
                    .with_context(|| format!("无法找到编码器 (id: {codec_id:?})"))?,
            )
            .context("无法添加音频输出流")?;
        ost.set_parameters(data.params);
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
        }
        ost.set_time_base(data.time_base);
        audio_mapping.insert(data.ist_index, ost.index());
        ost_count += 1;
    }

    octx.set_metadata(video_ictx.metadata().to_owned());
    octx.write_header().context("写入输出文件头失败")?;

    let ost_time_bases: Vec<ffmpeg::Rational> = (0..ost_count)
        .map(|i| {
            octx.stream(i)
                .with_context(|| format!("无法获取输出流 {i}"))
                .map(|s| s.time_base())
        })
        .collect::<Result<Vec<_>>>()
        .context("获取输出流时基列表失败")?;

    for (stream, mut packet) in video_ictx.packets() {
        if let Some(&ost_index) = video_mapping.get(&stream.index()) {
            packet.rescale_ts(stream.time_base(), ost_time_bases[ost_index]);
            packet.set_position(-1);
            packet.set_stream(ost_index);
            packet
                .write_interleaved(&mut octx)
                .context("写入视频包失败")?;
        }
    }
    drop(video_ictx);

    for (ist_index, time_base, packet) in &mut audio_packets {
        if let Some(&ost_index) = audio_mapping.get(ist_index) {
            packet.rescale_ts(*time_base, ost_time_bases[ost_index]);
            packet.set_position(-1);
            packet.set_stream(ost_index);
            packet
                .write_interleaved(&mut octx)
                .context("写入音频包失败")?;
        }
    }

    octx.write_trailer().context("写入输出文件尾失败")?;

    Ok(())
}
