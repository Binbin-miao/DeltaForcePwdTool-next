//! 任务保险（`SafePins`）：5 个竖直滑槽，每槽一根竖直白游标 + 一个白色目标方块。
//!
//! 规则：游标滑到目标方块位置时按空格把该槽锁死（下方指示条变亮绿）。
//! 所以「按下」的判据是 `|游标 x - 目标方块 x| <= 容差`，不是旧实现的「回到槽中心」——
//! 槽中心是布局坐标，跟游戏规则毫无关系。
//!
//! 判据链：
//! 1. 指示条逐槽统计绿色占比 ≥ [`LOCK_GREEN_RATIO`] → 该槽已锁定；
//! 2. 激活槽 = 第一个未锁定的槽；
//! 3. 在激活槽 ±[`SEARCH_HALF_W`]px 内做白色连通域，取「竖直细长块（游标）」+「近方形块（目标）」
//!    中距离最近的一对；
//! 4. 连续 [`TARGET_MISS_LIMIT`] 帧找不到目标方块 → Abort（目标被游标挡住时绝不猜）。

use std::sync::atomic::{AtomicBool, Ordering};

use delta_capture::Frame;
use delta_config::PxAz3;
use delta_scene::Az3Kind;

use crate::geom;
use crate::{Az3Outcome, Clock, Pointer, Screen};

/// 指示条里单个槽的绿色占比达到该值即判定已锁定。
const LOCK_GREEN_RATIO: f32 = 0.10;
/// 竖直游标：高度下限。
const CURSOR_MIN_H: u32 = 18;
/// 竖直游标：宽度上限（细长）。
const CURSOR_MAX_W: u32 = 10;
/// 目标方块：宽高都落在该闭区间内算「近方形」。
const TARGET_SIDE: (u32, u32) = (5, 16);
/// 目标方块 / 游标的最小连通面积（滤噪点）。
const MIN_BLOB_PIXELS: u32 = 12;
/// 每个滑槽在配置中心两侧的搜索半宽（实测相邻槽间距 ≈105px）。
const SEARCH_HALF_W: i32 = 55;
/// 连续多少帧找不到目标方块才放弃。
const TARGET_MISS_LIMIT: u32 = 20;

/// 逐槽绿色占比 → 是否已锁定。
fn locked_slots(indicator: &Frame, band_x: i32, pin_centers_x: &[i32]) -> Vec<bool> {
    let half_w = slot_half_width(pin_centers_x);
    pin_centers_x
        .iter()
        .map(|&center| {
            let local = center - band_x;
            let clip = geom::clip(
                indicator,
                delta_geom::PxRect {
                    x: local - half_w,
                    y: 0,
                    w: half_w * 2,
                    h: indicator.h as i32,
                },
            );
            geom::ratio_in(indicator, clip, geom::is_green_glow) >= LOCK_GREEN_RATIO
        })
        .collect()
}

/// 槽半宽 = 相邻槽间距的一半（指示条按槽分块，间距以外不属于任何槽）。
fn slot_half_width(pin_centers_x: &[i32]) -> i32 {
    let spacing = pin_centers_x
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .min()
        .unwrap_or(48);
    (spacing / 2).max(8)
}

/// 激活槽内的（游标 x, 目标方块 x），均为全局坐标。
fn cursor_and_target(
    track: &Frame,
    band_x: i32,
    pin_cx: i32,
    scale: f32,
) -> Option<(i32, i32)> {
    // 阈值是 720p 实测口径：实机 1080p 下游标 27px 高、目标方块 8..24px 宽，
    // 按原值判会把目标方块全滤掉（上限 16 → 只剩噪声）。按屏幕倍数放大后再比。
    let half_w = crate::scaled_len(SEARCH_HALF_W, scale);
    let min_blob = crate::scaled_area(MIN_BLOB_PIXELS, scale);
    let cursor_min_h = crate::scaled_len(CURSOR_MIN_H as i32, scale).max(1) as u32;
    let cursor_max_w = crate::scaled_len(CURSOR_MAX_W as i32, scale).max(1) as u32;
    let target_side = (
        crate::scaled_len(TARGET_SIDE.0 as i32, scale).max(1) as u32,
        crate::scaled_len(TARGET_SIDE.1 as i32, scale).max(1) as u32,
    );
    let window = geom::clip(
        track,
        delta_geom::PxRect {
            x: pin_cx - band_x - half_w,
            y: 0,
            w: half_w * 2,
            h: track.h as i32,
        },
    );
    if window.is_empty() {
        return None;
    }

    let whites = geom::blobs(track, window, geom::is_white, min_blob);
    let cursors: Vec<_> = whites
        .iter()
        .filter(|blob| blob.clip.h() >= cursor_min_h && blob.clip.w() <= cursor_max_w)
        .collect();
    let targets: Vec<_> = whites
        .iter()
        .filter(|blob| {
            (target_side.0..=target_side.1).contains(&blob.clip.w())
                && (target_side.0..=target_side.1).contains(&blob.clip.h())
        })
        .collect();

    let mut best: Option<(i32, i32, i32)> = None;
    for cursor in &cursors {
        for target in &targets {
            let distance = (cursor.center_x() - target.center_x()).abs();
            if best.is_none_or(|(best_distance, _, _)| distance < best_distance) {
                best = Some((distance, cursor.center_x(), target.center_x()));
            }
        }
    }

    let (_, cursor_x, target_x) = best?;
    Some((band_x + cursor_x, band_x + target_x))
}

