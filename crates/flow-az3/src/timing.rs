//! 大保险（`SafeTiming`）：5 个纵向滚动字符槽 + 中间水平亮框 + 右侧「选定密码」。
//!
//! 真实界面机制（实机录像与 `images/ui/timing.png` 实证）：
//! - 屏幕提示：「破译方法：当绿色的密码到达中央时，按空格锁定密码」；
//! - 5 列字符各自纵向滚动，当某列目标（绿色字符）进入中央水平亮框时按空格（`0x39`）；
//! - 错按则整条变红（Abort 止损）；
//! - 连续 5 次锁定且目标消失/面板关闭 → `Done { confirmed: true }`。

use std::sync::atomic::{AtomicBool, Ordering};

use delta_config::PxAz3;
use delta_scene::Az3Kind;

use crate::geom;
use crate::{Az3Outcome, Clock, Pointer, Screen};

/// 大保险固定 5 列字符。
const TOTAL_CHARS: usize = 5;
/// 单列中央亮框内「出现绿色目标字符」的像素下限（720p 口径，按 `scale²` 放大）。
const MIN_COL_GREEN_PIXELS: u32 = 12;
/// 带内红色占比达到该值判定按错/失败。
const RED_ABORT_RATIO: f32 = 0.30;
/// 两次空格之间的最小冷却时间（毫秒），防止单次进入因边缘抖动连按两次。
const PRESS_COOLDOWN_MS: u64 = 180;
/// 按满 5 个字符之后等终态证据（绿色目标消失/面板关闭）的窗口。
const CONFIRM_WAIT_MS: u64 = 1_500;
/// 连续多少帧绿色目标完全不离开同一列（静态误判护栏，约 5s）。
const STILL_FRAMES: u32 = 100;

/// 统计 5 个纵向列在中央亮框内的绿色像素是否达到阈值。
fn green_columns(frame: &delta_capture::Frame, min_pixels: u32) -> [bool; TOTAL_CHARS] {
    let w = frame.w;
    let h = frame.h;
    if w < TOTAL_CHARS as u32 || h == 0 {
        return [false; TOTAL_CHARS];
    }
    let mut active = [false; TOTAL_CHARS];
    for (col, slot) in active.iter_mut().enumerate() {
        let x0 = (col as u32 * w) / TOTAL_CHARS as u32;
        let x1 = ((col as u32 + 1) * w) / TOTAL_CHARS as u32;
        let clip = geom::clip(
            frame,
            delta_geom::PxRect {
                x: x0 as i32,
                y: 0,
                w: (x1 - x0) as i32,
                h: h as i32,
            },
        );
        *slot = geom::count_in(frame, clip, geom::is_green_glow) >= min_pixels;
    }
    active
}

pub fn run(
    screen: &mut impl Screen,
    pointer: &mut impl Pointer,
    clock: &mut impl Clock,
    az3: &PxAz3,
    cancel: &AtomicBool,
) -> Az3Outcome {
    let band = az3.target_band;
    if band.w <= 0 || band.h <= 0 {
        return Az3Outcome::Aborted {
            kind: Az3Kind::SafeTiming,
            reason: "大保险目标区域未校准".to_owned(),
            events: pointer.event_count(),
        };
    }

    let scale = crate::screen_scale(screen);
    let min_col_green = crate::scaled_area(MIN_COL_GREEN_PIXELS, scale);
    let start_ms = clock.now_ms();
    let mut hits = 0usize;
    let mut was_green = [false; TOTAL_CHARS];
    let mut still_frames = 0u32;
    let mut last_press_ms: Option<u64> = None;
    let mut typed_all_at: Option<u64> = None;

    for _ in 0..az3.frame_budget {
        if cancel.load(Ordering::SeqCst) {
            return Az3Outcome::Aborted {
                kind: Az3Kind::SafeTiming,
                reason: "已取消".to_owned(),
                events: pointer.event_count(),
            };
        }
        let now = clock.now_ms();
        if now.saturating_sub(start_ms) >= az3.max_duration_ms {
            break;
        }

        let frame = match screen.grab(band) {
            Ok(frame) => frame,
            Err(error) => {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafeTiming,
                    reason: format!("大保险抓帧失败：{error}"),
                    events: pointer.event_count(),
                };
            }
        };

        let whole = geom::full(&frame);
        if geom::ratio_in(&frame, whole, geom::is_red_glow) >= RED_ABORT_RATIO {
            return Az3Outcome::Aborted {
                kind: Az3Kind::SafeTiming,
                reason: "大保险：按错了，整条已变红".to_owned(),
                events: pointer.event_count(),
            };
        }

        let col_green = green_columns(&frame, min_col_green);
        let any_green = col_green.iter().any(|&g| g);

        if let Some(started) = typed_all_at {
            if !any_green {
                return Az3Outcome::Done {
                    kind: Az3Kind::SafeTiming,
                    steps: hits,
                    events: pointer.event_count(),
                    summary: format!("大保险已输入 {TOTAL_CHARS} 个字符，面板已关闭"),
                    confirmed: true,
                };
            }
            if now.saturating_sub(started) >= CONFIRM_WAIT_MS {
                return Az3Outcome::Done {
                    kind: Az3Kind::SafeTiming,
                    steps: hits,
                    events: pointer.event_count(),
                    summary: format!(
                        "大保险已输入 {TOTAL_CHARS} 个字符，但高亮目标仍在（未确认完成）"
                    ),
                    confirmed: false,
                };
            }
            clock.sleep_ms(az3.poll_interval_ms);
            continue;
        }

        if hits > 0 && any_green && col_green == was_green {
            still_frames += 1;
            if still_frames >= STILL_FRAMES {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafeTiming,
                    reason: format!(
                        "大保险：连续 {still_frames} 帧绿色目标未移动，疑似误判界面，已停手"
                    ),
                    events: pointer.event_count(),
                };
            }
        } else {
            still_frames = 0;
        }

        let rising = (0..TOTAL_CHARS).any(|col| col_green[col] && !was_green[col]);
        let cooled = last_press_ms.is_none_or(|t| now.saturating_sub(t) >= PRESS_COOLDOWN_MS);
        if rising && cooled {
            if let Err(error) = pointer.key_space() {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafeTiming,
                    reason: format!("空格注入失败：{error}"),
                    events: pointer.event_count(),
                };
            }
            hits += 1;
            last_press_ms = Some(now);
            if hits >= TOTAL_CHARS {
                typed_all_at = Some(now);
            }
        }

        was_green = col_green;
        clock.sleep_ms(az3.poll_interval_ms);
    }

    if typed_all_at.is_some() {
        return Az3Outcome::Done {
            kind: Az3Kind::SafeTiming,
            steps: hits,
            events: pointer.event_count(),
            summary: format!("大保险已输入 {TOTAL_CHARS} 个字符，但没等到面板关闭（未确认完成）"),
            confirmed: false,
        };
    }

    Az3Outcome::Aborted {
        kind: Az3Kind::SafeTiming,
        reason: format!("大保险超时（已按 {hits}/{TOTAL_CHARS} 个字符）"),
        events: pointer.event_count(),
    }
}
