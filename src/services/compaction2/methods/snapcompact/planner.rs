//! `services/compaction2/methods/snapcompact/planner.ts` — snapcompact 帧
//! 规划。
//!
//! 这里一切都是数据驱动：provider/API family 从 FAMILY_PROFILES 选形状
//! 与价格；调用方（或显式环境变量）必须指明 family。刻意不做模型名
//! 子串匹配——CCH 没有可靠的 modality 元数据，family 未解析即方法
//! 不可用。
//!
//! 布局对齐 oh-my-pi：chrono 边缘各留一页高品质原文，中间成像；过大
//! 的中间做 HQ/LQ/HQ 注视化（丢掉最老的稠密切片），同时遵守
//! `maxFrames`（默认 80）、provider 图片预算、token 预算与 base64
//! 载荷预算。

use std::collections::HashMap;

use crate::services::compaction2::methods::snapcompact::serializer::{
    DIM_OFF, DIM_ON, NEWLINE_GLYPH,
};

/// TS `ProviderFamily`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum ProviderFamily {
    Anthropic,
    OpenAi,
    Google,
}

impl ProviderFamily {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderFamily::Anthropic => "anthropic",
            ProviderFamily::OpenAi => "openai",
            ProviderFamily::Google => "google",
        }
    }

    pub fn parse(value: &str) -> Option<ProviderFamily> {
        match value {
            "anthropic" => Some(ProviderFamily::Anthropic),
            "openai" => Some(ProviderFamily::OpenAi),
            "google" => Some(ProviderFamily::Google),
            _ => None,
        }
    }
}

/// TS `ImageDetail`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageDetail {
    Auto,
    Low,
    High,
    Original,
}

impl ImageDetail {
    pub fn as_str(&self) -> &'static str {
        match self {
            ImageDetail::Auto => "auto",
            ImageDetail::Low => "low",
            ImageDetail::High => "high",
            ImageDetail::Original => "original",
        }
    }
}

/// TS `Shape`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// 测试与 preserve data 使用的稳定形状名。
    pub name: &'static str,
    /// 字体名（见 `font.rs`）。
    pub font: &'static str,
    /// 目标单元步进（像素）。
    pub cell_width: usize,
    /// 目标单元行距（像素）。
    pub cell_height: usize,
    /// ink 变体；只实现了黑底白字。
    pub variant: &'static str,
    /// 每行文本重复打印的次数（内置形状均为 1）。
    pub line_repeat: usize,
    /// 帧边长（像素）。
    pub frame_size: usize,
    /// 该 family 的每帧计费 token 估算。
    pub frame_token_estimate: i64,
    /// 附在帧上的分辨率提示（仅 OpenAI）。
    pub image_detail: Option<ImageDetail>,
}

/// TS `FamilyProfile`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyProfile {
    pub family: ProviderFamily,
    /// 归档边缘使用的高可读层级。
    pub high: Shape,
    /// 注视化中间使用的更稠密层级。
    pub low: Shape,
    pub frame_size: usize,
    pub frame_token_estimate: i64,
    pub image_detail: Option<ImageDetail>,
    /// 部署的图片数预算（仅归档帧）。
    pub image_budget: usize,
}

/// TS `MAX_FRAMES_DEFAULT`：每次压缩携带的归档帧上限。
pub const MAX_FRAMES_DEFAULT: usize = 80;
/// TS `HQ_EDGE_FRAMES`：注视化归档每个 chrono 边缘渲染的 HQ 帧数。
pub const HQ_EDGE_FRAMES: usize = 3;
/// TS `FRAME_DATA_BYTES_ESTIMATE`：1932px 下每帧 base64 载荷的保守估算。
pub const FRAME_DATA_BYTES_ESTIMATE: i64 = 170_000;
/// TS `DEFAULT_FRAME_DATA_BYTES_BUDGET`：部署没有实测内联图片预算时的载荷上限。
pub const DEFAULT_FRAME_DATA_BYTES_BUDGET: i64 = 3_000_000;
/// 缩放电载荷估算的参考边长。
const FRAME_DATA_REFERENCE_EDGE: i64 = 1932;

