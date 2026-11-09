//! 界面状态与展示文案（纯逻辑，不碰 egui 绘制，便于直接单测）。
//!
//! 五态来自 spec D6 / D9：空闲 · 运行中 · 成功 · 未识别 · 超时。
//! 文案一律 ≤6 字、动词开头、零技术词。

use egui::Color32;

use crate::theme;

pub use delta_config::CaptureBackend;

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
/// 热键冲突提示自动消失时长（spec D2：2.5s）。
pub const HOTKEY_CONFLICT_SECS: f64 = 2.5;

/// 修饰键位（与 Win32 `MOD_ALT` / `MOD_CONTROL` / `MOD_SHIFT` 同值，注册时直接按位或）。
pub const CHORD_MOD_ALT: u32 = 0x1;
pub const CHORD_MOD_CONTROL: u32 = 0x2;
pub const CHORD_MOD_SHIFT: u32 = 0x4;

/// 可捕获的主键清单：`egui::Key` ↔ 配置里的规范短名。
///
/// **唯一词汇来源**：`delta-app::platform::parse_hotkey` 按同一套短名解析，
/// 那边的 `capturable_vocabulary_matches_the_parser` 用例把两张表钉在一起，
/// 任何一侧漂移都会立刻变红。新增可捕获键只需在这里加一行。
pub const CAPTURABLE: [(&str, egui::Key); 71] = [
    ("F1", egui::Key::F1),
    ("F2", egui::Key::F2),
    ("F3", egui::Key::F3),
    ("F4", egui::Key::F4),
    ("F5", egui::Key::F5),
    ("F6", egui::Key::F6),
    ("F7", egui::Key::F7),
    ("F8", egui::Key::F8),
    ("F9", egui::Key::F9),
    ("F10", egui::Key::F10),
    ("F11", egui::Key::F11),
    ("F12", egui::Key::F12),
    ("F13", egui::Key::F13),
    ("F14", egui::Key::F14),
    ("F15", egui::Key::F15),
    ("F16", egui::Key::F16),
    ("F17", egui::Key::F17),
    ("F18", egui::Key::F18),
    ("F19", egui::Key::F19),
    ("F20", egui::Key::F20),
    ("F21", egui::Key::F21),
    ("F22", egui::Key::F22),
    ("F23", egui::Key::F23),
    ("F24", egui::Key::F24),
    ("A", egui::Key::A),
    ("B", egui::Key::B),
    ("C", egui::Key::C),
    ("D", egui::Key::D),
    ("E", egui::Key::E),
    ("F", egui::Key::F),
    ("G", egui::Key::G),
    ("H", egui::Key::H),
    ("I", egui::Key::I),
    ("J", egui::Key::J),
    ("K", egui::Key::K),
    ("L", egui::Key::L),
    ("M", egui::Key::M),
    ("N", egui::Key::N),
    ("O", egui::Key::O),
    ("P", egui::Key::P),
    ("Q", egui::Key::Q),
    ("R", egui::Key::R),
    ("S", egui::Key::S),
    ("T", egui::Key::T),
    ("U", egui::Key::U),
    ("V", egui::Key::V),
    ("W", egui::Key::W),
    ("X", egui::Key::X),
    ("Y", egui::Key::Y),
    ("Z", egui::Key::Z),
    ("0", egui::Key::Num0),
    ("1", egui::Key::Num1),
    ("2", egui::Key::Num2),
    ("3", egui::Key::Num3),
    ("4", egui::Key::Num4),
    ("5", egui::Key::Num5),
    ("6", egui::Key::Num6),
    ("7", egui::Key::Num7),
    ("8", egui::Key::Num8),
    ("9", egui::Key::Num9),
    ("Space", egui::Key::Space),
    ("Insert", egui::Key::Insert),
    ("Delete", egui::Key::Delete),
    ("Home", egui::Key::Home),
    ("End", egui::Key::End),
    ("PageUp", egui::Key::PageUp),
    ("PageDown", egui::Key::PageDown),
    ("ArrowUp", egui::Key::ArrowUp),
    ("ArrowDown", egui::Key::ArrowDown),
    ("ArrowLeft", egui::Key::ArrowLeft),
    ("ArrowRight", egui::Key::ArrowRight),
];

/// 一次捕获到的快捷键：主键短名 + 修饰键位。
///
/// 固定大小且 `Copy`，可直接放进 `SettingsAction`。字段私有、只能经
/// [`KeyChord::capture`] 构造 —— 因此不存在「不支持的键」这种半成品值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    name: &'static str,
    modifiers: u32,
}

