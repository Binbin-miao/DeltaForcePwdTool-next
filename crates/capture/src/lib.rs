//! 截图后端（spec `docs/spec/spec-capture-backend.md`）：默认 WGC，BitBlt 为兼容档。
//!
//! 设计要点：
//! - Per-Monitor V2 DPI 感知在**任何** `GetDC` 之前完成（见 [`ensure_dpi_awareness`]）；
//! - **禁止常驻图形资源**：BitBlt 每次 `grab` 新建并释放 GDI 句柄，WGC 按任务建
//!   [`CaptureSession`]、任务结束 `Drop` 释放；
//! - 对外只暴露**同步单次** `grab(rect)`，不存在任何流式/回调/定时截图接口（spec D4）；
//! - 单次识别任务截图硬上限 [`FRAME_BUDGET`] 张，超限返回 [`CaptureError::BudgetExceeded`]；
//! - 失败一律留证：Win32 错误码与 WGC 的 HRESULT 都原样保留十六进制。

use std::time::Instant;

use delta_geom::{GeomError, MonitorInfo, PxRect};
use delta_vision::GrayImage;
use thiserror::Error;

mod dpi;
mod gdi;
mod screen_source;
mod session;
mod wgc;

pub use dpi::{ensure_dpi_awareness, DpiAwareness};
pub use gdi::{primary_monitor_size, Capturer};
pub use screen_source::{LiveScreen, ScreenSource};
pub use session::{CaptureBackend, CaptureSession, FRAME_BUDGET};

/// 黑帧判据：luma 低于该值视为「黑像素」（t2 任务 5）。
pub const BLACK_LUMA_CUTOFF: u8 = 8;
/// 黑帧判据：黑像素占比门限（默认 99.5%，t2 任务 5）。
pub const DEFAULT_BLACK_THRESHOLD_PCT: f32 = 99.5;
/// 32bpp BGRA：每像素 4 字节。
pub const BYTES_PER_PIXEL: u32 = 4;

/// 捕获错误。所有分支都保留可定位的信息，不允许静默降级。
#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("{api} 失败，Win32 错误码 {code}")]
    Win32 { api: &'static str, code: u32 },
    #[error(
        "捕获到全黑画面（{black_pct:.1}% 像素 luma < {cutoff}），通常是游戏处于独占全屏。请把游戏切换为无边框窗口模式后重试"
    )]
    BlackFrame { black_pct: f32, cutoff: u8 },
    #[error("捕获区域尺寸非法：{w}x{h}")]
    InvalidRect { w: i32, h: i32 },
    #[error("屏幕 DC 获取失败，Win32 错误码 {code}")]
    NoScreenDc { code: u32 },
    #[error("捕获区域与显示器不一致")]
    Geom(#[from] GeomError),
    #[error("本次识别任务的截图配额已用尽（上限 {limit} 张），拒绝继续抓屏")]
    BudgetExceeded { limit: u32 },
    #[error(
        "WGC 初始化失败：{step} 返回 HRESULT 0x{code:08X}（本机可能没有可用的 D3D11 适配器）"
    )]
    WgcInit { step: &'static str, code: i32 },
    #[error("WGC 抓帧失败：{step} 返回 HRESULT 0x{code:08X}")]
    WgcGrab { step: &'static str, code: i32 },
    #[error("WGC 连续 {attempts} 次没有取到新帧（共等待 {wait_ms}ms）")]
    WgcNoFrame { attempts: u32, wait_ms: u64 },
    #[error("WGC 后端不支持当前环境：{detail}")]
    WgcUnsupported { detail: &'static str },
    #[error("捕获区域 {x},{y} {w}x{h} 超出帧范围 {frame_w}x{frame_h}")]
    RectOutOfBounds {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        frame_w: u32,
        frame_h: u32,
    },
    #[error("{w}x{h} 的 BGRA 缓冲需要 {expected} 字节，实际 {actual} 字节")]
    BufferSizeMismatch {
        w: u32,
        h: u32,
        expected: usize,
        actual: usize,
    },
}

/// 一帧 BGRA 截图（top-down，行主序，无额外填充：`stride == w * 4`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub stride: u32,
    pub data: Vec<u8>,
    pub captured_at: Instant,
}

impl Frame {
    /// 由 BGRA 缓冲构造；长度必须恰好是 `w * h * 4`。
    pub fn new(w: u32, h: u32, data: Vec<u8>, captured_at: Instant) -> Result<Self, CaptureError> {
        if w == 0 || h == 0 {
            return Err(CaptureError::InvalidRect {
                w: w as i32,
                h: h as i32,
            });
        }
        let expected = Self::buffer_len(w, h);
        if data.len() != expected {
            return Err(CaptureError::BufferSizeMismatch {
                w,
                h,
                expected,
                actual: data.len(),
            });
        }
        Ok(Self {
            w,
            h,
            stride: Self::stride_of(w),
            data,
            captured_at,
        })
    }

    /// 32bpp 位图的行字节数。
    pub const fn stride_of(w: u32) -> u32 {
        w * BYTES_PER_PIXEL
    }

    /// 所需缓冲字节数。
    pub fn buffer_len(w: u32, h: u32) -> usize {
        (w as usize) * (h as usize) * BYTES_PER_PIXEL as usize
    }

    /// 灰度图（BT.601 整数近似，见 `delta_vision::GrayImage::from_bgra`）。
    pub fn luma(&self) -> GrayImage {
        GrayImage::from_bgra(self.w, self.h, self.stride, &self.data)
            .expect("Frame 构造时已校验 stride 与缓冲长度自洽，此处不可能失败")
    }

    /// 黑像素（luma < [`BLACK_LUMA_CUTOFF`]）占比，百分比数值（0..=100）。
    pub fn black_ratio_pct(&self) -> f32 {
        let gray = self.luma();
        let dark = gray
            .as_slice()
            .iter()
            .filter(|&&value| value < BLACK_LUMA_CUTOFF)
            .count();
        dark as f32 * 100.0 / (self.w as f32 * self.h as f32)
    }

    /// 黑帧判定：黑像素占比 ≥ `threshold_pct`。
    pub fn is_black(&self, threshold_pct: f32) -> bool {
        self.black_ratio_pct() >= threshold_pct
    }

    /// 组装 PNG 之前的取像素辅助（越界即 panic，调用方负责边界）。
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let index = ((y as usize) * (self.stride as usize)) + (x as usize) * 4;
        [self.data[index], self.data[index + 1], self.data[index + 2]]
    }
}

