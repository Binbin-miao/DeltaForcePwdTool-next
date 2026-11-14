//! 黄键台 / 模块按键台（`ModuleKeys`）：右侧 4×4 按键阵亮一个点一个，左侧屏幕里 5 个进度格。
//!
//! 几何**每帧自适应**（[`locate_keys`]）：固定百分比坐标已证伪 —— 换一版界面布局，
//! 旧配置的矩形就整体偏出去，四等分切出来的格子落在键缝里（点空）。
//!
//! 闭环（全部判据都来自实测录像，见 `.scratch/az3-fix-batch/spec.md`）：
//! 1. 每帧整帧抓**一次**，`locate_keys` 量出 16 颗键的**实测矩形**与屏幕里的进度格；
//! 2. **同一帧亮着的模块按钮全都要点**（用户拍板：不要求顺序、点过的记 `clicked`）；
//! 3. 点完等 [`CLICK_ACK_TIMEOUT_MS`]：进度推进 / 亮光熄灭视为吃下，连续 2 轮无效判 `dead`；
//! 4. 左侧进度格读到 **5 个 ✓** → Done。
//!
//! **靶点只有一类可以点**：[`KeyTarget::ModuleButton`]。屏幕内那 5 格是**只读**进度指示器，
//! 永远不发点击（[`KeyTarget::ProgressBox`]）—— 旧实现在屏幕格里挑最亮的去点，
//! 点的是进度指示器本身，一辈子都点不亮。
//!
//! **零值/退化边界**：进度格少于 [`MIN_PROGRESS_BOXES`] 个就读不到进度 —— 此时不判进度、
//! 不判死任何键，只等游戏吃下每次点击，靠「16 格点完 / 亮光全熄灭」收尾，
//! 并在文案里写明用的是退化判据。绝不假装读到了进度，也绝不因为读不到进度就盲点。

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
/// 单格进度勾选的绿色像素数下限（实测 ✓ 约 100~170px，留余量）。
const MIN_CHECK_PIXELS: u32 = 30;
/// 点击后等待游戏吃下的上限；超时判定这一轮无效。
const CLICK_ACK_TIMEOUT_MS: u64 = 400;
/// 同一颗键连续这么多轮「点了还亮且进度没动」才判死（spec B6）。
const DEAD_STRIKES: u8 = 2;
/// 连续多少帧定位失败才放弃（50ms 轮询下 ≈0.3s 容错）。
const LOCATE_FAIL_LIMIT: u32 = 6;
/// 少于这么多个进度格就读不到进度（退化判据）。
const MIN_PROGRESS_BOXES: usize = 3;
/// 定位失败时给用户的下一步动作。
const POSE_HINT: &str = "（请正对按键面板，让屏幕和 4×4 按键都进入画面）";
/// 进度格读不到时的收尾说明（这台机器可能真的读不出来）。
const PROGRESS_NOTE: &str = "；退化判据：进度格未识别，本条结论依据「亮光熄灭」";

/// 靶点分类：**只有 [`KeyTarget::ModuleButton`] 允许发点击**，[`KeyTarget::ProgressBox`] 只读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyTarget {
    /// 右侧 4×4 模块按钮（会发黄绿光，点它才推进）。
    ModuleButton,
    /// 屏幕内那 5 格进度指示（只读）。
    ProgressBox,
}

