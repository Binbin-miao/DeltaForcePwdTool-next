//! 黄键台 / 模块按键台（`ModuleKeys`）：右侧 4×4 按键阵亮一个点一个，左侧屏幕里 5 个进度框。
//!
//! 几何**每帧自适应**（[`locate_keys`]）：固定百分比坐标已证伪 —— 换一版界面布局，
//! 旧配置的矩形就整体偏出去，四等分切出来的格子落在键缝里（点空）。
//!
//! 闭环：
//! 1. 每帧整帧抓**一次**，`locate_keys` 量出 16 颗键的**实测矩形**与屏幕里的进度框；
//! 2. 逐格统计黄/绿高亮像素，选得分最高、≥ [`MIN_GLOW_PIXELS`]、**未点过 / 未判死**的格，点其中心；
//! 3. 点完等 [`CLICK_ACK_TIMEOUT_MS`]：进度 +1 视为有效，超时判该键无效（`dead`）；
//! 4. 进度勾满 [`TOTAL_STAGES`] → Done；16 格点完仍不满 → Abort。
//!
//! **零值/退化边界**：进度框少于 [`MIN_PROGRESS_BOXES`] 个时读不到进度 —— 此时不判进度、
//! 不判死任何键，只等游戏吃下每次点击，靠「16 格点完」收尾，并在文案里写明用的是退化判据。
//! 绝不假装读到了进度，也绝不因为读不到进度就盲点。

use std::sync::atomic::{AtomicBool, Ordering};

use delta_capture::Frame;
use delta_config::PxAz3;
use delta_geom::{PxPoint, PxRect};
use delta_scene::locate::{locate_keys, LocateError, LocatedKeys};
use delta_scene::Az3Kind;

use crate::geom;
use crate::{Az3Outcome, Clock, Pointer, Screen};

/// 网格边长（4×4 = 16 键）。
const GRID_SIDE: usize = 4;
const CELLS: usize = GRID_SIDE * GRID_SIDE;
/// 进度条勾选总数。
const TOTAL_STAGES: usize = 5;
/// 单格高亮像素数下限（滤掉反光与描边）。
const MIN_GLOW_PIXELS: u32 = 40;
/// 单格进度勾选的绿色像素数下限。
const MIN_CHECK_PIXELS: u32 = 10;
/// 点击后等待进度推进的上限；超时判定该键无效。
const CLICK_ACK_TIMEOUT_MS: u64 = 400;
/// 连续多少帧定位失败才放弃（50ms 轮询下 ≈0.3s 容错）。
const LOCATE_FAIL_LIMIT: u32 = 6;
/// 少于这么多个进度框就读不到进度（退化判据）。
const MIN_PROGRESS_BOXES: usize = 3;
/// 定位失败时给用户的下一步动作。
const POSE_HINT: &str = "（请正对按键面板，让屏幕和 4×4 按键都进入画面）";

pub fn run(
    screen: &mut impl Screen,
    pointer: &mut impl Pointer,
    clock: &mut impl Clock,
    az3: &PxAz3,
    cancel: &AtomicBool,
) -> Az3Outcome {
    let rect = screen.frame_rect();
    if rect.w <= 0 || rect.h <= 0 {
        return aborted(
            pointer.event_count(),
            "黄键台需要整幅画面做自适应定位，当前抓帧方式拿不到整帧范围".to_owned(),
        );
    }

    let poll = az3.poll_interval_ms.max(1);
    let began = clock.now_ms();
    let mut state = State::default();

    for _ in 0..az3.frame_budget.max(1) {
        if cancel.load(Ordering::SeqCst) {
            return aborted(pointer.event_count(), "已取消".to_owned());
        }
        if clock.now_ms().saturating_sub(began) >= az3.max_duration_ms {
            break;
        }
        let frame = match screen.grab(rect) {
            Ok(frame) => frame,
            Err(error) => {
                return aborted(pointer.event_count(), format!("黄键台抓帧失败：{error}"))
            }
        };
        if let Some(outcome) = state.step(&frame, pointer, clock) {
            return outcome;
        }
        clock.sleep_ms(poll);
    }

    aborted(
        pointer.event_count(),
        format!(
            "黄键台超时（已点击 {} 次 / {}）",
            state.clicks,
            state.progress_text()
        ),
    )
}

/// 一次任务的账本：把「点过哪些格 / 进度基准 / 定位连败」从主循环里挪出来。
#[derive(Default)]
struct State {
    clicked: [bool; CELLS],
    dead: [bool; CELLS],
    pending: Option<usize>,
    baseline: usize,
    /// 本帧读到的进度；`None` = 进度框不足，处于退化判据。
    checked: Option<usize>,
    click_ms: u64,
    clicks: usize,
    locate_misses: u32,
}

impl State {
    /// 推进一帧；返回 `Some` 表示这一帧就该收尾。
    fn step(
        &mut self,
        frame: &Frame,
        pointer: &mut impl Pointer,
        clock: &mut impl Clock,
    ) -> Option<Az3Outcome> {
        let located = match locate_keys(frame) {
            Ok(located) => {
                self.locate_misses = 0;
                located
            }
            Err(error) => return self.on_locate_miss(pointer.event_count(), error),
        };

        let checked = self.checked_in(frame, &located);
        self.checked = checked;
        if checked.is_some_and(|checked| checked >= TOTAL_STAGES) {
            let summary = format!("黄键台 {TOTAL_STAGES}/{TOTAL_STAGES} 完成");
            return Some(done(pointer.event_count(), &summary, self.clicks));
        }

        // 上一次点击还没有结论 → 本帧先别再点。
        if self.waiting(checked, clock.now_ms()) {
            return None;
        }

        let Some((index, at)) = pick_lit_cell(frame, &located.cells, &self.clicked, &self.dead) else {
            return self.on_no_target(checked, pointer.event_count());
        };

        if let Err(error) = pointer.click(at) {
            return Some(aborted(
                pointer.event_count(),
                format!("黄键台点击失败：{error}"),
            ));
        }
        self.clicked[index] = true;
        self.clicks += 1;
        self.baseline = checked.unwrap_or(0);
        self.click_ms = clock.now_ms();
        self.pending = Some(index);
        None
    }

