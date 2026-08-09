use std::{path::PathBuf, sync::Arc};

use anyhow::{bail, Context, Result};
use cosmic_text::{
    fontdb, Attrs, Buffer, Color, Family, FontSystem, Metrics, PlatformFallback, Shaping,
    SwashCache, Weight,
};
use image::RgbaImage;

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

/// 字体栈：按「用户字体 > 系统字体 > 项目字体」的优先级组织字体库，
/// 由 cosmic-text 在字形级自动回退。
pub(crate) struct FontStack {
    font_system: FontSystem,
    swash_cache: SwashCache,
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
    pub(crate) fn load(args: &Args) -> Result<Self> {
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

        Ok(Self {
            font_system,
            swash_cache: SwashCache::new(),
            primary_family,
            primary_weight,
        })
    }

    fn attrs(&self) -> Attrs<'_> {
        let mut attrs = Attrs::new().weight(self.primary_weight);
        if let Some(family) = &self.primary_family {
            attrs = attrs.family(Family::Name(family));
        }
        attrs
    }

    fn new_buffer(&mut self, text: &str, font_size: f32) -> Buffer {
        let mut buffer = Buffer::new(
            &mut self.font_system,
            Metrics::new(font_size, font_size * 1.2),
        );
        buffer.set_size(None, None);
        buffer.set_text(text, &self.attrs(), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
    }

    /// 干跑一次绘制，记录实际像素矩形的包围盒，返回 `(宽, 高, min_x, min_y)`。
    ///
    /// 与真实绘制共用同一套布局与栅格化，因此结果与实际渲染完全一致；
    /// 尺寸各边留 1px 余量，避免亚像素取整造成的边缘裁切。
    fn measure(&mut self, text: &str, font_size: f32, color: [u8; 3]) -> (u32, u32, i32, i32) {
        if text.is_empty() {
            return (0, 0, 0, 0);
        }
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;

        let mut buffer = self.new_buffer(text, font_size);
        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            Color::rgb(color[0], color[1], color[2]),
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

    /// 测量文本渲染所需的最小精灵尺寸（与 [`Self::draw_text`] 一致）。
    pub(crate) fn text_size(&mut self, text: &str, font_size: f32) -> (u32, u32) {
        let (w, h, ..) = self.measure(text, font_size, [255, 255, 255]);
        (w, h)
    }

    /// 将文本渲染进精灵图（透明底、抗锯齿 alpha），用于后续按弹幕不透明度混合。
    ///
    /// 精灵尺寸应来自 [`Self::text_size`]；本方法内部会再次干跑测量并平移对齐。
    pub(crate) fn draw_text(
        &mut self,
        sprite: &mut RgbaImage,
        text: &str,
        font_size: f32,
        color: image::Rgb<u8>,
    ) {
        let (w, h, min_x, min_y) = self.measure(text, font_size, color.0);
        if w == 0 || h == 0 {
            return;
        }
        let offset_x = 1 - min_x;
        let offset_y = 1 - min_y;

        let mut buffer = self.new_buffer(text, font_size);
        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            Color::rgb(color.0[0], color.0[1], color.0[2]),
            |x, y, w, h, pixel| {
                let (iw, ih) = (sprite.width() as i32, sprite.height() as i32);
                let px0 = x + offset_x;
                let py0 = y + offset_y;
                let (pr, pg, pb, pa) = (
                    ((pixel.0 >> 16) & 0xFF) as u8,
                    ((pixel.0 >> 8) & 0xFF) as u8,
                    (pixel.0 & 0xFF) as u8,
                    ((pixel.0 >> 24) & 0xFF) as u8,
                );
                for dy in 0..h as i32 {
                    let yy = py0 + dy;
                    if !(0..ih).contains(&yy) {
                        continue;
                    }
                    for dx in 0..w as i32 {
                        let xx = px0 + dx;
                        if (0..iw).contains(&xx) {
                            sprite.put_pixel(xx as u32, yy as u32, image::Rgba([pr, pg, pb, pa]));
                        }
                    }
                }
            },
        );
    }

    /// 布局后各字形实际使用的字体 ID（用于测试断言回退结果）。
    #[cfg(test)]
    fn glyph_font_ids(&mut self, text: &str, font_size: f32) -> Vec<fontdb::ID> {
        let buffer = self.new_buffer(text, font_size);
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
    fn test_text_size_scales_with_font_size() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        let (w25, h25) = stack.text_size("中文弹幕测试", 25.0);
        let (w50, h50) = stack.text_size("中文弹幕测试", 50.0);
        assert!(w25 > 0 && h25 > 0);
        assert!(w50 > w25);
        assert!(h50 > h25);
    }

    #[test]
    fn test_empty_text_zero_size() {
        let args = args_with(&[], false);
        let mut stack = FontStack::load(&args).unwrap();
        assert_eq!(stack.text_size("", 25.0), (0, 0));
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
            let (w, h) = stack.text_size(text, 25.0);
            assert!(w > 0 && h > 0, "文本 {text:?} 尺寸应为正");
            let mut sprite = RgbaImage::new(w, h);
            stack.draw_text(&mut sprite, text, 25.0, image::Rgb([255, 255, 255]));
            let opaque = sprite
                .as_raw()
                .chunks_exact(4)
                .any(|px| px[3] > 0);
            assert!(opaque, "文本 {text:?} 应渲染出墨迹");
        }
    }
}