/// 原始形状几何。`frameSize` 是 family 级值，按需套用。
#[derive(Debug, Clone, Copy)]
struct ShapeGeometry {
    name: &'static str,
    font: &'static str,
    cell_width: usize,
    cell_height: usize,
    line_repeat: usize,
}

const SHAPES: &[(&'static str, ShapeGeometry)] = &[
    // 5x8 字形落在其天然 5x8 单元上（最稠密层级）。
    (
        "5x8-bw",
        ShapeGeometry {
            name: "5x8-bw",
            font: "5x8",
            cell_width: 5,
            cell_height: 8,
            line_repeat: 1,
        },
    ),
    // 5x8 字形落在 16px 行距上（额外行距，高可读层级）。
    (
        "5on16-bw",
        ShapeGeometry {
            name: "5on16-bw",
            font: "5x8",
            cell_width: 5,
            cell_height: 16,
            line_repeat: 1,
        },
    ),
    // 5x8 字形落在 22px 行距上（更多行距；Google/OpenAI 的赢家）。
    (
        "5on22-bw",
        ShapeGeometry {
            name: "5on22-bw",
            font: "5x8",
            cell_width: 5,
            cell_height: 22,
            line_repeat: 1,
        },
    ),
    // 5x8 字形落在 6px 步进上（一像素字距）。
    (
        "6on16-bw",
        ShapeGeometry {
            name: "6on16-bw",
            font: "5x8",
            cell_width: 6,
            cell_height: 16,
            line_repeat: 1,
        },
    ),
];

/// TS `SHAPE_NAMES`。
pub fn shape_names() -> Vec<&'static str> {
    SHAPES.iter().map(|(name, _)| *name).collect()
}

/// TS `isShapeName(value)`。
pub fn is_shape_name(value: &str) -> bool {
    SHAPES.iter().any(|(name, _)| *name == value)
}

fn shape_geometry(name: &str) -> Option<ShapeGeometry> {
    SHAPES
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, geometry)| *geometry)
}

fn shape(name: &str, frame_size: usize) -> Option<Shape> {
    let base = shape_geometry(name)?;
    // family profile 在解析时替换该估算；形状本身只需要保守默认值。
    Some(Shape {
        name: base.name,
        font: base.font,
        cell_width: base.cell_width,
        cell_height: base.cell_height,
        variant: "bw",
        line_repeat: base.line_repeat,
        frame_size,
        frame_token_estimate: 5_024,
        image_detail: None,
    })
}

/// TS `FAMILY_PROFILES`。
///
/// Provider family profile。价格是保守的目录派生估算，不是模型名启发式：
/// - Anthropic：4,784 视觉 token 上限 + 5% 余量（5,024）；
/// - OpenAI：同一上限，直到部署证明更小的规则；
/// - Google：固定默认 `media_resolution` 预算（1,120）。
pub fn family_profile(family: ProviderFamily) -> FamilyProfile {
    match family {
        ProviderFamily::Anthropic => FamilyProfile {
            family,
            high: shape("6on16-bw", 1568).unwrap(),
            low: shape("5x8-bw", 1568).unwrap(),
            frame_size: 1568,
            frame_token_estimate: 5_024,
            image_detail: None,
            image_budget: MAX_FRAMES_DEFAULT,
        },
        ProviderFamily::OpenAi => FamilyProfile {
            family,
            high: shape("5on22-bw", 1568).unwrap(),
            low: shape("5x8-bw", 1568).unwrap(),
            frame_size: 1568,
            frame_token_estimate: 5_024,
            image_detail: Some(ImageDetail::Original),
            image_budget: MAX_FRAMES_DEFAULT,
        },
        ProviderFamily::Google => FamilyProfile {
            family,
            high: shape("5on22-bw", 2048).unwrap(),
            low: shape("5x8-bw", 2048).unwrap(),
            frame_size: 2048,
            frame_token_estimate: 1_120,
            image_detail: None,
            image_budget: MAX_FRAMES_DEFAULT,
        },
    }
}

/// TS `isProviderFamily(value)`。
pub fn is_provider_family(value: &str) -> bool {
    ProviderFamily::parse(value).is_some()
}

