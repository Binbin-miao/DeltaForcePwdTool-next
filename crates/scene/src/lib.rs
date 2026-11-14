//! 一键场景判别（spec D6）。
//!
//! 契约：
//! 1. `detect(frame, view) -> SceneVerdict`，四类 `Morse | Fingerprint | Console | Unknown`；
//! 2. 打分只用**结构特征**（前景占比、轮廓长条数、列投影周期），**禁止整屏模板匹配**
//!    —— 分辨率一变模板就废，而结构比值不变；
//! 3. 采纳规则：最高分 > [`ADOPT_THRESHOLD`] 且与次高分差 > [`ADOPT_GAP`]，否则 `Unknown`；
//! 4. `Unknown` 的调用方**必须拒绝执行**（[`SceneVerdict::is_actionable`] 为 `false`），
//!    不得兜底成任意场景去乱点（spec D6 硬约束）。

mod features;
pub mod locate;

use delta_capture::Frame;
use delta_config::{PxAz3, PxView, MORSE_REGION_COUNT};
use delta_vision::{BinaryImage, GrayImage};

pub use features::{
    column_segments, crop, edge_density, equidistant_column_peaks, otsu_foreground_ratio,
    outline_stats, ramp, to_region, OutlineStats,
};

/// 采纳门槛：最高分必须**严格大于**此值。
pub const ADOPT_THRESHOLD: f32 = 0.6;
/// 采纳门槛：与次高分之差必须**严格大于**此值，否则视为「两个场景都像」。
pub const ADOPT_GAP: f32 = 0.15;

/// 指纹候选格边缘密度门（归一化梯度，分辨率无关）。
const EDGE_FLOOR: f32 = 0.004;
/// 名字区「有笔画」的梯度区间。空白区恒为 0；成行汉字（笔画密、间距薄）实测在 0.02 以上。
const NAME_EDGE: (f32, f32) = (0.008, 0.025);
/// 摩斯格「有内容」的前景占比区间。
const MORSE_PRESENCE: (f32, f32) = (0.002, 0.02);
/// LED 区「有亮点」的前景占比区间。
const LED_PRESENCE: (f32, f32) = (0.01, 0.05);
/// LED 区能被切出的最少字符段数（spec §7.1：`NN-NN-NN-NN` = 8 位 + 3 个横杠）。
const LED_MIN_SEGMENTS: f32 = 7.0;
/// 操作台面板的最少竖列数（4 列旋钮）。
const PANEL_MIN_COLUMNS: usize = 4;
/// 旋钮搜索窗内「暖色像素」占比区间（低于下限 = 没有旋钮）。
const KNOB_WARM_RATIO: (f32, f32) = (0.01, 0.035);

/// 四类场景。`Unknown` 是**拒绝执行**信号，不是「猜一个」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scene {
    Morse,
    Fingerprint,
    Console,
    Unknown,
}

impl Scene {
    /// 场景分数组里的下标顺序。
    pub const ORDER: [Scene; 3] = [Scene::Morse, Scene::Fingerprint, Scene::Console];

    /// 是否识别成功（`Unknown` → `false`，调用方必须据此拒绝执行）。
    pub fn is_known(self) -> bool {
        !matches!(self, Scene::Unknown)
    }

    /// 分类名（日志与 UI 展示用）。
    pub fn name(self) -> &'static str {
        match self {
            Scene::Morse => "Morse",
            Scene::Fingerprint => "Fingerprint",
            Scene::Console => "Console",
            Scene::Unknown => "Unknown",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Scene::Morse => "检测到摩斯密码界面",
            Scene::Fingerprint => "检测到指纹密码界面",
            Scene::Console => "检测到操作台界面",
            Scene::Unknown => "未检测到游戏界面",
        }
    }
}

/// 判别结论。`scores` 恒定按 [`Scene::ORDER`] 排列（莫斯 / 指纹 / 操作台），便于日志对账。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneVerdict {
    pub scene: Scene,
    /// 采纳时 = 最高分；`Unknown` 时 = 最高分但未过门槛。
    pub confidence: f32,
    pub scores: [f32; 3],
    /// 可读原因，`Unknown` 时直接给 UI 显示（如「未检测到游戏界面」）。
    pub reason: &'static str,
}

impl SceneVerdict {
    /// `Unknown` → `false`。调用方**必须**据此拒绝执行（spec D6）。
    pub fn is_actionable(&self) -> bool {
        self.scene.is_known()
    }

    /// 某个场景的得分（下标顺序与 [`Scene::ORDER`] 一致）。
    pub fn score_of(&self, scene: Scene) -> f32 {
        match scene {
            Scene::Morse => self.scores[0],
            Scene::Fingerprint => self.scores[1],
            Scene::Console => self.scores[2],
            Scene::Unknown => 0.0,
        }
    }
}

/// 一键判别主入口。`view` 是校准后的只读像素视图（百分比 → 像素已换算）。
///
/// 内部任何区域裁剪失败都只让对应分数变 0，绝不 panic、绝不兜底成某场景。
pub fn detect(frame: &Frame, view: &PxView) -> SceneVerdict {
    let gray = frame.luma();
    // 操作台一类（旧旋钮台 + AZ3 信号线台）必须**同时**有一颗暖色旋钮：
    // 单看「LED 亮点 + 面板 4 列」会把摩斯界面判成操作台（深蓝面板 + 数字键盘 +
    // 顶部指南针刻度三项全过），随后真的去拖旋钮。旋钮是这一类独有的硬证据。
    let console = console_score(&gray, view) * knob_score(frame, view);
    decide([
        morse_score(&gray, view),
        fingerprint_score(&gray, view),
        console,
    ])
}

