//! `services/compaction2/methods/snapcompact/png.ts` — snapcompact 帧的最小
//! PNG 编解码器（移植自 oh-my-pi，MIT）。
//!
//! 不用图像库：IHDR/IDAT/IEND 块与 CRC32 手写。TS 侧用 `node:zlib` 的
//! deflate/inflate 压缩扫描线流；Rust 依赖清单没有 flate2，这里用
//! RFC 1950/1951 的 stored-block（BTYPE=00）deflate 产出合法 zlib 流，
//! 解码侧只识别 stored 块（压缩块以错误形式拒绝，带 TODO 标注）。
//! 帧为 8-bit 灰度（color type 0）——黑 ink、中灰 dim ink、白底——
//! 保持载荷小、结构易验证。

use once_cell::sync::Lazy;

use crate::services::compaction2::methods::snapcompact::font::FontProvider;
use crate::services::compaction2::methods::snapcompact::planner::{count_cells, geometry, Shape};
use crate::services::compaction2::methods::snapcompact::serializer::{
    DIM_OFF, DIM_ON, NEWLINE_GLYPH,
};

const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
const COLOR_TYPE_GRAYSCALE: u8 = 0;
const BIT_DEPTH: u8 = 8;

static CRC_TABLE: Lazy<[u32; 256]> = Lazy::new(|| {
    let mut table = [0u32; 256];
    for (index, entry) in table.iter_mut().enumerate() {
        let mut c = index as u32;
        for _ in 0..8 {
            c = if c & 1 == 1 {
                0xedb88320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    table
});

/// TS `crc32(bytes)`：标准 PNG CRC-32。
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for byte in bytes {
        c = CRC_TABLE[((c ^ *byte as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

fn write_uint32(target: &mut Vec<u8>, value: u32) {
    target.extend_from_slice(&value.to_be_bytes());
}

fn read_uint32(bytes: &[u8], offset: usize) -> u32 {
    let mut value = 0u32;
    for index in 0..4 {
        value = (value << 8) | bytes.get(offset + index).copied().unwrap_or(0) as u32;
    }
    value
}

fn chunk(chunk_type: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    write_uint32(&mut out, data.len() as u32);
    out.extend_from_slice(chunk_type.as_bytes());
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(chunk_type.as_bytes());
    crc_input.extend_from_slice(data);
    write_uint32(&mut out, crc32(&crc_input));
    out
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = parts.iter().map(|part| part.len()).sum();
    let mut out = Vec::with_capacity(total);
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

/// 标准 base64 编码（RFC 4648，含 padding）。
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(triple >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// RFC 1950 zlib 压缩：2 字节头 + stored DEFLATE 块 + Adler-32。
///
/// stored 块不压缩数据（每块最多 65535 字节载荷），载荷因此比
/// `level: 9` 大几个百分点，但对所有 zlib 消费者都是合法流。
/// TODO(flate2/napi)：接通真实 deflate 后替换。
fn zlib_deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65535 * 5 + 11);
    // CMF=0x78（32K 窗口 + deflate），FLG=0x01 使 (CMF*256+FLG) % 31 == 0。
    out.extend_from_slice(&[0x78, 0x01]);
    let mut offset = 0usize;
    while offset < data.len() {
        let length = std::cmp::min(65535, data.len() - offset);
        let final_block = offset + length >= data.len();
        out.push(if final_block { 0b001 } else { 0b000 });
        out.push((length & 0xff) as u8);
        out.push((length >> 8) as u8);
        let nlen = !(length as u16);
        out.push((nlen & 0xff) as u8);
        out.push((nlen >> 8) as u8);
        out.extend_from_slice(&data[offset..offset + length]);
        offset += length;
    }
    if data.is_empty() {
        out.push(0b001);
        out.extend_from_slice(&[0x00, 0x00, 0xff, 0xff]);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// RFC 1950 Adler-32 校验和。
fn adler32(data: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65521;
    let mut a = 1u32;
    let mut b = 0u32;
    for byte in data {
        a = (a + *byte as u32) % MOD_ADLER;
        b = (b + a) % MOD_ADLER;
    }
    (b << 16) | a
}

/// RFC 1950 zlib 解压：只识别 stored DEFLATE 块。
///
/// TODO(flate2/napi)：解压块（fixed/dynamic Huffman）暂不支持。
fn zlib_inflate_stored(input: &[u8]) -> Result<Vec<u8>, String> {
    if input.len() < 6 {
        return Err("zlib stream too short".to_string());
    }
    let mut out = Vec::new();
    let mut offset = 2usize; // 跳过 zlib 头
    loop {
        let header = input.get(offset).copied().ok_or("truncated zlib stream")?;
        offset += 1;
        let block_type = (header >> 1) & 0b11;
        let final_block = header & 0b001 != 0;
        if block_type != 0 {
            return Err(format!("unsupported deflate block type: {block_type}"));
        }
        let length = (input.get(offset).copied().unwrap_or(0) as usize)
            | ((input.get(offset + 1).copied().unwrap_or(0) as usize) << 8);
        let nlen = (input.get(offset + 2).copied().unwrap_or(0) as usize)
            | ((input.get(offset + 3).copied().unwrap_or(0) as usize) << 8);
        offset += 4;
        let computed_nlen = !(length as u16);
        if (nlen as u16) != computed_nlen {
            return Err("corrupt stored deflate block (LEN/NLEN mismatch)".to_string());
        }
        if offset + length > input.len() {
            return Err("truncated stored deflate block".to_string());
        }
        out.extend_from_slice(&input[offset..offset + length]);
        offset += length;
        if final_block {
            break;
        }
    }
    Ok(out)
}

/// TS `encodeGrayscalePng(width, height, pixels)`：把 8-bit 灰度像素编码为 PNG。
pub fn encode_grayscale_png(
    width: usize,
    height: usize,
    pixels: &[u8],
) -> Result<Vec<u8>, String> {
    if width == 0 {
        return Err(format!("invalid PNG width: {width}"));
    }
    if height == 0 {
        return Err(format!("invalid PNG height: {height}"));
    }
    if pixels.len() != width * height {
        return Err(format!(
            "pixel buffer has {} bytes, expected {}",
            pixels.len(),
            width * height
        ));
    }

    let mut ihdr = Vec::with_capacity(13);
    write_uint32(&mut ihdr, width as u32);
    write_uint32(&mut ihdr, height as u32);
    ihdr.push(BIT_DEPTH);
    ihdr.push(COLOR_TYPE_GRAYSCALE);
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace

    // 每条扫描线前置 filter type 0（None）。
    let mut raw = Vec::with_capacity(height * (width + 1));
    for y in 0..height {
        raw.push(0);
        let start = y * width;
        raw.extend_from_slice(&pixels[start..start + width]);
    }
    let idat = zlib_deflate_stored(&raw);

    Ok(concat(&[
        PNG_SIGNATURE.to_vec(),
        chunk("IHDR", &ihdr),
        chunk("IDAT", &idat),
        chunk("IEND", &[]),
    ]))
}

/// TS `PngChunkInfo`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PngChunkInfo {
    pub typ: String,
    pub length: u32,
    pub crc_ok: bool,
}

/// TS `PngStructure`。
#[derive(Debug, Clone)]
pub struct PngStructure {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    pub chunks: Vec<PngChunkInfo>,
    /// 拼接的 IDAT 载荷（仍是 zlib 压缩的）。
    pub idat: Vec<u8>,
}

fn verify_signature(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < PNG_SIGNATURE.len() {
        return Err("not a PNG: truncated signature".to_string());
    }
    if bytes[..PNG_SIGNATURE.len()] != PNG_SIGNATURE {
        return Err("not a PNG: bad signature".to_string());
    }
    Ok(())
}

/// TS `inspectPng(bytes)`。
///
/// 解析 PNG 块结构并校验每个 CRC。畸形输入返回错误；测试用它证明手写
/// 编码器可往返。
pub fn inspect_png(bytes: &[u8]) -> Result<PngStructure, String> {
    verify_signature(bytes)?;
    let mut offset = PNG_SIGNATURE.len();
    let mut chunks: Vec<PngChunkInfo> = Vec::new();
    let mut idat_parts: Vec<Vec<u8>> = Vec::new();
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut saw_header = false;
    let mut saw_end = false;

    while offset + 12 <= bytes.len() {
        let length = read_uint32(bytes, offset) as usize;
        let type_start = offset + 4;
        let data_start = offset + 8;
        let data_end = data_start + length;
        let crc_end = data_end + 4;
        if crc_end > bytes.len() {
            return Err("truncated PNG chunk".to_string());
        }
        let typ: String = bytes[type_start..data_start]
            .iter()
            .map(|byte| *byte as char)
            .collect();
        let crc_input = &bytes[type_start..data_end];
        let expected = read_uint32(bytes, data_end);
        let crc_ok = crc32(crc_input) == expected;
        chunks.push(PngChunkInfo {
            typ: typ.clone(),
            length: length as u32,
            crc_ok,
        });
        let data = &bytes[data_start..data_end];

        if typ == "IHDR" {
            if length != 13 {
                return Err("bad IHDR length".to_string());
            }
            width = read_uint32(data, 0);
            height = read_uint32(data, 4);
            bit_depth = data.get(8).copied().unwrap_or(0);
            color_type = data.get(9).copied().unwrap_or(0);
            saw_header = true;
        } else if typ == "IDAT" {
            idat_parts.push(data.to_vec());
        } else if typ == "IEND" {
            saw_end = true;
            break;
        }
        offset = crc_end;
    }

    if !saw_header {
        return Err("PNG has no IHDR".to_string());
    }
    if !saw_end {
        return Err("PNG has no IEND".to_string());
    }
    Ok(PngStructure {
        width,
        height,
        bit_depth,
        color_type,
        chunks,
        idat: concat(&idat_parts),
    })
}

/// TS `decodeGrayscalePng(bytes)`：解码 `encodeGrayscalePng` 产出的
/// 8-bit 灰度 PNG。
pub fn decode_grayscale_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let structure = inspect_png(bytes)?;
    if structure.bit_depth != BIT_DEPTH {
        return Err(format!("unsupported PNG bit depth: {}", structure.bit_depth));
    }
    if structure.color_type != COLOR_TYPE_GRAYSCALE {
        return Err(format!(
            "unsupported PNG color type: {}",
            structure.color_type
        ));
    }
    let raw = zlib_inflate_stored(&structure.idat)?;
    let stride = structure.width as usize + 1;
    if raw.len() != structure.height as usize * stride {
        return Err("unexpected IDAT payload length".to_string());
    }
    let mut pixels = vec![0u8; structure.width as usize * structure.height as usize];
    for y in 0..structure.height as usize {
        let filter = raw[y * stride];
        if filter != 0 {
            return Err(format!("unsupported PNG filter type: {filter}"));
        }
        let src = &raw[y * stride + 1..(y + 1) * stride];
        pixels[y * structure.width as usize..(y + 1) * structure.width as usize]
            .copy_from_slice(src);
    }
    Ok((structure.width, structure.height, pixels))
}

/// TS `RenderedFrame`。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RenderedFrame {
    /// Base64 编码的 PNG。
    pub data: String,
    pub mime_type: &'static str,
    pub cols: usize,
    pub rows: usize,
    /// 打到该帧上的可见字符数。
    pub chars: usize,
    pub font: &'static str,
    pub variant: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<&'static str>,
}

const INK_BLACK: u8 = 0;
const INK_DIM: u8 = 128;
const PAPER: u8 = 255;
const MIN_FRAME_HEIGHT: usize = 64;

fn draw_glyph(
    pixels: &mut [u8],
    frame_width: usize,
    x0: i64,
    y0: i64,
    font: &dyn FontProvider,
    code_point: u32,
    ink: u8,
) {
    let glyph = font
        .glyph(code_point)
        .or_else(|| font.glyph(0x3f))
        .or_else(|| font.glyph(0x20));
    let Some(glyph) = glyph else {
        return;
    };
    for gy in 0..glyph.height {
        let row = *glyph.rows.get(gy).unwrap_or(&0) as u32;
        let y = y0 + gy as i64;
        if y < 0 {
            continue;
        }
        for gx in 0..glyph.width {
            if row & (0x80 >> gx) == 0 {
                continue;
            }
            let x = x0 + gx as i64;
            if x < 0 || x >= frame_width as i64 {
                continue;
            }
            let index = (y as usize) * frame_width + x as usize;
            if index < pixels.len() {
                pixels[index] = ink;
            }
        }
    }
}

/// TS `renderFrame(text, shape, font)`。
///
/// 把归一化文本渲染到一帧。帧宽是形状的 `frameSize`；高紧贴实际打印的
/// 行数，下限 64px。
pub fn render_frame(text: &str, shape: &Shape, font: &dyn FontProvider) -> RenderedFrame {
    let geo = geometry(shape, None);
    let visible_cells = count_cells(text);
    let rows_used = std::cmp::max(
        1usize,
        (visible_cells + geo.cols - 1) / geo.cols,
    );
    let height = std::cmp::max(
        MIN_FRAME_HEIGHT,
        rows_used * shape.cell_height * shape.line_repeat,
    );
    let width = shape.frame_size;
    let mut pixels = vec![PAPER; width * height];

    let mut cell = 0usize;
    let mut dim = false;
    let mut drawn = 0usize;
    for character in text.chars() {
        if character == DIM_ON {
            dim = true;
            continue;
        }
        if character == DIM_OFF {
            dim = false;
            continue;
        }
        if cell >= geo.capacity {
            break;
        }
        let col = cell % geo.cols;
        let row = cell / geo.cols;
        let x0 = (col * shape.cell_width) as i64;
        let y0 = (row * shape.cell_height * shape.line_repeat) as i64;
        let ink = if dim { INK_DIM } else { INK_BLACK };
        if character == NEWLINE_GLYPH {
            // 全块单元；上游字体打印的同一可见标记。
            for dy in 0..shape.cell_height {
                let y = y0 + dy as i64;
                if y >= height as i64 {
                    break;
                }
                let row_start = y as usize * width + x0 as usize;
                let row_end = std::cmp::min(x0 as usize + shape.cell_width, width);
                if row_start < pixels.len() {
                    let fill_end = std::cmp::min(row_start + (row_end - x0 as usize), pixels.len());
                    for index in row_start..fill_end {
                        pixels[index] = ink;
                    }
                }
            }
        } else {
            draw_glyph(
                &mut pixels,
                width,
                x0,
                y0,
                font,
                character as u32,
                ink,
            );
        }
        cell += 1;
        drawn += 1;
    }

    let png = encode_grayscale_png(width, height, &pixels)
        .unwrap_or_else(|_error| {
            // 帧渲染失败不能让压缩崩溃：回退为 1x1 白帧，载荷错误会被
            // 帧预算/校验路径拒绝。
            encode_grayscale_png(1, 1, &[PAPER]).unwrap_or_default()
        });
    RenderedFrame {
        data: base64_encode(&png),
        mime_type: "image/png",
        cols: geo.cols,
        rows: rows_used,
        chars: drawn,
        font: shape.font,
        variant: shape.variant,
        detail: shape.image_detail.map(|detail| detail.as_str()),
    }
}
