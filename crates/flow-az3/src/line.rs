//! AZ3 信号线台（横线台）：**每帧自适应定位 + 只上下拖动 + 列自动推进**。
//!
//! 实机机制（录像实测，见 `.scratch/az3/line-geometry.md`）：
//! - 屏幕里有 4 列、每列 4 个档位，对应左侧行标签 `10 / 11 / 01 / 00`（自上而下）；
//! - 右上 LED 给出 4 组目标序列，如 `10-11-01-10`；
//! - **操作 = 在屏幕里按住左键上下拖动**（等价于「旋转旋钮」），把**当前列**的档位拖到目标行；
//! - 一列到位后游戏自动推进到下一列；4 列全对 → 黄黑斜纹「信息已校准」。
//!
//! 所以这里只做三件事：定位（[`locate`]）、读 LED、纵拖。**不横拖、不点屏幕以外的地方、
//! 看不出档位就零输入退出** —— 旧实现拿固定坐标去猜，在摩斯面板上发过 64 个乱拖事件。

use std::sync::atomic::{AtomicBool, Ordering};

use delta_capture::Frame;
use delta_config::PxAz3;
use delta_flow_console::{parse_led_sequence, TargetSequence};
use delta_geom::{PxPoint, PxRect};
use delta_scene::locate::{locate_line, LocatedLine, LINE_COLUMNS};
use delta_vision::{detect_yellow_hatch, DiagonalStripeParams};

use crate::{Az3Outcome, Clock, Pointer, Screen};

/// LED 读数缝：生产实现是 [`read_led_sequence`]，测试直接给固定序列。
///
/// 用 `fn` 指针（不捕获环境）而不是闭包 trait：调用点能直接把函数名传进来。
pub type LedReader = fn(&Frame, PxRect) -> Result<TargetSequence, String>;

/// 连续多少帧定位失败才放弃（50ms 轮询下 ≈0.3s 容错）。
const LOCATE_FAIL_LIMIT: u32 = 6;
/// 连续多少帧读不出 LED 才放弃。
const LED_FAIL_LIMIT: u32 = 4;
/// 连续多少帧看不出当前列档位才放弃。
const SELECT_FAIL_LIMIT: u32 = 8;
/// 「这一列停住了」的判据：档位落在目标行后要连续停这么久才算校准完成
/// （松手后画面还会滑一点，立刻判定会误收）。
const STABLE_MS: u64 = 600;
/// 一次拖动之后留给游戏反应的观察窗。
const SETTLE_MS: u64 = 500;
/// 一次拖动的分步数（任务要求 6~10 步，取 8）。
const DRAG_STEPS: i32 = 8;
/// 单列最多拖几次；到次数仍未到位就记 `dead`，不再碰它。
const MAX_ATTEMPTS: u32 = 3;
/// 定位失败时给用户的下一步动作。
const POSE_HINT: &str = "（请正对操作台、让屏幕和红旋钮都进入画面）";

/// 一次任务的进度账本：把「数到第几帧 / 哪列死掉」从主循环里挪出来。
#[derive(Default)]
struct Progress {
    frames: u32,
    locate_misses: u32,
    led_misses: u32,
    select_misses: [u32; LINE_COLUMNS],
    attempts: [u32; LINE_COLUMNS],
    dead: [bool; LINE_COLUMNS],
    calibrated: [bool; LINE_COLUMNS],
    aligned_since: [Option<u64>; LINE_COLUMNS],
    targets: [usize; LINE_COLUMNS],
    drags: u32,
}

