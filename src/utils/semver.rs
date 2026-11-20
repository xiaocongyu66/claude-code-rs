//! semver.rs — semver 比较工具。
//!
//! 重写自 `src/utils/semver.ts`。TS 优先用 Bun.semver、Node 侧回退
//! npm semver（loose: true）；Rust 端实现等价的比较与 satisfies 语义。
//! Cargo.toml 无 semver 依赖，核心解析 + 比较 + 常用 range
//!（精确、x/x-range、^、~、*
//!、||、空白列表）手写实现；range 之外的语法回退为精确比较失败
//!（false），等价 npm semver 的 null 行为接近。
//!
//! 与 npm semver loose mode 的已知差异：
//! - 完全不支持 prerelease tag 的 range 处理细节（prerelease 仅用于
//!   版本比较；`satisfies("1.0.0-beta", "^1.0.0")` 返回 false，与 npm
//!   严格模式一致，npm loose 下也是 false）
//! - 不支持 build metadata 影响比较（build metadata 总是被忽略，符合 semver 规范）

/// 解析出的语义化版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// 预发布标识（如 "beta.1"）；None 表示无预发布。
    pub pre: Option<String>,
}

/// 解析 semver 字符串；失败返回 None。
pub fn parse(version: &str) -> Option<Version> {
    let version = version.trim();
    if version.is_empty() {
        return None;
    }
    // 忽略 build metadata（+ 后）
    let version = version.split('+').next().unwrap_or(version);
    let (core, pre) = match version.split_once('-') {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (version, None),
    };

    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let parse_num = |s: &str| -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let major = parse_num(parts[0])?;
    let minor = if parts.len() > 1 {
        parse_num(parts[1])?
    } else {
        0
    };
    let patch = if parts.len() > 2 {
        parse_num(parts[2])?
    } else {
        0
    };

    // 预发布段必须非空
    if matches!(&pre, Some(p) if p.is_empty()) {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
        pre,
    })
}

/// 按 semver 优先级比较（-1 / 0 / 1）。
pub fn compare(a: &str, b: &str) -> Option<i32> {
    let va = parse(a)?;
    let vb = parse(b)?;
    Some(compare_versions(&va, &vb))
}

fn compare_versions(va: &Version, vb: &Version) -> i32 {
    let core = (va.major, va.minor, va.patch).cmp(&(vb.major, vb.minor, vb.patch));
    if core != std::cmp::Ordering::Equal {
        return match core {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Greater => 1,
            _ => 0,
        };
    }
    // 主版本号相等：有预发布 < 无预发布
    match (&va.pre, &vb.pre) {
        (None, None) => 0,
        (Some(_), None) => -1,
        (None, Some(_)) => 1,
        (Some(pa), Some(pb)) => compare_prerelease(pa, pb),
    }
}

/// 按 semver 规范比较预发布标识：点分标识符逐段比较，
/// 数字段按数值、字母段按 ASCII；较短的是前驱。
fn compare_prerelease(a: &str, b: &str) -> i32 {
    let a_parts: Vec<&str> = a.split('.').collect();
    let b_parts: Vec<&str> = b.split('.').collect();
    for (pa, pb) in a_parts.iter().zip(b_parts.iter()) {
        let a_num = pa.bytes().all(|b| b.is_ascii_digit());
        let b_num = pb.bytes().all(|b| b.is_ascii_digit());
        let ord = match (a_num, b_num) {
            (true, true) => {
                let an: u64 = pa.parse().unwrap_or(0);
                let bn: u64 = pb.parse().unwrap_or(0);
                an.cmp(&bn)
            }
            (true, false) => std::cmp::Ordering::Less, // 数字段 < 字母段
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => pa.cmp(pb),
        };
        if ord != std::cmp::Ordering::Equal {
            return match ord {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Greater => 1,
                _ => 0,
            };
        }
    }
    match a_parts.len().cmp(&b_parts.len()) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        _ => 0,
    }
}

pub fn gt(a: &str, b: &str) -> bool {
    matches!(compare(a, b), Some(1))
}

pub fn gte(a: &str, b: &str) -> bool {
    matches!(compare(a, b), Some(0 | 1))
}

pub fn lt(a: &str, b: &str) -> bool {
    matches!(compare(a, b), Some(-1))
}

pub fn lte(a: &str, b: &str) -> bool {
    matches!(compare(a, b), Some(-1 | 0))
}

pub fn order(a: &str, b: &str) -> i32 {
    compare(a, b).unwrap_or(0)
}

/// 版本是否满足 range 表达式。
///
/// 支持：精确版本、`*`/`x` 通配、`x.y.z` 主.次.补 部分通配、
/// `~`（补丁级）、`^`（兼容级）、`||` 或列、空白与逗号与列。
/// 任意子表达式中命中即满足。
pub fn satisfies(version: &str, range: &str) -> bool {
    let Some(v) = parse(version) else {
        return false;
    };
    for disjunct in range.split("||") {
        for comparator in disjunct.split([',', ' ']).filter(|s| !s.is_empty()) {
            if satisfies_comparator(&v, comparator) {
                return true;
            }
        }
    }
    false
}

