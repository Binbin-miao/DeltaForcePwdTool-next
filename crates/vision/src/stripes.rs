//! 黄黑斜纹遮蔽层检测（spec D7：操作台「完成」判据）。
//!
//! 判据由两个互相独立的统计量组成，任何一个不满足都返回 `false`：
//! 1. **方向性**：45° 斜纹是 `I = f(x - y)`，其沿 `(1,-1)` 方向的梯度能量显著大于沿 `(1,1)` 方向；
//!    横纹/竖纹在两个方向上的能量相当，比值过不了门限。
//! 2. **交替性**：足够比例的行上存在多次高对比度明暗翻转（黄黑交替）。
//!
//! 阈值全部由 [`DiagonalStripeParams`] 暴露，默认值由本文件的合成图测试锁定。

use crate::{GrayImage, VisionError};

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
}

impl Default for DiagonalStripeParams {
    fn default() -> Self {
        Self {
            min_anti_energy: 20.0,
            min_direction_ratio: 1.5,
            min_contrast: 40,
            min_alternating_row_ratio: 0.5,
            min_transitions_per_row: 2,
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
        Ok(())
    }
}

/// 检测面板区域内是否存在 45° 黄黑斜纹遮蔽层。
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
}
