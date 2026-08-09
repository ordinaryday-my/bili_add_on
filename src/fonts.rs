use std::{num::NonZeroUsize, path::PathBuf, sync::Arc};

use anyhow::{bail, Context, Result};
use cosmic_text::{
    fontdb, Attrs, Buffer, Color, Family, FontSystem, Metrics, PlatformFallback, Shaping,
    SwashCache, Weight,
};
use image::{Rgb, RgbaImage};
use lru::LruCache;

use crate::interaction::Args;

/// 项目内置字体（加载顺序即同权重下的回退优先级）。
struct ProjectFont {
    name: &'static str,
    bytes: &'static [u8],
}

const PROJECT_FONTS: &[ProjectFont] = &[
    ProjectFont {
        name: "SourceHanSansSC-Regular-2.otf",
        bytes: include_bytes!("../fonts/SourceHanSansSC-Regular-2.otf"),
    },
    ProjectFont {
        name: "NotoSansSymbols2-Regular.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols2-Regular.ttf"),
    },
    ProjectFont {
        name: "NotoSansSymbols-Black.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols/NotoSansSymbols-Black.ttf"),
    },
    ProjectFont {
        name: "NotoSansSymbols-Bold.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols/NotoSansSymbols-Bold.ttf"),
    },
    ProjectFont {
        name: "NotoSansSymbols-ExtraBold.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols/NotoSansSymbols-ExtraBold.ttf"),
    },
    ProjectFont {
        name: "NotoSansSymbols-ExtraLight.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols/NotoSansSymbols-ExtraLight.ttf"),
    },
    ProjectFont {
        name: "NotoSansSymbols-Light.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols/NotoSansSymbols-Light.ttf"),
    },
    ProjectFont {
        name: "NotoSansSymbols-Medium.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols/NotoSansSymbols-Medium.ttf"),
    },
    ProjectFont {
        name: "NotoSansSymbols-Regular.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols/NotoSansSymbols-Regular.ttf"),
    },
    ProjectFont {
        name: "NotoSansSymbols-SemiBold.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols/NotoSansSymbols-SemiBold.ttf"),
    },
    ProjectFont {
        name: "NotoSansSymbols-Thin.ttf",
        bytes: include_bytes!("../fonts/NotoSansSymbols/NotoSansSymbols-Thin.ttf"),
    },
];

/// 精灵图 LRU 缓存容量上限（条目数）。
const SPRITE_CACHE_CAP: usize = 512;
/// 每渲染多少条文本收紧一次 cosmic-text 的 shape 运行缓存。
const SHAPE_CACHE_TRIM_INTERVAL: u32 = 512;
/// shape 缓存保留最近多少次的命中。
const SHAPE_CACHE_KEEP_AGES: u64 = 2048;

/// 精灵缓存键：文本 + 渲染字号（位模式）+ 颜色。
#[derive(Clone, Hash, PartialEq, Eq)]
struct SpriteKey {
    text: String,
    font_size: u32,
    color: u32,
}

impl SpriteKey {
    fn new(text: &str, font_size: f32, color: Rgb<u8>) -> Self {
        Self {
            text: text.to_string(),
            font_size: font_size.to_bits(),
            color: u32::from_be_bytes([color.0[0], color.0[1], color.0[2], 0]),
        }
    }
}

/// 字体栈：按「用户字体 > 系统字体 > 项目字体」的优先级组织字体库，
/// 由 cosmic-text 在字形级自动回退。
pub struct FontStack {
    font_system: FontSystem,
    swash_cache: SwashCache,
    sprite_cache: LruCache<SpriteKey, RgbaImage>,
    render_count: u32,
    primary_family: Option<String>,
    primary_weight: Weight,
}

