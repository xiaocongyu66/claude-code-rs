//! schema_output.rs — 生成 settings 的 JSON Schema 字符串。
//!
//! 重写自 `src/utils/settings/schemaOutput.ts`。TS 用 `toJSONSchema(zod
//! schema)` 生成；Rust 无 zod，改为按 SettingsSchema 的字段表手工构造等价
//! 的 JSON Schema（对齐 `toJSONSchema(..., { unrepresentable: 'any' })` 的
//! 输出形状：type/properties/enum/items/minimum/maximum/description，
//! passthrough → `"additionalProperties": true`）。
//!
//! describe 文本搬运自 types.ts 的 schema 定义。门控字段（feature flag /
//! USER_TYPE / CLAUDE_CODE_ENABLE_XAA）按运行时门控条件包含——与 TS 在
//! schema 构造期展开的行为一致。

use serde_json::{json, Map, Value};

use crate::utils::settings::constants::CLAUDE_CODE_SETTINGS_SCHEMA_URL;
use crate::utils::settings::types::{feature_enabled, known_top_level_keys};
use crate::utils::slow_operations::json_stringify_pretty;

fn ty(kind: &str, desc: &str) -> Value {
    json!({ "type": kind, "description": desc })
}

fn ty_enum(values: &[&str], desc: &str) -> Value {
    json!({ "type": "string", "enum": values, "description": desc })
}

fn arr_of_string(desc: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": desc })
}

fn record_string(desc: &str) -> Value {
    json!({
        "type": "object",
        "additionalProperties": { "type": "string" },
        "description": desc,
    })
}

fn int_range(min: Option<f64>, max: Option<f64>, desc: &str) -> Value {
    let mut m = Map::new();
    m.insert("type".to_string(), Value::String("integer".to_string()));
    if let Some(min) = min {
        m.insert("minimum".to_string(), json!(min));
    }
    if let Some(max) = max {
        m.insert("maximum".to_string(), json!(max));
    }
    m.insert("description".to_string(), Value::String(desc.to_string()));
    Value::Object(m)
}

fn mcp_server_entry_schema(desc: &str) -> Value {
    let entry = json!({
        "type": "object",
        "properties": {
            "serverName": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$",
                "description": "Name of the MCP server" },
            "serverCommand": { "type": "array", "items": { "type": "string" }, "minItems": 1,
                "description": "Command array [command, ...args] to match exactly" },
            "serverUrl": { "type": "string",
                "description": "URL pattern with wildcard support (e.g., \"https://*.example.com/*\")" },
        },
    });
    json!({ "type": "array", "items": entry, "description": desc })
}

fn marketplace_source_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "source": { "type": "string", "enum": [
                "url", "github", "git", "npm", "file", "directory", "hostPattern", "pathPattern", "settings"
            ]},
            "url": { "type": "string", "format": "uri" },
            "repo": { "type": "string" },
            "ref": { "type": "string" },
            "path": { "type": "string" },
            "sparsePaths": { "type": "array", "items": { "type": "string" } },
            "package": { "type": "string" },
            "hostPattern": { "type": "string" },
            "pathPattern": { "type": "string" },
            "name": { "type": "string" },
            "plugins": { "type": "array", "items": { "type": "object" } },
            "owner": { "type": "object" },
            "headers": { "type": "object", "additionalProperties": { "type": "string" } }
        },
        "required": ["source"],
    })
}

fn permissions_schema() -> Value {
    let mut props = Map::new();
    props.insert(
        "allow".to_string(),
        arr_of_string("List of permission rules for allowed operations"),
    );
    props.insert(
        "deny".to_string(),
        arr_of_string("List of permission rules for denied operations"),
    );
    props.insert(
        "ask".to_string(),
        arr_of_string(
            "List of permission rules that should always prompt for confirmation",
        ),
    );
    props.insert(
        "defaultMode".to_string(),
        ty_enum(
            &["acceptEdits", "plan", "bypassPermissions", "default", "dontAsk", "auto"],
            "Default permission mode when Claude Code needs access",
        ),
    );
    props.insert(
        "disableBypassPermissionsMode".to_string(),
        ty_enum(&["disable"], "Disable the ability to bypass permission prompts"),
    );
    if feature_enabled("TRANSCRIPT_CLASSIFIER") {
        props.insert(
            "disableAutoMode".to_string(),
            ty_enum(&["disable"], "Disable auto mode"),
        );
    }
    props.insert(
        "additionalDirectories".to_string(),
        arr_of_string("Additional directories to include in the permission scope"),
    );
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "additionalProperties": true,
        "description": "Tool usage permissions configuration",
    })
}