impl KeyTarget {
    /// 查表：可点击的靶点只有一种。新增靶点默认**不可点**（fail-closed）。
    fn clickable(self) -> bool {
        matches!(self, Self::ModuleButton)
    }
}

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
    // 实机分辨率系数（1080p → 1.5）：像素数阈值按它的平方放大。
    let scale = crate::screen_scale(screen);
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
        // `locate_keys` 内部把帧归一到 720p 判定、坐标再乘回原图，所以这里拿到的是
        // **原图坐标**，必须配**原帧**统计像素。像素数阈值是 720p 口径，
        // 1080p 下同一块区域像素多 2.25 倍 —— 按 `scale²` 放大才对得上。
        if let Some(outcome) = state.step(&frame, scale, pointer, clock) {
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
    /// 同一颗键「点了还亮且进度不动」的轮数（到 [`DEAD_STRIKES`] 判死）。
    strikes: [u8; CELLS],
    /// 本轮已点、还没得到游戏回应的键。
    pending: Vec<usize>,
    baseline: usize,
    /// 本帧读到的进度；`None` = 进度格不足，处于退化判据。
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
        scale: f32,
        pointer: &mut impl Pointer,
        clock: &mut impl Clock,
    ) -> Option<Az3Outcome> {
        let min_glow = crate::scaled_area(MIN_GLOW_PIXELS, scale);
        let located = match locate_keys(frame) {
            Ok(located) => {
                self.locate_misses = 0;
                located
            }
            // 定位失败 → 本帧**零注入**（`on_locate_miss` 只会返回「继续等」或「收尾」）。
            Err(error) => return self.on_locate_miss(pointer.event_count(), error),
        };

        let checked = self.checked_in(frame, &located, crate::scaled_area(MIN_CHECK_PIXELS, scale));
        self.checked = checked;
        if checked.is_some_and(|checked| checked >= TOTAL_STAGES) {
            let summary = format!("黄键台 {TOTAL_STAGES}/{TOTAL_STAGES} 完成");
            return Some(done(pointer.event_count(), &summary, self.clicks));
        }

        // 上一轮点击还没有结论 → 本帧先别再点。
        let lit = lit_cells(frame, &located.cells, min_glow);
        if self.waiting(checked, &lit, clock.now_ms()) {
            return None;
        }

        let targets = pick_lit_targets(frame, &located, &self.clicked, &self.dead, min_glow);
        if targets.is_empty() {
            return self.on_no_target(checked, pointer.event_count());
        }
        for (index, at) in targets {
            if let Err(error) = pointer.click(at) {
                return Some(aborted(
                    pointer.event_count(),
                    format!("黄键台点击失败：{error}"),
                ));
            }
            self.clicked[index] = true;
            self.clicks += 1;
            self.pending.push(index);
        }
        self.baseline = checked.unwrap_or(0);
        self.click_ms = clock.now_ms();
        None
    }

    /// 进度已勾选数；进度格不足 [`MIN_PROGRESS_BOXES`] 个 → `None`（退化，不判进度）。
    fn checked_in(&self, frame: &Frame, located: &LocatedKeys, min_check: u32) -> Option<usize> {
        if located.progress.len() < MIN_PROGRESS_BOXES {
            return None;
        }
        Some(count_checked(frame, &located.progress, min_check))
    }

    /// 处理上一轮点击的回应；返回 `true` = 本帧还没有结论，先别再点。
    fn waiting(&mut self, checked: Option<usize>, lit: &[usize], now: u64) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let elapsed = now.saturating_sub(self.click_ms) >= CLICK_ACK_TIMEOUT_MS;
        // 判据一（最可靠，能读到进度格时）：进度推进就算这一轮吃下了。
        if checked.is_some_and(|checked| checked > self.baseline) {
            self.pending.clear();
            return false;
        }
        // 判据二（永远可用）：**亮光熄灭**。点对了游戏会收掉该模块高亮；
        // 亮光还在说明这一轮没生效（或点错了模块）。
        let still_lit = self.pending.iter().any(|index| lit.contains(index));
        if !still_lit {
            self.pending.clear();
            return false;
        }
        if !elapsed {
            return true;
        }
        // 超时仍亮着：累计无效轮数，到上限判死；没到上限的放回可点池重试（spec B5/B6）。
        for index in std::mem::take(&mut self.pending) {
            self.strikes[index] = self.strikes[index].saturating_add(1);
            if self.strikes[index] >= DEAD_STRIKES {
                self.dead[index] = true;
            } else {
                self.clicked[index] = false;
            }
        }
        false
    }

    /// 没有可点的亮键：要么等下一帧，要么（不能再期待任何亮键）收尾。
    fn on_no_target(&self, checked: Option<usize>, events: u64) -> Option<Az3Outcome> {
        let exhausted = (0..CELLS).all(|index| self.clicked[index] || self.dead[index]);
        if !exhausted {
            return None;
        }
        match checked {
            None => {
                // 退化判据：进度读不到，以「16 格点完」收尾，且必须写明用的是哪条判据。
                let summary = format!("黄键台 {CELLS} 键已全部点过{PROGRESS_NOTE}");
                Some(done(events, &summary, self.clicks))
            }
            Some(checked) => Some(aborted(
                events,
                format!("黄键台：{CELLS} 键已点完但进度未满（{checked}/{TOTAL_STAGES}）"),
            )),
        }
    }

    /// 定位失败：允许抖动几帧，超了**立刻收尾且此后零注入**（fail-closed）。
    ///
    /// 面板消失有两种含义，只能按**看得见的证据**分开，不许猜：
    /// - 进度读到 `5/5` → 真的完成了 ⇒ `Done`；
    /// - 进度读不到、但已经点过键 → 大概率解完被游戏收走 ⇒ `Done`（否则每次成功都报失败）；
    /// - 其余（一次没点过，或进度明明只到 `x/5`）⇒ `Abort`，并说清卡在哪。
    fn on_locate_miss(&mut self, events: u64, error: LocateError) -> Option<Az3Outcome> {
        self.locate_misses += 1;
        if self.locate_misses < LOCATE_FAIL_LIMIT {
            return None;
        }
        match (self.checked, self.clicks > 0) {
            (Some(checked), _) if checked >= TOTAL_STAGES => Some(done(
                events,
                &format!("黄键台 {TOTAL_STAGES}/{TOTAL_STAGES} 完成"),
                self.clicks,
            )),
            (None, true) => Some(done(
                events,
                &format!(
                    "黄键台面板已消失（已点击 {} 次），判定已完成{PROGRESS_NOTE}",
                    self.clicks
                ),
                self.clicks,
            )),
            (checked, _) => {
                let progress = match checked {
                    Some(checked) => format!("进度只有 {checked}/{TOTAL_STAGES}，"),
                    None => String::new(),
                };
                Some(aborted(
                    events,
                    format!("{error}{POSE_HINT}（{progress}面板已消失，已停止注入）"),
                ))
            }
        }
    }

    /// 进度文案：读不到进度时说清「读不到」，不编数字。
    fn progress_text(&self) -> String {
        match self.checked {
            Some(checked) => format!("进度 {checked}/{TOTAL_STAGES}"),
            None => "进度格未识别，未判进度".to_owned(),
        }
    }
}

