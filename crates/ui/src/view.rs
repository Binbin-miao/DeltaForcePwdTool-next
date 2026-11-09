//! 绘制层：主窗口（五态 + 收起态）、免责声明、校准向导。
//!
//! 全部用 `Painter` 手绘（像素级对齐 token），不用 egui 默认控件皮肤 —— 这样状态色
//! 只会出现在 2px 描边 / 状态点 / 数值文字上，不会铺成大面积色块。

use delta_geom::PctRect;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::state::{
    read_capture, Action, CaptureBackend, CaptureOutcome, CaptureTestResult, KeyChord, Phase,
    UiState,
};
use crate::theme;
use crate::wizard::{Step, Verdict, Wizard, WizardScene};

/// 免责声明条款（spec D12）。逐行短句，避免长段中文挤压。
pub const DISCLAIMER_LINES: [&str; 4] = [
    "输入注入带系统标记",
    "无法伪装真实按键",
    "仅本机自动化使用",
    "风险自担",
];

/// 主窗口一帧：返回用户动作（无动作为 `None`）。
pub fn main_window(ui: &mut egui::Ui, state: &mut UiState) -> Option<Action> {
    let time = state.phase_elapsed;
    let full = ui.max_rect();
    let painter = ui.painter().clone();
    paint_shell(&painter, full, state.phase, time);
    if state.collapsed {
        collapsed_strip(ui, full, state)
    } else if state.disclaimer_pending {
        disclaimer(ui, full)
    } else {
        main_layout(ui, full, state, time)
    }
}

/// 外壳：外扩 2px 黑 + 1px 内框（egui 无模糊阴影，只能这样模拟）。
fn paint_shell(painter: &egui::Painter, full: Rect, phase: Phase, time: f64) {
    painter.rect_filled(full, theme::RADIUS_WINDOW, theme::SHADOW);
    let body = full.shrink(theme::SHADOW_EXTENT);
    painter.rect_filled(body, theme::RADIUS_WINDOW, theme::BG_PANEL);
    painter.rect_stroke(
        body,
        theme::RADIUS_WINDOW,
        Stroke::new(1.0, theme::LINE_STRONG),
        egui::StrokeKind::Inside,
    );
    let alpha = phase.status_alpha(time);
    if alpha > 0.0 {
        painter.rect_stroke(
            body,
            theme::RADIUS_WINDOW,
            theme::stroke_status(phase.color(), alpha),
            egui::StrokeKind::Inside,
        );
    }
}

/// 空态（供未来复用）：一整块面板底。
fn paint_panel(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, theme::RADIUS_PANEL, theme::BG_RAISED);
    painter.rect_stroke(
        rect,
        theme::RADIUS_PANEL,
        Stroke::new(1.0, theme::LINE_SUBTLE),
        egui::StrokeKind::Inside,
    );
    // 顶/左 1px 高光制造「浮起」。
    painter.line_segment(
        [rect.left_top() + Vec2::splat(1.0), rect.right_top() + Vec2::new(-1.0, 1.0)],
        Stroke::new(1.0, theme::HIGHLIGHT),
    );
}

/// 交互区域：返回是否被点击。
fn clickable(ui: &egui::Ui, rect: Rect, key: &str, sense: Sense) -> egui::Response {
    // `make_persistent_id` 用根 Ui 的 id 加盐，跨帧稳定且天然按控件分桶。
    ui.interact(rect, ui.make_persistent_id(key), sense)
}

