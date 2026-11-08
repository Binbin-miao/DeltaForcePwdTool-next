//! 点阵字符读取（操作台 LED 目标序列，spec D7：字符集仅 `0` `1` `-`）。
//!
//! 判定链（spec §7.1，真实视频帧实测 11/11 全对）：
//!
//! 1. 整条 LED 区 Otsu 二值化 → 列投影切字符段 → 每段求**墨迹包围盒** `(w, h)`；
//! 2. 取所有段高度的**中位数** `h_ref` 当满高基准（`-` 是矮的，中位数自动把它排除）；
//! 3. **主判据 = 宽高比**：`h < 0.6·h_ref` → `-`；`w/h > 0.55` → `0`；否则 `1`；
//! 4. **二次校验**：把该段归一化到 32×32 与内嵌真实字形（`assets/led/*.png`）做 NCC，
//!    得分 < 0.6 且内部 5×3 结构签名也不达标时返回 [`VisionError::LedAmbiguous`]，
//!    **绝不用宽高比硬猜**。
//!
//! 为什么要「结构签名」而不是只靠宽高比：游戏 LED 的 `1` 带满宽底衬线时，包围盒宽度会
//! 与 `0` 相同（合成字料就是这种形态），宽高比不可分、只有内部墨迹分布可分。宽高比仍是
//! 主路径（快、对真实字形鲁棒），结构签名只在「宽体」这一格上做消歧。

use std::sync::OnceLock;

use crate::{BinaryImage, GrayImage, OtsuResult, Region, VisionError};

/// 满高判据：段高低于 `DASH_HEIGHT_RATIO × h_ref` 的判 `-`（spec §7.1）。
pub const LED_DASH_HEIGHT_RATIO: f32 = 0.6;
/// `0` 与 `1` 的宽高比分界（spec §7.1：`0` 实测 0.66~0.69，`1` 实测 0.33~0.41）。
pub const LED_ZERO_ASPECT: f32 = 0.55;
/// 二次校验门限：真实字形模板 NCC 低于该值即视为不可识别（spec §7.1）。
pub const LED_TEMPLATE_MIN_SCORE: f32 = 0.6;
/// 结构签名「决定性」门限：达到该值即认为内部墨迹分布已能独立确认字符
/// （合成点阵字料与内嵌 3×5 图案**完全同源**，得分恒为 1.0）。
pub const LED_SHAPE_DECISIVE: f32 = 0.85;
/// 结构签名「可接受」门限：模板 NCC 未过门限时，签名达到该值仍认可判定。
pub const LED_SHAPE_ACCEPT: f32 = 0.9;
/// 满高锚点：整条 LED 至少要有段的墨迹高度达到区域高度的该比例，否则说明这条带子里
/// 根本没有满高字形（多半是 `-` 或污渍），此时拒绝出字而不是把矮条猜成 `0`。
pub const LED_ANCHOR_MIN_BAND_FILL: f32 = 0.35;

/// 归一化后的字形栅格边长（与内嵌模板一致）。
const TEMPLATE_SIZE: u32 = 32;
/// 结构签名的行列数（5 行 × 3 列）。
const SIGNATURE_ROWS: usize = 5;
const SIGNATURE_COLS: usize = 3;

/// 内嵌点阵图案，作为结构签名（第二判据）的参考模板。
const PATTERNS: [(char, [[u8; SIGNATURE_COLS]; SIGNATURE_ROWS]); 3] = [
    (
        '0',
        [
            [1, 1, 1],
            [1, 0, 1],
            [1, 0, 1],
            [1, 0, 1],
            [1, 1, 1],
        ],
    ),
    (
        '1',
        [
            [0, 1, 0],
            [1, 1, 0],
            [0, 1, 0],
            [0, 1, 0],
            [1, 1, 1],
        ],
    ),
    (
        '-',
        [
            [0, 0, 0],
            [0, 0, 0],
            [1, 1, 1],
            [0, 0, 0],
            [0, 0, 0],
        ],
    ),
];

/// 真实游戏 LED 字形（从视频帧导出，32×32 灰度，**汇编期内嵌**，不依赖运行时路径）。
const TEMPLATE_PNGS: [(char, &[u8]); 3] = [
    ('0', include_bytes!("../../../assets/led/led_0.png")),
    ('1', include_bytes!("../../../assets/led/led_1.png")),
    ('-', include_bytes!("../../../assets/led/led_dash.png")),
];

