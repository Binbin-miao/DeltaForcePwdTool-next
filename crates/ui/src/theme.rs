//! 主题体系：六套色板值类型 + 派生 token + 自定义主色（OKLCH 派生）。
//!
//! 硬约束（`docs/design/ui-themes.md` §5）：
//! - **颜色只有 [`Palette`]**（14 项 token），旧的 `theme::ACCENT` 之类常量已全部删除，
//!   不允许两套颜色系统并存；自绘函数一律显式收 `pal: &Palette`，不做 thread_local 访问器。
//! - **非颜色 token 保持 `const`**（间距 / 窗口尺寸 / 区段高度 / 交互尺寸），它们与主题无关。
//! - 派生 token（`btn_fill_hover` / `switch_track_on` / …）只作为 [`Palette`] 的方法存在，
//!   视图层不得就地算颜色。
//! - 自定义主色只允许用户选**色相**，明度与彩度由 §2.7 的 OKLCH 规则算，
//!   杜绝「高明度黄 + 白字」这类必挂的组合。

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke};

pub use delta_config::ThemeId;

// ── 六套色板（逐项抄 `ui-themes.md` §2.1~§2.6 的 hex，不得自创）────────────

/// `0xRRGGBB` → 不透明 `Color32`（`const` 可用，色板表才能保持纯字面量）。
const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb(
        ((hex >> 16) & 0xFF) as u8,
        ((hex >> 8) & 0xFF) as u8,
        (hex & 0xFF) as u8,
    )
}

/// 一套主题的全部取值。颜色 14 项 + `dark` 标志 + 圆角/字号。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Palette {
    /// 深色主题为 `true`：决定文字色、浮起高光与阴影的处理方式。
    pub dark: bool,
    pub bg_void: Color32,
    pub bg_panel: Color32,
    pub bg_raised: Color32,
    pub bg_inset: Color32,
    pub line_strong: Color32,
    pub line_subtle: Color32,
    pub text_1: Color32,
    pub text_2: Color32,
    pub text_3: Color32,
    pub accent: Color32,
    /// accent 上的文字色（深色主题恒深字、浅色主题恒白字，见 §2.7）。
    pub accent_text_on: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub radius_button: u8,
    pub radius_panel: u8,
    pub font_title: f32,
    pub font_body: f32,
    pub font_label: f32,
    pub font_value: f32,
}

/// 石墨：无彩色，靠留白。
const GRAPHITE: Palette = Palette {
    dark: true,
    bg_void: rgb(0x0A0A0B),
    bg_panel: rgb(0x121315),
    bg_raised: rgb(0x1B1D20),
    bg_inset: rgb(0x0C0D0F),
    line_strong: rgb(0x34383D),
    line_subtle: rgb(0x25282C),
    text_1: rgb(0xEDEFEF),
    text_2: rgb(0x9CA2A7),
    text_3: rgb(0x767C81),
    accent: rgb(0xE8EBEA),
    accent_text_on: rgb(0x0F1011),
    ok: rgb(0x5FBF8A),
    warn: rgb(0xE0913F),
    err: rgb(0xE0594A),
    radius_button: 4,
    radius_panel: 8,
    font_title: 15.0,
    font_body: 13.0,
    font_label: 11.0,
    font_value: 26.0,
};

/// 夜航：深蓝黑 + 电蓝。
const NIGHTWATCH: Palette = Palette {
    dark: true,
    bg_void: rgb(0x070B14),
    bg_panel: rgb(0x0F1520),
    bg_raised: rgb(0x18202E),
    bg_inset: rgb(0x0A0F18),
    line_strong: rgb(0x2C3849),
    line_subtle: rgb(0x1D2633),
    text_1: rgb(0xE6EDF7),
    text_2: rgb(0x9AABBF),
    text_3: rgb(0x6E8098),
    accent: rgb(0x4C8DFF),
    accent_text_on: rgb(0x06101F),
    ok: rgb(0x3FD68F),
    warn: rgb(0xF2A93B),
    err: rgb(0xFF5C5C),
    radius_button: 6,
    radius_panel: 10,
    font_title: 15.0,
    font_body: 13.0,
    font_label: 11.0,
    font_value: 26.0,
};

