//! tree_sitter_analysis.rs — bash 命令安全校验用的 tree-sitter AST 分析。
//!
//! 重写自 `src/utils/bash/treeSitterAnalysis.ts`。从 tree-sitter 解析树中
//! 提取安全相关信息，比 regex/shell-quote 解析更精确。每个函数接受根节点
//! 与命令字符串，返回结构化数据。
//!
//! 已知差异：
//! - TS 端用字符索引切片命令串（AST 的 startIndex/endIndex 实为 UTF-8 字节
//!   偏移）；Rust 端直接按字节偏移切片 &str，语义更一致。
//! - TS 端用对象展开 `{ ...node, children: [x] }` 构造单孩子节点再递归；
//!   Rust 端直接对子节点切片递归（walk_top_level 接受 &[&TsNode]）。
//! - 节点字段采用 snake_case，复用 bash_parser::TsNode。

use std::collections::HashSet;

use crate::utils::bash::bash_parser::TsNode;

/// 引号上下文
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuoteContext {
    /// 移除单引号内容、保留双引号内容的命令文本
    pub with_double_quotes: String,
    /// 移除全部引号内内容的命令文本
    pub fully_unquoted: String,
    /// 移除引号内内容但保留引号字符（'、"）
    pub unquoted_keep_quote_chars: String,
}

/// 复合命令结构
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompoundStructure {
    /// 顶层是否有复合算符（&& || ;）
    pub has_compound_operators: bool,
    /// 是否有流水线
    pub has_pipeline: bool,
    /// 是否有子 shell
    pub has_subshell: bool,
    /// 是否有命令组 {...}
    pub has_command_group: bool,
    /// 顶层复合算符类型
    pub operators: Vec<String>,
    /// 按复合算符切分的命令片段
    pub segments: Vec<String>,
}

/// 危险模式
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DangerousPatterns {
    /// 有 $() 或反引号命令替换（在不会使其安全的引号外）
    pub has_command_substitution: bool,
    /// 有 <() 或 >() 进程替换
    pub has_process_substitution: bool,
    /// 有 ${...} 参数展开
    pub has_parameter_expansion: bool,
    /// 有 heredoc
    pub has_heredoc: bool,
    /// 有注释
    pub has_comment: bool,
}

/// 完整分析结果
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeSitterAnalysis {
    pub quote_context: QuoteContext,
    pub compound_structure: CompoundStructure,
    /// 是否存在真正的算符节点（; && ||）——false 时 \; 只是普通词参数
    pub has_actual_operator_nodes: bool,
    pub dangerous_patterns: DangerousPatterns,
}

/// 各类引号 span（起始、结束字节偏移）
#[derive(Debug, Clone, Default)]
struct QuoteSpans {
    /// raw_string（单引号）
    raw: Vec<(usize, usize)>,
    /// ansi_c_string（$'...'）
    ansi_c: Vec<(usize, usize)>,
    /// string（双引号）
    double: Vec<(usize, usize)>,
    /// 带引号定界符的 heredoc_redirect
    heredoc: Vec<(usize, usize)>,
}

/// 单趟收集所有引号相关 span。
///
/// 复刻原逐类型遍历语义：每次原遍历在自身类型处停止。raw_string 遍历会
/// 穿过 string 节点（不是它的类型）去够 $(...) 内嵌的 raw_string，而
/// string 遍历在外层 string 处停止。用 inDouble 标志收集每条路径上
/// *最外层* 的 string span，同时仍下钻 $()/${} 体收取内部
/// raw_string/ansi_c_string。
///
/// raw_string / ansi_c_string / 带引号 heredoc 体在 bash 中是字面文本，
/// 不可能有嵌套引号节点——直接返回。
fn collect_quote_spans(node: &TsNode, out: &mut QuoteSpans, in_double: bool) {
    match node.node_type.as_str() {
        "raw_string" => {
            out.raw.push((node.start_index, node.end_index));
            return; // 字面体，无嵌套引号
        }
        "ansi_c_string" => {
            out.ansi_c.push((node.start_index, node.end_index));
            return; // 字面体
        }
        "string" => {
            // 只收集最外层的 string（对应旧的逐类型遍历在首个匹配处停止）
            if !in_double {
                out.double.push((node.start_index, node.end_index));
            }
            for child in &node.children {
                collect_quote_spans(child, out, true);
            }
            return;
        }
        "heredoc_redirect" => {
            // 带引号 heredoc（<<'EOF'、<<"EOF"、<<\EOF）：体是字面量。
            // 不带引号的（<<EOF）会展开 $()/${}——体内可能含 $(cmd 'x')
            // 其内部 '...' 是真正的 raw_string 节点。
            // 检测：heredoc_start 文本以 ' " \ 开头
            let mut is_quoted = false;
            for child in &node.children {
                if child.node_type == "heredoc_start" {
                    is_quoted = child
                        .text
                        .chars()
                        .next()
                        .map(|c| c == '\'' || c == '"' || c == '\\')
                        .unwrap_or(false);
                    break;
                }
            }
            if is_quoted {
                out.heredoc.push((node.start_index, node.end_index));
                return; // 字面体，无嵌套引号节点
            }
            // 非引号：继续下钻 heredoc_body → command_substitution → 内部引号节点
        }
        _ => {}
    }
    for child in &node.children {
        collect_quote_spans(child, out, in_double);
    }
}

