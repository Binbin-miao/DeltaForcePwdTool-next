//! 后台执行层：热键泵线程 + 识别执行线程，全部通过 `mpsc` 与界面通信。
//!
//! 铁律（t9 禁止项）：识别绝不在 UI 线程跑。UI 线程只做两件事：`try_recv` 事件、
//! `request_repaint` 唤醒重绘。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use delta_capture::{CaptureBackend, CaptureSession, Frame};
use delta_config::{Config, Flow, FlowFlags, PxView, Timings};
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
    /// 跑完了但结论**没有被证据确认**（黄键台退化档：进度格读不出）。
    ///
    /// 单独立一条，不复用 `Failed`：`Failed` 在界面上是「未识别」（压根没认出场子），
    /// 与「点过了但没法核实」是两回事；也不能走 `Succeeded` —— 那就是用户实测的假成功。
    Unverified(String),
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
    /// 强制跑指定 AZ3 变体（F10 黄键台 / F11 横线台）：连「操作台里是哪个台子」都不判，
    /// 用户按键即命令（见 `run_once` 的 `forced_az3`）。
    ForceAz3(delta_scene::Az3Kind),
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
    busy: Arc<AtomicBool>,
    polling: Arc<AtomicBool>,
}

impl Worker {
    #[cfg(test)]
    pub(crate) fn offline() -> Self {
        let (_event_tx, events) = mpsc::channel();
        let (commands, _command_rx) = mpsc::channel();
        let (hotkey_tx, _hotkey_rx) = mpsc::channel();
        Self {
            events,
            commands,
            hotkey_tx,
            waker: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            polling: Arc::new(AtomicBool::new(false)),
        }
    }

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
        // 忙时门：执行器一开工就关上，热键泵据此把重复按键**在按下那一刻**丢掉。
        // 只在执行器里判断是不够的 —— 执行器是单线程顺序消费的，忙时进队的按键会
        // 一直躺在队列里，这一轮结束（成功/失败/取消）后立刻又被跑一遍，
        // 用户看到的就是「按了三下，跑三次」。
        let busy = Arc::new(AtomicBool::new(false));
        let polling = Arc::new(AtomicBool::new(false));
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));

        // 七个热键：一键识别（自动判别）+ 五个强制键（F5 摩斯 / F6 指纹 / F7 操作台
        // / F10 黄键台 / F11 横线台）+ 识别总开关（F9）。全部都能在设置里改；
        // 改键消息带槽位，泵只重绑被改的那一个。
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
            let busy = busy.clone();
            let polling = polling.clone();
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
                    // 长度直接取自 `HotkeySlot::ALL`：加槽位时这里不必再回来数一遍。
                    let mut bound: [Option<(u32, Hotkey, String)>; delta_config::HotkeySlot::ALL.len()] =
                        std::array::from_fn(|_| None);
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
                    let mut last_trigger_at = std::time::Instant::now()
                        .checked_sub(Duration::from_secs(10))
                        .unwrap_or_else(std::time::Instant::now);
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
                                } else if *cmd == Cmd::Cancel {
                                    // 取消同理就地生效：排进队列要等这一轮跑完（最多 15 秒），
                                    // 而「按住 F 想立刻停」等不起。
                                    cancel.store(true, Ordering::SeqCst);
                                    wake(&waker);
                                } else if busy.load(Ordering::SeqCst) {
                                    // 仅在「仍在等待场景轮询且距上次触发 ≥400ms」时允许重按打断重试；
                                    // 一旦进入实际输入/点击/拖动阶段（!polling），绝不打断正在执行的输入！
                                    if polling.load(Ordering::SeqCst)
                                        && last_trigger_at.elapsed() >= Duration::from_millis(400)
                                    {
                                        last_trigger_at = std::time::Instant::now();
                                        cancel.store(true, Ordering::SeqCst);
                                        wake(&waker);
                                        if trigger.send(*cmd).is_err() {
                                            break;
                                        }
                                    }
                                } else if last_trigger_at.elapsed() >= Duration::from_millis(250) {
                                    last_trigger_at = std::time::Instant::now();
                                    if trigger.send(*cmd).is_err() {
                                        break;
                                    }
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
            let gate = busy.clone();
            let polling = polling.clone();
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
                                while matches!(command_rx.try_recv(), Ok(Cmd::Trigger)) {}
                                if gate.swap(true, Ordering::SeqCst) {
                                    continue;
                                }
                                let _gate = BusyGate(&gate);
                                cancel.store(false, Ordering::SeqCst);
                                run_once(&config, monitor, &event_tx, &cancel, Some(&polling), None, None);
                            }
                            Cmd::Force(kind) => {
                                if gate.swap(true, Ordering::SeqCst) {
                                    continue;
                                }
                                let _gate = BusyGate(&gate);
                                cancel.store(false, Ordering::SeqCst);
                                run_once(&config, monitor, &event_tx, &cancel, Some(&polling), Some(kind), None);
                            }
                            Cmd::ForceAz3(kind) => {
                                if gate.swap(true, Ordering::SeqCst) {
                                    continue;
                                }
                                let _gate = BusyGate(&gate);
                                cancel.store(false, Ordering::SeqCst);
                                // AZ3 变体都住在「操作台」这个场景下：强制场景 = Console，
                                // 强制变体藏在第二个参数里（跳过 `az3_variant` 投票）。
                                run_once(
                                    &config,
                                    monitor,
                                    &event_tx,
                                    &cancel,
                                    Some(&polling),
                                    Some(SceneKind::Console),
                                    Some(kind),
                                );
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
            busy,
            polling,
        }
    }

    /// 请求取消（流程边界生效；单次流程内部不可抢占，见 `run_once` 注释）。
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// 供 F 键钩子转发线程使用的取消句柄：仅在仍在等待场景轮询时才允许重按 F 打断重试。
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn busy_flag(&self) -> Arc<AtomicBool> {
        self.busy.clone()
    }

    pub fn polling_flag(&self) -> Arc<AtomicBool> {
        self.polling.clone()
    }
}

/// 忙时门的放行凭据：`run_once` 正常结束、提前取消、超时返回，甚至 panic 展开，
/// 都必须把门重新打开 —— 手写 `store(false)` 漏一条出口，之后所有热键就永久失灵了。
struct BusyGate<'a>(&'a AtomicBool);

impl Drop for BusyGate<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
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
        delta_config::HotkeySlot::Keys => Cmd::ForceAz3(delta_scene::Az3Kind::ModuleKeys),
        delta_config::HotkeySlot::Line => Cmd::ForceAz3(delta_scene::Az3Kind::SignalLine),
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
/// 配额取 [`READY_FRAME_BUDGET`]：就绪等待两阶段最多烧 150 帧，默认的 20 张连 2 秒都撑不住
/// （见该常量的注释）。失败即报错，**不回退到 BitBlt**（spec D5 显式边界：自动降级会掩盖问题）。
fn open_session(config: &Config, tx: &mpsc::Sender<UiEvent>) -> Option<CaptureSession> {
    match CaptureSession::with_budget(config.capture_backend, READY_FRAME_BUDGET) {
        Ok(session) => Some(session),
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(error.to_string()));
            None
        }
    }
}

/// 抓帧失败的原因文案：**配额用尽必须单独点明**。
///
/// 配额耗尽与「画面抓不到」是两件完全不同的事，混成「未检测到游戏界面」会把下一轮排查
/// 直接带偏（会去怀疑模板/分辨率，而真正的原因是帧数早用完了）。
fn grab_note(error: delta_capture::CaptureError) -> String {
    match error {
        delta_capture::CaptureError::BudgetExceeded { limit } => {
            format!("抓帧配额用尽（上限 {limit} 张）：本次任务抓帧次数已用完，请重试")
        }
        other => other.to_string(),
    }
}

// ── 就绪等待（Q1/Q3：面板 0.4s 才淡入、首颗亮键 1.8s 才出现）──

/// 就绪轮询的**总预算**：既是 [`wait_keys_ready`] 等面板的余额，也是场景等待的上界。
///
/// 必须 > [`QUICK_REJECT_MS`]（现 5000），否则拒绝分支等不满、退化成 `Exhausted`。
/// 6000 = 5000 拒绝时限 + 1000 抓帧/编码余量（[`READY_POLL_MS`] 一帧 ⇒ 12 帧余量）。
/// 为什么是 6000 而不是 3500：黄键台首次打开有「拧按钮」的开场动画，动画期间模板分低、
/// 面板结构也量不全（`keys_panel_ready` 不给 `Found`），3500 会在动画结束前就放弃，
/// 正好错过动画结束后那第一次亮黄键 —— 用户看到的就是「拧完亮黄键，程序完全没反应」。
/// 历史上这里是 2100（「面板出现到第一颗亮键 1.4s × 1.5」），就绪闸不再看亮键后
/// 这个依据已失效，改为服从上面的不等式。
pub const READY_BUDGET_MS: u64 = 9500;
/// 等待期间的抓帧间隔（80ms ≈ 12.5fps；6s 约 75 帧/阶段，两阶段仍在本阶段配额之内）。
///
/// 间隔必须与 [`READY_FRAME_BUDGET`] 一起看：配额是**帧数**，预算与间隔决定**帧数**。
const READY_POLL_MS: u64 = 80;
/// 就绪等待（模板闸 + 面板就绪两阶段）**共用的抓帧配额**。
///
/// 旧实现用默认 [`FRAME_BUDGET`]（20 张）：40ms 一帧 ⇒ 800ms 就把 20 张耗尽，之后
/// `grab` 直接 `BudgetExceeded`，画面再好也判不出来 —— 这是「程序完全没反应」的第二个来源。
/// 现在按最坏情况算：`READY_BUDGET_MS / READY_POLL_MS` = 75 帧/阶段，两阶段 ≤150，
/// 配额 200 留出余量（单元测试 [`ready_frame_budget_covers_both_stages`] 钉住这个不等式）。
const READY_FRAME_BUDGET: u32 = 300;
/// 场景判别的容忍线：连续这么多帧 `Unknown` 才允许判「预算耗尽」（spec P6）。
const SCENE_UNKNOWN_FRAMES: u32 = 3;
/// 快速拒绝的时间线：**5 秒**（用户拍板：「模板没命中时先等 3 秒再拒绝」，后按实测放宽到 5s）。
///
/// 0~`QUICK_REJECT_MS` 之间「一帧都不像任何解锁界面」是**正常现象**，一律不得拒绝。
/// 这个窗必须容得下三件事，缺一条就会误拒真面板：
/// 1. 面板淡入实测 0.400~1.400s（`.scratch/az3-fix-batch/spec.md` §0⑤ 锚点表）；
/// 2. 黄键台首次打开还有一段「拧按钮」开场动画，动画期间模板分同样上不去；
/// 3. 玩家镜头在**靠近过程中会移动**，同一块面板在画面里会漂、会虚 —— 实测录屏 92.00~92.75s
///    整段模板分掉到 <0.60（其余时段 0.894~0.955），随后又恢复。
///
/// 旧的 400ms 太短：40ms 一帧、400ms 只有 10 帧，镜头一动就整段落在「一帧都不像」，
/// 于是一个**已确认存在**的面板被当场拒掉 —— 这就是「按一次 F 没用」的直接原因。
///
/// **必须 < [`READY_BUDGET_MS`]**（现 6000）：轮询预算是拒绝时限的上界，预算短于时限时
/// 这条拒绝永远等不满，所有非解锁界面都会退化成「预算耗尽」的错归因。
const QUICK_REJECT_MS: u64 = 8000;

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

/// 一次轮询帧的三分类结论（**同一帧**同时喂给结构判别与 UI 模板）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollVerdict {
    /// 可以动手；`hint` = 模板确认过的场景（`None` = 交给结构判别/投票）。
    Ready(Option<delta_scene::Scene>),
    /// 还说不清，继续等（覆盖淡入期、镜头移动期与模板灰带）。
    NotYet,
    /// 明确不是解锁界面（已耗 ≥ [`QUICK_REJECT_MS`] 且模板 `Reject`）。
    Reject(&'static str),
}

/// `SafePins` 兜底要连续成立多少帧才放行（其余判据仍是单帧放行）。
///
/// 实测依据：跑图时「像任务保险」的帧是**零星闪过**的（2026-11-16 两次误判相隔约 3 秒、
/// 中间夹着 outright `Reject`），而真面板打开后会**稳定存在**。80ms 一帧 ⇒ 3 帧 ≈ 240ms，
/// 只让稳定存在的东西通过。这是「按了 5 次空格才发现判错」的第二道止血闸。
const PINS_STREAK_FRAMES: u32 = 3;

/// `SafePins` 的连续命中计数：**只活在一次轮询循环里**（[`poll_scene_ready`] 持有）。
///
/// 绝不放进 `ui_guard`：那是跨任务复用的对象，上一次跑图残留的计数会直接带进下一次，
/// 让「零星闪过的误判帧」白捡一次连击 —— 这正是「多次误判」的成因之一。
#[derive(Debug, Default)]
struct PinsStreak {
    consecutive: u32,
}

impl PinsStreak {
    /// 记一帧 `SafePins`，返回当前连击数（含本帧）。
    fn hit(&mut self) -> u32 {
        self.consecutive += 1;
        self.consecutive
    }

    /// 记一帧「不是 `SafePins`」：连击清零。
    fn miss(&mut self) {
        self.consecutive = 0;
    }

    /// 连击是否已够放行。
    fn satisfied(&self) -> bool {
        self.consecutive >= PINS_STREAK_FRAMES
    }
}

