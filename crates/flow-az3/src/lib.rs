//! AZ3 骇入电脑 4 类小游戏统一执行入口。
//!
//! - `SafeTiming`（大保险）：5 个纵向滚动字符槽的绿色目标进入中央亮框时按空格（`0x39`）
//! - `SafePins`（任务保险）：5 个竖槽的白色游标压到目标方块时按空格（`0x39`）
//! - `ModuleKeys`（黄键台）：4×4 网格亮哪个点哪个，进度勾满 5 个即停
//! - `SignalLine`（信号线台）：每帧自适应定位 + 只上下拖动（见 [`line`]）
//! - `None`：拒绝执行，零输入
//!
//! 四条流程共享同一套「找不到特征就 Abort、绝不盲点」的纪律，像素原语见 [`geom`]。

pub mod adapters;
mod geom;
pub mod locate;
pub mod keys;
pub mod line;
pub mod pins;
pub mod timing;

use std::sync::atomic::AtomicBool;

use delta_capture::Frame;
use delta_config::PxAz3;
use delta_geom::{PxPoint, PxRect};
use delta_scene::Az3Kind;

pub use adapters::{SystemClock, SystemPointer, SystemScreen};

/// 单次任务最大实时抓帧时长上限（15s，spec U3）。
pub const AZ3_MAX_DURATION_MS: u64 = 15_000;
/// 单次任务最大实时抓帧配额（300 帧 ≈ 15s @ 20fps，spec U3）。
pub const AZ3_FRAME_BUDGET: u32 = 300;

/// 抓帧缝。
pub trait Screen {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String>;

    /// 可抓取的整帧范围。信号线台要靠整帧做自适应定位，所以必须先知道屏幕尺寸；
    /// 默认零面积 = 「本实现不支持整帧抓取」，调用方据此零输入退出（不猜）。
    fn frame_rect(&self) -> PxRect {
        PxRect::ZERO
    }
}

/// 输入注入缝（点击 + 空格键 `0x39` + 分步纵拖）。
///
/// `begin/step/end` 是信号线台「按住左键上下拖」的三个原语，语义与 `delta-input`
/// 一致：`begin` 只做一次绝对归位，位移只能靠 `step` 的相对增量；
/// 成对调用必须由调用方在 `finally` 式路径上保证 `end` 一定执行（见 `line::drag`）。
pub trait Pointer {
    fn click(&mut self, at: PxPoint) -> Result<(), String>;
    fn key_space(&mut self) -> Result<(), String>;
    /// 归位到 `at` 并按下左键。
    fn begin(&mut self, at: PxPoint) -> Result<(), String>;
    /// 相对位移一步（向下为正）。
    fn step(&mut self, dx: i32, dy: i32) -> Result<(), String>;
    /// 抬起左键。
    fn end(&mut self) -> Result<(), String>;
    fn event_count(&self) -> u64;
}

/// 时钟缝（零真实阻塞单测）。
pub trait Clock {
    fn now_ms(&self) -> u64;
    fn sleep_ms(&mut self, ms: u64);
}

/// 实机屏幕相对 720p 的放大倍数（1080p → 1.5，1440p → 2.0）。
///
/// 大保险 / 任务保险只抓**小区域**，区域帧不能按自身高度归一化（那会放大到 720）。
/// 它们需要的是这个系数：把「720p 实测的像素阈值」乘上去，才对得上实机画面。
/// 拿不到整帧范围时退化为 1.0（按 720p 处理，绝不会因为测量失败而放大阈值）。
pub fn screen_scale(screen: &impl Screen) -> f32 {
    let full = screen.frame_rect();
    if full.h <= 0 {
        return 1.0;
    }
    (full.h as f32 / delta_scene::locate::REFERENCE_HEIGHT as f32).max(1.0)
}

/// 长度/坐标按屏幕倍数放大。
pub fn scaled_len(value: i32, scale: f32) -> i32 {
    if scale <= 1.0 {
        return value;
    }
    (value as f32 * scale).round() as i32
}

/// 像素面积按屏幕倍数的**平方**放大（面积是二维量）。
pub fn scaled_area(value: u32, scale: f32) -> u32 {
    if scale <= 1.0 {
        return value;
    }
    (value as f32 * scale * scale).round() as u32
}

/// AZ3 流程统一返回结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Az3Outcome {
    Done {
        kind: Az3Kind,
        steps: usize,
        events: u64,
        summary: String,
        /// 完成是否**有进度证据**（`true` = 判据直接读到了目标状态，例如黄键台 5/5 全勾）。
        ///
        /// 黄键台的退化档（进度格读不出，按「在册待点键点完」收尾）一律 `false`：
        /// 见过 2 颗亮键、点完 2 颗 != 板子上没有剩余（用户实测就是这么漏 3 颗的）。
        /// 调用方据此分档留证（`_ok` / `_unverified`）并区分文案，**不许**把它当成功播报。
        /// 大保险 / 任务保险 / 信号线台的判据本身就是确定读数，恒为 `true`。
        confirmed: bool,
    },
    Aborted {
        kind: Az3Kind,
        reason: String,
        events: u64,
    },
}

impl Az3Outcome {
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done { .. })
    }

    /// 这条结论有没有进度证据；`Aborted` 恒为 `false`（它本来就不是完成）。
    pub fn is_confirmed(&self) -> bool {
        matches!(self, Self::Done { confirmed: true, .. })
    }

    pub fn kind(&self) -> Az3Kind {
        match self {
            Self::Done { kind, .. } | Self::Aborted { kind, .. } => *kind,
        }
    }

    pub fn events(&self) -> u64 {
        match self {
            Self::Done { events, .. } | Self::Aborted { events, .. } => *events,
        }
    }
}