impl FontStack {
    /// 依据参数加载全部字体源并构造字体栈。
    ///
    /// 加载顺序决定同权重候选的决胜序（fontdb ID 递增）：
    /// 1. 用户通过 `--font` 传入的字体（按传入顺序）
    /// 2. 系统字体（仅 `--system-fonts` 开启时）
    /// 3. 项目内置字体（思源黑体 → Noto Sans Symbols 2 → Noto Sans Symbols 家族）
    ///
    /// 未提供用户字体时不显式指定主字体族，回退链会先经过系统字体再落到项目字体。
    pub fn load(args: &Args) -> Result<Self> {
        let mut db = fontdb::Database::new();

        let mut primary_family: Option<String> = None;
        let mut primary_weight = Weight::NORMAL;

        for path in &args.font {
            load_user_font(&mut db, path, &mut primary_family, &mut primary_weight)?;
        }

        if args.system_fonts {
            db.load_system_fonts();
        }

        for font in PROJECT_FONTS {
            let ids = db.load_font_source(fontdb::Source::Binary(Arc::new(font.bytes.to_vec())));
            if ids.is_empty() {
                bail!(
                    "内置字体加载失败: {}，字体文件可能已损坏",
                    font.name
                );
            }
        }

        let locale = sys_locale::get_locale().unwrap_or_else(|| String::from("en-US"));
        let font_system =
            FontSystem::new_with_locale_and_db_and_fallback(locale, db, PlatformFallback);

        let mut stack = Self {
            font_system,
            swash_cache: SwashCache::new(),
            sprite_cache: LruCache::new(NonZeroUsize::new(SPRITE_CACHE_CAP).unwrap()),
            render_count: 0,
            primary_family,
            primary_weight,
        };
        // 预热字体匹配缓存，消除首次排版时对全库字面的解析尖峰
        let mut attrs = Attrs::new().weight(stack.primary_weight);
        if let Some(family) = &stack.primary_family {
            attrs = attrs.family(Family::Name(family));
        }
        stack.font_system.get_font_matches(&attrs);

        Ok(stack)
    }