fn parse_positive_int(value: Option<&str>) -> Option<i64> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let parsed = value.parse::<i64>().ok()?;
    (parsed > 0).then_some(parsed)
}

fn env_value(env: Option<&HashMap<String, Option<String>>>, key: &str) -> Option<String> {
    env?.get(key).and_then(|value| value.clone()).filter(|value| !value.is_empty())
}

/// TS `ProfileInput`。
#[derive(Debug, Clone, Default)]
pub struct ProfileInput<'a> {
    pub env: Option<&'a HashMap<String, Option<String>>>,
    /// settings/caller 的显式 family；优先于环境变量。
    pub family: Option<ProviderFamily>,
    /// 显式形状名；优先于环境变量。
    pub shape: Option<&'a str>,
    /// 显式帧边长（像素）。
    pub frame_size: Option<i64>,
}

/// TS `resolveProfile(input)`。
///
/// 为读取方解析 profile。没有显式 family 或覆盖非法时返回 `None`——
/// 调用方必须据此把 snapcompact 视为不可用。
pub fn resolve_profile(input: &ProfileInput<'_>) -> Option<FamilyProfile> {
    let env = input.env;
    let env_family = env_value(env, "CCH_SNAPCOMPACT_FAMILY")
        .or_else(|| env_value(env, "CCH_SNAPCOMPACT_API_FAMILY"));
    let family = input.family.or_else(|| {
        env_family
            .as_deref()
            .and_then(ProviderFamily::parse)
    });
    let family = family?;
    let base = family_profile(family);

    let shape_name = input.shape.map(str::to_string).or_else(|| env_value(env, "CCH_SNAPCOMPACT_SHAPE"));
    if let Some(shape_name) = shape_name.as_deref() {
        if !is_shape_name(shape_name) {
            return None;
        }
    }

    let frame_size = input
        .frame_size
        .or_else(|| parse_positive_int(env_value(env, "CCH_SNAPCOMPACT_FRAME_SIZE").as_deref()))
        .unwrap_or(base.frame_size as i64) as usize;
    let frame_token_estimate = parse_positive_int(
        env_value(env, "CCH_SNAPCOMPACT_FRAME_TOKENS").as_deref(),
    )
    .unwrap_or(base.frame_token_estimate);
    let image_budget = parse_positive_int(
        env_value(env, "CCH_SNAPCOMPACT_IMAGE_BUDGET").as_deref(),
    )
    .unwrap_or(base.image_budget as i64) as usize;

    let high = match shape_name.as_deref() {
        Some(shape_name) => shape(shape_name, frame_size)?,
        None => Shape { frame_size, ..base.high },
    };
    let low = Shape { frame_size, ..base.low };
    Some(FamilyProfile {
        family,
        high,
        low,
        frame_size,
        frame_token_estimate,
        image_budget,
        ..base
    })
}

/// TS `Geometry`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub cols: usize,
    pub rows: usize,
    pub capacity: usize,
}

/// TS `geometry(shape, size?)`。
pub fn geometry(shape: &Shape, size: Option<usize>) -> Geometry {
    let size = size.unwrap_or(shape.frame_size);
    let cols = std::cmp::max(1, size / shape.cell_width);
    let rows = std::cmp::max(
        1,
        size / shape.cell_height / std::cmp::max(1, shape.line_repeat),
    );
    Geometry {
        cols,
        rows,
        capacity: cols * rows,
    }
}

/// 一个字符占的单元数：dim 开关是零宽，其余（含 newline glyph）占一单元。
fn cell_units(character: char) -> usize {
    if character == DIM_ON || character == DIM_OFF {
        0
    } else {
        1
    }
}

/// TS `countCells(text)`：`text` 的可见单元数（不含 dim 开关）。
pub fn count_cells(text: &str) -> usize {
    text.chars().map(cell_units).sum()
}

