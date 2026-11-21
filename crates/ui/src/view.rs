//! 绘制层：主窗口（L-A 双列开关 · 单卡主体）、设置浮层。
//!
//! 全部用 `Painter` 手绘（像素级对齐 token），不用 egui 默认控件皮肤。
//! 颜色一律来自显式传入的 `pal: &Palette`（`docs/design/ui-themes.md` §5 的结论：
//! 不做 thread_local 访问器，否则这批纯函数就无法单测）。

use delta_geom::PctRect;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::state::{
    read_capture, Action, CaptureBackend, CaptureOutcome, CaptureTestResult, GridCell, KeyChord,
    Phase, PrimaryState, ResultState, SceneKind, ThemeId, UiState,
};
use crate::theme::{self, Palette};

/// 主窗口一帧：返回用户动作（无动作为 `None`）。
pub fn main_window(ui: &mut egui::Ui, state: &mut UiState, pal: &Palette) -> Option<Action> {
    let full = ui.max_rect();
    let painter = ui.painter().clone();
    paint_shell(&painter, full, pal);
    if state.collapsed {
        collapsed_strip(ui, full, state, pal)
    } else {
        main_layout(ui, full, state, pal)
    }
}

// ── 外壳与通用零件 ───────────────────────────────────────────────

/// 外壳：深色外扩 2px 阴影；浅色主题改 1px `line_strong` 外框（egui 无模糊，黑边会脏）。
///
/// **外壳不参与状态动画**：只画一层恒定 1px 描边。状态一律由标题栏的 7px 状态点
/// 与状态文字表达 —— 此前「运行中呼吸 / 未识别 2Hz 闪」会沿整个窗口画 2px 亮色外框，
/// 每按一次热键就闪一次黄条，实测被用户判为干扰。
fn paint_shell(painter: &egui::Painter, full: Rect, pal: &Palette) {
    let corner = pal.corner_panel();
    if pal.dark {
        painter.rect_filled(full, corner, pal.shadow());
    }
    let body = if pal.dark {
        full.shrink(theme::SHADOW_EXTENT)
    } else {
        full.shrink(1.0)
    };
    painter.rect_filled(body, corner, pal.bg_panel);
    painter.rect_stroke(
        body,
        corner,
        Stroke::new(1.0, pal.line_strong),
        egui::StrokeKind::Inside,
    );
}

/// 面板底 + 1px 描边 + 顶部 1px 高光（浅色主题靠描边撑层级）。
fn paint_panel(painter: &egui::Painter, rect: Rect, pal: &Palette) {
    let corner = pal.corner_panel();
    painter.rect_filled(rect, corner, pal.bg_raised);
    painter.rect_stroke(rect, corner, pal.panel_stroke(), egui::StrokeKind::Inside);
    painter.line_segment(
        [
            rect.left_top() + Vec2::splat(1.0),
            rect.right_top() + Vec2::new(-1.0, 1.0),
        ],
        Stroke::new(1.0, pal.highlight()),
    );
}

/// 交互区域：返回响应（`make_persistent_id` 按控件分桶，跨帧稳定）。
fn clickable(ui: &egui::Ui, rect: Rect, key: &str, sense: Sense) -> egui::Response {
    ui.interact(rect, ui.make_persistent_id(key), sense)
}

/// 状态点：7px 定尺寸，只呼吸 alpha（不放大）。
fn status_dot(painter: &egui::Painter, center: Pos2, phase: Phase, time: f64, pal: &Palette) {
    let alpha = if phase == Phase::Idle {
        1.0
    } else {
        phase.status_alpha(time).max(0.25)
    };
    painter.circle_filled(
        center,
        theme::DOT_RADIUS,
        theme::with_alpha(phase.color(pal), alpha),
    );
}

/// 小图标按钮（图钉 / 收起 / 退出 / 展开）。
fn icon_button(ui: &egui::Ui, rect: Rect, key: &str, glyph: &str, pal: &Palette) -> bool {
    let response = clickable(ui, rect, key, Sense::click());
    let color = if response.hovered() {
        pal.accent
    } else {
        pal.text_2
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        pal.text(pal.font_body),
        color,
    );
    response.clicked()
}

/// 文字链（底部「校准向导 / 设置」、错误态的「重试」）。
fn link(ui: &egui::Ui, rect: Rect, label: &str, size: f32, pal: &Palette) -> bool {
    let response = clickable(ui, rect, label, Sense::click());
    let color = if response.hovered() {
        pal.accent
    } else {
        pal.text_3
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        pal.text(size),
        color,
    );
    response.clicked()
}

/// 28×16 轨道 + 12px 滑块（状态由滑块位置表达，「开/关」二字已删除）。
fn switch(painter: &egui::Painter, center: Pos2, on: bool, pal: &Palette) {
    let track = Rect::from_center_size(center, Vec2::new(theme::SWITCH_W, theme::SWITCH_H));
    let radius = egui::CornerRadius::same((theme::SWITCH_H / 2.0) as u8);
    if on {
        painter.rect_filled(track, radius, pal.switch_track_on());
    } else {
        painter.rect_filled(track, radius, pal.switch_track_off());
        painter.rect_stroke(
            track,
            radius,
            Stroke::new(1.0, pal.line_strong),
            egui::StrokeKind::Inside,
        );
    }
    let inset = theme::SWITCH_THUMB / 2.0 + 2.0;
    let x = if on {
        track.right() - inset
    } else {
        track.left() + inset
    };
    painter.circle_filled(
        Pos2::new(x, track.center().y),
        theme::SWITCH_THUMB / 2.0,
        if on {
            pal.switch_thumb_on()
        } else {
            pal.switch_thumb_off()
        },
    );
}

// ── 主布局（`ui-themes.md` §3.1 的纵向分段表）─────────────────────

/// 开关网格每行的格数（3 列一行流式排）。
const GRID_COLS: usize = 3;

/// 网格行数：**由格数算出来**（`div_ceil`），不再硬编码 2 行。
fn grid_rows(cells: usize) -> usize {
    cells.div_ceil(GRID_COLS)
}

/// 网格区总高：`行数 × 行高 + (行数 − 1) × 行距`。
fn grid_height(rows: usize) -> f32 {
    if rows == 0 {
        return 0.0;
    }
    let rows = rows as f32;
    rows * theme::GRID_ROW_H + (rows - 1.0) * theme::GRID_ROW_GAP
}

/// L-A 分段的六个区段与开关网格的每格落点（格数决定行数）。
#[derive(Debug, Clone)]
struct MainRows {
    title: Rect,
    result: Rect,
    grid: Vec<Rect>,
    primary: Rect,
    status: Rect,
    footer: Rect,
}

/// 由内容区与格数算出纵向分段（纯函数，便于直接断言 400×420 / 340×384）。
///
/// 标称 426 = 32+8+96+12+146+12+44+8+32+8+28（网格 4 行：4×32+3×6 = 146）；
/// 窗口更矮时**先压结果区（96→64），再压段间距（→4）**——不压开关行高、不删行。
fn main_rows(content: Rect, cells: usize) -> MainRows {
    let width = content.width();
    let grid_h = grid_height(grid_rows(cells));
    let fixed = theme::TITLE_BAR_H
        + grid_h
        + theme::PRIMARY_H
        + theme::STATUS_H
        + theme::FOOTER_H;
    let mut gaps = [8.0_f32, 12.0, 12.0, 8.0, 8.0];
    let total = |result_h: f32, gaps: &[f32; 5]| fixed + result_h + gaps.iter().sum::<f32>();
    let mut result_h = theme::RESULT_H;
    while total(result_h, &gaps) > content.height() && result_h > theme::RESULT_MIN_H {
        result_h -= 4.0;
    }
    while total(result_h, &gaps) > content.height() && gaps.iter().any(|gap| *gap > 4.0) {
        for gap in gaps.iter_mut() {
            *gap = (*gap - 2.0).max(4.0);
        }
    }
    // 富余空间全部给结果区（窗口拉高时不留死白）。
    let slack = content.height() - total(result_h, &gaps);
    if slack > 0.0 {
        result_h += slack;
    }

    let row = |top: f32, height: f32| {
        Rect::from_min_size(Pos2::new(content.left(), top), Vec2::new(width, height))
    };
    let mut y = content.top();
    let title = row(y, theme::TITLE_BAR_H);
    y = title.bottom() + gaps[0];
    let result = row(y, result_h);
    y = result.bottom() + gaps[1];
    let grid_area = row(y, grid_h);
    y = grid_area.bottom() + gaps[2];
    let primary = row(y, theme::PRIMARY_H);
    y = primary.bottom() + gaps[3];
    let status = row(y, theme::STATUS_H);
    y = status.bottom() + gaps[4];
    let footer = row(y, theme::FOOTER_H);

    // 3 列一行：列宽 =（内容宽 − 2×列距）/3，行高固定 32（行数由格数决定）。
    let cell_w = (width - theme::GRID_COL_GAP * (GRID_COLS as f32 - 1.0)) / GRID_COLS as f32;
    let grid = (0..cells)
        .map(|index| {
            let column = index % GRID_COLS;
            let row_index = index / GRID_COLS;
            Rect::from_min_size(
                Pos2::new(
                    content.left() + column as f32 * (cell_w + theme::GRID_COL_GAP),
                    grid_area.top()
                        + row_index as f32 * (theme::GRID_ROW_H + theme::GRID_ROW_GAP),
                ),
                Vec2::new(cell_w, theme::GRID_ROW_H),
            )
        })
        .collect();

    MainRows {
        title,
        result,
        grid,
        primary,
        status,
        footer,
    }
}

