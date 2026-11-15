//! eframe 应用：把 `delta-ui` 的画面、`delta-app::worker` 的后台结果和配置串起来。
//!
//! 线程纪律：`ui` 里只 `try_recv` 事件与绘制；识别一律在后台线程跑。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use delta_config::Config;
use delta_geom::{MonitorInfo, PxRect};
use delta_ui::{
    console_result, conflict_copy, Action, CaptureBackend, CaptureTestResult, KeyChord, Phase,
    Progress, SceneKind, Step, ThemeId, UiState, Verdict, Wizard, WizardAction, WizardFrame,
    WizardScene,
};

use crate::keyhook::KeyHook;
use crate::platform;
use crate::worker::{grab_full, grab_once, Cmd, UiEvent, Worker};

/// 保存后自动关窗延时（spec D9：1.5s）。
const SAVE_CLOSE_DELAY: Duration = Duration::from_millis(1500);
/// 运行中重绘间隔（呼吸动画）。
const RUNNING_REPAINT: Duration = Duration::from_millis(16);
/// 非空闲态重绘间隔（让 200ms 高亮/闪烁按时结束）。
const IDLE_REPAINT: Duration = Duration::from_millis(100);
/// 预览图降采样倍数（全屏纹理不必 1:1）。
const PREVIEW_STEP: u32 = 2;
/// 测试截图缩略图上限（spec D5：≤160×90；留一行文案位后取 160×64）。
const THUMB_MAX: [u32; 2] = [160, 64];
/// 图钉切换失败提示（release 版 `windows_subsystem="windows"` 丢弃 stderr，只能靠界面提示）。
const NOTICE_ON_TOP_FAILED: &str = "置顶切换失败";
/// 图钉状态已生效但落盘失败的提示（此时三者已回滚成一致）。
const NOTICE_ON_TOP_UNSAVED: &str = "设置未保存";
/// 置顶回滚失败的提示：窗口确实停在新状态，只是没落盘 —— 不再谎称「已回滚」。
const NOTICE_ON_TOP_ROLLBACK_FAILED: &str = "置顶已变未保存";
/// 补录会话建不起来（无 D3D11 适配器等）的提示。
const NOTICE_TEMPLATE_SESSION_FAILED: &str = "补录会话启动失败";
/// 补录 PNG 写盘失败的提示（已删掉本次写出的所有格，目录里不留残份）。
const NOTICE_TEMPLATE_WRITE_FAILED: &str = "模板写入失败";
/// 开关已生效但没存进配置文件的提示（重启后会回到旧状态）。
const NOTICE_SETTINGS_UNSAVED: &str = "设置未保存";
/// F 键钩子装不上的提示（已回滚成「关」，不谎称在监听）。
const NOTICE_KEY_HOOK_FAILED: &str = "F 键监听装不上";

/// 应用状态。
pub struct DeltaApp {
    state: UiState,
    wizard: Option<Wizard>,
    settings_open: bool,
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    monitor: MonitorInfo,
    worker: Worker,
    preview: Option<egui::TextureHandle>,
    existing: Vec<(String, delta_geom::PctRect)>,
    save_finished_at: Option<Instant>,
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
        let mut app = Self {
            state,
            wizard: None,
            settings_open: false,
            config,
            config_path,
            monitor,
            worker,
            preview: None,
            existing: Vec::new(),
            save_finished_at: None,
            phase_started: Instant::now(),
            capture_probe: None,
            key_hook: None,
            applied_theme: None,
            wake_handed: false,
        };
        // 配置说开着就当场装上（装不上只留痕，绝不因为钩子失败而不给启动）。
        app.sync_key_hook();
        app
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

    /// 校准保存：把主显示器物理分辨率写成**当前实际读数**，作为百分比坐标的锚点。
    ///
    /// 只有校准动作才更新基线，因此换分辨率而未重新校准时会持续提醒。
    fn persist_calibrated(&mut self) -> Result<(), String> {
        let mut config = self.snapshot();
        config.monitor = self.monitor;
        delta_config::save(&self.config_path, &config).map_err(|error| error.to_string())
    }

