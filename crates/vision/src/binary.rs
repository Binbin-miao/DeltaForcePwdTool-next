//! Otsu 二值化 + 位打包二值图（spec §2.1 `core/morse.py:24-55` 的第一步）。

use crate::{GrayImage, VisionError};

/// Otsu 的结果：阈值 + 二值图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtsuResult {
    pub threshold: u8,
    pub binary: BinaryImage,
}

/// 位打包二值图：每像素 1 bit（`Vec<u64>`），省掉上位图 8 倍内存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryImage {
    w: u32,
    h: u32,
    bits: Vec<u64>,
    foreground: u32,
}

impl BinaryImage {
    /// 全背景构造。
    pub fn new(w: u32, h: u32) -> Result<Self, VisionError> {
        if w == 0 || h == 0 {
            return Err(VisionError::InvalidDimensions { w, h });
        }
        Ok(Self::blank(w, h))
    }

    /// 内部构造：尺寸由调用方保证为正（例如来自已校验的 `GrayImage`）。
    fn blank(w: u32, h: u32) -> Self {
        let words = (w as usize * h as usize).div_ceil(64);
        Self {
            w,
            h,
            bits: vec![0; words],
            foreground: 0,
        }
    }

    /// 宽。
    pub fn width(&self) -> u32 {
        self.w
    }

    /// 高。
    pub fn height(&self) -> u32 {
        self.h
    }

    /// 前景像素数量。
    pub fn foreground_count(&self) -> u32 {
        self.foreground
    }

    /// 读取像素；越界视为背景（轮廓扫描会主动越界查询，见 `contour` 模块）。
    pub fn get(&self, x: u32, y: u32) -> bool {
        if x >= self.w || y >= self.h {
            return false;
        }
        let index = (y as usize) * (self.w as usize) + (x as usize);
        (self.bits[index / 64] >> (index % 64)) & 1 == 1
    }

    /// 写入像素；越界静默忽略（仅供构造期使用）。
    pub fn set(&mut self, x: u32, y: u32, value: bool) {
        if x >= self.w || y >= self.h {
            return;
        }
        let index = (y as usize) * (self.w as usize) + (x as usize);
        let mask = 1u64 << (index % 64);
        let was_set = (self.bits[index / 64] & mask) != 0;
        match (value, was_set) {
            (true, false) => {
                self.bits[index / 64] |= mask;
                self.foreground += 1;
            }
            (false, true) => {
                self.bits[index / 64] &= !mask;
                self.foreground -= 1;
            }
            _ => {}
        }
    }

    /// 按给定阈值二值化（`> threshold` 为前景）。
    pub fn from_gray_threshold(gray: &GrayImage, threshold: u8) -> Self {
        let mut binary = Self::blank(gray.w, gray.h);
        for y in 0..gray.h {
            let row = gray.row(y);
            for (x, &value) in row.iter().enumerate() {
                if value > threshold {
                    binary.set(x as u32, y, true);
                }
            }
        }
        binary
    }
}

impl GrayImage {
    /// Otsu 阈值二值化：直方图 256 bin，最大化类间方差。
    ///
    /// 退化输入约定（不产生 NaN）：
    /// - 全图同灰度 → `threshold = 该灰度`，二值图为全背景（没有任何前景）；
    /// - 只有两种灰度 → 取低灰度值作为阈值。
    pub fn otsu(&self) -> OtsuResult {
        let data = self.as_slice();
        let mut histogram = [0u32; 256];
        for &value in data {
            histogram[value as usize] += 1;
        }
        let threshold = otsu_threshold(&histogram, data.len() as f64);
        OtsuResult {
            threshold,
            binary: BinaryImage::from_gray_threshold(self, threshold),
        }
    }
}

/// 类间方差最大化，返回 `(0..=255)` 的阈值。
fn otsu_threshold(histogram: &[u32; 256], total: f64) -> u8 {
    let sum_all: f64 = histogram
        .iter()
        .enumerate()
        .map(|(value, &count)| value as f64 * f64::from(count))
        .sum();
    let mut weight_back = 0.0;
    let mut sum_back = 0.0;
    let mut best_variance = -1.0;
    let mut threshold = 0u8;
    for (value, &count) in histogram.iter().enumerate() {
        weight_back += f64::from(count);
        if weight_back <= 0.0 {
            continue;
        }
        let weight_fore = total - weight_back;
        if weight_fore <= 0.0 {
            // 后续没有任何像素：只在「全图同灰度」这种没有类间方差的情况下取当前值，
            // 否则保留已经找到的最优阈值（不能把最佳阈值覆盖成最高灰度）。
            if best_variance < 0.0 {
                threshold = value as u8;
            }
            break;
        }
        sum_back += value as f64 * f64::from(count);
        let mean_back = sum_back / weight_back;
        let mean_fore = (sum_all - sum_back) / weight_fore;
        let variance = weight_back * weight_fore * (mean_back - mean_fore) * (mean_back - mean_fore);
        if variance > best_variance {
            best_variance = variance;
            threshold = value as u8;
        }
    }
    threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otsu_splits_bimodal_image() {
        // 左半 20、右半 220：Otsu 阈值应落在两者之间。
        let mut data = Vec::new();
        for _ in 0..10 {
            data.extend(std::iter::repeat_n(20, 10));
            data.extend(std::iter::repeat_n(220, 10));
        }
        let image = GrayImage::new(20, 10, data).expect("合法图像");
        let OtsuResult { threshold, binary } = image.otsu();
        assert!((20..220).contains(&threshold), "阈值 {threshold} 未落在两峰之间");
        assert_eq!(binary.foreground_count(), 100);
        assert!(!binary.get(0, 0));
        assert!(binary.get(19, 0));
    }

    #[test]
    fn otsu_on_uniform_image_yields_no_foreground() {
        let image = GrayImage::filled(8, 8, 100).expect("合法图像");
        let OtsuResult { threshold, binary } = image.otsu();
        assert_eq!(threshold, 100);
        assert_eq!(binary.foreground_count(), 0);
    }

    #[test]
    fn otsu_on_two_levels_uses_lower_level() {
        let mut data = vec![0u8; 64];
        data[0] = 255;
        let image = GrayImage::new(8, 8, data).expect("合法图像");
        let OtsuResult { threshold, binary } = image.otsu();
        assert_eq!(threshold, 0);
        assert_eq!(binary.foreground_count(), 1);
    }

    #[test]
    fn bit_packing_roundtrips_every_pixel() {
        let mut binary = BinaryImage::new(9, 3).expect("合法尺寸");
        assert_eq!(binary.width(), 9);
        assert_eq!(binary.height(), 3);
        for x in 0..9 {
            binary.set(x, 1, x % 3 == 0);
        }
        assert_eq!(binary.foreground_count(), 3);
        for x in 0..9 {
            assert_eq!(binary.get(x, 1), x % 3 == 0, "x={x}");
        }
        assert!(!binary.get(0, 0));
        assert!(!binary.get(100, 0), "越界读取视为背景");
        binary.set(0, 1, false);
        assert_eq!(binary.foreground_count(), 2);
    }

    #[test]
    fn zero_size_binary_image_is_rejected() {
        assert!(matches!(
            BinaryImage::new(0, 5),
            Err(VisionError::InvalidDimensions { .. })
        ));
    }
}
