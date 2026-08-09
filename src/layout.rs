use std::time::Duration;

use bit_set::BitSet;
use image::{RgbImage, RgbaImage};

use crate::{
    danmaku::{Danmaku, DanmakuMode},
    fonts::FontStack,
    interaction::Args,
    utils::{blit_cached_text, GrowableVec},
};

/// 计算最长弹幕的完全显示截止时间（供 --longest 扩展视频时长）。
pub(crate) fn compute_max_danmaku_deadline(
    danmakus: &[Danmaku],
    fonts: &mut FontStack,
    args: &Args,
    video_width: u32,
    frame_duration_secs: f64,
) -> f64 {
    let mut max_deadline = 0.0f64;
    for dan in danmakus {
        let deadline_secs = match dan.mode {
            DanmakuMode::Scroll | DanmakuMode::Reverse => {
                let text_width =
                    fonts.text_width(&dan.text, (dan.font_size as f32) * args.font_scale);
                let travel_frames = (text_width + video_width).div_ceil(args.speed);
                dan.time.as_secs_f64() + travel_frames as f64 * frame_duration_secs
            }
            DanmakuMode::Top | DanmakuMode::Bottom => dan.time.as_secs_f64() + args.fixed_duration,
            _ => dan.time.as_secs_f64(),
        };
        if deadline_secs > max_deadline {
            max_deadline = deadline_secs;
        }
    }
    max_deadline
}

pub(crate) struct DrawParams<'a> {
    pub(crate) image: &'a mut RgbImage,
    pub(crate) base_pitch: u32,
    pub(crate) rail_cnt: u32,
    pub(crate) area_top: i64,
    pub(crate) opacity: f64,
    pub(crate) min_space: i64,
    pub(crate) now: Duration,
}

pub(crate) const RAIL_OFFSET: usize = 1000;

/// 在轨道占用位图中查找第一条空闲的虚拟轨道（连续 `n_rails` 条基础轨道）。
///
/// `from_bottom == true` 时从底部向上扫描（底部固定弹幕），否则自上而下。
/// 返回基础轨道序号，找不到返回 `None`。
pub(crate) fn find_free_track(
    occupieds: &BitSet,
    n_rails: u32,
    rail_cnt: u32,
    from_bottom: bool,
) -> Option<u32> {
    let last_start = rail_cnt.saturating_sub(n_rails);
    let mut i = if from_bottom { last_start } else { 0 };
    loop {
        let free = (0..n_rails).all(|k| !occupieds.contains(RAIL_OFFSET + (i + k) as usize));
        if free {
            return Some(i);
        }
        if from_bottom {
            if i == 0 {
                break;
            }
            i -= 1;
        } else {
            if i >= last_start {
                break;
            }
            i += 1;
        }
    }
    None
}

/// 计算某字号的虚拟轨道数：该字号墨迹高度需要几个基础轨道（B站虚拟轨道机制）。
///
/// `ink_ref` 为标准字号（25）在 `font_scale` 下的墨迹高度；其余字号按比例缩放。
pub(crate) fn compute_n_rails(ink_ref: u32, base_pitch: u32, font_size: usize) -> u32 {
    let ink_h_font = (ink_ref as u64 * font_size as u64).div_ceil(25) as u32;
    ink_h_font.div_ceil(base_pitch).max(1)
}

pub(crate) fn mark_track_occupied(occupieds: &mut BitSet, track: u32, n_rails: u32) {
    for k in 0..n_rails {
        occupieds.insert(RAIL_OFFSET + (track + k) as usize);
    }
}