/// 暗焰：暖褐 + 橙。
const EMBER: Palette = Palette {
    dark: true,
    bg_void: rgb(0x100C08),
    bg_panel: rgb(0x191310),
    bg_raised: rgb(0x241B15),
    bg_inset: rgb(0x0D0906),
    line_strong: rgb(0x3F3024),
    line_subtle: rgb(0x2C221A),
    text_1: rgb(0xF2E7DA),
    text_2: rgb(0xB8A48E),
    text_3: rgb(0x8A7663),
    accent: rgb(0xFF9E3D),
    accent_text_on: rgb(0x1A0F04),
    ok: rgb(0x6FBF73),
    warn: rgb(0xE8A33C),
    err: rgb(0xE0553C),
    radius_button: 3,
    radius_panel: 6,
    font_title: 15.0,
    font_body: 13.0,
    font_label: 11.0,
    font_value: 24.0,
};

/// 素纸：米纸 + 墨蓝。
const PAPER: Palette = Palette {
    dark: false,
    bg_void: rgb(0xDCD5C6),
    bg_panel: rgb(0xF7F3EA),
    bg_raised: rgb(0xFFFDF8),
    bg_inset: rgb(0xEDE7DA),
    line_strong: rgb(0xC3B8A4),
    line_subtle: rgb(0xDCD4C5),
    text_1: rgb(0x211E19),
    text_2: rgb(0x57503F),
    text_3: rgb(0x7C7362),
    accent: rgb(0x2C3E6B),
    accent_text_on: rgb(0xFBF8F1),
    ok: rgb(0x2F6B45),
    warn: rgb(0x8A5A00),
    err: rgb(0xA32B1B),
    radius_button: 4,
    radius_panel: 8,
    font_title: 15.0,
    font_body: 13.0,
    font_label: 11.0,
    font_value: 26.0,
};

/// 霜白（默认）：冷白 + 近黑。
const FROST: Palette = Palette {
    dark: false,
    bg_void: rgb(0xDDE2E8),
    bg_panel: rgb(0xF4F6F8),
    bg_raised: rgb(0xFDFDFE),
    bg_inset: rgb(0xE9EDF1),
    line_strong: rgb(0xC2CAD3),
    line_subtle: rgb(0xDAE0E6),
    text_1: rgb(0x161C22),
    text_2: rgb(0x4E5A66),
    text_3: rgb(0x78848F),
    accent: rgb(0x1C2024),
    accent_text_on: rgb(0xF7F9FA),
    ok: rgb(0x12705A),
    warn: rgb(0x8A5A00),
    err: rgb(0xB02A2A),
    radius_button: 5,
    radius_panel: 10,
    font_title: 15.0,
    font_body: 13.0,
    font_label: 11.0,
    font_value: 28.0,
};

/// 瓷青：浅灰绿单色。
const CELADON: Palette = Palette {
    dark: false,
    bg_void: rgb(0xD8E0D9),
    bg_panel: rgb(0xEDF1EC),
    bg_raised: rgb(0xF8FAF7),
    bg_inset: rgb(0xE1E8E2),
    line_strong: rgb(0xB8C6BA),
    line_subtle: rgb(0xD2DCD3),
    text_1: rgb(0x1B2A22),
    text_2: rgb(0x4A5C50),
    text_3: rgb(0x6F8075),
    accent: rgb(0x1F6B4F),
    accent_text_on: rgb(0xF4F8F4),
    ok: rgb(0x227653),
    warn: rgb(0x8A5A0A),
    err: rgb(0xA83322),
    radius_button: 8,
    radius_panel: 12,
    font_title: 15.0,
    font_body: 13.0,
    font_label: 11.0,
    font_value: 24.0,
};

/// 六套色板表（顺序即设置页的列表顺序）。
pub const ALL: [ThemeId; 6] = [
    ThemeId::Graphite,
    ThemeId::Nightwatch,
    ThemeId::Ember,
    ThemeId::Paper,
    ThemeId::Frost,
    ThemeId::Celadon,
];

