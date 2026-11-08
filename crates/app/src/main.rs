//! 入口：DPI 感知 → 单实例 → 配置 → 窗口（字体在首帧前安装）。
//!
//! 打包后不弹控制台（`windows_subsystem = "windows"`）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use delta_app::{config_path, platform, DeltaApp};
use delta_geom::MonitorInfo;

/// 启动失败时的兜底分辨率（`monitor_info()` 取不到时）。
const FALLBACK_MONITOR: (i32, i32) = (1920, 1080);

fn main() -> eframe::Result {
    // ── 1. DPI 感知：必须在任何 GetDC / 建窗之前（spec D9）────────────
    let _ = delta_capture::ensure_dpi_awareness();

    // ── 2. 单实例：已有实例就激活它然后退出 ────────────────────────
    let Some(_instance) = platform::acquire_instance() else {
        platform::activate_existing_app(delta_ui::APP_TITLE);
        return Ok(());
    };

    // ── 3. 配置（文件不存在即默认配置，不报错）──────────────────────
    let path = config_path();
    let loaded = delta_config::load(&path).unwrap_or_else(|error| {
        eprintln!("配置读取失败，使用默认配置：{error}");
        delta_config::Loaded {
            config: delta_config::Config::default(),
            view: None,
        }
    });
    let config = loaded.config;
    let monitor = delta_capture::monitor_info().unwrap_or_else(|_| {
        MonitorInfo::new(FALLBACK_MONITOR.0, FALLBACK_MONITOR.1).expect("兜底分辨率合法")
    });
    // 分辨率变了只提示不阻断：`DeltaApp::new` 会把它变成界面上的一次性警示条
    // （release 版 `windows_subsystem = "windows"` 丢弃 stderr，提醒必须走 UI）。

    // ── 4. 窗口：无边框、不透明、置顶、可缩放 ───────────────────────
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(delta_ui::APP_TITLE)
            .with_inner_size(delta_ui::theme::WINDOW_SIZE)
            .with_min_inner_size(delta_ui::theme::WINDOW_MIN)
            .with_decorations(false)
            .with_transparent(false)
            .with_window_level(egui::WindowLevel::AlwaysOnTop)
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        delta_ui::APP_TITLE,
        options,
        Box::new(move |cc| {
            // 字体与样式必须在首帧之前装好（否则首帧豆腐块）。
            delta_ui::install(&cc.egui_ctx);
            Ok(Box::new(DeltaApp::new(monitor, config, path)))
        }),
    )
}
