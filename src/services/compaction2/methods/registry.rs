//! `services/compaction2/methods/registry.ts` — 方法链解析。

use crate::services::compaction2::contracts::{CompactionMethod, CompactionMethodName};

/// 默认回退顺序：server-native 优先，可移植摘要兜底。
pub const DEFAULT_METHOD_ORDER: [CompactionMethodName; 5] = [
    CompactionMethodName::Remote,
    CompactionMethodName::Snapcompact,
    CompactionMethodName::Handoff,
    CompactionMethodName::Shake,
    CompactionMethodName::Soft,
];

/// TS `isCompactionMethodName(value)`：合法方法名判定。
pub fn is_compaction_method_name(value: &str) -> Option<CompactionMethodName> {
    match value {
        "remote" => Some(CompactionMethodName::Remote),
        "snapcompact" => Some(CompactionMethodName::Snapcompact),
        "handoff" => Some(CompactionMethodName::Handoff),
        "shake" => Some(CompactionMethodName::Shake),
        "soft" => Some(CompactionMethodName::Soft),
        _ => None,
    }
}

/// TS `resolveMethodOrder(order?)`：过滤非法条目、保序去重，全部过滤掉
/// 时回落默认链。
pub fn resolve_method_order(order: Option<&[String]>) -> Vec<CompactionMethodName> {
    let source: Vec<String> = match order {
        Some(order) if !order.is_empty() => order.to_vec(),
        _ => DEFAULT_METHOD_ORDER
            .iter()
            .map(|method| method.as_str().to_string())
            .collect(),
    };
    let mut resolved: Vec<CompactionMethodName> = Vec::new();
    for entry in source {
        if let Some(method) = is_compaction_method_name(&entry) {
            if !resolved.contains(&method) {
                resolved.push(method);
            }
        }
    }
    if resolved.is_empty() {
        DEFAULT_METHOD_ORDER.to_vec()
    } else {
        resolved
    }
}

/// TS `indexMethods(methods)`：name → method 索引。
pub fn index_methods(
    methods: &[Box<dyn CompactionMethod>],
) -> std::collections::HashMap<CompactionMethodName, usize> {
    let mut index = std::collections::HashMap::new();
    for (position, method) in methods.iter().enumerate() {
        index.insert(method.name(), position);
    }
    index
}
