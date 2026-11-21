//! `services/compaction2/methods/snapcompact/font.ts` — snapcompact 帧的
//! 位图字体。
//!
//! 生产字体是 X.org 的 `5x8` BDF 字体，随 oh-my-pi 发布于
//! `crates/pi-natives/src/fonts/5x8.bdf`（ucs-fonts，Markus Kuhn）。其
//! BDF 头声明：
//!
//!   COPYRIGHT "Public domain font.  Share and enjoy."
//!
//! 下面只嵌入可打印 ASCII 子集（U+0020..U+007E），派生数据因此仍是公共
//! 领域。新增字体前必须先核对上游 LICENSE/COPYRIGHT 头；来源不明的字体
//! 不得入库。
//!
//! `TEST_FONT_5X7` 是刻意极小的手写 5x7 字体，供测试使用，也是可注入
//! FontProvider 的示例。它不派生自任何第三方字体。

use std::collections::HashMap;

/// 一个字形位图。`rows[y]` 的像素位于 bit 7..3（最左像素是 bit 7），
/// 与 BDF 源编码一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapGlyph {
    pub width: usize,
    pub height: usize,
    pub rows: Vec<u8>,
}

/// 可注入的字形源。渲染器一次只问一个码点，`glyph` 返回 `None` 时回落
/// 到 `?`/空白单元。
pub trait FontProvider: Send + Sync {
    fn name(&self) -> &str;
    fn width(&self) -> usize;
    fn height(&self) -> usize;
    fn glyph(&self, code_point: u32) -> Option<&BitmapGlyph>;
}

/// X.org `5x8.bdf` 的可打印 ASCII 字形，每字形 16 个十六进制字符
/// （8 行各一字节，按码点顺序 U+0020..U+007E）。公共领域；出处见文件头。
const ASCII_5X8_HEX: &str = concat!(
    "0000000000000000002020202000200000505050000000005050F850F8505000",
    "2070A07028702000004050205010000040A0A040A0A050000020202000000000",
    "0020404040402000004020202020400000009060F060900000002020F8202000",
    "000000000030204000000000F000000000000000002070200010102040808000",
    "00205070505020000020602020207000006090106080F00000F0206010906000",
    "002060A0F020200000F080E010906000006080E09090600000F0102020404000",
    "0060906090906000006090907010600000006060006060000000303000302040",
    "0010204040201000000000F000F0000000402010102040000020501020002000",
    "304898A8A890403000609090F090900000E090E09090E0000060908080906000",
    "00E090909090E00000F080E08080F00000F080E08080800000609080B0906000",
    "009090F09090900000702020202070000070202020A040000090A0C0A0A09000",
    "008080808080F0000090F0F0909090000090D0F0B0B090000060909090906000",
    "00E09090E080800000609090D0B0601000E09090E09090000060904020906000",
    "00702020202020000090909090906000009090909060600000909090F0F09000",
    "0090906060909000008888502020200000F010204080F0000070404040407000",
    "00808040201010000070101010107000002050000000000000000000000000F0",
    "00402000000000000000007090907000008080E09090E0000000003040403000",
    "001010709090700000000060B0C0600000205040E04040000000006090701060",
    "008080E0909090000020006020207000001000101010502000808090E0909000",
    "0060202020207000000000D0A8A8A800000000E0909090000000006090906000",
    "000000E090E080800000007090701010000000A0D08080000000003060106000",
    "004040E0405020000000009090907000000000505050200000000088A8A85000",
    "00000090606090000000009090709060000000F02040F000304020C020403000",
    "0020202020202000C02040304020C0000050A00000000000"
);

/// TS `FIRST_ASCII = 0x20`。
const FIRST_ASCII: u32 = 0x20;

fn parse_glyph(hex: &str, width: usize, height: usize) -> BitmapGlyph {
    let rows = (0..height)
        .map(|y| {
            u8::from_str_radix(&hex[y * 2..y * 2 + 2], 16).unwrap_or(0)
        })
        .collect();
    BitmapGlyph { width, height, rows }
}

struct HexFont {
    name: &'static str,
    width: usize,
    height: usize,
    glyphs: HashMap<u32, BitmapGlyph>,
}

impl FontProvider for HexFont {
    fn name(&self) -> &str {
        self.name
    }
    fn width(&self) -> usize {
        self.width
    }
    fn height(&self) -> usize {
        self.height
    }
    fn glyph(&self, code_point: u32) -> Option<&BitmapGlyph> {
        self.glyphs.get(&code_point)
    }
}

fn create_ascii_font(
    name: &'static str,
    width: usize,
    height: usize,
    hex: &str,
) -> Box<dyn FontProvider> {
    let stride = height * 2;
    let mut glyphs = HashMap::new();
    let mut index = 0usize;
    while index * stride + stride <= hex.len() {
        let start = index * stride;
        let slice = &hex[start..start + stride];
        glyphs.insert(FIRST_ASCII + index as u32, parse_glyph(slice, width, height));
        index += 1;
    }
    Box::new(HexFont {
        name,
        width,
        height,
        glyphs,
    })
}

/// 公共领域 X.org 5x8 ASCII 字体（默认生产字体）。
pub fn five_by_eight_font() -> Box<dyn FontProvider> {
    create_ascii_font("5x8", 5, 8, ASCII_5X8_HEX)
}

struct TestFont {
    glyphs: HashMap<u32, BitmapGlyph>,
    fallback: BitmapGlyph,
}

impl FontProvider for TestFont {
    fn name(&self) -> &str {
        "test-5x7"
    }
    fn width(&self) -> usize {
        5
    }
    fn height(&self) -> usize {
        7
    }
    fn glyph(&self, code_point: u32) -> Option<&BitmapGlyph> {
        self.glyphs.get(&code_point).or(Some(&self.fallback))
    }
}

fn glyph_5x7(rows: Vec<u8>) -> BitmapGlyph {
    BitmapGlyph { width: 5, height: 7, rows }
}

/// 刻意极小的手写 5x7 测试字体。只有少量真字形；其余可打印码点渲染为
/// 方框轮廓，测试因此仍能在不依赖内置字体的情况下断言帧结构。
pub fn test_font_5x7() -> Box<dyn FontProvider> {
    let mut glyphs = HashMap::new();
    let blank = glyph_5x7(vec![0, 0, 0, 0, 0, 0, 0]);
    glyphs.insert(0x20, blank);
    glyphs.insert(0x41, glyph_5x7(vec![0x70, 0x88, 0x88, 0xf8, 0x88, 0x88, 0x88])); // A
    glyphs.insert(0x42, glyph_5x7(vec![0xf0, 0x88, 0x88, 0xf0, 0x88, 0x88, 0xf0])); // B
    glyphs.insert(0x43, glyph_5x7(vec![0x70, 0x88, 0x80, 0x80, 0x80, 0x88, 0x70])); // C
    glyphs.insert(0x30, glyph_5x7(vec![0x70, 0x88, 0x98, 0xa8, 0xc8, 0x88, 0x70])); // 0
    glyphs.insert(0x31, glyph_5x7(vec![0x20, 0x60, 0x20, 0x20, 0x20, 0x20, 0x70])); // 1
    glyphs.insert(0x2e, glyph_5x7(vec![0, 0, 0, 0, 0, 0x60, 0x60])); // .
    glyphs.insert(0x3f, glyph_5x7(vec![0x70, 0x88, 0x08, 0x10, 0x20, 0, 0x20])); // ?
    Box::new(TestFont {
        glyphs,
        fallback: glyph_5x7(vec![0x70, 0x88, 0x88, 0x88, 0x88, 0x88, 0x70]),
    })
}
