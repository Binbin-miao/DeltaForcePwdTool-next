//! 事件建模、投递与计数器。
//!
//! 关键设计：**事件规划（纯函数）与系统投递分离**。规划部分可以在测试里断言事件序列，
//! 投递部分只在 `Sink::Live` 模式下才真的碰 `SendInput` —— 测试绝不移动用户的真实光标。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use delta_geom::PxPoint;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEINPUT, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::InputError;

/// 单个子步最大位移（浮光：Delta Force 里一次大跳会被丢帧）。
///
/// **这条不许放宽**：游戏 raw input 每子步 ≤20px 是硬约束，加大必丢事件。
pub const MAX_SUBSTEP_PX: i32 = 20;
/// 左键按下到抬起的保持时长（上游 `mouse.py` 的 0.05s）。
pub const CLICK_HOLD: Duration = Duration::from_millis(50);
/// 连点之间的间隔（上游 `click_sequence(delay=0.05)`）。
pub const CLICK_GAP: Duration = Duration::from_millis(50);
/// 注入事件的 `dwExtraInfo` 标记（spec D12：注入必然可识别）。
pub const INJECTION_TAG: usize = 0x4445_4C54; // "DELT"

/// 一条逻辑输入事件。既是投递单元，也是测试里可断言的事件序列单元。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Injected {
    /// 绝对定位（仅用于归位到起点，**不参与拖动位移**）。
    MoveAbsolute { x: u16, y: u16 },
    /// 相对位移（拖动的唯一手段）。
    Move { dx: i32, dy: i32 },
    LeftDown,
    LeftUp,
    Key { scancode: u16, up: bool },
}

/// 进程级输入事件总计数（t8 用它断言「完成/中止后零新增输入」）。
static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 已投递（或已记录）的输入事件总数。
pub fn event_count() -> u64 {
    EVENT_COUNTER.load(Ordering::SeqCst)
}

/// 把基线归零，便于上层 `let baseline = event_count(); ... assert_eq!(event_count() - baseline, 0)`。
pub fn reset_event_count() {
    EVENT_COUNTER.store(0, Ordering::SeqCst);
}

/// 录制句柄：与 [`crate::Drag`] 共享同一份事件日志，`Drag` 被 `Drop` 之后仍可读取。
#[derive(Debug, Clone, Default)]
pub struct RecordHandle(Arc<Mutex<Vec<Injected>>>);

impl RecordHandle {
    pub fn events(&self) -> Vec<Injected> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn len(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 投递目标。`Live` 才真正调用 `SendInput`。
#[derive(Debug, Clone)]
pub(crate) enum Sink {
    Live,
    Record(RecordHandle),
}

impl Sink {
    pub(crate) fn emit(&self, event: Injected) -> Result<(), InputError> {
        EVENT_COUNTER.fetch_add(1, Ordering::SeqCst);
        match self {
            Self::Live => send(&event),
            Self::Record(handle) => {
                handle
                    .0
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push(event);
                Ok(())
            }
        }
    }
}

/// 主显示器物理分辨率（与 spec D9 一致：只认主显示器）。
pub(crate) fn screen_size() -> Result<(i32, i32), InputError> {
    // SAFETY: GetSystemMetrics 无参数副作用，任何线程可安全调用。
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if w <= 0 || h <= 0 {
        // 拿不到分辨率时退回 1920x1080：绝对定位只用于归位，偏差不影响相对拖动。
        return Ok((1920, 1080));
    }
    Ok((w, h))
}

/// 物理像素 → `SendInput` 的 0..=65535 归一化坐标。
pub(crate) fn normalise(value: i32, extent: i32) -> u16 {
    if extent <= 1 {
        return 0;
    }
    let clamped = value.clamp(0, extent - 1);
    ((i64::from(clamped) * 65_535) / i64::from(extent - 1)) as u16
}

/// 单击事件规划：绝对定位 → 按下 → 抬起。
pub fn plan_click_events(at: PxPoint, screen: (i32, i32)) -> Vec<Injected> {
    vec![
        Injected::MoveAbsolute {
            x: normalise(at.x, screen.0),
            y: normalise(at.y, screen.1),
        },
        Injected::LeftDown,
        Injected::LeftUp,
    ]
}

/// 数字键事件规划：scancode 按下 + 抬起。
pub fn plan_digit_events(digit: char) -> Result<Vec<Injected>, InputError> {
    let scancode = crate::scancode_for_digit(digit).ok_or(InputError::NotADigit(digit))?;
    Ok(vec![
        Injected::Key {
            scancode,
            up: false,
        },
        Injected::Key {
            scancode,
            up: true,
        },
    ])
}

/// 把 `(dx, dy)` 拆成每步都不超过 [`MAX_SUBSTEP_PX`] 的子步序列，**和严格等于原位移**。
///
/// 用累积端点差分而不是逐段四舍五入，保证任意比例下总和零漂移。
pub fn split_steps(dx: i32, dy: i32) -> Vec<(i32, i32)> {
    let longest = dx.abs().max(dy.abs());
    if longest == 0 {
        return Vec::new();
    }
    let count = (i64::from(longest) + i64::from(MAX_SUBSTEP_PX) - 1) / i64::from(MAX_SUBSTEP_PX);
    let mut steps = Vec::with_capacity(count as usize);
    let (mut previous_x, mut previous_y) = (0i64, 0i64);
    for index in 1..=count {
        let target_x = i64::from(dx) * index / count;
        let target_y = i64::from(dy) * index / count;
        steps.push(((target_x - previous_x) as i32, (target_y - previous_y) as i32));
        previous_x = target_x;
        previous_y = target_y;
    }
    steps
}

/// 直接投递一个事件并计入计数器（点击与数字键路径用；拖动走 `Sink`）。
pub(crate) fn inject(event: &Injected) -> Result<(), InputError> {
    EVENT_COUNTER.fetch_add(1, Ordering::SeqCst);
    send(event)
}

/// 真正投递一个事件。`SendInput` 返回少于 1 即为被拦截（UIPI / 会话隔离）。
pub(crate) fn send(event: &Injected) -> Result<(), InputError> {
    let input = match *event {
        Injected::MoveAbsolute { x, y } => mouse_input(
            i32::from(x),
            i32::from(y),
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
        ),
        Injected::Move { dx, dy } => mouse_input(dx, dy, MOUSEEVENTF_MOVE),
        Injected::LeftDown => mouse_input(0, 0, MOUSEEVENTF_LEFTDOWN),
        Injected::LeftUp => mouse_input(0, 0, MOUSEEVENTF_LEFTUP),
        Injected::Key { scancode, up } => keyboard_input(scancode, key_flags(up)),
    };
    // SAFETY: INPUT 已完全初始化，长度与 size_of::<INPUT>() 一致。
    let accepted = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if accepted == 1 {
        Ok(())
    } else {
        Err(InputError::Rejected {
            accepted,
            total: 1,
        })
    }
}

fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTION_TAG,
            },
        },
    }
}

