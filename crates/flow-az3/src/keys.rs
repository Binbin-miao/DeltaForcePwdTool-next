//! 黄键台 / 模块按键台（`ModuleKeys`）：右侧 4×4 按键阵亮一个点一个，左侧屏幕里 5 个进度格。
//!
//! 几何**每帧自适应**（[`locate_keys`]）：固定百分比坐标已证伪 —— 换一版界面布局，
//! 旧配置的矩形就整体偏出去，四等分切出来的格子落在键缝里（点空）。
//!
//! 闭环（全部判据都来自实测录像，见 `.scratch/az3-fix-batch/spec.md`）：
//! 1. 每帧整帧抓**一次**，`locate_keys` 量出 16 颗键的**实测矩形**与屏幕里的进度格；
//! 2. **亮过黄光的模块按钮全都要点**（用户原话：「黄键亮了之后应该先记住位置，然后点击那些位置，
//!    而不是黄键消失后就停下」）—— 黄光会被游戏自己收掉，所以「该点哪些」查**观察记忆**
//!    （[`State::observed`]），不看这一帧还亮不亮；不要求顺序，点过的记 `clicked`；
//!    黄（待点）与绿（已激活）必须是两个谓词：点完变绿就该立刻收手，不许重复点；
//! 3. 点完等 [`CLICK_ACK_TIMEOUT_MS`]：**只有进度推进或变绿勾才算吃下**
//!    （黄光熄灭 ≠ 完成：熄灭也可能只是游戏自己收了高亮），连续 [`DEAD_STRIKES`] 轮无效判 `dead`；
//! 4. 进度**回退**（`checked < peak_checked`）= 游戏换了新一局 / 点了无效键把状态清掉 ⇒ 清账本重来；
//! 5. 左侧进度格读到 **5 个 ✓** → Done。
//!
//! **靶点只有一类可以点**：[`KeyTarget::ModuleButton`]。屏幕内那 5 格是**只读**进度指示器，
//! 永远不发点击（[`KeyTarget::ProgressBox`]）—— 旧实现在屏幕格里挑最亮的去点，
//! 点的是进度指示器本身，一辈子都点不亮。
//!
//! **零值/退化边界**：进度格少于 [`MIN_PROGRESS_BOXES`] 个就读不到进度 —— 此时不判进度、
//! 不判死任何键，只等游戏吃下每次点击，靠「16 格点完 / 亮光全熄灭」收尾，
//! 并在文案里写明用的是退化判据。绝不假装读到了进度，也绝不因为读不到进度就盲点。
//!
//! **退化档收尾 ≠ 完成**：读不到进度格时收尾的结论一律带 `confirmed = false`
//! （[`Az3Outcome::Done`]），留证走 `_unverified`、界面文案写「未确认完成」。
//! 曾经把退化收尾直接报成「已完成」—— 实测只见过 2 颗亮键、点完 2 颗就报完成，
//! 而板子上还剩 3 颗（用户实机反馈）。
//!
//! **过程日志**（[`run_with_seed`] 的 `trace` 出参）：收尾时账本把自己交代清楚 ——
//! 判据档、进度格读到几/共几、点了几下点了哪几格（第几行第几列）、见过哪些格亮过、
//! 哪些格判死、跑了几帧/耗时多少毫秒。只有 `steps` 一个数字时，「见过 2 格」与
//! 「见过 5 格但 3 格判死」在日志里长得一模一样，没人能看出为什么漏点；
//! 缺了帧数/耗时，也看不出「0.2 秒就收工」是点完了还是根本没等到后续闪光。

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
/// 单颗键「已变绿勾」的像素数下限（与 `delta_scene::locate` 的 `SLOT_CHECK_PIXELS` 同源同值）。
///
/// 那个常量在 scene 侧是私有的，跨 crate 取不到；两处必须同步 —— 否则「同一颗键算不算变绿」
/// 在定位层与流程层会给出两个答案。键面比进度格小得多，所以下限取像素而不是占比。
const GREEN_CHECK_PIXELS: u32 = 6;
/// 点击后等待游戏确认（变绿勾 / 进度推进）的上限；超时判定这一轮无效。
///
/// 1080p 实测「点击 → 动画 → 变绿」明显超过 400ms，400ms 会在动画中途判超时、白记 strike。
const CLICK_ACK_TIMEOUT_MS: u64 = 700;
/// 同一颗键连续这么多轮「点了还亮且进度没动」才判死（spec B6 / 用户原话「点击完右边按键
/// 没变绿的要重试点击」）。
///
/// 旧的 2 次就判死 + [`is_spent`] 永真 ⇒ 同一颗键**整局**再也不会被点，用户看到的就是
/// 「点了一下就再也不点了，右边按键一直没变绿」。4 次 = 3 次重新尝试，
/// 覆盖实测那 700ms 内的动画抖动/站位微调，同时又留着上限不死磕一颗坏键。
///
/// **跨常量约束**（LESSON-007：两个各自合理的数字相乘会变死代码）：16 颗键各重试满
/// `DEAD_STRIKES` 轮 = `16 × 4 × 2 = 128` 个输入事件，必须**装得进**正常档安全阀
/// `AZ3_MAX_INPUT_EVENTS`（现 160）。旧上限 120 装不下 128 ⇒ 「16 格全亮且进度永不动」
/// 那一局会先撞安全阀报「疑似误判」，把一局**合法但没解开**的收尾说成「多半不是键台」。
/// 160 仍远低于实测误判乱点（344 / 330 / 370）：真跑飞时远远越过它。
pub(crate) const DEAD_STRIKES: u8 = 4;
/// 连续多少帧定位失败才放弃（`Az3Config::poll_interval_ms` 默认 50ms ⇒ 20 帧 ≈1.0s 轮询，
/// 加上每帧抓帧 + 判定的开销，墙钟约 1.2~1.4s）。
///
/// 旧的 6 帧（≈0.3s）在用户「按下热键那一瞬间手还在动、站位还在微调」时必然放弃 ——
/// 于是每次都得按第二次 F。同仓 `flow-console` 给的是 5~6s，这里取 20 帧作第一步。
const LOCATE_FAIL_LIMIT: u32 = 20;
/// 少于这么多个进度格就读不到进度（退化判据）。
const MIN_PROGRESS_BOXES: usize = 3;
/// 退化模式下最多允许「盘面被重置」几次（防死循环兜底，spec §3 t2）。
const MAX_DEGENERATE_RESETS: u8 = 3;
/// **收工静默期**：最后一次「看到新亮键」之后至少再等这么久，才允许按退化判据收尾。
///
/// 这个数字来自用户实机录屏的逐帧统计（面板出现起算，左侧进度）：
/// | 时刻 | 进度 |
/// |---|---|
/// | 93.4s | 0 勾（面板刚成形） |
/// | 94.0s | 1 勾 |
/// | 95.0s | 2 勾 |
/// | 95.8s | 3 勾 |
/// | 96.3s 起 | 3 勾不动，程序已停手 |
///
/// 三次增加之间隔 **0.6s / 1.0s / 0.8s**：这台机器**一次只亮一个键**，点完才亮下一个。
/// 旧实现点完手上这颗后，只要**一帧**（50ms）没看到新亮键就收工 —— 容错窗口比游戏节奏
/// 短一个数量级，所以每局都卡在第 3 颗左右（不是偶发）。
/// 取 2000ms = 实测最大间隔 1.0s 的 2 倍余量，也是 `CLICK_ACK_TIMEOUT_MS` 的近 3 倍：
/// 宁可多等 2 秒（预算 15s 够等 7 次），也不要把「还没亮」误判成「已经点完」。
const QUIET_MS: u64 = 2_000;
/// 定位失败时给用户的下一步动作。
const POSE_HINT: &str = "（请正对按键面板，让屏幕和 4×4 按键都进入画面）";
/// 进度格读不到时的收尾说明（这台机器可能真的读不出来）。
const PROGRESS_NOTE: &str = "；退化判据：进度格未识别，本条结论依据「绿勾确认 / 在册待点键点完」";

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
    let mut trace = String::new();
    run_with_seed(screen, pointer, clock, az3, cancel, None, &mut trace)
}

/// 带「开跑前那一帧」的入口：那一帧里已经亮着的键也要进观察记忆。
///
/// `seed` = 就绪等待确认的那一帧。存在的理由：按 F 的瞬间黄键正好亮着，
/// 而这一帧通常在新建任务会话之前就抓完了 —— 丢了它，那颗键要等它下次亮才被记住。
/// 定位失败就跳过（绝不 panic，也绝不因此放弃任务）。
///
/// `trace` = **过程日志正文**的出参（多行，见 [`State::evidence`]）：无论收尾是
/// `Done` 还是 `Aborted` 都会被写满，调用方直接拿它落证据日志。原有内容会被覆盖。
pub fn run_with_seed(
    screen: &mut impl Screen,
    pointer: &mut impl Pointer,
    clock: &mut impl Clock,
    az3: &PxAz3,
    cancel: &AtomicBool,
    seed: Option<&Frame>,
    trace: &mut String,
) -> Az3Outcome {
    let mut state = State::default();
    let rect = screen.frame_rect();
    if rect.w <= 0 || rect.h <= 0 {
        return finish(
            &state,
            aborted(
                pointer.event_count(),
                "黄键台需要整幅画面做自适应定位，当前抓帧方式拿不到整帧范围".to_owned(),
            ),
            trace,
        );
    }

    let poll = az3.poll_interval_ms.max(1);
    let began = clock.now_ms();
    // 实机分辨率系数（1080p → 1.5）：像素数阈值按它的平方放大。
    let scale = crate::screen_scale(screen);
    // 全局事件计数是**进程级累计量**，必须减基线才是「本任务发了多少」。
    let event_baseline = pointer.event_count();
    if let Some(frame) = seed {
        state.observe_seed(frame, scale, began);
    }

    for _ in 0..az3.frame_budget.max(1) {
        if cancel.load(Ordering::SeqCst) {
            return finish(
                &state,
                aborted(pointer.event_count(), "已取消".to_owned()),
                trace,
            );
        }
        if clock.now_ms().saturating_sub(began) >= az3.max_duration_ms {
            break;
        }
        let frame = match screen.grab(rect) {
            Ok(frame) => frame,
            Err(error) => {
                return finish(
                    &state,
                    aborted(pointer.event_count(), format!("黄键台抓帧失败：{error}")),
                    trace,
                )
            }
        };
        // `locate_keys` 内部把帧归一到 720p 判定、坐标再乘回原图，所以这里拿到的是
        // **原图坐标**，必须配**原帧**统计像素。像素数阈值是 720p 口径，
        // 1080p 下同一块区域像素多 2.25 倍 —— 按 `scale²` 放大才对得上。
        if let Some(outcome) = state.step(&frame, scale, pointer, clock, event_baseline) {
            return finish(&state, outcome, trace);
        }
        clock.sleep_ms(poll);
    }

    finish(
        &state,
        aborted(
            pointer.event_count(),
            format!(
                "黄键台超时（已点击 {} 次 / {}）{}",
                state.clicks,
                state.progress_text(),
                state.quiet_suffix()
            ),
        ),
        trace,
    )
}

