//! eframe 应用：把 `delta-ui` 的画面、`delta-app::worker` 的后台结果和配置串起来。
//!
//! 线程纪律：`ui` 里只 `try_recv` 事件与绘制；识别一律在后台线程跑。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use delta_config::Config;
use delta_geom::{MonitorInfo, PxRect};
use delta_ui::{
    Action, Phase, Progress, Step, UiState, Verdict, Wizard, WizardAction, WizardFrame,
    WizardScene,
};

use crate::worker::{grab_full, Cmd, UiEvent, Worker};

/// 保存后自动关窗延时（spec D9：1.5s）。
const SAVE_CLOSE_DELAY: Duration = Duration::from_millis(1500);
/// 运行中重绘间隔（呼吸动画）。
const RUNNING_REPAINT: Duration = Duration::from_millis(16);
/// 非空闲态重绘间隔（让 200ms 高亮/闪烁按时结束）。
const IDLE_REPAINT: Duration = Duration::from_millis(100);
/// 预览图降采样倍数（全屏纹理不必 1:1）。
const PREVIEW_STEP: u32 = 2;

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
    keep_topmost: bool,
}

impl DeltaApp {
    /// 构造（配置已由 `main` 读好；DPI 感知也已在此之前完成）。
    pub fn new(monitor: MonitorInfo, config: Config, config_path: PathBuf) -> Self {
        let missing = config.validate();
        let state = UiState {
            disclaimer_pending: !config.accepted_disclaimer,
            hotkey: config.hotkeys.scene.clone(),
            missing: missing.iter().map(ToString::to_string).collect(),
            ..UiState::default()
        };
        let config = Arc::new(Mutex::new(config));
        let worker = Worker::spawn(config.clone(), monitor);
        Self {
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
            keep_topmost: true,
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

    /// 校准保存：把主显示器物理分辨率写成**当前实际读数**，作为百分比坐标的锚点。
    ///
    /// 只有校准动作才更新基线，因此换分辨率而未重新校准时会持续提醒。
    fn persist_calibrated(&mut self) -> Result<(), String> {
        let mut config = self.snapshot();
        config.monitor = self.monitor;
        delta_config::save(&self.config_path, &config).map_err(|error| error.to_string())
    }

    /// 重新同步「未校准项」与热键标签。
    fn sync_calibration(&mut self) {
        let config = self.snapshot();
        self.state.missing = config.validate().iter().map(ToString::to_string).collect();
        self.state.hotkey = config.hotkeys.scene.clone();
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
                }
                UiEvent::Succeeded(message) => self.finish(Phase::Success, message),
                UiEvent::Failed(message) => self.finish(Phase::Unrecognized, message),
                UiEvent::Rejected(message) => self.finish(Phase::Unrecognized, message),
                UiEvent::TimedOut(message) => self.finish(Phase::TimedOut, message),
            }
            ctx.request_repaint();
        }
    }

    /// 收尾成某一态。
    fn finish(&mut self, phase: Phase, message: String) {
        self.state.finish(phase, message);
        self.phase_started = Instant::now();
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
            Action::AcceptDisclaimer => {
                if let Ok(mut config) = self.config.lock() {
                    config.accepted_disclaimer = true;
                }
                self.state.disclaimer_pending = false;
                if let Err(reason) = self.persist() {
                    self.finish(Phase::Unrecognized, reason);
                }
            }
            Action::ToggleFlow(scene) => self.state.toggles.toggle(scene),
        }
        if self.keep_topmost {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::AlwaysOnTop,
            ));
        }
    }

    /// 进入校准向导。
    fn enter_wizard(&mut self, _ctx: &egui::Context) {
        self.wizard = Some(Wizard::new());
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
    fn capture_texture(
        &self,
        ctx: &egui::Context,
        region: Option<PxRect>,
    ) -> Option<egui::TextureHandle> {
        let frame = match region {
            Some(rect) => {
                let mut capturer =
                    delta_capture::Capturer::new(self.monitor.w, self.monitor.h).ok()?;
                capturer.grab(rect).ok()?
            }
            None => grab_full(self.monitor).ok()?,
        };
        Some(ctx.load_texture(
            "delta-preview",
            frame_to_image(&frame),
            egui::TextureOptions::LINEAR,
        ))
    }

    /// 校准向导一帧。
    fn wizard_frame(&mut self, ui: &mut egui::Ui) {
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
        let action = delta_ui::wizard_window(ui, wizard, frame);
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
    fn save_template_cells(&self) {
        let Some(wizard) = self.wizard.as_ref() else {
            return;
        };
        let role = wizard.role_name.trim();
        if role.is_empty() {
            return;
        }
        let config = self.snapshot();
        let dir = crate::images_dir().join(role);
        let _ = std::fs::create_dir_all(&dir);
        let mut capturer = match delta_capture::Capturer::new(self.monitor.w, self.monitor.h) {
            Ok(capturer) => capturer,
            Err(_) => return,
        };
        for (index, rect) in config.fingerprint.candidate_boxes.iter().enumerate() {
            let Ok(pixels) = rect.to_px(self.monitor) else {
                continue;
            };
            if pixels.w <= 0 || pixels.h <= 0 {
                continue;
            }
            let Ok(frame) = capturer.grab(pixels) else {
                continue;
            };
            let gray = frame.luma();
            let Ok(file) = std::fs::File::create(dir.join(format!("{}.png", index + 1))) else {
                continue;
            };
            let mut encoder = png::Encoder::new(file, gray.w, gray.h);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(gray.as_slice());
            }
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
    fn readonly_test(&self, scene: WizardScene) -> Verdict {
        let config = self.snapshot();
        let Ok(view) = config.try_px_view() else {
            return Verdict::Failed("请先校准".to_owned());
        };
        let mut capturer = match delta_capture::Capturer::new(self.monitor.w, self.monitor.h) {
            Ok(capturer) => capturer,
            Err(error) => return Verdict::Failed(error.to_string()),
        };
        match scene {
            WizardScene::Morse => {
                let Some(region) = view.regions.first() else {
                    return Verdict::Failed("请先校准".to_owned());
                };
                match capturer.grab(region.rect) {
                    Ok(frame) => match delta_flow_morse::decode_gray(&frame.luma()) {
                        Ok(digit) => Verdict::Read(digit.to_string()),
                        Err(error) => Verdict::Failed(error.to_string()),
                    },
                    Err(error) => Verdict::Failed(error.to_string()),
                }
            }
            WizardScene::Console => match capturer.grab(view.console.led_region) {
                Ok(frame) => match delta_vision::read_glyph_string(&frame.luma()) {
                    Ok(text) => Verdict::Read(text),
                    Err(error) => Verdict::Failed(error.to_string()),
                },
                Err(error) => Verdict::Failed(error.to_string()),
            },
            WizardScene::Fingerprint | WizardScene::Recapture => match grab_full(self.monitor) {
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
        self.state.phase_elapsed = self.phase_started.elapsed().as_secs_f64();
        self.drain_events(&ctx);

        if self.wizard.is_some() {
            self.wizard_frame(ui);
            self.schedule_repaint(&ctx);
            return;
        }
        if self.settings_open {
            let action = delta_ui::view::settings_panel(
                ui,
                delta_ui::view::SettingsView {
                    hotkey: &self.state.hotkey,
                    ocr_enabled: self.snapshot().ocr_enabled,
                    config_path: &self.config_path.display().to_string(),
                },
            );
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
                Some(delta_ui::view::SettingsAction::Close) => self.settings_open = false,
                None => {}
            }
            return;
        }

        if let Some(action) = delta_ui::main_window(ui, &mut self.state) {
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