/// 一个已归一化的字形参考：`(字符, 32×32 灰度向量)`。
type Glyph = (char, Vec<f32>);

/// 读取一行点阵字符，返回如 `"01-00-01-11"` 的字符串。
pub fn read_glyph_string(image: &GrayImage) -> Result<String, VisionError> {
    if image.w < 2 || image.h < 2 {
        return Err(VisionError::InvalidDimensions {
            w: image.w,
            h: image.h,
        });
    }
    let binary = image.otsu().binary;
    let masks = segment_masks(&binary);
    if masks.is_empty() {
        return Err(VisionError::NoGlyph);
    }

    let reference_height = median_height(&masks);
    ensure_full_height_anchor(&masks, image.h)?;
    let templates = templates()?;

    let mut recognized = String::with_capacity(masks.len());
    for (index, mask) in masks.iter().enumerate() {
        let glyph = image.crop(mask.bbox)?;
        recognized.push(classify(index, mask, &glyph, reference_height, templates)?);
    }
    Ok(recognized)
}

/// 一个字符段：列区间 + 墨迹包围盒（都取自二值图）。
struct GlyphMask {
    bbox: Region,
    /// 段内墨迹的 `(x 偏移, y)` 列表，用于结构签名（坐标系相对 `bbox` 左上角）。
    ink: Vec<(u32, u32)>,
}

