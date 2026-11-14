//! 黄键台 / 模块按键台（`ModuleKeys`）：右侧 4×4 按键网格亮一个点一个，左侧 5 个进度勾选。
//!
//! 旧实现对网格内所有亮像素求**单一质心**：3 个键同时亮时质心落在键缝里，点空；
//! 已点亮的绿键又一直被当成目标，形成死循环。这里改成**逐格打分 + 已点标记**：
//! 1. `button_grid_region` 四等分成 16 格，逐格统计黄/绿高亮像素数；
//! 2. 选得分最高、≥ [`MIN_GLOW_PIXELS`]、且**未被点过 / 未被判死**的格子，点它的几何中心；
//! 3. 点完等 [`CLICK_ACK_TIMEOUT_MS`]：进度 +1 视为有效，超时视为该键无效（标记 dead 后继续）；
//! 4. 进度勾选读满 [`TOTAL_STAGES`] 即 Done；16 格全点过仍不满 → Abort。

use std::sync::atomic::{AtomicBool, Ordering};

use delta_capture::Frame;
use delta_config::PxAz3;
use delta_geom::PxPoint;
use delta_scene::Az3Kind;

use crate::geom::{self, Clip};
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

/// 第 `index` 格在网格帧内的窗口。
fn cell_clip(frame: &Frame, index: usize) -> Clip {
    let whole = geom::full(frame);
    let cell_w = (frame.w / GRID_SIDE as u32).max(1);
    let cell_h = (frame.h / GRID_SIDE as u32).max(1);
    let row = (index / GRID_SIDE) as u32;
    let col = (index % GRID_SIDE) as u32;
    let x0 = col * cell_w;
    let y0 = row * cell_h;
    let x1 = if col + 1 == GRID_SIDE as u32 {
        frame.w
    } else {
        x0 + cell_w
    };
    let y1 = if row + 1 == GRID_SIDE as u32 {
        frame.h
    } else {
        y0 + cell_h
    };
    Clip {
        x0: (whole.x0 + x0).min(whole.x1),
        y0: (whole.y0 + y0).min(whole.y1),
        x1: (whole.x0 + x1).min(whole.x1),
        y1: (whole.y0 + y1).min(whole.y1),
    }
}

/// 选当前最亮的未点格：返回（格序号，全局点击坐标）。
fn pick_lit_cell(
    frame: &Frame,
    origin: PxPoint,
    clicked: &[bool; CELLS],
    dead: &[bool; CELLS],
) -> Option<(usize, PxPoint)> {
    let mut best: Option<(u32, usize, PxPoint)> = None;
    for index in 0..CELLS {
        if clicked[index] || dead[index] {
            continue;
        }
        let clip = cell_clip(frame, index);
        let glow = geom::count_in(frame, clip, geom::is_yellow_or_green_glow);
        if glow < MIN_GLOW_PIXELS {
            continue;
        }
        let center = PxPoint {
            x: origin.x + (clip.x0 + clip.w() / 2) as i32,
            y: origin.y + (clip.y0 + clip.h() / 2) as i32,
        };
        if best.is_none_or(|(best_glow, _, _)| glow > best_glow) {
            best = Some((glow, index, center));
        }
    }
    best.map(|(_, index, center)| (index, center))
}

/// 进度条已勾选数（0..=5）：五等分后逐格统计黄/绿像素。
fn count_checked_boxes(frame: &Frame) -> usize {
    let whole = geom::full(frame);
    let slot_w = (frame.w / TOTAL_STAGES as u32).max(1);
    (0..TOTAL_STAGES)
        .filter(|stage| {
            let x0 = *stage as u32 * slot_w;
            let x1 = if stage + 1 == TOTAL_STAGES {
                frame.w
            } else {
                x0 + slot_w
            };
            let clip = Clip {
                x0: whole.x0 + x0,
                y0: whole.y0,
                x1: (whole.x0 + x1).min(whole.x1),
                y1: whole.y1,
            };
            geom::count_in(frame, clip, geom::is_yellow_or_green_glow) >= MIN_CHECK_PIXELS
        })
        .count()
}

