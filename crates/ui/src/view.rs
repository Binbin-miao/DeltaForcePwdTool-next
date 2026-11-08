//! 绘制层：主窗口（五态 + 收起态）、免责声明、校准向导。
//!
//! 全部用 `Painter` 手绘（像素级对齐 token），不用 egui 默认控件皮肤 —— 这样状态色
//! 只会出现在 2px 描边 / 状态点 / 数值文字上，不会铺成大面积色块。

use delta_geom::PctRect;
use egui::{Align2, Color32, Id, Pos2, Rect, Sense, Stroke, Vec2};

use crate::state::{Action, Phase, UiState};
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
    let time = ui.ctx().input(|input| input.time);
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
    ui.interact(rect, Id::new(("delta-ui", key)), sense)
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
        if toggle_row(ui, row, scene.label(), usable, &painter) {
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
    paint_panel(&painter, progress);
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

/// 收起态：176×36 悬浮条（状态点 + 场景名 + 迷你进度 + 展开箭头）。
fn collapsed_strip(ui: &egui::Ui, full: Rect, state: &mut UiState) -> Option<Action> {
    let painter = ui.painter().clone();
    let time = ui.ctx().input(|input| input.time);
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
