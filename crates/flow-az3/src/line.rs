//! 横线台 / 信号线台（`SignalLine`）：复用 `delta-flow-console` 的四列旋钮闭环，
//! 但**几何与色域都换成 AZ3 自己的**：
//! - 几何：`view.az3.line_*`（AZ3 屏幕 x 0.163–0.603），不是旧操作台的 v5 面板（x 0.05–0.53）；
//! - 色域：深蓝面板 + 暗红旋钮（旧操作台是青绿面板 + 米黄旋钮）。
//!
//! 复用而不是重写：LED 读数解析、自举增益、列超时那套状态机已经在 console 里跑通了，
//! 这里只是把「看哪里、看什么颜色」换成 AZ3 的。

use delta_capture::CaptureSession;
use delta_config::{Config, PxConsole, PxView};
use delta_flow_console::{ConsoleParams, HsvRange};
use delta_scene::Az3Kind;

use crate::Az3Outcome;

/// 定位失败时给用户的下一步动作（写在文案里，而不是让人去翻日志）。
const CALIBRATION_HINT: &str = "（可在设置→校准向导微调信号线台区域）";

/// AZ3 信号线台的像素视图：几何取自 `view.az3`，与旧操作台完全隔离。
fn az3_console_view(view: &PxView) -> PxConsole {
    PxConsole {
        led_region: view.az3.line_led_region,
        panel_region: view.az3.line_panel_region,
        col_x: view.az3.line_col_x.clone(),
        row_y: view.az3.line_row_y.clone(),
        knob_center: view.az3.line_knob_center,
    }
}

/// AZ3 信号线台色域：深蓝面板 + 暗红旋钮。
fn az3_params(config: &Config) -> ConsoleParams {
    let mut params = ConsoleParams::default().with_timings(&config.timings);
    params.knob_hsv = HsvRange::at_least(0.0, 14.0, 90.0, 70.0);
    params.panel_hsv = HsvRange {
        h_min: 95.0,
        h_max: 125.0,
        s_min: 35.0,
        s_max: 170.0,
        v_min: 25.0,
        v_max: 95.0,
    };
    params
}

/// 复用 `delta-flow-console::run` 执行 4 列对齐与「信息已校准」停手。
pub fn run(config: &Config, view: &PxView, session: &mut CaptureSession) -> Az3Outcome {
    let mut pointer = delta_flow_console::SystemPointer::new();
    let mut clock = delta_flow_console::SystemClock::new();
    let params = az3_params(config);
    let console = az3_console_view(view);
    let outcome = {
        let mut screen = delta_flow_console::SystemScreen::new(session);
        delta_flow_console::run(&mut screen, &mut pointer, &mut clock, &console, &params)
    };
    match outcome {
        Ok(result) if result.is_done() => Az3Outcome::Done {
            kind: Az3Kind::SignalLine,
            steps: 4,
            events: result.events(),
            summary: "信号线台已对齐校准".to_owned(),
        },
        Ok(result) => Az3Outcome::Aborted {
            kind: Az3Kind::SignalLine,
            reason: format!(
                "信号线台：{}{CALIBRATION_HINT}",
                result
                    .abort_reason()
                    .map_or_else(|| "已取消".to_owned(), |reason| reason.to_string())
            ),
            events: result.events(),
        },
        Err(error) => Az3Outcome::Aborted {
            kind: Az3Kind::SignalLine,
            reason: format!("信号线台：{error}{CALIBRATION_HINT}"),
            events: 0,
        },
    }
}