/// 统一调度入口（`SignalLine` 走 [`line::run`]，其余三类均可通过 mock 单测）。
pub fn run(
    kind: Az3Kind,
    screen: &mut impl Screen,
    pointer: &mut impl Pointer,
    clock: &mut impl Clock,
    az3: &PxAz3,
    cancel: &AtomicBool,
) -> Az3Outcome {
    match kind {
        Az3Kind::SafeTiming => timing::run(screen, pointer, clock, az3, cancel),
        Az3Kind::SafePins => pins::run(screen, pointer, clock, az3, cancel),
        Az3Kind::ModuleKeys => keys::run(screen, pointer, clock, az3, cancel),
        Az3Kind::SignalLine => Az3Outcome::Aborted {
            kind: Az3Kind::SignalLine,
            reason: "信号线台请通过 line::run 调用操作台闭环".to_owned(),
            events: pointer.event_count(),
        },
        Az3Kind::None => Az3Outcome::Aborted {
            kind: Az3Kind::None,
            reason: "未检测到 AZ3 游戏界面，拒绝执行".to_owned(),
            events: pointer.event_count(),
        },
    }
}

/// 跨模块共用的测试夹具：四条流程与信号线台的单测都用同一份假几何
/// （100% 脱敏：这是一份与任何真实机位无关的合成坐标）。
#[cfg(test)]
pub(crate) mod tests_support {
    use delta_config::PxAz3;
    use delta_geom::{PxPoint, PxRect};

    fn rect(x: i32, y: i32, w: i32, h: i32) -> PxRect {
        PxRect { x, y, w, h }
    }

