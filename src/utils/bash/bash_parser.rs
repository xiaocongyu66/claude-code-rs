//! bash_parser.rs — 纯 Rust bash 解析器，产出 tree-sitter-bash 兼容 AST。
//!
//! 重写自 `src/utils/bash/bashParser.ts`。词法器 + 递归下降解析器，
//! 下游（parser.ts / ast.ts / prefix.ts / ParsedCommand.ts）按节点类型与
//! 字段遍历产出。startIndex / endIndex 为 UTF-8 字节偏移（与 TS 端一致）。
//!
//! 已知差异：
//! - TS 端用 JS 字符串下标 + UTF-8 字节偏移双轨记账（byteTable + 二分）；
//!   Rust 字符串本身就是 UTF-8，字节偏移贯穿全程，byteTable/sliceBytes/
//!   restoreLexToByte 全部省略，行为等价。
//! - 超时用 `Instant` 检测（默认 50ms，`timeout_ms` 为 None 时取默认值）；
//!   预算 MAX_NODES = 50_000。超限返回 None，等价于 TS 的 throw → null。
//! - 深度嵌套输入在 64MB 栈的工作线程中解析（等价于 JS 引擎的大调用栈）；
//!   线程内 panic 经 join 吞掉返回 None。
//! - 节点字段采用 snake_case（node_type / start_index / end_index）。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 超时上限（毫秒）——与 TS 端 PARSE_TIMEOUT_MS 一致。
const PARSE_TIMEOUT_MS: u64 = 50;

/// 节点预算上限——与 TS 端 MAX_NODES 一致。
const MAX_NODES: usize = 50_000;

/// tree-sitter-bash 兼容 AST 节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsNode {
    pub node_type: String,
    pub text: String,
    /// UTF-8 字节偏移
    pub start_index: usize,
    /// UTF-8 字节偏移（exclusive）
    pub end_index: usize,
    pub children: Vec<TsNode>,
}

/// 解析失败/中止统一返回 None（与 TS 端 `null` 对齐）。
pub fn parse_source(source: &str, timeout_ms: Option<u64>) -> Option<TsNode> {
    let timeout = timeout_ms.unwrap_or(PARSE_TIMEOUT_MS);
    let owned = source.to_string();
    let result = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .name("bash-parser".into())
        .spawn(move || parse_inline(&owned, timeout))
        .map(|handle| handle.join().ok().flatten())
        .unwrap_or_else(|_| parse_inline(source, timeout));
    result
}

fn parse_inline(source: &str, timeout: u64) -> Option<TsNode> {
    let mut p = ParseState::new(source, timeout);
    match p.parse_program() {
        Ok(prog) if !p.aborted => Some(prog),
        _ => None,
    }
}

/// shell 关键字集合（TS 端 SHELL_KEYWORDS）。
pub fn is_shell_keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "then" | "elif" | "else" | "fi" | "while" | "until" | "for" | "in" | "do"
            | "done" | "case" | "esac" | "function" | "select"
    )
}

fn is_special_var(c: char) -> bool {
    matches!(c, '?' | '$' | '@' | '*' | '#' | '-' | '!' | '_')
}

fn is_decl_keyword(w: &str) -> bool {
    matches!(w, "export" | "declare" | "typeset" | "readonly" | "local")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseErr {
    Budget,
    Timeout,
}

type PResult<T> = Result<T, ParseErr>;

// ───────────────────────────── Tokenizer ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    Number,
    Op,
    Newline,
    Comment,
    DQuote,
    SQuote,
    AnsiC,
    Dollar,
    DollarParen,
    DollarBrace,
    DollarDParen,
    Backtick,
    LtParen,
    GtParen,
    Eof,
}

#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    kind: TokenKind,
    value: &'a str,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Cmd,
    Arg,
}

#[derive(Debug, Clone)]
struct HeredocPending {
    delim: String,
    strip_tabs: bool,
    quoted: bool,
    body_start: usize,
    body_end: usize,
    end_start: usize,
    end_end: usize,
}

struct Lexer<'a> {
    src: &'a str,
    /// UTF-8 字节偏移（等价于 TS 端的 byte offset b）
    i: usize,
    heredocs: VecDeque<HeredocPending>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Lexer { src, i: 0, heredocs: VecDeque::new() }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.i..].chars().next()
    }

    /// TS 端 peek(L, off)
    fn peek_off(&self, off: usize) -> Option<char> {
        self.src[self.i..].chars().nth(off)
    }

    fn advance(&mut self) {
        if let Some(c) = self.peek() {
            self.i += c.len_utf8();
        }
    }

    fn skip_blanks(&mut self) {
        while self.i < self.src.len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == ' ' || c == '\t' || c == '\r' {
                self.advance();
            } else if c == '\\' {
                let nx = self.peek_off(1);
                if nx == Some('\n') || (nx == Some('\r') && self.peek_off(2) == Some('\n')) {
                    self.advance();
                    self.advance();
                    if nx == Some('\r') {
                        self.advance();
                    }
                } else if nx == Some(' ') || nx == Some('\t') {
                    self.advance();
                    self.advance();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// 扫描下一个 token。上下文敏感：cmd 位置把 `[` 当算符，arg 位置当普通字符。
    fn next_token(&mut self, ctx: Ctx) -> Token<'a> {
        self.skip_blanks();
        let start = self.i;
        if self.i >= self.src.len() {
            return Token { kind: TokenKind::Eof, value: "", start, end: start };
        }
        let c = match self.peek() {
            Some(c) => c,
            None => return Token { kind: TokenKind::Eof, value: "", start, end: start },
        };
        let c1 = self.peek_off(1);
        let c2 = self.peek_off(2);

        if c == '\n' {
            self.advance();
            return Token { kind: TokenKind::Newline, value: "\n", start, end: self.i };
        }
        if c == '#' {
            let si = self.i;
            while self.i < self.src.len() && self.peek() != Some('\n') {
                self.advance();
            }
            return Token { kind: TokenKind::Comment, value: &self.src[si..self.i], start, end: self.i };
        }

        // 多字符算符（最长匹配优先）
        if c == '&' && c1 == Some('&') {
            self.advance();
            self.advance();
            return op(start, self.i, "&&", self.src);
        }
        if c == '|' && c1 == Some('|') {
            self.advance();
            self.advance();
            return op(start, self.i, "||", self.src);
        }
        if c == '|' && c1 == Some('&') {
            self.advance();
            self.advance();
            return op(start, self.i, "|&", self.src);
        }
        if c == ';' && c1 == Some(';') && c2 == Some('&') {
            self.advance();
            self.advance();
            self.advance();
            return op(start, self.i, ";;&", self.src);
        }
        if c == ';' && c1 == Some(';') {
            self.advance();
            self.advance();
            return op(start, self.i, ";;", self.src);
        }
        if c == ';' && c1 == Some('&') {
            self.advance();
            self.advance();
            return op(start, self.i, ";&", self.src);
        }
        if c == '>' && c1 == Some('>') {
            self.advance();
            self.advance();
            return op(start, self.i, ">>", self.src);
        }
        if c == '>' && c1 == Some('&') && c2 == Some('-') {
            self.advance();
            self.advance();
            self.advance();
            return op(start, self.i, ">&-", self.src);
        }
        if c == '>' && c1 == Some('&') {
            self.advance();
            self.advance();
            return op(start, self.i, ">&", self.src);
        }
        if c == '>' && c1 == Some('|') {
            self.advance();
            self.advance();
            return op(start, self.i, ">|", self.src);
        }
        if c == '&' && c1 == Some('>') && c2 == Some('>') {
            self.advance();
            self.advance();
            self.advance();
            return op(start, self.i, "&>>", self.src);
        }
        if c == '&' && c1 == Some('>') {
            self.advance();
            self.advance();
            return op(start, self.i, "&>", self.src);
        }
        if c == '<' && c1 == Some('<') && c2 == Some('<') {
            self.advance();
            self.advance();
            self.advance();
            return op(start, self.i, "<<<", self.src);
        }
        if c == '<' && c1 == Some('<') && c2 == Some('-') {
            self.advance();
            self.advance();
            self.advance();
            return op(start, self.i, "<<-", self.src);
        }
        if c == '<' && c1 == Some('<') {
            self.advance();
            self.advance();
            return op(start, self.i, "<<", self.src);
        }
        if c == '<' && c1 == Some('&') && c2 == Some('-') {
            self.advance();
            self.advance();
            self.advance();
            return op(start, self.i, "<&-", self.src);
        }
        if c == '<' && c1 == Some('&') {
            self.advance();
            self.advance();
            return op(start, self.i, "<&", self.src);
        }
        if c == '<' && c1 == Some('(') {
            self.advance();
            self.advance();
            return Token { kind: TokenKind::LtParen, value: "<(", start, end: self.i };
        }
        if c == '>' && c1 == Some('(') {
            self.advance();
            self.advance();
            return Token { kind: TokenKind::GtParen, value: ">(", start, end: self.i };
        }
        if c == '(' && c1 == Some('(') {
            self.advance();
            self.advance();
            return op(start, self.i, "((", self.src);
        }
        if c == ')' && c1 == Some(')') {
            self.advance();
            self.advance();
            return op(start, self.i, "))", self.src);
        }
        if c == '|' || c == '&' || c == ';' || c == '>' || c == '<' {
            self.advance();
            return op(start, self.i, &self.src[start..self.i], self.src);
        }
        if c == '(' || c == ')' {
            self.advance();
            return op(start, self.i, &self.src[start..self.i], self.src);
        }

        // cmd 位置：[ [[ { 开始 test/group；arg 位置它们是普通字符
        if ctx == Ctx::Cmd {
            if c == '[' && c1 == Some('[') {
                self.advance();
                self.advance();
                return op(start, self.i, "[[", self.src);
            }
            if c == '[' {
                self.advance();
                return op(start, self.i, "[", self.src);
            }
            if c == '{' && matches!(c1, Some(' ') | Some('\t') | Some('\n')) {
                self.advance();
                return op(start, self.i, "{", self.src);
            }
            if c == '}' {
                self.advance();
                return op(start, self.i, "}", self.src);
            }
            if c == '!' && matches!(c1, Some(' ') | Some('\t')) {
                self.advance();
                return op(start, self.i, "!", self.src);
            }
        }

        if c == '"' {
            self.advance();
            return Token { kind: TokenKind::DQuote, value: "\"", start, end: self.i };
        }
        if c == '\'' {
            let si = self.i;
            self.advance();
            while self.i < self.src.len() && self.peek() != Some('\'') {
                self.advance();
            }
            if self.i < self.src.len() {
                self.advance();
            }
            return Token { kind: TokenKind::SQuote, value: &self.src[si..self.i], start, end: self.i };
        }

        if c == '$' {
            if c1 == Some('(') && c2 == Some('(') {
                self.advance();
                self.advance();
                self.advance();
                return Token { kind: TokenKind::DollarDParen, value: "$((", start, end: self.i };
            }
            if c1 == Some('(') {
                self.advance();
                self.advance();
                return Token { kind: TokenKind::DollarParen, value: "$(", start, end: self.i };
            }
            if c1 == Some('{') {
                self.advance();
                self.advance();
                return Token { kind: TokenKind::DollarBrace, value: "${", start, end: self.i };
            }
            if c1 == Some('\'') {
                // ANSI-C string $'...'
                let si = self.i;
                self.advance();
                self.advance();
                while self.i < self.src.len() && self.peek() != Some('\'') {
                    if self.peek() == Some('\\') && self.peek_off(1).is_some() {
                        self.advance();
                    }
                    self.advance();
                }
                if self.i < self.src.len() {
                    self.advance();
                }
                return Token { kind: TokenKind::AnsiC, value: &self.src[si..self.i], start, end: self.i };
            }
            self.advance();
            return Token { kind: TokenKind::Dollar, value: "$", start, end: self.i };
        }

        if c == '`' {
            self.advance();
            return Token { kind: TokenKind::Backtick, value: "`", start, end: self.i };
        }

        // 重定向前面的文件描述符：数字后紧跟 > 或 <
        if is_digit(c) {
            let mut j = self.i;
            while j < self.src.len() && is_digit_byte(self.src.as_bytes()[j]) {
                j += 1;
            }
            let after = self.src.as_bytes().get(j).copied();
            if after == Some(b'>') || after == Some(b'<') {
                let si = self.i;
                while self.i < j {
                    self.advance();
                }
                return Token { kind: TokenKind::Word, value: &self.src[si..self.i], start, end: self.i };
            }
        }

        // 词 / 数字
        if is_word_start(c) || c == '{' || c == '}' {
            let si = self.i;
            while self.i < self.src.len() {
                let ch = match self.peek() {
                    Some(c) => c,
                    None => break,
                };
                if ch == '\\' {
                    if self.i + 1 >= self.src.len() {
                        // 行尾孤立 `\` —— tree-sitter 把它排除在词外
                        break;
                    }
                    // 转义下一字符（包括行连接）
                    self.advance();
                    self.advance();
                    continue;
                }
                if !is_word_char(ch) && ch != '{' && ch != '}' {
                    break;
                }
                self.advance();
            }
            if self.i > si {
                let v = &self.src[si..self.i];
                if is_signed_int_text(v) {
                    return Token { kind: TokenKind::Number, value: v, start, end: self.i };
                }
                return Token { kind: TokenKind::Word, value: v, start, end: self.i };
            }
            // 空词（行尾孤立 `\`）——落到下面的单字符消费
        }

        // 未知字符——单字符词
        let cs = &self.src[self.i..self.i + c.len_utf8()];
        self.advance();
        Token { kind: TokenKind::Word, value: cs, start, end: self.i }
    }
}

fn op<'a>(start: usize, end: usize, value: &'a str, _src: &'a str) -> Token<'a> {
    Token { kind: TokenKind::Op, value, start, end }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '_' | '/' | '.' | '-' | '+' | ':' | '@' | '%' | ',' | '~' | '^' | '?' | '*' | '!' | '=' | '[' | ']')
}

fn is_word_start(c: char) -> bool {
    is_word_char(c) || c == '\\'
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}

fn is_digit_byte(b: u8) -> bool {
    b.is_ascii_digit()
}

fn is_hex_digit(c: char) -> bool {
    c.is_ascii_hexdigit()
}

fn is_base_digit(c: char) -> bool {
    is_ident_char(c) || c == '@'
}

/// 非引号 heredoc 定界符字符
fn is_heredoc_delim_char(c: char) -> bool {
    !matches!(
        c,
        ' ' | '\t' | '\n' | '<' | '>' | '|' | '&' | ';' | '(' | ')' | '\'' | '"' | '`' | '\\'
    )
}

/// `/^-?\d+$/`
fn is_signed_int_text(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    !t.is_empty() && t.bytes().all(is_digit_byte)
}

/// `/^\d+$/`
fn is_digits_text(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_digit_byte)
}

/// `/^[ \t]+$/`（非空白跳过）
fn is_blank_text(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b == b' ' || b == b'\t')
}

/// `/^-?(0x)?[0-9]+#$/`（BASE# 前缀）
fn is_base_prefix_text(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    let rest = t.strip_prefix("0x").unwrap_or(t);
    rest.len() >= 2
        && rest.as_bytes()[rest.len() - 1] == b'#'
        && rest[..rest.len() - 1].bytes().all(is_digit_byte)
}

/// `/[*?]/`
fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// `/^[-+?*@!][a-zA-Z]/`
fn is_extglob_prefix_text(s: &str) -> bool {
    let mut chars = s.chars();
    let first = chars.next();
    let second = chars.next();
    matches!(first, Some('-' | '+' | '?' | '*' | '@' | '!')) && matches!(second, Some(c) if c.is_ascii_alphabetic())
}

/// `/[*?(]/`
fn has_glob_paren_text(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('(')
}

/// `/[*?+@!]\(/`
fn has_extglob_paren(s: &str) -> bool {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if matches!(b[i], b'*' | b'?' | b'+' | b'@' | b'!')
            && i + 1 < b.len()
            && b[i + 1] == b'('
        {
            return true;
        }
    }
    false
}

// ───────────────────────────── Parser ─────────────────────────────

struct ParseState<'a> {
    lex: Lexer<'a>,
    node_count: usize,
    deadline: Instant,
    aborted: bool,
    /// 反引号嵌套深度
    in_backtick: usize,
    /// parseSimpleCommand 的提前停止 token（`[` 回溯用）
    stop_token: Option<&'static str>,
}

impl<'a> ParseState<'a> {
    fn new(src: &'a str, timeout_ms: u64) -> Self {
        ParseState {
            lex: Lexer::new(src),
            node_count: 0,
            deadline: Instant::now() + Duration::from_millis(timeout_ms),
            aborted: false,
            in_backtick: 0,
            stop_token: None,
        }
    }

