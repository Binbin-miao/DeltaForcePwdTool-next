//! eframe 应用：把 `delta-ui` 的画面、`delta-app::worker` 的后台结果和配置串起来。
//!
//! 线程纪律：`ui` 里只 `try_recv` 事件与绘制；识别一律在后台线程跑。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use delta_config::Config;
use delta_geom::{MonitorInfo, PxRect};
use delta_ui::{
    console_result, conflict_copy, Action, CaptureBackend, CaptureTestResult, KeyChord, Phase,
    Progress, SceneKind, ThemeId, Toggles, UiState,
};

use crate::keyhook::KeyHook;
use crate::platform;
use crate::worker::{Cmd, UiEvent, Worker};

/// 运行中重绘间隔（呼吸动画）。
const RUNNING_REPAINT: Duration = Duration::from_millis(16);
/// 非空闲态重绘间隔（让 200ms 高亮/闪烁按时结束）。
const IDLE_REPAINT: Duration = Duration::from_millis(100);
/// 测试截图缩略图上限（spec D5：≤160×90；留一行文案位后取 160×64）。
const THUMB_MAX: [u32; 2] = [160, 64];
/// 图钉切换失败提示（release 版 `windows_subsystem="windows"` 丢弃 stderr，只能靠界面提示）。
const NOTICE_ON_TOP_FAILED: &str = "置顶切换失败";
/// 图钉状态已生效但落盘失败的提示（此时三者已回滚成一致）。
const NOTICE_ON_TOP_UNSAVED: &str = "设置未保存";
/// 置顶回滚失败的提示：窗口确实停在新状态，只是没落盘 —— 不再谎称「已回滚」。
const NOTICE_ON_TOP_ROLLBACK_FAILED: &str = "置顶已变未保存";
/// 开关已生效但没存进配置文件的提示（重启后会回到旧状态）。
const NOTICE_SETTINGS_UNSAVED: &str = "设置未保存";
/// F 键钩子装不上的提示（已回滚成「关」，不谎称在监听）。
const NOTICE_KEY_HOOK_FAILED: &str = "F 键监听装不上";

/// 应用状态。
pub struct DeltaApp {
    state: UiState,
    settings_open: bool,
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    monitor: MonitorInfo,
    worker: Worker,
    phase_started: Instant,
    /// 正在飞的「测试截图」后台任务（`None` = 空闲）。
    capture_probe: Option<Receiver<Result<CaptureProbe, String>>>,
    /// F 键低级键盘钩子（`None` = 钩子已关 / 装不上）。
    key_hook: Option<KeyHook>,
    /// 上一帧灌进 egui 的主题（只在变化的帧调一次 `apply_visuals`）。
    applied_theme: Option<(ThemeId, Option<String>)>,
    /// 是否已把 egui 的唤醒把手交给后台（只在首帧交一次）。
    wake_handed: bool,
}

/// 「测试截图」从后台线程回传的东西（纹理上传只能留到 UI 线程做）。
struct CaptureProbe {
    width: u32,
    height: u32,
    elapsed_ms: u64,
    image: egui::ColorImage,
}

impl DeltaApp {
    /// 构造（配置已由 `main` 读好；DPI 感知也已在此之前完成）。
    pub fn new(monitor: MonitorInfo, config: Config, config_path: PathBuf) -> Self {
        let state = initial_state(&config, monitor);
        let config = Arc::new(Mutex::new(config));
        let worker = Worker::spawn(config.clone(), monitor);
        let mut app = Self::assemble(monitor, config, config_path, state, worker);
        // 配置说开着就当场装上（装不上只留痕，绝不因为钩子失败而不给启动）。
        app.sync_key_hook();
        app
    }

    fn assemble(
        monitor: MonitorInfo,
        config: Arc<Mutex<Config>>,
        config_path: PathBuf,
        state: UiState,
        worker: Worker,
    ) -> Self {
        Self {
            state,
            settings_open: false,
            config,
            config_path,
            monitor,
            worker,
            phase_started: Instant::now(),
            capture_probe: None,
            key_hook: None,
            applied_theme: None,
            wake_handed: false,
        }
    }

    /// 当前配置快照。
    fn snapshot(&self) -> Config {
        self.config
            .lock()
            .map(|config| config.clone())
            .unwrap_or_default()
    }

    /// 写回配置（原子写；失败不静默）。
    ///
    /// **不动 `monitor` 基线**：基线代表「最后一次校准时的分辨率」，是
    /// `resolution_changed()` 的比较锚点。若普通保存（免责声明、开关变更）也覆盖它，
    /// 换分辨率后的提醒就只会响一次，之后基线被悄悄改成新分辨率。
    fn persist(&mut self) -> Result<(), String> {
        let config = self.snapshot();
        delta_config::save(&self.config_path, &config).map_err(|error| error.to_string())
    }

