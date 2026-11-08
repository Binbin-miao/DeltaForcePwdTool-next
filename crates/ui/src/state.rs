//! 界面状态与展示文案（纯逻辑，不碰 egui 绘制，便于直接单测）。
//!
//! 五态来自 spec D6 / D9：空闲 · 运行中 · 成功 · 未识别 · 超时。
//! 文案一律 ≤6 字、动词开头、零技术词。

use egui::Color32;

use crate::theme;

/// 场景（与 `delta-scene::Scene` 一一对应，UI 侧不反向依赖业务 crate）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneKind {
    Unknown,
    Morse,
    Fingerprint,
    Console,
}

impl SceneKind {
    /// 状态条上显示的短名。
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "未识别",
            Self::Morse => "摩斯解码",
            Self::Fingerprint => "指纹识别",
            Self::Console => "操作台",
        }
    }

    /// 目标序列是否可显示（只有操作台有 4 组 LED 目标）。
    pub fn shows_sequence(self) -> bool {
        matches!(self, Self::Console)
    }
}

/// 五态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
    Success,
    Unrecognized,
    TimedOut,
}

impl Phase {
    /// 状态文案（必须配文字，不靠颜色单独表意）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "待机",
            Self::Running => "识别中",
            Self::Success => "已完成",
            Self::Unrecognized => "未识别",
            Self::TimedOut => "识别超时",
        }
    }

    /// 状态点 / 描边用的状态色。
    pub fn color(self) -> Color32 {
        match self {
            Self::Idle => theme::TEXT_3,
            Self::Running => theme::ACCENT,
            Self::Success => theme::OK,
            Self::Unrecognized => theme::WARN,
            Self::TimedOut => theme::ERR,
        }
    }

    /// 主按钮文案。
    pub fn primary_label(self, hotkey: &str) -> String {
        match self {
            Self::Idle => format!("开始识别  {hotkey}"),
            Self::Running => "取消操作".to_owned(),
            Self::Success | Self::Unrecognized | Self::TimedOut => "重新识别".to_owned(),
        }
    }

    /// 主按钮动作。
    pub fn primary_action(self) -> Action {
        match self {
            Self::Idle | Self::Success | Self::Unrecognized | Self::TimedOut => Action::Start,
            Self::Running => Action::Cancel,
        }
    }

    /// 状态描边 alpha（0.0 = 不画状态描边）。
    ///
    /// - 运行中：1.2s 呼吸（只变外框与状态点，整窗不闪、不弹 toast，不遮挡游戏）；
    /// - 未识别：2Hz 慢闪；
    /// - 超时：静态常亮不闪（提示「别再等了，手动确认」）；
    /// - 成功：200ms 单次高亮后回到常态。
    pub fn status_alpha(self, time: f64) -> f32 {
        match self {
            Self::Idle => 0.0,
            Self::Success => {
                if time < SUCCESS_HIGHLIGHT_SECS {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Running => {
                let phase = (time / BREATH_PERIOD_SECS).rem_euclid(1.0);
                0.35 + 0.35 * (1.0 + (phase * std::f64::consts::TAU).sin()) as f32 / 2.0
            }
            Self::Unrecognized => {
                if time.rem_euclid(BLINK_PERIOD_SECS) < BLINK_PERIOD_SECS / 2.0 {
                    1.0
                } else {
                    0.25
                }
            }
            Self::TimedOut => 1.0,
        }
    }
}

/// 运行中描边呼吸周期（spec：1.2s）。
pub const BREATH_PERIOD_SECS: f64 = 1.2;
/// 未识别慢闪周期（spec：2Hz）。
pub const BLINK_PERIOD_SECS: f64 = 0.5;
/// 成功单次高亮时长（spec：200ms）。
pub const SUCCESS_HIGHLIGHT_SECS: f64 = 0.2;

/// 换分辨率警示文案（≤10 字、动词开头、零技术词）。
///
/// 语义：百分比坐标仍然有效，但绝对像素会偏移 —— 建议重跑一次校准。
pub const RESOLUTION_NOTICE: &str = "屏幕变了 · 建议重校";

/// 用户动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Start,
    Cancel,
    Collapse,
    Expand,
    Quit,
    OpenCalibration,
    OpenSettings,
    AcceptDisclaimer,
    ToggleFlow(SceneKind),
    /// 点掉一次性警示（如「屏幕变了 · 建议重校」）。
    DismissNotice,
}