/// 标题栏上的「识别总开关」胶囊：开=绿色实心，关=灰色空心。
///
/// 关掉后 `run_once` 在入口直接返回，**连截图都不抓**（不只是不注入）。
/// 放在标题栏而不是设置页，是因为这个开关的用途就是「临时别动」——
/// 对枪、跑图、看录像时需要一秒关掉，翻设置页太慢。
fn master_toggle(ui: &egui::Ui, rect: Rect, on: bool, hotkey: &str, pal: &Palette) -> bool {
    let (fill, text_color, label) = if on {
        (pal.ok, pal.bg_panel, "识别中")
    } else {
        (pal.bg_panel, pal.text_3, "已暂停")
    };
    let painter = ui.painter();
    let corner = rect.height() / 2.0;
    painter.rect_filled(rect, corner, fill);
    if !on {
        painter.rect_stroke(
            rect,
            corner,
            Stroke::new(1.0, pal.line_strong),
            egui::StrokeKind::Inside,
        );
    }
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        pal.text(pal.font_label),
        text_color,
    );
    // 胶囊只有 66px 宽，塞不下键名 —— 所以键名走悬停提示：既告诉用户「这个能点」，
    // 也告诉他有热键可用（不然「识别中」看起来只是个状态徽标）。
    clickable(ui, rect, "master-toggle", Sense::click())
        .on_hover_text(format!(
            "点这里{} · 热键 {hotkey}",
            if on { "暂停识别" } else { "恢复识别" }
        ))
        .clicked()
}

fn main_layout(ui: &egui::Ui, full: Rect, state: &mut UiState, pal: &Palette) -> Option<Action> {
    let content = full.shrink2(Vec2::new(theme::SPACE_M, theme::SPACE_S));
    let cells = state.grid();
    let rows = main_rows(content, cells.len());
    let mut action = None;

    // ── 1 标题条：状态点 + 状态文字 + 右侧三个 16px 图标 ────────────
    let drag = clickable(ui, rows.title, "title-drag", Sense::click_and_drag());
    if drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    let painter = ui.painter();
    status_dot(
        painter,
        rows.title.left_center() + Vec2::new(theme::SPACE_S, 0.0),
        state.phase,
        state.phase_elapsed,
        pal,
    );
    painter.text(
        rows.title.left_center() + Vec2::new(theme::SPACE_S + theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        state.status_line(),
        pal.text(pal.font_title),
        pal.text_1,
    );
    let icon = theme::ICON_BUTTON;
    let icon_at = |index: f32| {
        Rect::from_min_size(
            Pos2::new(
                rows.title.right() - icon * index - theme::SPACE_XS * index,
                rows.title.top() + (theme::TITLE_BAR_H - icon) / 2.0,
            ),
            Vec2::splat(icon),
        )
    };
    // 识别总开关：放在标题栏、紧贴右侧图标之前 —— 窗口一打开第一眼就能看到，
    // 不用翻设置页。开着是绿色「识别中」，关掉是灰色「已暂停」。
    let master_w = 66.0;
    let master = Rect::from_min_size(
        Pos2::new(
            rows.title.right()
                - icon * 3.0
                - theme::SPACE_XS * 3.0
                - theme::SPACE_S
                - master_w,
            rows.title.top() + (theme::TITLE_BAR_H - theme::MASTER_TOGGLE_H) / 2.0,
        ),
        Vec2::new(master_w, theme::MASTER_TOGGLE_H),
    );
    if master_toggle(ui, master, state.master_enabled, &state.hotkey_master, pal) {
        action = Some(Action::ToggleMaster);
    }
    let (quit, collapse, pin) = (icon_at(1.0), icon_at(2.0), icon_at(3.0));
    if icon_button(ui, pin, "pin", if state.always_on_top { "■" } else { "□" }, pal) {
        action = Some(Action::ToggleAlwaysOnTop);
    }
    if icon_button(ui, collapse, "collapse", "△", pal) {
        action = Some(Action::Collapse);
    }
    if icon_button(ui, quit, "quit", "×", pal) {
        action = Some(Action::Quit);
    }

    // ── 2 结果区：空闲不画卡；有结果单卡 + 五态描边 ──────────────────
    if let Some(produced) = result_area(ui, rows.result, state, pal) {
        action = Some(produced);
    }

    // ── 3 开关网格：3 列一行流式排，格数与行数都由 `state.grid()` 决定 ─────
    for (index, cell) in cells.into_iter().enumerate() {
        let on = state.cell_on(cell);
        if !grid_cell(ui, rows.grid[index], cell, on, &state.hotkey, pal) {
            continue;
        }
        match cell {
            // shortcut: 7 个功能开关直接写 `UiState`。app 侧 `Action` 目前只有
            // `SceneKind` 粒度，而本轮不许动 `crates/app`；下一轮接线改成
            // `Action::ToggleFlowId(FlowId)` 后把这行换回去（`ToggleFlow` 保留给场景级）。
            GridCell::Flow(flow) => state.toggles.toggle_flow(flow),
            GridCell::FingerprintAutoClick => action = Some(Action::ToggleFingerprintAutoClick),
            GridCell::MorseAutoDownload => action = Some(Action::ToggleMorseAutoDownload),
            GridCell::Keycap => {}
        }
    }

    // ── 4 主按钮：满宽实心 accent ───────────────────────────────
    if let Some(produced) = primary_button(ui, rows.primary, state, pal) {
        action = Some(produced);
    }

    // ── 5 状态行：12px 文案 + 底部 2px 进度轨 ────────────────────
    status_row(ui, rows.status, state, pal);

    // ── 6 底部文字链 ───────────────────────────────────────────
    let link_w = 6.0 * pal.font_label + theme::SPACE_S;
    let right = Rect::from_min_size(
        Pos2::new(rows.footer.right() - link_w, rows.footer.top()),
        Vec2::new(link_w, rows.footer.height()),
    );
    if link(ui, right, "设置", pal.font_label, pal) {
        action = Some(Action::OpenSettings);
    }

    resize_grip(ui, full, pal);
    action
}

/// 结果区（§3.4 五态 + spec D2b 三场景）。
fn result_area(ui: &egui::Ui, rect: Rect, state: &UiState, pal: &Palette) -> Option<Action> {
    let painter = ui.painter();
    let result_state = state.result_state();
    if result_state == ResultState::Idle {
        // 空闲：**不画边框**，只一行 13px 提示（空的高对比卡会被读成「加载失败」）。
        painter.text(
            rect.left_center() + Vec2::new(theme::SPACE_S, 0.0),
            Align2::LEFT_CENTER,
            state.idle_hint(),
            pal.text(pal.font_body),
            pal.text_2,
        );
        return None;
    }

    painter.rect_filled(rect, pal.corner_panel(), pal.bg_inset);
    if let Some(border) = result_state.border(pal) {
        painter.rect_stroke(
            rect,
            pal.corner_panel(),
            Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
    }

    let mut action = None;
    match state.result_content() {
        Some(content) => {
            painter.text(
                Pos2::new(rect.center().x, rect.center().y - pal.font_label * 0.7),
                Align2::CENTER_CENTER,
                &content.headline,
                theme::mono(pal.font_value),
                pal.text_1,
            );
            // 11px 来源标签（摩斯 / 指纹 / 操作台）。
            painter.text(
                Pos2::new(rect.center().x, rect.bottom() - theme::SPACE_M),
                Align2::CENTER_BOTTOM,
                source_label(state.scene),
                pal.text(pal.font_label),
                pal.text_3,
            );
            // 12px 状态标签在右下。
            painter.text(
                rect.right_bottom() - Vec2::new(theme::SPACE_M, theme::SPACE_M),
                Align2::RIGHT_BOTTOM,
                &content.tag,
                pal.text(pal.font_label + 1.0),
                pal.text_2,
            );
        }
        None => {
            // 运行中还没有结果 / 失败原因：居中一行，绝不画假进度。
            let text = if state.message.is_empty() {
                state.progress.label()
            } else {
                state.message.clone()
            };
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                text,
                pal.text(pal.font_body),
                if result_state == ResultState::Error {
                    pal.err
                } else {
                    pal.text_2
                },
            );
        }
    }

    match result_state {
        ResultState::Partial => {
            painter.text(
                rect.left_bottom() + Vec2::new(theme::SPACE_M, -theme::SPACE_M),
                Align2::LEFT_BOTTOM,
                "识别可能有误，请核对",
                pal.text(pal.font_label),
                pal.warn,
            );
        }
        ResultState::Error => {
            let retry = Rect::from_min_size(
                Pos2::new(
                    rect.left() + theme::SPACE_M,
                    rect.bottom() - theme::SPACE_M - 16.0,
                ),
                Vec2::new(theme::SPACE_XL + theme::SPACE_S, 16.0),
            );
            if link(ui, retry, "重试", pal.font_label, pal) {
                action = Some(Action::Start);
            }
        }
        _ => {}
    }
    action
}

/// 结果区来源标签（spec D2b 的三种场景）。
fn source_label(scene: SceneKind) -> &'static str {
    match scene {
        SceneKind::Morse => "摩斯",
        SceneKind::Fingerprint => "指纹",
        SceneKind::Console => "操作台",
        SceneKind::Unknown => "识别结果",
    }
}

/// 一格开关（或热键键帽）；返回「这一格被点了」。
///
/// 点击之后做什么由调用方按 [`GridCell`] 分派 —— 功能开关写 `UiState`，
/// 两个附加开关返回 [`Action`]（它们要落盘），键帽没有动作。
fn grid_cell(
    ui: &egui::Ui,
    rect: Rect,
    cell: GridCell,
    on: bool,
    hotkey: &str,
    pal: &Palette,
) -> bool {
    let painter = ui.painter();
    if cell == GridCell::Keycap {
        // 键帽：空心描边块 + 等宽热键名，不可点。
        painter.rect_stroke(
            rect,
            pal.corner_button(),
            Stroke::new(1.0, pal.line_strong),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            hotkey,
            theme::mono(pal.font_label),
            pal.text_2,
        );
        return false;
    }

    let label = cell.label();
    let response = clickable(ui, rect, label, Sense::click());
    painter.rect_filled(
        rect,
        pal.corner_button(),
        if response.hovered() {
            pal.bg_raised
        } else {
            pal.bg_panel
        },
    );
    painter.rect_stroke(
        rect,
        pal.corner_button(),
        Stroke::new(1.0, pal.line_subtle),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.left_center() + Vec2::new(theme::SPACE_S, 0.0),
        Align2::LEFT_CENTER,
        label,
        pal.text(pal.font_label + 1.0),
        if on { pal.text_1 } else { pal.text_2 },
    );
    // 3 列布局一格只有 ~120px：文案左贴 8px，轨道右缘也贴 8px（`switch` 按中心点画，
    // 所以中心 = 右 − 8 − 半个轨道宽），保证 4 个汉字 + 开关都塞得进最小窗。
    switch(
        painter,
        rect.right_center() - Vec2::new(theme::SPACE_S + theme::SWITCH_W / 2.0, 0.0),
        on,
        pal,
    );
    response.clicked()
}

