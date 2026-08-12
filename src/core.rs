use core::panic;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossbeam_channel::bounded;
use image::{Rgb, RgbImage};

use crate::{
    danmaku::{Danmaku, DanmakuMode},
    decoder::VideoDecoder,
    encoder::FfmpegEncoder,
    fonts::FontStack,
    i18n::Lang,
    interaction::Args,
    layout::{
        Direction, DrawParams, NormalComponent, compute_max_danmaku_deadline, compute_n_rails,
        del_dead, draw_fixed_danmakus, draw_scroll_danmakus, scroll,
    },
    utils::{GrowableVec, Ignore, sprite_ink_bounds},
};

const PROGRESS_INTERVAL_MS: u64 = 100;

pub(crate) fn video_process(
    mut decoder: VideoDecoder,
    mut encoder: FfmpegEncoder,
    mut danmakus: Vec<Danmaku>,
    args: &Args,
    frame_duration_secs: f64,
    range: Option<(f64, f64)>,
    lang: Lang,
) -> Result<()> {
    if let Some((start, end)) = range {
        decoder.set_range(start, end);
        danmakus.retain(|dan| {
            let t = dan.time.as_secs_f64();
            t >= start && t < end
        });
        let shift = Duration::from_secs_f64(start);
        for dan in &mut danmakus {
            dan.time = dan.time.saturating_sub(shift);
        }
    }

    danmakus.sort_unstable_by_key(|dan| std::cmp::Reverse(dan.time));

    let (video_width, video_height) = decoder.size();
    let area_top = (video_height as f64 * args.top_ratio) as u32;
    let area_bottom = (video_height as f64 * args.bottom_ratio) as u32;
    let area_height = area_bottom - area_top;

    let mut scroll_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut top_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut bottom_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut reverse_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);

    let mut fonts = FontStack::load(args).context("字体加载失败")?;

    // 标准字号（25 × font_scale）参考墨迹高度：轨道基准间距 = 墨迹高度 + line_spacing，
    // 保证标准字号弹幕的相邻行视觉间隙恰为 line_spacing（轨道间无死区）。
    let sample_img = fonts.render_sprite("字", 25.0 * args.font_scale, Rgb([255, 255, 255]));
    let ink_ref = sprite_ink_bounds(&sample_img)
        .map(|(t, b)| b.saturating_sub(t))
        .unwrap_or(1)
        .max(1);
    let base_pitch = ink_ref.saturating_add(args.line_spacing).max(1);
    let rail_cnt = area_height / base_pitch;

    let mut frame_count = 0u64;
    let mut total_frames = match range {
        Some((start, end)) => ((end - start) / frame_duration_secs).ceil() as u64,
        None => decoder.frame_count(),
    };
    let total_reporter = Arc::new(AtomicU64::new(0));

    if args.longest {
        let video_duration = match range {
            Some((start, end)) => end - start,
            None if decoder.frame_rate() > 0.0 => {
                decoder.frame_count() as f64 / decoder.frame_rate() as f64
            }
            _ => 0.0,
        };
        let max_deadline = compute_max_danmaku_deadline(
            &danmakus,
            &mut fonts,
            args,
            video_width,
            frame_duration_secs,
        );
        if max_deadline > video_duration {
            let ext_stop_orig = match range {
                Some((start, end)) => (max_deadline + start).min(end),
                None => max_deadline,
            };
            decoder.set_extend_to(ext_stop_orig, frame_duration_secs);
            total_frames =
                ((max_deadline + frame_duration_secs) / frame_duration_secs).ceil() as u64;
        }
    }
    decoder.set_total_reporter(total_reporter.clone());

    thread::scope(move |s| -> Result<()> {
        const RECYCLE_LIM: usize = 8;
        let (recycle_s, recycle_r) = bounded::<RgbImage>(RECYCLE_LIM);
        for _ in 0..RECYCLE_LIM {
            recycle_s
                .send(RgbImage::new(video_width, video_height))
                .expect("初始化帧缓冲池失败");
        }

        let (decode_s, decode_r) = bounded(RECYCLE_LIM);
        let decode_producer = thread::Builder::new()
            .name("decode".to_string())
            .spawn_scoped(s, move || -> Result<()> {
                loop {
                    let Ok(mut image) = recycle_r.recv() else {
                        break;
                    };
                    let ts_secs = match decoder.next_frame_into(&mut image)? {
                        Some(ts) => ts,
                        None => break,
                    };
                    let dur = Duration::from_secs_f64(ts_secs);
                    if decode_s.send((ts_secs, dur, image)).is_err() {
                        break;
                    }
                }
                Ok(())
            });

        let (encode_s, encode_v) = bounded(RECYCLE_LIM);
        let process_pipeline = thread::Builder::new()
            .name("render".to_string())
            .spawn_scoped(s, move || -> Result<()> {
                let mut last_progress =
                    Instant::now() - Duration::from_millis(PROGRESS_INTERVAL_MS);
                let mut final_shown = false;
                loop {
                    let Ok((ts_secs, dur, mut image)) = decode_r.recv() else {
                        break;
                    };
                    let ready_idx = danmakus.partition_point(|dan| dan.time > dur);
                    let enqueue = danmakus.drain(ready_idx..).rev();

                    for d in enqueue {
                        let scale = (d.font_size as f32) * args.font_scale;
                        let color = d.color;
                        let cached_text = fonts.render_sprite(&d.text, scale, color);
                        let width = cached_text.width();
                        // 虚拟轨道数：该字号墨迹高度需要几个基础轨道（B站虚拟轨道机制）
                        let n_rails = compute_n_rails(ink_ref, base_pitch, d.font_size);
                        let (travel, dead_line) = match d.mode {
                            DanmakuMode::Scroll | DanmakuMode::Reverse => {
                                let travel_frames = (width + video_width).div_ceil(args.speed);
                                let travel = Duration::from_secs_f64(
                                    travel_frames as f64 * frame_duration_secs,
                                );
                                (travel, dur + travel)
                            }
                            DanmakuMode::Top | DanmakuMode::Bottom => {
                                let travel = Duration::from_secs_f64(args.fixed_duration);
                                (travel, dur + travel)
                            }
                            _ => (Duration::ZERO, Duration::from_millis(0)),
                        };
                        match d.mode {
                            DanmakuMode::Scroll => scroll_slots
                                .set_first_empty((
                                    d,
                                    NormalComponent {
                                        x: video_width as i64,
                                        y: None,
                                        width,
                                        n_rails,
                                        travel,
                                        dead_line,
                                        cached_text: Some(cached_text),
                                    },
                                ))
                                .ignore(),
                            DanmakuMode::Bottom => bottom_slots
                                .set_first_empty((
                                    d,
                                    NormalComponent {
                                        x: video_width as i64 / 2 - width as i64 / 2,
                                        y: None,
                                        width,
                                        n_rails,
                                        travel,
                                        dead_line,
                                        cached_text: Some(cached_text),
                                    },
                                ))
                                .ignore(),
                            DanmakuMode::Top => top_slots
                                .set_first_empty((
                                    d,
                                    NormalComponent {
                                        x: video_width as i64 / 2 - width as i64 / 2,
                                        y: None,
                                        width,
                                        n_rails,
                                        travel,
                                        dead_line,
                                        cached_text: Some(cached_text),
                                    },
                                ))
                                .ignore(),
                            DanmakuMode::Reverse => reverse_slots
                                .set_first_empty((
                                    d,
                                    NormalComponent {
                                        x: -(width as i64),
                                        y: None,
                                        width,
                                        n_rails,
                                        travel,
                                        dead_line,
                                        cached_text: Some(cached_text),
                                    },
                                ))
                                .ignore(),
                            _ => (),
                        };
                    }

                    del_dead(&mut scroll_slots, dur);
                    del_dead(&mut reverse_slots, dur);
                    del_dead(&mut top_slots, dur);
                    del_dead(&mut bottom_slots, dur);

                    let mut draw_params = DrawParams {
                        image: &mut image,
                        base_pitch,
                        rail_cnt,
                        area_top: area_top as i64,
                        opacity: args.opacity,
                        min_space: args.min_space as i64,
                        now: dur,
                    };

                    draw_scroll_danmakus(&mut draw_params, &mut scroll_slots, Direction::ToLeft);
                    draw_scroll_danmakus(&mut draw_params, &mut reverse_slots, Direction::ToRight);
                    scroll(
                        scroll_slots
                            .iter_mut()
                            .filter_map(|cur| cur.as_mut().map(|(_, c)| c))
                            .filter(|c| c.y.is_some()),
                        Direction::ToLeft,
                        args.speed,
                    );
                    scroll(
                        reverse_slots
                            .iter_mut()
                            .filter_map(|cur| cur.as_mut().map(|(_, c)| c))
                            .filter(|c| c.y.is_some()),
                        Direction::ToRight,
                        args.speed,
                    );

                    draw_fixed_danmakus(&mut draw_params, &mut top_slots, false);
                    draw_fixed_danmakus(&mut draw_params, &mut bottom_slots, true);

                    if encode_s.send((image, ts_secs)).is_err() {
                        break;
                    }
                    frame_count += 1;
                    if !args.quiet {
                        let exact = total_reporter.load(Ordering::Relaxed);
                        let is_final = exact > 0 && frame_count >= exact;
                        let now = Instant::now();
                        if is_final
                            || now - last_progress >= Duration::from_millis(PROGRESS_INTERVAL_MS)
                        {
                            render_progress(frame_count, total_frames, exact, lang);
                            last_progress = now;
                            if is_final {
                                final_shown = true;
                            }
                        }
                    }
                }

                // 收尾刷新：解码线程在通道关闭前已写入精确总数，
                // 补打一次确保进度条走到 100%
                if !args.quiet && !final_shown {
                    let exact = total_reporter.load(Ordering::Relaxed);
                    render_progress(frame_count, total_frames, exact, lang);
                    if exact == 0 || frame_count < exact {
                        // 异常路径：收尾行未带换行，补一个避免后续输出粘连
                        eprintln!();
                    }
                }

                Ok(())
            });

        let encode_customer = thread::Builder::new()
            .name("encode".to_string())
            .spawn_scoped(s, move || -> Result<()> {
                loop {
                    let t0 = Instant::now();
                    let Ok((image, ts_secs)) = encode_v.recv() else {
                        break;
                    };
                    encoder.timings.wait_us += t0.elapsed().as_micros();

                    encoder
                        .encode(&image, ts_secs)
                        .with_context(|| format!("编码帧失败 (时间戳: {ts_secs})"))?;
                    let _ = recycle_s.send(image);
                }

                encoder
                    .finish()
                    .context("编码器完成写入失败（输出文件不可用）")?;
                Ok(())
            });

        let res: Result<Vec<_>, _> = [decode_producer, process_pipeline, encode_customer]
            .into_iter()
            .collect();
        let handles = res.context("系统错误: 无法创建线程")?;
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(res) => res,
                Err(e) => {
                    if let Some(msg) = e.downcast_ref::<&'static str>() {
                        panic!("线程 panic: {msg}")
                    } else if let Some(msg) = e.downcast_ref::<String>() {
                        panic!("线程 panic: {msg}")
                    } else {
                        panic!("线程 panic: 未知错误")
                    }
                }
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(())
    })?;

    Ok(())
}

fn render_progress(current: u64, estimate: u64, exact_total: u64, lang: Lang) {
    const BAR_WIDTH: usize = 30;

    let total = if exact_total > 0 {
        exact_total
    } else {
        estimate.max(current)
    };
    if total == 0 {
        eprint!("\r{}", lang.t_fmt("render_progress", current));
    } else {
        let pct = (current as f64 / total as f64 * 100.0) as u32;
        let filled = ((BAR_WIDTH as f64 * current as f64 / total as f64) as usize).min(BAR_WIDTH);
        let empty = BAR_WIDTH - filled;
        if exact_total > 0 && current >= exact_total {
            eprintln!(
                "\r[{}{}] {:>3}% ({current}/{total})",
                "█".repeat(filled),
                "░".repeat(empty),
                pct,
            );
        } else {
            eprint!(
                "\r[{}{}] {:>3}% ({current}/{total})",
                "█".repeat(filled),
                "░".repeat(empty),
                pct,
            );
        }
    }
}