fn compaction_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "enabled": { "type": "boolean", "description": "Whether automatic V2 compaction is enabled (default: true)" },
            "methodOrder": { "type": "array", "items": {}, "description":
                "Preferred fallback order for compaction methods. Unknown names are ignored (default: remote, snapcompact, handoff, shake, soft)" },
            "thresholdTokens": int_range(Some(1.0), None, "Fixed token threshold; overrides thresholdPercent when set"),
            "thresholdPercent": int_range(Some(1.0), Some(99.0), "Threshold as a percentage of the context window (1-99)"),
            "reserveTokens": int_range(Some(1.0), None,
                "Tokens reserved for the response (default: max(15% of window, 16384))"),
            "keepRecentTokens": int_range(Some(0.0), None, "Most recent tokens kept intact (default: 20000)"),
            "midTurnEnabled": { "type": "boolean", "description": "Check thresholds at mid-turn tool boundaries (default: true)" },
            "autoContinue": { "type": "boolean", "description": "Continue automatically after compaction (default: true)" },
            "asyncEnabled": { "type": "boolean", "description": "Speculatively compact in the background (default: true)" },
            "idleEnabled": { "type": "boolean", "description": "Compact while idle (default: false)" },
            "idleThresholdTokens": int_range(Some(1.0), None, "Idle token threshold (default: 200000)"),
            "idleTimeoutSeconds": int_range(Some(60.0), None, "Seconds to wait while idle before compacting (default: 300)"),
        },
        "additionalProperties": true,
        "description": "V2 compaction policy (COMPACTION_V2). Invalid fields are ignored and fall back to the built-in defaults.",
    })
}

fn sandbox_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "enabled": { "type": "boolean" },
            "failIfUnavailable": { "type": "boolean" },
            "autoAllowBashIfSandboxed": { "type": "boolean" },
            "allowUnsandboxedCommands": { "type": "boolean" },
            "network": {
                "type": "object",
                "properties": {
                    "allowedDomains": arr_of_string(""),
                    "allowManagedDomainsOnly": { "type": "boolean" },
                    "allowUnixSockets": arr_of_string("macOS only: Unix socket paths to allow."),
                    "allowAllUnixSockets": { "type": "boolean" },
                    "allowLocalBinding": { "type": "boolean" },
                    "httpProxyPort": { "type": "number" },
                    "socksProxyPort": { "type": "number" },
                },
            },
            "filesystem": {
                "type": "object",
                "properties": {
                    "allowWrite": arr_of_string("Additional paths to allow writing within the sandbox."),
                    "denyWrite": arr_of_string("Additional paths to deny writing within the sandbox."),
                    "denyRead": arr_of_string("Additional paths to deny reading within the sandbox."),
                    "allowRead": arr_of_string("Paths to re-allow reading within denyRead regions."),
                    "allowManagedReadPathsOnly": { "type": "boolean" },
                },
            },
            "ignoreViolations": {
                "type": "object",
                "additionalProperties": { "type": "array", "items": { "type": "string" } },
            },
            "enableWeakerNestedSandbox": { "type": "boolean" },
            "enableWeakerNetworkIsolation": { "type": "boolean" },
            "excludedCommands": arr_of_string(""),
            "strictSandboxMode": { "type": "boolean" },
            "unsandboxedCommandsDisabled": { "type": "boolean" },
            "ripgrep": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "args": arr_of_string(""),
                },
                "required": ["command"],
            },
        },
        "additionalProperties": true,
    })
}

