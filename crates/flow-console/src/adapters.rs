//! 生产环境的三个 trait 实现：包真实截图、真实输入注入与真实时钟。
//!
//! 单元测试**不会**构造这三个类型（见 `src/tests_support.rs`），因此测试全程不碰真实
//! 屏幕与光标 —— 这是 t8「可测试性」条款的落地方式。

use std::time::{Duration, Instant};

use delta_capture::{CaptureSession, Frame};
use delta_geom::{PxPoint, PxRect};
use delta_input::{event_count, Drag};

use crate::{Clock, Pointer, Screen};

/// 真实屏幕抓取（**借用任务会话**：后端与配额由会话统一管辖，spec D3 / D4）。
pub struct SystemScreen<'a> {
    session: &'a mut CaptureSession,
}

impl<'a> SystemScreen<'a> {
    /// 借用一次识别任务的采集会话（DPI 感知已在主入口完成）。
    pub fn new(session: &'a mut CaptureSession) -> Self {
        Self { session }
    }
}

impl Screen for SystemScreen<'_> {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
        self.session.grab(rect).map_err(|error| error.to_string())
    }
}

/// 真实输入注入（分步相对拖动 + 单击取焦）。
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
    fn begin(&mut self, at: PxPoint) -> Result<(), String> {
        self.drag.begin(at).map_err(|error| error.to_string())
    }

    fn step(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        self.drag.step(dx, dy).map_err(|error| error.to_string())
    }

    fn end(&mut self) -> Result<(), String> {
        self.drag.end().map_err(|error| error.to_string())
    }

    fn click(&mut self, at: PxPoint) -> Result<(), String> {
        delta_input::click(at).map_err(|error| error.to_string())
    }

    fn event_count(&self) -> u64 {
        event_count()
    }
}

/// 真实时钟：`now_ms` 单调、`sleep_ms` 真实阻塞。
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
