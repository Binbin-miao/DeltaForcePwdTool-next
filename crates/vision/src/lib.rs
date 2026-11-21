//! 纯图像算法底座（Deep Module）。
//!
//! 规格条款：
//! - **spec §2.1 `core/fingerprint.py:86-96`**：手写 NCC 严格等价 `TM_CCOEFF_NORMED`；
//! - **spec §2.1 `core/morse.py:24-55`**：Otsu → 外轮廓 → 点划判据（`aspect > 2.0`）；
//! - **spec §2.2 不变量 7**：噪点门 `w*h < 10`、点划阈值 `aspect > 2.0`（严格大于）。
//!
//! 本 crate 只做纯函数，不碰截图、输入与业务，因此可以被任意线程并行调用。

use thiserror::Error;

mod binary;
mod contour;
mod led;
mod ncc;
mod stripes;

pub use binary::{BinaryImage, OtsuResult};
pub use contour::{external_contours, Contour};
pub use led::{
    read_glyph_string, LED_DASH_HEIGHT_RATIO, LED_SHAPE_ACCEPT, LED_SHAPE_DECISIVE,
    LED_TEMPLATE_MIN_SCORE, LED_ZERO_ASPECT,
};
pub use ncc::{match_template_ncc, NccMatch};
pub use stripes::{detect_diagonal_stripe, detect_yellow_hatch, DiagonalStripeParams};

/// spec §2.2 不变量 7：噪点门（`w*h < 10` 的连通域直接丢弃）。
pub const NOISE_AREA_GATE: u32 = 10;
/// spec §2.2 不变量 7：点划阈值，`aspect > 2.0` 判划，`= 2.0` 判点。
pub const DASH_ASPECT_THRESHOLD: f32 = 2.0;

/// 图像算法错误。所有退化输入都显式报错或显式返回确定值，绝不产生 NaN。
#[derive(Debug, Clone, PartialEq, Error)]
pub enum VisionError {
    #[error("图像尺寸非法：{w}x{h}")]
    InvalidDimensions { w: u32, h: u32 },
    #[error("{w}x{h} 图像需要 {expected} 字节，实际 {actual} 字节")]
    BufferSizeMismatch {
        w: u32,
        h: u32,
        expected: usize,
        actual: usize,
    },
    #[error("裁剪区域 ({x},{y},{w},{h}) 越出图像 {image_w}x{image_h}")]
    RegionOutOfBounds {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        image_w: u32,
        image_h: u32,
    },
    #[error("模板 {tw}x{th} 大于图像 {iw}x{ih}，无法滑动匹配")]
    TemplateLargerThanImage { tw: u32, th: u32, iw: u32, ih: u32 },
    #[error("区域内未检测到任何字符")]
    NoGlyph,
    #[error("算法参数非法：{name}")]
    InvalidParams { name: &'static str },
    #[error(
        "第 {index} 个 LED 字符双判据都不达标：模板 NCC {template_score:.3} / 结构签名 {shape_score:.3}"
    )]
    LedAmbiguous {
        index: usize,
        template_score: f32,
        shape_score: f32,
    },
    #[error(
        "LED 带内没有满高字形（最高 {tallest}px < 带高 {band_height}px 的 {min_ratio:.0}%），拒绝出字"
    )]
    LedNoAnchor {
        tallest: u32,
        band_height: u32,
        min_ratio: f32,
    },
    #[error("内嵌 LED 字形模板不可用（构建期资源损坏）")]
    LedTemplateUnavailable,
    #[error("PNG 字节不可解码（损坏、或位深/色彩类型不受支持）")]
    PngDecode,
}

/// 图像内的整数区域（左上原点，单位像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    /// 区域面积（用于 spec §2.2 不变量 7 的噪点门）。
    pub fn area(self) -> u32 {
        self.w.saturating_mul(self.h)
    }

    /// 宽高比；高为 0 时返回 `f32::INFINITY`（调用方先过噪点门）。
    pub fn aspect(self) -> f32 {
        if self.h == 0 {
            return f32::INFINITY;
        }
        self.w as f32 / self.h as f32
    }
}

/// 8 位灰度图（单通道，行主序、无行填充）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrayImage {
    pub w: u32,
    pub h: u32,
    data: Vec<u8>,
}

impl GrayImage {
    /// 从已有的灰度缓冲构造。
    pub fn new(w: u32, h: u32, data: Vec<u8>) -> Result<Self, VisionError> {
        if w == 0 || h == 0 {
            return Err(VisionError::InvalidDimensions { w, h });
        }
        let expected = (w as usize) * (h as usize);
        if data.len() != expected {
            return Err(VisionError::BufferSizeMismatch {
                w,
                h,
                expected,
                actual: data.len(),
            });
        }
        Ok(Self { w, h, data })
    }

    /// 填充单一灰度的图（测试与占位用）。
    pub fn filled(w: u32, h: u32, value: u8) -> Result<Self, VisionError> {
        Self::new(w, h, vec![value; (w as usize) * (h as usize)])
    }

