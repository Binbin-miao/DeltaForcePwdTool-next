//! 操作台旋钮闭环状态机（spec §2.3 全量）。
//!
//! ```text
//! Detect → Parse → Plan → ColumnAlign → RowAlign → Dwell → Verify
//!                            ↑                                  │
//!                            └──── 该列已接受（斜纹/列推进）──────┘
//! Verify: 面板青色占比 < 8% → Done（立即停手）
//! ```
//!
//! 状态推进是**单一返回点**结构：所有 `Done`/`Aborted` 都由 [`Session::execute`] 的
//! `match` 分支返回，因此「完成后不再发输入」「Aborted 不自动重试」是结构保证，而不是纪律要求。

use delta_config::PxConsole;
use delta_geom::PxPoint;
use thiserror::Error;

use crate::calibration::{Calibration, ColumnSwitch};
use crate::panel::{analyze_panel, knob_center, knob_to_grid, PanelObservation};
use crate::params::{
    parse_led_sequence, ConsoleParams, TargetSequence, CONSOLE_COLUMNS, CONSOLE_ROWS,
};
use crate::{Clock, ConsoleError, Pointer, Screen};

/// 状态机的全部状态（含终态）。UI 可读它做进度显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleState {
    /// 空闲：热键/场景判别刚命中，尚未开始。
    Idle,
    /// 捕获并定位面板 / 行标签 / LED / 旋钮。
    Detect,
    /// 读 LED 4 组 + 黄块 `(列, 行)`。
    Parse,
    /// 列 → 目标行映射。
    Plan,
    /// 横拖把黄块送到目标列。
    ColumnAlign,
    /// 纵拖把黄块送到目标行。
    RowAlign,
    /// 松开后静置 `dwell_ms`。
    Dwell,
    /// 重捕获判定：面板消失 / 斜纹出现 / 列推进 / 无变化。
    Verify,
    /// 终态：面板已消失。
    Done,
    /// 终态：放弃，**禁止自动重试**，需用户再按键重入。
    Aborted,
}

/// `Aborted` 的结构化原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AbortReason {
    #[error("LED 目标序列解析失败（要求 4 组 × 2 位，仅 0/1）")]
    ParseFailed,
    #[error("拖动对旋钮没有任何影响（含取焦重试后仍无效）")]
    DragNoEffect,
    #[error("停留确认后未见推进，重试次数用尽")]
    DwellTimeout,
    #[error("单列超过配置的单列超时上限仍未收敛")]
    ColumnTimeout,
    #[error("单列步数超过上限")]
    StepOverflow,
    #[error("超过全局超时上限")]
    GlobalTimeout,
    #[error("未定位到操作台面板")]
    NotDetected,
}

/// 一次闭环的终局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleOutcome {
    /// 主判据命中：面板整体消失（spec §7.4）。
    Done {
        /// 已确认被接受的列数（面板消失时最后一列可能未经斜纹确认，故可能小于 4）。
        columns: usize,
        /// 返回时的累计输入事件数；调用方可据此断言之后不再增长。
        events: u64,
    },
    /// 放弃。调用方**不得**自动重试。
    Aborted {
        reason: AbortReason,
        columns: usize,
        elapsed_ms: u64,
        events: u64,
    },
}

impl ConsoleOutcome {
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done { .. })
    }

    pub fn abort_reason(&self) -> Option<AbortReason> {
        match self {
            Self::Done { .. } => None,
            Self::Aborted { reason, .. } => Some(*reason),
        }
    }

    pub fn events(&self) -> u64 {
        match self {
            Self::Done { events, .. } | Self::Aborted { events, .. } => *events,
        }
    }
}

/// 执行一次完整的操作台闭环。
///
/// 返回 `Err` 仅表示**配置不可用**（未校准），此时一帧都没抓、一个事件都没发。
pub fn run(
    screen: &mut dyn Screen,
    pointer: &mut dyn Pointer,
    clock: &mut dyn Clock,
    view: &PxConsole,
    params: &ConsoleParams,
) -> Result<ConsoleOutcome, ConsoleError> {
    validate_view(view)?;
    Ok(Session::new(screen, pointer, clock, view.clone(), *params).execute())
}