pub(crate) fn draw_fixed_danmakus(
    params: &mut DrawParams,
    fixed_slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    from_bottom: bool,
) {
    let cap = fixed_slots.len() + RAIL_OFFSET;
    let mut occupieds = BitSet::with_capacity(cap);
    let mut ensure_y_q = Vec::new();
    let mut pending_y: Vec<(i64, u32)> = Vec::new();
    let mut drop_q = Vec::new();
    for (idx, opt) in fixed_slots
        .iter()
        .enumerate()
        .filter(|(_, opt)| opt.is_some())
    {
        let (_dan, comp) = opt.as_ref().unwrap();
        if let Some(y) = comp.y {
            blit_cached_text(
                params.image,
                comp.cached_text.as_ref().unwrap(),
                comp.x as i32,
                (y + params.area_top) as i32,
                params.opacity,
            );
            continue;
        }

        let raw_occupieds = fixed_slots.iter().filter_map(|opt| {
            opt.as_ref().map(|(_, c)| c).and_then(|comp| {
                if comp.y.is_some() {
                    Some((comp.y.unwrap(), comp.n_rails))
                } else {
                    None
                }
            })
        });

        for (y, n_rails) in raw_occupieds {
            debug_assert_eq!(y % params.base_pitch as i64, 0);
            let track = (y / params.base_pitch as i64) as u32;
            mark_track_occupied(&mut occupieds, track, n_rails);
        }

        for &(y, n_rails) in &pending_y {
            let track = (y / params.base_pitch as i64) as u32;
            mark_track_occupied(&mut occupieds, track, n_rails);
        }

        // B站式丢弃：延迟放置的弹幕剩余寿命不足，直接不显示
        if comp.dead_line.saturating_sub(params.now) < comp.travel {
            drop_q.push(idx);
            occupieds.reset();
            continue;
        }

        let Some(track) = find_free_track(&occupieds, comp.n_rails, params.rail_cnt, from_bottom)
        else {
            occupieds.reset();
            continue;
        };
        let y = track as i64 * params.base_pitch as i64;

        blit_cached_text(
            params.image,
            comp.cached_text.as_ref().unwrap(),
            comp.x as i32,
            (y + params.area_top) as i32,
            params.opacity,
        );
        pending_y.push((y, comp.n_rails));
        ensure_y_q.push((idx, y));
        occupieds.reset();
    }

    for (idx, y) in ensure_y_q {
        fixed_slots[idx].as_mut().unwrap().1.y = Some(y);
    }
    for idx in drop_q {
        fixed_slots[idx] = None;
    }
}

pub(crate) fn del_dead(
    slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    dur: Duration,
) {
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

pub(crate) fn scroll<'a>(
    comps: impl Iterator<Item = &'a mut NormalComponent>,
    direction: Direction,
    speed: u32,
) {
    for comp in comps {
        match direction {
            Direction::ToLeft => comp.x -= speed as i64,
            Direction::ToRight => comp.x += speed as i64,
        };
    }
}

pub(crate) enum Direction {
    ToLeft,
    ToRight,
}