/// 控制信号线台的闭环入口。
///
/// 每帧最多抓一次屏；`read_led` 由调用方注入（生产传 [`read_led_sequence`]）。
pub fn run(
    screen: &mut impl Screen,
    pointer: &mut impl Pointer,
    clock: &mut impl Clock,
    read_led: LedReader,
    az3: &PxAz3,
    cancel: &AtomicBool,
) -> Az3Outcome {
    let budget = az3.frame_budget.max(1);
    let poll = az3.poll_interval_ms.max(1);
    let begun = clock.now_ms();
    let mut progress = Progress::default();

    while progress.frames < budget {
        if cancel.load(Ordering::SeqCst) {
            return aborted(pointer.event_count(), "已取消".to_owned());
        }
        if clock.now_ms().saturating_sub(begun) >= az3.max_duration_ms {
            return aborted(pointer.event_count(), "超时".to_owned());
        }
        progress.frames += 1;
        let rect = screen.frame_rect();
        let frame = match screen.grab(rect) {
            Ok(frame) => frame,
            Err(error) => return aborted(pointer.event_count(), error),
        };
        // `locate_line` 内部归一到 720p 判定、坐标已乘回原图，所以拖动坐标直接就是
        // 真实屏幕坐标，无需再换算（相对位移也是原图尺度）。
        if let Some(outcome) = progress.step(&frame, pointer, clock, read_led, cancel) {
            return outcome;
        }
        clock.sleep_ms(poll);
    }
    aborted(pointer.event_count(), "抓帧配额已用尽".to_owned())
}

/// 生产用 LED 读数：裁出字条 → 字形识别 → 解析成 4 组 2 位 0/1。
pub fn read_led_sequence(frame: &Frame, led: PxRect) -> Result<TargetSequence, String> {
    let strip = delta_scene::crop(&frame.luma(), led).ok_or_else(|| "LED 区域越界".to_owned())?;
    let text = delta_vision::read_glyph_string(&strip).map_err(|error| error.to_string())?;
    parse_led_sequence(&text).map_err(|error| error.to_string())
}

impl Progress {
    /// 推进一帧；返回 `Some` 表示这一帧就该收尾。
    fn step(
        &mut self,
        frame: &Frame,
        pointer: &mut impl Pointer,
        clock: &mut impl Clock,
        read_led: LedReader,
        cancel: &AtomicBool,
    ) -> Option<Az3Outcome> {
        let line = match locate_line(frame) {
            Ok(line) => line,
            Err(error) => return self.on_locate_miss(frame, pointer.event_count(), error),
        };
        self.locate_misses = 0;

        // 「信息已校准」是游戏自己给的完成信号：出现就收工。
        // 不能只依赖 `track` 的 4 列全部读到档位 —— 实测第 4 列容易被屏幕右缘裁掉
        // 或完成时不显示黄色定位框，那样 4 列永远凑不齐，会白等到超时。
        if hatch_visible(frame) {
            let summary = format!("信息已校准（共拖 {} 次）", self.drags);
            return Some(done(pointer.event_count(), &summary));
        }

        let sequence = match read_led(frame, line.led) {
            Ok(sequence) => sequence,
            Err(error) => return self.on_led_miss(pointer.event_count(), &error),
        };
        self.led_misses = 0;
        self.targets = sequence.rows();

        if self.track(&line, clock.now_ms()) {
            let summary = format!("信号线台已对齐校准（4 列，共拖 {} 次）", self.drags);
            return Some(done(pointer.event_count(), &summary));
        }
        let Some(column) = self.current() else {
            // 没有可推进的列，且 `track` 没判全绿：说明有列被判死（看不出档位）。
            // 再空转只会烧帧配额，直接说清是哪几列没搞定。
            let missed: Vec<String> = (0..LINE_COLUMNS)
                .filter(|&column| self.dead[column] && !self.calibrated[column])
                .map(|column| (column + 1).to_string())
                .collect();
            let reason = if missed.is_empty() {
                "4 列都没能对齐".to_owned()
            } else {
                format!("第 {} 列没能对齐（看不出信号线位置，已跳过）", missed.join("、"))
            };
            return Some(aborted(pointer.event_count(), reason));
        };
        self.drive_line(&line, column, pointer, clock, cancel)
    }

    /// 定位失败的处理：允许抖动几帧，超了才放弃（全程零输入）。
    fn on_locate_miss(
        &mut self,
        frame: &Frame,
        events: u64,
        error: delta_scene::locate::LocateError,
    ) -> Option<Az3Outcome> {
        // 定位失败时唯一可能的好消息：黄黑斜纹「信息已校准」把面板盖住了。
        if hatch_visible(frame) {
            return Some(done(events, "信息已校准（黄黑斜纹已出现）"));
        }
        self.locate_misses += 1;
        if self.locate_misses < LOCATE_FAIL_LIMIT {
            return None;
        }
        let reason = format!("{error}{POSE_HINT}");
        Some(aborted(events, reason))
    }

