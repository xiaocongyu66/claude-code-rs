//! ansi_to_svg.rs — ANSI 转义终端文本 → SVG。
//!
//! 重写自 `src/utils/ansiToSvg.ts`。支持基本 ANSI 前景色码、
//! 256 色（38;5;n）与 24 位真彩（38;2;r;g;b），用 `<tspan>` 渲染
//! 彩色片段（SVG 渲染器自带字符定位，无需手算字宽）。
//! XML 转义复用 `xml.rs`。

use crate::utils::xml::escape_xml;

/// RGB 颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnsiColor {
    pub r: u32,
    pub g: u32,
    pub b: u32,
}

/// 默认终端颜色表（30-37、90-97）。
const ANSI_COLORS: &[(u32, AnsiColor)] = &[
    (30, AnsiColor { r: 0, g: 0, b: 0 }), // black
    (
        31,
        AnsiColor {
            r: 205,
            g: 49,
            b: 49,
        },
    ), // red
    (
        32,
        AnsiColor {
            r: 13,
            g: 188,
            b: 121,
        },
    ), // green
    (
        33,
        AnsiColor {
            r: 229,
            g: 229,
            b: 16,
        },
    ), // yellow
    (
        34,
        AnsiColor {
            r: 36,
            g: 114,
            b: 200,
        },
    ), // blue
    (
        35,
        AnsiColor {
            r: 188,
            g: 63,
            b: 188,
        },
    ), // magenta
    (
        36,
        AnsiColor {
            r: 17,
            g: 168,
            b: 205,
        },
    ), // cyan
    (
        37,
        AnsiColor {
            r: 229,
            g: 229,
            b: 229,
        },
    ), // white
    // 亮色
    (
        90,
        AnsiColor {
            r: 102,
            g: 102,
            b: 102,
        },
    ), // bright black (gray)
    (
        91,
        AnsiColor {
            r: 241,
            g: 76,
            b: 76,
        },
    ), // bright red
    (
        92,
        AnsiColor {
            r: 35,
            g: 209,
            b: 139,
        },
    ), // bright green
    (
        93,
        AnsiColor {
            r: 245,
            g: 245,
            b: 67,
        },
    ), // bright yellow
    (
        94,
        AnsiColor {
            r: 59,
            g: 142,
            b: 234,
        },
    ), // bright blue
    (
        95,
        AnsiColor {
            r: 214,
            g: 112,
            b: 214,
        },
    ), // bright magenta
    (
        96,
        AnsiColor {
            r: 41,
            g: 184,
            b: 219,
        },
    ), // bright cyan
    (
        97,
        AnsiColor {
            r: 255,
            g: 255,
            b: 255,
        },
    ), // bright white
];

pub const DEFAULT_FG: AnsiColor = AnsiColor {
    r: 229,
    g: 229,
    b: 229,
}; // 浅灰
pub const DEFAULT_BG: AnsiColor = AnsiColor {
    r: 30,
    g: 30,
    b: 30,
}; // 深灰

/// 单个相同样式的文本片段。
#[derive(Debug, Clone)]
pub struct TextSpan {
    pub text: String,
    pub color: AnsiColor,
    pub bold: bool,
}

/// `ansi_to_svg` 的选项；字段为 `None` 时使用默认值。
#[derive(Debug, Clone)]
pub struct AnsiToSvgOptions {
    pub font_family: Option<String>,
    pub font_size: Option<f64>,
    pub line_height: Option<f64>,
    pub padding_x: Option<f64>,
    pub padding_y: Option<f64>,
    pub background_color: Option<String>,
    pub border_radius: Option<f64>,
}

impl Default for AnsiToSvgOptions {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: None,
            line_height: None,
            padding_x: None,
            padding_y: None,
            background_color: None,
            border_radius: None,
        }
    }
}

