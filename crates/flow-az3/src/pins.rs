//! 任务保险（`SafePins`）：**一条连续长轨道**，上面分散若干「白色目标方块」与「白色细竖线游标」。
//!
//! 规则：游标滑到目标方块位置时按空格把该槽锁死（下方指示条逐格变绿）。
//! 所以「按下」的判据是 `|游标 x − 目标 x| <= 轨道宽 × 0.6%`，**不是**旧实现的
//! 「回到槽中心」—— 槽中心是布局坐标，跟游戏规则毫无关系。
//!
//! 判据链（每帧一次整帧抓取，几何全部现场量，见 [`delta_scene::locate::locate_pins`]）：
//! 1. 量出轨道 + 目标白块 + 游标细竖线，并把每个目标配一个最近的游标；
//! 2. 逐对判水平距离：**离开过容差区之后**再回到容差内才按一次空格（防连按）；
//! 3. 完成证据：底部指示条绿条数达到 [`INDICATOR_SLOTS`] 格 ⇒ `Done{confirmed:true}`；
//!    一格绿条都读不到（指示条不在画面里 / 阈值不符）⇒ 退化为「所有见过的目标都按过一遍」
//!    收尾，且 `confirmed:false`（照 `keys.rs` 的退化范式：**绝不假装成功**）。
//! 4. 按键止损（见 `run` 内两道护栏）：空格的游戏内语义是**跳跃**，误判时乱按就是在
//!    让角色自己跳。按满 [`INDICATOR_SLOTS`] 次仍 0 槽，或按键数超过 `INDICATOR_SLOTS * 2`
//!    ⇒ 立即 Abort，把「按了几次」写进结论。
//!
//! 旧实现拿配置里的 `track_band` 裁一条带、再按 5 个写死的 `pin_center_x` 开 5 个搜索窗：
//! 实机轨道上目标与游标的位置每局都变、对数随进度变化（2~5 对），固定槽窗必然开在空白处。

use std::sync::atomic::{AtomicBool, Ordering};

use delta_capture::Frame;
use delta_config::PxAz3;
use delta_geom::PxRect;
use delta_scene::locate::locate_pins;
use delta_scene::Az3Kind;

use crate::geom;
use crate::{Az3Outcome, Clock, Pointer, Screen};

/// 底部指示条的槽数（界面常量：5 格，与轨道上有几对无关）。
const INDICATOR_SLOTS: usize = 5;
/// 指示条里一格绿条的尺寸下限（720p 实测 40~60 × 20~30px）。
const LOCK_BAR_MIN: (u32, u32) = (12, 6);
/// 指示条绿条的最小连通面积（720p 口径，滤掉 HUD 上的零星绿点）。
const LOCK_BAR_MIN_AREA: u32 = 64;
/// 连续多少帧量不出轨道才放弃（50ms 轮询 ⇒ 20 帧 ≈1s，留给镜头微调）。
const LOCATE_FAIL_LIMIT: u32 = 20;

/// 目标位的记账：**按目标中心 x 记账，不按 pair 下标**。
///
/// 已锁定的目标会从轨道上消失、游标偶尔被目标挡住一帧，两种情况下标都会整体左移；
/// 按下标记账会让「同一个目标」被重新武装 ⇒ 同一个位置连按两次空格。
#[derive(Default)]
struct Arming {
    /// `(目标中心 x, 已离开过容差区, 已按过空格)`。
    slots: Vec<(i32, bool, bool)>,
}

impl Arming {
    /// 找到（或新建）这个目标位的记账条目。
    ///
    /// 匹配用**两倍容差**：目标方块是静止的，两帧之间的 x 只会因为定位抖动差 1~2px。
    fn entry(&mut self, target_x: i32, tol: i32) -> &mut (i32, bool, bool) {
        let slack = (tol * 2).max(2);
        let found = self
            .slots
            .iter()
            .position(|(x, _, _)| (x - target_x).abs() <= slack);
        let index = match found {
            Some(index) => index,
            None => {
                self.slots.push((target_x, false, false));
                self.slots.len() - 1
            }
        };
        &mut self.slots[index]
    }

    /// 见过的每个目标都按过一次（退化完成判据）。
    fn all_pressed(&self) -> bool {
        !self.slots.is_empty() && self.slots.iter().all(|(_, _, pressed)| *pressed)
    }
}

/// 指示条已锁定的槽数（绿条数）；读不到就是 0。
///
/// **0 一律当「读不到」**，绝不当「全部锁定」：把没解开的一局报成完成是最坏的结论。
fn locked_bars(frame: &Frame, band: PxRect, scale: f32) -> usize {
    let clip = geom::clip(frame, band);
    if clip.is_empty() {
        return 0;
    }
    let (min_w, min_h) = (
        crate::scaled_len(LOCK_BAR_MIN.0 as i32, scale).max(0) as u32,
        crate::scaled_len(LOCK_BAR_MIN.1 as i32, scale).max(0) as u32,
    );
    geom::blobs(
        frame,
        clip,
        geom::is_green_glow,
        crate::scaled_area(LOCK_BAR_MIN_AREA, scale),
    )
    .into_iter()
    .filter(|blob| blob.clip.w() >= min_w && blob.clip.h() >= min_h)
    .count()
}

