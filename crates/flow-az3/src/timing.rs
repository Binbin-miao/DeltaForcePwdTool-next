//! 大保险（`SafeTiming`）：5 个字符格的高亮横带 + 逐格右移的绿色游标，到中间格按空格。
//!
//! 判据链（每一步失败都有明确出口，绝不盲按）：
//! 1. **找带**：逐行统计高亮（绿或红）占比，取占比最高且 ≥ [`BAND_ROW_MIN_RATIO`] 的连续行段；
//!    连续 [`BAND_MISS_LIMIT`] 帧找不到 → Abort（多半是没进游戏或区域没校准）。
//! 2. **按错识别**：带内红色占比 ≥ [`RED_ABORT_RATIO`] → 立即 Abort（整条变红是唯一失败信号）。
//! 3. **找游标**：带内最大绿色连通域（面积 ≥ [`MIN_CURSOR_PIXELS`]），只取它的 `cx`；
//!    用全带质心会被整条绿底拉偏，这是旧实现的根因。
//! 4. **按空格**：格宽 = 带宽 / 5，`idx == 2` 即中间格；只有「上一帧不在中间格」时才按一次，
//!    同一格停留绝不重复按。按满 [`TOTAL_CHARS`] 次即完成（大保险固定 5 个字符）。

use std::sync::atomic::{AtomicBool, Ordering};

use delta_config::PxAz3;
use delta_scene::Az3Kind;

use crate::geom::{self, Clip};
use crate::{Az3Outcome, Clock, Pointer, Screen};

/// 带内某一行的高亮占比达到该值才算「带的一行」。
const BAND_ROW_MIN_RATIO: f32 = 0.25;
/// 连续多少帧找不到带才放弃（单帧被 UI 浮层遮住不算失败）。
const BAND_MISS_LIMIT: u32 = 8;
/// 游标连通域最小面积（滤掉字符边缘的绿色噪点）。
const MIN_CURSOR_PIXELS: u32 = 12;
/// 带内红色占比达到该值判定按早了。
const RED_ABORT_RATIO: f32 = 0.30;
/// 大保险固定 5 个字符。
const TOTAL_CHARS: i32 = 5;
/// 中间格的格序号（0 起）。
const CENTER_CELL: i32 = 2;

fn is_highlight(color: [u8; 3]) -> bool {
    geom::is_green_glow(color) || geom::is_red_glow(color)
}

/// 游标是带内**最亮**的绿块：高亮带底是一层半透明绿罩（`is_green_glow` 就够），
/// 游标比它亮一档，必须用更严的阈值切开，否则整条带会被当成一个巨型连通域。
fn is_bright_green([b, g, r]: [u8; 3]) -> bool {
    g > 170 && g > r.saturating_add(45) && g > b.saturating_add(35)
}

/// 在整帧内找出实际高亮带（行范围）；找不到返回 `None`。
fn detect_band(frame: &delta_capture::Frame) -> Option<Clip> {
    let whole = geom::full(frame);
    let ratios = geom::row_ratios(frame, whole, is_highlight);
    if ratios.is_empty() {
        return None;
    }

    let mut best: Option<(u32, u32, f32)> = None;
    let mut start: Option<u32> = None;
    for (y, ratio) in ratios.iter().enumerate() {
        let inside = *ratio >= BAND_ROW_MIN_RATIO;
        match (inside, start) {
            (true, None) => start = Some(y as u32),
            (false, Some(begin)) => {
                consider(&mut best, &ratios, begin, y as u32);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        consider(&mut best, &ratios, begin, ratios.len() as u32);
    }

    let (begin, end, _) = best?;
    Some(geom::rows(whole, begin, end))
}

/// 记录「平均占比最高」的连续行段；同分取更长的一段。
fn consider(best: &mut Option<(u32, u32, f32)>, ratios: &[f32], begin: u32, end: u32) {
    if end <= begin {
        return;
    }
    let span = &ratios[begin as usize..end as usize];
    let mean = span.iter().sum::<f32>() / span.len() as f32;
    let better = match *best {
        None => true,
        Some((_, _, best_mean)) => mean > best_mean,
    };
    if better {
        *best = Some((begin, end, mean));
    }
}

/// 绿色游标所在格序号（0..=4）。
///
/// 游标的形态特征：占满带高、明显窄于一个字符格。加上这两条结构约束后，
/// 即使带底本身也是绿的，也只会挑出那块会移动的亮绿，不会被整条带拉偏。
fn cursor_cell(frame: &delta_capture::Frame, band: Clip) -> Option<i32> {
    let width = band.w() as i32;
    if width <= 0 {
        return None;
    }
    let min_height = (band.h() / 2).max(1);
    let max_width = (band.w() / 3).max(4);
    let cursor = geom::blobs(frame, band, is_bright_green, MIN_CURSOR_PIXELS)
        .into_iter()
        .find(|blob| blob.clip.w() <= max_width && blob.clip.h() >= min_height)?;
    let offset = (cursor.center_x() - band.x0 as i32) * TOTAL_CHARS / width;
    Some(offset.clamp(0, TOTAL_CHARS - 1))
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

    let start_ms = clock.now_ms();
    let mut hits = 0usize;
    let mut band_misses = 0u32;
    let mut last_cell: Option<i32> = None;

    for _ in 0..az3.frame_budget {
        if cancel.load(Ordering::SeqCst) {
            return Az3Outcome::Aborted {
                kind: Az3Kind::SafeTiming,
                reason: "已取消".to_owned(),
                events: pointer.event_count(),
            };
        }
        if clock.now_ms().saturating_sub(start_ms) >= az3.max_duration_ms {
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

        let detected = detect_band(&frame);
        let Some(actual) = detected else {
            band_misses += 1;
            if band_misses >= BAND_MISS_LIMIT {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafeTiming,
                    reason: "大保险：未找到高亮带".to_owned(),
                    events: pointer.event_count(),
                };
            }
            // 刻意保留 `last_cell`：单帧检测抖动不代表游标离开了中间格，
            // 清掉它就会把「还在中格」当成「重新进入中格」，同一字符按两次 → 整条变红。
            clock.sleep_ms(az3.poll_interval_ms);
            continue;
        };
        band_misses = 0;

        if geom::ratio_in(&frame, actual, geom::is_red_glow) >= RED_ABORT_RATIO {
            return Az3Outcome::Aborted {
                kind: Az3Kind::SafeTiming,
                reason: "大保险：按早了，整条已变红".to_owned(),
                events: pointer.event_count(),
            };
        }

        let cell = cursor_cell(&frame, actual);
        if cell == Some(CENTER_CELL) && last_cell != Some(CENTER_CELL) {
            if let Err(error) = pointer.key_space() {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafeTiming,
                    reason: format!("空格注入失败：{error}"),
                    events: pointer.event_count(),
                };
            }
            hits += 1;
            if hits >= TOTAL_CHARS as usize {
                return Az3Outcome::Done {
                    kind: Az3Kind::SafeTiming,
                    steps: hits,
                    events: pointer.event_count(),
                    summary: format!("大保险已按满 {TOTAL_CHARS} 个字符"),
                };
            }
        }
        last_cell = cell;

        clock.sleep_ms(az3.poll_interval_ms);
    }

    Az3Outcome::Aborted {
        kind: Az3Kind::SafeTiming,
        reason: format!("大保险超时（已按 {hits}/{TOTAL_CHARS} 个字符）"),
        events: pointer.event_count(),
    }
}