    /// LED 读失败的处理：允许抖动几帧，超了才放弃。
    fn on_led_miss(&mut self, events: u64, error: &str) -> Option<Az3Outcome> {
        self.led_misses += 1;
        if self.led_misses < LED_FAIL_LIMIT {
            return None;
        }
        let reason = format!("右上角数字条读不出来（要求 4 组、每组 2 位 0/1）：{error}");
        Some(aborted(events, reason))
    }

    /// 更新「哪列已校准」：档位在目标行上连续停满 `STABLE_MS`。返回是否 4 列全部校准。
    fn track(&mut self, line: &LocatedLine, now: u64) -> bool {
        for column in 0..LINE_COLUMNS {
            let aligned = line.selected.get(column).copied().flatten() == Some(self.targets[column]);
            if !aligned {
                self.aligned_since[column] = None;
                continue;
            }
            let since = *self.aligned_since[column].get_or_insert(now);
            if now.saturating_sub(since) >= STABLE_MS {
                self.calibrated[column] = true;
            }
        }
        self.calibrated.iter().all(|&done| done)
    }

    /// 当前列 = 第一个还没校准、也没被判死的列。
    fn current(&self) -> Option<usize> {
        (0..LINE_COLUMNS).find(|&column| !self.calibrated[column] && !self.dead[column])
    }

    /// 驱动某一列：看得出档位就纵拖到目标行，看不出就等下一帧（绝不盲拖）。
    fn drive_line(
        &mut self,
        line: &LocatedLine,
        column: usize,
        pointer: &mut impl Pointer,
        clock: &mut impl Clock,
        cancel: &AtomicBool,
    ) -> Option<Az3Outcome> {
        let Some(row) = line.selected.get(column).copied().flatten() else {
            self.select_misses[column] += 1;
            if self.select_misses[column] < SELECT_FAIL_LIMIT {
                return None;
            }
            // 实测 t78/t88（未开始的静止画面）整屏都没有琥珀框，t96 才逐列出现；
            // 已完成的列也可能不再显示框。没有框就没有位置信息 —— 这一列判死、不再碰它，
            // 但不立刻收工：剩下还有别的列要拖，或者游戏稍后会给出「信息已校准」。
            self.dead[column] = true;
            return self.on_column_dead(pointer.event_count());
        };
        self.select_misses[column] = 0;
        if row == self.targets[column] {
            // 已经在这一列的目标行上：只等「停住 600ms」，绝不再拖（否则会把对齐好的线拖走）。
            return None;
        }
        if self.attempts[column] >= MAX_ATTEMPTS {
            self.dead[column] = true;
            return self.on_column_dead(pointer.event_count());
        }
        let (Some(from), Some(to)) = (
            line.slot_center(column, row),
            line.slot_center(column, self.targets[column]),
        ) else {
            return Some(aborted(pointer.event_count(), "档位坐标换算失败".to_owned()));
        };
        if let Err(error) = drag(pointer, from, to.y - from.y, cancel) {
            return Some(aborted(pointer.event_count(), error));
        }
        self.attempts[column] += 1;
        self.drags += 1;
        clock.sleep_ms(SETTLE_MS);
        None
    }

    /// 一列被判死之后：4 列全死才收工，否则继续推进剩下的列。
    fn on_column_dead(&self, events: u64) -> Option<Az3Outcome> {
        if !self.dead.iter().all(|&dead| dead) {
            return None;
        }
        let reason = format!("4 列都没能对齐（每列拖了 {MAX_ATTEMPTS} 次仍不到目标行）");
        Some(aborted(events, reason))
    }

}

fn done(events: u64, summary: &str) -> Az3Outcome {
    Az3Outcome::Done {
        kind: delta_scene::Az3Kind::SignalLine,
        steps: LINE_COLUMNS,
        events,
        summary: summary.to_owned(),
    }
}

fn aborted(events: u64, reason: String) -> Az3Outcome {
    Az3Outcome::Aborted {
        kind: delta_scene::Az3Kind::SignalLine,
        reason: format!("信号线台：{reason}"),
        events,
    }
}

/// 完成判据：屏幕里出现「黄框 + 45° 黄黑斜纹」浮层。
fn hatch_visible(frame: &Frame) -> bool {
    let params = DiagonalStripeParams::default();
    detect_yellow_hatch(&frame.data, frame.w, frame.h, frame.stride, &params).unwrap_or(false)
}

