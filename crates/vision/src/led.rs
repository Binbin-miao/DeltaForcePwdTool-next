//! 点阵字符读取（操作台 LED 目标序列，spec D7：字符集仅 `0` `1` `-`）。
//!
//! 流程：Otsu → 按列投影切字符格 → 每格归一化为 5×3 的灰度签名 → 与内嵌三套点阵模板做 NCC。
//! 只取列方向切分、保留整幅高度，这样 `-` 这种只在中间一行有墨的字符不会被裁掉变成常量图。
//! 阈值由合成图测试锁定；低于门限显式报错，绝不返回猜出来的字符。

use crate::{BinaryImage, GrayImage, OtsuResult, VisionError};

/// 字符识别门限；低于该 NCC 得分即视为不可识别。
pub const LED_MIN_SCORE: f32 = 0.5;

/// 归一化签名的行列数（5 行 × 3 列）。
const SIGNATURE_ROWS: usize = 5;
const SIGNATURE_COLS: usize = 3;

/// 内嵌点阵模板：`(字符, 5 行 × 3 列 点阵)`。
const GLYPHS: [(char, [[u8; SIGNATURE_COLS]; SIGNATURE_ROWS]); 3] = [
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

/// 读取一行点阵字符，返回如 `"10-"` 的字符串。
pub fn read_glyph_string(image: &GrayImage) -> Result<String, VisionError> {
    let OtsuResult { binary, .. } = image.otsu();
    let segments = column_segments(&binary);
    if segments.is_empty() {
        return Err(VisionError::NoGlyph);
    }

    let mut recognized = String::with_capacity(segments.len());
    for (index, (start, end_inclusive)) in segments.iter().enumerate() {
        let signature = column_signature(image, *start, *end_inclusive + 1);
        let mut best = (f32::MIN, '?');
        for (character, pattern) in GLYPHS {
            let score = ncc(&signature, &glyph_vector(&pattern));
            if score > best.0 {
                best = (score, character);
            }
        }
        if best.0 < LED_MIN_SCORE {
            return Err(VisionError::GlyphUnrecognized {
                index,
                best_score: best.0,
                min_score: LED_MIN_SCORE,
            });
        }
        recognized.push(best.1);
    }
    Ok(recognized)
}

/// 按列投影切出有墨的连续列区间（闭区间）。
fn column_segments(binary: &BinaryImage) -> Vec<(u32, u32)> {
    let mut segments = Vec::new();
    let mut start: Option<u32> = None;
    for x in 0..binary.width() {
        let has_ink = (0..binary.height()).any(|y| binary.get(x, y));
        match (has_ink, start) {
            (true, None) => start = Some(x),
            (false, Some(begin)) => {
                segments.push((begin, x - 1));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        segments.push((begin, binary.width() - 1));
    }
    segments
}

/// 把一个字符区间（`[start, end)` 列 × 全高）压成 5×3 灰度签名。
fn column_signature(image: &GrayImage, start: u32, end: u32) -> Vec<f32> {
    let end = end.min(image.w).max(start + 1);
    let mut signature = vec![0.0f32; SIGNATURE_ROWS * SIGNATURE_COLS];
    for row in 0..SIGNATURE_ROWS {
        let y0 = (image.h as usize * row) / SIGNATURE_ROWS;
        let y1 = (image.h as usize * (row + 1)) / SIGNATURE_ROWS;
        let y1 = y1.max(y0 + 1).min(image.h as usize);
        for column in 0..SIGNATURE_COLS {
            let x0 = start as usize + ((end - start) as usize * column) / SIGNATURE_COLS;
            let x1 = start as usize + ((end - start) as usize * (column + 1)) / SIGNATURE_COLS;
            let x1 = x1.max(x0 + 1).min(end as usize);
            let mut sum = 0u32;
            let mut count = 0u32;
            for y in y0..y1 {
                let row_slice = image.row(y as u32);
                for &value in &row_slice[x0..x1] {
                    sum += u32::from(value);
                    count += 1;
                }
            }
            signature[row * SIGNATURE_COLS + column] = if count == 0 {
                0.0
            } else {
                sum as f32 / count as f32
            };
        }
    }
    signature
}

/// 点阵模板 → 灰度向量（有墨 = 255）。
fn glyph_vector(pattern: &[[u8; SIGNATURE_COLS]; SIGNATURE_ROWS]) -> Vec<f32> {
    pattern
        .iter()
        .flat_map(|row| row.iter())
        .map(|&cell| if cell == 1 { 255.0 } else { 0.0 })
        .collect()
}

/// 向量版 NCC（零方差 → 0.0）。
fn ncc(left: &[f32], right: &[f32]) -> f32 {
    let count = left.len() as f32;
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

    /// 把点阵字符渲染成位图条：每格 3×3 像素，字间 2 像素空白。
    fn render_strip(characters: &str) -> GrayImage {
        let cell = 3u32;
        let glyph_w = cell * SIGNATURE_COLS as u32;
        let glyph_h = cell * SIGNATURE_ROWS as u32;
        let gap = 2u32;
        let count = characters.chars().count() as u32;
        let width = count * glyph_w + count.saturating_sub(1) * gap;
        let mut pixels = vec![0u8; (width * glyph_h) as usize];

        for (index, character) in characters.chars().enumerate() {
            let pattern = GLYPHS
                .iter()
                .find(|(c, _)| *c == character)
                .map(|(_, pattern)| *pattern)
                .expect("测试只渲染内嵌字符");
            let origin_x = index as u32 * (glyph_w + gap);
            for (row, pattern_row) in pattern.iter().enumerate() {
                for (column, &ink) in pattern_row.iter().enumerate() {
                    if ink == 0 {
                        continue;
                    }
                    for dy in 0..cell {
                        for dx in 0..cell {
                            let x = origin_x + column as u32 * cell + dx;
                            let y = row as u32 * cell + dy;
                            pixels[(y * width + x) as usize] = 255;
                        }
                    }
                }
            }
        }
        GrayImage::new(width, glyph_h, pixels).expect("渲染尺寸合法")
    }

    #[test]
    fn reads_full_target_sequence() {
        let image = render_strip("10-");
        assert_eq!(read_glyph_string(&image).expect("应全部识别"), "10-");
    }

    #[test]
    fn reads_each_single_character() {
        for character in ['0', '1', '-'] {
            let image = render_strip(&character.to_string());
            assert_eq!(
                read_glyph_string(&image).expect("单字符应识别"),
                character.to_string()
            );
        }
    }

    #[test]
    fn scaled_glyphs_still_recognized() {
        // 放大 3 倍（每格 9 像素）仍应识别：签名归一化与尺度无关。
        let cell = 9u32;
        let glyph_w = cell * SIGNATURE_COLS as u32;
        let glyph_h = cell * SIGNATURE_ROWS as u32;
        let width = glyph_w * 4 + 3 * 4;
        let mut pixels = vec![0u8; (width * glyph_h) as usize];
        for (index, character) in "01-1".chars().enumerate() {
            let pattern = GLYPHS
                .iter()
                .find(|(c, _)| *c == character)
                .map(|(_, pattern)| *pattern)
                .expect("内嵌字符");
            let origin_x = index as u32 * (glyph_w + 4);
            for (row, pattern_row) in pattern.iter().enumerate() {
                for (column, &ink) in pattern_row.iter().enumerate() {
                    if ink == 0 {
                        continue;
                    }
                    for dy in 0..cell {
                        for dx in 0..cell {
                            let x = origin_x + column as u32 * cell + dx;
                            let y = row as u32 * cell + dy;
                            pixels[(y * width + x) as usize] = 255;
                        }
                    }
                }
            }
        }
        let image = GrayImage::new(width, glyph_h, pixels).expect("尺寸合法");
        assert_eq!(read_glyph_string(&image).expect("应识别"), "01-1");
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
    fn unknown_shape_is_rejected_instead_of_guessed() {
        // 竖条：有墨但不符合 0/1/- 任一模板，必须显式拒绝。
        let mut pixels = vec![0u8; 3 * 15];
        for y in 0..15 {
            pixels[y * 3] = 255;
        }
        let image = GrayImage::new(3, 15, pixels).expect("合法图像");
        let error = read_glyph_string(&image).expect_err("未知字形不得被猜成字符");
        assert!(
            matches!(error, VisionError::GlyphUnrecognized { index: 0, .. }),
            "{error}"
        );
    }

    #[test]
    fn signature_is_scale_invariant_on_constant_cell() {
        let image = GrayImage::filled(9, 15, 255).expect("合法图像");
        let signature = column_signature(&image, 0, 9);
        assert!(signature.iter().all(|&value| value == 255.0));
    }
}