/// 单帧综合探测：**模板优先，结构判别只在模板库为空时兜底**。
///
/// 顺序不可交换（实测依据：全片 1125 采样点里面板在时 NCC 0.894~0.955，
/// 其余 p90 只有 0.206 —— 模板的分离度远大于结构判别，它该说了算）：
///
/// 1. 模板库非空 ⇒ **模板 verdict 就是结论**：
///    - `Target(scene)` → 立刻 [`PollVerdict::Ready`]，**不再补抓第二帧复核**。
///      两帧复核会让镜头移动时把**已经确认**的面板撤回（`Downgrade` → `NotYet`），
///      实测 92.00~92.75s 就是这样把 90s 已确认的结论推翻的；
///    - `Reject` 且已过 [`QUICK_REJECT_MS`] → [`PollVerdict::Reject`]；
///    - 其余（`Reject` 未到时限 / 灰带）→ [`PollVerdict::NotYet`]。
///      这里**绝不**回落到结构判别：走廊/背包上结构判别常判出 `Console`，
///      一旦回落就是拿模板已经否掉的画面去按键盘。
/// 2. 模板库为空（素材被删/没打包）⇒ 只剩结构判别可用，命中即动手（fail-open 到 v6.7 行为），
///    并在 `run_once` 打一行「模板库为空，退化到结构判别」。
fn classify_poll_frame(
    guard: &ui_guard::UiGuard,
    frame: &Frame,
    view: &PxView,
    elapsed_ms: u64,
    pins: &mut PinsStreak,
) -> PollVerdict {
    // 1. 没有可用模板：结构判别是唯一判据（命中即动手）。
    if guard.is_empty() {
        let structural = delta_scene::detect(frame, view).is_actionable()
            || delta_scene::az3_variant(frame, view).is_known();
        return match structural {
            true => PollVerdict::Ready(None),
            false => PollVerdict::NotYet,
        };
    }
    // 2. 有模板：模板命中直接放行；模板落在 Gray/Reject 时，若多尺度自适应定位
    //    （`SignalLine` / `ModuleKeys` / `Morse`）已在当帧确认成立，则无需干等 5s 直接放行
    //    （适配 80~120 FOV 下横线台/黄键台因画面缩放落在 Gray 区的情况）。
    match guard.verdict(frame) {
        ui_guard::UiVerdict::Target(scene) => PollVerdict::Ready(Some(scene)),
        ui_guard::UiVerdict::Reject(reason) if elapsed_ms >= QUICK_REJECT_MS => {
            match delta_scene::az3_variant(frame, view) {
                delta_scene::Az3Kind::SafePins => {
                    pins.hit();
                    match pins.satisfied() {
                        true => PollVerdict::Ready(None),
                        false => PollVerdict::NotYet,
                    }
                }
                delta_scene::Az3Kind::SafeTiming
                | delta_scene::Az3Kind::SignalLine
                | delta_scene::Az3Kind::ModuleKeys => PollVerdict::Ready(None),
                delta_scene::Az3Kind::None => {
                    pins.miss();
                    if morse_cells_ready(frame, view) {
                        PollVerdict::Ready(Some(delta_scene::Scene::Morse))
                    } else {
                        PollVerdict::Reject(reason)
                    }
                }
            }
        }
        ui_guard::UiVerdict::Reject(_) | ui_guard::UiVerdict::Gray => {
            match delta_scene::az3_variant(frame, view) {
                delta_scene::Az3Kind::SignalLine | delta_scene::Az3Kind::ModuleKeys => {
                    PollVerdict::Ready(None)
                }
                _ => {
                    if morse_cells_ready(frame, view) {
                        PollVerdict::Ready(Some(delta_scene::Scene::Morse))
                    } else {
                        PollVerdict::NotYet
                    }
                }
            }
        }
    }
}



/// 摩斯三格是否已经画出可解码的点划（要求暗底面板 + 足够对比度 + 三格全部合法解码，
/// 既防止开门淡入期抢跑，也防止跑图地面纹理误触发摩斯）。
fn morse_cells_ready(frame: &Frame, view: &PxView) -> bool {
    if view.regions.is_empty() {
        return false;
    }
    let luma = frame.luma();
    view.regions.iter().all(|region| {
        let Some(cell) = delta_scene::crop(&luma, region.rect) else {
            return false;
        };
        let bytes = cell.as_slice();
        if bytes.is_empty() {
            return false;
        }
        let dark = bytes.iter().filter(|&&v| v <= 95).count();
        if dark * 100 < bytes.len() * 50 {
            return false;
        }
        let min = bytes.iter().copied().min().unwrap_or(0);
        let max = bytes.iter().copied().max().unwrap_or(0);
        max.saturating_sub(min) >= 35 && delta_flow_morse::decode_gray(&cell).is_ok()
    })
}

/// 等场景就绪的收尾结论（`Locate<T>` 表达不了「明确拒绝」这一态）。
#[derive(Debug)]
enum ReadyOutcome {
    /// 可以动手了；`hint` = 模板确认过的场景（`None` = 交给结构判别/投票）。
    Ready {
        frame: Frame,
        hint: Option<delta_scene::Scene>,
    },
    /// 明确不是解锁界面（模板库非空、且模板一张都不像；这一态已不参考结构判别）。
    Rejected(&'static str),
    /// 预算耗尽：等不到任何判据，一帧都不像已知界面。
    Exhausted,
    /// 取消 / 抓帧失败（原因已经发给界面）。
    Stopped,
}

/// 等画面能被判别成一个已知界面（Q3：摩斯面板打开也有 1.4s 空窗）。
///
/// 「已知」= 常规判别命中、AZ3 二级判别命中任一玩法、**或** UI 模板确认是某个解锁界面。
/// 三条判据共享同一帧、同一轮预算：不存在「先抓一帧判模板，再抓一帧判结构」。
/// 轮询主体：**抓帧与判定分离**，判定（就绪 / 拒绝 / 耗尽）可以零真实等待地跑。
///
/// `budget_ms` 必须大于拒绝时限 [`QUICK_REJECT_MS`]：预算先到就直接 `Exhausted`，
/// 「这不是解锁界面」这条**准确**文案永远发不出去，所有走廊/背包/地图都会被错归因成
/// 「未检测到游戏界面」。生产上由 [`READY_BUDGET_MS`] 提供，测试用它钉住这个不等式。
fn poll_scene_ready(
    clock: &mut impl Clock,
    cancel: &AtomicBool,
    guard: &ui_guard::UiGuard,
    view: &PxView,
    budget_ms: u64,
    mut probe: impl FnMut() -> Result<Frame, String>,
) -> Result<ReadyOutcome, String> {
    let began = clock.now_ms();
    let mut probes = 0u32;
    let mut pins = PinsStreak::default();
    loop {
        // 每轮读一次取消位：关掉总开关后不许还干等。
        if cancel.load(Ordering::SeqCst) {
            return Err("已取消".to_owned());
        }
        probes += 1;
        let frame = probe()?;
        let elapsed = clock.now_ms().saturating_sub(began);
        match classify_poll_frame(guard, &frame, view, elapsed, &mut pins) {
            PollVerdict::Ready(hint) => return Ok(ReadyOutcome::Ready { frame, hint }),
            PollVerdict::Reject(reason) => return Ok(ReadyOutcome::Rejected(reason)),
            PollVerdict::NotYet => {}
        }
        // 预算耗尽仍说不清 ⇒ 交给上层报「未检测到游戏界面」。
        if elapsed >= budget_ms && probes >= SCENE_UNKNOWN_FRAMES {
            return Ok(ReadyOutcome::Exhausted);
        }
        clock.sleep_ms(READY_POLL_MS.min(budget_ms.saturating_sub(elapsed).max(1)));
    }
}

/// 黄键台就绪探针：**面板结构量得出来**就带着这一帧就绪（亮键像素不参与）。
fn keys_ready_probe(frame: Frame) -> Locate<Frame> {
    match delta_scene::locate::keys_panel_ready(&frame) {
        Locate::Found(()) => Locate::Found(frame),
        Locate::NotYet => Locate::NotYet,
        Locate::Absent => Locate::Absent,
    }
}

/// 等黄键台真的可以动手：**面板结构量得出来**就算就绪（亮键像素不参与），返回就绪的那一帧。
///
/// 旧的判据是「面板可定位 **且** 亮键像素 ≥3000px@720p」—— 但面板刚开是 **1% 进度、零亮键**，
/// 于是这条闸要求「先亮起来才算打开」，必然一路 `NotYet` 到 2100ms 超时，
/// 日志里报成「就绪等待失败 / 面板已消失」，而模板分明明 0.95。
/// 亮键只作为 flow 内部的点击判据（`lit_key_pixels`），不决定「能不能开跑」。
fn wait_keys_ready(
    session: &mut CaptureSession,
    full: PxRect,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<UiEvent>,
) -> Locate<Frame> {
    let mut clock = delta_flow_az3::SystemClock::new();
    wait_ready(&mut clock, cancel, READY_BUDGET_MS, 1, || match session.grab(full) {
        Ok(frame) => keys_ready_probe(frame),
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(grab_note(error)));
            Locate::Absent
        }
    })
}

/// 信号线台就绪探针：4×4 网格定位成功即就绪；镜头推近期间返回 `NotYet` 继续等待。
fn line_ready_probe(frame: Frame) -> Locate<Frame> {
    match delta_scene::locate::locate_line(&frame) {
        Ok(_) => Locate::Found(frame),
        Err(delta_scene::locate::LocateError::Frame { .. }) => Locate::Absent,
        Err(_) => Locate::NotYet,
    }
}

/// 等信号线台 CRT 网格画出来（镜头推近动画约 200~500ms）。
fn wait_line_ready(
    session: &mut CaptureSession,
    full: PxRect,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<UiEvent>,
) -> Locate<Frame> {
    let mut clock = delta_flow_az3::SystemClock::new();
    wait_ready(&mut clock, cancel, READY_BUDGET_MS, 1, || match session.grab(full) {
        Ok(frame) => line_ready_probe(frame),
        Err(error) => {
            let _ = tx.send(UiEvent::Failed(grab_note(error)));
            Locate::Absent
        }
    })
}

/// 就绪判别的依据。
#[derive(Debug, Clone, Copy)]
enum ReadyBasis {
    /// 强制模式（F5/F6/F7）：用户按键就是命令，连模板都不必读。
    Forced,
    /// 自动模式：等面板淡入，由结构判别或 UI 模板确认。
    Detect,
}

impl ReadyBasis {
    /// 强制模式 ⇒ 场景判别整段不参与，直接抓当前帧。
    ///
    /// 写成 `matches!(Forced)` 而不是 `!matches!(Detect)`：新增变体时后者会静默 fail-open，
    /// 把「还没想清楚的新模式」当成「可以跳过判别」。
    fn skips_detect(self) -> bool {
        matches!(self, ReadyBasis::Forced)
    }
}

/// 失败留证需要的两样东西：哪块屏、哪个采集后端。
#[derive(Debug, Clone, Copy)]
struct Evidence {
    monitor: MonitorInfo,
    backend: CaptureBackend,
}

/// 一次就绪等待要用的全部上下文。
///
/// 这七样东西永远一起走（会话 / 整屏矩形 / 视图 / 模板库 / 取消位 / 留证 / 事件口），
/// 拆成裸参数既超 clippy 的 7 参上限，也让调用点读不出「它们在描述同一件事」。
struct ReadyContext<'a, S> {
    session: &'a mut S,
    full: PxRect,
    view: &'a PxView,
    guard: &'a ui_guard::UiGuard,
    cancel: &'a AtomicBool,
    evidence: Evidence,
    tx: &'a mpsc::Sender<UiEvent>,
}

/// 就绪的那一帧 + 模板确认过的场景。
struct ReadyFrame {
    frame: Frame,
    hint: Option<delta_scene::Scene>,
}

