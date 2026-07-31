use std::{
    borrow::Cow,
    ops::{Index, IndexMut},
};

use image::{Rgb, RgbImage, RgbaImage};

pub type IntoIter<T> = std::vec::IntoIter<T>;

/// 将 RGB24 的 `[y0, y1)` 行带转换为 4:2:0 YUV（BT.601 limited range）。
///
/// `dst` 为连续输出缓冲，大小为 `width * height * 3 / 2`：
/// - `semi_planar == true`（NV12）: Y 平面 + 交错 U/V 平面
/// - `semi_planar == false`（YUV420P）: Y 平面 + U 平面 + V 平面
///
/// `y0` 必须为偶数；`y1` 可为奇数（最后一行）。仅处理 2x2 块的色度，
/// 奇数边缘行的 Y 按像素计算，色度取其上一行配对。
///
/// # Safety
/// 调用方必须保证 `src`/`dst` 指向的缓冲大小与 `width * height` 匹配，
/// 且各并行线程写入 `dst` 的 `[y0, y1)` 行带区域互不重叠。
pub unsafe fn rgb24_to_yuv420_band_raw(
    src: *const u8,
    width: usize,
    height: usize,
    y0: usize,
    y1: usize,
    dst: *mut u8,
    semi_planar: bool,
) {
    unsafe { rgb24_to_yuv420_band_scalar(src, width, height, y0, y1, dst, semi_planar) };
}

unsafe fn rgb24_to_yuv420_band_scalar(
    src: *const u8,
    width: usize,
    height: usize,
    y0: usize,
    y1: usize,
    dst: *mut u8,
    semi_planar: bool,
) {
    let y_size = width * height;
    let w2 = width / 2;
    debug_assert!(y0 % 2 == 0 && y1 <= height && y0 < y1);

    unsafe {
        for y in y0..y1 {
            let row = src.add(y * width * 3);
            let drow = dst.add(y * width);
            for x in 0..width {
                let p = row.add(x * 3);
                let r = *p as i32;
                let g = *p.add(1) as i32;
                let b = *p.add(2) as i32;
                let yv = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                *drow.add(x) = yv.clamp(16, 235) as u8;
            }
        }

        for by in (y0 / 2)..y1.div_ceil(2) {
            let r0 = by * 2;
            let r1 = (by * 2 + 1).min(height - 1);
            for bx in 0..w2 {
                let mut su = 0i32;
                let mut sv = 0i32;
                for &r in &[r0, r1] {
                    for dx in 0..2 {
                        let p = src.add((r * width + (bx * 2 + dx)) * 3);
                        let rr = *p as i32;
                        let g = *p.add(1) as i32;
                        let b = *p.add(2) as i32;
                        su += -38 * rr - 74 * g + 112 * b + 128;
                        sv += 112 * rr - 94 * g - 18 * b + 128;
                    }
                }
                let idx = by * w2 + bx;
                let u = ((su >> 10) + 128).clamp(16, 240) as u8;
                let v = ((sv >> 10) + 128).clamp(16, 240) as u8;
                if semi_planar {
                    let base = y_size + idx * 2;
                    *dst.add(base) = u;
                    *dst.add(base + 1) = v;
                } else {
                    *dst.add(y_size + idx) = u;
                    *dst.add(y_size + w2 * height.div_ceil(2) + idx) = v;
                }
            }
        }
    }
}

/// 将 RGB24 转换为 4:2:0 YUV（BT.601 limited range）。
#[allow(dead_code)]
pub fn rgb24_to_yuv420(src: &[u8], width: usize, height: usize, dst: &mut [u8], semi_planar: bool) {
    let y_size = width * height;
    let w2 = width / 2;
    assert!(src.len() >= y_size * 3);
    assert!(dst.len() >= y_size + w2 * (height.div_ceil(2)) * 2);
    unsafe {
        rgb24_to_yuv420_band_raw(
            src.as_ptr(),
            width,
            height,
            0,
            height,
            dst.as_mut_ptr(),
            semi_planar,
        );
    }
}

pub struct GrowableVec<T> {
    inner: Vec<T>,
    initor: T,
}

