//! 后台执行层：热键泵线程 + 识别执行线程，全部通过 `mpsc` 与界面通信。
//!
//! 铁律（t9 禁止项）：识别绝不在 UI 线程跑。UI 线程只做两件事：`try_recv` 事件、
//! `request_repaint` 唤醒重绘。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use delta_capture::{CaptureBackend, CaptureSession, Frame, FRAME_BUDGET};
use delta_config::{Config, PxView, Timings};
use delta_flow_az3::Clock;
use delta_geom::{MonitorInfo, PxPoint, PxRect};
use delta_hotkey::Hotkey;
use delta_scene::locate::Locate;
use delta_ui::SceneKind;

use crate::{platform, ui_guard};

/// 热键等待切片：既保证热键响应，也保证线程能及时看到退出标志。
const HOTKEY_POLL: Duration = Duration::from_millis(200);

/// 总开关关掉时给用户的唯一一句解释（检查入口与识别入口共用，防两处文案漂移）。
const NOTICE_MASTER_PAUSED: &str = "识别已暂停（设置里可重新打开）";

/// 两帧认出的界面不一致时的解释（界面在识别过程中变了 ⇒ 零注入）。
const CHANGED_NOTICE: &str = "界面在识别过程中变了，请重新按 F";

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
    /// 改键成功：界面只在这条回执之后才换显示（`slot` 指明是哪一个键）。
    HotkeyApplied {
        slot: delta_config::HotkeySlot,
        name: String,
    },
    /// 改键失败（spec D2）：`kept = Some(旧键)` 表示旧键已回滚可用；
    /// `kept = None` 表示连旧键都占不到，本会话只剩按钮。
    HotkeyConflict {
        wanted: String,
        kept: Option<String>,
    },
    /// 识别总开关被热键翻转（`enabled` = 翻转后的新状态）。
    ///
    /// 界面收到后只做两件事：把标题栏胶囊切过去 + 把新状态落盘 —— 识别是否真的停，
    /// 早在配置里定过了（`run_once` 入口读它）。
    MasterToggled { enabled: bool },
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
    /// 翻转识别总开关（不发起任何识别）。
    ToggleMaster,
    /// 请求取消（在流程边界生效）。
    Cancel,
}

/// 唤醒界面的回调：后台在改完状态后调一次，让界面立刻画一帧。
///
/// 用回调而不是直接持有 `egui::Context`：后台不必认识 UI 框架，测试也能换成计数器。
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// 后台句柄。
pub struct Worker {
    pub events: mpsc::Receiver<UiEvent>,
    pub commands: mpsc::Sender<Cmd>,
    /// 改键通道：把新热键名送进热键泵线程，实现「改完立刻生效」。
    hotkey_tx: mpsc::Sender<(delta_config::HotkeySlot, String)>,
    /// 唤醒界面的把手（首帧由 UI 线程注入）。
    ///
    /// 少了它，后台改完状态只能等界面「下一次碰巧跑一帧」——egui 空闲时不重绘，
    /// 用户按 `F9` 关掉识别，标题栏还挂着「识别中」，看着就像按键没用。
    waker: Arc<Mutex<Option<Waker>>>,
    shutdown: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl Worker {
    /// 热更新全局热键：改完立即生效，无需重启。
    ///
    /// 返回 `false` 表示热键线程已经退出（例如 `Hotkeys::new()` 失败），本次改键不可能生效。
    /// 调用方必须据此当场提示 —— 否则用户会等一个永远不会来的回执（spec D2「不允许静默失败」）。
    /// 请求改键：只重绑被指定的槽位。
    pub fn set_hotkey(&self, slot: delta_config::HotkeySlot, name: String) -> bool {
        self.hotkey_tx.send((slot, name)).is_ok()
    }

    /// 交出唤醒把手：UI 线程首帧调用一次；重复调用只是覆盖同一个 `Context`，无副作用。
    pub fn wake_with(&self, ctx: egui::Context) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(Arc::new(move || ctx.request_repaint()));
        }
    }
}