/// 主布局（自上而下：标题条 32 / 序列 72 / 三开关 / 主按钮 36 / 进度 / 底部 28）。
fn main_layout(ui: &egui::Ui, full: Rect, state: &mut UiState, time: f64) -> Option<Action> {
    let painter = ui.painter().clone();
    let content = full.shrink2(Vec2::new(theme::SPACE_M, theme::SPACE_S));
    let width = content.width();
    let mut action = None;

    // ── 标题条 ────────────────────────────────────────────────
    let title = Rect::from_min_size(content.min, Vec2::new(width, theme::TITLE_BAR_H));
    let drag = clickable(ui, title, "title-drag", Sense::click_and_drag());
    if drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    status_dot(&painter, title.left_center() + Vec2::new(theme::SPACE_S, 0.0), state.phase, time);
    painter.text(
        title.left_center() + Vec2::new(theme::SPACE_S + theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        state.status_line(),
        theme::text(theme::FONT_TITLE),
        theme::TEXT_1,
    );
    // 图钉（置顶开关）排在 `△` 左边：实心 ■ = 已钉住，空心 □ = 未钉住，
    // 两种状态一眼可分。三个按钮的矩形互不相交，`△`/`×` 的点击区域不被覆盖。
    let pin = Rect::from_min_size(
        Pos2::new(
            title.right() - theme::SPACE_XXL - theme::SPACE_L * 2.0 - theme::SPACE_XS,
            title.top() + theme::SPACE_XS,
        ),
        Vec2::new(theme::SPACE_L, theme::SPACE_L),
    );
    let pin_glyph = if state.always_on_top { "■" } else { "□" };
    if icon_button(ui, pin, "pin", pin_glyph) {
        action = Some(Action::ToggleAlwaysOnTop);
    }
    let collapse = Rect::from_min_size(
        Pos2::new(title.right() - theme::SPACE_XXL - theme::SPACE_L, title.top() + theme::SPACE_XS),
        Vec2::new(theme::SPACE_L, theme::SPACE_L),
    );
    let quit = Rect::from_min_size(
        Pos2::new(title.right() - theme::SPACE_XL, title.top() + theme::SPACE_XS),
        Vec2::new(theme::SPACE_L, theme::SPACE_L),
    );
    if icon_button(ui, collapse, "collapse", "△") {
        action = Some(Action::Collapse);
    }
    if icon_button(ui, quit, "quit", "×") {
        action = Some(Action::Quit);
    }

    // ── 目标序列 ──────────────────────────────────────────────
    let sequence = Rect::from_min_size(
        Pos2::new(content.left(), title.bottom() + theme::SPACE_M),
        Vec2::new(width, theme::SEQUENCE_H),
    );
    paint_panel(&painter, sequence);
    painter.rect_filled(
        sequence,
        theme::RADIUS_PANEL,
        theme::BG_INSET,
    );
    painter.text(
        sequence.center(),
        Align2::CENTER_CENTER,
        state.sequence_text(),
        theme::mono(theme::FONT_VALUE),
        if state.scene.shows_sequence() {
            theme::TEXT_1
        } else {
            theme::TEXT_3
        },
    );

    // ── 三个功能开关 ─────────────────────────────────────────
    let mut top = sequence.bottom() + theme::SPACE_S;
    for (scene, enabled) in state.toggles.entries() {
        let row = Rect::from_min_size(
            Pos2::new(content.left(), top),
            Vec2::new(width, theme::TOGGLE_H),
        );
        let usable = state.is_calibrated() && enabled;
        // 操作台是 BETA：标签直接带标记，让用户一眼知道这块还在自测阶段。
        let label = if matches!(scene, crate::SceneKind::Console) {
            "操作台 · BETA"
        } else {
            scene.label()
        };
        if toggle_row(ui, row, label, usable, &painter) {
            action = Some(Action::ToggleFlow(scene));
        }
        top = row.bottom() + theme::SPACE_XS;
    }

    // ── 主按钮 ───────────────────────────────────────────────
    let primary = Rect::from_min_size(
        Pos2::new(content.left(), top + theme::SPACE_XS),
        Vec2::new(width, theme::PRIMARY_H),
    );
    let label = state.phase.primary_label(&state.hotkey);
    let enabled = state.phase == Phase::Running || state.start_enabled();
    if primary_button(ui, primary, &label, enabled, state.phase) {
        action = Some(match state.phase.primary_action() {
            Action::Start if !state.is_calibrated() => Action::OpenCalibration,
            other => other,
        });
    }

    // ── 进度区 ───────────────────────────────────────────────
    let footer = Rect::from_min_size(
        Pos2::new(content.left(), content.bottom() - theme::FOOTER_H),
        Vec2::new(width, theme::FOOTER_H),
    );
    let progress = Rect::from_min_max(
        Pos2::new(content.left(), primary.bottom() + theme::SPACE_S),
        Pos2::new(content.right(), footer.top() - theme::SPACE_S),
    );
    if let Some(notice) = state.notice_text() {
        if notice_bar(ui, progress, notice) {
            action = Some(Action::DismissNotice);
        }
    } else {
        progress_panel(&painter, progress, state);
    }

    // ── 底部文字链 ───────────────────────────────────────────
    if link(ui, footer.left_center(), "校准向导", &painter) {
        action = Some(Action::OpenCalibration);
    }
    if link(ui, footer.right_center() - Vec2::new(theme::SPACE_XXL, 0.0), "设置", &painter) {
        action = Some(Action::OpenSettings);
    }

    resize_grip(ui, full);
    action
}

/// 进度区：进度文案 + 2px 进度条。
fn progress_panel(painter: &egui::Painter, progress: Rect, state: &UiState) {
    paint_panel(painter, progress);
    let note = if state.missing.is_empty() {
        state.progress.label()
    } else {
        "请先校准".to_owned()
    };
    let detail = if state.message.is_empty() {
        note
    } else {
        format!("{} · {}", note, state.message)
    };
    painter.text(
        progress.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        detail,
        theme::text(theme::FONT_BODY),
        theme::TEXT_2,
    );
    let track = Rect::from_min_size(
        Pos2::new(progress.left() + theme::SPACE_M, progress.bottom() - theme::SPACE_M),
        Vec2::new(progress.width() - theme::SPACE_M * 2.0, 2.0),
    );
    painter.rect_filled(track, theme::RADIUS_BUTTON, theme::BG_INSET);
    if state.progress.ratio() > 0.0 {
        let filled = Rect::from_min_size(track.min, Vec2::new(track.width() * state.progress.ratio(), 2.0));
        painter.rect_filled(filled, theme::RADIUS_BUTTON, theme::ACCENT);
    }
}

/// 一次性警示条：**借用进度区的位置与面板底**（不新增高度，窗口固定 400×420），
/// 描边与文字用 `WARN`（琥珀橙）—— 提醒不是错误，不用红色。
///
/// 返回「知道了」是否被点。
fn notice_bar(ui: &egui::Ui, rect: Rect, notice: &str) -> bool {
    let painter = ui.painter().clone();
    paint_panel(&painter, rect);
    painter.rect_stroke(
        rect,
        theme::RADIUS_PANEL,
        theme::stroke_active(theme::WARN),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        notice,
        theme::text(theme::FONT_BODY),
        theme::WARN,
    );
    let button = notice_button_rect(rect);
    secondary_button(ui, button, "知道了")
}

/// 「知道了」按钮位置：贴右居中，四周留 `SPACE_M` 边距（窄到 64px 高也不越界）。
fn notice_button_rect(rect: Rect) -> Rect {
    Rect::from_min_size(
        Pos2::new(
            rect.right() - theme::SPACE_M - theme::NOTICE_BUTTON_W,
            rect.center().y - theme::NOTICE_BUTTON_H / 2.0,
        ),
        Vec2::new(theme::NOTICE_BUTTON_W, theme::NOTICE_BUTTON_H),
    )
}

/// 收起态：176×36 悬浮条（状态点 + 场景名 + 迷你进度 + 展开箭头）。
fn collapsed_strip(ui: &egui::Ui, full: Rect, state: &mut UiState) -> Option<Action> {
    let painter = ui.painter().clone();
    let time = state.phase_elapsed;
    let strip = full.shrink(theme::SHADOW_EXTENT);
    let mut action = None;
    let drag = clickable(ui, strip, "collapsed-drag", Sense::click_and_drag());
    if drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    status_dot(&painter, strip.left_center() + Vec2::new(theme::SPACE_M, 0.0), state.phase, time);
    painter.text(
        strip.left_center() + Vec2::new(theme::SPACE_XL, 0.0),
        Align2::LEFT_CENTER,
        state.scene.label(),
        theme::text(theme::FONT_BODY),
        theme::TEXT_1,
    );
    let mini = Rect::from_min_size(
        Pos2::new(strip.left() + 104.0, strip.center().y - 1.0),
        Vec2::new(40.0, 2.0),
    );
    painter.rect_filled(mini, theme::RADIUS_BUTTON, theme::BG_INSET);
    if state.progress.ratio() > 0.0 {
        painter.rect_filled(
            Rect::from_min_size(mini.min, Vec2::new(mini.width() * state.progress.ratio(), 2.0)),
            theme::RADIUS_BUTTON,
            theme::ACCENT,
        );
    }
    let expand = Rect::from_min_size(
        Pos2::new(strip.right() - theme::SPACE_XL, strip.center().y - theme::SPACE_S),
        Vec2::splat(theme::SPACE_L),
    );
    if icon_button(ui, expand, "expand", "▽") {
        action = Some(Action::Expand);
    }
    action
}

/// 免责声明（D12）：未确认时占满中央。
fn disclaimer(ui: &egui::Ui, full: Rect) -> Option<Action> {
    let painter = ui.painter().clone();
    let body = full.shrink(theme::SPACE_XL);
    paint_panel(&painter, body);
    painter.text(
        Pos2::new(body.center().x, body.top() + theme::SPACE_L),
        Align2::CENTER_CENTER,
        "使用前确认",
        theme::text(theme::FONT_TITLE),
        theme::TEXT_1,
    );
    for (index, line) in DISCLAIMER_LINES.iter().enumerate() {
        painter.text(
            Pos2::new(
                body.center().x,
                body.top() + theme::SPACE_XL + theme::SPACE_L * (index as f32 + 1.0),
            ),
            Align2::CENTER_CENTER,
            *line,
            theme::text(theme::FONT_BODY),
            theme::TEXT_2,
        );
    }
    let agree = Rect::from_min_size(
        Pos2::new(body.left() + theme::SPACE_XL, body.bottom() - theme::SPACE_XXL - theme::PRIMARY_H),
        Vec2::new(body.width() - theme::SPACE_XXL * 2.0, theme::PRIMARY_H),
    );
    primary_button(ui, agree, "对，继续", true, Phase::Idle).then_some(Action::AcceptDisclaimer)
}

/// 状态点：颜色即状态色，尺寸固定，不呼吸放大（只呼吸 alpha）。
fn status_dot(painter: &egui::Painter, center: Pos2, phase: Phase, time: f64) {
    let alpha = if phase == Phase::Idle {
        1.0
    } else {
        phase.status_alpha(time).max(0.25)
    };
    painter.circle_filled(
        center,
        theme::DOT_RADIUS,
        with_alpha(phase.color(), alpha),
    );
}

/// 带 alpha 的颜色。
fn with_alpha(color: Color32, alpha: f32) -> Color32 {
    let [r, g, b, _] = color.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (255.0 * alpha.clamp(0.0, 1.0)) as u8)
}

/// 小图标按钮（收起 / 退出 / 展开）。
fn icon_button(ui: &egui::Ui, rect: Rect, key: &str, glyph: &str) -> bool {
    let response = clickable(ui, rect, key, Sense::click());
    let painter = ui.painter();
    let color = if response.hovered() {
        theme::ACCENT
    } else {
        theme::TEXT_2
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        theme::text(theme::FONT_BODY),
        color,
    );
    response.clicked()
}

