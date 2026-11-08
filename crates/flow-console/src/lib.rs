//! 操作台旋钮小游戏的闭环代操作（spec D7 / §2.3 状态机全量 + 自举表 / §7 实测标定）。
//!
//! # 结构（Deep Module）
//!
//! 对外只有三件东西：三个 trait 缝、一套参数、一个入口 [`run`]。
//!
//! ```text
//! run(screen, pointer, clock, view, params) -> ConsoleOutcome
//!   Screen  : 抓一帧指定区域            （生产 = GDI，测试 = 脚本化假帧）
//!   Pointer : begin / step / end / click（生产 = SendInput，测试 = 记账式假指针）
//!   Clock   : now_ms / sleep_ms         （生产 = Instant，测试 = 虚拟时钟，零真实等待）
//! ```
//!
//! # 硬约束（t8 红线）
//!
//! - **禁止写死**任何像素灵敏度、方向符号、停留时长：全部来自 [`ConsoleParams`] 或运行时自举
//!   （[`Calibration`]）。
//! - **完成信号一旦命中立即停手**：`Done`/`Aborted` 由 `machine` 的单一返回点产出，之后不再发
//!   任何输入。收尾的 `LEFTUP` 属于「不许把鼠标键卡死」的安全动作，发生在返回之前。
//! - **`Aborted` 禁止自动重试**：本 crate 不提供任何重试循环，必须由用户再次触发。
//! - 本层**不**实现截图、模板匹配、输入原语，只组合下层 crate 的已有能力。

mod adapters;
mod calibration;
mod machine;
mod panel;
mod params;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_support;

pub use adapters::{SystemClock, SystemPointer, SystemScreen};
pub use calibration::{Calibration, ColumnSwitch};
pub use machine::{run, AbortReason, ConsoleOutcome, ConsoleState};
pub use panel::{analyze_panel, knob_center, knob_to_grid, KnobBlock, PanelObservation};
pub use params::{
    parse_led_sequence, rgb_to_hsv, ConsoleParams, Hsv, HsvRange, LedParseError, TargetSequence,
    CONSOLE_COLUMNS, CONSOLE_ROWS, LED_GROUP_COUNT, LED_GROUP_LEN, ROW_LABELS,
};

use delta_capture::Frame;
use delta_geom::{PxPoint, PxRect};
use thiserror::Error;

/// 抓帧缝（生产见 [`SystemScreen`]，测试见注入的假 Screen）。
pub trait Screen {
    /// 抓取 `rect` 指定的屏幕区域。失败必须带可读原因，不允许返回全黑帧冒充成功。
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String>;
}

/// 指针注入缝（生产见 [`SystemPointer`]）。
///
/// 语义与 `delta-input` 一致：`begin` 只做一次绝对归位，位移只能靠 `step` 的相对增量。
pub trait Pointer {
    fn begin(&mut self, at: PxPoint) -> Result<(), String>;
    fn step(&mut self, dx: i32, dy: i32) -> Result<(), String>;
    fn end(&mut self) -> Result<(), String>;
    /// 单击（首拖无反应时点面板中央取焦，spec §2.3）。
    fn click(&mut self, at: PxPoint) -> Result<(), String>;
    /// 累计注入事件数 —— 「完成信号命中后零后续输入」的断言依据（spec §4）。
    fn event_count(&self) -> u64;
}

/// 时钟缝：把等待与超时都变成可注入的量，测试零真实阻塞。
pub trait Clock {
    fn now_ms(&self) -> u64;
    fn sleep_ms(&mut self, ms: u64);
}

/// 启动前的配置错误（spec D9：校准缺失的必填区域必须阻止流程启动）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConsoleError {
    #[error("操作台列坐标数量 {found} ≠ {expected}，需先完成校准")]
    ColumnCount { found: usize, expected: usize },
    #[error("操作台行坐标数量 {found} ≠ {expected}，需先完成校准")]
    RowCount { found: usize, expected: usize },
    #[error("操作台{region}区域未校准（面积为 0）")]
    RegionEmpty { region: &'static str },
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    fn calibrated_view() -> delta_config::PxConsole {
        delta_config::PxConsole {
            led_region: PxRect {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
            panel_region: PxRect {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
            col_x: vec![1, 2, 3, 4],
            row_y: vec![1, 2, 3, 4],
            knob_center: PxPoint::ZERO,
        }
    }

    #[test]
    fn uncalibrated_view_is_rejected_before_any_capture() {
        let empty_panel = delta_config::PxConsole {
            panel_region: PxRect::ZERO,
            ..calibrated_view()
        };
        assert_eq!(
            machine::validate_view(&empty_panel),
            Err(ConsoleError::RegionEmpty { region: "面板" })
        );

        let empty_led = delta_config::PxConsole {
            led_region: PxRect::ZERO,
            ..calibrated_view()
        };
        assert_eq!(
            machine::validate_view(&empty_led),
            Err(ConsoleError::RegionEmpty { region: "LED" })
        );

        let short_columns = delta_config::PxConsole {
            col_x: vec![1, 2],
            ..calibrated_view()
        };
        assert_eq!(
            machine::validate_view(&short_columns),
            Err(ConsoleError::ColumnCount {
                found: 2,
                expected: CONSOLE_COLUMNS
            })
        );

        let short_rows = delta_config::PxConsole {
            row_y: vec![1, 2, 3],
            ..calibrated_view()
        };
        assert_eq!(
            machine::validate_view(&short_rows),
            Err(ConsoleError::RowCount {
                found: 3,
                expected: CONSOLE_ROWS
            })
        );

        assert_eq!(machine::validate_view(&calibrated_view()), Ok(()));
    }
}