fn satisfies_comparator(v: &Version, comparator: &str) -> bool {
    let comparator = comparator.trim();
    if comparator == "*" || comparator == "x" || comparator.is_empty() {
        return true;
    }

    if let Some(rest) = comparator.strip_prefix('^') {
        let Some(bound) = parse(rest) else {
            return false;
        };
        // >= bound，且主版本号相同（0.x 的 ^ 只锁定 minor，0.0.x 只锁 patch）
        if compare_versions(v, &bound) < 0 {
            return false;
        }
        if bound.major != 0 {
            return v.major == bound.major;
        }
        if bound.minor != 0 {
            return v.major == bound.major && v.minor == bound.minor;
        }
        return v.major == bound.major && v.minor == bound.minor && v.patch == bound.patch;
    }

    if let Some(rest) = comparator.strip_prefix('~') {
        let Some(bound) = parse(rest) else {
            return false;
        };
        // >= bound 且 minor 相同（major.0 的 ~ 允许 minor 变化）
        if compare_versions(v, &bound) < 0 {
            return false;
        }
        if bound.minor != 0 || bound.patch != 0 {
            return v.major == bound.major && v.minor == bound.minor;
        }
        return v.major == bound.major;
    }

    if let Some(rest) = comparator.strip_prefix('>') {
        let strict = !rest.starts_with('=');
        let rest = rest.trim_start_matches('=');
        let Some(bound) = parse(rest) else {
            return false;
        };
        return match compare_versions(v, &bound) {
            c if c > 0 => true,
            0 => !strict,
            _ => false,
        };
    }
    if let Some(rest) = comparator.strip_prefix('<') {
        let strict = !rest.starts_with('=');
        let rest = rest.trim_start_matches('=');
        let Some(bound) = parse(rest) else {
            return false;
        };
        return match compare_versions(v, &bound) {
            c if c < 0 => true,
            0 => !strict,
            _ => false,
        };
    }

    // x-range：1.2.x / 1.x / 1.2.3
    let parts: Vec<&str> = comparator.split('.').collect();
    if parts.iter().any(|p| *p == "x" || *p == "X" || *p == "*") {
        // 通配位置之后全部视为通配
        let mut iter = parts.iter();
        let major_str = iter.next().copied().unwrap_or("0");
        let Some(major) = major_str.parse::<u64>().ok() else {
            return false;
        };
        if v.major != major {
            return false;
        }
        let minor = match iter.next().copied() {
            Some("x") | Some("X") | Some("*") | None => return true,
            Some(m) => match m.parse::<u64>() {
                Ok(n) => n,
                Err(_) => return false,
            },
        };
        if v.minor != minor {
            return false;
        }
        return match iter.next().copied() {
            Some("x") | Some("X") | Some("*") | None => true,
            Some(p) => p.parse::<u64>().map_or(false, |n| v.patch == n),
        };
    }

    // 精确版本
    match parse(comparator) {
        Some(bound) => compare_versions(v, &bound) == 0,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison() {
        assert!(gt("2.0.0", "1.2.3"));
        assert!(gte("1.2.3", "1.2.3"));
        assert!(lt("1.2.3", "2.0.0"));
        assert!(lte("1.2.3", "1.2.3"));
        assert_eq!(order("1.0.0", "1.0.0"), 0);
        assert_eq!(order("1.0.0", "1.0.1"), -1);
        assert_eq!(order("2.0.0", "1.9.9"), 1);
    }

    #[test]
    fn prerelease_precedence() {
        // 1.0.0-alpha < 1.0.0
        assert!(lt("1.0.0-alpha", "1.0.0"));
        // 数字段比较：alpha.2 > alpha.10？不——数字按数值：alpha.10 > alpha.2
        assert!(lt("1.0.0-alpha.2", "1.0.0-alpha.10"));
        // 字母段按 ASCII：beta > alpha
        assert!(lt("1.0.0-alpha", "1.0.0-beta"));
    }

    #[test]
    fn satisfies_ranges() {
        assert!(satisfies("1.2.3", "^1.2.0"));
        assert!(satisfies("1.9.9", "^1.2.0"));
        assert!(!satisfies("2.0.0", "^1.2.0"));
        assert!(!satisfies("1.1.9", "^1.2.0"));

        assert!(satisfies("1.2.9", "~1.2.3"));
        assert!(!satisfies("1.3.0", "~1.2.3"));
        assert!(!satisfies("1.3.0", "~1.2")); // ~1.2 锁定 minor
        assert!(satisfies("1.3.0", "~1")); // ~1 允许 minor/patch 变化

        assert!(satisfies("1.2.3", "1.2.3"));
        assert!(satisfies("1.2.3", "1.2.x"));
        assert!(satisfies("1.9.0", "1.x"));
        assert!(satisfies("9.9.9", "*"));

        assert!(satisfies("2.0.0", "^1.0.0 || ^2.0.0"));
        assert!(satisfies("1.5.0", "1.0.0 - 2.0.0") == false); // hyphen range 不支持 → false

        assert!(satisfies("1.2.3", ">=1.2.0"));
        assert!(satisfies("1.2.0", ">=1.2.0"));
        assert!(satisfies("1.2.3", ">1.2.2"));
        assert!(satisfies("1.2.2", "<=1.2.2"));
        assert!(satisfies("1.2.1", "<1.2.2"));

        assert!(!satisfies("1.0.0-beta", "^1.0.0"));
    }
}