/// TS `paginateCells(text, capacity, _cols)`。
///
/// 把 `text` 切成每页至多 `capacity` 单元的连续页。页是连续子串，因此
/// 每页都从单元 0 起独立渲染，注视化可以重切中间而不丢文本。
pub fn paginate_cells(text: &str, capacity: usize, _cols: usize) -> Vec<String> {
    if capacity == 0 {
        return Vec::new();
    }
    let mut pages: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut cell = 0usize;
    let mut has_cell = false;
    for (index, character) in text.char_indices() {
        let width = cell_units(character);
        if width == 0 {
            continue;
        }
        if has_cell && cell + width > capacity {
            pages.push(text[start..index].to_string());
            start = index;
            cell = 0;
        }
        cell += width;
        has_cell = true;
    }
    if has_cell {
        pages.push(text[start..].to_string());
    }
    pages
}

/// TS `maxFramesForDataBudget(shape, budget)`。
///
/// base64 字节预算隐含的帧数上限。帧按 1932px 参考边长下的 170KB 估算
/// 依像素面积缩放计费。
pub fn max_frames_for_data_budget(shape: &Shape, budget: i64) -> usize {
    let ratio = ((shape.frame_size as f64 / FRAME_DATA_REFERENCE_EDGE as f64).powi(2)).min(1.0);
    let frame_bytes = std::cmp::min(
        FRAME_DATA_BYTES_ESTIMATE,
        (FRAME_DATA_BYTES_ESTIMATE as f64 * ratio).ceil() as i64,
    );
    std::cmp::max(1, (budget / frame_bytes) as usize)
}

/// TS `BudgetOptions`。
#[derive(Debug, Clone, Default)]
pub struct BudgetOptions {
    /// 调用方上限；钳制到 MAX_FRAMES_DEFAULT。
    pub max_frames: Option<usize>,
    /// 归档帧可用的 token 预算。
    pub token_budget: Option<i64>,
    /// base64 载荷预算。
    pub data_budget_bytes: Option<i64>,
}

/// TS `resolveMaxFrames(profile, options?)`：从全部预算解析有效帧上限。
pub fn resolve_max_frames(profile: &FamilyProfile, options: &BudgetOptions) -> usize {
    let requested = std::cmp::min(options.max_frames.unwrap_or(MAX_FRAMES_DEFAULT), MAX_FRAMES_DEFAULT);
    let token_frames = match options.token_budget {
        None => usize::MAX,
        Some(token_budget) => std::cmp::max(
            1,
            (token_budget / profile.frame_token_estimate) as usize,
        ),
    };
    let data_frames = max_frames_for_data_budget(
        &profile.high,
        options.data_budget_bytes.unwrap_or(DEFAULT_FRAME_DATA_BYTES_BUDGET),
    );
    std::cmp::max(
        1,
        requested
            .min(profile.image_budget)
            .min(token_frames)
            .min(data_frames),
    )
}

/// TS `FramePlan`。
#[derive(Debug, Clone)]
pub struct FramePlan {
    pub text: String,
    pub shape: Shape,
}

/// TS `ArchivePlan`。
#[derive(Debug, Clone)]
pub struct ArchivePlan {
    pub frames: Vec<FramePlan>,
    pub text_head: String,
    pub text_tail: String,
    pub kept_text: String,
    pub truncated_chars: usize,
    pub max_frames: usize,
    pub family: ProviderFamily,
}

/// 把页按某一层级包成计划帧。
fn plan_frames(pages: &[String], shape: Shape) -> Vec<FramePlan> {
    pages
        .iter()
        .map(|text| FramePlan {
            text: text.clone(),
            shape,
        })
        .collect()
}

