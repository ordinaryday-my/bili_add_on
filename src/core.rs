use std::{collections::HashSet, path::Path, time::Duration};

use ab_glyph::{FontVec, PxScale};
use anyhow::{anyhow, Context, Result};
use image::{ImageBuffer, Rgb, RgbImage};
use imageproc::drawing::{draw_text_mut, text_size};
use ndarray::Array3;
use video_rs::{encode::Settings, Decoder, Encoder, Time};

use crate::{
    core::Direction::{ToLeft, ToRight},
    danmaku::{Danmaku, DanmakuMode},
    interaction::Args,
    utils::{GrowableVec, Ignore},
};

pub fn video_process(
    mut decoder: Decoder,
    mut encoder: Encoder,
    mut danmakus: Vec<Danmaku>,
    args: &Args,
    frame_durtion: Time,
) -> Result<()> {
    // let advance_singlation = DanmakuMode::advance();
    let (video_width, video_height) = decoder.size();
    let gap = args.line_spacing;
    let area_top = (video_height as f64 * args.top_ratio) as u32;
    let area_bottom = (video_height as f64 * args.bottom_ratio) as u32;
    let area_height = area_bottom - area_top;

    let base_font_size = args.font_scale * 25.0;
    let line_height = base_font_size as u32 + gap;
    let rail_cnt = area_height / line_height;

    let mut scroll_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut top_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut bottom_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);
    let mut reverse_slots: GrowableVec<Option<(Danmaku, NormalComponent)>> = GrowableVec::new(None);

    static SOURCE_FONT: &[u8] = include_bytes!("../fonts/SourceHanSansSC-Regular-2.otf");
    let regular = FontVec::try_from_vec(SOURCE_FONT.to_vec())
        .expect("内置字体加载失败: SourceHanSansSC-Regular-2.otf，字体文件可能已损坏或不存在");

    for decode in decoder.decode_iter() {
        let (ts, frame) = match decode {
            Ok(res) => res,
            Err(video_rs::Error::DecodeExhausted) => break,
            r => r?,
        };
        let mut image = array3_to_rgb_image(&frame)?;
        let dur = time_to_duration(ts);

        // #region 每帧处理

        // 找到在当前时间需要显示的弹幕
        let enqueue = danmakus.extract_if(.., |d| d.time <= time_to_duration(ts));

        // 分类加入slots
        for d in enqueue {
            let scale = PxScale::from(d.font_size as f32);
            let (width, height) = text_size(scale, &regular, &d.text);
            let dead_line = match d.mode {
                DanmakuMode::Scroll | DanmakuMode::Reverse => {
                    let travel_frames = (width + video_width).div_ceil(args.speed);
                    dur + Duration::from_secs_f64(
                        travel_frames as f64 * frame_durtion.as_secs_f64(),
                    )
                }
                DanmakuMode::Top | DanmakuMode::Bottom => {
                    dur + Duration::from_secs_f64(args.fixed_duration)
                }
                _ => Duration::from_millis(0),
            };
            match d.mode {
                DanmakuMode::Scroll => scroll_slots
                    .set_first_empty((
                        d,
                        NormalComponent {
                            x: video_width as i64,
                            y: None,
                            width,
                            height,
                            dead_line,
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
                            height,
                            dead_line,
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
                            height,
                            dead_line,
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
                            height,
                            dead_line,
                        },
                    ))
                    .ignore(),
                _ => (), // 忽略Advance和Bds
            };
        }

        // 处理元素删除
        del_dead(&mut scroll_slots, dur);
        del_dead(&mut reverse_slots, dur);
        del_dead(&mut top_slots, dur);
        del_dead(&mut bottom_slots, dur);

        // 绘制文字
        // 绘制滚动弹幕
        draw_scroll_danmukus(
            &mut image,
            &mut scroll_slots,
            &regular,
            line_height,
            rail_cnt,
            args.font_scale,
            area_top as i64,
            ToLeft,
            args.opacity,
        );
        draw_scroll_danmukus(
            &mut image,
            &mut reverse_slots,
            &regular,
            line_height,
            rail_cnt,
            args.font_scale,
            area_top as i64,
            ToRight,
            args.opacity,
        );
        // 滚动弹幕移动
        scroll(
            scroll_slots
                .iter_mut()
                .filter_map(|cur| cur.as_mut().map(|(_, c)| c))
                .filter(|c| c.y.is_some()),
            ToLeft,
            args.speed,
        );
        scroll(
            reverse_slots
                .iter_mut()
                .filter_map(|cur| cur.as_mut().map(|(_, c)| c))
                .filter(|c| c.y.is_some()),
            ToRight,
            args.speed,
        );

        draw_fixed_danmukus(
            &mut image,
            &mut top_slots,
            &regular,
            line_height,
            rail_cnt,
            args.font_scale,
            area_top as i64,
            true,
            args.opacity,
        );
        draw_fixed_danmukus(
            &mut image,
            &mut bottom_slots,
            &regular,
            line_height,
            rail_cnt,
            args.font_scale,
            area_top as i64,
            false,
            args.opacity,
        );
        // 记得font-size要乘ratio

        // #endregion
        let video_frame = rgb_image_to_array3(&image);
        encoder
            .encode(&video_frame, ts)
            .with_context(|| format!("编码帧失败 (时间戳: {ts:?})"))?;
    }

    encoder.finish()?;

    Ok(())
}

