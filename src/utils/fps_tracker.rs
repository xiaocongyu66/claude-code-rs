//! fps_tracker.rs — 渲染循环的 FPS 追踪器。
//!
//! 重写自 `src/utils/fpsTracker.ts`。内存模型与 TS 一致：帧耗时进入
//! 4096 槽的定长环（Float64）+ 增量 1ms 直方图。读取 O(buckets)，
//! p99 来源为最近 4096 帧而非全部历史。

use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct FpsMetrics {
    pub average_fps: f64,
    pub low_1pct_fps: f64,
}

const RING_CAPACITY: usize = 4096;
const HISTOGRAM_BUCKET_WIDTH_MS: f64 = 1.0;
const HISTOGRAM_RANGE_MS: f64 = 100.0;
// [0, HISTOGRAM_RANGE_MS) 每 1ms 一桶，外加一个 >= RANGE 的溢出桶
const HISTOGRAM_BUCKETS: usize = (HISTOGRAM_RANGE_MS as usize) + 1;
const OVERFLOW_BUCKET: usize = HISTOGRAM_BUCKETS - 1;

fn histogram_bucket(duration_ms: f64) -> usize {
    if duration_ms.is_nan() || duration_ms >= HISTOGRAM_RANGE_MS {
        return OVERFLOW_BUCKET;
    }
    if duration_ms < 0.0 {
        return 0;
    }
    (duration_ms / HISTOGRAM_BUCKET_WIDTH_MS).floor() as usize
}

pub struct FpsTracker {
    ring: Vec<f64>,
    ring_head: usize,
    ring_size: usize,
    // 全部历史帧数（标量）：averageFps 保持原"整段会话 fps"语义，
    // 只有 p99 来源是窗口化的
    total_frames: u64,
    bucket_counts: Vec<i64>,
    bucket_sums: Vec<f64>,
    first_render: Option<Instant>,
    last_render: Option<Instant>,
}

impl FpsTracker {
    pub fn new() -> Self {
        Self {
            ring: vec![0.0; RING_CAPACITY],
            ring_head: 0,
            ring_size: 0,
            total_frames: 0,
            bucket_counts: vec![0; HISTOGRAM_BUCKETS],
            bucket_sums: vec![0.0; HISTOGRAM_BUCKETS],
            first_render: None,
            last_render: None,
        }
    }

    /// 记录一帧的耗时（ms）。非有限值钳到溢出桶边界，避免 NaN 污染
    /// 桶内累加和。
    pub fn record(&mut self, mut duration_ms: f64) {
        if !duration_ms.is_finite() {
            duration_ms = HISTOGRAM_RANGE_MS;
        }
        let now = Instant::now();
        if self.first_render.is_none() {
            self.first_render = Some(now);
        }
        self.last_render = Some(now);

        if self.ring_size == RING_CAPACITY {
            // 覆盖最旧槽并回退其直方图贡献，保持计数与窗口严格一致
            let evicted = self.ring[self.ring_head];
            let evicted_bucket = histogram_bucket(evicted);
            self.bucket_counts[evicted_bucket] -= 1;
            self.bucket_sums[evicted_bucket] -= evicted;
        } else {
            self.ring_size += 1;
        }
        self.ring[self.ring_head] = duration_ms;
        self.ring_head = (self.ring_head + 1) % RING_CAPACITY;
        self.total_frames += 1;

        let bucket = histogram_bucket(duration_ms);
        self.bucket_counts[bucket] += 1;
        self.bucket_sums[bucket] += duration_ms;
    }

    /// 当前指标；没有任何有效帧或总时长 <= 0 时返回 None。
    pub fn get_metrics(&self) -> Option<FpsMetrics> {
        let (first, last) = (self.first_render?, self.last_render?);
        if self.total_frames == 0 {
            return None;
        }
        let total_time_ms = last.duration_since(first).as_secs_f64() * 1000.0;
        if total_time_ms <= 0.0 {
            return None;
        }

        let average_fps = self.total_frames as f64 / (total_time_ms / 1000.0);

        // low1PctFps 为直方图桶近似的 p99（桶内均值作估计）
        let p99_frame_time_ms = self.top_percentile_frame_time(0.01);
        let low_1pct_fps = if p99_frame_time_ms > 0.0 {
            1000.0 / p99_frame_time_ms
        } else {
            0.0
        };

        Some(FpsMetrics {
            average_fps: (average_fps * 100.0).round() / 100.0,
            low_1pct_fps: (low_1pct_fps * 100.0).round() / 100.0,
        })
    }

    /// 通过增量直方图取第 k 大的保留帧耗时，
    /// k = ceil(n * percentile) —— 对齐旧的 sort 语义
    ///（percentile 0.01 → 最慢的 ceil(0.01·n) 帧）。O(buckets)。
    fn top_percentile_frame_time(&self, percentile: f64) -> f64 {
        let k = ((self.ring_size as f64) * percentile).ceil().max(1.0) as u64;
        let mut cumulative: i64 = 0;
        for bucket in (0..=OVERFLOW_BUCKET).rev() {
            let count = self.bucket_counts[bucket];
            if count == 0 {
                continue;
            }
            cumulative += count;
            if cumulative >= k as i64 {
                return self.bucket_sums[bucket] / count as f64;
            }
        }
        0.0
    }

    /// 泄漏回归测试用的内部诊断（非公开 FpsMetrics API 的一部分）：
    /// 保留窗口大小 vs 硬容量。
    pub fn get_internal_stats(&self) -> (usize, usize, u64) {
        (self.ring_size, RING_CAPACITY, self.total_frames)
    }
}

impl Default for FpsTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frames_no_metrics() {
        let tracker = FpsTracker::new();
        assert!(tracker.get_metrics().is_none());
    }

    #[test]
    fn records_and_reports() {
        let mut tracker = FpsTracker::new();
        for _ in 0..10 {
            tracker.record(16.6);
        }
        let metrics = tracker.get_metrics().expect("frames recorded");
        assert!(metrics.average_fps > 0.0);
        assert!(metrics.low_1pct_fps > 0.0);
        let (retained, capacity, total) = tracker.get_internal_stats();
        assert_eq!((retained, capacity, total), (10, 4096, 10));
    }

    #[test]
    fn ring_eviction_keeps_counts_bounded() {
        let mut tracker = FpsTracker::new();
        for _ in 0..(RING_CAPACITY + 100) {
            tracker.record(5.0);
        }
        let (retained, _, total) = tracker.get_internal_stats();
        assert_eq!(retained, RING_CAPACITY);
        assert_eq!(total, (RING_CAPACITY + 100) as u64);
        // 所有保留帧都是 5ms → 直方图非溢出桶
        let total_count: i64 = tracker.bucket_counts.iter().sum();
        assert_eq!(total_count, RING_CAPACITY as i64);
    }

    #[test]
    fn non_finite_durations_do_not_poison() {
        let mut tracker = FpsTracker::new();
        tracker.record(f64::NAN);
        tracker.record(f64::INFINITY);
        tracker.record(16.0);
        let metrics = tracker.get_metrics().expect("finite frame exists");
        assert!(metrics.average_fps.is_finite());
    }
}