/// 采纳规则（纯函数，便于直接钉死门槛语义）。
pub fn decide(scores: [f32; 3]) -> SceneVerdict {
    let mut ranked = [0usize, 1, 2];
    ranked.sort_by(|left, right| scores[*right].total_cmp(&scores[*left]));

    let best = ranked[0];
    let confidence = scores[best];
    let separation = confidence - scores[ranked[1]];

    if confidence > ADOPT_THRESHOLD && separation > ADOPT_GAP {
        let scene = Scene::ORDER[best];
        return SceneVerdict {
            scene,
            confidence,
            scores,
            reason: scene.reason(),
        };
    }
    SceneVerdict {
        scene: Scene::Unknown,
        confidence,
        scores,
        reason: Scene::Unknown.reason(),
    }
}

/// 摩斯场景得分：3 个摩斯格「有内容」+「点划结构（≥3 符号且 ≥1 划）」。
///
/// 任一格裁不出来就按「该格无内容」计 0 分（不 panic，也不放弃另外两格）。
pub fn morse_score(gray: &GrayImage, view: &PxView) -> f32 {
    let regions: Vec<&delta_config::PxRegion> =
        view.regions.iter().take(MORSE_REGION_COUNT).collect();
    if regions.len() < MORSE_REGION_COUNT {
        return 0.0;
    }

    let mut presence = 0.0f32;
    let mut structure = 0.0f32;
    for region in regions {
        let Some(cell) = crop(gray, region.rect) else {
            continue;
        };
        presence += ramp(
            otsu_foreground_ratio(&cell),
            MORSE_PRESENCE.0,
            MORSE_PRESENCE.1,
        );
        if outline_stats(&cell).is_symbol_like() {
            structure += 1.0;
        }
    }
    let cells = MORSE_REGION_COUNT as f32;
    0.45 * (presence / cells) + 0.55 * (structure / cells)
}

/// 指纹场景得分：候选格边缘密度显著高于空白对照 + 名字区有笔画。
///
/// 「显著高于空白」用整帧平均梯度当对照基线：指纹界面的候选格纹理远密于背景，
/// 纯色桌面上候选格与基线都是 0，因此不会误判。
pub fn fingerprint_score(gray: &GrayImage, view: &PxView) -> f32 {
    let boxes = &view.fingerprint.candidate_boxes;
    if boxes.is_empty() {
        return 0.0;
    }

    let baseline = edge_density(gray);
    let floor = (baseline * 1.5).max(EDGE_FLOOR);

    let mut measured = 0usize;
    let mut active = 0usize;
    for cell_rect in boxes {
        let Some(cell) = crop(gray, *cell_rect) else {
            continue;
        };
        measured += 1;
        if edge_density(&cell) > floor {
            active += 1;
        }
    }
    if measured == 0 {
        return 0.0;
    }

    let active_fraction = active as f32 / measured as f32;
    let name_score = crop(gray, view.fingerprint.name_region)
        .map_or(0.0, |name| ramp(edge_density(&name), NAME_EDGE.0, NAME_EDGE.1));
    0.4 * active_fraction + 0.6 * name_score
}

/// 操作台场景得分：LED 区亮点 + 列投影可切 ≥7 个字符段；面板区 4 条等距竖列。
pub fn console_score(gray: &GrayImage, view: &PxView) -> f32 {
    let led_score = crop(gray, view.console.led_region).map_or(0.0, |led| {
        let binary = led.otsu().binary;
        let bright = ramp(
            otsu_foreground_ratio(&led),
            LED_PRESENCE.0,
            LED_PRESENCE.1,
        );
        let glyphs = ramp(column_segments(&binary, 2) as f32, 6.0, LED_MIN_SEGMENTS);
        0.5 * bright + 0.5 * glyphs
    });

    let panel_score = crop(gray, view.console.panel_region)
        .and_then(|panel| equidistant_column_peaks(&panel.otsu().binary, PANEL_MIN_COLUMNS))
        .map_or(0.0, |_| 1.0);

    0.7 * led_score + 0.3 * panel_score
}

/// 旋钮搜索窗：半宽 = 屏宽 / 32（1080p ≈ 60px，720p ≈ 40px），远大于旋钮直径、
/// 又小于「另一个界面元素」到旋钮的距离。
fn knob_window(center: delta_geom::PxPoint, frame: &Frame) -> delta_geom::PxRect {
    let half = (frame.w as i32 / 32).max(24);
    delta_geom::PxRect {
        x: center.x - half,
        y: center.y - half,
        w: half * 2,
        h: half * 2,
    }
}

/// 暖色像素：红通道明显高于蓝通道，且不太暗。
///
/// 旧操作台米黄旋钮 ≈ `[80, 205, 235]`、AZ3 信号线台暗红旋钮 ≈ `[30, 40, 140]`，都命中；
/// 摩斯/指纹/仓库的深蓝面板与白字红蓝差为负，不命中。
fn is_warm_knob([blue, _, red]: [u8; 3]) -> bool {
    u16::from(red) >= 70 && u16::from(red) > u16::from(blue) + 40
}