/// 文字链（底部「校准向导 / 设置」）。
fn link(ui: &egui::Ui, anchor: Pos2, label: &str, painter: &egui::Painter) -> bool {
    let width = label.chars().count() as f32 * theme::FONT_LABEL + theme::SPACE_S;
    let rect = Rect::from_center_size(anchor + Vec2::new(width / 2.0, 0.0), Vec2::new(width, theme::FOOTER_H));
    let response = clickable(ui, rect, label, Sense::click());
    let color = if response.hovered() {
        theme::ACCENT
    } else {
        theme::TEXT_3
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme::text(theme::FONT_LABEL),
        color,
    );
    response.clicked()
}

/// 功能开关行：左侧标签，右侧开关点。
fn toggle_row(ui: &egui::Ui, rect: Rect, label: &str, on: bool, painter: &egui::Painter) -> bool {
    let response = clickable(ui, rect, label, Sense::click());
    let hovered = response.hovered();
    painter.rect_filled(
        rect,
        theme::RADIUS_BUTTON,
        if hovered { theme::BG_RAISED } else { theme::BG_PANEL },
    );
    painter.rect_stroke(
        rect,
        theme::RADIUS_BUTTON,
        Stroke::new(1.0, theme::LINE_SUBTLE),
        egui::StrokeKind::Inside,
    );
    let text_color = if on { theme::TEXT_1 } else { theme::TEXT_3 };
    painter.text(
        rect.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        label,
        theme::text(theme::FONT_BODY),
        text_color,
    );
    let switch = Pos2::new(rect.right() - theme::SPACE_L, rect.center().y);
    painter.circle_stroke(switch, theme::DOT_RADIUS, Stroke::new(1.0, theme::LINE_STRONG));
    if on {
        painter.circle_filled(switch, theme::DOT_RADIUS, theme::ACCENT);
    }
    painter.text(
        Pos2::new(switch.x - theme::SPACE_M, switch.y),
        Align2::RIGHT_CENTER,
        if on { "开" } else { "关" },
        theme::text(theme::FONT_LABEL),
        text_color,
    );
    response.clicked()
}

/// 主按钮：满宽 36，运行中变「取消操作」。
fn primary_button(ui: &egui::Ui, rect: Rect, label: &str, enabled: bool, phase: Phase) -> bool {
    let response = clickable(ui, rect, label, Sense::click());
    if !enabled {
        let painter = ui.painter();
        painter.rect_filled(rect, theme::RADIUS_BUTTON, theme::BG_PANEL);
        painter.rect_stroke(
            rect,
            theme::RADIUS_BUTTON,
            Stroke::new(1.0, theme::LINE_SUBTLE),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            theme::text(theme::FONT_BODY),
            theme::TEXT_3,
        );
        return false;
    }
    let painter = ui.painter();
    let hovered = response.hovered();
    painter.rect_filled(
        rect,
        theme::RADIUS_BUTTON,
        if hovered { theme::BG_RAISED } else { theme::BG_PANEL },
    );
    painter.rect_stroke(
        rect,
        theme::RADIUS_BUTTON,
        theme::stroke_active(if hovered { phase.color() } else { theme::ACCENT }),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme::text(theme::FONT_BODY),
        theme::TEXT_1,
    );
    response.clicked()
}

/// 右下角 12×12 抓边：按住即进入系统缩放。
fn resize_grip(ui: &egui::Ui, full: Rect) {
    let grip = Rect::from_min_size(
        Pos2::new(
            full.right() - theme::RESIZE_GRIP - theme::SHADOW_EXTENT,
            full.bottom() - theme::RESIZE_GRIP - theme::SHADOW_EXTENT,
        ),
        Vec2::splat(theme::RESIZE_GRIP),
    );
    let response = clickable(ui, grip, "resize-grip", Sense::drag());
    let painter = ui.painter();
    for step in 0..3 {
        let offset = theme::SPACE_XS * (step as f32 + 1.0);
        painter.line_segment(
            [
                Pos2::new(grip.right() - offset, grip.bottom() - 1.0),
                Pos2::new(grip.right() - 1.0, grip.bottom() - offset),
            ],
            Stroke::new(1.0, theme::LINE_STRONG),
        );
    }
    if response.drag_started() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::BeginResize(
                egui::viewport::ResizeDirection::SouthEast,
            ));
    }
}

/// 向导一帧的外部素材（预览纹理由 `delta-app` 截图后上传）。
pub struct WizardFrame<'a> {
    pub preview: Option<&'a egui::TextureHandle>,
    /// 主显示器物理分辨率（显示用）。
    pub monitor: [u32; 2],
    /// 已存区域的百分比框（画琥珀虚线）。
    pub existing: &'a [(String, PctRect)],
}

/// 向导动作。
#[derive(Debug, Clone, PartialEq)]
pub enum WizardAction {
    /// 拖框完成（百分比坐标，坐标为预览图内相对位置）。
    Drawn(PctRect),
    Primary,
    Redo,
    Back,
    Close,
}

/// 校准向导一帧。
pub fn wizard_window(
    ui: &mut egui::Ui,
    wizard: &mut Wizard,
    frame: WizardFrame<'_>,
) -> Option<WizardAction> {
    let mut action = None;
    if ui.ctx().input(|input| input.key_pressed(egui::Key::Escape)) {
        action = Some(WizardAction::Back);
    }
    {
        {
            let full = ui.max_rect();
            let painter = ui.painter().clone();
            let content = full.shrink2(Vec2::new(theme::SPACE_M, theme::SPACE_S));
            paint_shell(&painter, full, Phase::Idle, 0.0);

            // 步序标题：`3/5 排错复看`
            painter.text(
                content.left_top(),
                Align2::LEFT_TOP,
                format!("{}/5 {}", wizard.step.index(), wizard.step.title()),
                theme::text(theme::FONT_TITLE),
                theme::TEXT_1,
            );
            painter.text(
                content.right_top(),
                Align2::RIGHT_TOP,
                format!("{}×{}", frame.monitor[0], frame.monitor[1]),
                theme::text(theme::FONT_LABEL),
                theme::TEXT_3,
            );

            let stage = Rect::from_min_max(
                Pos2::new(content.left(), content.top() + theme::SPACE_XXL),
                Pos2::new(content.right(), content.bottom() - theme::PRIMARY_H - theme::SPACE_XL),
            );
            match wizard.step {
                Step::PickScene => paint_scene_cards(ui, stage, wizard),
                Step::SelectRegion => {
                    if let Some(drawn) = paint_region_stage(ui, stage, wizard, &frame) {
                        action = Some(WizardAction::Drawn(drawn));
                    }
                }
                Step::Review => paint_review(ui, stage, wizard, &frame),
                Step::TestRun => paint_test_run(ui, stage, wizard),
                Step::Save => paint_saved(ui, stage, wizard),
            }

            // 补录模板：复看步顺带填角色名（只此一处用 egui 原生输入框）。
            if wizard.step == Step::Review && wizard.scene == Some(WizardScene::Recapture) {
                let field = Rect::from_min_size(
                    Pos2::new(content.left(), content.top() + theme::SPACE_XXL),
                    Vec2::new(content.width(), theme::PRIMARY_H),
                );
                painter.text(
                    Pos2::new(field.left(), field.top() - theme::SPACE_XS),
                    Align2::LEFT_BOTTOM,
                    "填角色名",
                    theme::text(theme::FONT_LABEL),
                    theme::TEXT_2,
                );
                let mut buffer = wizard.role_name.clone();
                let response = ui.put(
                    field,
                    egui::TextEdit::singleline(&mut buffer).hint_text("角色名"),
                );
                if response.changed() {
                    wizard.role_name = buffer;
                }
            }

            // 底部按钮行
            let row = Rect::from_min_size(
                Pos2::new(content.left(), content.bottom() - theme::PRIMARY_H),
                Vec2::new(content.width(), theme::PRIMARY_H),
            );
            if wizard.step.has_redo() {
                let redo = Rect::from_min_size(row.min, Vec2::new(row.width() * 0.4, row.height()));
                if secondary_button(ui, redo, "重画一次") {
                    action = Some(WizardAction::Redo);
                }
                let primary = Rect::from_min_size(
                    Pos2::new(redo.right() + theme::SPACE_S, row.top()),
                    Vec2::new(row.width() - redo.width() - theme::SPACE_S, row.height()),
                );
                let enabled = wizard.can_advance();
                if primary_button(ui, primary, wizard.step.primary(), enabled, Phase::Running) {
                    action = Some(WizardAction::Primary);
                }
            } else {
                let enabled = wizard.can_advance();
                if primary_button(ui, row, wizard.step.primary(), enabled, Phase::Running) {
                    action = Some(WizardAction::Primary);
                }
            }
        }
    }
    action
}