    /// 重新同步「未校准项」。
    ///
    /// **不碰 `state.hotkey`**：那个值只由热键线程的回执改写（`HotkeyApplied`）。
    /// 若在这里按配置回填，一次「注册失败但已落盘」的改键会借校准动作重新变成
    /// 「界面显示新键、实际仍是旧键」的假保存。
    fn sync_calibration(&mut self) {
        let config = self.snapshot();
        self.state.missing = config.validate().iter().map(ToString::to_string).collect();
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
            Action::OpenCalibration => self.enter_wizard(ctx),
            Action::OpenSettings => self.settings_open = true,
            Action::ToggleFlow(scene) => self.state.toggles.toggle(scene),
            // 全关时的修复动作（主按钮文案「请开启至少一项」的落点）。
            Action::EnableFlows => self.state.toggles.enable_all(),
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
                // 发送端随钩子卸载一起消失 → `recv` 出错 → 本线程自然退出。
                while events.recv().is_ok() {
                    if commands.send(Cmd::Trigger).is_err() {
                        break;
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

    /// 开关落盘：失败必须显形（内存里已生效，但重启后会丢）。
    fn persist_settings(&mut self) {
        if let Err(reason) = self.persist() {
            eprintln!("[settings] 开关已生效但落盘失败：{reason}");
            self.state.notice = Some(NOTICE_SETTINGS_UNSAVED.to_owned());
        }
    }

    /// 进入校准向导。
    fn enter_wizard(&mut self, _ctx: &egui::Context) {        self.wizard = Some(Wizard::new());
        self.settings_open = false;
        self.preview = None;
        self.existing.clear();
        self.save_finished_at = None;
    }

    /// 某一帧的预览是否需要重抓（进入框选步 but 还没有底图）。
    fn ensure_preview(&mut self, ctx: &egui::Context) {
        let Some(wizard) = &self.wizard else {
            return;
        };
        let needs_full = matches!(wizard.step, Step::SelectRegion) && self.preview.is_none();
        if needs_full {
            self.preview = self.capture_texture(ctx, None);
        }
    }

    /// 截图并上传成纹理；`region` 为 `None` 时抓全屏。
    ///
    /// 走 [`grab_once`] / [`grab_full`]（独立会话，后端取配置里的 `capture_backend`）：
    /// 本函数在 UI 线程，Wgc 会在这里一次性初始化设备（约 30~50ms）；预览有缓存
    /// （只在 `preview.is_none()` 或框选完成时抓一次），不会落到每帧。
    fn capture_texture(
        &self,
        ctx: &egui::Context,
        region: Option<PxRect>,
    ) -> Option<egui::TextureHandle> {
        let backend = self.snapshot().capture_backend;
        let frame = match region {
            Some(rect) => grab_once(rect, backend).ok()?,
            None => grab_full(self.monitor, backend).ok()?,
        };
        Some(ctx.load_texture(
            "delta-preview",
            frame_to_image(&frame),
            egui::TextureOptions::LINEAR,
        ))
    }

    /// 校准向导一帧。
    fn wizard_frame(&mut self, ui: &mut egui::Ui, pal: &delta_ui::theme::Palette) {
        let ctx = ui.ctx().clone();
        self.ensure_preview(&ctx);
        self.run_pending_test();
        let existing = self.existing.clone();
        let preview = self.preview.as_ref();
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let frame = WizardFrame {
            preview,
            monitor: [self.monitor.w.max(0) as u32, self.monitor.h.max(0) as u32],
            existing: &existing,
        };
        let action = delta_ui::wizard_window(ui, wizard, frame, pal);
        if let Some(action) = action {
            self.apply_wizard_action(action, &ctx);
        }
        // 卡片里可能刚选了场景：重建已存区域列表（每帧一次，成本可忽略）。
        let scene = self.wizard.as_ref().and_then(|wizard| wizard.scene);
        self.existing = existing_rects(&self.snapshot(), scene);
        if let Some(saved_at) = self.save_finished_at {
            if saved_at.elapsed() >= SAVE_CLOSE_DELAY {
                self.close_wizard();
            } else {
                ctx.request_repaint_after(SAVE_CLOSE_DELAY);
            }
        }
    }

    fn apply_wizard_action(&mut self, action: WizardAction, ctx: &egui::Context) {
        match action {
            WizardAction::Drawn(rect) => {
                let Some(wizard) = self.wizard.as_mut() else {
                    return;
                };
                let (Some(scene), Some(spec)) = (wizard.scene, wizard.current().copied()) else {
                    return;
                };
                if let Ok(mut config) = self.config.lock() {
                    delta_ui::apply_drawn(&mut config, scene, spec, rect);
                }
                wizard.record_drawn();
                self.existing = existing_rects(&self.snapshot(), Some(scene));
                self.preview = self.region_texture(ctx, rect);
            }
            WizardAction::Primary => {
                let Some(wizard) = self.wizard.as_mut() else {
                    return;
                };
                let was = wizard.step;
                if !wizard.advance() {
                    return;
                }
                if was == Step::Save {
                    let recapture = wizard.scene == Some(WizardScene::Recapture);
                    match self.persist_calibrated() {
                        Ok(()) => {
                            if recapture {
                                self.save_template_cells();
                            }
                            self.save_finished_at = Some(Instant::now());
                        }
                        Err(reason) => self.finish(Phase::Unrecognized, reason),
                    }
                }
                self.sync_calibration();
            }
            WizardAction::Redo => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.redo();
                }
                self.preview = None;
            }
            WizardAction::Back => {
                let stepped = self.wizard.as_mut().is_some_and(Wizard::back);
                if !stepped {
                    self.close_wizard();
                }
            }
            WizardAction::Close => self.close_wizard(),
        }
    }

    /// 补录模板：把 9 个候选格截下来，按 `images/<角色名>/<序号>.png` 入库（spec 风险缓解）。
    ///
    /// **九格共用一个会话**：`CaptureSession::new` 在 Wgc 档要建一次 D3D11 设备
    /// （30~50ms + 帧池重建），逐格新建等于把这笔代价付九遍（实测累计 300~450ms 卡顿）。
    /// 抓取内容与顺序与逐格版本完全一致：同一个 `candidate_boxes` 顺序、同样逐格 `luma()`。
    ///
    /// **要么全存、要么不存**：WGC 在静态画面（暂停/加载/菜单）下可能只出第一帧，
    /// 后面各格全抓不到；残缺模板会让后续识别一直失败且原因难查，所以格数不齐时
    /// 直接中止并出提示，不落半份模板。写盘同理：[`persist_cell_pngs`] 先把九格写成
    /// `.tmp`，全部成功后才 rename 换名 —— 任何一步失败都不会碰到目录里原有的模板。
    fn save_template_cells(&mut self) {
        let Some(wizard) = self.wizard.as_ref() else {
            return;
        };
        let role = wizard.role_name.trim();
        if role.is_empty() {
            return;
        }
        let dir = crate::images_dir().join(role);
        let _ = std::fs::create_dir_all(&dir);
        let config = self.snapshot();
        let Ok(mut session) = delta_capture::CaptureSession::new(config.capture_backend) else {
            self.state.notice = Some(NOTICE_TEMPLATE_SESSION_FAILED.to_owned());
            return;
        };
        let captured = capture_cells(&config.fingerprint.candidate_boxes, self.monitor, |rect| {
            session.grab(rect).map_err(|error| error.to_string())
        });
        // 九格抓完统一释放：设备与 20 张配额一次性还清。
        drop(session);
        if let Some(notice) = incomplete_cells_notice(&captured) {
            self.state.notice = Some(notice);
            return;
        }
        if let Err(reason) = persist_cell_pngs(&dir, &captured.frames) {
            eprintln!("[templates] 模板落盘失败：{reason}");
            self.state.notice = Some(NOTICE_TEMPLATE_WRITE_FAILED.to_owned());
        }
    }

    /// 关闭向导（静默保留草稿：已框的百分比已经写进内存配置）。
    fn close_wizard(&mut self) {
        self.wizard = None;
        self.preview = None;
        self.save_finished_at = None;
        self.sync_calibration();
    }

    /// 把百分比矩形换算成截图纹理（「当场重抓该区域」）。
    fn region_texture(
        &self,
        ctx: &egui::Context,
        rect: delta_geom::PctRect,
    ) -> Option<egui::TextureHandle> {
        let pixels = rect.to_px(self.monitor).ok()?;
        if pixels.w <= 0 || pixels.h <= 0 {
            return None;
        }
        self.capture_texture(ctx, Some(pixels))
    }

    /// 进入试跑步时自动跑一次只读识别（**不产生任何输入**）。
    fn run_pending_test(&mut self) {
        let pending = self
            .wizard
            .as_ref()
            .is_some_and(|wizard| wizard.step == Step::TestRun && wizard.verdict == Verdict::Pending);
        if !pending {
            return;
        }
        let scene = self
            .wizard
            .as_ref()
            .and_then(|wizard| wizard.scene)
            .unwrap_or(WizardScene::Morse);
        let verdict = self.readonly_test(scene);
        if let Some(wizard) = self.wizard.as_mut() {
            wizard.verdict = verdict;
        }
    }

    /// 只读识别：按场景读一次现有校准区域。
    ///
    /// 走独立会话的 `grab_once` / `grab_full`（后端取配置里的 `capture_backend`）：
    /// 本函数在 UI 线程，Wgc 的一次性初始化开销只落在这唯一一次试跑上。
    fn readonly_test(&self, scene: WizardScene) -> Verdict {
        let config = self.snapshot();
        let Ok(view) = config.try_px_view() else {
            return Verdict::Failed("请先校准".to_owned());
        };
        let backend = config.capture_backend;
        match scene {
            WizardScene::Morse => {
                let Some(region) = view.regions.first() else {
                    return Verdict::Failed("请先校准".to_owned());
                };
                match grab_once(region.rect, backend) {
                    Ok(frame) => match delta_flow_morse::decode_gray(&frame.luma()) {
                        Ok(digit) => Verdict::Read(digit.to_string()),
                        Err(error) => Verdict::Failed(error.to_string()),
                    },
                    Err(reason) => Verdict::Failed(reason),
                }
            }
            WizardScene::Console => match grab_once(view.console.led_region, backend) {
                Ok(frame) => match delta_vision::read_glyph_string(&frame.luma()) {
                    Ok(text) => Verdict::Read(text),
                    Err(error) => Verdict::Failed(error.to_string()),
                },
                Err(reason) => Verdict::Failed(reason),
            },
            WizardScene::Fingerprint | WizardScene::Recapture => match grab_full(self.monitor, backend) {
                Ok(frame) => {
                    let verdict = delta_scene::detect(&frame, &view);
                    if verdict.is_actionable() {
                        Verdict::Read(verdict.scene.name().to_owned())
                    } else {
                        Verdict::Failed(verdict.reason.to_owned())
                    }
                }
                Err(reason) => Verdict::Failed(reason),
            },
        }
    }

    /// 非空闲态维持重绘（呼吸 / 闪烁 / 200ms 高亮）。
    fn schedule_repaint(&self, ctx: &egui::Context) {
        if self.state.phase == Phase::Running {
            ctx.request_repaint_after(RUNNING_REPAINT);
        } else if self.state.phase != Phase::Idle {
            ctx.request_repaint_after(IDLE_REPAINT);
        }
        if self.wizard.is_some() {
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

        if self.wizard.is_some() {
            self.wizard_frame(ui, &pal);
            self.schedule_repaint(&ctx);
            return;
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
            // 关键词编辑框的文本归 UI 状态所有：先取出、交给面板编辑、再放回。
            let mut keywords = std::mem::take(&mut self.state.keywords_buffer);
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
                    ocr_enabled: self.snapshot().ocr_enabled,
                    config_path: &self.config_path.display().to_string(),
                    capture_backend: self.state.capture_backend,
                    capture_locked,
                    capture_test: capture_test.as_ref(),
                    capture_test_error: capture_test_error.as_deref(),
                    theme_id,
                    accent_override: accent.as_deref(),
                    f_key_hook,
                    keywords: &mut keywords,
                },
                &pal,
            );
            self.state.keywords_buffer = keywords;
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
        self.schedule_repaint(&ctx);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 后台线程可能在没有输入时完成，靠这里把窗口叫醒。
        if self.state.phase == Phase::Running {
            ctx.request_repaint();
        }
    }
}

/// 九格抓取结果：抓到的帧 + 抓失败的候选格序号。
///
/// 分开记 `failed` 而不是只返回成功的帧：跳过一个「未校准的空格」是正常的，
/// 抓帧报错（WGC 静态画面下拿不到新帧）不是 —— 后者必须能被调用方看见并中止保存。
struct CellCapture {
    frames: Vec<(usize, delta_capture::Frame)>,
    failed: Vec<usize>,
}

/// 缺格提示；`None` 表示九格齐备、可以落盘。
///
/// 措辞必须是「未保存」：`save_template_cells` 在缺格时**一张 PNG 都不写**，
/// 说「只存下 n/9 格」会让用户以为手里还有 n 格可用模板，实际后续识别全失败。
fn incomplete_cells_notice(captured: &CellCapture) -> Option<String> {
    if captured.failed.is_empty() {
        return None;
    }
    if captured.frames.is_empty() {
        // 「一帧都没抓到」（设备/窗口异常）与「静态画面只出首帧」是两种不同的故障，
        // 排查方向完全不同，不能共用一句「缺 9 格」。
        return Some(format!("一帧都没抓到，未保存（{}/9 格失败）", captured.failed.len()));
    }
    Some(format!("缺 {} 格，未保存", captured.failed.len()))
}

/// 模板目录里的正式文件名：`<序号>.png`。
fn cell_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("{}.png", index + 1))
}