/// TS `planArchive(text, profile, options?)`。
///
/// 把归一化的归档文本（旧→新）布局为：两端文本 + 中间成像。
pub fn plan_archive(text: &str, profile: &FamilyProfile, options: &BudgetOptions) -> ArchivePlan {
    let max_frames = resolve_max_frames(profile, options);
    let high = profile.high;
    let low = profile.low;
    let geo_high = geometry(&high, None);
    let edge_cap = geo_high.capacity;

    if text.chars().count() <= 2 * edge_cap {
        return ArchivePlan {
            frames: Vec::new(),
            text_head: text.to_string(),
            text_tail: String::new(),
            kept_text: text.to_string(),
            truncated_chars: 0,
            max_frames,
            family: profile.family,
        };
    }

    if max_frames < 1 {
        let text_head: String = text.chars().take(edge_cap).collect();
        let text_tail: String = text.chars().rev().take(edge_cap).collect::<Vec<_>>().into_iter().rev().collect();
        return ArchivePlan {
            frames: Vec::new(),
            kept_text: format!("{text_head}{text_tail}"),
            truncated_chars: text.chars().count().saturating_sub(text_head.chars().count() + text_tail.chars().count()),
            text_head,
            text_tail,
            max_frames,
            family: profile.family,
        };
    }

    let text_head: String = text.chars().take(edge_cap).collect();
    let text_tail: String = text.chars().rev().take(edge_cap).collect::<Vec<_>>().into_iter().rev().collect();
    let image_text: String = text
        .chars()
        .skip(edge_cap)
        .take(text.chars().count().saturating_sub(2 * edge_cap))
        .collect();
    if image_text.is_empty() {
        return ArchivePlan {
            frames: Vec::new(),
            text_head: text.to_string(),
            text_tail: String::new(),
            kept_text: text.to_string(),
            truncated_chars: 0,
            max_frames,
            family: profile.family,
        };
    }

    let high_pages = paginate_cells(&image_text, geo_high.capacity, geo_high.cols);
    if high_pages.len() <= max_frames {
        let kept_text = format!("{text_head}{image_text}{text_tail}");
        return ArchivePlan {
            frames: plan_frames(&high_pages, high),
            text_head,
            text_tail,
            kept_text,
            truncated_chars: 0,
            max_frames,
            family: profile.family,
        };
    }

    // 注视化：HQ 边缘，更稠密的中间，丢掉最老的稠密切片。
    let geo_low = geometry(&low, None);
    let edge_frames = std::cmp::min(HQ_EDGE_FRAMES, (max_frames - 1) / 2);
    let head_pages = high_pages[..edge_frames.min(high_pages.len())].to_vec();
    let tail_pages = if edge_frames > 0 {
        let start = high_pages.len().saturating_sub(edge_frames);
        high_pages[start..].to_vec()
    } else {
        Vec::new()
    };
    let image_head = head_pages.join("");
    let image_tail = tail_pages.join("");
    let image_chars: Vec<char> = image_text.chars().collect();
    let middle_source: String = image_chars[image_head.chars().count()
        ..image_chars.len().saturating_sub(image_tail.chars().count())]
        .iter()
        .collect();
    let mut middle_pages = paginate_cells(&middle_source, geo_low.capacity, geo_low.cols);
    let middle_budget = max_frames.saturating_sub(2 * edge_frames);
    let mut truncated_chars = 0usize;
    let mut middle_text = middle_source.clone();
    if middle_pages.len() > middle_budget {
        let dropped_count = middle_pages.len() - middle_budget;
        let dropped: String = middle_pages[..dropped_count].join("");
        truncated_chars = dropped.chars().count();
        middle_text = middle_source[middle_source.char_indices().nth(dropped.chars().count()).map(|(index, _)| index).unwrap_or_else(|| middle_source.len())..].to_string();
        middle_pages = middle_pages[dropped_count..].to_vec();
    }

    let kept_text = format!("{text_head}{image_head}{middle_text}{image_tail}{text_tail}");
    let mut frames = plan_frames(&head_pages, high);
    frames.extend(plan_frames(&middle_pages, low));
    frames.extend(plan_frames(&tail_pages, high));
    ArchivePlan {
        frames,
        text_head,
        text_tail,
        kept_text,
        truncated_chars,
        max_frames,
        family: profile.family,
    }
}

/// TS `frameDataBytes(frames)`：一组帧的 base64 字节长度。
pub fn frame_data_bytes(frames: &[crate::services::compaction2::methods::snapcompact::png::RenderedFrame]) -> i64 {
    frames.iter().map(|frame| frame.data.len() as i64).sum()
}

/// TS `hasRenderableText(text)`：`text` 归一化后非空（是可渲染的归档）。
pub fn has_renderable_text(text: &str) -> bool {
    text.chars().any(|character| {
        character != DIM_ON && character != DIM_OFF && character != NEWLINE_GLYPH
    })
}
