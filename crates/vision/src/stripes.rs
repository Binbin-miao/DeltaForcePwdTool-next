//! 黄黑斜纹遮蔽层检测（spec D7：操作台「完成」判据）。
//!
//! 对外唯一入口是 [`detect_yellow_hatch`]，它按 spec §7.3 的顺序分两阶段执行：
//!
//! 1. **黄色门（彩色域）**：先用 `H∈[20,35], S≥120, V≥180` 的亮黄像素做连通域，
//!    由足够大的连通域并出**矩形候选包围盒**。找不到候选 → 直接 `false`。
//!    这一阶段是防误判的关键：面板内任何高对比 45° 图案（黑白斜纹对话框、网格纹理）
//!    都不会被当成浮层，从而不会让状态机提前「跳列」。
//! 2. **方向性 + 交替性（候选包围盒内）**：[`detect_diagonal_stripe`] 在候选包围盒内判断
//!    「45° 斜纹是 `I = f(x - y)`，沿 `(1,-1)` 的梯度能量显著大于 `(1,1)`」以及
//!    「足够比例的行上存在多次高对比明暗翻转」。横纹/竖纹/纯黄块在这一阶段被否决。
//!
//! 阈值全部由 [`DiagonalStripeParams`] 暴露，默认值由本文件的合成图测试锁定。

use crate::{GrayImage, Region, VisionError};

/// 斜纹检测参数（默认值见 [`DiagonalStripeParams::default`]）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiagonalStripeParams {
    /// 45° 方向平均梯度能量下限（0..255 灰度差）。
    pub min_anti_energy: f32,
    /// `anti / (diag + 1)` 的比值下限，用来排除横纹与竖纹。
    pub min_direction_ratio: f32,
    /// 判为一次「明暗翻转」所需的最小相邻列灰度差。
    pub min_contrast: u8,
    /// 满足交替性的行占全部行的比例下限。
    pub min_alternating_row_ratio: f32,
    /// 单行内至少需要几次翻转才算「交替行」。
    pub min_transitions_per_row: u32,
    /// 黄色门第一阶段：色域 hue 下限（OpenCV 8 位标度 `0..180`，spec §7.3 `H∈[20,35]`）。
    pub box_hue_min: f32,
    /// 黄色门第一阶段：色域 hue 上限。
    pub box_hue_max: f32,
    /// 黄色门第一阶段：饱和度下限（spec §7.3 `S≥120`，标度 `0..255`）。
    pub box_sat_min: f32,
    /// 黄色门第一阶段：明度下限（spec §7.3 `V≥180`）。
    pub box_val_min: f32,
    /// 单个黄色连通域至少这么多个像素才参与候选（滤掉旋钮之类的小块）。
    pub box_min_component_pixels: u32,
    /// 候选包围盒最小宽度（spec §7.3 实测浮层约 250×70，下限留足移动/缩放余量）。
    pub box_min_w: u32,
    /// 候选包围盒最小高度。
    pub box_min_h: u32,
    /// 候选包围盒内黄色像素密度下限：散点把包围盒撑大时不得充当候选。
    pub box_min_yellow_density: f32,
}

impl Default for DiagonalStripeParams {
    fn default() -> Self {
        Self {
            min_anti_energy: 20.0,
            min_direction_ratio: 1.5,
            min_contrast: 40,
            min_alternating_row_ratio: 0.5,
            min_transitions_per_row: 2,
            box_hue_min: 20.0,
            box_hue_max: 35.0,
            box_sat_min: 120.0,
            box_val_min: 180.0,
            box_min_component_pixels: 200,
            box_min_w: 80,
            box_min_h: 24,
            box_min_yellow_density: 0.08,
        }
    }
}