/// 这一格属于哪一类靶点：**落在进度格上的矩形一律是只读进度指示器**。
///
/// 这是「进度行坐标零点击」的结构性保证 —— 不靠调用点自觉，靠类型筛。
fn classify(target: PxRect, progress: &[PxRect]) -> KeyTarget {
    if progress.iter().any(|slot| intersects(*slot, target)) {
        KeyTarget::ProgressBox
    } else {
        KeyTarget::ModuleButton
    }
}

/// 两个矩形是否有交叠（任一面积为 0 即视为不交叠）。
fn intersects(left: PxRect, right: PxRect) -> bool {
    let overlap_w = left.right().min(right.right()) - left.x.max(right.x);
    let overlap_h = left.bottom().min(right.bottom()) - left.y.max(right.y);
    overlap_w > 0 && overlap_h > 0
}

/// 本帧**亮着**的格序号（含已点过的，供 ack 判据用）。
fn lit_cells(frame: &Frame, cells: &[PxRect], min_glow: u32) -> Vec<usize> {
    (0..cells.len())
        .filter(|&index| glow_of(frame, cells, index) >= min_glow)
        .collect()
}

/// 本帧要点的**全部**亮键：亮的、没点过、没判死、且属于可点击靶点。
///
/// 用户拍板：同一帧亮着的模块按钮**全都要点，不要求顺序**（实测同亮 5 个）；
/// 只挑「最亮的一个」会让 5 个亮键互相等待，最后卡在 4/5。
fn pick_lit_targets(
    frame: &Frame,
    located: &LocatedKeys,
    clicked: &[bool; CELLS],
    dead: &[bool; CELLS],
    min_glow: u32,
) -> Vec<(usize, PxPoint)> {
    located
        .cells
        .iter()
        .enumerate()
        .filter(|(index, _)| !is_spent(*index, clicked, dead))
        .filter(|(_, cell)| classify(**cell, &located.progress).clickable())
        .filter(|(index, _)| glow_of(frame, &located.cells, *index) >= min_glow)
        .map(|(index, cell)| (index, center_of(*cell)))
        .collect()
}

/// 某一格的黄/绿高亮像素数（越界返回 0）。
fn glow_of(frame: &Frame, cells: &[PxRect], index: usize) -> u32 {
    let Some(cell) = cells.get(index) else {
        return 0;
    };
    geom::count_in(frame, geom::clip(frame, *cell), geom::is_yellow_or_green_glow)
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

/// 进度格里已勾选的个数（**绿 ✓ 像素** ≥ `min_check`）。
fn count_checked(frame: &Frame, boxes: &[PxRect], min_check: u32) -> usize {
    boxes
        .iter()
        .filter(|&&rect| geom::count_in(frame, geom::clip(frame, rect), geom::is_check_green) >= min_check)
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
