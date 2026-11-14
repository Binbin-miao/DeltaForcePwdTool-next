//! AZ3 骇入电脑 4 类小游戏统一执行入口。
//!
//! - `SafeTiming`（大保险）：高亮横带逐格右移的绿色游标过中间格时按空格（`0x39`）
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

/// AZ3 流程统一返回结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Az3Outcome {
    Done {
        kind: Az3Kind,
        steps: usize,
        events: u64,
        summary: String,
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
    const TRACK_W: u32 = 531;
    const TRACK_H: u32 = 76;
    const INDICATOR_W: u32 = 531;
    const INDICATOR_H: u32 = 86;
    const GRID_W: u32 = 269;
    const GRID_H: u32 = 234;
    const PROGRESS_W: u32 = 198;
    const PROGRESS_H: u32 = 90;

    /// 高亮带底色：半透明绿罩，够 `is_green_glow` 但不够「亮绿」。
    const BAND_GREEN: [u8; 3] = [60, 150, 60];
    /// 游标：亮绿。
    const CURSOR_GREEN: [u8; 3] = [80, 240, 90];
    /// 按错后整条变红。
    const FAIL_RED: [u8; 3] = [200, 60, 60];
    const WHITE: [u8; 3] = [240, 240, 240];
    const LIT_YELLOW: [u8; 3] = [235, 200, 40];
    const LOCK_GREEN: [u8; 3] = [40, 210, 60];
    /// 黄键台台面：比暗键亮，暗键阵才数得出来。
    const TABLE_GREY: [u8; 3] = [120, 120, 120];
    /// 未点亮的暗键。
    const DARK_KEY: [u8; 3] = [40, 40, 40];

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

    /// 黄键台专用：网格帧恒定，进度帧按脚本推进。
    struct SplitScreen {
        grid_rect: PxRect,
        grid: Frame,
        progress: Vec<Frame>,
        pulled: usize,
    }

    impl Screen for SplitScreen {
        fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
            if rect == self.grid_rect {
                return Ok(self.grid.clone());
            }
            let last = self.progress.len().saturating_sub(1);
            let frame = self.progress.get(self.pulled.min(last)).cloned();
            self.pulled += 1;
            frame.ok_or_else(|| "进度帧脚本为空".to_owned())
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

    /// 大保险帧：一条高亮带 + 落在第 `cell` 格的亮绿游标。
    fn timing_frame(cell: u32) -> Frame {
        let cell_center = ((cell as f32 + 0.5) * TARGET_W as f32 / 5.0) as i32;
        paint_frame(
            TARGET_W,
            TARGET_H,
            &[
                (rect(0, 20, TARGET_W as i32, 40), BAND_GREEN),
                (rect(cell_center - 7, 20, 14, 40), CURSOR_GREEN),
            ],
        )
    }

    /// 按错后的整条红带。
    fn failed_timing_frame() -> Frame {
        paint_frame(TARGET_W, TARGET_H, &[(rect(0, 20, TARGET_W as i32, 40), FAIL_RED)])
    }

    /// 任务保险指示条：前 `locked` 个槽为绿。
    fn indicator_frame(locked: usize) -> Frame {
        let centers = pin_local_centers();
        let bars: Vec<(PxRect, [u8; 3])> = centers
            .iter()
            .take(locked)
            .map(|&cx| (rect(cx - 30, 20, 60, 30), LOCK_GREEN))
            .collect();
        paint_frame(INDICATOR_W, INDICATOR_H, &bars)
    }

    /// 任务保险滑槽：`cursor_x` 处一根白游标；`target_x` 有目标方块。
    fn track_frame(target_x: Option<i32>, cursor_x: i32) -> Frame {
        let mut shapes = vec![(rect(cursor_x - 4, 16, 8, 54), WHITE)];
        if let Some(target) = target_x {
            shapes.push((rect(target - 5, 4, 10, 10), WHITE));
        }
        paint_frame(TRACK_W, TRACK_H, &shapes)
    }

    fn pin_local_centers() -> Vec<i32> {
        let az3 = test_az3();
        az3.pin_center_x
            .iter()
            .map(|center| center - az3.track_band.x)
            .collect()
    }

    /// 黄键台网格：灰台面上第 `lit` / `also` 格亮黄，其余为暗键。
    fn grid_frame(lit: Option<usize>, also: Option<usize>) -> Frame {
        let mut shapes = vec![(rect(0, 0, GRID_W as i32, GRID_H as i32), TABLE_GREY)];
        for index in 0..16 {
            if Some(index) == lit || Some(index) == also {
                continue;
            }
            let (x0, y0) = cell_origin(index);
            shapes.push((rect(x0 + 6, y0 + 4, 55, 50), DARK_KEY));
        }
        for index in [lit, also].into_iter().flatten() {
            let (x0, y0) = cell_origin(index);
            shapes.push((rect(x0 + 6, y0 + 4, 55, 50), LIT_YELLOW));
        }
        paint_frame(GRID_W, GRID_H, &shapes)
    }

    fn cell_origin(index: usize) -> (i32, i32) {        let cell_w = (GRID_W / 4) as i32;
        let cell_h = (GRID_H / 4) as i32;
        (((index % 4) as i32) * cell_w, ((index / 4) as i32) * cell_h)
    }

    /// 16 格全亮（模拟「所有键都点过却推不动进度」）。
    fn grid_all_lit() -> Frame {
        let mut shapes = vec![(rect(0, 0, GRID_W as i32, GRID_H as i32), TABLE_GREY)];
        for index in 0..16 {
            let (x0, y0) = cell_origin(index);
            shapes.push((rect(x0 + 6, y0 + 4, 55, 50), LIT_YELLOW));
        }
        paint_frame(GRID_W, GRID_H, &shapes)
    }

    /// 该格的全局点击坐标（与 `keys::cell_clip` 的切分口径逐像素一致）。
    fn cell_center_global(index: usize) -> PxPoint {
        let az3 = test_az3();
        let cell_w = (GRID_W / 4) as i32;
        let cell_h = (GRID_H / 4) as i32;
        let col = (index % 4) as i32;
        let row = (index / 4) as i32;
        let x0 = col * cell_w;
        let y0 = row * cell_h;
        let x1 = if col == 3 { GRID_W as i32 } else { x0 + cell_w };
        let y1 = if row == 3 { GRID_H as i32 } else { y0 + cell_h };
        PxPoint {
            x: az3.button_grid_region.x + x0 + (x1 - x0) / 2,
            y: az3.button_grid_region.y + y0 + (y1 - y0) / 2,
        }
    }

    /// 进度条：前 `checked` 个勾选格为绿。
    fn progress_frame(checked: usize) -> Frame {
        let slot = PROGRESS_W as i32 / 5;
        let shapes: Vec<(PxRect, [u8; 3])> = (0..checked)
            .map(|index| (rect(index as i32 * slot + 5, 20, 25, 40), LOCK_GREEN))
            .collect();
        paint_frame(PROGRESS_W, PROGRESS_H, &shapes)
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
    fn safe_timing_presses_once_per_pass_and_finishes_after_five_chars() {
        // 游标每轮「先到最右格、再回中间格」→ 每轮恰好按一次，5 轮完成。
        let mut frames = Vec::new();
        for _ in 0..5 {
            frames.push(timing_frame(4));
            frames.push(timing_frame(2));
        }
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafeTiming, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.spaces, 5);
    }

    #[test]
    fn safe_timing_does_not_repeat_press_while_staying_on_center_cell() {
        // 游标停在中间格不动 → 只能触发一次空格（去重，防连按）。
        let mut screen = ScriptScreen::new(vec![timing_frame(2)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafeTiming, &mut screen, &mut pointer);
        assert_eq!(pointer.spaces, 1, "{outcome:?}");
        assert!(!outcome.is_done());
    }

    #[test]
    fn safe_timing_does_not_repeat_press_when_band_flickers() {
        // 中格 → 单帧找不到带（连通域被字符高光切碎）→ 中格：只允许按 1 次。
        let mut screen = ScriptScreen::new(vec![
            timing_frame(2),
            paint_frame(TARGET_W, TARGET_H, &[]),
            timing_frame(2),
        ]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafeTiming, &mut screen, &mut pointer);
        assert_eq!(pointer.spaces, 1, "{outcome:?}");
        assert!(!outcome.is_done());
    }

    #[test]
    fn safe_timing_aborts_when_no_band_is_found() {
        let mut screen = ScriptScreen::new(vec![paint_frame(TARGET_W, TARGET_H, &[])]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafeTiming, &mut screen, &mut pointer);
        assert!(!outcome.is_done());
        assert!(abort_reason(&outcome).contains("未找到高亮带"), "{outcome:?}");
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

    // ── 任务保险 ──

    #[test]
    fn safe_pins_presses_when_cursor_meets_target_and_finishes_all_locked() {
        let centers = pin_local_centers();
        let last = centers[4];
        // 首帧游标还在容差区外（预武装），第二帧才压到目标上 → 按一次 → 全锁。
        let mut frames = vec![
            indicator_frame(4),
            track_frame(Some(last), last + 30),
            indicator_frame(4),
            track_frame(Some(last), last + 6),
            indicator_frame(5),
        ];
        frames.push(indicator_frame(5));
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.spaces, 1);
    }

    #[test]
    fn safe_pins_does_not_press_on_the_very_first_frame_already_aligned() {
        // 首帧就对齐 = 还没「离开过容差区」，不许直接按（否则会按在早一格的时机上）。
        let centers = pin_local_centers();
        let last = centers[4];
        let mut screen = ScriptScreen::new(vec![
            indicator_frame(4),
            track_frame(Some(last), last),
        ]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert_eq!(pointer.spaces, 0, "{outcome:?}");
        assert!(!outcome.is_done());
    }

    #[test]
    fn safe_pins_finds_target_offset_from_the_slot_center() {
        // 目标方块偏在槽中心 +45px（实测槽宽约 105px）：搜索半宽必须够大才找得到。
        let centers = pin_local_centers();
        let last = centers[4];
        let offset = last + 45;
        let mut screen = ScriptScreen::new(vec![
            indicator_frame(4),
            track_frame(Some(offset), last + 20),
            indicator_frame(4),
            track_frame(Some(offset), offset),
            indicator_frame(5),
        ]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.spaces, 1);
    }

    #[test]
    fn safe_pins_does_not_repeat_press_for_the_same_target() {
        // 预武装一次后游标一直压在目标上、槽始终不锁定 → 只允许按一次空格。
        let centers = pin_local_centers();
        let last = centers[4];
        let mut screen = ScriptScreen::new(vec![
            indicator_frame(4),
            track_frame(Some(last), last + 30),
            indicator_frame(4),
            track_frame(Some(last), last),
        ]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert_eq!(pointer.spaces, 1, "{outcome:?}");
        assert!(!outcome.is_done());
    }

    #[test]
    fn safe_pins_aborts_when_target_square_is_missing() {
        // 只有游标、没有目标方块 → 绝不猜位置，连续 20 帧后放弃。
        let centers = pin_local_centers();
        let mut frames = vec![indicator_frame(0), track_frame(None, centers[0])];
        frames.push(track_frame(None, centers[0]));
        let mut screen = ScriptScreen::new(frames);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::SafePins, &mut screen, &mut pointer);
        assert!(!outcome.is_done());
        assert!(abort_reason(&outcome).contains("第 1 槽目标位未识别"), "{outcome:?}");
        assert_eq!(pointer.spaces, 0);
    }

    // ── 黄键台 ──

    fn key_screen(grid: Frame, progress: Vec<Frame>) -> SplitScreen {
        SplitScreen {
            grid_rect: test_az3().button_grid_region,
            grid,
            progress,
            pulled: 0,
        }
    }

    #[test]
    fn module_keys_clicks_lit_cell_and_finishes_at_five_checks() {
        let mut screen = key_screen(
            grid_frame(Some(0), None),
            vec![progress_frame(4), progress_frame(5)],
        );
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.clicks, vec![cell_center_global(0)]);
    }

    #[test]
    fn module_keys_aborts_when_no_key_lights_up() {
        let mut screen = key_screen(grid_frame(None, None), vec![progress_frame(0)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(!outcome.is_done());
        assert!(abort_reason(&outcome).contains("已点击 0 次"), "{outcome:?}");
        assert!(pointer.clicks.is_empty());
    }

    #[test]
    fn module_keys_aborts_once_all_sixteen_keys_are_spent() {
        // 16 格全点过、进度仍不动 → 必须立即 Abort，而不是烧满 15s 超时。
        let mut screen = key_screen(grid_all_lit(), vec![progress_frame(0)]);
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(!outcome.is_done());
        assert_eq!(pointer.clicks.len(), 16, "{outcome:?}");
        assert!(abort_reason(&outcome).contains("16 键已点完但进度未满"), "{outcome:?}");
    }

    #[test]
    fn module_keys_never_reclicks_the_same_cell() {
        // 进度永不推进：两个亮键各点一次后都被判死，同一格绝不重复点击。
        let mut screen = key_screen(
            grid_frame(Some(0), Some(5)),
            vec![progress_frame(0)],
        );
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert!(!outcome.is_done());
        assert_eq!(
            pointer.clicks,
            vec![cell_center_global(0), cell_center_global(5)],
            "{outcome:?}"
        );
    }

    #[test]
    fn module_keys_never_reclicks_a_key_that_stays_lit_after_progress() {
        // 点亮的键在进度推进后仍然亮着（游戏常见表现）——这种键绝不能再点第二次，
        // 否则就是旧实现里那个「同一亮键点到超时」的死循环。
        let mut screen = key_screen(
            grid_frame(Some(0), Some(5)),
            vec![progress_frame(0), progress_frame(1), progress_frame(2)],
        );
        let mut pointer = MockPointer::default();
        let outcome = run_with(Az3Kind::ModuleKeys, &mut screen, &mut pointer);
        assert_eq!(
            pointer.clicks,
            vec![cell_center_global(0), cell_center_global(5)],
            "{outcome:?}"
        );
    }
}