fn draw_fixed_danmukus(
    image: &mut RgbImage,
    fixed_slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    font: &FontVec,
    line_height: u32,
    rail_cnt: u32,
    font_size_ratio: f32,
    area_top: i64,
    to_bottom: bool,
    opacity: f64,
) {
    let mut ensure_y_q = Vec::new();
    for (idx, opt) in fixed_slots
        .iter()
        .enumerate()
        .filter(|(_, opt)| opt.is_some())
    {
        let (dan, comp) = opt.as_ref().unwrap();
        let scale = PxScale::from(dan.font_size as f32 * font_size_ratio);
        if let Some(y) = comp.y {
            let canvas_pos = (comp.x as i32, (y + area_top) as i32);
            draw_text_mut(
                image,
                apply_opacity(dan.color, opacity),
                canvas_pos.0,
                canvas_pos.1,
                scale,
                font,
                &dan.text,
            );
            continue;
        }

        let raw_occupieds = fixed_slots.iter().filter_map(|opt| {
            opt.as_ref().map(|(_, c)| c).and_then(|comp| {
                if comp.y.is_some() {
                    Some((comp.height, comp.y.unwrap()))
                } else {
                    None
                }
            })
        });
        let mut occupieds = HashSet::with_capacity(fixed_slots.len());
        for ( height, y) in raw_occupieds {
            occupieds.insert(y);
            if height > line_height {
                occupieds.insert(y + (line_height as i64) * (height / line_height) as i64);
            }
        }

        let rail_hs = rail_hs(line_height, rail_cnt);
        let free = rail_hs.filter(|h| !occupieds.contains(h));
        let mut free: Box<dyn Iterator<Item = _>> = if to_bottom {
            Box::new(free)
        } else {
            Box::new(free.collect::<Vec<_>>().into_iter().rev())
        };

        let y = match free.next() {
            None => {
                continue;
            }
            Some(y) => y,
        };

        let canvas_pos = (comp.x as i32, (y + area_top) as i32);
        draw_text_mut(
            image,
            apply_opacity(dan.color, opacity),
            canvas_pos.0,
            canvas_pos.1,
            scale,
            font,
            &dan.text,
        );
        // 插入修改y序列
        ensure_y_q.push((idx, y));
    }

    for (idx, y) in ensure_y_q {
        fixed_slots[idx].as_mut().unwrap().1.y = Some(y); // 该unwrap由上一个循环的filter保证
    }
}