fn hooks_schema() -> Value {
    let hook = json!({
        "type": "object",
        "properties": {
            "matcher": { "type": "string", "description": "String pattern to match (e.g. tool names like \"Write\")" },
            "hooks": { "type": "array", "items": {
                "type": "object",
                "properties": {
                    "type": { "type": "string", "enum": ["command", "prompt", "agent", "http"] },
                    "command": { "type": "string" },
                    "prompt": { "type": "string" },
                    "url": { "type": "string", "format": "uri" },
                    "if": { "type": "string" },
                    "shell": { "type": "string", "enum": ["bash", "powershell"] },
                    "timeout": { "type": "number" },
                    "model": { "type": "string" },
                    "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                    "allowedEnvVars": arr_of_string(""),
                    "statusMessage": { "type": "string" },
                    "once": { "type": "boolean" },
                    "async": { "type": "boolean" },
                    "asyncRewake": { "type": "boolean" },
                },
                "required": ["type"],
            }},
        },
        "required": ["hooks"],
    });
    json!({
        "type": "object",
        "additionalProperties": { "type": "array", "items": hook },
        "description": "Custom commands to run before/after tool executions",
    })
}

fn auto_mode_schema() -> Value {
    let mut props = Map::new();
    props.insert("allow".to_string(), arr_of_string("Rules for the auto mode classifier allow section"));
    props.insert("soft_deny".to_string(), arr_of_string("Rules for the auto mode classifier deny section"));
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        props.insert("deny".to_string(), arr_of_string("Back-compat alias for ant users; external users use soft_deny"));
    }
    props.insert("environment".to_string(), arr_of_string("Entries for the auto mode classifier environment section"));
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "description": "Auto mode classifier prompt customization",
    })
}

