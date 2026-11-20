//! embedded_natives.rs — 内嵌原生模块表。
//!
//! 重写自 `src/utils/embeddedNatives.gen.ts`。仓库中的这份是空模板：
//! dev / 常规构建下 load_native_module 直接走 vendor/ 回退；编译单文件
//! 二进制时原本由构建插件把目标平台的 base64 内容注入此表。
//!
//! key   = 模块名（token-counter / transcript-parser / color-diff /
//! file-index / search）
//! value = .node 文件内容的 base64

use std::collections::HashMap;
use std::sync::OnceLock;

static EMBEDDED_NATIVES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

/// 内嵌原生模块表（构建时注入）；空模板下恒为空表。
pub fn embedded_natives() -> &'static HashMap<&'static str, &'static str> {
    EMBEDDED_NATIVES.get_or_init(HashMap::new)
}
