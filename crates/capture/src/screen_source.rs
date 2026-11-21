//! 截图来源抽象（真机是 [`LiveScreen`]，测试是内存画面）。
//!
//! `delta-flow-morse` 与 `delta-flow-fingerprint` 原先各自逐字重复了这份适配；
//! 两者都已依赖本 crate，因此收敛到这里，保持单一实现（不改依赖图）。

use delta_geom::PxRect;

use crate::{CaptureSession, Frame};

/// 截图来源。
pub trait ScreenSource {
    /// 抓取区域内的一帧（失败带可读原因，不静默）。
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String>;
}

/// 真机截图适配：**借用任务会话**，后端与配额都由会话统一管辖（spec D3 / D4）。
///
/// 适配层不再自建抓屏器：一次识别任务只有一条采集链路，配额才可能真的封顶。
pub struct LiveScreen<'a> {
    session: &'a mut CaptureSession,
}

impl<'a> LiveScreen<'a> {
    /// 借用任务会话（会话的生命周期由调用方持有）。
    pub fn new(session: &'a mut CaptureSession) -> Self {
        Self { session }
    }
}

impl ScreenSource for LiveScreen<'_> {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
        self.session.grab(rect).map_err(|error| error.to_string())
    }
}