/// 单格写盘的临时文件名：九格全部写成 `.tmp` 后才统一 rename 成 [`cell_path`]。
fn cell_tmp_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("{}.png.tmp", index + 1))
}

/// 把九格写进模板目录：**先全部写 `.tmp`，九格都成功后再逐个 rename 落定**。
///
/// 两阶段的原因：`File::create` 正式名会就地截断旧模板。若第 5 格写失败，
/// `1..4.png` 已是新图、`5..9.png` 还是旧图 —— 目录里照样九张齐全，`loading.rs`
/// 按连续前缀零警告接受，新旧混合的模板会静默生效，正是最坏的残份。
/// 所以所有可能失败的动作（写 PNG、`finish` 刷 IEND）都排在 rename 之前；
/// rename 只做不带数据写入的换名，旧模板在那一刻才被替换。
fn persist_cell_pngs(
    dir: &Path,
    cells: &[(usize, delta_capture::Frame)],
) -> Result<(), String> {
    for (index, frame) in cells {
        if let Err(reason) = write_cell_png(&cell_tmp_path(dir, *index), &frame.luma()) {
            discard_cell_temps(dir, cells);
            return Err(reason);
        }
    }
    for (index, _) in cells {
        let (temp, target) = (cell_tmp_path(dir, *index), cell_path(dir, *index));
        if let Err(error) = std::fs::rename(&temp, &target) {
            // 未 rename 的 `.tmp` 清掉；已经落定的格**不删** —— 那是新且可解码的整格，
            // 删掉才是真丢数据。
            discard_cell_temps(dir, cells);
            return Err(format!("{target:?}: {error}"));
        }
    }
    Ok(())
}