impl DiagonalStripeParams {
    /// 校验参数合理（防止除零、负阈值导致的恒真）。
    fn validate(&self) -> Result<(), VisionError> {
        let finite = self.min_anti_energy.is_finite() && self.min_anti_energy >= 0.0;
        if !finite {
            return Err(VisionError::InvalidParams {
                name: "min_anti_energy",
            });
        }
        if !self.min_direction_ratio.is_finite() || self.min_direction_ratio <= 0.0 {
            return Err(VisionError::InvalidParams {
                name: "min_direction_ratio",
            });
        }
        if !(0.0..=1.0).contains(&self.min_alternating_row_ratio) {
            return Err(VisionError::InvalidParams {
                name: "min_alternating_row_ratio",
            });
        }
        if !self.box_hue_min.is_finite()
            || !self.box_hue_max.is_finite()
            || self.box_hue_max <= self.box_hue_min
        {
            return Err(VisionError::InvalidParams {
                name: "box_hue_min/box_hue_max",
            });
        }
        let channel_in_range =
            |value: f32| value.is_finite() && (0.0..=255.0).contains(&value);
        if !channel_in_range(self.box_sat_min) || !channel_in_range(self.box_val_min) {
            return Err(VisionError::InvalidParams {
                name: "box_sat_min/box_val_min",
            });
        }
        if !(0.0..=1.0).contains(&self.box_min_yellow_density) {
            return Err(VisionError::InvalidParams {
                name: "box_min_yellow_density",
            });
        }
        if self.box_min_w < 3 || self.box_min_h < 3 {
            return Err(VisionError::InvalidParams {
                name: "box_min_w/box_min_h",
            });
        }
        Ok(())
    }
}

/// 检测彩色帧内是否存在「黄色边框 + 内部 45° 黄黑斜纹」浮层（spec §7.3）。
///
/// 两阶段顺序不可交换：**先**在彩色帧里用黄色色域找矩形候选，**再**在候选包围盒内验证
/// 45° 方向性。找不到黄色候选时直接返回 `Ok(false)`，因此面板内其它高对比 45° 图案
/// 不会让状态机误判「该列已被接受」。
pub fn detect_yellow_hatch(
    bgra: &[u8],
    w: u32,
    h: u32,
    stride: u32,
    params: &DiagonalStripeParams,
) -> Result<bool, VisionError> {
    if w < 3 || h < 3 {
        return Err(VisionError::InvalidDimensions { w, h });
    }
    params.validate()?;
    let Some(region) = yellow_candidate(bgra, w, h, stride, params)? else {
        return Ok(false);
    };
    let candidate = crop_luma(bgra, stride, region);
    detect_diagonal_stripe(&candidate, params)
}

/// 第一阶段：黄色像素连通域 → 矩形候选包围盒。
fn yellow_candidate(
    bgra: &[u8],
    w: u32,
    h: u32,
    stride: u32,
    params: &DiagonalStripeParams,
) -> Result<Option<Region>, VisionError> {
    let mask = yellow_mask(bgra, w, h, stride, params)?;
    let min_pixels = params.box_min_component_pixels.max(1);
    let mut labels = vec![0u32; mask.len()];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_label = 0u32;
    let mut union: Option<Region> = None;
    let mut union_pixels = 0u32;

    for seed in 0..mask.len() {
        if !mask[seed] || labels[seed] != 0 {
            continue;
        }
        next_label += 1;
        labels[seed] = next_label;
        stack.push(seed);
        let mut pixels = 0u32;
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        while let Some(index) = stack.pop() {
            let (x, y) = (index as u32 % w, index as u32 / w);
            pixels += 1;
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            // 8 邻域：斜纹带的抗锯齿边缘会把对角相邻像素连成一片。
            for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                    let neighbor = (ny as usize) * (w as usize) + nx as usize;
                    if mask[neighbor] && labels[neighbor] == 0 {
                        labels[neighbor] = next_label;
                        stack.push(neighbor);
                    }
                }
            }
        }
        if pixels < min_pixels {
            continue;
        }
        let component = Region {
            x: x0,
            y: y0,
            w: x1 - x0 + 1,
            h: y1 - y0 + 1,
        };
        union = Some(match union {
            None => component,
            // 斜纹由多条互不相连的黄色带组成：合并成同一块浮层候选。
            Some(current) => {
                let x = current.x.min(component.x);
                let y = current.y.min(component.y);
                let right = (current.x + current.w).max(component.x + component.w);
                let bottom = (current.y + current.h).max(component.y + component.h);
                Region {
                    x,
                    y,
                    w: right - x,
                    h: bottom - y,
                }
            }
        });
        union_pixels += pixels;
    }

    let Some(candidate) = union else {
        return Ok(None);
    };
    if candidate.w < params.box_min_w || candidate.h < params.box_min_h {
        return Ok(None);
    }
    let density = union_pixels as f32 / candidate.area() as f32;
    if density < params.box_min_yellow_density {
        return Ok(None);
    }
    Ok(Some(candidate))
}

