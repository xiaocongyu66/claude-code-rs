//! set.rs — Set 运算工具。
//!
//! 重写自 `src/utils/set.ts`。原文件标注为热路径（"this code is hot"），
//! 用 `HashSet` 提供同名语义。

use std::collections::HashSet;
use std::hash::Hash;

/// 差集：存在于 `a` 但不存在于 `b` 的元素。
pub fn difference<A>(a: &HashSet<A>, b: &HashSet<A>) -> HashSet<A>
where
    A: Eq + Hash + Clone,
{
    a.difference(b).cloned().collect()
}

/// 是否存在交集。任一集合为空时直接返回 false。
pub fn intersects<A>(a: &HashSet<A>, b: &HashSet<A>) -> bool
where
    A: Eq + Hash,
{
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a.intersection(b).next().is_some()
}

/// `a` 是否为 `b` 的子集（`a` 中每个元素都在 `b` 中）。
pub fn every<A>(a: &HashSet<A>, b: &HashSet<A>) -> bool
where
    A: Eq + Hash,
{
    a.iter().all(|item| b.contains(item))
}

/// 并集。
pub fn union<A>(a: &HashSet<A>, b: &HashSet<A>) -> HashSet<A>
where
    A: Eq + Hash + Clone,
{
    a.union(b).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn set_operations() {
        let a = set(&["a", "b", "c"]);
        let b = set(&["b", "c", "d"]);
        assert_eq!(difference(&a, &b), set(&["a"]));
        assert!(intersects(&a, &b));
        assert!(!intersects(&a, &set(&[])));
        assert!(every(&set(&["b"]), &b));
        assert!(!every(&a, &b));
        assert_eq!(union(&a, &b), set(&["a", "b", "c", "d"]));
    }
}