/// 收尾：先把账本写成过程日志正文，再把结论原样返回。
///
/// 留证正文**只有这一个来源** —— 结论（`Done`/`Aborted`）与「凭什么这么判」必须一起离开
/// 本模块，否则日志里只剩四个字（用户实机反馈：日志简陋到看不出为什么漏点）。
fn finish(state: &State, outcome: Az3Outcome, trace: &mut String) -> Az3Outcome {
    *trace = state.evidence(outcome.events());
    outcome
}

/// 一次任务的账本：把「点过哪些格 / 进度峰值 / 定位连败」从主循环里挪出来。
#[derive(Default)]
struct State {
    clicked: [bool; CELLS],
    dead: [bool; CELLS],
    /// 本次任务里**曾经见到它发黄光**的键。只增不减（`reset_board_marks` / 盘面重扫时整体清零）。
    ///
    /// 存在的理由：黄光会被游戏自己收掉，但该键仍然需要被点（用户明确要求）。
    /// 与 `clicked` 的分工：`clicked` = 点过，`observed` = 见过 —— 「本帧亮不亮」与「该不该点」
    /// 是两个不同的问题，混成一个就是「黄键消失后就停下」的根因。
    observed: [bool; CELLS],
    /// 同一颗键「点了还亮着且进度不动」的轮数（到 [`DEAD_STRIKES`] 判死）。
    strikes: [u8; CELLS],
    /// 本轮已点、还没得到游戏回应的键。
    pending: Vec<usize>,
    baseline: usize,
    /// 本局见过的最高进度。`None` = 还没读到过进度读数（与「读到 0」不是一回事）。
    peak_checked: Option<usize>,
    /// 本帧读到的进度；`None` = 进度格不足，处于退化判据。
    checked: Option<usize>,
    click_ms: u64,
    clicks: usize,
    locate_misses: u32,
    /// 退化模式下已经「清账本重来」过几次。
    degenerate_resets: u8,
    /// 本帧定位到几个进度格。`checked` 为 `None` 时也要能说清「是 0 个还是 1~2 个」——
    /// 「一个都没认出来」与「认出 2 个但不够门槛」是两种完全不同的现场。
    progress_boxes: usize,
    /// 进度**回退**（换局 / 状态被清）导致清账本的次数（报告口径）。
    board_resets: u32,
    /// 报告口径：本任务**累计**点过的格。
    ///
    /// 与 `clicked` 的分工：`clicked` 是「当前这局盘面还能不能再点」（重开一局/重扫要清），
    /// 这里是「这次任务到底点过哪几格」（只增不减）。用户问的正是后者。
    touched: [bool; CELLS],
    /// 报告口径：本任务累计**见过发黄光**的格（同上，不随重开一局清零）。
    seen: [bool; CELLS],
    /// 报告口径：本任务累计**判死**的格（同上）。
    dead_ever: [bool; CELLS],
    /// 报告口径：每颗判死的键**被判死前重试了几轮**（同上，不随重开一局清零）。
    ///
    /// 与 `strikes` 的分工：`strikes` 是「本局盘面还差几轮判死」（重开一局必须清零），
    /// 这里是「它到底重试过几次」，供日志的 `判死格位` 括注。少了它，日志只能写「点了 N 次」——
    /// 用户看不出「到底重试过没有」，而那正是这一轮要回答的问题（spec B4）。
    dead_retries: [u8; CELLS],
    /// 最后一次「**从没亮过 → 亮**」的时刻（毫秒）。在册格重复亮不算新消息。
    ///
    /// 收工静默期的起算点：游戏一次只亮一个键，点完手上这颗之后一个新的目标可能
    /// 0.6~1.0s 才出现（实测），所以「这一帧没有可点的键」绝不能立刻当选工信号。
    last_new_lit_ms: Option<u64>,
    /// 本次收尾前为了等静默期而**继续等待**的帧数（报告口径，日志要能区分
    /// 「预算耗尽」与「静默期没等到就超时」）。
    quiet_wait_frames: u32,
    /// 本任务一共推进了几帧（只数 `step` 真正处理过的帧）。
    frames: u32,
    /// 首帧的时刻与收尾前最后一帧的时刻（墙钟毫秒，来自 [`Clock`]）。
    ///
    /// 收尾太快的取证：实测用户那局从面板出现到收工只有约 0.2 秒 —— 退化判据下
    /// 「一帧没看到新亮键就收工」与「真的全点完了」在日志里必须能区分，
    /// 所以帧数与耗时必须落进正文（下一轮直接看得出是不是收尾过早）。
    first_ms: Option<u64>,
    last_ms: Option<u64>,
}

impl State {
    /// 首帧 → 收尾的墙钟耗时（毫秒）；还没推进过任何帧时为 0。
    fn elapsed_ms(&self) -> u64 {
        match (self.first_ms, self.last_ms) {
            (Some(first), Some(last)) => last.saturating_sub(first),
            _ => 0,
        }
    }

    /// 推进一帧；返回 `Some` 表示这一帧就该收尾。
    ///
    /// `event_baseline` = 本任务开始时的进程级事件计数，用来算「本任务发了多少」。
    fn step(
        &mut self,
        frame: &Frame,
        scale: f32,
        pointer: &mut impl Pointer,
        clock: &mut impl Clock,
        event_baseline: u64,
    ) -> Option<Az3Outcome> {
        // 先记账再判定：定位失败的帧也要算进「推进了几帧」，否则「跑了几帧就收工」会被低估。
        let now = clock.now_ms();
        self.frames = self.frames.saturating_add(1);
        self.first_ms.get_or_insert(now);
        self.last_ms = Some(now);
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
        self.progress_boxes = located.progress.len();
        if let Some(current) = checked {
            if self.peak_checked.is_some_and(|peak| current < peak) {
                // 进度回退 = 新一局 / 状态被清：留着旧账本会让新一局「一个键都不点」等到超时。
                self.reset_board_marks(current);
            } else {
                self.peak_checked = Some(current);
            }
            if current >= TOTAL_STAGES {
                let summary = format!("黄键台 {TOTAL_STAGES}/{TOTAL_STAGES} 完成");
                return Some(done(pointer.event_count(), &summary, self.clicks, true));
            }
        }

        // 上一轮点击还没有结论 → 本帧只点**新观察到的**键（`pending` 里的已被 `clicked` 挡住），
        // 但「先记住位置」这件事与等待无关：顺序必须是先并入记忆、再选目标，
        // 否则本帧刚亮的键这一帧就选不到。
        let yellow_lit = yellow_lit_cells(frame, &located.cells, min_glow);
        self.absorb_yellow(&yellow_lit, now);

        // 没有待确认的键就不扫绿勾：16 格像素扫描是热路径上实打实的开销。
        let acked_green = if self.pending.is_empty() {
            Vec::new()
        } else {
            green_cells(frame, &located.cells, crate::scaled_area(GREEN_CHECK_PIXELS, scale))
        };
        let waiting = self.waiting(checked, &acked_green, clock.now_ms());

        let targets = pick_yellow_targets(&located, &self.observed, &self.clicked, &self.dead);
        if targets.is_empty() {
            // 还在等回应时不得判穷尽：那会把「点了不生效」误判成「这一局点完了」。
            if waiting {
                return None;
            }
            // **收工静默期闸**：这台机器一次只亮一个键，点完手上这颗后下一个目标可能
            // 0.6~1.0s 才亮（实测）。旧实现在这里一帧就收工 —— 容错窗口 50ms 比游戏节奏
            // 短一个数量级，所以每局都卡在第 3 颗。这里只延后收尾，零输入、不改选靶。
            if self.inside_quiet_window(now) {
                self.quiet_wait_frames = self.quiet_wait_frames.saturating_add(1);
                return None;
            }
            let reset_candidate = has_unspent_yellow(&yellow_lit, &located, &self.dead);
            return self.on_no_target(checked, reset_candidate, pointer.event_count());
        }

        // 用**当帧**的可读性选档（不缓存第一次的结论）：退化中途进度突然读到了，
        // 立刻切回更严的正常档。误判路径通常「什么都读不全」⇒ 落进退化档；
        // 真面板一定读得到进度格 ⇒ 正常档。分层是为了不误杀「真面板但进度格读不清」。
        let cap = event_cap(checked);
        for (index, at) in targets {
            if let Err(error) = pointer.click(at) {
                return Some(aborted(
                    pointer.event_count(),
                    format!("黄键台点击失败：{error}"),
                ));
            }
            self.clicked[index] = true;
            self.touched[index] = true;
            self.clicks += 1;
            self.pending.push(index);
            // 每发一次点击就判一次上限：真跑飞时（误判成黄键台）必须当场停手，
            // 不能等这一帧把剩余黄键全点完。
            if over_input_cap(pointer.event_count(), event_baseline, cap) {
                return Some(aborted(pointer.event_count(), over_cap_reason(cap)));
            }
        }
        self.baseline = checked.unwrap_or(0);
        self.click_ms = clock.now_ms();
        None
    }

