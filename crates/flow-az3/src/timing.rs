//! 大保险（`SafeTiming`）：5 个字符格的高亮横带 + 逐格右移的绿色游标，到中间格按空格。
//!
//! 判据链（每一步失败都有明确出口，绝不盲按）：
//! 1. **找带**：逐行统计高亮（绿或红）占比，取占比最高且 ≥ [`BAND_ROW_MIN_RATIO`] 的连续行段；
//!    连续 [`BAND_MISS_LIMIT`] 帧找不到 → Abort（多半是没进游戏或区域没校准）。
//! 2. **按错识别**：带内红色占比 ≥ [`RED_ABORT_RATIO`] → 立即 Abort（整条变红是唯一失败信号）。
//! 3. **找游标**：带内最大绿色连通域（面积 ≥ [`MIN_CURSOR_PIXELS`]），只取它的 `cx`；
//!    用全带质心会被整条绿底拉偏，这是旧实现的根因。
//! 4. **按空格**：格宽 = 带宽 / 5，`idx == 2` 即中间格；**必须真的读到过游标不在中间格**
//!    之后才允许按一次 —— 读不清的帧既不算「离开」也不算「进入」。旧实现拿 `None` 覆盖
//!    `last_cell`，面板抖一下就会对同一个字符按两次，整条直接变红（用户反馈的「大保险怎么
//!    按都不对」）。同一格重复进入绝不重复按。
//! 5. **五次输入 ≠ 完成**：按满 [`TOTAL_CHARS`] 次只是我们自己的计数，不是面板给的证据。
//!    终态证据只有一条 —— 高亮带消失（面板关了）。等不到就按未确认完成收尾
//!    （`confirmed = false`），绝不把「我按了 5 下」当成解开。
//! 6. **按键止损**：按下过一次之后，连续 [`STILL_FRAMES`] 个可读帧都停在同一格 ⇒ 游标根本
//!    没在扫（真面板的游标一定在换格），这条带是假的，立刻 Abort（空格在游戏里是**跳跃**）。

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
/// 按满 5 个字符之后等终态证据（高亮带消失）的窗口。
///
/// 面板关闭有动画，给足一秒半；等不到就按「未确认完成」收尾，而不是假装成功。
const CONFIRM_WAIT_MS: u64 = 1_500;
/// 连续多少个「读到了格序号」的帧都停在同一格，就判定这条带的游标不在扫。
///
/// 真面板的游标逐格右移，按完必换格。这里刻意给得很宽（100 帧 ≈ 5s）：这道护栏的用处
/// 只是**给一句准话**（「游标没在扫」比「超时」有用得多），不是省时间；
/// 宁可晚一点报，也绝不能把扫得慢的真面板误杀。
const STILL_FRAMES: u32 = 100;

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
    // 上一次**确实读到**的格序号。读不清的帧不动它 —— 见 `cell` 那段注释。
    let mut last_cell: Option<i32> = None;
    // 连续多少个可读帧停在同一格（游标扫动监视）。
    let mut still_frames = 0u32;
    // 按满 5 个字符的时刻；`Some` = 已进入「等终态证据」阶段，此后一个键都不再按。
    let mut typed_all_at: Option<u64> = None;

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
            // 按满 5 个字符之后高亮带消失 = 面板真关了。这是唯一能拿到的终态证据。
            if typed_all_at.is_some() {
                return Az3Outcome::Done {
                    kind: Az3Kind::SafeTiming,
                    steps: hits,
                    events: pointer.event_count(),
                    summary: format!("大保险已输入 {TOTAL_CHARS} 个字符，面板已关闭"),
                    confirmed: true,
                };
            }
            band_misses += 1;
            if band_misses >= BAND_MISS_LIMIT {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafeTiming,
                    reason: "大保险：未找到高亮带".to_owned(),
                    events: pointer.event_count(),
                };
            }
            // 刻意保留 `last_cell` / `still_frames`：单帧检测抖动不代表游标离开了中间格，
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

        // 已经按满 5 个：不再按任何键，只等带子消失（面板关闭）。
        if let Some(started) = typed_all_at {
            if clock.now_ms().saturating_sub(started) >= CONFIRM_WAIT_MS {
                return Az3Outcome::Done {
                    kind: Az3Kind::SafeTiming,
                    steps: hits,
                    events: pointer.event_count(),
                    // 「我按了 5 下」不是证据：面板还在，就是没法确认。
                    summary: format!(
                        "大保险已输入 {TOTAL_CHARS} 个字符，但高亮带仍在（未确认完成）"
                    ),
                    confirmed: false,
                };
            }
            clock.sleep_ms(az3.poll_interval_ms);
            continue;
        }

        let cell = cursor_cell(&frame, actual);

        // 游标扫动监视：按下过一次之后，连续 `STILL_FRAMES` 个可读帧都停在同一格 ⇒ 游标没在扫。
        // 只数「读到了格序号」的帧：读不清不算证据，否则闪一下就重置，护栏永远不响。
        if hits > 0 {
            if cell.is_some() && cell == last_cell {
                still_frames += 1;
            } else if cell.is_some() {
                still_frames = 0;
            }
            if still_frames >= STILL_FRAMES {
                return Az3Outcome::Aborted {
                    kind: Az3Kind::SafeTiming,
                    reason: format!(
                        "大保险：连续 {still_frames} 帧游标都没换过格，疑似误判界面，已停手"
                    ),
                    events: pointer.event_count(),
                };
            }
        }

        // 只有「上一次确实读到的是非中间格」才算重新进入中间格。
        // `None`（读不清）既不是离开也不是进入 —— 拿它当离开就是同一个字符按两次。
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
                // 不在这里报完成：等带子消失才是证据（见上面那段）。
                typed_all_at = Some(clock.now_ms());
            }
        }
        // 读不清就不覆盖：写进 `None` 等于伪造一次「离开中格」。
        if cell.is_some() {
            last_cell = cell;
        }

        clock.sleep_ms(az3.poll_interval_ms);
    }

    // 帧预算 / 总时长用完了还没等到终态证据：仍然是「未确认完成」，不是失败也不是成功。
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