/// 旋钮存在性得分（0 = 没有旋钮，1 = 明确有）。
///
/// 这是「操作台一类」的硬证据：旧旋钮台在 `console.knob_center`、AZ3 信号线台在
/// `az3.line_knob_center`，两处任一命中即可。未配置（0,0）的坐标直接跳过。
fn knob_score(frame: &Frame, view: &PxView) -> f32 {
    let best = [view.console.knob_center, view.az3.line_knob_center]
        .into_iter()
        .filter(|center| center.x > 0 || center.y > 0)
        .map(|center| color_ratio(frame, knob_window(center, frame), is_warm_knob))
        .fold(0.0f32, f32::max);
    ramp(best, KNOB_WARM_RATIO.0, KNOB_WARM_RATIO.1)
}


/// AZ3 骇入电脑小游戏的 4 种细分类型（`None` 表示未命中任何 AZ3 界面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Az3Kind {
    /// 大保险：5×5 字符矩阵 + 右侧绿色「破解成功」圆钮，光标扫到高亮行按空格。
    SafeTiming,
    /// 任务保险：5 个竖向游标滑槽 + 底部绿条状态灯，每槽过中心按空格。
    SafePins,
    /// 黄键台 / 模块按键台：左侧 16:9 屏幕 + 右侧 4×4 按键网格，亮起即点。
    ModuleKeys,
    /// 横线台 / 信号线台：4 条白竖线 + 红色旋钮 + 右上角 LED 序列。
    SignalLine,
    /// 未识别到任何 AZ3 界面（必须拒绝执行，零输入）。
    None,
}

impl Az3Kind {
    /// 是否命中已知的 AZ3 玩法。
    pub fn is_known(self) -> bool {
        !matches!(self, Self::None)
    }

    /// 中文展示名（UI 与日志用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::SafeTiming => "大保险",
            Self::SafePins => "任务保险",
            Self::ModuleKeys => "黄键台",
            Self::SignalLine => "信号线台",
            Self::None => "未知",
        }
    }
}

// ── AZ3 二级变体判别（有序：先强特征，后弱特征）──

/// 黄键台：整帧黄/绿高亮占比下限。
///
/// 这只是**便宜的快速过滤**（整帧一遍谓词），用来跳过明显无关的画面；
/// 真正的结构判据是 [`locate::locate_keys`]。阈值按整帧口径取 0.001：
/// 720p 下一颗 40×40 的亮键 ≈ 1600px / 921600 ≈ 0.0017，仍能过关。
const AZ3_KEYS_FRAME_GLOW_RATIO: f32 = 0.001;
/// 任务保险：轨道带内竖直白块数量下限（5 槽，容忍 1 根被游标遮住）。
const AZ3_PINS_MIN_SLOTS: usize = 4;
/// 任务保险：每个滑槽在配置中心两侧的搜索半宽（与执行层同口径）。
const AZ3_PIN_SEARCH_HALF_W: i32 = 40;
/// 任务保险：竖直白块的高度下限。
const AZ3_PIN_BAR_MIN_H: u32 = 18;
/// 任务保险：竖直白块的宽度上限（细长才是游标，粗白块是别的 UI）。
const AZ3_PIN_BAR_MAX_W: u32 = 10;
/// 大保险：目标带亮绿占比下限。
const AZ3_TIMING_GREEN_RATIO: f32 = 0.01;
/// 大保险：目标带能切出的等距字符列下限。
const AZ3_TIMING_MIN_COLUMNS: usize = 3;

/// 在已进入游戏界面的前提下，细分判别 4 类 AZ3 骇入小游戏；
/// 空白桌面、摩斯/指纹、**或 v5 旧操作台**都返回 [`Az3Kind::None`]（拒绝执行 / 交回旧流程）。
///
/// **有序**判别而非打分排序：四类的强特征几乎不共存，按「证据强度」依次排除，
/// 每个分支用自己的阈值。所有阈值都用 `>=`（低对比帧也要能过）。
pub fn az3_variant(frame: &Frame, view: &PxView) -> Az3Kind {
    if frame.w < 32 || frame.h < 32 {
        return Az3Kind::None;
    }
    let az3 = &view.az3;
    if looks_like_keys(frame) {
        return Az3Kind::ModuleKeys;
    }
    if looks_like_pins(frame, az3) {
        return Az3Kind::SafePins;
    }
    if looks_like_timing(frame, az3) {
        return Az3Kind::SafeTiming;
    }
    if looks_like_signal_line(frame) {
        return Az3Kind::SignalLine;
    }
    Az3Kind::None
}

/// 黄键台：**每帧自适应定位**能同时找到 4×4 暗键阵 + 左侧屏幕。
///
/// 旧实现拿配置里的 `az3.button_grid_region` 去裁右侧网格 —— 换一版界面布局，
/// 那个固定矩形就整体偏出去（只盖住键阵右半），于是「黄键台」时而被判成别的场景、
/// 时而判对了却按错的坐标去点。现在判别与执行共用同一份定位证据（[`locate::locate_keys`]），
/// 不再有两套坐标。
///
/// 先跑便宜的全帧黄/绿占比过滤，再跑昂贵的连通域定位（先便宜后昂贵）。
fn looks_like_keys(frame: &Frame) -> bool {
    let whole = delta_geom::PxRect {
        x: 0,
        y: 0,
        w: frame.w as i32,
        h: frame.h as i32,
    };
    if color_ratio(frame, whole, is_yellow_or_green_glow) < AZ3_KEYS_FRAME_GLOW_RATIO {
        return false;
    }
    locate::locate_keys(frame).is_ok()
}