    /// 收后台事件。
    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.worker.events.try_recv() {
            match event {
                UiEvent::Started => {
                    self.state.begin_run();
                    self.phase_started = Instant::now();
                }
                UiEvent::Scene { scene, sequence } => {
                    self.state.scene = scene;
                    self.state.sequence = sequence;
                }
                UiEvent::Progress { done, total, note } => {
                    self.state.progress = Progress::new(done, total, note);
                    self.refresh_console_result();
                }
                UiEvent::Result(content) => self.state.set_result(content),
                UiEvent::Succeeded(message) => self.finish(Phase::Success, message),
                // 「跑完了但没证据」是独立一态：不许落「未识别」，更不许落「已完成」。
                UiEvent::Unverified(message) => self.finish(Phase::Unverified, message),
                UiEvent::Failed(message) => self.finish(Phase::Unrecognized, message),
                UiEvent::Rejected(message) => self.finish(Phase::Unrecognized, message),
                UiEvent::TimedOut(message) => self.finish(Phase::TimedOut, message),
                UiEvent::HotkeyApplied { slot, name } => {
                    // 只有拿到成功回执才换显示 + 落盘（D2；成功路径只写一次盘）。
                    // 反过来，注册失败就什么都不写 —— 配置里永远不会留下一个没生效的键。
                    if let Ok(mut config) = self.config.lock() {
                        delta_config::HotkeySlot::write(slot, &mut config.hotkeys, name.clone());
                    }
                    self.state.set_hotkey_name(slot, name);
                    self.state.dismiss_hotkey_conflict();
                    if let Err(reason) = self.persist() {
                        eprintln!("[hotkey] 热键已生效但落盘失败：{reason}");
                    }
                }
                UiEvent::HotkeyConflict { wanted, kept } => {
                    let now = ctx.input(|input| input.time);
                    eprintln!("[hotkey] {wanted} 注册失败，回滚结果：{kept:?}");
                    // `state.hotkey` 保持旧值 —— 这正是「假保存」的反面。
                    self.state
                        .show_hotkey_conflict(conflict_copy(kept.as_deref()), now);
                }
                UiEvent::MasterToggled { enabled } => {
                    // 事件只当「该刷新了」的信号：它排队期间按钮可能又翻过一次，
                    // 真值一律回配置里读，免得标题栏、配置、磁盘三者各说一套。
                    let actual = match self.config.lock() {
                        Ok(config) => config.master_enabled,
                        Err(_) => enabled,
                    };
                    self.apply_master(actual);
                }
            }
            ctx.request_repaint();
        }
    }

    /// 收尾成某一态。
    fn finish(&mut self, phase: Phase, message: String) {
        self.state.finish(phase, message);
        // 操作台没有独立的结果事件：序列来自 `UiEvent::Scene`，已拖列数来自进度。
        self.refresh_console_result();
        self.phase_started = Instant::now();
    }

    /// 操作台结果区（spec D2b 第三场景）。
    fn refresh_console_result(&mut self) {
        if self.state.scene != SceneKind::Console {
            return;
        }
        let content = console_result(
            self.state.sequence.as_deref(),
            self.state.progress.done,
            self.state.progress.total,
        );
        self.state.set_result(content);
    }

    /// 提交一次捕获到的热键（spec D3 / D5）。
    ///
    /// 只把**规范写法**交给热键线程；这里既不换显示、也不落盘 —— 二者都等
    /// `HotkeyApplied` 回执。这样一次注册失败绝不会变成「配置已改但没生效」的假保存。
    ///
    /// 捕获结果直接用虚拟键码 + 修饰位生成 `Hotkey`，不再走「拼字符串→再解析」的往返。
    fn commit_hotkey(&mut self, chord: KeyChord, now: f64) {
        let slot = self.state.hotkey_slot;
        let Some(hotkey) = platform::hotkey_from_chord(chord) else {
            // 捕获表与解析表漂移才会走到这里：说出来，绝不设默认值糊过去。
            eprintln!("[hotkey] 捕获结果 {chord:?} 无法映射到虚拟键码");
            self.state.show_hotkey_conflict(conflict_copy(None), now);
            return;
        };
        if !self
            .worker
            .set_hotkey(slot, platform::format_hotkey(&hotkey))
        {
            // 热键线程已退出（例如注册接口建不起来）：当场显形，不留「无回执」的死角。
            eprintln!("[hotkey] 热键线程不可用，本次改键未生效");
            self.state.show_hotkey_conflict(conflict_copy(None), now);
        }
    }

    /// 切换截图后端：写配置 → 落盘 → 下一次抓帧立即生效（spec D1，无需重启）。
    fn set_capture_backend(&mut self, backend: CaptureBackend) {
        if let Ok(mut config) = self.config.lock() {
            config.capture_backend = backend;
        }
        self.state.capture_backend = backend;
        // 换了后端，上一次的测试结果就不再代表当前档位：清空，别显示过期证据。
        self.state.capture_test = None;
        self.state.capture_test_error = None;
        if let Err(reason) = self.persist() {
            // 落盘失败要看得见，但不动主状态机（热键/配置错误不该把界面切成「未识别」）。
            self.state
                .set_capture_test_error(format!("配置落盘失败：{reason}"));
        }
    }

    /// 点「抓一帧」：**后台线程**建临时会话抓一帧后立即释放（spec D3 / D5）。
    ///
    /// 绝不在 UI 线程做 D3D11 初始化 —— 那会卡住整帧；结果靠 `request_repaint` 回填。
    fn start_capture_test(&mut self, ctx: &egui::Context) {
        if self.capture_probe.is_some() {
            // 上一次还在飞：不叠线程、不重复抓帧（D5「不点不抓」）。
            return;
        }
        let backend = self.snapshot().capture_backend;
        let monitor = self.monitor;
        let (tx, rx) = mpsc::channel();
        let wake = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("delta-capture-test".to_owned())
            .spawn(move || {
                let _ = tx.send(capture_probe_once(backend, monitor));
                wake.request_repaint();
            });
        match spawned {
            Ok(_) => self.capture_probe = Some(rx),
            Err(error) => self
                .state
                .set_capture_test_error(format!("截图线程创建失败：{error}")),
        }
    }

    /// 收后台的测试截图结果；纹理上传在这里（UI 线程）完成。
    fn drain_capture_probe(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.capture_probe.as_ref() else {
            return;
        };
        let received = receiver.try_recv();
        if matches!(received, Err(TryRecvError::Empty)) {
            return;
        }
        self.capture_probe = None;
        match received {
            Ok(Ok(probe)) => {
                let thumbnail = ctx.load_texture(
                    "delta-capture-test",
                    probe.image,
                    egui::TextureOptions::LINEAR,
                );
                self.state.set_capture_test(CaptureTestResult {
                    width: probe.width,
                    height: probe.height,
                    elapsed_ms: probe.elapsed_ms,
                    thumbnail,
                });
            }
            Ok(Err(reason)) => self.state.set_capture_test_error(reason),
            Err(TryRecvError::Disconnected) => self
                .state
                .set_capture_test_error("测试截图线程意外退出".to_owned()),
            Err(TryRecvError::Empty) => {}
        }
        ctx.request_repaint();
    }

    /// 处理主界面动作。
    fn handle(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Start => {
                self.state.begin_run();
                let _ = self.worker.commands.send(Cmd::Trigger);
            }
            Action::Cancel => {
                self.worker.request_cancel();
                self.finish(Phase::Idle, "已取消".to_owned());
            }
            Action::Collapse | Action::Expand => {
                self.state.collapsed = action == Action::Collapse;
                let size = if self.state.collapsed {
                    delta_ui::theme::COLLAPSED_SIZE
                } else {
                    delta_ui::theme::WINDOW_SIZE
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(size[0], size[1])));
            }
            Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Action::OpenSettings => self.settings_open = true,
            Action::ToggleFlow(scene) => {
                self.state.toggles.toggle(scene);
                self.sync_flows();
            }
            // 全关时的修复动作（主按钮文案「请开启至少一项」的落点）。
            Action::EnableFlows => {
                self.state.toggles.enable_all();
                self.sync_flows();
            }
            Action::SetTheme(theme) => self.set_theme(theme),
            Action::SetAccent(accent) => self.set_accent(accent),
            Action::CommitKeywords(raw) => self.commit_keywords(&raw),
            Action::ToggleFingerprintAutoClick => self.toggle_fingerprint_auto_click(),
            Action::ToggleMorseAutoDownload => self.toggle_morse_auto_download(),
            Action::ToggleFKeyHook => self.toggle_f_key_hook(),
            Action::DismissNotice => self.state.dismiss_notice(),
            Action::ToggleAlwaysOnTop => self.toggle_always_on_top(),
            Action::ToggleMaster => self.toggle_master(),
        }
    }

    /// 切换窗口置顶（标题条图钉）。
    ///
    /// 顺序刻意是「先真正改窗口，再写配置」：`SetWindowPos` 失败就什么都不写，
    /// 配置里永远不会留下一个没生效的置顶状态 —— 与热键改键同一条纪律。
    ///
    /// **失败必须显形**：release 版没有 stderr，所以两种情况都走 `state.notice`；
    /// 落盘失败时把窗口、内存配置、界面一并退回旧值，保证「窗口 = 界面 = 磁盘」三者一致，
    /// 不留「窗口已置顶、磁盘说没置顶」的假保存。
    fn toggle_always_on_top(&mut self) {
        let previous = self.state.always_on_top;
        let wanted = !previous;
        if platform::apply_always_on_top(wanted).is_err() {
            // 窗口级失败不动状态机：不该把界面切成「未识别」。
            self.state.notice = Some(NOTICE_ON_TOP_FAILED.to_owned());
            return;
        }
        if let Ok(mut config) = self.config.lock() {
            config.always_on_top = wanted;
        }
        self.state.always_on_top = wanted;
        if self.persist().is_ok() {
            return;
        }
        // 落盘失败：回滚窗口与内存，别让用户看到一个存不下来的开关。
        if platform::apply_always_on_top(previous).is_err() {
            // 回滚也失败：窗口确实停在新状态，就别再谎称它回去了 —— 承认现状、说清没存下，
            // 也别让内存配置与窗口打架（下一次切换要从真实值算起）。
            if let Ok(mut config) = self.config.lock() {
                config.always_on_top = wanted;
            }
            self.state.always_on_top = wanted;
            self.state.notice = Some(NOTICE_ON_TOP_ROLLBACK_FAILED.to_owned());
            return;
        }
        if let Ok(mut config) = self.config.lock() {
            config.always_on_top = previous;
        }
        self.state.always_on_top = previous;
        self.state.notice = Some(NOTICE_ON_TOP_UNSAVED.to_owned());
    }

    /// 指纹自动点击开关（spec D2 第 4 行）：写配置 → 更新界面 → 落盘。
    ///
    /// 下一轮识别在 `run_once` 开头重新取配置快照，因此**不必重启**就生效。
    fn toggle_fingerprint_auto_click(&mut self) {
        let wanted = !self.state.fingerprint_auto_click;
        if let Ok(mut config) = self.config.lock() {
            config.fingerprint_auto_click = wanted;
        }
        self.state.set_fingerprint_auto_click(wanted);
        self.persist_settings();
    }

    /// 摩斯自动下载开关（spec D2 第 5 行 → `config.auto_confirm`）。
    fn toggle_morse_auto_download(&mut self) {
        let wanted = !self.state.morse_auto_download;
        if let Ok(mut config) = self.config.lock() {
            config.auto_confirm = wanted;
        }
        self.state.set_morse_auto_download(wanted);
        self.persist_settings();
    }

    /// 识别总开关：关掉后任何热键（F/F5/F6/F7/F8）都不再触发识别，连截图都不抓。
    ///
    /// 与 F 键钩子开关的分工：那个只管 F 键这一路入口，这个管全部入口，
    /// 而且**不卸载钩子**（钩子还要负责在重新打开后立刻可用）。
    ///
    /// 取值一律回配置里翻（而不是翻界面里的副本）：同一个开关还有一条热键入口，
    /// 它随时可能在后台翻过一次，界面里的副本未必是最新的。
    fn toggle_master(&mut self) {
        let wanted = match self.config.lock() {
            Ok(mut config) => {
                config.master_enabled = !config.master_enabled;
                config.master_enabled
            }
            Err(_) => return,
        };
        self.apply_master(wanted);
    }

    /// 把「识别总开关」的新状态摊到界面 + 磁盘上（两条入口共用的收尾）。
    fn apply_master(&mut self, enabled: bool) {
        self.state.master_enabled = enabled;
        self.state.notice = Some(if enabled {
            "识别已开启".to_owned()
        } else {
            "识别已暂停：热键不再触发".to_owned()
        });
        self.persist_settings();
    }

    /// F 键监听开关（spec D3 边界 3）：先真装/真卸，再落盘。
    ///
    /// 想开却装不上（被安全软件拦、会话不允许）→ 回滚成关并显形，
    /// 配置里绝不留一个「写着开、其实没监听」的假状态。
    fn toggle_f_key_hook(&mut self) {
        let previous = self.state.f_key_hook;
        let wanted = !previous;
        if let Ok(mut config) = self.config.lock() {
            config.f_key_hook_enabled = wanted;
        }
        self.sync_key_hook();
        if wanted && self.key_hook.is_none() {
            if let Ok(mut config) = self.config.lock() {
                config.f_key_hook_enabled = previous;
            }
            self.state.set_f_key_hook(previous);
            self.state.notice = Some(NOTICE_KEY_HOOK_FAILED.to_owned());
            return;
        }
        self.state.set_f_key_hook(wanted);
        self.persist_settings();
    }

    /// 换主题（spec D7）：写状态 → 落盘；下一帧自绘控件与原生兜底同时换色，**不重启**。
    fn set_theme(&mut self, theme: ThemeId) {
        if self.state.theme_id == theme {
            return;
        }
        self.state.theme_id = theme;
        if let Ok(mut config) = self.config.lock() {
            config.theme_id = theme;
        }
        self.persist_settings();
    }

    /// 换主色（`None` = 恢复该主题自带的 accent）。
    fn set_accent(&mut self, accent: Option<String>) {
        self.state.accent_override = accent.clone();
        if let Ok(mut config) = self.config.lock() {
            config.accent_override = accent;
        }
        self.persist_settings();
    }

    /// 提交游戏窗口关键词：空输入**不覆盖**现有关键词
    /// （空列表 = 永不响应 F，等于静默把监听关掉）。
    fn commit_keywords(&mut self, raw: &str) {
        let keywords = UiState::parse_keywords(raw);
        if keywords.is_empty() {
            self.state.keywords_buffer = self.state.game_title_keywords.join(", ");
            return;
        }
        self.state.set_game_title_keywords(keywords.clone());
        if let Ok(mut config) = self.config.lock() {
            config.game_title_keywords = keywords;
        }
        // 钩子侧热更新关键词（不必重装钩子）。
        self.sync_key_hook();
        self.persist_settings();
    }

    /// 让钩子跟上配置：开 → 装上（关键词热更新），关 → 卸载。
    ///
    /// ⚠️ 关闭时**只是卸载**。**绝不**回退成 `RegisterHotKey("F")` —— 那是全局独占，
    /// 会把游戏的 F 交互键吞掉，正是这个钩子存在的理由（D3 边界 4）。
    fn sync_key_hook(&mut self) {
        let (wanted, keywords) = {
            let config = self.snapshot();
            (config.f_key_hook_enabled, config.game_title_keywords)
        };
        match (wanted, self.key_hook.is_some()) {
            (true, false) => self.install_key_hook(keywords),
            (true, true) => {
                if let Some(hook) = self.key_hook.as_ref() {
                    hook.set_keywords(keywords);
                }
            }
            // 句柄一丢就 `Drop` → 卸载钩子；F 键从此与本程序无关。
            (false, true) => self.key_hook = None,
            (false, false) => {}
        }
    }

    /// 装钩子 + 起一个只做转发的线程：钩子事件 → 执行器（与热键**同一个** `Cmd::Trigger`）。
    ///
    /// 为什么不让 UI 线程 `try_recv`：egui 空闲时可能很久不跑一帧，
    /// F 键要等到「下一次鼠标移动」才生效是不可接受的延迟。
    fn install_key_hook(&mut self, keywords: Vec<String>) {
        let (sink, events) = mpsc::sync_channel::<()>(8);
        let commands = self.worker.commands.clone();
        let cancel = self.worker.cancel_flag();
        let busy = self.worker.busy_flag();
        let polling = self.worker.polling_flag();
        let hook = match KeyHook::install(keywords, sink) {
            Ok(hook) => hook,
            // 装不上也必须退回「关」：只打日志会让界面继续显示「开」而实际没在监听，
            // 用户按 F 毫无反应却以为功能正常（与热键改键同一条「不留假状态」纪律）。
            Err(reason) => {
                self.abandon_key_hook(&format!("安装失败：{reason}"));
                return;
            }
        };
        let forwarder = std::thread::Builder::new()
            .name("delta-keyhook-fwd".to_owned())
            .spawn(move || {
                let mut last_at = Instant::now()
                    .checked_sub(Duration::from_secs(10))
                    .unwrap_or_else(Instant::now);
                // 发送端随钩子卸载一起消失 → `recv` 出错 → 本线程自然退出。
                // 仅在「空闲」或「仍在等待场景轮询且距上次触发 ≥400ms」时才投递新识别；
                // 一旦已识别成功并进入输入/点击/拖动阶段（busy && !polling），绝不打断正在执行的输入！
                while events.recv().is_ok() {
                    if busy.load(std::sync::atomic::Ordering::SeqCst) {
                        if polling.load(std::sync::atomic::Ordering::SeqCst)
                            && last_at.elapsed() >= Duration::from_millis(400)
                        {
                            last_at = Instant::now();
                            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                            if commands.send(Cmd::Trigger).is_err() {
                                break;
                            }
                        }
                    } else if last_at.elapsed() >= Duration::from_millis(250) {
                        last_at = Instant::now();
                        if commands.send(Cmd::Trigger).is_err() {
                            break;
                        }
                    }
                }
            });
        match forwarder {
            Ok(_) => self.key_hook = Some(hook),
            Err(error) => {
                // 钩子装上了但没人转发 → 「界面说开、按 F 毫无反应」的假状态。
                // 当场卸掉钩子（`Drop` → Unhook + 清槽位），并把配置/界面一并退回「关」。
                drop(hook);
                self.abandon_key_hook(&format!("事件转发线程启动失败：{error}"));
            }
        }
    }

    /// 钩子没装成时把三处状态一并退回「关」：句柄、内存配置、界面，并给出可见提示。
    ///
    /// 不落盘：磁盘上仍留着用户的「开」，下次启动会重新尝试安装——
    /// 若环境一直不允许，每次启动都会显形一次「F 键监听装不上」，而不是悄悄失效。
    fn abandon_key_hook(&mut self, reason: &str) {
        eprintln!("[keyhook] {reason}");
        self.key_hook = None;
        if let Ok(mut config) = self.config.lock() {
            config.f_key_hook_enabled = false;
        }
        self.state.set_f_key_hook(false);
        self.state.notice = Some(NOTICE_KEY_HOOK_FAILED.to_owned());
    }

    /// 主页 7 个功能开关 → 配置 → 磁盘（**7 个字段全写**，不做部分保存）。
    ///
    /// 两处调用：
    /// 1. 本文件的开关动作（`ToggleFlow` / `EnableFlows`）；
    /// 2. 每帧画完主视图之后 —— `delta_ui` 的网格开关**直接改 `UiState`**
    ///    （视图侧没有 `FlowId` 粒度的 `Action`），app 只能在这一帧结束后把差异收进配置。
    ///
    /// 只有真变过才落盘：读路径不会给定型帧平白写一次配置文件。
    fn sync_flows(&mut self) {
        let wanted = delta_config::FlowFlags::from_flags(self.state.toggles.flags());
        let changed = match self.config.lock() {
            Ok(mut config) => {
                let changed = config.flows != wanted;
                config.flows = wanted;
                changed
            }
            // 锁中毒 = 配置内容已不可信：不写盘，也不造出界面与磁盘之间的假一致。
            Err(_) => return,
        };
        if changed {
            self.persist_settings();
        }
    }

    /// 开关落盘：失败必须显形（内存里已生效，但重启后会丢）。
    fn persist_settings(&mut self) {
        if let Err(reason) = self.persist() {
            eprintln!("[settings] 开关已生效但落盘失败：{reason}");
            self.state.notice = Some(NOTICE_SETTINGS_UNSAVED.to_owned());
        }
    }

    /// 非空闲态维持重绘（呼吸 / 闪烁 / 200ms 高亮）。
    fn schedule_repaint(&self, ctx: &egui::Context) {
        if self.state.phase == Phase::Running {
            ctx.request_repaint_after(RUNNING_REPAINT);
        } else if self.state.phase != Phase::Idle {
            ctx.request_repaint_after(IDLE_REPAINT);
        }
    }
}

