//! 分步相对拖动（spec D8）。
//!
//! ```text
//! begin(at)   绝对定位到起点 → LEFTDOWN
//! step(dx,dy) 相对 MOVE 分步，每子步 ≤20px、间隔 2ms
//! end()       LEFTUP
//! Drop        兜底 LEFTUP（异常路径不得把鼠标键卡死）
//! ```
//!
//! 拖动全程**不使用** `SetCursorPos`（会和相对移动打架），也不使用
//! `MOUSEEVENTF_ABSOLUTE` 做位移 —— 绝对定位只出现在 `begin` 的归位那一步。

use delta_geom::PxPoint;

use crate::inject::{screen_size, split_steps, Injected, RecordHandle, Sink, SUBSTEP_DELAY};
use crate::InputError;

/// 保持左键按下状态的拖动手柄。
///
/// 状态机是 `Idle -> Pressed -> Idle`，两个方向都显式报错：
/// 未按下就 `step`、已按下又 `begin`，都返回 [`InputError`]，不静默。
#[derive(Debug)]
pub struct Drag {
    sink: Sink,
    pressed: bool,
}

impl Drag {
    /// 真实注入模式。
    pub fn new() -> Self {
        Self {
            sink: Sink::Live,
            pressed: false,
        }
    }

    /// 录制模式：事件序列写进返回的 [`RecordHandle`]，**不碰系统光标**。测试专用。
    pub fn recording() -> (Self, RecordHandle) {
        let handle = RecordHandle::default();
        (
            Self {
                sink: Sink::Record(handle.clone()),
                pressed: false,
            },
            handle,
        )
    }

    /// 定位到 `at` 并按下左键。
    pub fn begin(&mut self, at: PxPoint) -> Result<(), InputError> {
        if self.pressed {
            return Err(InputError::AlreadyPressed);
        }
        let screen = screen_size()?;
        let (x, y) = (
            crate::inject::normalise(at.x, screen.0),
            crate::inject::normalise(at.y, screen.1),
        );
        self.sink.emit(Injected::MoveAbsolute { x, y })?;
        self.sink.emit(Injected::LeftDown)?;
        self.pressed = true;
        Ok(())
    }

    /// 相对位移一步。内部拆成 ≤20px 的子步，子步之间等 2ms。
    pub fn step(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        if !self.pressed {
            return Err(InputError::NotPressed);
        }
        for (step_x, step_y) in split_steps(dx, dy) {
            self.sink.emit(Injected::Move {
                dx: step_x,
                dy: step_y,
            })?;
            // 录制模式跳过等待：测试要快、要确定。
            if !self.sink.is_recording() {
                std::thread::sleep(SUBSTEP_DELAY);
            }
        }
        Ok(())
    }

    /// 抬起左键。
    pub fn end(&mut self) -> Result<(), InputError> {
        if !self.pressed {
            return Err(InputError::NotPressed);
        }
        // 先把状态改回 Idle，再投递：即使投递失败，`Drop` 也不会重复抬手。
        self.pressed = false;
        self.sink.emit(Injected::LeftUp)
    }

    pub fn is_pressed(&self) -> bool {
        self.pressed
    }
}

impl Default for Drag {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Drag {
    /// 异常路径兜底：只要还按着键，就一定要抬起来。
    fn drop(&mut self) {
        if self.pressed {
            let _ = self.end();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::MAX_SUBSTEP_PX;

    #[test]
    fn drag_event_order_is_down_moves_up() {
        let (mut drag, log) = Drag::recording();
        drag.begin(PxPoint { x: 100, y: 200 }).expect("begin");
        drag.step(60, -35).expect("step");
        drag.end().expect("end");

        let events = log.events();
        assert!(
            matches!(events[0], Injected::MoveAbsolute { .. }),
            "第一步必须归位到起点：{events:?}"
        );
        assert_eq!(events[1], Injected::LeftDown);
        assert_eq!(*events.last().expect("非空"), Injected::LeftUp);
        assert_eq!(
            events.iter().filter(|e| **e == Injected::LeftDown).count(),
            1,
            "只允许一次按下"
        );

        let moves: Vec<(i32, i32)> = events
            .iter()
            .filter_map(|event| match event {
                Injected::Move { dx, dy } => Some((*dx, *dy)),
                _ => None,
            })
            .collect();
        assert_eq!(moves.iter().map(|m| m.0).sum::<i32>(), 60);
        assert_eq!(moves.iter().map(|m| m.1).sum::<i32>(), -35);
        for (dx, dy) in moves {
            assert!(
                dx.abs() <= MAX_SUBSTEP_PX && dy.abs() <= MAX_SUBSTEP_PX,
                "子步越界：{dx},{dy}"
            );
        }
    }

    #[test]
    fn drag_never_uses_absolute_move_while_stepping() {
        let (mut drag, log) = Drag::recording();
        drag.begin(PxPoint::ZERO).expect("begin");
        drag.step(100, 100).expect("step");
        drag.step(-100, 100).expect("step");

        let absolute = log
            .events()
            .iter()
            .filter(|event| matches!(event, Injected::MoveAbsolute { .. }))
            .count();
        assert_eq!(absolute, 1, "绝对定位只允许出现在 begin 归位那一步");
    }

    #[test]
    fn drop_releases_the_button_on_exception_paths() {
        let log = {
            let (mut drag, log) = Drag::recording();
            drag.begin(PxPoint { x: 5, y: 5 }).expect("begin");
            drag.step(30, 30).expect("step");
            assert!(drag.is_pressed());
            log
            // drag 在此 drop
        };
        let events = log.events();
        assert_eq!(
            *events.last().expect("非空"),
            Injected::LeftUp,
            "Drop 必须兜底抬手，否则鼠标键会卡死"
        );
    }

    #[test]
    fn drop_is_silent_when_never_pressed() {
        let log = {
            let (_drag, log) = Drag::recording();
            log
        };
        assert!(log.is_empty(), "没按下就不该有任何事件");
    }

    #[test]
    fn step_and_end_without_begin_are_explicit_errors() {
        let (mut drag, _log) = Drag::recording();
        assert_eq!(drag.step(1, 1).expect_err("未按下"), InputError::NotPressed);
        assert_eq!(drag.end().expect_err("未按下"), InputError::NotPressed);
    }

    #[test]
    fn double_begin_is_rejected() {
        let (mut drag, log) = Drag::recording();
        drag.begin(PxPoint::ZERO).expect("begin");
        assert_eq!(
            drag.begin(PxPoint::ZERO).expect_err("重复按下"),
            InputError::AlreadyPressed
        );
        drag.end().expect("end");
        assert_eq!(
            log.events().iter().filter(|e| **e == Injected::LeftDown).count(),
            1
        );
    }

    #[test]
    fn zero_delta_step_emits_nothing_but_keeps_pressed() {
        let (mut drag, log) = Drag::recording();
        drag.begin(PxPoint::ZERO).expect("begin");
        let before = log.len();
        drag.step(0, 0).expect("空步");
        assert_eq!(log.len(), before, "零位移不得产生事件");
        assert!(drag.is_pressed());
    }
}