/// 第一步：三张场景卡片 + 补录入口。
fn paint_scene_cards(ui: &egui::Ui, stage: Rect, wizard: &mut Wizard) {
    let painter = ui.painter().clone();
    let card_h = (stage.height() - theme::SPACE_S * 3.0) / 4.0;
    for (index, scene) in WizardScene::ALL.iter().enumerate() {
        let card = Rect::from_min_size(
            Pos2::new(stage.left(), stage.top() + (card_h + theme::SPACE_S) * index as f32),
            Vec2::new(stage.width(), card_h),
        );
        let response = clickable(ui, card, scene.label(), Sense::click());
        paint_panel(&painter, card);
        painter.text(
            Pos2::new(card.left() + theme::SPACE_M, card.center().y - theme::SPACE_XS),
            Align2::LEFT_CENTER,
            scene.label(),
            theme::text(theme::FONT_BODY),
            theme::TEXT_1,
        );
        painter.text(
            Pos2::new(card.left() + theme::SPACE_M, card.center().y + theme::SPACE_S),
            Align2::LEFT_CENTER,
            scene.hint(),
            theme::text(theme::FONT_LABEL),
            theme::TEXT_3,
        );
        if response.hovered() {
            painter.rect_stroke(
                card,
                theme::RADIUS_PANEL,
                theme::stroke_active(theme::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
        if response.clicked() {
            wizard.choose(*scene);
        }
    }
}

/// 第二步：拖一次即存（松手生效，无确认按钮），已存区域画琥珀虚线框。
fn paint_region_stage(
    ui: &egui::Ui,
    stage: Rect,
    wizard: &mut Wizard,
    frame: &WizardFrame<'_>,
) -> Option<PctRect> {
    let painter = ui.painter().clone();
    painter.rect_filled(stage, theme::RADIUS_PANEL, theme::BG_INSET);
    painter.rect_stroke(
        stage,
        theme::RADIUS_PANEL,
        Stroke::new(1.0, theme::LINE_SUBTLE),
        egui::StrokeKind::Inside,
    );
    if let Some(texture) = frame.preview {
        painter.image(
            texture.id(),
            stage,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    for (_, rect) in frame.existing {
        dashed_pct_rect(&painter, stage, *rect, theme::ACCENT);
    }
    if let Some(spec) = wizard.current() {
        painter.text(
            Pos2::new(stage.left(), stage.top() + theme::SPACE_XS),
            Align2::LEFT_TOP,
            format!(
                "{} {}/{}",
                spec.label,
                wizard.cursor + 1,
                wizard.targets().len()
            ),
            theme::text(theme::FONT_LABEL),
            theme::TEXT_1,
        );
    }
    let response = clickable(ui, stage, "region-drag", Sense::drag());
    if !response.drag_stopped() {
        return None;
    }
    let from = ui.input(|input| input.pointer.press_origin())?;
    let to = response.interact_pointer_pos()?;
    let drawn = Rect::from_two_pos(from, to).intersect(stage);
    if drawn.width() < theme::SPACE_S || drawn.height() < theme::SPACE_S {
        return None;
    }
    Some(pct_rect_from_screen(stage, drawn))
}

/// 第三步：回显预览 + 结果文案（「重画一次」/「对，继续」在底部）。
fn paint_review(ui: &egui::Ui, stage: Rect, wizard: &Wizard, frame: &WizardFrame<'_>) {
    let painter = ui.painter().clone();
    painter.rect_filled(stage, theme::RADIUS_PANEL, theme::BG_INSET);
    if let Some(texture) = frame.preview {
        let target = stage.shrink(theme::SPACE_S);
        painter.image(
            texture.id(),
            target,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        painter.text(
            stage.center(),
            Align2::CENTER_CENTER,
            "暂无回显",
            theme::text(theme::FONT_BODY),
            theme::TEXT_3,
        );
    }
    painter.text(
        Pos2::new(stage.left(), stage.bottom() - theme::SPACE_M),
        Align2::LEFT_BOTTOM,
        format!("已框 {} 处", wizard.drawn_count()),
        theme::text(theme::FONT_LABEL),
        theme::TEXT_2,
    );
    let _ = ui;
}

/// 第四步：只读识别结果（不产生任何输入）。
fn paint_test_run(ui: &egui::Ui, stage: Rect, wizard: &Wizard) {
    let painter = ui.painter().clone();
    paint_panel(&painter, stage);
    let (text, color) = match &wizard.verdict {
        Verdict::Pending => ("还没有试跑".to_owned(), theme::TEXT_3),
        Verdict::Read(value) => (format!("读到 {value}"), theme::OK),
        Verdict::Failed(reason) => (format!("没读到 · {reason}"), theme::WARN),
    };
    painter.text(
        stage.center(),
        Align2::CENTER_CENTER,
        text,
        theme::mono(theme::FONT_TITLE),
        color,
    );
    painter.text(
        Pos2::new(stage.center().x, stage.bottom() - theme::SPACE_M),
        Align2::CENTER_BOTTOM,
        "试跑不产生任何输入",
        theme::text(theme::FONT_LABEL),
        theme::TEXT_3,
    );
    let _ = ui;
}

/// 第五步：保存提示。
fn paint_saved(ui: &egui::Ui, stage: Rect, wizard: &Wizard) {
    let painter = ui.painter().clone();
    paint_panel(&painter, stage);
    painter.text(
        stage.center(),
        Align2::CENTER_CENTER,
        "已保存 · 换分辨率也能用",
        theme::text(theme::FONT_BODY),
        theme::OK,
    );
    let _ = (ui, wizard);
}

/// 次要按钮（描边款）。
fn secondary_button(ui: &egui::Ui, rect: Rect, label: &str) -> bool {
    let response = clickable(ui, rect, label, Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, theme::RADIUS_BUTTON, theme::BG_PANEL);
    painter.rect_stroke(
        rect,
        theme::RADIUS_BUTTON,
        theme::stroke_normal(),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme::text(theme::FONT_BODY),
        if response.hovered() {
            theme::TEXT_1
        } else {
            theme::TEXT_2
        },
    );
    response.clicked()
}

/// 屏幕坐标 → 百分比（相对预览区域，clamp 到 [0,1]）。
pub fn pct_rect_from_screen(stage: Rect, drawn: Rect) -> PctRect {
    let width = stage.width().max(1.0);
    let height = stage.height().max(1.0);
    let x = ((drawn.left() - stage.left()) / width).clamp(0.0, 1.0);
    let y = ((drawn.top() - stage.top()) / height).clamp(0.0, 1.0);
    let w = (drawn.width() / width).clamp(0.0, 1.0 - x);
    let h = (drawn.height() / height).clamp(0.0, 1.0 - y);
    PctRect { x, y, w, h }
}

/// 百分比矩形 → 屏幕矩形。
pub fn screen_rect_from_pct(stage: Rect, rect: PctRect) -> Rect {
    Rect::from_min_size(
        Pos2::new(
            stage.left() + stage.width() * rect.x,
            stage.top() + stage.height() * rect.y,
        ),
        Vec2::new(stage.width() * rect.w, stage.height() * rect.h),
    )
}

/// 琥珀虚线框（校准里「已存区域」的标记）。
fn dashed_pct_rect(painter: &egui::Painter, stage: Rect, rect: PctRect, color: Color32) {
    if rect.is_empty() {
        return;
    }
    let screen = screen_rect_from_pct(stage, rect);
    let stroke = Stroke::new(1.0, color);
    let dash = theme::SPACE_XS;
    let mut x = screen.left();
    while x < screen.right() {
        let end = (x + dash).min(screen.right());
        painter.line_segment([Pos2::new(x, screen.top()), Pos2::new(end, screen.top())], stroke);
        painter.line_segment(
            [Pos2::new(x, screen.bottom()), Pos2::new(end, screen.bottom())],
            stroke,
        );
        x += dash * 2.0;
    }
    let mut y = screen.top();
    while y < screen.bottom() {
        let end = (y + dash).min(screen.bottom());
        painter.line_segment([Pos2::new(screen.left(), y), Pos2::new(screen.left(), end)], stroke);
        painter.line_segment(
            [Pos2::new(screen.right(), y), Pos2::new(screen.right(), end)],
            stroke,
        );
        y += dash * 2.0;
    }
}


/// 截图后端的两个选项（spec D1：只两档，默认 WGC；顺序即下拉顺序）。
const CAPTURE_BACKENDS: [CaptureBackend; 2] = [CaptureBackend::Wgc, CaptureBackend::BitBlt];
/// 带副标题的设置行高度（标题 + 灰色小字说明，spec D6）。
const SETTINGS_ROW_H: f32 = 40.0;
/// 测试结果区高度（缩略图 + 一行「宽×高 · 耗时ms」）。
const CAPTURE_RESULT_H: f32 = 90.0;
/// 缩略图上限（spec D5 建议 ≤160×90；这里让出一行文字位，取 160×64）。
const CAPTURE_THUMB_MAX: [f32; 2] = [160.0, 64.0];
/// 右侧值槽宽度（要放下 `Windows Graphics Capture ▼`）。
const BACKEND_VALUE_W: f32 = 168.0;
/// 下拉面板宽度（要放下「兼容模式 · 可能截不到独占全屏」）。
const BACKEND_MENU_W: f32 = 220.0;
/// 行内小按钮宽度（`抓一帧`）。
const ROW_BUTTON_W: f32 = 72.0;

/// 下拉里每一项的文案。
///
/// 兼容档必须自曝风险（spec D1 显式边界：UI 上标注「可能截不到独占全屏」），
/// 因此它不叫「BitBlt」而叫「兼容模式」。
fn backend_label(backend: CaptureBackend) -> &'static str {
    match backend {
        CaptureBackend::Wgc => "Windows Graphics Capture",
        CaptureBackend::BitBlt => "兼容模式 · 可能截不到独占全屏",
    }
}

/// 设置面板的行骨架（坐标相对窗口，已扣除面板外边距）。
///
/// 高度预算（默认窗口 400×420 → 内容区 376×404）：
/// 标题 32 + 热键 28 + 8 + 兜底 28 + 8 + 截图方式 40 + 8 + 测试截图 40 + 8 + 结果区 90。
/// 结果区与配置路径**互斥**（spec D6 允许测试结果临时替换配置文件路径区域），
/// 因此最小窗口 340×360 下两者也不会同时出现、更不会挤压底部按钮。
#[derive(Debug, Clone, Copy)]
struct SettingsRows {
    hotkey: Rect,
    ocr: Rect,
    backend: Rect,
    test: Rect,
    /// 结果区（缩略图 / 失败原因）。
    result: Rect,
    /// 配置路径区；有测试结果时为 `None`（与结果区二选一）。
    config: Option<Rect>,
    footer: Rect,
}

fn settings_rows(content: Rect, has_result: bool) -> SettingsRows {
    let row = |top: f32, height: f32| {
        Rect::from_min_size(Pos2::new(content.left(), top), Vec2::new(content.width(), height))
    };
    let top = content.top();
    let hotkey = row(top + theme::SPACE_XXL, theme::TOGGLE_H);
    let ocr = row(hotkey.bottom() + theme::SPACE_S, theme::TOGGLE_H);
    let backend = row(ocr.bottom() + theme::SPACE_S, SETTINGS_ROW_H);
    let test = row(backend.bottom() + theme::SPACE_S, SETTINGS_ROW_H);
    let extra_top = test.bottom() + theme::SPACE_S;
    SettingsRows {
        hotkey,
        ocr,
        backend,
        test,
        result: row(extra_top, CAPTURE_RESULT_H),
        config: (!has_result).then(|| row(extra_top, SETTINGS_ROW_H)),
        footer: Rect::from_min_size(
            Pos2::new(content.left(), content.bottom() - theme::PRIMARY_H),
            Vec2::new(content.width(), theme::PRIMARY_H),
        ),
    }
}

/// 行内右侧控件槽：固定高度、右对齐，与左侧标题一个水平带。
fn row_slot(rect: Rect, width: f32) -> Rect {
    Rect::from_min_size(
        Pos2::new(
            rect.right() - theme::SPACE_M - width,
            rect.center().y - theme::TOGGLE_H / 2.0,
        ),
        Vec2::new(width, theme::TOGGLE_H),
    )
}

/// 一行设置：左侧标题 + 下方灰色小字说明 + 行底 1px 分隔（spec D6 的行式结构）。
fn paint_settings_row(painter: &egui::Painter, rect: Rect, title: &str, hint: &str, dim: bool) {
    let title_color = if dim { theme::TEXT_3 } else { theme::TEXT_1 };
    painter.text(
        Pos2::new(rect.left() + theme::SPACE_M, rect.top() + theme::SPACE_XS),
        Align2::LEFT_TOP,
        title,
        theme::text(theme::FONT_BODY),
        title_color,
    );
    painter.text(
        Pos2::new(
            rect.left() + theme::SPACE_M,
            rect.top() + theme::SPACE_XS + theme::FONT_TITLE,
        ),
        Align2::LEFT_TOP,
        hint,
        theme::text(theme::FONT_LABEL),
        theme::TEXT_3,
    );
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, theme::LINE_SUBTLE),
    );
}

/// 行内小按钮：`enabled == false` 时置灰且不响应点击（运行中禁止切换 / 抓帧）。
fn row_button(ui: &egui::Ui, rect: Rect, label: &str, enabled: bool) -> bool {
    if enabled {
        return secondary_button(ui, rect, label);
    }
    let painter = ui.painter();
    painter.rect_filled(rect, theme::RADIUS_BUTTON, theme::BG_PANEL);
    painter.rect_stroke(
        rect,
        theme::RADIUS_BUTTON,
        Stroke::new(1.0, theme::LINE_SUBTLE),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme::text(theme::FONT_LABEL),
        theme::TEXT_3,
    );
    false
}

/// 按源图长宽比把图塞进给定框内（居中，不拉伸）。
fn fit_inside(area: Rect, width: u32, height: u32) -> Rect {
    let source = Vec2::new(width.max(1) as f32, height.max(1) as f32);
    let scale = (area.width() / source.x).min(area.height() / source.y);
    Rect::from_center_size(area.center(), source * scale)
}

/// 设置浮层素材（热键 / 文字兜底 / 截图方式 / 测试截图 / 配置路径）。
pub struct SettingsView<'a> {
    pub hotkey: &'a str,
    /// 是否正在捕获新热键（捕获态只显示提示，没有任何文本输入框）。
    pub hotkey_editing: bool,
    /// 热键冲突提示文案（`None` = 不显示）。
    pub hotkey_conflict: Option<&'a str>,
    pub ocr_enabled: bool,
    pub config_path: &'a str,
    /// 当前截图后端（spec D1）。
    pub capture_backend: CaptureBackend,
    /// 运行中 → 截图方式与测试截图都置灰（spec D1 显式边界）。
    pub capture_locked: bool,
    /// 最近一次测试截图的结果（spec D5：缩略图 + 宽×高 · 耗时）。
    pub capture_test: Option<&'a CaptureTestResult>,
    /// 最近一次测试截图的失败原因（与结果互斥，失败显示红色文案）。
    pub capture_test_error: Option<&'a str>,
}

/// 设置浮层动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    /// 只读检查游戏与场景，不产生任何输入。
    Probe,
    ToggleOcr,
    /// 点热键行 → 进入捕获态。
    EditHotkey,
    /// 捕获成功 → 提交这一组键（调用方格式化后写配置并热更新）。
    CommitHotkey(KeyChord),
    /// 捕获态下 Esc / 点热键行外 → 只退出捕获，不关面板（D4）。
    CancelHotkey,
    /// 切换截图后端（写配置 + 下一次抓帧立即生效，spec D1）。
    SetCaptureBackend(CaptureBackend),
    /// 点「抓一帧」：调用方在后台线程建临时会话抓一帧（spec D5）。
    TestCapture,
    Close,
}

/// 捕获态的一帧结算：可捕获键提交、Esc 或点行外取消、其余继续等。
fn resolve_capture(ui: &egui::Ui, editing: bool, hotkey_row: Rect) -> Option<SettingsAction> {
    if !editing {
        return None;
    }
    match read_capture(&ui.input(|input| input.events.clone())) {
        CaptureOutcome::Commit(chord) => return Some(SettingsAction::CommitHotkey(chord)),
        CaptureOutcome::Cancel => return Some(SettingsAction::CancelHotkey),
        CaptureOutcome::Pending => {}
    }
    let clicked_outside = ui.input(|input| {
        input.pointer.any_click()
            && input
                .pointer
                .interact_pos()
                .is_some_and(|position| !hotkey_row.contains(position))
    });
    if clicked_outside {
        return Some(SettingsAction::CancelHotkey);
    }
    None
}

/// 设置浮层（覆在主界面上，不新开窗口）。
pub fn settings_panel(ui: &mut egui::Ui, view: SettingsView<'_>) -> Option<SettingsAction> {
    let mut action = None;
    // D4：捕获态下 Esc 只取消捕获 —— 所以「Esc 关面板」必须先排除捕获态。
    if !view.hotkey_editing && ui.ctx().input(|input| input.key_pressed(egui::Key::Escape)) {
        action = Some(SettingsAction::Close);
    }
    let full = ui.max_rect();
    let painter = ui.painter().clone();
    paint_shell(&painter, full, Phase::Idle, 0.0);
    let content = full.shrink2(Vec2::new(theme::SPACE_M, theme::SPACE_S));
    painter.text(
        content.left_top(),
        Align2::LEFT_TOP,
        "设置",
        theme::text(theme::FONT_TITLE),
        theme::TEXT_1,
    );

    let has_result = view.capture_test.is_some() || view.capture_test_error.is_some();
    let rows = settings_rows(content, has_result);

    // ── 全局热键（点一下即可捕获，捕获态没有任何文本输入框）──────────
    paint_panel(&painter, rows.hotkey);
    painter.text(
        rows.hotkey.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        "全局热键",
        theme::text(theme::FONT_BODY),
        theme::TEXT_2,
    );
    // 捕获结算先于既有控件：这一帧的按键/取消不会被按钮点击覆盖掉。
    if action.is_none() {
        action = resolve_capture(ui, view.hotkey_editing, rows.hotkey);
    }
    if view.hotkey_editing {
        // 捕获态：只提示按键，绝不提供文本输入（spec D3 删自由输入）。
        painter.text(
            rows.hotkey.left_center() + Vec2::new(content.width() * 0.45, 0.0),
            Align2::LEFT_CENTER,
            "请按下新热键…",
            theme::text(theme::FONT_BODY),
            theme::ACCENT,
        );
        painter.text(
            rows.hotkey.right_center() - Vec2::new(theme::SPACE_M, 0.0),
            Align2::RIGHT_CENTER,
            "Esc 取消",
            theme::text(theme::FONT_LABEL),
            theme::TEXT_3,
        );
    } else {
        let response = ui.interact(
            rows.hotkey,
            ui.scope_id().with("hotkey-row"),
            egui::Sense::click(),
        );
        // 冲突态：右侧这一格换成红色冲突文案（文案里已含「仍为 F8」，旧键信息不丢）。
        // 字号用 `FONT_LABEL`：最小宽度 340 下长组合键不会压到左侧「全局热键」。
        let conflict = view.hotkey_conflict;
        let (label, font, color) = match (conflict, response.hovered()) {
            (Some(text), _) => (text.to_owned(), theme::text(theme::FONT_LABEL), theme::ERR),
            (None, true) => (
                format!("{} · 点这里改", view.hotkey),
                theme::mono(theme::FONT_BODY),
                theme::ACCENT,
            ),
            (None, false) => (
                view.hotkey.to_owned(),
                theme::mono(theme::FONT_BODY),
                theme::TEXT_1,
            ),
        };
        painter.text(
            rows.hotkey.right_center() - Vec2::new(theme::SPACE_M, 0.0),
            Align2::RIGHT_CENTER,
            label,
            font,
            color,
        );
        if response.clicked() {
            action = Some(SettingsAction::EditHotkey);
        }
    }

    // ── 文字兜底开关 ──────────────────────────────────────────
    if toggle_row(ui, rows.ocr, "文字兜底", view.ocr_enabled, &painter) {
        action = Some(SettingsAction::ToggleOcr);
    }

    // ── 截图方式（spec D1 / D6：点开自绘下拉，不引入 ComboBox 的浅色默认外观）──
    let locked = view.capture_locked;
    paint_settings_row(
        &painter,
        rows.backend,
        "截图方式",
        if locked {
            "识别中不可切换"
        } else {
            "切换后下一次抓帧立即生效"
        },
        locked,
    );
    let value_slot = row_slot(rows.backend, BACKEND_VALUE_W);
    let menu_id = ui.make_persistent_id("capture-backend-menu");
    let mut menu_open =
        !locked && ui.ctx().data(|data| data.get_temp::<bool>(menu_id).unwrap_or(false));
    let value_text = format!(
        "{}  {}",
        backend_label(view.capture_backend),
        if menu_open { '▲' } else { '▼' }
    );
    let value_clicked = if locked {
        painter.text(
            value_slot.right_center(),
            Align2::RIGHT_CENTER,
            value_text,
            theme::text(theme::FONT_BODY),
            theme::TEXT_3,
        );
        false
    } else {
        let response = clickable(ui, value_slot, "capture-backend-value", Sense::click());
        painter.text(
            value_slot.right_center(),
            Align2::RIGHT_CENTER,
            value_text,
            theme::text(theme::FONT_BODY),
            if response.hovered() {
                theme::ACCENT
            } else {
                theme::TEXT_1
            },
        );
        response.clicked()
    };
    if value_clicked {
        menu_open = !menu_open;
    }

    // ── 测试截图（spec D5：只在点击时抓一帧，不点不抓）──────────────
    paint_settings_row(
        &painter,
        rows.test,
        "测试截图",
        "抓一帧验证当前后端能出图",
        locked,
    );
    let test_button = row_slot(rows.test, ROW_BUTTON_W);
    if row_button(ui, test_button, "抓一帧", !locked) {
        action = Some(SettingsAction::TestCapture);
    }

    // ── 结果区：缩略图 + `宽×高 · 耗时ms`，或红色失败原因 ────────────
    if let Some(result) = view.capture_test {
        let thumb = fit_inside(
            Rect::from_center_size(
                Pos2::new(rows.result.center().x, rows.result.center().y - theme::SPACE_S),
                Vec2::new(CAPTURE_THUMB_MAX[0], CAPTURE_THUMB_MAX[1]),
            ),
            result.width,
            result.height,
        );
        painter.image(
            result.thumbnail.id(),
            thumb,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        painter.rect_stroke(
            thumb,
            theme::RADIUS_BUTTON,
            Stroke::new(1.0, theme::LINE_SUBTLE),
            egui::StrokeKind::Inside,
        );
        painter.text(
            Pos2::new(rows.result.center().x, rows.result.bottom() - theme::SPACE_XS),
            Align2::CENTER_BOTTOM,
            result.summary(),
            theme::mono(theme::FONT_LABEL),
            theme::TEXT_2,
        );
    } else if let Some(reason) = view.capture_test_error {
        // 错误码在被截断时最容易被丢掉，所以交给 egui 自动换行，绝不手写单行文本。
        let mut error_ui = ui.new_child(egui::UiBuilder::new().max_rect(rows.result));
        error_ui.add(
            egui::Label::new(
                egui::RichText::new(reason)
                    .size(theme::FONT_LABEL)
                    .color(theme::ERR),
            )
            .wrap(),
        );
    }

    // ── 配置路径（有测试结果时让位，spec D6 允许的替换）──────────────
    if let Some(config) = rows.config {
        painter.text(
            config.left_top(),
            Align2::LEFT_TOP,
            "配置文件",
            theme::text(theme::FONT_LABEL),
            theme::TEXT_3,
        );
        painter.text(
            Pos2::new(config.left(), config.top() + theme::SPACE_L),
            Align2::LEFT_TOP,
            view.config_path,
            theme::mono(theme::FONT_LABEL),
            theme::TEXT_3,
        );
    }

    // ── 底部：检查游戏 / 返回 ─────────────────────────────────
    let footer = rows.footer;
    let probe = Rect::from_min_size(footer.min, Vec2::new(footer.width() * 0.48, footer.height()));
    if secondary_button(ui, probe, "检查游戏") {
        action = Some(SettingsAction::Probe);
    }
    let back = Rect::from_min_size(
        Pos2::new(probe.right() + theme::SPACE_S, footer.top()),
        Vec2::new(footer.width() - probe.width() - theme::SPACE_S, footer.height()),
    );
    if primary_button(ui, back, "返回", true, Phase::Idle) {
        action = Some(SettingsAction::Close);
    }

    // ── 下拉最后画：压在其它行之上，点击优先级也高于下面的按钮 ──────────
    let mut option_clicked = false;
    if menu_open {
        let item_height = theme::TOGGLE_H;
        let menu = Rect::from_min_size(
            Pos2::new(
                value_slot.right() - BACKEND_MENU_W,
                rows.backend.bottom() - theme::SPACE_XS,
            ),
            Vec2::new(
                BACKEND_MENU_W,
                item_height * CAPTURE_BACKENDS.len() as f32 + theme::SPACE_S,
            ),
        );
        paint_panel(&painter, menu);
        for (index, backend) in CAPTURE_BACKENDS.into_iter().enumerate() {
            let item = Rect::from_min_size(
                Pos2::new(
                    menu.left() + 1.0,
                    menu.top() + theme::SPACE_XS / 2.0 + item_height * index as f32,
                ),
                Vec2::new(menu.width() - 2.0, item_height),
            );
            let key = format!("capture-backend-option-{index}");
            let response = clickable(ui, item, &key, Sense::click());
            let selected = backend == view.capture_backend;
            let color = match (selected, response.hovered()) {
                (true, _) => theme::ACCENT,
                (false, true) => theme::TEXT_1,
                (false, false) => theme::TEXT_2,
            };
            painter.text(
                item.left_center() + Vec2::new(theme::SPACE_S, 0.0),
                Align2::LEFT_CENTER,
                backend_label(backend),
                theme::text(theme::FONT_LABEL),
                color,
            );
            if response.clicked() {
                option_clicked = true;
                menu_open = false;
                if !selected {
                    action = Some(SettingsAction::SetCaptureBackend(backend));
                }
            }
        }
    }
    if menu_open && !value_clicked && !option_clicked && ui.input(|input| input.pointer.any_click())
    {
        menu_open = false;
    }
    ui.ctx().data_mut(|data| data.insert_temp(menu_id, menu_open));
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage() -> Rect {
        Rect::from_min_size(Pos2::new(100.0, 200.0), Vec2::new(400.0, 300.0))
    }

    #[test]
    fn pct_rect_round_trips_through_screen_space() {
        let original = PctRect {
            x: 0.25,
            y: 0.5,
            w: 0.5,
            h: 0.25,
        };
        let screen = screen_rect_from_pct(stage(), original);
        let back = pct_rect_from_screen(stage(), screen);
        assert!((back.x - original.x).abs() < 1e-4);
        assert!((back.y - original.y).abs() < 1e-4);
        assert!((back.w - original.w).abs() < 1e-4);
        assert!((back.h - original.h).abs() < 1e-4);
    }

    /// 在一帧里跑一次设置浮层，返回它给出的动作。
    fn panel_action(
        events: Vec<egui::Event>,
        hotkey_editing: bool,
        hotkey_conflict: Option<&str>,
    ) -> Option<SettingsAction> {
        let ctx = egui::Context::default();
        let mut action = None;
        let input = egui::RawInput {
            events,
            ..egui::RawInput::default()
        };
        ctx.run_ui(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                action = settings_panel(
                    ui,
                    SettingsView {
                        hotkey: "F8",
                        hotkey_editing,
                        hotkey_conflict,
                        ocr_enabled: true,
                        config_path: "C:/cfg.toml",
                        capture_backend: CaptureBackend::Wgc,
                        capture_locked: false,
                        capture_test: None,
                        capture_test_error: None,
                    },
                );
            });
        })
        // 无后端可上传纹理：显式清空，别让 epaint 把「未消费的 delta」当成泄漏。
        .textures_delta
        .clear();
        action
    }

    /// 在 400×420 画布上跑三帧并在 `pos` 完成一次点击。
    ///
    /// 三帧是 egui 的点击语义决定的：第一帧登记控件矩形，第二帧按下、第三帧松开才成点击。
    /// 用无内边距的 `Frame::NONE`：行骨架按 `ui.max_rect()` 计算，画面区必须与
    /// `settings_rows` 的入参完全一致，测试的计算位置才和真实点击位置对齐。
    fn click_action(pos: Pos2, capture_locked: bool) -> Option<SettingsAction> {
        let ctx = egui::Context::default();
        let full = Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(theme::WINDOW_SIZE[0], theme::WINDOW_SIZE[1]),
        );
        let press = |pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let mut action = None;
        let frame = |events: Vec<egui::Event>, action: &mut Option<SettingsAction>| {
            let input = egui::RawInput {
                screen_rect: Some(full),
                events,
                ..egui::RawInput::default()
            };
            ctx.run_ui(input, |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        let produced = settings_panel(
                            ui,
                            SettingsView {
                                hotkey: "F8",
                                hotkey_editing: false,
                                hotkey_conflict: None,
                                ocr_enabled: true,
                                config_path: "C:/cfg.toml",
                                capture_backend: CaptureBackend::Wgc,
                                capture_locked,
                                capture_test: None,
                                capture_test_error: None,
                            },
                        );
                        if produced.is_some() {
                            *action = produced;
                        }
                    });
            })
            .textures_delta
            .clear();
        };
        frame(Vec::new(), &mut action);
        frame(vec![egui::Event::PointerMoved(pos), press(true)], &mut action);
        frame(vec![press(false)], &mut action);
        action
    }

    /// 面板行骨架（与 `settings_panel` 同一个函数算出来，测试位置才可信）。
    fn rows_for(size: [f32; 2], has_result: bool) -> SettingsRows {
        let full = Rect::from_min_size(Pos2::ZERO, Vec2::new(size[0], size[1]));
        let content = full.shrink2(Vec2::new(theme::SPACE_M, theme::SPACE_S));
        settings_rows(content, has_result)
    }

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn escape_while_capturing_cancels_without_closing_the_panel() {
        // D4：捕获态下 Esc 只能退出捕获，绝不能落到「关闭设置面板」。
        let action = panel_action(
            vec![key_event(egui::Key::Escape, egui::Modifiers::default())],
            true,
            None,
        );
        assert_eq!(action, Some(SettingsAction::CancelHotkey));

        // 非捕获态下 Esc 仍然是「关面板」，语义没被改坏。
        let action = panel_action(
            vec![key_event(egui::Key::Escape, egui::Modifiers::default())],
            false,
            None,
        );
        assert_eq!(action, Some(SettingsAction::Close));
    }

    #[test]
    fn capturing_a_key_commits_the_chord_and_unsupported_keys_keep_waiting() {
        let action = panel_action(
            vec![key_event(egui::Key::F9, egui::Modifiers::default())],
            true,
            None,
        );
        match action {
            Some(SettingsAction::CommitHotkey(chord)) => {
                assert_eq!(chord.main_name(), "F9");
                assert_eq!(chord.modifiers(), 0);
            }
            other => panic!("按 F9 必须提交 F9，实际 {other:?}"),
        }

        // 不可捕获的键不提交也不取消；纯修饰键同理。
        assert_eq!(
            panel_action(
                vec![key_event(egui::Key::Slash, egui::Modifiers::default())],
                true,
                None
            ),
            None,
            "按 / 必须继续等待"
        );
        assert_eq!(
            panel_action(
                vec![egui::Event::ModifiersChanged(egui::Modifiers {
                    ctrl: true,
                    ..egui::Modifiers::default()
                })],
                true,
                None
            ),
            None,
            "只按 Ctrl 不构成热键"
        );
    }

    #[test]
    fn panel_renders_a_conflict_line_without_taking_the_click() {
        // 冲突提示只影响绘制；此时点热键行仍是「进入捕获」，语义不变。
        assert_eq!(panel_action(Vec::new(), false, Some("热键冲突 · 仍为 F8")), None);
    }

    #[test]
    fn pct_rect_is_clamped_inside_the_preview() {
        // 拖框越出预览区时，百分比不得超出 [0,1]（否则 Config 校验会整份拒绝）。
        let drawn = Rect::from_min_size(Pos2::new(80.0, 150.0), Vec2::new(600.0, 500.0));
        let clamped = pct_rect_from_screen(stage(), drawn);
        assert_eq!(clamped.x, 0.0);
        assert_eq!(clamped.y, 0.0);
        assert!(clamped.x + clamped.w <= 1.0 + 1e-6);
        assert!(clamped.y + clamped.h <= 1.0 + 1e-6);
    }

    #[test]
    fn notice_button_stays_inside_the_result_slot() {
        // 进度区实测可用 100px，但按 spec 的 PROGRESS_H=64 这个最紧的高度兜底：
        // 警示条只能借用这块区域，绝不能把下方元素挤出窗口。
        let slot = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(376.0, theme::PROGRESS_H));
        let button = notice_button_rect(slot);
        assert!(slot.contains_rect(button), "「知道了」必须完整落在结果区内");
        let text_room = button.left() - slot.left() - theme::SPACE_M;
        assert!(
            text_room >= 9.0 * theme::FONT_BODY,
            "文案位被按钮侵占：只剩 {text_room}px"
        );
    }

    #[test]
    fn settings_rows_fit_the_window_with_and_without_a_test_result() {
        for size in [theme::WINDOW_SIZE, theme::WINDOW_MIN] {
            for has_result in [false, true] {
                let full = Rect::from_min_size(Pos2::ZERO, Vec2::new(size[0], size[1]));
                let content = full.shrink2(Vec2::new(theme::SPACE_M, theme::SPACE_S));
                let rows = rows_for(size, has_result);
                for (name, rect) in [
                    ("热键", rows.hotkey),
                    ("兜底", rows.ocr),
                    ("截图方式", rows.backend),
                    ("测试截图", rows.test),
                    ("结果区", rows.result),
                    ("底部按钮", rows.footer),
                ] {
                    assert!(
                        content.contains_rect(rect),
                        "{size:?} 下「{name}」溢出内容区：{rect:?}"
                    );
                }
                assert!(
                    !rows.result.intersects(rows.footer),
                    "{size:?} 下结果区挤压底部按钮"
                );
                // 结果区与配置路径互斥：两者不能同时消失，也绝不能重叠。
                assert_eq!(rows.config.is_some(), !has_result);
                if let Some(config) = rows.config {
                    assert!(content.contains_rect(config), "{size:?} 下配置路径溢出");
                    assert!(
                        !config.intersects(rows.footer),
                        "{size:?} 下配置路径挤压底部按钮"
                    );
                }
                // 两行新控件（截图方式 / 测试截图）必须放得下标题 + 副标题。
                for rect in [rows.backend, rows.test] {
                    assert!(rect.height() >= theme::FONT_TITLE + theme::FONT_BODY + theme::SPACE_XS);
                }
            }
        }
    }

    #[test]
    fn clicking_the_test_row_asks_for_one_capture() {
        let rows = rows_for(theme::WINDOW_SIZE, false);
        let button = row_slot(rows.test, ROW_BUTTON_W);
        assert_eq!(
            click_action(button.center(), false),
            Some(SettingsAction::TestCapture),
            "点「抓一帧」必须只发一个抓帧请求"
        );
    }

    #[test]
    fn capture_rows_are_dead_while_a_task_runs() {
        // 运行中（D1）：截图方式与测试截图都不可点 —— 点击不该产生任何动作。
        let rows = rows_for(theme::WINDOW_SIZE, false);
        let button = row_slot(rows.test, ROW_BUTTON_W);
        assert_eq!(click_action(button.center(), true), None, "运行中禁止抓帧");
        let value = row_slot(rows.backend, BACKEND_VALUE_W);
        assert_eq!(click_action(value.center(), true), None, "运行中禁止切换后端");
    }

    #[test]
    fn backend_labels_cover_both_options_and_flag_the_compat_risk() {
        for backend in CAPTURE_BACKENDS {
            assert!(!backend_label(backend).is_empty());
        }
        assert_eq!(CAPTURE_BACKENDS, [CaptureBackend::Wgc, CaptureBackend::BitBlt]);
        // 兼容档必须在 UI 上自曝「可能截不到独占全屏」（spec D1 显式边界）。
        assert!(backend_label(CaptureBackend::BitBlt).contains("截不到独占全屏"));
        assert_eq!(
            CAPTURE_BACKENDS.len(),
            2,
            "只有两档；新增档位必须同时补文案与配置解析"
        );
    }

    #[test]
    fn thumbnail_fits_inside_its_box_without_stretching() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(CAPTURE_THUMB_MAX[0], CAPTURE_THUMB_MAX[1]));
        let fitted = fit_inside(area, 1920, 1080);
        assert!(area.contains_rect(fitted), "缩略图不得越出结果区");
        let ratio = fitted.width() / fitted.height();
        assert!((ratio - 16.0 / 9.0).abs() < 0.05, "长宽比被拉伸：{ratio}");
    }

    #[test]
    fn with_alpha_keeps_rgb_and_scales_alpha() {
        let half = with_alpha(theme::ACCENT, 0.5);
        let [r, g, b, a] = half.to_srgba_unmultiplied();
        let [er, eg, eb, _] = theme::ACCENT.to_srgba_unmultiplied();
        // Color32 内部是预乘存储，回读会有 ±1 的取整误差。
        for (actual, expected) in [(r, er), (g, eg), (b, eb)] {
            assert!(
                (i32::from(actual) - i32::from(expected)).abs() <= 2,
                "通道 {actual} 与 {expected} 偏差过大"
            );
        }
        assert!((127..=128).contains(&a));
    }
}