/// 主按钮：可用态满宽实心 accent；不可用态改文案 + 描边（不静默禁用）。
fn primary_button(ui: &egui::Ui, rect: Rect, state: &mut UiState, pal: &Palette) -> Option<Action> {
    let form = state.primary_state();
    let response = clickable(ui, rect, "primary", Sense::click());
    let painter = ui.painter();
    let corner = pal.corner_button();
    if form.filled() {
        let fill = if response.is_pointer_button_down_on() {
            pal.btn_fill_active()
        } else if response.hovered() {
            pal.btn_fill_hover()
        } else {
            pal.btn_fill()
        };
        painter.rect_filled(rect, corner, fill);
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            form.label(),
            pal.text(pal.font_title),
            pal.accent_text_on,
        );
        // 右侧 30% alpha 的热键提示。
        painter.text(
            rect.right_center() - Vec2::new(theme::SPACE_M, 0.0),
            Align2::RIGHT_CENTER,
            &state.hotkey,
            theme::mono(pal.font_label),
            theme::with_alpha(pal.accent_text_on, 0.30),
        );
    } else {
        painter.rect_filled(rect, corner, pal.bg_raised);
        painter.rect_stroke(rect, corner, Stroke::new(1.0, pal.line_strong), egui::StrokeKind::Inside);
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            form.label(),
            pal.text(pal.font_title),
            pal.text_2,
        );
    }
    if !response.clicked() {
        return None;
    }
    if form == PrimaryState::Running {
        // 运行态防连点：300ms 内的第二次点击被吞（§3.4 Loading 行）。
        let now = ui.input(|input| input.time);
        if !state.cancel_allowed(now) {
            return None;
        }
    }
    Some(form.action())
}

/// 状态行：左 12px 文案（一次性警示时改说警示）+ 底部 2px 进度轨。
fn status_row(ui: &egui::Ui, rect: Rect, state: &mut UiState, pal: &Palette) {
    let painter = ui.painter();
    let track = Rect::from_min_size(
        Pos2::new(rect.left(), rect.bottom() - theme::PROGRESS_TRACK_H),
        Vec2::new(rect.width(), theme::PROGRESS_TRACK_H),
    );
    painter.rect_filled(track, pal.corner_button(), pal.progress_track());
    let ratio = state.progress.ratio();
    if ratio > 0.0 {
        painter.rect_filled(
            Rect::from_min_size(track.min, Vec2::new(track.width() * ratio, track.height())),
            pal.corner_button(),
            pal.progress_fill(),
        );
    }

    if let Some(notice) = state.notice.clone() {
        painter.text(
            Pos2::new(rect.left(), rect.center().y - theme::PROGRESS_TRACK_H),
            Align2::LEFT_CENTER,
            notice.as_str(),
            pal.text(pal.font_body),
            pal.warn,
        );
        let dismiss = Rect::from_min_size(
            Pos2::new(
                rect.right() - theme::NOTICE_BUTTON_W,
                rect.center().y - 10.0 - theme::PROGRESS_TRACK_H,
            ),
            Vec2::new(theme::NOTICE_BUTTON_W, 20.0),
        );
        if link(ui, dismiss, "知道了", pal.font_label, pal) {
            state.dismiss_notice();
        }
        return;
    }

    let text = if state.message.is_empty() {
        state.status_line()
    } else {
        format!("{} · {}", state.status_line(), state.message)
    };
    painter.text(
        Pos2::new(rect.left(), rect.center().y - theme::PROGRESS_TRACK_H),
        Align2::LEFT_CENTER,
        text,
        pal.text(pal.font_label + 1.0),
        pal.text_2,
    );
}

/// 收起态：176×36 悬浮条（状态点 + 场景名 + 迷你进度 + 展开箭头）。
fn collapsed_strip(ui: &egui::Ui, full: Rect, state: &mut UiState, pal: &Palette) -> Option<Action> {
    let strip = if pal.dark {
        full.shrink(theme::SHADOW_EXTENT)
    } else {
        full.shrink(1.0)
    };
    let mut action = None;
    let drag = clickable(ui, strip, "collapsed-drag", Sense::click_and_drag());
    if drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    let painter = ui.painter();
    status_dot(
        painter,
        strip.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        state.phase,
        state.phase_elapsed,
        pal,
    );
    painter.text(
        strip.left_center() + Vec2::new(theme::SPACE_XL, 0.0),
        Align2::LEFT_CENTER,
        state.scene.label(),
        pal.text(pal.font_body),
        pal.text_1,
    );
    let mini = Rect::from_min_size(
        Pos2::new(strip.left() + 104.0, strip.center().y - 1.0),
        Vec2::new(40.0, 2.0),
    );
    painter.rect_filled(mini, pal.corner_button(), pal.progress_track());
    if state.progress.ratio() > 0.0 {
        painter.rect_filled(
            Rect::from_min_size(mini.min, Vec2::new(mini.width() * state.progress.ratio(), 2.0)),
            pal.corner_button(),
            pal.progress_fill(),
        );
    }
    let expand = Rect::from_min_size(
        Pos2::new(strip.right() - theme::SPACE_XL, strip.center().y - theme::SPACE_S),
        Vec2::splat(theme::SPACE_L),
    );
    if icon_button(ui, expand, "expand", "▽", pal) {
        action = Some(Action::Expand);
    }
    action
}

/// 右下角 12×12 抓边：按住进入系统缩放。
fn resize_grip(ui: &egui::Ui, full: Rect, pal: &Palette) {
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
            Stroke::new(1.0, pal.line_strong),
        );
    }
    if response.drag_started() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::BeginResize(
                egui::viewport::ResizeDirection::SouthEast,
            ));
    }
}

/// 次要按钮（描边款）。
fn secondary_button(ui: &egui::Ui, rect: Rect, label: &str, pal: &Palette) -> bool {
    let response = clickable(ui, rect, label, Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, pal.corner_button(), pal.bg_panel);
    painter.rect_stroke(
        rect,
        pal.corner_button(),
        Stroke::new(1.0, pal.line_strong),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        pal.text(pal.font_body),
        if response.hovered() {
            pal.text_1
        } else {
            pal.text_2
        },
    );
    response.clicked()
}

/// 实心主按钮（向导里的「继续」）。
fn filled_button(ui: &egui::Ui, rect: Rect, label: &str, enabled: bool, pal: &Palette) -> bool {
    let response = clickable(ui, rect, label, Sense::click());
    let painter = ui.painter();
    if !enabled {
        painter.rect_filled(rect, pal.corner_button(), pal.bg_raised);
        painter.rect_stroke(
            rect,
            pal.corner_button(),
            Stroke::new(1.0, pal.line_strong),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            pal.text(pal.font_title),
            pal.text_2,
        );
        return false;
    }
    let fill = if response.hovered() {
        pal.btn_fill_hover()
    } else {
        pal.btn_fill()
    };
    painter.rect_filled(rect, pal.corner_button(), fill);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        pal.text(pal.font_title),
        pal.accent_text_on,
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

/// accent 虚线框（校准里「已存区域」的标记）。

/// 截图后端的两个选项（默认 WGC；顺序即下拉顺序）。
const CAPTURE_BACKENDS: [CaptureBackend; 2] = [CaptureBackend::Wgc, CaptureBackend::BitBlt];
/// 带副标题的设置行高度（标题 + 灰色小字说明）。
const SETTINGS_ROW_H: f32 = 40.0;
/// 单行控件高度。
const SETTINGS_LINE_H: f32 = 28.0;
/// 行间距。
const SETTINGS_GAP: f32 = 6.0;
/// 底部动作条高度。
const SETTINGS_FOOTER_H: f32 = 36.0;
/// 测试结果区高度（缩略图 + 一行「宽×高 · 耗时ms」）。
const CAPTURE_RESULT_H: f32 = 90.0;
/// 缩略图上限。
const CAPTURE_THUMB_MAX: [f32; 2] = [160.0, 64.0];
/// 右侧值槽宽度（要放下 `Windows Graphics Capture ▼`）。
const BACKEND_VALUE_W: f32 = 168.0;
/// 下拉面板宽度。
const BACKEND_MENU_W: f32 = 220.0;
/// 行内小按钮宽度（`抓一帧`）。
const ROW_BUTTON_W: f32 = 72.0;
/// 主色预设色相（用户只选色相，明度彩度由 OKLCH 规则算）。
const ACCENT_HUES: [f32; 8] = [0.0, 30.0, 45.0, 120.0, 180.0, 210.0, 270.0, 330.0];

/// 下拉里每一项的文案。兼容档必须自曝风险（spec D1）。
fn backend_label(backend: CaptureBackend) -> &'static str {
    match backend {
        CaptureBackend::Wgc => "Windows Graphics Capture",
        CaptureBackend::BitBlt => "兼容模式 · 可能截不到独占全屏",
    }
}

/// 设置面板的行骨架（坐标相对**滚动后的内层区**）。
///
/// 顺序：热键 / 文字兜底 / 主题 / 主色 / F 键监听 / 游戏关键词 / 截图方式 / 测试截图 / 结果或配置路径。
/// 内容高于视口时由 [`settings_panel`] 内部滚动（主界面不允许滚动）。
#[derive(Debug, Clone, Copy)]
struct SettingsRows {
    master: Rect,
    /// 总开关热键（紧跟总开关行：开关和它的快捷键摆在上下相邻两行）。
    master_key: Rect,
    hotkey: Rect,
    morse: Rect,
    fingerprint: Rect,
    console: Rect,
    /// 黄键台 / 横线台（F10/F11）：紧跟在「操作台」之后。
    keys: Rect,
    line: Rect,
    ocr: Rect,
    theme: Rect,
    accent: Rect,
    fkey: Rect,
    timings: [Rect; delta_config::TimingField::ALL.len()],
    keywords: Rect,
    backend: Rect,
    test: Rect,
    /// 结果区（缩略图 / 失败原因）。
    result: Rect,
    /// 配置路径区；有测试结果时为 `None`（与结果区二选一）。
    config: Option<Rect>,
}

