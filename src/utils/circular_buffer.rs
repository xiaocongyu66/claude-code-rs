//! circular_buffer.rs — 定长环形缓冲区。
//!
//! 重写自 `src/utils/CircularBuffer.ts`。容量固定，满时逐出最旧条目。

/// 定长环形缓冲区：`add` 满时驱逐最旧元素，`get_recent` 取最近 N 条。
pub struct CircularBuffer<T: Clone> {
    buffer: Vec<Option<T>>,
    head: usize,
    size: usize,
    capacity: usize,
}

impl<T: Clone> CircularBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: (0..capacity).map(|_| None).collect(),
            head: 0,
            size: 0,
            capacity,
        }
    }

    /// 追加条目；缓冲区满时驱逐最旧条目。
    /// 容量为 0 时为 no-op（对齐 JS 端 `% 0` 产生的 NaN 索引行为）。
    pub fn add(&mut self, item: T) {
        if self.capacity == 0 {
            return;
        }
        self.buffer[self.head] = Some(item);
        self.head = (self.head + 1) % self.capacity;
        if self.size < self.capacity {
            self.size += 1;
        }
    }

    /// 一次性追加多个条目。
    pub fn add_all(&mut self, items: impl IntoIterator<Item = T>) {
        for item in items {
            self.add(item);
        }
    }

    /// 取最近 `count` 条（不足时返回全部），顺序为从旧到新。
    pub fn get_recent(&self, count: usize) -> Vec<T> {
        let available = count.min(self.size);
        let start = if self.size < self.capacity {
            0
        } else {
            self.head
        };
        let mut result = Vec::with_capacity(available);
        for i in 0..available {
            let index = (start + self.size - available + i) % self.capacity;
            if let Some(item) = &self.buffer[index] {
                result.push(item.clone());
            }
        }
        result
    }

    /// 当前缓冲区内全部条目，从旧到新。
    pub fn to_vec(&self) -> Vec<T> {
        let mut result = Vec::with_capacity(self.size);
        if self.size == 0 {
            return result;
        }
        let start = if self.size < self.capacity {
            0
        } else {
            self.head
        };
        for i in 0..self.size {
            let index = (start + i) % self.capacity;
            if let Some(item) = &self.buffer[index] {
                result.push(item.clone());
            }
        }
        result
    }

    /// 清空全部条目。
    pub fn clear(&mut self) {
        for slot in self.buffer.iter_mut() {
            *slot = None;
        }
        self.head = 0;
        self.size = 0;
    }

    /// 当前条目数。
    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_when_full() {
        let mut buf = CircularBuffer::<i32>::new(3);
        for v in 1..=5 {
            buf.add(v);
        }
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.to_vec(), vec![3, 4, 5]);
    }

    #[test]
    fn get_recent_partial() {
        let mut buf = CircularBuffer::<i32>::new(10);
        for v in 1..=5 {
            buf.add(v);
        }
        assert_eq!(buf.get_recent(3), vec![3, 4, 5]);
        assert_eq!(buf.get_recent(100), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn clear_resets() {
        let mut buf = CircularBuffer::<i32>::new(3);
        buf.add_all([1, 2, 3]);
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.to_vec(), Vec::<i32>::new());
        buf.add(9);
        assert_eq!(buf.to_vec(), vec![9]);
    }
}