/// 把一格灰度帧写成 PNG；任一步失败都返回 `Err`，由调用方统一清场。
///
/// 单独成函数是为了让「写失败必须显形」这件事有单一落点：不在这里 `continue`，
/// 也不要让 `write_image_data` / `finish` 的返回值被丢掉。
fn write_cell_png(path: &Path, gray: &delta_vision::GrayImage) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|error| format!("{path:?}: {error}"))?;
    let mut encoder = png::Encoder::new(file, gray.w, gray.h);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|error| format!("{path:?}: {error}"))?;
    writer
        .write_image_data(gray.as_slice())
        .map_err(|error| format!("{path:?}: {error}"))?;
    // png 0.17 的 `Writer::drop` 不写 IEND 块，不 finish 的 PNG 可能不完整。
    writer.finish().map_err(|error| format!("{path:?}: {error}"))
}

/// 删掉本次产生的 `.tmp` 残份（删不掉就忽略：尽力不留残份，不为此再堆一层提示）。
///
/// 只碰 `.tmp`：正式 `<序号>.png` 一律不动，用户原有的模板必须绝对安全。
fn discard_cell_temps(dir: &Path, cells: &[(usize, delta_capture::Frame)]) {
    for (index, _) in cells {
        let _ = std::fs::remove_file(cell_tmp_path(dir, *index));
    }
}

