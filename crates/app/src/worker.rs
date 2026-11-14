//! 后台执行层：热键泵线程 + 识别执行线程，全部通过 `mpsc` 与界面通信。
//!
//! 铁律（t9 禁止项）：识别绝不在 UI 线程跑。UI 线程只做两件事：`try_recv` 事件、
//! `request_repaint` 唤醒重绘。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use delta_capture::{CaptureBackend, CaptureSession, Frame, FRAME_BUDGET};
use delta_config::{Config, PxView, Timings};
use delta_geom::{MonitorInfo, PxPoint, PxRect};
use delta_hotkey::{Hotkey, Hotkeys as HotkeyRegistry};
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
    /// 结果区内容（spec D2b：摩斯 / 指纹 / 操作台共用一块结果区）。
    ///
    /// 排版（`4 2 7` / `1 → 3 → 5` / `01 · 00 · 01 · 11`）由 `delta_ui` 的纯函数负责，
    /// 流程层只交原始数据 —— 这样结果区的形状有唯一来源，三处不会各排一套。
    Result(delta_ui::ResultContent),
    Succeeded(String),
    Failed(String),
    /// spec D6：未识别必须拒绝执行，零输入。
    Rejected(String),
    TimedOut(String),
    /// 改键成功：`state.hotkey` 只在这条回执之后才换成新键。
    HotkeyApplied(String),
    /// 改键失败（spec D2）：`kept = Some(旧键)` 表示旧键已回滚可用；
    /// `kept = None` 表示连旧键都占不到，本会话只剩按钮。
    HotkeyConflict {
        wanted: String,
        kept: Option<String>,
    },
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
    /// 返回 `false` 表示热键线程已经退出（例如 `Hotkeys::new()` 失败），本次改键不可能生效。
    /// 调用方必须据此当场提示 —— 否则用户会等一个永远不会来的回执（spec D2「不允许静默失败」）。
    pub fn set_hotkey(&self, name: String) -> bool {
        self.hotkey_tx.send(name).is_ok()
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
            let notify = event_tx.clone();
            std::thread::Builder::new()
                .name("delta-hotkey".to_owned())
                .spawn(move || {
                    // 注册失败（被其它程序占用）只是没有该热键，主按钮仍可用。
                    let Ok(hotkeys) = delta_hotkey::Hotkeys::new() else {
                        return;
                    };
                    // 启动读配置是 spec D3 唯一允许回落 F8 的地方，且必须留痕。
                    let resolved: Vec<(String, Hotkey, Cmd)> = bindings
                        .iter()
                        .map(|(name, cmd)| (name.clone(), platform::startup_hotkey(name), *cmd))
                        .collect();
                    let mut routes: Vec<(u32, Cmd)> = Vec::new();
                    let mut scene_id: Option<u32> = None;
                    // 「一键识别」当前真正在案的键名 + 实际注册用的 `Hotkey`。
                    // 改键失败时靠它回滚（D2），因此不能只记名字、更不能重新解析。
                    let (mut scene_name, mut scene_key) = resolved
                        .iter()
                        .find(|(_, _, cmd)| matches!(cmd, Cmd::Trigger))
                        .map_or_else(
                            || (String::new(), Hotkey::default()),
                            |(name, hotkey, _)| (name.clone(), *hotkey),
                        );
                    for (_, hotkey, cmd) in &resolved {
                        if let Ok(id) = hotkeys.register(*hotkey) {
                            if matches!(cmd, Cmd::Trigger) {
                                scene_id = Some(id);
                            }
                            routes.push((id, *cmd));
                        }
                    }
                    while !stop.load(Ordering::Relaxed) {
                        // 改键只重绑「一键识别」；其余三键改配置文件后重启生效。
                        if let Ok(wanted) = hotkey_rx.try_recv() {
                            if let Some(id) = scene_id.take() {
                                let _ = hotkeys.unregister(id);
                                routes.retain(|(route, _)| *route != id);
                            }
                            // 解析失败只可能是捕获表与解析表漂移，一样按「注册失败」显形。
                            let registered = platform::parse_hotkey(&wanted)
                                .map_err(|_| ())
                                .and_then(|hotkey| {
                                    hotkeys.register(hotkey).map(|id| (id, hotkey)).map_err(|_| ())
                                });
                            match registered {
                                Ok((id, hotkey)) => {
                                    scene_id = Some(id);
                                    routes.push((id, Cmd::Trigger));
                                    scene_name = wanted.clone();
                                    scene_key = hotkey;
                                    let _ = notify.send(UiEvent::HotkeyApplied(wanted));
                                }
                                Err(()) => {
                                    let kept = restore_scene(
                                        &hotkeys,
                                        &scene_name,
                                        scene_key,
                                        &mut routes,
                                        &mut scene_id,
                                    );
                                    let _ = notify.send(UiEvent::HotkeyConflict { wanted, kept });
                                }
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

/// 改键失败后的回滚（spec D2）：把旧键重新注册回去。
///
/// 返回界面该显示的「仍为 X」；`None` = 连旧键都占不到（接口被别的程序抢走了）。
fn restore_scene(
    hotkeys: &HotkeyRegistry,
    scene_name: &str,
    scene_key: Hotkey,
    routes: &mut Vec<(u32, Cmd)>,
    scene_id: &mut Option<u32>,
) -> Option<String> {
    let Ok(id) = hotkeys.register(scene_key) else {
        eprintln!("[hotkey] 旧键 {scene_name} 也注册不上，本会话只剩按钮可用");
        return None;
    };
    *scene_id = Some(id);
    routes.push((id, Cmd::Trigger));
    Some(scene_name.to_owned())
}

/// 取当前配置快照（锁内只做 clone，不做耗时操作）。
fn snapshot(config: &Arc<Mutex<Config>>) -> Config {
    config
        .lock()
        .map(|config| config.clone())
        .unwrap_or_default()
}

/// 整屏像素矩形。
fn full_rect(monitor: MonitorInfo) -> PxRect {
    PxRect {
        x: 0,
        y: 0,
        w: monitor.w,
        h: monitor.h,
    }
}

/// 非任务路径的一次性抓屏：**独立会话**，抓一帧即 `drop`（spec D3 的两个例外之一）。
///
/// `backend` 一律由调用点传配置里的 `capture_backend`：用户选了哪个后端就全程用它，
/// 不再有一条「偷偷固定 BitBlt」的旁路。这些调用点都在 UI 线程，Wgc 会在这一帧里
/// 一次性初始化设备与采集会话（约 30~50ms）；由于预览/回显都有缓存（只在
/// `preview.is_none()` 或框选完成时抓一次），这笔开销不会落到每帧上。
///
/// 每次调用新建会话也意味着**不占任务的 20 张配额**（`FRAME_BUDGET` 随任务会话生灭）。
pub fn grab_once(rect: PxRect, backend: CaptureBackend) -> Result<Frame, String> {
    let mut session = CaptureSession::new(backend).map_err(|error| error.to_string())?;
    session.grab(rect).map_err(|error| error.to_string())
}

/// 截一张全屏（物理像素）。
pub fn grab_full(monitor: MonitorInfo, backend: CaptureBackend) -> Result<Frame, String> {
    grab_once(full_rect(monitor), backend)
}

/// 建一次识别任务的采集会话（spec D3：任务开始建、结束 `Drop`）。
///
/// 失败即报错，**不回退到 BitBlt**（spec D5 显式边界：自动降级会掩盖问题）。
fn open_session(config: &Config, tx: &mpsc::Sender<UiEvent>) -> Option<CaptureSession> {
    match CaptureSession::new(config.capture_backend) {
        Ok(session) => Some(session),
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
            None
        }
    }
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
    let Some(mut session) = open_session(&snapshot, tx) else {
        return;
    };
    match session.grab(full_rect(monitor)) {
        Ok(frame) => {
            let verdict = delta_scene::detect(&frame, &view);
            let scene = map_scene(verdict.scene);
            let az3 = if scene == SceneKind::Console {
                delta_scene::az3_variant(&frame, &view)
            } else {
                delta_scene::Az3Kind::None
            };
            let sequence = if scene == SceneKind::Console && reads_led_sequence(az3) {
                read_sequence(&mut session, led_region_for(&view, az3))
                    .ok()
                    .map(|raw| delta_ui::format_sequence(&raw))
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
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
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
    // spec D3：任务开始建会话；函数任何出口（成功 / 失败 / 取消 / 超时）都会 `drop` 它，
    // 因此程序空闲时不可能持有 D3D11 设备或采集会话。
    let Some(mut session) = open_session(&snapshot, tx) else {
        return;
    };

    let frame = match session.grab(full_rect(monitor)) {
        Ok(frame) => frame,
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
            return;
        }
    };
    // 强制模式（上游 F5/F6 的等价物）：跳过判别直接用指定场景。
    // Console 有两条实现路径（v5 旧操作台 / AZ3 4 类玩法），所以必须先问 AZ3：
    // 命中才走 `run_az3`，未命中一律留给旧 `run_console` —— 否则旧操作台会被 AZ3 顶掉。
    // 自动模式下若常规判别为 Unknown，再进一步检查是否为 AZ3 4 类玩法之一；
    // 仍为 Az3Kind::None 时立即拒绝，零输入（spec D6）。
    let (scene, az3_kind, actionable, reason) = match forced {
        Some(SceneKind::Console) => (
            SceneKind::Console,
            delta_scene::az3_variant(&frame, &view),
            true,
            "",
        ),
        Some(kind) => (kind, delta_scene::Az3Kind::None, true, ""),
        None => {
            let verdict = delta_scene::detect(&frame, &view);
            if verdict.is_actionable() {
                let mapped = map_scene(verdict.scene);
                let az3 = if mapped == SceneKind::Console {
                    delta_scene::az3_variant(&frame, &view)
                } else {
                    delta_scene::Az3Kind::None
                };
                (mapped, az3, true, verdict.reason)
            } else {
                let az3 = delta_scene::az3_variant(&frame, &view);
                if az3.is_known() {
                    (SceneKind::Console, az3, true, "检测到 AZ3 操作台界面")
                } else {
                    (
                        SceneKind::Unknown,
                        delta_scene::Az3Kind::None,
                        false,
                        verdict.reason,
                    )
                }
            }
        }
    };
    let sequence = if scene == SceneKind::Console && reads_led_sequence(az3_kind) {
        read_sequence(&mut session, led_region_for(&view, az3_kind))
            .ok()
            .map(|raw| delta_ui::format_sequence(&raw))
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
        SceneKind::Morse => run_morse(&snapshot, &view, &mut session, tx),
        SceneKind::Fingerprint => run_fingerprint(&snapshot, &view, &mut session, tx),
        SceneKind::Console => {
            if az3_kind.is_known() {
                // AZ3 实时抓帧上限 300 帧 / 15s（仅任务期存在，结束即 Drop；空闲期零抓帧）。
                drop(session);
                run_az3(&snapshot, &view, az3_kind, monitor, cancel, tx);
            } else {
                // 旧操作台复用当前会话（配额与留证逻辑都在 `run_console` 内），
                // 会话随本函数出口 Drop。
                run_console(&snapshot, &view, &mut session, monitor, tx);
            }
        }
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

/// 该分支是否需要读 LED 目标序列：只有信号线台与 v5 旧操作台有 LED 读数区。
///
/// 大保险 / 任务保险 / 黄键台没有 LED，多抓一帧只是白烧配额。
fn reads_led_sequence(az3: delta_scene::Az3Kind) -> bool {
    matches!(
        az3,
        delta_scene::Az3Kind::SignalLine | delta_scene::Az3Kind::None
    )
}

/// LED 读数区：AZ3 信号线台有自己的位置，旧操作台用 v5 的，不能混用。
fn led_region_for(view: &PxView, az3: delta_scene::Az3Kind) -> PxRect {
    match az3 {
        delta_scene::Az3Kind::SignalLine => view.az3.line_led_region,
        _ => view.console.led_region,
    }
}

/// 读操作台 LED 目标序列（只读，不发输入）；走任务会话，因此计入配额。
fn read_sequence(session: &mut CaptureSession, led_region: PxRect) -> Result<String, String> {
    let frame = session
        .grab(led_region)
        .map_err(|error| error.to_string())?;
    delta_vision::read_glyph_string(&frame.luma()).map_err(|error| error.to_string())
}

/// 摩斯流程（薄适配：`LiveScreen` / `LiveInput` 由 flow crate 提供）。
fn run_morse(config: &Config, view: &PxView, session: &mut CaptureSession, tx: &mpsc::Sender<UiEvent>) {
    let mut input = delta_flow_morse::LiveInput;
    let job = delta_flow_morse::MorseRun {
        view,
        timings: &config.timings,
        // 上游 morse_auto_click 的等价物：控制输入完成后是否自动点确认。
        auto_click: config.auto_confirm,
    };
    // 适配层只借用会话：跑完即释放借用，才能读 `frames_taken` 判配额。
    let outcome = {
        let mut screen = delta_flow_morse::LiveScreen::new(session);
        delta_flow_morse::run(&mut screen, &mut input, &job)
    };
    if over_budget(session, tx, view.monitor, config.capture_backend) {
        return;
    }
    let total = outcome.cells.len();
    let _ = tx.send(UiEvent::Progress {
        done: outcome.cells.iter().filter(|cell| cell.digit.is_some()).count(),
        total,
        note: "已输入数字".to_owned(),
    });
    if outcome.digits_complete() {
        let _ = tx.send(UiEvent::Result(delta_ui::morse_result(
            &outcome.password,
            config.auto_confirm,
        )));
        let _ = tx.send(UiEvent::Succeeded(format!("已输入数字 {}", outcome.password)));
    } else {
        let _ = tx.send(UiEvent::Result(delta_ui::morse_result(
            &outcome.password,
            config.auto_confirm,
        )));
        let _ = tx.send(UiEvent::Failed(format!("未识别 {}", outcome.password)));
    }
}

/// 指纹流程（模板库按 exe 目录解析；OCR 兜底按配置开关）。
fn run_fingerprint(
    config: &Config,
    view: &PxView,
    session: &mut CaptureSession,
    tx: &mpsc::Sender<UiEvent>,
) {
    let images = crate::images_dir();
    let library = match delta_templates::RoleLibrary::load_from_dir(&images) {
        Ok(library) => library,
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
            return;
        }
    };
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
    let outcome = {
        let mut screen = delta_flow_fingerprint::LiveScreen::new(session);
        delta_flow_fingerprint::run(
            &mut screen,
            &mut input,
            reader.as_mut().map(|reader| reader as &mut dyn delta_flow_fingerprint::TextReader),
            &job,
        )
    };
    if over_budget(session, tx, view.monitor, config.capture_backend) {
        return;
    }
    match outcome {
        Ok(outcome) => {
            let _ = tx.send(UiEvent::Progress {
                done: outcome.clicks.len(),
                total: outcome.pairs.len(),
                note: "已点完格子".to_owned(),
            });
            // 指纹结果区：按模板号升序的点击顺序（`1 → 3 → 5`），标签点明是否真的点了。
            let order: Vec<u32> = outcome.pairs.iter().map(|pair| pair.tmpl_no).collect();
            let _ = tx.send(UiEvent::Result(delta_ui::fingerprint_result(
                &order,
                config.fingerprint_auto_click,
            )));
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

/// AZ3 统一闭环入口（使用 `CaptureSession::with_budget` 300 帧 / 15s 预算）。
fn run_az3(
    config: &Config,
    view: &PxView,
    kind: delta_scene::Az3Kind,
    monitor: MonitorInfo,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<UiEvent>,
) {
    let budget = view.az3.frame_budget.max(delta_flow_az3::AZ3_FRAME_BUDGET);
    let mut session = match CaptureSession::with_budget(config.capture_backend, budget) {
        Ok(s) => s,
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
            return;
        }
    };
    let mut pointer = delta_flow_az3::SystemPointer::new();
    let mut clock = delta_flow_az3::SystemClock::new();
    let mut screen = delta_flow_az3::SystemScreen::new(&mut session);
    // 信号线台现在是「每帧自适应定位 + 只上下拖」的闭环，入口在 `line::run`；
    // 其余三类仍走统调度。
    let outcome = if kind == delta_scene::Az3Kind::SignalLine {
        delta_flow_az3::line::run(
            &mut screen,
            &mut pointer,
            &mut clock,
            delta_flow_az3::line::read_led_sequence,
            &view.az3,
            cancel,
        )
    } else {
        delta_flow_az3::run(kind, &mut screen, &mut pointer, &mut clock, &view.az3, cancel)
    };
    if session.frames_taken() >= session.budget() {
        let detail = format!("已抓帧数: {}（上限 {}）", session.frames_taken(), session.budget());
        save_failure_evidence("AZ3 截图次数超限", monitor, config.capture_backend, &detail);
        let _ = tx.send(UiEvent::Failed("截图次数超限".to_owned()));
        return;
    }
    match outcome {
        delta_flow_az3::Az3Outcome::Done { kind, steps, summary, .. } => {
            save_evidence(
                "ok",
                &format!("{} 已完成", kind.label()),
                monitor,
                config.capture_backend,
                &format!("AZ3类型: {}", kind.label()),
            );
            let _ = tx.send(UiEvent::Progress {
                done: steps,
                total: steps.max(1),
                note: summary.clone(),
            });
            let _ = tx.send(UiEvent::Result(delta_ui::az3_result(kind.label(), &summary)));
            let _ = tx.send(UiEvent::Succeeded(format!("{} · 已完成", kind.label())));
        }
        delta_flow_az3::Az3Outcome::Aborted { kind, reason, events } => {
            save_failure_evidence(
                &reason,
                monitor,
                config.capture_backend,
                &format!("AZ3类型: {}, 已发输入事件: {events}", kind.label()),
            );
            let _ = tx.send(UiEvent::Failed(reason));
        }
    }
}

/// 操作台流程（`flow-console` 自带 `SystemPointer` / `SystemClock`；截图走任务会话）。
fn run_console(
    config: &Config,
    view: &PxView,
    session: &mut CaptureSession,
    monitor: MonitorInfo,
    tx: &mpsc::Sender<UiEvent>,
) {
    let mut pointer = delta_flow_console::SystemPointer::new();
    let mut clock = delta_flow_console::SystemClock::new();
    let params = delta_flow_console::ConsoleParams::default().with_timings(&config.timings);
    let outcome = {
        let mut screen = delta_flow_console::SystemScreen::new(session);
        delta_flow_console::run(&mut screen, &mut pointer, &mut clock, &view.console, &params)
    };
    if over_budget(session, tx, monitor, config.capture_backend) {
        return;
    }
    match outcome {
        Ok(outcome) => {
            if outcome.is_done() {
                save_evidence(
                    "ok",
                    "操作台已完成",
                    monitor,
                    config.capture_backend,
                    &format!("已发输入事件: {}", outcome.events()),
                );
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
                save_failure_evidence(
                    &reason,
                    monitor,
                    config.capture_backend,
                    &format!("已发输入事件: {}", outcome.events()),
                );
                let _ = tx.send(UiEvent::Failed(reason));
            }
        }
        Err(error) => {
            let reason = error.to_string();
            save_failure_evidence(&reason, monitor, config.capture_backend, "已发输入事件: 0");
            let _ = tx.send(UiEvent::Failed(reason));
        }
    }
}

/// 配额是否已用尽（纯判定，便于单测）。
fn budget_exhausted(used: u32) -> bool {
    used >= FRAME_BUDGET
}

/// 配额用尽（spec D4）：转成固定文案 + 留证，且**只报一次**。
///
/// 返回 `true` 表示本次任务已经按「超限」收尾，调用方必须立即返回。
fn over_budget(
    session: &CaptureSession,
    tx: &mpsc::Sender<UiEvent>,
    monitor: MonitorInfo,
    backend: CaptureBackend,
) -> bool {
    if !budget_exhausted(session.frames_taken()) {
        return false;
    }
    let detail = format!(
        "已抓帧数: {}（上限 {FRAME_BUDGET}）",
        session.frames_taken()
    );
    save_failure_evidence("截图次数超限", monitor, backend, &detail);
    let _ = tx.send(UiEvent::Failed("截图次数超限".to_owned()));
    true
}

/// 另一个后端：只给留证路径做兜底用。
fn other_backend(backend: CaptureBackend) -> CaptureBackend {
    match backend {
        CaptureBackend::Wgc => CaptureBackend::BitBlt,
        CaptureBackend::BitBlt => CaptureBackend::Wgc,
    }
}

/// 留证抓屏：先用配置的后端，失败再换另一个后端补齐一次。
///
/// 留证发生在任务**已经失败之后**：若任务正是因所选后端初始化失败而挂，
/// 只用它留证等于没有证据。这是留证路径专有的例外，识别任务本身绝不自动降级（spec D5）。
fn evidence_frame(monitor: MonitorInfo, backend: CaptureBackend) -> Result<Frame, String> {
    let first = match grab_full(monitor, backend) {
        Ok(frame) => return Ok(frame),
        Err(error) => error,
    };
    let retry = other_backend(backend);
    grab_full(monitor, retry).map_err(|error| {
        format!(
            "{} 失败：{first}；{} 重试也失败：{error}",
            backend.label(),
            retry.label()
        )
    })
}

/// 任务留证：全屏截图 + 文本日志，落到 `logs/task_<时间戳>_<tag>.*`。
///
/// 自测期**成功与失败都留**（用户要求「每次按 F 都能保留数据」）：截屏能看出当时
/// 卡在哪一列、目标序列读成了什么、面板有没有消失；日志记录时间、结论、分辨率与
/// `detail`（操作台是已发事件数，配额超限是已抓帧数）。任何一步失败都只打 stderr，
/// **不允许把主流程带崩**。
fn save_evidence(
    tag: &str,
    reason: &str,
    monitor: MonitorInfo,
    backend: CaptureBackend,
    detail: &str,
) {
    let dir = std::path::Path::new("logs");
    if let Err(error) = std::fs::create_dir_all(dir) {
        eprintln!("[console] 日志目录创建失败：{error}");
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let base = dir.join(format!("task_{stamp}_{tag}"));

    // 留证抓屏是 spec D4 允许的例外之一：独立会话、每次任务最多 1 张、不计入 20 张配额。
    match evidence_frame(monitor, backend) {
        Ok(frame) => {
            if let Err(error) = write_frame_png(&base.with_extension("png"), &frame) {
                eprintln!("[console] 截图保存失败：{error}");
            }
        }
        Err(error) => eprintln!("[console] 截图抓取失败：{error}"),
    }

    let text = format!(
        "时间(unix): {stamp}\n结论: {reason}\n分辨率: {}x{}\n{detail}\n",
        monitor.w, monitor.h
    );
    if let Err(error) = std::fs::write(base.with_extension("txt"), text) {
        eprintln!("[console] 日志保存失败：{error}");
    }
    prune_evidence(dir);
}

/// 证据保留组数（每组 png + txt）。超过就删最旧的，避免自测一晚上攒出几个 G。
const EVIDENCE_KEEP: usize = 40;

/// 只保留最近 [`EVIDENCE_KEEP`] 组 `task_*` 证据。读不到目录就静默返回。
fn prune_evidence(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut groups: std::collections::BTreeMap<u64, Vec<std::path::PathBuf>> =
        std::collections::BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("task_") else {
            continue;
        };
        let Some(stamp) = rest.split('_').next().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        groups.entry(stamp).or_default().push(entry.path());
    }
    while groups.len() > EVIDENCE_KEEP {
        let Some((_, files)) = groups.pop_first() else {
            break;
        };
        for path in files {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 失败留证（保留旧入口名，语义等同 `save_evidence("fail", ..)`）。
fn save_failure_evidence(
    reason: &str,
    monitor: MonitorInfo,
    backend: CaptureBackend,
    detail: &str,
) {
    save_evidence("fail", reason, monitor, backend, detail);
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
    fn evidence_falls_back_to_the_other_backend() {
        // 留证兜底的对象必须真的「另一个」，否则任务因所选后端挂掉时拿不到证据。
        assert_eq!(other_backend(CaptureBackend::Wgc), CaptureBackend::BitBlt);
        assert_eq!(other_backend(CaptureBackend::BitBlt), CaptureBackend::Wgc);
    }

    #[test]
    fn budget_is_exhausted_only_at_the_limit() {
        assert!(!budget_exhausted(0), "没抓过帧不算超限");
        assert!(!budget_exhausted(FRAME_BUDGET - 1), "少一张还能抓");
        assert!(budget_exhausted(FRAME_BUDGET), "正好用满即超限");
        assert!(budget_exhausted(FRAME_BUDGET + 1), "越界也算超限");
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

    /// 两个 LED 区可区分的最小视图：旧操作台 LED 在 (869,113)，AZ3 在 (809,175)。
    fn led_view() -> PxView {
        PxView {
            monitor: MonitorInfo::new(1280, 720).expect("合法显示器"),
            regions: Vec::new(),
            fingerprint: delta_config::PxFingerprint {
                name_region: PxRect::ZERO,
                number_region: PxRect::ZERO,
                candidate_boxes: Vec::new(),
            },
            console: delta_config::PxConsole {
                led_region: PxRect {
                    x: 869,
                    y: 113,
                    w: 378,
                    h: 47,
                },
                panel_region: PxRect::ZERO,
                col_x: Vec::new(),
                row_y: Vec::new(),
                knob_center: PxPoint::ZERO,
            },
            az3: delta_config::PxAz3 {
                line_led_region: PxRect {
                    x: 809,
                    y: 175,
                    w: 288,
                    h: 54,
                },
                ..delta_config::PxAz3::default()
            },
            confirm_clicks: Vec::new(),
        }
    }

    #[test]
    fn led_sequence_is_read_only_for_signal_line_and_legacy_console() {
        // 大保险/任务保险/黄键台没有 LED 区，读它等于白烧一张截图配额。
        assert!(reads_led_sequence(delta_scene::Az3Kind::SignalLine));
        assert!(reads_led_sequence(delta_scene::Az3Kind::None));
        assert!(!reads_led_sequence(delta_scene::Az3Kind::SafeTiming));
        assert!(!reads_led_sequence(delta_scene::Az3Kind::SafePins));
        assert!(!reads_led_sequence(delta_scene::Az3Kind::ModuleKeys));
    }

    #[test]
    fn led_region_follows_the_route_instead_of_the_v5_default() {
        let view = led_view();
        assert_eq!(
            led_region_for(&view, delta_scene::Az3Kind::SignalLine),
            view.az3.line_led_region,
            "AZ3 信号线台必须读自己的 LED 区"
        );
        assert_eq!(
            led_region_for(&view, delta_scene::Az3Kind::None),
            view.console.led_region,
            "旧操作台必须读 v5 的 LED 区"
        );
    }
}
