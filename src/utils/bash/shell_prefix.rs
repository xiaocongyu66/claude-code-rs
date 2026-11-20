//! shell_prefix.rs — shell 前缀命令格式化。
//!
//! 重写自 `src/utils/bash/shellPrefix.ts`。语义对齐：`formatShellPrefixCommand`
//! 按最后一个 `-` 前的空格切分可执行路径与参数，分别 quote 后拼接。

use crate::utils::bash::shell_quote::quote;

/// 解析可能包含可执行路径与参数的 shell 前缀并格式化。
///
/// 例：
/// - `"bash"` → `'bash'`
/// - `"/usr/bin/bash -c"` → `'/usr/bin/bash' -c`
/// - `"C:\Program Files\Git\bin\bash.exe -c"` → `'C:\Program Files\Git\bin\bash.exe' -c`
pub fn format_shell_prefix_command(prefix: &str, command: &str) -> String {
    // 在 `-` 前的最后一个空格处切分可执行文件与参数。
    let space_before_dash = prefix.rfind(" -");
    if let Some(idx) = space_before_dash {
        if idx > 0 {
            let exec_path = &prefix[..idx];
            let args = &prefix[idx + 1..];
            return format!(
                "{} {} {}",
                quote(&[exec_path.to_string()]),
                args,
                quote(&[command.to_string()])
            );
        }
    }
    format!(
        "{} {}",
        quote(&[prefix.to_string()]),
        quote(&[command.to_string()])
    )
}