/// 请界面立刻画一帧（把手还没注入时什么也不做，不至于因此卡住后台）。
fn wake(waker: &Mutex<Option<Waker>>) {
    if let Ok(slot) = waker.lock() {
        if let Some(call) = slot.as_ref() {
            call();
        }
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
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));

        // 五个热键：一键识别（自动判别）+ 三个强制键（对齐上游 F5/F6，另加操作台 F7）
        // + 识别总开关（F9）。五个都能在设置里改；改键消息带槽位，泵只重绑被改的那一个。
        let bindings: Vec<(delta_config::HotkeySlot, String)> = {
            let keys = match config.lock() {
                Ok(cfg) => cfg.hotkeys.clone(),
                Err(_) => delta_config::Hotkeys::default(),
            };
            delta_config::HotkeySlot::ALL
                .iter()
                .map(|slot| (*slot, slot.read(&keys).to_owned()))
                .collect()
        };
        let (hotkey_tx, hotkey_rx) =
            mpsc::channel::<(delta_config::HotkeySlot, String)>();
        {
            let trigger = commands.clone();
            let stop = shutdown.clone();
            let notify = event_tx.clone();
            let cancel = cancel.clone();
            let config = config.clone();
            let waker = waker.clone();
            std::thread::Builder::new()
                .name("delta-hotkey".to_owned())
                .spawn(move || {
                    // 注册失败（被其它程序占用）只是没有该热键，主按钮仍可用。
                    let Ok(hotkeys) = delta_hotkey::Hotkeys::new() else {
                        return;
                    };
                    // 启动读配置是 spec D3 唯一允许回落 F8 的地方，且必须留痕。
                    let mut routes: Vec<(u32, Cmd)> = Vec::new();
                    // 每个槽当前在案的 `(注册 id, 实际 Hotkey, 键名)`；改键失败靠它回滚（D2），
                    // 所以不能只记名字，也不能事后重新解析。
                    let mut bound: [Option<(u32, Hotkey, String)>; 5] =
                        [None, None, None, None, None];
                    for (slot, name) in &bindings {
                        let hotkey = platform::startup_hotkey(name);
                        match hotkeys.register(hotkey) {
                            Ok(id) => {
                                bound[slot.index()] = Some((id, hotkey, name.clone()));
                                routes.push((id, cmd_for(*slot)));
                            }
                            // 缺少这个键不代表程序不可用（主按钮还在），但**必须留痕**：
                            // 尤其是总开关键 —— 按了没反应却查不到原因是最难报的一类故障。
                            // eprintln 在 release 下没人看得见，所以同时走「改键失败」那条
                            // 界面提示通路（`kept: None` = 启动就没注册上，显示名保持旧值）。
                            Err(error) => {
                                eprintln!(
                                    "[hotkey] {} 的 {} 注册不上（{error}），本会话没有这个键",
                                    slot.label(),
                                    name
                                );
                                let _ = notify.send(UiEvent::HotkeyConflict {
                                    wanted: name.clone(),
                                    kept: None,
                                });
                            }
                        }
                    }
                    while !stop.load(Ordering::Relaxed) {
                        // 改键：只重绑被改的那一个槽位，其余三个原样不动。
                        if let Ok((slot, wanted)) = hotkey_rx.try_recv() {
                            let index = slot.index();
                            let cmd = cmd_for(slot);
                            let previous = bound[index].take();
                            if let Some((id, _, _)) = previous {
                                let _ = hotkeys.unregister(id);
                                routes.retain(|(route, _)| *route != id);
                            }
                            // 解析失败只可能是捕获表与解析表漂移，一样按「注册失败」显形。
                            let registered = platform::parse_hotkey(&wanted)
                                .ok()
                                .and_then(|hotkey| hotkeys.register(hotkey).ok().map(|id| (id, hotkey)));
                            match registered {
                                Some((id, hotkey)) => {
                                    bound[index] = Some((id, hotkey, wanted.clone()));
                                    routes.push((id, cmd));
                                    let _ = notify
                                        .send(UiEvent::HotkeyApplied { slot, name: wanted });
                                }
                                None => {
                                    // 回滚：把这一槽原来的键重新注册回去。
                                    let kept = previous.and_then(|(_, hotkey, name)| {
                                        let id = hotkeys.register(hotkey).ok()?;
                                        routes.push((id, cmd));
                                        bound[index] = Some((id, hotkey, name.clone()));
                                        Some(name)
                                    });
                                    let _ = notify.send(UiEvent::HotkeyConflict { wanted, kept });
                                }
                            }
                        }
                        if let Some(event) = hotkeys.next_event(HOTKEY_POLL) {
                            if let Some((_, cmd)) = routes.iter().find(|(id, _)| *id == event.id) {
                                // 总开关就地翻转，不排进执行器队列：那一队是按顺序跑的，
                                // 正跑着一轮识别时排进去要等它结束（最多 15 秒），
                                // 「对枪时按一下就能停」等不起。
                                if *cmd == Cmd::ToggleMaster {
                                    toggle_master(&config, &notify, &cancel);
                                    wake(&waker);
                                } else if trigger.send(*cmd).is_err() {
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
            let waker = waker.clone();
            std::thread::Builder::new()
                .name("delta-executor".to_owned())
                .spawn(move || {
                    while let Ok(command) = command_rx.recv() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        // 先把界面叫醒再干活：热键是在窗口没有焦点时按下的，
                        // 不主动要一帧，「已开始识别」要等到用户下次碰窗口才显示得出来。
                        wake(&waker);
                        match command {
                            Cmd::Cancel => cancel.store(true, Ordering::SeqCst),
                            // 正常路径在热键泵里就地翻转（见泵内注释）；这里是第二条通路：
                            // 谁把 ToggleMaster 塞进队列，也一样按同一份实现生效。
                            Cmd::ToggleMaster => toggle_master(&config, &event_tx, &cancel),
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
            waker,
            shutdown,
            cancel,
        }
    }

    /// 请求取消（流程边界生效；单次流程内部不可抢占，见 `run_once` 注释）。
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// 槽位 → 该键按下后派发的命令。
fn cmd_for(slot: delta_config::HotkeySlot) -> Cmd {
    match slot {
        // 总开关是唯一一个「不发起识别」的槽位：它只翻转放行权。
        delta_config::HotkeySlot::Master => Cmd::ToggleMaster,
        delta_config::HotkeySlot::Scene => Cmd::Trigger,
        delta_config::HotkeySlot::Morse => Cmd::Force(SceneKind::Morse),
        delta_config::HotkeySlot::Fingerprint => Cmd::Force(SceneKind::Fingerprint),
        delta_config::HotkeySlot::Console => Cmd::Force(SceneKind::Console),
    }
}

/// 翻转识别总开关：**配置是唯一真相**，界面只是它的镜子。
///
/// 读-改-写必须在同一把锁里完成 —— 标题栏按钮和热键可能同时来翻同一个开关，
/// 先读后写会互相覆盖成一个谁都没按过的状态。
///
/// 关掉时顺手置 `cancel`：当前这一轮在下一个流程边界收手，不再往下点键
/// （用户拍板：「立刻中断」）。开启时**不**清 `cancel` —— 那是别人的请求，
/// 不该被这个键顺手撤销；真正的清零点在每个新任务入口。
fn toggle_master(config: &Mutex<Config>, notify: &mpsc::Sender<UiEvent>, cancel: &AtomicBool) {
    let enabled = match config.lock() {
        Ok(mut config) => {
            let wanted = !config.master_enabled;
            config.master_enabled = wanted;
            wanted
        }
        // 锁中毒 = 配置内容已不可信：宁可什么都不做，也不在不确定的状态下继续放行识别。
        Err(_) => return,
    };
    if !enabled {
        cancel.store(true, Ordering::SeqCst);
    }
    let _ = notify.send(UiEvent::MasterToggled { enabled });
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

// ── 就绪等待（Q1/Q3：面板 0.4s 才淡入、首颗亮键 1.8s 才出现）──

/// ready 等待的总预算：面板出现到第一颗可识别亮键实测 1.4s × 1.5（spec P1）。
pub const READY_BUDGET_MS: u64 = 2100;
/// 等待期间的抓帧间隔（40ms ≈ 25fps；2.1s 约 52 帧，任务会话 300 帧装得下）。
const READY_POLL_MS: u64 = 40;
/// 场景判别的容忍线：连续这么多帧 `Unknown` **且** 已耗这么久才允许拒绝（spec P6）。
const SCENE_UNKNOWN_FRAMES: u32 = 3;
const SCENE_UNKNOWN_MIN_MS: u64 = 1200;

/// 通用就绪等待：每轮取一次 `probe`，`Found` 立刻返回；预算用尽返回最后一次的三态。
///
/// **每轮读一次 `cancel`**（铁律：关掉总开关后不许还干等 2.1s）。
/// `NotYet` / `Absent` 都不提前退出 —— 0~1.8s 之间画面本来就是「什么都没有」，
/// 提前判失败正是「首开要按两次 F」的根因（spec B1/B2）。
fn wait_ready<T>(
    clock: &mut impl Clock,
    cancel: &AtomicBool,
    budget_ms: u64,
    min_probes: u32,
    mut probe: impl FnMut() -> Locate<T>,
) -> Locate<T> {
    let began = clock.now_ms();
    let mut probes = 0u32;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Locate::Absent;
        }
        probes += 1;
        let seen = probe();
        if seen.is_found() {
            return seen;
        }
        let elapsed = clock.now_ms().saturating_sub(began);
        if elapsed >= budget_ms && probes >= min_probes {
            return seen;
        }
        clock.sleep_ms(READY_POLL_MS.min(budget_ms.saturating_sub(elapsed).max(1)));
    }
}

/// 等画面能被判别成一个已知界面（Q3：摩斯面板打开也有 1.4s 空窗）。
///
/// 「已知」= 常规判别命中，或 AZ3 二级判别命中任一玩法。
fn wait_scene_ready(
    session: &mut CaptureSession,
    full: PxRect,
    view: &PxView,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<UiEvent>,
) -> Locate<Frame> {
    let mut clock = delta_flow_az3::SystemClock::new();
    wait_ready(
        &mut clock,
        cancel,
        SCENE_UNKNOWN_MIN_MS,
        SCENE_UNKNOWN_FRAMES,
        || match session.grab(full) {
            Ok(frame) => {
                let known = delta_scene::detect(&frame, view).is_actionable()
                    || delta_scene::az3_variant(&frame, view).is_known();
                if known {
                    Locate::Found(frame)
                } else {
                    Locate::NotYet
                }
            }
            Err(error) => {
                let _ = tx.send(UiEvent::Failed(error.to_string()));
                Locate::Absent
            }
        },
    )
}

/// 等黄键台真的可以动手：面板可定位 **且** 亮键像素 ≥3000px@720p，返回就绪的那一帧。
fn wait_keys_ready(
    session: &mut CaptureSession,
    full: PxRect,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<UiEvent>,
) -> Locate<Frame> {
    let mut clock = delta_flow_az3::SystemClock::new();
    wait_ready(&mut clock, cancel, READY_BUDGET_MS, 1, || {
        match session.grab(full) {
            Ok(frame) => match delta_scene::locate::keys_ready(&frame) {
                Locate::Found(()) => Locate::Found(frame),
                Locate::NotYet => Locate::NotYet,
                Locate::Absent => Locate::Absent,
            },
            Err(error) => {
                let _ = tx.send(UiEvent::Failed(error.to_string()));
                Locate::Absent
            }
        }
    })
}

/// Q5 第二道闸的三种结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiGate {
    /// 模板命中：这就是某个解锁界面 ⇒ 免掉场景投票，直接按它开跑。
    Strong(delta_scene::Scene),
    /// 说不清（灰带 / 模板库为空 / 抓不到帧）⇒ 原路径完全不变。
    Pass,
    /// 一张都不像 ⇒ 不是解锁界面，零注入。
    Reject(&'static str),
}

/// Q5 第二道闸：先看一眼这是不是**某个解锁界面**（正样本模板）。
///
/// 抓帧只抓**一帧**、且用独立会话（不占任务配额）；模板库为空 / 抓帧失败一律 `Pass`
/// （缺素材绝不拒绝识别，抓不到帧也交给后面的正式路径报错）。
fn ui_gate(guard: &ui_guard::UiGuard, snapshot: &Config, monitor: MonitorInfo) -> UiGate {
    if guard.is_empty() {
        return UiGate::Pass;
    }
    let Ok(frame) = grab_full(monitor, snapshot.capture_backend) else {
        return UiGate::Pass;
    };
    match guard.verdict(&frame) {
        ui_guard::UiVerdict::Target(scene) => UiGate::Strong(scene),
        ui_guard::UiVerdict::Reject(reason) => {
            save_evidence(
                "reject",
                reason,
                monitor,
                snapshot.capture_backend,
                "Q5 第二道闸（UI 模板未命中任何解锁界面）",
            );
            UiGate::Reject(reason)
        }
        ui_guard::UiVerdict::Gray => UiGate::Pass,
    }
}

/// 就绪判别的依据 —— 前两种都**不必**等结构判别，直接抓当前帧。
#[derive(Debug, Clone, Copy)]
enum ReadyBasis {
    /// UI 模板已确认是某个解锁界面（面板已经画出来了，再等只是白等 1.2s）。
    Template(delta_scene::Scene),
    /// 强制模式（F5/F6/F7）：用户按键就是命令。
    Forced,
    /// 自动模式：等结构判别长起来（原路径）。
    Detect,
}

impl ReadyBasis {
    /// 已知场景或强制模式 ⇒ 场景投票整段不参与。
    fn skips_detect(self) -> bool {
        !matches!(self, ReadyBasis::Detect)
    }

    /// 模板确认过的场景（给免投票的场景分类用）。
    fn confirmed(self) -> Option<delta_scene::Scene> {
        match self {
            ReadyBasis::Template(scene) => Some(scene),
            ReadyBasis::Forced | ReadyBasis::Detect => None,
        }
    }
}

/// 失败留证需要的两样东西：哪块屏、哪个采集后端。
#[derive(Debug, Clone, Copy)]
struct Evidence {
    monitor: MonitorInfo,
    backend: CaptureBackend,
}

/// 等到一帧**真的可以动手**的画面；拿不到就当场把拒绝/失败原因发出去并返回 `None`。
///
/// 三件事收在这一个函数里（调用点不必知道先等什么后等什么）：
/// 1. 等场景判别出结果（Q3：`Unknown` 连续 3 帧且 ≥1.2s 才判失败，spec B12）；
/// 2. 模板命中 / 强制模式跳过判别（前者已经知道是哪个界面，后者用户按键就是命令）；
/// 3. 取消位在同一轮里生效（用户关掉总开关不该还干等）。
fn await_frame(
    session: &mut CaptureSession,
    full: PxRect,
    view: &PxView,
    cancel: &AtomicBool,
    basis: ReadyBasis,
    evidence: Evidence,
    tx: &mpsc::Sender<UiEvent>,
) -> Option<Frame> {
    if basis.skips_detect() || wait_scene_ready(session, full, view, cancel, tx).is_found() {
        return grab_now(session, full, tx);
    }
    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(UiEvent::Failed("已取消".to_owned()));
        return None;
    }
    let reason = format!("{SCENE_UNKNOWN_MIN_MS}ms / {SCENE_UNKNOWN_FRAMES} 帧内没认出界面");
    save_evidence(
        "reject",
        &reason,
        evidence.monitor,
        evidence.backend,
        "场景判别失败",
    );
    let _ = tx.send(UiEvent::Rejected("未检测到游戏界面".to_owned()));
    None
}

/// 补抓一帧；失败即报错。
fn grab_now(
    session: &mut CaptureSession,
    full: PxRect,
    tx: &mpsc::Sender<UiEvent>,
) -> Option<Frame> {
    match session.grab(full) {
        Ok(frame) => Some(frame),
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
            None
        }
    }
}

/// 开跑前的最后一道复核：变体必须还是同一个，且（黄键台的）就绪判据必须成立。
///
/// 返回 `Err(文案)` = 必须拒绝（调用方零注入退出）。
fn confirm_az3(
    verdict: Locate<Frame>,
    view: &PxView,
    expected: delta_scene::Az3Kind,
    config: &Config,
    monitor: MonitorInfo,
) -> Result<(), String> {
    let frame = match verdict {
        Locate::Found(frame) => frame,
        Locate::NotYet => {
            let reason = format!(
                "{} {}ms 内没亮起来（面板已定位，但亮键像素不足 {}px）",
                expected.label(),
                READY_BUDGET_MS,
                delta_scene::locate::READY_LIT_MIN_PX
            );
            save_evidence("reject", &reason, monitor, config.capture_backend, "就绪等待超时");
            return Err(reason);
        }
        Locate::Absent => {
            let reason = format!("{} 面板已消失", expected.label());
            save_evidence("reject", &reason, monitor, config.capture_backend, "就绪等待失败");
            return Err(reason);
        }
    };
    let confirmed = delta_scene::az3_variant(&frame, view);
    if confirmed == expected {
        return Ok(());
    }
    let reason = format!(
        "界面复核不一致（判别为 {}，复核为 {}）",
        expected.label(),
        confirmed.label()
    );
    save_evidence("reject", &reason, monitor, config.capture_backend, "变体复核");
    Err(reason)
}

/// 只读检查：找窗 + 场景判别，**零输入**。
fn probe(config: &Arc<Mutex<Config>>, monitor: MonitorInfo, tx: &mpsc::Sender<UiEvent>) {
    let _ = tx.send(UiEvent::Started);
    let snapshot = snapshot(config);
    if !snapshot.master_enabled {
        // 总开关关掉：热键还能按下，但这里直接返回 —— 不截图、不注入、不弹流程。
        let _ = tx.send(UiEvent::Rejected(NOTICE_MASTER_PAUSED.to_owned()));
        return;
    }
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

    // 总开关闸门必须排在**所有**副作用之前：不激活游戏窗口、不建采集会话、不抓帧、不注入。
    // 少了它，关掉总开关后按 F8/F5/F6/F7 仍会完整跑一轮并真的去动键盘
    // （用户拍板：「关掉就别动我的键盘」）。
    if !snapshot.master_enabled {
        let _ = tx.send(UiEvent::Rejected(NOTICE_MASTER_PAUSED.to_owned()));
        return;
    }

    // Q5 第二道闸（用户拍板：解锁界面用 UI 判定模板）：先看一眼这是不是**某个解锁界面**。
    // 命中 ⇒ 免掉场景投票直接按它开跑；一张都不像 ⇒ 零注入当场拒绝；说不清 ⇒ 一切照旧。
    // 放在 `focus_game_window()` 之前：先把「不该动的界面」挡掉，再谈激活游戏。
    // 强制模式（F5/F6/F7）不参与这道闸 —— 用户按键就是命令，连模板都不必读。
    //
    // 注意这里每轮都重新扫目录 + 解码模板：当前 3 张 200×112（约 33KB）合计远低于 1ms，
    // 改模板后立刻生效，不值得为它引入缓存失效逻辑。
    // shortcut: 模板 ≤ ~20 张时直读；超过就该加「目录 mtime 命中即复用」的缓存。
    let guard = match forced {
        Some(_) => ui_guard::UiGuard::default(),
        None => ui_guard::UiGuard::load(&crate::ui_images_dir()),
    };
    let strong = match forced {
        Some(_) => None,
        None => match ui_gate(&guard, &snapshot, monitor) {
            UiGate::Strong(scene) => Some(scene),
            UiGate::Reject(reason) => {
                let _ = tx.send(UiEvent::Rejected(reason.to_owned()));
                return;
            }
            UiGate::Pass => None,
        },
    };
    let basis = match strong {
        Some(scene) => ReadyBasis::Template(scene),
        None if forced.is_some() => ReadyBasis::Forced,
        None => ReadyBasis::Detect,
    };

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
    let full = full_rect(monitor);

    // Q1/Q3：面板 0.4s 才淡入、首颗亮键 1.8s 才出现 —— 单帧判别必然判失败，
    // 这就是「第一次按 F 没用、要按两次」的根因。这里等画面真的长起来再判。
    let Some(frame) = await_frame(
        &mut session,
        full,
        &view,
        cancel,
        basis,
        Evidence {
            monitor,
            backend: snapshot.capture_backend,
        },
        tx,
    ) else {
        return;
    };
    // 第二帧复核（spec B15/B17）：用**本来就要用的这一帧**再判一次，零额外抓帧。
    // 单帧命中不足以下手：两帧不一致就说明界面在变，宁可交回判别/拒绝。
    let hint = match basis.confirmed() {
        Some(expected) => match ui_guard::recheck(
            ui_guard::UiVerdict::Target(expected),
            guard.verdict(&frame),
        ) {
            ui_guard::Recheck::Keep(scene) => Some(scene),
            ui_guard::Recheck::Downgrade => None,
            ui_guard::Recheck::Changed => {
                let _ = tx.send(UiEvent::Rejected(CHANGED_NOTICE.to_owned()));
                return;
            }
        },
        None => None,
    };
    // 强制模式（上游 F5/F6 的等价物）：跳过判别直接用指定场景。
    // Console 有两条实现路径（v5 旧操作台 / AZ3 4 类玩法），所以必须先问 AZ3：
    // 命中才走 `run_az3`，未命中一律留给旧 `run_console` —— 否则旧操作台会被 AZ3 顶掉。
    // 自动模式：UI 模板命中就直接采信；否则常规判别，仍为 Unknown 时再问 AZ3 4 类玩法，
    // 都没有就立即拒绝，零输入（spec D6）。
    let (scene, az3_kind, actionable, reason) = match forced {
        Some(SceneKind::Console) => (
            SceneKind::Console,
            delta_scene::az3_variant(&frame, &view),
            true,
            "",
        ),
        Some(kind) => (kind, delta_scene::Az3Kind::None, true, ""),
        None => auto_classify(&frame, &view, hint),
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
        // 自测期把「按了热键但没认出界面」也留一份证：否则用户只能说「按了没用」，
        // 没法区分是钩子压根没触发、还是判成了 Unknown（两种情况修法完全不同）。
        save_evidence(
            "reject",
            &format!("未识别：{reason}"),
            monitor,
            snapshot.capture_backend,
            &format!("场景: {scene:?}, AZ3: {az3_kind:?}"),
        );
        let _ = tx.send(UiEvent::Rejected(reason.to_owned()));
        return;
    }
    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(UiEvent::Failed("已取消".to_owned()));
        return;
    }

    // Q1/Q5：**真开跑前再抓一帧**。
    // - 黄键台：必须「面板可定位 **且** 亮键 ≥3000px」两条同时成立（spec B2/B3、P1）；
    // - 同时复核 AZ3 变体，不一致立即拒绝 —— 把「误判 × 上百次乱点」压到 ≤1 帧。
    if scene == SceneKind::Console && az3_kind.is_known() {
        let verdict = if az3_kind == delta_scene::Az3Kind::ModuleKeys {
            wait_keys_ready(&mut session, full, cancel, tx)
        } else {
            // 其余 AZ3 玩法没有「亮键像素」这条判据，复核用的就是刚判别过的那一帧。
            Locate::Found(frame.clone())
        };
        if let Err(reason) = confirm_az3(verdict, &view, az3_kind, &snapshot, monitor) {
            let _ = tx.send(UiEvent::Rejected(reason));
            return;
        }
    }

    match scene {
        SceneKind::Morse => run_morse(&snapshot, &view, &mut session, cancel, tx),
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

/// 自动模式的场景分类：**UI 模板优先**（它已经回答了「是哪个界面」），否则结构判别 + AZ3 兜底。
///
/// 返回 `(场景, AZ3 变体, 是否可执行, 原因)`。
fn auto_classify(
    frame: &Frame,
    view: &PxView,
    hint: Option<delta_scene::Scene>,
) -> (SceneKind, delta_scene::Az3Kind, bool, &'static str) {
    // 模板命中就不必投票：结构分数只可能把已经确定的事情投乱。
    if let Some(known) = hint {
        let scene = map_scene(known);
        return (
            scene,
            az3_variant_of(scene, frame, view),
            true,
            "UI 模板确认解锁界面",
        );
    }
    let verdict = delta_scene::detect(frame, view);
    if verdict.is_actionable() {
        let scene = map_scene(verdict.scene);
        return (scene, az3_variant_of(scene, frame, view), true, verdict.reason);
    }
    let az3 = delta_scene::az3_variant(frame, view);
    if az3.is_known() {
        return (SceneKind::Console, az3, true, "检测到 AZ3 操作台界面");
    }
    (
        SceneKind::Unknown,
        delta_scene::Az3Kind::None,
        false,
        verdict.reason,
    )
}

/// 只有 `Console` 有 AZ3 子玩法（v5 旧操作台 / AZ3 四类玩法），其余场景恒 `None`。
fn az3_variant_of(
    scene: SceneKind,
    frame: &Frame,
    view: &PxView,
) -> delta_scene::Az3Kind {
    if scene == SceneKind::Console {
        delta_scene::az3_variant(frame, view)
    } else {
        delta_scene::Az3Kind::None
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
fn run_morse(
    config: &Config,
    view: &PxView,
    session: &mut CaptureSession,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<UiEvent>,
) {
    let mut input = delta_flow_morse::LiveInput;
    let job = delta_flow_morse::MorseRun {
        view,
        timings: &config.timings,
        // 上游 morse_auto_click 的等价物：控制输入完成后是否自动点确认。
        auto_click: config.auto_confirm,
        // 输入横条 ROI：输错接力的观察窗口（未标定 ⇒ 零面积 ⇒ 不做错误态观察）。
        panel: delta_flow_morse::panel_roi(view),
    };
    // 适配层只借用会话：跑完即释放借用，才能读 `frames_taken` 判配额。
    let outcome = {
        let mut screen = delta_flow_morse::LiveScreen::new(session);
        delta_flow_morse::run(&mut screen, &mut input, &job, cancel)
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
    if outcome.is_complete() {
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
        // 输错接力失败也说清是哪一档失败：解码失败 vs 连续敲错。
        let detail = outcome
            .errors
            .last()
            .cloned()
            .unwrap_or_else(|| format!("未识别 {}", outcome.password));
        let _ = tx.send(UiEvent::Failed(detail));
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
    // exe 同级目录（不能用相对路径：双击启动时 cwd 可能是 System32，用户找不到文件）。
    let dir = crate::logs_dir();
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
    prune_evidence(&dir);
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

    /// 总开关热键：一次按下 = 配置翻转 + 界面收到新状态 + 关掉时立刻中断当前这一轮。
    #[test]
    fn master_toggle_flips_the_config_and_cancels_the_running_round() {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc::channel;

        let config = Arc::new(Mutex::new(Config::default()));
        let cancel = AtomicBool::new(false);

        // 关：配置真翻过去 + 立刻请求中断 + 通知界面。
        let (tx, rx) = channel();
        assert!(config.lock().expect("锁").master_enabled, "默认开");
        toggle_master(&config, &tx, &cancel);
        assert!(!config.lock().expect("锁").master_enabled, "配置必须真翻过去");
        assert!(cancel.load(Ordering::SeqCst), "关掉时必须请求中断当前这一轮");
        assert!(
            matches!(rx.try_recv(), Ok(UiEvent::MasterToggled { enabled: false })),
            "界面必须收到「已关」"
        );

        // 开：状态翻回来、界面收到「已开」，但**不**顺手撤销别人请求的中断。
        let (tx, rx) = channel();
        toggle_master(&config, &tx, &cancel);
        assert!(config.lock().expect("锁").master_enabled);
        assert!(
            cancel.load(Ordering::SeqCst),
            "开启不该撤销中断请求（清零点在任务入口）"
        );
        assert!(
            matches!(rx.try_recv(), Ok(UiEvent::MasterToggled { enabled: true })),
            "界面必须收到「已开」"
        );
    }

    /// 总开关关掉后，**真实识别入口**也必须一个输入事件都不产生（spec §5.3 t1 / B3 真实路径）。
    ///
    /// 只断言那一条 `NOTICE_MASTER_PAUSED`：它只能由闸门在 `focus_game_window()` 与
    /// `try_px_view()` **之前**发出，所以文案对上即证明没激活游戏窗口、没建采集会话、没抓帧。
    #[test]
    fn paused_master_makes_run_once_stop_before_touching_the_game_window() {
        use std::sync::mpsc::channel;

        let config = Arc::new(Mutex::new(Config::default()));
        config.lock().expect("锁").master_enabled = false;
        let cancel = AtomicBool::new(false);
        let (tx, rx) = channel();

        run_once(
            &config,
            MonitorInfo::new(1280, 720).expect("合法显示器"),
            &tx,
            &cancel,
            None,
        );

        let events: Vec<UiEvent> = rx.try_iter().collect();
        assert_eq!(
            events.len(),
            2,
            "关掉时只许发「开始」+「已暂停」，多一条就意味着跑了识别：{events:?}"
        );
        assert!(matches!(events[0], UiEvent::Started), "先报开始");
        assert!(
            matches!(&events[1], UiEvent::Rejected(notice) if notice == NOTICE_MASTER_PAUSED),
            "必须是总开关闸门的文案，实际：{:?}",
            events[1]
        );
        assert!(
            !cancel.load(Ordering::SeqCst),
            "闸门是只读放行判断，不该动别人的取消位"
        );
    }

    /// 只读检查入口与识别入口共用同一道闸门与同一句文案（防两处文案漂移）。
    #[test]
    fn paused_master_gate_uses_the_same_notice_for_probe_and_run_once() {
        use std::sync::mpsc::channel;

        let config = Arc::new(Mutex::new(Config::default()));
        config.lock().expect("锁").master_enabled = false;
        let (tx, rx) = channel();

        probe(
            &config,
            MonitorInfo::new(1280, 720).expect("合法显示器"),
            &tx,
        );

        let events: Vec<UiEvent> = rx.try_iter().collect();
        assert_eq!(events.len(), 2, "关掉时检查入口也不许抓图：{events:?}");
        assert!(
            matches!(&events[1], UiEvent::Rejected(notice) if notice == NOTICE_MASTER_PAUSED),
            "检查入口文案必须与识别入口同源，实际：{:?}",
            events[1]
        );
    }

    /// 总开关槽位必须派发翻转命令，而不是任何一个识别命令。
    #[test]
    fn master_slot_maps_to_the_toggle_command() {
        assert_eq!(cmd_for(delta_config::HotkeySlot::Master), Cmd::ToggleMaster);
        assert_ne!(cmd_for(delta_config::HotkeySlot::Scene), Cmd::ToggleMaster);
    }

    /// 唤醒把手没注入时不能崩；注入后必须真的调一次。
    ///
    /// 这条守的是实测踩到的坑：热键在窗口没焦点时按下，egui 空闲不重绘，
    /// 后台改完状态界面还挂着旧显示 —— 用户看到的结论就是「这个键没用」。
    #[test]
    fn wake_calls_the_injected_waker_only_after_injection() {
        use std::sync::atomic::AtomicUsize;

        let hits = Arc::new(AtomicUsize::new(0));
        let slot: Mutex<Option<Waker>> = Mutex::new(None);

        wake(&slot);
        assert_eq!(hits.load(Ordering::SeqCst), 0, "还没注入把手时不该有动作");

        let counter = hits.clone();
        *slot.lock().expect("锁") = Some(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        wake(&slot);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "注入后每次唤醒必须真的调一次");
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

    /// 纯黑画面：模板命中路径根本不看内容（`detect` 不参与），正好用来证明这一点。
    fn black_frame() -> Frame {
        Frame::new(
            128,
            96,
            vec![0u8; 128 * 96 * 4],
            std::time::Instant::now(),
        )
        .expect("合成帧")
    }

    #[test]
    fn a_template_hit_routes_without_asking_the_scene_detector() {
        // 「UI 模板确认解锁界面」这句原因只有 `ReadyBasis::Template` 分支会产生，
        // 拿到它 = 场景投票确实没跑（全黑帧也不可能靠结构分数认出任何界面）。
        let (scene, az3, actionable, reason) = auto_classify(
            &black_frame(),
            &led_view(),
            Some(delta_scene::Scene::Morse),
        );
        assert_eq!(scene, SceneKind::Morse);
        assert_eq!(az3, delta_scene::Az3Kind::None);
        assert!(actionable, "模板命中 ⇒ 可以直接开跑");
        assert_eq!(reason, "UI 模板确认解锁界面");
    }

    #[test]
    fn an_unknown_screen_without_a_template_stays_zero_injection() {
        let (scene, _, actionable, _) = auto_classify(&black_frame(), &led_view(), None);
        assert_eq!(scene, SceneKind::Unknown);
        assert!(!actionable, "既没模板命中又认不出界面 ⇒ 必须零注入");
    }

    #[test]
    fn only_a_confirmed_template_or_force_mode_skips_scene_detection() {
        assert!(ReadyBasis::Template(delta_scene::Scene::Console).skips_detect());
        assert!(ReadyBasis::Forced.skips_detect());
        assert!(!ReadyBasis::Detect.skips_detect(), "灰带 / 无模板必须照旧走 detect");
        assert_eq!(
            ReadyBasis::Template(delta_scene::Scene::Morse).confirmed(),
            Some(delta_scene::Scene::Morse)
        );
        assert_eq!(ReadyBasis::Forced.confirmed(), None);
        assert_eq!(ReadyBasis::Detect.confirmed(), None);
    }
}
