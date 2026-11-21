//! 手写归一化互相关（NCC），严格等价 OpenCV `TM_CCOEFF_NORMED`。
//!
//! 公式（spec §2.1 `core/fingerprint.py:86-96`）：
//!
//! ```text
//! score = Σ(I - Ī)(T - T̄) / sqrt(Σ(I - Ī)² · Σ(T - T̄)²)
//! ```
//!
//! 性能手段：模板侧 `T' = T - T̄` 与 `ΣT'²` 预计算；窗口侧 `ΣI`、`ΣI²` 用积分图 O(1) 取得，
//! 因此每个窗口只需一遍内层点积。因为 `ΣT' = 0`，交叉项可以直接写成 `Σ I·T'`，无需再减 Ī。
//!
//! 精度取舍：积分图用 f64（像素平方和可达 1e10 量级，f32 尾数不够，会与 OpenCV 差出 1e-4）；
//! 内层点积按行在 f32 累加再并入 f64 —— 单行项数小，误差稳定在 1e-6 量级，满足「差异 < 1e-4」。

use crate::{GrayImage, VisionError};

/// 零方差判据：低于该值视为常量区域。
const VARIANCE_EPSILON: f64 = 1e-6;

/// 一次模板匹配的最优结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NccMatch {
    pub x: u32,
    pub y: u32,
    pub score: f32,
}

/// 在全图上滑动模板，返回得分最高的左上角坐标与得分。
///
/// 退化约定：
/// - 模板零方差（纯色）→ 返回 `(0, 0, 0.0)`，绝不产生 NaN；
/// - 窗口零方差（纯色）→ 该位置得分按 0.0 处理，不参与竞争；
/// - 全部窗口都退化为 0.0 时返回 `(0, 0, 0.0)`；
/// - 得分并列时保留扫描顺序中第一个（`y` 外层、`x` 内层）。
pub fn match_template_ncc(image: &GrayImage, tmpl: &GrayImage) -> Result<NccMatch, VisionError> {
    let (iw, ih) = (image.w as usize, image.h as usize);
    let (tw, th) = (tmpl.w as usize, tmpl.h as usize);
    if tw > iw || th > ih {
        return Err(VisionError::TemplateLargerThanImage {
            tw: tmpl.w,
            th: tmpl.h,
            iw: image.w,
            ih: image.h,
        });
    }

    let count = (tw * th) as f64;
    let template_mean = tmpl.as_slice().iter().map(|&v| f64::from(v)).sum::<f64>() / count;
    let template_zero_mean: Vec<f32> = tmpl
        .as_slice()
        .iter()
        .map(|&v| f32::from(v) - template_mean as f32)
        .collect();
    let template_variance: f64 = template_zero_mean
        .iter()
        .map(|&v| f64::from(v) * f64::from(v))
        .sum();
    if template_variance <= VARIANCE_EPSILON {
        return Ok(NccMatch {
            x: 0,
            y: 0,
            score: 0.0,
        });
    }

    let (sums, square_sums) = integral_images(image);
    let stride = iw + 1;
    let mut best: Option<NccMatch> = None;

    for y in 0..=(ih - th) {
        for x in 0..=(iw - tw) {
            let window_total = window_sum(&sums, stride, x, y, tw, th);
            let window_square_sum = window_sum(&square_sums, stride, x, y, tw, th);
            let window_variance = window_square_sum - window_total * window_total / count;
            if window_variance <= VARIANCE_EPSILON {
                continue;
            }
            let mut cross = 0.0f64;
            for j in 0..th {
                let row = image.row((y + j) as u32);
                let template_row = &template_zero_mean[j * tw..(j + 1) * tw];
                let mut row_dot = 0.0f32;
                for (value, &weight) in row[x..x + tw].iter().zip(template_row) {
                    row_dot += f32::from(*value) * weight;
                }
                cross += f64::from(row_dot);
            }
            let score = (cross / (window_variance * template_variance).sqrt()) as f32;
            let score = score.clamp(-1.0, 1.0);
            let better = best.is_none_or(|current| score > current.score);
            if better {
                best = Some(NccMatch {
                    x: x as u32,
                    y: y as u32,
                    score,
                });
            }
        }
    }

    Ok(best.unwrap_or(NccMatch {
        x: 0,
        y: 0,
        score: 0.0,
    }))
}

/// 前缀和 + 前缀平方和，尺寸 `(w+1) × (h+1)`，首行首列为 0。
fn integral_images(image: &GrayImage) -> (Vec<f64>, Vec<f64>) {
    let stride = image.w as usize + 1;
    let mut sums = vec![0.0f64; stride * (image.h as usize + 1)];
    let mut square_sums = vec![0.0f64; stride * (image.h as usize + 1)];
    for y in 0..image.h as usize {
        let row = image.row(y as u32);
        let mut row_sum = 0.0f64;
        let mut row_square_sum = 0.0f64;
        for (x, &value) in row.iter().enumerate() {
            let value = f64::from(value);
            row_sum += value;
            row_square_sum += value * value;
            sums[(y + 1) * stride + x + 1] = sums[y * stride + x + 1] + row_sum;
            square_sums[(y + 1) * stride + x + 1] = square_sums[y * stride + x + 1] + row_square_sum;
        }
    }
    (sums, square_sums)
}