fn settings_rows(content: Rect, has_result: bool) -> SettingsRows {
    let row = |top: f32, height: f32| {
        Rect::from_min_size(Pos2::new(content.left(), top), Vec2::new(content.width(), height))
    };
    let mut top = content.top();
    let mut next = |height: f32| {
        let rect = row(top, height);
        top = rect.bottom() + SETTINGS_GAP;
        rect
    };
    let master = next(SETTINGS_LINE_H);
    let master_key = next(SETTINGS_LINE_H);
    let hotkey = next(SETTINGS_LINE_H);
    let morse = next(SETTINGS_LINE_H);
    let fingerprint = next(SETTINGS_LINE_H);
    let console = next(SETTINGS_LINE_H);
    let keys = next(SETTINGS_LINE_H);
    let line = next(SETTINGS_LINE_H);
    let ocr = next(SETTINGS_LINE_H);
    let theme_row = next(SETTINGS_LINE_H);
    let accent = next(SETTINGS_LINE_H);
    let fkey = next(SETTINGS_LINE_H);
    let timings = std::array::from_fn(|_| next(SETTINGS_LINE_H));
    let keywords = next(SETTINGS_ROW_H);
    let backend = next(SETTINGS_ROW_H);
    let test = next(SETTINGS_ROW_H);
    let extra_height = if has_result {
        CAPTURE_RESULT_H
    } else {
        SETTINGS_ROW_H
    };
    let extra = next(extra_height);
    SettingsRows {
        master,
        master_key,
        hotkey,
        morse,
        fingerprint,
        console,
        keys,
        line,
        ocr,
        theme: theme_row,
        accent,
        fkey,
        timings,
        keywords,
        backend,
        test,
        result: extra,
        config: (!has_result).then_some(extra),
    }
}

/// 设置页滚动区的总高度（含标题留给视口的余量）。
fn settings_inner_height(has_result: bool) -> f32 {
    // 12 行基础单行控件 + 5 行模块化延迟配置（摩斯识别后/点下载延迟、黄键台识别后/首键额外延迟、连续点击间隔）。
    let timing_count = delta_config::TimingField::ALL.len() as f32;
    let lines = (12.0 + timing_count) * SETTINGS_LINE_H;
    let rows = 2.0 * SETTINGS_ROW_H + if has_result { CAPTURE_RESULT_H } else { SETTINGS_ROW_H };
    let gaps = (15.0 + timing_count) * SETTINGS_GAP;
    theme::TITLE_BAR_H + theme::SPACE_S + lines + rows + gaps
}

/// 行内右侧控件槽：固定高度、右对齐。
fn row_slot(rect: Rect, width: f32) -> Rect {
    Rect::from_min_size(
        Pos2::new(
            rect.right() - theme::SPACE_M - width,
            rect.center().y - SETTINGS_LINE_H / 2.0,
        ),
        Vec2::new(width, SETTINGS_LINE_H),
    )
}

/// 一行设置：左侧标题 + 下方灰色小字说明 + 行底 1px 分隔。
fn paint_settings_row(painter: &egui::Painter, rect: Rect, title: &str, hint: &str, dim: bool, pal: &Palette) {
    painter.text(
        Pos2::new(rect.left() + theme::SPACE_M, rect.top() + theme::SPACE_XS),
        Align2::LEFT_TOP,
        title,
        pal.text(pal.font_body),
        if dim { pal.text_3 } else { pal.text_1 },
    );
    painter.text(
        Pos2::new(
            rect.left() + theme::SPACE_M,
            rect.top() + theme::SPACE_XS + pal.font_title,
        ),
        Align2::LEFT_TOP,
        hint,
        pal.text(pal.font_label),
        pal.text_3,
    );
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, pal.line_subtle),
    );
}

/// 行内小按钮：`enabled == false` 时置灰且不响应点击。
fn row_button(ui: &egui::Ui, rect: Rect, label: &str, enabled: bool, pal: &Palette) -> bool {
    if enabled {
        return secondary_button(ui, rect, label, pal);
    }
    let painter = ui.painter();
    painter.rect_filled(rect, pal.corner_button(), pal.bg_panel);
    painter.rect_stroke(
        rect,
        pal.corner_button(),
        Stroke::new(1.0, pal.line_subtle),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        pal.text(pal.font_label),
        pal.text_3,
    );
    false
}

/// 按源图长宽比把图塞进给定框内（居中，不拉伸）。
fn fit_inside(area: Rect, width: u32, height: u32) -> Rect {
    let source = Vec2::new(width.max(1) as f32, height.max(1) as f32);
    let scale = (area.width() / source.x).min(area.height() / source.y);
    Rect::from_center_size(area.center(), source * scale)
}

/// 设置浮层素材。
pub struct SettingsView<'a> {
    /// 识别总开关：关掉后任何热键都不再触发识别。
    pub master_enabled: bool,
    /// 翻转识别总开关的键（关着的时候它照样有效）。
    pub hotkey_master: &'a str,
    pub hotkey: &'a str,
    /// 三个强制入口的热键名（与 `hotkey` 同一套捕获机制）。
    pub hotkey_morse: &'a str,
    pub hotkey_fingerprint: &'a str,
    pub hotkey_console: &'a str,
    pub hotkey_keys: &'a str,
    pub hotkey_line: &'a str,
    /// 正在捕获的槽位（`hotkey_editing` 为真时才有意义）。
    pub hotkey_slot: delta_config::HotkeySlot,
    /// 是否正在捕获新热键（捕获态只显示提示，没有任何文本输入框）。
    pub hotkey_editing: bool,
    /// 热键冲突提示文案（`None` = 不显示）。
    pub hotkey_conflict: Option<&'a str>,
    pub ocr_enabled: bool,
    pub config_path: &'a str,
    pub capture_backend: CaptureBackend,
    pub capture_locked: bool,
    pub capture_test: Option<&'a CaptureTestResult>,
    pub capture_test_error: Option<&'a str>,
    /// 当前主题（六套之一）。
    pub theme_id: ThemeId,
    /// 自定义主色（`#RRGGBB`）。
    pub accent_override: Option<&'a str>,
    /// F 键监听总开关（spec D3 边界 3）。
    pub f_key_hook: bool,
    /// 五项可配置延迟（毫秒，顺序与 `TimingField::ALL` 一致）。
    pub timings: [u64; delta_config::TimingField::ALL.len()],
    /// 五项可配置延迟的文本输入缓冲（支持手动输入毫秒数）。
    pub timing_buffers: &'a mut [String; delta_config::TimingField::ALL.len()],
    /// 游戏窗口关键词编辑框（逗号分隔，回车/失焦提交）。
    pub keywords: &'a mut String,
}

/// 设置浮层动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsAction {
    /// 只读检查游戏与场景，不产生任何输入。
    Probe,
    ToggleOcr,
    /// 翻转「识别总开关」。
    ToggleMaster,
    /// 点某一行热键 → 进入捕获态（带槽位，四个键各自独立改）。
    EditHotkey(delta_config::HotkeySlot),
    /// 捕获成功 → 提交这一组键。
    CommitHotkey(KeyChord),
    /// 捕获态下 Esc / 点热键行外 → 只退出捕获，不关面板（D4）。
    CancelHotkey,
    /// 切换截图后端。
    SetCaptureBackend(CaptureBackend),
    /// 点「抓一帧」。
    TestCapture,
    /// 换主题（立即生效，不重启）。
    SetTheme(ThemeId),
    /// 换主色（`None` = 恢复该主题默认 accent）。
    SetAccent(Option<String>),
    /// F 键监听总开关。
    ToggleFKeyHook,
    /// 调整某一项识别/点击延时（毫秒）。
    SetTiming(delta_config::TimingField, u64),
    /// 提交关键词原文。
    CommitKeywords(String),
    Close,
}

/// 一行热键：左侧标题、右侧键名。点行进入捕获；正在捕获的那一行显示提示。
///
/// 四行共用同一套捕获结算（见 `resolve_capture`），所以这里只负责画与点。
struct HotkeyRow<'a> {
    rect: Rect,
    title: &'a str,
    key: &'a str,
    slot: delta_config::HotkeySlot,
}

