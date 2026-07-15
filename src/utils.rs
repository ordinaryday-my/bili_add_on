use std::{
    borrow::Cow,
    ops::{Index, IndexMut},
};

use image::Rgb;

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
