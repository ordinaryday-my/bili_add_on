use std::{
    borrow::Cow,
    ops::{Index, IndexMut},
};

use image::{Rgb, RgbImage, RgbaImage};

pub type IntoIter<T> = std::vec::IntoIter<T>;

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

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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

/// 返回精灵图像中非透明像素的垂直包围盒 `(top, bottom)`（行号，含两端）。
///
/// 全透明时返回 `None`。
pub fn sprite_ink_bounds(sprite: &RgbaImage) -> Option<(u32, u32)> {
    let (w, h) = sprite.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let raw = sprite.as_raw();
    let row_has_ink = |yy: u32| -> bool {
        let start = yy as usize * w as usize * 4;
        let end = start + w as usize * 4;
        raw[start..end].chunks_exact(4).any(|px| px[3] > 0)
    };
    let mut top = None;
    let mut bottom = None;
    for yy in 0..h {
        if row_has_ink(yy) {
            top = Some(yy);
            break;
        }
    }
    for yy in (0..h).rev() {
        if row_has_ink(yy) {
            bottom = Some(yy);
            break;
        }
    }
    match (top, bottom) {
        (Some(t), Some(b)) => Some((t, b)),
        _ => None,
    }
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
    fn test_sprite_ink_bounds_basic() {
        let mut sprite = RgbaImage::new(4, 8);
        sprite.put_pixel(0, 2, image::Rgba([255, 0, 0, 255]));
        sprite.put_pixel(3, 5, image::Rgba([0, 255, 0, 128]));
        let (top, bottom) = sprite_ink_bounds(&sprite).unwrap();
        assert_eq!((top, bottom), (2, 5));
    }

    #[test]
    fn test_sprite_ink_bounds_empty() {
        let sprite = RgbaImage::new(4, 8);
        assert_eq!(sprite_ink_bounds(&sprite), None);
        assert_eq!(sprite_ink_bounds(&RgbaImage::new(0, 0)), None);
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