/// 从 256 色调色板取色。
fn get_256_color(index: u32) -> AnsiColor {
    if index < 16 {
        // 标准色 0-15
        return match index {
            0 => AnsiColor { r: 0, g: 0, b: 0 },   // black
            1 => AnsiColor { r: 128, g: 0, b: 0 }, // red
            2 => AnsiColor { r: 0, g: 128, b: 0 }, // green
            3 => AnsiColor {
                r: 128,
                g: 128,
                b: 0,
            }, // yellow
            4 => AnsiColor { r: 0, g: 0, b: 128 }, // blue
            5 => AnsiColor {
                r: 128,
                g: 0,
                b: 128,
            }, // magenta
            6 => AnsiColor {
                r: 0,
                g: 128,
                b: 128,
            }, // cyan
            7 => AnsiColor {
                r: 192,
                g: 192,
                b: 192,
            }, // white
            8 => AnsiColor {
                r: 128,
                g: 128,
                b: 128,
            }, // bright black
            9 => AnsiColor { r: 255, g: 0, b: 0 }, // bright red
            10 => AnsiColor { r: 0, g: 255, b: 0 }, // bright green
            11 => AnsiColor {
                r: 255,
                g: 255,
                b: 0,
            }, // bright yellow
            12 => AnsiColor { r: 0, g: 0, b: 255 }, // bright blue
            13 => AnsiColor {
                r: 255,
                g: 0,
                b: 255,
            }, // bright magenta
            14 => AnsiColor {
                r: 0,
                g: 255,
                b: 255,
            }, // bright cyan
            _ => AnsiColor {
                r: 255,
                g: 255,
                b: 255,
            }, // bright white
        };
    }

    // 216 色立方体（16-231）
    if index < 232 {
        let i = index - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        return AnsiColor {
            r: if r == 0 { 0 } else { 55 + r * 40 },
            g: if g == 0 { 0 } else { 55 + g * 40 },
            b: if b == 0 { 0 } else { 55 + b * 40 },
        };
    }

    // 灰阶（232-255）
    let gray = (index - 232) * 10 + 8;
    AnsiColor {
        r: gray,
        g: gray,
        b: gray,
    }
}

fn lookup_basic_color(code: u32) -> Option<AnsiColor> {
    ANSI_COLORS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, color)| *color)
}

/// 解析 ANSI 转义序列：基本色（30-37、90-97）、256 色、24 位真彩。
pub fn parse_ansi(text: &str) -> Vec<Vec<TextSpan>> {
    let mut lines: Vec<Vec<TextSpan>> = Vec::new();

    for line in text.split('\n') {
        let mut spans: Vec<TextSpan> = Vec::new();
        let mut current_color = DEFAULT_FG;
        let mut bold = false;
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // ANSI 转义序列：ESC [ ... <final letter>
            if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '[' {
                let mut j = i + 2;
                while j < chars.len() && !chars[j].is_ascii_alphabetic() {
                    j += 1;
                }

                if j < chars.len() && chars[j] == 'm' {
                    // 颜色/样式码
                    let codes: Vec<i64> = chars[i + 2..j]
                        .iter()
                        .collect::<String>()
                        .split(';')
                        .map(|s| {
                            if s.is_empty() {
                                0 // 与 JS Number("")=0 对齐
                            } else {
                                s.parse().unwrap_or(0)
                            }
                        })
                        .collect();

                    let mut k = 0;
                    while k < codes.len() {
                        let code = codes[k];
                        if code == 0 {
                            // Reset
                            current_color = DEFAULT_FG;
                            bold = false;
                        } else if code == 1 {
                            bold = true;
                        } else if (30..=37).contains(&code) || (90..=97).contains(&code) {
                            current_color = lookup_basic_color(code as u32).unwrap_or(DEFAULT_FG);
                        } else if code == 39 {
                            current_color = DEFAULT_FG;
                        } else if code == 38 {
                            // 扩展色
                            if codes.get(k + 1) == Some(&5) && k + 2 < codes.len() {
                                // 256 色：38;5;n
                                let color_index = codes[k + 2] as u32;
                                current_color = get_256_color(color_index);
                                k += 2;
                            } else if codes.get(k + 1) == Some(&2) && k + 4 < codes.len() {
                                // 24 位真彩：38;2;r;g;b
                                current_color = AnsiColor {
                                    r: codes[k + 2] as u32,
                                    g: codes[k + 3] as u32,
                                    b: codes[k + 4] as u32,
                                };
                                k += 4;
                            }
                        }
                        k += 1;
                    }
                }

                i = j + 1;
                continue;
            }

            // 普通字符 —— 找到同样式文本的连续区间
            let text_start = i;
            while i < chars.len() && chars[i] != '\u{1b}' {
                i += 1;
            }
            let span_text: String = chars[text_start..i].iter().collect();
            if !span_text.is_empty() {
                spans.push(TextSpan {
                    text: span_text,
                    color: current_color,
                    bold,
                });
            }
        }

        // 空行保留一个空 span 以维持行数
        if spans.is_empty() {
            spans.push(TextSpan {
                text: String::new(),
                color: DEFAULT_FG,
                bold: false,
            });
        }

        lines.push(spans);
    }

    lines
}