impl eframe::App for DeltaApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 把「叫醒界面」的把手交给后台一次性：热键在窗口没有焦点时按下，
        // 没有它，后台改完状态（尤其总开关）界面要等下一次碰巧重绘才显示。
        if !self.wake_handed {
            self.worker.wake_with(ctx.clone());
            platform::exclude_app_from_capture();
            self.wake_handed = true;
        }
        self.state.phase_elapsed = self.phase_started.elapsed().as_secs_f64();
        self.drain_events(&ctx);

        // 主题：自绘控件以 `Palette` 为准；原生控件只在主题/主色**变化的帧**灌一次。
        let pal = self.state.palette();
        let theme_key = (self.state.theme_id, self.state.accent_override.clone());
        if self.applied_theme.as_ref() != Some(&theme_key) {
            delta_ui::theme::apply_visuals(&ctx, &pal);
            self.applied_theme = Some(theme_key);
        }

        if self.settings_open {
            self.drain_capture_probe(&ctx);
            let now = ctx.input(|input| input.time);
            // 冲突提示到期即消失：每帧结算一次，视图只拿到结果。
            let conflict = self.state.hotkey_conflict_text(now).map(str::to_owned);
            let hotkey_text = self.state.hotkey.clone();
            let hotkey_morse = self.state.hotkey_morse.clone();
            let hotkey_fingerprint = self.state.hotkey_fingerprint.clone();
            let hotkey_console = self.state.hotkey_console.clone();
            let hotkey_keys = self.state.hotkey_keys.clone();
            let hotkey_line = self.state.hotkey_line.clone();
            let hotkey_master = self.state.hotkey_master.clone();
            let hotkey_editing = self.state.hotkey_editing;
            let capture_locked = !self.state.capture_switchable();
            let capture_test = self.state.capture_test.clone();
            let capture_test_error = self.state.capture_test_error.clone();
            let (theme_id, accent, f_key_hook) = (
                self.state.theme_id,
                self.state.accent_override.clone(),
                self.state.f_key_hook,
            );
            let snapshot_cfg = self.snapshot();
            let timings = std::array::from_fn(|index| {
                delta_config::TimingField::ALL[index].read(&snapshot_cfg.timings)
            });
            // 关键词与延时输入框的文本归 UI 状态所有：先取出、交给面板编辑、再放回。
            let mut keywords = std::mem::take(&mut self.state.keywords_buffer);
            let mut timing_buffers = std::mem::take(&mut self.state.timing_buffers);
            let action = delta_ui::view::settings_panel(
                ui,
                delta_ui::view::SettingsView {
                    master_enabled: self.state.master_enabled,
                    hotkey: &hotkey_text,
                    hotkey_master: &hotkey_master,
                    hotkey_morse: &hotkey_morse,
                    hotkey_fingerprint: &hotkey_fingerprint,
                    hotkey_console: &hotkey_console,
                    hotkey_keys: &hotkey_keys,
                    hotkey_line: &hotkey_line,
                    hotkey_slot: self.state.hotkey_slot,
                    hotkey_editing,
                    hotkey_conflict: conflict.as_deref(),
                    ocr_enabled: snapshot_cfg.ocr_enabled,
                    config_path: &self.config_path.display().to_string(),
                    capture_backend: self.state.capture_backend,
                    capture_locked,
                    capture_test: capture_test.as_ref(),
                    capture_test_error: capture_test_error.as_deref(),
                    theme_id,
                    accent_override: accent.as_deref(),
                    f_key_hook,
                    timings,
                    timing_buffers: &mut timing_buffers,
                    keywords: &mut keywords,
                },
                &pal,
            );
            self.state.keywords_buffer = keywords;
            self.state.timing_buffers = timing_buffers;
            match action {
                Some(delta_ui::view::SettingsAction::Probe) => {
                    self.settings_open = false;
                    let _ = self.worker.commands.send(Cmd::Probe);
                }
                Some(delta_ui::view::SettingsAction::ToggleOcr) => {
                    if let Ok(mut config) = self.config.lock() {
                        config.ocr_enabled = !config.ocr_enabled;
                    }
                    let _ = self.persist();
                }
                Some(delta_ui::view::SettingsAction::EditHotkey(slot)) => {
                    self.state.hotkey_editing = true;
                    self.state.hotkey_slot = slot;
                    self.state.dismiss_hotkey_conflict();
                }
                Some(delta_ui::view::SettingsAction::CancelHotkey) => {
                    self.state.hotkey_editing = false;
                }
                Some(delta_ui::view::SettingsAction::CommitHotkey(chord)) => {
                    self.state.hotkey_editing = false;
                    self.commit_hotkey(chord, now);
                }
                Some(delta_ui::view::SettingsAction::SetCaptureBackend(backend)) => {
                    self.set_capture_backend(backend);
                }
                Some(delta_ui::view::SettingsAction::TestCapture) => self.start_capture_test(&ctx),
                Some(delta_ui::view::SettingsAction::SetTheme(theme)) => self.set_theme(theme),
                Some(delta_ui::view::SettingsAction::SetAccent(accent)) => self.set_accent(accent),
                Some(delta_ui::view::SettingsAction::ToggleMaster) => self.toggle_master(),
                Some(delta_ui::view::SettingsAction::ToggleFKeyHook) => self.toggle_f_key_hook(),
                Some(delta_ui::view::SettingsAction::SetTiming(field, value)) => {
                    if let Ok(mut config) = self.config.lock() {
                        field.write(&mut config.timings, value);
                        for (index, item) in delta_config::TimingField::ALL.into_iter().enumerate() {
                            if item == field {
                                self.state.timing_buffers[index] =
                                    item.read(&config.timings).to_string();
                            }
                        }
                    }
                    self.persist_settings();
                }
                Some(delta_ui::view::SettingsAction::CommitKeywords(raw)) => {
                    self.commit_keywords(&raw);
                }
                Some(delta_ui::view::SettingsAction::Close) => self.settings_open = false,
                None => {}
            }
            return;
        }

        if let Some(action) = delta_ui::main_window(ui, &mut self.state, &pal) {
            self.handle(action, &ctx);
        }
        // 网格里的 7 个功能开关由视图**直接改** `UiState`（不经过 `Action`），
        // 所以这一帧结束后把差异收进配置并落盘 —— 少了这一句，开关只活在内存里，
        // 重启后配置里还是旧的，界面与执行闸门各说一套。
        self.sync_flows();
        self.schedule_repaint(&ctx);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 后台线程可能在没有输入时完成，靠这里把窗口叫醒。
        if self.state.phase == Phase::Running {
            ctx.request_repaint();
        }
    }
}

