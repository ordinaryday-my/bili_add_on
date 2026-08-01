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

/// 将临时视频与原始视频的音频流合并到输出文件，仅保留 `[start, end)` 时段
/// （原始时间轴，秒）的音频包，并将其时间戳减去 `start` 以对齐裁剪后的视频。
///
/// `range == None` 时保留全部音频。
pub fn remux_audio_range(
    video_temp: &Path,
    original: &Path,
    output: &Path,
    range: Option<(f64, f64)>,
) -> Result<()> {
    let mut audio_ictx = ffmpeg::format::input(original)
        .with_context(|| format!("无法打开原文件以提取音频: {}", original.display()))?;

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

    for (stream, packet) in audio_ictx.packets() {
        if stream.parameters().medium() == ffmpeg::media::Type::Audio {
            if let Some((start, end)) = range {
                let tb = stream.time_base();
                let to_secs = |v: i64| v as f64 * tb.numerator() as f64 / tb.denominator() as f64;
                let Some(pts) = packet.pts() else {
                    continue;
                };
                let ts = to_secs(pts);
                if ts < start || ts >= end {
                    continue;
                }
                let mut packet = packet;
                let shift_units = (start * tb.denominator() as f64 / tb.numerator() as f64) as i64;
                packet.set_pts(packet.pts().map(|p| p - shift_units));
                packet.set_dts(packet.dts().map(|d| d - shift_units));
                audio_packets.push((stream.index(), tb, packet));
            } else {
                audio_packets.push((stream.index(), stream.time_base(), packet));
            }
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