/// `ThemeId` → 静态色板。
///
/// `ThemeId` 定义在 `delta-config`（配置要持久化它，而本 crate 依赖 config，反向不成立），
/// 所以这里是自由函数而非 `impl ThemeId`：函数签名与 `impl` 方法等价，调用点同样显式、
/// 无全局状态。
pub const fn palette(id: ThemeId) -> &'static Palette {
    match id {
        ThemeId::Graphite => &GRAPHITE,
        ThemeId::Nightwatch => &NIGHTWATCH,
        ThemeId::Ember => &EMBER,
        ThemeId::Paper => &PAPER,
        ThemeId::Frost => &FROST,
        ThemeId::Celadon => &CELADON,
    }
}

impl Palette {
    /// 主题的基础色板。
    pub const fn base(id: ThemeId) -> Self {
        *palette(id)
    }

    /// 按 §2.7 用色相派生主色（其余 token 不动）。
    pub fn with_accent(self, hue_deg: f32) -> Self {
        let (base_l, chroma) = if self.dark { (0.72, 0.14) } else { (0.48, 0.13) };
        // 兜底 1：目标底色对比不足 4.5 时按 L 步进（深色往上、浅色往下），最多 8 步。
        let mut lightness = base_l;
        let mut accent = oklch::to_srgb(lightness, chroma, hue_deg);
        let step = if self.dark { 0.03 } else { -0.03 };
        for _ in 0..8 {
            if contrast_ratio(accent, self.bg_panel) >= 4.5 {
                break;
            }
            lightness = (lightness + step).clamp(0.05, 0.98);
            accent = oklch::to_srgb(lightness, chroma, hue_deg);
        }
        // 兜底 2：按钮文字与按钮底色的对比也必须 ≥4.5（§2.7 的实测区间）。
        let text = accent_text_on(self.dark);
        for _ in 0..8 {
            if contrast_ratio(text, accent) >= 4.5 {
                break;
            }
            lightness = (lightness + step).clamp(0.05, 0.98);
            accent = oklch::to_srgb(lightness, chroma, hue_deg);
        }
        Self {
            accent,
            accent_text_on: text,
            ..self
        }
    }

    // ── 派生 token（§2.0，不允许在视图里就地算）────────────────────

    pub fn btn_fill(&self) -> Color32 {
        self.accent
    }

    pub fn btn_fill_hover(&self) -> Color32 {
        mix(self.accent, self.text_1, 0.10)
    }

    pub fn btn_fill_active(&self) -> Color32 {
        mix(self.accent, self.text_1, 0.18)
    }

    /// 关态轨道底：`bg_void` 填充 + 1px `line_strong` 描边（描边由视图画）。
    pub fn switch_track_off(&self) -> Color32 {
        self.bg_void
    }

    pub fn switch_thumb_off(&self) -> Color32 {
        self.text_3
    }

    /// 开态轨道：实心 accent，无描边。
    pub fn switch_track_on(&self) -> Color32 {
        self.accent
    }

    pub fn switch_thumb_on(&self) -> Color32 {
        self.accent_text_on
    }

    pub fn progress_track(&self) -> Color32 {
        self.line_subtle
    }

    pub fn progress_fill(&self) -> Color32 {
        self.accent
    }

    /// 键盘焦点环：accent @ 55% alpha，2px。
    pub fn focus_ring(&self) -> Color32 {
        with_alpha(self.accent, 0.55)
    }

    /// 顶部 1px 浮起高光：深色白 6%、浅色黑 5%。
    pub fn highlight(&self) -> Color32 {
        if self.dark {
            Color32::from_rgba_premultiplied(0x0F, 0x0F, 0x0F, 0x0F)
        } else {
            Color32::from_rgba_premultiplied(0x0D, 0x0D, 0x0D, 0x0D)
        }
    }

    /// 外扩阴影：深色黑 30%；浅色主题**不用阴影**（egui 无模糊，浅底黑边会脏）。
    pub fn shadow(&self) -> Color32 {
        if self.dark {
            Color32::from_rgba_premultiplied(0x00, 0x00, 0x00, 0x4D)
        } else {
            Color32::TRANSPARENT
        }
    }