/// 积分图 O(1) 窗口求和。
fn window_sum(integral: &[f64], stride: usize, x: usize, y: usize, w: usize, h: usize) -> f64 {
    let top_left = y * stride + x;
    let top_right = y * stride + x + w;
    let bottom_left = (y + h) * stride + x;
    let bottom_right = (y + h) * stride + x + w;
    integral[bottom_right] - integral[top_right] - integral[bottom_left] + integral[top_left]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_from(pixels: &[u8], w: u32, h: u32) -> GrayImage {
        GrayImage::new(w, h, pixels.to_vec()).expect("测试图像尺寸合法")
    }

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-5
    }

    #[test]
    fn matches_hand_computed_ccoeff_normed() {
        // 手算参考值（与 OpenCV TM_CCOEFF_NORMED 同公式）：
        // T = [[0,1],[0,1]] → T' = [-.5,.5,-.5,.5]，ΣT'² = 1
        // 窗口 (1,1) = [[0,1],[1,2]] → Ī = 1，Σ(I-Ī)² = 2
        // score = (0·-.5 + 1·.5 + 1·-.5 + 2·.5) / sqrt(2·1) = 1/sqrt2 = 0.7071068
        let image = image_from(
            &[0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 2, 0, 0, 0, 0, 0],
            4,
            4,
        );
        let tmpl = image_from(&[0, 1, 0, 1], 2, 2);
        let found = match_template_ncc(&image, &tmpl).expect("尺寸合法");
        assert_eq!((found.x, found.y), (1, 1), "最优位置错误：{found:?}");
        assert!(
            close(found.score, std::f32::consts::FRAC_1_SQRT_2),
            "得分 {} 与手算值 1/sqrt(2) 不符",
            found.score
        );
    }

    #[test]
    fn exact_occurrence_scores_one() {
        let mut pixels = vec![10u8; 64];
        let pattern = [200u8, 30, 90, 250, 5, 120, 180, 60, 33];
        for (index, value) in pattern.iter().enumerate() {
            let x = 2 + index % 3;
            let y = 3 + index / 3;
            pixels[y * 8 + x] = *value;
        }
        let image = image_from(&pixels, 8, 8);
        let tmpl = image
            .crop(crate::Region {
                x: 2,
                y: 3,
                w: 3,
                h: 3,
            })
            .expect("区域内裁剪");
        let found = match_template_ncc(&image, &tmpl).expect("尺寸合法");
        assert_eq!((found.x, found.y), (2, 3));
        assert!(close(found.score, 1.0), "完全一致必须得 1.0，实际 {}", found.score);
    }

    #[test]
    fn inverted_template_scores_minus_one() {
        let pattern = [0u8, 0, 0, 0, 0, 0, 255, 255, 255];
        let image = image_from(&pattern, 3, 3);
        let mut inverted = pattern;
        for value in &mut inverted {
            *value = 255 - *value;
        }
        let tmpl = image_from(&inverted, 3, 3);
        let found = match_template_ncc(&image, &tmpl).expect("尺寸合法");
        assert!(close(found.score, -1.0), "反相必须得 -1.0，实际 {}", found.score);
    }

    #[test]
    fn zero_variance_template_returns_zero_not_nan() {
        let image = image_from(&[0, 128, 255, 64, 32, 16, 8, 4, 2], 3, 3);
        let flat_template = image_from(&[7, 7, 7, 7], 2, 2);
        let found = match_template_ncc(&image, &flat_template).expect("尺寸合法");
        assert_eq!(
            found,
            NccMatch {
                x: 0,
                y: 0,
                score: 0.0
            }
        );
        assert!(found.score.is_finite());
    }

    #[test]
    fn zero_variance_window_is_skipped() {
        let image = image_from(&[50; 16], 4, 4);
        let tmpl = image_from(&[0, 255, 0, 255], 2, 2);
        let found = match_template_ncc(&image, &tmpl).expect("尺寸合法");
        assert_eq!(found.score, 0.0, "纯色窗口不得给出 NaN 或非零得分");
    }

    #[test]
    fn single_pixel_degenerate_inputs() {
        let image = image_from(&[42], 1, 1);
        let tmpl = image_from(&[42], 1, 1);
        let found = match_template_ncc(&image, &tmpl).expect("尺寸合法");
        assert_eq!(
            found,
            NccMatch {
                x: 0,
                y: 0,
                score: 0.0
            }
        );
    }

    #[test]
    fn template_larger_than_image_is_an_error() {
        let image = image_from(&[1, 2, 3, 4], 2, 2);
        let tmpl = image_from(&[1, 2, 3, 4, 5, 6, 7, 8, 9], 3, 3);
        let error = match_template_ncc(&image, &tmpl).expect_err("模板大于图像必须报错");
        assert!(
            matches!(error, VisionError::TemplateLargerThanImage { .. }),
            "{error}"
        );
    }

    #[test]
    #[ignore = "基准：cargo test -p delta-vision --release -- --ignored bench_ncc"]
    fn bench_ncc_105x105() {
        let mut pixels = vec![0u8; 105 * 105];
        for (index, value) in pixels.iter_mut().enumerate() {
            *value = ((index * 37) % 251) as u8;
        }
        let image = image_from(&pixels, 105, 105);
        let tmpl = image.clone();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            let _ = match_template_ncc(&image, &tmpl).expect("尺寸合法");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(40),
            "1000 次 105x105 匹配耗时 {elapsed:?}，超出 40ms 预算"
        );
    }
}