fn del_dead(slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>, dur: Duration) {
    for opt in slots.iter_mut() {
        let Some((_, comp)) = &opt else {
            continue;
        };

        if comp.dead_line <= dur {
            let _ = comp;
            *opt = None;
        }
    }
}

fn scroll<'a>(
    comps: impl Iterator<Item = &'a mut NormalComponent>,
    direction: Direction,
    speed: u32,
) {
    for comp in comps {
        match direction {
            ToLeft => comp.x -= speed as i64,
            ToRight => comp.x += speed as i64,
        };
    }
}

enum Direction {
    ToLeft,
    ToRight,
}

fn draw_scroll_danmukus(
    big_image: &mut RgbImage,
    scroll_slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    font: &FontVec,
    line_height: u32,
    rail_cnt: u32,
    font_size_ratio: f32,
    area_top: i64,
    dir: Direction,
    opacity: f64,
) {
    let mut ensure_y_q = Vec::new();
    for (idx, opt) in scroll_slots
        .iter()
        .enumerate()
        .filter(|(_, opt)| opt.is_some())
    {
        let (dan, comp) = opt.as_ref().unwrap();
        let scale = PxScale::from(dan.font_size as f32 * font_size_ratio);
        if let Some(y) = comp.y {
            let canvas_pos = (comp.x as i32, (y + area_top) as i32);
            draw_text_mut(
                big_image,
                apply_opacity(dan.color, opacity),
                canvas_pos.0,
                canvas_pos.1,
                scale,
                font,
                &dan.text,
            );
            continue;
        }

        let raw_occupieds = scroll_slots.iter().filter_map(|opt| {
            opt.as_ref().map(|(_, c)| c).and_then(|comp| {
                if comp.y.is_some() {
                    Some((comp.width, comp.height, comp.x, comp.y.unwrap()))
                } else {
                    None
                }
            })
        });
        let mut occupieds = HashSet::with_capacity(scroll_slots.len());
        for (width, height, x, y) in raw_occupieds {
            debug_assert_eq!(y % line_height as i64, 0);
            match dir {
                ToLeft => {
                    if width as i64 + x > comp.x {
                        occupieds.insert(y);
                    } else {
                        continue;
                    }
                }
                ToRight => {
                    if x < comp.x + comp.width as i64 {
                        occupieds.insert(y);
                    } else {
                        continue;
                    }
                }
            }
            if height > line_height {
                occupieds.insert(y + (line_height as i64) * (height / line_height) as i64);
            }
        }

        let rail_hs = rail_hs(line_height, rail_cnt);
        let mut free = rail_hs.filter(|h| !occupieds.contains(h));

        let y = match free.next() {
            None => {
                continue;
            }
            Some(y) => y,
        };

        let canvas_pos = (comp.x as i32, (y + area_top) as i32);
        draw_text_mut(
            big_image,
            apply_opacity(dan.color, opacity),
            canvas_pos.0,
            canvas_pos.1,
            scale,
            font,
            &dan.text,
        );
        // 插入修改y序列
        ensure_y_q.push((idx, y));
    }

    for (idx, y) in ensure_y_q {
        scroll_slots[idx].as_mut().unwrap().1.y = Some(y); // 该unwrap由上一个循环的filter保证
    }
}

fn rail_hs(line_height: u32, rail_cnt: u32) -> impl Iterator<Item = i64> {
    std::iter::successors(Some(0i64), move |prev| {
        let next = prev + line_height as i64;
        if next < (rail_cnt * line_height) as i64 {
            Some(next)
        } else {
            None
        }
    })
}

fn apply_opacity(color: Rgb<u8>, opacity: f64) -> Rgb<u8> {
    Rgb([
        (color[0] as f64 * opacity) as u8,
        (color[1] as f64 * opacity) as u8,
        (color[2] as f64 * opacity) as u8,
    ])
}