    pub fn corner_button(&self) -> CornerRadius {
        CornerRadius::same(self.radius_button)
    }

    pub fn corner_panel(&self) -> CornerRadius {
        CornerRadius::same(self.radius_panel)
    }

    /// 面板底 + 1px 内描边 + 顶部高光所需的三个值（浅色主题靠 1px 描边撑层级）。
    pub fn panel_stroke(&self) -> Stroke {
        Stroke::new(1.0, if self.dark { self.line_subtle } else { self.line_strong })
    }

    pub fn mono(&self, size: f32) -> FontId {
        FontId::new(size, FontFamily::Monospace)
    }

    pub fn text(&self, size: f32) -> FontId {
        FontId::new(size, FontFamily::Proportional)
    }
}

/// 按「主题 + 用户主色（hex）」解析出最终色板。
///
/// `accent_override` 存的是 hex 字符串（配置层不引入自定义类型）：解析成 OKLCH 后**只取色相**，
/// 明度与彩度仍由 §2.7 的规则决定 —— 用户选不到低对比的组合。
pub fn resolve(id: ThemeId, accent_override: Option<&str>) -> Palette {
    let base = Palette::base(id);
    let Some(hex) = accent_override.and_then(parse_hex) else {
        return base;
    };
    let (chroma, hue) = oklch::of(hex);
    if !chroma.is_finite() || chroma < 0.01 {
        // 灰色（含各深色主题的默认 accent）没有可用色相：保持主题自带主色。
        return base;
    }
    base.with_accent(hue)
}

/// `#RRGGBB` / `RRGGBB` → 颜色；非法输入返回 `None`（调用方回落到主题默认色）。
pub fn parse_hex(text: &str) -> Option<Color32> {
    let digits = text.trim().trim_start_matches('#');
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok().map(rgb)
}