/// 按列投影切段，并在段内取墨迹包围盒。
fn segment_masks(binary: &BinaryImage) -> Vec<GlyphMask> {
    let mut masks = Vec::new();
    let mut start: Option<u32> = None;
    for x in 0..binary.width() {
        let has_ink = (0..binary.height()).any(|y| binary.get(x, y));
        match (has_ink, start) {
            (true, None) => start = Some(x),
            (false, Some(begin)) => {
                masks.extend(mask_for_columns(binary, begin, x));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        masks.extend(mask_for_columns(binary, begin, binary.width()));
    }
    masks
}

/// `[start, end)` 列内求包围盒；整段没有墨迹时返回空。
fn mask_for_columns(binary: &BinaryImage, start: u32, end: u32) -> Option<GlyphMask> {
    let mut min_x = u32::MAX;
    let mut max_x = 0;
    let mut min_y = u32::MAX;
    let mut max_y = 0;
    let mut ink = Vec::new();
    for x in start..end {
        for y in 0..binary.height() {
            if !binary.get(x, y) {
                continue;
            }
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
            ink.push((x, y));
        }
    }
    if ink.is_empty() {
        return None;
    }
    let bbox = Region {
        x: min_x,
        y: min_y,
        w: max_x - min_x + 1,
        h: max_y - min_y + 1,
    };
    let ink = ink
        .into_iter()
        .map(|(x, y)| (x - min_x, y - min_y))
        .collect();
    Some(GlyphMask { bbox, ink })
}

/// 所有段高度的中位数（`-` 只占少数，故中位数即满高基准）。
fn median_height(masks: &[GlyphMask]) -> u32 {
    let mut heights: Vec<u32> = masks.iter().map(|mask| mask.bbox.h).collect();
    heights.sort_unstable();
    heights[heights.len() / 2]
}

/// 整条带子里必须至少有一个满高字形，否则拒绝出字（不瞎猜）。
fn ensure_full_height_anchor(masks: &[GlyphMask], band_height: u32) -> Result<(), VisionError> {
    let tallest = masks.iter().map(|mask| mask.bbox.h).max().unwrap_or(0);
    if (tallest as f32) >= LED_ANCHOR_MIN_BAND_FILL * band_height as f32 {
        return Ok(());
    }
    Err(VisionError::LedNoAnchor {
        tallest,
        band_height,
        min_ratio: LED_ANCHOR_MIN_BAND_FILL,
    })
}

/// 单个字符段的判定：宽高比主判据 → 结构签名消歧 → 模板 NCC 复核。
fn classify(
    index: usize,
    mask: &GlyphMask,
    glyph: &GrayImage,
    reference_height: u32,
    templates: &[Glyph],
) -> Result<char, VisionError> {
    let candidate = aspect_candidate(mask, reference_height);
    let template_score = template_ncc(glyph, candidate, templates);
    let shape_score = shape_ncc(mask, candidate);
    if template_score >= LED_TEMPLATE_MIN_SCORE || shape_score >= LED_SHAPE_ACCEPT {
        return Ok(candidate);
    }
    Err(VisionError::LedAmbiguous {
        index,
        template_score,
        shape_score,
    })
}

/// 宽高比主判据（spec §7.1）。宽体一格用结构签名消歧 `0` / 带衬线的 `1`。
fn aspect_candidate(mask: &GlyphMask, reference_height: u32) -> char {
    if (mask.bbox.h as f32) < LED_DASH_HEIGHT_RATIO * reference_height as f32 {
        return '-';
    }
    if mask.bbox.aspect() <= LED_ZERO_ASPECT {
        return '1';
    }
    // 宽体：`0`（环/框）与「满宽底衬线的 1」包围盒同宽，只能看内部墨迹分布。
    decisive_shape(mask).unwrap_or('0')
}

/// 结构签名的决定性判定：得分达到 [`LED_SHAPE_DECISIVE`] 才返回字符。
fn decisive_shape(mask: &GlyphMask) -> Option<char> {
    let signature = shape_signature(mask);
    let mut best = (f32::MIN, '?');
    for (character, pattern) in PATTERNS.iter().filter(|(c, _)| *c != '-') {
        let score = ncc(&signature, &pattern_vector(pattern));
        if score > best.0 {
            best = (score, *character);
        }
    }
    (best.0 >= LED_SHAPE_DECISIVE).then_some(best.1)
}

/// 判定字符的 5×3 结构签名得分（第二判据的「可接受」来源）。
fn shape_ncc(mask: &GlyphMask, character: char) -> f32 {
    let Some((_, pattern)) = PATTERNS.iter().find(|(c, _)| *c == character) else {
        return 0.0;
    };
    ncc(&shape_signature(mask), &pattern_vector(pattern))
}

/// 墨迹分布压成 5 行 × 3 列的平均覆盖率（0.0~1.0）。
fn shape_signature(mask: &GlyphMask) -> Vec<f32> {
    let mut counts = [0u32; SIGNATURE_ROWS * SIGNATURE_COLS];
    for &(x, y) in &mask.ink {
        let row = bucket(y, mask.bbox.h, SIGNATURE_ROWS);
        let column = bucket(x, mask.bbox.w, SIGNATURE_COLS);
        counts[row * SIGNATURE_COLS + column] += 1;
    }
    let mut signature = vec![0.0f32; counts.len()];
    for row in 0..SIGNATURE_ROWS {
        for column in 0..SIGNATURE_COLS {
            let width = span(mask.bbox.w, column, SIGNATURE_COLS);
            let height = span(mask.bbox.h, row, SIGNATURE_ROWS);
            let area = (width as f32) * (height as f32);
            signature[row * SIGNATURE_COLS + column] =
                (counts[row * SIGNATURE_COLS + column] as f32 / area).clamp(0.0, 1.0);
        }
    }
    signature
}

/// 把坐标 `value`（范围 `0..extent`）落到 `parts` 等分桶中的桶号。
fn bucket(value: u32, extent: u32, parts: usize) -> usize {
    if extent == 0 {
        return 0;
    }
    (((value as usize) * parts) / extent as usize).min(parts - 1)
}

/// 第 `index` 个等分桶覆盖的像素数（至少 1，避免除零）。
fn span(extent: u32, index: usize, parts: usize) -> u32 {
    let begin = (extent as usize * index) / parts;
    let end = (extent as usize * (index + 1)) / parts;
    (end.saturating_sub(begin)).max(1) as u32
}

/// 图案 → 灰度向量（有墨 = 1.0）。
fn pattern_vector(pattern: &[[u8; SIGNATURE_COLS]; SIGNATURE_ROWS]) -> Vec<f32> {
    pattern
        .iter()
        .flat_map(|row| row.iter())
        .map(|&cell| f32::from(cell))
        .collect()
}

/// 内嵌真实字形模板（懒解码一次）。
fn templates() -> Result<&'static [Glyph], VisionError> {
    static CACHE: OnceLock<Option<Vec<Glyph>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            TEMPLATE_PNGS
                .iter()
                .map(|(character, bytes)| {
                    let image = decode_png_gray(bytes).ok()?;
                    Some((*character, normalized(&image).ok()?))
                })
                .collect::<Option<Vec<Glyph>>>()
        })
        .as_deref()
        .ok_or(VisionError::LedTemplateUnavailable)
}