    fn src(&self) -> &'a str {
        self.lex.src
    }

    fn peek(&self) -> Option<char> {
        self.lex.peek()
    }

    fn peek1(&self) -> Option<char> {
        self.lex.peek_off(1)
    }

    fn peek2(&self) -> Option<char> {
        self.lex.peek_off(2)
    }

    /// TS 端 peek(L, off) 的通用形式
    fn peek_off(&self, off: usize) -> Option<char> {
        self.lex.peek_off(off)
    }

    fn advance(&mut self) {
        self.lex.advance();
    }

    fn save(&self) -> usize {
        self.lex.i
    }

    fn restore(&mut self, s: usize) {
        self.lex.i = s;
    }

    fn skip_blanks(&mut self) {
        self.lex.skip_blanks();
    }

    fn next_token(&mut self, ctx: Ctx) -> Token<'a> {
        self.lex.next_token(ctx)
    }

    fn check_budget(&mut self) -> PResult<()> {
        self.node_count += 1;
        if self.node_count > MAX_NODES {
            self.aborted = true;
            return Err(ParseErr::Budget);
        }
        if (self.node_count & 0x7f) == 0 && Instant::now() > self.deadline {
            self.aborted = true;
            return Err(ParseErr::Timeout);
        }
        Ok(())
    }

    fn mk(&mut self, ty: &str, start: usize, end: usize, children: Vec<TsNode>) -> PResult<TsNode> {
        self.check_budget()?;
        Ok(TsNode {
            node_type: ty.to_string(),
            text: self.src()[start..end].to_string(),
            start_index: start,
            end_index: end,
            children,
        })
    }

    fn mk_char(&mut self, c: char, start: usize, end: usize, children: Vec<TsNode>) -> PResult<TsNode> {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.mk(s, start, end, children)
    }

    fn leaf(&mut self, ty: &str, tok: &Token) -> PResult<TsNode> {
        self.mk(ty, tok.start, tok.end, vec![])
    }

    fn parse_program(&mut self) -> PResult<TsNode> {
        let mut children = Vec::new();
        self.skip_blanks();
        loop {
            let save = self.save();
            let t = self.next_token(Ctx::Cmd);
            if t.kind == TokenKind::Newline {
                self.skip_blanks();
                continue;
            }
            self.restore(save);
            break;
        }
        let prog_start = self.lex.i;
        while self.lex.i < self.src().len() {
            let save = self.save();
            let t = self.next_token(Ctx::Cmd);
            if t.kind == TokenKind::Eof {
                break;
            }
            if t.kind == TokenKind::Newline {
                continue;
            }
            if t.kind == TokenKind::Comment {
                children.push(self.leaf("comment", &t)?);
                continue;
            }
            self.restore(save);
            let stmts = self.parse_statements(None)?;
            let stmts_empty = stmts.is_empty();
            children.extend(stmts);
            if stmts_empty {
                // 无法解析——产出 ERROR 并跳过一个 token
                let err_tok = self.next_token(Ctx::Cmd);
                if err_tok.kind == TokenKind::Eof {
                    break;
                }
                // 顶层的孪生 `;;`（如 case 外的 `var=;;`）——tree-sitter 静默丢弃
                if err_tok.kind == TokenKind::Op && err_tok.value == ";;" && !children.is_empty() {
                    continue;
                }
                children.push(self.mk("ERROR", err_tok.start, err_tok.end, vec![])?);
            }
        }
        // tree-sitter 把行尾空白算进 program 范围
        let prog_end = if !children.is_empty() { self.src().len() } else { prog_start };
        self.mk("program", prog_start, prog_end, children)
    }

    /// 解析由 ; & 换行分隔的语句序列。`;` `&` 作为兄弟叶子（不包进 list，
    /// 只有 && || 才包）。遇到终止符或 EOF 停止。
    fn parse_statements(&mut self, terminator: Option<&'static str>) -> PResult<Vec<TsNode>> {
        let mut out = Vec::new();
        loop {
            self.skip_blanks();
            let save = self.save();
            let t = self.next_token(Ctx::Cmd);
            if t.kind == TokenKind::Eof {
                self.restore(save);
                break;
            }
            if t.kind == TokenKind::Newline {
                if !self.lex.heredocs.is_empty() {
                    self.scan_heredoc_bodies();
                }
                continue;
            }
            if t.kind == TokenKind::Comment {
                out.push(self.leaf("comment", &t)?);
                continue;
            }
            if let Some(term) = terminator {
                if t.kind == TokenKind::Op && t.value == term {
                    self.restore(save);
                    break;
                }
            }
            if t.kind == TokenKind::Op
                && matches!(t.value, ")" | "}" | ";;" | ";&" | ";;&" | "))" | "]]" | "]")
            {
                self.restore(save);
                break;
            }
            if t.kind == TokenKind::Backtick && self.in_backtick > 0 {
                self.restore(save);
                break;
            }
            if t.kind == TokenKind::Word
                && matches!(t.value, "then" | "elif" | "else" | "fi" | "do" | "done" | "esac")
            {
                self.restore(save);
                break;
            }
            self.restore(save);
            let stmt = self.parse_and_or()?;
            let Some(stmt) = stmt else { break };
            out.push(stmt);
            // 分隔符
            self.skip_blanks();
            let save2 = self.save();
            let sep = self.next_token(Ctx::Cmd);
            if sep.kind == TokenKind::Op && (sep.value == ";" || sep.value == "&") {
                out.push(self.leaf(sep.value, &sep)?);
            } else if sep.kind == TokenKind::Newline {
                if !self.lex.heredocs.is_empty() {
                    self.scan_heredoc_bodies();
                }
            } else {
                self.restore(save2);
            }
        }
        Ok(out)
    }

    /// 由 && || 连接的流水线链。左结合嵌套。
    /// tree-sitter 怪癖：末尾重定向会把整个 list 包进 redirected_statement。
    fn parse_and_or(&mut self) -> PResult<Option<TsNode>> {
        let first = self.parse_pipeline()?;
        let Some(mut left) = first else { return Ok(None) };
        loop {
            let save = self.save();
            let t = self.next_token(Ctx::Cmd);
            if t.kind == TokenKind::Op && (t.value == "&&" || t.value == "||") {
                let op = self.leaf(t.value, &t)?;
                self.skip_newlines();
                let right = self.parse_pipeline()?;
                let Some(right) = right else {
                    left = self.mk("list", left.start_index, op.end_index, vec![left, op])?;
                    break;
                };
                // 若右侧是 redirected_statement，把重定向提升到包住 list
                if right.node_type == "redirected_statement" && right.children.len() >= 2 {
                    let mut redirs = right.children;
                    let inner = redirs.remove(0);
                    let mut list_kids = vec![left, op];
                    list_kids.push(inner);
                    let list_node = self.mk("list", list_kids[0].start_index, list_kids[2].end_index, list_kids)?;
                    let last_end = redirs[redirs.len() - 1].end_index;
                    let mut kids = vec![list_node];
                    kids.extend(redirs);
                    left = self.mk("redirected_statement", kids[0].start_index, last_end, kids)?;
                } else {
                    left = self.mk(
                        "list",
                        left.start_index,
                        right.end_index,
                        vec![left, op, right],
                    )?;
                }
            } else {
                self.restore(save);
                break;
            }
        }
        Ok(Some(left))
    }

    fn skip_newlines(&mut self) {
        loop {
            let save = self.save();
            let t = self.next_token(Ctx::Cmd);
            if t.kind != TokenKind::Newline {
                self.restore(save);
                break;
            }
        }
    }

    /// 由 | 或 |& 连接的命令。算符为扁平叶子。
    fn parse_pipeline(&mut self) -> PResult<Option<TsNode>> {
        let first = self.parse_command()?;
        let Some(first) = first else { return Ok(None) };
        let mut parts = vec![first];
        loop {
            let save = self.save();
            let t = self.next_token(Ctx::Cmd);
            if t.kind == TokenKind::Op && (t.value == "|" || t.value == "|&") {
                let op = self.leaf(t.value, &t)?;
                self.skip_newlines();
                let next = self.parse_command()?;
                let Some(next) = next else {
                    parts.push(op);
                    break;
                };
                // 提升右侧的重定向以包住当前流水线片段
                if next.node_type == "redirected_statement" && next.children.len() >= 2 {
                    let mut redirs = next.children;
                    let inner = redirs.remove(0);
                    let inner_end = inner.end_index;
                    parts.push(op);
                    parts.push(inner);
                    let pipe_node =
                        self.mk("pipeline", parts[0].start_index, inner_end, parts.clone())?;
                    let last_end = redirs[redirs.len() - 1].end_index;
                    let mut kids = vec![pipe_node];
                    kids.extend(redirs);
                    let wrapped =
                        self.mk("redirected_statement", kids[0].start_index, last_end, kids)?;
                    parts = vec![wrapped];
                    continue;
                }
                parts.push(op);
                parts.push(next);
            } else {
                self.restore(save);
                break;
            }
        }
        if parts.len() == 1 {
            return Ok(Some(parts.swap_remove(0)));
        }
        let start = parts[0].start_index;
        let end = parts[parts.len() - 1].end_index;
        Ok(Some(self.mk("pipeline", start, end, parts)?))
    }

    /// 解析单个命令：simple / compound / 控制结构。
    fn parse_command(&mut self) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        let save = self.save();
        let t = self.next_token(Ctx::Cmd);

        if t.kind == TokenKind::Eof {
            self.restore(save);
            return Ok(None);
        }

        // 取反 —— tree-sitter 只包住命令本身，重定向留给外层
        if t.kind == TokenKind::Op && t.value == "!" {
            let bang = self.leaf("!", &t)?;
            let inner = self.parse_command()?;
            let Some(inner) = inner else {
                self.restore(save);
                return Ok(None);
            };
            if inner.node_type == "redirected_statement" && inner.children.len() >= 2 {
                let mut redirs = inner.children;
                let cmd = redirs.remove(0);
                let neg =
                    self.mk("negated_command", bang.start_index, cmd.end_index, vec![bang, cmd])?;
                let last_end = redirs[redirs.len() - 1].end_index;
                let mut kids = vec![neg];
                kids.extend(redirs);
                return Ok(Some(
                    self.mk("redirected_statement", kids[0].start_index, last_end, kids)?,
                ));
            }
            return Ok(Some(
                self.mk("negated_command", bang.start_index, inner.end_index, vec![bang, inner])?,
            ));
        }

        if t.kind == TokenKind::Op && t.value == "(" {
            let open = self.leaf("(", &t)?;
            let body = self.parse_statements(Some(")"))?;
            let close_tok = self.next_token(Ctx::Cmd);
            let close = if close_tok.kind == TokenKind::Op && close_tok.value == ")" {
                self.leaf(")", &close_tok)?
            } else {
                self.mk(")", open.end_index, open.end_index, vec![])?
            };
            let open_start = open.start_index;
            let close_end = close.end_index;
            let mut kids = vec![open];
            kids.extend(body);
            kids.push(close);
            let node = self.mk("subshell", open_start, close_end, kids)?;
            return self.maybe_redirect(node, false).map(Some);
        }

        if t.kind == TokenKind::Op && t.value == "((" {
            let open = self.leaf("((", &t)?;
            let exprs = self.parse_arith_comma_list("))", ArithMode::Var)?;
            let close_tok = self.next_token(Ctx::Cmd);
            let close = if close_tok.value == "))" {
                self.leaf("))", &close_tok)?
            } else {
                self.mk("))", open.end_index, open.end_index, vec![])?
            };
            let open_start = open.start_index;
            let close_end = close.end_index;
            let mut kids = vec![open];
            kids.extend(exprs);
            kids.push(close);
            return Ok(Some(self.mk("compound_statement", open_start, close_end, kids)?));
        }

        if t.kind == TokenKind::Op && t.value == "{" {
            let open = self.leaf("{", &t)?;
            let body = self.parse_statements(Some("}"))?;
            let close_tok = self.next_token(Ctx::Cmd);
            let close = if close_tok.kind == TokenKind::Op && close_tok.value == "}" {
                self.leaf("}", &close_tok)?
            } else {
                self.mk("}", open.end_index, open.end_index, vec![])?
            };
            let open_start = open.start_index;
            let close_end = close.end_index;
            let mut kids = vec![open];
            kids.extend(body);
            kids.push(close);
            let node = self.mk("compound_statement", open_start, close_end, kids)?;
            return self.maybe_redirect(node, false).map(Some);
        }

        if t.kind == TokenKind::Op && (t.value == "[" || t.value == "[[") {
            let open = self.leaf(t.value, &t)?;
            let closer = if t.value == "[" { "]" } else { "]]" };
            // 语法允许 `[` 内是 _expression 或 redirected_statement：先按表达式解析，
            // 解析不到 `]` 就回溯按 redirected_statement 解析。
            let expr_save = self.save();
            let mut expr = self.parse_test_expr(closer)?;
            self.skip_blanks();
            if t.value == "[" && self.peek() != Some(']') {
                self.restore(expr_save);
                let prev_stop = self.stop_token;
                self.stop_token = Some("]");
                let rstmt = self.parse_command()?;
                self.stop_token = prev_stop;
                if let Some(r) = &rstmt {
                    if r.node_type == "redirected_statement" {
                        expr = rstmt.clone();
                    } else {
                        self.restore(expr_save);
                        expr = self.parse_test_expr(closer)?;
                    }
                } else {
                    self.restore(expr_save);
                    expr = self.parse_test_expr(closer)?;
                }
                self.skip_blanks();
            }
            let close_tok = self.next_token(Ctx::Arg);
            let open_end = open.end_index;
            let close = if close_tok.value == closer {
                self.leaf(closer, &close_tok)?
            } else {
                self.mk(closer, open_end, open_end, vec![])?
            };
            let mut kids = vec![open];
            if let Some(e) = expr {
                kids.push(e);
            }
            kids.push(close);
            return Ok(Some(self.mk(
                "test_command",
                kids[0].start_index,
                kids[kids.len() - 1].end_index,
                kids,
            )?));
        }

        if t.kind == TokenKind::Word {
            match t.value {
                "if" => {
                    let node = self.parse_if(&t)?;
                    return self.maybe_redirect(node, true).map(Some);
                }
                "while" | "until" => {
                    let node = self.parse_while(&t)?;
                    return self.maybe_redirect(node, true).map(Some);
                }
                "for" | "select" => {
                    let node = self.parse_for(&t)?;
                    return self.maybe_redirect(node, true).map(Some);
                }
                "case" => {
                    let node = self.parse_case(&t)?;
                    return self.maybe_redirect(node, true).map(Some);
                }
                "function" => return Ok(Some(self.parse_function(&t)?)),
                _ if is_decl_keyword(t.value) => {
                    let node = self.parse_declaration(&t)?;
                    return self.maybe_redirect(node, false).map(Some);
                }
                "unset" | "unsetenv" => {
                    let node = self.parse_unset(&t)?;
                    return self.maybe_redirect(node, false).map(Some);
                }
                _ => {}
            }
        }

        self.restore(save);
        self.parse_simple_command()
    }

    /// 解析 simple command：[赋值]* 词 [参数|重定向]*
    /// 只有单个赋值且无命令时返回 variable_assignment。
    fn parse_simple_command(&mut self) -> PResult<Option<TsNode>> {
        let start = self.lex.i;
        let mut assignments = Vec::new();
        let mut pre_redirects = Vec::new();

        loop {
            self.skip_blanks();
            if let Some(a) = self.try_parse_assignment()? {
                assignments.push(a);
                continue;
            }
            if let Some(r) = self.try_parse_redirect(false)? {
                pre_redirects.push(r);
                continue;
            }
            break;
        }

        self.skip_blanks();
        let save = self.save();
        let name_tok = self.next_token(Ctx::Cmd);
        let no_command = matches!(
            name_tok.kind,
            TokenKind::Eof | TokenKind::Newline | TokenKind::Comment
        ) || (name_tok.kind == TokenKind::Op
            && name_tok.value != "{"
            && name_tok.value != "["
            && name_tok.value != "[[")
            || (name_tok.kind == TokenKind::Word
                && is_shell_keyword(name_tok.value)
                && name_tok.value != "in");
        if no_command {
            self.restore(save);
            // 无命令——孤立赋值或重定向
            if assignments.len() == 1 && pre_redirects.is_empty() {
                return Ok(Some(assignments.swap_remove(0)));
            }
            if !pre_redirects.is_empty() && assignments.is_empty() {
                // 裸重定向 → 只含 file_redirect 的 redirected_statement
                let rs = pre_redirects[0].start_index;
                let re = pre_redirects[pre_redirects.len() - 1].end_index;
                return Ok(Some(self.mk("redirected_statement", rs, re, pre_redirects)?));
            }
            if assignments.len() > 1 && pre_redirects.is_empty() {
                // `A=1 B=2` 无命令 → variable_assignments（复数）
                let as_ = assignments[0].start_index;
                let ae = assignments[assignments.len() - 1].end_index;
                return Ok(Some(self.mk("variable_assignments", as_, ae, assignments)?));
            }
            if !assignments.is_empty() || !pre_redirects.is_empty() {
                let mut all = assignments;
                all.extend(pre_redirects);
                let end = all[all.len() - 1].end_index;
                return Ok(Some(self.mk("command", start, end, all)?));
            }
            return Ok(None);
        }
        self.restore(save);

        // 函数定义：name() { ... }
        let fn_save = self.save();
        let nm = self.parse_word(Ctx::Cmd)?;
        let nm_is_word = matches!(&nm, Some(n) if n.node_type == "word");
        if nm_is_word {
            self.skip_blanks();
            if self.peek() == Some('(') && self.peek1() == Some(')') {
                let o_tok = self.next_token(Ctx::Cmd);
                let c_tok = self.next_token(Ctx::Cmd);
                let o_paren = self.leaf("(", &o_tok)?;
                let c_paren = self.leaf(")", &c_tok)?;
                self.skip_blanks();
                self.skip_newlines();
                let body = self.parse_command()?;
                if let Some(body) = body {
                    // redirected_statement(compound_statement, file_redirect...) 时
                    // 把重定向提升到 function_definition 层（tree-sitter 语法）
                    let mut body_kids = vec![body.clone()];
                    if body.node_type == "redirected_statement"
                        && body.children.len() >= 2
                        && body.children[0].node_type == "compound_statement"
                    {
                        body_kids = body.children.clone();
                    }
                    let nm_start = nm.as_ref().map(|n| n.start_index).unwrap_or(o_paren.start_index);
                    let last_end = body_kids[body_kids.len() - 1].end_index;
                    let mut kids = Vec::with_capacity(body_kids.len() + 3);
                    if let Some(nm) = nm {
                        kids.push(nm);
                    }
                    kids.push(o_paren);
                    kids.push(c_paren);
                    kids.extend(body_kids);
                    return Ok(Some(self.mk("function_definition", nm_start, last_end, kids)?));
                }
            }
        }
        self.restore(fn_save);

        let name_arg = self.parse_word(Ctx::Cmd)?;
        let name_arg = match name_arg {
            Some(n) => n,
            None => {
                if assignments.len() == 1 {
                    return Ok(Some(assignments.swap_remove(0)));
                }
                return Ok(None);
            }
        };

        let name_start = name_arg.start_index;
        let name_end = name_arg.end_index;
        let cmd_name = self.mk("command_name", name_start, name_end, vec![name_arg])?;

        let mut args = Vec::new();
        let mut redirects = Vec::new();
        let mut heredoc_redirect = None;

        loop {
            self.skip_blanks();
            // 命令后重定向是贪心的（repeat1 $._literal）——出现一个重定向后，
            // 后续字面量按语法的 prec.left 归入重定向。
            let r = self.try_parse_redirect(true)?;
            if let Some(r) = r {
                match r.node_type.as_str() {
                    "heredoc_redirect" => heredoc_redirect = Some(r),
                    "herestring_redirect" => args.push(r),
                    _ => redirects.push(r),
                }
                continue;
            }
            // 出现过 file_redirect 后，命令参数结束
            if !redirects.is_empty() {
                break;
            }
            // `[` 回溯——遇到 `]` 停止，让外层处理
            if self.stop_token == Some("]") && self.peek() == Some(']') {
                break;
            }
            let save2 = self.save();
            let pk = self.next_token(Ctx::Arg);
            if pk.kind == TokenKind::Eof
                || pk.kind == TokenKind::Newline
                || pk.kind == TokenKind::Comment
                || (pk.kind == TokenKind::Op
                    && matches!(
                        pk.value,
                        "|" | "|&" | "&&" | "||" | ";" | ";;" | ";&" | ";;&" | "&" | ")" | "}" | "))"
                    ))
            {
                self.restore(save2);
                break;
            }
            self.restore(save2);
            let arg = self.parse_word(Ctx::Arg)?;
            let Some(arg) = arg else {
                // 孤立 `(` 在参数位置——tree-sitter 解析为 subshell 参数
                if self.peek() == Some('(') {
                    let o_tok = self.next_token(Ctx::Cmd);
                    let open = self.leaf("(", &o_tok)?;
                    let body = self.parse_statements(Some(")"))?;
                    let c_tok = self.next_token(Ctx::Cmd);
                    let close = if c_tok.kind == TokenKind::Op && c_tok.value == ")" {
                        self.leaf(")", &c_tok)?
                    } else {
                        self.mk(")", open.end_index, open.end_index, vec![])?
                    };
                    let open_start = open.start_index;
                    let close_end = close.end_index;
                    let mut kids = vec![open];
                    kids.extend(body);
                    kids.push(close);
                    args.push(self.mk("subshell", open_start, close_end, kids)?);
                    continue;
                }
                break;
            };
            // 孤立 `=` 是 bash 语法错误——tree-sitter 用 ERROR 包裹
            if arg.node_type == "word" && arg.text == "=" {
                let s = arg.start_index;
                let e = arg.end_index;
                args.push(self.mk("ERROR", s, e, vec![arg])?);
                continue;
            }
            // 词后紧跟 ``(``（无空白）是语法错误——tree-sitter 用 ERROR 包裹
            if (arg.node_type == "word" || arg.node_type == "concatenation")
                && self.peek() == Some('(')
                && self.lex.i == arg.end_index
            {
                let s = arg.start_index;
                let e = arg.end_index;
                args.push(self.mk("ERROR", s, e, vec![arg])?);
                continue;
            }
            args.push(arg);
        }

        // preRedirects（如 `2>&1 cat`）按语法放在 command 内、command_name 前。
        // 注意：TS 端引用共享使同一节点同时出现在 command 与 redirected_statement
        // 中；Rust 端克隆一份进 cmd_children，原件留给 heredoc 分支的 allR。
        let mut cmd_children = assignments;
        cmd_children.extend(pre_redirects.iter().cloned());
        cmd_children.push(cmd_name);
        cmd_children.extend(args);
        let cmd_start = cmd_children[0].start_index;
        let cmd_end = cmd_children[cmd_children.len() - 1].end_index;
        let cmd = self.mk("command", cmd_start, cmd_end, cmd_children)?;

        if let Some(mut hr) = heredoc_redirect {
            // 现在扫描 heredoc 体
            self.scan_heredoc_bodies();
            let hd = self.lex.heredocs.pop_front();
            if let Some(hd) = hd {
                if hr.children.len() >= 2 {
                    let content = if hd.quoted {
                        Vec::new()
                    } else {
                        self.parse_heredoc_body_content(hd.body_start, hd.body_end)?
                    };
                    let body_node = self.mk("heredoc_body", hd.body_start, hd.body_end, content)?;
                    let end_node = self.mk("heredoc_end", hd.end_start, hd.end_end, vec![])?;
                    hr.children.push(body_node);
                    hr.children.push(end_node);
                    hr.end_index = hd.end_end;
                    hr.text = self.src()[hr.start_index..hd.end_end].to_string();
                }
            }
            let hr_end = hr.end_index;
            let mut all_r = pre_redirects;
            all_r.push(hr);
            all_r.extend(redirects);
            // rStart：preRedirects 存在时取 min(cmd.startIndex, preRedirects[0].startIndex)
            let r_start = match all_r.first() {
                Some(first) => first.start_index.min(cmd.start_index),
                None => cmd.start_index,
            };
            let mut kids = vec![cmd];
            kids.extend(all_r);
            return Ok(Some(self.mk("redirected_statement", r_start, hr_end, kids)?));
        }

        if !redirects.is_empty() {
            let re = redirects[redirects.len() - 1].end_index;
            let rs = cmd.start_index;
            let mut kids = vec![cmd];
            kids.extend(redirects);
            return Ok(Some(self.mk("redirected_statement", rs, re, kids)?));
        }

        Ok(Some(cmd))
    }

    fn maybe_redirect(&mut self, node: TsNode, allow_herestring: bool) -> PResult<TsNode> {
        let mut redirects = Vec::new();
        loop {
            self.skip_blanks();
            let save = self.save();
            let r = self.try_parse_redirect(false)?;
            let Some(r) = r else { break };
            if r.node_type == "herestring_redirect" && !allow_herestring {
                self.restore(save);
                break;
            }
            redirects.push(r);
        }
        if redirects.is_empty() {
            return Ok(node);
        }
        let re = redirects[redirects.len() - 1].end_index;
        let rs = node.start_index;
        let mut kids = vec![node];
        kids.extend(redirects);
        self.mk("redirected_statement", rs, re, kids)
    }

    fn try_parse_assignment(&mut self) -> PResult<Option<TsNode>> {
        let save = self.save();
        self.skip_blanks();
        let start_b = self.lex.i;
        // 必须以标识符开头
        if !matches!(self.peek(), Some(c) if is_ident_start(c)) {
            self.restore(save);
            return Ok(None);
        }
        while matches!(self.peek(), Some(c) if is_ident_char(c)) {
            self.advance();
        }
        let name_end = self.lex.i;
        // 可选下标
        let mut sub_end = name_end;
        if self.peek() == Some('[') {
            self.advance();
            let mut depth = 1;
            while depth > 0 {
                let Some(c) = self.peek() else { break };
                if c == '[' {
                    depth += 1;
                } else if c == ']' {
                    depth -= 1;
                }
                self.advance();
            }
            sub_end = self.lex.i;
        }
        let c = self.peek();
        let c1 = self.peek1();
        let op: &'static str;
        if c == Some('=') && c1 != Some('=') {
            op = "=";
        } else if c == Some('+') && c1 == Some('=') {
            op = "+=";
        } else {
            self.restore(save);
            return Ok(None);
        }
        let name_node = self.mk("variable_name", start_b, name_end, vec![])?;
        let mut lhs = name_node;
        if sub_end > name_end {
            let br_open = self.mk("[", name_end, name_end + 1, vec![])?;
            let idx = self.parse_subscript_index(name_end + 1, sub_end - 1)?;
            let br_close = self.mk("]", sub_end - 1, sub_end, vec![])?;
            let ln = lhs.start_index;
            lhs = self.mk("subscript", ln, sub_end, vec![lhs, br_open, idx, br_close])?;
        }
        let op_start = self.lex.i;
        self.advance();
        if op == "+=" {
            self.advance();
        }
        let op_end = self.lex.i;
        let op_node = self.mk(op, op_start, op_end, vec![])?;
        let mut val = None;
        if self.peek() == Some('(') {
            // 数组
            let ao_tok = self.next_token(Ctx::Cmd);
            let a_open = self.leaf("(", &ao_tok)?;
            let mut elems = vec![a_open.clone()];
            loop {
                self.skip_blanks();
                if self.peek() == Some(')') {
                    break;
                }
                let e = self.parse_word(Ctx::Arg)?;
                let Some(e) = e else { break };
                elems.push(e);
            }
            let ac_tok = self.next_token(Ctx::Cmd);
            let a_close = if ac_tok.value == ")" {
                self.leaf(")", &ac_tok)?
            } else {
                self.mk(")", a_open.end_index, a_open.end_index, vec![])?
            };
            let ao_start = a_open.start_index;
            let ac_end = a_close.end_index;
            elems.push(a_close);
            val = Some(self.mk("array", ao_start, ac_end, elems)?);
        } else {
            let c2 = self.peek();
            if !matches!(c2, None | Some(' ') | Some('\t') | Some('\n') | Some(';') | Some('&') | Some('|') | Some(')') | Some('}')) {
                val = self.parse_word(Ctx::Arg)?;
            }
        }
        let end = match &val {
            Some(v) => v.end_index,
            None => op_end,
        };
        let mut kids = vec![lhs, op_node];
        if let Some(v) = val {
            kids.push(v);
        }
        Ok(Some(self.mk("variable_assignment", start_b, end, kids)?))
    }

    /// 下标内容按 tree-sitter 语法做算术解析
    fn parse_subscript_index_inline(&mut self) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        let c = self.peek();
        // @ 或 * 单独出现 → word
        if matches!(c, Some('@') | Some('*')) && self.peek1() == Some(']') {
            let s = self.lex.i;
            self.advance();
            return Ok(Some(self.mk("word", s, self.lex.i, vec![])?));
        }
        // ((expr)) → compound_statement
        if c == Some('(') && self.peek1() == Some('(') {
            let o_start = self.lex.i;
            self.advance();
            self.advance();
            let open = self.mk("((", o_start, self.lex.i, vec![])?;
            let inner = self.parse_arith_expr("))", ArithMode::Var)?;
            self.skip_blanks();
            let close = if self.peek() == Some(')') && self.peek1() == Some(')') {
                let cs = self.lex.i;
                self.advance();
                self.advance();
                self.mk("))", cs, self.lex.i, vec![])?
            } else {
                self.mk("))", self.lex.i, self.lex.i, vec![])?
            };
            let open_start = open.start_index;
            let close_end = close.end_index;
            let mut kids = vec![open];
            if let Some(inner) = inner {
                kids.push(inner);
            }
            kids.push(close);
            return Ok(Some(self.mk("compound_statement", open_start, close_end, kids)?));
        }
        // 算术——下标中的裸标识符用 'word' 模式
        Ok(self.parse_arith_expr("]", ArithMode::Word)?)
    }

    /// 字节区间的下标解析（为预扫描调用方保留）
    fn parse_subscript_index(&mut self, start_b: usize, end_b: usize) -> PResult<TsNode> {
        let text = self.src()[start_b..end_b].to_string();
        if is_digits_text(&text) {
            return self.mk("number", start_b, end_b, vec![]);
        }
        if let Some(name) = text.strip_prefix('$') {
            let body = &name;
            let valid = !body.is_empty()
                && body.chars().next().map(is_ident_start).unwrap_or(false)
                && body.chars().skip(1).all(is_ident_char);
            if valid {
                let dollar = self.mk("$", start_b, start_b + 1, vec![])?;
                let vn = self.mk("variable_name", start_b + 1, end_b, vec![])?;
                return self.mk("simple_expansion", start_b, end_b, vec![dollar, vn]);
            }
        }
        if text.len() == 2 {
            let mut chars = text.chars();
            if chars.next() == Some('$') {
                if let Some(sc) = chars.next() {
                    if is_special_var(sc) {
                        let dollar = self.mk("$", start_b, start_b + 1, vec![])?;
                        let vn = self.mk("special_variable_name", start_b + 1, end_b, vec![])?;
                        return self.mk("simple_expansion", start_b, end_b, vec![dollar, vn]);
                    }
                }
            }
        }
        self.mk("word", start_b, end_b, vec![])
    }

    /// 当前位置能否开始重定向目标字面量
    fn is_redirect_literal_start(&self) -> bool {
        let c = match self.peek() {
            Some(c) => c,
            None => return false,
        };
        if c == '\n' {
            return false;
        }
        if matches!(c, '|' | '&' | ';' | '(' | ')') {
            return false;
        }
        if c == '<' || c == '>' {
            // <( >( 是进程替换——那些是字面量
            return self.peek1() == Some('(');
        }
        // N< N> 文件描述符前缀——开始一个新重定向
        if is_digit(c) {
            let bytes = self.src().as_bytes();
            let mut j = self.lex.i;
            while j < bytes.len() && is_digit_byte(bytes[j]) {
                j += 1;
            }
            let after = bytes.get(j).copied();
            if after == Some(b'>') || after == Some(b'<') {
                return false;
            }
        }
        if c == '}' {
            return false;
        }
        if self.stop_token == Some("]") && c == ']' {
            return false;
        }
        true
    }

    /// 解析重定向算符 + 目标。greedy=true 时 file_redirect 按 repeat1 消费字面量。
    fn try_parse_redirect(&mut self, greedy: bool) -> PResult<Option<TsNode>> {
        let save = self.save();
        self.skip_blanks();
        // 文件描述符前缀？
        let mut fd = None;
        if matches!(self.peek(), Some(c) if is_digit(c)) {
            let start_b = self.lex.i;
            let bytes = self.src().as_bytes();
            let mut j = self.lex.i;
            while j < bytes.len() && is_digit_byte(bytes[j]) {
                j += 1;
            }
            let after = bytes.get(j).copied();
            if after == Some(b'>') || after == Some(b'<') {
                while self.lex.i < j {
                    self.advance();
                }
                fd = Some(self.mk("file_descriptor", start_b, self.lex.i, vec![])?);
            }
        }
        let t = self.next_token(Ctx::Arg);
        if t.kind != TokenKind::Op {
            self.restore(save);
            return Ok(None);
        }
        let v = t.value;
        if v == "<<<" {
            let op = self.leaf("<<<", &t)?;
            self.skip_blanks();
            let target = self.parse_word(Ctx::Arg)?;
            let end = target.as_ref().map(|x| x.end_index).unwrap_or(op.end_index);
            let start_idx;
            let mut kids = Vec::new();
            if let Some(fd) = fd {
                start_idx = fd.start_index;
                kids.push(fd);
            } else {
                start_idx = op.start_index;
            }
            kids.push(op);
            if let Some(target) = target {
                kids.push(target);
            }
            return Ok(Some(self.mk("herestring_redirect", start_idx, end, kids)?));
        }
        if v == "<<" || v == "<<-" {
            let op = self.leaf(v, &t)?;
            // heredoc 起始——定界符词（可能是引号形式）
            self.skip_blanks();
            let d_start = self.lex.i;
            let mut quoted = false;
            let mut delim = String::new();
            let dc = self.peek();
            if dc == Some('\'') || dc == Some('"') {
                quoted = true;
                self.advance();
                while self.lex.i < self.src().len() {
                    let ch = match self.peek() {
                        Some(ch) => ch,
                        None => break,
                    };
                    if Some(ch) == dc {
                        break;
                    }
                    delim.push(ch);
                    self.advance();
                }
                if self.lex.i < self.src().len() {
                    self.advance();
                }
            } else if dc == Some('\\') {
                // 反斜杠转义定界符：\X —— 只转义一个字符，体是字面量
                quoted = true;
                self.advance();
                if self.lex.i < self.src().len() && self.peek() != Some('\n') {
                    if let Some(ch) = self.peek() {
                        delim.push(ch);
                    }
                    self.advance();
                }
                // 后面可能还有标识符字符（如 <<\EOF → EOF）
                while let Some(ch) = self.peek() {
                    if !is_ident_char(ch) {
                        break;
                    }
                    delim.push(ch);
                    self.advance();
                }
            } else {
                // 非引号定界符：bash 接受大多数非元字符
                while let Some(ch) = self.peek() {
                    if !is_heredoc_delim_char(ch) {
                        break;
                    }
                    delim.push(ch);
                    self.advance();
                }
            }
            let d_end = self.lex.i;
            let start_node = self.mk("heredoc_start", d_start, d_end, vec![])?;
            // 登记待处理 heredoc——体在下个换行处扫描
            self.lex.heredocs.push_back(HeredocPending {
                delim,
                strip_tabs: v == "<<-",
                quoted,
                body_start: 0,
                body_end: 0,
                end_start: 0,
                end_end: 0,
            });
            let op_start = op.start_index;
            let mut kids = vec![op, start_node];
            let start_idx;
            if let Some(fd) = fd {
                start_idx = fd.start_index;
                let mut k = vec![fd];
                k.extend(kids);
                kids = k;
            } else {
                start_idx = op_start;
            }
            // tree-sitter 把 heredoc_start 与换行之间的流水线/list/file_redirect
            // 嵌套为 heredoc_redirect 的子节点——必须正确解析，不能静默丢弃
            loop {
                self.skip_blanks();
                let tc = match self.peek() {
                    Some(c) => c,
                    None => break,
                };
                if tc == '\n' {
                    break;
                }
                // 定界符后的文件重定向：cat <<EOF > out.txt
                if tc == '>' || tc == '<' || is_digit(tc) {
                    let r_save = self.save();
                    let r = self.try_parse_redirect(false)?;
                    if let Some(r) = &r {
                        if r.node_type == "file_redirect" {
                            kids.push(r.clone());
                            continue;
                        }
                    }
                    self.restore(r_save);
                }
                // 定界符后的流水线：one <<EOF | grep two
                if tc == '|' && self.peek1() != Some('|') {
                    self.advance();
                    self.skip_blanks();
                    let mut pipe_cmds = Vec::new();
                    loop {
                        let cmd = self.parse_command()?;
                        let Some(cmd) = cmd else { break };
                        pipe_cmds.push(cmd);
                        self.skip_blanks();
                        if self.peek() == Some('|') && self.peek1() != Some('|') {
                            let ps = self.lex.i;
                            self.advance();
                            pipe_cmds.push(self.mk("|", ps, self.lex.i, vec![])?);
                            self.skip_blanks();
                            continue;
                        }
                        break;
                    }
                    if !pipe_cmds.is_empty() {
                        let p_start = pipe_cmds[0].start_index;
                        let p_end = pipe_cmds[pipe_cmds.len() - 1].end_index;
                        kids.push(self.mk("pipeline", p_start, p_end, pipe_cmds)?);
                    }
                    continue;
                }
                // && / || after heredoc_start：只嵌套右侧命令
                if (tc == '&' && self.peek1() == Some('&')) || (tc == '|' && self.peek1() == Some('|')) {
                    self.advance();
                    self.advance();
                    self.skip_blanks();
                    let rhs = self.parse_command()?;
                    if let Some(rhs) = rhs {
                        kids.push(rhs);
                    }
                    continue;
                }
                // 终止符 / 未处理元字符 —— 行尾消费为 ERROR
                if matches!(tc, '&' | ';' | '(' | ')') {
                    let e_start = self.lex.i;
                    while self.lex.i < self.src().len() && self.peek() != Some('\n') {
                        self.advance();
                    }
                    kids.push(self.mk("ERROR", e_start, self.lex.i, vec![])?);
                    break;
                }
                // 行尾词参数：newins <<-EOF - org.freedesktop.service
                let w = self.parse_word(Ctx::Arg)?;
                if let Some(w) = w {
                    kids.push(w);
                    continue;
                }
                let e_start = self.lex.i;
                while self.lex.i < self.src().len() && self.peek() != Some('\n') {
                    self.advance();
                }
                if self.lex.i > e_start {
                    kids.push(self.mk("ERROR", e_start, self.lex.i, vec![])?);
                }
                break;
            }
            return Ok(Some(self.mk("heredoc_redirect", start_idx, self.lex.i, kids)?));
        }
        // 关闭 fd 变体：<&- >&- 目标可选（0 或 1 个）
        if v == "<&-" || v == ">&-" {
            let op = self.leaf(v, &t)?;
            let mut kids = Vec::new();
            let start_idx;
            if let Some(fd) = fd {
                start_idx = fd.start_index;
                kids.push(fd);
            } else {
                start_idx = op.start_index;
            }
            let op_end = op.end_index;
            kids.push(op);
            self.skip_blanks();
            let d_save = self.lex.i;
            let dest = if self.is_redirect_literal_start() {
                self.parse_word(Ctx::Arg)?
            } else {
                None
            };
            let end = match &dest {
                Some(d) => d.end_index,
                None => {
                    self.restore(d_save);
                    op_end
                }
            };
            if let Some(dest) = dest {
                kids.push(dest);
            }
            return Ok(Some(self.mk("file_redirect", start_idx, end, kids)?));
        }
        if matches!(v, ">" | ">>" | ">&" | ">|" | "&>" | "&>>" | "<" | "<&") {
            let op = self.leaf(v, &t)?;
            let mut kids = Vec::new();
            if let Some(fd) = fd {
                kids.push(fd);
            }
            kids.push(op.clone());
            // 目标是 repeat1($._literal)——贪心消费字面量直到非字面量
            let mut end = op.end_index;
            let mut taken = 0;
            loop {
                self.skip_blanks();
                if !self.is_redirect_literal_start() {
                    break;
                }
                if !greedy && taken >= 1 {
                    break;
                }
                let tc = self.peek();
                let tc1 = self.peek1();
                let mut target = None;
                if matches!(tc, Some('<') | Some('>')) && tc1 == Some('(') {
                    target = self.parse_process_sub()?;
                } else {
                    target = self.parse_word(Ctx::Arg)?;
                }
                let Some(target) = target else { break };
                end = target.end_index;
                kids.push(target);
                taken += 1;
            }
            let start_idx = kids[0].start_index;
            return Ok(Some(self.mk("file_redirect", start_idx, end, kids)?));
        }
        self.restore(save);
        Ok(None)
    }

    fn parse_process_sub(&mut self) -> PResult<Option<TsNode>> {
        let Some(c) = self.peek() else { return Ok(None) };
        if !matches!(c, '<' | '>') || self.peek1() != Some('(') {
            return Ok(None);
        }
        let start = self.lex.i;
        self.advance();
        self.advance();
        let open_ty = format!("{}(", c);
        let open = self.mk(&open_ty, start, self.lex.i, vec![])?;
        let body = self.parse_statements(Some(")"))?;
        self.skip_blanks();
        let close = if self.peek() == Some(')') {
            let cs = self.lex.i;
            self.advance();
            self.mk(")", cs, self.lex.i, vec![])?
        } else {
            self.mk(")", self.lex.i, self.lex.i, vec![])?
        };
        let open_start = open.start_index;
        let close_end = close.end_index;
        let mut kids = vec![open];
        kids.extend(body);
        kids.push(close);
        Ok(Some(self.mk("process_substitution", open_start, close_end, kids)?))
    }

    fn scan_heredoc_bodies(&mut self) {
        // 跳到换行
        let bytes = self.src().as_bytes();
        while self.lex.i < bytes.len() && bytes[self.lex.i] != b'\n' {
            self.advance();
        }
        if self.lex.i < bytes.len() {
            self.advance();
        }
        let mut pending = std::mem::take(&mut self.lex.heredocs);
        let mut leftover = VecDeque::new();
        'outer: while let Some(mut hd) = pending.pop_front() {
            hd.body_start = self.lex.i;
            let delim = hd.delim.clone();
            let delim_len = delim.len();
            let strip_tabs = hd.strip_tabs;
            let mut found = false;
            while self.lex.i < bytes.len() {
                let line_start = self.lex.i;
                let line_start_b = self.lex.i;
                // <<- 跳过行首 tab
                let mut check_i = line_start;
                if strip_tabs {
                    while check_i < bytes.len() && bytes[check_i] == b'\t' {
                        check_i += 1;
                    }
                }
                if self.src()[check_i..].starts_with(&delim)
                    && (check_i + delim_len >= bytes.len()
                        || bytes[check_i + delim_len] == b'\n'
                        || bytes[check_i + delim_len] == b'\r')
                {
                    hd.body_end = line_start_b;
                    while self.lex.i < check_i {
                        self.advance();
                    }
                    hd.end_start = self.lex.i;
                    for _ in 0..delim_len {
                        self.advance();
                    }
                    hd.end_end = self.lex.i;
                    if self.lex.i < bytes.len() && bytes[self.lex.i] == b'\n' {
                        self.advance();
                    }
                    found = true;
                    break;
                }
                // 消费一行
                while self.lex.i < bytes.len() && bytes[self.lex.i] != b'\n' {
                    self.advance();
                }
                if self.lex.i < bytes.len() {
                    self.advance();
                }
            }
            if !found {
                // 未闭合
                hd.body_end = self.lex.i;
                hd.end_start = self.lex.i;
                hd.end_end = self.lex.i;
            }
            self.lex.heredocs.push_back(hd);
            if found {
                // TS 端找到定界符即 return，剩余登记项保持原样
                leftover = pending;
                break 'outer;
            }
        }
        self.lex.heredocs.extend(leftover);
    }

    fn parse_heredoc_body_content(&mut self, start: usize, end: usize) -> PResult<Vec<TsNode>> {
        let saved = self.save();
        self.lex.i = start;
        let mut out = Vec::new();
        let mut content_start = self.lex.i;
        // tree-sitter-bash 的 heredoc_body 隐藏首个文本片段——
        // 只有第一个展开式之后的内容才作为 heredoc_content 节点
        let mut saw_expansion = false;
        while self.lex.i < end {
            let c = self.peek();
            // 反斜杠转义抑制展开
            if c == Some('\\') {
                let nxt = self.peek1();
                if matches!(nxt, Some('$') | Some('`') | Some('\\')) {
                    self.advance();
                    self.advance();
                    continue;
                }
                self.advance();
                continue;
            }
            if c == Some('$') || c == Some('`') {
                let pre_b = self.lex.i;
                let exp = self.parse_dollar_like()?;
                if let Some(exp) = &exp {
                    if matches!(
                        exp.node_type.as_str(),
                        "simple_expansion" | "expansion" | "command_substitution" | "arithmetic_expansion"
                    ) {
                        if saw_expansion && pre_b > content_start {
                            out.push(self.mk("heredoc_content", content_start, pre_b, vec![])?);
                        }
                        out.push(exp.clone());
                        content_start = self.lex.i;
                        saw_expansion = true;
                    }
                }
                continue;
            }
            self.advance();
        }
        // 有展开式才输出 heredoc_content 子节点，否则 heredoc_body 是叶子
        if saw_expansion {
            out.push(self.mk("heredoc_content", content_start, end, vec![])?);
        }
        self.restore(saved);
        Ok(out)
    }

    /// 词位置元素：裸词、字符串、展开或它们的拼接。多片段包进 concatenation。
    fn parse_word(&mut self, ctx: Ctx) -> PResult<Option<TsNode>> {
        let _ = ctx; // TS 端也仅用于未来扩展（_ctx）
        self.skip_blanks();
        let mut parts = Vec::new();
        while self.lex.i < self.src().len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if matches!(c, ' ' | '\t' | '\n' | '\r' | '|' | '&' | ';' | '(' | ')') {
                break;
            }
            // < > 是重定向算符，<( >( 除外（进程替换）
            if c == '<' || c == '>' {
                if self.peek1() == Some('(') {
                    let ps = self.parse_process_sub()?;
                    if let Some(ps) = ps {
                        parts.push(ps);
                    }
                    continue;
                }
                break;
            }
            if c == '"' {
                parts.push(self.parse_double_quoted()?);
                continue;
            }
            if c == '\'' {
                let tok = self.next_token(Ctx::Arg);
                parts.push(self.leaf("raw_string", &tok)?);
                continue;
            }
            if c == '$' {
                let c1 = self.peek1();
                if c1 == Some('\'') {
                    let tok = self.next_token(Ctx::Arg);
                    parts.push(self.leaf("ansi_c_string", &tok)?);
                    continue;
                }
                if c1 == Some('"') {
                    // 翻译字符串：$ 叶子 + string 节点
                    let d_tok_start = self.lex.i;
                    self.advance();
                    parts.push(self.mk("$", d_tok_start, d_tok_start + 1, vec![])?);
                    parts.push(self.parse_double_quoted()?);
                    continue;
                }
                if c1 == Some('`') {
                    // `$` 后跟反引号——tree-sitter 省略 $ 本身
                    self.advance();
                    continue;
                }
                let exp = self.parse_dollar_like()?;
                if let Some(exp) = exp {
                    parts.push(exp);
                }
                continue;
            }
            if c == '`' {
                if self.in_backtick > 0 {
                    break;
                }
                let bt = self.parse_backtick()?;
                if let Some(bt) = bt {
                    parts.push(bt);
                }
                continue;
            }
            // 花括号表达式 {1..5} 或 {a,b,c}
            if c == '{' {
                let be = self.try_parse_brace_expr()?;
                if let Some(be) = be {
                    parts.push(be);
                    continue;
                }
                // `{` 后紧跟命令终止符时是独立词
                let nc = self.peek1();
                if matches!(
                    nc,
                    Some(';') | Some('|') | Some('&') | Some('\n') | None | Some(')') | Some(' ') | Some('\t')
                ) {
                    let b_start = self.lex.i;
                    self.advance();
                    parts.push(self.mk("word", b_start, self.lex.i, vec![])?);
                    continue;
                }
                // 否则把 { } 当作词片段
                let cat = self.try_parse_brace_like_cat()?;
                if let Some(cat) = cat {
                    parts.extend(cat);
                    continue;
                }
            }
            // 参数位置的独立 `}` 是一个词
            if c == '}' {
                let b_start = self.lex.i;
                self.advance();
                parts.push(self.mk("word", b_start, self.lex.i, vec![])?);
                continue;
            }
            // `[` `]` 是单字符词片段
            if c == '[' || c == ']' {
                let b_start = self.lex.i;
                self.advance();
                parts.push(self.mk("word", b_start, self.lex.i, vec![])?);
                continue;
            }
            // 裸词片段
            let frag = self.parse_bare_word()?;
            let Some(frag) = frag else { break };
            // `NN#${...}` `NN#$(...)` → (number (expansion|command_substitution))
            if frag.node_type == "word"
                && is_base_prefix_text(&frag.text)
                && self.peek() == Some('$')
                && matches!(self.peek1(), Some('{') | Some('('))
            {
                let exp = self.parse_dollar_like()?;
                if let Some(exp) = exp {
                    parts.push(self.mk("number", frag.start_index, exp.end_index, vec![exp])?);
                    continue;
                }
            }
            parts.push(frag);
        }
        if parts.is_empty() {
            return Ok(None);
        }
        if parts.len() == 1 {
            return Ok(Some(parts.swap_remove(0)));
        }
        let start = parts[0].start_index;
        let end = parts[parts.len() - 1].end_index;
        Ok(Some(self.mk("concatenation", start, end, parts)?))
    }

    fn parse_bare_word(&mut self) -> PResult<Option<TsNode>> {
        let start = self.lex.i;
        let start_i = self.lex.i;
        while self.lex.i < self.src().len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == '\\' {
                if self.lex.i + 1 >= self.src().len() {
                    // 行尾孤立 `\`——tree-sitter 产出不含 `\` 的词 + 兄弟 ERROR
                    break;
                }
                let nx = self.peek1();
                if nx == Some('\n') || (nx == Some('\r') && self.peek2() == Some('\n')) {
                    // 行连接会打断词（tree-sitter 怪癖）
                    break;
                }
                self.advance();
                self.advance();
                continue;
            }
            if matches!(
                c,
                ' ' | '\t' | '\n' | '\r' | '|' | '&' | ';' | '(' | ')' | '<' | '>' | '"' | '\'' | '$' | '`' | '{' | '}' | '[' | ']'
            ) {
                break;
            }
            self.advance();
        }
        if self.lex.i == start {
            return Ok(None);
        }
        let text = self.src()[start_i..self.lex.i].to_string();
        let ty = if is_signed_int_text(&text) { "number" } else { "word" };
        Ok(Some(self.mk(ty, start, self.lex.i, vec![])?))
    }

    fn try_parse_brace_expr(&mut self) -> PResult<Option<TsNode>> {
        // {N..M}，N/M 是数字或单字符
        let save = self.save();
        if self.peek() != Some('{') {
            return Ok(None);
        }
        let o_start = self.lex.i;
        self.advance();
        let o_end = self.lex.i;
        let p1_start = self.lex.i;
        while matches!(self.peek(), Some(c) if is_digit(c) || is_ident_start(c)) {
            self.advance();
        }
        let p1_end = self.lex.i;
        if p1_end == p1_start || self.peek() != Some('.') || self.peek1() != Some('.') {
            self.restore(save);
            return Ok(None);
        }
        let dot_start = self.lex.i;
        self.advance();
        self.advance();
        let dot_end = self.lex.i;
        let p2_start = self.lex.i;
        while matches!(self.peek(), Some(c) if is_digit(c) || is_ident_start(c)) {
            self.advance();
        }
        let p2_end = self.lex.i;
        if p2_end == p2_start || self.peek() != Some('}') {
            self.restore(save);
            return Ok(None);
        }
        let c_start = self.lex.i;
        self.advance();
        let c_end = self.lex.i;
        let p1_text = self.src()[p1_start..p1_end].to_string();
        let p2_text = self.src()[p2_start..p2_end].to_string();
        let p1_is_num = is_digits_text(&p1_text);
        let p2_is_num = is_digits_text(&p2_text);
        // 有效花括号表达式：两边都是数字或都是单字符。混合则拒绝。
        if p1_is_num != p2_is_num {
            self.restore(save);
            return Ok(None);
        }
        if !p1_is_num && (p1_text.len() != 1 || p2_text.len() != 1) {
            self.restore(save);
            return Ok(None);
        }
        let p1_type = if p1_is_num { "number" } else { "word" };
        let p2_type = if p2_is_num { "number" } else { "word" };
        let mut kids = vec![self.mk("{", o_start, o_end, vec![])?];
        kids.push(self.mk(p1_type, p1_start, p1_end, vec![])?);
        kids.push(self.mk("..", dot_start, dot_end, vec![])?);
        kids.push(self.mk(p2_type, p2_start, p2_end, vec![])?);
        kids.push(self.mk("}", c_start, c_end, vec![])?);
        Ok(Some(self.mk("brace_expression", o_start, c_end, kids)?))
    }

    fn try_parse_brace_like_cat(&mut self) -> PResult<Option<Vec<TsNode>>> {
        // {a,b,c} 或 {} → 按 tree-sitter 拆成词片段
        if self.peek() != Some('{') {
            return Ok(None);
        }
        let o_start = self.lex.i;
        self.advance();
        let o_end = self.lex.i;
        let mut inner = vec![self.mk("word", o_start, o_end, vec![])?];
        while self.lex.i < self.src().len() {
            let bc = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if matches!(bc, '}' | '\n' | ';' | '|' | '&' | ' ' | '\t' | '<' | '>' | '(' | ')') {
                break;
            }
            // `[` `]` 是单字符词
            if bc == '[' || bc == ']' {
                let b_start = self.lex.i;
                self.advance();
                inner.push(self.mk("word", b_start, self.lex.i, vec![])?);
                continue;
            }
            let mid_start = self.lex.i;
            while self.lex.i < self.src().len() {
                let mc = match self.peek() {
                    Some(c) => c,
                    None => break,
                };
                if matches!(
                    mc,
                    '}' | '\n' | ';' | '|' | '&' | ' ' | '\t' | '<' | '>' | '(' | ')' | '[' | ']'
                ) {
                    break;
                }
                self.advance();
            }
            let mid_end = self.lex.i;
            if mid_end > mid_start {
                let mid_text = self.src()[mid_start..mid_end].to_string();
                let mid_type = if is_signed_int_text(&mid_text) { "number" } else { "word" };
                inner.push(self.mk(mid_type, mid_start, mid_end, vec![])?);
            } else {
                break;
            }
        }
        if self.peek() == Some('}') {
            let c_start = self.lex.i;
            self.advance();
            inner.push(self.mk("word", c_start, self.lex.i, vec![])?);
        }
        Ok(Some(inner))
    }

    fn parse_double_quoted(&mut self) -> PResult<TsNode> {
        let q_start = self.lex.i;
        self.advance();
        let q_end = self.lex.i;
        let open_q = self.mk("\"", q_start, q_end, vec![])?;
        let mut parts = vec![open_q];
        let mut content_start = self.lex.i;
        while self.lex.i < self.src().len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == '"' {
                break;
            }
            if c == '\\' && self.peek1().is_some() {
                self.advance();
                self.advance();
                continue;
            }
            if c == '\n' {
                // 在换行处切开 string_content
                self.flush_string_content(&mut parts, content_start)?;
                self.advance();
                content_start = self.lex.i;
                continue;
            }
            if c == '$' {
                let c1 = self.peek1();
                if c1 == Some('(')
                    || c1 == Some('{')
                    || matches!(c1, Some(c) if is_ident_start(c))
                    || matches!(c1, Some(c) if is_special_var(c))
                    || matches!(c1, Some(c) if is_digit(c))
                {
                    self.flush_string_content(&mut parts, content_start)?;
                    let exp = self.parse_dollar_like()?;
                    if let Some(exp) = exp {
                        parts.push(exp);
                    }
                    content_start = self.lex.i;
                    continue;
                }
                // 不在串尾的裸 $：tree-sitter 输出匿名 '$' token，切开 string_content
                if c1 != Some('"') && c1.is_some() {
                    self.flush_string_content(&mut parts, content_start)?;
                    let d_s = self.lex.i;
                    self.advance();
                    parts.push(self.mk("$", d_s, self.lex.i, vec![])?);
                    content_start = self.lex.i;
                    continue;
                }
            }
            if c == '`' {
                self.flush_string_content(&mut parts, content_start)?;
                let bt = self.parse_backtick()?;
                if let Some(bt) = bt {
                    parts.push(bt);
                }
                content_start = self.lex.i;
                continue;
            }
            self.advance();
        }
        self.flush_string_content(&mut parts, content_start)?;
        let close = if self.peek() == Some('"') {
            let c_start = self.lex.i;
            self.advance();
            self.mk("\"", c_start, self.lex.i, vec![])?
        } else {
            self.mk("\"", self.lex.i, self.lex.i, vec![])?
        };
        let close_end = close.end_index;
        parts.push(close);
        Ok(self.mk("string", q_start, close_end, parts)?)
    }

    /// 仅在空白段被 tree-sitter 的 extras 规则吞掉时跳过 string_content
    fn flush_string_content(
        &mut self,
        parts: &mut Vec<TsNode>,
        content_start: usize,
    ) -> PResult<()> {
        if self.lex.i > content_start {
            let txt = &self.src()[content_start..self.lex.i];
            if !is_blank_text(txt) {
                parts.push(self.mk("string_content", content_start, self.lex.i, vec![])?);
            }
        }
        Ok(())
    }

    fn parse_dollar_like(&mut self) -> PResult<Option<TsNode>> {
        let c1 = self.peek1();
        let d_start = self.lex.i;
        if c1 == Some('(') && self.peek2() == Some('(') {
            // $(( 算术 ))
            self.advance();
            self.advance();
            self.advance();
            let open = self.mk("$((", d_start, self.lex.i, vec![])?;
            let exprs = self.parse_arith_comma_list("))", ArithMode::Var)?;
            self.skip_blanks();
            let close = if self.peek() == Some(')') && self.peek1() == Some(')') {
                let c_start = self.lex.i;
                self.advance();
                self.advance();
                self.mk("))", c_start, self.lex.i, vec![])?
            } else {
                self.mk("))", self.lex.i, self.lex.i, vec![])?
            };
            let close_end = close.end_index;
            let mut kids = vec![open];
            kids.extend(exprs);
            kids.push(close);
            return Ok(Some(self.mk("arithmetic_expansion", d_start, close_end, kids)?));
        }
        if c1 == Some('[') {
            // $[ 算术 ] —— 旧版 bash 语法
            self.advance();
            self.advance();
            let open = self.mk("$[", d_start, self.lex.i, vec![])?;
            let exprs = self.parse_arith_comma_list("]", ArithMode::Var)?;
            self.skip_blanks();
            let close = if self.peek() == Some(']') {
                let c_start = self.lex.i;
                self.advance();
                self.mk("]", c_start, self.lex.i, vec![])?
            } else {
                self.mk("]", self.lex.i, self.lex.i, vec![])?
            };
            let close_end = close.end_index;
            let mut kids = vec![open];
            kids.extend(exprs);
            kids.push(close);
            return Ok(Some(self.mk("arithmetic_expansion", d_start, close_end, kids)?));
        }
        if c1 == Some('(') {
            self.advance();
            self.advance();
            let open = self.mk("$(", d_start, self.lex.i, vec![])?;
            let mut body = self.parse_statements(Some(")"))?;
            self.skip_blanks();
            let close = if self.peek() == Some(')') {
                let c_start = self.lex.i;
                self.advance();
                self.mk(")", c_start, self.lex.i, vec![])?
            } else {
                self.mk(")", self.lex.i, self.lex.i, vec![])?
            };
            // $(< file) 简写：展开 redirected_statement → 直接 file_redirect
            if body.len() == 1
                && body[0].node_type == "redirected_statement"
                && body[0].children.len() == 1
                && body[0].children[0].node_type == "file_redirect"
            {
                body = body.remove(0).children;
            }
            let close_end = close.end_index;
            let mut kids = vec![open];
            kids.extend(body);
            kids.push(close);
            return Ok(Some(self.mk("command_substitution", d_start, close_end, kids)?));
        }
        if c1 == Some('{') {
            self.advance();
            self.advance();
            let open = self.mk("${", d_start, self.lex.i, vec![])?;
            let inner = self.parse_expansion_body()?;
            let close = if self.peek() == Some('}') {
                let c_start = self.lex.i;
                self.advance();
                self.mk("}", c_start, self.lex.i, vec![])?
            } else {
                self.mk("}", self.lex.i, self.lex.i, vec![])?
            };
            let close_end = close.end_index;
            let mut kids = vec![open];
            kids.extend(inner);
            kids.push(close);
            return Ok(Some(self.mk("expansion", d_start, close_end, kids)?));
        }
        // 简单展开 $VAR 或 $? $$ $@ 等
        self.advance();
        let d_end = self.lex.i;
        let dollar = self.mk("$", d_start, d_end, vec![])?;
        let nc = self.peek();
        // $_ 仅在不跟更多标识符字符时是 special_variable_name
        if nc == Some('_') && !matches!(self.peek1(), Some(c) if is_ident_char(c)) {
            let v_start = self.lex.i;
            self.advance();
            let vn = self.mk("special_variable_name", v_start, self.lex.i, vec![])?;
            return Ok(Some(self.mk("simple_expansion", d_start, self.lex.i, vec![dollar, vn])?));
        }
        if matches!(nc, Some(c) if is_ident_start(c)) {
            let v_start = self.lex.i;
            while matches!(self.peek(), Some(c) if is_ident_char(c)) {
                self.advance();
            }
            let vn = self.mk("variable_name", v_start, self.lex.i, vec![])?;
            return Ok(Some(self.mk("simple_expansion", d_start, self.lex.i, vec![dollar, vn])?));
        }
        if matches!(nc, Some(c) if is_digit(c)) {
            let v_start = self.lex.i;
            self.advance();
            let vn = self.mk("variable_name", v_start, self.lex.i, vec![])?;
            return Ok(Some(self.mk("simple_expansion", d_start, self.lex.i, vec![dollar, vn])?));
        }
        if matches!(nc, Some(c) if is_special_var(c)) {
            let v_start = self.lex.i;
            self.advance();
            let vn = self.mk("special_variable_name", v_start, self.lex.i, vec![])?;
            return Ok(Some(self.mk("simple_expansion", d_start, self.lex.i, vec![dollar, vn])?));
        }
        // 裸 $ —— 只输出 $ 叶子
        Ok(Some(dollar))
    }

    fn parse_expansion_body(&mut self) -> PResult<Vec<TsNode>> {
        let mut out = Vec::new();
        self.skip_blanks();
        // 诡异情形：${#!} ${!#} ${!##} ${!# } ${!## } 都输出空 (expansion)
        {
            let c0 = self.peek();
            let c1 = self.peek1();
            if c0 == Some('#') && c1 == Some('!') && self.peek2() == Some('}') {
                self.advance();
                self.advance();
                return Ok(out);
            }
            if c0 == Some('!') && c1 == Some('#') {
                // ${!#} ${!##}，可选空格后 }
                let mut j = 2;
                if self.peek_off(j) == Some('#') {
                    j += 1;
                }
                if self.peek_off(j) == Some(' ') {
                    j += 1;
                }
                if self.peek_off(j) == Some('}') {
                    for _ in 0..j {
                        self.advance();
                    }
                    return Ok(out);
                }
            }
        }
        // 可选 # 前缀（取长度）
        if self.peek() == Some('#') {
            let s = self.lex.i;
            self.advance();
            out.push(self.mk("#", s, self.lex.i, vec![])?);
        }
        // 可选 ! = ~ 前缀（间接展开 / zsh 风格）——仅后跟标识符或数字时
        let pc = self.peek();
        if let Some(pc) = pc {
            if matches!(pc, '!' | '=' | '~')
                && matches!(self.peek1(), Some(c) if is_ident_start(c) || is_digit(c))
            {
                let s = self.lex.i;
                self.advance();
                out.push(self.mk_char(pc, s, self.lex.i, vec![])?);
            }
        }
        self.skip_blanks();
        // 变量名
        if matches!(self.peek(), Some(c) if is_ident_start(c)) {
            let s = self.lex.i;
            while matches!(self.peek(), Some(c) if is_ident_char(c)) {
                self.advance();
            }
            out.push(self.mk("variable_name", s, self.lex.i, vec![])?);
        } else if matches!(self.peek(), Some(c) if is_digit(c)) {
            let s = self.lex.i;
            while matches!(self.peek(), Some(c) if is_digit(c)) {
                self.advance();
            }
            out.push(self.mk("variable_name", s, self.lex.i, vec![])?);
        } else if matches!(self.peek(), Some(c) if is_special_var(c)) {
            let s = self.lex.i;
            self.advance();
            out.push(self.mk("special_variable_name", s, self.lex.i, vec![])?);
        }
        // 可选下标 [idx] —— 按算术解析
        if self.peek() == Some('[') {
            let var_node = out.last().cloned();
            let br_open = self.lex.i;
            self.advance();
            let br_open_node = self.mk("[", br_open, self.lex.i, vec![])?;
            let idx = self.parse_subscript_index_inline()?;
            self.skip_blanks();
            let br_close = self.lex.i;
            if self.peek() == Some(']') {
                self.advance();
            }
            let br_close_node = self.mk("]", br_close, self.lex.i, vec![])?;
            if let Some(var_node) = var_node {
                let var_start = var_node.start_index;
                let mut kids = vec![var_node, br_open_node];
                if let Some(idx) = idx {
                    kids.push(idx);
                }
                kids.push(br_close_node);
                let sub = self.mk("subscript", var_start, self.lex.i, kids)?;
                if let Some(last) = out.last_mut() {
                    *last = sub;
                }
            }
        }
        self.skip_blanks();
        // 尾随 * 或 @（间接展开 / 参数变换）
        let tc = self.peek();
        if let Some(tc) = tc {
            if matches!(tc, '*' | '@') && self.peek1() == Some('}') {
                let s = self.lex.i;
                self.advance();
                out.push(self.mk_char(tc, s, self.lex.i, vec![])?);
                return Ok(out);
            }
        }
        if tc == Some('@') && matches!(self.peek1(), Some(c) if is_ident_start(c)) {
            // ${var@U} 变换——@ 是匿名节点
            let s = self.lex.i;
            self.advance();
            out.push(self.mk("@", s, self.lex.i, vec![])?);
            while matches!(self.peek(), Some(c) if is_ident_char(c)) {
                self.advance();
            }
            return Ok(out);
        }
        // 操作符 :- := :? :+ - = ? + # ## % %% / // ^ ^^ , ,,
        let c = self.peek();
        // 裸 `:` 子串操作符 ${var:off:len} —— 必须先于通用操作符处理
        if c == Some(':') {
            let c1 = self.peek1();
            // `:\n` 或 `:}` —— 空子串展开，不输出节点
            if c1 == Some('\n') || c1 == Some('}') {
                self.advance();
                while self.peek() == Some('\n') {
                    self.advance();
                }
                return Ok(out);
            }
            if !matches!(c1, Some('-') | Some('=') | Some('?') | Some('+')) {
                self.advance();
                self.skip_blanks();
                // 偏移 —— 算术
                let off_c = self.peek();
                let mut off = None;
                if off_c == Some('-') && matches!(self.peek1(), Some(c) if is_digit(c)) {
                    let ns = self.lex.i;
                    self.advance();
                    while matches!(self.peek(), Some(c) if is_digit(c)) {
                        self.advance();
                    }
                    off = Some(self.mk("number", ns, self.lex.i, vec![])?);
                } else {
                    off = self.parse_arith_expr(":}", ArithMode::Var)?;
                }
                if let Some(off) = off {
                    out.push(off);
                }
                self.skip_blanks();
                if self.peek() == Some(':') {
                    self.advance();
                    self.skip_blanks();
                    let len_c = self.peek();
                    let mut len = None;
                    if len_c == Some('-') && matches!(self.peek1(), Some(c) if is_digit(c)) {
                        let ns = self.lex.i;
                        self.advance();
                        while matches!(self.peek(), Some(c) if is_digit(c)) {
                            self.advance();
                        }
                        len = Some(self.mk("number", ns, self.lex.i, vec![])?);
                    } else {
                        len = self.parse_arith_expr("}", ArithMode::Var)?;
                    }
                    if let Some(len) = len {
                        out.push(len);
                    }
                }
                return Ok(out);
            }
        }
        if let Some(c) = c {
            if !matches!(c, ':' | '#' | '%' | '/' | '^' | ',' | '-' | '=' | '?' | '+') {
                return Ok(out);
            }
            let s = self.lex.i;
            let c1 = self.peek1();
            let mut op = String::new();
            op.push(c);
            if c == ':' && matches!(c1, Some('-') | Some('=') | Some('?') | Some('+')) {
                self.advance();
                self.advance();
                if let Some(c1) = c1 {
                    op.push(c1);
                }
            } else if matches!(c, '#' | '%' | '/' | '^' | ',') && c1 == Some(c) {
                // 双写算符
                self.advance();
                self.advance();
                op.push(c);
            } else {
                self.advance();
            }
            out.push(self.mk(&op, s, self.lex.i, vec![])?);
            let is_pattern = matches!(
                op.as_str(),
                "#" | "##" | "%" | "%%" | "/" | "//" | "^" | "^^" | "," | ",,"
            );
            if op == "/" || op == "//" {
                // 可选 /# 或 /% 锚点前缀
                if let Some(ac) = self.peek() {
                    if matches!(ac, '#' | '%') {
                        let a_start = self.lex.i;
                        self.advance();
                        out.push(self.mk_char(ac, a_start, self.lex.i, vec![])?);
                    }
                }
                // 替换左侧模式
                if self.peek() == Some('"') {
                    out.push(self.parse_double_quoted()?);
                    let tail = self.parse_expansion_rest(RestMode::Regex, true)?;
                    if let Some(tail) = tail {
                        out.push(tail);
                    }
                } else {
                    let regex = self.parse_expansion_rest(RestMode::Regex, true)?;
                    if let Some(regex) = regex {
                        out.push(regex);
                    }
                }
                if self.peek() == Some('/') {
                    let sep_start = self.lex.i;
                    self.advance();
                    out.push(self.mk("/", sep_start, self.lex.i, vec![])?);
                    let repl = self.parse_expansion_rest(RestMode::ReplWord, false)?;
                    if let Some(repl) = repl {
                        // seq(cmd_sub, word) 特例 → 兄弟节点
                        if repl.node_type == "concatenation"
                            && repl.children.len() == 2
                            && repl.children[0].node_type == "command_substitution"
                        {
                            let mut kids = repl.children;
                            out.push(kids.remove(0));
                            out.push(kids.remove(0));
                        } else {
                            out.push(repl);
                        }
                    }
                }
            } else if matches!(op.as_str(), "#" | "##" | "%" | "%%") {
                // 模式删除：每个引号片段是兄弟节点
                let parts = self.parse_expansion_regex_segmented()?;
                out.extend(parts);
            } else {
                let rest = self.parse_expansion_rest(
                    if is_pattern { RestMode::Regex } else { RestMode::Word },
                    false,
                )?;
                if let Some(rest) = rest {
                    out.push(rest);
                }
            }
        }
        Ok(out)
    }

    fn parse_expansion_rest(
        &mut self,
        mode: RestMode,
        stop_at_slash: bool,
    ) -> PResult<Option<TsNode>> {
        // 不 skipBlanks —— `${var:- }` 的空格就是那个词
        let start = self.lex.i;
        // 值替换 RHS 以 `(` 开头时解析为数组
        if mode == RestMode::Word && self.peek() == Some('(') {
            self.advance();
            let open = self.mk("(", start, self.lex.i, vec![])?;
            let mut elems = vec![open];
            while self.lex.i < self.src().len() {
                self.skip_blanks();
                let c = self.peek();
                if matches!(c, Some(')') | Some('}') | Some('\n') | None) {
                    break;
                }
                let w_start = self.lex.i;
                while self.lex.i < self.src().len() {
                    let wc = self.peek();
                    if matches!(wc, Some(')') | Some('}') | Some(' ') | Some('\t') | Some('\n') | None) {
                        break;
                    }
                    self.advance();
                }
                if self.lex.i > w_start {
                    elems.push(self.mk("word", w_start, self.lex.i, vec![])?);
                } else {
                    break;
                }
            }
            if self.peek() == Some(')') {
                let c_start = self.lex.i;
                self.advance();
                elems.push(self.mk(")", c_start, self.lex.i, vec![])?);
            }
            while self.peek() == Some('\n') {
                self.advance();
            }
            return Ok(Some(self.mk("array", start, self.lex.i, elems)?));
        }
        // REGEX 模式：扁平单区间扫描
        if mode == RestMode::Regex {
            let mut brace_depth = 0;
            while self.lex.i < self.src().len() {
                let c = match self.peek() {
                    Some(c) => c,
                    None => break,
                };
                if c == '\n' {
                    break;
                }
                if brace_depth == 0 {
                    if c == '}' {
                        break;
                    }
                    if stop_at_slash && c == '/' {
                        break;
                    }
                }
                if c == '\\' && self.peek1().is_some() {
                    self.advance();
                    self.advance();
                    continue;
                }
                if c == '"' || c == '\'' {
                    self.advance();
                    while self.lex.i < self.src().len() && self.peek() != Some(c) {
                        if self.peek() == Some('\\') && self.peek1().is_some() {
                            self.advance();
                        }
                        self.advance();
                    }
                    if self.peek() == Some(c) {
                        self.advance();
                    }
                    continue;
                }
                // 跳过嵌套 ${...} $(...) $[...]，避免其 } / / 提前终止
                if c == '$' {
                    let c1 = self.peek1();
                    if c1 == Some('{') {
                        let mut d = 0;
                        self.advance();
                        self.advance();
                        d += 1;
                        while d > 0 {
                            let Some(nc) = self.peek() else { break };
                            if nc == '{' {
                                d += 1;
                            } else if nc == '}' {
                                d -= 1;
                            }
                            self.advance();
                        }
                        continue;
                    }
                    if c1 == Some('(') {
                        let mut d = 0;
                        self.advance();
                        self.advance();
                        d += 1;
                        while d > 0 {
                            let Some(nc) = self.peek() else { break };
                            if nc == '(' {
                                d += 1;
                            } else if nc == ')' {
                                d -= 1;
                            }
                            self.advance();
                        }
                        continue;
                    }
                }
                if c == '{' {
                    brace_depth += 1;
                } else if c == '}' && brace_depth > 0 {
                    brace_depth -= 1;
                }
                self.advance();
            }
            let end = self.lex.i;
            while self.peek() == Some('\n') {
                self.advance();
            }
            if end == start {
                return Ok(None);
            }
            return Ok(Some(self.mk("regex", start, end, vec![])?));
        }
        // WORD 模式：分段解析
        let mut parts = Vec::new();
        let mut seg_start = self.lex.i;
        let mut brace_depth = 0;
        while self.lex.i < self.src().len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == '\n' {
                break;
            }
            if brace_depth == 0 {
                if c == '}' {
                    break;
                }
                if stop_at_slash && c == '/' {
                    break;
                }
            }
            if c == '\\' && self.peek1().is_some() {
                self.advance();
                self.advance();
                continue;
            }
            let c1 = self.peek1();
            if c == '$' {
                if matches!(c1, Some('{') | Some('(') | Some('[')) {
                    if self.lex.i > seg_start {
                        parts.push(self.mk("word", seg_start, self.lex.i, vec![])?);
                    }
                    let exp = self.parse_dollar_like()?;
                    if let Some(exp) = exp {
                        parts.push(exp);
                    }
                    seg_start = self.lex.i;
                    continue;
                }
                if c1 == Some('\'') {
                    // $'...' ANSI-C 字符串
                    if self.lex.i > seg_start {
                        parts.push(self.mk("word", seg_start, self.lex.i, vec![])?);
                    }
                    let a_start = self.lex.i;
                    self.advance();
                    self.advance();
                    while self.lex.i < self.src().len() && self.peek() != Some('\'') {
                        if self.peek() == Some('\\') && self.peek1().is_some() {
                            self.advance();
                        }
                        self.advance();
                    }
                    if self.peek() == Some('\'') {
                        self.advance();
                    }
                    parts.push(self.mk("ansi_c_string", a_start, self.lex.i, vec![])?);
                    seg_start = self.lex.i;
                    continue;
                }
                if matches!(c1, Some(c) if is_ident_start(c))
                    || matches!(c1, Some(c) if is_digit(c))
                    || matches!(c1, Some(c) if is_special_var(c))
                {
                    if self.lex.i > seg_start {
                        parts.push(self.mk("word", seg_start, self.lex.i, vec![])?);
                    }
                    let exp = self.parse_dollar_like()?;
                    if let Some(exp) = exp {
                        parts.push(exp);
                    }
                    seg_start = self.lex.i;
                    continue;
                }
            }
            if c == '"' {
                if self.lex.i > seg_start {
                    parts.push(self.mk("word", seg_start, self.lex.i, vec![])?);
                }
                parts.push(self.parse_double_quoted()?);
                seg_start = self.lex.i;
                continue;
            }
            if c == '\'' {
                if self.lex.i > seg_start {
                    parts.push(self.mk("word", seg_start, self.lex.i, vec![])?);
                }
                let r_start = self.lex.i;
                self.advance();
                while self.lex.i < self.src().len() && self.peek() != Some('\'') {
                    self.advance();
                }
                if self.peek() == Some('\'') {
                    self.advance();
                }
                parts.push(self.mk("raw_string", r_start, self.lex.i, vec![])?);
                seg_start = self.lex.i;
                continue;
            }
            if matches!(c, '<' | '>') && c1 == Some('(') {
                if self.lex.i > seg_start {
                    parts.push(self.mk("word", seg_start, self.lex.i, vec![])?);
                }
                let ps = self.parse_process_sub()?;
                if let Some(ps) = ps {
                    parts.push(ps);
                }
                seg_start = self.lex.i;
                continue;
            }
            if c == '`' {
                if self.lex.i > seg_start {
                    parts.push(self.mk("word", seg_start, self.lex.i, vec![])?);
                }
                let bt = self.parse_backtick()?;
                if let Some(bt) = bt {
                    parts.push(bt);
                }
                seg_start = self.lex.i;
                continue;
            }
            // 花括号计数，避免嵌套 {a,b} 提前终止
            if c == '{' {
                brace_depth += 1;
            } else if c == '}' && brace_depth > 0 {
                brace_depth -= 1;
            }
            self.advance();
        }
        if self.lex.i > seg_start {
            parts.push(self.mk("word", seg_start, self.lex.i, vec![])?);
        }
        // 消费 } 前的换行
        while self.peek() == Some('\n') {
            self.advance();
        }
        // tree-sitter 跳过 RHS 开头的空白（仅当后面还有内容时）
        if parts.len() > 1
            && parts[0].node_type == "word"
            && is_blank_text(&parts[0].text)
        {
            parts.remove(0);
        }
        if parts.is_empty() {
            return Ok(None);
        }
        if parts.len() == 1 {
            return Ok(Some(parts.swap_remove(0)));
        }
        let start = parts[0].start_index;
        let end = parts[parts.len() - 1].end_index;
        Ok(Some(self.mk("concatenation", start, end, parts)?))
    }

    /// # ## % %% 操作符的模式 —— 每个引号片段是独立兄弟节点
    fn parse_expansion_regex_segmented(&mut self) -> PResult<Vec<TsNode>> {
        let mut out = Vec::new();
        let mut seg_start = self.lex.i;
        while self.lex.i < self.src().len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == '}' || c == '\n' {
                break;
            }
            if c == '\\' && self.peek1().is_some() {
                self.advance();
                self.advance();
                continue;
            }
            if c == '"' {
                if self.lex.i > seg_start {
                    out.push(self.mk("regex", seg_start, self.lex.i, vec![])?);
                }
                out.push(self.parse_double_quoted()?);
                seg_start = self.lex.i;
                continue;
            }
            if c == '\'' {
                if self.lex.i > seg_start {
                    out.push(self.mk("regex", seg_start, self.lex.i, vec![])?);
                }
                let r_start = self.lex.i;
                self.advance();
                while self.lex.i < self.src().len() && self.peek() != Some('\'') {
                    self.advance();
                }
                if self.peek() == Some('\'') {
                    self.advance();
                }
                out.push(self.mk("raw_string", r_start, self.lex.i, vec![])?);
                seg_start = self.lex.i;
                continue;
            }
            // 嵌套 ${...} $(...) —— 不透明扫描
            if c == '$' {
                let c1 = self.peek1();
                if c1 == Some('{') {
                    let mut d = 1;
                    self.advance();
                    self.advance();
                    while d > 0 {
                        let Some(nc) = self.peek() else { break };
                        if nc == '{' {
                            d += 1;
                        } else if nc == '}' {
                            d -= 1;
                        }
                        self.advance();
                    }
                    continue;
                }
                if c1 == Some('(') {
                    let mut d = 1;
                    self.advance();
                    self.advance();
                    while d > 0 {
                        let Some(nc) = self.peek() else { break };
                        if nc == '(' {
                            d += 1;
                        } else if nc == ')' {
                            d -= 1;
                        }
                        self.advance();
                    }
                    continue;
                }
            }
            self.advance();
        }
        if self.lex.i > seg_start {
            out.push(self.mk("regex", seg_start, self.lex.i, vec![])?);
        }
        while self.peek() == Some('\n') {
            self.advance();
        }
        Ok(out)
    }

    fn parse_backtick(&mut self) -> PResult<Option<TsNode>> {
        let start = self.lex.i;
        self.advance();
        let open = self.mk("`", start, self.lex.i, vec![])?;
        self.in_backtick += 1;
        let mut body = Vec::new();
        loop {
            self.skip_blanks();
            if self.peek() == Some('`') || self.peek().is_none() {
                break;
            }
            let save = self.save();
            let t = self.next_token(Ctx::Cmd);
            if t.kind == TokenKind::Eof || t.kind == TokenKind::Backtick {
                self.restore(save);
                break;
            }
            if t.kind == TokenKind::Newline {
                continue;
            }
            self.restore(save);
            let stmt = self.parse_and_or()?;
            let Some(stmt) = stmt else { break };
            body.push(stmt);
            self.skip_blanks();
            if self.peek() == Some('`') {
                break;
            }
            let save2 = self.save();
            let sep = self.next_token(Ctx::Cmd);
            if sep.kind == TokenKind::Op && (sep.value == ";" || sep.value == "&") {
                body.push(self.leaf(sep.value, &sep)?);
            } else if sep.kind != TokenKind::Newline {
                self.restore(save2);
            }
        }
        self.in_backtick -= 1;
        let close = if self.peek() == Some('`') {
            let c_start = self.lex.i;
            self.advance();
            self.mk("`", c_start, self.lex.i, vec![])?
        } else {
            self.mk("`", self.lex.i, self.lex.i, vec![])?
        };
        // 空反引号（仅空白/换行）被 tree-sitter 完全省略
        if body.is_empty() {
            return Ok(None);
        }
        let close_end = close.end_index;
        let mut kids = vec![open];
        kids.extend(body);
        kids.push(close);
        Ok(Some(self.mk("command_substitution", start, close_end, kids)?))
    }

    fn parse_if(&mut self, if_tok: &Token) -> PResult<TsNode> {
        let if_kw = self.leaf("if", if_tok)?;
        let if_start = if_kw.start_index;
        let mut kids = vec![if_kw];
        let cond = self.parse_statements(None)?;
        kids.extend(cond);
        self.consume_keyword("then", &mut kids)?;
        let body = self.parse_statements(None)?;
        kids.extend(body);
        loop {
            let save = self.save();
            let t = self.next_token(Ctx::Cmd);
            if t.kind == TokenKind::Word && t.value == "elif" {
                let e_kw = self.leaf("elif", &t)?;
                let e_cond = self.parse_statements(None)?;
                let mut e_kids = vec![e_kw.clone()];
                e_kids.extend(e_cond);
                self.consume_keyword("then", &mut e_kids)?;
                let e_body = self.parse_statements(None)?;
                e_kids.extend(e_body);
                let e_start = e_kw.start_index;
                let last_end = e_kids[e_kids.len() - 1].end_index;
                kids.push(self.mk("elif_clause", e_start, last_end, e_kids)?);
            } else if t.kind == TokenKind::Word && t.value == "else" {
                let el_kw = self.leaf("else", &t)?;
                let el_body = self.parse_statements(None)?;
                let el_start = el_kw.start_index;
                let last_end = el_body.last().map(|l| l.end_index).unwrap_or(el_kw.end_index);
                let mut v = vec![el_kw];
                v.extend(el_body);
                kids.push(self.mk("else_clause", el_start, last_end, v)?);
            } else {
                self.restore(save);
                break;
            }
        }
        self.consume_keyword("fi", &mut kids)?;
        let last_end = kids[kids.len() - 1].end_index;
        self.mk("if_statement", if_start, last_end, kids)
    }

    fn parse_while(&mut self, kw_tok: &Token) -> PResult<TsNode> {
        let kw = self.leaf(kw_tok.value, kw_tok)?;
        let kw_start = kw.start_index;
        let mut kids = vec![kw];
        let cond = self.parse_statements(None)?;
        kids.extend(cond);
        if let Some(dg) = self.parse_do_group()? {
            kids.push(dg);
        }
        let last_end = kids[kids.len() - 1].end_index;
        self.mk("while_statement", kw_start, last_end, kids)
    }

    fn parse_for(&mut self, for_tok: &Token) -> PResult<TsNode> {
        let for_kw = self.leaf(for_tok.value, for_tok)?;
        let for_start = for_kw.start_index;
        self.skip_blanks();
        // C 风格 for (( ; ; )) —— 仅 `for`
        if for_tok.value == "for" && self.peek() == Some('(') && self.peek1() == Some('(') {
            let o_start = self.lex.i;
            self.advance();
            self.advance();
            let open = self.mk("((", o_start, self.lex.i, vec![])?;
            let mut kids = vec![for_kw.clone(), open];
            // init; cond; update —— 都用 'assign' 模式
            for k in 0..3 {
                self.skip_blanks();
                let es = self.parse_arith_comma_list(if k < 2 { ";" } else { "))" }, ArithMode::Assign)?;
                kids.extend(es);
                if k < 2 && self.peek() == Some(';') {
                    let s = self.lex.i;
                    self.advance();
                    kids.push(self.mk(";", s, self.lex.i, vec![])?);
                }
            }
            self.skip_blanks();
            if self.peek() == Some(')') && self.peek1() == Some(')') {
                let c_start = self.lex.i;
                self.advance();
                self.advance();
                kids.push(self.mk("))", c_start, self.lex.i, vec![])?);
            }
            // 可选 ; 或换行
            let save = self.save();
            let sep = self.next_token(Ctx::Cmd);
            if sep.kind == TokenKind::Op && sep.value == ";" {
                kids.push(self.leaf(";", &sep)?);
            } else if sep.kind != TokenKind::Newline {
                self.restore(save);
            }
            if let Some(dg) = self.parse_do_group()? {
                kids.push(dg);
            } else {
                // C 风格 for 也可以用 { ... } 体
                self.skip_newlines();
                self.skip_blanks();
                if self.peek() == Some('{') {
                    let b_open_b = self.lex.i;
                    self.advance();
                    let brace = self.mk("{", b_open_b, self.lex.i, vec![])?;
                    let body = self.parse_statements(Some("}"))?;
                    let b_close = if self.peek() == Some('}') {
                        let cs = self.lex.i;
                        self.advance();
                        self.mk("}", cs, self.lex.i, vec![])?
                    } else {
                        self.mk("}", self.lex.i, self.lex.i, vec![])?
                    };
                    let brace_start = brace.start_index;
                    let b_close_end = b_close.end_index;
                    let mut bk = vec![brace];
                    bk.extend(body);
                    bk.push(b_close);
                    kids.push(self.mk("compound_statement", brace_start, b_close_end, bk)?);
                }
            }
            let last_end = kids[kids.len() - 1].end_index;
            return self.mk("c_style_for_statement", for_start, last_end, kids);
        }
        // 常规 for VAR in words; do ... done
        let mut kids = vec![for_kw];
        let var_tok = self.next_token(Ctx::Arg);
        kids.push(self.mk("variable_name", var_tok.start, var_tok.end, vec![])?);
        self.skip_blanks();
        let save = self.save();
        let in_tok = self.next_token(Ctx::Arg);
        if in_tok.kind == TokenKind::Word && in_tok.value == "in" {
            kids.push(self.leaf("in", &in_tok)?);
            loop {
                self.skip_blanks();
                let c = self.peek();
                if matches!(c, Some(';') | Some('\n') | None) {
                    break;
                }
                let w = self.parse_word(Ctx::Arg)?;
                let Some(w) = w else { break };
                kids.push(w);
            }
        } else {
            self.restore(save);
        }
        // 分隔符
        let save2 = self.save();
        let sep = self.next_token(Ctx::Cmd);
        if sep.kind == TokenKind::Op && sep.value == ";" {
            kids.push(self.leaf(";", &sep)?);
        } else if sep.kind != TokenKind::Newline {
            self.restore(save2);
        }
        if let Some(dg) = self.parse_do_group()? {
            kids.push(dg);
        }
        let last_end = kids[kids.len() - 1].end_index;
        self.mk("for_statement", for_start, last_end, kids)
    }

    fn parse_do_group(&mut self) -> PResult<Option<TsNode>> {
        self.skip_newlines();
        let save = self.save();
        let do_tok = self.next_token(Ctx::Cmd);
        if do_tok.kind != TokenKind::Word || do_tok.value != "do" {
            self.restore(save);
            return Ok(None);
        }
        let do_kw = self.leaf("do", &do_tok)?;
        let body = self.parse_statements(None)?;
        let mut kids = vec![do_kw.clone()];
        kids.extend(body);
        self.consume_keyword("done", &mut kids)?;
        let do_start = do_kw.start_index;
        let last_end = kids[kids.len() - 1].end_index;
        Ok(Some(self.mk("do_group", do_start, last_end, kids)?))
    }

    fn parse_case(&mut self, case_tok: &Token) -> PResult<TsNode> {
        let case_kw = self.leaf("case", case_tok)?;
        let case_start = case_kw.start_index;
        let mut kids = vec![case_kw];
        self.skip_blanks();
        let word = self.parse_word(Ctx::Arg)?;
        if let Some(word) = word {
            kids.push(word);
        }
        self.skip_blanks();
        self.consume_keyword("in", &mut kids)?;
        self.skip_newlines();
        loop {
            self.skip_blanks();
            self.skip_newlines();
            let save = self.save();
            let t = self.next_token(Ctx::Arg);
            if t.kind == TokenKind::Word && t.value == "esac" {
                kids.push(self.leaf("esac", &t)?);
                break;
            }
            if t.kind == TokenKind::Eof {
                break;
            }
            self.restore(save);
            let item = self.parse_case_item()?;
            let Some(item) = item else { break };
            kids.push(item);
        }
        let last_end = kids[kids.len() - 1].end_index;
        self.mk("case_statement", case_start, last_end, kids)
    }

    fn parse_case_item(&mut self) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        let start = self.lex.i;
        let mut kids = Vec::new();
        // 可选的前置 `(`
        if self.peek() == Some('(') {
            let s = self.lex.i;
            self.advance();
            kids.push(self.mk("(", s, self.lex.i, vec![])?);
        }
        // 模式
        let mut is_first_alt = true;
        loop {
            self.skip_blanks();
            let c = self.peek();
            if c == Some(')') || c.is_none() {
                break;
            }
            let pats = self.parse_case_pattern()?;
            if pats.is_empty() {
                break;
            }
            // tree-sitter 怪癖：第一个带引号的替代项展平为兄弟节点；
            // 后续替代项包进 (concatenation) 且裸段用 word 而非 extglob_pattern
            if !is_first_alt && pats.len() > 1 {
                let mut rewritten = Vec::with_capacity(pats.len());
                for p in pats {
                    if p.node_type == "extglob_pattern" {
                        rewritten.push(self.mk("word", p.start_index, p.end_index, vec![])?);
                    } else {
                        rewritten.push(p);
                    }
                }
                let first = rewritten[0].start_index;
                let last = rewritten[rewritten.len() - 1].end_index;
                kids.push(self.mk("concatenation", first, last, rewritten)?);
            } else {
                kids.extend(pats);
            }
            is_first_alt = false;
            self.skip_blanks();
            // 替代项之间的 \<newline> 行连接
            if self.peek() == Some('\\') && self.peek1() == Some('\n') {
                self.advance();
                self.advance();
                self.skip_blanks();
            }
            if self.peek() == Some('|') {
                let s = self.lex.i;
                self.advance();
                kids.push(self.mk("|", s, self.lex.i, vec![])?);
                // | 后的 \<newline> 也是行连接
                if self.peek() == Some('\\') && self.peek1() == Some('\n') {
                    self.advance();
                    self.advance();
                }
            } else {
                break;
            }
        }
        if self.peek() == Some(')') {
            let s = self.lex.i;
            self.advance();
            kids.push(self.mk(")", s, self.lex.i, vec![])?);
        }
        let body = self.parse_statements(None)?;
        let body_len = body.len();
        kids.extend(body);
        let save = self.save();
        let term = self.next_token(Ctx::Cmd);
        if term.kind == TokenKind::Op && matches!(term.value, ";;" | ";&" | ";;&") {
            kids.push(self.leaf(term.value, &term)?);
        } else {
            self.restore(save);
        }
        if kids.is_empty() {
            return Ok(None);
        }
        // tree-sitter 怪癖：空体 + 单模式且匹配 extglob 算符前缀（无 glob 元字符）时降级为 word
        if body_len == 0 {
            let mut i = 0;
            while i < kids.len() {
                if kids[i].node_type == "extglob_pattern" {
                    let text = self.src()[kids[i].start_index..kids[i].end_index].to_string();
                    if is_extglob_prefix_text(&text) && !has_glob_paren_text(&text) {
                        let s = kids[i].start_index;
                        let e = kids[i].end_index;
                        kids[i] = self.mk("word", s, e, vec![])?;
                    }
                }
                i += 1;
            }
        }
        let last_end = kids[kids.len() - 1].end_index;
        Ok(Some(self.mk("case_item", start, last_end, kids)?))
    }

    fn parse_case_pattern(&mut self) -> PResult<Vec<TsNode>> {
        self.skip_blanks();
        let save = self.save();
        let start = self.lex.i;
        let mut paren_depth = 0;
        let mut has_dollar = false;
        let mut has_bracket_outside_paren = false;
        let mut has_quote = false;
        while self.lex.i < self.src().len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == '\\' {
                if self.peek1().is_some() {
                    self.advance();
                    self.advance();
                    continue;
                }
                self.advance();
                continue;
            }
            if c == '"' || c == '\'' {
                has_quote = true;
                // 跳过引号段，避免其内容（空格 | 等）干扰前瞻扫描
                self.advance();
                while self.lex.i < self.src().len() && self.peek() != Some(c) {
                    if self.peek() == Some('\\') && self.peek1().is_some() {
                        self.advance();
                    }
                    self.advance();
                }
                if self.peek() == Some(c) {
                    self.advance();
                }
                continue;
            }
            // 括号计数：模式内的 ( 开作用域；平衡前不在 ) 或 | 处中断
            if c == '(' {
                paren_depth += 1;
                self.advance();
                continue;
            }
            if paren_depth > 0 {
                if c == ')' {
                    paren_depth -= 1;
                    self.advance();
                    continue;
                }
                if c == '\n' {
                    break;
                }
                self.advance();
                continue;
            }
            if matches!(c, ')' | '|' | ' ' | '\t' | '\n') {
                break;
            }
            if c == '$' {
                has_dollar = true;
            }
            if c == '[' {
                has_bracket_outside_paren = true;
            }
            self.advance();
        }
        if self.lex.i == start {
            return Ok(vec![]);
        }
        let text = self.src()[start..self.lex.i].to_string();
        let has_extglob_paren = has_extglob_paren(&text);
        // 模式中的引号段：tree-sitter 按引号边界拆成多个兄弟节点
        if has_quote && !has_extglob_paren {
            self.restore(save);
            return self.parse_case_pattern_segmented();
        }
        // 含 [ 或 $ 的模式按 word 解析拆成 concatenation —— 除非有 extglob 括号
        if !has_extglob_paren && (has_dollar || has_bracket_outside_paren) {
            self.restore(save);
            let w = self.parse_word(Ctx::Arg)?;
            return Ok(w.map(|x| vec![x]).unwrap_or_default());
        }
        // 以 extglob 算符字符（+ - ? * @ !）开头后跟标识符字符的模式是 extglob_pattern
        let ty = if has_extglob_paren || has_glob_meta(&text) || is_extglob_prefix_text(&text) {
            "extglob_pattern"
        } else {
            "word"
        };
        Ok(vec![self.mk(ty, start, self.lex.i, vec![])?])
    }

    /// 含引号 case 模式的分段扫描：`*"foo"*` → [extglob_pattern, string, extglob_pattern]
    fn parse_case_pattern_segmented(&mut self) -> PResult<Vec<TsNode>> {
        let mut parts = Vec::new();
        let mut seg_start = self.lex.i;
        while self.lex.i < self.src().len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == '\\' && self.peek1().is_some() {
                self.advance();
                self.advance();
                continue;
            }
            if c == '"' {
                if self.lex.i > seg_start {
                    let t = &self.src()[seg_start..self.lex.i];
                    let ty = if has_glob_meta(t) { "extglob_pattern" } else { "word" };
                    parts.push(self.mk(ty, seg_start, self.lex.i, vec![])?);
                }
                parts.push(self.parse_double_quoted()?);
                seg_start = self.lex.i;
                continue;
            }
            if c == '\'' {
                if self.lex.i > seg_start {
                    let t = &self.src()[seg_start..self.lex.i];
                    let ty = if has_glob_meta(t) { "extglob_pattern" } else { "word" };
                    parts.push(self.mk(ty, seg_start, self.lex.i, vec![])?);
                }
                let tok = self.next_token(Ctx::Arg);
                parts.push(self.leaf("raw_string", &tok)?);
                seg_start = self.lex.i;
                continue;
            }
            if matches!(c, ')' | '|' | ' ' | '\t' | '\n') {
                break;
            }
            self.advance();
        }
        if self.lex.i > seg_start {
            let t = &self.src()[seg_start..self.lex.i];
            let ty = if has_glob_meta(t) { "extglob_pattern" } else { "word" };
            parts.push(self.mk(ty, seg_start, self.lex.i, vec![])?);
        }
        Ok(parts)
    }

    fn parse_function(&mut self, fn_tok: &Token) -> PResult<TsNode> {
        let fn_kw = self.leaf("function", fn_tok)?;
        let fn_start = fn_kw.start_index;
        self.skip_blanks();
        let name_tok = self.next_token(Ctx::Arg);
        let name = self.mk("word", name_tok.start, name_tok.end, vec![])?;
        let mut kids = vec![fn_kw.clone(), name];
        self.skip_blanks();
        if self.peek() == Some('(') && self.peek1() == Some(')') {
            let o = self.next_token(Ctx::Cmd);
            let c = self.next_token(Ctx::Cmd);
            kids.push(self.leaf("(", &o)?);
            kids.push(self.leaf(")", &c)?);
        }
        self.skip_blanks();
        self.skip_newlines();
        let body = self.parse_command()?;
        if let Some(body) = body {
            // 把 redirected_statement(compound_statement, ...) 的重定向提升到
            // function_definition 层
            if body.node_type == "redirected_statement"
                && body.children.len() >= 2
                && body.children[0].node_type == "compound_statement"
            {
                kids.extend(body.children);
            } else {
                kids.push(body);
            }
        }
        let last_end = kids[kids.len() - 1].end_index;
        self.mk("function_definition", fn_start, last_end, kids)
    }

    fn parse_declaration(&mut self, kw_tok: &Token) -> PResult<TsNode> {
        let kw = self.leaf(kw_tok.value, kw_tok)?;
        let kw_start = kw.start_index;
        let mut kids = vec![kw];
        loop {
            self.skip_blanks();
            let c = self.peek();
            if matches!(c, None | Some('\n') | Some(';') | Some('&') | Some('|') | Some(')') | Some('<') | Some('>')) {
                break;
            }
            if let Some(a) = self.try_parse_assignment()? {
                kids.push(a);
                continue;
            }
            // 引号字符串或拼接：`export "FOO=bar"`、`export 'X'`
            if matches!(c, Some('"') | Some('\'') | Some('$')) {
                let w = self.parse_word(Ctx::Arg)?;
                if let Some(w) = w {
                    kids.push(w);
                    continue;
                }
                break;
            }
            // -a 之类的 flag 或裸变量名
            let save = self.save();
            let tok = self.next_token(Ctx::Arg);
            if tok.kind == TokenKind::Word || tok.kind == TokenKind::Number {
                if tok.value.starts_with('-') {
                    kids.push(self.leaf("word", &tok)?);
                } else if tok.value.chars().next().map(is_ident_start).unwrap_or(false) {
                    kids.push(self.mk("variable_name", tok.start, tok.end, vec![])?);
                } else {
                    kids.push(self.leaf("word", &tok)?);
                }
            } else {
                self.restore(save);
                break;
            }
        }
        let last_end = kids[kids.len() - 1].end_index;
        self.mk("declaration_command", kw_start, last_end, kids)
    }

    fn parse_unset(&mut self, kw_tok: &Token) -> PResult<TsNode> {
        let kw = self.leaf("unset", kw_tok)?;
        let kw_start = kw.start_index;
        let mut kids = vec![kw];
        loop {
            self.skip_blanks();
            let c = self.peek();
            if matches!(c, None | Some('\n') | Some(';') | Some('&') | Some('|') | Some(')') | Some('<') | Some('>')) {
                break;
            }
            // 用 parseWord（而非裸 nextToken）让 `unset 'a[$(id)]'` 输出 raw_string
            // 子节点，ast.ts 才能拦截算术下标代码执行向量
            let arg = self.parse_word(Ctx::Arg)?;
            let Some(arg) = arg else { break };
            if arg.node_type == "word" {
                if arg.text.starts_with('-') {
                    kids.push(arg);
                } else {
                    let s = arg.start_index;
                    let e = arg.end_index;
                    kids.push(self.mk("variable_name", s, e, vec![])?);
                }
            } else {
                kids.push(arg);
            }
        }
        let last_end = kids[kids.len() - 1].end_index;
        self.mk("unset_command", kw_start, last_end, kids)
    }

    fn consume_keyword(&mut self, name: &str, kids: &mut Vec<TsNode>) -> PResult<()> {
        self.skip_newlines();
        let save = self.save();
        let t = self.next_token(Ctx::Cmd);
        if t.kind == TokenKind::Word && t.value == name {
            kids.push(self.leaf(name, &t)?);
        } else {
            self.restore(save);
        }
        Ok(())
    }

    // ───────────────────── Test & Arithmetic Expressions ─────────────────────

    fn parse_test_expr(&mut self, closer: &str) -> PResult<Option<TsNode>> {
        self.parse_test_or(closer)
    }

    fn parse_test_or(&mut self, closer: &str) -> PResult<Option<TsNode>> {
        let mut left = self.parse_test_and(closer)?;
        if left.is_none() {
            return Ok(None);
        }
        loop {
            self.skip_blanks();
            let save = self.save();
            if self.peek() == Some('|') && self.peek1() == Some('|') {
                let s = self.lex.i;
                self.advance();
                self.advance();
                let op = self.mk("||", s, self.lex.i, vec![])?;
                let right = self.parse_test_and(closer)?;
                let Some(right) = right else {
                    self.restore(save);
                    break;
                };
                let Some(l) = left.take() else { return Ok(None) };
                left = Some(self.mk("binary_expression", l.start_index, right.end_index, vec![l, op, right])?);
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_test_and(&mut self, closer: &str) -> PResult<Option<TsNode>> {
        let mut left = self.parse_test_unary(closer)?;
        if left.is_none() {
            return Ok(None);
        }
        loop {
            self.skip_blanks();
            if self.peek() == Some('&') && self.peek1() == Some('&') {
                let s = self.lex.i;
                self.advance();
                self.advance();
                let op = self.mk("&&", s, self.lex.i, vec![])?;
                let right = self.parse_test_unary(closer)?;
                let Some(right) = right else { break };
                let Some(l) = left.take() else { return Ok(None) };
                left = Some(self.mk("binary_expression", l.start_index, right.end_index, vec![l, op, right])?);
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_test_unary(&mut self, closer: &str) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        if self.peek() == Some('(') {
            let s = self.lex.i;
            self.advance();
            let open = self.mk("(", s, self.lex.i, vec![])?;
            let inner = self.parse_test_or(closer)?;
            self.skip_blanks();
            let close = if self.peek() == Some(')') {
                let cs = self.lex.i;
                self.advance();
                self.mk(")", cs, self.lex.i, vec![])?
            } else {
                self.mk(")", self.lex.i, self.lex.i, vec![])?
            };
            let open_start = open.start_index;
            let close_end = close.end_index;
            let mut kids = vec![open];
            if let Some(inner) = inner {
                kids.push(inner);
            }
            kids.push(close);
            return Ok(Some(self.mk("parenthesized_expression", open_start, close_end, kids)?));
        }
        self.parse_test_binary(closer)
    }

    /// `!` 取反 / test 算符 / 括号 —— 但不是二院比较式
    fn parse_test_negatable_primary(&mut self, closer: &str) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        let c = self.peek();
        if c == Some('!') {
            let s = self.lex.i;
            self.advance();
            let bang = self.mk("!", s, self.lex.i, vec![])?;
            let inner = self.parse_test_negatable_primary(closer)?;
            let Some(inner) = inner else { return Ok(Some(bang)) };
            return Ok(Some(self.mk("unary_expression", bang.start_index, inner.end_index, vec![bang, inner])?));
        }
        if c == Some('-') && matches!(self.peek1(), Some(c) if is_ident_start(c)) {
            let s = self.lex.i;
            self.advance();
            while matches!(self.peek(), Some(c) if is_ident_char(c)) {
                self.advance();
            }
            let op = self.mk("test_operator", s, self.lex.i, vec![])?;
            self.skip_blanks();
            let arg = self.parse_test_primary(closer)?;
            let Some(arg) = arg else { return Ok(Some(op)) };
            return Ok(Some(self.mk("unary_expression", op.start_index, arg.end_index, vec![op, arg])?));
        }
        self.parse_test_primary(closer)
    }

    fn parse_test_binary(&mut self, closer: &str) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        let left = self.parse_test_negatable_primary(closer)?;
        let Some(left) = left else { return Ok(None) };
        self.skip_blanks();
        // 二院比较：== != =~ -eq -lt 等
        let c = self.peek();
        let c1 = self.peek1();
        let mut op = None;
        let os = self.lex.i;
        if c == Some('=') && c1 == Some('=') {
            self.advance();
            self.advance();
            op = Some(self.mk("==", os, self.lex.i, vec![])?);
        } else if c == Some('!') && c1 == Some('=') {
            self.advance();
            self.advance();
            op = Some(self.mk("!=", os, self.lex.i, vec![])?);
        } else if c == Some('=') && c1 == Some('~') {
            self.advance();
            self.advance();
            op = Some(self.mk("=~", os, self.lex.i, vec![])?);
        } else if c == Some('=') {
            self.advance();
            op = Some(self.mk("=", os, self.lex.i, vec![])?);
        } else if c == Some('<') && c1 != Some('<') {
            self.advance();
            op = Some(self.mk("<", os, self.lex.i, vec![])?);
        } else if c == Some('>') && c1 != Some('>') {
            self.advance();
            op = Some(self.mk(">", os, self.lex.i, vec![])?);
        } else if c == Some('-') && matches!(c1, Some(c) if is_ident_start(c)) {
            self.advance();
            while matches!(self.peek(), Some(c) if is_ident_char(c)) {
                self.advance();
            }
            op = Some(self.mk("test_operator", os, self.lex.i, vec![])?);
        }
        let Some(op) = op else { return Ok(Some(left)) };
        self.skip_blanks();
        // [[ ]] 中 ==/!=/=/=~ 的 RHS 用特殊模式解析
        if closer == "]]" {
            let op_text = op.node_type.clone();
            if op_text == "=~" {
                self.skip_blanks();
                // RHS 整体是引号字符串时输出 string/raw_string 而非 regex
                let rc = self.peek();
                let mut rhs = None;
                if rc == Some('"') || rc == Some('\'') {
                    let save = self.save();
                    let quoted = if rc == Some('"') {
                        self.parse_double_quoted()?
                    } else {
                        let tok = self.next_token(Ctx::Arg);
                        self.leaf("raw_string", &tok)?
                    };
                    // 检查 RHS 是否到此为止：只有空白然后是 ]] / && / || / 换行
                    let bytes = self.src().as_bytes();
                    let mut j = self.lex.i;
                    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                        j += 1;
                    }
                    let nc = bytes.get(j).copied().unwrap_or(0);
                    let nc1 = bytes.get(j + 1).copied().unwrap_or(0);
                    if (nc == b']' && nc1 == b']')
                        || (nc == b'&' && nc1 == b'&')
                        || (nc == b'|' && nc1 == b'|')
                        || nc == b'\n'
                        || j >= bytes.len()
                    {
                        rhs = Some(quoted);
                    } else {
                        self.restore(save);
                    }
                }
                if rhs.is_none() {
                    rhs = self.parse_test_regex_rhs()?;
                }
                let Some(rhs) = rhs else { return Ok(Some(left)) };
                return Ok(Some(self.mk("binary_expression", left.start_index, rhs.end_index, vec![left, op, rhs])?));
            }
            // 单 `=` 输出 (regex)；`==` `!=` 输出 extglob_pattern
            if op_text == "=" {
                let rhs = self.parse_test_regex_rhs()?;
                let Some(rhs) = rhs else { return Ok(Some(left)) };
                return Ok(Some(self.mk("binary_expression", left.start_index, rhs.end_index, vec![left, op, rhs])?));
            }
            if op_text == "==" || op_text == "!=" {
                let parts = self.parse_test_extglob_rhs()?;
                if parts.is_empty() {
                    return Ok(Some(left));
                }
                let last_end = parts[parts.len() - 1].end_index;
                let mut kids = vec![left, op];
                kids.extend(parts);
                return Ok(Some(self.mk("binary_expression", kids[0].start_index, last_end, kids)?));
            }
        }
        let right = self.parse_test_primary(closer)?;
        let Some(right) = right else { return Ok(Some(left)) };
        Ok(Some(self.mk("binary_expression", left.start_index, right.end_index, vec![left, op, right])?))
    }

    /// [[ ]] 中 =~ 的 RHS —— 扫描为单个 (regex)，括号/方括号计数避免 | ( ) 破坏解析
    fn parse_test_regex_rhs(&mut self) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        let start = self.lex.i;
        let mut paren_depth = 0;
        let mut bracket_depth = 0;
        while self.lex.i < self.src().len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == '\\' && self.peek1().is_some() {
                self.advance();
                self.advance();
                continue;
            }
            if c == '\n' {
                break;
            }
            if paren_depth == 0 && bracket_depth == 0 {
                if c == ']' && self.peek1() == Some(']') {
                    break;
                }
                if c == ' ' || c == '\t' {
                    // 前瞻空白后的 ]] 或 && / ||
                    let bytes = self.src().as_bytes();
                    let mut j = self.lex.i;
                    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                        j += 1;
                    }
                    let nc = bytes.get(j).copied().unwrap_or(0);
                    let nc1 = bytes.get(j + 1).copied().unwrap_or(0);
                    if (nc == b']' && nc1 == b']')
                        || (nc == b'&' && nc1 == b'&')
                        || (nc == b'|' && nc1 == b'|')
                    {
                        break;
                    }
                    self.advance();
                    continue;
                }
            }
            if c == '(' {
                paren_depth += 1;
            } else if c == ')' && paren_depth > 0 {
                paren_depth -= 1;
            } else if c == '[' {
                bracket_depth += 1;
            } else if c == ']' && bracket_depth > 0 {
                bracket_depth -= 1;
            }
            self.advance();
        }
        if self.lex.i == start {
            return Ok(None);
        }
        Ok(Some(self.mk("regex", start, self.lex.i, vec![])?))
    }

    /// [[ ]] 中 ==/!=/= 的 RHS —— 返回片段数组；裸文本 → extglob_pattern
    fn parse_test_extglob_rhs(&mut self) -> PResult<Vec<TsNode>> {
        self.skip_blanks();
        let mut parts = Vec::new();
        let mut seg_start = self.lex.i;
        let mut paren_depth = 0;
        while self.lex.i < self.src().len() {
            let c = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if c == '\\' && self.peek1().is_some() {
                self.advance();
                self.advance();
                continue;
            }
            if c == '\n' {
                break;
            }
            if paren_depth == 0 {
                if c == ']' && self.peek1() == Some(']') {
                    break;
                }
                if c == ' ' || c == '\t' {
                    let bytes = self.src().as_bytes();
                    let mut j = self.lex.i;
                    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                        j += 1;
                    }
                    let nc = bytes.get(j).copied().unwrap_or(0);
                    let nc1 = bytes.get(j + 1).copied().unwrap_or(0);
                    if (nc == b']' && nc1 == b']')
                        || (nc == b'&' && nc1 == b'&')
                        || (nc == b'|' && nc1 == b'|')
                    {
                        break;
                    }
                    self.advance();
                    continue;
                }
            }
            // $ " ' 即使在 @( ) extglob 括号内也要解析
            let c1 = self.peek1();
            if c == '$' {
                if c1 == Some('(')
                    || c1 == Some('{')
                    || matches!(c1, Some(c) if is_ident_start(c))
                    || matches!(c1, Some(c) if is_special_var(c))
                {
                    if self.lex.i > seg_start {
                        let t = &self.src()[seg_start..self.lex.i];
                        let ty = if is_digits_text(t) { "number" } else { "extglob_pattern" };
                        parts.push(self.mk(ty, seg_start, self.lex.i, vec![])?);
                    }
                    let exp = self.parse_dollar_like()?;
                    if let Some(exp) = exp {
                        parts.push(exp);
                    }
                    seg_start = self.lex.i;
                    continue;
                }
            }
            if c == '"' {
                if self.lex.i > seg_start {
                    let t = &self.src()[seg_start..self.lex.i];
                    let ty = if is_digits_text(t) { "number" } else { "extglob_pattern" };
                    parts.push(self.mk(ty, seg_start, self.lex.i, vec![])?);
                }
                parts.push(self.parse_double_quoted()?);
                seg_start = self.lex.i;
                continue;
            }
            if c == '\'' {
                if self.lex.i > seg_start {
                    let t = &self.src()[seg_start..self.lex.i];
                    let ty = if is_digits_text(t) { "number" } else { "extglob_pattern" };
                    parts.push(self.mk(ty, seg_start, self.lex.i, vec![])?);
                }
                let tok = self.next_token(Ctx::Arg);
                parts.push(self.leaf("raw_string", &tok)?);
                seg_start = self.lex.i;
                continue;
            }
            if c == '(' {
                paren_depth += 1;
            } else if c == ')' && paren_depth > 0 {
                paren_depth -= 1;
            }
            self.advance();
        }
        if self.lex.i > seg_start {
            let t = &self.src()[seg_start..self.lex.i];
            let ty = if is_digits_text(t) { "number" } else { "extglob_pattern" };
            parts.push(self.mk(ty, seg_start, self.lex.i, vec![])?);
        }
        Ok(parts)
    }

    fn parse_test_primary(&mut self, closer: &str) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        if closer == "]" && self.peek() == Some(']') {
            return Ok(None);
        }
        if closer == "]]" && self.peek() == Some(']') && self.peek1() == Some(']') {
            return Ok(None);
        }
        self.parse_word(Ctx::Arg)
    }

    // ───────────────────────── Arithmetic ─────────────────────────

    fn parse_arith_expr(&mut self, stop: &str, mode: ArithMode) -> PResult<Option<TsNode>> {
        self.parse_arith_ternary(stop, mode)
    }

    /// 顶层：逗号分隔列表
    fn parse_arith_comma_list(&mut self, stop: &str, mode: ArithMode) -> PResult<Vec<TsNode>> {
        let mut out = Vec::new();
        loop {
            let e = self.parse_arith_ternary(stop, mode)?;
            if let Some(e) = e {
                out.push(e);
            }
            self.skip_blanks();
            if self.peek() == Some(',') && !self.is_arith_stop(stop) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(out)
    }

    fn parse_arith_ternary(&mut self, stop: &str, mode: ArithMode) -> PResult<Option<TsNode>> {
        let cond = self.parse_arith_binary(stop, 0, mode)?;
        let Some(cond) = cond else { return Ok(None) };
        self.skip_blanks();
        if self.peek() == Some('?') {
            let qs = self.lex.i;
            self.advance();
            let q = self.mk("?", qs, self.lex.i, vec![])?;
            let t = self.parse_arith_binary(":", 0, mode)?;
            self.skip_blanks();
            let colon = if self.peek() == Some(':') {
                let cs = self.lex.i;
                self.advance();
                self.mk(":", cs, self.lex.i, vec![])?
            } else {
                self.mk(":", self.lex.i, self.lex.i, vec![])?
            };
            let f = self.parse_arith_ternary(stop, mode)?;
            let last_end = f.as_ref().map(|x| x.end_index).unwrap_or(colon.end_index);
            let mut kids = vec![cond, q];
            if let Some(t) = t {
                kids.push(t);
            }
            kids.push(colon);
            if let Some(f) = f {
                kids.push(f);
            }
            return Ok(Some(self.mk("ternary_expression", kids[0].start_index, last_end, kids)?));
        }
        Ok(Some(cond))
    }

    /// 扫描下一个算术二院算符，返回 (文本, 长度)
    fn scan_arith_op(&self) -> Option<(&'static str, usize)> {
        let c = self.peek()?;
        let c1 = self.peek1();
        let c2 = self.peek2();
        Some(match (c, c1, c2) {
            ('<', Some('<'), Some('=')) => ("<<=", 3),
            ('>', Some('>'), Some('=')) => (">>=", 3),
            ('*', Some('*'), _) => ("**", 2),
            ('<', Some('<'), _) => ("<<", 2),
            ('>', Some('>'), _) => (">>", 2),
            ('=', Some('='), _) => ("==", 2),
            ('!', Some('='), _) => ("!=", 2),
            ('<', Some('='), _) => ("<=", 2),
            ('>', Some('='), _) => (">=", 2),
            ('&', Some('&'), _) => ("&&", 2),
            ('|', Some('|'), _) => ("||", 2),
            ('+', Some('='), _) => ("+=", 2),
            ('-', Some('='), _) => ("-=", 2),
            ('*', Some('='), _) => ("*=", 2),
            ('/', Some('='), _) => ("/=", 2),
            ('%', Some('='), _) => ("%=", 2),
            ('&', Some('='), _) => ("&=", 2),
            ('^', Some('='), _) => ("^=", 2),
            ('|', Some('='), _) => ("|=", 2),
            ('+', Some(c1v), _) if c1v != '+' => ("+", 1),
            ('-', Some(c1v), _) if c1v != '-' => ("-", 1),
            ('*', _, _) => ("*", 1),
            ('/', _, _) => ("/", 1),
            ('%', _, _) => ("%", 1),
            ('<', _, _) => ("<", 1),
            ('>', _, _) => (">", 1),
            ('&', _, _) => ("&", 1),
            ('|', _, _) => ("|", 1),
            ('^', _, _) => ("^", 1),
            ('=', _, _) => ("=", 1),
            _ => return None,
        })
    }

    /// 优先级爬升的二院表达式解析
    fn parse_arith_binary(
        &mut self,
        stop: &str,
        min_prec: u8,
        mode: ArithMode,
    ) -> PResult<Option<TsNode>> {
        let mut left = self.parse_arith_unary(stop, mode)?;
        if left.is_none() {
            return Ok(None);
        }
        loop {
            self.skip_blanks();
            if self.is_arith_stop(stop) {
                break;
            }
            if self.peek() == Some(',') {
                break;
            }
            let op_info = self.scan_arith_op();
            let Some((op_text, op_len)) = op_info else { break };
            let Some(prec) = arith_prec(op_text) else { break };
            if prec < min_prec {
                break;
            }
            let os = self.lex.i;
            for _ in 0..op_len {
                self.advance();
            }
            let op = self.mk(op_text, os, self.lex.i, vec![])?;
            let next_min = if is_arith_right_assoc(op_text) { prec } else { prec + 1 };
            let right = self.parse_arith_binary(stop, next_min, mode)?;
            let Some(right) = right else { break };
            let Some(l) = left.take() else { return Ok(None) };
            left = Some(self.mk("binary_expression", l.start_index, right.end_index, vec![l, op, right])?);
        }
        Ok(left)
    }

    fn parse_arith_unary(&mut self, stop: &str, mode: ArithMode) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        if self.is_arith_stop(stop) {
            return Ok(None);
        }
        let c = self.peek();
        let c1 = self.peek1();
        // 前缀 ++ --
        if matches!((c, c1), (Some('+'), Some('+')) | (Some('-'), Some('-'))) {
            let s = self.lex.i;
            self.advance();
            self.advance();
            let op = if c == Some('+') {
                self.mk("++", s, self.lex.i, vec![])?
            } else {
                self.mk("--", s, self.lex.i, vec![])?
            };
            let inner = self.parse_arith_unary(stop, mode)?;
            let Some(inner) = inner else { return Ok(Some(op)) };
            return Ok(Some(self.mk("unary_expression", op.start_index, inner.end_index, vec![op, inner])?));
        }
        if let Some(c) = c {
            if !matches!(c, '-' | '+' | '!' | '~') {
                return self.parse_arith_postfix(stop, mode);
            }
            // word/assign 模式（c 风格 for 头）中 `-N` 是单个数字字面量
            if mode != ArithMode::Var && c == '-' && matches!(c1, Some(c) if is_digit(c)) {
                let s = self.lex.i;
                self.advance();
                while matches!(self.peek(), Some(c) if is_digit(c)) {
                    self.advance();
                }
                return Ok(Some(self.mk("number", s, self.lex.i, vec![])?));
            }
            let s = self.lex.i;
            self.advance();
            let op = self.mk_char(c, s, self.lex.i, vec![])?;
            let inner = self.parse_arith_unary(stop, mode)?;
            let Some(inner) = inner else { return Ok(Some(op)) };
            return Ok(Some(self.mk("unary_expression", op.start_index, inner.end_index, vec![op, inner])?));
        }
        self.parse_arith_postfix(stop, mode)
    }

    fn parse_arith_postfix(&mut self, stop: &str, mode: ArithMode) -> PResult<Option<TsNode>> {
        let prim = self.parse_arith_primary(stop, mode)?;
        let Some(prim) = prim else { return Ok(None) };
        let c = self.peek();
        let c1 = self.peek1();
        if matches!((c, c1), (Some('+'), Some('+')) | (Some('-'), Some('-'))) {
            let s = self.lex.i;
            self.advance();
            self.advance();
            let op = if c == Some('+') {
                self.mk("++", s, self.lex.i, vec![])?
            } else {
                self.mk("--", s, self.lex.i, vec![])?
            };
            return Ok(Some(self.mk("postfix_expression", prim.start_index, op.end_index, vec![prim, op])?));
        }
        Ok(Some(prim))
    }

    fn parse_arith_primary(&mut self, stop: &str, mode: ArithMode) -> PResult<Option<TsNode>> {
        self.skip_blanks();
        if self.is_arith_stop(stop) {
            return Ok(None);
        }
        let c = self.peek();
        if c == Some('(') {
            let s = self.lex.i;
            self.advance();
            let open = self.mk("(", s, self.lex.i, vec![])?;
            let inners = self.parse_arith_comma_list(")", mode)?;
            self.skip_blanks();
            let close = if self.peek() == Some(')') {
                let cs = self.lex.i;
                self.advance();
                self.mk(")", cs, self.lex.i, vec![])?
            } else {
                self.mk(")", self.lex.i, self.lex.i, vec![])?
            };
            let open_start = open.start_index;
            let close_end = close.end_index;
            let mut kids = vec![open];
            kids.extend(inners);
            kids.push(close);
            return Ok(Some(self.mk("parenthesized_expression", open_start, close_end, kids)?));
        }
        if c == Some('"') {
            return Ok(Some(self.parse_double_quoted()?));
        }
        if c == Some('$') {
            return self.parse_dollar_like();
        }
        if matches!(c, Some(c) if is_digit(c)) {
            let s = self.lex.i;
            while matches!(self.peek(), Some(c) if is_digit(c)) {
                self.advance();
            }
            // 十六进制 0x1f
            if self.lex.i - s == 1
                && c == Some('0')
                && matches!(self.peek(), Some('x') | Some('X'))
            {
                self.advance();
                while matches!(self.peek(), Some(c) if is_hex_digit(c)) {
                    self.advance();
                }
            }
            // 进制记法 BASE#DIGITS，如 2#1010、16#ff
            else if self.peek() == Some('#') {
                self.advance();
                while matches!(self.peek(), Some(c) if is_base_digit(c)) {
                    self.advance();
                }
            }
            return Ok(Some(self.mk("number", s, self.lex.i, vec![])?));
        }
        if matches!(c, Some(c) if is_ident_start(c)) {
            let s = self.lex.i;
            while matches!(self.peek(), Some(c) if is_ident_char(c)) {
                self.advance();
            }
            let nc = self.peek();
            // assign 模式（c 风格 for init）：输出 variable_assignment，
            // 使 `a = b = c = 1` 链式嵌套
            if mode == ArithMode::Assign {
                self.skip_blanks();
                let ac = self.peek();
                let ac1 = self.peek1();
                if ac == Some('=') && ac1 != Some('=') {
                    let vn = self.mk("variable_name", s, self.lex.i, vec![])?;
                    let es = self.lex.i;
                    self.advance();
                    let eq = self.mk("=", es, self.lex.i, vec![])?;
                    let val = self.parse_arith_ternary(stop, mode)?;
                    let end = val.as_ref().map(|v| v.end_index).unwrap_or(eq.end_index);
                    let mut kids = vec![vn, eq];
                    if let Some(val) = val {
                        kids.push(val);
                    }
                    return Ok(Some(self.mk("variable_assignment", s, end, kids)?));
                }
            }
            // 下标
            if nc == Some('[') {
                let vn = self.mk("variable_name", s, self.lex.i, vec![])?;
                let br_s = self.lex.i;
                self.advance();
                let br_open = self.mk("[", br_s, self.lex.i, vec![])?;
                let idx = self.parse_arith_ternary("]", ArithMode::Var)?.or(self.parse_dollar_like()?);
                self.skip_blanks();
                let br_close = if self.peek() == Some(']') {
                    let cs = self.lex.i;
                    self.advance();
                    self.mk("]", cs, self.lex.i, vec![])?
                } else {
                    self.mk("]", self.lex.i, self.lex.i, vec![])?
                };
                let br_close_end = br_close.end_index;
                let mut kids = vec![vn, br_open];
                if let Some(idx) = idx {
                    kids.push(idx);
                }
                kids.push(br_close);
                return Ok(Some(self.mk("subscript", s, br_close_end, kids)?));
            }
            // 裸标识符：var 模式 → variable_name；word/assign 模式 → word
            let ident_type = if mode == ArithMode::Var { "variable_name" } else { "word" };
            return Ok(Some(self.mk(ident_type, s, self.lex.i, vec![])?));
        }
        Ok(None)
    }

    fn is_arith_stop(&self, stop: &str) -> bool {
        match stop {
            "))" => self.peek() == Some(')') && self.peek1() == Some(')'),
            ")" => self.peek() == Some(')'),
            ";" => self.peek() == Some(';'),
            ":" => self.peek() == Some(':'),
            "]" => self.peek() == Some(']'),
            "}" => self.peek() == Some('}'),
            ":}" => self.peek() == Some(':') || self.peek() == Some('}'),
            _ => matches!(self.peek(), None | Some('\n')),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArithMode {
    Var,
    Word,
    Assign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestMode {
    Regex,
    Word,
    ReplWord,
}

/// 算术算符优先级（越高结合越紧）
fn arith_prec(op: &str) -> Option<u8> {
    Some(match op {
        "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "^=" | "|=" => 2,
        "||" => 4,
        "&&" => 5,
        "|" => 6,
        "^" => 7,
        "&" => 8,
        "==" | "!=" => 9,
        "<" | ">" | "<=" | ">=" => 10,
        "<<" | ">>" => 11,
        "+" | "-" => 12,
        "*" | "/" | "%" => 13,
        "**" => 14,
        _ => return None,
    })
}

/// 右结合算符（赋值与幂）
fn is_arith_right_assoc(op: &str) -> bool {
    matches!(
        op,
        "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "^=" | "|=" | "**"
    )
}
