//! 百分比 ↔ 物理像素几何换算（Deep Module）。
//!
//! 规格条款：
//! - **spec D9**：坐标以百分比存储（相对主显示器），换算必须使用主显示器的**物理分辨率**；
//!   校准缺失的必填区域禁止启动对应流程。
//! - **t1 任务 3**：换算四舍五入到整数像素；`px → pct → px` 在同一分辨率下必须完全还原（幂等）；
//!   越界返回 [`GeomError::OutOfScreen`]，裁剪只在校准 UI 显式调用 [`PxRect::clamp_to_screen`] 时发生。
//!
//! 对外只有窄接口：两组坐标类型 + `to_px` / `to_pct`，浮点误差在边界一次性被 `round` 吸收，
//! 模块内部不再做任何隐式缩放，因此上游「连续 load 二次缩放漂移」在这一层被结构性消除。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 百分比合法区间下界。
pub const PCT_MIN: f32 = 0.0;
/// 百分比合法区间上界。
pub const PCT_MAX: f32 = 1.0;

/// 几何换算错误。全部为**显式失败**，不允许静默裁剪或静默回退。
#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum GeomError {
    #[error("主显示器尺寸非法：{w}x{h}（未校准或读数为零）")]
    InvalidMonitor { w: i32, h: i32 },
    #[error("百分比越出 [0,1]：{value}")]
    PctOutOfRange { value: f32 },
    #[error("矩形尺寸为负：w={w}, h={h}")]
    NegativeSize { w: i32, h: i32 },
    #[error("区域越出主显示器：rect=({x},{y},{w},{h}) 屏幕={monitor_w}x{monitor_h}")]
    OutOfScreen {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        monitor_w: i32,
        monitor_h: i32,
    },
}

/// 主显示器物理分辨率（pixel 为单位的整数尺寸）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub w: i32,
    pub h: i32,
}

impl MonitorInfo {
    /// 唯一合法的构造入口：拒绝任何非正尺寸。
    pub fn new(w: i32, h: i32) -> Result<Self, GeomError> {
        if w <= 0 || h <= 0 {
            return Err(GeomError::InvalidMonitor { w, h });
        }
        Ok(Self { w, h })
    }

    /// 是否已校准（spec D9：未校准不得启动对应流程）。
    pub fn is_calibrated(&self) -> bool {
        self.w > 0 && self.h > 0
    }

    fn checked(&self) -> Result<Self, GeomError> {
        Self::new(self.w, self.h)
    }
}

impl Default for MonitorInfo {
    /// 未校准占位值：`is_calibrated()` 为 false，参与任何换算前必须用 [`MonitorInfo::new`] 校验。
    fn default() -> Self {
        Self { w: 0, h: 0 }
    }
}

/// 百分比矩形，四个分量都在 `[0.0, 1.0]`，相对主显示器宽高。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PctRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Default for PctRect {
    /// 未校准占位值（零面积）。
    fn default() -> Self {
        Self::EMPTY
    }
}

impl PctRect {
    /// 未校准 / 零面积占位矩形。
    pub const EMPTY: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    /// 零面积判定：校准 UI 用它把区域标记为「缺失」。
    pub fn is_empty(self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// 百分比 → 物理像素（四舍五入）。
    pub fn to_px(self, monitor: MonitorInfo) -> Result<PxRect, GeomError> {
        let monitor = monitor.checked()?;
        for value in [self.x, self.y, self.w, self.h] {
            if !value.is_finite() || !(PCT_MIN..=PCT_MAX).contains(&value) {
                return Err(GeomError::PctOutOfRange { value });
            }
        }
        Ok(PxRect {
            x: scale(self.x, monitor.w),
            y: scale(self.y, monitor.h),
            w: scale(self.w, monitor.w),
            h: scale(self.h, monitor.h),
        })
    }
}

/// 物理像素矩形（主显示器坐标系，原点左上）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Default for PxRect {
    /// 零面积占位值。
    fn default() -> Self {
        Self::ZERO
    }
}

