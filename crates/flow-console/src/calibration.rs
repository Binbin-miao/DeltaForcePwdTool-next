//! 会话内自举标定（spec §2.3「运行时自举」表）。
//!
//! 六项未知里与本结构相关的四项：拖动灵敏度、拖动方向、横拖是否切列、停留确认时长。
//! 全部由**实测观测**驱动，默认值只作为第一次试探的假设，不参与最终判定。

use crate::params::ConsoleParams;

/// 横向换列策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnSwitch {
    /// 假设横拖能切列；若实测无效，切换到 [`ColumnSwitch::AutoAdvance`]。
    Drag,
    /// 实测横拖不切列 —— 改为等游戏自动跳列（spec §2.3）。
    AutoAdvance,
}

/// 一次操作台会话内的自举状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    /// `Δy_panel / Δy_mouse`；`None` 表示还没做过试探。
    pub gain: Option<f32>,
    /// 纵向映射符号：`Δpanel_y = gain * vertical_dir * dy_mouse`。
    pub vertical_dir: i32,
    /// 横向映射符号：`Δcolumn = horizontal_dir * 拖动方向`。
    pub horizontal_dir: i32,
    /// 换列策略。
    pub column_switch: ColumnSwitch,
    /// 列推进符号（+1 = 接受后列索引增大）。
    pub column_progress: i32,
    /// 当前停留确认时长。
    pub dwell_ms: u64,
    /// 是否已做过「点面板取焦」这一次性补救。
    pub focused: bool,
    /// 已完成的试探次数。
    pub probes: u32,
}

impl Calibration {
    /// 以「+1 方向、无增益、横拖可切列、停留取配置默认」的初始假设开始。
    pub fn new(params: &ConsoleParams) -> Self {
        Self {
            gain: None,
            vertical_dir: 1,
            horizontal_dir: 1,
            column_switch: ColumnSwitch::Drag,
            column_progress: 1,
            dwell_ms: params.dwell_ms,
            focused: false,
            probes: 0,
        }
    }

    /// 用一次试探的实测结果更新灵敏度与方向。
    ///
    /// 返回 `false` 表示**这次拖动完全没有反应**（`Δy_panel == 0`）—— 调用方据此走
    /// 「先点面板取焦再试一次」的补救，或判定 `DragNoEffect`。
    pub fn observe(&mut self, dy_mouse: i32, dy_panel: i32, params: &ConsoleParams) -> bool {
        if dy_mouse == 0 || dy_panel == 0 {
            return false;
        }
        self.probes = self.probes.saturating_add(1);
        let ratio = dy_panel.unsigned_abs() as f32 / dy_mouse.unsigned_abs() as f32;
        self.gain = Some(ratio.clamp(params.min_gain, params.max_gain));
        self.vertical_dir = if (dy_panel > 0) == (dy_mouse > 0) {
            1
        } else {
            -1
        };
        true
    }

    /// 由「剩余面板位移」反算鼠标位移：`|鼠标| = clamp(|剩余| / 增益, [min,max])`。
    pub fn vertical_step(&self, remaining_px: i32, params: &ConsoleParams) -> i32 {
        if remaining_px == 0 {
            return 0;
        }
        let magnitude = match self.gain {
            Some(gain) => (remaining_px.unsigned_abs() as f32 / gain).round() as i32,
            // 还没自举出增益：按 spec §2.3 走一次固定试探位移。
            None => params.probe_px,
        }
        .clamp(params.min_step_px, params.max_step_px);
        let direction = if remaining_px > 0 {
            self.vertical_dir
        } else {
            -self.vertical_dir
        };
        direction * magnitude
    }

    /// Verify 未见推进时递增停留时长（spec §2.3：+300ms，上限 2400ms）。
    pub fn lengthen_dwell(&mut self, params: &ConsoleParams) {
        self.dwell_ms = self
            .dwell_ms
            .saturating_add(params.dwell_step_ms)
            .min(params.dwell_max_ms);
    }

    /// 新的一列开始：停留时长回到配置默认值。
    pub fn reset_dwell(&mut self, params: &ConsoleParams) {
        self.dwell_ms = params.dwell_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_and_direction_are_learned_from_one_probe() {
        let params = ConsoleParams::default();
        let mut calibration = Calibration::new(&params);
        assert_eq!(calibration.gain, None);
        // 鼠标下移 20px，面板下移 30px → 增益 1.5，同向。
        assert!(calibration.observe(20, 30, &params));
        assert_eq!(calibration.gain, Some(1.5));
        assert_eq!(calibration.vertical_dir, 1);
        assert_eq!(calibration.probes, 1);

        // 反向映射：鼠标下移，面板上移 → 方向取负。
        let mut reversed = Calibration::new(&params);
        assert!(reversed.observe(20, -30, &params));
        assert_eq!(reversed.vertical_dir, -1);
        // 需要「面板下移」时，鼠标必须上移。
        assert_eq!(reversed.vertical_step(150, &params), -20);
    }

    #[test]
    fn zero_response_is_reported_not_smuggled_into_a_gain() {
        let params = ConsoleParams::default();
        let mut calibration = Calibration::new(&params);
        assert!(!calibration.observe(20, 0, &params));
        assert_eq!(calibration.gain, None);
        assert_eq!(calibration.probes, 0);
    }

    #[test]
    fn step_is_clamped_to_the_configured_band() {
        let params = ConsoleParams::default();
        let mut calibration = Calibration::new(&params);
        calibration.observe(20, 40, &params); // 增益 2.0
                                              // 剩余 1000px → 1000/2=500 → 夹到 20。
        assert_eq!(calibration.vertical_step(1000, &params), 20);
        // 剩余 4px → 4/2=2 → 抬到下限 4。
        assert_eq!(calibration.vertical_step(4, &params), 4);
        // 未自举时走固定试探位移。
        let fresh = Calibration::new(&params);
        assert_eq!(fresh.vertical_step(1000, &params), params.probe_px);
        assert_eq!(fresh.vertical_step(0, &params), 0);
    }

    #[test]
    fn dwell_grows_by_300_and_stops_at_the_ceiling() {
        let params = ConsoleParams::default();
        let mut calibration = Calibration::new(&params);
        assert_eq!(calibration.dwell_ms, 900);
        calibration.lengthen_dwell(&params);
        assert_eq!(calibration.dwell_ms, 1_200);
        calibration.lengthen_dwell(&params);
        assert_eq!(calibration.dwell_ms, 1_500);
        for _ in 0..10 {
            calibration.lengthen_dwell(&params);
        }
        assert_eq!(calibration.dwell_ms, params.dwell_max_ms);
        calibration.reset_dwell(&params);
        assert_eq!(calibration.dwell_ms, 900);
    }
}