/// 校准门槛：区域面积为 0 或列/行坐标不足 4 个即拒绝启动（spec D9）。
pub(crate) fn validate_view(view: &PxConsole) -> Result<(), ConsoleError> {
    if view.panel_region.is_empty() {
        return Err(ConsoleError::RegionEmpty { region: "面板" });
    }
    if view.led_region.is_empty() {
        return Err(ConsoleError::RegionEmpty { region: "LED" });
    }
    if view.col_x.len() != CONSOLE_COLUMNS {
        return Err(ConsoleError::ColumnCount {
            found: view.col_x.len(),
            expected: CONSOLE_COLUMNS,
        });
    }
    if view.row_y.len() != CONSOLE_ROWS {
        return Err(ConsoleError::RowCount {
            found: view.row_y.len(),
            expected: CONSOLE_ROWS,
        });
    }
    Ok(())
}

/// 黄块的完整定位结果（绝对屏幕坐标 + 网格索引）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KnobFix {
    center: PxPoint,
    column: usize,
    row: usize,
}

/// 一列的目标：列索引 + 目标行索引。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ColumnTarget {
    column: usize,
    row: usize,
}

/// 单次观测的三态：找到旋钮 / 面板已消失 / 丢失（抓帧失败或认不出旋钮）。
enum Observe {
    Fixed(KnobFix),
    Gone,
    Lost,
}

/// 一次对齐驱动的终局。
enum AlignResult {
    /// 已到达目标（行列都在容差内）。
    Reached,
    /// 面板已消失 —— 立即 `Done`。
    Gone,
    /// 本次策略无效，回 `ColumnAlign` 重新决策（例如切到「等自动跳列」）。
    Retry,
    /// 失败。
    Failed(AbortReason),
}

/// 状态推进结果。
enum Step {
    Next(ConsoleState),
    Done,
    Abort(AbortReason),
}

/// 开环对齐的最大轮数（spec D7：拖完截一张验证，误差超容差才补下一轮，最多 3 轮）。
const ALIGN_ROUNDS: u32 = 3;

struct Session<'a> {
    screen: &'a mut dyn Screen,
    pointer: &'a mut dyn Pointer,
    clock: &'a mut dyn Clock,
    view: PxConsole,
    params: ConsoleParams,
    calibration: Calibration,
    sequence: Option<TargetSequence>,
    pending: Vec<ColumnTarget>,
    knob: Option<KnobFix>,
    cursor: usize,
    started_ms: u64,
    column_deadline_ms: u64,
    parse_started_ms: u64,
    parse_attempts: u32,
    retries: u32,
    steps: u32,
    accepted: usize,
    abort_reason: AbortReason,
}

impl<'a> Session<'a> {
    fn new(
        screen: &'a mut dyn Screen,
        pointer: &'a mut dyn Pointer,
        clock: &'a mut dyn Clock,
        view: PxConsole,
        params: ConsoleParams,
    ) -> Self {
        let started_ms = clock.now_ms();
        Self {
            screen,
            pointer,
            clock,
            view,
            params,
            calibration: Calibration::new(&params),
            sequence: None,
            pending: Vec::with_capacity(CONSOLE_COLUMNS),
            knob: None,
            cursor: 0,
            started_ms,
            column_deadline_ms: started_ms,
            parse_started_ms: started_ms,
            parse_attempts: 0,
            retries: 0,
            steps: 0,
            accepted: 0,
            abort_reason: AbortReason::NotDetected,
        }
    }

    fn execute(mut self) -> ConsoleOutcome {
        let mut state = ConsoleState::Detect;
        loop {
            if self.elapsed_ms() > self.params.global_timeout_ms {
                return self.finish_abort(AbortReason::GlobalTimeout);
            }
            let step = match state {
                ConsoleState::Detect => self.step_detect(),
                ConsoleState::Parse => self.step_parse(),
                ConsoleState::Plan => self.step_plan(),
                ConsoleState::ColumnAlign => self.step_column_align(),
                ConsoleState::RowAlign => self.step_row_align(),
                ConsoleState::Dwell => self.step_dwell(),
                ConsoleState::Verify => self.step_verify(),
                // 终态只能由 finish_* 写入；闭环内不会读回，保留分支以满足穷尽性。
                ConsoleState::Idle => Step::Abort(AbortReason::NotDetected),
                ConsoleState::Done => Step::Done,
                ConsoleState::Aborted => Step::Abort(self.abort_reason),
            };
            match step {
                Step::Next(next) => state = next,
                Step::Done => return self.finish_done(),
                Step::Abort(reason) => return self.finish_abort(reason),
            }
        }
    }