/// 把 ANSI 文本转换为 SVG。
pub fn ansi_to_svg(ansi_text: &str, options: &AnsiToSvgOptions) -> String {
    let font_family = options
        .font_family
        .as_deref()
        .unwrap_or("Menlo, Monaco, monospace");
    let font_size = options.font_size.unwrap_or(14.0);
    let line_height = options.line_height.unwrap_or(22.0);
    let padding_x = options.padding_x.unwrap_or(24.0);
    let padding_y = options.padding_y.unwrap_or(24.0);
    let background_color = options.background_color.as_deref().unwrap_or(&format!(
        "rgb({}, {}, {})",
        DEFAULT_BG.r, DEFAULT_BG.g, DEFAULT_BG.b
    ));
    let border_radius = options.border_radius.unwrap_or(8.0);

    let mut lines = parse_ansi(ansi_text);

    // 修剪尾部空行
    while lines
        .last()
        .map(|spans| spans.iter().all(|span| span.text.trim().is_empty()))
        .unwrap_or(false)
    {
        lines.pop();
    }

    // 等宽字体字宽约 0.6 * fontSize，仅用于 SVG 尺寸估算
    let char_width_estimate = font_size * 0.6;
    let max_line_length = lines
        .iter()
        .map(|spans| spans.iter().map(|s| s.text.chars().count()).sum::<usize>())
        .max()
        .unwrap_or(0);
    let width = ((max_line_length as f64) * char_width_estimate + padding_x * 2.0).ceil();
    let height = lines.len() as f64 * line_height + padding_y * 2.0;

    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">\n"
    );
    svg.push_str(&format!(
        "  <rect width=\"100%\" height=\"100%\" fill=\"{}\" rx=\"{}\" ry=\"{}\"/>\n",
        background_color, border_radius, border_radius
    ));
    svg.push_str("  <style>\n");
    svg.push_str(&format!(
        "    text {{ font-family: {}; font-size: {}px; white-space: pre; }}\n",
        font_family, font_size
    ));
    svg.push_str("    .b { font-weight: bold; }\n");
    svg.push_str("  </style>\n");

    for (line_index, spans) in lines.iter().enumerate() {
        let y = padding_y + (line_index + 1) as f64 * line_height - (line_height - font_size) / 2.0;
        svg.push_str(&format!(
            "  <text x=\"{}\" y=\"{}\" xml:space=\"preserve\">",
            padding_x, y
        ));

        for span in spans {
            if span.text.is_empty() {
                continue;
            }
            let color_str = format!("rgb({}, {}, {})", span.color.r, span.color.g, span.color.b);
            let bold_class = if span.bold { " class=\"b\"" } else { "" };
            svg.push_str(&format!(
                "<tspan fill=\"{}\"{}>{}</tspan>",
                color_str,
                bold_class,
                escape_xml(&span.text)
            ));
        }

        svg.push_str("</text>\n");
    }

    svg.push_str("</svg>");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;
    use AnsiColor as C;

    #[test]
    fn parses_plain_lines() {
        let parsed = parse_ansi("hello\nworld");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0][0].text, "hello");
        assert_eq!(parsed[0][0].color, DEFAULT_FG);
    }

    #[test]
    fn empty_lines_keep_placeholder_span() {
        let parsed = parse_ansi("a\n\nb");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[1].len(), 1);
        assert_eq!(parsed[1][0].text, "");
    }

    #[test]
    fn parses_basic_and_bright_colors() {
        let parsed = parse_ansi("\u{1b}[31mred\u{1b}[0mplain\u{1b}[92mgreen");
        assert_eq!(
            parsed[0][0].color,
            C {
                r: 205,
                g: 49,
                b: 49
            }
        );
        assert_eq!(parsed[0][1].color, DEFAULT_FG);
        assert_eq!(
            parsed[0][2].color,
            C {
                r: 35,
                g: 209,
                b: 139
            }
        );
    }

    #[test]
    fn parses_bold() {
        let parsed = parse_ansi("\u{1b}[1mbold\u{1b}[0mnormal");
        assert!(parsed[0][0].bold);
        assert!(!parsed[0][1].bold);
    }

    #[test]
    fn parses_256_and_truecolor() {
        let parsed = parse_ansi("\u{1b}[38;5;196mX\u{1b}[38;2;10;20;30mY");
        assert_eq!(parsed[0][0].color, get_256_color(196));
        assert_eq!(
            parsed[0][1].color,
            C {
                r: 10,
                g: 20,
                b: 30
            }
        );
    }

    #[test]
    fn svg_shape() {
        let svg = ansi_to_svg("\u{1b}[32mgreen & <text>", &AnsiToSvgOptions::default());
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.ends_with("</svg>"));
        // XML 转义
        assert!(svg.contains("green &amp; &lt;text&gt;"));
        assert!(svg.contains("<tspan fill=\"rgb(13, 188, 121)\""));
        // 空文本片段被跳过
        assert!(svg.contains("</text>\n</svg>"));
    }

    #[test]
    fn trailing_empty_lines_trimmed() {
        let svg = ansi_to_svg("hello\n\n\n", &AnsiToSvgOptions::default());
        assert!(svg.contains("</text>\n</svg>"));
    }
}
