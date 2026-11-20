//! object_group_by.rs — Object.groupBy 的 Rust 移植。
//!
//! 重写自 `src/utils/objectGroupBy.ts`。
//! 参考：<https://tc39.es/ecma262/multipage/fundamental-objects.html#sec-object.groupby>

use std::collections::HashMap;
use std::hash::Hash;

/// 按 `key_selector(item, index)` 分组，保留插入顺序（Rust 端使用
/// `HashMap`；调用方如需有序遍历可自行排序）。
pub fn object_group_by<T, K, F>(
    items: impl IntoIterator<Item = T>,
    key_selector: F,
) -> HashMap<K, Vec<T>>
where
    K: Eq + Hash,
    F: Fn(&T, usize) -> K,
{
    let mut result: HashMap<K, Vec<T>> = HashMap::new();
    for (index, item) in items.into_iter().enumerate() {
        let key = key_selector(&item, index);
        result.entry(key).or_default().push(item);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_by_index_parity() {
        let grouped = object_group_by(vec![1, 2, 3, 4, 5], |_, i| i % 2 == 0);
        assert_eq!(grouped.get(&true), Some(&vec![1, 3, 5]));
        assert_eq!(grouped.get(&false), Some(&vec![2, 4]));
    }

    #[test]
    fn empty_input() {
        let grouped: HashMap<i32, Vec<i32>> = object_group_by(Vec::<i32>::new(), |x, _| *x);
        assert!(grouped.is_empty());
    }
}