impl PxRect {
    /// 零面积占位矩形。
    pub const ZERO: Self = Self {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    /// 右边界（饱和运算，避免校准越界输入造成整数溢出 panic）。
    pub fn right(self) -> i32 {
        self.x.saturating_add(self.w)
    }

    /// 下边界（饱和运算）。
    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.h)
    }

    /// 物理像素 → 百分比。要求矩形完整落在屏幕内，否则返回 [`GeomError::OutOfScreen`]。
    pub fn to_pct(self, monitor: MonitorInfo) -> Result<PctRect, GeomError> {
        let monitor = monitor.checked()?;
        self.ensure_on_screen(monitor)?;
        Ok(PctRect {
            x: ratio(self.x, monitor.w),
            y: ratio(self.y, monitor.h),
            w: ratio(self.w, monitor.w),
            h: ratio(self.h, monitor.h),
        })
    }

    /// 越界校验：越界即报错，不裁剪。
    pub fn ensure_on_screen(self, monitor: MonitorInfo) -> Result<(), GeomError> {
        let monitor = monitor.checked()?;
        if self.w < 0 || self.h < 0 {
            return Err(GeomError::NegativeSize {
                w: self.w,
                h: self.h,
            });
        }
        if self.x < 0 || self.y < 0 || self.right() > monitor.w || self.bottom() > monitor.h {
            return Err(GeomError::OutOfScreen {
                x: self.x,
                y: self.y,
                w: self.w,
                h: self.h,
                monitor_w: monitor.w,
                monitor_h: monitor.h,
            });
        }
        Ok(())
    }

    /// 裁剪到屏幕边界。**仅**校准 UI 在拖拽越界时显式调用（t1 任务 3）。
    /// 显示器未校准（尺寸非正）时返回 [`PxRect::ZERO`]。
    pub fn clamp_to_screen(self, monitor: MonitorInfo) -> Self {
        let Ok(monitor) = monitor.checked() else {
            return Self::ZERO;
        };
        let x = self.x.clamp(0, monitor.w);
        let y = self.y.clamp(0, monitor.h);
        let w = self.w.clamp(0, monitor.w.saturating_sub(x));
        let h = self.h.clamp(0, monitor.h.saturating_sub(y));
        Self { x, y, w, h }
    }

    /// 零面积判定。
    pub fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

/// 百分比点。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PctPoint {
    pub x: f32,
    pub y: f32,
}

impl Default for PctPoint {
    /// 未校准占位值。
    fn default() -> Self {
        Self { x: 0.0, y: 0.0 }
    }
}

impl PctPoint {
    /// 百分比 → 物理像素（四舍五入）。
    pub fn to_px(self, monitor: MonitorInfo) -> Result<PxPoint, GeomError> {
        let monitor = monitor.checked()?;
        for value in [self.x, self.y] {
            if !value.is_finite() || !(PCT_MIN..=PCT_MAX).contains(&value) {
                return Err(GeomError::PctOutOfRange { value });
            }
        }
        Ok(PxPoint {
            x: scale(self.x, monitor.w),
            y: scale(self.y, monitor.h),
        })
    }
}

/// 物理像素点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PxPoint {
    pub x: i32,
    pub y: i32,
}

impl Default for PxPoint {
    /// 零坐标占位值。
    fn default() -> Self {
        Self::ZERO
    }
}

impl PxPoint {
    /// 零坐标占位点。
    pub const ZERO: Self = Self { x: 0, y: 0 };

    /// 物理像素 → 百分比。越界返回 [`GeomError::OutOfScreen`]。
    pub fn to_pct(self, monitor: MonitorInfo) -> Result<PctPoint, GeomError> {
        let monitor = monitor.checked()?;
        if self.x < 0 || self.y < 0 || self.x >= monitor.w || self.y >= monitor.h {
            return Err(GeomError::OutOfScreen {
                x: self.x,
                y: self.y,
                w: 0,
                h: 0,
                monitor_w: monitor.w,
                monitor_h: monitor.h,
            });
        }
        Ok(PctPoint {
            x: ratio(self.x, monitor.w),
            y: ratio(self.y, monitor.h),
        })
    }
}

/// 百分比 → 整数像素：`round` 吸收浮点误差，保证 `px → pct → px` 幂等。
fn scale(pct: f32, extent: i32) -> i32 {
    (f64::from(pct) * f64::from(extent)).round() as i32
}