impl<T> GrowableVec<T> {
    pub fn new(default_value: T) -> Self {
        Self {
            inner: Vec::new(),
            initor: default_value,
        }
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &T> + DoubleEndedIterator<Item = &T> {
        self.inner.iter()
    }

    pub fn iter_mut(
        &mut self,
    ) -> impl ExactSizeIterator<Item = &mut T> + DoubleEndedIterator<Item = &mut T> {
        self.inner.iter_mut()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<U> GrowableVec<Option<U>>
where
    U: Clone,
{
    pub fn first_empty(&mut self) -> &mut Option<U> {
        let len = self.len();
        let idx = self.iter_mut().position(|cur| cur.is_none()).unwrap_or(len);
        &mut self[idx]
    }

    pub fn set_first_empty(&mut self, value: U) -> &mut Option<U> {
        let empty = self.first_empty();
        *empty = Some(value);
        empty
    }
}

impl<T> Index<usize> for GrowableVec<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        &self.inner[index]
    }
}

impl<T> IndexMut<usize> for GrowableVec<T>
where
    T: Clone,
{
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        if index >= self.inner.len() {
            self.inner.resize(index + 1, self.initor.clone());
        }
        &mut self.inner[index]
    }
}

impl<T> From<Vec<T>> for GrowableVec<T>
where
    T: Default,
{
    fn from(value: Vec<T>) -> Self {
        Self {
            inner: value,
            initor: T::default(),
        }
    }
}

impl<V> FromIterator<V> for GrowableVec<V>
where
    V: Default,
{
    fn from_iter<T: IntoIterator<Item = V>>(iter: T) -> Self {
        Self {
            inner: iter.into_iter().collect(),
            initor: V::default(),
        }
    }
}

impl<T> IntoIterator for GrowableVec<T> {
    type Item = T;

    type IntoIter = IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}