fn paint_hotkey_row(
    ui: &egui::Ui,
    row: &HotkeyRow<'_>,
    viewport: Rect,
    view: &SettingsView<'_>,
    pal: &Palette,
) -> Option<SettingsAction> {
    let rect = row.rect;
    let painter = ui.painter().clone().with_clip_rect(viewport);
    paint_panel(&painter, rect, pal);
    painter.text(
        rect.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        row.title,
        pal.text(pal.font_body),
        pal.text_2,
    );
    if view.hotkey_editing && view.hotkey_slot == row.slot {
        painter.text(
            rect.left_center() + Vec2::new(rect.width() * 0.45, 0.0),
            Align2::LEFT_CENTER,
            "请按下新热键…",
            pal.text(pal.font_body),
            pal.accent,
        );
        painter.text(
            rect.right_center() - Vec2::new(theme::SPACE_M, 0.0),
            Align2::RIGHT_CENTER,
            "Esc 取消",
            pal.text(pal.font_label),
            pal.text_3,
        );
        return None;
    }
    let id = format!("hotkey-row-{}", row.slot.index());
    let response = clickable(ui, rect.intersect(viewport), &id, Sense::click());
    let (label, color) = match (view.hotkey_conflict, response.hovered()) {
        (Some(text), _) => (text.to_owned(), pal.err),
        (None, true) => (format!("{} · 点这里改", row.key), pal.accent),
        (None, false) => (row.key.to_owned(), pal.text_1),
    };
    painter.text(
        rect.right_center() - Vec2::new(theme::SPACE_M, 0.0),
        Align2::RIGHT_CENTER,
        label,
        theme::mono(pal.font_body),
        color,
    );
    response.clicked().then_some(SettingsAction::EditHotkey(row.slot))
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

/// 捕获态的收口：编辑态下点面板上任何控件，只允许「取消捕获」，不许顺手改别的设置。
///
/// 没有这一步，先算出的 `CancelHotkey` 会被后面 `ToggleOcr`/`SetTheme`/… 覆盖：
/// 开关被悄悄翻转，而捕获态还挂着，用户既不知道为什么状态变了，也退不出捕获。
fn settle_capture(editing: bool, action: Option<SettingsAction>) -> Option<SettingsAction> {
    if !editing {
        return action;
    }
    match action {
        // 捕获本身的四个动作 + 关面板 + 只读检查：都是用户明确的意图，原样放行。
        None
        | Some(SettingsAction::EditHotkey(_))
        | Some(SettingsAction::CommitHotkey(_))
        | Some(SettingsAction::CancelHotkey)
        | Some(SettingsAction::Close)
        | Some(SettingsAction::Probe) => action,
        // 其余一律降级成取消捕获（fail-closed）：新增控件默认也不会绕过捕获态。
        Some(_) => Some(SettingsAction::CancelHotkey),
    }
}

/// 滚轮 → 滚动量（设置页内部滚动；主界面永不滚动）。
fn scroll_offset(ui: &egui::Ui, viewport: Rect, inner_height: f32) -> f32 {
    let id = ui.make_persistent_id("settings-scroll");
    let max = (inner_height - viewport.height()).max(0.0);
    let wheel = ui.input(|input| input.smooth_scroll_delta.y);
    let current = ui.ctx().data(|data| data.get_temp::<f32>(id).unwrap_or(0.0));
    // 滚轮向上（正）→ 内容上移 → offset 减小。
    let updated = (current - wheel).clamp(0.0, max);
    ui.ctx().data_mut(|data| data.insert_temp(id, updated));
    updated
}

/// 设置浮层（覆在主界面上，不新开窗口）。
pub fn settings_panel(
    ui: &mut egui::Ui,
    view: SettingsView<'_>,
    pal: &Palette,
) -> Option<SettingsAction> {
    let mut action = None;
    // D4：捕获态下 Esc 只取消捕获 —— 所以「Esc 关面板」必须先排除捕获态。
    if !view.hotkey_editing && ui.ctx().input(|input| input.key_pressed(egui::Key::Escape)) {
        action = Some(SettingsAction::Close);
    }
    let full = ui.max_rect();
    let painter = ui.painter().clone();
    paint_shell(&painter, full, pal);
    let content = full.shrink2(Vec2::new(theme::SPACE_M, theme::SPACE_S));
    painter.text(
        content.left_top(),
        Align2::LEFT_TOP,
        "设置",
        pal.text(pal.font_title),
        pal.text_1,
    );

    let has_result = view.capture_test.is_some() || view.capture_test_error.is_some();
    let footer = Rect::from_min_size(
        Pos2::new(content.left(), content.bottom() - SETTINGS_FOOTER_H),
        Vec2::new(content.width(), SETTINGS_FOOTER_H),
    );
    // 滚动区 = 标题下方到动作条上方。
    let viewport = Rect::from_min_max(
        Pos2::new(content.left(), content.top() + theme::TITLE_BAR_H),
        Pos2::new(content.right(), footer.top() - theme::SPACE_S),
    );
    let inner_height = settings_inner_height(has_result);
    let offset = scroll_offset(ui, viewport, inner_height);
    let inner = Rect::from_min_size(
        Pos2::new(viewport.left(), viewport.top() - offset),
        Vec2::new(viewport.width(), inner_height.max(viewport.height())),
    );
    let rows = settings_rows(inner, has_result);
    // 只画视口内的像素，并且只在视口内响应点击（滚出视野的行不该能被点到）。
    let painter = ui.painter().clone().with_clip_rect(viewport);
    let hit = |rect: Rect| rect.intersect(viewport);

    // ── 识别总开关（关掉后任何热键都不再触发，连截图都不抓）──────
    if toggle_row(
        ui,
        hit(rows.master),
        if view.master_enabled {
            "识别总开关 · 开"
        } else {
            "识别总开关 · 已暂停"
        },
        view.master_enabled,
        pal,
    ) {
        action = Some(SettingsAction::ToggleMaster);
    }

    // ── 五个热键（各自点一下即可捕获）────────────────────────────
    let hotkey_rows = [
        HotkeyRow {
            rect: rows.master_key,
            title: "总开关热键",
            key: view.hotkey_master,
            slot: delta_config::HotkeySlot::Master,
        },
        HotkeyRow {
            rect: rows.hotkey,
            title: "全局热键",
            key: view.hotkey,
            slot: delta_config::HotkeySlot::Scene,
        },
        HotkeyRow {
            rect: rows.morse,
            title: "摩斯热键",
            key: view.hotkey_morse,
            slot: delta_config::HotkeySlot::Morse,
        },
        HotkeyRow {
            rect: rows.fingerprint,
            title: "指纹热键",
            key: view.hotkey_fingerprint,
            slot: delta_config::HotkeySlot::Fingerprint,
        },
        HotkeyRow {
            rect: rows.console,
            title: "操作台热键",
            key: view.hotkey_console,
            slot: delta_config::HotkeySlot::Console,
        },
        HotkeyRow {
            rect: rows.keys,
            title: "黄键台热键",
            key: view.hotkey_keys,
            slot: delta_config::HotkeySlot::Keys,
        },
        HotkeyRow {
            rect: rows.line,
            title: "横线台热键",
            key: view.hotkey_line,
            slot: delta_config::HotkeySlot::Line,
        },
    ];
    // 捕获结算先于既有控件：这一帧的按键/取消不会被按钮点击覆盖掉。
    if action.is_none() && view.hotkey_editing {
        let editing_rect = hotkey_rows
            .iter()
            .find(|row| row.slot == view.hotkey_slot)
            .map_or(rows.hotkey, |row| row.rect);
        action = resolve_capture(ui, true, hit(editing_rect));
    }
    for row in &hotkey_rows {
        if let Some(produced) = paint_hotkey_row(ui, row, viewport, &view, pal) {
            action = Some(produced);
        }
    }

    // ── 文字兜底开关 ──────────────────────────────────────────
    if toggle_row(ui, hit(rows.ocr), "文字兜底", view.ocr_enabled, pal) {
        action = Some(SettingsAction::ToggleOcr);
    }

    // ── 主题（六套，点一下立即生效）──────────────────────────────
    paint_theme_row(ui, rows.theme, viewport, view.theme_id, pal)
        .into_iter()
        .for_each(|theme| action = Some(SettingsAction::SetTheme(theme)));

    // ── 主色（色相预设 + 恢复默认）───────────────────────────────
    if let Some(produced) = paint_accent_row(ui, rows.accent, viewport, &view, pal) {
        action = Some(produced);
    }

    // ── F 键监听 ─────────────────────────────────────────────
    if toggle_row(ui, hit(rows.fkey), "F 键监听", view.f_key_hook, pal) {
        action = Some(SettingsAction::ToggleFKeyHook);
    }

    // ── 模块化识别/点击延迟设置（点 `-`/`+` 步进调节，中间输入框支持手动填入毫秒值）──
    for (index, field) in delta_config::TimingField::ALL.into_iter().enumerate() {
        if let Some(produced) = paint_timing_row(
            ui,
            rows.timings[index],
            viewport,
            field,
            view.timings[index],
            &mut view.timing_buffers[index],
            pal,
        ) {
            action = Some(produced);
        }
    }

    // ── 游戏窗口关键词（逗号分隔，回车/失焦提交）──────────────────
    paint_settings_row(
        &painter,
        rows.keywords,
        "游戏窗口关键词",
        "命中才响应 F 键，逗号分隔",
        false,
        pal,
    );
    let field = hit(row_slot(rows.keywords, content.width() * 0.45));
    let response = ui.put(
        field,
        egui::TextEdit::singleline(&mut *view.keywords).hint_text("三角洲, Delta Force"),
    );
    if response.lost_focus()
        || (response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)))
    {
        action = Some(SettingsAction::CommitKeywords(view.keywords.clone()));
    }

    // ── 截图方式（自绘下拉，不引入 ComboBox 的默认外观）──────────────
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
        pal,
    );
    // 绘制用未裁剪的原始矩形（滚出视口时由 painter 的 clip 负责裁掉），
    // 命中判定才用 viewport 裁剪——否则整行会跟着裁剪边界跑位。
    let value_slot = row_slot(rows.backend, BACKEND_VALUE_W);
    let value_hit = value_slot.intersect(viewport);
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
            pal.text(pal.font_body),
            pal.text_3,
        );
        false
    } else {
        let response = clickable(ui, value_hit, "capture-backend-value", Sense::click());
        painter.text(
            value_slot.right_center(),
            Align2::RIGHT_CENTER,
            value_text,
            pal.text(pal.font_body),
            if response.hovered() {
                pal.accent
            } else {
                pal.text_1
            },
        );
        response.clicked()
    };
    if value_clicked {
        menu_open = !menu_open;
    }

    // ── 测试截图 ─────────────────────────────────────────────
    paint_settings_row(
        &painter,
        rows.test,
        "测试截图",
        "抓一帧验证当前后端能出图",
        locked,
        pal,
    );
    if row_button(ui, hit(row_slot(rows.test, ROW_BUTTON_W)), "抓一帧", !locked, pal) {
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
            pal.corner_button(),
            Stroke::new(1.0, pal.line_subtle),
            egui::StrokeKind::Inside,
        );
        painter.text(
            Pos2::new(rows.result.center().x, rows.result.bottom() - theme::SPACE_XS),
            Align2::CENTER_BOTTOM,
            result.summary(),
            theme::mono(pal.font_label),
            pal.text_2,
        );
    } else if let Some(reason) = view.capture_test_error {
        // 错误码在被截断时最容易被丢掉，所以交给 egui 自动换行。
        let mut error_ui = ui.new_child(egui::UiBuilder::new().max_rect(hit(rows.result)));
        error_ui.add(
            egui::Label::new(
                egui::RichText::new(reason)
                    .size(pal.font_label)
                    .color(pal.err),
            )
            .wrap(),
        );
    }

    // ── 配置路径（有测试结果时让位）──────────────────────────────
    if let Some(config) = rows.config {
        painter.text(
            config.left_top(),
            Align2::LEFT_TOP,
            "配置文件",
            pal.text(pal.font_label),
            pal.text_3,
        );
        painter.text(
            Pos2::new(config.left(), config.top() + theme::SPACE_L),
            Align2::LEFT_TOP,
            view.config_path,
            theme::mono(pal.font_label),
            pal.text_3,
        );
    }

    // ── 底部动作条（固定，不随滚动）──────────────────────────────
    let probe = Rect::from_min_size(footer.min, Vec2::new(footer.width() * 0.48, footer.height()));
    if secondary_button(ui, probe, "检查游戏", pal) {
        action = Some(SettingsAction::Probe);
    }
    let back = Rect::from_min_size(
        Pos2::new(probe.right() + theme::SPACE_S, footer.top()),
        Vec2::new(footer.width() - probe.width() - theme::SPACE_S, footer.height()),
    );
    if filled_button(ui, back, "返回", true, pal) {
        action = Some(SettingsAction::Close);
    }

    // ── 下拉最后画：压在其它行之上，点击优先级也高于下面的按钮 ──────────
    let mut option_clicked = false;
    if menu_open {
        let item_height = SETTINGS_LINE_H;
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
        let menu_painter = ui.painter().with_clip_rect(full);
        paint_panel(&menu_painter, menu, pal);
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
                (true, _) => pal.accent,
                (false, true) => pal.text_1,
                (false, false) => pal.text_2,
            };
            menu_painter.text(
                item.left_center() + Vec2::new(theme::SPACE_S, 0.0),
                Align2::LEFT_CENTER,
                backend_label(backend),
                pal.text(pal.font_label),
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
    settle_capture(view.hotkey_editing, action)
}