    /// 把「本帧发黄光」的格并入观察记忆（`step` 与 [`run_with_seed`] 共用同一段逻辑）。
    ///
    /// `now` 只用来刷新 [`QUIET_MS`] 静默期的起算点：**只有新成员**（`observed` 从假变真）
    /// 才算「游戏给了新目标」；已经在册的格子重复亮不是新消息，不该把静默期无限推后。
    fn absorb_yellow(&mut self, yellow_lit: &[usize], now: u64) {
        for index in yellow_lit {
            let fresh = self.observed.get(*index).is_some_and(|seen| !*seen);
            if let Some(slot) = self.observed.get_mut(*index) {
                *slot = true;
            }
            if let Some(slot) = self.seen.get_mut(*index) {
                *slot = true;
            }
            if fresh {
                self.last_new_lit_ms = Some(now);
            }
        }
    }

    /// 并入「开跑前那一帧」的观察结果；面板定位不出来就跳过（不 panic、不放弃任务）。
    fn observe_seed(&mut self, frame: &Frame, scale: f32, now: u64) {
        let Ok(located) = locate_keys(frame) else {
            return;
        };
        let min_glow = crate::scaled_area(MIN_GLOW_PIXELS, scale);
        self.absorb_yellow(&yellow_lit_cells(frame, &located.cells, min_glow), now);
    }

    /// 静默期是否还没过（`None` = 一次都没见过亮键 ⇒ 不适用静默期，交给预算兜底）。
    fn inside_quiet_window(&self, now: u64) -> bool {
        self.last_new_lit_ms
            .is_some_and(|last| now.saturating_sub(last) < QUIET_MS)
    }

    /// 超时文案的后缀：把「预算/时长耗尽」与「静默期还没等到就撞上超时」分开写。
    fn quiet_suffix(&self) -> String {
        if self.quiet_wait_frames == 0 {
            String::new()
        } else {
            format!("，其中 {} 帧在等静默期", self.quiet_wait_frames)
        }
    }

    /// 清掉「本局盘面」的标记，重开一局。
    ///
    /// **不清** `clicks` 与 `locate_misses`：那是给用户看的报告口径（一共点过几次、
    /// 丢过几帧），重开一局不该把历史抹掉。
    fn reset_board_marks(&mut self, current_progress: usize) {
        self.clicked = [false; CELLS];
        self.dead = [false; CELLS];
        self.strikes = [0; CELLS];
        // 观察记忆记录的是**这块盘面**的证据：新盘面一到，旧证据必须一起作废（spec B7）。
        self.observed = [false; CELLS];
        self.pending.clear();
        self.baseline = current_progress;
        self.peak_checked = Some(current_progress);
        self.board_resets = self.board_resets.saturating_add(1);
    }

    /// 进度已勾选数；进度格不足 [`MIN_PROGRESS_BOXES`] 个 → `None`（退化，不判进度）。
    fn checked_in(&self, frame: &Frame, located: &LocatedKeys, min_check: u32) -> Option<usize> {
        if located.progress.len() < MIN_PROGRESS_BOXES {
            return None;
        }
        Some(count_checked(frame, &located.progress, min_check))
    }