/// 任务保险：5 个配置槽里至少有 4 个槽内有细长竖直白块（游标）。
///
/// 必须按「槽」计数而不是数全带白块：LED 数字、面板竖线这类白色元素也可能细长，
/// 但它们不会同时出现在 5 个滑槽位置上。
fn looks_like_pins(frame: &Frame, az3: &PxAz3) -> bool {
    let filled = az3
        .pin_center_x
        .iter()
        .filter(|&&center| {
            let slot = delta_geom::PxRect {
                x: center - AZ3_PIN_SEARCH_HALF_W,
                y: az3.track_band.y,
                w: AZ3_PIN_SEARCH_HALF_W * 2,
                h: az3.track_band.h,
            };
            vertical_white_bars(frame, slot) > 0
        })
        .count();
    filled >= AZ3_PINS_MIN_SLOTS
}

/// 大保险：目标带有亮绿高亮，且能被切成多个等距字符列。
fn looks_like_timing(frame: &Frame, az3: &PxAz3) -> bool {
    let target = az3.target_band;
    color_ratio(frame, target, is_green_glow) >= AZ3_TIMING_GREEN_RATIO
        && color_columns(frame, target, is_green_glow, AZ3_TIMING_MIN_COLUMNS)
}

/// 信号线台：**每帧自适应定位**能同时找到旋钮 + 青色屏幕 + LED 数字条 + 4 列档位。
///
/// 旧实现拿配置里的固定百分比去裁 LED 区再量旋钮窗暖色占比 —— 实测旋钮中心在
/// `(0.604,0.554)~(0.766,0.573)` 之间漂，同一份配置时而命中、时而全落空，
/// 于是「信号线台」时而被判成别的场景、时而在空旷场景上被判成信号线台。
///
/// 现在判别与执行共用同一份定位证据（[`locate::locate_line`]），不再有两套坐标。
/// 旋钮放在第一步，所以非信号线台的画面会在第一步就失败退出：不用为每帧付出整段定位。
fn looks_like_signal_line(frame: &Frame) -> bool {
    locate::locate_line(frame).is_ok()
}

/// 指定颜色在裁剪区内是否形成 ≥`min_columns` 条等距竖列。
fn color_columns(
    frame: &Frame,
    rect: delta_geom::PxRect,
    pred: impl Fn([u8; 3]) -> bool,
    min_columns: usize,
) -> bool {
    let x0 = rect.x.max(0) as u32;
    let y0 = rect.y.max(0) as u32;
    let x1 = (rect.right().max(0) as u32).min(frame.w);
    let y1 = (rect.bottom().max(0) as u32).min(frame.h);
    let Ok(mut binary) = BinaryImage::new(x1.saturating_sub(x0), y1.saturating_sub(y0)) else {
        return false;
    };
    let (w, h) = (binary.width(), binary.height());
    if w == 0 || h == 0 {
        return false;
    }
    for y in 0..h {
        for x in 0..w {
            binary.set(x, y, pred(frame.pixel(x0 + x, y0 + y)));
        }
    }
    equidistant_column_peaks(&binary, min_columns).is_some()
}

/// 竖直白块数量：逐列统计白像素，够高且够窄的连续列段算一根。
fn vertical_white_bars(frame: &Frame, rect: delta_geom::PxRect) -> usize {
    let x0 = rect.x.max(0) as u32;
    let y0 = rect.y.max(0) as u32;
    let x1 = (rect.right().max(0) as u32).min(frame.w);
    let y1 = (rect.bottom().max(0) as u32).min(frame.h);
    if x1 <= x0 || y1 <= y0 {
        return 0;
    }

    let mut bars = 0usize;
    let mut run = 0u32;
    for x in x0..x1 {
        let tall = (y0..y1)
            .filter(|&y| is_bright_white(frame.pixel(x, y)))
            .count() as u32;
        if tall >= AZ3_PIN_BAR_MIN_H {
            run += 1;
            continue;
        }
        if (1..=AZ3_PIN_BAR_MAX_W).contains(&run) {
            bars += 1;
        }
        run = 0;
    }
    if (1..=AZ3_PIN_BAR_MAX_W).contains(&run) {
        bars += 1;
    }
    bars
}

/// 统计指定像素矩形内满足谓词的像素占比（0..=1）。
fn color_ratio(frame: &Frame, rect: delta_geom::PxRect, pred: impl Fn([u8; 3]) -> bool) -> f32 {
    let x0 = rect.x.max(0) as u32;
    let y0 = rect.y.max(0) as u32;
    let x1 = (rect.right() as u32).min(frame.w);
    let y1 = (rect.bottom() as u32).min(frame.h);
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let mut hits = 0u64;
    let mut total = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            total += 1;
            if pred(frame.pixel(x, y)) {
                hits += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        hits as f32 / total as f32
    }
}

fn is_green_glow([b, g, r]: [u8; 3]) -> bool {
    g > 120 && g > r.saturating_add(25) && g > b.saturating_add(25)
}