    // ---------- 状态 ----------

    /// `Detect`：面板青色结构特征必须存在，且旋钮必须能落到校准网格上。
    fn step_detect(&mut self) -> Step {
        let Some(observation) = self.observe_panel() else {
            return Step::Abort(AbortReason::NotDetected);
        };
        if observation.cyan_pct < self.params.panel_gone_cyan_pct {
            return Step::Abort(AbortReason::NotDetected);
        }
        let Some(fix) = self.fix_from(&observation) else {
            return Step::Abort(AbortReason::NotDetected);
        };
        self.knob = Some(fix);
        self.cursor = fix.column;
        self.parse_started_ms = self.clock.now_ms();
        self.parse_attempts = 0;
        Step::Next(ConsoleState::Parse)
    }

    /// `Parse`：读 LED 4 组；空串 / 组数不足 / 非法字符一律 `ParseFailed`，且**绝不拖动**。
    fn step_parse(&mut self) -> Step {
        match self.read_sequence() {
            Ok(sequence) => {
                self.sequence = Some(sequence);
                Step::Next(ConsoleState::Plan)
            }
            Err(_) => {
                self.parse_attempts = self.parse_attempts.saturating_add(1);
                let expired = self.clock.now_ms().saturating_sub(self.parse_started_ms)
                    >= self.params.parse_timeout_ms;
                if self.parse_attempts >= self.params.parse_frames || expired {
                    return Step::Abort(AbortReason::ParseFailed);
                }
                self.clock.sleep_ms(self.params.parse_retry_ms);
                Step::Next(ConsoleState::Parse)
            }
        }
    }

    /// `Plan`：列 → 目标行映射。
    fn step_plan(&mut self) -> Step {
        let Some(sequence) = self.sequence else {
            return Step::Abort(AbortReason::ParseFailed);
        };
        self.pending = (0..CONSOLE_COLUMNS)
            .map(|column| ColumnTarget {
                column,
                row: sequence.row_for(column),
            })
            .collect();
        self.retries = 0;
        self.steps = 0;
        self.arm_column_deadline();
        Step::Next(ConsoleState::ColumnAlign)
    }

    /// `ColumnAlign`：黄块不在目标列 → 横拖；横拖无效则切「等自动跳列」。
    fn step_column_align(&mut self) -> Step {
        let Some(target) = self.pending.first().copied() else {
            return Step::Next(ConsoleState::Verify);
        };
        if self.column_expired() {
            return Step::Abort(AbortReason::ColumnTimeout);
        }
        if self.cursor == target.column {
            return Step::Next(ConsoleState::RowAlign);
        }
        match self.calibration.column_switch {
            ColumnSwitch::Drag => self.drag_to_column(target.column),
            ColumnSwitch::AutoAdvance => self.poll_auto_advance(target.column),
        }
    }

    /// `RowAlign`：开环纵拖（一次到位 → 一张验证 → 超容差才补轮）。
    fn step_row_align(&mut self) -> Step {
        let Some(target) = self.pending.first().copied() else {
            return Step::Next(ConsoleState::Verify);
        };
        if self.column_expired() {
            return Step::Abort(AbortReason::ColumnTimeout);
        }
        let Some(knob) = self.knob else {
            return Step::Abort(AbortReason::NotDetected);
        };
        let target_y = self.view.row_y[target.row];
        if (target_y - knob.center.y).abs() <= self.params.row_tolerance_px {
            return Step::Next(ConsoleState::Dwell);
        }
        if self.calibration.gain.is_none() {
            match self.probe_vertical() {
                AlignResult::Reached => {}
                AlignResult::Gone => return Step::Done,
                AlignResult::Retry => return Step::Next(ConsoleState::ColumnAlign),
                AlignResult::Failed(reason) => return Step::Abort(reason),
            }
        }
        match self.drive_row(target_y) {
            AlignResult::Reached => Step::Next(ConsoleState::Dwell),
            AlignResult::Gone => Step::Done,
            AlignResult::Retry => Step::Next(ConsoleState::ColumnAlign),
            AlignResult::Failed(reason) => Step::Abort(reason),
        }
    }

