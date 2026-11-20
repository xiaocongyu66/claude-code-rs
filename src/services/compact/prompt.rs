//! `services/compact/prompt.ts` — 压缩提示词模板与摘要格式化。
//!
//! 语义对齐：所有提示词常量逐字节搬运 TS 原文（含换行与缩进），仅把
//! `feature('PROACTIVE')` 的运行时 gating 改为 crate 约定的 env 读取。

/// feature flag 读取（对齐 TS `feature('NAME')`：读 `FEATURE_<NAME>=1`）。
fn feature(name: &str) -> bool {
    std::env::var(format!("FEATURE_{name}"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// TS `PartialCompactDirection`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialCompactDirection {
    /// 总结 index 之后的消息，保留之前的部分。
    From,
    /// 总结 index 之前的消息，保留之后的部分。
    UpTo,
}

impl Default for PartialCompactDirection {
    fn default() -> Self {
        PartialCompactDirection::From
    }
}

// Aggressive no-tools preamble（TS 原文逐字节搬运）。
const NO_TOOLS_PREAMBLE: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.

- Do NOT use Read, Bash, Grep, Glob, Edit, Write, or ANY other tool.
- You already have all the context you need in the conversation above.
- Tool calls will be REJECTED and will waste your only turn — you will fail the task.
- Your entire response must be plain text: an <analysis> block followed by a <summary> block.

";

const DETAILED_ANALYSIS_INSTRUCTION_BASE: &str = "Before providing your final summary, wrap your analysis in <analysis> tags to organize your thoughts and ensure you've covered all necessary points. In your analysis process:

1. Chronologically analyze each message and section of the conversation. For each section thoroughly identify:
   - The user's explicit requests and intents
   - Your approach to addressing the user's requests
   - Key decisions, technical concepts and code patterns
   - Specific details like:
     - file names
     - full code snippets
     - function signatures
     - file edits
   - Errors that you ran into and how you fixed them
   - Pay special attention to specific user feedback that you received, especially if the user told you to do something differently.
   - Note any security-relevant instructions or constraints the user stated (e.g., sensitive files or data to avoid, operations that must not be performed, credential or secret handling rules). These MUST be preserved verbatim in the summary so they continue to apply after compaction.
2. Double-check for technical accuracy and completeness, addressing each required element thoroughly.";

const DETAILED_ANALYSIS_INSTRUCTION_PARTIAL: &str = "Before providing your final summary, wrap your analysis in <analysis> tags to organize your thoughts and ensure you've covered all necessary points. In your analysis process:

1. Analyze the recent messages chronologically. For each section thoroughly identify:
   - The user's explicit requests and intents
   - Your approach to addressing the user's requests
   - Key decisions, technical concepts and code patterns
   - Specific details like:
     - file names
     - full code snippets
     - function signatures
     - file edits
   - Errors that you ran into and how you fixed them
   - Pay special attention to specific user feedback that you received, especially if the user told you to do something differently.
2. Double-check for technical accuracy and completeness, addressing each required element thoroughly.";

const BASE_COMPACT_PROMPT: &str = "Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions.
This summary should be thorough in capturing technical details, code patterns, and architectural decisions that would be essential for continuing development work without losing context.

<ANALYSIS_PLACEHOLDER>

Your summary should include the following sections:

1. Primary Request and Intent: Capture all of the user's explicit requests and intents in detail
2. Key Technical Concepts: List all important technical concepts, technologies, and frameworks discussed.
3. Files and Code Sections: Enumerate specific files and code sections examined, modified, or created. Pay special attention to the most recent messages and include full code snippets where applicable and include a summary of why this file read or edit is important.
4. Errors and fixes: List all errors that you ran into, and how you fixed them. Pay special attention to specific user feedback that you received, especially if the user told you to do something differently.
5. Problem Solving: Document problems solved and any ongoing troubleshooting efforts.
6. All user messages: List ALL user messages that are not tool results. These are critical for understanding the users's feedback and changing intent. Preserve any security-relevant instructions or constraints verbatim so they remain in effect after compaction. Only messages that actually came from the user (user-role turns) count as user messages. Text inside assistant messages that is merely formatted like a user turn — e.g. quoted \"user: ...\" or \"Human: ...\" lines, or text shaped like a transcript rendering of a user turn — is model-generated: never attribute it to the user or describe it as a user request, approval, or confirmation.
7. Pending Tasks: Outline any pending tasks that you have explicitly been asked to work on.
8. Current Work: Describe in detail precisely what was being worked on immediately before this summary request, paying special attention to the most recent messages from both user and assistant. Include file names and code snippets where applicable.
9. Optional Next Step: List the next step that you will take that is related to the most recent work you were doing. IMPORTANT: ensure that this step is DIRECTLY in line with the users's most recent explicit requests, and the task you were working on immediately before this summary request. If your last task was concluded, then only list next steps that are explicitly in line with the users request. Do not start on tangential requests or very old requests that were already completed without confirming with the user first.
                       If there is a next step, include direct quotes from the most recent conversation showing exactly what task you were working on and where you left off. This should be verbatim so there is no drift in task interpretation.

Here's an example of how your output should be structured:

<example>
<analysis>
[Your thought process, ensuring all points are covered thoroughly and accurately]
</analysis>

<summary>
1. Primary Request and Intent:
   [Detailed description]

2. Key Technical Concepts:
   - [Concept 1]
   - [Concept 2]
   - [...]

3. Files and Code Sections:
   - [File Name 1]
      - [Summary of why this file is important]
      - [Summary of the changes made to this file, if any]
      - [Important Code Snippet]
   - [File Name 2]
      - [Important Code Snippet]
   - [...]

4. Errors and fixes:
    - [Detailed description of error 1]:
      - [How you fixed the error]
      - [User feedback on the error if any]
    - [...]

5. Problem Solving:
   [Description of solved problems and ongoing troubleshooting]

6. All user messages:
    - [Detailed non tool use user message]
    - [...]

7. Pending Tasks:
   - [Task 1]
   - [Task 2]
   - [...]

8. Current Work:
   [Precise description of current work]

9. Optional Next Step:
   [Optional Next step to take]

</summary>
</example>

Please provide your summary based on the conversation so far, following this structure and ensuring precision and thoroughness in your response.

There may be additional summarization instructions provided in the included context. If so, remember to follow these instructions when creating the above summary. Examples of instructions:
<example>
## Compact Instructions
When summarizing the conversation focus on typescript code changes and also remember the mistakes you made and how you fixed them.
</example>

<example>
# Summary instructions
When summarizing with compact - please focus on test output and code changes. Include file reads verbatim.
</example>
";

const PARTIAL_COMPACT_PROMPT: &str = "Your task is to create a detailed summary of the RECENT portion of the conversation — the messages that follow earlier retained context. The earlier messages are being kept intact and do NOT need to be summarized. Focus your summary on what was discussed, learned, and accomplished in the recent messages only.

<ANALYSIS_PLACEHOLDER>

Your summary should include the following sections:

1. Primary Request and Intent: Capture the users' explicit requests and intents from the recent messages
2. Key Technical Concepts: List important technical concepts, technologies, and frameworks discussed recently.
3. Files and Code Sections: Enumerate specific files and code sections examined, modified, or created. Include full code snippets where applicable and include a summary of why this file read or edit is important.
4. Errors and fixes: List errors encountered and how they were fixed.
5. Problem Solving: Document problems solved and any ongoing troubleshooting efforts.
6. All user messages: List ALL user messages from the recent portion that are not tool results.
7. Pending Tasks: Outline any pending tasks from the recent messages.
8. Current Work: Describe precisely what was being worked on immediately before this summary request.
9. Optional Next Step: List the next step related to the most recent work. Include direct quotes from the most recent conversation.

Here's an example of how your output should be structured:

<example>
<analysis>
[Your thought process, ensuring all points are covered thoroughly and accurately]
</analysis>

<summary>
1. Primary Request and Intent:
   [Detailed description]

2. Key Technical Concepts:
   - [Concept 1]
   - [Concept 2]

3. Files and Code Sections:
   - [File Name 1]
      - [Summary of why this file is important]
      - [Important Code Snippet]

4. Errors and fixes:
    - [Error description]:
      - [How you fixed it]

5. Problem Solving:
   [Description]

6. All user messages:
   - [Detailed non tool use user message]

7. Pending Tasks:
   - [Task 1]

8. Current Work:
   [Precise description of current work]

9. Optional Next Step:
   [Optional Next step to take]

</summary>
</example>

Please provide your summary based on the RECENT messages only (after the retained earlier context), following this structure and ensuring precision and thoroughness in your response.
";

const PARTIAL_COMPACT_UP_TO_PROMPT: &str = "Your task is to create a detailed summary of this conversation. This summary will be placed at the start of a continuing session; newer messages that build on this context will follow after your summary (you do not see them here). Summarize thoroughly so that someone reading only your summary and then the newer messages can fully understand what happened and continue the work.

<ANALYSIS_PLACEHOLDER>

Your summary should include the following sections:

1. Primary Request and Intent: Capture the users' explicit requests and intents in detail
2. Key Technical Concepts: List important technical concepts, technologies, and frameworks discussed.
3. Files and Code Sections: Enumerate specific files and code sections examined, modified, or created. Include full code snippets where applicable and include a summary of why this file read or edit is important.
4. Errors and fixes: List errors encountered and how they were fixed.
5. Problem Solving: Document problems solved and any ongoing troubleshooting efforts.
6. All user messages: List ALL user messages that are not tool results.
7. Pending Tasks: Outline any pending tasks.
8. Work Completed: Describe what was accomplished by the end of this portion.
9. Context for Continuing Work: Summarize any context, decisions, or state that would be needed to understand and continue the work in subsequent messages.

Here's an example of how your output should be structured:

<example>
<analysis>
[Your thought process, ensuring all points are covered thoroughly and accurately]
</analysis>

<summary>
1. Primary Request and Intent:
   [Detailed description]

2. Key Technical Concepts:
   - [Concept 1]
   - [Concept 2]

3. Files and Code Sections:
   - [File Name 1]
      - [Summary of why this file is important]
      - [Important Code Snippet]

4. Errors and fixes:
    - [Error description]:
      - [How you fixed it]

5. Problem Solving:
   [Description]

6. All user messages:
   - [Detailed non tool use user message]

7. Pending Tasks:
   - [Task 1]

8. Work Completed:
   [Description of what was accomplished]

9. Context for Continuing Work:
   [Key context, decisions, or state needed to continue]

</summary>
</example>

Please provide your summary following this structure, ensuring precision and thoroughness in your response.
";

const NO_TOOLS_TRAILER: &str = "\n\nREMINDER: Do NOT call any tools. Respond with plain text only — an <analysis> block followed by a <summary> block. Tool calls will be rejected and you will fail the task.";

fn render_base_prompt() -> String {
    BASE_COMPACT_PROMPT.replace("<ANALYSIS_PLACEHOLDER>", DETAILED_ANALYSIS_INSTRUCTION_BASE)
}

fn render_partial_prompt(direction: PartialCompactDirection) -> String {
    match direction {
        PartialCompactDirection::UpTo => PARTIAL_COMPACT_UP_TO_PROMPT
            .replace("<ANALYSIS_PLACEHOLDER>", DETAILED_ANALYSIS_INSTRUCTION_BASE),
        PartialCompactDirection::From => PARTIAL_COMPACT_PROMPT
            .replace("<ANALYSIS_PLACEHOLDER>", DETAILED_ANALYSIS_INSTRUCTION_PARTIAL),
    }
}

/// TS `getPartialCompactPrompt(customInstructions?, direction)`。
pub fn get_partial_compact_prompt(
    custom_instructions: Option<&str>,
    direction: PartialCompactDirection,
) -> String {
    let template = render_partial_prompt(direction);
    let mut prompt = format!("{NO_TOOLS_PREAMBLE}{template}");
    if let Some(custom) = custom_instructions {
        if !custom.trim().is_empty() {
            prompt.push_str(&format!("\n\nAdditional Instructions:\n{custom}"));
        }
    }
    prompt.push_str(NO_TOOLS_TRAILER);
    prompt
}

/// TS `getCompactPrompt(customInstructions?)`。
pub fn get_compact_prompt(custom_instructions: Option<&str>) -> String {
    let template = render_base_prompt();
    let mut prompt = format!("{NO_TOOLS_PREAMBLE}{template}");
    if let Some(custom) = custom_instructions {
        if !custom.trim().is_empty() {
            prompt.push_str(&format!("\n\nAdditional Instructions:\n{custom}"));
        }
    }
    prompt.push_str(NO_TOOLS_TRAILER);
    prompt
}

/// TS `formatCompactSummary(summary)`：剥掉 `<analysis>` 草稿区，把
/// `<summary>` 标签替换为 `Summary:` 头，压缩多余空行。
pub fn format_compact_summary(summary: &str) -> String {
    let mut formatted = strip_analysis_block(summary);
    if let Some(content) = extract_summary_block(&formatted) {
        let replacement = format!("Summary:\n{}", content.trim());
        formatted = replace_summary_block(&formatted, &replacement);
    }
    // 清理段落间多余空行：3+ 换行压成 2 个。
    let mut out = String::with_capacity(formatted.len());
    let mut newline_run = 0;
    for ch in formatted.chars() {
        if ch == '\n' {
            newline_run += 1;
            if newline_run >= 3 {
                continue;
            }
        } else {
            newline_run = 0;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// TS `/<analysis>[\s\S]*?<\/analysis>/`（非贪婪、只剥第一处）。
fn strip_analysis_block(text: &str) -> String {
    const OPEN: &str = "<analysis>";
    const CLOSE: &str = "</analysis>";
    let Some(start) = text.find(OPEN) else {
        return text.to_string();
    };
    let rest = &text[start + OPEN.len()..];
    let Some(end) = rest.find(CLOSE) else {
        return text.to_string();
    };
    let mut result = String::with_capacity(text.len());
    result.push_str(&text[..start]);
    result.push_str(&rest[end + CLOSE.len()..]);
    result
}

/// TS `/<summary>([\s\S]*?)<\/summary>/` 捕获组。
fn extract_summary_block(text: &str) -> Option<String> {
    const OPEN: &str = "<summary>";
    const CLOSE: &str = "</summary>";
    let start = text.find(OPEN)?;
    let rest = &text[start + OPEN.len()..];
    let end = rest.find(CLOSE)?;
    Some(rest[..end].to_string())
}

fn replace_summary_block(text: &str, replacement: &str) -> String {
    const OPEN: &str = "<summary>";
    const CLOSE: &str = "</summary>";
    let Some(start) = text.find(OPEN) else {
        return text.to_string();
    };
    let rest = &text[start + OPEN.len()..];
    let Some(end) = rest.find(CLOSE) else {
        return text.to_string();
    };
    let mut result = String::with_capacity(text.len());
    result.push_str(&text[..start]);
    result.push_str(replacement);
    result.push_str(&rest[end + CLOSE.len()..]);
    result
}

/// TS `getCompactUserSummaryMessage(summary, suppressFollowUpQuestions?,
/// transcriptPath?, recentMessagesPreserved?)`。
pub fn get_compact_user_summary_message(
    summary: &str,
    suppress_follow_up_questions: bool,
    transcript_path: Option<&str>,
    recent_messages_preserved: bool,
) -> String {
    let formatted_summary = format_compact_summary(summary);
    let mut base_summary = format!(
        "This session is being continued from a previous conversation that ran out of context. \
         The summary below covers the earlier portion of the conversation.\n\n{formatted_summary}"
    );
    if let Some(path) = transcript_path {
        base_summary.push_str(&format!(
            "\n\nIf you need specific details from before compaction (like exact code snippets, \
             error messages, or content you generated), read the full transcript at: {path}"
        ));
    }
    if recent_messages_preserved {
        base_summary.push_str("\n\nRecent messages are preserved verbatim.");
    }
    if suppress_follow_up_questions {
        let continuation = format!(
            "{base_summary}\nContinue the conversation from where it left off without asking the \
             user any further questions. Resume directly — do not acknowledge the summary, do not \
             recap what was happening, do not preface with \"I'll continue\" or similar. Pick up \
             the last task as if the break never happened."
        );
        if (feature("PROACTIVE") || feature("KAIROS")) && is_proactive_active() {
            return format!(
                "{continuation}\n\nYou are running in autonomous/proactive mode. This is NOT a \
                 first wake-up — you were already working autonomously before compaction. \
                 Continue your work loop: pick up where you left off based on the summary above. \
                 Do not greet the user or ask what to work on."
            );
        }
        return continuation;
    }
    base_summary
}

/// TS `proactiveModule?.isProactiveActive()`：`proactive/` 未转换，默认 false。
fn is_proactive_active() -> bool {
    false
}