/// 构造 settings 的 JSON Schema（对象形式）。
pub fn settings_json_schema_value() -> Value {
    let mut props: Map<String, Value> = Map::new();

    props.insert(
        "$schema".to_string(),
        ty_enum(&[CLAUDE_CODE_SETTINGS_SCHEMA_URL], "JSON Schema reference for Claude Code settings"),
    );
    props.insert("apiKeyHelper".to_string(), ty("string", "Path to a script that outputs authentication values"));
    props.insert("awsCredentialExport".to_string(), ty("string", "Path to a script that exports AWS credentials"));
    props.insert("awsAuthRefresh".to_string(), ty("string", "Path to a script that refreshes AWS authentication"));
    props.insert("gcpAuthRefresh".to_string(), ty("string", "Command to refresh GCP authentication"));
    if std::env::var("CLAUDE_CODE_ENABLE_XAA").map(|v| v == "1").unwrap_or(false) {
        props.insert("xaaIdp".to_string(), json!({
            "type": "object",
            "properties": {
                "issuer": { "type": "string", "format": "uri", "description": "IdP issuer URL for OIDC discovery" },
                "clientId": { "type": "string", "description": "Claude Code's client_id registered at the IdP" },
                "callbackPort": int_range(Some(1.0), None,
                    "Fixed loopback callback port for the IdP OIDC login."),
            },
            "required": ["issuer", "clientId"],
            "description": "XAA (SEP-990) IdP connection.",
        }));
    }
    props.insert("fileSuggestion".to_string(), json!({
        "type": "object",
        "properties": {
            "type": { "type": "string", "enum": ["command"] },
            "command": { "type": "string" },
        },
        "required": ["type", "command"],
        "description": "Custom file suggestion configuration for @ mentions",
    }));
    props.insert("respectGitignore".to_string(), ty("boolean", "Whether file picker should respect .gitignore files (default: true). Note: .ignore files are always respected."));
    props.insert("cleanupPeriodDays".to_string(), int_range(Some(0.0), None,
        "Number of days to retain chat transcripts (default: 30). Setting to 0 disables session persistence entirely: no transcripts are written and existing transcripts are deleted at startup."));
    props.insert("env".to_string(), json!({
        "type": "object",
        "additionalProperties": { "type": "string" },
        "description": "Environment variables to set for Claude Code sessions",
    }));
    props.insert("attribution".to_string(), json!({
        "type": "object",
        "properties": {
            "commit": { "type": "string", "description": "Attribution text for git commits, including any trailers. Empty string hides attribution." },
            "pr": { "type": "string", "description": "Attribution text for pull request descriptions. Empty string hides attribution." },
        },
        "description": "Customize attribution text for commits and PRs.",
    }));
    props.insert("includeCoAuthoredBy".to_string(), ty("boolean", "Deprecated: Use attribution instead. Whether to include Claude's co-authored by attribution in commits and PRs (defaults to true)"));
    props.insert("includeGitInstructions".to_string(), ty("boolean", "Include built-in commit and PR workflow instructions in Claude's system prompt (default: true)"));
    props.insert("permissions".to_string(), permissions_schema());
    props.insert("toolAliases".to_string(), record_string("Map of tool-name aliases consumed by permission-rule matching."));
    props.insert("modelType".to_string(), ty_enum(
        &["anthropic", "openai", "gemini", "grok"],
        "API provider type. \"anthropic\" uses the Anthropic API (default), \"openai\" uses the OpenAI Chat Completions API, \"gemini\" uses the Gemini API, and \"grok\" uses the xAI Grok API (OpenAI-compatible)."));
    props.insert("model".to_string(), ty("string", "Override the default model used by Claude Code"));
    props.insert("availableModels".to_string(), arr_of_string("Allowlist of models that users can select."));
    props.insert("modelOverrides".to_string(), record_string("Override mapping from Anthropic model ID to provider-specific model ID."));
    props.insert("contextWindowOverrides".to_string(), json!({
        "type": "object",
        "additionalProperties": { "type": "integer", "minimum": 1 },
        "description": "Per-model context window overrides (model ID -> context window in tokens).",
    }));
    props.insert("compaction".to_string(), compaction_schema());
    props.insert("enableAllProjectMcpServers".to_string(), ty("boolean", "Whether to automatically approve all MCP servers in the project"));
    props.insert("enabledMcpjsonServers".to_string(), arr_of_string("List of approved MCP servers from .mcp.json"));
    props.insert("disabledMcpjsonServers".to_string(), arr_of_string("List of rejected MCP servers from .mcp.json"));
    props.insert("allowedMcpServers".to_string(), mcp_server_entry_schema(
        "Enterprise allowlist of MCP servers that can be used. If undefined, all servers are allowed. If empty array, no servers are allowed. Denylist takes precedence."));
    props.insert("deniedMcpServers".to_string(), mcp_server_entry_schema(
        "Enterprise denylist of MCP servers that are explicitly blocked. Denylist takes precedence over allowlist."));
    props.insert("hooks".to_string(), hooks_schema());
    props.insert("worktree".to_string(), json!({
        "type": "object",
        "properties": {
            "symlinkDirectories": arr_of_string("Directories to symlink from main repository to worktrees to avoid disk bloat."),
            "sparsePaths": arr_of_string("Directories to include when creating worktrees, via git sparse-checkout (cone mode)."),
        },
        "description": "Git worktree configuration for --worktree flag.",
    }));
    props.insert("disableAllHooks".to_string(), ty("boolean", "Disable all hooks and statusLine execution"));
    props.insert("defaultShell".to_string(), ty_enum(&["bash", "powershell"], "Default shell for input-box ! commands."));
    props.insert("allowManagedHooksOnly".to_string(), ty("boolean", "When true (and set in managed settings), only hooks from managed settings run."));
    props.insert("allowedHttpHookUrls".to_string(), arr_of_string("Allowlist of URL patterns that HTTP hooks may target. Supports * as a wildcard."));
    props.insert("httpHookAllowedEnvVars".to_string(), arr_of_string("Allowlist of environment variable names HTTP hooks may interpolate into headers."));
    props.insert("allowManagedPermissionRulesOnly".to_string(), ty("boolean", "When true (and set in managed settings), only permission rules (allow/deny/ask) from managed settings are respected."));
    props.insert("allowManagedMcpServersOnly".to_string(), ty("boolean", "When true (and set in managed settings), allowedMcpServers is only read from managed settings."));
    props.insert("strictPluginOnlyCustomization".to_string(), json!({
        "oneOf": [
            { "type": "boolean" },
            { "type": "array", "items": { "type": "string", "enum": ["skills", "agents", "hooks", "mcp"] } },
        ],
        "description": "When set in managed settings, blocks non-plugin customization sources for the listed surfaces.",
    }));
    props.insert("statusLine".to_string(), json!({
        "type": "object",
        "properties": {
            "type": { "type": "string", "enum": ["command"] },
            "command": { "type": "string" },
            "padding": { "type": "number" },
            "refreshInterval": { "type": "number" },
        },
        "required": ["type", "command"],
        "description": "Custom status line display configuration",
    }));
    props.insert("statusLineEnabled".to_string(), ty("boolean", "Whether to render the fork built-in status line. Toggled with /statusline."));
    props.insert("enabledPlugins".to_string(), json!({
        "type": "object",
        "additionalProperties": {
            "oneOf": [
                { "type": "array", "items": { "type": "string" } },
                { "type": "boolean" },
            ],
        },
        "description": "Enabled plugins using plugin-id@marketplace-id format.",
    }));
    props.insert("extraKnownMarketplaces".to_string(), json!({
        "type": "object",
        "additionalProperties": json!({
            "type": "object",
            "properties": {
                "source": marketplace_source_schema(),
                "installLocation": { "type": "string" },
                "autoUpdate": { "type": "boolean" },
            },
            "required": ["source"],
        }),
        "description": "Additional marketplaces to make available for this repository.",
    }));
    props.insert("strictKnownMarketplaces".to_string(), json!({
        "type": "array",
        "items": marketplace_source_schema(),
        "description": "Enterprise strict list of allowed marketplace sources.",
    }));
    props.insert("blockedMarketplaces".to_string(), json!({
        "type": "array",
        "items": marketplace_source_schema(),
        "description": "Enterprise blocklist of marketplace sources.",
    }));
    props.insert("forceLoginMethod".to_string(), ty_enum(&["claudeai", "console"], "Force a specific login method."));
    props.insert("forceLoginOrgUUID".to_string(), ty("string", "Organization UUID to use for OAuth login"));
    props.insert("otelHeadersHelper".to_string(), ty("string", "Path to a script that outputs OpenTelemetry headers"));
    props.insert("outputStyle".to_string(), ty("string", "Controls the output style for assistant responses"));
    props.insert("language".to_string(), ty("string", "Preferred language for Claude responses and voice dictation"));
    props.insert("skipWebFetchPreflight".to_string(), ty("boolean", "Skip the WebFetch blocklist check for enterprise environments"));
    props.insert("webSearchAdapter".to_string(), ty_enum(&["api", "bing", "brave", "exa", "tavily"], "Web search backend adapter."));
    props.insert("webFetchAdapter".to_string(), ty_enum(&["tavily", "http"], "Web fetch backend."));
    props.insert("tavilyEndpointUrl".to_string(), ty("string", "Custom Tavily API endpoint URL."));
    props.insert("braveApiKey".to_string(), ty("string", "Brave Search API key."));
    props.insert("webFetchHttpTimeoutMs".to_string(), int_range(Some(1.0), None, "HTTP timeout in milliseconds for the HTTP direct web fetch backend. Defaults to 60000 (60s)."));
    props.insert("exaApiKey".to_string(), ty("string", "Exa AI API key."));
    props.insert("exaEndpointUrl".to_string(), ty("string", "Custom Exa AI MCP endpoint URL."));
    props.insert("sandbox".to_string(), sandbox_schema());
    props.insert("feedbackSurveyRate".to_string(), int_range(Some(0.0), Some(1.0), "Probability (0-1) that the session quality survey appears when eligible."));
    props.insert("spinnerTipsEnabled".to_string(), ty("boolean", "Whether to show tips in the spinner"));
    props.insert("uiLocale".to_string(), ty("string", "UI language (BCP-47 tag): en, zh-CN, zh-TW, ja, ko."));
    props.insert("spinnerVerbs".to_string(), json!({
        "type": "object",
        "properties": {
            "mode": { "type": "string", "enum": ["append", "replace"] },
            "verbs": { "type": "array", "items": { "type": "string" } },
        },
        "required": ["mode", "verbs"],
        "description": "Customize spinner verbs.",
    }));
    props.insert("spinnerTipsOverride".to_string(), json!({
        "type": "object",
        "properties": {
            "excludeDefault": { "type": "boolean" },
            "tips": { "type": "array", "items": { "type": "string" } },
        },
        "required": ["tips"],
        "description": "Override spinner tips.",
    }));
    props.insert("syntaxHighlightingDisabled".to_string(), ty("boolean", "Whether to disable syntax highlighting in diffs"));
    props.insert("terminalTitleFromRename".to_string(), ty("boolean", "Whether /rename updates the terminal tab title (defaults to true)."));
    props.insert("alwaysThinkingEnabled".to_string(), ty("boolean", "When false, thinking is disabled."));
    let effort_values: Vec<&str> = if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        vec!["low", "medium", "high", "xhigh", "max"]
    } else {
        vec!["low", "medium", "high", "xhigh"]
    };
    props.insert("effortLevel".to_string(), ty_enum(&effort_values, "Persisted effort level for supported models."));
    props.insert("advisorModel".to_string(), ty("string", "Advisor model for the server-side advisor tool."));
    props.insert("fastMode".to_string(), ty("boolean", "When true, fast mode is enabled."));
    props.insert("fastModePerSessionOptIn".to_string(), ty("boolean", "When true, fast mode does not persist across sessions."));
    props.insert("promptSuggestionEnabled".to_string(), ty("boolean", "When false, prompt suggestions are disabled."));
    props.insert("poorMode".to_string(), ty("boolean", "When true, poor mode is active — extract_memories and prompt_suggestion are disabled to save tokens."));
    props.insert("showClearContextOnPlanAccept".to_string(), ty("boolean", "When true, the plan-approval dialog offers a \"clear context\" option. Defaults to false."));
    props.insert("agent".to_string(), ty("string", "Name of an agent (built-in or custom) to use for the main thread."));
    props.insert("companyAnnouncements".to_string(), arr_of_string("Company announcements to display at startup."));
    props.insert("pluginConfigs".to_string(), json!({
        "type": "object",
        "additionalProperties": json!({
            "type": "object",
            "properties": {
                "mcpServers": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "additionalProperties": {
                            "oneOf": [
                                { "type": "string" },
                                { "type": "number" },
                                { "type": "boolean" },
                                { "type": "array", "items": { "type": "string" } },
                            ],
                        },
                    },
                    "description": "User configuration values for MCP servers keyed by server name",
                },
                "options": {
                    "type": "object",
                    "additionalProperties": {
                        "oneOf": [
                            { "type": "string" },
                            { "type": "number" },
                            { "type": "boolean" },
                            { "type": "array", "items": { "type": "string" } },
                        ],
                    },
                    "description": "Non-sensitive option values from plugin manifest userConfig, keyed by option name.",
                },
            },
        }),
        "description": "Per-plugin configuration including MCP server user configs, keyed by plugin ID (plugin@marketplace format)",
    }));
    props.insert("remote".to_string(), json!({
        "type": "object",
        "properties": {
            "defaultEnvironmentId": { "type": "string", "description": "Default environment ID to use for remote sessions" },
        },
        "description": "Remote session configuration",
    }));
    props.insert("autoUpdatesChannel".to_string(), ty_enum(&["latest", "stable"], "Release channel for auto-updates (latest or stable)"));
    if feature_enabled("LODESTONE") {
        props.insert("disableDeepLinkRegistration".to_string(), ty_enum(&["disable"], "Prevent claude-cli:// protocol handler registration with the OS"));
    }
    props.insert("minimumVersion".to_string(), ty("string", "Minimum version to stay on - prevents downgrades when switching to stable channel"));
    props.insert("plansDirectory".to_string(), ty("string", "Custom directory for plan files, relative to project root."));
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        props.insert("classifierPermissionsEnabled".to_string(), ty("boolean", "Enable AI-based classification for Bash(prompt:...) permission rules"));
    }
    if feature_enabled("PROACTIVE") || feature_enabled("KAIROS") {
        props.insert("minSleepDurationMs".to_string(), int_range(Some(0.0), None, "Minimum duration in milliseconds that the Sleep tool must sleep for."));
        props.insert("maxSleepDurationMs".to_string(), int_range(Some(-1.0), None, "Maximum duration in milliseconds that the Sleep tool can sleep for. Set to -1 for indefinite sleep."));
    }
    if feature_enabled("VOICE_MODE") {
        props.insert("voiceEnabled".to_string(), ty("boolean", "Enable voice mode (hold-to-talk dictation)"));
        props.insert("voiceProvider".to_string(), ty_enum(&["anthropic", "doubao"], "Voice STT backend: \"anthropic\" (default) or \"doubao\" (Doubao ASR)"));
    }
    if feature_enabled("KAIROS") {
        props.insert("assistant".to_string(), ty("boolean", "Start Claude in assistant mode"));
        props.insert("assistantName".to_string(), ty("string", "Display name for the assistant"));
    }
    props.insert("channelsEnabled".to_string(), ty("boolean", "Teams/Enterprise opt-in for channel notifications. Default off."));
    props.insert("allowedChannelPlugins".to_string(), json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "marketplace": { "type": "string" },
                "plugin": { "type": "string" },
            },
            "required": ["marketplace", "plugin"],
        },
        "description": "Teams/Enterprise allowlist of channel plugins.",
    }));
    if feature_enabled("KAIROS") || feature_enabled("KAIROS_BRIEF") {
        props.insert("defaultView".to_string(), ty_enum(&["chat", "transcript"], "Default transcript view: chat or transcript."));
    }
    props.insert("prefersReducedMotion".to_string(), ty("boolean", "Reduce or disable animations for accessibility"));
    props.insert("autoMemoryEnabled".to_string(), ty("boolean", "Enable auto-memory for this project."));
    props.insert("autoMemoryDirectory".to_string(), ty("string", "Custom directory path for auto-memory storage."));
    props.insert("autoDreamEnabled".to_string(), ty("boolean", "Enable background memory consolidation (auto-dream)."));
    props.insert("showThinkingSummaries".to_string(), ty("boolean", "Show thinking summaries in the transcript view (ctrl+o). Default: false."));
    props.insert("skipDangerousModePermissionPrompt".to_string(), ty("boolean", "Whether the user has accepted the bypass permissions mode dialog"));
    if feature_enabled("TRANSCRIPT_CLASSIFIER") {
        props.insert("skipAutoPermissionPrompt".to_string(), ty("boolean", "Whether the user has accepted the auto mode opt-in dialog"));
        props.insert("useAutoModeDuringPlan".to_string(), ty("boolean", "Whether plan mode uses auto mode semantics when auto mode is available (default: true)"));
        props.insert("autoMode".to_string(), auto_mode_schema());
    }
    props.insert("disableAutoMode".to_string(), ty_enum(&["disable"], "Disable auto mode"));
    props.insert("sshConfigs".to_string(), json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "name": { "type": "string" },
                "sshHost": { "type": "string" },
                "sshPort": { "type": "integer" },
                "sshIdentityFile": { "type": "string" },
                "startDirectory": { "type": "string" },
            },
            "required": ["id", "name", "sshHost"],
        },
        "description": "SSH connection configurations for remote environments.",
    }));
    props.insert("claudeMdExcludes".to_string(), arr_of_string("Glob patterns or absolute paths of CLAUDE.md files to exclude from loading."));
    props.insert("cacheThreshold".to_string(), int_range(Some(0.0), Some(100.0), "Prompt cache hit rate threshold (0-100). Default: 70."));
    props.insert("cacheWarningEnabled".to_string(), ty("boolean", "Whether to show cache hit rate warnings. Default: true."));
    props.insert("pluginTrustMessage".to_string(), ty("string", "Custom message to append to the plugin trust warning shown before installation."));
    props.insert("workspaceApiKey".to_string(), ty("string", "Workspace API key (sk-ant-api03-*) saved via /login UI."));

    // 门控字段保持与 known_top_level_keys 一致（文档性断言，不阻断生成）。
    let _ = known_top_level_keys();

    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "Claude Code Settings",
        "type": "object",
        "properties": Value::Object(props),
        "additionalProperties": true,
    })
}

/// `generateSettingsJSONSchema()`：pretty-printed JSON Schema 字符串。
pub fn generate_settings_json_schema() -> String {
    json_stringify_pretty(&settings_json_schema_value(), 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_valid_json_object() {
        let s = generate_settings_json_schema();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "object");
        assert_eq!(v["additionalProperties"], true);
        let props = v["properties"].as_object().unwrap();
        assert!(props.contains_key("model"));
        assert!(props.contains_key("permissions"));
        assert!(props.contains_key("$schema"));
    }

    #[test]
    fn enum_fields_carry_values() {
        let v = settings_json_schema_value();
        let model_type = &v["properties"]["modelType"];
        assert_eq!(model_type["type"], "string");
        let allowed: Vec<&str> = model_type["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(allowed, vec!["anthropic", "openai", "gemini", "grok"]);
    }
}