/// 进度区内容。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub note: String,
}

impl Progress {
    pub fn new(done: usize, total: usize, note: impl Into<String>) -> Self {
        Self {
            done,
            total,
            note: note.into(),
        }
    }

    /// `2/4 · 已停稳`；`total == 0` 时只显示备注。
    pub fn label(&self) -> String {
        if self.total == 0 {
            return self.note.clone();
        }
        if self.note.is_empty() {
            return format!("{}/{}", self.done, self.total);
        }
        format!("{}/{} · {}", self.done, self.total, self.note)
    }

    /// 进度比例（`total == 0` → 0.0，不产生 NaN）。
    pub fn ratio(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
    }
}

/// 界面全量状态。
#[derive(Debug, Clone)]
pub struct UiState {
    pub phase: Phase,
    pub scene: SceneKind,
    /// 操作台目标序列，已按 `01 · 00 · 01 · 11` 排版。
    pub sequence: Option<String>,
    pub progress: Progress,
    /// 结果文案（如「已输入数字」）。
    pub message: String,
    pub collapsed: bool,
    /// 免责声明未确认 → 界面中央显示条款。
    pub disclaimer_pending: bool,
    /// 一次性警示文案（`None` = 不显示）；当前只用于「屏幕变了 · 建议重校」。
    pub notice: Option<String>,
    /// 用户已点掉过警示 → 本会话不再出现。
    pub notice_dismissed: bool,
    /// 主热键标签（如 `F8`）。
    pub hotkey: String,
    /// 未校准项（空 = 已校准）。
    pub missing: Vec<String>,
    /// 三个功能入口的开关。
    pub toggles: Toggles,
    /// 进入当前状态后经过的秒数（成功 200ms 高亮 / 呼吸相位都靠它）。
    pub phase_elapsed: f64,
}

/// 三个功能入口开关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toggles {
    pub morse: bool,
    pub fingerprint: bool,
    pub console: bool,
}

impl Toggles {
    pub fn get(self, scene: SceneKind) -> bool {
        match scene {
            SceneKind::Morse => self.morse,
            SceneKind::Fingerprint => self.fingerprint,
            SceneKind::Console => self.console,
            SceneKind::Unknown => false,
        }
    }

    /// 翻转一个开关；`Unknown` 没有开关可翻。
    pub fn toggle(&mut self, scene: SceneKind) {
        match scene {
            SceneKind::Morse => self.morse = !self.morse,
            SceneKind::Fingerprint => self.fingerprint = !self.fingerprint,
            SceneKind::Console => self.console = !self.console,
            SceneKind::Unknown => {}
        }
    }

    /// 三个入口按 spec 顺序列出。
    pub fn entries(self) -> [(SceneKind, bool); 3] {
        [
            (SceneKind::Morse, self.morse),
            (SceneKind::Fingerprint, self.fingerprint),
            (SceneKind::Console, self.console),
        ]
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            scene: SceneKind::Unknown,
            sequence: None,
            progress: Progress::default(),
            message: String::new(),
            collapsed: false,
            disclaimer_pending: true,
            notice: None,
            notice_dismissed: false,
            hotkey: "F8".to_owned(),
            missing: Vec::new(),
            toggles: Toggles {
                morse: true,
                fingerprint: true,
                console: true,
            },
            phase_elapsed: 0.0,
        }
    }
}

impl UiState {
    /// 是否已校准（`validate()` 为空）。
    pub fn is_calibrated(&self) -> bool {
        self.missing.is_empty()
    }

    /// 主按钮是否可点：免责声明已确认 + 已校准 + 不在运行中。
    pub fn start_enabled(&self) -> bool {
        !self.disclaimer_pending && self.is_calibrated() && self.phase != Phase::Running
    }

