//! 生产环境适配器：封装 `CaptureSession`、`delta-input` 与 `Instant`/`sleep`。

use std::time::{Duration, Instant};

use delta_capture::{CaptureSession, Frame};
use delta_geom::{PxPoint, PxRect};
use delta_input::{event_count, Drag};

use crate::{Clock, Pointer, Screen};

/// 真实屏幕抓取（借用任务会话，受 15s / 300 帧预算管辖）。
pub struct SystemScreen<'a> {
    session: &'a mut CaptureSession,
    /// 整帧范围（构造时量一次；量不到就是零面积，信号线台据此零输入退出）。
    full: PxRect,
}

impl<'a> SystemScreen<'a> {
    pub fn new(session: &'a mut CaptureSession) -> Self {
        let full = delta_capture::monitor_info()
            .map(|monitor| PxRect {
                x: 0,
                y: 0,
                w: monitor.w,
                h: monitor.h,
            })
            .unwrap_or(PxRect::ZERO);
        Self { session, full }
    }
}

impl Screen for SystemScreen<'_> {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
        self.session.grab(rect).map_err(|error| error.to_string())
    }

    fn frame_rect(&self) -> PxRect {
        self.full
    }
}

/// 真实输入注入（左键点击 + 空格键 `0x39` + 分步纵拖）。
///
/// 拖动句柄 `Drag` 自带 `Drop` 兜底抬手：即使上层在拖动中途报错/取消，
/// 左键也不会卡住。
#[derive(Default)]
pub struct SystemPointer {
    drag: Drag,
}

impl SystemPointer {
    pub fn new() -> Self {
        Self { drag: Drag::new() }
    }
}

impl Pointer for SystemPointer {
    fn click(&mut self, at: PxPoint) -> Result<(), String> {
        delta_input::click(at).map_err(|error| error.to_string())
    }

    fn key_space(&mut self) -> Result<(), String> {
        delta_input::key_space().map_err(|error| error.to_string())
    }

    fn begin(&mut self, at: PxPoint) -> Result<(), String> {
        self.drag.begin(at).map_err(|error| error.to_string())
    }

    fn step(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        self.drag.step(dx, dy).map_err(|error| error.to_string())
    }

    fn end(&mut self) -> Result<(), String> {
        self.drag.end().map_err(|error| error.to_string())
    }

    fn event_count(&self) -> u64 {
        event_count()
    }
}

/// 真实时钟。
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    fn sleep_ms(&mut self, ms: u64) {
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}