/// 构建 span 覆盖的字符位置集合
fn build_position_set(spans: &[(usize, usize)]) -> HashSet<usize> {
    let mut set = HashSet::new();
    for &(start, end) in spans {
        for i in start..end {
            set.insert(i);
        }
    }
    set
}

/// 丢弃被其他 span 完全包含的 span，只留最外层。嵌套引号（如
/// `"$(echo 'hi')"`）会产生重叠 span——先删内层再处理可避免索引错位。
fn drop_contained_spans(spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
    spans
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            !spans.iter().enumerate().any(|(j, other)| {
                j != *i && other.0 <= s.0 && other.1 >= s.1 && (other.0 < s.0 || other.1 > s.1)
            })
        })
        .map(|(_, s)| *s)
        .collect()
}

/// 从字符串中移除 span 覆盖的字符区间
fn remove_spans(command: &str, spans: &[(usize, usize)]) -> String {
    if spans.is_empty() {
        return command.to_string();
    }
    let mut sorted = drop_contained_spans(spans);
    // 按起始降序，从后往前删避免偏移错位
    sorted.sort_by(|a, b| b.0.cmp(&a.0));
    let mut result = command.to_string();
    for (start, end) in sorted {
        let mut s = String::with_capacity(result.len() - (end - start));
        s.push_str(&result[..start]);
        s.push_str(&result[end..]);
        result = s;
    }
    result
}

/// 把 span 内容替换为只剩引号定界符（保留 ' 和 " 字符）
fn replace_spans_keep_quotes(command: &str, spans: &[(usize, usize, &str, &str)]) -> String {
    if spans.is_empty() {
        return command.to_string();
    }
    let mut sorted: Vec<(usize, usize, &str, &str)> = spans.to_vec();
    sorted.sort_by(|a, b| b.0.cmp(&a.0));
    let mut result = command.to_string();
    for (start, end, open, close) in sorted {
        let mut s = String::with_capacity(result.len() + open.len() + close.len());
        s.push_str(&result[..start]);
        s.push_str(open);
        s.push_str(close);
        s.push_str(&result[end..]);
        result = s;
    }
    result
}