    fn attrs(&self) -> Attrs<'_> {
        let mut attrs = Attrs::new().weight(self.primary_weight);
        if let Some(family) = &self.primary_family {
            attrs = attrs.family(Family::Name(family));
        }
        attrs
    }

    /// 排版文本（一次 shape），返回可复用的 Buffer。
    fn shape(&mut self, text: &str, font_size: f32) -> Buffer {
        let mut buffer = Buffer::new(
            &mut self.font_system,
            Metrics::new(font_size, font_size * 1.2),
        );
        buffer.set_size(None, None);
        buffer.set_text(text, &self.attrs(), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
    }

    /// 在已排版 Buffer 上干跑一次绘制，记录实际像素矩形的包围盒，返回 `(宽, 高, min_x, min_y)`。
    ///
    /// 与真实绘制共用同一套布局与栅格化，因此结果与实际渲染完全一致；
    /// 尺寸各边留 1px 余量，避免亚像素取整造成的边缘裁切。
    fn measure_buffer(&mut self, buffer: &mut Buffer, color: Rgb<u8>) -> (u32, u32, i32, i32) {
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;

        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            Color::rgb(color.0[0], color.0[1], color.0[2]),
            |x, y, w, h, _| {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x + w as i32);
                max_y = max_y.max(y + h as i32);
            },
        );

        if min_x == i32::MAX {
            return (0, 0, 0, 0);
        }
        (
            (max_x - min_x + 2) as u32,
            (max_y - min_y + 2) as u32,
            min_x,
            min_y,
        )
    }

    /// 渲染文本精灵（透明底、抗锯齿 alpha），经 LRU 缓存复用重复弹幕的绘制结果。
    ///
    /// 无墨迹（如纯空格）时返回空精灵且不写入缓存。
    pub fn render_sprite(
        &mut self,
        text: &str,
        font_size: f32,
        color: Rgb<u8>,
    ) -> RgbaImage {
        if text.is_empty() {
            return RgbaImage::new(0, 0);
        }
        let key = SpriteKey::new(text, font_size, color);
        if let Some(sprite) = self.sprite_cache.get(&key) {
            return sprite.clone();
        }

        // 周期性收紧 cosmic-text 的 shape 运行缓存（库本身无淘汰逻辑）
        self.render_count += 1;
        if self.render_count.is_multiple_of(SHAPE_CACHE_TRIM_INTERVAL) {
            self.font_system
                .shape_run_cache
                .trim(SHAPE_CACHE_KEEP_AGES);
        }

        let mut buffer = self.shape(text, font_size);
        let (w, h, min_x, min_y) = self.measure_buffer(&mut buffer, color);
        if w == 0 || h == 0 {
            return RgbaImage::new(0, 0);
        }
        let mut sprite = RgbaImage::new(w, h);
        self.draw_buffer(&mut buffer, &mut sprite, color, min_x, min_y);
        self.sprite_cache.put(key, sprite.clone());
        sprite
    }

    /// 将已排版的 Buffer 绘制进精灵图（raw 缓冲区直写）。
    fn draw_buffer(
        &mut self,
        buffer: &mut Buffer,
        sprite: &mut RgbaImage,
        color: Rgb<u8>,
        min_x: i32,
        min_y: i32,
    ) {
        let offset_x = 1 - min_x;
        let offset_y = 1 - min_y;
        let (iw, ih) = (sprite.width() as i32, sprite.height() as i32);
        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            Color::rgb(color.0[0], color.0[1], color.0[2]),
            |x, y, w, h, pixel| {
                let px0 = x + offset_x;
                let py0 = y + offset_y;
                let (pr, pg, pb, pa) = (
                    ((pixel.0 >> 16) & 0xFF) as u8,
                    ((pixel.0 >> 8) & 0xFF) as u8,
                    (pixel.0 & 0xFF) as u8,
                    ((pixel.0 >> 24) & 0xFF) as u8,
                );
                let raw = sprite.as_mut();
                for dy in 0..h as i32 {
                    let yy = py0 + dy;
                    if !(0..ih).contains(&yy) {
                        continue;
                    }
                    let row = yy as usize * iw as usize;
                    for dx in 0..w as i32 {
                        let xx = px0 + dx;
                        if (0..iw).contains(&xx) {
                            let i = (row + xx as usize) * 4;
                            raw[i] = pr;
                            raw[i + 1] = pg;
                            raw[i + 2] = pb;
                            raw[i + 3] = pa;
                        }
                    }
                }
            },
        );
    }

    /// 测量文本渲染宽度（字形 hitbox，无需绘制），供滚动时长估算。
    pub fn text_width(&mut self, text: &str, font_size: f32) -> u32 {
        if text.is_empty() {
            return 0;
        }
        let buffer = self.shape(text, font_size);
        let mut max_x = 0.0f32;
        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                max_x = max_x.max(glyph.x + glyph.w);
            }
        }
        max_x.ceil() as u32
    }

    /// 布局后各字形实际使用的字体 ID（用于测试断言回退结果）。
    #[cfg(test)]
    fn glyph_font_ids(&mut self, text: &str, font_size: f32) -> Vec<fontdb::ID> {
        let buffer = self.shape(text, font_size);
        buffer
            .layout_runs()
            .flat_map(|run| run.glyphs.iter().map(|g| g.font_id))
            .collect()
    }

    /// 某字族在库中所有字面的 ID（按加载顺序）。
    #[cfg(test)]
    fn face_ids_by_family(&self, family: &str) -> Vec<fontdb::ID> {
        self.font_system
            .db()
            .faces()
            .filter(|face| face.families.iter().any(|(name, _)| name == family))
            .map(|face| face.id)
            .collect()
    }
}