    /// 处理上一轮点击的回应；返回 `true` = 本帧还没有结论，先别再点**同一批**键。
    ///
    /// **黄光熄灭 ≠ 完成**：熄灭有两种互斥含义 —— ① 我点掉了（那会变绿勾）/ ② 游戏自己收了高亮。
    /// 旧实现把「看不到黄光了」当成「吃下了」，于是没生效的点击也被记成成功（`clicked`
    /// 保持 `true`），那颗键**再也不会被点**。现在只认两种证据：进度推进、变绿勾。
    fn waiting(&mut self, checked: Option<usize>, acked_green: &[usize], now: u64) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        // 判据一（最可靠，能读到进度格时）：进度推进就算这一轮吃下了。
        if checked.is_some_and(|checked| checked > self.baseline) {
            self.pending.clear();
            return false;
        }
        // 判据二：pending 里有格**变绿勾** ⇒ 游戏确认了这一下（不记 strike、不判死）。
        if self.pending.iter().any(|index| acked_green.contains(index)) {
            self.pending.clear();
            return false;
        }
        if now.saturating_sub(self.click_ms) < CLICK_ACK_TIMEOUT_MS {
            return true;
        }
        // 超时仍未变绿：累计无效轮数，到上限判死；没到上限的放回可点池重试（spec B3）。
        for index in std::mem::take(&mut self.pending) {
            self.strikes[index] = self.strikes[index].saturating_add(1);
            if self.strikes[index] >= DEAD_STRIKES {
                self.dead[index] = true;
                self.dead_ever[index] = true;
                self.dead_retries[index] = self.strikes[index];
            } else {
                self.clicked[index] = false;
            }
        }
        false
    }

    /// 这局盘面里**曾经发过黄光**的格数（文案口径：只数在册的键，不是 16 格）。
    fn lit_once_count(&self) -> usize {
        self.observed.iter().filter(|lit| **lit).count()
    }

    /// 没有可点的黄键：要么等下一帧，要么（不能再期待任何黄键）收尾。
    ///
    /// `reset_candidate` = 盘面上还有「亮着、没判死、且可点击」的键，但每一格都已点过 ——
    /// 这只可能是**盘面被重置过**（读不到进度时唯一的观测信号）。退化模式下允许有限次重扫，
    /// 超过 [`MAX_DEGENERATE_RESETS`] 立刻 `Abort`，绝不「无限重试同一批键」。
    fn on_no_target(
        &mut self,
        checked: Option<usize>,
        reset_candidate: bool,
        events: u64,
    ) -> Option<Az3Outcome> {
        // 该点的集合是 `observed`：从没亮过的暗格不需要点，不该拦住收尾（spec A6/B6）。
        // 旧判据要求 16 格全 `clicked`，14 个从未亮过的暗格会永远挡住收尾 ⇒ 一路空转到
        // `frame_budget`(300)，用户看到「超时」，而实际早就点完了。
        //
        // 但「什么都没见过」≠「都点完了」：`observed` 全空说明这一局还没开始亮（首帧、或退化
        // 重扫刚清空账本），此时收尾会把「还没看到」误判成「已经点完」。所以先要求至少见过一颗。
        //
        // 已知边界：本判据隐含「一局内该亮的键在短时内全部出现」（spec L13 实测同盘同亮 5 个）。
        // 「分批点亮」的机关靠的是 [`step`] 里的 [`QUIET_MS`] 静默期闸 —— 走到这里说明
        // 已经「连续 QUIET_MS 没有新键亮起」，才允许按本条收尾。
        let lit_once = self.lit_once_count();
        if lit_once == 0 {
            return None;
        }
        let exhausted = (0..CELLS).all(|index| {
            !self.observed[index] || self.clicked[index] || self.dead[index]
        });
        if !exhausted {
            return None;
        }
        match checked {
            None if reset_candidate && self.degenerate_resets < MAX_DEGENERATE_RESETS => {
                self.degenerate_resets += 1;
                self.clicked = [false; CELLS];
                // 盘面被重置过 ⇒ 旧盘面的黄光证据一起作废，否则会拿旧证据去点新盘面（spec B7）。
                self.observed = [false; CELLS];
                None
            }
            None if reset_candidate => Some(aborted(
                events,
                format!(
                    "黄键台：进度持续回退且读不到进度格（已重扫 {MAX_DEGENERATE_RESETS} 次，已点击 {} 次）",
                    self.clicks
                ),
            )),
            None => {
                // 退化判据：进度读不到，以「在册待点键全点完」收尾，且必须写明用的是哪条判据。
                //
                // **绝不报「已完成」**：本局只可能见过 1~3 颗亮键（闪光采集率不足时另外几颗
                // 根本没进记忆），点完这几颗 != 板子上没有剩余。实测就是这个坑 —— 只见过 2 颗、
                // 点完 2 颗就报完成，板子上还剩 3 颗。所以结论是「未确认完成」，`confirmed = false`。
                let summary = format!(
                    "黄键台 {lit_once} 个亮过的键已全部点过，但未确认完成{PROGRESS_NOTE}"
                );
                Some(done(events, &summary, self.clicks, false))
            }
            Some(checked) => Some(aborted(
                events,
                // 数字同样取**实际在册数**（不是 `CELLS`）：没亮过的键不该被算进「点完」。
                format!("黄键台：{lit_once} 键已点完但进度未满（{checked}/{TOTAL_STAGES}）"),
            )),
        }
    }

    /// 定位失败：允许抖动几帧，超了**立刻收尾且此后零注入**（fail-closed）。
    ///
    /// 面板消失有两种含义，只能按**看得见的证据**分开，不许猜：
    /// - 进度读到 `5/5` → 真的完成了 ⇒ `Done`（`confirmed = true`）；
    /// - 进度读不到、但已经点过键 → 大概率解完被游戏收走 ⇒ `Done`，但**不确认**
    ///   （`confirmed = false`：没有进度证据，可能只是面板被拉远/遮挡）；
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
                true,
            )),
            (None, true) => Some(done(
                events,
                &format!(
                    "黄键台面板已消失（已点击 {} 次），未确认完成{PROGRESS_NOTE}",
                    self.clicks
                ),
                self.clicks,
                false,
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
    ///
    /// 有判死的键时**必须把重试次数一起写出来**（spec B4）：用户原话是「点击完右边按键没变绿
    /// 的要重试点击」，所以「点了一下就放弃」与「重试了 4 次才放弃」在文案里必须是两句话。
    fn progress_text(&self) -> String {
        let progress = match self.checked {
            Some(checked) => format!("进度 {checked}/{TOTAL_STAGES}"),
            None => "进度格未识别，未判进度".to_owned(),
        };
        let dead_count = self.dead_ever.iter().filter(|dead| **dead).count();
        if dead_count == 0 {
            return progress;
        }
        format!(
            "{progress}；判死 {dead_count} 颗：{}",
            dead_list(&self.dead_ever, &self.dead_retries)
        )
    }

    /// 过程日志正文（多行）：把「凭什么收尾 / 到底点了几颗 / 见过与判死哪些格 / 跑了几帧」写全。
    ///
    /// 五类事实必须分开写，因为它们对应完全不同的现场：
    /// 1. **判据档** —— 正常档（读到进度格，5/5 才收尾）还是退化档（读不到，按在册键收尾）；
    /// 2. **进度格** —— 几/共几已勾选；读不到就写「未识别」并给出这次认出几个格子；
    /// 3. **点击数 + 点击格位** —— 一共发了几下、落在第几行第几列（裸下标没人看得懂）；
    /// 4. **见过哪些格 / 判死格位** —— 漏点的第一嫌疑是「压根没采到那颗键的闪光」，
    ///    只有把「见过 2 格」与「见过 5 格但 3 格判死」区分开，才谈得上定位问题；
    ///    判死格位还带**每颗的重试次数**（`（重试 K 次）`）：用户要看见「重试」真的发生过；
    /// 5. **帧数 / 耗时 / 静默等待帧数** —— 实测有「0.2 秒就收工」的局：游戏是分批亮的，
    ///    「跑 4 帧就收工」一眼就能看出可疑；`静默等待` 则把「等到期才收尾」与
    ///    「静默期没等到就撞上超时」区分开（前者 N 帧会明显偏多）。
    fn evidence(&self, events: u64) -> String {
        let tier = match self.checked {
            Some(_) => format!("正常档（读到进度格，{TOTAL_STAGES}/{TOTAL_STAGES} 全勾即完成）"),
            None => "退化档（进度格未识别，判据为「在册待点键全部点完」，结论不确认）".to_owned(),
        };
        let progress = match self.checked {
            Some(checked) => format!(
                "{checked}/{TOTAL_STAGES} 已勾选（本次定位到 {} 格）",
                self.progress_boxes
            ),
            None => format!(
                "未识别（本次只定位到 {} 格 / 共 {TOTAL_STAGES} 格）",
                self.progress_boxes
            ),
        };
        format!(
            "判据档: {tier}\n进度格: {progress}\n点击数: {}（本任务共发出 {events} 个输入事件）\n点击格位: {}\n见过哪些格: {}（共 {} 格）\n判死格位: {}\n耗时: {} 帧 / {}ms（首帧→收尾）\n静默等待: 已等 {} 帧 / {QUIET_MS}ms\n盘面重置: {} 次，退化重扫: {} 次，定位失败帧: {}",
            self.clicks,
            list_cells(&self.touched),
            list_cells(&self.seen),
            self.seen.iter().filter(|lit| **lit).count(),
            dead_list(&self.dead_ever, &self.dead_retries),
            self.frames,
            self.elapsed_ms(),
            self.quiet_wait_frames,
            self.board_resets,
            self.degenerate_resets,
            self.locate_misses,
        )
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

/// 本帧**仍在发黄光**的格序号（已经变绿的完成态不在其中）。
///
/// 它回答的是「本帧谁亮着」——并入观察记忆与盘面重置检测用；**不是选靶依据**。
fn yellow_lit_cells(frame: &Frame, cells: &[PxRect], min_glow: u32) -> Vec<usize> {
    (0..cells.len())
        .filter(|&index| yellow_of(frame, cells, index) >= min_glow)
        .collect()
}

/// 本帧**已经变绿勾**的格序号（与 [`yellow_lit_cells`] 成对：一个答「还亮着」，一个答「已激活」）。
fn green_cells(frame: &Frame, cells: &[PxRect], min_green: u32) -> Vec<usize> {
    (0..cells.len())
        .filter(|&index| is_check_green_cell(frame, cells, index, min_green))
        .collect()
}

/// 这一格是否已经变绿勾（越界返回 `false`：宁可继续等，也不把不存在的格当成已完成）。
fn is_check_green_cell(frame: &Frame, cells: &[PxRect], index: usize, min_green: u32) -> bool {
    let Some(cell) = cells.get(index) else {
        return false;
    };
    geom::count_in(frame, geom::clip(frame, *cell), geom::is_check_green) >= min_green
}

/// 盘面上是否还有「亮着、没判死、且属于可点击靶点」的键。
///
/// 只在「每一格都点过了却还有黄键亮着」时才为真 —— 那就是盘面被重置过的证据。
fn has_unspent_yellow(yellow_lit: &[usize], located: &LocatedKeys, dead: &[bool; CELLS]) -> bool {
    yellow_lit.iter().any(|&index| {
        !dead.get(index).copied().unwrap_or(true)
            && located
                .cells
                .get(index)
                .is_some_and(|cell| classify(*cell, &located.progress).clickable())
    })
}

/// 本帧要点的**全部在册待点**键：曾见过发黄光、没点过、没判死，且属于可点击靶点。
///
/// 只查账本（`observed` / `clicked` / `dead`），**不看这一帧的亮光** —— 黄光会被游戏自己收掉，
/// 用「本帧还亮着」当条件会让熄灭的键永久退出目标池（用户原话：「黄键消失后就停下」）。
/// 用户拍板：这些键**全都要点，不要求顺序**（实测同亮 5 个）；只挑「最亮的一个」会让
/// 5 个亮键互相等待，最后卡在 4/5。
fn pick_yellow_targets(
    located: &LocatedKeys,
    observed: &[bool; CELLS],
    clicked: &[bool; CELLS],
    dead: &[bool; CELLS],
) -> Vec<(usize, PxPoint)> {
    located
        .cells
        .iter()
        .enumerate()
        .filter(|(index, _)| !is_spent(*index, clicked, dead))
        .filter(|(_, cell)| classify(**cell, &located.progress).clickable())
        .filter(|(index, _)| observed.get(*index).copied().unwrap_or(false))
        .map(|(index, cell)| (index, center_of(*cell)))
        .collect()
}

/// 某一格的**纯黄**高亮像素数（越界或已变绿返回 0）。
///
/// 只回答「这一帧亮不亮」，**不代表这一格该被点**（该不该点由观察记忆决定）。
/// 必须只认黄：绿勾判据（[`geom::is_check_green`]）是「这一格已经完成」，
/// 把它算成「还亮着」会让 ack 永远等不到，同一颗键被反复点。
fn yellow_of(frame: &Frame, cells: &[PxRect], index: usize) -> u32 {
    let Some(cell) = cells.get(index) else {
        return 0;
    };
    geom::count_in(frame, geom::clip(frame, *cell), geom::is_yellow_glow)
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

/// 一颗格子的**人话**坐标：`index = 行 * GRID_SIDE + 列`（`LocatedKeys::cells` 是行优先）。
///
/// 日志里绝不写裸下标：用户对着面板数格子，看到「第2行第3列」才能一眼核对。
fn cell_name(index: usize) -> String {
    format!(
        "第{}行第{}列",
        index / GRID_SIDE + 1,
        index % GRID_SIDE + 1
    )
}

/// 把格子标记画成清单（`第1行第2列、第3行第4列`）；一个都没有时写「无」。
fn list_cells(flags: &[bool; CELLS]) -> String {
    let names: Vec<String> = (0..CELLS)
        .filter(|&index| flags.get(index).copied().unwrap_or(false))
        .map(cell_name)
        .collect();
    if names.is_empty() {
        "无".to_owned()
    } else {
        names.join("、")
    }
}

/// 判死清单（`第1行第2列（重试 4 次）`）；一个都没有时写「无」。
///
/// `重试 K 次` 的 K = 这颗键**点了 K 次都没等到变绿勾/进度推进**才被判死的轮数
/// （`strikes` 的终值）。日志里只写「点了 N 次」看不出「到底重试过没有」，
/// 而那正是用户这一轮要问的事（spec B4）。
fn dead_list(dead: &[bool; CELLS], retries: &[u8; CELLS]) -> String {
    let names: Vec<String> = (0..CELLS)
        .filter(|&index| dead.get(index).copied().unwrap_or(false))
        .map(|index| {
            format!(
                "{}（重试 {} 次）",
                cell_name(index),
                retries.get(index).copied().unwrap_or(0)
            )
        })
        .collect();
    if names.is_empty() {
        "无".to_owned()
    } else {
        names.join("、")
    }
}

/// 收尾结论。`confirmed` = 这条结论**有没有进度格证据**：
/// 正常档 5/5 为 `true`；靠退化判据（进度格读不出）收尾一律 `false`，
/// 由调用方落 `_unverified` 证据并写「未确认完成」，绝不冒充成功。
fn done(events: u64, summary: &str, steps: usize, confirmed: bool) -> Az3Outcome {
    Az3Outcome::Done {
        kind: Az3Kind::ModuleKeys,
        steps,
        events,
        summary: summary.to_owned(),
        confirmed,
    }
}

fn aborted(events: u64, reason: String) -> Az3Outcome {
    Az3Outcome::Aborted {
        kind: Az3Kind::ModuleKeys,
        reason,
        events,
    }
}

/// 本帧适用的输入事件上限：进度格读得到 ⇒ 正常档；读不到 ⇒ 退化档。
///
/// 退化档宽松（16 格点完才算一轮，还要允许重扫），正常档收紧（一屏只点几颗键）。
/// **必须按当帧状态判**：退化中途进度突然读到了就立刻切回正常档，绝不沿用旧结论。
fn event_cap(checked: Option<usize>) -> u32 {
    match checked {
        Some(_) => delta_config::AZ3_MAX_INPUT_EVENTS,
        None => delta_config::AZ3_MAX_INPUT_EVENTS_DEGRADED,
    }
}

/// 本任务已发出的输入事件是否到了本帧档位的硬上限（`baseline` = 任务开始时的进程级计数）。
fn over_input_cap(events: u64, baseline: u64, cap: u32) -> bool {
    events.saturating_sub(baseline) >= u64::from(cap)
}

/// 撞上限时的收尾文案：必须说清「这是疑似误判」，不是「这局解不完」。
fn over_cap_reason(cap: u32) -> String {
    format!("疑似误判：已达输入事件上限（{cap}），已停止注入")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const LIT_YELLOW: [u8; 3] = [235, 200, 40];
    const CHECK_GREEN: [u8; 3] = [40, 210, 60];

    fn frame_with_rect(w: u32, h: u32, rect: PxRect, rgb: [u8; 3]) -> Frame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in rect.y.max(0)..rect.bottom().min(h as i32) {
            for x in rect.x.max(0)..rect.right().min(w as i32) {
                let i = ((y as u32 * w + x as u32) * 4) as usize;
                data[i] = rgb[2];
                data[i + 1] = rgb[1];
                data[i + 2] = rgb[0];
                data[i + 3] = 255;
            }
        }
        Frame::new(w, h, data, Instant::now()).expect("合法测试帧")
    }

    #[test]
    fn yellow_of_ignores_an_activated_green_button() {
        // B2：绿键的 yellow_of 必须是 0，否则 ack 判据永远认为「还亮着」。
        let cell = PxRect {
            x: 10,
            y: 10,
            w: 20,
            h: 20,
        };
        let yellow = frame_with_rect(60, 60, cell, LIT_YELLOW);
        let green = frame_with_rect(60, 60, cell, CHECK_GREEN);

        assert_eq!(yellow_of(&yellow, &[cell], 0), 400, "整格亮黄 = 400px");
        assert_eq!(yellow_of(&green, &[cell], 0), 0, "已变绿的键不得算作待点黄键");
    }

    #[test]
    fn green_cells_sees_only_the_activated_key() {
        // 反例必须能失败（LESSON-005）：纯黄键不算绿勾，只有变绿的键才算。
        let cell = PxRect {
            x: 10,
            y: 10,
            w: 20,
            h: 20,
        };
        let yellow = frame_with_rect(60, 60, cell, LIT_YELLOW);
        let green = frame_with_rect(60, 60, cell, CHECK_GREEN);

        assert!(
            green_cells(&yellow, &[cell], GREEN_CHECK_PIXELS).is_empty(),
            "纯黄键不许算成已激活"
        );
        assert_eq!(
            green_cells(&green, &[cell], GREEN_CHECK_PIXELS),
            vec![0],
            "变绿键必须算已激活"
        );
    }

    #[test]
    fn a_dark_but_not_green_key_is_not_acknowledged() {
        // B3：黄光熄灭 + 未变绿 + 进度不动 ⇒ 不许当成吃下（这正是把未点到的键误判成完成的元凶）。
        let mut state = State::default();
        state.clicked[2] = true;
        state.pending.push(2);
        state.click_ms = 100;

        assert!(state.waiting(Some(0), &[], 200), "熄灭但没变绿 ⇒ 必须继续等");
        assert_eq!(state.pending, vec![2], "还没结论，待确认不许清空");
        assert!(state.clicked[2], "还没结论，不许放回可点池重试");
        assert_eq!(state.strikes[2], 0, "等待期内不记 strike");

        // 700ms 后仍没变绿 ⇒ 记一轮无效，未判死则放回可点池重试。
        assert!(!state.waiting(Some(0), &[], 100 + CLICK_ACK_TIMEOUT_MS));
        assert_eq!(state.strikes[2], 1);
        assert!(!state.clicked[2], "未判死必须允许重试");
        assert!(!state.dead[2]);
    }

    #[test]
    fn a_key_that_was_lit_once_is_still_picked_after_the_glow_goes_out() {
        // B1（用户原话）：亮过一帧就记住位置，黄光熄灭那一帧仍然要把它选进目标池。
        let lit = panel_frame(&[7], true);
        let dark = panel_frame(&[], true);
        let lit_cells = locate_keys(&lit).expect("合成面板必须定位成功").cells;
        let located = locate_keys(&dark).expect("合成面板必须定位成功");
        assert!(
            yellow_lit_cells(&dark, &located.cells, MIN_GLOW_PIXELS).is_empty(),
            "前提：这一帧一颗键都不亮"
        );

        let mut state = State::default();
        state.absorb_yellow(&yellow_lit_cells(&lit, &lit_cells, MIN_GLOW_PIXELS), 0);
        assert!(state.observed[7], "亮过就必须记住");

        let targets = pick_yellow_targets(
            &located,
            &state.observed,
            &[false; CELLS],
            &[false; CELLS],
        );
        assert_eq!(targets.len(), 1, "只该有那一颗被记住的键：{targets:?}");
        assert_eq!(targets[0].0, 7, "熄灭的键仍必须被点：{targets:?}");
    }

    #[test]
    fn a_key_that_never_lit_is_never_picked() {
        // B2：从没亮过的暗键永远不许点（防止对暗键乱按）。
        let frame = panel_frame(&[0, 1, 2, 3, 4], true);
        let located = locate_keys(&frame).expect("合成面板必须定位成功");
        let blank = State::default();

        let targets = pick_yellow_targets(
            &located,
            &blank.observed,
            &[false; CELLS],
            &[false; CELLS],
        );
        assert!(targets.is_empty(), "记忆全空时一个都不许点：{targets:?}");

        // 对照：把本帧亮光并入记忆后，正好这 5 颗可点。
        let mut state = State::default();
        state.absorb_yellow(&yellow_lit_cells(&frame, &located.cells, MIN_GLOW_PIXELS), 0);
        let picked: Vec<usize> = pick_yellow_targets(
            &located,
            &state.observed,
            &[false; CELLS],
            &[false; CELLS],
        )
        .into_iter()
        .map(|(index, _)| index)
        .collect();
        assert_eq!(picked, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn waiting_clears_pending_immediately_when_the_button_turns_green() {
        // B3：点后变绿 ⇒ 下一帧就能继续点别的键，不记 strike、不判死。
        let mut state = State::default();
        state.clicked[2] = true;
        state.pending.push(2);
        state.click_ms = 100;

        assert!(!state.waiting(Some(0), &[2], 110), "黄光已熄灭（转绿）⇒ 立刻放行");
        assert!(state.pending.is_empty());
        assert_eq!(state.strikes[2], 0, "正常变绿不该记入超时失误");
        assert!(!state.dead[2], "正常变绿不许判死");
    }

    #[test]
    fn a_progress_regression_clears_the_whole_board() {
        // B4：进度回退 ⇒ clicked / dead / strikes / pending 全清，报告口径（clicks）保留。
        let mut state = State {
            peak_checked: Some(3),
            pending: vec![3],
            clicks: 7,
            ..State::default()
        };
        state.clicked[0] = true;
        state.dead[1] = true;
        state.strikes[2] = 2;

        state.reset_board_marks(0);

        assert!(state.clicked.iter().all(|flag| !flag), "clicked 必须全清");
        assert!(state.dead.iter().all(|flag| !flag), "dead 必须全清");
        assert!(state.strikes.iter().all(|&count| count == 0), "strikes 必须全清");
        assert!(state.pending.is_empty(), "pending 必须清空");
        assert_eq!(state.peak_checked, Some(0), "峰值跟着回到新一局的读数");
        assert_eq!(state.clicks, 7, "已点击总数是报告口径，重开一局不该抹掉");
    }

    #[test]
    fn degenerate_resets_stop_at_the_limit_and_abort() {
        // B5：读不到进度时每次「重置」都是盲重扫，必须有上限 —— 不许死循环。
        let mut state = State {
            observed: [true; CELLS],
            clicked: [true; CELLS],
            ..State::default()
        };
        for _ in 0..MAX_DEGENERATE_RESETS {
            assert!(
                state.on_no_target(None, true, 0).is_none(),
                "上限之内允许清零重扫"
            );
            assert!(state.clicked.iter().all(|flag| !flag));
            // 重扫把账本全清了；模拟「又把这局该点的键都见过并点完」，才能走到上限。
            state.observed = [true; CELLS];
            state.clicked = [true; CELLS];
        }

        let outcome = state.on_no_target(None, true, 0).expect("到上限必须收尾");
        assert!(!outcome.is_done(), "持续回退不是完成：{outcome:?}");
        assert!(
            matches!(&outcome, Az3Outcome::Aborted { reason, .. } if reason.contains("读不到进度格")),
            "文案必须说清是退化判据下的持续回退：{outcome:?}"
        );
    }

    #[test]
    fn a_spent_board_without_a_lit_button_finishes_on_the_degraded_criterion() {
        // 退化模式正常收尾：16 格都见过并点过、且没有黄键还亮着 ⇒ Done，并写明用的是哪条判据。
        let mut state = State {
            observed: [true; CELLS],
            clicked: [true; CELLS],
            ..State::default()
        };
        let outcome = state.on_no_target(None, false, 0).expect("应当收尾");
        assert!(outcome.is_done(), "{outcome:?}");
        assert!(
            matches!(&outcome, Az3Outcome::Done { summary, .. } if summary.contains("退化判据")),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_board_with_only_two_lit_keys_finishes_instead_of_spinning_to_the_budget() {
        // A6/B6：退化路径（读不到进度）下只有 2 格亮过并点完 ⇒ 立刻收尾，不许空转到 frame_budget。
        // 旧判据要求 16 格全 `clicked`，14 个从未亮过的暗格会永远挡住收尾 ⇒ 「点完了却报超时」。
        let mut state = State::default();
        for index in [0, 1] {
            state.observed[index] = true;
            state.clicked[index] = true;
            state.clicks += 1;
        }

        let outcome = state.on_no_target(None, false, 0).expect("在册待点键点完必须立刻收尾");
        match outcome {
            Az3Outcome::Done { steps, summary, .. } => {
                assert_eq!(steps, 2, "报告口径必须是实际点击数");
                assert!(
                    summary.contains("2 个亮过的键"),
                    "文案必须给实际在册数，不许再写 16 键：{summary}"
                );
            }
            other => panic!("退化路径点完必须 Done，实际：{other:?}"),
        }
    }

    #[test]
    fn an_empty_observation_is_not_mistaken_for_a_finished_board() {
        // 反例守卫：什么都没见过 ≠ 都点完了。首帧还没亮键、或退化重扫刚清空账本时收尾，
        // 会把「还没看到」误判成「已经点完」（退化档报完成、正常档报「0 键点完但进度未满」）。
        let mut state = State::default();
        assert!(
            state.on_no_target(None, false, 0).is_none(),
            "一次都没见过亮键时不许收尾"
        );
    }

    #[test]
    fn the_frame_at_the_moment_of_pressing_f_seeds_the_observed_memory() {
        // A12/B11：按 F 那一刻正好亮着的键，必须在开跑前就进记忆（不等下一帧）。
        let frame = panel_frame(&[7], true);
        let located = locate_keys(&frame).expect("合成面板必须定位成功");

        let mut state = State::default();
        state.observe_seed(&frame, 1.0, 0);
        assert!(state.observed[7], "种子帧里亮着的键必须被记住");

        let picked: Vec<usize> =
            pick_yellow_targets(&located, &state.observed, &state.clicked, &state.dead)
                .into_iter()
                .map(|(index, _)| index)
                .collect();
        assert_eq!(picked, vec![7], "种子里的键必须能被选中：{picked:?}");
    }

    #[test]
    fn unspent_yellow_is_only_detected_on_clickable_cells() {
        let progress = vec![PxRect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        }];
        let located = LocatedKeys {
            grid: PxRect::ZERO,
            cells: vec![
                PxRect {
                    x: 100,
                    y: 100,
                    w: 10,
                    h: 10,
                },
                PxRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10,
                },
            ],
            screen: PxRect::ZERO,
            progress,
        };
        assert!(
            has_unspent_yellow(&[0], &located, &[false; CELLS]),
            "可点击的亮键 = 盘面可能被重置"
        );
        assert!(
            !has_unspent_yellow(&[1], &located, &[false; CELLS]),
            "进度指示器上的亮光不是模块按钮，不得据此重扫"
        );
        assert!(
            !has_unspent_yellow(&[0], &located, &[true; CELLS]),
            "已判死的键不算「还有黄键亮着」"
        );
    }

    // ── 输入事件硬上限（分层安全网：正常档 / 退化档）──

    #[test]
    fn the_event_cap_follows_the_current_frame_readability() {
        // 进度格读得到 ⇒ 正常档（一屏只点几颗键）；读不到 ⇒ 退化档（16 格点完才算一轮）。
        assert_eq!(
            event_cap(Some(0)),
            delta_config::AZ3_MAX_INPUT_EVENTS,
            "读得到进度就必须用紧的那一档"
        );
        assert_eq!(
            event_cap(None),
            delta_config::AZ3_MAX_INPUT_EVENTS_DEGRADED,
            "读不到进度只能用松的那一档，否则会误杀退化场景"
        );
        // 经局部变量比较：直接写 `assert!(CONST < CONST)` 会被 clippy 的
        // `assertions_on_constants` 判为无意义断言，而本项目禁用 `#[allow]`。
        let normal = delta_config::AZ3_MAX_INPUT_EVENTS;
        let degraded = delta_config::AZ3_MAX_INPUT_EVENTS_DEGRADED;
        assert!(
            normal < degraded,
            "分层方向必须是「退化更松」（{normal} < {degraded}），写反等于两种场景全误杀"
        );
    }

    #[test]
    fn a_cap_is_only_reached_at_or_above_its_own_limit() {
        // 两档各自在自己的界线上触发：少一个事件不许收手，正好到达必须收手。
        for cap in [
            delta_config::AZ3_MAX_INPUT_EVENTS,
            delta_config::AZ3_MAX_INPUT_EVENTS_DEGRADED,
        ] {
            let baseline = 1_000u64;
            let limit = u64::from(cap);
            assert!(
                !over_input_cap(baseline + limit - 1, baseline, cap),
                "差一个事件时不许收手（cap={cap}）"
            );
            assert!(
                over_input_cap(baseline + limit, baseline, cap),
                "正好到上限必须收手（cap={cap}）"
            );
        }
        // 基线必须被减掉：进程级累计计数（含前面几十次任务）不许把新任务顶到上限。
        assert!(
            !over_input_cap(
                100_000,
                100_000 - 1,
                delta_config::AZ3_MAX_INPUT_EVENTS
            ),
            "只看本任务发出的事件数，绝不看进程累计"
        );
    }

    #[test]
    fn the_cap_reason_names_the_misjudgement_and_the_limit() {
        let reason = over_cap_reason(delta_config::AZ3_MAX_INPUT_EVENTS_DEGRADED);
        assert!(
            reason.contains("疑似误判") && reason.contains("300"),
            "文案必须让用户看出「这是疑似误判」并说清上限：{reason}"
        );
    }

    // ── 合成黄键台 + 三种桩：让两档上限都能在真实 `run` 循环里各触发一次 ──
    //
    // 100% 脱敏的假几何，只复刻「相对结构」（暗键阵 / 左侧屏幕 / 一行进度框）。

    const PANEL_W: u32 = 480;
    const PANEL_H: u32 = 320;
    const MACHINE: [u8; 3] = [152, 152, 150];
    const SCREEN_DARK: [u8; 3] = [30, 32, 34];
    const DARK_KEY: [u8; 3] = [36, 38, 40];
    const BOX_BORDER: [u8; 3] = [200, 200, 200];
    const SCREEN: PxRect = PxRect {
        x: 40,
        y: 60,
        w: 160,
        h: 140,
    };
    const KEY_SIZE: (i32, i32) = (34, 30);
    const COL_X: [i32; 4] = [240, 284, 328, 372];
    const ROW_Y: [i32; 4] = [50, 96, 142, 188];
    const BOX_SIZE: (i32, i32) = (24, 18);
    const BOX_X: [i32; 5] = [48, 76, 104, 132, 160];
    const BOX_Y: i32 = 132;

    fn key_rect(index: usize) -> PxRect {
        PxRect {
            x: COL_X[index % 4],
            y: ROW_Y[index / 4],
            w: KEY_SIZE.0,
            h: KEY_SIZE.1,
        }
    }

    fn box_rect(stage: usize) -> PxRect {
        PxRect {
            x: BOX_X[stage],
            y: BOX_Y,
            w: BOX_SIZE.0,
            h: BOX_SIZE.1,
        }
    }

    fn fill(data: &mut [u8], rect: PxRect, rgb: [u8; 3]) {
        for y in rect.y.max(0)..rect.bottom().min(PANEL_H as i32) {
            for x in rect.x.max(0)..rect.right().min(PANEL_W as i32) {
                let i = ((y as u32 * PANEL_W + x as u32) * 4) as usize;
                data[i..i + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
            }
        }
    }

    /// 合成黄键台帧；`boxes = false` ⇒ 进度框不存在 ⇒ 走**退化档**。
    fn panel_frame(lit: &[usize], boxes: bool) -> Frame {
        panel_frame_checked(lit, 0, boxes)
    }

    /// 同上，但前 `checked` 个进度框内部变绿（= 进度已勾选 `checked`/5）。
    ///
    /// 正常档收尾（5/5 全勾）与「进度读到 4/5」都必须能造出来，否则「`confirmed = true`
    /// 只可能来自进度证据」这条断言就没有反例可校验。
    fn panel_frame_checked(lit: &[usize], checked: usize, boxes: bool) -> Frame {
        let mut data = vec![0u8; (PANEL_W * PANEL_H * 4) as usize];
        fill(
            &mut data,
            PxRect {
                x: 0,
                y: 0,
                w: PANEL_W as i32,
                h: PANEL_H as i32,
            },
            MACHINE,
        );
        fill(&mut data, SCREEN, SCREEN_DARK);
        if boxes {
            for stage in 0..BOX_X.len() {
                let frame = box_rect(stage);
                if stage < checked {
                    // 已勾选：框内整片填绿（像素数远超 `MIN_CHECK_PIXELS`）。
                    fill(
                        &mut data,
                        PxRect {
                            x: frame.x + 2,
                            y: frame.y + 2,
                            w: frame.w - 4,
                            h: frame.h - 4,
                        },
                        CHECK_GREEN,
                    );
                }
                // 细边框（空框 = 未勾选）：够 `readable_progress` 认出 5 个进度格。
                for edge in [
                    PxRect { h: 2, ..frame },
                    PxRect {
                        y: frame.bottom() - 2,
                        h: 2,
                        ..frame
                    },
                    PxRect { w: 2, ..frame },
                    PxRect {
                        x: frame.right() - 2,
                        w: 2,
                        ..frame
                    },
                ] {
                    fill(&mut data, edge, BOX_BORDER);
                }
            }
        }
        for index in 0..CELLS {
            let key = key_rect(index);
            fill(&mut data, key, DARK_KEY);
            if lit.contains(&index) {
                fill(
                    &mut data,
                    PxRect {
                        x: key.x + key.w / 4,
                        y: key.y + key.h / 4,
                        w: key.w / 2,
                        h: key.h / 2,
                    },
                    LIT_YELLOW,
                );
            }
        }
        Frame::new(PANEL_W, PANEL_H, data, Instant::now()).expect("合法测试帧")
    }

    /// 固定帧屏幕桩。
    struct FixedScreen {
        frame: Frame,
    }

    impl Screen for FixedScreen {
        fn grab(&mut self, _rect: PxRect) -> Result<Frame, String> {
            Ok(self.frame.clone())
        }

        fn frame_rect(&self) -> PxRect {
            PxRect {
                x: 0,
                y: 0,
                w: PANEL_W as i32,
                h: PANEL_H as i32,
            }
        }
    }

    /// 「每次点击计多少个事件」可调的假指针。
    ///
    /// 生产一次点击 = 按下 + 抬起 = 2 个事件；这里允许调高，就能在几次点击内跨到上限，
    /// 从而在**真实 `run` 循环**里验证两档上限各自生效，而不必真点 60 次。
    struct CountingPointer {
        per_click: u64,
        emitted: u64,
    }

    impl Pointer for CountingPointer {
        fn click(&mut self, _at: PxPoint) -> Result<(), String> {
            self.emitted += self.per_click;
            Ok(())
        }

        fn key_space(&mut self) -> Result<(), String> {
            self.emitted += self.per_click;
            Ok(())
        }

        fn begin(&mut self, _at: PxPoint) -> Result<(), String> {
            self.emitted += 1;
            Ok(())
        }

        fn step(&mut self, _dx: i32, _dy: i32) -> Result<(), String> {
            self.emitted += 1;
            Ok(())
        }

        fn end(&mut self) -> Result<(), String> {
            self.emitted += 1;
            Ok(())
        }

        fn event_count(&self) -> u64 {
            self.emitted
        }
    }

    struct StepClock {
        now: u64,
    }

    impl Clock for StepClock {
        fn now_ms(&self) -> u64 {
            self.now
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.now += ms;
        }
    }

    fn play_keys(frame: &Frame, per_click: u64, az3: &delta_config::PxAz3) -> Az3Outcome {
        let mut screen = FixedScreen {
            frame: frame.clone(),
        };
        let mut pointer = CountingPointer {
            per_click,
            emitted: 0,
        };
        let mut clock = StepClock { now: 0 };
        let cancel = AtomicBool::new(false);
        run(&mut screen, &mut pointer, &mut clock, az3, &cancel)
    }

    /// 同上，但把**过程日志正文**一起取回（留证正文的唯一来源，见 [`State::evidence`]）。
    fn play_keys_traced(frame: &Frame, az3: &delta_config::PxAz3) -> (Az3Outcome, String) {
        let mut screen = FixedScreen {
            frame: frame.clone(),
        };
        let mut pointer = CountingPointer {
            per_click: 2,
            emitted: 0,
        };
        let mut clock = StepClock { now: 0 };
        let cancel = AtomicBool::new(false);
        let mut trace = String::new();
        let outcome = run_with_seed(
            &mut screen,
            &mut pointer,
            &mut clock,
            az3,
            &cancel,
            None,
            &mut trace,
        );
        (outcome, trace)
    }

    // ── 完成判据分档 + 过程日志正文（用户实测：只点到 3 颗却报「已完成」）──

    #[test]
    fn a_degenerate_finish_is_never_confirmed_even_after_clicking_every_seen_key() {
        // 进度格读不到 ⇒ 只能按「在册待点键点完」收尾。这条结论**没有进度证据**，
        // 必须 `confirmed = false`：只见过 2 颗亮键时点完 2 颗 != 板子上没剩东西
        // （实测只见过 2 颗、板子上还剩 3 颗）。
        let az3 = crate::tests_support::az3();
        let (outcome, trace) = play_keys_traced(&panel_frame(&[0, 5], false), &az3);

        assert!(outcome.is_done(), "{outcome:?}");
        assert!(
            !outcome.is_confirmed(),
            "退化档收尾绝不许自认「已确认」：{outcome:?}"
        );
        assert!(
            matches!(&outcome, Az3Outcome::Done { summary, .. } if summary.contains("未确认完成")),
            "文案必须写明未确认完成：{outcome:?}"
        );
        // 日志正文必须交代清楚「凭什么收尾」。
        assert!(trace.contains("判据档: 退化档"), "{trace}");
        assert!(trace.contains("进度格: 未识别"), "{trace}");
        assert!(
            trace.contains(&format!("点击数: {}", 2 * usize::from(DEAD_STRIKES))),
            "两格各「重试到判死」= DEAD_STRIKES 下：{trace}"
        );
        assert!(
            trace.contains("点击格位: 第1行第1列、第2行第2列"),
            "点击格位必须是「第几行第几列」：{trace}"
        );
        assert!(
            trace.contains(&format!(
                "判死格位: 第1行第1列（重试 {DEAD_STRIKES} 次）、第2行第2列（重试 {DEAD_STRIKES} 次）"
            )),
            "两格都点不动，判死清单必须写明各自重试了几次：{trace}"
        );
        // 实测那局 0.2 秒就收工：帧数/耗时留在正文里，下一轮一眼能看出收尾是否过早。
        let elapsed = trace
            .lines()
            .find(|line| line.starts_with("耗时: "))
            .expect("正文必须给出帧数与耗时");
        assert!(!elapsed.contains("耗时: 0 帧"), "真跑过的任务至少有一帧：{elapsed}");
    }

    #[test]
    fn only_a_five_of_five_progress_reading_confirms_the_finish() {
        // 正常档：进度格读到 5/5 ⇒ 这才是有证据的完成，`confirmed = true`。
        let az3 = crate::tests_support::az3();
        let (outcome, trace) = play_keys_traced(&panel_frame_checked(&[], 5, true), &az3);

        assert!(outcome.is_done(), "{outcome:?}");
        assert!(
            outcome.is_confirmed(),
            "读到 5/5 必须是有证据的完成：{outcome:?}"
        );
        assert!(trace.contains("判据档: 正常档"), "{trace}");
        assert!(trace.contains("进度格: 5/5 已勾选"), "{trace}");
        assert!(trace.contains("点击数: 0"), "5/5 时一下都不该点：{trace}");
    }

    #[test]
    fn the_evidence_body_answers_why_only_two_keys_were_clicked() {
        // 日志正文的四类事实缺一不可：判据档 / 进度格读数 / 点击数+格位 / 见过与判死清单。
        // 缺了「见过哪些格」，「见过 2 格」与「见过 5 格但 3 格判死」在日志里长得一模一样。
        let mut state = State::default();
        for index in [0, 9] {
            state.observed[index] = true;
            state.seen[index] = true;
            state.touched[index] = true;
        }
        state.dead_ever[3] = true;
        state.dead_retries[3] = DEAD_STRIKES;
        state.checked = None;
        state.progress_boxes = 2;
        state.clicks = 2;
        state.frames = 12;
        state.first_ms = Some(1_000);
        state.last_ms = Some(1_550);
        state.quiet_wait_frames = 6;

        let body = state.evidence(4);

        assert!(body.contains("判据档: 退化档"), "{body}");
        assert!(
            body.contains("进度格: 未识别（本次只定位到 2 格 / 共 5 格）"),
            "读不到进度就必须写「未识别」并给出认出几格：{body}"
        );
        assert!(body.contains("点击数: 2（本任务共发出 4 个输入事件）"), "{body}");
        assert!(body.contains("点击格位: 第1行第1列、第3行第2列"), "{body}");
        assert!(
            body.contains("见过哪些格: 第1行第1列、第3行第2列（共 2 格）"),
            "{body}"
        );
        assert!(
            body.contains(&format!("判死格位: 第1行第4列（重试 {DEAD_STRIKES} 次）")),
            "判死格位必须带重试次数，否则看不出「点了一下就放弃」还是「重试过才放弃」：{body}"
        );
        assert!(
            body.contains("耗时: 12 帧 / 550ms（首帧→收尾）"),
            "帧数与耗时必须能看出「跑了几帧就收工」：{body}"
        );
        assert!(
            body.contains(&format!("静默等待: 已等 6 帧 / {QUIET_MS}ms")),
            "静默等待帧数必须落盘，才能区分「预算耗尽」与「静默期没等到」：{body}"
        );
        assert_eq!(body.lines().count(), 9, "九行字段各答一个问题：{body}");
    }

    #[test]
    fn a_progress_reading_of_four_of_five_stays_unconfirmed_and_logs_the_reading() {
        // 读到 4/5 却已经没有可点的键（点过/判死）⇒ Aborted，但日志必须给出进度读数，
        // 用户才能区分「进度没动」与「格子没认出来」。
        let mut state = State {
            observed: [true; CELLS],
            clicked: [true; CELLS],
            checked: Some(TOTAL_STAGES - 1),
            progress_boxes: TOTAL_STAGES,
            ..State::default()
        };
        let outcome = state.on_no_target(Some(TOTAL_STAGES - 1), false, 0).expect("应当收尾");
        assert!(!outcome.is_done(), "{outcome:?}");
        let body = state.evidence(outcome.events());
        assert!(body.contains("判据档: 正常档"), "{body}");
        assert!(body.contains("进度格: 4/5 已勾选"), "{body}");
    }

    // ── 收工静默期（用户录屏逐帧：进度 0→1→2→3 的间隔 0.6s / 1.0s / 0.8s）──
    //
    // 那台机器**一次只亮一个键**，点完才亮下一个。旧实现点完手上这颗后一帧（50ms）没看到
    // 新亮键就收工 ⇒ 每局都卡在第 3 颗。下面四条用例钉住：静默期内不许收工、期满才许收工、
    // 从没亮过不适用静默期、静默期内又亮新键要重新起算。

    /// 一份「盘面已点完、进度读不到」的账本：`cells` 这几格都见过且都点过。
    fn spent_board(cells: &[usize], last_new_lit_ms: u64) -> State {
        let mut state = State::default();
        for index in cells {
            state.observed[*index] = true;
            state.seen[*index] = true;
            state.touched[*index] = true;
            state.clicked[*index] = true;
        }
        state.last_new_lit_ms = Some(last_new_lit_ms);
        state
    }

    /// 直接推一帧账本（不经过 `run` 的预算/时长外壳），便于精确控制时钟。
    fn step_board(state: &mut State, frame: &Frame, clock: &mut StepClock) -> Option<Az3Outcome> {
        let mut pointer = CountingPointer {
            per_click: 2,
            emitted: 0,
        };
        state.step(frame, 1.0, &mut pointer, clock, 0)
    }

    #[test]
    fn a_spent_board_inside_the_quiet_window_is_not_finished() {
        // ① 见过 2 格、都点完，但静默期没过 ⇒ 绝不能收工（这就是用户实测的漏 3 颗）。
        let frame = panel_frame(&[], false);
        let mut state = spent_board(&[0, 5], 0);
        let mut clock = StepClock {
            now: QUIET_MS / 2,
        };

        let outcome = step_board(&mut state, &frame, &mut clock);
        assert!(outcome.is_none(), "静默期未过就收工 = 每局漏 3 颗：{outcome:?}");
        assert_eq!(state.quiet_wait_frames, 1, "等静默期的帧数必须记账");
        assert_eq!(state.clicks, 0, "等静默期必须零输入");
    }

    #[test]
    fn a_spent_board_finishes_only_after_the_quiet_window_has_passed() {
        // ② 期满才允许按退化判据收尾，且必须是 `confirmed = false`（没有进度证据）。
        let frame = panel_frame(&[], false);
        let mut state = spent_board(&[0, 5], 0);
        // 差 1ms 也不行（`QUIET_MS = 0` 时这条会红：证明闸真的按常量在起作用）。
        let mut clock = StepClock {
            now: QUIET_MS.saturating_sub(1),
        };
        assert!(
            step_board(&mut state, &frame, &mut clock).is_none(),
            "差 1ms 就收工说明闸没生效"
        );

        clock.now = QUIET_MS;
        let outcome = step_board(&mut state, &frame, &mut clock).expect("静默期过了应当收尾");
        assert!(outcome.is_done(), "{outcome:?}");
        assert!(
            !outcome.is_confirmed(),
            "退化档收尾不许自认已确认：{outcome:?}"
        );
    }

    #[test]
    fn a_board_that_never_lit_does_not_enter_the_quiet_window() {
        // ③ 一次都没见过亮键 ⇒ 静默期不适用，行为与改动前一致（靠预算兜底，不许白等）。
        let frame = panel_frame(&[], false);
        let mut state = State::default();
        let mut clock = StepClock { now: 0 };
        assert!(step_board(&mut state, &frame, &mut clock).is_none());
        clock.now = 60_000;
        assert!(
            step_board(&mut state, &frame, &mut clock).is_none(),
            "没见过亮键时也不该收尾（旧行为：继续等）"
        );
        assert_eq!(
            state.quiet_wait_frames, 0,
            "从没亮过不该算「在为静默期等待」"
        );
    }

    #[test]
    fn a_new_lit_key_inside_the_quiet_window_restarts_it() {
        // ④ 静默期内又亮了新键 ⇒ 起算点刷新，继续等下一个。
        //
        // 时刻表**不许依赖 `DEAD_STRIKES` 的具体数字**（旧版把 750/1500/2250/3000 刻死，
        // 那套数字是 DEAD_STRIKES=2 的副产物）：这里让新亮的那颗键「再失误一次就判死」，
        // 于是「盘面无靶点」这个前提与常量无关，用例只盯静默期本身。
        let mut state = spent_board(&[0], 0);
        let mut clock = StepClock { now: 0 };

        // t=1900（静默期还剩 100ms）：键 5 新亮 ⇒ 点掉它，起算点刷新到 1900。
        clock.now = 1_900;
        assert!(step_board(&mut state, &panel_frame(&[5], false), &mut clock).is_none());
        assert_eq!(
            state.last_new_lit_ms,
            Some(1_900),
            "见到新亮键必须刷新静默期，否则会按旧起算点提前收工"
        );
        assert!(state.clicked[5], "新亮的键必须当场点掉");

        // t=2600：键 5 这一次超时即判死 ⇒ 盘面无靶点。
        state.strikes[5] = DEAD_STRIKES - 1;
        state.click_ms = 1_900;
        clock.now = 2_600;
        let outcome = step_board(&mut state, &panel_frame(&[], false), &mut clock);
        assert!(state.dead[5], "前提：此刻盘面上已经没有可点的键");
        // 若起算点没刷新（仍是 0），此刻早已过 2000ms 静默期 ⇒ 会立刻收工。
        // 刷新之后静默期到 3900ms 才满 ⇒ 必须继续等。
        assert!(
            outcome.is_none(),
            "新亮键刷新了静默期，此刻还不许收工：{outcome:?}"
        );
        assert_eq!(state.quiet_wait_frames, 1, "必须记成「在等静默期」而不是「无靶点收尾」");
    }

    // ── 定位连败的容错窗口（用户：按下热键时手还在动、站位还在微调）──

    /// 整片纯黑的帧：没有任何暗键候选，`locate_keys` 必然失败。
    fn unlocatable_frame() -> Frame {
        frame_with_rect(PANEL_W, PANEL_H, PxRect::ZERO, [0, 0, 0])
    }

    #[test]
    fn nineteen_locate_misses_are_tolerated_and_the_twentieth_stops_the_run() {
        // 实测旧上限 6 帧（≈0.4s）就收尾：用户按 F 的那一瞬间画面还没定住，于是必按第二次。
        // 这条钉住「容错窗口 = 20 帧」：前 19 帧只记账不收尾，第 20 帧才收尾，且全程零注入。
        let frame = unlocatable_frame();
        let mut state = State::default();
        let mut clock = StepClock { now: 0 };

        for misses in 1..20u32 {
            let outcome = step_board(&mut state, &frame, &mut clock);
            assert!(
                outcome.is_none(),
                "第 {misses} 帧定位失败就收尾 = 容错窗口还是旧的 6 帧：{outcome:?}"
            );
            assert_eq!(state.locate_misses, misses, "失败帧数必须逐帧记账");
        }
        assert_eq!(state.clicks, 0, "定位失败期间绝不注入");

        let outcome = step_board(&mut state, &frame, &mut clock).expect("第 20 帧必须收尾");
        assert!(!outcome.is_done(), "一次没点过就收尾不可能算完成：{outcome:?}");
        assert_eq!(state.locate_misses, 20);
    }

    // ── 点了没变绿必须重试（用户原话：「点击完右边按键没变绿的要重试点击」）──

    #[test]
    fn a_key_that_never_turns_green_is_retried_three_times_before_it_is_given_up_on() {
        // 反例：`DEAD_STRIKES = 2` 时第 2 轮就判死，这条断言在第 2 轮就红。
        let mut state = State::default();
        for round in 1..=3u8 {
            state.clicked[5] = true;
            state.pending.push(5);
            state.click_ms = 0;
            assert!(
                !state.waiting(None, &[], CLICK_ACK_TIMEOUT_MS),
                "第 {round} 轮超时后必须放回可点池，不许直接判死"
            );
            assert!(!state.dead[5], "第 {round} 轮还没到判死线");
            assert!(!state.clicked[5], "第 {round} 轮必须允许重试");
            assert_eq!(state.strikes[5], round, "无效轮数必须逐轮累计");
        }

        state.clicked[5] = true;
        state.pending.push(5);
        state.click_ms = 0;
        assert!(!state.waiting(None, &[], CLICK_ACK_TIMEOUT_MS));
        assert!(state.dead[5], "第 4 轮才判死（旧值 2 会在第 2 轮就走到这里）");
        assert_eq!(
            state.dead_retries[5], 4,
            "判死时必须记下重试过几次，否则日志只能写「点了 N 次」"
        );
    }

    #[test]
    fn each_cap_tier_stops_at_its_own_limit_in_a_real_run() {
        let az3 = crate::tests_support::az3();
        let normal = delta_config::AZ3_MAX_INPUT_EVENTS;
        let degraded = delta_config::AZ3_MAX_INPUT_EVENTS_DEGRADED;

        // ① 正常档：进度格读得到，一次点击就跨到 normal ⇒ 当场收手并报「疑似误判」。
        let outcome = play_keys(&panel_frame(&[0, 1, 2, 3, 4], true), u64::from(normal), &az3);
        assert!(
            matches!(&outcome, Az3Outcome::Aborted { reason, .. }
                if reason.contains("疑似误判") && reason.contains(&normal.to_string())),
            "正常档必须在 {normal} 处收手：{outcome:?}"
        );

        // ② 同一个点击成本，但**进度格读不到** ⇒ 退化档更松，不许在 normal 处收手
        //    （证明分档真的按当帧可读性选，而不是写死一个数）。
        let mut quick = az3.clone();
        quick.max_duration_ms = 600;
        let outcome = play_keys(&panel_frame(&[0], false), u64::from(normal), &quick);
        assert!(
            !format!("{outcome:?}").contains("疑似误判"),
            "刚过 {normal} 个事件时退化档必须放行：{outcome:?}"
        );

        // ③ 退化档在自己的界线上收手。
        let outcome = play_keys(&panel_frame(&[0, 1, 2, 3, 4], false), u64::from(degraded), &az3);
        assert!(
            matches!(&outcome, Az3Outcome::Aborted { reason, .. }
                if reason.contains("疑似误判") && reason.contains(&degraded.to_string())),
            "退化档必须在 {degraded} 处收手：{outcome:?}"
        );
    }
}
