//! 后台执行层：热键泵线程 + 识别执行线程，全部通过 `mpsc` 与界面通信。
//!
//! 铁律（t9 禁止项）：识别绝不在 UI 线程跑。UI 线程只做两件事：`try_recv` 事件、
//! `request_repaint` 唤醒重绘。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use delta_capture::{Capturer, Frame};
use delta_config::{Config, PxView, Timings};
use delta_geom::{MonitorInfo, PxPoint, PxRect};
use delta_ui::SceneKind;

use crate::platform;

/// 热键等待切片：既保证热键响应，也保证线程能及时看到退出标志。
const HOTKEY_POLL: Duration = Duration::from_millis(200);

/// 后台 → UI 的事件。
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// 开始执行（UI 切运行态）。
    Started,
    /// 场景判别结果 + 操作台目标序列（已排好版）。
    Scene {
        scene: SceneKind,
        sequence: Option<String>,
    },
    /// 进度。
    Progress {
        done: usize,
        total: usize,
        note: String,
    },
    Succeeded(String),
    Failed(String),
    /// spec D6：未识别必须拒绝执行，零输入。
    Rejected(String),
    TimedOut(String),
}

/// UI → 后台的命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// 一键识别（自动判别场景）。
    Trigger,
    /// 强制跑指定场景，跳过自动判别（对齐上游「F5 只跑摩斯 / F6 只跑指纹」）。
    Force(SceneKind),
    /// 只读检查：找窗 + 场景判别，不产生任何输入。
    Probe,
    /// 请求取消（在流程边界生效）。
    Cancel,
}

