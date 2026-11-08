//! 结构特征测量（**与分辨率无关**：全部输出都是比值，不含绝对像素常数）。
//!
//! 一键场景判别（spec D6）禁止用整屏模板匹配，理由是一换分辨率就失效；
//! 因此这里只用三类结构量：亮/前景占比、轮廓长条数、列投影周期结构。

use delta_geom::PxRect;
use delta_vision::{
    external_contours, BinaryImage, GrayImage, Region, DASH_ASPECT_THRESHOLD, NOISE_AREA_GATE,
};

/// 像素矩形 → 图像内整数区域；零面积或越界返回 `None`（不静默裁剪）。
pub fn to_region(rect: PxRect, image_w: u32, image_h: u32) -> Option<Region> {
    if rect.w <= 0 || rect.h <= 0 || rect.x < 0 || rect.y < 0 {
        return None;
    }
    let region = Region {
        x: rect.x as u32,
        y: rect.y as u32,
        w: rect.w as u32,
        h: rect.h as u32,
    };
    if region.x.checked_add(region.w)? > image_w || region.y.checked_add(region.h)? > image_h {
        return None;
    }
    Some(region)
}

/// 按像素矩形裁剪灰度图；矩形非法时返回 `None`（对应区域视为「无内容」）。
pub fn crop(gray: &GrayImage, rect: PxRect) -> Option<GrayImage> {
    let region = to_region(rect, gray.w, gray.h)?;
    gray.crop(region).ok()
}

/// 线性斜坡：`low` 以下 → 0，`high` 以上 → 1，中间线性。
pub fn ramp(value: f32, low: f32, high: f32) -> f32 {
    if !(value.is_finite() && low.is_finite() && high.is_finite()) {
        return 0.0;
    }
    if high <= low {
        return if value >= high { 1.0 } else { 0.0 };
    }
    ((value - low) / (high - low)).clamp(0.0, 1.0)
}

/// Otsu 前景占比（0..1）。用前景占比而不是绝对亮度，暗底亮字与亮底暗字一视同仁。
pub fn otsu_foreground_ratio(gray: &GrayImage) -> f32 {
    let binary = gray.otsu().binary;
    let area = gray.w as f32 * gray.h as f32;
    if area <= 0.0 {
        return 0.0;
    }
    binary.foreground_count() as f32 / area
}

/// 归一化平均梯度幅值（0..1）：`mean(|dx| + |dy|) / 2 / 255`。
///
/// 分辨率无关：是单位像素的平均跳变幅度，与格子大小无关。
pub fn edge_density(gray: &GrayImage) -> f32 {
    if gray.w < 2 || gray.h < 2 {
        return 0.0;
    }
    let mut total = 0u64;
    for y in 0..gray.h {
        let row = gray.row(y);
        for x in 0..(gray.w - 1) {
            let (left, right) = (row[x as usize], row[x as usize + 1]);
            total += u64::from(left.abs_diff(right));
        }
    }
    for y in 0..(gray.h - 1) {
        let (upper, lower) = (gray.row(y), gray.row(y + 1));
        for x in 0..gray.w {
            total += u64::from(upper[x as usize].abs_diff(lower[x as usize]));
        }
    }
    let samples = u64::from(gray.w) * u64::from(gray.h - 1) + u64::from(gray.w - 1) * u64::from(gray.h);
    if samples == 0 {
        return 0.0;
    }
    (total as f32 / samples as f32) / 255.0
}

/// 摩斯格里的轮廓统计：过滤噪点后的轮廓总数与「长条（点划里的划）」数。
///
/// 口径严格对齐 `morse.py:37-42` 与 spec §2.2 不变量 7：
/// `w*h < 10` 直接丢弃；`aspect > 2.0` 判划（`== 2.0` 判点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutlineStats {
    pub total: usize,
    pub dashes: usize,
}

impl OutlineStats {
    /// 上游点划结构的必要条件：至少 3 个符号，且至少有 1 条划。
    pub fn is_symbol_like(self) -> bool {
        self.total >= 3 && self.dashes >= 1
    }
}

/// 单格轮廓统计。
pub fn outline_stats(gray: &GrayImage) -> OutlineStats {
    let binary = gray.otsu().binary;
    let mut stats = OutlineStats {
        total: 0,
        dashes: 0,
    };
    for contour in external_contours(&binary) {
        if contour.region.area() < NOISE_AREA_GATE {
            continue;
        }
        stats.total += 1;
        if contour.aspect() > DASH_ASPECT_THRESHOLD {
            stats.dashes += 1;
        }
    }
    stats
}

/// 列投影切字符：每列前景像素数 > `min_pixels` 视为字符列，返回连续段个数。
///
/// 与 `delta_vision::read_glyph_string` 的列切分同口径（spec §7.1：列内亮像素 >2）。
pub fn column_segments(binary: &BinaryImage, min_pixels: u32) -> usize {
    let w = binary.width();
    let h = binary.height();
    let mut segments = 0usize;
    let mut inside = false;
    for x in 0..w {
        let count = (0..h).filter(|&y| binary.get(x, y)).count() as u32;
        let is_text_column = count > min_pixels;
        if is_text_column && !inside {
            segments += 1;
        }
        inside = is_text_column;
    }
    segments
}