/// 颜色 → `#RRGGBB`（写配置用）。
pub fn to_hex(color: Color32) -> String {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// 深色主题恒深字、浅色主题恒白字的 accent 文字色（§2.7）。
fn accent_text_on(dark: bool) -> Color32 {
    if dark {
        rgb(0x12100E)
    } else {
        rgb(0xFBFAF7)
    }
}

/// 两色之间的 sRGB 线性插值（`t = 0.10 / 0.18` 的悬停混色）。
fn mix(from: Color32, to: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let [fr, fg, fb, _] = from.to_srgba_unmultiplied();
    let [tr, tg, tb, _] = to.to_srgba_unmultiplied();
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgb(lerp(fr, tr), lerp(fg, tg), lerp(fb, tb))
}

/// 带 alpha 的同色。
pub fn with_alpha(color: Color32, alpha: f32) -> Color32 {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    Color32::from_rgba_unmultiplied(r, g, b, (255.0 * alpha.clamp(0.0, 1.0)).round() as u8)
}

/// WCAG 2.1 相对亮度。
fn relative_luminance(color: Color32) -> f32 {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    let channel = |value: u8| {
        let c = value as f32 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// WCAG 2.1 对比度（1.0~21.0）。
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (lighter, darker) = if la > lb { (la, lb) } else { (lb, la) };
    (lighter + 0.05) / (darker + 0.05)
}

/// OKLCH（perceptually uniform）转换：只实现本模块需要的 sRGB ↔ OKLCH 单点换算。
///
/// 自己写而不引 crate：规范（`ui-themes.md` §2.7）明确禁止为本功能新增第三方依赖。
mod oklch {
    use egui::Color32;

    fn to_linear(value: f32) -> f32 {
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }

    fn encode(value: f32) -> f32 {
        if value <= 0.003_130_8 {
            value * 12.92
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        }
    }

    /// 线性 sRGB → Oklab。
    fn lab(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
        let l = 0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b;
        let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
        let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;
        let (l, m, s) = (l.cbrt(), m.cbrt(), s.cbrt());
        (
            0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
            1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
            0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
        )
    }

    /// Oklab → 线性 sRGB（可能越出 [0,1]，由调用方判色域）。
    fn linear(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
        let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
        let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
        let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
        let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
        (
            4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3,
            -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3,
            -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3,
        )
    }

    /// 颜色 → `(chroma, hue_deg)`。
    pub(super) fn of(color: Color32) -> (f32, f32) {
        let [r, g, b, _] = color.to_srgba_unmultiplied();
        let (_, a, b) = lab(to_linear(r as f32 / 255.0), to_linear(g as f32 / 255.0), to_linear(b as f32 / 255.0));
        let chroma = (a * a + b * b).sqrt();
        let hue = b.atan2(a).to_degrees();
        (chroma, if hue < 0.0 { hue + 360.0 } else { hue })
    }

    /// `oklch(L, C, hue)` → sRGB；越界按 `C *= 0.93` 迭代至色域内（§2.7）。
    pub(super) fn to_srgb(l: f32, chroma: f32, hue_deg: f32) -> Color32 {
        let mut chroma = chroma;
        let hue = hue_deg.to_radians();
        let mut rgb = (0.0, 0.0, 0.0);
        for _ in 0..24 {
            rgb = linear(l, chroma * hue.cos(), chroma * hue.sin());
            let inside = [rgb.0, rgb.1, rgb.2]
                .iter()
                .all(|channel| (-1e-4..=1.0 + 1e-4).contains(channel));
            if inside {
                break;
            }
            chroma *= 0.93;
        }
        let encode_channel = |value: f32| (encode(value.clamp(0.0, 1.0)) * 255.0).round() as u8;
        Color32::from_rgb(
            encode_channel(rgb.0),
            encode_channel(rgb.1),
            encode_channel(rgb.2),
        )
    }
}

// ── 字号与字体 ────────────────────────────────────────────────────

/// 等宽字体 id（数值与序列）。
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// 正文 / 标题字体 id。
pub fn text(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

// ── 间距（只允许 4/8/12/16/24/32）─────────────────────────────────
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 16.0;
pub const SPACE_XL: f32 = 24.0;
pub const SPACE_XXL: f32 = 32.0;

// ── 窗口与区段尺寸（L-A 布局，`ui-themes.md` §3.1）─────────────────
pub const WINDOW_SIZE: [f32; 2] = [400.0, 420.0];
/// 最小窗口。高度比标称少 36：开关网格固定 4 行（7 功能 + 3 附加），
/// 再矮就只能压结果区或叠行 —— 这里守住「不放不下就缩小」这条底线。
pub const WINDOW_MIN: [f32; 2] = [340.0, 384.0];
pub const COLLAPSED_SIZE: [f32; 2] = [176.0, 36.0];
/// 标题条：状态点 7px + 状态文字 15px + 右侧三个 16px 图标按钮。
pub const TITLE_BAR_H: f32 = 32.0;
/// 结果区（单卡）标称高度。
pub const RESULT_H: f32 = 96.0;
/// 结果区可压缩下限（最小窗口下仍要放得下 26px 结果 + 来源标签）。
pub const RESULT_MIN_H: f32 = 64.0;
/// 开关网格的单行高（**行数不固定**：由格数按 3 列算 `div_ceil`）。
pub const GRID_ROW_H: f32 = 32.0;
pub const GRID_ROW_GAP: f32 = 6.0;
/// 列间空隙：3 列一行（内容宽 376 = 3×120 + 2×8，最小窗 316 = 3×100 + 2×8）。
pub const GRID_COL_GAP: f32 = 8.0;
/// 主按钮：满宽 44。
pub const PRIMARY_H: f32 = 44.0;
/// 状态行：文案 12px + 底部 2px 进度轨。
pub const STATUS_H: f32 = 32.0;
/// 底部文字链。
pub const FOOTER_H: f32 = 28.0;
/// 开关轨道 28×16 + 12px 滑块。
pub const SWITCH_W: f32 = 28.0;
pub const SWITCH_H: f32 = 16.0;
pub const SWITCH_THUMB: f32 = 12.0;
/// 标题条图标按钮边长。
pub const ICON_BUTTON: f32 = 16.0;
/// 标题栏「识别总开关」胶囊的高度（宽度在视图里按文案定）。
pub const MASTER_TOGGLE_H: f32 = 20.0;
/// 警示条里「知道了」按钮尺寸。
pub const NOTICE_BUTTON_W: f32 = 64.0;
pub const NOTICE_BUTTON_H: f32 = 24.0;
/// 右下角自绘抓边尺寸。
pub const RESIZE_GRIP: f32 = 12.0;
/// 标题条状态点半径（7px 直径）。
pub const DOT_RADIUS: f32 = 3.5;
/// 深色主题外扩阴影宽度。
pub const SHADOW_EXTENT: f32 = 2.0;
/// 进度轨高度。
pub const PROGRESS_TRACK_H: f32 = 2.0;

// ── 安装 ─────────────────────────────────────────────────────────

/// 装字体（**只装一次**，绝不随主题变）。
pub fn install_fonts(ctx: &egui::Context) {
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
}

/// 把色板灌进 egui 原生控件（可重入；只在主题变化的帧调用一次）。
///
/// 自绘控件以 [`Palette`] 为准，这里只兜底内嵌原生控件（文本编辑框、滚动条、tooltip）。
pub fn apply_visuals(ctx: &egui::Context, pal: &Palette) {
    ctx.all_styles_mut(|style| {
        style.visuals = if pal.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        style.visuals.panel_fill = pal.bg_void;
        style.visuals.window_fill = pal.bg_panel;
        style.visuals.override_text_color = Some(pal.text_1);
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, pal.line_strong);
        style.visuals.selection.bg_fill = with_alpha(pal.accent, 0.35);
        style.spacing.item_spacing = egui::vec2(SPACE_S, SPACE_S);
    });
}

/// 字体 + 默认色板（首帧之前调用一次）。
pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    apply_visuals(ctx, palette(ThemeId::default()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 14 项颜色 token 一个不少，且六套彼此可区分。
    #[test]
    fn six_palettes_have_all_fourteen_colour_tokens() {
        let mut seen: Vec<[u8; 4]> = Vec::new();
        for id in ALL {
            let pal = palette(id);
            let tokens = [
                pal.bg_void,
                pal.bg_panel,
                pal.bg_raised,
                pal.bg_inset,
                pal.line_strong,
                pal.line_subtle,
                pal.text_1,
                pal.text_2,
                pal.text_3,
                pal.accent,
                pal.accent_text_on,
                pal.ok,
                pal.warn,
                pal.err,
            ];
            assert_eq!(tokens.len(), 14, "{id:?} 的 token 数量必须是 14");
            for token in tokens {
                assert_ne!(token, Color32::TRANSPARENT, "{id:?} 有空 token");
            }
            // 一套之内底色必须互不相同（否则「面板浮起」看不出来）。
            assert_ne!(pal.bg_void, pal.bg_panel, "{id:?}");
            assert_ne!(pal.bg_panel, pal.bg_raised, "{id:?}");
            // accent 与 accent_text_on 必须真的有对比。
            assert!(
                contrast_ratio(pal.accent, pal.accent_text_on) >= 4.5,
                "{id:?} 的按钮文字对比不足"
            );
            seen.push(pal.accent.to_array());
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 6, "六套主色必须互异");
    }

    /// §2.8 的实测门槛：正文/次要文字/状态色/accent 及其上文字。
    #[test]
    fn contrast_thresholds_hold_for_every_palette() {
        for id in ALL {
            let pal = palette(id);
            let check = |name: &str, fg: Color32, bg: Color32, min: f32| {
                let ratio = contrast_ratio(fg, bg);
                assert!(ratio >= min, "{id:?} {name} 只有 {ratio:.2}:1，要求 ≥{min}");
            };
            check("text_1/bg_panel", pal.text_1, pal.bg_panel, 7.0);
            check("text_2/bg_panel", pal.text_2, pal.bg_panel, 4.5);
            // text_3 只允许出现在非关键标签上（§2.8 门槛 ≥3）。
            check("text_3/bg_panel", pal.text_3, pal.bg_panel, 3.0);
            check("accent/bg_panel", pal.accent, pal.bg_panel, 4.5);
            for (name, status) in [("ok", pal.ok), ("warn", pal.warn), ("err", pal.err)] {
                check(name, status, pal.bg_panel, 4.5);
            }
        }
    }

    /// §2.7：任意色相派生出的主色，按钮文字对比必须 ≥4.5（WCAG AA）。
    #[test]
    fn custom_accent_keeps_button_text_readable_for_any_hue() {
        let mut worst = f32::MAX;
        for id in ALL {
            for hue in (0..360).step_by(5) {
                let pal = Palette::base(id).with_accent(hue as f32);
                let ratio = contrast_ratio(pal.accent, pal.accent_text_on);
                worst = worst.min(ratio);
                assert!(
                    ratio >= 4.5,
                    "{id:?} 色相 {hue} 的按钮文字对比只有 {ratio:.2}:1"
                );
                // 深色主题恒深字、浅色主题恒白字（不允许出现"反色"）。
                let expect_dark_text = id != ThemeId::Frost
                    && id != ThemeId::Paper
                    && id != ThemeId::Celadon;
                let [r, g, b, _] = pal.accent_text_on.to_srgba_unmultiplied();
                let is_dark_text = (u16::from(r) + u16::from(g) + u16::from(b)) < 300;
                assert_eq!(is_dark_text, expect_dark_text, "{id:?} 色相 {hue} 文字色反了");
            }
        }
        assert!(worst >= 4.5, "最差对比 {worst:.2}:1");
    }

    #[test]
    fn custom_accent_moves_the_hue_and_keeps_status_colours() {
        let base = Palette::base(ThemeId::Nightwatch);
        let warm = base.with_accent(35.0);
        assert_ne!(warm.accent, base.accent, "改色相必须真的改主色");
        assert_eq!(warm.ok, base.ok, "状态色不跟随 accent（§2.7）");
        assert_eq!(warm.warn, base.warn);
        assert_eq!(warm.err, base.err);
        assert_eq!(warm.bg_panel, base.bg_panel, "底色不跟随 accent");
        // 派生出来的色相应回到原处（±6°）。
        let (_, hue) = oklch::of(warm.accent);
        assert!((hue - 35.0).abs() < 6.0, "色相漂移过大：{hue}");
    }

    #[test]
    fn accent_override_round_trips_through_hex() {
        let pal = resolve(ThemeId::Celadon, Some("#B23A2A"));
        assert_ne!(pal.accent, palette(ThemeId::Celadon).accent);
        let hex = to_hex(pal.accent);
        assert!(parse_hex(&hex).is_some(), "{hex} 必须能读回");
        // 非法输入回落主题默认色，不 panic。
        assert_eq!(resolve(ThemeId::Celadon, Some("不是颜色")), *palette(ThemeId::Celadon));
        assert_eq!(resolve(ThemeId::Celadon, None), *palette(ThemeId::Celadon));
        // 灰色没有可用色相 → 保持主题自带主色（默认 Frost 就是近黑）。
        assert_eq!(resolve(ThemeId::Frost, Some("#808080")), *palette(ThemeId::Frost));
    }

    #[test]
    fn derived_tokens_follow_the_rules() {
        for id in ALL {
            let pal = palette(id);
            assert_eq!(pal.btn_fill(), pal.accent);
            assert_eq!(pal.switch_track_on(), pal.accent);
            assert_eq!(pal.switch_thumb_on(), pal.accent_text_on);
            assert_eq!(pal.progress_track(), pal.line_subtle);
            assert_eq!(pal.switch_track_off(), pal.bg_void);
            assert_ne!(pal.btn_fill_hover(), pal.btn_fill());
            // 悬停混色必须朝 text_1 方向走。
            let lum = |c: Color32| relative_luminance(c);
            let toward = lum(pal.text_1) - lum(pal.accent);
            assert!(
                (lum(pal.btn_fill_hover()) - lum(pal.accent)).signum() == toward.signum(),
                "{id:?} 悬停色没朝文字色混合"
            );
            if pal.dark {
                assert_ne!(pal.shadow(), Color32::TRANSPARENT);
            } else {
                assert_eq!(pal.shadow(), Color32::TRANSPARENT, "浅色主题不用阴影");
            }
        }
    }
}