    /// `Dwell`：松开后静置。时长本身是自举量。
    fn step_dwell(&mut self) -> Step {
        self.clock.sleep_ms(self.calibration.dwell_ms);
        Step::Next(ConsoleState::Verify)
    }

    /// `Verify`：主判据（面板消失）优先，其次斜纹浮层 / 列推进，最后重试。
    fn step_verify(&mut self) -> Step {
        let Some(observation) = self.observe_panel() else {
            return Step::Abort(AbortReason::NotDetected);
        };
        if observation.cyan_pct < self.params.panel_gone_cyan_pct {
            return Step::Done;
        }
        let fix = self.fix_from(&observation);
        if let Some(fix) = fix {
            // 斜纹浮层/亮黄边框与旋钮同属黄色域，会把旋钮质心的像素数撑大、坐标拉偏。
            // 列进度照用（接受判据只看列索引），但**纵向几何基准只在可信帧上更新**：开环估拖距
            // 完全依赖这个基准，一旦被污染就会算出离谱的拖距（闭环时代每步重观测掩盖了这点）。
            if self.knob_trusted(&observation) {
                self.knob = Some(fix);
            }
            self.cursor = fix.column;
        }
        let Some(target) = self.pending.first().copied() else {
            // 4 列全部已接受：只剩「等面板整体消失」这一条路。
            return self.retry_after_stall();
        };
        let advanced = fix.is_some_and(|knob| knob.column != target.column);
        if observation.overlay || advanced {
            self.pending.remove(0);
            self.accepted = self.accepted.saturating_add(1);
            self.retries = 0;
            self.steps = 0;
            if let Some(knob) = fix {
                if knob.column != target.column {
                    // 自举「列推进方向」：接受后列索引往哪边走。
                    self.calibration.column_progress = sign_of(knob.column as i32 - target.column as i32);
                }
            }
            self.calibration.reset_dwell(&self.params);
            self.reorder_pending();
            self.arm_column_deadline();
            return Step::Next(ConsoleState::ColumnAlign);
        }
        if self.column_expired() {
            return Step::Abort(AbortReason::ColumnTimeout);
        }
        self.retry_after_stall()
    }

    // ---------- 子流程 ----------

    /// 纵向试探（spec §2.3）：首拖走 `probe_px`，量出灵敏度与方向；无反应则取焦重试一次。
    ///
    /// 成功返回 [`AlignResult::Reached`]，语义是「已拿到可用增益，可以进入主行程」。
    fn probe_vertical(&mut self) -> AlignResult {
        for attempt in 0..2u32 {
            if attempt > 0 {
                // 首拖无反应 → 点面板中央取焦，**只做一次**。
                if self.calibration.focused {
                    return AlignResult::Failed(AbortReason::DragNoEffect);
                }
                self.calibration.focused = true;
                if self.pointer.click(self.panel_center()).is_err() {
                    return AlignResult::Failed(AbortReason::DragNoEffect);
                }
                self.clock.sleep_ms(self.params.focus_settle_ms);
            }
            let Some(before) = self.knob else {
                return AlignResult::Failed(AbortReason::NotDetected);
            };
            let dy = self.params.probe_px * self.calibration.vertical_dir;
            if self.pointer.begin(before.center).is_err() {
                return AlignResult::Failed(AbortReason::DragNoEffect);
            }
            let moved = self.pointer.step(0, dy);
            let released = self.pointer.end();
            if moved.is_err() || released.is_err() {
                return AlignResult::Failed(AbortReason::DragNoEffect);
            }
            self.steps = self.steps.saturating_add(1);
            match self.observe() {
                Observe::Fixed(after) => {
                    if self
                        .calibration
                        .observe(dy, after.center.y - before.center.y, &self.params)
                    {
                        return AlignResult::Reached;
                    }
                }
                Observe::Gone => return AlignResult::Gone,
                Observe::Lost => return AlignResult::Failed(AbortReason::NotDetected),
            }
        }
        AlignResult::Failed(AbortReason::DragNoEffect)
    }