/// 抓取区域并做黑帧检查的便捷入口（供上层流程直接使用）。
///
/// 每次调用独立申请并释放后端资源；黑帧返回错误而不是把全黑帧交给下游。
pub fn grab_checked(capturer: &mut Capturer, rect: PxRect) -> Result<Frame, CaptureError> {
    let frame = capturer.grab(rect)?;
    let black_pct = frame.black_ratio_pct();
    if black_pct >= DEFAULT_BLACK_THRESHOLD_PCT {
        return Err(CaptureError::BlackFrame {
            black_pct,
            cutoff: BLACK_LUMA_CUTOFF,
        });
    }
    Ok(frame)
}

/// 屏幕尺寸的便捷包装（校准向导首次启动时用）。
pub fn monitor_info() -> Result<MonitorInfo, CaptureError> {
    primary_monitor_size()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_with(fill: impl Fn(u32) -> [u8; 3], count: u32) -> Frame {
        let mut data = Vec::with_capacity(Frame::buffer_len(count, 1));
        for index in 0..count {
            let [b, g, r] = fill(index);
            data.extend_from_slice(&[b, g, r, 255]);
        }
        Frame::new(count, 1, data, Instant::now()).expect("测试帧长度正确")
    }

    #[test]
    fn stride_is_four_bytes_per_pixel() {
        let frame = frame_with(|_| [0, 0, 0], 7);
        assert_eq!(frame.stride, 28);
        assert_eq!(frame.stride, Frame::stride_of(7));
        assert_eq!(Frame::buffer_len(7, 3), 7 * 3 * 4);
    }

    #[test]
    fn luma_uses_bgra_channel_order() {
        // BGRA：纯蓝 / 纯绿 / 纯红 / 白。
        let frame = frame_with(
            |index| match index {
                0 => [255, 0, 0],
                1 => [0, 255, 0],
                2 => [0, 0, 255],
                _ => [255, 255, 255],
            },
            4,
        );
        assert_eq!(frame.luma().as_slice(), &[28, 149, 76, 255]);
        assert_eq!(frame.pixel(2, 0), [0, 0, 255]);
    }

    #[test]
    fn frame_rejects_wrong_buffer_size() {
        let error = Frame::new(2, 2, vec![0; 15], Instant::now()).expect_err("长度不符必须报错");
        assert!(
            matches!(
                error,
                CaptureError::BufferSizeMismatch {
                    expected: 16,
                    actual: 15,
                    ..
                }
            ),
            "{error}"
        );
        assert!(matches!(
            Frame::new(0, 4, Vec::new(), Instant::now()),
            Err(CaptureError::InvalidRect { w: 0, h: 4 })
        ));
    }

    #[test]
    fn black_detection_boundary_matches_spec() {
        // 1000 像素：994 黑 → 99.4%，996 黑 → 99.6%。
        let almost_all_dark = frame_with(
            |index| if index < 994 { [0, 0, 0] } else { [255, 255, 255] },
            1000,
        );
        let ratio = almost_all_dark.black_ratio_pct();
        assert!((ratio - 99.4).abs() < 0.01, "实际 {ratio}");
        assert!(!almost_all_dark.is_black(DEFAULT_BLACK_THRESHOLD_PCT));

        let dark_enough = frame_with(
            |index| if index < 996 { [0, 0, 0] } else { [255, 255, 255] },
            1000,
        );
        let ratio = dark_enough.black_ratio_pct();
        assert!((ratio - 99.6).abs() < 0.01, "实际 {ratio}");
        assert!(dark_enough.is_black(DEFAULT_BLACK_THRESHOLD_PCT));
    }

    #[test]
    fn black_frame_error_carries_actionable_advice() {
        let frame = frame_with(|_| [0, 0, 0], 64);
        assert!(frame.is_black(DEFAULT_BLACK_THRESHOLD_PCT));
        let error = CaptureError::BlackFrame {
            black_pct: frame.black_ratio_pct(),
            cutoff: BLACK_LUMA_CUTOFF,
        };
        assert!(error.to_string().contains("无边框窗口"), "{error}");
        assert!(error.to_string().contains("100.0%"), "{error}");
    }

    #[test]
    fn white_frame_is_not_black() {
        let frame = frame_with(|_| [255, 255, 255], 100);
        assert_eq!(frame.black_ratio_pct(), 0.0);
        assert!(!frame.is_black(DEFAULT_BLACK_THRESHOLD_PCT));
    }

    #[test]
    fn dpi_awareness_is_applied_once_and_stable() {
        let first = ensure_dpi_awareness();
        let second = ensure_dpi_awareness();
        assert_eq!(first, second, "DPI 感知必须幂等（OnceLock）");
    }
}