pub fn run(
    screen: &mut impl Screen,
    pointer: &mut impl Pointer,
    clock: &mut impl Clock,
    az3: &PxAz3,
    cancel: &AtomicBool,
) -> Az3Outcome {
    let start_ms = clock.now_ms();
    let poll = az3.poll_interval_ms.max(1);
    // 实机分辨率系数：指示条判据是 720p 口径的尺寸阈值，必须按屏幕倍数放大。
    let scale = crate::screen_scale(screen);
    let mut arm = Arming::default();
    let mut presses = 0usize;
    let mut locked = 0usize;
    let mut misses = 0u32;
    // 连续多少帧「轨道在、但一对都配不上」—— 只进超时文案，不单独 Abort：
    // 开局游标还没出来时配不上是正常的（[`locate_pins`] 的零值边界），早退会把真面板当成错界面。
    let mut unpaired = 0u32;

    for _ in 0..az3.frame_budget.max(1) {
        if cancel.load(Ordering::SeqCst) {
            return aborted("已取消".to_owned(), pointer);
        }
        if clock.now_ms().saturating_sub(start_ms) >= az3.max_duration_ms {
            break;
        }
        // 整帧抓一次：轨道、目标、游标、指示条全在这一帧里，判定与执行共用同一份证据。
        let frame = match screen.grab(screen.frame_rect()) {
            Ok(frame) => frame,
            Err(error) => {
                return aborted(format!("任务保险抓帧失败：{error}"), pointer);
            }
        };
        let pins = match locate_pins(&frame) {
            Ok(pins) => pins,
            Err(error) => {
                misses += 1;
                if misses >= LOCATE_FAIL_LIMIT {
                    return aborted(format!("任务保险：{error}"), pointer);
                }
                clock.sleep_ms(poll);
                continue;
            }
        };
        misses = 0;

        locked = locked_bars(&frame, az3.indicator_band, scale);
        if locked >= INDICATOR_SLOTS {
            return Az3Outcome::Done {
                kind: Az3Kind::SafePins,
                steps: INDICATOR_SLOTS,
                events: pointer.event_count(),
                summary: format!("任务保险 {INDICATOR_SLOTS}/{INDICATOR_SLOTS} 槽已锁定"),
                confirmed: true,
            };
        }

        if pins.pairs.is_empty() {
            unpaired += 1;
        } else {
            unpaired = 0;
        }

        let tol = pins.tolerance();
        for &(target_index, _, distance) in &pins.pairs {
            let Some(target) = pins.targets.get(target_index) else {
                continue;
            };
            let slot = arm.entry(target.x + target.w / 2, tol);
            if distance > tol {
                // 离开容差区：这个目标重新允许按一次。
                slot.1 = true;
                continue;
            }
            if !slot.1 {
                // 从来没离开过容差区（首帧就对齐）⇒ 不许按：那会按在早一格的时机上。
                continue;
            }
            if let Err(error) = pointer.key_space() {
                return aborted(format!("空格注入失败：{error}"), pointer);
            }
            slot.1 = false;
            slot.2 = true;
            presses += 1;
            // 护栏一：按满一格都没锁上 —— 真面板每按一次必然锁一格（指示条逐格变绿）。
            // 按满 5 次却始终 0 槽，只可能是把「像轨道的东西」（黄栏杆 / 地面警示条）当成了
            // 任务保险，此时每一次空格都是游戏里的**跳跃**，必须立刻停手。
            if presses >= INDICATOR_SLOTS && locked == 0 {
                return aborted(
                    format!(
                        "任务保险：按了 {presses} 次空格但没有任何槽锁定，疑似误判界面，已停手"
                    ),
                    pointer,
                );
            }
            // 护栏二：绝对上限。防「locked 偶尔读到 1」让护栏一失效 ——
            // 真面板固定 5 槽，任何超过 10 次的按键都只能是误判。
            if presses > INDICATOR_SLOTS * 2 {
                return aborted(
                    format!("任务保险：按键次数超过上限（{presses} 次），已停手"),
                    pointer,
                );
            }
        }

        // 退化完成判据：指示条一格绿条都读不到，只能以「见过的目标都按过一遍」收尾，
        // 且结论不确认（没有进度证据，可能只是指示条被裁到画面外）。
        if locked == 0 && arm.all_pressed() {
            return Az3Outcome::Done {
                kind: Az3Kind::SafePins,
                steps: presses,
                events: pointer.event_count(),
                summary: format!(
                    "任务保险：{presses} 个目标各按过一次，但指示条绿条读不出（未确认完成）"
                ),
                confirmed: false,
            };
        }

        clock.sleep_ms(poll);
    }

    aborted(
        format!(
            "任务保险超时（已按 {presses} 次 / 已锁定 {locked} 槽 / 未能配对到目标 {unpaired} 帧）"
        ),
        pointer,
    )
}

fn aborted(reason: String, pointer: &impl Pointer) -> Az3Outcome {
    Az3Outcome::Aborted {
        kind: Az3Kind::SafePins,
        reason,
        events: pointer.event_count(),
    }
}
