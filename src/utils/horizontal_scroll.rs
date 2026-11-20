//! horizontal_scroll.rs — 水平滚动窗口计算。
//!
//! 重写自 `src/utils/horizontalScroll.ts`。在给定可用宽度内计算可见
//! 条目窗口，保证选中条目始终可见。采用边缘式滚动：仅当选中条目即将
//! 越出可见范围时才滚动，并把选中条目贴到窗口边缘（而非居中）。

/// 水平滚动窗口结果。end_index 为半开区间上界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HorizontalScrollWindow {
    pub start_index: usize,
    pub end_index: usize,
    pub show_left_arrow: bool,
    pub show_right_arrow: bool,
}

/// 计算可见窗口。
///
/// - `item_widths`：各条目宽度（若适用应包含分隔符）
/// - `available_width`：条目可用总宽度
/// - `arrow_width`：滚动指示箭头宽度（含间距）
/// - `selected_idx`：选中条目序号（必须保持可见）
/// - `first_item_has_separator`：首条目宽度是否包含应忽略的分隔符
pub fn calculate_horizontal_scroll_window(
    item_widths: &[usize],
    available_width: usize,
    arrow_width: usize,
    selected_idx: usize,
    first_item_has_separator: bool,
) -> HorizontalScrollWindow {
    let total_items = item_widths.len();

    if total_items == 0 {
        return HorizontalScrollWindow {
            start_index: 0,
            end_index: 0,
            show_left_arrow: false,
            show_right_arrow: false,
        };
    }

    // 选中序号裁剪到合法范围
    let clamped_selected = selected_idx.min(total_items - 1);

    // 全部放得下则全显
    let total_width: usize = item_widths.iter().sum();
    if total_width <= available_width {
        return HorizontalScrollWindow {
            start_index: 0,
            end_index: total_items,
            show_left_arrow: false,
            show_right_arrow: false,
        };
    }

    // 前缀宽度累加表，便于区间查询
    let mut cumulative_widths: Vec<usize> = Vec::with_capacity(total_items + 1);
    cumulative_widths.push(0);
    for w in item_widths {
        cumulative_widths.push(cumulative_widths.last().copied().unwrap_or(0) + w);
    }

    // 区间 [start, end) 的渲染宽度
    let range_width = |start: usize, end: usize| -> usize {
        let base = cumulative_widths[end] - cumulative_widths[start];
        // 起点非 0 且首条目内含分隔符时减 1（首个可见条目不渲染前置分隔符）
        if first_item_has_separator && start > 0 {
            base.saturating_sub(1)
        } else {
            base
        }
    };

    // 是否需要显示箭头 → 有效可用宽度
    let effective_width = |start: usize, end: usize| -> usize {
        let mut width = available_width;
        if start > 0 {
            width = width.saturating_sub(arrow_width); // 左箭头
        }
        if end < total_items {
            width = width.saturating_sub(arrow_width); // 右箭头
        }
        width
    };

    let mut start_index = 0;
    let mut end_index = 1;

    // 先从 0 尽可能向右扩展
    while end_index < total_items
        && range_width(start_index, end_index + 1) <= effective_width(start_index, end_index + 1)
    {
        end_index += 1;
    }

    // 选中条目已在可见区间内，完成
    if clamped_selected >= start_index && clamped_selected < end_index {
        return HorizontalScrollWindow {
            start_index,
            end_index,
            show_left_arrow: start_index > 0,
            show_right_arrow: end_index < total_items,
        };
    }

    if clamped_selected >= end_index {
        // 选中在右侧 —— 让选中条目贴右边缘
        end_index = clamped_selected + 1;
        start_index = clamped_selected;
        while start_index > 0
            && range_width(start_index - 1, end_index)
                <= effective_width(start_index - 1, end_index)
        {
            start_index -= 1;
        }
    } else {
        // 选中在左侧 —— 让选中条目贴左边缘
        start_index = clamped_selected;
        end_index = clamped_selected + 1;
        while end_index < total_items
            && range_width(start_index, end_index + 1)
                <= effective_width(start_index, end_index + 1)
        {
            end_index += 1;
        }
    }

    HorizontalScrollWindow {
        start_index,
        end_index,
        show_left_arrow: start_index > 0,
        show_right_arrow: end_index < total_items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(n: usize, w: usize) -> Vec<usize> {
        vec![w; n]
    }

    #[test]
    fn empty() {
        let win = calculate_horizontal_scroll_window(&[], 100, 3, 0, true);
        assert_eq!((win.start_index, win.end_index), (0, 0));
    }

    #[test]
    fn all_fit() {
        let win = calculate_horizontal_scroll_window(&widths(3, 10), 100, 3, 0, true);
        assert_eq!((win.start_index, win.end_index), (0, 3));
        assert!(!win.show_left_arrow && !win.show_right_arrow);
    }

    #[test]
    fn keeps_selected_visible_when_scrolling_right() {
        let win = calculate_horizontal_scroll_window(&widths(10, 10), 45, 3, 9, true);
        assert!(win.end_index == 10);
        assert!(win.start_index <= 9 && win.end_index > 9);
        assert!(win.show_left_arrow);
        assert!(!win.show_right_arrow);
    }

    #[test]
    fn no_arrows_when_at_start() {
        let win = calculate_horizontal_scroll_window(&widths(10, 10), 45, 3, 0, true);
        assert_eq!((win.start_index, win.end_index), (0, 4));
        assert!(!win.show_left_arrow);
        assert!(win.show_right_arrow);
    }
}