    /// 序列显示串：非操作台场景显示 `──`（spec：非操作台场景没有目标序列）。
    pub fn sequence_text(&self) -> String {
        match (&self.sequence, self.scene.shows_sequence()) {
            (Some(sequence), true) => sequence.clone(),
            _ => "──".to_owned(),
        }
    }

    /// 功能入口是否可用（未校准时灰置）。
    pub fn flow_enabled(&self, scene: SceneKind) -> bool {
        self.is_calibrated() && !self.disclaimer_pending && self.toggles.get(scene)
    }

    /// 状态条文案：`待机 · 操作台`。
    pub fn status_line(&self) -> String {
        format!("{} · {}", self.phase.label(), self.scene.label())
    }

    /// 进入运行态（警示让位给进度，否则用户看不见识别进展）。
    pub fn begin_run(&mut self) {
        self.phase = Phase::Running;
        self.progress = Progress::new(0, 0, "");
        self.message.clear();
        self.notice = None;
    }

    /// 装上「屏幕变了」警示：用户已点掉过就不再打扰。
    pub fn warn_resolution_changed(&mut self) {
        if !self.notice_dismissed {
            self.notice = Some(RESOLUTION_NOTICE.to_owned());
        }
    }

    /// 当前应渲染的警示文案。
    pub fn notice_text(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 点掉当前警示：本会话不再出现。
    pub fn dismiss_notice(&mut self) {
        self.notice = None;
        self.notice_dismissed = true;
    }

    /// 收尾成某一态。
    pub fn finish(&mut self, phase: Phase, message: impl Into<String>) {
        self.phase = phase;
        self.message = message.into();
    }
}

/// 把 `01-00-01-11` 排成 `01 · 00 · 01 · 11`；长度不是 4 组 × 2 位时原样返回。
pub fn format_sequence(raw: &str) -> String {
    let groups: Vec<&str> = raw.split('-').collect();
    let well_formed = groups.len() == 4 && groups.iter().all(|group| group.len() == 2);
    if !well_formed {
        return raw.to_owned();
    }
    groups.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_phases_have_distinct_labels_and_colors() {
        let phases = [
            Phase::Idle,
            Phase::Running,
            Phase::Success,
            Phase::Unrecognized,
            Phase::TimedOut,
        ];
        let mut labels: Vec<&str> = phases.iter().map(|phase| phase.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 5, "五态文案必须互异");
        let mut colors: Vec<[u8; 4]> = phases.iter().map(|p| p.color().to_array()).collect();
        colors.sort_unstable();
        colors.dedup();
        assert_eq!(colors.len(), 5, "五态颜色必须互异");
    }

    #[test]
    fn running_border_breathes_within_range() {
        let samples: Vec<f32> = (0..12)
            .map(|step| Phase::Running.status_alpha(step as f64 * 0.1))
            .collect();
        assert!(samples.iter().all(|a| (0.35..=0.7).contains(a)), "{samples:?}");
        let max = samples.iter().cloned().fold(f32::MIN, f32::max);
        let min = samples.iter().cloned().fold(f32::MAX, f32::min);
        assert!(max - min > 0.2, "呼吸幅度过小：{min}~{max}");
    }

    #[test]
    fn unrecognized_blinks_at_two_hertz() {
        // 周期 0.5s：前半亮、后半暗。
        assert!(Phase::Unrecognized.status_alpha(0.1) > Phase::Unrecognized.status_alpha(0.4));
    }

    #[test]
    fn timeout_border_is_static() {
        assert_eq!(Phase::TimedOut.status_alpha(0.1), Phase::TimedOut.status_alpha(3.7));
        assert_eq!(Phase::TimedOut.status_alpha(0.1), 1.0);
    }

    #[test]
    fn idle_draws_no_status_border() {
        assert_eq!(Phase::Idle.status_alpha(1.0), 0.0);
    }

    #[test]
    fn primary_button_follows_phase() {
        assert_eq!(Phase::Idle.primary_label("F8"), "开始识别  F8");
        assert_eq!(Phase::Running.primary_label("F8"), "取消操作");
        assert_eq!(Phase::Success.primary_label("F8"), "重新识别");
        assert_eq!(Phase::Idle.primary_action(), Action::Start);
        assert_eq!(Phase::Running.primary_action(), Action::Cancel);
    }

    #[test]
    fn start_is_blocked_until_disclaimer_and_calibration() {
        let mut state = UiState::default();
        assert!(!state.start_enabled(), "免责声明未确认时必须拒绝");
        state.disclaimer_pending = false;
        assert!(state.start_enabled());
        state.missing = vec!["操作台 LED 目标序列区域".to_owned()];
        assert!(!state.start_enabled(), "未校准时必须灰置");
        state.missing.clear();
        state.begin_run();
        assert!(!state.start_enabled(), "运行中主按钮变取消");
    }

    #[test]
    fn sequence_text_hides_for_non_console_scenes() {
        let mut state = UiState {
            sequence: Some("01 · 00 · 01 · 11".to_owned()),
            ..UiState::default()
        };
        assert_eq!(state.sequence_text(), "──", "摩斯/指纹没有目标序列");
        state.scene = SceneKind::Console;
        assert_eq!(state.sequence_text(), "01 · 00 · 01 · 11");
        state.sequence = None;
        assert_eq!(state.sequence_text(), "──");
    }

    #[test]
    fn sequence_is_formatted_into_four_groups() {
        assert_eq!(format_sequence("01-00-01-11"), "01 · 00 · 01 · 11");
        // 畸形读数原样透传，不吞不猜。
        assert_eq!(format_sequence("0100"), "0100");
        assert_eq!(format_sequence("01-0-01-11"), "01-0-01-11");
    }

    #[test]
    fn progress_label_matches_spec_copy() {
        assert_eq!(Progress::new(2, 4, "已停稳").label(), "2/4 · 已停稳");
        assert_eq!(Progress::new(3, 4, "").label(), "3/4");
        assert_eq!(Progress::default().label(), "");
        assert_eq!(Progress::default().ratio(), 0.0);
        assert_eq!(Progress::new(2, 4, "").ratio(), 0.5);
    }

    #[test]
    fn flow_entries_are_greyed_when_uncalibrated() {
        let mut state = UiState {
            disclaimer_pending: false,
            ..UiState::default()
        };
        assert!(state.flow_enabled(SceneKind::Morse));
        state.missing = vec!["摩斯区域 #1".to_owned()];
        assert!(!state.flow_enabled(SceneKind::Morse), "未校准必须灰置");
        state.missing.clear();
        state.toggles.toggle(SceneKind::Console);
        assert!(!state.flow_enabled(SceneKind::Console), "关掉的入口不响应");
        state.toggles.toggle(SceneKind::Unknown);
        assert_eq!(state.toggles.entries().len(), 3);
    }

    #[test]
    fn resolution_change_raises_a_dismissible_notice() {
        let mut state = UiState::default();
        assert_eq!(state.notice_text(), None, "默认不打扰用户");

        state.warn_resolution_changed();
        let notice = state.notice_text().expect("换分辨率必须提示");
        assert!(notice.contains("重校"), "文案要点出「重校」：{notice}");
        // 「≤10 字」只算字面字数，空格与分隔点不计。
        let words = notice
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '·')
            .count();
        assert!(words <= 10, "文案超长（{words} 字）：{notice}");

        state.dismiss_notice();
        assert_eq!(state.notice_text(), None, "点掉后立即消失");
        state.warn_resolution_changed();
        assert_eq!(state.notice_text(), None, "本会话不得重复弹");
    }

    #[test]
    fn starting_a_run_clears_the_notice_but_keeps_progress_visible() {
        let mut state = UiState {
            disclaimer_pending: false,
            ..UiState::default()
        };
        state.warn_resolution_changed();
        state.begin_run();
        assert_eq!(state.notice_text(), None, "识别中要能看见进度");
        assert_eq!(state.phase, Phase::Running);
    }

    #[test]
    fn status_line_joins_phase_and_scene() {
        let mut state = UiState {
            scene: SceneKind::Console,
            ..UiState::default()
        };
        assert_eq!(state.status_line(), "待机 · 操作台");
        state.finish(Phase::Unrecognized, "未识别到游戏界面");
        assert_eq!(state.status_line(), "未识别 · 操作台");
    }
}