/// 按百分比矩形逐格抓图，返回序号与帧（顺序与入参一致；空格跳过，抓失败另记）。
///
/// `grab` 由调用方提供**同一个会话**的抓帧闭包 —— 「九格只初始化一次后端」就落在这里。
/// 抽成独立函数是为了能在测试里换掉 `grab`，把「一次会话 + 逐格顺序不变」钉死。
fn capture_cells(
    rects: &[delta_geom::PctRect],
    monitor: MonitorInfo,
    mut grab: impl FnMut(PxRect) -> Result<delta_capture::Frame, String>,
) -> CellCapture {
    let mut captured = CellCapture {
        frames: Vec::new(),
        failed: Vec::new(),
    };
    for (index, rect) in rects.iter().enumerate() {
        let Ok(pixels) = rect.to_px(monitor) else {
            captured.failed.push(index);
            continue;
        };
        if pixels.w <= 0 || pixels.h <= 0 {
            continue;
        }
        match grab(pixels) {
            Ok(frame) => captured.frames.push((index, frame)),
            Err(_) => captured.failed.push(index),
        }
    }
    captured
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
        hotkey_morse: config.hotkeys.morse.clone(),
        hotkey_fingerprint: config.hotkeys.fingerprint.clone(),
        hotkey_console: config.hotkeys.console.clone(),
        hotkey_keys: config.hotkeys.keys.clone(),
        hotkey_line: config.hotkeys.line.clone(),
        hotkey_master: config.hotkeys.master.clone(),
        hotkey_slot: delta_config::HotkeySlot::Scene,
        game_title_keywords: config.game_title_keywords.clone(),
        keywords_buffer: config.game_title_keywords.join(", "),
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

/// 全屏帧 → egui 图像（降采样，顺便把 BGRA 转成 RGBA）。
fn frame_to_image(frame: &delta_capture::Frame) -> egui::ColorImage {
    let step = PREVIEW_STEP.max(1);
    let width = (frame.w / step).max(1);
    let height = (frame.h / step).max(1);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let [b, g, r] = frame.pixel(x * step, y * step);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba)
}