impl KeyChord {
    /// 从一次按键事件捕获。
    ///
    /// 不支持的键返回 `None`，调用方必须保持捕获态继续等待（不提交、不取消）。
    /// 纯修饰键不构成热键：egui 对 Ctrl/Alt/Shift 只发 `Event::ModifiersChanged`，
    /// 不产生 `Event::Key`，所以这里天然拿不到只有修饰键的和弦。
    pub fn capture(key: egui::Key, modifiers: egui::Modifiers) -> Option<Self> {
        let (name, _) = CAPTURABLE.iter().find(|(_, candidate)| *candidate == key)?;
        Some(Self {
            name,
            modifiers: modifier_bits(modifiers),
        })
    }

    /// 主键短名（如 `F9`、`Space`）。
    pub fn main_name(self) -> &'static str {
        self.name
    }

    /// 修饰键位（`CHORD_MOD_*` 的按位或）。
    pub fn modifiers(self) -> u32 {
        self.modifiers
    }
}

fn modifier_bits(modifiers: egui::Modifiers) -> u32 {
    (if modifiers.ctrl { CHORD_MOD_CONTROL } else { 0 })
        | (if modifiers.alt { CHORD_MOD_ALT } else { 0 })
        | (if modifiers.shift { CHORD_MOD_SHIFT } else { 0 })
}

/// 冲突提示文案（D2 的两种失败）。
///
/// `kept = Some(旧键)` = 回滚成功，旧键仍可用；`None` = 连旧键都占不到。
pub fn conflict_copy(kept: Option<&str>) -> String {
    match kept {
        Some(kept) => format!("热键冲突 · 仍为 {kept}"),
        None => "热键不可用 · 用按钮".to_owned(),
    }
}

/// 捕获态下，一帧事件流的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// 还没按到能构成热键的键 —— 继续等，既不提交也不取消。
    Pending,
    /// 无修饰的 `Esc`：只取消捕获，不关面板（D4）。
    Cancel,
    /// 捕获到一个可提交的组合。
    Commit(KeyChord),
}

/// 从一帧 egui 事件流里读出捕获结果。
///
/// 只认「新按下、非长按重复」的键事件：长按会刷出一串重复事件，若照单全收
/// 会在用户还没松手时连提交好几次。不可捕获的键（含纯修饰键）一律忽略并继续等待 ——
/// 用户按 Ctrl 之后紧接着按 F9，中间那一拍 Ctrl 不该被当成「无效键」把捕获踢掉。
pub fn read_capture(events: &[egui::Event]) -> CaptureOutcome {
    for event in events {
        let egui::Event::Key {
            key,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } = event
        else {
            continue;
        };
        if *key == egui::Key::Escape && modifier_bits(*modifiers) == 0 {
            return CaptureOutcome::Cancel;
        }
        if let Some(chord) = KeyChord::capture(*key, *modifiers) {
            return CaptureOutcome::Commit(chord);
        }
    }
    CaptureOutcome::Pending
}

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
    /// 标题条图钉：切换窗口置顶（默认关）。
    ToggleAlwaysOnTop,
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

/// 「测试截图」的结果（spec D5）：抓到的尺寸、耗时与已上传的缩略图纹理。
///
/// 纹理在 UI 线程上传（`Context::load_texture`），这里只持有句柄 —— 逐像素画图会
/// 让一帧多出上万条绘制指令，缩略图必须是纹理。
#[derive(Clone)]
pub struct CaptureTestResult {
    /// 实际抓到的像素宽（整屏）。
    pub width: u32,
    /// 实际抓到的像素高。
    pub height: u32,
    /// 抓这一帧耗时（毫秒，含后端初始化）。
    pub elapsed_ms: u64,
    /// 缩略图纹理（≤160×64）。
    pub thumbnail: egui::TextureHandle,
}

impl CaptureTestResult {
    /// 结果行文案：`1920×1080 · 18ms`（spec D5 的显示要求）。
    pub fn summary(&self) -> String {
        format!("{}×{} · {}ms", self.width, self.height, self.elapsed_ms)
    }
}