/// 等到一帧**真的可以动手**的画面；拿不到就当场把拒绝/失败原因发出去并返回 `None`。
///
/// 三件事收在这一个函数里（调用点不必知道先等什么后等什么）：
/// 1. 等面板长起来并确认界面（Q3/Q5：模板与结构判别在同一轮里出结论）；
/// 2. 明确不是解锁界面 ⇒ 立刻按模板文案拒绝（[`QUICK_REJECT_MS`] 之后）；
/// 3. 取消位在同一轮里生效（用户关掉总开关不该还干等）。
fn await_frame<D: TaskDesktop>(
    ctx: ReadyContext<'_, D::Session>,
    basis: ReadyBasis,
    desktop: &mut D,
) -> Option<ReadyFrame> {
    let ReadyContext {
        session,
        full,
        view,
        guard,
        cancel,
        evidence,
        tx,
    } = ctx;
    // 强制模式：用户按键就是命令，不进轮询、不看模板、不拒绝。
    if basis.skips_detect() {
        return match D::ready_grab(session, full) {
            Ok(frame) => Some(ReadyFrame { frame, hint: None }),
            Err(reason) => {
                let _ = tx.send(UiEvent::Failed(reason));
                None
            }
        };
    }
    let outcome = match poll_scene_ready(desktop.clock(), cancel, guard, view, READY_BUDGET_MS, || {
        D::ready_grab(session, full)
    }) {
        Ok(outcome) => outcome,
        Err(reason) => {
            let _ = tx.send(UiEvent::Failed(reason));
            ReadyOutcome::Stopped
        }
    };
    match outcome {
        ReadyOutcome::Ready { frame, hint } => Some(ReadyFrame { frame, hint }),
        ReadyOutcome::Rejected(reason) => {
            desktop.evidence("reject", reason,
                "Q5 快速拒绝（模板未命中任何解锁界面）", evidence);
            let _ = tx.send(UiEvent::Rejected(reason.to_owned()));
            None
        }
        ReadyOutcome::Exhausted => {
            let reason = format!("{READY_BUDGET_MS}ms / {SCENE_UNKNOWN_FRAMES} 帧内没认出界面");
            desktop.evidence("reject", &reason, "场景判别失败", evidence);
            let _ = tx.send(UiEvent::Rejected("未检测到游戏界面".to_owned()));
            None
        }
        ReadyOutcome::Stopped => None,
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
) -> Result<Frame, String> {
    let frame = match verdict {
        Locate::Found(frame) => frame,
        Locate::NotYet => {
            // 面板底色已在、挡位/按键阵还没量全：这是「还没画完」，不是「没亮」。
            let reason = format!(
                "{} {}ms 内没能量出完整面板（底色已在，按键阵/进度格认不全）",
                expected.label(),
                READY_BUDGET_MS
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
    if confirmed == expected
        || (expected == delta_scene::Az3Kind::SignalLine
            && delta_scene::locate::locate_line(&frame).is_ok())
        || (expected == delta_scene::Az3Kind::ModuleKeys
            && delta_scene::locate::locate_keys(&frame).is_ok())
    {
        return Ok(frame);
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
                read_sequence(&mut delta_flow_morse::LiveScreen::new(&mut session), led_region_for(&view, az3))
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

/// 功能开关闸门拦下时的唯一文案来源（`<功能名>` 与主页开关格同名）。
///
/// 必须点出「在主页哪里能打开」：只说「已关闭」，用户会以为程序坏了；
/// 反过来只关掉不提示，按 F10 毫无反应是最难查的一类现象。
fn flow_disabled_notice(flow: Flow) -> String {
    format!("{}开关已关闭（在主页开关里打开后可继续）", flow.label())
}

/// 判别结果 → 它对应的那个功能开关。`Unknown` 没有对应功能 ⇒ `None`。
///
/// 这是「哪个开关管哪条流程」的**唯一映射表**：`console` 只管 v5 旧旋钮操作台，
/// AZ3 四台各由自己的开关支配 —— 拿 `console` 一个键管四台，用户关掉大保险会连
/// 黄键台一起失效（这正是「独立开关」要避免的）。
fn flow_of(scene: SceneKind, az3: delta_scene::Az3Kind) -> Option<Flow> {
    use delta_scene::Az3Kind;
    match (scene, az3) {
        (SceneKind::Morse, _) => Some(Flow::Morse),
        (SceneKind::Fingerprint, _) => Some(Flow::Fingerprint),
        (SceneKind::Console, Az3Kind::SafeTiming) => Some(Flow::Az3Timing),
        (SceneKind::Console, Az3Kind::SafePins) => Some(Flow::Az3Pins),
        (SceneKind::Console, Az3Kind::ModuleKeys) => Some(Flow::Az3Keys),
        (SceneKind::Console, Az3Kind::SignalLine) => Some(Flow::Az3Line),
        // AZ3 四台都没命中 ⇒ 只剩 v5 旧旋钮操作台。
        (SceneKind::Console, Az3Kind::None) => Some(Flow::Console),
        // 没认出界面：与开关无关，交给原「未识别」拒绝分支（不许误报成「开关已关闭」）。
        (SceneKind::Unknown, _) => None,
    }
}

/// 开关闸门：`Ok(())` = 放行，`Err(文案)` = 零注入退出。
fn flow_gate(flows: FlowFlags, flow: Flow) -> Result<(), String> {
    match flows.get(flow) {
        true => Ok(()),
        false => Err(flow_disabled_notice(flow)),
    }
}

/// 强制入口在**抓任何一帧之前**就能确定的功能。
///
/// `F5` / `F6` / `F10` / `F11` 直接点名了场景与变体，所以它们的闸门不必等一帧；
/// `F7`（`Force(Console)` 且没指定变体）还要投票判是哪个台子 ⇒ `None`，交给判别后的闸门。
/// 强制 ≠ 绕过开关：用户关掉「黄键台」后按 F10 必须什么都不做。
fn forced_flow(forced: Option<SceneKind>, forced_az3: Option<delta_scene::Az3Kind>) -> Option<Flow> {
    match (forced, forced_az3) {
        (Some(SceneKind::Console), Some(kind)) => flow_of(SceneKind::Console, kind),
        // F7 只点名了「操作台」，是哪个台子还要投票 ⇒ 交给判别后的那道闸门。
        (Some(SceneKind::Console), None) => None,
        (Some(scene), _) => flow_of(scene, delta_scene::Az3Kind::None),
        (None, _) => None,
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
    polling: Option<&AtomicBool>,
    forced: Option<SceneKind>,
    forced_az3: Option<delta_scene::Az3Kind>,
) {
    let mut desktop = LiveDesktop {
        clock: delta_flow_az3::SystemClock::new(),
        input: delta_flow_morse::LiveInput,
        polling,
    };
    run_once_with(config, TaskRequest { monitor, tx, cancel, forced, forced_az3 }, &mut desktop);
}

#[derive(Clone, Copy)]
struct TaskRequest<'a> {
    monitor: MonitorInfo,
    tx: &'a mpsc::Sender<UiEvent>,
    cancel: &'a AtomicBool,
    forced: Option<SceneKind>,
    forced_az3: Option<delta_scene::Az3Kind>,
}

struct FlowContext<'a, S> {
    config: &'a Config,
    view: &'a PxView,
    session: S,
    frame: Frame,
    scene: SceneKind,
    az3_kind: delta_scene::Az3Kind,
    request: TaskRequest<'a>,
}

trait TaskDesktop {
    type Session: delta_flow_morse::ScreenSource;
    type Input: delta_flow_morse::InputSink;
    type Clock: Clock;

    fn load_guard(&mut self) -> ui_guard::UiGuard;
    fn focus(&mut self) -> Result<(), String>;
    fn open(&mut self, config: &Config, tx: &mpsc::Sender<UiEvent>) -> Option<Self::Session>;
    fn ready_grab(session: &mut Self::Session, rect: PxRect) -> Result<Frame, String>;
    fn clock(&mut self) -> &mut Self::Clock;
    fn input(&mut self) -> &mut Self::Input;
    fn budget(&self, session: &Self::Session) -> (u32, u32);
    fn evidence(&mut self, tag: &str, reason: &str, detail: &str, context: Evidence);
    fn prepare_az3(&mut self, flow: &mut FlowContext<'_, Self::Session>) -> bool;
    fn set_polling(&self, _active: bool) {}
    fn morse_input_boxes(&self, _monitor: MonitorInfo) -> Option<[PxRect; 3]> {
        None
    }
    fn fingerprint(&mut self, flow: FlowContext<'_, Self::Session>);
    fn console(&mut self, flow: FlowContext<'_, Self::Session>);
    fn az3(&mut self, flow: FlowContext<'_, Self::Session>);
}

struct LiveDesktop<'a> {
    clock: delta_flow_az3::SystemClock,
    input: delta_flow_morse::LiveInput,
    polling: Option<&'a AtomicBool>,
}

struct TaskCapture(CaptureSession);

impl delta_flow_morse::ScreenSource for TaskCapture {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
        self.0.grab(rect).map_err(|error| error.to_string())
    }
}

impl TaskDesktop for LiveDesktop<'_> {
    type Session = TaskCapture;
    type Input = delta_flow_morse::LiveInput;
    type Clock = delta_flow_az3::SystemClock;

    fn load_guard(&mut self) -> ui_guard::UiGuard {
        ui_guard::UiGuard::load(&crate::ui_images_dir())
    }

    fn focus(&mut self) -> Result<(), String> {
        platform::focus_game_window().map(|_| ())
    }

    fn open(&mut self, config: &Config, tx: &mpsc::Sender<UiEvent>) -> Option<Self::Session> {
        open_session(config, tx).map(TaskCapture)
    }

    fn ready_grab(session: &mut Self::Session, rect: PxRect) -> Result<Frame, String> {
        session.0.grab(rect).map_err(grab_note)
    }

    fn clock(&mut self) -> &mut Self::Clock {
        &mut self.clock
    }

    fn input(&mut self) -> &mut Self::Input {
        &mut self.input
    }

    fn budget(&self, session: &Self::Session) -> (u32, u32) {
        (session.0.frames_taken(), session.0.budget())
    }

    fn evidence(&mut self, tag: &str, reason: &str, detail: &str, context: Evidence) {
        save_evidence(tag, reason, context.monitor, context.backend, detail);
    }

    fn prepare_az3(&mut self, flow: &mut FlowContext<'_, Self::Session>) -> bool {
        let session = &mut flow.session.0;
        let request = flow.request;
        let full = full_rect(request.monitor);
        if request.forced_az3.is_some() {
            if flow.az3_kind == delta_scene::Az3Kind::ModuleKeys {
                if let Locate::Found(ready) = wait_keys_ready(session, full, request.cancel, request.tx) {
                    flow.frame = ready;
                }
            } else if flow.az3_kind == delta_scene::Az3Kind::SignalLine {
                if let Locate::Found(ready) = wait_line_ready(session, full, request.cancel, request.tx) {
                    flow.frame = ready;
                }
            }
            return true;
        }
        let verdict = if flow.az3_kind == delta_scene::Az3Kind::ModuleKeys {
            wait_keys_ready(session, full, request.cancel, request.tx)
        } else if flow.az3_kind == delta_scene::Az3Kind::SignalLine {
            wait_line_ready(session, full, request.cancel, request.tx)
        } else {
            Locate::Found(flow.frame.clone())
        };
        match confirm_az3(verdict, flow.view, flow.az3_kind, flow.config, request.monitor) {
            Ok(confirmed_frame) => {
                flow.frame = confirmed_frame;
                true
            }
            Err(reason) => {
                let _ = request.tx.send(UiEvent::Rejected(reason));
                false
            }
        }
    }

    fn set_polling(&self, active: bool) {
        if let Some(flag) = self.polling {
            flag.store(active, Ordering::SeqCst);
        }
    }

    fn morse_input_boxes(&self, monitor: MonitorInfo) -> Option<[PxRect; 3]> {
        Some(delta_flow_morse::input_boxes_for_monitor(monitor))
    }

    fn fingerprint(&mut self, mut flow: FlowContext<'_, Self::Session>) {
        run_fingerprint(flow.config, flow.view, &mut flow.session.0, flow.request.tx);
    }

    fn console(&mut self, mut flow: FlowContext<'_, Self::Session>) {
        run_console(flow.config, flow.view, &mut flow.session.0, flow.request.monitor, flow.request.tx);
    }

    fn az3(&mut self, flow: FlowContext<'_, Self::Session>) {
        drop(flow.session);
        run_az3(flow.config, flow.view, flow.az3_kind, flow.request.monitor,
            flow.request.cancel, flow.request.tx, Some(&flow.frame));
    }
}

fn run_once_with<D: TaskDesktop>(
    config: &Arc<Mutex<Config>>,
    request: TaskRequest<'_>,
    desktop: &mut D,
) {
    let TaskRequest { monitor, tx, cancel, forced, forced_az3 } = request;
    let _ = tx.send(UiEvent::Started);
    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(UiEvent::Failed("已取消".to_owned()));
        return;
    }
    let snapshot = snapshot(config);

    // 总开关闸门必须排在**所有**副作用之前：不激活游戏窗口、不建采集会话、不抓帧、不注入。
    // 少了它，关掉总开关后按 F8/F5/F6/F7 仍会完整跑一轮并真的去动键盘
    // （用户拍板：「关掉就别动我的键盘」）。
    if !snapshot.master_enabled {
        let _ = tx.send(UiEvent::Rejected(NOTICE_MASTER_PAUSED.to_owned()));
        return;
    }

    // 功能开关闸门（强制入口）：F5/F6/F10/F11 已经点名了功能，**不必等任何一帧**就能判，
    // 所以这条排在 `focus_game_window()` 与 `open_session()` 之前 —— 关掉的功能
    // 一次激活、一次抓帧都不做。强制 ≠ 绕过开关。
    if let Some(flow) = forced_flow(forced, forced_az3) {
        if let Err(notice) = flow_gate(snapshot.flows, flow) {
            let _ = tx.send(UiEvent::Rejected(notice));
            return;
        }
    }

    // Q5：模板闸**并入**面板等待循环（用户拍板：解锁界面用 UI 判定模板）。
    // 单帧先闸是不可行的 —— 面板 0~400ms 还在淡入，那一帧必然「一张都不像」，
    // 于是要么误拒、要么白等 1.2s 报「未检测到游戏界面」。搬进轮询后用**时间**区分
    // 「还没画完」与「这不是解锁界面」（见 `QUICK_REJECT_MS`）。
    //
    // 放在 `focus_game_window()` 之前加载模板：激活窗口本身是副作用，与加载无关。
    // 强制模式（F5/F6/F7）用空库（`UiGuard::default()`）—— 它完全不参与模板判定。
    //
    // 注意这里每轮都重新扫目录 + 解码模板：当前 3 张 200×112（约 33KB）合计远低于 1ms，
    // 改模板后立刻生效，不值得为它引入缓存失效逻辑。
    // shortcut: 模板 ≤ ~20 张时直读；超过就该加「目录 mtime 命中即复用」的缓存。
    let guard = match forced {
        Some(_) => ui_guard::UiGuard::default(),
        None => desktop.load_guard(),
    };
    let basis = if forced.is_some() {
        ReadyBasis::Forced
    } else {
        ReadyBasis::Detect
    };
    // 素材被删/没打包 ⇒ 模板闸整段失效，退化到结构判别。这里**每次任务只打一行**：
    // 轮询里逐帧打印会让一次 F8 刷 30 行（同 `ui_guard::should_log` 的理由）。
    if !basis.skips_detect() && guard.is_empty() {
        eprintln!("[ui] 模板库为空，退化到结构判别");
    }

    if let Err(reason) = desktop.focus() {
        let _ = tx.send(UiEvent::Rejected(reason));
        return;
    }
    let Ok(view) = snapshot.try_px_view() else {
        let _ = tx.send(UiEvent::Rejected("请先校准".to_owned()));
        return;
    };
    // spec D3:任务开始建会话；函数任何出口（成功 / 失败 / 取消 / 超时）都会 `drop` 它，
    // 因此程序空闲时不可能持有 D3D11 设备或采集会话。
    let Some(mut session) = desktop.open(&snapshot, tx) else {
        return;
    };
    let full = full_rect(monitor);

    // Q1/Q3/Q5：面板 0.4s 才淡入、首颗亮键 1.8s 才出现 —— 单帧判别必然判失败，
    // 这就是「第一次按 F 没用、要按两次」的根因。这里等画面真的长起来再判，
    // 模板判定在同一轮里完成（拒绝也从这里出去，不再是「先聚焦再白等」）。
    desktop.set_polling(true);
    let ready_frame = await_frame(
        ReadyContext {
            session: &mut session,
            full,
            view: &view,
            guard: &guard,
            cancel,
            evidence: Evidence {
                monitor,
                backend: snapshot.capture_backend,
            },
            tx,
        },
        basis,
        desktop,
    );
    desktop.set_polling(false);
    let Some(ReadyFrame {
        frame,
        hint,
    }) = ready_frame else {
        return;
    };
    // 强制模式（上游 F5/F6 的等价物）：跳过判别直接用指定场景。
    // Console 有两条实现路径（v5 旧操作台 / AZ3 4 类玩法），所以必须先问 AZ3：
    // 命中才走 `run_az3`，未命中一律留给旧 `run_console` —— 否则旧操作台会被 AZ3 顶掉。
    // 自动模式：UI 模板命中就直接采信；否则常规判别，仍为 Unknown 时再问 AZ3 4 类玩法，
    // 都没有就立即拒绝，零输入（spec D6）。
    let (scene, mut az3_kind, actionable, reason) = match forced {
        Some(SceneKind::Console) => (
            SceneKind::Console,
            // 强制指定了变体就直接采信（F10/F11）；否则才走「操作台里是哪个台子」的投票。
            forced_az3.unwrap_or_else(|| delta_scene::az3_variant(&frame, &view)),
            true,
            "",
        ),
        Some(kind) => (kind, delta_scene::Az3Kind::None, true, ""),
        None => auto_classify(&frame, &view, hint),
    };
    if scene == SceneKind::Console && az3_kind == delta_scene::Az3Kind::None {
        let template_az3 = guard.best_az3_hint(&frame);
        if template_az3.is_known() {
            az3_kind = template_az3;
        }
    }
    // 功能开关闸门（自动判别 / F7 路径）：判别必须看一眼画面，这一帧已经抓过了，
    // 但闸门排在**读序列、就绪等待、任何注入之前** —— 关掉的功能不再抓第二帧、
    // 不建第二个会话、一个输入事件都不发。
    if let Some(flow) = flow_of(scene, az3_kind) {
        if let Err(notice) = flow_gate(snapshot.flows, flow) {
            let _ = tx.send(UiEvent::Rejected(notice));
            return;
        }
    }
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
        desktop.evidence("reject", &format!("未识别：{reason}"),
            &format!("场景: {scene:?}, AZ3: {az3_kind:?}"),
            Evidence { monitor, backend: snapshot.capture_backend });
        let _ = tx.send(UiEvent::Rejected(reason.to_owned()));
        return;
    }
    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(UiEvent::Failed("已取消".to_owned()));
        return;
    }

    // Q1/Q5：**真开跑前再抓一帧**。
    // - 黄键台：只等「面板结构量得出来」（spec B2/B3、P1；亮键像素不是就绪判据）；
    // - 自动模式还要复核 AZ3 变体，不一致立即拒绝 —— 把「误判 × 上百次乱点」压到 ≤1 帧。
    // - 强制模式（F10/F11）**不复核、不拒绝**：用户已明确站在哪个台子前。但黄键台仍要等面板
    //   量得出来 —— 面板刚打开是 1% 进度、零亮键，单帧必然定位失败；等不到就用手上这一帧
    //   交给 flow 自己容错（强制语义下宁可让流程兜底，也不在这里把用户拦下来）。
    let mut flow = FlowContext {
        config: &snapshot, view: &view, session, frame, scene, az3_kind, request,
    };
    if scene == SceneKind::Console && az3_kind.is_known() && !desktop.prepare_az3(&mut flow) {
        return;
    }

    match flow.scene {
        SceneKind::Morse => {
            if flow.request.forced.is_none() && !morse_cells_ready(&flow.frame, flow.view) {
                for _ in 0..10 {
                    if cancel.load(Ordering::SeqCst) {
                        break;
                    }
                    desktop.clock().sleep_ms(50);
                    if let Ok(next_frame) =
                        D::ready_grab(&mut flow.session, full)
                    {
                        let ready = morse_cells_ready(&next_frame, flow.view);
                        flow.frame = next_frame;
                        if ready {
                            break;
                        }
                    }
                }
            }
            let input_boxes = desktop.morse_input_boxes(monitor);
            let mut seeded_source = SeededMorseSource {
                inner: &mut flow.session,
                seed: &flow.frame,
                regions: &flow.view.regions,
                prefer_seed: input_boxes.is_some(),
            };
            let outcome = run_morse_with(
                flow.config,
                flow.view,
                &mut seeded_source,
                desktop.input(),
                cancel,
                input_boxes,
            );
            let (used, limit) = desktop.budget(&flow.session);
            if budget_exhausted(used, limit) {
                let reason = budget_reason(used, limit);
                desktop.evidence("fail", &reason, &format!("已抓帧数: {used}（上限 {limit}）"),
                    Evidence { monitor: view.monitor, backend: snapshot.capture_backend });
                let _ = tx.send(UiEvent::Failed(reason));
                return;
            }
            publish_morse_outcome(flow.config, flow.view.monitor, &outcome, tx);
        }
        SceneKind::Fingerprint => desktop.fingerprint(flow),
        SceneKind::Console if az3_kind.is_known() => desktop.az3(flow),
        SceneKind::Console => desktop.console(flow),
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
    if morse_cells_ready(frame, view) {
        return (
            SceneKind::Morse,
            delta_scene::Az3Kind::None,
            true,
            "摩斯三格已就绪",
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
fn read_sequence(session: &mut impl delta_flow_morse::ScreenSource, led_region: PxRect) -> Result<String, String> {
    let frame = session.grab(led_region)?;
    delta_vision::read_glyph_string(&frame.luma()).map_err(|error| error.to_string())
}

/// 带首帧兜底的摩斯抓帧源：
/// 实机模式下（`prefer_seed = true`）若就绪帧已清晰解出摩斯格，直接裁切就绪帧零耗时返回，
/// 避免增加延迟后连续多次 `WgcSession::grab` 把双缓冲帧池掏空触发 `WgcNoFrame`；
/// 其他区域或 fresh 抓帧失败时也优先回退就绪帧，绝不因单次无新帧导致整轮卡死不按。
struct SeededMorseSource<'a, S> {
    inner: &'a mut S,
    seed: &'a Frame,
    regions: &'a [delta_config::PxRegion],
    prefer_seed: bool,
}

impl<S: delta_flow_morse::ScreenSource> delta_flow_morse::ScreenSource for SeededMorseSource<'_, S> {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
        let is_cell = self.regions.iter().any(|region| region.rect == rect);
        if self.prefer_seed && is_cell {
            if let Some(seeded) = crop_subframe(self.seed, rect) {
                if delta_flow_morse::decode_gray(&seeded.luma()).is_ok() {
                    return Ok(seeded);
                }
            }
        }
        match self.inner.grab(rect) {
            Ok(fresh) => {
                if is_cell && delta_flow_morse::decode_gray(&fresh.luma()).is_err() {
                    if let Some(fallback) = crop_subframe(self.seed, rect) {
                        if delta_flow_morse::decode_gray(&fallback.luma()).is_ok() {
                            return Ok(fallback);
                        }
                    }
                }
                Ok(fresh)
            }
            Err(err) => {
                if is_cell {
                    crop_subframe(self.seed, rect).ok_or(err)
                } else {
                    Err(err)
                }
            }
        }
    }
}

fn crop_subframe(frame: &Frame, rect: PxRect) -> Option<Frame> {
    if rect.x < 0
        || rect.y < 0
        || rect.w <= 0
        || rect.h <= 0
        || rect.right() > frame.w as i32
        || rect.bottom() > frame.h as i32
    {
        return None;
    }
    let mut data = Vec::with_capacity(Frame::buffer_len(rect.w as u32, rect.h as u32));
    for y in rect.y..rect.bottom() {
        let start = (y as usize * frame.stride as usize) + rect.x as usize * 4;
        data.extend_from_slice(&frame.data[start..start + rect.w as usize * 4]);
    }
    Frame::new(rect.w as u32, rect.h as u32, data, frame.captured_at).ok()
}

/// 生产和离线宿主共用 job 构造与公开摩斯流程，不改变玩法。
fn run_morse_with<S: delta_flow_morse::ScreenSource, I: delta_flow_morse::InputSink>(
    config: &Config,
    view: &PxView,
    source: &mut S,
    input: &mut I,
    cancel: &AtomicBool,
    input_boxes: Option<[PxRect; 3]>,
) -> delta_flow_morse::MorseOutcome {
    let mut rem = config.timings.morse_recognize_delay_ms;
    while rem > 0 {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let step = rem.min(50);
        input.sleep_ms(step);
        rem -= step;
    }
    let job = delta_flow_morse::MorseRun {
        view,
        timings: &config.timings,
        // 上游 morse_auto_click 的等价物：控制输入完成后是否自动点确认。
        auto_click: config.auto_confirm,
        // 输入横条 ROI：输错接力的观察窗口（未标定 ⇒ 零面积 ⇒ 不做错误态观察）。
        panel: delta_flow_morse::panel_roi(view),
        input_boxes,
    };
    delta_flow_morse::run(source, input, &job, cancel)
}

fn publish_morse_outcome(
    config: &Config,
    monitor: MonitorInfo,
    outcome: &delta_flow_morse::MorseOutcome,
    tx: &mpsc::Sender<UiEvent>,
) {
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
        // 输错接力失败也说清是哪一档失败：解码失败 vs 连续敲错，并落盘留证。
        let detail = outcome
            .errors
            .last()
            .cloned()
            .unwrap_or_else(|| format!("未识别 {}", outcome.password));
        let log_detail = format!(
            "密码: {} | 已敲: {:?} | 跳过: {:?} | 异常: {:?}",
            outcome.password, outcome.pressed, outcome.skipped, outcome.errors
        );
        save_failure_evidence(&detail, monitor, config.capture_backend, &log_detail);
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
///
/// `seed` = 就绪等待确认的那一帧：黄键台要把它**已经看到的黄键位置**带进流程
/// （按 F 的瞬间黄键正好亮着时，这一帧不能丢）。只有黄键台用得上，其余玩法忽略它。
fn run_az3(
    config: &Config,
    view: &PxView,
    kind: delta_scene::Az3Kind,
    monitor: MonitorInfo,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<UiEvent>,
    seed: Option<&Frame>,
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
    // 过程日志正文（多行）：黄键台的账本自己写，收尾时写满（`Done` / `Aborted` 都有）。
    // 其余玩法还没接，留空 —— 留证正文会退回「只有类型 + 事件数」的老格式。
    let mut trace = String::new();
    // 信号线台现在是「每帧自适应定位 + 只上下拖」的闭环，入口在 `line::run`；
    // 黄键台要带首帧观察种子（t7）；其余两类仍走统调度。
    let outcome = if kind == delta_scene::Az3Kind::SignalLine {
        delta_flow_az3::line::run(
            &mut screen,
            &mut pointer,
            &mut clock,
            delta_flow_az3::line::read_led_sequence,
            &view.az3,
            cancel,
        )
    } else if kind == delta_scene::Az3Kind::ModuleKeys {
        delta_flow_az3::keys::run_with_seed(
            &mut screen,
            &mut pointer,
            &mut clock,
            &view.az3,
            cancel,
            seed,
            &mut trace,
        )
    } else {
        delta_flow_az3::run(kind, &mut screen, &mut pointer, &mut clock, &view.az3, cancel)
    };
    match outcome {
        delta_flow_az3::Az3Outcome::Done {
            kind,
            steps,
            summary,
            confirmed,
            events,
        } => {
            // 完成分两档：`confirmed = true` 是「判据读到了目标状态」（黄键台 5/5 全勾）；
            // `false` 是退化档收尾（进度格读不出，只能按「在册待点键点完」推断）。
            // 后者**不许**报「已完成」：实测只见过 2 颗亮键时，点完 2 颗就报成功，
            // 而板子上还剩 3 颗（用户实机反馈）。
            let verdict = if confirmed {
                format!("{} 已完成", kind.label())
            } else {
                format!("{} 未确认完成（进度格读不出，请人工核对）", kind.label())
            };
            save_evidence(
                if confirmed { "ok" } else { "unverified" },
                &verdict,
                monitor,
                config.capture_backend,
                &az3_evidence_detail(kind.label(), events, &trace),
            );
            let _ = tx.send(UiEvent::Progress {
                done: steps,
                total: steps.max(1),
                note: summary.clone(),
            });
            let _ = tx.send(UiEvent::Result(delta_ui::az3_result(kind.label(), &summary)));
            if confirmed {
                let _ = tx.send(UiEvent::Succeeded(format!("{} · 已完成", kind.label())));
            } else {
                // 独立事件：界面显示「未确认完成」（黄色），不是「未识别」也不是「已完成」。
                let _ = tx.send(UiEvent::Unverified(format!(
                    "{} · 未确认完成（进度格读不出，请人工核对）",
                    kind.label()
                )));
            }
        }
        delta_flow_az3::Az3Outcome::Aborted { kind, reason, events } => {
            // 配额耗尽时 reason 必须是配额口径，不能被 flow 层「画面抓不到」之类的通用文案盖掉
            // （spec B10）。只在这里改判文案：`Done` 分支一行不动，否则「5/5 点完却报失败」。
            let used = session.frames_taken();
            let limit = session.budget();
            let reason = if budget_exhausted(used, limit) {
                budget_reason(used, limit)
            } else {
                reason
            };
            save_failure_evidence(
                &reason,
                monitor,
                config.capture_backend,
                &az3_evidence_detail(kind.label(), events, &trace),
            );
            let _ = tx.send(UiEvent::Failed(reason));
        }
    }
}

/// AZ3 留证正文：表头（类型 + 已发事件数）+ 流程给的过程日志。
///
/// 过程日志为空（还没接的诊断流程）时退回老格式，保证每次留证至少有类型与事件数 ——
/// 用户实测的抱怨正是「日志只有四行，看不出为什么漏点」，正文绝不能是空的。
fn az3_evidence_detail(label: &str, events: u64, trace: &str) -> String {
    let header = format!("AZ3类型: {label}, 已发输入事件: {events}");
    if trace.trim().is_empty() {
        header
    } else {
        format!("{header}\n{trace}")
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
///
/// **尺子由调用方传**：会话配额分两层（就绪会话 [`READY_FRAME_BUDGET`] = 200、
/// 其余会话默认 `FRAME_BUDGET` = 20），判定若硬编码默认值，改成 200 之后
/// 「已抓 20 帧」会被当成超限，三条单测也会静默失效（永远为假 ⇒ 超限检测形同不存在）。
fn budget_exhausted(used: u32, limit: u32) -> bool {
    used >= limit
}

/// 配额耗尽的**唯一**文案来源（`over_budget` 与 AZ3 收尾共用，防两处口径漂移）。
///
/// 必须含「抓帧配额用尽」与 `used/limit`（spec B10）：配额耗尽与「画面抓不到」
/// 是两件完全不同的事，退化成「未检测到界面」会把下一轮排查直接带偏。
fn budget_reason(used: u32, limit: u32) -> String {
    format!("抓帧配额用尽（已抓 {used}/{limit} 帧）")
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
    let used = session.frames_taken();
    let limit = session.budget();
    if !budget_exhausted(used, limit) {
        return false;
    }
    let reason = budget_reason(used, limit);
    let detail = format!("已抓帧数: {used}（上限 {limit}）");
    save_failure_evidence(&reason, monitor, backend, &detail);
    let _ = tx.send(UiEvent::Failed(reason));
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
    use delta_capture::FRAME_BUDGET;
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

    /// 忙时门：空门放行第一个任务，重入必须被拒，凭据一落地门就重新打开。
    #[test]
    fn busy_gate_admits_the_first_task_and_reopens_when_it_drops() {
        let busy = AtomicBool::new(false);

        assert!(!busy.swap(true, Ordering::SeqCst), "空门必须放行第一个任务");
        {
            let _gate = BusyGate(&busy);
            // 忙时重复按 F / 功能热键：热键泵读的就是这个值（读 true 就不入队）。
            assert!(busy.load(Ordering::SeqCst), "跑着的时候门必须是关的");
            assert!(
                busy.swap(true, Ordering::SeqCst),
                "门关着时第二个任务必须被拒"
            );
        }
        assert!(!busy.load(Ordering::SeqCst), "凭据落地必须把门重新打开");
    }

    /// panic 展开也必须放行：`run_once` 里任何一处炸了，都不能让所有热键永久失灵。
    #[test]
    fn busy_gate_reopens_after_a_panic() {
        let busy = AtomicBool::new(false);
        assert!(!busy.swap(true, Ordering::SeqCst));

        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _gate = BusyGate(&busy);
            panic!("识别线程里炸了");
        }));

        assert!(crashed.is_err(), "这个用例本来就要让它炸");
        assert!(!busy.load(Ordering::SeqCst), "panic 之后必须放行下一个任务");
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
    /// 通过共用 dispatch 的四 fake 计数证明零依赖调用，并保留原闸门文案断言。
    #[test]
    fn paused_master_makes_run_once_stop_before_touching_the_game_window() {
        use std::sync::mpsc::channel;

        let config = Arc::new(Mutex::new(Config::default()));
        config.lock().expect("锁").master_enabled = false;
        let cancel = AtomicBool::new(false);
        let (tx, rx) = channel();

        let mut desktop = OfflineDesktop::default();
        run_once_with(&config, TaskRequest {
            monitor: MonitorInfo::new(1280, 720).expect("合成几何"),
            tx: &tx, cancel: &cancel, forced: None, forced_az3: None,
        }, &mut desktop);
        assert_eq!(desktop.effects(), [0, 0, 0, 0]);

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

    /// F10/F11 必须各自派发**自己那个** AZ3 变体，而不是笼统的「操作台」。
    ///
    /// 这是「拆成两个强制键」的核心断言：两个槽位对调或都落到 `Console`，
    /// 用户按 F10 会跑横线台，而界面上看不出任何异常。
    #[test]
    fn keys_and_line_slots_force_their_own_az3_variant() {
        assert_eq!(
            cmd_for(delta_config::HotkeySlot::Keys),
            Cmd::ForceAz3(delta_scene::Az3Kind::ModuleKeys),
            "F10 必须是黄键台"
        );
        assert_eq!(
            cmd_for(delta_config::HotkeySlot::Line),
            Cmd::ForceAz3(delta_scene::Az3Kind::SignalLine),
            "F11 必须是横线台"
        );
        // 旧 F7 保持原样：它仍然只到「操作台」，由模板闸再判台子。
        assert_eq!(
            cmd_for(delta_config::HotkeySlot::Console),
            Cmd::Force(SceneKind::Console)
        );
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
        // 测的是**函数语义**（`used >= limit`），不是任何常量的取值：显式传尺子。
        assert!(!budget_exhausted(0, FRAME_BUDGET), "没抓过帧不算超限");
        assert!(!budget_exhausted(FRAME_BUDGET - 1, FRAME_BUDGET), "少一张还能抓");
        assert!(budget_exhausted(FRAME_BUDGET, FRAME_BUDGET), "正好用满即超限");
        assert!(budget_exhausted(FRAME_BUDGET + 1, FRAME_BUDGET), "越界也算超限");
    }

    /// 尺子必须来自会话本身：就绪会话配额 200，抓到 20 张远未用尽。
    ///
    /// 这条守的正是 t6 的回归：`budget_exhausted` 一旦退回硬编码 `FRAME_BUDGET`（20），
    /// 就绪阶段会在配额只用掉 10% 时被误杀成「抓帧配额用尽」——首开动画期间尤其致命。
    #[test]
    fn used_frames_below_the_ready_budget_are_not_exhausted() {
        assert!(
            !budget_exhausted(FRAME_BUDGET, READY_FRAME_BUDGET),
            "默认 20 张不是就绪会话的尺子：已抓 {} 帧 / 上限 {} 帧，远未用尽",
            FRAME_BUDGET,
            READY_FRAME_BUDGET
        );
    }

    /// 配额耗尽的文案必须点明「抓帧配额用尽」并带上 `used/limit`（spec A9/B10）。
    ///
    /// 退化成「未检测到界面 / 不识别」会把下一轮排查带去怀疑模板与分辨率。
    #[test]
    fn budget_reason_names_the_quota_and_the_numbers() {
        let reason = budget_reason(READY_FRAME_BUDGET, READY_FRAME_BUDGET);
        assert!(
            reason.contains("抓帧配额用尽"),
            "文案必须点明配额用尽，实际：{reason}"
        );
        assert!(
            reason.contains("300/300"),
            "文案必须给出 used/limit，实际：{reason}"
        );
    }

    /// 就绪两阶段（场景 + 面板）各自跑满预算，也不得把配额耗光（spec A8/B9）。
    #[test]
    fn ready_frame_budget_covers_both_stages() {
        let frames_per_stage = READY_BUDGET_MS / READY_POLL_MS;
        let required = 2 * frames_per_stage;
        assert!(
            u64::from(READY_FRAME_BUDGET) >= required,
            "就绪配额不够两阶段用：READY_FRAME_BUDGET = {READY_FRAME_BUDGET} 帧 < \
             2 × (READY_BUDGET_MS / READY_POLL_MS) = 2 × ({READY_BUDGET_MS} / {READY_POLL_MS}) \
             = {required} 帧"
        );
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
        // 「UI 模板确认解锁界面」这句原因只会在拿到模板 hint 时产生 ——
        // hint 现在由 `classify_poll_frame` 的两帧复核给出（`ReadyFrame.hint`）。
        // 拿到它就等于场景投票确实没跑（全黑帧也不可能靠结构分数认出任何界面）。
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
    fn only_force_mode_skips_scene_detection() {
        assert!(ReadyBasis::Forced.skips_detect(), "强制模式：用户按键就是命令");
        assert!(
            !ReadyBasis::Detect.skips_detect(),
            "自动模式必须照旧等结构判别 / 模板确认"
        );
    }

    /// 模板库（真实素材）。B7/B8 的前提是「这个界面一张模板都不像」。
    fn ui_templates_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images/ui")
    }

    /// 骨架帧：没有任何版面结构 ⇒ 结构判别与 AZ3 二级判别都不会命中。
    fn flat_frame() -> Frame {
        Frame::new(128, 96, vec![128u8; 128 * 96 * 4], std::time::Instant::now()).expect("合成帧")
    }

    /// 把 `images/ui/<name>.png` 按 [`ui_guard::ROI`] 铺回一帧整屏画面（等价于「屏幕上就是这张界面」）。
    ///
    /// ROI 与 [`ui_guard`] 内部同一块区域：贴进去之后模板分≈1.0，与实机命中同口径。
    fn frame_showing(name: &str) -> Frame {
        const W: u32 = 1280;
        const H: u32 = 720;
        let source = delta_templates::decode_png_gray(&ui_templates_dir().join(format!("{name}.png")))
            .expect("模板必须能解码")
            .resize_to(ui_guard::TEMPLATE_W, ui_guard::TEMPLATE_H)
            .expect("模板口径");
        let (left, top, right, bottom) = ui_guard::ROI;
        let (x0, y0) = ((W as f32 * left) as u32, (H as f32 * top) as u32);
        let (x1, y1) = ((W as f32 * right) as u32, (H as f32 * bottom) as u32);
        let mut data = vec![0u8; (W * H * 4) as usize];
        for y in y0..y1 {
            for x in x0..x1 {
                let tx = (x - x0) * ui_guard::TEMPLATE_W / (x1 - x0);
                let ty = (y - y0) * ui_guard::TEMPLATE_H / (y1 - y0);
                let value = source.as_slice()[(ty * ui_guard::TEMPLATE_W + tx) as usize];
                let index = ((y * W + x) * 4) as usize;
                data[index..index + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        Frame::new(W, H, data, std::time::Instant::now()).expect("合成帧")
    }

    /// 往 BGRA 缓冲里画一个实色矩形（越界自动裁掉）。
    fn paint(data: &mut [u8], w: u32, rect: PxRect, rgb: [u8; 3]) {
        for y in rect.y.max(0)..rect.bottom() {
            for x in rect.x.max(0)..rect.right() {
                let index = ((y as u32 * w + x as u32) * 4) as usize;
                data[index..index + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
            }
        }
    }

    /// 旧操作台画面（右上 LED `01-00-01-11` + 面板 4 条等距竖列 + 米黄旋钮）。
    ///
    /// 结构判别会判成 `Console`，而模板库里的三张解锁界面（morse/keys/line）都对不上 ——
    /// 正是「走廊/背包上结构像操作台、模板一张都不像」的合成版。
    fn console_frame() -> Frame {
        const W: u32 = 1280;
        const H: u32 = 720;
        let mut data = vec![0u8; (W * H * 4) as usize];
        let rect = |x: i32, y: i32, w: i32, h: i32| PxRect { x, y, w, h };
        let mut cursor = 870;
        for (group, digits) in ["01", "00", "01", "11"].iter().enumerate() {
            for digit in digits.chars() {
                let width = if digit == '1' { 6 } else { 12 };
                paint(&mut data, W, rect(cursor, 92, width, 40), [255, 255, 255]);
                cursor += width + 6;
            }
            if group < 3 {
                paint(&mut data, W, rect(cursor, 108, 14, 8), [255, 255, 255]);
                cursor += 14 + 8;
            }
        }
        for x in [160, 300, 440, 580] {
            paint(&mut data, W, rect(x, 200, 8, 300), [255, 255, 255]);
        }
        paint(&mut data, W, rect(725, 320, 40, 40), [235, 205, 80]);
        Frame::new(W, H, data, std::time::Instant::now()).expect("合成帧")
    }

    /// 与 [`console_frame`] 配套的校准视图：面板/LED/旋钮都按 1280×720 口径给齐。
    fn console_view() -> PxView {
        let rect = |x: i32, y: i32, w: i32, h: i32| PxRect { x, y, w, h };
        PxView {
            console: delta_config::PxConsole {
                led_region: rect(855, 85, 390, 55),
                panel_region: rect(85, 90, 595, 515),
                col_x: vec![160, 300, 440, 580],
                row_y: vec![200, 300, 400, 500],
                knob_center: PxPoint { x: 745, y: 340 },
            },
            ..led_view()
        }
    }

    #[test]
    fn a_confirmed_panel_is_not_revoked_by_a_following_corridor_frame() {
        // 实测：面板在时模板分 0.894~0.955，走廊/镜头移动时 p90 只有 0.206 —— 分离度足够。
        // 旧实现拿到 `Target` 后补抓第二帧复核，镜头一动第二帧掉回灰带就 `Downgrade` 撤回，
        // 于是 90s 已确认的黄键台在 92s 被推翻（用户看到的就是「按一次 F 没用」）。
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        let panel = frame_showing("keys");
        let corridor = flat_frame();
        assert_eq!(
            guard.verdict(&panel),
            ui_guard::UiVerdict::Target(delta_scene::Scene::Console),
            "前提：面板帧在模板口径下就是黄键台"
        );
        assert_eq!(
            guard.verdict(&corridor),
            ui_guard::UiVerdict::Reject(ui_guard::NOT_UNLOCK_SCREEN),
            "前提：走廊帧一张都不像（旧实现正是拿它把面板撤回的）"
        );

        // 复刻 `wait_scene_ready` 的收尾顺序：非 `NotYet` 当帧返回，后面的帧根本不参与。
        let frames = [panel, corridor];
        let mut decided = PollVerdict::NotYet;
        let mut stopped_at = 0usize;
        let mut pins = PinsStreak::default();
        for (index, frame) in frames.iter().enumerate() {
            stopped_at = index;
            decided = classify_poll_frame(&guard, frame, &led_view(), index as u64 * 40, &mut pins);
            if decided != PollVerdict::NotYet {
                break;
            }
        }
        assert_eq!(
            decided,
            PollVerdict::Ready(Some(delta_scene::Scene::Console)),
            "模板命中就必须一路 Ready 到动手：{decided:?}"
        );
        assert_eq!(
            stopped_at, 0,
            "面板确认当帧收尾，走廊帧一次都不许读（补抓第二帧就是撤回的来源）"
        );
    }

    #[test]
    fn fading_panel_at_300ms_is_not_rejected() {
        // B7：面板淡入要 0.4s。300ms 那一刻「一帧都不像」是正常现象，必须继续等。
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        assert!(!guard.is_empty(), "前提：模板库必须加载到素材");
        assert_eq!(
            guard.verdict(&flat_frame()),
            ui_guard::UiVerdict::Reject(ui_guard::NOT_UNLOCK_SCREEN),
            "前提：这一帧在模板口径下就是「一张都不像」"
        );
        let verdict = classify_poll_frame(
            &guard,
            &flat_frame(),
            &led_view(),
            300,
            &mut PinsStreak::default(),
        );
        assert_eq!(
            verdict,
            PollVerdict::NotYet,
            "淡入期不许拒绝，否则「第一次按 F 没用」的老毛病会回来"
        );
    }

    #[test]
    fn an_inventory_screen_is_quick_rejected_after_quick_reject_ms() {
        // B8：背包/仓库（模板全 < MAX_REJECT_NCC、结构未命中）⇒ 过了 QUICK_REJECT_MS 立刻拒绝，
        // 文案必须是「这不是解锁界面」，不是干等 1.2s 后的「未检测到游戏界面」。
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        let verdict = classify_poll_frame(
            &guard,
            &flat_frame(),
            &led_view(),
            QUICK_REJECT_MS,
            &mut PinsStreak::default(),
        );
        assert_eq!(
            verdict,
            PollVerdict::Reject(ui_guard::NOT_UNLOCK_SCREEN),
            "过了时限必须报准确的文案：{verdict:?}"
        );
    }

    /// 任务保险结构帧（1280×720 合成，100% 脱敏）：一条中灰长轨道 + 5 对「目标方块 + 游标」。
    ///
    /// `images/ui/` 里**没有** `pins.png` 模板（实测硬凑也凑不出来：40 帧正样本 `best` 中位
    /// 0.183），所以这一帧在模板口径下必然是 `Reject` —— 正是「Q5 模板闸把保险柜拒死」的复现。
    fn pins_screen_frame() -> Frame {
        const W: u32 = 1280;
        const H: u32 = 720;
        let mut data = vec![0u8; (W * H * 4) as usize];
        let rect = |x: i32, y: i32, w: i32, h: i32| PxRect { x, y, w, h };
        paint(&mut data, W, rect(300, 188, 750, 15), [67, 67, 67]);
        for center_x in [413, 527, 634, 739, 845] {
            paint(&mut data, W, rect(center_x - 5, 187, 10, 17), [255, 255, 255]);
            paint(&mut data, W, rect(center_x + 38, 182, 5, 27), [255, 255, 255]);
        }
        Frame::new(W, H, data, std::time::Instant::now()).expect("合成帧")
    }

    #[test]
    fn a_pins_frame_rejected_by_templates_is_rescued_by_the_az3_structure() {
        // Q5 根因 A：`images/ui/` 没有 `pins.png`/`timing.png`，这两个界面的模板结论恒为
        // `Reject` ⇒ 用户看到「按 F 完全没反应」。兜底只在「模板已判拒绝 **且** 等满
        // QUICK_REJECT_MS」之后放行，且只放行 AZ3 二级判别点名的保险柜两类。
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        let frame = pins_screen_frame();
        assert_eq!(
            guard.verdict(&frame),
            ui_guard::UiVerdict::Reject(ui_guard::NOT_UNLOCK_SCREEN),
            "前提：没有 pins 模板，这一帧在模板口径下就是「一张都不像」"
        );
        assert_eq!(
            delta_scene::az3_variant(&frame, &led_view()),
            delta_scene::Az3Kind::SafePins,
            "前提：AZ3 二级判别认得出这是任务保险"
        );

        // 未到时限：继续等（兜底不许把「拒绝时限」这道闸拆掉）。
        let mut pins = PinsStreak::default();
        assert_eq!(
            classify_poll_frame(
                &guard,
                &frame,
                &led_view(),
                QUICK_REJECT_MS - 1,
                &mut pins
            ),
            PollVerdict::NotYet,
            "拒绝时限之内一律不许下结论"
        );
        // 到时限之后：**连续 3 帧**都确认是保险柜才放行（时间闸，见 `PINS_STREAK_FRAMES`）；
        // hint=None（不冒充任何模板场景）。
        for expected in [
            PollVerdict::NotYet,
            PollVerdict::NotYet,
            PollVerdict::Ready(None),
        ] {
            assert_eq!(
                classify_poll_frame(&guard, &frame, &led_view(), QUICK_REJECT_MS, &mut pins),
                expected,
                "保险柜要连续 {PINS_STREAK_FRAMES} 帧稳定存在才许放行"
            );
        }
    }

    /// 脚本化抓帧：第 `i` 次抓帧返回 `frames[i]`（脚本用完后重复最后一帧），
    /// 并记住一共抓了几帧 —— `probe` 闭包自己回读不了计数。
    struct ScriptedFrames {
        frames: Vec<Frame>,
        served: usize,
    }

    impl ScriptedFrames {
        fn new(frames: Vec<Frame>) -> Self {
            Self { frames, served: 0 }
        }

        fn probe(&mut self) -> Result<Frame, String> {
            let index = self.served.min(self.frames.len() - 1);
            self.served += 1;
            Ok(self.frames[index].clone())
        }
    }

    /// `QUICK_REJECT_MS` 之前的帧数（这段窗里一律 `NotYet`，不会计入连击）。
    fn frames_before_deadline() -> usize {
        (QUICK_REJECT_MS / READY_POLL_MS) as usize + 1
    }

    #[test]
    fn a_pins_frame_needs_three_consecutive_frames_inside_the_poll_loop() {
        // Z4：时间闸必须长在轮询循环里（状态跨帧），且**第 3 帧**才放行。
        // 反例：单帧就放行 ⇒ 第 1 个保险柜帧就 Ready（`served` 会停在 deadline 那一帧）。
        //
        // 断言里的 `3` 是**用户拍板的策略值**（80ms 一帧 ⇒ 3 帧 ≈ 240ms），故意不引用
        // `PINS_STREAK_FRAMES`：引用它会让「把常数改成 1」这种退化测试自己跟着放宽。
        const SPEC_FRAMES: usize = 3;
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        let mut frames = vec![flat_frame(); frames_before_deadline()];
        for _ in 0..SPEC_FRAMES {
            frames.push(pins_screen_frame());
        }
        let mut script = ScriptedFrames::new(frames);
        let cancel = AtomicBool::new(false);
        let mut clock = MockClock { now: 0 };

        let outcome = poll_scene_ready(
            &mut clock,
            &cancel,
            &guard,
            &led_view(),
            READY_BUDGET_MS,
            || script.probe(),
        )
        .expect("抓帧不失败");

        assert!(
            matches!(outcome, ReadyOutcome::Ready { hint: None, .. }),
            "连续 {SPEC_FRAMES} 帧保险柜必须放行：{outcome:?}"
        );
        assert_eq!(
            script.served,
            frames_before_deadline() + SPEC_FRAMES,
            "必须等到第 {SPEC_FRAMES} 帧才动手（提前放行 = 零星误判帧过关）"
        );
    }

    #[test]
    fn a_non_pins_frame_clears_the_pins_streak() {
        // Z5：连击中间夹一帧「不是保险柜」⇒ 计数清零，下一次要从 1 重新数。
        // 反例：不清零 ⇒ 后面第 2 帧（总共第 5 帧）就 Ready ⇒ 红。
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        let pins_frame = pins_screen_frame();
        let corridor = flat_frame();
        let mut streak = PinsStreak::default();
        let classify = |frame: &Frame, streak: &mut PinsStreak| {
            classify_poll_frame(&guard, frame, &led_view(), QUICK_REJECT_MS, streak)
        };

        assert_eq!(classify(&pins_frame, &mut streak), PollVerdict::NotYet);
        assert_eq!(classify(&pins_frame, &mut streak), PollVerdict::NotYet);
        assert_eq!(
            classify(&corridor, &mut streak),
            PollVerdict::Reject(ui_guard::NOT_UNLOCK_SCREEN),
            "前提：夹在中间的这一帧是「这不是解锁界面」"
        );
        assert_eq!(
            classify(&pins_frame, &mut streak),
            PollVerdict::NotYet,
            "清零后只数到 1 帧，不许放行"
        );
        assert_eq!(classify(&pins_frame, &mut streak), PollVerdict::NotYet);
        assert_eq!(
            classify(&pins_frame, &mut streak),
            PollVerdict::Ready(None),
            "重新数满 {PINS_STREAK_FRAMES} 帧后必须放行"
        );
    }

    #[test]
    fn a_corridor_frame_is_still_rejected_after_the_fallback() {
        // 反例（兜底放太宽）：只要不是保险柜结构，就该照旧「这不是解锁界面」。
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        let frame = flat_frame();
        assert_eq!(
            delta_scene::az3_variant(&frame, &led_view()),
            delta_scene::Az3Kind::None,
            "前提：走廊帧上没有任何 AZ3 结构"
        );
        assert_eq!(
            classify_poll_frame(
                &guard,
                &frame,
                &led_view(),
                QUICK_REJECT_MS,
                &mut PinsStreak::default()
            ),
            PollVerdict::Reject(ui_guard::NOT_UNLOCK_SCREEN),
            "兜底只认保险柜，不许放宽成「什么都放行」"
        );
    }

    #[test]
    fn a_template_hit_keeps_its_scene_hint() {
        // 反例（判定顺序写反）：兜底只挂在 `Reject` 分支上，模板点名了界面就必须原样带着场景走，
        // 一旦被兜底改写成 `Ready(None)`，worker 就丢了「这是莫斯」这条路由信息。
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        let frame = frame_showing("morse");
        assert_eq!(
            guard.verdict(&frame),
            ui_guard::UiVerdict::Target(delta_scene::Scene::Morse),
            "前提：这一帧在模板口径下就是莫斯面板"
        );
        assert_eq!(
            classify_poll_frame(
                &guard,
                &frame,
                &led_view(),
                QUICK_REJECT_MS,
                &mut PinsStreak::default()
            ),
            PollVerdict::Ready(Some(delta_scene::Scene::Morse)),
            "模板结论必须原样透传"
        );
    }

    #[test]
    fn an_empty_template_library_never_rejects() {
        // 素材被误删/没打包 ⇒ 快速拒绝整段失效，退化为 v6.7 的 1.2s 路径（绝不放狠话）。
        let guard = ui_guard::UiGuard::default();
        let verdict = classify_poll_frame(
            &guard,
            &flat_frame(),
            &led_view(),
            QUICK_REJECT_MS * 3,
            &mut PinsStreak::default(),
        );
        assert_eq!(verdict, PollVerdict::NotYet, "没有模板就不许拒绝");
    }

    /// 零真实等待的时钟：`sleep_ms` 只推进虚拟时间（判定逻辑可零阻塞实测）。
    struct MockClock {
        now: u64,
    }

    impl Clock for MockClock {
        fn now_ms(&self) -> u64 {
            self.now
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.now += ms;
        }
    }

    #[test]
    fn ready_budget_must_exceed_quick_reject() {
        // 两个常量各自合理但相乘会变死代码（3000 的拒绝时限 > 2100 的预算 ⇒ Reject 分支永不触发）。
        // 经局部变量比较：直接写 `assert!(CONST > CONST)` 会被 clippy 的
        // `assertions_on_constants` 判为无意义断言，而本项目禁用 `#[allow]`。
        let budget = READY_BUDGET_MS;
        let reject_deadline = QUICK_REJECT_MS;
        assert!(
            budget > reject_deadline,
            "预算 {budget}ms 必须大于拒绝时限 {reject_deadline}ms，否则「这不是解锁界面」\
             这句准确文案永远发不出去，全被错归因成「未检测到游戏界面」"
        );
        assert!(
            budget - reject_deadline >= 400,
            "还要留出抓帧/编码余量（40ms 一帧 ⇒ 至少 400ms ≈ 10 帧）"
        );
    }

    #[test]
    fn an_unlockable_screen_is_rejected_not_exhausted() {
        // 全黑帧 + 真实模板库：模板判 Reject、结构判不命中。
        // 过了拒绝时限必须报**准确**文案，而不是等预算耗尽报「未检测到游戏界面」——
        // 后者会把「这不是解锁界面」这条证据整条抹掉（留证里就再也看不到它）。
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        assert!(!guard.is_empty(), "前提：模板库必须加载到素材");
        let frame = flat_frame();
        let cancel = AtomicBool::new(false);

        let mut clock = MockClock { now: 0 };
        let outcome = poll_scene_ready(
            &mut clock,
            &cancel,
            &guard,
            &led_view(),
            READY_BUDGET_MS,
            || Ok(frame.clone()),
        )
        .expect("抓帧不失败");
        assert!(
            matches!(
                outcome,
                ReadyOutcome::Rejected(reason) if reason == ui_guard::NOT_UNLOCK_SCREEN
            ),
            "必须是「这不是解锁界面」，实际 {outcome:?}"
        );
        assert!(
            clock.now >= QUICK_REJECT_MS,
            "拒绝必须发生在拒绝时限之后，实际 {}ms",
            clock.now
        );

        // 反向验证：预算压回旧值 2100 ⇒ 同一帧只能报「预算耗尽」，拒绝文案根本走不到。
        // 这条是 `READY_BUDGET_MS > QUICK_REJECT_MS` 的行为级证据（改回 2100 必红）。
        let mut clock = MockClock { now: 0 };
        let outcome = poll_scene_ready(
            &mut clock,
            &cancel,
            &guard,
            &led_view(),
            2100,
            || Ok(frame.clone()),
        )
        .expect("抓帧不失败");
        assert!(
            matches!(outcome, ReadyOutcome::Exhausted),
            "预算短于拒绝时限时必须退化成 Exhausted（2100 就是这个错），实际 {outcome:?}"
        );
    }

    /// 1080p 黄键台（合成）：金属台面 + 左侧近黑屏幕 + 16 颗**未点亮**的暗键。
    ///
    /// 几何按 720p 实测口径 × 1.5 放大，与 locate 层的 1080p 用例同一套相对结构；
    /// 故意不画任何亮键 —— 面板刚打开就是「1% 进度、零亮键」。
    fn hd_panel_frame() -> Frame {
        const W: u32 = 1920;
        const H: u32 = 1080;
        let mut data = vec![0u8; (W * H * 4) as usize];
        for pixel in data.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[150, 152, 152, 255]);
        }
        let scale = |value: i32| (value as f32 * 1.5).round() as i32;
        let hd = |x: i32, y: i32, w: i32, h: i32| PxRect {
            x: scale(x),
            y: scale(y),
            w: scale(w),
            h: scale(h),
        };
        // 左侧屏幕（近黑）：面板底色在 ⇒ locate 才会给 `Found` 而不是 `Absent`。
        paint(&mut data, W, hd(80, 100, 200, 180), [30, 32, 34]);
        // 4×4 暗键阵：全部是「未点亮的键面」颜色，一颗都不亮。
        for x in [322, 382, 442, 502] {
            for y in [92, 147, 202, 257] {
                paint(&mut data, W, hd(x, y, 44, 40), [36, 38, 40]);
            }
        }
        Frame::new(W, H, data, std::time::Instant::now()).expect("合成帧")
    }

    #[test]
    fn a_zero_lit_panel_is_ready_for_module_keys() {
        // D 项：就绪闸只等面板结构。这一帧面板齐全、亮键像素为 0（1% 进度），
        // 旧判据（面板 + 亮键 ≥ 阈值）必然 NotYet 到预算耗尽 —— 日志里报成「面板已消失」。
        let frame = hd_panel_frame();
        let cells = delta_scene::locate::locate_keys(&frame)
            .expect("合成面板必须定位成功")
            .cells;
        assert_eq!(
            delta_scene::locate::lit_key_pixels(&frame, &cells),
            0,
            "前提：这一帧一颗亮键都没有"
        );

        let located = keys_ready_probe(frame.clone());
        assert!(located.is_found(), "面板结构在、零亮键 ⇒ 就绪，实际 {located:?}");

        // 合成就绪等待：第一轮就该放行（不许把「没亮」当成「没打开」）。
        let cancel = AtomicBool::new(false);
        let mut clock = MockClock { now: 0 };
        let verdict = wait_ready(&mut clock, &cancel, READY_BUDGET_MS, 1, || {
            keys_ready_probe(frame.clone())
        });
        assert!(verdict.is_found(), "就绪等待必须第一轮放行：{verdict:?}");
        assert_eq!(clock.now, 0, "就绪不该消耗任何等待时间");
    }

    #[test]
    fn a_low_score_frame_never_acts_even_if_structure_says_console() {
        // 实测模板分：走廊 0.000、地图 0.048、办公室 0.001~0.126、背包 0.147、信号线 0.197，
        // 全部远低于 `MAX_REJECT_NCC` 0.30；而结构判别的分数用的是另一把尺子。
        let guard = ui_guard::UiGuard::load(&ui_templates_dir());
        let frame = console_frame();
        let view = console_view();
        assert!(
            delta_scene::detect(&frame, &view).is_actionable(),
            "前提：结构判别在这一帧上就是可动手的操作台"
        );
        assert!(
            matches!(guard.verdict(&frame), ui_guard::UiVerdict::Reject(_)),
            "前提：模板分落在拒绝区（一张都不像）"
        );

        for elapsed in (0..=QUICK_REJECT_MS + 500).step_by(250) {
            let verdict = classify_poll_frame(
                &guard,
                &frame,
                &view,
                elapsed,
                &mut PinsStreak::default(),
            );
            assert!(
                !matches!(verdict, PollVerdict::Ready(_)),
                "模板分极低时任何时刻都不许 Ready（elapsed={elapsed}ms）：{verdict:?}"
            );
            if elapsed < QUICK_REJECT_MS {
                assert_eq!(
                    verdict,
                    PollVerdict::NotYet,
                    "淡入/镜头移动窗内只能继续等（elapsed={elapsed}ms）"
                );
            } else {
                assert_eq!(
                    verdict,
                    PollVerdict::Reject(ui_guard::NOT_UNLOCK_SCREEN),
                    "过了时限必须拒绝（elapsed={elapsed}ms）"
                );
            }
        }
        // 对照：没有素材可判时才允许退回结构判别（fail-open，不许把工具变成永久失效）。
        assert_eq!(
            classify_poll_frame(
                &ui_guard::UiGuard::default(),
                &frame,
                &view,
                0,
                &mut PinsStreak::default()
            ),
            PollVerdict::Ready(None),
            "模板库为空时必须退化到结构判别"
        );
    }

    #[test]
    fn az3_evidence_detail_keeps_the_trace_verbatim_behind_the_header() {
        // 留证正文 = 表头（类型 + 事件数）+ 流程写的过程日志，**逐行原样保留**：
        // 用户实测抱怨「日志只有四行看不出为什么漏点」，正文不许再被裁剪成一句话。
        let trace = "判据档: 退化档（进度格未识别）\n进度格: 未识别（本次只定位到 2 格 / 共 5 格）\n点击数: 4（本任务共发出 8 个输入事件）\n点击格位: 第1行第1列、第2行第2列\n见过哪些格: 第1行第1列、第2行第2列（共 2 格）\n判死格位: 无\n耗时: 4 帧 / 150ms（首帧→收尾）";
        let body = az3_evidence_detail("黄键台", 8, trace);

        assert!(
            body.starts_with("AZ3类型: 黄键台, 已发输入事件: 8\n"),
            "表头必须带类型与事件数：{body}"
        );
        for field in ["判据档", "进度格", "点击数", "点击格位", "见过哪些格", "判死格位", "耗时"] {
            assert!(body.contains(field), "正文缺字段「{field}」：{body}");
        }
        assert!(body.ends_with(trace), "过程日志必须逐字保留：{body}");
        assert_eq!(body.lines().count(), 1 + trace.lines().count());

        // 还没接过程日志的流程：正文退回老格式，但绝不能是空正文。
        assert_eq!(
            az3_evidence_detail("大保险", 6, "   "),
            "AZ3类型: 大保险, 已发输入事件: 6"
        );
    }

    /// 只关掉 `off` 里列出的开关，其余全开。
    fn flows_with(off: &[Flow]) -> FlowFlags {
        let mut flags = [true; Flow::ALL.len()];
        for flow in off {
            flags[flow.index()] = false;
        }
        FlowFlags::from_flags(flags)
    }

    /// (场景, AZ3 变体, 对应开关) 的完整组合表。
    fn flow_cases() -> [(SceneKind, delta_scene::Az3Kind, Flow); 7] {
        use delta_scene::Az3Kind;
        [
            (SceneKind::Morse, Az3Kind::None, Flow::Morse),
            (SceneKind::Fingerprint, Az3Kind::None, Flow::Fingerprint),
            (SceneKind::Console, Az3Kind::SafeTiming, Flow::Az3Timing),
            (SceneKind::Console, Az3Kind::SafePins, Flow::Az3Pins),
            (SceneKind::Console, Az3Kind::ModuleKeys, Flow::Az3Keys),
            (SceneKind::Console, Az3Kind::SignalLine, Flow::Az3Line),
            (SceneKind::Console, Az3Kind::None, Flow::Console),
        ]
    }

    /// W-T2 / W-T3：每个判别结果都归**自己那个**开关管，关掉就拦下，文案点明功能名。
    ///
    /// 反例①：只闸 `console` 一个开关 → 黄键台那两行 `flow_of` 落不到自己的开关，红。
    /// 反例②：`flow_gate` 恒 `Ok`（只写不读）→ 所有 `expect_err` 立刻红。
    #[test]
    fn every_flow_has_its_own_switch_and_is_blocked_when_it_is_off() {
        for (scene, az3, flow) in flow_cases() {
            assert_eq!(
                flow_of(scene, az3),
                Some(flow),
                "{scene:?}/{az3:?} 该归「{}」管",
                flow.label()
            );
            assert!(
                flow_gate(FlowFlags::all_on(), flow).is_ok(),
                "全开时「{}」必须放行",
                flow.label()
            );
            let notice = flow_gate(flows_with(&[flow]), flow).expect_err("关掉必须拦住");
            assert!(
                notice.contains(flow.label()),
                "文案要点出是哪个功能：{notice}"
            );
            assert!(
                notice.contains("开关已关闭") && notice.contains("主页"),
                "文案要说出「开关已关闭」与去哪打开：{notice}"
            );
        }
        // 没认出界面时没有对应功能：不许把「未识别」说成「开关已关闭」。
        assert_eq!(flow_of(SceneKind::Unknown, delta_scene::Az3Kind::None), None);
    }

    /// 关掉一个开关**只**影响它自己（这就是「独立启用/禁用」的定义）。
    ///
    /// 反例：7 个开关共用一个 bool / 映射表里两行抄重 → 关「摩斯码」时别的功能一起失效。
    #[test]
    fn turning_one_switch_off_leaves_every_other_flow_runnable() {
        for off in Flow::ALL {
            let flows = flows_with(&[off]);
            for (scene, az3, _) in flow_cases() {
                let flow = flow_of(scene, az3).expect("已知界面必有对应开关");
                assert_eq!(
                    flow_gate(flows, flow).is_err(),
                    flow == off,
                    "关「{}」时对「{}」的放行判断错了",
                    off.label(),
                    flow.label()
                );
            }
        }
        // 7 个全关：每条流程都被拦住（不会有哪条悄悄漏跑）。
        let all_off = flows_with(&Flow::ALL);
        for (_, _, flow) in flow_cases() {
            assert!(flow_gate(all_off, flow).is_err(), "「{}」漏跑了", flow.label());
        }
    }

    /// W-T4：4 台 AZ3 各由自己的开关支配，`console` 只管 v5 旧旋钮。
    ///
    /// 反例：拿 `console` 一个开关管全部 AZ3 → 第一行就红（用户关掉大保险会连任务保险一起失效）。
    #[test]
    fn az3_variants_are_not_governed_by_the_legacy_console_switch() {
        // 旧操作台关着，任务保险照样能跑。
        let console_off = flows_with(&[Flow::Console]);
        assert!(
            flow_gate(console_off, Flow::Az3Pins).is_ok(),
            "旧操作台开关不该关掉任务保险"
        );
        // 任务保险关着，旧操作台照样能跑。
        let pins_off = flows_with(&[Flow::Az3Pins]);
        assert!(
            flow_gate(pins_off, Flow::Console).is_ok(),
            "任务保险开关不该关掉旧操作台"
        );
        assert!(flow_gate(pins_off, Flow::Az3Pins).is_err(), "自己的开关才算数");
    }

    /// W-T5：强制入口的开关在**抓帧前**就能确定；F7 的台子还要投票 ⇒ 交给判别后的闸门。
    ///
    /// 反例：`forced_flow` 对 `Force(Console)` 直接返回 `Flow::Console` → F7 在
    /// 「操作台关、任务保险开」时被误拦（明明该由投票结果说了算）。
    #[test]
    fn forced_hotkeys_map_to_the_switch_they_name() {
        use delta_scene::Az3Kind;
        assert_eq!(
            forced_flow(Some(SceneKind::Morse), None),
            Some(Flow::Morse),
            "F5 点名摩斯"
        );
        assert_eq!(
            forced_flow(Some(SceneKind::Fingerprint), None),
            Some(Flow::Fingerprint),
            "F6 点名指纹"
        );
        assert_eq!(
            forced_flow(Some(SceneKind::Console), Some(Az3Kind::ModuleKeys)),
            Some(Flow::Az3Keys),
            "F10 点名黄键台"
        );
        assert_eq!(
            forced_flow(Some(SceneKind::Console), Some(Az3Kind::SignalLine)),
            Some(Flow::Az3Line),
            "F11 点名信号线台"
        );
        assert_eq!(
            forced_flow(Some(SceneKind::Console), None),
            None,
            "F7 还要投票判台子，不能在抓帧前抢答"
        );
        assert_eq!(forced_flow(None, None), None, "自动路径不预先闸");
    }

    /// W-T5（真实路径）：关掉的功能，强制热键也必须在**激活窗口 / 建会话之前**退出。
    ///
    /// 同一生产 dispatch 注入四 fake，同时钉住事件文案与零依赖调用，不能只靠文案证明隔离。
    #[test]
    fn a_disabled_flow_stops_forced_hotkeys_before_any_side_effect() {
        use std::sync::mpsc::channel;

        let cases = [
            (Some(SceneKind::Morse), None, Flow::Morse),
            (Some(SceneKind::Fingerprint), None, Flow::Fingerprint),
            (
                Some(SceneKind::Console),
                Some(delta_scene::Az3Kind::ModuleKeys),
                Flow::Az3Keys,
            ),
            (
                Some(SceneKind::Console),
                Some(delta_scene::Az3Kind::SignalLine),
                Flow::Az3Line,
            ),
        ];
        for (forced, forced_az3, off) in cases {
            let config = Arc::new(Mutex::new(Config {
                flows: flows_with(&[off]),
                ..Config::default()
            }));
            let cancel = AtomicBool::new(false);
            let (tx, rx) = channel();

            let mut desktop = OfflineDesktop::default();
            run_once_with(&config, TaskRequest {
                monitor: MonitorInfo::new(1280, 720).expect("合成几何"),
                tx: &tx, cancel: &cancel, forced, forced_az3,
            }, &mut desktop);
            assert_eq!(desktop.effects(), [0, 0, 0, 0]);

            let events: Vec<UiEvent> = rx.try_iter().collect();
            assert_eq!(
                events.len(),
                2,
                "关掉「{}」后强制键必须零副作用退出：{events:?}",
                off.label()
            );
            assert!(matches!(events[0], UiEvent::Started), "先报开始");
            assert!(
                matches!(
                    &events[1],
                    UiEvent::Rejected(notice)
                        if notice == &format!("{}开关已关闭（在主页开关里打开后可继续）", off.label())
                ),
                "必须是该功能的开关闸门文案，实际：{:?}",
                events[1]
            );
            assert!(
                !cancel.load(Ordering::SeqCst),
                "闸门是只读放行判断，不该动别人的取消位"
            );
        }
    }

    /// 合成摩斯三区。
    ///
    /// **百分比是 0..1 的比例，不是 0..100 的百分数**（`PctRect` 的约定，见 `delta_geom`）。
    /// 旧写法把三个区写成 `x: 10.0 / 35.0 / 60.0`、`y: 20.0`、`w: 20.0`、`h: 12.0`，
    /// `PctRect::to_px` 直接 `PctOutOfRange` ⇒ `try_px_view()` 失败 ⇒ `run_once_with`
    /// 在「请先校准」那一行就收手，采集会话一次都没建（`session.opens == 0`）。
    /// 两条 dispatch 用例（`enabled_…` / `offscene_…`）红的根因就在这里，与业务代码无关。
    fn offline_task_config() -> Arc<Mutex<Config>> {
        Arc::new(Mutex::new(Config {
            monitor: MonitorInfo::new(1280, 720).expect("合成几何"),
            regions: [0.10_f32, 0.35, 0.60].into_iter().enumerate().map(|(index, x)| {
                delta_config::RegionEntry {
                    name: format!("synthetic-cell-{index}"),
                    rect: delta_geom::PctRect { x, y: 0.20, w: 0.20, h: 0.12 },
                }
            }).collect(),
            auto_confirm: false,
            f_key_hook_enabled: false,
            ocr_enabled: false,
            ..Config::default()
        }))
    }

    #[derive(Clone, Default)]
    struct OfflineClock(std::rc::Rc<std::cell::Cell<u64>>);

    impl Clock for OfflineClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.0.set(self.0.get() + ms);
        }
    }

    #[derive(Default)]
    struct OfflineWindow {
        calls: usize,
        guard_reads: usize,
    }

    #[derive(Default)]
    struct OfflineCaptureLog {
        opens: usize,
        requested: Vec<PxRect>,
    }

    struct OfflineCapture {
        log: std::rc::Rc<std::cell::RefCell<OfflineCaptureLog>>,
        frames: Vec<(PxRect, Frame)>,
        full: PxRect,
        blank: Frame,
        panel: PxRect,
        panel_frame: Frame,
        replay: Option<Frame>,
        used: u32,
        limit: u32,
    }

    impl delta_flow_morse::ScreenSource for OfflineCapture {
        fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
            assert!(self.used < self.limit, "脚本配额不足，不允许退回 Live");
            self.used += 1;
            self.log.borrow_mut().requested.push(rect);
            if let Some(frame) = &self.replay {
                return replay_crop(frame, rect);
            }
            if rect == self.full {
                return Ok(self.blank.clone());
            }
            if rect == self.panel {
                return Ok(self.panel_frame.clone());
            }
            self.frames.iter().find(|(region, _)| *region == rect)
                .map(|(_, frame)| frame.clone())
                .ok_or_else(|| format!("未登记脚本矩形: {rect:?}"))
        }
    }

    fn replay_png(path: &std::path::Path) -> Frame {
        let decoder = png::Decoder::new(std::io::BufReader::new(
            std::fs::File::open(path).expect("真实录像PNG必须存在"),
        ));
        let mut reader = decoder.read_info().expect("真实录像PNG头");
        let mut buffer = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buffer).expect("真实录像PNG像素");
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        let channels = match info.color_type {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            other => panic!("真实录像必须是RGB/RGBA，实际{other:?}"),
        };
        let mut bgra = Vec::with_capacity(Frame::buffer_len(info.width, info.height));
        for pixel in buffer[..info.buffer_size()].chunks_exact(channels) {
            bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        Frame::new(info.width, info.height, bgra, std::time::Instant::now())
            .expect("真实录像BGRA长度")
    }

    fn replay_crop(frame: &Frame, rect: PxRect) -> Result<Frame, String> {
        if rect.x < 0 || rect.y < 0 || rect.w <= 0 || rect.h <= 0
            || rect.right() > frame.w as i32 || rect.bottom() > frame.h as i32
        {
            return Err(format!("原帧{}x{}无法裁切{rect:?}", frame.w, frame.h));
        }
        let mut data = Vec::with_capacity(Frame::buffer_len(rect.w as u32, rect.h as u32));
        for y in rect.y..rect.bottom() {
            let start = (y as usize * frame.stride as usize) + rect.x as usize * 4;
            data.extend_from_slice(&frame.data[start..start + rect.w as usize * 4]);
        }
        Frame::new(rect.w as u32, rect.h as u32, data, frame.captured_at)
            .map_err(|error| error.to_string())
    }

    #[derive(Default)]
    struct OfflineInput {
        clock: OfflineClock,
        digits: Vec<u8>,
        digit_times: Vec<u64>,
        clicks: Vec<PxPoint>,
        waits: Vec<u64>,
    }

    impl delta_flow_morse::InputSink for OfflineInput {
        fn press_digit(&mut self, digit: u8) -> Result<(), String> {
            self.digits.push(digit);
            self.digit_times.push(self.clock.now_ms());
            Ok(())
        }

        fn click(&mut self, at: PxPoint) -> Result<(), String> {
            self.clicks.push(at);
            Ok(())
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.waits.push(ms);
            self.clock.sleep_ms(ms);
        }
    }

    #[derive(Default)]
    struct OfflineEvidence {
        notes: Vec<String>,
    }

    struct OfflineDesktop {
        window: OfflineWindow,
        session: std::rc::Rc<std::cell::RefCell<OfflineCaptureLog>>,
        input: OfflineInput,
        clock: OfflineClock,
        evidence: OfflineEvidence,
        replay: Option<Frame>,
    }

    impl Default for OfflineDesktop {
        fn default() -> Self {
            let clock = OfflineClock::default();
            Self {
                window: OfflineWindow::default(),
                session: std::rc::Rc::default(),
                input: OfflineInput { clock: clock.clone(), ..OfflineInput::default() },
                clock,
                evidence: OfflineEvidence::default(),
                replay: None,
            }
        }
    }

    impl OfflineDesktop {
        fn effects(&self) -> [usize; 4] {
            let session = self.session.borrow();
            [self.window.calls + self.window.guard_reads, session.opens + session.requested.len(),
                self.input.digits.len() + self.input.clicks.len() + self.input.waits.len(),
                self.evidence.notes.len()]
        }
    }

    fn offline_frame(rect: PxRect, dots: bool) -> Frame {
        let (w, h) = (rect.w as u32, rect.h as u32);
        let mut data = vec![0; (w * h * 4) as usize];
        if dots {
            for y in 0..h {
                for x in 0..w {
                    if (25..37).contains(&y)
                        && (0..5).any(|dot| (16 + dot * 40..28 + dot * 40).contains(&x))
                    {
                        let start = ((y * w + x) * 4) as usize;
                        data[start..start + 4].fill(255);
                    }
                }
            }
        }
        Frame::new(w, h, data, std::time::Instant::now()).expect("合成 BGRA 长度")
    }

    impl TaskDesktop for OfflineDesktop {
        type Session = OfflineCapture;
        type Input = OfflineInput;
        type Clock = OfflineClock;

        fn load_guard(&mut self) -> ui_guard::UiGuard {
            self.window.guard_reads += 1;
            if self.replay.is_some() {
                ui_guard::UiGuard::load(&ui_templates_dir())
            } else {
                ui_guard::UiGuard::default()
            }
        }

        fn focus(&mut self) -> Result<(), String> {
            self.window.calls += 1;
            Ok(())
        }

        fn open(&mut self, config: &Config, _tx: &mpsc::Sender<UiEvent>) -> Option<Self::Session> {
            self.session.borrow_mut().opens += 1;
            let view = config.try_px_view().expect("合成配置可解析");
            let full = full_rect(view.monitor);
            let panel = delta_flow_morse::panel_roi(&view);
            Some(OfflineCapture {
                log: self.session.clone(),
                frames: view.regions.iter().map(|region| {
                    (region.rect, offline_frame(region.rect, true))
                }).collect(),
                full,
                blank: offline_frame(full, false),
                panel,
                panel_frame: offline_frame(panel, false),
                replay: self.replay.clone(),
                used: 0,
                // 就绪轮询脚本容量，不冒称生产 FRAME_BUDGET/WGC 构造验证。
                limit: 128,
            })
        }

        fn ready_grab(session: &mut Self::Session, rect: PxRect) -> Result<Frame, String> {
            delta_flow_morse::ScreenSource::grab(session, rect)
        }

        fn clock(&mut self) -> &mut Self::Clock {
            &mut self.clock
        }

        fn input(&mut self) -> &mut Self::Input {
            &mut self.input
        }

        fn budget(&self, session: &Self::Session) -> (u32, u32) {
            (session.used, session.limit)
        }

        fn evidence(&mut self, tag: &str, reason: &str, detail: &str, _context: Evidence) {
            self.evidence.notes.push(format!("{tag}: {reason}; {detail}"));
        }

        fn prepare_az3(&mut self, _flow: &mut FlowContext<'_, Self::Session>) -> bool {
            panic!("本 S0 fake 未支持 AZ3 准备，不允许 fallback Live")
        }

        fn fingerprint(&mut self, _flow: FlowContext<'_, Self::Session>) {
            panic!("本 S0 fake 未支持指纹公开 run，不允许 fallback Live")
        }

        fn console(&mut self, _flow: FlowContext<'_, Self::Session>) {
            panic!("本 S0 fake 未支持旧操作台公开 run，不允许 fallback Live")
        }

        fn az3(&mut self, _flow: FlowContext<'_, Self::Session>) {
            panic!("本 S0 fake 未支持 AZ3 公开 run，不允许 fallback Live")
        }
    }

    #[test]
    fn enabled_dispatch_runs_morse_through_four_offline_dependencies() {
        let config = offline_task_config();
        let cancel = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        let mut desktop = OfflineDesktop::default();
        run_once_with(&config, TaskRequest {
            monitor: MonitorInfo::new(1280, 720).expect("合成几何"),
            tx: &tx, cancel: &cancel, forced: Some(SceneKind::Morse), forced_az3: None,
        }, &mut desktop);
        assert_eq!(desktop.window.calls, 1);
        let session = desktop.session.borrow();
        assert_eq!(session.opens, 1);
        assert_eq!(desktop.input.digits, vec![5, 5, 5]);
        assert_eq!(desktop.input.clicks, Vec::<PxPoint>::new());
        assert_eq!(desktop.evidence.notes, Vec::<String>::new());
        let view = config.lock().expect("合成配置锁").try_px_view().expect("合成视图");
        let panel = delta_flow_morse::panel_roi(&view);
        assert!(panel.w > 0 && panel.h > 0);
        let expected: Vec<_> = std::iter::once(full_rect(view.monitor))
            .chain(view.regions.iter().map(|region| region.rect))
            .chain([panel; 3]).collect();
        assert_eq!(session.requested, expected, "首帧、三格解码、三次真实非零 panel 观察");
        // 只记录按压时刻，不在这一层断言节奏：正常路径的 50ms 由 `flow-morse` 自己的用例钉住。
        assert_eq!(desktop.input.digit_times.len(), 3);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|event| matches!(event, UiEvent::Progress { done: 3, total: 3, .. })));
        assert!(events.iter().any(|event| matches!(event, UiEvent::Result(_))));
        assert!(events.iter().any(|event| matches!(event, UiEvent::Succeeded(reason) if reason == "已输入数字 555")));
        assert!(!events.iter().any(|event| matches!(event, UiEvent::Failed(_) | UiEvent::Rejected(_))));
    }

    fn replay_real_morse_dispatch(forced: Option<SceneKind>) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source = root.join(".scratch/astra-recovery/media-research-20261117-astra-a1/record20261117-020241_f0008640.png");
        let frame = replay_png(&source);
        assert_eq!((frame.w, frame.h), (1920, 1080));
        let monitor = MonitorInfo::new(1920, 1080).expect("原录像几何");
        let loaded = delta_config::load(&root.join("dist/delta-pwd-tool.config.json"))
            .expect("只读旧dist配置");
        assert!(loaded.config.confirm_clicks.is_empty(), "不填自定义确认点绕问题");
        assert!(loaded.config.auto_confirm);
        let config = Arc::new(Mutex::new(loaded.config.with_runtime_monitor(monitor)));
        let view = config.lock().expect("配置锁").try_px_view().expect("真实分辨率视图");
        let decoded: Vec<_> = view.regions.iter().map(|region| {
            let cell = replay_crop(&frame, region.rect).expect("原ROI裁切");
            (region.rect, delta_flow_morse::decode_gray(&cell.luma()))
        }).collect();
        eprintln!("[real-replay] source={} 原ROI={decoded:?} panel={:?} confirm={:?}",
            source.display(), delta_flow_morse::panel_roi(&view), view.confirm_clicks);
        let cancel = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        let mut desktop = OfflineDesktop { replay: Some(frame), ..OfflineDesktop::default() };
        run_once_with(&config, TaskRequest {
            monitor, tx: &tx, cancel: &cancel, forced, forced_az3: None,
        }, &mut desktop);
        let events: Vec<_> = rx.try_iter().collect();
        eprintln!("[real-replay] digits={:?} clicks={:?} requests={:?} events={events:?}",
            desktop.input.digits, desktop.input.clicks, desktop.session.borrow().requested);
        assert!(events.iter().any(|event| matches!(event,
            UiEvent::Progress { done: 3, total: 3, .. })),
            "原录像三个可见点划组必须全部解码；实际{events:?}");
        assert_eq!(desktop.input.digits.len(), 3, "三个可见数字必须实际输入");
        assert!(!desktop.input.clicks.is_empty(), "auto_confirm开且有当帧按钮必须实际确认");
    }

    #[test]
    fn real_morse_forced_dispatch_reads_all_three_recorded_cells() {
        replay_real_morse_dispatch(Some(SceneKind::Morse));
    }

    #[test]
    fn real_morse_auto_dispatch_reads_all_three_recorded_cells() {
        replay_real_morse_dispatch(None);
    }

    #[test]
    fn pre_cancel_dispatch_calls_no_dependency() {
        let config = offline_task_config();
        let cancel = AtomicBool::new(true);
        let (tx, rx) = mpsc::channel();
        let mut desktop = OfflineDesktop::default();
        run_once_with(&config, TaskRequest {
            monitor: MonitorInfo::new(1280, 720).expect("合成几何"),
            tx: &tx, cancel: &cancel, forced: Some(SceneKind::Morse), forced_az3: None,
        }, &mut desktop);
        assert_eq!(desktop.effects(), [0, 0, 0, 0]);
        assert!(rx.try_iter().any(|event| matches!(event, UiEvent::Failed(reason) if reason == "已取消")));
    }

    #[test]
    fn offscene_dispatch_never_calls_input() {
        let config = offline_task_config();
        let cancel = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        let mut desktop = OfflineDesktop::default();
        run_once_with(&config, TaskRequest {
            monitor: MonitorInfo::new(1280, 720).expect("合成几何"),
            tx: &tx, cancel: &cancel, forced: None, forced_az3: None,
        }, &mut desktop);
        assert_eq!(desktop.window.calls, 1);
        assert_eq!(desktop.window.guard_reads, 1);
        let session = desktop.session.borrow();
        assert_eq!(session.opens, 1);
        assert!(session.requested.len() > 1);
        assert!(session.requested.iter().all(|rect| *rect == full_rect(MonitorInfo { w: 1280, h: 720 })));
        assert_eq!(desktop.input.digits, Vec::<u8>::new());
        assert_eq!(desktop.input.clicks, Vec::<PxPoint>::new());
        assert_eq!(desktop.input.waits, Vec::<u64>::new());
        assert!(desktop.clock.now_ms() >= READY_BUDGET_MS);
        assert_eq!(desktop.evidence.notes.len(), 1);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|event| matches!(event, UiEvent::Rejected(reason) if reason == "未检测到游戏界面")));
        assert!(!events.iter().any(|event| matches!(event, UiEvent::Succeeded(_))));
    }

    /// 同一个功能开着时，强制键不会被闸门拦下（否则上面那条用例可能只是「什么都拦」）。
    ///
    /// 断言到「不是开关文案」为止：真跑下去会去打游戏窗口，测试环境里不该发生，
    /// 这里只要证明闸门放行了 —— 后面的失败来自窗口/采集，与开关无关。
    #[test]
    #[ignore = "旧 enabled 例可达真实窗口/采集；四 fake dispatch 接通前禁止执行"]
    fn live_an_enabled_flow_is_not_stopped_by_the_flow_gate() {
        use std::sync::mpsc::channel;

        let config = Arc::new(Mutex::new(Config {
            flows: flows_with(&[Flow::Morse]),
            ..Config::default()
        }));
        let cancel = AtomicBool::new(true); // 先置取消：真进去也会立刻在流程边界收手。
        let (tx, rx) = channel();

        run_once(
            &config,
            MonitorInfo::new(1280, 720).expect("合法显示器"),
            &tx,
            &cancel,
            None,
            Some(SceneKind::Fingerprint),
            None,
        );

        let events: Vec<UiEvent> = rx.try_iter().collect();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                UiEvent::Rejected(notice) if notice.contains("开关已关闭")
            )),
            "指纹开着，不该被「摩斯码已关闭」拦下：{events:?}"
        );
    }
}