pub(crate) fn draw_scroll_danmakus(
    params: &mut DrawParams,
    scroll_slots: &mut GrowableVec<Option<(Danmaku, NormalComponent)>>,
    dir: Direction,
) {
    let cap = scroll_slots.len() + RAIL_OFFSET;
    let mut occupieds = BitSet::with_capacity(cap);
    let mut ensure_y_q = Vec::new();
    let mut pending_y: Vec<(i64, u32)> = Vec::new();
    let mut drop_q = Vec::new();
    for (idx, opt) in scroll_slots
        .iter()
        .enumerate()
        .filter(|(_, opt)| opt.is_some())
    {
        let (_dan, comp) = opt.as_ref().unwrap();
        if let Some(y) = comp.y {
            blit_cached_text(
                params.image,
                comp.cached_text.as_ref().unwrap(),
                comp.x as i32,
                (y + params.area_top) as i32,
                params.opacity,
            );
            continue;
        }

        let raw_occupieds = scroll_slots.iter().filter_map(|opt| {
            opt.as_ref().map(|(_, c)| c).and_then(|comp| {
                if comp.y.is_some() {
                    Some((comp.width, comp.x, comp.y.unwrap(), comp.n_rails))
                } else {
                    None
                }
            })
        });

        for (width, x, y, n_rails) in raw_occupieds {
            debug_assert_eq!(y % params.base_pitch as i64, 0);
            let overlap = match dir {
                Direction::ToLeft => width as i64 + x + params.min_space > comp.x,
                Direction::ToRight => x < comp.x + comp.width as i64 + params.min_space,
            };
            if !overlap {
                continue;
            }
            let track = (y / params.base_pitch as i64) as u32;
            mark_track_occupied(&mut occupieds, track, n_rails);
        }

        for &(y, n_rails) in &pending_y {
            let track = (y / params.base_pitch as i64) as u32;
            mark_track_occupied(&mut occupieds, track, n_rails);
        }

        // B站式丢弃：延迟放置的弹幕剩余寿命不足以完整滚出，直接不显示
        if comp.dead_line.saturating_sub(params.now) < comp.travel {
            drop_q.push(idx);
            occupieds.reset();
            continue;
        }

        let Some(track) = find_free_track(&occupieds, comp.n_rails, params.rail_cnt, false) else {
            occupieds.reset();
            continue;
        };
        let y = track as i64 * params.base_pitch as i64;

        blit_cached_text(
            params.image,
            comp.cached_text.as_ref().unwrap(),
            comp.x as i32,
            (y + params.area_top) as i32,
            params.opacity,
        );
        pending_y.push((y, comp.n_rails));
        ensure_y_q.push((idx, y));
        occupieds.reset();
    }

    for (idx, y) in ensure_y_q {
        scroll_slots[idx].as_mut().unwrap().1.y = Some(y);
    }
    for idx in drop_q {
        scroll_slots[idx] = None;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NormalComponent {
    pub(crate) x: i64,
    pub(crate) y: Option<i64>,
    pub(crate) width: u32,
    pub(crate) n_rails: u32,
    pub(crate) travel: Duration,
    pub(crate) dead_line: Duration,
    pub(crate) cached_text: Option<RgbaImage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::Direction::ToLeft;
    use image::Rgb;

    #[test]
    fn test_find_free_track_basic() {
        let mut occ = BitSet::new();
        mark_track_occupied(&mut occ, 1, 2);
        assert_eq!(find_free_track(&occ, 1, 10, false), Some(0));
        assert_eq!(find_free_track(&occ, 2, 10, false), Some(3));
        assert_eq!(find_free_track(&occ, 3, 10, false), Some(3));
    }

    #[test]
    fn test_find_free_track_from_bottom() {
        let mut occ = BitSet::new();
        mark_track_occupied(&mut occ, 7, 1);
        assert_eq!(find_free_track(&occ, 1, 10, true), Some(9));
        assert_eq!(find_free_track(&occ, 3, 10, true), Some(4));
    }

    #[test]
    fn test_find_free_track_none() {
        let mut occ = BitSet::new();
        for t in 0..10 {
            mark_track_occupied(&mut occ, t, 1);
        }
        assert_eq!(find_free_track(&occ, 1, 10, false), None);
        assert_eq!(find_free_track(&occ, 1, 10, true), None);
    }

    #[test]
    fn test_compute_n_rails_bilibili_table() {
        // ink_ref=32（scale 2 实测）、base_pitch=36：18→1, 25→1, 36→2, 45→2, 64→3
        let n = |s: usize| compute_n_rails(32, 36, s);
        assert_eq!(n(18), 1);
        assert_eq!(n(25), 1);
        assert_eq!(n(36), 2);
        assert_eq!(n(45), 2);
        assert_eq!(n(64), 3);
    }

    #[test]
    fn test_compute_n_rails_min_one() {
        assert_eq!(compute_n_rails(32, 36, 1), 1);
    }

    #[test]
    fn test_scroll_to_left() {
        let mut comp = NormalComponent {
            x: 100,
            y: Some(0),
            width: 10,
            n_rails: 1,
            travel: Duration::ZERO,
            dead_line: Duration::ZERO,
            cached_text: None,
        };
        scroll(std::iter::once(&mut comp), Direction::ToLeft, 3);
        assert_eq!(comp.x, 97);
    }

    #[test]
    fn test_scroll_to_right() {
        let mut comp = NormalComponent {
            x: 100,
            y: Some(0),
            width: 10,
            n_rails: 1,
            travel: Duration::ZERO,
            dead_line: Duration::ZERO,
            cached_text: None,
        };
        scroll(std::iter::once(&mut comp), Direction::ToRight, 5);
        assert_eq!(comp.x, 105);
    }

    #[test]
    fn test_scroll_only_active() {
        let mut comp1 = NormalComponent {
            x: 100,
            y: Some(10),
            width: 10,
            n_rails: 1,
            travel: Duration::ZERO,
            dead_line: Duration::ZERO,
            cached_text: None,
        };
        let mut comp2 = NormalComponent {
            x: 200,
            y: None,
            width: 10,
            n_rails: 1,
            travel: Duration::ZERO,
            dead_line: Duration::ZERO,
            cached_text: None,
        };

        let comps: Vec<&mut NormalComponent> = vec![&mut comp1, &mut comp2];
        let active = comps.into_iter().filter(|c| c.y.is_some());
        scroll(active, Direction::ToLeft, 2);

        assert_eq!(comp1.x, 98);
        assert_eq!(comp2.x, 200); // y is None, so it shouldn't be iterated
    }

    #[test]
    fn test_del_dead_removes_expired() {
        let dan = Danmaku {
            time: Duration::from_secs(0),
            mode: DanmakuMode::Scroll,
            font_size: 25,
            color: Rgb([255, 0, 0]),
            text: "test".into(),
        };
        let comp = NormalComponent {
            x: 0,
            y: None,
            width: 10,
            n_rails: 1,
            travel: Duration::ZERO,
            dead_line: Duration::from_secs(10),
            cached_text: None,
        };

        let mut slots = GrowableVec::new(None);
        slots[0] = Some((dan.clone(), comp.clone()));

        del_dead(&mut slots, Duration::from_secs(11));
        assert!(slots[0].is_none());
    }

    #[test]
    fn test_del_dead_keeps_active() {
        let dan = Danmaku {
            time: Duration::from_secs(0),
            mode: DanmakuMode::Scroll,
            font_size: 25,
            color: Rgb([255, 0, 0]),
            text: "test".into(),
        };
        let comp = NormalComponent {
            x: 0,
            y: None,
            width: 10,
            n_rails: 1,
            travel: Duration::ZERO,
            dead_line: Duration::from_secs(10),
            cached_text: None,
        };

        let mut slots = GrowableVec::new(None);
        slots[0] = Some((dan.clone(), comp));

        del_dead(&mut slots, Duration::from_secs(9));
        assert!(slots[0].is_some());
    }

    #[test]
    fn test_del_dead_exact_deadline() {
        let dan = Danmaku {
            time: Duration::from_secs(0),
            mode: DanmakuMode::Scroll,
            font_size: 25,
            color: Rgb([255, 0, 0]),
            text: "test".into(),
        };
        let comp = NormalComponent {
            x: 0,
            y: None,
            width: 10,
            n_rails: 1,
            travel: Duration::ZERO,
            dead_line: Duration::from_secs(10),
            cached_text: None,
        };

        let mut slots = GrowableVec::new(None);
        slots[0] = Some((dan, comp));

        del_dead(&mut slots, Duration::from_secs(10));
        assert!(slots[0].is_none());
    }

    fn danmaku_comp(
        dead_line: Duration,
        travel: Duration,
        mode: DanmakuMode,
    ) -> (Danmaku, NormalComponent) {
        let dan = Danmaku {
            time: Duration::from_secs(0),
            mode,
            font_size: 25,
            color: Rgb([255, 255, 255]),
            text: "测试".into(),
        };
        let comp = NormalComponent {
            x: 100,
            y: None,
            width: 10,
            n_rails: 1,
            travel,
            dead_line,
            cached_text: Some(RgbaImage::new(10, 10)),
        };
        (dan, comp)
    }

    fn test_draw_params(image: &mut RgbImage, now: Duration) -> DrawParams<'_> {
        DrawParams {
            image,
            base_pitch: 10,
            rail_cnt: 4,
            area_top: 0,
            opacity: 1.0,
            min_space: 0,
            now,
        }
    }

    #[test]
    fn test_scroll_placed_when_remaining_equals_travel() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        slots[0] = Some(danmaku_comp(
            Duration::from_secs(10),
            Duration::from_secs(5),
            DanmakuMode::Scroll,
        ));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_scroll_danmakus(&mut params, &mut slots, ToLeft);
        let y = slots[0].as_ref().unwrap().1.y;
        assert_eq!(y, Some(0), "剩余寿命足够时应放置");
    }

    #[test]
    fn test_scroll_dropped_when_remaining_less_than_travel() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        slots[0] = Some(danmaku_comp(
            Duration::from_secs(8),
            Duration::from_secs(5),
            DanmakuMode::Scroll,
        ));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_scroll_danmakus(&mut params, &mut slots, ToLeft);
        assert!(slots[0].is_none(), "剩余寿命(3s) < travel(5s) 时应丢弃");
    }

    #[test]
    fn test_scroll_kept_when_placed_immediately() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        // 第一帧即放置：now == 入队时刻
        let (dan, comp) = danmaku_comp(
            Duration::from_secs(10),
            Duration::from_secs(5),
            DanmakuMode::Scroll,
        );
        slots[0] = Some((dan, comp));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_scroll_danmakus(&mut params, &mut slots, ToLeft);
        assert!(slots[0].is_some());
        assert_eq!(slots[0].as_ref().unwrap().1.y, Some(0));
    }

    #[test]
    fn test_fixed_dropped_when_remaining_less_than_travel() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        slots[0] = Some(danmaku_comp(
            Duration::from_secs(8),
            Duration::from_secs(5),
            DanmakuMode::Top,
        ));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_fixed_danmakus(&mut params, &mut slots, false);
        assert!(slots[0].is_none(), "固定弹幕延迟放置同样丢弃");
    }

    #[test]
    fn test_fixed_kept_when_remaining_equals_travel() {
        let mut image = RgbImage::new(20, 40);
        let mut slots = GrowableVec::new(None);
        slots[0] = Some(danmaku_comp(
            Duration::from_secs(10),
            Duration::from_secs(5),
            DanmakuMode::Top,
        ));
        let mut params = test_draw_params(&mut image, Duration::from_secs(5));
        draw_fixed_danmakus(&mut params, &mut slots, false);
        assert_eq!(slots[0].as_ref().unwrap().1.y, Some(0));
    }
}