    /// 纵拖到目标 Y：步长由自举增益反算，夹在 `[min_step, max_step]`。
    fn drive_row(&mut self, target_y: i32) -> AlignResult {
        let Some(knob) = self.knob else {
            return AlignResult::Failed(AbortReason::NotDetected);
        };
        if self.pointer.begin(knob.center).is_err() {
            return AlignResult::Failed(AbortReason::DragNoEffect);
        }
        let result = self.drive_row_pressed(target_y);
        // 收尾抬手：无论成败都必须放开左键，这不是「后续输入」而是安全必需。
        if self.pointer.end().is_err() {
            return AlignResult::Failed(AbortReason::DragNoEffect);
        }
        result
    }

    /// 开环纵拖：用已学到的增益一次算出整段位移 → 切子步全部注入（子步间**不截图**）→
    /// 拖完截**一张**验证；误差超容差才补下一轮，最多 [`ALIGN_ROUNDS`] 轮（spec D7）。
    fn drive_row_pressed(&mut self, target_y: i32) -> AlignResult {
        let mut stalled = 0u32;
        for _ in 0..ALIGN_ROUNDS {
            let Some(before) = self.knob else {
                return AlignResult::Failed(AbortReason::NotDetected);
            };
            let remaining = target_y - before.center.y;
            if remaining.abs() <= self.params.row_tolerance_px {
                return AlignResult::Reached;
            }
            let total = self.calibration.total_step(remaining, &self.params);
            let (dy, count) = plan_steps(total, &self.params);
            if let Err(reason) = self.inject_steps(0, dy, count) {
                return AlignResult::Failed(reason);
            }
            match self.observe() {
                Observe::Fixed(after) => {
                    let injected = dy.saturating_mul(count as i32);
                    let moved = after.center.y - before.center.y;
                    if self.calibration.observe(injected, moved, &self.params) {
                        stalled = 0;
                    } else {
                        stalled = stalled.saturating_add(1);
                        if stalled >= self.params.stall_limit {
                            return AlignResult::Failed(AbortReason::DragNoEffect);
                        }
                    }
                    if (target_y - after.center.y).abs() <= self.params.row_tolerance_px {
                        return AlignResult::Reached;
                    }
                }
                Observe::Gone => return AlignResult::Gone,
                Observe::Lost => return AlignResult::Failed(AbortReason::NotDetected),
            }
        }
        // 三轮仍未进容差：开环估算在这台机器上不可信 —— 报错让用户重入，绝不无限补步。
        AlignResult::Failed(AbortReason::DragNoEffect)
    }

    /// 开环注入：把估算出的一段位移切成 `count` 个子步一次做完；**子步之间不截图**。
    fn inject_steps(&mut self, dx: i32, dy: i32, count: u32) -> Result<(), AbortReason> {
        if count == 0 {
            return Ok(());
        }
        if self.steps.saturating_add(count) > self.params.max_steps {
            return Err(AbortReason::StepOverflow);
        }
        for _ in 0..count {
            if self.pointer.step(dx, dy).is_err() {
                return Err(AbortReason::DragNoEffect);
            }
            self.steps = self.steps.saturating_add(1);
        }
        Ok(())
    }

    /// 横拖到目标列；列索引连续 `horizontal_attempts` 次不变 → 切「等自动跳列」。
    fn drag_to_column(&mut self, target: usize) -> Step {
        let Some(knob) = self.knob else {
            return Step::Abort(AbortReason::NotDetected);
        };
        if self.pointer.begin(knob.center).is_err() {
            return Step::Abort(AbortReason::DragNoEffect);
        }
        let result = self.drag_to_column_pressed(target);
        if self.pointer.end().is_err() {
            return Step::Abort(AbortReason::DragNoEffect);
        }
        match result {
            AlignResult::Reached => Step::Next(ConsoleState::RowAlign),
            AlignResult::Gone => Step::Done,
            AlignResult::Retry => Step::Next(ConsoleState::ColumnAlign),
            AlignResult::Failed(reason) => Step::Abort(reason),
        }
    }