/// 纵拖：按下 → 8 步相对位移 → 抬起。**抬起一定执行**（报错/取消也不许把左键卡住）。
fn drag(
    pointer: &mut impl Pointer,
    from: PxPoint,
    dy: i32,
    cancel: &AtomicBool,
) -> Result<(), String> {
    pointer
        .begin(from)
        .map_err(|error| format!("按下失败：{error}"))?;
    let moved = push(pointer, dy, cancel);
    let released = pointer.end().map_err(|error| format!("抬起失败：{error}"));
    moved.and(released)
}

fn push(pointer: &mut impl Pointer, dy: i32, cancel: &AtomicBool) -> Result<(), String> {
    let mut moved = 0;
    for step in 0..DRAG_STEPS {
        if cancel.load(Ordering::SeqCst) {
            return Err("已取消".to_owned());
        }
        let target = dy * (step + 1) / DRAG_STEPS;
        pointer
            .step(0, target - moved)
            .map_err(|error| format!("拖动失败：{error}"))?;
        moved = target;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    // ── 100% 脱敏的合成机台（与任何真实机位无关，只复刻相对结构）──

    const W: u32 = 640;
    const H: u32 = 360;
    const MACHINE: [u8; 3] = [148, 152, 150];
    const BEZEL: [u8; 3] = [10, 190, 190];
    const PANEL: [u8; 3] = [10, 55, 60];
    const SLOT: [u8; 3] = [190, 225, 225];
    const AMBER: [u8; 3] = [250, 210, 60];
    const LED_DARK: [u8; 3] = [25, 25, 25];
    const LED_GLYPH: [u8; 3] = [240, 240, 240];
    const KNOB: [u8; 3] = [140, 40, 30];
    const SCREEN: PxRect = PxRect {
        x: 40,
        y: 40,
        w: 380,
        h: 240,
    };
    const CONTENT: PxRect = PxRect {
        x: 52,
        y: 52,
        w: 356,
        h: 216,
    };
    const LED_PANEL: PxRect = PxRect {
        x: 450,
        y: 60,
        w: 180,
        h: 50,
    };
    const COLUMNS: [i32; 4] = [100, 180, 260, 340];
    const ROWS: [i32; 4] = [90, 140, 190, 240];

    fn rect(x: i32, y: i32, w: i32, h: i32) -> PxRect {
        PxRect { x, y, w, h }
    }

    fn ring(canvas: &mut Canvas, frame: PxRect, thickness: i32, rgb: [u8; 3]) {
        canvas.paint(rect(frame.x, frame.y, frame.w, thickness), rgb);
        canvas.paint(
            rect(frame.x, frame.bottom() - thickness, frame.w, thickness),
            rgb,
        );
        canvas.paint(rect(frame.x, frame.y, thickness, frame.h), rgb);
        canvas.paint(
            rect(frame.right() - thickness, frame.y, thickness, frame.h),
            rgb,
        );
    }

    struct Canvas {
        data: Vec<u8>,
    }

    impl Canvas {
        fn new(fill: [u8; 3]) -> Self {
            let mut data = vec![0u8; (W * H * 4) as usize];
            for pixel in data.as_chunks_mut::<4>().0 {
                pixel[0] = fill[2];
                pixel[1] = fill[1];
                pixel[2] = fill[0];
                pixel[3] = 255;
            }
            Self { data }
        }

        fn paint(&mut self, area: PxRect, rgb: [u8; 3]) -> &mut Self {
            for y in area.y.max(0)..area.bottom().min(H as i32) {
                for x in area.x.max(0)..area.right().min(W as i32) {
                    let index = ((y as u32 * W + x as u32) * 4) as usize;
                    self.data[index] = rgb[2];
                    self.data[index + 1] = rgb[1];
                    self.data[index + 2] = rgb[0];
                }
            }
            self
        }

        fn frame(&self) -> Frame {
            Frame::new(W, H, self.data.clone(), std::time::Instant::now()).expect("合法合成帧")
        }
    }

    /// 机台一帧：`selected` 是每列被琥珀框圈住的档位行。
    fn machine(selected: [Option<usize>; LINE_COLUMNS], knob: bool) -> Frame {
        let mut canvas = Canvas::new(MACHINE);
        canvas.paint(SCREEN, BEZEL);
        canvas.paint(CONTENT, PANEL);
        for column in COLUMNS {
            for row in ROWS {
                canvas.paint(rect(column - 4, row - 9, 8, 18), SLOT);
            }
        }
        for (index, row) in selected.iter().enumerate() {
            let Some(row) = row else { continue };
            let slot = rect(COLUMNS[index] - 10, ROWS[*row] - 15, 20, 30);
            ring(&mut canvas, slot, 3, AMBER);
        }
        canvas.paint(LED_PANEL, LED_DARK);
        for index in 0..11 {
            let area = match index % 3 {
                2 => rect(LED_PANEL.x + 12 + index * 14, LED_PANEL.y + 22, 5, 6),
                _ => rect(LED_PANEL.x + 12 + index * 14, LED_PANEL.y + 12, 9, 26),
            };
            ring(&mut canvas, area, 2, LED_GLYPH);
        }
        if knob {
            canvas.paint(rect(505, 235, 30, 30), KNOB);
        }
        canvas.frame()
    }

    /// 固定 LED 读数（测试里不跑字形识别，只验闭环控制）。
    fn fixed_led(_frame: &Frame, _led: PxRect) -> Result<TargetSequence, String> {
        parse_led_sequence("10-11-01-10").map_err(|error| error.to_string())
    }

    struct MachineScreen(Rc<RefCell<Game>>);

    impl Screen for MachineScreen {
        fn grab(&mut self, _rect: PxRect) -> Result<Frame, String> {
            let game = self.0.borrow();
            Ok(machine(game.selected, game.knob))
        }

        fn frame_rect(&self) -> PxRect {
            rect(0, 0, W as i32, H as i32)
        }
    }

    #[derive(Default)]
    struct Game {
        selected: [Option<usize>; LINE_COLUMNS],
        knob: bool,
        dragging: Option<(usize, i32)>,
        begin_points: Vec<PxPoint>,
        steps: Vec<(i32, i32)>,
        releases: u32,
        events: u64,
        /// 模拟「拖动无效」：位移被游戏吃掉。
        stuck: bool,
    }

    impl Game {
        fn new(selected: [Option<usize>; LINE_COLUMNS]) -> Self {
            Self {
                selected,
                knob: true,
                ..Self::default()
            }
        }
    }

    struct MachinePointer(Rc<RefCell<Game>>);

    impl Pointer for MachinePointer {
        fn click(&mut self, _at: PxPoint) -> Result<(), String> {
            Ok(())
        }

        fn key_space(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn begin(&mut self, at: PxPoint) -> Result<(), String> {
            let mut game = self.0.borrow_mut();
            game.begin_points.push(at);
            game.events += 2;
            let column = COLUMNS
                .iter()
                .position(|&x| (x - at.x).abs() <= 8)
                .ok_or_else(|| format!("按下点不在任何列中心：{at:?}"))?;
            game.dragging = Some((column, 0));
            Ok(())
        }

        fn step(&mut self, dx: i32, dy: i32) -> Result<(), String> {
            let mut game = self.0.borrow_mut();
            game.events += 1;
            game.steps.push((dx, dy));
            let (column, moved) = game.dragging.ok_or_else(|| "未按下就拖动".to_owned())?;
            game.dragging = Some((column, moved + dy));
            Ok(())
        }

        fn end(&mut self) -> Result<(), String> {
            let mut game = self.0.borrow_mut();
            game.events += 1;
            game.releases += 1;
            let (column, moved) = game
                .dragging
                .take()
                .ok_or_else(|| "未按下就抬起".to_owned())?;
            if game.stuck {
                return Ok(());
            }
            let gap = f32::from((ROWS[1] - ROWS[0]) as i16);
            let current = f32::from(game.selected[column].unwrap_or(0) as i16);
            let next = (current + (moved as f32 / gap)).round().clamp(0.0, 3.0) as usize;
            game.selected[column] = Some(next);
            Ok(())
        }

        fn event_count(&self) -> u64 {
            self.0.borrow().events
        }
    }

    struct TestClock {
        now: u64,
    }

    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.now
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.now += ms;
        }
    }

    fn play(mut game: Game) -> (Az3Outcome, Rc<RefCell<Game>>) {
        let shared = Rc::new(RefCell::new(std::mem::take(&mut game)));
        let mut screen = MachineScreen(shared.clone());
        let mut pointer = MachinePointer(shared.clone());
        let mut clock = TestClock { now: 0 };
        let cancel = AtomicBool::new(false);
        let az3 = crate::tests_support::az3();
        let outcome = run(
            &mut screen,
            &mut pointer,
            &mut clock,
            fixed_led,
            &az3,
            &cancel,
        );
        (outcome, shared)
    }

    fn reason_of(outcome: &Az3Outcome) -> String {
        match outcome {
            Az3Outcome::Aborted { reason, .. } => reason.clone(),
            Az3Outcome::Done { summary, .. } => summary.clone(),
        }
    }

    #[test]
    fn drags_every_column_to_its_target_row_with_vertical_moves_only() {
        // 目标 `10-11-01-10` = 行 0/1/2/0；起点全在行 3。
        let (outcome, game) = play(Game::new([Some(3); 4]));
        let game = game.borrow();
        assert!(outcome.is_done(), "{outcome:?}");
        assert_eq!(game.selected, [Some(0), Some(1), Some(2), Some(0)]);
        assert!(
            game.steps.iter().all(|&(dx, _)| dx == 0),
            "只允许纵拖：{:?}",
            game.steps
        );
        assert!(game.releases >= LINE_COLUMNS as u32, "每列至少按下/抬起一次");
        let expected: Vec<PxPoint> = COLUMNS
            .iter()
            .map(|&x| PxPoint { x, y: ROWS[3] })
            .collect();
        assert_eq!(
            game.begin_points, expected,
            "按下点必须落在「列中心 × 当前档位」上"
        );
    }

    #[test]
    fn already_aligned_columns_are_never_dragged() {
        let (outcome, game) = play(Game::new([Some(0), Some(1), Some(2), Some(0)]));
        let game = game.borrow();
        assert!(outcome.is_done(), "{outcome:?}");
        assert!(game.begin_points.is_empty(), "已对齐的列一次都不许碰");
        assert_eq!(game.events, 0, "全程零输入");
    }

    #[test]
    fn column_without_marker_is_never_dragged() {
        // 第 2 列没有黄色定位框：这一列绝不拖（没有位置信息）。
        // 但不能因为一列读不到就整体放弃 —— 其余列该拖还是要拖，
        // 收尾时说清是哪几列没搞定（实测第 4 列常被屏幕右缘裁掉）。
        let (outcome, game) = play(Game::new([Some(3), None, Some(2), Some(0)]));
        let game = game.borrow();
        assert!(!outcome.is_done(), "{outcome:?}");
        assert!(
            game.begin_points.iter().all(|point| point.x != COLUMNS[1]),
            "看不出档位的那列绝不拖：{:?}",
            game.begin_points
        );
        assert!(
            reason_of(&outcome).contains("第 2 列没能对齐"),
            "{outcome:?}"
        );
    }

    #[test]
    fn blank_frame_aborts_on_the_knob_with_zero_events() {
        struct Blank;
        impl Screen for Blank {
            fn grab(&mut self, _rect: PxRect) -> Result<Frame, String> {
                Ok(Canvas::new([0, 0, 0]).frame())
            }

            fn frame_rect(&self) -> PxRect {
                rect(0, 0, W as i32, H as i32)
            }
        }
        struct Quiet(Rc<RefCell<Game>>);
        impl Pointer for Quiet {
            fn click(&mut self, _at: PxPoint) -> Result<(), String> {
                self.0.borrow_mut().events += 1;
                Ok(())
            }
            fn key_space(&mut self) -> Result<(), String> {
                self.0.borrow_mut().events += 1;
                Ok(())
            }
            fn begin(&mut self, _at: PxPoint) -> Result<(), String> {
                self.0.borrow_mut().events += 1;
                Ok(())
            }
            fn step(&mut self, _dx: i32, _dy: i32) -> Result<(), String> {
                self.0.borrow_mut().events += 1;
                Ok(())
            }
            fn end(&mut self) -> Result<(), String> {
                self.0.borrow_mut().events += 1;
                Ok(())
            }
            fn event_count(&self) -> u64 {
                self.0.borrow().events
            }
        }

        let shared = Rc::new(RefCell::new(Game::default()));
        let mut screen = Blank;
        let mut pointer = Quiet(shared.clone());
        let mut clock = TestClock { now: 0 };
        let cancel = AtomicBool::new(false);
        let az3 = crate::tests_support::az3();
        let outcome = run(
            &mut screen,
            &mut pointer,
            &mut clock,
            fixed_led,
            &az3,
            &cancel,
        );
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.event_count(), 0, "定位失败必须零输入");
        assert!(reason_of(&outcome).contains("没找到红色旋钮"), "{outcome:?}");
    }

    #[test]
    fn screen_without_knob_aborts_on_the_knob() {
        struct NoKnob;
        impl Screen for NoKnob {
            fn grab(&mut self, _rect: PxRect) -> Result<Frame, String> {
                Ok(machine([Some(0), Some(1), Some(2), Some(0)], false))
            }

            fn frame_rect(&self) -> PxRect {
                rect(0, 0, W as i32, H as i32)
            }
        }

        let shared = Rc::new(RefCell::new(Game::default()));
        let mut screen = NoKnob;
        let mut pointer = MachinePointer(shared.clone());
        let mut clock = TestClock { now: 0 };
        let cancel = AtomicBool::new(false);
        let az3 = crate::tests_support::az3();
        let outcome = run(
            &mut screen,
            &mut pointer,
            &mut clock,
            fixed_led,
            &az3,
            &cancel,
        );
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.event_count(), 0);
        assert!(reason_of(&outcome).contains("没找到红色旋钮"), "{outcome:?}");
    }

    #[test]
    fn unreadable_led_aborts_without_touching_the_mouse() {
        fn broken_led(_frame: &Frame, _led: PxRect) -> Result<TargetSequence, String> {
            Err("只读到 2 组".to_owned())
        }
        let shared = Rc::new(RefCell::new(Game::new([Some(3); 4])));
        let mut screen = MachineScreen(shared.clone());
        let mut pointer = MachinePointer(shared.clone());
        let mut clock = TestClock { now: 0 };
        let cancel = AtomicBool::new(false);
        let az3 = crate::tests_support::az3();
        let outcome = run(
            &mut screen,
            &mut pointer,
            &mut clock,
            broken_led,
            &az3,
            &cancel,
        );
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(pointer.event_count(), 0);
        assert!(reason_of(&outcome).contains("数字条读不出来"), "{outcome:?}");
    }

    #[test]
    fn a_column_that_never_moves_is_given_up_after_three_drags() {
        let mut game = Game::new([Some(3); 4]);
        game.stuck = true;
        let (outcome, game) = play(game);
        let game = game.borrow();
        assert!(!outcome.is_done(), "{outcome:?}");
        assert_eq!(
            game.begin_points.len(),
            (MAX_ATTEMPTS as usize) * LINE_COLUMNS,
            "4 列各拖满 {MAX_ATTEMPTS} 次才放弃"
        );
        assert!(reason_of(&outcome).contains("4 列都没能对齐"), "{outcome:?}");
    }

    #[test]
    fn drag_splits_into_eight_equal_vertical_steps() {
        let shared = Rc::new(RefCell::new(Game::default()));
        let mut pointer = MachinePointer(shared.clone());
        let cancel = AtomicBool::new(false);
        drop(drag(
            &mut pointer,
            PxPoint { x: 100, y: 240 },
            -150,
            &cancel,
        ));
        let game = shared.borrow();
        assert_eq!(game.steps.len(), DRAG_STEPS as usize);
        assert_eq!(game.steps.iter().map(|&(_, dy)| dy).sum::<i32>(), -150);
        assert!(game.steps.iter().all(|&(dx, _)| dx == 0));
        assert_eq!(game.releases, 1, "抬起必须发生");
        assert!(game.dragging.is_none());
    }

    #[test]
    fn cancel_during_drag_still_releases_the_button() {
        let shared = Rc::new(RefCell::new(Game::default()));
        let mut pointer = MachinePointer(shared.clone());
        let cancel = AtomicBool::new(true);
        let outcome = drag(&mut pointer, PxPoint { x: 100, y: 240 }, -150, &cancel);
        assert_eq!(outcome, Err("已取消".to_owned()));
        assert_eq!(shared.borrow().releases, 1, "取消也必须抬手，否则鼠标键会卡死");
    }
}