/// 解码内嵌 PNG 为 32×32 灰度。
fn decode_png_gray(bytes: &[u8]) -> Result<GrayImage, VisionError> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|_| VisionError::LedTemplateUnavailable)?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|_| VisionError::LedTemplateUnavailable)?;
    let (w, h) = (info.width, info.height);
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err(VisionError::LedTemplateUnavailable),
    };
    let mut pixels = Vec::with_capacity((w as usize) * (h as usize));
    for pixel in buffer[..(w as usize) * (h as usize) * channels].chunks_exact(channels) {
        let value = if channels >= 3 {
            ((77 * u16::from(pixel[0]) + 150 * u16::from(pixel[1]) + 29 * u16::from(pixel[2]))
                >> 8) as u8
        } else {
            pixel[0]
        };
        pixels.push(value);
    }
    GrayImage::new(w, h, pixels)
}

/// 字形归一化：裁墨迹包围盒 → 等比缩放到塞进 32×32 → 居中（与内嵌模板同口径）。
fn normalized(image: &GrayImage) -> Result<Vec<f32>, VisionError> {
    let bbox = ink_bbox(image)?;
    let cropped = image.crop(bbox)?;
    let scale = (TEMPLATE_SIZE as f32 / cropped.w as f32)
        .min(TEMPLATE_SIZE as f32 / cropped.h as f32);
    let target_w = ((cropped.w as f32 * scale).round() as u32).clamp(1, TEMPLATE_SIZE);
    let target_h = ((cropped.h as f32 * scale).round() as u32).clamp(1, TEMPLATE_SIZE);
    let resized = cropped.resize_to(target_w, target_h)?;
    let offset_x = (TEMPLATE_SIZE - target_w) / 2;
    let offset_y = (TEMPLATE_SIZE - target_h) / 2;
    let mut canvas = vec![0.0f32; (TEMPLATE_SIZE * TEMPLATE_SIZE) as usize];
    for y in 0..target_h {
        for x in 0..target_w {
            let index = ((y + offset_y) * TEMPLATE_SIZE + x + offset_x) as usize;
            canvas[index] = f32::from(resized.pixel(x, y));
        }
    }
    Ok(canvas)
}