pub fn run(
    screen: &mut impl Screen,
    pointer: &mut impl Pointer,
    clock: &mut impl Clock,
    az3: &PxAz3,
    cancel: &AtomicBool,
) -> Az3Outcome {
    let grid = az3.button_grid_region;
    let progress = az3.progress_region;
    if grid.w <= 0 || grid.h <= 0 || progress.w <= 0 || progress.h <= 0 {
        return Az3Outcome::Aborted {
            kind: Az3Kind::ModuleKeys,
            reason: "黄键台区域未校准".to_owned(),
            events: pointer.event_count(),
        };
    }

    let start_ms = clock.now_ms();
    let origin = PxPoint {
        x: grid.x,
        y: grid.y,
    };
    let mut clicked = [false; CELLS];
    let mut dead = [false; CELLS];
    let mut pending: Option<usize> = None;
    let mut baseline_checked = 0usize;
    let mut checked = 0usize;
    let mut click_ms = 0u64;
    let mut clicks = 0usize;

    for _ in 0..az3.frame_budget {
        if cancel.load(Ordering::SeqCst) {
            return Az3Outcome::Aborted {
                kind: Az3Kind::ModuleKeys,
                reason: "已取消".to_owned(),
                events: pointer.event_count(),
            };
        }
        let now = clock.now_ms();
        if now.saturating_sub(start_ms) >= az3.max_duration_ms {
            break;
        }

        let progress_frame = match screen.grab(progress) {
            Ok(frame) => frame,
            Err(error) => {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::ModuleKeys,
                    reason: format!("黄键台进度条抓帧失败：{error}"),
                    events: pointer.event_count(),
                };
            }
        };
        checked = count_checked_boxes(&progress_frame);
        if checked >= TOTAL_STAGES {
            return Az3Outcome::Done {
                kind: Az3Kind::ModuleKeys,
                steps: clicks.max(TOTAL_STAGES),
                events: pointer.event_count(),
                summary: format!("黄键台 {TOTAL_STAGES}/{TOTAL_STAGES} 完成"),
            };
        }

        if let Some(index) = pending {
            if checked > baseline_checked {
                pending = None;
            } else if now.saturating_sub(click_ms) >= CLICK_ACK_TIMEOUT_MS {
                dead[index] = true;
                pending = None;
            } else {
                clock.sleep_ms(az3.poll_interval_ms);
                continue;
            }
        }

        let grid_frame = match screen.grab(grid) {
            Ok(frame) => frame,
            Err(error) => {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::ModuleKeys,
                    reason: format!("黄键台网格抓帧失败：{error}"),
                    events: pointer.event_count(),
                };
            }
        };

        let Some((index, at)) = pick_lit_cell(&grid_frame, origin, &clicked, &dead) else {
            // 没有可点的格子：全部格子都已点过或判死 → 直接放弃，不烧满 15s 超时。
            let exhausted = clicked
                .iter()
                .zip(dead.iter())
                .all(|(clicked, dead)| *clicked || *dead);
            if exhausted {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::ModuleKeys,
                    reason: format!("黄键台：{CELLS} 键已点完但进度未满（{checked}/{TOTAL_STAGES}）"),
                    events: pointer.event_count(),
                };
            }
            clock.sleep_ms(az3.poll_interval_ms);
            continue;
        };

        if let Err(error) = pointer.click(at) {
            return Az3Outcome::Aborted {
                kind: Az3Kind::ModuleKeys,
                reason: format!("黄键台点击失败：{error}"),
                events: pointer.event_count(),
            };
        }
        clicked[index] = true;
        clicks += 1;
        baseline_checked = checked;
        click_ms = now;
        pending = Some(index);

        clock.sleep_ms(az3.poll_interval_ms);
    }

    Az3Outcome::Aborted {
        kind: Az3Kind::ModuleKeys,
        reason: format!("黄键台超时（已点击 {clicks} 次 / 进度 {checked}/{TOTAL_STAGES}）"),
        events: pointer.event_count(),
    }
}
