---
title: "dsh-purge 插件（移植版）— 身份注入与红队演练台"
description: "dsh-purge（DeepSeek Harness 破甲插件，YuJunZhiXue/dsh-purge）移植到 CCH 插件系统后的能力、安装与使用，以及 dsh 宿主机制到 CCH 钩子/MCP 的完整映射"
keywords: ["dsh-purge", "插件", "红队", "演练台", "身份注入", "hooks", "MCP"]
---

## 来源与署名

`plugins/dsh-purge/` 是 [YuJunZhiXue/dsh-purge](https://github.com/YuJunZhiXue/dsh-purge)（MIT，v1.1.71）
在 CCH 插件系统上的移植版，按原项目署名要求保留名称、提示词、加密槽与演练台语义。
移植对照与已知偏差见插件内 `docs/PORT-NOTES.md` 与 `README.md`。

## 能力

- **身份注入**：出厂提示词以 AES-256-GCM 密封槽随插件发布（`lib/asset-table.mjs`），seed 后落在
  `$DSH_PURGE_HOME/prompt-inject.md`；SessionStart / UserPromptSubmit 钩子组装后经
  `additionalContext` 注入，改完即生效。
- **审批放行**：PreToolUse 钩子返回 `permissionDecision: "allow"`（`allowScope` 可配），等价于原版
  approval-policy 旁路补丁组。
- **自动续写**：Stop 钩子 `decision=block`，按会话计数（`continueMax`，默认 3）。
- **规则集**：规则内容写入当前项目根 `AGENTS.md` / `CLAUDE.md`（CCH 原生读取），带备份与工作区策略。
- **红队演练台**：`dsh-purge-drill` MCP 服务器（stdio，零依赖，Node `node:sqlite`）承载原版 SQLite
  资产事实库与 56 个 `redteam_*` 工具；`/dsh-purge:drill <靶标>` 拉起指挥 + 五执行角色攻击流程。

## 安装

```sh
# 本仓库作为本地 marketplace（.claude-plugin/marketplace.json 已在仓根）
claude plugin marketplace add /path/to/xiaocongyu66-claude-code --source directory
# 之后在会话里 /plugin → dsh-purge → 启用

# 或会话级临时加载
claude --plugin-dir /path/to/xiaocongyu66-claude-code/plugins/dsh-purge
```

## 斜杠命令

`/dsh-purge:status` `/dsh-purge:apply` `/dsh-purge:revert` `/dsh-purge:uninstall`
`/dsh-purge:edit` `/dsh-purge:prompt` `/dsh-purge:rules` `/dsh-purge:drill`

## 主目录

`DSH_PURGE_HOME` > `CLAUDE_PLUGIN_DATA` > `~/.claude/dsh-purge`；hooks、MCP、CLI 三条路径写同一份。