/// 灰度图的墨迹包围盒。
///
/// 阈值优先用 Otsu；**全均匀的图**（例如归一化后的实心点阵块）Otsu 会给出「无前景」，
/// 此时整块本身就是墨迹，包围盒取全图 —— 否则实心字形会被误判成「没有墨迹」。
fn ink_bbox(image: &GrayImage) -> Result<Region, VisionError> {
    let OtsuResult { binary, .. } = image.otsu();
    let columns = binary.width();
    let rows = binary.height();
    let mut min_x = u32::MAX;
    let mut max_x = 0;
    let mut min_y = u32::MAX;
    let mut max_y = 0;
    for y in 0..rows {
        for x in 0..columns {
            if !binary.get(x, y) {
                continue;
            }
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
    }
    if min_x != u32::MAX {
        return Ok(Region {
            x: min_x,
            y: min_y,
            w: max_x - min_x + 1,
            h: max_y - min_y + 1,
        });
    }
    if image.as_slice().iter().any(|value| *value > 0) {
        return Ok(Region {
            x: 0,
            y: 0,
            w: columns,
            h: rows,
        });
    }
    Err(VisionError::NoGlyph)
}

/// 字形与内嵌模板的 NCC（同归一到 32×32 后逐元素）。
fn template_ncc(glyph: &GrayImage, character: char, templates: &[Glyph]) -> f32 {
    let Some((_, reference)) = templates.iter().find(|(c, _)| *c == character) else {
        return 0.0;
    };
    match normalized(glyph) {
        Ok(vector) => ncc(&vector, reference),
        Err(_) => 0.0,
    }
}

/// 向量版 NCC（零方差 → 0.0）。
fn ncc(left: &[f32], right: &[f32]) -> f32 {
    let count = left.len() as f32;
    if count == 0.0 || right.len() != left.len() {
        return 0.0;
    }
    let left_mean = left.iter().sum::<f32>() / count;
    let right_mean = right.iter().sum::<f32>() / count;
    let mut covariance = 0.0f32;
    let mut left_variance = 0.0f32;
    let mut right_variance = 0.0f32;
    for (a, b) in left.iter().zip(right) {
        let da = a - left_mean;
        let db = b - right_mean;
        covariance += da * db;
        left_variance += da * da;
        right_variance += db * db;
    }
    let denominator = (left_variance * right_variance).sqrt();
    if denominator <= f32::EPSILON {
        return 0.0;
    }
    (covariance / denominator).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// spec §7.1 实测字形盒（单位像素，真实帧读数）。
    const ZERO_BOX: (u32, u32) = (21, 32);
    const ONE_BOX: (u32, u32) = (13, 32);
    const DASH_BOX: (u32, u32) = (13, 8);

    /// 按实测宽高比渲染一条带子：字形盒宽度/高度照抄 spec §7.1，字间 3px 空白。
    fn render_measured_strip(characters: &str) -> GrayImage {
        let band_height = 32u32;
        let gap = 3u32;
        let boxes: Vec<(u32, u32)> = characters
            .chars()
            .map(|character| match character {
                '0' => ZERO_BOX,
                '1' => ONE_BOX,
                '-' => DASH_BOX,
                other => panic!("测试只渲染 0/1/-，收到 {other}"),
            })
            .collect();
        let width = boxes.iter().map(|(w, _)| *w).sum::<u32>()
            + gap * (boxes.len() as u32).saturating_sub(1);
        let mut pixels = vec![0u8; (width * band_height) as usize];
        let mut pen_x = 0;
        for (w, h) in &boxes {
            // 满高字形顶齐满，`-` 垂直居中（真实字形在带子中间）。
            let top = (band_height - h) / 2;
            for y in top..top + h {
                for x in pen_x..pen_x + w {
                    pixels[(y * width + x) as usize] = 255;
                }
            }
            pen_x += w + gap;
        }
        GrayImage::new(width, band_height, pixels).expect("尺寸合法")
    }

    #[test]
    fn reads_spec_measured_target_sequence() {
        // spec §7.1 样例读数：`01-00-01-11`（11 个字符，8 满高 + 3 个横线）。
        let image = render_measured_strip("01-00-01-11");
        assert_eq!(
            read_glyph_string(&image).expect("实测宽高比必须全部读对"),
            "01-00-01-11"
        );
    }

    #[test]
    fn median_reference_height_ignores_dashes() {
        // 3 个 `-` 只占少数：中位数仍是 32，`-` 才不会被当成矮体 `0`。
        let image = render_measured_strip("01-00-01-11");
        let masks = segment_masks(&image.otsu().binary);
        assert_eq!(masks.len(), 11, "11 个字符段");
        assert_eq!(median_height(&masks), 32);
    }

    #[test]
    fn synthetic_blocky_glyphs_still_recognized() {
        // 与 `flow-console` 合成字料同形：5 行 × 3 列方块，`1` 带满宽底衬线（宽高比不可分）。
        let image = render_blocky_strip("01-00-01-11", 6);
        assert_eq!(read_glyph_string(&image).expect("结构签名应消歧"), "01-00-01-11");
    }

    #[test]
    fn scale_invariant_on_measured_boxes() {
        let small = render_measured_strip("10-1");
        let expected = read_glyph_string(&small).expect("基准读数");
        assert_eq!(expected, "10-1");
        // 同一批字形放大 2 倍后结论不变（宽高比与签名都与尺度无关）。
        let doubled = upscale(&small, 2);
        assert_eq!(read_glyph_string(&doubled).expect("放大后仍应识别"), expected);
    }

    #[test]
    fn dash_only_strip_is_rejected_instead_of_guessed() {
        // 退化输入：整条带子只有两段极矮的横线 → 没有满高锚点，必须报错。
        let image = render_measured_strip("--");
        let error = read_glyph_string(&image).expect_err("没有满高字形不得出字");
        assert!(
            matches!(error, VisionError::LedNoAnchor { tallest: 8, .. }),
            "{error}"
        );
    }

    #[test]
    fn unidentifiable_shape_is_rejected_instead_of_guessed() {
        // 宽高比落在 `0` 区间（21×32），但内部是一条斜线：两条判据都不达标 → 拒绝。
        let width = 21u32;
        let height = 32u32;
        let mut pixels = vec![0u8; (width * height) as usize];
        for y in 0..height {
            let x = (y * (width - 1)) / (height - 1);
            for dx in 0..2u32 {
                let px = (x + dx).min(width - 1);
                pixels[(y * width + px) as usize] = 255;
            }
        }
        let image = GrayImage::new(width, height, pixels).expect("尺寸合法");
        let error = read_glyph_string(&image).expect_err("斜线不得被猜成 0 或 1");
        assert!(
            matches!(error, VisionError::LedAmbiguous { index: 0, .. }),
            "{error}"
        );
    }

    #[test]
    fn blank_region_reports_no_glyph() {
        let image = GrayImage::filled(40, 20, 0).expect("合法图像");
        assert_eq!(read_glyph_string(&image), Err(VisionError::NoGlyph));
    }

    #[test]
    fn solid_block_reports_no_glyph() {
        // 全白：Otsu 后没有任何前景像素，属于「未点亮」，不是「识别失败」。
        let image = GrayImage::filled(9, 15, 255).expect("合法图像");
        assert_eq!(read_glyph_string(&image), Err(VisionError::NoGlyph));
    }

    #[test]
    fn embedded_templates_decode_to_32px_glyphs() {
        let templates = templates().expect("内嵌字形必须可解码");
        assert_eq!(templates.len(), 3);
        for (character, vector) in templates {
            assert_eq!(vector.len(), (TEMPLATE_SIZE * TEMPLATE_SIZE) as usize);
            let ink = vector.iter().filter(|value| **value > 0.0).count();
            assert!(ink > 100, "字形 {character} 的墨迹过少（{ink} px）");
        }
    }

    #[test]
    fn template_self_match_scores_one() {
        // 归一化口径必须自洽：内嵌字形与自己比，NCC 应为 1.0。
        let templates = templates().expect("内嵌字形");
        for (character, bytes) in TEMPLATE_PNGS {
            let glyph = decode_png_gray(bytes).expect("解码");
            let score = template_ncc(&glyph, character, templates);
            assert!(
                score > 0.99,
                "字形 {character} 自匹配得分 {score:.3} 异常（归一化口径不自洽）"
            );
        }
    }

    /// 与 `flow-console` 合成字料同形的渲染（5×3 方块，`1` 满宽底衬线）。
    fn render_blocky_strip(characters: &str, cell: u32) -> GrayImage {
        let glyph_h = cell * SIGNATURE_ROWS as u32;
        let advance = cell * 4;
        let count = characters.chars().count() as u32;
        let width = 2 + advance * count;
        let mut pixels = vec![0u8; (width * glyph_h) as usize];
        let mut pen_x = 2u32;
        for character in characters.chars() {
            let pattern = PATTERNS
                .iter()
                .find(|(c, _)| *c == character)
                .map(|(_, pattern)| pattern)
                .expect("只渲染内嵌字符");
            for (row, line) in pattern.iter().enumerate() {
                for (column, &ink) in line.iter().enumerate() {
                    if ink == 0 {
                        continue;
                    }
                    for dy in 0..cell {
                        for dx in 0..cell {
                            let x = pen_x + column as u32 * cell + dx;
                            let y = row as u32 * cell + dy;
                            pixels[(y * width + x) as usize] = 255;
                        }
                    }
                }
            }
            pen_x += advance;
        }
        GrayImage::new(width, glyph_h, pixels).expect("尺寸合法")
    }

    /// 最近邻整数倍放大（保持宽高比，用于尺度不变性断言）。
    fn upscale(image: &GrayImage, factor: u32) -> GrayImage {
        let w = image.w * factor;
        let h = image.h * factor;
        let mut pixels = vec![0u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                pixels[(y * w + x) as usize] = image.pixel(x / factor, y / factor);
            }
        }
        GrayImage::new(w, h, pixels).expect("尺寸合法")
    }
}