    /// 32bpp BGRA（`BitBlt` + `GetDIBits` 的输出，行主序 top-down）→ 灰度。
    ///
    /// 这就是 t3 任务 1 的 `from_frame`：由 `delta_capture::Frame::luma` 调用，
    /// 因此本 crate 不需要反向依赖 capture。BT.601 整数近似 `(77r + 150g + 29b) >> 8`。
    pub fn from_bgra(w: u32, h: u32, stride: u32, data: &[u8]) -> Result<Self, VisionError> {
        if w == 0 || h == 0 {
            return Err(VisionError::InvalidDimensions { w, h });
        }
        let row_bytes = (w as usize) * 4;
        let stride = stride as usize;
        if stride < row_bytes || data.len() < stride.saturating_mul(h as usize) {
            return Err(VisionError::BufferSizeMismatch {
                w,
                h,
                expected: stride.saturating_mul(h as usize).max(row_bytes),
                actual: data.len(),
            });
        }
        let mut out = Vec::with_capacity((w as usize) * (h as usize));
        for row in data.chunks_exact(stride).take(h as usize) {
            let (pixels, _remainder) = row[..row_bytes].as_chunks::<4>();
            for pixel in pixels {
                out.push(luma_from_bgra(pixel[0], pixel[1], pixel[2]));
            }
        }
        Self::new(w, h, out)
    }

    /// PNG 字节 → 灰度（Grayscale / 灰+alpha / RGB / RGBA / 索引色一律归一化到 8 位单通道）。
    ///
    /// 与 [`Self::from_bgra`] 同一套 BT.601 灰度口径，因此「落盘的灰度 PNG」再解码回来
    /// 与运行时从 BGRA 算出的灰度逐像素一致 —— 真实画面才能当回归夹具用。
    /// 16 位与调色板 PNG 由 `png` 的解码期变换统一到 8 位 RGB，调用方不必关心原始格式。
    pub fn from_png(bytes: &[u8]) -> Result<Self, VisionError> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().map_err(|_| VisionError::PngDecode)?;
        let mut buffer = vec![0u8; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buffer)
            .map_err(|_| VisionError::PngDecode)?;
        let (w, h) = (info.width, info.height);
        let channels = match info.color_type {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Indexed => return Err(VisionError::PngDecode),
        };
        let pixels: Vec<u8> = buffer[..(w as usize) * (h as usize) * channels]
            .chunks_exact(channels)
            .map(|pixel| match channels {
                1 | 2 => pixel[0],
                _ => luma_from_bgra(pixel[2], pixel[1], pixel[0]),
            })
            .collect();
        Self::new(w, h, pixels)
    }

    /// 只读像素缓冲（行主序）。
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    /// 取一行的灰度切片。
    pub fn row(&self, y: u32) -> &[u8] {
        let w = self.w as usize;
        let start = (y as usize) * w;
        &self.data[start..start + w]
    }

    /// 取一个像素。越界即 panic（内部调用方都已先做边界校验）。
    pub fn pixel(&self, x: u32, y: u32) -> u8 {
        self.data[(y as usize) * (self.w as usize) + (x as usize)]
    }

    /// 裁剪；越界返回 [`VisionError::RegionOutOfBounds`]，不静默裁剪。
    pub fn crop(&self, region: Region) -> Result<Self, VisionError> {
        if region.w == 0
            || region.h == 0
            || region.x.saturating_add(region.w) > self.w
            || region.y.saturating_add(region.h) > self.h
        {
            return Err(VisionError::RegionOutOfBounds {
                x: region.x,
                y: region.y,
                w: region.w,
                h: region.h,
                image_w: self.w,
                image_h: self.h,
            });
        }
        let mut data = Vec::with_capacity(region.area() as usize);
        for y in region.y..region.y + region.h {
            let row = self.row(y);
            data.extend_from_slice(&row[region.x as usize..(region.x + region.w) as usize]);
        }
        Self::new(region.w, region.h, data)
    }

    /// 双线性缩放（**非等比**：宽高各自独立，保留上游「非等比 resize」语义，spec §2.1）。
    pub fn resize_to(&self, w: u32, h: u32) -> Result<Self, VisionError> {
        if w == 0 || h == 0 {
            return Err(VisionError::InvalidDimensions { w, h });
        }
        if w == self.w && h == self.h {
            return Ok(self.clone());
        }
        let sw = self.w as f32;
        let sh = self.h as f32;
        let scale_x = sw / w as f32;
        let scale_y = sh / h as f32;
        let mut data = Vec::with_capacity((w as usize) * (h as usize));
        for dy in 0..h {
            let sy = ((dy as f32 + 0.5) * scale_y - 0.5).clamp(0.0, sh - 1.0);
            let y0 = sy.floor();
            let y1 = (y0 + 1.0).min(sh - 1.0);
            let fy = sy - y0;
            let row0 = self.row(y0 as u32);
            let row1 = self.row(y1 as u32);
            for dx in 0..w {
                let sx = ((dx as f32 + 0.5) * scale_x - 0.5).clamp(0.0, sw - 1.0);
                let x0 = sx.floor();
                let x1 = (x0 + 1.0).min(sw - 1.0);
                let fx = sx - x0;
                let (x0, x1) = (x0 as usize, x1 as usize);
                let top = f32::from(row0[x0]) * (1.0 - fx) + f32::from(row0[x1]) * fx;
                let bottom = f32::from(row1[x0]) * (1.0 - fx) + f32::from(row1[x1]) * fx;
                let value = top * (1.0 - fy) + bottom * fy;
                data.push(value.round().clamp(0.0, 255.0) as u8);
            }
        }
        Self::new(w, h, data)
    }

    /// [`GrayImage::resize_to`] 的别名，保留 t3 任务 1 的命名。
    pub fn resize_bilinear(&self, w: u32, h: u32) -> Result<Self, VisionError> {
        self.resize_to(w, h)
    }
}