/// 已存区域的百分比框（向导里画琥珀虚线）。
fn existing_rects(
    config: &Config,
    scene: Option<WizardScene>,
) -> Vec<(String, delta_geom::PctRect)> {
    let Some(scene) = scene else {
        return Vec::new();
    };
    let mut rects = Vec::new();
    match scene {
        WizardScene::Morse => {
            for (index, region) in config.regions.iter().enumerate().take(3) {
                rects.push((format!("摩斯 {index}"), region.rect));
            }
        }
        WizardScene::Fingerprint | WizardScene::Recapture => {
            rects.push(("角色名".to_owned(), config.fingerprint.name_region));
            rects.push(("数字区".to_owned(), config.fingerprint.number_region));
            for (index, rect) in config.fingerprint.candidate_boxes.iter().enumerate() {
                rects.push((format!("候选 {index}"), *rect));
            }
        }
        WizardScene::Console => {
            rects.push(("目标序列".to_owned(), config.console.led_region));
            rects.push(("面板".to_owned(), config.console.panel_region));
        }
    }
    rects
        .into_iter()
        .filter(|(_, rect)| !rect.is_empty())
        .collect()
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
    fn app_with_temp_config(tag: &str) -> DeltaApp {
        let dir = std::env::temp_dir().join(format!("delta-app-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let config = Config {
            monitor: monitor(1920, 1080),
            f_key_hook_enabled: false,
            ..Config::default()
        };
        DeltaApp::new(config.monitor, config, dir.join("settings.json"))
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

    #[test]
    fn template_cells_keep_candidate_order_and_skip_empty_boxes() {
        let monitor = monitor(2560, 1440);
        // 第 5 格清空（未校准的候选格）→ 必须被跳过，但不许打乱其余各格的身份。
        let mut boxes = delta_config::builtin::candidate_boxes();
        boxes[4] = delta_geom::PctRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        let mut asked = Vec::new();
        let captured = capture_cells(&boxes, monitor, |rect| {
            asked.push(rect);
            Ok(delta_capture::Frame::new(
                rect.w as u32,
                rect.h as u32,
                vec![0; (rect.w * rect.h * 4) as usize],
                Instant::now(),
            )
            .expect("测试帧长度正确"))
        });
        let cells = captured.frames;
        assert_eq!(
            cells.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 5, 6, 7, 8],
            "补录顺序必须是候选格顺序，空掉的格子只跳过不顶位"
        );
        // 第一格是 2K 基准下内置坐标的实测像素值（独立已知量，不由被测代码推导）。
        assert_eq!(asked[0], PxRect { x: 1522, y: 560, w: 105, h: 105 });
        assert_eq!(asked.len(), cells.len(), "每次成功抓取都必须对应一格");
        assert!(
            captured.failed.is_empty(),
            "未校准的空格不算抓失败：{:?}",
            captured.failed
        );
    }

    #[test]
    fn capture_cells_reports_failed_grabs_so_a_half_template_is_never_saved() {
        // WGC 在静态画面（暂停/加载/菜单）下的真实失败模式：共用会话拿到第一帧后再无新帧。
        // 抓不到必须被数出来并拦住保存，否则会存下 1 格模板，后续识别一直失败且原因难查。
        let monitor = monitor(2560, 1440);
        let boxes = delta_config::builtin::candidate_boxes();
        let mut attempts = 0;
        let captured = capture_cells(&boxes, monitor, |rect| {
            attempts += 1;
            if attempts == 2 || attempts == 5 {
                return Err("WGC 未取到新帧".to_owned());
            }
            delta_capture::Frame::new(
                rect.w as u32,
                rect.h as u32,
                vec![0; (rect.w * rect.h * 4) as usize],
                Instant::now(),
            )
            .map_err(|error| error.to_string())
        });
        assert_eq!(captured.failed, vec![1, 4], "失败格号必须如实上报");
        assert_eq!(
            captured.frames.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 2, 3, 5, 6, 7, 8],
            "成功的格子仍按候选顺序排列"
        );
        assert_eq!(
            incomplete_cells_notice(&captured),
            Some("缺 2 格，未保存".to_owned()),
            "残缺时必须明说「一格都没存」，而不是让人以为还有 7 格可用"
        );
    }

    /// 落盘测试专用临时目录。
    fn cells_scratch_dir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "delta-pwd-cells-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("临时目录可建");
        root
    }

    /// 九格样本：每格尺寸与像素值都不同，读回时才能分辨「串格」与「整片同值」。
    fn sample_cells(count: usize) -> Vec<(usize, delta_capture::Frame)> {
        (0..count)
            .map(|index| {
                let (w, h) = (3 + index as u32, 2 + index as u32);
                let value = (index as u8 + 1) * 10;
                let frame = delta_capture::Frame::new(
                    w,
                    h,
                    vec![value; (w * h * 4) as usize],
                    Instant::now(),
                )
                .expect("测试帧长度正确");
                (index, frame)
            })
            .collect()
    }

    #[test]
    fn saved_cells_round_trip_through_the_template_decoder() {
        // 写盘测试必须**读回来**：只断言文件存在的话，缺 IEND 的残断 PNG 也能让用例变绿。
        // 这里用模板库自己的解码器（`loading.rs::decode_png_gray`）逐格读回，
        // 尺寸与像素都要原样复原 —— 这条同时守住「写完必须 finish」。
        let root = cells_scratch_dir("roundtrip");
        let cells = sample_cells(9);
        assert_eq!(persist_cell_pngs(&root, &cells), Ok(()), "九格必须全部落盘");

        for (index, frame) in &cells {
            let path = root.join(format!("{}.png", index + 1));
            let decoded = delta_templates::decode_png_gray(&path)
                .unwrap_or_else(|error| panic!("{} 必须能被模板库解码：{error}", path.display()));
            assert_eq!((decoded.w, decoded.h), (frame.w, frame.h), "尺寸必须原样复原");
            assert_eq!(
                decoded.as_slice(),
                frame.luma().as_slice(),
                "像素必须原样复原（第 {index} 格不许串格）"
            );
            assert!(
                !root.join(format!("{}.png.tmp", index + 1)).exists(),
                "落定后不许留 .tmp"
            );
        }

        let gray = cells[0].1.luma();
        assert!(
            write_cell_png(&root.join("no-such-dir/2.png"), &gray).is_err(),
            "父目录不存在时必须返回错误，不能静默吞掉"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_cell_write_leaves_every_existing_template_untouched() {
        // 崩溃现场：直接覆盖写正式名时，第 5 格失败会留下「1-4 新、5-9 旧」的九张齐全目录，
        // `loading.rs` 按连续前缀零警告接受 —— 新旧混合模板静默生效，比没有 PNG 更坏。
        // 这里把第 5 格写盘逼失败，断言原有九张**一张都没被截断**，且清理只清 `.tmp`。
        let root = cells_scratch_dir("atomic");
        let old = delta_vision::GrayImage::filled(2, 2, 7).expect("合法图");
        for index in 0..9 {
            write_cell_png(&cell_path(&root, index), &old).expect("旧模板先落盘");
        }
        // 第 5 格的 `.tmp` 位置被一个目录占住：Windows 下 `File::create` 必失败。
        std::fs::create_dir_all(cell_tmp_path(&root, 4)).expect("占位目录可建");

        let error = persist_cell_pngs(&root, &sample_cells(9)).expect_err("写不进去必须报错");
        assert!(error.contains("5.png.tmp"), "错误必须点名是哪一格：{error}");

        for index in 0..9 {
            let path = cell_path(&root, index);
            let decoded = delta_templates::decode_png_gray(&path)
                .unwrap_or_else(|error| panic!("{} 必须完好：{error}", path.display()));
            assert!(
                decoded.as_slice().iter().all(|&value| value == 7),
                "{index} 号旧模板被截断覆盖了"
            );
        }
        for index in 0..4 {
            assert!(
                !cell_tmp_path(&root, index).exists(),
                "失败清场必须删掉本次写的 .tmp"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn zero_frames_are_reported_differently_from_a_partial_capture() {
        // 「一帧都没抓到」（设备/窗口异常）与「静态画面只出首帧」排查方向完全不同，
        // 提示必须能分辨；两者都不保存。
        let monitor = monitor(2560, 1440);
        let boxes = delta_config::builtin::candidate_boxes();
        let captured = capture_cells(&boxes, monitor, |_| Err("WGC 未取到首帧".to_owned()));
        assert!(captured.frames.is_empty(), "抓帧全失败时不该有帧");
        let notice = incomplete_cells_notice(&captured).expect("零帧必须提示");
        assert!(notice.contains("一帧都没抓到"), "{notice}");
        assert!(notice.contains("未保存"), "{notice}");
    }

    #[test]
    fn complete_cells_raise_no_notice() {
        let monitor = monitor(2560, 1440);
        let boxes = delta_config::builtin::candidate_boxes();
        let captured = capture_cells(&boxes, monitor, |rect| {
            delta_capture::Frame::new(
                rect.w as u32,
                rect.h as u32,
                vec![0; (rect.w * rect.h * 4) as usize],
                Instant::now(),
            )
            .map_err(|error| error.to_string())
        });
        assert_eq!(captured.frames.len(), delta_config::CANDIDATE_BOX_COUNT);
        assert_eq!(incomplete_cells_notice(&captured), None, "九格齐了就不要打扰用户");
    }

    #[test]
    fn template_cells_share_one_capture_session() {
        // 跑**产品默认后端**（Wgc），不是兼容档：真机上九格如果各建一次会话，
        // `frames_taken()` 只会是 1；一个会话抓满九格才等于九 —— 这就是
        // 「只初始化一次 D3D11」的可观测证据。
        let Ok(monitor) = delta_capture::monitor_info() else {
            return;
        };
        let backend = CaptureBackend::default();
        let Ok(mut session) = delta_capture::CaptureSession::new(backend) else {
            eprintln!("[test] {backend:?} 在测试环境不可用（无可用 D3D11 适配器），跳过本用例");
            return;
        };
        let boxes = delta_config::builtin::candidate_boxes();
        let captured = capture_cells(&boxes, monitor, |rect| {
            session.grab(rect).map_err(|error| error.to_string())
        });
        let frames = captured.frames.len() as u32;
        // 先判「九格是否齐」，齐了才有资格断言；否则静态桌面上 WGC 只出首帧会让
        // 「每格都成功」这个前提不成立，断言必红（本用例曾经因此随机失败）。
        if frames < delta_config::CANDIDATE_BOX_COUNT as u32 {
            // 静态桌面下 WGC 只出首帧是已知行为，此时保存会被 `incomplete_cells_notice` 拦住，
            // 本用例无法验证「九格一个会话」：明确跳过，而不是把断言降级成 `> 0` 的安慰剂
            // （`> 0` 在只出首帧的机器上会静默变绿，等于没验）。
            eprintln!("[test] {backend:?} 只出了 {frames} 帧（静态桌面），跳过九格断言");
            return;
        }
        // 九格齐了才验核心命题：一个会话抓满九格（`frames_taken` 与成功帧数相等）。
        assert_eq!(
            session.frames_taken(),
            frames,
            "抓到的每一格都必须出自同一个会话"
        );
        for (_, frame) in &captured.frames {
            assert_eq!(frame.luma().as_slice().len(), (frame.w * frame.h) as usize);
        }
    }

    #[test]
    fn thumbnail_stays_within_the_size_budget_and_keeps_every_pixel() {
        let frame = delta_capture::Frame::new(
            640,
            360,
            vec![0; 640 * 360 * 4],
            Instant::now(),
        )
        .expect("测试帧长度正确");
        let image = thumbnail_image(&frame);
        assert!(
            image.size[0] <= THUMB_MAX[0] as usize && image.size[1] <= THUMB_MAX[1] as usize,
            "缩略图超限：{:?}",
            image.size
        );
        assert_eq!(image.pixels.len(), image.size[0] * image.size[1]);
    }
}