/// 逐像素黄色掩码（`stride` 允许大于 `w*4`）。
fn yellow_mask(
    bgra: &[u8],
    w: u32,
    h: u32,
    stride: u32,
    params: &DiagonalStripeParams,
) -> Result<Vec<bool>, VisionError> {
    let row_bytes = (w as usize) * 4;
    let stride = stride as usize;
    if stride < row_bytes || bgra.len() < stride.saturating_mul(h as usize) {
        return Err(VisionError::BufferSizeMismatch {
            w,
            h,
            expected: stride.saturating_mul(h as usize).max(row_bytes),
            actual: bgra.len(),
        });
    }
    let mut mask = Vec::with_capacity((w as usize) * (h as usize));
    for row in bgra.chunks_exact(stride).take(h as usize) {
        let (pixels, _remainder) = row[..row_bytes].as_chunks::<4>();
        for pixel in pixels {
            mask.push(is_hatch_yellow(pixel[0], pixel[1], pixel[2], params));
        }
    }
    Ok(mask)
}

/// 单个 BGRA 像素是否落在黄色边框色域内（OpenCV 8 位标度）。
fn is_hatch_yellow(b: u8, g: u8, r: u8, params: &DiagonalStripeParams) -> bool {
    let (hue, saturation, value) = hsv_from_bgra(b, g, r);
    hue >= params.box_hue_min
        && hue <= params.box_hue_max
        && saturation >= params.box_sat_min
        && value >= params.box_val_min
}

/// 8 位 BGRA → OpenCV 8 位 HSV（`h ∈ 0..180`，`s/v ∈ 0..255`），不引第三方依赖。
///
/// 标度必须与 `delta_flow_console::params::rgb_to_hsv` 一致，否则 spec §7 的标定阈值不可移植。
fn hsv_from_bgra(b: u8, g: u8, r: u8) -> (f32, f32, f32) {
    let (rf, gf, bf) = (f32::from(r), f32::from(g), f32::from(b));
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let delta = max - min;
    let saturation = if max <= 0.0 { 0.0 } else { 255.0 * delta / max };
    let raw_hue = if delta <= 0.0 {
        0.0
    } else if max == rf {
        30.0 * ((gf - bf) / delta)
    } else if max == gf {
        30.0 * (2.0 + (bf - rf) / delta)
    } else {
        30.0 * (4.0 + (rf - gf) / delta)
    };
    (
        if raw_hue < 0.0 { raw_hue + 180.0 } else { raw_hue },
        saturation,
        max,
    )
}

/// 把候选包围盒从彩色帧裁成灰度图（第二阶段判据的输入）。
fn crop_luma(bgra: &[u8], stride: u32, region: Region) -> GrayImage {
    let stride = stride as usize;
    let mut data = Vec::with_capacity(region.area() as usize);
    for y in region.y..region.y + region.h {
        let row = (y as usize) * stride;
        for x in region.x..region.x + region.w {
            let offset = row + (x as usize) * 4;
            data.push(crate::luma_from_bgra(
                bgra[offset],
                bgra[offset + 1],
                bgra[offset + 2],
            ));
        }
    }
    GrayImage::new(region.w, region.h, data).expect("候选包围盒来自实际像素坐标，尺寸必然自洽")
}