#[derive(Debug, Clone)]
struct NormalComponent {
    x: i64,
    y: Option<i64>,
    width: u32,
    height: u32,
    dead_line: Duration,
}

// /// 将 DynamicImage 转为 video-rs 所需 Array3<u8>
// pub fn dyn_img_to_video_array(img: DynamicImage) -> Array3<u8> {
//     let (width, height) = (img.width() as usize, img.height() as usize);
//     let rgb = img.into_rgb8();
//     Array3::from_shape_vec((height, width, 3), rgb.into_raw()).expect("图片像素数据长度不匹配尺寸")
// }

// pub fn frame_to_dynamic_image(array: &Array3<u8>) -> Option<DynamicImage> {
//     let (height, width, _) = array.dim();
//     let raw_pixels = array.as_slice()?;
//     ImageBuffer::from_raw(width as u32, height as u32, raw_pixels.to_vec())
//         .map(DynamicImage::ImageRgb8)
// }

pub fn same_specifications(
    decoder: &Decoder,
    path: impl AsRef<Path>,
) -> anyhow::Result<(Encoder, Time)> {
    let path = path.as_ref();

    let (width, height) = decoder.size();

    let settings = Settings::preset_h264_yuv420p(width as usize, height as usize, false);

    let encoder = Encoder::new(path, settings)?;

    let frame_rate = decoder.frame_rate();
    let frame_duration = Time::from_secs_f64(1.0 / frame_rate as f64);

    Ok((encoder, frame_duration))
}

fn time_to_duration(t: Time) -> Duration {
    let s = t.as_secs_f64();
    Duration::from_secs_f64(s)
}

/// 将 RGB 格式的 Array3<u8> 转换为 image 库的 RgbImage。
///
/// # 参数
/// - `array`: 形状应为 `(height, width, 3)` 的数组，数据连续且按行优先存储。
///
/// # 返回
/// 成功时返回 `RgbImage`，否则返回错误。
pub fn array3_to_rgb_image(array: &Array3<u8>) -> Result<RgbImage> {
    // 检查维度是否正确
    let (height, width, channels) = array.dim();
    if channels != 3 {
        return Err(anyhow!(
            "视频帧通道数异常：期望 RGB 3 通道，实际得到 {channels} 通道"
        ));
    }

    // 确保数据是连续存储的，并获取切片
    let layout = array.as_standard_layout();
    let slice = layout
        .as_slice()
        .ok_or_else(|| anyhow!("视频帧数据不连续或使用了非标准内存布局，无法转换为图像"))?;

    // 使用 from_vec 创建图像，它会进行数据拷贝（如果需要）
    // 注意：from_vec 要求数据长度 == width * height * 3
    let img = ImageBuffer::from_vec(width as u32, height as u32, slice.to_vec())
        .ok_or_else(|| {
            anyhow!(
                "图像缓冲区创建失败 ({width}x{height}x3)，像素数据长度与尺寸不匹配"
            )
        })?;

    Ok(img)
}

/// 将 image 库的 RgbImage 转换为 RGB 格式的 Array3<u8>。
///
/// # 参数
/// - `img`: 一个 RgbImage 实例。
///
/// # 返回
/// 形状为 `(height, width, 3)` 的 Array3。
pub fn rgb_image_to_array3(img: &RgbImage) -> Array3<u8> {
    let (width, height) = img.dimensions();
    let raw_pixels = img.as_raw(); // 这是一个 &[u8]，长度为 width*height*3

    // 将切片转换为 Vec，然后构建 Array3
    // 注意：ImageBuffer 的数据是连续的，且按行优先存储
    Array3::from_shape_vec((height as usize, width as usize, 3), raw_pixels.to_vec())
        .expect("图像转视频帧失败：尺寸不匹配，像素数据长度与维度 (height×width×3) 不一致")
}