    /// 开环横拖：按「差几列就横拖几个步长」一次到位 → 截**一张**确认；最多 [`ALIGN_ROUNDS`] 轮。
    fn drag_to_column_pressed(&mut self, target: usize) -> AlignResult {
        let mut unchanged = 0u32;
        for _ in 0..ALIGN_ROUNDS {
            let before = self.cursor;
            if before == target {
                return AlignResult::Reached;
            }
            let towards = sign_of(target as i32 - before as i32);
            let dx = self.params.horizontal_step_px * self.calibration.horizontal_dir * towards;
            let count = (target as i32 - before as i32).unsigned_abs();
            if let Err(reason) = self.inject_steps(dx, 0, count) {
                return AlignResult::Failed(reason);
            }
            let fix = match self.observe() {
                Observe::Fixed(fix) => fix,
                Observe::Gone => return AlignResult::Gone,
                Observe::Lost => return AlignResult::Failed(AbortReason::NotDetected),
            };
            if fix.column == before {
                unchanged = unchanged.saturating_add(1);
                if unchanged >= self.params.horizontal_attempts {
                    // 实测横拖不切列 → 换「等自动跳列」策略（spec §2.3）。
                    self.calibration.column_switch = ColumnSwitch::AutoAdvance;
                    return AlignResult::Retry;
                }
                continue;
            }
            // 自举横拖方向：观察列索引变化与注入方向的对应关系。
            self.calibration.horizontal_dir = sign_of(fix.column as i32 - before as i32) * sign_of(dx);
            unchanged = 0;
            if fix.column == target {
                return AlignResult::Reached;
            }
        }
        // 三轮没切到目标列：报错让上层放弃，不无限横拖。
        AlignResult::Failed(AbortReason::DragNoEffect)
    }

    /// 「等自动跳列」：不注入输入，只轮询列索引（spec D7：500ms × 最多 2 次，即最多 2 张）。
    fn poll_auto_advance(&mut self, target: usize) -> Step {
        for _ in 0..self.params.auto_advance_polls {
            self.clock.sleep_ms(self.params.auto_advance_poll_ms);
            if self.column_expired() {
                return Step::Abort(AbortReason::ColumnTimeout);
            }
            match self.observe() {
                Observe::Fixed(fix) if fix.column == target => {
                    return Step::Next(ConsoleState::RowAlign)
                }
                Observe::Fixed(_) => continue,
                Observe::Gone => return Step::Done,
                Observe::Lost => return Step::Abort(AbortReason::NotDetected),
            }
        }
        Step::Abort(AbortReason::DragNoEffect)
    }

    // ---------- 观测与收尾 ----------

    fn observe_panel(&mut self) -> Option<PanelObservation> {
        let frame = self.screen.grab(self.view.panel_region).ok()?;
        Some(analyze_panel(&frame, &self.params))
    }

    fn observe(&mut self) -> Observe {
        let Some(observation) = self.observe_panel() else {
            return Observe::Lost;
        };
        if observation.cyan_pct < self.params.panel_gone_cyan_pct {
            return Observe::Gone;
        }
        match self.fix_from(&observation) {
            Some(fix) => {
                self.knob = Some(fix);
                self.cursor = fix.column;
                Observe::Fixed(fix)
            }
            None => Observe::Lost,
        }
    }

    fn fix_from(&self, observation: &PanelObservation) -> Option<KnobFix> {
        let block = observation.knob.as_ref()?;
        let (column, row) = knob_to_grid(
            block,
            self.view.panel_region,
            &self.view,
            self.params.grid_snap_max_px,
        )?;
        Some(KnobFix {
            center: knob_center(block, self.view.panel_region),
            column,
            row,
        })
    }

    /// 这一帧的旋钮块是否可信（只判上限：下限 `knob_min_pixels` 已在 `analyze_panel` 里把关）。
    fn knob_trusted(&self, observation: &PanelObservation) -> bool {
        observation
            .knob
            .as_ref()
            .is_some_and(|block| block.pixels <= self.params.knob_max_pixels)
    }

    fn read_sequence(&mut self) -> Result<TargetSequence, String> {
        let frame = self.screen.grab(self.view.led_region)?;
        let text = delta_vision::read_glyph_string(&frame.luma()).map_err(|error| error.to_string())?;
        parse_led_sequence(&text).map_err(|error| error.to_string())
    }

    fn panel_center(&self) -> PxPoint {
        PxPoint {
            x: self.view.panel_region.x + self.view.panel_region.w / 2,
            y: self.view.panel_region.y + self.view.panel_region.h / 2,
        }
    }