/// 延迟调节行：左侧标题，右侧 `-` 按钮 / 可手动编辑的数字输入框 + `ms` / `+` 按钮。
fn paint_timing_row(
    ui: &mut egui::Ui,
    rect: Rect,
    viewport: Rect,
    field: delta_config::TimingField,
    current_ms: u64,
    buffer: &mut String,
    pal: &Palette,
) -> Option<SettingsAction> {
    let painter = ui.painter().clone().with_clip_rect(viewport);
    paint_panel(&painter, rect, pal);
    painter.text(
        rect.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        field.label(),
        pal.text(pal.font_body),
        pal.text_2,
    );
    let btn_w = 24.0;
    let unit_w = 20.0;
    let edit_w = 52.0;
    let right = rect.right() - theme::SPACE_S;
    let plus_rect = Rect::from_min_size(
        Pos2::new(right - btn_w, rect.top() + 3.0),
        Vec2::new(btn_w, rect.height() - 6.0),
    );
    let unit_rect = Rect::from_min_size(
        Pos2::new(plus_rect.left() - 2.0 - unit_w, rect.top() + 3.0),
        Vec2::new(unit_w, rect.height() - 6.0),
    );
    let val_rect = Rect::from_min_size(
        Pos2::new(unit_rect.left() - 2.0 - edit_w, rect.top() + 3.0),
        Vec2::new(edit_w, rect.height() - 6.0),
    );
    let minus_rect = Rect::from_min_size(
        Pos2::new(val_rect.left() - 4.0 - btn_w, rect.top() + 3.0),
        Vec2::new(btn_w, rect.height() - 6.0),
    );
    let step = field.step_ms();
    let min_ms = field.min_ms();
    let max_ms = field.max_ms();
    let mut out = None;

    // `-` 按钮：每行独立 ID（避免 5 行共用 "-" 导致点击失灵），且用 viewport 裁剪绘制。
    let can_minus = current_ms > min_ms;
    let minus_hit = minus_rect.intersect(viewport);
    let minus_resp = clickable(
        ui,
        minus_hit,
        &format!("timing-minus-{}", field.label()),
        if can_minus { Sense::click() } else { Sense::hover() },
    );
    painter.rect_filled(minus_rect, pal.corner_button(), pal.bg_panel);
    painter.rect_stroke(
        minus_rect,
        pal.corner_button(),
        Stroke::new(1.0, if can_minus { pal.line_strong } else { pal.line_subtle }),
        egui::StrokeKind::Inside,
    );
    painter.text(
        minus_rect.center(),
        Align2::CENTER_CENTER,
        "-",
        pal.text(pal.font_body),
        if !can_minus {
            pal.text_3
        } else if minus_resp.hovered() {
            pal.accent
        } else {
            pal.text_1
        },
    );
    if can_minus && minus_resp.clicked() {
        let next_ms = current_ms.saturating_sub(step).max(min_ms);
        *buffer = next_ms.to_string();
        out = Some(SettingsAction::SetTiming(field, next_ms));
    }

    // 中间手动输入框 + `ms` 单位。
    let val_hit = val_rect.intersect(viewport);
    if val_hit.height() >= 12.0 {
        let edit_resp = ui.put(
            val_hit,
            egui::TextEdit::singleline(buffer)
                .id(ui.make_persistent_id(format!("timing-edit-{}", field.label())))
                .font(theme::mono(pal.font_label)),
        );
        if edit_resp.lost_focus()
            || (edit_resp.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)))
        {
            let digits: String = buffer.chars().filter(char::is_ascii_digit).collect();
            if let Ok(parsed) = digits.parse::<u64>() {
                let clamped = parsed.clamp(min_ms, max_ms);
                *buffer = clamped.to_string();
                out = Some(SettingsAction::SetTiming(field, clamped));
            } else {
                *buffer = current_ms.to_string();
            }
        } else if !edit_resp.has_focus() && buffer.trim() != current_ms.to_string() {
            *buffer = current_ms.to_string();
        }
    }
    painter.text(
        unit_rect.left_center(),
        Align2::LEFT_CENTER,
        "ms",
        theme::mono(pal.font_label),
        pal.text_3,
    );

    // `+` 按钮：每行独立 ID，且用 viewport 裁剪绘制。
    let can_plus = current_ms < max_ms;
    let plus_hit = plus_rect.intersect(viewport);
    let plus_resp = clickable(
        ui,
        plus_hit,
        &format!("timing-plus-{}", field.label()),
        if can_plus { Sense::click() } else { Sense::hover() },
    );
    painter.rect_filled(plus_rect, pal.corner_button(), pal.bg_panel);
    painter.rect_stroke(
        plus_rect,
        pal.corner_button(),
        Stroke::new(1.0, if can_plus { pal.line_strong } else { pal.line_subtle }),
        egui::StrokeKind::Inside,
    );
    painter.text(
        plus_rect.center(),
        Align2::CENTER_CENTER,
        "+",
        pal.text(pal.font_body),
        if !can_plus {
            pal.text_3
        } else if plus_resp.hovered() {
            pal.accent
        } else {
            pal.text_1
        },
    );
    if can_plus && plus_resp.clicked() {
        let next_ms = current_ms.saturating_add(step).min(max_ms);
        *buffer = next_ms.to_string();
        out = Some(SettingsAction::SetTiming(field, next_ms));
    }
    out
}

/// 开关行（左标签 + 右 28×16 开关轨道）。
fn toggle_row(ui: &egui::Ui, rect: Rect, label: &str, on: bool, pal: &Palette) -> bool {
    let response = clickable(ui, rect, label, Sense::click());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        pal.corner_button(),
        if response.hovered() {
            pal.bg_raised
        } else {
            pal.bg_panel
        },
    );
    painter.rect_stroke(
        rect,
        pal.corner_button(),
        Stroke::new(1.0, pal.line_subtle),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        label,
        pal.text(pal.font_body),
        if on { pal.text_1 } else { pal.text_2 },
    );
    switch(
        painter,
        rect.right_center() - Vec2::new(theme::SPACE_L, 0.0),
        on,
        pal,
    );
    response.clicked()
}

/// 主题行：六个名字，选中项用 accent 高亮；返回新选中的主题（没点 = `None`）。
fn paint_theme_row(
    ui: &egui::Ui,
    rect: Rect,
    viewport: Rect,
    current: ThemeId,
    pal: &Palette,
) -> Option<ThemeId> {
    let painter = ui.painter();
    painter.text(
        rect.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        "主题",
        pal.text(pal.font_body),
        pal.text_2,
    );
    let mut chosen = None;
    let chip_w = 30.0;
    let start = rect.right() - theme::SPACE_M - chip_w * theme::ALL.len() as f32;
    for (index, id) in theme::ALL.into_iter().enumerate() {
        let chip = Rect::from_min_size(
            Pos2::new(start + chip_w * index as f32, rect.top()),
            Vec2::new(chip_w, rect.height()),
        );
        // 绘制用原始 chip（越界由 painter clip 处理），命中才与视口求交。
        let response = clickable(
            ui,
            chip.intersect(viewport),
            &format!("theme-{}", id.token()),
            Sense::click(),
        );
        let selected = id == current;
        let color = match (selected, response.hovered()) {
            (true, _) => pal.accent,
            (false, true) => pal.text_1,
            (false, false) => pal.text_3,
        };
        painter.text(
            chip.center(),
            Align2::CENTER_CENTER,
            id.label(),
            pal.text(pal.font_label),
            color,
        );
        if selected {
            painter.rect_stroke(
                chip.shrink2(Vec2::new(2.0, 2.0)),
                pal.corner_button(),
                Stroke::new(1.0, pal.accent),
                egui::StrokeKind::Inside,
            );
        }
        if response.clicked() && !selected {
            chosen = Some(id);
        }
    }
    chosen
}