/// 黄/绿高亮（黄键台的亮键）。与 `flow-az3::geom` 同口径：
/// `r + g > 3b` 必须在 u16 上算，u8 饱和会让 r+g 恒为 255 → 亮黄键漏检；
/// `b` 上限 120 是为了容下实测 B≈120 的米黄旋钮（靠暗键阵结构拦截误判）。
fn is_yellow_or_green_glow([b, g, r]: [u8; 3]) -> bool {
    let is_green = is_green_glow([b, g, r]);
    let is_yellow = r > 140
        && g > 120
        && b <= 120
        && (u16::from(r) + u16::from(g)) > u16::from(b) * 3;
    is_green || is_yellow
}

fn is_bright_white([b, g, r]: [u8; 3]) -> bool {
    r > 185 && g > 185 && b > 185
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_config::{PxConsole, PxFingerprint, PxRegion};
    use delta_geom::{MonitorInfo, PxPoint, PxRect};
    use std::time::Instant;

    // ── 合成测试图：`_upstream` 里没有游戏截图，按 spec §7.2 的比例自造 ──

    const W: u32 = 1280;
    const H: u32 = 720;
    const WHITE: [u8; 3] = [255, 255, 255];

    struct Canvas {
        w: u32,
        h: u32,
        data: Vec<u8>,
    }

    impl Canvas {
        fn new(w: u32, h: u32) -> Self {
            Self {
                w,
                h,
                data: vec![0; (w * h * 4) as usize],
            }
        }

        fn paint(&mut self, rect: PxRect, rgb: [u8; 3]) {
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
                        continue;
                    }
                    let index = ((y as u32 * self.w + x as u32) * 4) as usize;
                    self.data[index] = rgb[2];
                    self.data[index + 1] = rgb[1];
                    self.data[index + 2] = rgb[0];
                    self.data[index + 3] = 255;
                }
            }
        }

        fn frame(self) -> Frame {
            Frame::new(self.w, self.h, self.data, Instant::now()).expect("合成帧长度正确")
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> PxRect {
        PxRect { x, y, w, h }
    }

    /// 校准视图。面板 / LED 采用 spec §7.2 的实测占比；摩斯格与候选格放在互不重叠的区域，
    /// 这样每个合成帧只有目标场景的结构特征，其余分数应逼近 0。
    fn spec_view() -> PxView {
        let mut boxes = Vec::new();
        for row in 0..3 {
            for col in 0..3 {
                boxes.push(rect(100 + col * 190, 150 + row * 175, 170, 150));
            }
        }
        PxView {
            monitor: MonitorInfo::new(W as i32, H as i32).expect("合法显示器"),
            regions: (0..MORSE_REGION_COUNT)
                .map(|index| PxRegion {
                    name: format!("morse_cell_{}", index + 1),
                    rect: rect(880, 300 + index as i32 * 100, 340, 70),
                })
                .collect(),
            fingerprint: PxFingerprint {
                name_region: rect(880, 30, 360, 50),
                number_region: rect(880, 170, 200, 50),
                candidate_boxes: boxes,
            },
            console: PxConsole {
                led_region: rect(855, 85, 390, 55),
                panel_region: rect(85, 90, 595, 515),
                col_x: vec![160, 300, 440, 580],
                row_y: vec![200, 300, 400, 500],
                knob_center: PxPoint { x: 745, y: 340 },
            },
            az3: spec_az3(),
            confirm_clicks: Vec::new(),
        }
    }

    /// AZ3 像素视图：1280×720 下 `builtin::AZ3_*` 百分比换算结果，与真机 `to_px` 同口径。
    fn spec_az3() -> PxAz3 {
        PxAz3 {
            grid_region: rect(275, 238, 397, 216),
            target_band: rect(288, 317, 371, 86),
            crack_button: rect(768, 238, 179, 173),
            timing_window_px: 23,
            track_band: rect(365, 155, 531, 76),
            indicator_band: rect(365, 216, 531, 86),
            pin_center_x: vec![416, 527, 634, 739, 845],
            pin_tolerance_px: 15,
            button_grid_region: rect(640, 194, 269, 234),
            progress_region: rect(335, 315, 198, 90),
            line_led_region: rect(809, 175, 288, 54),
            line_panel_region: rect(209, 184, 563, 317),
            line_col_x: vec![339, 465, 599, 698],
            line_row_y: vec![248, 292, 340, 389],
            line_knob_center: PxPoint { x: 941, y: 407 },
            max_duration_ms: 15_000,
            frame_budget: 300,
            poll_interval_ms: 50,
        }
    }

    /// 摩斯密码界面：3 个格各 5 个符号（4 点 1 划），符合「≥3 符号且 ≥1 划」。
    fn morse_frame() -> Frame {
        let mut canvas = Canvas::new(W, H);
        for cell in 0..MORSE_REGION_COUNT {
            let top = 300 + cell as i32 * 100 + 28;
            for symbol in 0..5 {
                let left = 900 + symbol * 60;
                let width = if symbol == 4 { 40 } else { 12 };
                canvas.paint(rect(left, top, width, 12), WHITE);
            }
        }
        canvas.frame()
    }

    /// 指纹密码界面：9 个候选格画横条纹（模拟指纹内纹理）+ 名字区若干文字笔画。
    fn fingerprint_frame() -> Frame {
        let mut canvas = Canvas::new(W, H);
        let view = spec_view();
        for cell in &view.fingerprint.candidate_boxes {
            for line in 0..cell.h / 8 {
                let y = cell.y + 4 + line * 8;
                canvas.paint(rect(cell.x + 10, y, cell.w - 20, 3), WHITE);
            }
            canvas.paint(rect(cell.x + cell.w / 2 - 3, cell.y + 10, 6, cell.h - 20), WHITE);
        }
        // 名字区：模拟 4 个汉字的笔画柱。
        for glyph in 0..4 {
            let left = view.fingerprint.name_region.x + 20 + glyph * 60;
            canvas.paint(rect(left, 42, 6, 26), WHITE);
            canvas.paint(rect(left + 14, 42, 6, 26), WHITE);
            canvas.paint(rect(left, 52, 34, 4), WHITE);
        }
        canvas.frame()
    }

    /// 操作台界面：右上 LED `NN-NN-NN-NN`（8 位 + 3 横杠）+ 面板 4 条等距竖列。
    fn console_frame() -> Frame {
        let mut canvas = Canvas::new(W, H);
        let mut cursor = 870;
        for (group, digits) in ["01", "00", "01", "11"].iter().enumerate() {
            for digit in digits.chars() {
                let width = if digit == '1' { 6 } else { 12 };
                canvas.paint(rect(cursor, 92, width, 40), WHITE);
                cursor += width + 6;
            }
            if group < 3 {
                canvas.paint(rect(cursor, 108, 14, 8), WHITE);
                cursor += 14 + 8;
            }
        }
        // 面板：4 条等距竖列。
        for x in [160, 300, 440, 580] {
            canvas.paint(rect(x, 200, 8, 300), WHITE);
        }
        // 米黄旋钮（v5 实测色）：操作台一类的硬证据。
        canvas.paint(rect(725, 320, 40, 40), [235, 205, 80]);
        canvas.frame()
    }

    #[test]
    fn morse_frame_is_recognised_as_morse() {
        let verdict = detect(&morse_frame(), &spec_view());
        assert_eq!(verdict.scene, Scene::Morse, "{verdict:?}");
        assert!(verdict.is_actionable());
        assert!(verdict.confidence > ADOPT_THRESHOLD, "{verdict:?}");
        assert!(
            verdict.score_of(Scene::Fingerprint) < 0.5 && verdict.score_of(Scene::Console) < 0.5,
            "其余场景必须低分：{verdict:?}"
        );
    }

    #[test]
    fn fingerprint_frame_is_recognised_as_fingerprint() {
        let verdict = detect(&fingerprint_frame(), &spec_view());
        assert_eq!(verdict.scene, Scene::Fingerprint, "{verdict:?}");
        assert!(verdict.score_of(Scene::Console) < 0.5, "{verdict:?}");
    }

    #[test]
    fn console_frame_is_recognised_as_console() {
        let verdict = detect(&console_frame(), &spec_view());
        assert_eq!(verdict.scene, Scene::Console, "{verdict:?}");
        assert!(verdict.score_of(Scene::Morse) < 0.5, "{verdict:?}");
    }

    #[test]
    fn blank_desktop_frame_is_unknown_and_not_actionable() {
        let blank = Canvas::new(W, H).frame();
        let verdict = detect(&blank, &spec_view());
        assert_eq!(verdict.scene, Scene::Unknown, "{verdict:?}");
        assert!(!verdict.is_actionable(), "Unknown 必须拒绝执行（spec D6）");
        assert!(verdict.confidence < ADOPT_THRESHOLD, "{verdict:?}");
        assert_eq!(verdict.reason, "未检测到游戏界面");
        assert!(verdict.scores.iter().all(|&score| score == 0.0), "{verdict:?}");

        // 纯色（非黑）桌面同样不得命中任何场景。
        let grey = {
            let mut canvas = Canvas::new(W, H);
            canvas.paint(rect(0, 0, W as i32, H as i32), [128, 128, 128]);
            canvas.frame()
        };
        let grey_verdict = detect(&grey, &spec_view());
        assert_eq!(grey_verdict.scene, Scene::Unknown, "{grey_verdict:?}");
        assert!(grey_verdict.confidence < ADOPT_THRESHOLD);
    }

    #[test]
    fn adoption_rule_needs_both_threshold_and_gap() {
        // 过门槛且拉开差距 → 采纳。
        assert_eq!(decide([0.9, 0.5, 0.1]).scene, Scene::Morse);
        assert_eq!(decide([0.1, 0.9, 0.2]).scene, Scene::Fingerprint);
        assert_eq!(decide([0.2, 0.1, 0.85]).scene, Scene::Console);

        // 过门槛但差距不足（0.9 - 0.8 = 0.1 ≤ 0.15）→ Unknown。
        assert_eq!(decide([0.9, 0.8, 0.1]).scene, Scene::Unknown);
        // 门槛是严格大于：0.6 不采纳。
        assert_eq!(decide([0.6, 0.1, 0.0]).scene, Scene::Unknown);
        assert_eq!(decide([0.61, 0.1, 0.0]).scene, Scene::Morse);
        // 差距是严格大于：0.75 - 0.6 = 0.15 不采纳。
        assert_eq!(decide([0.75, 0.6, 0.0]).scene, Scene::Unknown);
        assert_eq!(decide([0.76, 0.6, 0.0]).scene, Scene::Morse);
        // 全 0 → Unknown，且 confidence 就是最高分（0）。
        let none = decide([0.0, 0.0, 0.0]);
        assert_eq!(none.scene, Scene::Unknown);
        assert_eq!(none.confidence, 0.0);
    }

    #[test]
    fn scores_are_reported_in_fixed_order() {
        let verdict = decide([0.2, 0.7, 0.1]);
        assert_eq!(verdict.scores, [0.2, 0.7, 0.1]);
        assert_eq!(verdict.score_of(Scene::Morse), 0.2);
        assert_eq!(verdict.score_of(Scene::Fingerprint), 0.7);
        assert_eq!(verdict.score_of(Scene::Console), 0.1);
        assert_eq!(verdict.score_of(Scene::Unknown), 0.0);
        assert_eq!(
            Scene::ORDER.map(Scene::name),
            ["Morse", "Fingerprint", "Console"]
        );
    }

    /// 大保险：目标带 (288,317)-(659,403) 内 5 个等距亮绿字符格。
    fn safe_timing_frame() -> Frame {
        let mut canvas = Canvas::new(W, H);
        for cell in 0..5 {
            canvas.paint(rect(294 + cell * 74, 330, 48, 40), [40, 220, 60]);
        }
        canvas.frame()
    }

    /// 任务保险：轨道带 (365,155)-(896,231) 内 5 根细长白竖条（实测槽中心 x）。
    fn safe_pins_frame() -> Frame {
        let mut canvas = Canvas::new(W, H);
        for center_x in [416, 527, 634, 739, 845] {
            canvas.paint(rect(center_x - 4, 160, 8, 60), WHITE);
        }
        canvas.frame()
    }

    /// 黄键台：金属台面 + 左侧深色屏幕（内有一行 5 个进度框）+ 右侧 4×4 暗键阵。
    ///
    /// 第 5 颗键（行 1 列 1）**整颗亮成黄块** = 「亮键」：这样全帧黄/绿像素够多，
    /// 能过场景判别那道「便宜的快速过滤」（放宽到整帧口径后仍要求 ≥0.1%）。
    /// 台面必须比键亮（灰 120 vs 键 40），暗键阵才数得出来。
    fn module_keys_frame() -> Frame {
        let mut canvas = Canvas::new(W, H);
        canvas.paint(rect(540, 170, 440, 300), [120, 120, 120]);
        canvas.paint(rect(555, 200, 145, 140), [20, 22, 24]);
        for stage in 0..5 {
            let frame = rect(570 + stage * 24, 290, 18, 14);
            canvas.paint(rect(frame.x, frame.y, frame.w, 2), [200, 200, 200]);
            canvas.paint(rect(frame.x, frame.bottom() - 2, frame.w, 2), [200, 200, 200]);
            canvas.paint(rect(frame.x, frame.y, 2, frame.h), [200, 200, 200]);
            canvas.paint(rect(frame.right() - 2, frame.y, 2, frame.h), [200, 200, 200]);
        }
        for row in 0..4 {
            for col in 0..4 {
                let key = rect(760 + col * 52, 200 + row * 52, 38, 38);
                let lit = row == 1 && col == 1;
                canvas.paint(key, if lit { [235, 200, 40] } else { [40, 40, 40] });
            }
        }
        canvas.frame()
    }

    /// v5 旧操作台：青绿面板 + 面板 4 列 + 米黄旋钮 + 旧 LED。
    ///
    /// 关键点：米黄旋钮的琥珀色满足「黄绿高亮」——这正是旧操作台曾被误判成黄键台的路径。
    /// 现在黄键台要求「4×4 暗键阵 + 左侧屏幕」这套结构证据，旧面板凑不出来。
    fn legacy_console_frame() -> Frame {
        let mut canvas = Canvas::new(W, H);
        // 青绿面板（H≈90 / S≈120 / V≈110），覆盖 AZ3 网格所在区域。
        canvas.paint(rect(560, 150, 420, 330), [0, 110, 110]);
        // 旧操作台面板的 4 条等距竖列。
        for x in [160, 300, 440, 580] {
            canvas.paint(rect(x, 200, 8, 300), WHITE);
        }
        // 米黄旋钮（被误判成「亮键」的那颗）。
        canvas.paint(rect(700, 316, 40, 30), [235, 205, 80]);
        // 旧 LED 序列（在 view.console.led_region 里）。
        let mut cursor = 870;
        for _ in 0..8 {
            canvas.paint(rect(cursor, 92, 12, 40), WHITE);
            cursor += 26;
        }
        canvas.frame()
    }

    /// 信号线台：AZ3 LED 区 (809,175)-(1097,229) 内 `NN-NN-NN-NN` + 面板 4 条竖线 + 暗红旋钮。
    /// 完整信号线台：青色屏幕 + 4×4 档位 + LED 条 + 暗红旋钮（自适应定位的合成机台）。
    fn signal_line_frame() -> Frame {
        crate::locate::fixture::machine_frame([None; 4])
    }

    /// 摩斯界面：深蓝面板 + 三格摩斯 + 右侧数字键盘 + **顶部指南针刻度条**。
    ///
    /// `knob` 非空时在同一位置补一颗暗红旋钮，用来证明「旋钮存在」是唯一变量。
    /// 这一帧曾经被判成「信号线台」并真的去拖旋钮（实机日志：摩斯界面发出 64 个输入事件）。
    fn morse_like_frame(knob: Option<[u8; 3]>) -> Frame {
        let mut canvas = Canvas::new(W, H);
        // 深蓝面板（覆盖 AZ3 面板区），带 4 条等距竖线（数字键盘的列）。
        canvas.paint(rect(300, 160, 700, 460), [30, 45, 70]);
        for x in [339, 465, 599, 698] {
            canvas.paint(rect(x, 250, 4, 60), WHITE);
        }
        // 顶部指南针刻度：一串数字（旧的 LED 判据就是被它骗过的）。
        let mut cursor = 815;
        for _ in 0..8 {
            canvas.paint(rect(cursor, 185, 12, 40), WHITE);
            cursor += 26;
        }
        if let Some(rgb) = knob {
            canvas.paint(rect(925, 390, 32, 32), rgb);
        }
        canvas.frame()
    }

    #[test]
    fn console_needs_a_warm_knob() {
        // 摩斯界面（深蓝面板 + 4 条竖列 + 顶部刻度数字）没有暖色旋钮 ——
        // 旧判据会把它当成信号线台并拖走旋钮，实机日志已复现（64 个输入事件）。
        let morse = morse_like_frame(None);
        assert_ne!(detect(&morse, &spec_view()).scene, Scene::Console);
        assert_eq!(az3_variant(&morse, &spec_view()), Az3Kind::None);

        // 同一帧补上暗红旋钮后**仍然不是**信号线台：自适应定位要的是「旋钮 + 青色屏幕 +
        // LED 数字条 + 4 列档位」这一整套结构证据，单有一颗旋钮不够。
        let with_knob = morse_like_frame(Some([140, 40, 30]));
        assert_eq!(az3_variant(&with_knob, &spec_view()), Az3Kind::None);
    }

    #[test]
    fn az3_variant_identifies_all_four_types_and_rejects_blank() {
        let view = spec_view();

        assert_eq!(az3_variant(&Canvas::new(W, H).frame(), &view), Az3Kind::None);
        assert_eq!(az3_variant(&module_keys_frame(), &view), Az3Kind::ModuleKeys);
        assert_eq!(az3_variant(&safe_pins_frame(), &view), Az3Kind::SafePins);
        assert_eq!(az3_variant(&safe_timing_frame(), &view), Az3Kind::SafeTiming);
        assert_eq!(az3_variant(&signal_line_frame(), &view), Az3Kind::SignalLine);
    }

    #[test]
    fn yellow_glow_survives_large_red_green_sums() {
        // 谓词入参是 [B, G, R]。u8 饱和会让 r+g 恒为 255 → 亮黄键漏检。
        assert!(is_yellow_or_green_glow([105, 200, 240]), "亮黄键必须判成高亮");
        assert!(is_yellow_or_green_glow([120, 200, 230]), "米黄旋钮也会命中（靠暗键阵拦截）");
        assert!(!is_yellow_or_green_glow([255, 255, 255]), "纯白不是高亮");
    }

    #[test]
    fn az3_variant_requires_the_4x4_dark_key_grid() {
        // 「亮色 + 4 条等距竖列」不足以判黄键台：面板右缘 + 琥珀旋钮同样能凑出这两条。
        // 必须有 4×4 暗键阵的结构证据才算数。
        let bars_with_amber = {
            let mut canvas = Canvas::new(W, H);
            for x in [660, 725, 790, 855] {
                canvas.paint(rect(x, 210, 8, 200), WHITE);
            }
            canvas.paint(rect(700, 316, 40, 30), [235, 205, 80]);
            canvas.frame()
        };
        assert_eq!(az3_variant(&bars_with_amber, &spec_view()), Az3Kind::None);
    }

    #[test]
    fn az3_variant_does_not_hijack_the_legacy_console() {
        // v5 旧操作台：青绿面板 + 米黄旋钮（落在 AZ3 网格区里）+ 旧 LED。
        // 必须先被认成 Console，但 AZ3 变体必须是 None —— 否则 worker 会拿 AZ3 网格坐标去点旧面板。
        let legacy = legacy_console_frame();
        assert_eq!(detect(&legacy, &spec_view()).scene, Scene::Console);
        assert_eq!(az3_variant(&legacy, &spec_view()), Az3Kind::None);

        // 最简情形：LED 只在旧 `view.console.led_region` 里，AZ3 LED 区是空的。
        let led_only = {
            let mut canvas = Canvas::new(W, H);
            let mut cursor = 870;
            for _ in 0..8 {
                canvas.paint(rect(cursor, 92, 12, 40), WHITE);
                cursor += 26;
            }
            canvas.frame()
        };
        assert_eq!(az3_variant(&led_only, &spec_view()), Az3Kind::None);
        assert_eq!(az3_variant(&console_frame(), &spec_view()), Az3Kind::None);
    }

    #[test]
    fn az3_variant_ignores_low_contrast_non_features() {
        // 纯灰桌面：既没有亮键也没有白竖条，必须拒绝执行。
        let grey = {
            let mut canvas = Canvas::new(W, H);
            canvas.paint(rect(0, 0, W as i32, H as i32), [128, 128, 128]);
            canvas.frame()
        };
        assert_eq!(az3_variant(&grey, &spec_view()), Az3Kind::None);
    }
}