    /// 按实测的列推进方向把「下一列」提到队首。
    ///
    /// 队首已经是当前列（黄块就停在那里）时保持不动 —— 否则会把下一列误跳到更后面去。
    fn reorder_pending(&mut self) {
        if self.pending.first().is_some_and(|target| target.column == self.cursor) {
            return;
        }
        let wanted = self.cursor as i32 + self.calibration.column_progress;
        if let Some(index) = self
            .pending
            .iter()
            .position(|target| target.column as i32 == wanted)
        {
            self.pending.swap(0, index);
        }
    }

    fn retry_after_stall(&mut self) -> Step {
        if self.retries >= self.params.column_retries {
            return Step::Abort(AbortReason::DwellTimeout);
        }
        self.retries = self.retries.saturating_add(1);
        self.calibration.lengthen_dwell(&self.params);
        Step::Next(ConsoleState::Dwell)
    }

    fn arm_column_deadline(&mut self) {
        self.column_deadline_ms = self.clock.now_ms().saturating_add(self.params.column_timeout_ms);
    }

    fn column_expired(&self) -> bool {
        self.clock.now_ms() > self.column_deadline_ms
    }

    fn elapsed_ms(&self) -> u64 {
        self.clock.now_ms().saturating_sub(self.started_ms)
    }

    fn finish_done(&self) -> ConsoleOutcome {
        ConsoleOutcome::Done {
            columns: self.accepted,
            events: self.pointer.event_count(),
        }
    }

    fn finish_abort(&mut self, reason: AbortReason) -> ConsoleOutcome {
        self.abort_reason = reason;
        ConsoleOutcome::Aborted {
            reason,
            columns: self.accepted,
            elapsed_ms: self.elapsed_ms(),
            events: self.pointer.event_count(),
        }
    }
}

/// 三值符号：负 / 零 / 正 → `-1 / 0 / 1`。
fn sign_of(value: i32) -> i32 {
    value.signum()
}

/// 开环估算：把一次总位移切成「每步大小 × 步数」，每步夹在 `[min_step_px, max_step_px]`。
///
/// 用均分而不是「前面塞满 `max_step`、末尾留零头」：零头会掉到 `min_step_px` 以下，
/// 而低于下限的抖动会被游戏吞掉，导致「明明还差几像素却再也不动」。
fn plan_steps(total: i32, params: &ConsoleParams) -> (i32, u32) {
    let magnitude = total.unsigned_abs();
    if magnitude == 0 {
        return (0, 0);
    }
    let max_step = params.max_step_px.unsigned_abs().max(1);
    let min_step = params.min_step_px.unsigned_abs().max(1);
    let count = magnitude.div_ceil(max_step).max(1);
    let per = magnitude.div_ceil(count).clamp(min_step, max_step);
    (total.signum() * per as i32, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_reason_messages_name_the_actual_gate() {
        assert_eq!(
            AbortReason::ParseFailed.to_string(),
            "LED 目标序列解析失败（要求 4 组 × 2 位，仅 0/1）"
        );
        assert!(AbortReason::ColumnTimeout.to_string().contains("单列超时"));
        assert!(AbortReason::GlobalTimeout.to_string().contains("全局"));
    }

    #[test]
    fn sign_of_is_three_valued() {
        assert_eq!(sign_of(-7), -1);
        assert_eq!(sign_of(0), 0);
        assert_eq!(sign_of(9), 1);
    }

    #[test]
    fn plan_steps_keeps_every_sub_step_inside_the_configured_band() {
        let params = ConsoleParams::default(); // min 4 / max 20
        // 零位移：一个子步都不发。
        assert_eq!(plan_steps(0, &params), (0, 0));
        // 恰好整倍数：3 步 × 20px，一点不浪费。
        assert_eq!(plan_steps(60, &params), (20, 3));
        // 25px：均分成 2 × 13px（而不是 20 + 5 那种零头）。
        assert_eq!(plan_steps(25, &params), (13, 2));
        // 小于下限的位移抬到 min_step_px，宁可多走 1px 也不发无效抖动。
        assert_eq!(plan_steps(3, &params), (4, 1));
        // 负位移保留方向。
        assert_eq!(plan_steps(-60, &params), (-20, 3));
        // 长行程等价于「上限步长 × 步数」。
        assert_eq!(plan_steps(1_000, &params), (20, 50));
    }
}