/// 主色行：8 个色相预设 + 「恢复默认」。改动即时预览（下一帧就用新色板重绘）。
fn paint_accent_row(
    ui: &egui::Ui,
    rect: Rect,
    viewport: Rect,
    view: &SettingsView<'_>,
    pal: &Palette,
) -> Option<SettingsAction> {
    let painter = ui.painter();
    painter.text(
        rect.left_center() + Vec2::new(theme::SPACE_M, 0.0),
        Align2::LEFT_CENTER,
        "主色",
        pal.text(pal.font_body),
        pal.text_2,
    );
    let dot = 14.0;
    let step = dot + 8.0;
    let reset_w = 5.0 * pal.font_label;
    let start = rect.right() - theme::SPACE_M - reset_w - step * ACCENT_HUES.len() as f32;
    let current = view.accent_override.and_then(theme::parse_hex);
    let base = *theme::palette(view.theme_id);
    let mut action = None;
    for (index, hue) in ACCENT_HUES.into_iter().enumerate() {
        let center = Pos2::new(
            start + step * index as f32 + dot / 2.0,
            rect.center().y,
        );
        let swatch = Rect::from_center_size(center, Vec2::splat(dot));
        let response = clickable(
            ui,
            swatch.intersect(viewport),
            &format!("accent-hue-{}", hue as u32),
            Sense::click(),
        );
        let color = base.with_accent(hue).accent;
        painter.circle_filled(center, dot / 2.0, color);
        if current.is_some_and(|selected| selected == color) {
            painter.circle_stroke(center, dot / 2.0 + 2.0, Stroke::new(1.0, pal.text_1));
        }
        // 悬停放大视觉反馈（不改变布局）。
        if response.hovered() {
            painter.circle_stroke(center, dot / 2.0 + 3.0, Stroke::new(1.0, pal.accent));
        }
        if response.clicked() {
            action = Some(SettingsAction::SetAccent(Some(theme::to_hex(color))));
        }
    }
    let reset = Rect::from_min_size(
        Pos2::new(
            rect.right() - theme::SPACE_M - reset_w,
            rect.center().y - rect.height() / 2.0,
        ),
        Vec2::new(reset_w, rect.height()),
    );
    if link(ui, reset, "恢复默认", pal.font_label, pal) {
        action = Some(SettingsAction::SetAccent(None));
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Toggles;

    fn stage() -> Rect {
        Rect::from_min_size(Pos2::new(100.0, 200.0), Vec2::new(400.0, 300.0))
    }

    fn content_for(size: [f32; 2]) -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(size[0], size[1]))
            .shrink2(Vec2::new(theme::SPACE_M, theme::SPACE_S))
    }

    fn pal() -> Palette {
        *theme::palette(ThemeId::default())
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

    /// L-A 分段必须在 400×420 与最小 340×384 下都放得下：10 格开关全区可见、
    /// 结果区不压主按钮、所有区段都在内容区内。
    #[test]
    fn main_rows_match_the_l_a_segments_on_both_window_sizes() {
        for size in [theme::WINDOW_SIZE, theme::WINDOW_MIN] {
            let content = content_for(size);
            let rows = main_rows(content, UiState::default().grid().len());
            for (name, rect) in [
                ("标题条", rows.title),
                ("结果区", rows.result),
                ("主按钮", rows.primary),
                ("状态行", rows.status),
                ("底部", rows.footer),
            ] {
                assert!(
                    content.contains_rect(rect),
                    "{size:?} 下「{name}」溢出：{rect:?}"
                );
            }
            for (index, cell) in rows.grid.iter().enumerate() {
                assert!(
                    content.contains_rect(*cell),
                    "{size:?} 下第 {index} 格开关溢出：{cell:?}"
                );
                assert!(cell.height() >= theme::SWITCH_H + 8.0, "开关行被压扁");
            }
            // 纵向不允许互相压。
            assert!(rows.title.bottom() <= rows.result.top());
            assert!(rows.result.bottom() <= rows.grid[0].top());
            assert!(
                rows.grid.last().expect("网格不得为空").bottom() <= rows.primary.top(),
                "{size:?} 下开关网格压到主按钮"
            );
            assert!(rows.primary.bottom() <= rows.status.top(), "结果区压到底部区段");
            assert!(rows.status.bottom() <= rows.footer.top());
            // 同一行的三列不得重叠，且宽度相同。
            assert!((rows.grid[0].width() - rows.grid[1].width()).abs() < 0.01);
            assert!(rows.grid[0].right() <= rows.grid[1].left());
            assert!(rows.grid[1].right() <= rows.grid[2].left(), "一行三列，各列不重叠");
        }
    }

    /// 3 列的硬约束：一格里的「文案 + 28px 开关」必须塞进**最小窗**的一格。
    ///
    /// 反例：给某功能起 6 个汉字的名字 / 偷偷把字号调大 → 文案压到开关上或画出格子。
    #[test]
    fn grid_labels_fit_a_cell_at_the_minimum_window_size() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let cells = UiState::default().grid();
        let rect = main_rows(content_for(theme::WINDOW_MIN), cells.len()).grid[0];
        let pal = pal();
        let mut widest = 0.0_f32;
        ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ctx, |ui| {
                    for cell in &cells {
                        let galley = ui.painter().layout_no_wrap(
                            cell.label().to_owned(),
                            pal.text(pal.font_label + 1.0),
                            pal.text_1,
                        );
                        let width = galley.size().x;
                        assert!(
                            width + theme::SPACE_S + theme::SWITCH_W + theme::SPACE_S
                                <= rect.width(),
                            "「{}」宽 {width:.1}px，塞不进 {:.1}px 的格子",
                            cell.label(),
                            rect.width()
                        );
                        widest = widest.max(width);
                    }
                });
        })
        .textures_delta
        .clear();
        assert!(widest > 0.0, "字体没装上，量不出宽度");
    }

    /// U-T6：网格行数跟着格数走（3 列一行流式排），不再硬编码 2 行。
    ///
    /// 反例：写死 `[Rect; 6]` / 写死 2 行 → 10 格时第 7~10 格没有落点（或全挤在一行）。
    #[test]
    fn grid_rows_follow_the_cell_count() {
        assert_eq!(grid_rows(9), 3, "9 格 = 3 行");
        assert_eq!(grid_rows(10), 4, "10 格 = 4 行（最后一格单独占一行）");
        assert_eq!(grid_rows(0), 0, "0 格不画行");

        let content = content_for(theme::WINDOW_SIZE);
        let rows = main_rows(content, 10);
        assert_eq!(rows.grid.len(), 10, "落点数量必须等于格数");
        let row_step = theme::GRID_ROW_H + theme::GRID_ROW_GAP;
        for index in [3, 6, 9] {
            assert!(
                (rows.grid[index].top() - rows.grid[index - 3].top() - row_step).abs() < 0.01,
                "第 {index} 格必须比第 {} 格低一行",
                index - 3
            );
            assert!(
                (rows.grid[index].left() - rows.grid[0].left()).abs() < 0.01,
                "每行都从第一列起排"
            );
        }
        // 9 格时第 7~9 格仍在第 3 行（不能因为少一格就多长一行）。
        let nine = main_rows(content, 9);
        assert_eq!(nine.grid.len(), 9);
        assert!((nine.grid[8].top() - nine.grid[6].top()).abs() < 0.01, "9 格只有 3 行");
    }

    /// 400×420 下分段落在 §3.1 表格的标称位置（允许 ±2px：富余空间补给了结果区）。
    #[test]
    fn nominal_segments_land_on_the_spec_table() {
        let content = content_for(theme::WINDOW_SIZE);
        let rows = main_rows(content, 10);
        let y = |rect: Rect| rect.top() - content.top();
        assert!((y(rows.title) - 0.0).abs() < 0.01);
        assert!((y(rows.result) - 40.0).abs() < 0.01, "结果区应在 48（内容内 40）");
        assert!((y(rows.primary) - 284.0).abs() < 0.01, "主按钮应在 276（内容内 284）");
        // 网格从 3 行变 4 行（+38px），这 38px 从结果区出：112 → 74（≥ RESULT_MIN_H）。
        assert!((rows.result.height() - 74.0).abs() < 0.01, "富余高度补给结果区");
        assert!(rows.result.height() >= theme::RESULT_MIN_H, "结果区不得低于可压缩下限");
    }

    #[test]
    fn pct_rect_is_clamped_inside_the_preview() {
        let drawn = Rect::from_min_size(Pos2::new(80.0, 150.0), Vec2::new(600.0, 500.0));
        let clamped = pct_rect_from_screen(stage(), drawn);
        assert_eq!(clamped.x, 0.0);
        assert_eq!(clamped.y, 0.0);
        assert!(clamped.x + clamped.w <= 1.0 + 1e-6);
        assert!(clamped.y + clamped.h <= 1.0 + 1e-6);
    }

    #[test]
    fn result_area_labels_cover_the_three_scenarios() {
        assert_eq!(source_label(SceneKind::Morse), "摩斯");
        assert_eq!(source_label(SceneKind::Fingerprint), "指纹");
        assert_eq!(source_label(SceneKind::Console), "操作台");
        assert_eq!(source_label(SceneKind::Unknown), "识别结果");
    }

    /// 捕获态下点面板控件只能「取消捕获」（D4）：开关不许被顺手翻掉，捕获态不许挂着。
    #[test]
    fn settle_capture_only_cancels_when_a_control_is_clicked_while_editing() {
        let chord = KeyChord::capture(egui::Key::F9, egui::Modifiers::NONE).expect("F9 可捕获");
        // 捕获本身的动作 + 关面板 + 只读检查：用户明确意图，原样放行。
        let kept = [
            None,
            Some(SettingsAction::EditHotkey(delta_config::HotkeySlot::Morse)),
            Some(SettingsAction::CommitHotkey(chord)),
            Some(SettingsAction::CancelHotkey),
            Some(SettingsAction::Close),
            Some(SettingsAction::Probe),
        ];
        for action in kept {
            assert_eq!(
                settle_capture(true, action.clone()),
                action,
                "捕获态下这是明确意图，不该被降级：{action:?}"
            );
            assert_eq!(
                settle_capture(false, action.clone()),
                action,
                "非编辑态一律原样：{action:?}"
            );
        }
        // 其余控件：编辑态下一律降级为「取消捕获」，非编辑态保持原样。
        let controls = [
            SettingsAction::ToggleMaster,
            SettingsAction::ToggleOcr,
            SettingsAction::ToggleFKeyHook,
            SettingsAction::SetTiming(delta_config::TimingField::MorseRecognizeDelay, 40),
            SettingsAction::SetTheme(ThemeId::default()),
            SettingsAction::SetAccent(None),
            SettingsAction::SetCaptureBackend(CaptureBackend::Wgc),
            SettingsAction::TestCapture,
            SettingsAction::CommitKeywords("三角洲".to_owned()),
        ];
        for control in controls {
            assert_eq!(
                settle_capture(true, Some(control.clone())),
                Some(SettingsAction::CancelHotkey),
                "捕获态下控件动作必须降级：{control:?}"
            );
            assert_eq!(
                settle_capture(false, Some(control.clone())),
                Some(control),
                "非编辑态不该被动过"
            );
        }
    }

    /// 设置页的行骨架在两种窗口尺寸下都不越出滚动内容区（滚动由面板负责）。
    #[test]
    fn settings_rows_stay_inside_the_scroll_area() {
        for has_result in [false, true] {
            let inner_height = settings_inner_height(has_result);
            let inner = Rect::from_min_size(Pos2::ZERO, Vec2::new(376.0, inner_height));
            let rows = settings_rows(inner, has_result);
            for (name, rect) in [
                ("总开关", rows.master),
                ("总开关热键", rows.master_key),
                ("热键", rows.hotkey),
                ("兜底", rows.ocr),
                ("主题", rows.theme),
                ("主色", rows.accent),
                ("F 键监听", rows.fkey),
                ("关键词", rows.keywords),
                ("截图方式", rows.backend),
                ("测试截图", rows.test),
                ("结果区", rows.result),
            ] {
                assert!(
                    inner.contains_rect(rect),
                    "has_result={has_result} 时「{name}」溢出滚动区：{rect:?}"
                );
            }
            assert_eq!(rows.config.is_some(), !has_result);
            // 相邻行不重叠。
            assert!(rows.master.bottom() <= rows.master_key.top());
            assert!(rows.master_key.bottom() <= rows.hotkey.top());
            assert!(rows.theme.bottom() <= rows.accent.top());
            assert!(rows.fkey.bottom() <= rows.timings[0].top());
            assert!(rows.timings[delta_config::TimingField::ALL.len() - 1].bottom() <= rows.keywords.top());
        }
    }

    #[test]
    fn settings_scrolls_only_when_the_content_is_taller_than_the_viewport() {
        // 400×420：有测试结果时内容高于视口 → 必须能滚（主界面不滚，设置页允许）。
        let content = content_for(theme::WINDOW_SIZE);
        let viewport = content.height() - theme::TITLE_BAR_H - SETTINGS_FOOTER_H - theme::SPACE_S;
        assert!(
            settings_inner_height(true) > viewport,
            "带结果时设置页内容应高于视口，滚动条才有意义"
        );
        // 最小窗口下必然要滚。
        let content = content_for(theme::WINDOW_MIN);
        let viewport = content.height() - theme::TITLE_BAR_H - SETTINGS_FOOTER_H - theme::SPACE_S;
        assert!(settings_inner_height(false) > viewport);
    }

    /// 在一帧里跑一次设置浮层，返回它给出的动作。
    fn panel_action(events: Vec<egui::Event>, hotkey_editing: bool) -> Option<SettingsAction> {
        let ctx = egui::Context::default();
        let mut action = None;
        let input = egui::RawInput {
            events,
            ..egui::RawInput::default()
        };
        let mut keywords = "三角洲, Delta Force".to_owned();
        ctx.run_ui(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                action = settings_panel(
                    ui,
                    SettingsView {
                        master_enabled: true,
                        hotkey_master: "F9",
                        hotkey: "F8",
                        hotkey_morse: "F5",
                        hotkey_fingerprint: "F6",
                        hotkey_console: "F7",
                        hotkey_keys: "F10",
                        hotkey_line: "F11",
                        hotkey_slot: delta_config::HotkeySlot::Scene,
                        hotkey_editing,
                        hotkey_conflict: None,
                        ocr_enabled: true,
                        config_path: "C:/cfg.toml",
                        capture_backend: CaptureBackend::Wgc,
                        capture_locked: false,
                        capture_test: None,
                        capture_test_error: None,
                        theme_id: ThemeId::default(),
                        accent_override: None,
                        f_key_hook: true,
                        timings: [20, 600, 100, 50, 50],
                        timing_buffers: &mut [
                            "20".to_owned(),
                            "600".to_owned(),
                            "100".to_owned(),
                            "50".to_owned(),
                            "50".to_owned(),
                        ],
                        keywords: &mut keywords,
                    },
                    &pal(),
                );
            });
        })
        .textures_delta
        .clear();
        action
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
        let action = panel_action(
            vec![key_event(egui::Key::Escape, egui::Modifiers::default())],
            true,
        );
        assert_eq!(action, Some(SettingsAction::CancelHotkey));

        let action = panel_action(
            vec![key_event(egui::Key::Escape, egui::Modifiers::default())],
            false,
        );
        assert_eq!(action, Some(SettingsAction::Close));
    }

    #[test]
    fn capturing_a_key_commits_the_chord_and_rejects_plain_f() {
        match panel_action(vec![key_event(egui::Key::F9, egui::Modifiers::default())], true) {
            Some(SettingsAction::CommitHotkey(chord)) => {
                assert_eq!(chord.main_name(), "F9");
                assert_eq!(chord.modifiers(), 0);
            }
            other => panic!("按 F9 必须提交 F9，实际 {other:?}"),
        }
        // 无修饰的 F 会被全局独占 → 捕获阶段拒绝，继续等待（不提交、不取消）。
        assert_eq!(
            panel_action(vec![key_event(egui::Key::F, egui::Modifiers::default())], true),
            None,
            "F 不得被捕获成全局热键"
        );
        // 不可捕获的键与纯修饰键同理。
        assert_eq!(
            panel_action(
                vec![key_event(egui::Key::Questionmark, egui::Modifiers::default())],
                true
            ),
            None
        );
    }

    #[test]
    fn backend_labels_cover_both_options_and_flag_the_compat_risk() {
        for backend in CAPTURE_BACKENDS {
            assert!(!backend_label(backend).is_empty());
        }
        assert_eq!(CAPTURE_BACKENDS, [CaptureBackend::Wgc, CaptureBackend::BitBlt]);
        assert!(backend_label(CaptureBackend::BitBlt).contains("截不到独占全屏"));
    }

    #[test]
    fn thumbnail_fits_inside_its_box_without_stretching() {
        let area = Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(CAPTURE_THUMB_MAX[0], CAPTURE_THUMB_MAX[1]),
        );
        let fitted = fit_inside(area, 1920, 1080);
        assert!(area.contains_rect(fitted), "缩略图不得越出结果区");
        let ratio = fitted.width() / fitted.height();
        assert!((ratio - 16.0 / 9.0).abs() < 0.05, "长宽比被拉伸：{ratio}");
    }

    /// 五态 + 按钮谓词在渲染路径上的关键分支（不启动 GUI，只跑状态推导）。
    #[test]
    fn idle_state_draws_no_empty_card_and_button_copy_changes() {
        let mut state = UiState::default();
        assert_eq!(state.result_state(), ResultState::Idle);
        state.missing = vec!["摩斯区域 #1".to_owned()];
        assert!(!state.is_calibrated());
        state.missing.clear();
        state.toggles = Toggles::from_flags([false; 7]);
        assert_eq!(state.primary_state(), PrimaryState::NoFlow);
        state.result = Some(crate::state::morse_result("427", true));
        assert_eq!(
            state.result_content().map(|content| content.headline.as_str()),
            Some("4 2 7"),
            "结果区必须能拿到渲染文本"
        );
    }

    /// 无头跑一帧主界面与设置页：六态 × 两种窗口尺寸 × 收起态都必须画得出来。
    ///
    /// 这不是实机验证（不建窗口、不注入任何输入），只是把「画一帧会不会 panic」这条
    /// 底线钉住 —— 与设置页既有用例同一个套路。
    #[test]
    fn main_window_and_settings_render_every_phase_at_both_sizes() {
        let phases = [
            Phase::Idle,
            Phase::Running,
            Phase::Success,
            Phase::Unverified,
            Phase::Unrecognized,
            Phase::TimedOut,
        ];
        for size in [theme::WINDOW_SIZE, theme::WINDOW_MIN] {
            let full = Rect::from_min_size(Pos2::ZERO, Vec2::new(size[0], size[1]));
            for phase in phases {
                for collapsed in [false, true] {
                    let ctx = egui::Context::default();
                    let mut state = UiState {
                        phase,
                        collapsed,
                        ..UiState::default()
                    };
                    state.set_result(crate::state::morse_result("427", true));
                    let input = egui::RawInput {
                        screen_rect: Some(full),
                        ..egui::RawInput::default()
                    };
                    ctx.run_ui(input, |ctx| {
                        egui::CentralPanel::default()
                            .frame(egui::Frame::NONE)
                            .show(ctx, |ui| {
                                let _ = main_window(ui, &mut state, &pal());
                            });
                    })
                    .textures_delta
                    .clear();
                }
            }

            // 设置页：六套主题 × 有无测试结果（滚动必须自己兜住，不 panic）。
            for theme_id in theme::ALL {
                for has_result in [false, true] {
                    let ctx = egui::Context::default();
                    let mut keywords = "三角洲, Delta Force".to_owned();
                    let input = egui::RawInput {
                        screen_rect: Some(full),
                        ..egui::RawInput::default()
                    };
                    ctx.run_ui(input, |ctx| {
                        egui::CentralPanel::default()
                            .frame(egui::Frame::NONE)
                            .show(ctx, |ui| {
                                let _ = settings_panel(
                                    ui,
                                    SettingsView {
                                        master_enabled: true,
                                        hotkey_master: "F9",
                                        hotkey: "F8",
                                        hotkey_morse: "F5",
                                        hotkey_fingerprint: "F6",
                                        hotkey_console: "F7",
                                        hotkey_keys: "F10",
                                        hotkey_line: "F11",
                                        hotkey_slot: delta_config::HotkeySlot::Scene,
                                        hotkey_editing: false,
                                        hotkey_conflict: None,
                                        ocr_enabled: true,
                                        config_path: "C:/cfg.toml",
                                        capture_backend: CaptureBackend::Wgc,
                                        capture_locked: false,
                                        capture_test: None,
                                        capture_test_error: has_result.then_some("WGC 初始化失败：0x80070005"),
                                        theme_id,
                                        accent_override: Some("#B23A2A"),
                                        f_key_hook: true,
                                        timings: [20, 600, 100, 50, 50],
                                        timing_buffers: &mut [
                                            "20".to_owned(),
                                            "600".to_owned(),
                                            "100".to_owned(),
                                            "50".to_owned(),
                                            "50".to_owned(),
                                        ],
                                        keywords: &mut keywords,
                                    },
                                    theme::palette(theme_id),
                                );
                            });
                    })
                    .textures_delta
                    .clear();
                }
            }
        }
    }

    #[test]
    fn with_alpha_keeps_rgb_and_scales_alpha() {
        let accent = pal().accent;
        let half = theme::with_alpha(accent, 0.5);
        let [r, g, b, a] = half.to_srgba_unmultiplied();
        let [er, eg, eb, _] = accent.to_srgba_unmultiplied();
        for (actual, expected) in [(r, er), (g, eg), (b, eb)] {
            assert!(
                (i32::from(actual) - i32::from(expected)).abs() <= 2,
                "通道 {actual} 与 {expected} 偏差过大"
            );
        }
        assert!((127..=128).contains(&a));
    }
}