fn keyboard_input(scancode: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: Default::default(),
                wScan: scancode,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTION_TAG,
            },
        },
    }
}

fn key_flags(up: bool) -> KEYBD_EVENT_FLAGS {
    let mut flags = KEYEVENTF_SCANCODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_steps_never_exceeds_limit_and_sums_exactly() {
        for (dx, dy) in [(0, 0), (1, 0), (20, 0), (21, 0), (60, -35), (-1000, 7), (3, -137)] {
            let steps = split_steps(dx, dy);
            let sum_x: i32 = steps.iter().map(|s| s.0).sum();
            let sum_y: i32 = steps.iter().map(|s| s.1).sum();
            assert_eq!((sum_x, sum_y), (dx, dy), "位移总和必须零漂移：{steps:?}");
            for (step_x, step_y) in &steps {
                assert!(
                    step_x.abs() <= MAX_SUBSTEP_PX && step_y.abs() <= MAX_SUBSTEP_PX,
                    "子步越界：{step_x},{step_y}"
                );
            }
            let expected_steps = (dx.abs().max(dy.abs()) as f32 / MAX_SUBSTEP_PX as f32).ceil();
            assert_eq!(steps.len(), expected_steps as usize, "{steps:?}");
        }
    }

    #[test]
    fn zero_delta_produces_no_events() {
        assert!(split_steps(0, 0).is_empty());
    }

    #[test]
    fn normalise_maps_screen_corners_to_wire_range() {
        assert_eq!(normalise(0, 1920), 0);
        assert_eq!(normalise(1919, 1920), 65535);
        assert_eq!(normalise(-50, 1920), 0, "负坐标钳到左边界");
        assert_eq!(normalise(99999, 1920), 65535, "越界钳到右边界");
        assert_eq!(normalise(5, 0), 0, "无效分辨率不 panic");
    }

    #[test]
    fn digit_events_use_scancodes_and_pair_down_up() {
        let events = plan_digit_events('7').expect("数字键");
        assert_eq!(
            events,
            vec![
                Injected::Key {
                    scancode: 0x08,
                    up: false
                },
                Injected::Key {
                    scancode: 0x08,
                    up: true
                },
            ]
        );
        assert!(matches!(
            plan_digit_events('a'),
            Err(InputError::NotADigit('a'))
        ));
    }

    #[test]
    fn recorder_captures_events_without_touching_the_system() {
        let handle = RecordHandle::default();
        let sink = Sink::Record(handle.clone());
        sink.emit(Injected::LeftDown).expect("记录");
        sink.emit(Injected::LeftUp).expect("记录");
        assert_eq!(handle.events(), vec![Injected::LeftDown, Injected::LeftUp]);
        assert_eq!(handle.len(), 2);
    }

    #[test]
    fn key_flags_include_scan_code_and_optional_release() {
        let down = key_flags(false);
        let up = key_flags(true);
        assert!(down.contains(KEYEVENTF_SCANCODE));
        assert!(!down.contains(KEYEVENTF_KEYUP));
        assert!(up.contains(KEYEVENTF_SCANCODE));
        assert!(up.contains(KEYEVENTF_KEYUP));
    }
}