/// BT.601 整数近似：`(77r + 150g + 29b) >> 8`（避免浮点）。
fn luma_from_bgra(b: u8, g: u8, r: u8) -> u8 {
    let weighted = 77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(b);
    (weighted >> 8) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_bgra_honours_channel_order_and_stride() {
        // 2x2：蓝 / 绿 / 红 / 白（BGRA 顺序，验证不是把 B 当 R）。
        let mut data = Vec::new();
        for (b, g, r) in [(255u8, 0u8, 0u8), (0, 255, 0), (0, 0, 255), (255, 255, 255)] {
            data.extend_from_slice(&[b, g, r, 255]);
        }
        let gray = GrayImage::from_bgra(2, 2, 8, &data).expect("合法 BGRA");
        assert_eq!(gray.as_slice(), &[28, 149, 76, 255]);

        // 行填充（stride > w*4）必须被跳过。
        let mut padded = Vec::new();
        padded.extend_from_slice(&[255, 0, 0, 255, 255, 0, 0, 255, 9, 9, 9, 9]);
        padded.extend_from_slice(&[255, 0, 0, 255, 255, 0, 0, 255, 9, 9, 9, 9]);
        let padded_gray = GrayImage::from_bgra(2, 2, 12, &padded).expect("带填充的 BGRA");
        assert_eq!(padded_gray.as_slice(), &[28, 28, 28, 28]);
    }

    #[test]
    fn from_bgra_rejects_short_buffer() {
        let error = GrayImage::from_bgra(4, 4, 16, &[0u8; 8]).expect_err("缓冲不足必须报错");
        assert!(matches!(error, VisionError::BufferSizeMismatch { .. }));
    }

    #[test]
    fn invalid_dimensions_are_rejected() {
        assert!(matches!(
            GrayImage::new(0, 4, Vec::new()),
            Err(VisionError::InvalidDimensions { w: 0, h: 4 })
        ));
        assert!(matches!(
            GrayImage::new(2, 2, vec![0; 3]),
            Err(VisionError::BufferSizeMismatch { .. })
        ));
    }

    #[test]
    fn crop_extracts_subimage_and_rejects_out_of_bounds() {
        let image = GrayImage::new(3, 2, vec![1, 2, 3, 4, 5, 6]).expect("合法图像");
        let cropped = image
            .crop(Region {
                x: 1,
                y: 1,
                w: 2,
                h: 1,
            })
            .expect("区域内裁剪");
        assert_eq!(cropped.as_slice(), &[5, 6]);

        let error = image
            .crop(Region {
                x: 2,
                y: 0,
                w: 2,
                h: 1,
            })
            .expect_err("越界裁剪必须报错");
        assert!(matches!(error, VisionError::RegionOutOfBounds { .. }));
    }

    #[test]
    fn resize_preserves_constant_image_and_dimensions() {
        let image = GrayImage::filled(4, 4, 77).expect("合法图像");
        let resized = image.resize_to(9, 3).expect("非等比缩放");
        assert_eq!((resized.w, resized.h), (9, 3));
        assert!(resized.as_slice().iter().all(|&v| v == 77));
        assert_eq!(resized, resized.resize_bilinear(9, 3).expect("别名一致"));
        assert_eq!(image.resize_to(4, 4).expect("同尺寸"), image);
    }

    #[test]
    fn resize_keeps_gradient_monotonic() {
        let image = GrayImage::new(2, 1, vec![0, 200]).expect("合法图像");
        let resized = image.resize_to(4, 1).expect("放大");
        let values = resized.as_slice();
        assert!(values.windows(2).all(|pair| pair[0] <= pair[1]), "{values:?}");
        assert!(values[0] <= 40 && values[3] >= 160, "{values:?}");
    }

    #[test]
    fn region_metrics_match_spec_invariants() {
        let dash = Region {
            x: 0,
            y: 0,
            w: 30,
            h: 10,
        };
        assert_eq!(dash.area(), 300);
        assert!(dash.aspect() > DASH_ASPECT_THRESHOLD);
        let dot = Region {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        };
        assert!(
            dot.aspect() <= DASH_ASPECT_THRESHOLD,
            "aspect == 2.0 必须判点（严格大于才判划）"
        );
        assert_eq!(dot.aspect(), DASH_ASPECT_THRESHOLD);
    }
}
