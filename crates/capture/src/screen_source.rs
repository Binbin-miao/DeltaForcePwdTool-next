//! 截图来源抽象（真机是 [`LiveScreen`]，测试是内存画面）。
//!
//! `delta-flow-morse` 与 `delta-flow-fingerprint` 原先各自逐字重复了这份适配；
//! 两者都已依赖本 crate，因此收敛到这里，保持单一实现（不改依赖图）。

use delta_geom::PxRect;

use crate::{Capturer, Frame};

/// 截图来源。
pub trait ScreenSource {
    /// 抓取区域内的一帧（失败带可读原因，不静默）。
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String>;
}

/// 真机截图适配（复用 t2 的常驻 DC）。
pub struct LiveScreen<'a> {
    capturer: &'a mut Capturer,
}

impl<'a> LiveScreen<'a> {
    /// 包装一个常驻捕获器。
    pub fn new(capturer: &'a mut Capturer) -> Self {
        Self { capturer }
    }
}

impl ScreenSource for LiveScreen<'_> {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
        self.capturer.grab(rect).map_err(|error| error.to_string())
    }
}