fn load_user_font(
    db: &mut fontdb::Database,
    path: &PathBuf,
    primary_family: &mut Option<String>,
    primary_weight: &mut Weight,
) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("读取字体文件失败: {}", path.display()))?;
    let ids = db.load_font_source(fontdb::Source::Binary(Arc::new(bytes)));
    if ids.is_empty() {
        bail!("无法解析字体文件: {}", path.display());
    }
    let face = db
        .face(ids[0])
        .with_context(|| format!("字体文件解析失败: {}", path.display()))?;
    if primary_family.is_none() {
        *primary_family = Some(
            face.families
                .first()
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| face.post_script_name.clone()),
        );
        *primary_weight = face.weight;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args_with(fonts: &[&str], system_fonts: bool) -> Args {
        let mut argv = vec!["bili_add_on", "--input", "test.mp4", "--bvid", "BV1test"];
        for font in fonts {
            argv.push("--font");
            argv.push(font);
        }
        if system_fonts {
            argv.push("--system-fonts");
        }
        Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn test_sprite_size_scales_with_font_size() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        let s25 = stack.render_sprite("中文弹幕测试", 25.0, Rgb([255, 255, 255]));
        let s50 = stack.render_sprite("中文弹幕测试", 50.0, Rgb([255, 255, 255]));
        let (w25, h25) = s25.dimensions();
        let (w50, h50) = s50.dimensions();
        assert!(w25 > 0 && h25 > 0);
        assert!(w50 > w25);
        assert!(h50 > h25);
    }

    #[test]
    fn test_empty_text_zero_size() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        assert_eq!(stack.render_sprite("", 25.0, Rgb([255, 255, 255])).dimensions(), (0, 0));
        assert_eq!(stack.text_width("", 25.0), 0);
    }

    #[test]
    fn test_space_only_not_cached() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        // 纯空格无墨迹：返回空精灵且不写入缓存
        let sprite = stack.render_sprite("   ", 25.0, Rgb([255, 255, 255]));
        assert_eq!(sprite.dimensions(), (0, 0));
        assert_eq!(stack.sprite_cache.len(), 0);
    }

    #[test]
    fn test_sprite_cache_hit_identical() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        let a = stack.render_sprite("弹幕缓存测试", 25.0, Rgb([255, 255, 255]));
        let b = stack.render_sprite("弹幕缓存测试", 25.0, Rgb([255, 255, 255]));
        assert_eq!(a.dimensions(), b.dimensions());
        assert_eq!(a.as_raw(), b.as_raw());
        // 不同颜色视为不同缓存项
        let c = stack.render_sprite("弹幕缓存测试", 25.0, Rgb([255, 0, 0]));
        assert_ne!(a.as_raw(), c.as_raw());
        // 不同字号视为不同缓存项
        let d = stack.render_sprite("弹幕缓存测试", 26.0, Rgb([255, 255, 255]));
        assert_ne!(a.as_raw(), d.as_raw());
    }

    #[test]
    fn test_sprite_cache_cap() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        // 超过容量上限后缓存长度不再增长
        let total = SPRITE_CACHE_CAP + 128;
        for i in 0..total {
            let text = format!("缓存压力测试文本{i}");
            stack.render_sprite(&text, 25.0, Rgb([255, 255, 255]));
        }
        assert!(stack.sprite_cache.len() <= SPRITE_CACHE_CAP);
    }

    #[test]
    fn test_fallback_dingbat_cross() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        // ✟ (U+271F) 思源黑体与 Noto Sans Symbols 2 均缺，须回退到 Noto Sans Symbols 家族
        let ids = stack.glyph_font_ids("✟", 25.0);
        assert!(!ids.is_empty(), "✟ 应产生字形");
        let symbols_ids = stack.face_ids_by_family("Noto Sans Symbols");
        assert!(
            ids.iter().any(|id| symbols_ids.contains(id)),
            "✟ 应回退到 Noto Sans Symbols，实际字体: {ids:?}"
        );
    }

    #[test]
    fn test_fallback_dagger_primary() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        // † (U+2020) 思源黑体 format 12 覆盖，应由思源黑体渲染
        let ids = stack.glyph_font_ids("†", 25.0);
        let han_ids = stack.face_ids_by_family("Source Han Sans SC");
        assert!(
            ids.iter().any(|id| han_ids.contains(id)),
            "† 应由思源黑体渲染，实际字体: {ids:?}"
        );
    }

    #[test]
    fn test_fallback_ballot_check() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        // ☑ (U+2611) 思源黑体缺、Noto Sans Symbols 2 覆盖，应回退到 Noto Sans Symbols 2
        let ids = stack.glyph_font_ids("☑", 25.0);
        let sym2_ids = stack.face_ids_by_family("Noto Sans Symbols 2");
        assert!(
            ids.iter().any(|id| sym2_ids.contains(id)),
            "☑ 应回退到 Noto Sans Symbols 2，实际字体: {ids:?}"
        );
    }

    #[test]
    fn test_draw_produces_ink() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        for text in ["中文弹幕", "✟", "☑"] {
            let sprite = stack.render_sprite(text, 25.0, Rgb([255, 255, 255]));
            let (w, h) = sprite.dimensions();
            assert!(w > 0 && h > 0, "文本 {text:?} 尺寸应为正");
            let opaque = sprite.as_raw().chunks_exact(4).any(|px| px[3] > 0);
            assert!(opaque, "文本 {text:?} 应渲染出墨迹");
        }
    }
}