/// 一次性抓帧：独立临时会话（**不占任务配额**），抓一帧后立即 `drop`（spec D3 / D5）。
fn capture_probe_once(
    backend: CaptureBackend,
    monitor: MonitorInfo,
) -> Result<CaptureProbe, String> {
    let started = Instant::now();
    let rect = PxRect {
        x: 0,
        y: 0,
        w: monitor.w,
        h: monitor.h,
    };
    let mut session =
        delta_capture::CaptureSession::new(backend).map_err(|error| error.to_string())?;
    let frame = session.grab(rect).map_err(|error| error.to_string())?;
    drop(session);
    Ok(CaptureProbe {
        width: frame.w,
        height: frame.h,
        elapsed_ms: started.elapsed().as_millis() as u64,
        image: thumbnail_image(&frame),
    })
}

/// 整屏帧 → ≤160×64 缩略图（RGBA）；按整数步长抽样，不做逐像素缩放。
fn thumbnail_image(frame: &delta_capture::Frame) -> egui::ColorImage {
    let step_x = (frame.w as usize).div_ceil(THUMB_MAX[0] as usize).max(1) as u32;
    let step_y = (frame.h as usize).div_ceil(THUMB_MAX[1] as usize).max(1) as u32;
    let width = (frame.w as usize).div_ceil(step_x as usize).max(1);
    let height = (frame.h as usize).div_ceil(step_y as usize).max(1);
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let [b, g, r] = frame.pixel((x as u32 * step_x).min(frame.w - 1), (y as u32 * step_y).min(frame.h - 1));
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba)
}