/// 手写 `Debug`：`egui::TextureHandle` 没有实现 `Debug`，这里只暴露纹理 id。
impl std::fmt::Debug for CaptureTestResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureTestResult")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("elapsed_ms", &self.elapsed_ms)
            .field("thumbnail", &self.thumbnail.id())
            .finish()
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
    /// 设置面板里是否正在捕获新热键。
    pub hotkey_editing: bool,
    /// 热键冲突提示文案（`None` = 不显示）。
    pub hotkey_conflict: Option<String>,
    /// 冲突提示的自动消失时刻（秒，与 `egui` 的 `input.time` 同一时间源）。
    pub hotkey_conflict_until: f64,
    /// 当前截图后端（spec D1：切换立即生效，运行中禁止切换）。
    pub capture_backend: CaptureBackend,
    /// 最近一次「测试截图」的结果（`None` = 还没点过 / 已被清空）。
    pub capture_test: Option<CaptureTestResult>,
    /// 最近一次「测试截图」的失败原因（与 `capture_test` 互斥）。
    pub capture_test_error: Option<String>,
    /// 窗口是否已钉在顶层（标题条图钉的当前状态；默认不置顶）。
    pub always_on_top: bool,
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
            hotkey_editing: false,
            hotkey_conflict: None,
            hotkey_conflict_until: 0.0,
            capture_backend: CaptureBackend::Wgc,
            capture_test: None,
            capture_test_error: None,
            always_on_top: false,
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

    /// 装上热键冲突提示（D2：`2.5s` 后自动消失，也可点掉）。
    ///
    /// **不动 `self.hotkey`**：改键失败时界面必须仍显示旧键 —— 这正是「假保存」的反面。
    pub fn show_hotkey_conflict(&mut self, text: String, now: f64) {
        self.hotkey_conflict = Some(text);
        self.hotkey_conflict_until = now + HOTKEY_CONFLICT_SECS;
    }

    /// 当前应显示的冲突提示；到期即清空。
    pub fn hotkey_conflict_text(&mut self, now: f64) -> Option<&str> {
        if self.hotkey_conflict.is_some() && now >= self.hotkey_conflict_until {
            self.hotkey_conflict = None;
        }
        self.hotkey_conflict.as_deref()
    }

    /// 点掉冲突提示。
    pub fn dismiss_hotkey_conflict(&mut self) {
        self.hotkey_conflict = None;
    }

    /// 是否允许切换截图后端（spec D1：必须等当前识别任务结束）。
    pub fn capture_switchable(&self) -> bool {
        self.phase != Phase::Running
    }

    /// 装上一次测试截图的结果（成功即清掉旧错误，两者互斥）。
    pub fn set_capture_test(&mut self, result: CaptureTestResult) {
        self.capture_test = Some(result);
        self.capture_test_error = None;
    }

    /// 装上一次测试截图的失败原因（失败即清掉旧结果，不显示过期证据）。
    pub fn set_capture_test_error(&mut self, reason: String) {
        self.capture_test = None;
        self.capture_test_error = Some(reason);
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

    #[test]
    fn capture_accepts_supported_keys_and_ignores_the_rest() {
        let plain = egui::Modifiers::default();
        let chord = KeyChord::capture(egui::Key::F9, plain).expect("F9 可捕获");
        assert_eq!(chord.main_name(), "F9");
        assert_eq!(chord.modifiers(), 0);

        // 不支持的键必须返回 None（调用方继续等待），不能被当成和弦提交。
        assert_eq!(KeyChord::capture(egui::Key::Slash, plain), None);
        assert_eq!(KeyChord::capture(egui::Key::Escape, plain), None);
        assert_eq!(KeyChord::capture(egui::Key::Enter, plain), None);
        assert_eq!(KeyChord::capture(egui::Key::F25, plain), None);
    }

    #[test]
    fn chord_carries_the_win32_modifier_bits() {
        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            ..egui::Modifiers::default()
        };
        let chord = KeyChord::capture(egui::Key::F8, modifiers).expect("F8 可捕获");
        assert_eq!(chord.main_name(), "F8");
        assert_eq!(
            chord.modifiers(),
            CHORD_MOD_CONTROL | CHORD_MOD_ALT | CHORD_MOD_SHIFT,
            "修饰键位必须与 Win32 MOD_* 同值，注册时才能直接按位或"
        );
    }

    #[test]
    fn capturable_names_are_unique() {
        let mut names: Vec<&str> = CAPTURABLE.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CAPTURABLE.len(), "短名必须互异（配置里只能有一种写法）");
        let mut keys: Vec<egui::Key> = CAPTURABLE.iter().map(|(_, key)| *key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), CAPTURABLE.len(), "同一个键不得映射到两个短名");
    }

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn capture_commits_supported_keys_and_cancels_only_on_bare_escape() {
        let plain = egui::Modifiers::default();
        assert_eq!(
            read_capture(&[key_event(egui::Key::F9, plain)]),
            CaptureOutcome::Commit(KeyChord::capture(egui::Key::F9, plain).expect("F9 可捕获"))
        );
        // 无修饰的 Esc 只取消捕获（D4），面板是否关掉由视图决定。
        assert_eq!(
            read_capture(&[key_event(egui::Key::Escape, plain)]),
            CaptureOutcome::Cancel
        );
        // Ctrl+Esc 不是「取消」—— 它是一个合法的组合键，必须能被捕获。
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..egui::Modifiers::default()
        };
        assert_eq!(
            read_capture(&[key_event(egui::Key::Escape, ctrl)]),
            CaptureOutcome::Pending
        );
    }

    #[test]
    fn capture_keeps_waiting_on_unsupported_keys_and_modifier_only_presses() {
        let plain = egui::Modifiers::default();
        // 不可捕获的键：忽略并继续等，绝不取消（否则按错一下就掉出捕获态）。
        assert_eq!(
            read_capture(&[key_event(egui::Key::Slash, plain)]),
            CaptureOutcome::Pending
        );
        // 纯修饰键只产生 `ModifiersChanged`，不构成热键（D1 显式边界）。
        assert_eq!(
            read_capture(&[egui::Event::ModifiersChanged(egui::Modifiers {
                ctrl: true,
                ..egui::Modifiers::default()
            })]),
            CaptureOutcome::Pending
        );
        // 长按重复事件必须忽略，否则一次长按会连提交好几次。
        assert_eq!(
            read_capture(&[egui::Event::Key {
                key: egui::Key::F9,
                physical_key: None,
                pressed: true,
                repeat: true,
                modifiers: plain,
            }]),
            CaptureOutcome::Pending
        );
        assert_eq!(read_capture(&[]), CaptureOutcome::Pending);
    }

    #[test]
    fn capture_reads_the_modifier_combination_from_the_same_event() {
        let ctrl_shift = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..egui::Modifiers::default()
        };
        match read_capture(&[key_event(egui::Key::J, ctrl_shift)]) {
            CaptureOutcome::Commit(chord) => {
                assert_eq!(chord.main_name(), "J");
                assert_eq!(
                    chord.modifiers(),
                    CHORD_MOD_CONTROL | CHORD_MOD_SHIFT,
                    "修饰键位必须与 Win32 MOD_* 同值"
                );
            }
            other => panic!("必须捕获成组合键，实际 {other:?}"),
        }
    }

    #[test]
    fn hotkey_conflict_expires_and_never_touches_the_current_hotkey() {
        let mut state = UiState {
            hotkey: "F9".to_owned(),
            ..UiState::default()
        };
        assert_eq!(state.hotkey_conflict_text(0.0), None, "默认不打扰用户");

        state.show_hotkey_conflict(conflict_copy(Some("F9")), 10.0);
        assert_eq!(
            state.hotkey_conflict_text(10.1),
            Some("热键冲突 · 仍为 F9")
        );
        assert_eq!(state.hotkey, "F9", "改键失败时界面必须仍是旧键");

        assert_eq!(state.hotkey_conflict_text(12.4), Some("热键冲突 · 仍为 F9"));
        assert_eq!(state.hotkey_conflict_text(12.5), None, "2.5s 后自动消失");

        state.show_hotkey_conflict(conflict_copy(None), 20.0);
        assert_eq!(state.hotkey_conflict_text(20.0), Some("热键不可用 · 用按钮"));
        state.dismiss_hotkey_conflict();
        assert_eq!(state.hotkey_conflict_text(20.0), None, "也能点掉");
    }

    fn test_capture_result() -> CaptureTestResult {
        let ctx = egui::Context::default();
        let thumbnail = ctx.load_texture(
            "capture-test",
            egui::ColorImage::example(),
            egui::TextureOptions::NEAREST,
        );
        CaptureTestResult {
            width: 1920,
            height: 1080,
            elapsed_ms: 18,
            thumbnail,
        }
    }

    #[test]
    fn capture_test_result_summary_matches_spec_copy() {
        assert_eq!(test_capture_result().summary(), "1920×1080 · 18ms");
    }

    #[test]
    fn capture_test_result_and_error_never_show_up_together() {
        let mut state = UiState::default();
        assert_eq!(state.capture_backend, CaptureBackend::Wgc, "默认 WGC");
        assert!(state.capture_test.is_none() && state.capture_test_error.is_none());

        state.set_capture_test(test_capture_result());
        assert_eq!(state.capture_test.as_ref().map(CaptureTestResult::summary), Some("1920×1080 · 18ms".to_owned()));

        state.set_capture_test_error("WGC 初始化失败：0x80070005".to_owned());
        assert!(state.capture_test.is_none(), "失败不得继续显示上一次的证据");
        assert_eq!(
            state.capture_test_error.as_deref(),
            Some("WGC 初始化失败：0x80070005")
        );

        state.set_capture_test(test_capture_result());
        assert!(state.capture_test_error.is_none(), "成功即清掉旧错误");
    }

    #[test]
    fn capture_backend_is_locked_while_a_task_runs() {
        let mut state = UiState::default();
        assert!(state.capture_switchable());
        state.begin_run();
        assert!(!state.capture_switchable(), "运行中禁止切换后端（D1）");
        state.finish(Phase::Success, "已完成");
        assert!(state.capture_switchable(), "任务结束即可切换");
    }
}