    pub(crate) fn az3() -> PxAz3 {
        PxAz3 {
            grid_region: rect(288, 257, 371, 143),
            target_band: rect(288, 317, 371, 86),
            crack_button: rect(768, 238, 179, 173),
            timing_window_px: 23,
            track_band: rect(365, 155, 531, 76),
            indicator_band: rect(365, 216, 531, 86),
            pin_center_x: vec![416, 527, 634, 739, 845],
            pin_tolerance_px: 15,
            button_grid_region: rect(640, 194, 269, 234),
            progress_region: rect(335, 315, 198, 90),
            line_led_region: rect(809, 175, 288, 54),
            line_panel_region: rect(209, 184, 563, 317),
            line_col_x: vec![339, 465, 599, 698],
            line_row_y: vec![248, 292, 340, 389],
            line_knob_center: PxPoint { x: 941, y: 407 },
            max_duration_ms: 15_000,
            frame_budget: 300,
            poll_interval_ms: 50,
            ..PxAz3::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // ── 合成帧与假缝（全部为 100% 脱敏假数据）──

    const TARGET_W: u32 = 371;
    const TARGET_H: u32 = 86;

    /// 高亮带底色：半透明绿罩，够 `is_green_glow` 但不够「亮绿」。
    const BAND_GREEN: [u8; 3] = [60, 150, 60];
    /// 游标：亮绿。
    const CURSOR_GREEN: [u8; 3] = [80, 240, 90];
    /// 按错后整条变红。
    const FAIL_RED: [u8; 3] = [200, 60, 60];
    const WHITE: [u8; 3] = [240, 240, 240];
    const LIT_YELLOW: [u8; 3] = [235, 200, 40];
    const LOCK_GREEN: [u8; 3] = [40, 210, 60];

    /// 顺序脚本：播完停在最后一帧（等价于「画面静止」），便于断言"之后不再触发"。
    struct ScriptScreen {
        frames: Vec<Frame>,
        cursor: usize,
    }

    impl ScriptScreen {
        fn new(frames: Vec<Frame>) -> Self {
            Self { frames, cursor: 0 }
        }
    }

    impl Screen for ScriptScreen {
        fn grab(&mut self, _rect: PxRect) -> Result<Frame, String> {
            if self.frames.is_empty() {
                return Err("脚本帧为空".to_owned());
            }
            let last = self.frames.len() - 1;
            let frame = self.frames[self.cursor.min(last)].clone();
            self.cursor += 1;
            Ok(frame)
        }

        fn frame_rect(&self) -> PxRect {
            self.frames.first().map_or(PxRect::ZERO, |frame| PxRect {
                x: 0,
                y: 0,
                w: frame.w as i32,
                h: frame.h as i32,
            })
        }
    }

    /// 黄键台专用：整帧按脚本推进（面板几何恒定，只有亮键与进度框在变）。
    struct PanelScreen {
        rect: PxRect,
        frames: Vec<Frame>,
        pulled: usize,
    }

    impl PanelScreen {
        fn new(frames: Vec<Frame>) -> Self {
            let rect = frames.first().map_or(PxRect::ZERO, |frame| PxRect {
                x: 0,
                y: 0,
                w: frame.w as i32,
                h: frame.h as i32,
            });
            Self {
                rect,
                frames,
                pulled: 0,
            }
        }
    }

    impl Screen for PanelScreen {
        fn grab(&mut self, _rect: PxRect) -> Result<Frame, String> {
            let last = self.frames.len().saturating_sub(1);
            let frame = self.frames.get(self.pulled.min(last)).cloned();
            self.pulled += 1;
            frame.ok_or_else(|| "脚本帧为空".to_owned())
        }

        fn frame_rect(&self) -> PxRect {
            self.rect
        }
    }

    #[derive(Default)]
    struct MockPointer {
        clicks: Vec<PxPoint>,
        spaces: usize,
        events: u64,
    }

    impl Pointer for MockPointer {
        fn click(&mut self, at: PxPoint) -> Result<(), String> {
            self.clicks.push(at);
            self.events += 2;
            Ok(())
        }

        fn key_space(&mut self) -> Result<(), String> {
            self.spaces += 1;
            self.events += 2;
            Ok(())
        }

        fn begin(&mut self, _at: PxPoint) -> Result<(), String> {
            self.events += 1;
            Ok(())
        }

        fn step(&mut self, _dx: i32, _dy: i32) -> Result<(), String> {
            self.events += 1;
            Ok(())
        }

        fn end(&mut self) -> Result<(), String> {
            self.events += 1;
            Ok(())
        }

        fn event_count(&self) -> u64 {
            self.events
        }
    }

    struct MockClock {
        now: u64,
    }

    impl Clock for MockClock {
        fn now_ms(&self) -> u64 {
            self.now
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.now += ms;
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> PxRect {
        PxRect { x, y, w, h }
    }

    fn paint_frame(w: u32, h: u32, rects: &[(PxRect, [u8; 3])]) -> Frame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for (r, rgb) in rects {
            for y in r.y.max(0)..r.bottom().min(h as i32) {
                for x in r.x.max(0)..r.right().min(w as i32) {
                    let i = ((y as u32 * w + x as u32) * 4) as usize;
                    data[i] = rgb[2];
                    data[i + 1] = rgb[1];
                    data[i + 2] = rgb[0];
                    data[i + 3] = 255;
                }
            }
        }
        Frame::new(w, h, data, Instant::now()).expect("合法合成帧")
    }

    /// 大保险帧：第 `col` 列（0..5）中央亮框内出现绿色目标字符。
    fn timing_frame(col: u32) -> Frame {
        let col_w = TARGET_W as i32 / 5;
        let x0 = col as i32 * col_w + col_w / 4;
        paint_frame(
            TARGET_W,
            TARGET_H,
            &[(rect(x0, 20, (col_w / 2).max(8), 40), CURSOR_GREEN)],
        )
    }

    /// 按错后的整条红带。
    fn failed_timing_frame() -> Frame {
        paint_frame(TARGET_W, TARGET_H, &[(rect(0, 20, TARGET_W as i32, 40), FAIL_RED)])
    }

    // ── 任务保险合成帧（100% 脱敏：只复刻相对结构）──

    /// 整帧尺寸：轨道与底部指示条在**同一帧**里（执行层每帧只抓一次整帧）。
    const PINS_W: u32 = 1280;
    const PINS_H: u32 = 720;
    /// 轨道：横贯画面的中灰长条（实机 720p 实测 x300..1050 / y188..202 / 高 15）。
    const PINS_TRACK: PxRect = PxRect {
        x: 300,
        y: 188,
        w: 750,
        h: 15,
    };
    /// 轨道底色：中灰（实测行均值 67；指示条的绿条是饱和绿，两回事）。
    const TRACK_GREY: [u8; 3] = [67, 67, 67];
    /// 5 个目标方块的中心 x（间距 ≈105px，与实机同量级）。
    const PIN_TARGET_XS: [i32; 5] = [413, 527, 634, 739, 845];

    /// 任务保险整帧：轨道 + 每个目标一块「宽扁目标方块」+ 一根按 `offsets` 偏移的「窄高游标」
    /// + 底部指示条前 `locked` 格绿条。
    ///
    /// `offsets[i]` = 第 i 根游标相对它目标方块中心的水平偏移（`0` = 正好压在目标上）。
    fn pins_frame(offsets: &[i32], locked: usize) -> Frame {
        let mut shapes = vec![(PINS_TRACK, TRACK_GREY)];
        for (index, &center_x) in PIN_TARGET_XS.iter().enumerate() {
            shapes.push((rect(center_x - 5, 187, 10, 17), WHITE));
            let offset = offsets.get(index).copied().unwrap_or(0);
            // 游标纵向伸出轨道带（实测 h 20~30 > 带高 15）—— 这正是「游标」的判据本体。
            shapes.push((rect(center_x + offset - 2, 182, 5, 27), WHITE));
        }
        let band = test_az3().indicator_band;
        for slot in 0..locked.min(5) {
            shapes.push((
                rect(band.x + slot as i32 * 106, band.y + 20, 60, 30),
                LOCK_GREEN,
            ));
        }
        paint_frame(PINS_W, PINS_H, &shapes)
    }

    // ── 黄键台合成面板（100% 脱敏：与任何真实机位无关，只复刻相对结构）──

    const PANEL_W: u32 = 480;
    const PANEL_H: u32 = 320;
    /// 金属台面：亮灰 —— 键必须比台面暗，暗键阵才数得出来。
    const PANEL_GREY: [u8; 3] = [152, 152, 150];
    /// 屏幕内部：近黑。
    const SCREEN_DARK: [u8; 3] = [30, 32, 34];
    /// 未点亮的键面。
    const DARK_KEY: [u8; 3] = [36, 38, 40];
    /// 进度框的浅色细边框。
    const BOX_BORDER: [u8; 3] = [200, 200, 200];
    const PANEL_SCREEN: PxRect = PxRect {
        x: 40,
        y: 60,
        w: 160,
        h: 140,
    };
    const PANEL_KEY_SIZE: (i32, i32) = (34, 30);
    const PANEL_COL_X: [i32; 4] = [240, 284, 328, 372];
    const PANEL_ROW_Y: [i32; 4] = [50, 96, 142, 188];
    const PANEL_BOX_SIZE: (i32, i32) = (24, 18);
    const PANEL_BOX_X: [i32; 5] = [48, 76, 104, 132, 160];
    const PANEL_BOX_Y: i32 = 132;

    fn panel_key(index: usize) -> PxRect {
        rect(
            PANEL_COL_X[index % 4],
            PANEL_ROW_Y[index / 4],
            PANEL_KEY_SIZE.0,
            PANEL_KEY_SIZE.1,
        )
    }

    /// 第 `index` 颗键的几何中心（点击落点）。
    fn panel_key_center(index: usize) -> PxPoint {
        let key = panel_key(index);
        PxPoint {
            x: key.x + key.w / 2,
            y: key.y + key.h / 2,
        }
    }

    fn panel_box(stage: usize) -> PxRect {
        rect(PANEL_BOX_X[stage], PANEL_BOX_Y, PANEL_BOX_SIZE.0, PANEL_BOX_SIZE.1)
    }

    /// 一帧完整黄键台：金属台面 + 左侧屏幕 + 16 颗暗键 + 5 个进度框。
    ///
    /// `lit` 里的键在键面上多一块发光图标（键本身仍是暗块）；`checked` 个进度框内部变绿。
    /// 同一个构造函数覆盖「只亮一格」「同一格一直亮」「16 格全亮」「进度推进」全部状态。
    fn panel_frame(lit: &[usize], checked: usize, keys: usize, boxes: bool) -> Frame {
        let mut shapes = vec![(rect(0, 0, PANEL_W as i32, PANEL_H as i32), PANEL_GREY)];
        shapes.push((PANEL_SCREEN, SCREEN_DARK));
        if boxes {
            for stage in 0..PANEL_BOX_X.len() {
                let frame = panel_box(stage);
                if stage < checked {
                    shapes.push((
                        rect(frame.x + 2, frame.y + 2, frame.w - 4, frame.h - 4),
                        LOCK_GREEN,
                    ));
                }
                push_ring(&mut shapes, frame, 2, BOX_BORDER);
            }
        }
        for index in 0..keys.min(16) {
            let key = panel_key(index);
            shapes.push((key, DARK_KEY));
            if lit.contains(&index) {
                shapes.push((
                    rect(key.x + key.w / 4, key.y + key.h / 4, key.w / 2, key.h / 2),
                    LIT_YELLOW,
                ));
            }
        }
        paint_frame(PANEL_W, PANEL_H, &shapes)
    }

    /// 空心细边框（进度框就是这种）。
    fn push_ring(shapes: &mut Vec<(PxRect, [u8; 3])>, frame: PxRect, thickness: i32, rgb: [u8; 3]) {
        shapes.push((rect(frame.x, frame.y, frame.w, thickness), rgb));
        shapes.push((
            rect(frame.x, frame.bottom() - thickness, frame.w, thickness),
            rgb,
        ));
        shapes.push((rect(frame.x, frame.y, thickness, frame.h), rgb));
        shapes.push((
            rect(frame.right() - thickness, frame.y, thickness, frame.h),
            rgb,
        ));
    }

    /// 空白画面：看不到任何面板。
    fn blank_panel_frame() -> Frame {
        paint_frame(
            PANEL_W,
            PANEL_H,
            &[(rect(0, 0, PANEL_W as i32, PANEL_H as i32), PANEL_GREY)],
        )
    }

    fn test_az3() -> PxAz3 {
        tests_support::az3()
    }

    fn run_with(
        kind: Az3Kind,
        screen: &mut impl Screen,
        pointer: &mut MockPointer,
    ) -> Az3Outcome {
        let az3 = test_az3();
        let mut clock = MockClock { now: 0 };
        let cancel = AtomicBool::new(false);
        run(kind, screen, pointer, &mut clock, &az3, &cancel)
    }

    fn abort_reason(outcome: &Az3Outcome) -> &str {
        match outcome {
            Az3Outcome::Aborted { reason, .. } => reason,
            Az3Outcome::Done { summary, .. } => summary,
        }
    }

    // ── 调度层 ──

    #[test]
    fn none_kind_rejects_with_zero_input() {
        let mut screen = ScriptScreen::new(vec![]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::None, &mut screen, &mut pointer);
        assert!(!outcome.is_done());
        assert_eq!(pointer.events, 0);
        assert_eq!(pointer.spaces, 0);
        assert!(pointer.clicks.is_empty());
        assert!(abort_reason(&outcome).contains("拒绝执行"));
    }

    #[test]
    fn signal_line_kind_requires_the_console_loop() {
        let mut screen = ScriptScreen::new(vec![]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SignalLine, &mut screen, &mut pointer);
        assert_eq!(outcome.kind(), Az3Kind::SignalLine);
        assert!(abort_reason(&outcome).contains("line::run"));
        assert_eq!(pointer.events, 0);
    }

    // ── 大保险 ──

    #[test]
    fn safe_timing_presses_once_per_column_and_finishes_after_five_chars() {
        // 5 列依次有绿色目标进入中央亮框，每列间插空帧模拟滚动离开，末尾目标消失确认完成。
        let mut frames = Vec::new();
        for col in 0..5 {
            frames.push(timing_frame(col));
            frames.push(paint_frame(TARGET_W, TARGET_H, &[]));
        }
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafeTiming, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "{outcome:?}");
        assert!(outcome.is_confirmed(), "面板已关闭 = 有终态证据：{outcome:?}");
        assert_eq!(pointer.spaces, 5);
    }

    #[test]
    fn safe_timing_five_presses_without_the_panel_closing_stay_unconfirmed() {
        let mut frames = Vec::new();
        for col in 0..5 {
            frames.push(paint_frame(TARGET_W, TARGET_H, &[]));
            frames.push(timing_frame(col));
        }
        // 最后一帧仍然亮绿，未消失
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafeTiming, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "按满 5 个仍然是「跑完了」，不是中断：{outcome:?}");
        assert!(
            !outcome.is_confirmed(),
            "目标没消失就不能报确认完成：{outcome:?}"
        );
        assert_eq!(pointer.spaces, 5);
    }

    #[test]
    fn safe_timing_does_not_repeat_press_while_staying_on_same_column() {
        // 同一列绿色目标停在中央不动 → 只能触发一次空格，随后触发静态误判保护。
        let mut screen = ScriptScreen::new(vec![timing_frame(0)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafeTiming, &mut screen, &mut pointer);
        assert_eq!(pointer.spaces, 1, "{outcome:?}");
        assert!(!outcome.is_done());
        assert!(abort_reason(&outcome).contains("疑似误判"), "{outcome:?}");
    }

    #[test]
    fn safe_timing_aborts_when_no_target_appears() {
        let mut screen = ScriptScreen::new(vec![paint_frame(TARGET_W, TARGET_H, &[])]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafeTiming, &mut screen, &mut pointer);
        assert!(!outcome.is_done());
        assert!(abort_reason(&outcome).contains("大保险超时"), "{outcome:?}");
        assert_eq!(pointer.spaces, 0);
    }

    #[test]
    fn safe_timing_aborts_immediately_when_band_turns_red() {
        let mut screen = ScriptScreen::new(vec![failed_timing_frame()]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafeTiming, &mut screen, &mut pointer);
        assert!(!outcome.is_done());
        assert!(abort_reason(&outcome).contains("整条已变红"), "{outcome:?}");
        assert_eq!(pointer.spaces, 0);
    }

    // ── 任务保险（每帧量几何：轨道 + 目标方块 + 游标）──

    /// 任务保险的开场：轨道先静止 3 帧（与 `pins.rs` 的 `PIN_STABLE_FRAMES` 同口径）。
    ///
    /// 真面板就是「镜头停下、板子定住」之后才开始对准的；测试照抄这个先后顺序，
    /// 而不是把「已经稳住了」这件事偷偷当成默认前提。
    fn settled_pins(offsets: &[i32], locked: usize) -> Vec<Frame> {
        (0..3).map(|_| pins_frame(offsets, locked)).collect()
    }

    #[test]
    fn safe_pins_presses_when_a_cursor_meets_its_target_and_finishes_all_locked() {
        // 轨道先静止 3 帧（镜头停稳）→ 5 根游标都偏开 40px（预武装）→ 第 1 根压到目标上
        // ⇒ 按 1 次空格；指示条随后 5 格全绿 ⇒ Done 且**有进度证据**。
        let mut frames = settled_pins(&[40; 5], 0);
        frames.push(pins_frame(&[0, 40, 40, 40, 40], 0));
        frames.push(pins_frame(&[0, 40, 40, 40, 40], 5));
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "{outcome:?}");
        assert!(outcome.is_confirmed(), "指示条 5 格全绿 = 有进度证据：{outcome:?}");
        assert_eq!(pointer.spaces, 1, "{outcome:?}");
    }

    #[test]
    fn safe_pins_keeps_zero_input_while_the_frame_is_still_moving() {
        // 跑图：镜头一直在推，整块场景（轨道 + 目标 + 游标）每帧横向滑 15px。
        // 第 2 帧起第 1 根游标就「正好压在目标上」，但画面还在动 ⇒ 一格都不许按。
        // 反例：只看「离开过容差区」⇒ 镜头推过去的那一下就会被当成对准，空格 = 跳跃。
        let mut frames: Vec<Frame> = Vec::new();
        for step in 0..10i32 {
            let pan = step * 15;
            let mut shapes = vec![(
                PxRect {
                    x: PINS_TRACK.x + pan,
                    ..PINS_TRACK
                },
                TRACK_GREY,
            )];
            for (index, &center_x) in PIN_TARGET_XS.iter().enumerate() {
                let x = center_x + pan;
                shapes.push((rect(x - 5, 187, 10, 17), WHITE));
                // 第 1 根游标先偏开 40px（预武装），之后每帧都压在目标上。
                let offset = if index == 0 && step == 0 { 40 } else { 0 };
                shapes.push((rect(x + offset - 2, 182, 5, 27), WHITE));
            }
            frames.push(paint_frame(PINS_W, PINS_H, &shapes));
        }
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert_eq!(
            pointer.spaces, 0,
            "画面还在推的时候一格都不许按：{outcome:?}"
        );
        assert_eq!(pointer.events, 0, "零输入：{outcome:?}");
        assert!(!outcome.is_done(), "{outcome:?}");
    }

    #[test]
    fn safe_pins_does_not_press_on_the_very_first_frame_already_aligned() {
        // 首帧就对齐 = 还没「离开过容差区」，不许直接按（会按在早一格的时机上）。
        // 反例：去掉预武装 ⇒ 这里会看到 5 次空格（5 根游标全在对齐位）。
        let mut screen = ScriptScreen::new(vec![pins_frame(&[0, 0, 0, 0, 0], 0)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert_eq!(pointer.spaces, 0, "{outcome:?}");
        assert!(!outcome.is_done());
    }

    #[test]
    fn safe_pins_judges_each_pair_independently() {
        // 只有第 1、4 根游标压在自己的目标上（其余仍偏开 40px）⇒ 同一帧只按这 2 对。
        // 反例：只盯「第一个未锁定槽」的旧实现会连按同一对，或对没对齐的也按。
        let mut frames = settled_pins(&[40; 5], 0);
        frames.push(pins_frame(&[0, 40, 40, 0, 40], 0));
        frames.push(pins_frame(&[0, 40, 40, 0, 40], 5));
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.spaces, 2, "只有 2 对对齐 ⇒ 只按 2 次：{outcome:?}");
    }

    #[test]
    fn safe_pins_does_not_repeat_press_for_the_same_target() {
        // 预武装一次后游标一直压在目标上、指示条始终不亮 ⇒ 同一个目标只按一次。
        let mut frames = settled_pins(&[40; 5], 0);
        frames.push(pins_frame(&[0, 40, 40, 40, 40], 0));
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert_eq!(pointer.spaces, 1, "{outcome:?}");
        assert!(
            !outcome.is_confirmed(),
            "指示条一格不亮时绝不许报「确认完成」：{outcome:?}"
        );
    }

    #[test]
    fn safe_pins_reports_unpaired_tracks_instead_of_guessing() {
        // 只有目标白块、游标还没出来 ⇒ 一对也配不出来 ⇒ 一次都不按，收尾文案要说清「未配对」。
        // 反例：配不上就瞎按，或把「没配对」当成「已完成」。
        //
        // 用白块（而不是「只有游标」）造这个局面：轨道必须有亮线证据
        // （`PIN_TRACK_BRIGHT_MIN`），5 根 5px 宽的细游标只有 3.3% 亮像素，量不出轨道；
        // 而 5 块 10px 宽的白块有 6.7%，正是实机「板在、游标还没滑过来」的样子。
        let targets_only: Vec<(PxRect, [u8; 3])> = PIN_TARGET_XS
            .iter()
            .map(|&cx| (rect(cx - 5, 187, 10, 17), WHITE))
            .collect();
        let mut shapes = vec![(PINS_TRACK, TRACK_GREY)];
        shapes.extend(targets_only);
        let mut screen = ScriptScreen::new(vec![paint_frame(PINS_W, PINS_H, &shapes)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.spaces, 0, "没有目标位就零输入：{outcome:?}");
        assert!(
            abort_reason(&outcome).contains("未能配对到目标"),
            "必须说清卡在「配不上目标」：{outcome:?}"
        );
    }

    #[test]
    fn safe_pins_aborts_with_zero_input_when_there_is_no_track_at_all() {
        // 空白画面（没有任何中灰长条）⇒ 量不出轨道 ⇒ 连败后零输入退出。
        let mut screen = ScriptScreen::new(vec![paint_frame(PINS_W, PINS_H, &[])]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.events, 0, "定位失败必须零输入：{outcome:?}");
        assert!(
            abort_reason(&outcome).contains("轨道"),
            "文案要点明「没找到轨道」：{outcome:?}"
        );
    }

    #[test]
    fn safe_pins_stops_after_five_presses_when_no_slot_ever_locks() {
        // 误判界面：每帧都有 1 对「对齐」，但指示条一格都不亮（真面板每按一次必然锁一格）。
        // 空格的游戏内语义是跳跃 —— 必须在第 5 次按键后**立刻**停手，而不是按到超时。
        // 反例：没有护栏一 ⇒ 一直按到 frame_budget（实测同款日志按了 17 / 48 次）⇒ 红。
        // 轨道先静止 3 帧，之后每帧换一对「对齐」（真面板也是一路往下走，不会回头）。
        let mut frames = settled_pins(&[40; 5], 0);
        let steps: [&[i32]; 5] = [
            &[0, 40, 40, 40, 40],  // 第 1 次
            &[40, 0, 40, 40, 40],  // 第 2 次
            &[40, 40, 0, 40, 40],  // 第 3 次
            &[40, 40, 40, 0, 40],  // 第 4 次
            &[40, 40, 40, 40, 0],  // 第 5 次 → 立刻 Abort
        ];
        for step in steps {
            frames.push(pins_frame(step, 0));
        }
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.spaces, 5, "必须正好按 5 次就停手：{outcome:?}");
        assert_eq!(pointer.events, 10, "5 次空格 = 10 个输入事件：{outcome:?}");
        assert!(
            abort_reason(&outcome).contains("没有任何槽锁定"),
            "文案要点明「0 槽锁定」：{outcome:?}"
        );
    }

    #[test]
    fn safe_pins_never_exceeds_double_the_slot_count_even_if_one_slot_reads_locked() {
        // 护栏二：指示条偶尔读到 1 格绿（护栏一因此不触发），按键次数仍必须被硬上限截断。
        // 5 根游标每轮全部对齐 ⇒ 一帧按 5 次；第 3 轮的第 1 次（第 11 次）越过上限就 Abort。
        // 反例：只写护栏一 ⇒ 这里会一直按到 frame_budget ⇒ 红。
        let mut frames = settled_pins(&[40; 5], 1);
        for _ in 0..3 {
            frames.push(pins_frame(&[0, 0, 0, 0, 0], 1));
            frames.push(pins_frame(&[40, 40, 40, 40, 40], 1));
        }
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.spaces, 11, "越过上限的那一次就停：{outcome:?}");
        assert!(
            abort_reason(&outcome).contains("超过上限"),
            "文案要点明「按键次数超过上限」：{outcome:?}"
        );
    }

    // ── 黄键台（每帧自适应定位）──

    /// 面板恒定、只有发光键与进度框按脚本变化的整帧假屏幕。
    fn panel_screen(frames: Vec<Frame>) -> PanelScreen {
        PanelScreen::new(frames)
    }

    #[test]
    fn module_keys_clicks_the_lit_key_centre_and_finishes_at_five_checks() {
        // 进度 4/5 + 第 9 颗键发光 → 点它的几何中心一次 → 进度 5/5 → Done。
        let mut screen = panel_screen(vec![
            panel_frame(&[9], 4, 16, true),
            panel_frame(&[9], 5, 16, true),
        ]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.clicks, vec![panel_key_center(9)], "{outcome:?}");
    }

    #[test]
    fn module_keys_aborts_with_zero_input_when_no_panel_is_visible() {
        // 空白画面：定位连败 LOCATE_FAIL_LIMIT 帧后零输入退出，并给出「正对面板」的提示。
        let mut screen = panel_screen(vec![blank_panel_frame()]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.events, 0, "定位失败必须零输入");
        assert!(pointer.clicks.is_empty());
        assert!(abort_reason(&outcome).contains("4×4 按键阵"), "{outcome:?}");
        assert!(abort_reason(&outcome).contains("请正对按键面板"), "{outcome:?}");
    }

    #[test]
    fn module_keys_retries_a_stuck_key_until_the_strike_limit() {
        // 同一颗键一直亮、进度永不推进 → 点满 DEAD_STRIKES 轮（spec A2：2→4）才判死，
        // 判死后立刻按「1 键已点完但进度未满」收尾，不许无限重试。
        // 反例：`DEAD_STRIKES = 2` 时这里只会看到 2 次点击。
        let mut screen = panel_screen(vec![panel_frame(&[3], 0, 16, true)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(
            pointer.clicks,
            vec![panel_key_center(3); usize::from(keys::DEAD_STRIKES)],
            "同一颗键必须重试到判死线才放弃：{outcome:?}"
        );
    }

    #[test]
    fn module_keys_clicks_every_lit_button_and_never_the_progress_row() {
        // 用户拍板：同一帧亮着的模块按钮**全都要点**，不要求顺序（实测同亮 5 个）。
        // 屏幕内那 5 格是只读进度指示器 —— 任何一次点击都不许落在它们身上。
        let lit = [1usize, 4, 6, 11, 14];
        let mut screen = panel_screen(vec![panel_frame(&lit, 0, 16, true)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");
        for &index in &lit {
            assert!(
                pointer.clicks.contains(&panel_key_center(index)),
                "第 {index} 颗亮键必须被点：{:?}",
                pointer.clicks
            );
        }
        for stage in 0..5 {
            let box_ = panel_box(stage);
            let hit = pointer.clicks.iter().any(|point| {
                point.x >= box_.x
                    && point.x < box_.right()
                    && point.y >= box_.y
                    && point.y < box_.bottom()
            });
            assert!(!hit, "第 {stage} 个进度格只读，绝不许点击：{:?}", pointer.clicks);
        }
    }

    #[test]
    fn the_input_cap_leaves_room_for_a_full_sixteen_by_dead_strikes_retry() {
        // P8：跨常量约束（LESSON-007：两个各自合理的数字相乘会变死代码）。
        // `16 键 × DEAD_STRIKES(4) × 2 事件` = 128 是**合法**的最坏用量，正常档安全阀必须装得下。
        // 反例：把 `AZ3_MAX_INPUT_EVENTS` 改回 120 ⇒ 128 > 120 ⇒ 这条红。
        let cap = u64::from(delta_config::AZ3_MAX_INPUT_EVENTS);
        let worst_case = 16 * u64::from(keys::DEAD_STRIKES) * 2;
        assert!(
            worst_case <= cap,
            "合法最坏用量 {worst_case} 必须 ≤ 安全阀 {cap}，否则「重试到底」会被报成「疑似误判」"
        );
        // 抬高的同时不能失去安全网意义：实测误判乱点是 344/330/370 个事件。
        let misjudged = 344u64;
        assert!(cap < misjudged, "安全阀 {cap} 仍必须低于实测误判乱点 {misjudged}");
    }

    #[test]
    fn module_keys_spends_all_sixteen_keys_without_hitting_the_input_cap() {
        // 16 格全亮、进度始终 0：每格重试满 DEAD_STRIKES 轮才判死，共 16 × 4 = 64 次点击
        // = **128 个输入事件**。这个量必须**装得进**正常档安全阀（现 160）：
        // 它是「这一局没解开」的合法收尾，不该被误报成「疑似误判（多半不是键台）」。
        // LESSON-007 的跨常量冲突：`DEAD_STRIKES` 从 2 提到 4 后 128 > 旧的 120，
        // 于是上限跟着抬到 160 —— 这条断言就是钉住那个不等式。
        let mut screen = panel_screen(vec![panel_frame(&(0..16).collect::<Vec<_>>(), 0, 16, true)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");

        let cap = u64::from(delta_config::AZ3_MAX_INPUT_EVENTS);
        let worst_case = 16 * u64::from(keys::DEAD_STRIKES) * 2;
        assert!(
            worst_case <= cap,
            "跨常量约束：16 键 × DEAD_STRIKES × 2 事件 = {worst_case} 必须 ≤ 安全阀 {cap}，\
             否则「正常重试到底」会被报成「疑似误判」"
        );
        assert!(
            abort_reason(&outcome).contains("已点完但进度未满"),
            "合法收尾必须报「点完了但没解开」，不许报成误判：{outcome:?}"
        );
        assert_eq!(
            pointer.events, worst_case,
            "16 键各点满 DEAD_STRIKES 轮 = {worst_case} 个事件：{outcome:?}"
        );
        let mut centres: Vec<(i32, i32)> = pointer.clicks.iter().map(|p| (p.x, p.y)).collect();
        centres.sort_unstable();
        centres.dedup();
        assert_eq!(centres.len(), 16, "每一次点击都必须落在 16 颗键的中心上");
    }

    #[test]
    fn module_keys_acks_on_darkness_so_the_next_key_is_clicked_without_delay() {
        // 键 0 亮一帧就灭、紧跟着键 1 亮。**亮光熄灭就是最可靠的 ack**：
        // 实机进度框读不出来（屏幕 luma 中位数 48，5 个格子亮度差仅 5~10，
        // 任何绝对阈值都分不开），而「点对了游戏就收掉高亮」这条观测永远存在。
        //
        // 旧实现只认「进度推进」：熄灭被当成「点了没反应」，必须等满 400ms 才判死放行；
        // 那 400ms 里键 1 已经亮完又灭了 —— 表现为「明明是亮的，程序却一动不动」。
        let mut frames = vec![
            panel_frame(&[0], 0, 16, true),
            panel_frame(&[], 0, 16, true),
            panel_frame(&[1], 0, 16, true),
            panel_frame(&[1], 0, 16, true),
            panel_frame(&[], 0, 16, true),
        ];
        frames.push(panel_frame(&[], 0, 16, true));
        let mut screen = panel_screen(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(
            pointer.clicks.len() >= 2,
            "键 0 熄灭后必须立刻能点键 1：{outcome:?} clicks={:?}",
            pointer.clicks
        );
        let first = panel_key(0);
        let second = panel_key(1);
        assert_eq!(
            (pointer.clicks[0].x, pointer.clicks[0].y),
            (first.x + first.w / 2, first.y + first.h / 2)
        );
        assert_eq!(
            (pointer.clicks[1].x, pointer.clicks[1].y),
            (second.x + second.w / 2, second.y + second.h / 2),
            "第二次必须点在键 1 上，而不是重试键 0"
        );
    }

    #[test]
    fn module_keys_treats_a_vanished_panel_after_clicking_as_done() {
        // 点过键之后面板消失 = 解完了（游戏会把面板收走）。
        // 不区分的话，**每次成功都会被报成「找不到 4×4 按键阵」失败**。
        // 这里刻意用「进度格读不到」的退化机台（实机常态，见 LESSON-002）：
        // 读不到进度时唯一能依据的就是「点过键 + 面板消失」。
        let mut frames = vec![
            panel_frame(&[0], 0, 16, false),
            panel_frame(&[0], 0, 16, false),
            panel_frame(&[], 0, 16, false),
        ];
        // 之后面板整个消失（只剩灰底）。
        for _ in 0..8 {
            frames.push(blank_panel_frame());
        }
        let mut screen = panel_screen(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "面板消失应判完成，而不是报定位失败：{outcome:?}");
        assert!(!pointer.clicks.is_empty());
    }

    #[test]
    fn module_keys_does_not_claim_done_when_progress_never_moved() {
        // 反例钉死（fail-closed）：进度**明确读到** 0/5、面板却消失了 ⇒ 不许谎报完成。
        let mut frames = vec![
            panel_frame(&[0], 0, 16, true),
            panel_frame(&[0], 0, 16, true),
            panel_frame(&[], 0, 16, true),
        ];
        for _ in 0..8 {
            frames.push(blank_panel_frame());
        }
        let mut screen = panel_screen(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");
        assert!(
            abort_reason(&outcome).contains("进度只有 0/5"),
            "必须说清进度没动：{outcome:?}"
        );
    }

    #[test]
    fn module_keys_moves_on_when_a_key_stays_lit_after_click() {
        // 点完还是亮的（点错了/键无效）→ 超时判死，换下一颗**亮着**的键继续点。
        let mut screen = panel_screen(vec![
            panel_frame(&[0, 5], 0, 16, true),
            panel_frame(&[0, 5], 0, 16, true),
            panel_frame(&[0, 5], 0, 16, true),
            panel_frame(&[0, 5], 0, 16, true),
            panel_frame(&[0, 5], 0, 16, true),
        ]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        let mut centres: Vec<(i32, i32)> = pointer.clicks.iter().map(|p| (p.x, p.y)).collect();
        centres.dedup();
        assert!(
            centres.len() >= 2,
            "第一颗键超时判死后必须换第二颗亮键：{outcome:?} clicks={:?}",
            pointer.clicks
        );
    }

    #[test]
    fn module_keys_rejects_a_panel_with_only_ten_keys() {
        // 只有 10 颗键 → 对不上 4×4 → 零输入 Abort，绝不猜坐标。
        let mut screen = panel_screen(vec![panel_frame(&[], 0, 10, true)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.events, 0);
        assert!(abort_reason(&outcome).contains("4×4 按键阵"), "{outcome:?}");
    }

    #[test]
    fn module_keys_falls_back_to_the_degraded_criterion_without_progress_boxes() {
        // 进度框读不出来（退化）→ 不判进度；16 格都重试到判死之后按退化判据收尾，
        // 且文案必须说明「用的是退化判据」，不许假装读到了进度。
        // 退化档安全阀是 300 事件 = 150 次点击，装得下 16 × DEAD_STRIKES = 64 次。
        let mut screen = panel_screen(vec![panel_frame(&(0..16).collect::<Vec<_>>(), 0, 16, false)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "{outcome:?}");
        assert_eq!(
            pointer.clicks.len(),
            16 * usize::from(keys::DEAD_STRIKES),
            "16 格各重试到判死才收尾：{outcome:?}"
        );
        let mut centres: Vec<(i32, i32)> = pointer.clicks.iter().map(|p| (p.x, p.y)).collect();
        centres.sort_unstable();
        centres.dedup();
        assert_eq!(centres.len(), 16, "16 颗键都要点到");
        assert!(abort_reason(&outcome).contains("退化判据"), "{outcome:?}");
    }
}
