//! 生产环境适配器：封装 `CaptureSession`、`delta-input` 与 `Instant`/`sleep`。

use std::time::{Duration, Instant};

use delta_capture::{CaptureSession, Frame};
use delta_geom::{PxPoint, PxRect};
use delta_input::event_count;

use crate::{Clock, Pointer, Screen};

/// 真实屏幕抓取（借用任务会话，受 15s / 300 帧预算管辖）。
pub struct SystemScreen<'a> {
    session: &'a mut CaptureSession,
}

impl<'a> SystemScreen<'a> {
    pub fn new(session: &'a mut CaptureSession) -> Self {
        Self { session }
    }
}

impl Screen for SystemScreen<'_> {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
        self.session.grab(rect).map_err(|error| error.to_string())
    }
}

/// 真实输入注入（左键点击 + 空格键 `0x39`）。
#[derive(Default)]
pub struct SystemPointer;

impl SystemPointer {
    pub fn new() -> Self {
        Self
    }
}

impl Pointer for SystemPointer {
    fn click(&mut self, at: PxPoint) -> Result<(), String> {
        delta_input::click(at).map_err(|error| error.to_string())
    }

    fn key_space(&mut self) -> Result<(), String> {
        delta_input::key_space().map_err(|error| error.to_string())
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