/// 启动时的界面状态：未校准项 / 换分辨率警示。
///
/// **分辨率判断放在这里而不是 `main`**：`config` 与 `monitor` 本就是入参，
/// 装配根不必重复这条规则，也不会出现「算了一次又传一次」的双源。
fn initial_state(config: &Config, monitor: MonitorInfo) -> UiState {
    let mut state = UiState {
        // 显示**真实生效**的键：老配置里残留的 `sb250` 会被规范化成 F8（复审 S2-1）。
        hotkey: platform::format_hotkey(&platform::startup_hotkey(&config.hotkeys.scene)),
        capture_backend: config.capture_backend,
        always_on_top: config.always_on_top,
        // 两个自动开关 + F 键监听：界面上显示的必须是配置里的真实值，
        // 否则「界面说关、流程却在点」是最难查的一类错。
        fingerprint_auto_click: config.fingerprint_auto_click,
        morse_auto_download: config.auto_confirm,
        f_key_hook: config.f_key_hook_enabled,
        master_enabled: config.master_enabled,
        // 7 个功能开关的界面初值必须来自配置：否则重启后界面显示全开、
        // 而执行闸门按配置拦着 —— 用户看着「开着」却怎么按都没反应。
        toggles: Toggles::from_flags(config.flows.flags()),
        hotkey_morse: config.hotkeys.morse.clone(),
        hotkey_fingerprint: config.hotkeys.fingerprint.clone(),
        hotkey_console: config.hotkeys.console.clone(),
        hotkey_keys: config.hotkeys.keys.clone(),
        hotkey_line: config.hotkeys.line.clone(),
        hotkey_master: config.hotkeys.master.clone(),
        hotkey_slot: delta_config::HotkeySlot::Scene,
        game_title_keywords: config.game_title_keywords.clone(),
        keywords_buffer: config.game_title_keywords.join(", "),
        timing_buffers: std::array::from_fn(|index| {
            delta_config::TimingField::ALL[index]
                .read(&config.timings)
                .to_string()
        }),
        theme_id: config.theme_id,
        accent_override: config.accent_override.clone(),
        missing: config.validate().iter().map(ToString::to_string).collect(),
        ..UiState::default()
    };
    if config.resolution_changed(monitor) {
        state.warn_resolution_changed();
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(w: i32, h: i32) -> MonitorInfo {
        MonitorInfo::new(w, h).expect("测试分辨率合法")
    }

    #[test]
    fn resolution_change_reaches_the_ui_as_a_notice() {
        let config = Config {
            monitor: monitor(1920, 1080),
            ..Config::default()
        };
        let state = initial_state(&config, monitor(2560, 1440));
        let notice = state.notice_text().expect("换分辨率必须让用户看得见");
        assert!(notice.contains("重校"), "{notice}");
    }

    #[test]
    fn same_or_uncalibrated_resolution_stays_silent() {
        let config = Config {
            monitor: monitor(1920, 1080),
            ..Config::default()
        };
        assert_eq!(initial_state(&config, monitor(1920, 1080)).notice_text(), None);
        // 首次启动（基线 0x0）不算变更，不打扰。
        assert_eq!(initial_state(&Config::default(), monitor(2560, 1440)).notice_text(), None);
    }

    /// 只为本文件用例准备一个临时配置文件的 app（默认不装 F 键钩子：
    /// 全局钩子会监听真实按键，与"开关落盘"这个用例无关）。
    fn task_scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        assert!(tag.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-'));
        let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.scratch/astra-recovery");
        let metadata = std::fs::symlink_metadata(&parent).expect("受控 .scratch 必须存在");
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("任务时间戳")
            .as_nanos();
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(format!("astra-offline-app-{tag}-{}-{stamp}-{id}", std::process::id()));
        std::fs::create_dir(&dir).expect("任务专有目录必须新建成功，禁止复用既有目录");
        dir
    }

    fn app_with_temp_config(tag: &str) -> DeltaApp {
        let dir = task_scratch_dir(tag);
        let config = Config {
            monitor: monitor(1920, 1080),
            f_key_hook_enabled: false,
            ..Config::default()
        };
        let state = initial_state(&config, config.monitor);
        DeltaApp::assemble(
            config.monitor,
            Arc::new(Mutex::new(config)),
            dir.join("settings.json"),
            state,
            Worker::offline(),
        )
    }

    #[test]
    fn auto_switch_actions_reach_config_state_and_disk() {
        // spec D2 边界：关掉必须**真的不点**，且立刻生效于下一次识别、跨启动保持。
        let mut app = app_with_temp_config("auto-switches");
        assert!(app.state.fingerprint_auto_click, "默认开");
        assert!(app.state.morse_auto_download, "默认开");

        let ctx = egui::Context::default();
        app.handle(Action::ToggleFingerprintAutoClick, &ctx);
        app.handle(Action::ToggleMorseAutoDownload, &ctx);

        assert!(!app.state.fingerprint_auto_click, "界面必须跟着关");
        assert!(!app.state.morse_auto_download);

        let config = app.snapshot();
        // 这两个字段就是 `worker::run_fingerprint` / `run_morse` 读的那两个。
        assert!(!config.fingerprint_auto_click, "指纹流程读的是这个字段");
        assert!(!config.auto_confirm, "摩斯流程读的是这个字段");

        let saved = delta_config::load(&app.config_path)
            .expect("开关必须已落盘")
            .config;
        assert!(!saved.fingerprint_auto_click);
        assert!(!saved.auto_confirm);
    }

    /// 启动时 7 个开关的界面初值必须来自配置。
    ///
    /// 反例：`UiState::default()` 的全开盖过配置 → 重启后界面显示「开」、
    /// 而执行闸门按配置拦着，用户看着开着却怎么按都没反应。
    #[test]
    fn startup_flow_switches_come_from_the_config() {
        let mut flags = [true; delta_config::Flow::ALL.len()];
        flags[delta_config::Flow::Morse.index()] = false;
        flags[delta_config::Flow::Az3Keys.index()] = false;
        let config = Config {
            flows: delta_config::FlowFlags::from_flags(flags),
            ..Config::default()
        };
        let state = initial_state(&config, monitor(1920, 1080));
        assert!(!state.toggles.get_flow(delta_ui::FlowId::Morse), "配置里关着");
        assert!(!state.toggles.get_flow(delta_ui::FlowId::Az3Keys));
        assert!(
            state.toggles.get_flow(delta_ui::FlowId::Az3Line),
            "其余 5 个必须照配置保持开"
        );
    }

    /// 主页开关改动必须落进配置并写盘（重启后仍是关的）。
    ///
    /// 视图侧是**直接改 `UiState`** 的，所以这里模拟「画完一帧后收差异」那条路径。
    #[test]
    fn home_flow_switch_changes_reach_config_and_disk() {
        let mut app = app_with_temp_config("flow-switches");
        app.state.toggles.toggle_flow(delta_ui::FlowId::Az3Line);
        app.sync_flows();

        assert!(!app.snapshot().flows.get(delta_config::Flow::Az3Line), "配置必须跟着关");
        let saved = delta_config::load(&app.config_path)
            .expect("开关必须已落盘")
            .config;
        assert!(!saved.flows.get(delta_config::Flow::Az3Line), "磁盘必须是关的");
        assert!(
            saved.flows.get(delta_config::Flow::Morse),
            "其余 6 个必须原样保持开"
        );

        // 翻回来也要写回（不是只写一次）。
        app.state.toggles.toggle_flow(delta_ui::FlowId::Az3Line);
        app.sync_flows();
        let saved = delta_config::load(&app.config_path)
            .expect("开关必须已落盘")
            .config;
        assert!(saved.flows.get(delta_config::Flow::Az3Line), "重新打开必须写回");
    }

    #[test]
    fn all_seven_flow_switches_persist_independently_without_workers() {
        for off in delta_ui::FlowId::ALL {
            let mut app = app_with_temp_config(&format!("seven-flows-{}", off.index()));
            assert!(app.key_hook.is_none());
            app.state.toggles.toggle_flow(off);
            app.sync_flows();
            let saved = delta_config::load(&app.config_path).expect("临时配置").config;
            for flow in delta_config::Flow::ALL {
                assert_eq!(saved.flows.get(flow), flow.index() != off.index());
            }
        }
    }

    /// 没有差异时不许写盘：主视图每帧都会调 `sync_flows`，它不能变成「每帧写一次配置」。
    #[test]
    fn syncing_flows_without_a_change_never_touches_the_disk() {
        let mut app = app_with_temp_config("flow-noop");
        assert!(!app.config_path.exists(), "新任务目录不应含既有配置");
        app.sync_flows();
        assert!(
            !app.config_path.exists(),
            "没变过的开关不该产生配置文件写入"
        );
    }

    /// 漂移守卫：配置侧 `Flow::ALL` 与界面侧 `FlowId::ALL` 必须同序同名。
    ///
    /// 位序错位 = 用户关「黄键台」其实关掉了别的一个功能（`flags()` 是位置耦合的）。
    #[test]
    fn config_and_ui_flow_tables_stay_in_lockstep() {
        assert_eq!(
            delta_config::Flow::ALL.len(),
            delta_ui::FlowId::ALL.len(),
            "两边的开关数量必须一致"
        );
        for (config_flow, ui_flow) in delta_config::Flow::ALL
            .into_iter()
            .zip(delta_ui::FlowId::ALL)
        {
            assert_eq!(
                config_flow.index(),
                ui_flow.index(),
                "位序必须一致（旧配置按位序读）"
            );
            assert_eq!(
                config_flow.label(),
                ui_flow.label(),
                "主页开关文案必须一致"
            );
        }
    }

    #[test]
    fn legacy_unparsable_hotkey_is_shown_as_the_key_that_actually_works() {        // 复审 S2-1：面板必须显示真实生效的键，而不是配置里的原文。
        let config = Config {
            hotkeys: delta_config::Hotkeys {
                scene: "sb250".to_owned(),
                ..delta_config::Hotkeys::default()
            },
            ..Config::default()
        };
        assert_eq!(
            initial_state(&config, monitor(1920, 1080)).hotkey,
            "F8",
            "不可解析的旧值必须显示成实际生效的 F8"
        );

        // 可解析但写法不规范的旧值也要规范化成唯一写法。
        let config = Config {
            hotkeys: delta_config::Hotkeys {
                scene: "ctrl+f8".to_owned(),
                ..delta_config::Hotkeys::default()
            },
            ..Config::default()
        };
        assert_eq!(initial_state(&config, monitor(1920, 1080)).hotkey, "Ctrl+F8");
    }

    #[test]
    fn initial_state_carries_the_configured_capture_backend() {
        let config = Config {
            capture_backend: CaptureBackend::BitBlt,
            ..Config::default()
        };
        assert_eq!(
            initial_state(&config, monitor(1920, 1080)).capture_backend,
            CaptureBackend::BitBlt,
            "面板必须显示配置里的后端，而不是写死 WGC"
        );
    }

    #[test]
    #[ignore = "真实窗口抓屏探针；游戏运行期间禁止执行，须单独授权"]
    fn capture_probe_grabs_one_frame_or_reports_a_readable_error() {
        // 真机探针（spec D5 验收：点一次 → 一帧 → 宽×高 + 缩略图）。
        // 没有可用 D3D11 适配器时只要求错误可读，绝不静默回退。
        let Ok(monitor) = delta_capture::monitor_info() else {
            return;
        };
        match capture_probe_once(CaptureBackend::Wgc, monitor) {
            Ok(probe) => {
                assert_eq!((probe.width, probe.height), (monitor.w as u32, monitor.h as u32));
                assert!(probe.image.size[0] <= THUMB_MAX[0] as usize);
                assert!(probe.image.size[1] <= THUMB_MAX[1] as usize);
            }
            Err(reason) => {
                assert!(reason.contains("WGC"), "{reason}");
                assert!(reason.contains("0x"), "错误必须带 HRESULT：{reason}");
            }
        }
        // 兼容档不依赖 D3D11，必须能出图（spec D5：切到 BitBlt 仍可测试截图）。
        let compat = capture_probe_once(CaptureBackend::BitBlt, monitor)
            .expect("BitBlt 兼容档必须能抓一帧");
        assert_eq!((compat.width, compat.height), (monitor.w as u32, monitor.h as u32));
    }

}