/// 后台句柄。
pub struct Worker {
    pub events: mpsc::Receiver<UiEvent>,
    pub commands: mpsc::Sender<Cmd>,
    /// 改键通道：把新热键名送进热键泵线程，实现「改完立刻生效」。
    hotkey_tx: mpsc::Sender<String>,
    shutdown: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl Worker {
    /// 热更新全局热键：改完立即生效，无需重启。
    ///
    /// 新键注册失败（被别的程序占用）时只是暂时没有全局热键，主按钮仍可用，
    /// 不会让程序崩掉。
    pub fn set_hotkey(&self, name: String) {
        let _ = self.hotkey_tx.send(name);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

impl Worker {
    /// 起两个线程：热键泵 + 执行器。
    pub fn spawn(config: Arc<Mutex<Config>>, monitor: MonitorInfo) -> Self {
        let (event_tx, events) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));

        // 四个热键：一键识别（自动判别）+ 三个强制键（对齐上游 F5/F6，另加操作台 F7）。
        let bindings: Vec<(String, Cmd)> = {
            let keys = match config.lock() {
                Ok(cfg) => cfg.hotkeys.clone(),
                Err(_) => delta_config::Hotkeys::default(),
            };
            vec![
                (keys.scene, Cmd::Trigger),
                (keys.morse, Cmd::Force(SceneKind::Morse)),
                (keys.fingerprint, Cmd::Force(SceneKind::Fingerprint)),
                (keys.console, Cmd::Force(SceneKind::Console)),
            ]
        };
        let (hotkey_tx, hotkey_rx) = mpsc::channel::<String>();
        {
            let trigger = commands.clone();
            let stop = shutdown.clone();
            std::thread::Builder::new()
                .name("delta-hotkey".to_owned())
                .spawn(move || {
                    // 注册失败（被其它程序占用）只是没有该热键，主按钮仍可用。
                    let Ok(hotkeys) = delta_hotkey::Hotkeys::new() else {
                        return;
                    };
                    let mut routes: Vec<(u32, Cmd)> = Vec::new();
                    let mut scene_id: Option<u32> = None;
                    for (name, cmd) in &bindings {
                        if let Ok(id) = hotkeys.register(platform::parse_hotkey(name)) {
                            if matches!(cmd, Cmd::Trigger) {
                                scene_id = Some(id);
                            }
                            routes.push((id, *cmd));
                        }
                    }
                    while !stop.load(Ordering::Relaxed) {
                        // 改键只重绑「一键识别」；其余三键改配置文件后重启生效。
                        if let Ok(name) = hotkey_rx.try_recv() {
                            if let Some(id) = scene_id.take() {
                                let _ = hotkeys.unregister(id);
                                routes.retain(|(route, _)| *route != id);
                            }
                            if let Ok(id) = hotkeys.register(platform::parse_hotkey(&name)) {
                                scene_id = Some(id);
                                routes.push((id, Cmd::Trigger));
                            }
                        }
                        if let Some(event) = hotkeys.next_event(HOTKEY_POLL) {
                            if let Some((_, cmd)) = routes.iter().find(|(id, _)| *id == event.id) {
                                if trigger.send(*cmd).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
                .ok();
        }
        {
            let stop = shutdown.clone();
            let cancel = cancel.clone();
            std::thread::Builder::new()
                .name("delta-executor".to_owned())
                .spawn(move || {
                    while let Ok(command) = command_rx.recv() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        match command {
                            Cmd::Cancel => cancel.store(true, Ordering::SeqCst),
                            Cmd::Probe => {
                                cancel.store(false, Ordering::SeqCst);
                                probe(&config, monitor, &event_tx);
                            }
                            Cmd::Trigger => {
                                cancel.store(false, Ordering::SeqCst);
                                run_once(&config, monitor, &event_tx, &cancel, None);
                            }
                            Cmd::Force(kind) => {
                                cancel.store(false, Ordering::SeqCst);
                                run_once(&config, monitor, &event_tx, &cancel, Some(kind));
                            }
                        }
                    }
                })
                .ok();
        }

        Self {
            events,
            commands,
            hotkey_tx,
            shutdown,
            cancel,
        }
    }

    /// 请求取消（流程边界生效；单次流程内部不可抢占，见 `run_once` 注释）。
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// 取当前配置快照（锁内只做 clone，不做耗时操作）。
fn snapshot(config: &Arc<Mutex<Config>>) -> Config {
    config
        .lock()
        .map(|config| config.clone())
        .unwrap_or_default()
}

/// 截一张全屏（物理像素）。
pub fn grab_full(monitor: MonitorInfo) -> Result<Frame, String> {
    let mut capturer = Capturer::new(monitor.w, monitor.h).map_err(|error| error.to_string())?;
    let rect = PxRect {
        x: 0,
        y: 0,
        w: monitor.w,
        h: monitor.h,
    };
    capturer.grab(rect).map_err(|error| error.to_string())
}

/// 只读检查：找窗 + 场景判别，**零输入**。
fn probe(config: &Arc<Mutex<Config>>, monitor: MonitorInfo, tx: &mpsc::Sender<UiEvent>) {
    let _ = tx.send(UiEvent::Started);
    let snapshot = snapshot(config);
    if let Err(reason) = platform::focus_game_window() {
        let _ = tx.send(UiEvent::Rejected(reason));
        return;
    }
    let Ok(view) = snapshot.try_px_view() else {
        let _ = tx.send(UiEvent::Rejected("请先校准".to_owned()));
        return;
    };
    match grab_full(monitor) {
        Ok(frame) => {
            let verdict = delta_scene::detect(&frame, &view);
            let scene = map_scene(verdict.scene);
            let sequence = if scene == SceneKind::Console {
                read_sequence(&view).ok().map(|raw| delta_ui::format_sequence(&raw))
            } else {
                None
            };
            let _ = tx.send(UiEvent::Scene { scene, sequence });
            if verdict.is_actionable() {
                let _ = tx.send(UiEvent::Succeeded(format!(
                    "{} {:.2}",
                    scene.label(),
                    verdict.confidence
                )));
            } else {
                let _ = tx.send(UiEvent::Rejected("检查游戏".to_owned()));
            }
        }
        Err(reason) => {
            let _ = tx.send(UiEvent::Failed(reason));
        }
    }
}

/// 一次完整的一键识别。
///
/// 顺序不可交换（spec §2.2 不变量 8）：**找窗激活 → 截图判别 → 分发 flow**。
/// 场景判别为 `Unknown` 时立即拒绝，零输入（spec D6）。
///
/// 取消语义：`cancel` 标志在流程边界检查。单次 flow 内部不可抢占（`flow-*` 的
/// `run` 是同步函数），因此取消最快在本次 flow 结束后生效；这是有意的取舍 ——
/// 强行中断正在注入输入的流程会留下半数点击，比多等 1~2 秒危险得多。
fn run_once(
    config: &Arc<Mutex<Config>>,
    monitor: MonitorInfo,
    tx: &mpsc::Sender<UiEvent>,
    cancel: &AtomicBool,
    forced: Option<SceneKind>,
) {
    let _ = tx.send(UiEvent::Started);
    let snapshot = snapshot(config);

    if let Err(reason) = platform::focus_game_window() {
        let _ = tx.send(UiEvent::Rejected(reason));
        return;
    }
    let Ok(view) = snapshot.try_px_view() else {
        let _ = tx.send(UiEvent::Rejected("请先校准".to_owned()));
        return;
    };

    let frame = match grab_full(monitor) {
        Ok(frame) => frame,
        Err(reason) => {
            let _ = tx.send(UiEvent::Failed(reason));
            return;
        }
    };
    // 强制模式（上游 F5/F6 的等价物）：跳过判别直接用指定场景，
    // 用于自动判别不准时人工兜底。自动模式下判别为 Unknown 立即拒绝，零输入。
    let (scene, actionable, reason) = match forced {
        Some(kind) => (kind, true, ""),
        None => {
            let verdict = delta_scene::detect(&frame, &view);
            (
                map_scene(verdict.scene),
                verdict.is_actionable(),
                verdict.reason,
            )
        }
    };
    let sequence = if scene == SceneKind::Console {
        read_sequence(&view).ok().map(|raw| delta_ui::format_sequence(&raw))
    } else {
        None
    };
    let _ = tx.send(UiEvent::Scene { scene, sequence });

    if !actionable {
        // spec D6 硬约束：拒绝执行，不产生任何输入。
        let _ = tx.send(UiEvent::Rejected(reason.to_owned()));
        return;
    }
    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(UiEvent::Failed("已取消".to_owned()));
        return;
    }

    match scene {
        SceneKind::Morse => run_morse(&snapshot, &view, tx),
        SceneKind::Fingerprint => run_fingerprint(&snapshot, &view, tx),
        SceneKind::Console => run_console(&snapshot, &view, monitor, tx),
        SceneKind::Unknown => {
            let _ = tx.send(UiEvent::Rejected("检查游戏".to_owned()));
        }
    }
}

/// `delta-scene::Scene` → UI 侧场景枚举。
fn map_scene(scene: delta_scene::Scene) -> SceneKind {
    match scene {
        delta_scene::Scene::Morse => SceneKind::Morse,
        delta_scene::Scene::Fingerprint => SceneKind::Fingerprint,
        delta_scene::Scene::Console => SceneKind::Console,
        delta_scene::Scene::Unknown => SceneKind::Unknown,
    }
}

/// 读操作台 LED 目标序列（只读，不发输入）。
fn read_sequence(view: &PxView) -> Result<String, String> {
    let mut capturer = Capturer::new(view.monitor.w, view.monitor.h).map_err(|e| e.to_string())?;
    let frame = capturer
        .grab(view.console.led_region)
        .map_err(|error| error.to_string())?;
    delta_vision::read_glyph_string(&frame.luma()).map_err(|error| error.to_string())
}

/// 摩斯流程（薄适配：`LiveScreen` / `LiveInput` 由 flow crate 提供）。
fn run_morse(config: &Config, view: &PxView, tx: &mpsc::Sender<UiEvent>) {
    let mut capturer = match Capturer::new(view.monitor.w, view.monitor.h) {
        Ok(capturer) => capturer,
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
            return;
        }
    };
    let mut screen = delta_flow_morse::LiveScreen::new(&mut capturer);
    let mut input = delta_flow_morse::LiveInput;
    let job = delta_flow_morse::MorseRun {
        view,
        timings: &config.timings,
        // 上游 morse_auto_click 的等价物：控制输入完成后是否自动点确认。
        auto_click: config.auto_confirm,
    };
    let outcome = delta_flow_morse::run(&mut screen, &mut input, &job);
    let total = outcome.cells.len();
    let _ = tx.send(UiEvent::Progress {
        done: outcome.cells.iter().filter(|cell| cell.digit.is_some()).count(),
        total,
        note: "已输入数字".to_owned(),
    });
    if outcome.digits_complete() {
        let _ = tx.send(UiEvent::Succeeded(format!("已输入数字 {}", outcome.password)));
    } else {
        let _ = tx.send(UiEvent::Failed(format!("未识别 {}", outcome.password)));
    }
}

/// 指纹流程（模板库按 exe 目录解析；OCR 兜底按配置开关）。
fn run_fingerprint(config: &Config, view: &PxView, tx: &mpsc::Sender<UiEvent>) {
    let images = crate::images_dir();
    let library = match delta_templates::RoleLibrary::load_from_dir(&images) {
        Ok(library) => library,
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
            return;
        }
    };
    let mut capturer = match Capturer::new(view.monitor.w, view.monitor.h) {
        Ok(capturer) => capturer,
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
            return;
        }
    };
    let mut screen = delta_flow_fingerprint::LiveScreen::new(&mut capturer);
    let mut input = delta_flow_fingerprint::LiveInput;
    let mut reader = if config.ocr_enabled {
        delta_ocr::OcrEngine::new(delta_ocr::DEFAULT_LANGUAGE)
            .ok()
            .map(delta_flow_fingerprint::OcrReader::new)
    } else {
        None
    };
    let job = delta_flow_fingerprint::FingerprintJob {
        view,
        library: &library,
        timings: &config.timings,
        // 上游 fingerprint_auto_click 的等价物：false → 只出标注图，不点击。
        auto_click: config.fingerprint_auto_click,
        ocr_enabled: config.ocr_enabled,
        evidence_dir: crate::evidence_dir(),
    };
    match delta_flow_fingerprint::run(
        &mut screen,
        &mut input,
        reader.as_mut().map(|reader| reader as &mut dyn delta_flow_fingerprint::TextReader),
        &job,
    ) {
        Ok(outcome) => {
            let _ = tx.send(UiEvent::Progress {
                done: outcome.clicks.len(),
                total: outcome.pairs.len(),
                note: "已点完格子".to_owned(),
            });
            let _ = tx.send(UiEvent::Succeeded(format!(
                "{} · 已点 {} 格",
                outcome.role,
                outcome.clicks.len()
            )));
        }
        Err(failure) => {
            let _ = tx.send(UiEvent::Failed(failure.detail.clone()));
        }
    }
}

/// 操作台流程（`flow-console` 自带 `SystemScreen` / `SystemPointer` / `SystemClock`）。
fn run_console(
    config: &Config,
    view: &PxView,
    monitor: MonitorInfo,
    tx: &mpsc::Sender<UiEvent>,
) {
    let mut screen = match delta_flow_console::SystemScreen::new(monitor.w, monitor.h) {
        Ok(screen) => screen,
        Err(reason) => {
            let _ = tx.send(UiEvent::Failed(reason));
            return;
        }
    };
    let mut pointer = delta_flow_console::SystemPointer::new();
    let mut clock = delta_flow_console::SystemClock::new();
    let params = delta_flow_console::ConsoleParams::default().with_timings(&config.timings);
    match delta_flow_console::run(
        &mut screen,
        &mut pointer,
        &mut clock,
        &view.console,
        &params,
    ) {
        Ok(outcome) => {
            if outcome.is_done() {
                let _ = tx.send(UiEvent::Progress {
                    done: 4,
                    total: 4,
                    note: "已停稳旋钮".to_owned(),
                });
                let _ = tx.send(UiEvent::Succeeded("已完成".to_owned()));
            } else {
                let reason = outcome
                    .abort_reason()
                    .map_or_else(|| "已取消".to_owned(), |reason| reason.to_string());
                // 操作台处于 BETA：失败必留证（截图 + 文本日志），供用户自测定位。
                save_console_evidence(&reason, monitor, outcome.events());
                let _ = tx.send(UiEvent::Failed(reason));
            }
        }
        Err(error) => {
            let reason = error.to_string();
            save_console_evidence(&reason, monitor, 0);
            let _ = tx.send(UiEvent::Failed(reason));
        }
    }
}

/// 操作台失败留证：全屏截图 + 文本日志，落到 `logs/`。
///
/// 操作台是 BETA 功能，用户自测时靠这两份证据定位：截屏能看出当时卡在哪一列、
/// 目标序列读成了什么、面板有没有消失；日志记录时间、原因、分辨率与已发事件数。
/// 任何一步失败都只打 stderr，**不允许把主流程带崩**。
fn save_console_evidence(reason: &str, monitor: MonitorInfo, events: u64) {
    let dir = std::path::Path::new("logs");
    if let Err(error) = std::fs::create_dir_all(dir) {
        eprintln!("[console] 日志目录创建失败：{error}");
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let base = dir.join(format!("console_fail_{stamp}"));

    if let Ok(mut capturer) = Capturer::new(monitor.w, monitor.h) {
        let full = PxRect {
            x: 0,
            y: 0,
            w: monitor.w,
            h: monitor.h,
        };
        match capturer.grab(full) {
            Ok(frame) => {
                if let Err(error) = write_frame_png(&base.with_extension("png"), &frame) {
                    eprintln!("[console] 失败截图保存失败：{error}");
                }
            }
            Err(error) => eprintln!("[console] 失败截图抓取失败：{error}"),
        }
    }

    let text = format!(
        "时间(unix): {stamp}\n原因: {reason}\n分辨率: {}x{}\n已发输入事件: {events}\n",
        monitor.w, monitor.h
    );
    if let Err(error) = std::fs::write(base.with_extension("txt"), text) {
        eprintln!("[console] 失败日志保存失败：{error}");
    }
}

/// 把 BGRA 帧编码成 PNG（RGBA）。失败只回错误，不 panic。
fn write_frame_png(path: &std::path::Path, frame: &Frame) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.w, frame.h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let mut rgba = Vec::with_capacity((frame.w as usize) * (frame.h as usize) * 4);
    for y in 0..frame.h {
        for x in 0..frame.w {
            let [b, g, r] = frame.pixel(x, y);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    writer.write_image_data(&rgba)?;
    Ok(())
}

/// 供 UI 显示用的时序快照（当前未在 UI 中展示，保留给设置面板的后续扩展）。
pub fn timing_note(timings: &Timings) -> String {
    format!("停留 {}ms", timings.console_dwell_ms)
}

/// 中心点（供调试输出）。
pub fn rect_center(rect: PxRect) -> PxPoint {
    PxPoint {
        x: rect.x + rect.w / 2,
        y: rect.y + rect.h / 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_config::Config;

    #[test]
    fn scene_mapping_covers_every_variant() {
        assert_eq!(map_scene(delta_scene::Scene::Morse), SceneKind::Morse);
        assert_eq!(
            map_scene(delta_scene::Scene::Fingerprint),
            SceneKind::Fingerprint
        );
        assert_eq!(map_scene(delta_scene::Scene::Console), SceneKind::Console);
        assert_eq!(map_scene(delta_scene::Scene::Unknown), SceneKind::Unknown);
    }

    #[test]
    fn uncalibrated_config_has_missing_areas() {
        // 未校准时 `run_once` 必须走「请先校准」分支，而非拿零矩形去截图。
        let config = Config::default();
        assert!(!config.validate().is_empty());
        assert!(config.try_px_view().is_err());
    }

    #[test]
    fn rect_center_uses_integer_half() {
        let center = rect_center(PxRect {
            x: 10,
            y: 20,
            w: 40,
            h: 60,
        });
        assert_eq!((center.x, center.y), (30, 50));
    }
}
