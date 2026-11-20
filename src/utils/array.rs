//! array.rs — 数组工具。
//!
//! 重写自 `src/utils/array.ts`。三个纯函数，语义逐条对齐。

/// 在元素之间插入由 `separator(index)` 生成的分隔元素。
/// 空数组与单元素数组结果中不含分隔元素。
pub fn intersperse<A: Clone>(items: &[A], separator: impl Fn(usize) -> A) -> Vec<A> {
    let mut out = Vec::with_capacity(items.len() * 2);
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(separator(i));
        }
        out.push(item.clone());
    }
    out
}

/// 统计谓词成立的元素个数（`pred` 返回值按 JS 真值转换）。
pub fn count<T>(arr: &[T], pred: impl Fn(&T) -> bool) -> usize {
    arr.iter().filter(|x| pred(x)).count()
}

/// 去重，保留首次出现的顺序（对齐 JS `Set` 的插入序迭代）。
pub fn uniq<T: Eq + std::hash::Hash + Clone>(xs: impl IntoIterator<Item = T>) -> Vec<T> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for x in xs {
        if seen.insert(x.clone()) {
            out.push(x);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersperse_basic() {
        assert_eq!(
            intersperse(&[1, 2, 3], |i| -(i as i32)),
            vec![1, -1, 2, -2, 3]
        );
        assert_eq!(intersperse::<i32>(&[], |_| 0), Vec::<i32>::new());
    }

    #[test]
    fn count_works() {
        assert_eq!(count(&[1, 2, 3, 4], |x| x % 2 == 0), 2);
        assert_eq!(count::<i32>(&[], |_| true), 0);
    }

    #[test]
    fn uniq_keeps_first_occurrence() {
        assert_eq!(uniq([3, 1, 3, 2, 1]), vec![3, 1, 2]);
    }
}
