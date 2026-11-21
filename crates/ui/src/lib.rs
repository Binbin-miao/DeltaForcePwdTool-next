//! `delta-ui`：主界面与五态视觉（egui 手绘，无第三方主题）。
//!
//! 分层：`theme`（设计 token）→ `state`（五态与文案，纯逻辑）→ `view`（绘制）。业务与截图/输入全部留在 `delta-app`，本 crate 不做 I/O。

pub mod state;
pub mod theme;
pub mod view;

pub use state::{
    az3_result, conflict_copy, console_result, fingerprint_result, format_sequence, morse_result, read_capture,
    Action, CaptureBackend, CaptureOutcome, CaptureTestResult, FlowId, GridCell, KeyChord, LegacyToggles,
    Phase, PrimaryState, Progress, ResultContent, ResultState, SceneKind, ThemeId, Toggles, UiState,
    CAPTURABLE,
};
pub use view::{
    main_window, settings_panel, SettingsAction, SettingsView,
};

/// 窗口标题（单实例激活时按它找窗）。
pub const APP_TITLE: &str = "三角洲密码助手";

/// 安装字体与样式（必须在任何首帧之前调用）。
pub fn install(ctx: &egui::Context) {
    theme::install(ctx);
}