/// 等距竖列结构：列投影的峰个数（峰间距必须均匀，离散度 ≤ [`PEAK_SPACING_SPREAD`]）。
///
/// 操作台面板的「4 列旋钮」就是这种周期结构；指纹候选格是 3×3 网格，
/// 竖列是 3 条，因此这里要求至少 4 条才算命中，避免把指纹界面误判成操作台。
pub fn equidistant_column_peaks(binary: &BinaryImage, min_peaks: usize) -> Option<usize> {
    const PEAK_RELATIVE_THRESHOLD: f32 = 0.4;
    const PEAK_SPACING_SPREAD: f32 = 0.3;

    let w = binary.width();
    let h = binary.height();
    if w == 0 || h == 0 {
        return None;
    }
    let sums: Vec<u32> = (0..w)
        .map(|x| (0..h).filter(|&y| binary.get(x, y)).count() as u32)
        .collect();
    let tallest = *sums.iter().max()?;
    if tallest == 0 {
        return None;
    }
    let threshold = (tallest as f32 * PEAK_RELATIVE_THRESHOLD).max(1.0);

    let mut centers: Vec<f32> = Vec::new();
    let mut run_start: Option<usize> = None;
    for (x, &sum) in sums.iter().enumerate() {
        let above = sum as f32 >= threshold;
        match (above, run_start) {
            (true, None) => run_start = Some(x),
            (false, Some(start)) => {
                centers.push((start as f32 + x as f32 - 1.0) / 2.0);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        centers.push((start as f32 + w as f32 - 1.0) / 2.0);
    }

    if centers.len() < min_peaks {
        return None;
    }
    let gaps: Vec<f32> = centers.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let mean = gaps.iter().sum::<f32>() / gaps.len() as f32;
    if mean <= 0.0 {
        return None;
    }
    let spread = gaps
        .iter()
        .map(|gap| (gap - mean).abs())
        .fold(0.0f32, f32::max)
        / mean;
    (spread <= PEAK_SPACING_SPREAD).then_some(centers.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_vision::GrayImage;

    #[test]
    fn ramp_clamps_both_ends() {
        assert_eq!(ramp(0.0, 0.01, 0.05), 0.0);
        assert_eq!(ramp(0.05, 0.01, 0.05), 1.0);
        assert!((ramp(0.03, 0.01, 0.05) - 0.5).abs() < 1e-6);
        assert_eq!(ramp(f32::NAN, 0.01, 0.05), 0.0, "非法输入不得产生 NaN");
    }

    #[test]
    fn edge_density_is_zero_on_flat_and_high_on_stripes() {
        let flat = GrayImage::filled(16, 16, 90).expect("合法图");
        assert_eq!(edge_density(&flat), 0.0);

        let mut stripes = Vec::new();
        for y in 0..16u32 {
            for _ in 0..16u32 {
                stripes.push(if y % 2 == 0 { 255u8 } else { 0u8 });
            }
        }
        let striped = GrayImage::new(16, 16, stripes).expect("合法图");
        // 只有竖向跳变（横向跳变为 0），因此上限是 0.5。
        assert!(edge_density(&striped) > 0.4, "{}", edge_density(&striped));
        assert!(edge_density(&GrayImage::filled(1, 1, 0).expect("1x1")) == 0.0);
    }

    #[test]
    fn outline_stats_rejects_noise_and_counts_dashes() {
        let blank = GrayImage::filled(40, 20, 0).expect("合法图");
        assert_eq!(outline_stats(&blank).total, 0);

        // 一个 30x6 的长条 → 划；aspect = 5 > 2。
        let mut data = vec![0u8; 40 * 20];
        for y in 5..11u32 {
            for x in 2..32u32 {
                data[(y * 40 + x) as usize] = 255;
            }
        }
        let dash = GrayImage::new(40, 20, data).expect("合法图");
        let stats = outline_stats(&dash);
        assert_eq!(stats.total, 1);
        assert_eq!(stats.dashes, 1);
        assert!(!stats.is_symbol_like(), "只有 1 个轮廓不足以判符号");
    }

    #[test]
    fn otsu_foreground_ratio_ignores_uniform_frames() {
        assert_eq!(otsu_foreground_ratio(&GrayImage::filled(8, 8, 0).expect("合法")), 0.0);
        assert_eq!(otsu_foreground_ratio(&GrayImage::filled(8, 8, 128).expect("合法")), 0.0);
    }

    #[test]
    fn column_segments_splits_on_gaps() {
        let mut binary = BinaryImage::new(10, 4).expect("合法");
        for x in [1u32, 2, 5] {
            for y in 0..4u32 {
                binary.set(x, y, true);
            }
        }
        assert_eq!(column_segments(&binary, 2), 2, "两个字符段");
        assert_eq!(column_segments(&binary, 3), 2);
        assert_eq!(column_segments(&BinaryImage::new(4, 4).expect("合法"), 0), 0);
    }

    #[test]
    fn equidistant_column_peaks_needs_four_evenly_spaced_columns() {
        let mut panel = BinaryImage::new(80, 20).expect("合法");
        for x in [10u32, 30, 50, 70] {
            for y in 0..20u32 {
                panel.set(x, y, true);
            }
        }
        assert_eq!(equidistant_column_peaks(&panel, 4), Some(4));

        let mut three = BinaryImage::new(80, 20).expect("合法");
        for x in [10u32, 30, 50] {
            for y in 0..20u32 {
                three.set(x, y, true);
            }
        }
        assert_eq!(equidistant_column_peaks(&three, 4), None, "3 列不得判 4 列");

        let mut uneven = BinaryImage::new(80, 20).expect("合法");
        for x in [2u32, 20, 26, 70] {
            for y in 0..20u32 {
                uneven.set(x, y, true);
            }
        }
        assert_eq!(
            equidistant_column_peaks(&uneven, 4),
            None,
            "间距不均的 4 列不得命中"
        );
        assert_eq!(
            equidistant_column_peaks(&BinaryImage::new(8, 8).expect("合法"), 4),
            None
        );
    }
}
