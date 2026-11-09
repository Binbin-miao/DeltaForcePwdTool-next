//! 截图后端选择 + 单次识别任务的截图配额（spec D1 / D3 / D4）。
//!
//! 生命周期契约：
//! - 一次识别任务开始（按 F8 / 主按钮）时建 [`CaptureSession`]，任务结束（成功 / 失败 /
//!   取消 / 超时）时 `Drop` 释放全部采集对象 —— **程序空闲时不得持有任何采集对象**；
//! - 配额随会话存在：任务开始时为 [`FRAME_BUDGET`] 张，超限的 `grab` 直接报错且**不抓屏**；
//! - 后端切换由下一次 `CaptureSession::new` 生效，不需要重启程序。

use std::fmt;

use delta_geom::PxRect;

use crate::{gdi, wgc, CaptureError, Frame};

/// 单次识别任务的截图硬上限（spec D4）。
pub const FRAME_BUDGET: u32 = 20;

/// 截图后端（spec D1：用户可选，默认 WGC）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CaptureBackend {
    /// Windows Graphics Capture：能截无边框窗口与绝大多数 D3D 画面。
    #[default]
    Wgc,
    /// BitBlt 兼容模式：可能截不到独占全屏。
    BitBlt,
}

impl CaptureBackend {
    /// 稳定标识，供配置/UI 读写（未知值回落见 t11-d 的配置层）。
    pub const fn token(self) -> &'static str {
        match self {
            Self::Wgc => "wgc",
            Self::BitBlt => "bitblt",
        }
    }

    /// 面向用户的名称。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Wgc => "Windows Graphics Capture",
            Self::BitBlt => "BitBlt（兼容模式）",
        }
    }
}

impl fmt::Display for CaptureBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// 会话内的实际后端对象：枚举把「后端 = 已初始化的对象」编码进类型，无需 `Option` + 不可达分支。
enum Backend {
    BitBlt,
    Wgc(wgc::WgcSession),
}

/// 一次识别任务的采集会话：后端 + 已用配额，`Drop` 时释放全部采集对象。
pub struct CaptureSession {
    backend: Backend,
    kind: CaptureBackend,
    used: u32,
}

impl CaptureSession {
    /// 建立会话：`Wgc` 在这里就把设备与采集会话全部拉起，失败**立即**报错而不是以后静默降级。
    pub fn new(kind: CaptureBackend) -> Result<Self, CaptureError> {
        let backend = match kind {
            CaptureBackend::BitBlt => Backend::BitBlt,
            CaptureBackend::Wgc => Backend::Wgc(wgc::WgcSession::new()?),
        };
        Ok(Self {
            backend,
            kind,
            used: 0,
        })
    }

    /// 本会话使用的后端。
    pub fn backend(&self) -> CaptureBackend {
        self.kind
    }

    /// 已抓取的帧数（供测试与「超限只报一次」的日志去重使用）。
    pub fn frames_taken(&self) -> u32 {
        self.used
    }

    /// 抓取一帧：配额耗尽时**不抓屏**，直接返回 [`CaptureError::BudgetExceeded`]。
    pub fn grab(&mut self, rect: PxRect) -> Result<Frame, CaptureError> {
        if self.used >= FRAME_BUDGET {
            return Err(CaptureError::BudgetExceeded {
                limit: FRAME_BUDGET,
            });
        }
        self.used = self.used.saturating_add(1);
        match &mut self.backend {
            Backend::BitBlt => gdi::capture(rect),
            Backend::Wgc(session) => session.grab(rect),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_rect() -> PxRect {
        PxRect {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
        }
    }

    #[test]
    fn default_backend_is_wgc() {
        assert_eq!(CaptureBackend::default(), CaptureBackend::Wgc);
        assert_eq!(CaptureBackend::Wgc.token(), "wgc");
        assert_eq!(CaptureBackend::BitBlt.token(), "bitblt");
        assert!(CaptureBackend::BitBlt.to_string().contains("兼容模式"));
    }

    #[test]
    fn the_twenty_first_grab_is_rejected_by_the_budget() {
        let mut session =
            CaptureSession::new(CaptureBackend::BitBlt).expect("BitBlt 会话可建");
        for round in 0..FRAME_BUDGET {
            session
                .grab(tiny_rect())
                .unwrap_or_else(|error| panic!("第 {round} 次抓取不该被配额拒绝：{error}"));
        }
        assert_eq!(session.frames_taken(), FRAME_BUDGET);

        let error = session.grab(tiny_rect()).expect_err("第 21 次必须超限");
        assert!(
            matches!(
                error,
                CaptureError::BudgetExceeded {
                    limit: FRAME_BUDGET
                }
            ),
            "{error}"
        );
        // 超限的调用不抓屏、也不推进计数。
        assert_eq!(session.frames_taken(), FRAME_BUDGET);
        assert!(error.to_string().contains("配额"), "{error}");
    }

    #[test]
    fn a_new_session_starts_with_a_full_budget() {
        let mut first =
            CaptureSession::new(CaptureBackend::BitBlt).expect("BitBlt 会话可建");
        first.grab(tiny_rect()).expect("抓取");
        assert_eq!(first.frames_taken(), 1);
        drop(first);

        let second =
            CaptureSession::new(CaptureBackend::BitBlt).expect("BitBlt 会话可建");
        assert_eq!(second.frames_taken(), 0, "任务结束配额必须归零");
        assert_eq!(second.backend(), CaptureBackend::BitBlt);
    }

    #[test]
    fn wgc_captures_a_real_frame_when_the_backend_is_available() {
        let mut session = match CaptureSession::new(CaptureBackend::Wgc) {
            Ok(session) => session,
            // 没有可用 D3D11 适配器的机器会在初始化处失败；那条分支的可读性由上一个用例覆盖。
            Err(_) => return,
        };
        let frame = session
            .grab(PxRect {
                x: 100,
                y: 100,
                w: 32,
                h: 32,
            })
            .expect("WGC 抓帧");
        assert_eq!((frame.w, frame.h, frame.stride), (32, 32, 128));
        assert_eq!(frame.data.len(), 32 * 32 * 4);
        assert_eq!(session.frames_taken(), 1);
        assert_eq!(session.backend(), CaptureBackend::Wgc);
    }

    #[test]
    fn wgc_session_initialises_or_reports_a_readable_hresult() {
        // 本机可能没有可用的 D3D11 适配器：两条分支都必须给出可定位的信息，且绝不回退 BitBlt。
        match CaptureSession::new(CaptureBackend::Wgc) {
            Ok(session) => {
                assert_eq!(session.backend(), CaptureBackend::Wgc);
                assert_eq!(session.frames_taken(), 0);
            }
            Err(error) => {
                let text = error.to_string();
                assert!(text.contains("WGC 初始化失败"), "{text}");
                assert!(text.contains("0x"), "{text}");
            }
        }
    }
}