pub fn run(
    screen: &mut impl Screen,
    pointer: &mut impl Pointer,
    clock: &mut impl Clock,
    az3: &PxAz3,
    cancel: &AtomicBool,
) -> Az3Outcome {
    if az3.track_band.w <= 0
        || az3.track_band.h <= 0
        || az3.indicator_band.w <= 0
        || az3.indicator_band.h <= 0
        || az3.pin_center_x.is_empty()
    {
        return Az3Outcome::Aborted {
            kind: Az3Kind::SafePins,
            reason: "任务保险滑槽区域未校准".to_owned(),
            events: pointer.event_count(),
        };
    }

    let start_ms = clock.now_ms();
    let total_pins = az3.pin_center_x.len();
    // 实机分辨率系数：下面所有像素阈值乘以它。
    let scale = crate::screen_scale(screen);
    let tol = crate::scaled_len(az3.pin_tolerance_px.max(2), scale);
    let mut hits = 0usize;
    let mut locked_count = 0usize;
    let mut target_misses = 0u32;
    // 起始未武装：spec 要求「离开过容差区才按」，首帧恰好对齐不该直接按下去。
    let mut armed = false;

    for _ in 0..az3.frame_budget {
        if cancel.load(Ordering::SeqCst) {
            return Az3Outcome::Aborted {
                kind: Az3Kind::SafePins,
                reason: "已取消".to_owned(),
                events: pointer.event_count(),
            };
        }
        if clock.now_ms().saturating_sub(start_ms) >= az3.max_duration_ms {
            break;
        }

        let indicator = match screen.grab(az3.indicator_band) {
            Ok(frame) => frame,
            Err(error) => {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafePins,
                    reason: format!("任务保险指示条抓帧失败：{error}"),
                    events: pointer.event_count(),
                };
            }
        };
        let locked = locked_slots(&indicator, az3.indicator_band.x, &az3.pin_center_x);
        locked_count = locked.iter().filter(|ok| **ok).count();
        let Some(active_idx) = locked.iter().position(|ok| !ok) else {
            return Az3Outcome::Done {
                kind: Az3Kind::SafePins,
                steps: total_pins,
                events: pointer.event_count(),
                summary: format!("任务保险 {total_pins}/{total_pins} 槽已锁定"),
                confirmed: true,
            };
        };

        let track = match screen.grab(az3.track_band) {
            Ok(frame) => frame,
            Err(error) => {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafePins,
                    reason: format!("任务保险滑槽抓帧失败：{error}"),
                    events: pointer.event_count(),
                };
            }
        };

        let pair =
            cursor_and_target(&track, az3.track_band.x, az3.pin_center_x[active_idx], scale);
        let Some((cursor_x, target_x)) = pair else {
            target_misses += 1;
            if target_misses >= TARGET_MISS_LIMIT {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafePins,
                    reason: format!("任务保险：第 {} 槽目标位未识别", active_idx + 1),
                    events: pointer.event_count(),
                };
            }
            clock.sleep_ms(az3.poll_interval_ms);
            continue;
        };
        target_misses = 0;

        let distance = (cursor_x - target_x).abs();
        if distance > tol {
            armed = true;
        } else if armed {
            if let Err(error) = pointer.key_space() {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafePins,
                    reason: format!("空格注入失败：{error}"),
                    events: pointer.event_count(),
                };
            }
            armed = false;
            hits += 1;
        }

        clock.sleep_ms(az3.poll_interval_ms);
    }

    Az3Outcome::Aborted {
        kind: Az3Kind::SafePins,
        reason: format!("任务保险超时（已按 {hits} 次 / 已锁定 {locked_count} 槽）"),
        events: pointer.event_count(),
    }
}
