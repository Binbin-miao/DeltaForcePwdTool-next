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

    // ── 2. 管理员：游戏以管理员运行时，低权限进程的模拟点击会被 UIPI 丢弃，
    //        所以先尝试自我提权重启（与上游 Python 版要求管理员一致）。
    //        用户拒绝 UAC 也继续跑：界面与热键可用，仅在界面上提示注入可能无效。
    //        `--no-elevate` / 环境变量 `DELTA_PWD_NO_ELEVATE` 仅用于开发与自动化验证。
    let skip_elevate = std::env::args().any(|arg| arg == "--no-elevate")
        || std::env::var_os("DELTA_PWD_NO_ELEVATE").is_some();
    if !skip_elevate && !platform::is_elevated() && platform::relaunch_elevated() {
        return Ok(());
    }

    // ── 2.5. 单实例：已有实例就激活它然后退出 ────────────────────────
    let Some(_instance) = platform::acquire_instance() else {
        platform::activate_existing_app(delta_ui::APP_TITLE);
        return Ok(());
    };

    // ── 3. 屏幕尺寸（必须早于配置：百分比→像素一律按当前屏幕换算，与是否校准无关）
    let monitor = delta_capture::monitor_info().unwrap_or_else(|_| {
        MonitorInfo::new(FALLBACK_MONITOR.0, FALLBACK_MONITOR.1).expect("兜底分辨率合法")
    });

    // ── 4. 配置（文件缺失或区域为空 → 自动填内置坐标，不校准也能直接用）──
    let path = config_path();
    let loaded = delta_config::load(&path).unwrap_or_else(|error| {
        eprintln!("配置读取失败，使用内置配置：{error}");
        delta_config::Loaded {
            config: delta_config::Config::default(),
            view: None,
        }
    });
    let config = loaded.config.with_runtime_monitor(monitor);
    // 分辨率变了只提示不阻断：`DeltaApp::new` 会把它变成界面上的一次性警示条
    // （release 版 `windows_subsystem = "windows"` 丢弃 stderr，提醒必须走 UI）。

    // ── 4. 窗口：无边框、不透明、可缩放；置顶按配置（默认不置顶，能被游戏盖住）──
    let window_level = if config.always_on_top {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(delta_ui::APP_TITLE)
            .with_inner_size(delta_ui::theme::WINDOW_SIZE)
            .with_min_inner_size(delta_ui::theme::WINDOW_MIN)
            .with_decorations(false)
            .with_transparent(false)
            .with_window_level(window_level)
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