/// 第二阶段判据：在**已裁好的灰度图**内验证 45° 方向性与明暗交替性。
///
/// 这是 [`detect_yellow_hatch`] 的内部第二阶段，单独暴露只为让方向性判据可被独立测试；
/// **业务代码不应直接调用它** —— 它没有黄色门，任何高对比 45° 图案都能通过。
pub fn detect_diagonal_stripe(
    image: &GrayImage,
    params: &DiagonalStripeParams,
) -> Result<bool, VisionError> {
    if image.w < 3 || image.h < 3 {
        return Err(VisionError::InvalidDimensions {
            w: image.w,
            h: image.h,
        });
    }
    params.validate()?;

    let (anti_energy, anti_samples) = direction_energy(image, 1, -1)?;
    let (diag_energy, diag_samples) = direction_energy(image, 1, 1)?;
    let alternating_ratio = alternating_row_ratio(image, params);

    let anti_mean = anti_energy / (anti_samples.max(1) as f32);
    let diag_mean = diag_energy / (diag_samples.max(1) as f32);
    let directional = anti_mean >= params.min_anti_energy
        && anti_mean >= params.min_direction_ratio * (diag_mean + 1.0);

    Ok(directional && alternating_ratio >= params.min_alternating_row_ratio)
}

/// 沿 `(dx, dy)` 方向的平均梯度能量：返回（能量和，样本数）。
fn direction_energy(image: &GrayImage, dx: u32, dy: i32) -> Result<(f32, u32), VisionError> {
    let mut energy = 0.0f32;
    let mut samples = 0u32;
    for y in 0..image.h as i32 {
        let ny = y + dy;
        if ny < 0 || ny >= image.h as i32 {
            continue;
        }
        let current = image.row(y as u32);
        let neighbor = image.row(ny as u32);
        for x in 0..(image.w.saturating_sub(dx)) {
            let a = current[x as usize];
            let b = neighbor[(x + dx) as usize];
            energy += f32::from(a.abs_diff(b));
            samples += 1;
        }
    }
    Ok((energy, samples))
}

