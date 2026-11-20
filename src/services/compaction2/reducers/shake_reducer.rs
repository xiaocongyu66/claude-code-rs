//! `services/compaction2/reducers/shakeReducer.rs` — 纯 shake reducer 到
//! 方法链注入接口（W1 → W2）的适配器。
//!
//! 被卸载的文本经 CCH 现有 tool-result 存储持久化，替换为短的可恢复路径
//! 引用，被 shake 的区域可用常规 Read 工具读回。本模块不调模型。

use std::future::Future;
use std::pin::Pin;

use sha2::{Digest, Sha1};

use crate::services::compaction2::contracts::CompactionContext;
use crate::services::compaction2::methods::shake::{ShakeReducer, ShakeReduction};
use crate::services::compaction2::reducers::shake::{
    default_shake_config, shake_messages, ShakeConfig, ShakeRegion,
};
use crate::services::compaction2::tokens::estimate_tokens;

/// 自定义卸载函数（Rust 侧以 boxed future 支持 dyn 分发）。
pub type OffloadFn = Box<
    dyn Fn(&ShakeRegion, &CompactionContext) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>>
        + Send
        + Sync,
>;

/// TS `ShakeReducerOptions`。
pub struct ShakeReducerOptions {
    pub config: Option<ShakeConfig>,
    /// 覆盖默认 tool-result 存储卸载（测试用）。
    pub offload: Option<OffloadFn>,
}

/// TS `createShakeReducer(options?)`。
pub fn create_shake_reducer(options: Option<ShakeReducerOptions>) -> Box<dyn ShakeReducer> {
    let (config, custom_offload) = match options {
        Some(options) => (options.config, options.offload),
        None => (None, None),
    };
    let config = config.unwrap_or_else(default_shake_config);

    match custom_offload {
        Some(offload) => Box::new(CustomOffloadReducer { config, offload }),
        None => Box::new(DefaultReducer { config }),
    }
}

struct DefaultReducer {
    config: ShakeConfig,
}

impl ShakeReducer for DefaultReducer {
    fn reduce<'a>(
        &'a self,
        ctx: &'a CompactionContext,
    ) -> Pin<Box<dyn Future<Output = Option<ShakeReduction>> + Send + 'a>> {
        Box::pin(async move {
            let count_tokens = |text: &str| estimate_tokens(text, &ctx.model).tokens as usize;
            let offload = |region: &ShakeRegion| Some(default_offload_text(region));
            let result = shake_messages(&ctx.messages, count_tokens, &self.config, offload).await;
            if result.regions.is_empty() || result.tokens_saved == 0 {
                return None;
            }
            Some(ShakeReduction {
                messages: result.messages,
                tokens_saved: result.tokens_saved,
                warning: None,
            })
        })
    }
}

struct CustomOffloadReducer {
    config: ShakeConfig,
    offload: OffloadFn,
}

impl ShakeReducer for CustomOffloadReducer {
    fn reduce<'a>(
        &'a self,
        ctx: &'a CompactionContext,
    ) -> Pin<Box<dyn Future<Output = Option<ShakeReduction>> + Send + 'a>> {
        Box::pin(async move {
            let count_tokens = |text: &str| estimate_tokens(text, &ctx.model).tokens as usize;
            let offload = |region: &ShakeRegion| (self.offload)(region, ctx);
            let result = shake_messages(&ctx.messages, count_tokens, &self.config, offload).await;
            if result.regions.is_empty() || result.tokens_saved == 0 {
                return None;
            }
            Some(ShakeReduction {
                messages: result.messages,
                tokens_saved: result.tokens_saved,
                warning: None,
            })
        })
    }
}

/// TS `defaultOffload(region, ctx)` 的同步文本部分：
/// 确定性 id——同一来源重 shake 复用既有 artifact；含内容 hash
/// （tool_use_id 可能被第三方 provider 复用），落文件名前消毒。
fn default_offload_text(region: &ShakeRegion) -> String {
    let original_text = match region {
        ShakeRegion::ToolResult(region) => &region.original_text,
        ShakeRegion::Block(region) => &region.original_text,
    };
    let content_hash = {
        let mut hasher = Sha1::new();
        hasher.update(original_text.as_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(16);
        for byte in digest.iter().take(8) {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    };
    let stable_key = match region {
        ShakeRegion::ToolResult(region) => {
            format!("{}-{content_hash}", region.tool_use_id)
        }
        ShakeRegion::Block(region) => {
            format!(
                "{}-{}-{content_hash}",
                region.message_index, region.block_index
            )
        }
    };
    let id = format!(
        "shake-{}",
        stable_key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    );
    let label = match region {
        ShakeRegion::ToolResult(region) => region.label.clone(),
        ShakeRegion::Block(region) => region.label.clone(),
    };
    // TODO(utils/toolResultStorage.ts): persistToolResult(original_text, id)
    // 成功 → `[Shaken: ${label} content moved to ${filepath}]`；
    // 失败（'error' in persisted）→ 抛错让 shake_messages 跳过该区域
    // （无目标标记永不覆盖可恢复内容）。
    format!("[Shaken: {label} content moved to {id}]")
}