/// 从 tree-sitter AST 提取引号上下文。
///
/// 节点类型：
/// - raw_string: 单引号 ('...')
/// - string: 双引号 ("...")
/// - ansi_c_string: ANSI-C 引号（$'...'）——span 含前导 $
/// - heredoc_redirect: 仅带引号 heredoc（<<'EOF'、<<"EOF"、<<\EOF）——
///   整个重定向 span（<<、定界符、体、换行）都被剥离，因为体在 bash 中
///   是字面文本（无展开）。不带引号 heredoc（<<EOF）保留——bash 会展开
///   其中的 $(...)/${}，校验器需要看到这些模式。
pub fn extract_quote_context(root: &TsNode, command: &str) -> QuoteContext {
    let mut spans = QuoteSpans::default();
    collect_quote_spans(root, &mut spans, false);
    let single_quote_spans = spans.raw.clone();
    let ansi_c_spans = spans.ansi_c.clone();
    let double_quote_spans = spans.double.clone();
    let quoted_heredoc_spans = spans.heredoc.clone();
    let mut all_quote_spans: Vec<(usize, usize)> = Vec::new();
    all_quote_spans.extend(single_quote_spans.iter().copied());
    all_quote_spans.extend(ansi_c_spans.iter().copied());
    all_quote_spans.extend(double_quote_spans.iter().copied());
    all_quote_spans.extend(quoted_heredoc_spans.iter().copied());

    // 各输出变体要排除的位置集合。
    // withDoubleQuotes：整体移除单引号 span，外加双引号 span 的首尾 `"`
    // 定界符（保留中间内容）。与 regex extractQuotedContent() 语义一致——
    // `"` 切换引号状态但内容仍输出。
    let mut single_quote_spans_all: Vec<(usize, usize)> = Vec::new();
    single_quote_spans_all.extend(single_quote_spans.iter().copied());
    single_quote_spans_all.extend(ansi_c_spans.iter().copied());
    single_quote_spans_all.extend(quoted_heredoc_spans.iter().copied());
    let single_quote_set = build_position_set(&single_quote_spans_all);
    let mut double_quote_delim_set = HashSet::new();
    for &(start, end) in &double_quote_spans {
        double_quote_delim_set.insert(start); // 开 "
        double_quote_delim_set.insert(end - 1); // 闭 "
    }
    let mut with_double_quotes = String::with_capacity(command.len());
    for (i, ch) in command.char_indices() {
        if single_quote_set.contains(&i) || double_quote_delim_set.contains(&i) {
            continue;
        }
        with_double_quotes.push(ch);
    }

    // fullyUnquoted：移除全部引号内内容
    let fully_unquoted = remove_spans(command, &all_quote_spans);

    // unquotedKeepQuoteChars：移除内容但保留定界符字符
    let mut spans_with_quote_chars: Vec<(usize, usize, &str, &str)> = Vec::new();
    for &(start, end) in &single_quote_spans {
        spans_with_quote_chars.push((start, end, "'", "'"));
    }
    for &(start, end) in &ansi_c_spans {
        // ansi_c_string span 含前导 $；保留 $ 使结果与 regex 路径一致
        //（regex 路径把 $ 当作 ' 前面的未引号字符）
        spans_with_quote_chars.push((start, end, "$'", "'"));
    }
    for &(start, end) in &double_quote_spans {
        spans_with_quote_chars.push((start, end, "\"", "\""));
    }
    for &(start, end) in &quoted_heredoc_spans {
        // heredoc_redirect span 无内联引号定界符——整体剥离
        spans_with_quote_chars.push((start, end, "", ""));
    }
    let unquoted_keep_quote_chars = replace_spans_keep_quotes(command, &spans_with_quote_chars);

    QuoteContext {
        with_double_quotes,
        fully_unquoted,
        unquoted_keep_quote_chars,
    }
}

/// walkTopLevel 的迭代状态
#[derive(Default)]
struct CompoundWalkState {
    operators: Vec<String>,
    segments: Vec<String>,
    has_subshell: bool,
    has_command_group: bool,
    has_pipeline: bool,
}

/// 遍历顶层子节点，收集复合结构。
fn walk_top_level(children: &[&TsNode], st: &mut CompoundWalkState) {
    for &child in children {
        match child.node_type.as_str() {
            "list" => {
                // list 节点含 && 与 || 算符
                let list_kids: Vec<&TsNode> = child.children.iter().collect();
                for &list_child in &list_kids {
                    match list_child.node_type.as_str() {
                        "&&" | "||" => st.operators.push(list_child.node_type.clone()),
                        // 嵌套 list，或包住 list/pipeline 的 redirected_statement——
                        // 递归使内部算符/流水线被检测到。对
                        // `cmd1 && cmd2 2>/dev/null && cmd3`，
                        // redirected_statement 包住 list(cmd1 && cmd2)，
                        // 不递归会漏掉内部的 &&
                        "list" | "redirected_statement" => {
                            let single = [list_child];
                            walk_top_level(&single, st);
                        }
                        "pipeline" => {
                            st.has_pipeline = true;
                            st.segments.push(list_child.text.clone());
                        }
                        "subshell" => {
                            st.has_subshell = true;
                            st.segments.push(list_child.text.clone());
                        }
                        "compound_statement" => {
                            st.has_command_group = true;
                            st.segments.push(list_child.text.clone());
                        }
                        _ => {
                            st.segments.push(list_child.text.clone());
                        }
                    }
                }
            }
            ";" => st.operators.push(";".to_string()),
            "pipeline" => {
                st.has_pipeline = true;
                st.segments.push(child.text.clone());
            }
            "subshell" => {
                st.has_subshell = true;
                st.segments.push(child.text.clone());
            }
            "compound_statement" => {
                st.has_command_group = true;
                st.segments.push(child.text.clone());
            }
            "command" | "declaration_command" | "variable_assignment" => {
                st.segments.push(child.text.clone());
            }
            "redirected_statement" => {
                // `cd ~/src && find path 2>/dev/null` —— tree-sitter 用
                // redirected_statement 包住整个复合体：program →
                // redirected_statement → (list → cmd1, &&, cmd2) + file_redirect。
                // 递归检测内部结构；跳过 file_redirect 子节点
                let mut found_inner = false;
                let inner_kids: Vec<&TsNode> = child.children.iter().collect();
                for &inner in &inner_kids {
                    if inner.node_type == "file_redirect" {
                        continue;
                    }
                    found_inner = true;
                    let single = [inner];
                    walk_top_level(&single, st);
                }
                if !found_inner {
                    // 无体的孤立重定向（不应发生，但保险起见）
                    st.segments.push(child.text.clone());
                }
            }
            "negated_command" => {
                // `! cmd` —— 递归内部命令使其结构被分类，同时记录完整文本
                // 保持 segments 数量有意义
                st.segments.push(child.text.clone());
                let kids: Vec<&TsNode> = child.children.iter().collect();
                walk_top_level(&kids, st);
            }
            "if_statement" | "while_statement" | "for_statement" | "case_statement"
            | "function_definition" => {
                // 控制流构造本身是一个片段，递归使内部流水线/子 shell/
                // 算符被检测到
                st.segments.push(child.text.clone());
                let kids: Vec<&TsNode> = child.children.iter().collect();
                walk_top_level(&kids, st);
            }
            _ => {}
        }
    }
}