/// 存在明显明暗交替的行占比。
fn alternating_row_ratio(image: &GrayImage, params: &DiagonalStripeParams) -> f32 {
    let mut alternating_rows = 0u32;
    for y in 0..image.h {
        let row = image.row(y);
        let mut transitions = 0u32;
        for pair in row.windows(2) {
            if pair[0].abs_diff(pair[1]) >= params.min_contrast {
                transitions += 1;
            }
        }
        if transitions >= params.min_transitions_per_row {
            alternating_rows += 1;
        }
    }
    alternating_rows as f32 / image.h as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成图：亮度只由 `u = x - y` 决定（45° 斜纹）。
    fn diagonal_bands(w: u32, h: u32, period: i32) -> GrayImage {
        let mut pixels = Vec::with_capacity((w * h) as usize);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let band = (x - y).div_euclid(period).rem_euclid(2);
                pixels.push(if band == 0 { 240 } else { 20 });
            }
        }
        GrayImage::new(w, h, pixels).expect("合法图像")
    }

    /// 合成图：亮度只由 `y` 决定（横纹）。
    fn horizontal_bands(w: u32, h: u32, period: i32) -> GrayImage {
        let mut pixels = Vec::with_capacity((w * h) as usize);
        for y in 0..h as i32 {
            let band = y.div_euclid(period).rem_euclid(2);
            let value = if band == 0 { 240 } else { 20 };
            for _ in 0..w {
                pixels.push(value);
            }
        }
        GrayImage::new(w, h, pixels).expect("合法图像")
    }

    /// 合成图：亮度只由 `x` 决定（竖纹）。
    fn vertical_bands(w: u32, h: u32, period: i32) -> GrayImage {
        let mut pixels = Vec::with_capacity((w * h) as usize);
        for _ in 0..h as i32 {
            for x in 0..w as i32 {
                let band = x.div_euclid(period).rem_euclid(2);
                pixels.push(if band == 0 { 240 } else { 20 });
            }
        }
        GrayImage::new(w, h, pixels).expect("合法图像")
    }

    #[test]
    fn detects_synthetic_diagonal_mask() {
        let image = diagonal_bands(64, 64, 8);
        assert!(detect_diagonal_stripe(&image, &DiagonalStripeParams::default()).expect("尺寸合法"));
    }

    #[test]
    fn rejects_flat_panel() {
        let image = GrayImage::filled(64, 64, 128).expect("合法图像");
        assert!(!detect_diagonal_stripe(&image, &DiagonalStripeParams::default()).expect("尺寸合法"));
    }

    #[test]
    fn rejects_horizontal_and_vertical_bands() {
        let params = DiagonalStripeParams::default();
        assert!(
            !detect_diagonal_stripe(&horizontal_bands(64, 64, 8), &params).expect("尺寸合法"),
            "横纹不得被判为斜纹遮蔽层"
        );
        assert!(
            !detect_diagonal_stripe(&vertical_bands(64, 64, 8), &params).expect("尺寸合法"),
            "竖纹不得被判为斜纹遮蔽层"
        );
    }

    #[test]
    fn thresholds_are_configurable_and_effective() {
        let image = diagonal_bands(64, 64, 8);
        let strict = DiagonalStripeParams {
            min_anti_energy: 250.0,
            ..DiagonalStripeParams::default()
        };
        assert!(!detect_diagonal_stripe(&image, &strict).expect("尺寸合法"));

        let lax = DiagonalStripeParams {
            min_alternating_row_ratio: 0.0,
            ..DiagonalStripeParams::default()
        };
        assert!(detect_diagonal_stripe(&image, &lax).expect("尺寸合法"));
    }

    #[test]
    fn tiny_or_invalid_input_errors() {
        let tiny = GrayImage::filled(2, 2, 0).expect("合法图像");
        assert!(matches!(
            detect_diagonal_stripe(&tiny, &DiagonalStripeParams::default()),
            Err(VisionError::InvalidDimensions { .. })
        ));

        let bad_params = DiagonalStripeParams {
            min_direction_ratio: 0.0,
            ..DiagonalStripeParams::default()
        };
        assert!(detect_diagonal_stripe(&diagonal_bands(16, 16, 4), &bad_params).is_err());
    }

    // ---------- 黄色门（第一阶段）回归 ----------

    const HATCH_YELLOW: [u8; 3] = [250, 220, 60];
    const HATCH_BLACK: [u8; 3] = [20, 20, 20];
    const PANEL_DARK: [u8; 3] = [40, 60, 70];

    /// 合成 BGRA 画布（RGB 闭包 → BGRA 缓冲）。
    fn bgra_canvas(w: u32, h: u32, paint: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = paint(x, y);
                data.extend_from_slice(&[b, g, r, 255]);
            }
        }
        data
    }

    /// spec §7.3 的真实候选尺寸（帧内实测约 250×70）。
    const HATCH_W: u32 = 250;
    const HATCH_H: u32 = 70;

    fn is_border(x: u32, y: u32, w: u32, h: u32, thickness: u32) -> bool {
        x < thickness || y < thickness || x >= w - thickness || y >= h - thickness
    }

    /// 亮黄 3px 边框 + 内部 45° 黄黑斜纹（spec §7.3 的完整形态）。
    fn hatched_overlay(w: u32, h: u32, period: i32) -> Vec<u8> {
        bgra_canvas(w, h, |x, y| {
            if is_border(x, y, w, h, 3) {
                return HATCH_YELLOW;
            }
            let band = (x as i32 - y as i32).div_euclid(period).rem_euclid(2);
            if band == 0 {
                HATCH_YELLOW
            } else {
                HATCH_BLACK
            }
        })
    }

    #[test]
    fn detects_yellow_bordered_diagonal_hatch_at_spec_size() {
        let data = hatched_overlay(HATCH_W, HATCH_H, 8);
        let detected = detect_yellow_hatch(
            &data,
            HATCH_W,
            HATCH_H,
            HATCH_W * 4,
            &DiagonalStripeParams::default(),
        )
        .expect("尺寸合法");
        assert!(detected, "黄框 + 45° 黄黑斜纹必须判为浮层（spec §7.3 的 250×70 实尺）");
    }

    #[test]
    fn rejects_high_contrast_diagonal_without_yellow_box() {
        // 无任何黄色：面板里的黑白高对比斜纹（对话框 / 网格）绝不能被判成浮层。
        let data = bgra_canvas(HATCH_W, HATCH_H, |x, y| {
            let band = (x as i32 - y as i32).div_euclid(8).rem_euclid(2);
            if band == 0 {
                [245, 245, 245]
            } else {
                [10, 10, 10]
            }
        });
        let detected = detect_yellow_hatch(
            &data,
            HATCH_W,
            HATCH_H,
            HATCH_W * 4,
            &DiagonalStripeParams::default(),
        )
        .expect("尺寸合法");
        assert!(!detected, "没有黄色边框时，45° 黑白斜纹必须返回 false（防提前跳列）");
    }

    #[test]
    fn rejects_yellow_border_with_horizontal_bands() {
        let data = bgra_canvas(HATCH_W, HATCH_H, |x, y| {
            if is_border(x, y, HATCH_W, HATCH_H, 3) {
                return HATCH_YELLOW;
            }
            let band = (y as i32).div_euclid(8).rem_euclid(2);
            if band == 0 {
                HATCH_YELLOW
            } else {
                HATCH_BLACK
            }
        });
        let detected = detect_yellow_hatch(
            &data,
            HATCH_W,
            HATCH_H,
            HATCH_W * 4,
            &DiagonalStripeParams::default(),
        )
        .expect("尺寸合法");
        assert!(!detected, "有黄框但内部是横纹：第二阶段方向性必须否决");
    }

    #[test]
    fn rejects_yellow_border_with_flat_interior() {
        let data = bgra_canvas(HATCH_W, HATCH_H, |x, y| {
            if is_border(x, y, HATCH_W, HATCH_H, 3) {
                HATCH_YELLOW
            } else {
                [90, 90, 90]
            }
        });
        assert!(!detect_yellow_hatch(
            &data,
            HATCH_W,
            HATCH_H,
            HATCH_W * 4,
            &DiagonalStripeParams::default()
        )
        .expect("尺寸合法"));
    }

    #[test]
    fn rejects_knob_sized_yellow_blob() {
        // 旋钮块（20×20 米黄）落在黄色色域内，但候选包围盒过小 → 不得成为浮层。
        let data = bgra_canvas(HATCH_W, HATCH_H, |x, y| {
            let in_knob = (100..120).contains(&x) && (25..45).contains(&y);
            if in_knob {
                HATCH_YELLOW
            } else {
                PANEL_DARK
            }
        });
        assert!(!detect_yellow_hatch(
            &data,
            HATCH_W,
            HATCH_H,
            HATCH_W * 4,
            &DiagonalStripeParams::default()
        )
        .expect("尺寸合法"));
    }

    #[test]
    fn yellow_hatch_rejects_bad_params_and_short_buffer() {
        let data = hatched_overlay(64, 64, 8);
        let bad_params = DiagonalStripeParams {
            box_hue_max: 10.0,
            ..DiagonalStripeParams::default()
        };
        assert!(detect_yellow_hatch(&data, 64, 64, 256, &bad_params).is_err());
        assert!(matches!(
            detect_yellow_hatch(&[0u8; 16], 64, 64, 256, &DiagonalStripeParams::default()),
            Err(VisionError::BufferSizeMismatch { .. })
        ));
    }
}
