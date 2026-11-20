//! config.rs — analytics 共享开关（重写自 `src/services/analytics/config.ts`）。
//!
//! 职责（对齐 TS）：
//! - `isAnalyticsDisabled()`：测试环境 / Bedrock / Vertex / Foundry / 隐私级
//!   别 no-telemetry 或 essential-traffic 时禁用全部 analytics 操作。
//! - `isFeedbackSurveyDisabled()`：仅测试环境与隐私级别控制；不对 3P
//!   提供者生效（问卷是纯本地 UI，不含 transcript 数据）。

use crate::utils::env_utils::is_env_truthy;
use crate::utils::privacy_level::is_telemetry_disabled;

/// analytics 操作是否应被禁用。
pub fn is_analytics_disabled() -> bool {
    std::env::var("NODE_ENV").as_deref() == Ok("test")
        || is_env_truthy(std::env::var("CLAUDE_CODE_USE_BEDROCK").ok().as_deref())
        || is_env_truthy(std::env::var("CLAUDE_CODE_USE_VERTEX").ok().as_deref())
        || is_env_truthy(std::env::var("CLAUDE_CODE_USE_FOUNDRY").ok().as_deref())
        || is_telemetry_disabled()
}

/// 反馈问卷是否应被抑制（不对 3P 提供者生效）。
pub fn is_feedback_survey_disabled() -> bool {
    std::env::var("NODE_ENV").as_deref() == Ok("test") || is_telemetry_disabled()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_disables_analytics() {
        // NODE_ENV=test 一定禁用
        std::env::set_var("NODE_ENV", "test");
        assert!(is_analytics_disabled());
        assert!(is_feedback_survey_disabled());
        std::env::remove_var("NODE_ENV");
    }
}
