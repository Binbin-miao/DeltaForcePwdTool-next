//! `SendInput` 输入注入（spec D8、D12；上游 `utils/mouse.py:56-71`）。
//!
//! 硬约束（浮光实测结论，不得"优化"）：
//!
//! 1. 游戏收 raw input，**`MOUSEEVENTF_ABSOLUTE` 的瞬移收不到** —— 拖动只能靠
//!    `MOUSEEVENTF_MOVE` 的相对增量，且每子步 ≤20px，大跳会被丢；
//!    （子步之间不再固定 sleep 2ms：靠「步长不变 + 拖后复读校正」提速，见 `drag.rs`）
//! 2. 键盘用 `KEYEVENTF_SCANCODE`，不用 VK（游戏按物理键位读）；
//! 3. spec D12：注入事件必然带 `LLMHF_INJECTED` 标记，无法伪装真实输入。
//!    本模块给所有事件的 `dwExtraInfo` 打统一标记，便于上层与真实输入区分。

mod drag;
mod inject;
mod key;

pub use drag::Drag;
pub use inject::{
    event_count, plan_click_events, plan_digit_events, reset_event_count, split_steps, Injected,
    RecordHandle, CLICK_GAP, CLICK_HOLD, INJECTION_TAG, MAX_SUBSTEP_PX,
};
pub use key::{
    key_digit, key_digits, key_space, plan_key_digits, plan_key_space, scancode_for_digit,
    SPACE_SCANCODE,
};

use delta_geom::PxPoint;
use thiserror::Error;

/// 输入注入错误。全部显式返回，不 panic、不静默。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InputError {
    #[error("SendInput 只接受了 {accepted}/{total} 个事件（可能被 UIPI 或会话隔离拦截）")]
    Rejected { accepted: u32, total: u32 },
    #[error("拖动尚未 begin 就调用了 step/end")]
    NotPressed,
    #[error("拖动已经 begin 过，不能重复按下左键")]
    AlreadyPressed,
    #[error("字符 {0:?} 不是 0~9 数字键")]
    NotADigit(char),
}

/// 单击一个点：绝对定位 → 左键按下 → 保持 [`CLICK_HOLD`] → 左键抬起。
///
/// 上下各 1 个事件，间隔与上游 `mouse.py` 的 `time.sleep(0.05)` 一致。
pub fn click(at: PxPoint) -> Result<(), InputError> {
    let events = plan_click_events(at, inject::screen_size()?);
    for event in &events {
        inject::inject(event)?;
        if matches!(event, Injected::LeftDown) {
            std::thread::sleep(CLICK_HOLD);
        }
    }
    Ok(())
}

/// 串行点击多个点，点间间隔 [`CLICK_GAP`]（spec §2.2 不变量 6：点击内含 0.05s）。
pub fn click_all(points: &[PxPoint]) -> Result<(), InputError> {
    for (index, point) in points.iter().enumerate() {
        if index > 0 {
            std::thread::sleep(CLICK_GAP);
        }
        click(*point)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::{Counter, Sink};
    use std::sync::Arc;

    /// 事件计数是 t8 断言「斜纹命中后零新增输入」的唯一依据（spec §4 操作台门禁）。
    ///
    /// 计数器按实例记账：本用例持一个私有 [`Counter`] 并在本线程绑定它，于是同进程里
    /// 另外 7 条会 emit 的用例（`drag.rs` / `inject.rs`）即使在 cargo 并行调度下交错，
    /// 也只记到全局实例，不会污染这里的绝对次数断言。
    #[test]
    fn event_counter_counts_every_emitted_event_and_can_be_reset() {
        let counter = Arc::new(Counter::new());
        let _binding = counter.bind();
        counter.reset();
        let (mut drag, _log) = Drag::recording();
        drag.begin(PxPoint { x: 10, y: 10 }).expect("begin");
        drag.step(0, 20).expect("step");
        // begin = 定位 1 + 按下 1，step = 1。
        assert_eq!(counter.count(), 3, "每个事件都必须计数一次");

        counter.reset();
        let baseline = counter.count();
        drag.end().expect("end");
        assert_eq!(baseline, 0, "reset 后基线必须归零");
        assert_eq!(counter.count(), baseline + 1, "end 之后必须还有计数");
    }

    /// 实例化隔离的可执行证据：两个线程各绑各的计数器、各记不同的次数，互不串账。
    ///
    /// 把 `charge()` 改回无条件写全局，这条会读到 `[0, 0]` 而不是 `[32, 64]`，必然失败。
    #[test]
    fn bound_counters_are_isolated_between_threads() {
        /// 单线程绑定后记 `rounds` 次事件，返回本实例读到的次数。
        fn emit_bound(counter: Arc<Counter>, rounds: u64, tag: i32) -> std::thread::JoinHandle<u64> {
            std::thread::spawn(move || {
                let _binding = counter.bind();
                let sink = Sink::Record(RecordHandle::default());
                for _ in 0..rounds {
                    sink.emit(Injected::Move { dx: 1, dy: tag })
                        .expect("记录");
                }
                counter.count()
            })
        }

        let first = Arc::new(Counter::new());
        let second = Arc::new(Counter::new());
        let first_seen = emit_bound(Arc::clone(&first), 32, 0).join().expect("线程一");
        let second_seen = emit_bound(Arc::clone(&second), 64, 1)
            .join()
            .expect("线程二");

        assert_eq!(first_seen, 32, "第一个实例只该记自己线程的 32 次");
        assert_eq!(second_seen, 64, "第二个实例只该记自己线程的 64 次");
        assert_eq!(first.count(), 32, "串账：第一个实例被邻居污染");
        assert_eq!(second.count(), 64, "串账：第二个实例被邻居污染");
    }

    #[test]
    fn click_plan_is_absolute_move_then_down_then_up() {
        let events = plan_click_events(PxPoint { x: 1920, y: 1080 }, (1920, 1080));
        assert_eq!(events.len(), 3, "定位 + 按下 + 抬起");
        assert!(matches!(events[0], Injected::MoveAbsolute { .. }));
        assert_eq!(events[1], Injected::LeftDown);
        assert_eq!(events[2], Injected::LeftUp);
        // 右下角必须落在 65535（不能越界到 65536）。
        match events[0] {
            Injected::MoveAbsolute { x, y } => {
                assert_eq!(x, 65535, "x 归一化到 16 位上限");
                assert_eq!(y, 65535, "y 归一化到 16 位上限");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn click_all_is_serial_over_plan() {
        let points = [
            PxPoint { x: 1, y: 2 },
            PxPoint { x: 3, y: 4 },
            PxPoint { x: 5, y: 6 },
        ];
        // 计划必须与点序一一对应，且每点 3 个事件。
        let mut planned = Vec::new();
        for point in points {
            planned.extend(plan_click_events(point, (100, 100)));
        }
        assert_eq!(planned.len(), 9);
        assert_eq!(planned.iter().filter(|e| **e == Injected::LeftDown).count(), 3);
    }
}
