//! 设计 token（工装仪器感 / 军事终端感）。
//!
//! 硬约束：**控件里禁止散写魔法数** —— 颜色、字号、圆角、间距、尺寸全部来自本模块。
//! 视觉纪律：深色为主，状态色只用于 2px 描边 / 状态点 / 数值文字，不铺大面积填充；
//! 任何状态色必须配文字，不靠颜色单独表意；禁用紫色渐变、霓虹发光、大面积玻璃拟态。

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke};

// ── 颜色 ─────────────────────────────────────────────────────────
pub const BG_VOID: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x0E);
pub const BG_PANEL: Color32 = Color32::from_rgb(0x14, 0x18, 0x1A);
pub const BG_RAISED: Color32 = Color32::from_rgb(0x1C, 0x22, 0x24);
pub const BG_INSET: Color32 = Color32::from_rgb(0x0E, 0x11, 0x12);
pub const LINE_STRONG: Color32 = Color32::from_rgb(0x2E, 0x38, 0x3A);
pub const LINE_SUBTLE: Color32 = Color32::from_rgb(0x23, 0x2A, 0x2C);
pub const TEXT_1: Color32 = Color32::from_rgb(0xE4, 0xE8, 0xE6);
pub const TEXT_2: Color32 = Color32::from_rgb(0x98, 0xA3, 0xA1);
pub const TEXT_3: Color32 = Color32::from_rgb(0x5E, 0x6A, 0x69);
/// 主色：琥珀。
pub const ACCENT: Color32 = Color32::from_rgb(0xE0, 0xA3, 0x3F);
pub const OK: Color32 = Color32::from_rgb(0x57, 0xB8, 0x7E);
pub const WARN: Color32 = Color32::from_rgb(0xD9, 0x77, 0x42);
pub const ERR: Color32 = Color32::from_rgb(0xC9, 0x50, 0x3C);
/// 面板浮起用的顶部高光（白 6%）。
pub const HIGHLIGHT: Color32 = Color32::from_rgba_premultiplied(0x0F, 0x0F, 0x0F, 0x0F);
/// 窗口外扩阴影（黑 30%）。
pub const SHADOW: Color32 = Color32::from_rgba_premultiplied(0x00, 0x00, 0x00, 0x4D);

// ── 字号 ─────────────────────────────────────────────────────────
pub const FONT_VALUE: f32 = 20.0;
pub const FONT_TITLE: f32 = 15.0;
pub const FONT_BODY: f32 = 13.0;
pub const FONT_LABEL: f32 = 11.0;

/// 等宽字体 id（数值与序列）。
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// 正文 / 标题字体 id。
pub fn text(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

// ── 圆角与描边 ────────────────────────────────────────────────────
pub const RADIUS_BUTTON: CornerRadius = CornerRadius::same(2);
pub const RADIUS_PANEL: CornerRadius = CornerRadius::same(4);
pub const RADIUS_WINDOW: CornerRadius = CornerRadius::same(6);

/// 常态 1px 描边。
pub fn stroke_normal() -> Stroke {
    Stroke::new(1.0, LINE_STRONG)
}

/// 激活 2px 描边。
pub fn stroke_active(color: Color32) -> Stroke {
    Stroke::new(2.0, color)
}

/// 状态色描边（宽度固定 2px，alpha 用于呼吸）。
pub fn stroke_status(color: Color32, alpha: f32) -> Stroke {
    let alpha = alpha.clamp(0.0, 1.0);
    let [r, g, b, _] = color.to_array();
    Stroke::new(2.0, Color32::from_rgba_unmultiplied(r, g, b, (255.0 * alpha) as u8))
}

// ── 间距（只允许 4/8/12/16/24/32）─────────────────────────────────
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 16.0;
pub const SPACE_XL: f32 = 24.0;
pub const SPACE_XXL: f32 = 32.0;

// ── 窗口与区块尺寸 ────────────────────────────────────────────────
pub const WINDOW_SIZE: [f32; 2] = [400.0, 420.0];
pub const WINDOW_MIN: [f32; 2] = [340.0, 360.0];
pub const COLLAPSED_SIZE: [f32; 2] = [176.0, 36.0];
/// 顶部整条可拖动区高度。
pub const TITLE_BAR_H: f32 = 32.0;
pub const SEQUENCE_H: f32 = 72.0;
pub const TOGGLE_H: f32 = 28.0;
pub const PRIMARY_H: f32 = 36.0;
pub const PROGRESS_H: f32 = 64.0;
pub const FOOTER_H: f32 = 28.0;
/// 右下角自绘抓边尺寸。
pub const RESIZE_GRIP: f32 = 12.0;
/// 状态点半径。
pub const DOT_RADIUS: f32 = 3.5;
/// 外扩阴影宽度。
pub const SHADOW_EXTENT: f32 = 2.0;

/// 安装字体与基础样式。用内嵌单 face TTF（simhei 子集），不依赖系统字体、
/// 不使用 `msyh.ttc`（TTC 会被 epaint 只取 face 0，中文变豆腐块）。
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "ui-zh".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../../assets/fonts/ui-zh.ttf"
        ))),
    );
    // 内嵌字体排第一（覆盖全部界面文案），系统默认字体留作缺字兜底。
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "ui-zh".to_owned());
    }
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|style| {
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = BG_VOID;
        style.visuals.window_fill = BG_PANEL;
        style.visuals.override_text_color = Some(TEXT_1);
        style.visuals.widgets.noninteractive.bg_stroke = stroke_normal();
        style.spacing.item_spacing = egui::vec2(SPACE_S, SPACE_S);
    });
}