/// 整数像素 → 百分比（f64 中间量，再窄化为存储用的 f32）。
fn ratio(px: i32, extent: i32) -> f32 {
    (f64::from(px) / f64::from(extent)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITORS: [(i32, i32); 4] = [(1920, 1080), (2560, 1440), (3840, 2160), (1366, 768)];

    const RECTS: [PxRect; 5] = [
        PxRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        PxRect {
            x: 17,
            y: 143,
            w: 311,
            h: 97,
        },
        PxRect {
            x: 1899,
            y: 1079,
            w: 1,
            h: 1,
        },
        PxRect {
            x: 0,
            y: 0,
            w: 1360,
            h: 760,
        },
        PxRect {
            x: 640,
            y: 360,
            w: 512,
            h: 288,
        },
    ];

    fn monitor(w: i32, h: i32) -> MonitorInfo {
        MonitorInfo::new(w, h).expect("测试显示器尺寸固定为正")
    }

    #[test]
    fn px_pct_px_roundtrip_is_exact() {
        for (w, h) in MONITORS {
            let m = monitor(w, h);
            for rect in RECTS {
                let in_bounds = rect
                    .ensure_on_screen(m)
                    .is_ok_and(|()| rect.right() <= w && rect.bottom() <= h);
                if !in_bounds {
                    continue;
                }
                let back = rect
                    .to_pct(m)
                    .expect("屏幕内矩形必须可转百分比")
                    .to_px(m)
                    .expect("百分比必须可回转像素");
                assert_eq!(back, rect, "屏幕 {w}x{h} 上 {rect:?} 幂等还原失败");
            }
        }
    }

    #[test]
    fn pct_px_pct_roundtrip_is_stable() {
        let m = monitor(2560, 1440);
        for rect in RECTS {
            let Ok(px) = rect.to_pct(m) else { continue };
            let again = px
                .to_px(m)
                .expect("合法百分比必须可转像素")
                .to_pct(m)
                .expect("屏幕内像素必须可回百分比");
            assert_eq!(again, px);
        }
    }

    #[test]
    fn non_representable_percentage_rounds_to_nearest_pixel() {
        let m = monitor(1920, 1080);
        let rect = PctRect {
            x: 0.1,
            y: 0.5,
            w: 0.25,
            h: 0.3333,
        };
        let px = rect.to_px(m).expect("合法百分比");
        assert_eq!(
            px,
            PxRect {
                x: 192,
                y: 540,
                w: 480,
                h: 360
            }
        );
    }

    #[test]
    fn out_of_screen_is_an_error_not_a_silent_crop() {
        let m = monitor(1920, 1080);
        let oversize = PxRect {
            x: 1900,
            y: 1000,
            w: 100,
            h: 100,
        };
        assert_eq!(
            oversize.ensure_on_screen(m),
            Err(GeomError::OutOfScreen {
                x: 1900,
                y: 1000,
                w: 100,
                h: 100,
                monitor_w: 1920,
                monitor_h: 1080,
            })
        );
        assert!(oversize.to_pct(m).is_err());
    }

    #[test]
    fn clamp_is_only_reachable_by_explicit_call() {
        let m = monitor(1920, 1080);
        let oversize = PxRect {
            x: 1900,
            y: 1000,
            w: 100,
            h: 100,
        };
        assert_eq!(
            oversize.clamp_to_screen(m),
            PxRect {
                x: 1900,
                y: 1000,
                w: 20,
                h: 80
            }
        );
    }

    #[test]
    fn negative_size_and_out_of_range_percentage_are_rejected() {
        let m = monitor(1920, 1080);
        let negative = PxRect {
            x: 0,
            y: 0,
            w: -5,
            h: 10,
        };
        assert_eq!(
            negative.ensure_on_screen(m),
            Err(GeomError::NegativeSize { w: -5, h: 10 })
        );

        let bad_pct = PctRect {
            x: 1.5,
            y: 0.0,
            w: 0.1,
            h: 0.1,
        };
        assert_eq!(
            bad_pct.to_px(m),
            Err(GeomError::PctOutOfRange { value: 1.5 })
        );

        let nan_pct = PctRect {
            x: f32::NAN,
            y: 0.0,
            w: 0.1,
            h: 0.1,
        };
        assert!(nan_pct.to_px(m).is_err());
    }

    #[test]
    fn uncalibrated_monitor_is_rejected_everywhere() {
        let uncalibrated = MonitorInfo::default();
        assert!(!uncalibrated.is_calibrated());
        let rect = PxRect {
            x: 1,
            y: 2,
            w: 3,
            h: 4,
        };
        assert_eq!(
            rect.to_pct(uncalibrated),
            Err(GeomError::InvalidMonitor { w: 0, h: 0 })
        );
        assert_eq!(
            PctRect::EMPTY.to_px(uncalibrated),
            Err(GeomError::InvalidMonitor { w: 0, h: 0 })
        );
        assert_eq!(rect.clamp_to_screen(uncalibrated), PxRect::ZERO);
    }

    #[test]
    fn zero_area_rect_roundtrips_and_reports_empty() {
        let m = monitor(1920, 1080);
        let empty = PctRect::EMPTY;
        assert!(empty.is_empty());
        let px = empty.to_px(m).expect("零面积百分比合法");
        assert_eq!(px, PxRect::ZERO);
        assert!(px.is_empty());
        assert_eq!(px.to_pct(m).expect("零面积像素合法"), empty);
    }

    #[test]
    fn points_roundtrip_and_reject_out_of_screen() {
        let m = monitor(3840, 2160);
        let px = PxPoint { x: 960, y: 1080 };
        let pct = px.to_pct(m).expect("屏幕内点");
        assert_eq!(pct.to_px(m).expect("合法百分比"), px);

        let outside = PxPoint { x: 3840, y: 0 };
        assert!(outside.to_pct(m).is_err());
    }
}