pub fn decode_bytes(bytes: impl AsRef<[u8]>, content_type: &str) -> anyhow::Result<String> {
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

pub fn cow_u8_to_str(data: Cow<'_, [u8]>) -> anyhow::Result<Cow<'_, str>> {
    match data {
        Cow::Borrowed(bytes) => Ok(Cow::Borrowed(str::from_utf8(bytes)?)),
        Cow::Owned(vec) => Ok(Cow::Owned(String::from_utf8(vec)?)),
    }
}

pub fn decode_rgb(decimal_color: u32) -> Rgb<u8> {
    let r = ((decimal_color >> 16) & 0xFF) as u8;
    let g = ((decimal_color >> 8) & 0xFF) as u8;
    let b = (decimal_color & 0xFF) as u8;
    Rgb([r, g, b])
}

pub trait Ignore {
    fn ignore(self)
    where
        Self: Sized;
}

impl<T> Ignore for T {
    fn ignore(self)
    where
        Self: Sized,
    {
        drop(self);
    }
}

pub fn rail_hs(line_height: u32, rail_cnt: u32) -> impl Iterator<Item = i64> {
    std::iter::successors(Some(0i64), move |prev| {
        let next = prev + line_height as i64;
        if next < (rail_cnt * line_height) as i64 {
            Some(next)
        } else {
            None
        }
    })
}

pub fn blit_cached_text(frame: &mut RgbImage, sprite: &RgbaImage, x: i32, y: i32, opacity: f64) {
    let o256 = (opacity * 256.0).round() as u32;
    if o256 == 0 {
        return;
    }
    let (sw, sh) = sprite.dimensions();
    let (fw, fh) = frame.dimensions();
    let clip_x1 = x.max(0) as u32;
    let clip_y1 = y.max(0) as u32;
    let clip_x2 = ((x + sw as i32).max(0) as u32).min(fw);
    let clip_y2 = ((y + sh as i32).max(0) as u32).min(fh);
    if clip_x1 >= clip_x2 || clip_y1 >= clip_y2 {
        return;
    }
    let frame_stride = fw as usize * 3;
    let sprite_stride = sw as usize * 4;
    let frame_buf = frame.as_mut();
    let sprite_buf = sprite.as_raw();
    for fy in clip_y1..clip_y2 {
        let sy = (fy as i32 - y) as u32;
        let frame_row_start = fy as usize * frame_stride;
        let sprite_row_start = sy as usize * sprite_stride;
        for fx in clip_x1..clip_x2 {
            let sx = (fx as i32 - x) as u32;
            let si = sprite_row_start + sx as usize * 4;
            let sa = sprite_buf[si + 3] as u32;
            if sa == 0 {
                continue;
            }
            let ea = sa * o256 / 256;
            let inv_ea = 256 - ea;
            let fi = frame_row_start + fx as usize * 3;
            frame_buf[fi] =
                ((sprite_buf[si] as u32 * ea + frame_buf[fi] as u32 * inv_ea) / 256) as u8;
            frame_buf[fi + 1] =
                ((sprite_buf[si + 1] as u32 * ea + frame_buf[fi + 1] as u32 * inv_ea) / 256) as u8;
            frame_buf[fi + 2] =
                ((sprite_buf[si + 2] as u32 * ea + frame_buf[fi + 2] as u32 * inv_ea) / 256) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn test_decode_rgb_white() {
        assert_eq!(decode_rgb(0xFFFFFF), Rgb([255, 255, 255]));
    }

    #[test]
    fn test_decode_rgb_black() {
        assert_eq!(decode_rgb(0x000000), Rgb([0, 0, 0]));
    }

    #[test]
    fn test_decode_rgb_red() {
        assert_eq!(decode_rgb(0xFF0000), Rgb([255, 0, 0]));
    }

    #[test]
    fn test_decode_rgb_bilibili_white() {
        assert_eq!(decode_rgb(16777215), Rgb([255, 255, 255]));
    }

    #[test]
    fn test_decode_rgb_green() {
        assert_eq!(decode_rgb(0x00FF00), Rgb([0, 255, 0]));
    }

    #[test]
    fn test_decode_rgb_blue() {
        assert_eq!(decode_rgb(0x0000FF), Rgb([0, 0, 255]));
    }

    #[test]
    fn test_cow_u8_to_str_valid_utf8() {
        let input = Cow::Borrowed("hello".as_bytes());
        let result = cow_u8_to_str(input).unwrap();
        assert_eq!(result, Cow::Borrowed("hello"));
    }

    #[test]
    fn test_cow_u8_to_str_invalid_utf8() {
        let input = Cow::Borrowed(&[0xFF, 0xFE, 0xFD][..]);
        assert!(cow_u8_to_str(input).is_err());
    }

    #[test]
    fn test_cow_u8_to_str_owned() {
        let input = Cow::Owned(b"world".to_vec());
        let result = cow_u8_to_str(input).unwrap();
        assert_eq!(result, Cow::<'_, str>::Owned("world".to_string()));
    }

    #[test]
    fn test_decode_bytes_utf8() {
        let result = decode_bytes("你好世界".as_bytes(), "text/plain").unwrap();
        assert_eq!(result, "你好世界");
    }

    #[test]
    fn test_decode_bytes_gbk_declared() {
        let input: Vec<u8> = vec![0xCE, 0xD2, 0xCA, 0xC7]; // "我是" in GBK
        let result = decode_bytes(&input, "text/xml; charset=gbk").unwrap();
        assert_eq!(result, "我是");
    }

    #[test]
    fn test_growable_vec_new_and_len() {
        let gv = GrowableVec::<i32>::new(0);
        assert_eq!(gv.len(), 0);
    }

    #[test]
    fn test_growable_vec_index_mut_auto_grows() {
        let mut gv = GrowableVec::new(-1);
        gv[3] = 42;
        assert_eq!(gv.len(), 4);
        assert_eq!(gv[0], -1);
        assert_eq!(gv[1], -1);
        assert_eq!(gv[2], -1);
        assert_eq!(gv[3], 42);
    }

    #[test]
    fn test_growable_vec_iter() {
        let mut gv = GrowableVec::new(0);
        gv[0] = 10;
        gv[1] = 20;
        let collected: Vec<_> = gv.iter().copied().collect();
        assert_eq!(collected, vec![10, 20]);
    }

    #[test]
    fn test_growable_vec_first_empty_all_none() {
        let mut gv = GrowableVec::<Option<i32>>::new(None);
        gv[2] = None;
        let slot = gv.first_empty();
        assert!(slot.is_none());
    }

    #[test]
    fn test_growable_vec_set_first_empty() {
        let mut gv = GrowableVec::<Option<i32>>::new(None);
        gv[0] = Some(10);
        let slot = gv.set_first_empty(99);
        assert_eq!(*slot, Some(99));
        assert_eq!(gv[0], Some(10));
        assert_eq!(gv[1], Some(99));
    }

    #[test]
    fn test_growable_vec_set_first_empty_reuses_slot() {
        let mut gv = GrowableVec::<Option<i32>>::new(None);
        gv[0] = Some(1);
        gv[1] = Some(2);
        gv[1] = None; // clear slot 1
        let slot = gv.set_first_empty(3);
        assert_eq!(*slot, Some(3));
        assert_eq!(gv[0], Some(1));
        assert_eq!(gv[1], Some(3));
    }

    #[test]
    fn test_growable_vec_from_iter() {
        let gv: GrowableVec<i32> = vec![1, 2, 3].into_iter().collect();
        assert_eq!(gv.len(), 3);
        assert_eq!(gv[0], 1);
        assert_eq!(gv[2], 3);
    }

    #[test]
    fn test_growable_vec_into_iter() {
        let mut gv = GrowableVec::new(0);
        gv[0] = 5;
        gv[1] = 6;
        let values: Vec<_> = gv.into_iter().collect();
        assert_eq!(values, vec![5, 6]);
    }

    #[test]
    fn test_growable_vec_from_vec() {
        let v = vec![10, 20, 30];
        let gv = GrowableVec::from(v);
        assert_eq!(gv.len(), 3);
        assert_eq!(gv[0], 10);
    }

    #[test]
    fn test_rgb24_to_yuv420_black() {
        let w = 4;
        let h = 4;
        let src = vec![0u8; w * h * 3];
        let mut dst = vec![0u8; w * h * 3 / 2];
        rgb24_to_yuv420(&src, w, h, &mut dst, true);
        assert_eq!(dst[0], 16);
        assert_eq!(dst[w * h], 128);
        assert_eq!(dst[w * h + 1], 128);
    }

    #[test]
    fn test_rgb24_to_yuv420_white() {
        let w = 4;
        let h = 4;
        let src = vec![255u8; w * h * 3];
        let mut dst = vec![0u8; w * h * 3 / 2];
        rgb24_to_yuv420(&src, w, h, &mut dst, true);
        assert_eq!(dst[0], 235);
        assert_eq!(dst[w * h], 128);
        assert_eq!(dst[w * h + 1], 128);
    }

    #[test]
    fn test_rgb24_to_yuv420_red() {
        let w = 4;
        let h = 4;
        let mut src = vec![0u8; w * h * 3];
        for px in src.chunks_mut(3) {
            px[0] = 255;
        }
        let mut dst = vec![0u8; w * h * 3 / 2];
        rgb24_to_yuv420(&src, w, h, &mut dst, true);
        assert_eq!(dst[0], 82);
        assert_eq!(dst[w * h], 90);
        assert_eq!(dst[w * h + 1], 240);
    }

    #[test]
    fn test_rgb24_to_yuv420_semi_planar_matches_planar() {
        let w = 6;
        let h = 4;
        let src: Vec<u8> = (0..(w * h * 3)).map(|i| (i * 37 % 251) as u8).collect();
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        let mut yuv420p = vec![0u8; w * h * 3 / 2];
        rgb24_to_yuv420(&src, w, h, &mut nv12, true);
        rgb24_to_yuv420(&src, w, h, &mut yuv420p, false);

        let y_size = w * h;
        let uv_count = w / 2 * h / 2;
        assert_eq!(&nv12[..y_size], &yuv420p[..y_size]);
        for i in 0..uv_count {
            assert_eq!(nv12[y_size + i * 2], yuv420p[y_size + i]);
            assert_eq!(nv12[y_size + i * 2 + 1], yuv420p[y_size + uv_count + i]);
        }
    }

    #[test]
    fn test_rail_hs_basic() {
        let positions: Vec<i64> = rail_hs(30, 3).collect();
        assert_eq!(positions, vec![0, 30, 60]);
    }

    #[test]
    fn test_rail_hs_zero_rails() {
        let positions: Vec<i64> = rail_hs(10, 0).collect();
        assert_eq!(positions, vec![0]);
    }

    #[test]
    fn test_blit_cached_text_full_opacity() {
        let mut frame = RgbImage::new(2, 2);
        frame.fill(0);
        let mut sprite = RgbaImage::new(2, 2);
        sprite.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        sprite.put_pixel(1, 1, image::Rgba([0, 255, 0, 255]));

        blit_cached_text(&mut frame, &sprite, 0, 0, 1.0);

        let p = frame.get_pixel(0, 0);
        assert!(p.0[0] >= 254);
    }

    #[test]
    fn test_blit_cached_text_zero_opacity() {
        let mut frame = RgbImage::new(2, 2);
        frame.put_pixel(0, 0, image::Rgb([100, 100, 100]));
        let mut sprite = RgbaImage::new(2, 2);
        sprite.put_pixel(0, 0, image::Rgba([255, 255, 255, 255]));

        blit_cached_text(&mut frame, &sprite, 0, 0, 0.0);
        assert_eq!(frame.get_pixel(0, 0), &image::Rgb([100, 100, 100]));
    }
}
