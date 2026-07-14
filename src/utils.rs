use std::ops::{Index, IndexMut};

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

    pub fn remove(&mut self, index: usize) -> T {
        self.inner.remove(index)
    }

    pub fn swap_remove(&mut self, index: usize) -> T {
        self.inner.swap_remove(index)
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
            self.inner.resize(index, self.initor.clone());
        }
        &mut self.inner[index]
    }
}

impl<T> From<Vec<T>> for GrowableVec<T> where T: Default {
    fn from(value: Vec<T>) -> Self {
        Self {
            inner: value,
            initor: T::default(),
        }
    }
}

impl<V> FromIterator<V> for GrowableVec<V> where V: Default {
    fn from_iter<T: IntoIterator<Item = V>>(iter: T) -> Self {
        Self {
            inner: iter.into_iter().collect(),
            initor: V::default(),
        }
    }
}

impl<T> IntoIterator for GrowableVec<T> {
    type Item = T;

    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}