    /// 进度已勾选数；进度框不足 [`MIN_PROGRESS_BOXES`] 个 → `None`（退化，不判进度）。
    fn checked_in(&self, frame: &Frame, located: &LocatedKeys) -> Option<usize> {
        if located.progress.len() < MIN_PROGRESS_BOXES {
            return None;
        }
        Some(count_checked(frame, &located.progress))
    }

    /// 处理上一次点击的回应；返回 `true` = 本帧还没有结论，先别再点。
    fn waiting(&mut self, checked: Option<usize>, now: u64) -> bool {
        let Some(index) = self.pending else {
            return false;
        };
        let elapsed = now.saturating_sub(self.click_ms) >= CLICK_ACK_TIMEOUT_MS;
        match checked {
            Some(checked) if checked > self.baseline => {
                self.pending = None;
                false
            }
            Some(_) if elapsed => {
                self.dead[index] = true;
                self.pending = None;
                false
            }
            Some(_) => true,
            // 退化判据：读不到进度就没有「键是否生效」的观测通道 —— 只等游戏吃下这次点击，
            // 绝不据此判死（否则 16 颗键会在读不到进度时被逐个误杀）。
            None if elapsed => {
                self.pending = None;
                false
            }
            None => true,
        }
    }

    /// 没有可点的亮键：要么等下一帧，要么（16 格都点过 / 判死）收尾。
    fn on_no_target(&self, checked: Option<usize>, events: u64) -> Option<Az3Outcome> {
        let exhausted = (0..CELLS).all(|index| self.clicked[index] || self.dead[index]);
        if !exhausted {
            return None;
        }
        match checked {
            None => {
                let summary =
                    format!("黄键台 {CELLS} 键已全部点过（进度框未识别，按退化判据收尾）");
                Some(done(events, &summary, self.clicks))
            }
            Some(checked) => Some(aborted(
                events,
                format!("黄键台：{CELLS} 键已点完但进度未满（{checked}/{TOTAL_STAGES}）"),
            )),
        }
    }

    /// 定位失败：允许抖动几帧，超了才放弃（全程零输入）。
    fn on_locate_miss(&mut self, events: u64, error: LocateError) -> Option<Az3Outcome> {
        self.locate_misses += 1;
        if self.locate_misses < LOCATE_FAIL_LIMIT {
            return None;
        }
        Some(aborted(events, format!("{error}{POSE_HINT}")))
    }

    /// 进度文案：读不到进度时说清「读不到」，不编数字。
    fn progress_text(&self) -> String {
        match self.checked {
            Some(checked) => format!("进度 {checked}/{TOTAL_STAGES}"),
            None => "进度框未识别，未判进度".to_owned(),
        }
    }
}

/// 选当前最亮的未点格：返回（格序号，全局点击坐标）。
fn pick_lit_cell(
    frame: &Frame,
    cells: &[PxRect],
    clicked: &[bool; CELLS],
    dead: &[bool; CELLS],
) -> Option<(usize, PxPoint)> {
    cells
        .iter()
        .enumerate()
        .filter(|(index, _)| !is_spent(*index, clicked, dead))
        .map(|(index, cell)| {
            let glow = geom::count_in(frame, geom::clip(frame, *cell), geom::is_yellow_or_green_glow);
            (index, *cell, glow)
        })
        .filter(|(_, _, glow)| *glow >= MIN_GLOW_PIXELS)
        .max_by_key(|(_, _, glow)| *glow)
        .map(|(index, cell, _)| (index, center_of(cell)))
}

/// 这一格是否已经点过或判死（越界一律当「已用掉」，宁可不点）。
fn is_spent(index: usize, clicked: &[bool; CELLS], dead: &[bool; CELLS]) -> bool {
    let spent = |flags: &[bool; CELLS]| flags.get(index).copied().unwrap_or(true);
    spent(clicked) || spent(dead)
}

fn center_of(rect: PxRect) -> PxPoint {
    PxPoint {
        x: rect.x + rect.w / 2,
        y: rect.y + rect.h / 2,
    }
}

/// 进度框里已勾选的个数（绿色/黄色像素 ≥ [`MIN_CHECK_PIXELS`]）。
fn count_checked(frame: &Frame, boxes: &[PxRect]) -> usize {
    boxes
        .iter()
        .filter(|&&rect| {
            geom::count_in(frame, geom::clip(frame, rect), geom::is_yellow_or_green_glow)
                >= MIN_CHECK_PIXELS
        })
        .count()
}

fn done(events: u64, summary: &str, steps: usize) -> Az3Outcome {
    Az3Outcome::Done {
        kind: Az3Kind::ModuleKeys,
        steps,
        events,
        summary: summary.to_owned(),
    }
}

fn aborted(events: u64, reason: String) -> Az3Outcome {
    Az3Outcome::Aborted {
        kind: Az3Kind::ModuleKeys,
        reason,
        events,
    }
}
