//! Fixed-capacity history, newest last; the oldest point is dropped when full.
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct Ring<T> {
    points: VecDeque<T>,
    capacity: usize,
}

impl<T: Clone> Ring<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            points: VecDeque::with_capacity(capacity.min(1024)),
            capacity: capacity.max(1),
        }
    }
    pub fn push(&mut self, value: T) {
        if self.points.len() == self.capacity {
            self.points.pop_front();
        }
        self.points.push_back(value);
    }
    pub fn to_vec(&self) -> Vec<T> {
        self.points.iter().cloned().collect()
    }
    pub fn len(&self) -> usize {
        self.points.len()
    }
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
    pub fn last(&self) -> Option<&T> {
        self.points.back()
    }
}