/// 从 AST 提取复合命令结构
pub fn extract_compound_structure(root: &TsNode, command: &str) -> CompoundStructure {
    let mut st = CompoundWalkState::default();
    let children: Vec<&TsNode> = root.children.iter().collect();
    walk_top_level(&children, &mut st);

    // 若没有片段，整条命令就是一个片段
    if st.segments.is_empty() {
        st.segments.push(command.to_string());
    }

    CompoundStructure {
        has_compound_operators: !st.operators.is_empty(),
        has_pipeline: st.has_pipeline,
        has_subshell: st.has_subshell,
        has_command_group: st.has_command_group,
        operators: st.operators,
        segments: st.segments,
    }
}

/// AST 中是否存在真正的算符节点（; && ||）。
///
/// 消除 `find -exec \;` 误报的关键：tree-sitter 把 `\;` 解析为 word 节点
/// （find 的参数）而非 `;` 算符。若无实际 `;` 算符节点，则不存在复合
/// 命令，可跳过 hasBackslashEscapedOperator 检查。
pub fn has_actual_operator_nodes(root: &TsNode) -> bool {
    fn walk(node: &TsNode) -> bool {
        // 指示复合命令的算符类型
        if node.node_type == ";"
            || node.node_type == "&&"
            || node.node_type == "||"
            || node.node_type == "list"
        {
            return true;
        }
        node.children.iter().any(walk)
    }
    walk(root)
}

/// 从 AST 提取危险模式信息
pub fn extract_dangerous_patterns(root: &TsNode) -> DangerousPatterns {
    #[derive(Default)]
    struct State {
        command_substitution: bool,
        process_substitution: bool,
        parameter_expansion: bool,
        heredoc: bool,
        comment: bool,
    }
    fn walk(node: &TsNode, st: &mut State) {
        match node.node_type.as_str() {
            "command_substitution" => st.command_substitution = true,
            "process_substitution" => st.process_substitution = true,
            "expansion" => st.parameter_expansion = true,
            "heredoc_redirect" => st.heredoc = true,
            "comment" => st.comment = true,
            _ => {}
        }
        for child in &node.children {
            walk(child, st);
        }
    }
    let mut st = State::default();
    walk(root, &mut st);
    DangerousPatterns {
        has_command_substitution: st.command_substitution,
        has_process_substitution: st.process_substitution,
        has_parameter_expansion: st.parameter_expansion,
        has_heredoc: st.heredoc,
        has_comment: st.comment,
    }
}

/// 对命令做完整 tree-sitter 分析，一次性提取全部安全相关数据。
pub fn analyze_command(root: &TsNode, command: &str) -> TreeSitterAnalysis {
    TreeSitterAnalysis {
        quote_context: extract_quote_context(root, command),
        compound_structure: extract_compound_structure(root, command),
        has_actual_operator_nodes: has_actual_operator_nodes(root),
        dangerous_patterns: extract_dangerous_patterns(root),
    }
}
