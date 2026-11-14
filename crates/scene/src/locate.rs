//! AZ3 信号线台（横线台）的**每帧自适应定位**。
//!
//! 实机录像实测（见 `.scratch/az3/line-geometry.md`）：旋钮中心在 `(0.604,0.554)~(0.766,0.573)`
//! 之间漂，屏幕与 LED 同步漂 —— 任何固定百分比坐标都必然时而命中、时而全落空。因此这里
//! 每帧从画面重新量几何：
//!
//! 1. **旋钮**：整帧降采样粗扫暗红，取面积最大、宽高比 0.55–1.8、填充率 ≥0.45 的块；
//! 2. **屏幕**：旋钮左侧找青色边框构成的最大横向矩形轮廓，取其内部；
//! 3. **LED**：屏幕右缘与旋钮上方之间找「暗底 + 亮字」长条（4 组 2 位数字条）；
//! 4. **4 列**：屏幕内白色竖条的列投影，取间距最均匀的 4 个峰；
//! 5. **4 行**：只在那 4 个列窗内做行投影（信号线曲线在各列的穿越高度不同，会被稀释掉），
//!    取间距最均匀的 4 条带；
//! 6. **每列选中档位**：档位阵里被**琥珀色定位框**圈住的那一行（实测 t96：第 1/2/3 列分别
//!    在 10/11/01 行亮出琥珀框）。没有琥珀框 → `None`（**不猜**）。
//!
//! 性能红线：只扫候选小区域，绝不整帧逐像素 HSV。旋钮粗扫按步长降采样
//! （整帧 ≤ 480×270 次采样），屏幕在降采样网格上找轮廓，LED 与档位阵只扫窄窗。

use delta_capture::Frame;
use delta_geom::{PxPoint, PxRect};
use delta_vision::{external_contours, BinaryImage, Contour, Region};
use thiserror::Error;

use crate::features::column_segments;

/// 档位列数。
pub const LINE_COLUMNS: usize = 4;
/// 档位行数（行标签 `10/11/00/01` 自上而下）。
pub const LINE_ROWS: usize = 4;

/// 低于这个边长的帧直接判「画面太小」，不做任何探测。
const MIN_FRAME_SIDE: u32 = 160;

/// 旋钮粗扫的目标采样列数（分辨率无关：1920 宽 → 步长 4）。
const KNOB_SAMPLES: u32 = 480;
/// 粗扫最多精修多少个候选块（HUD 罗盘等大面积暗红会排在最前）。
const KNOB_CANDIDATES: usize = 8;
/// 旋钮块面积下限（720p 旋钮约 1200px，1080p 约 2700px）。
const KNOB_MIN_AREA: u32 = 300;
/// 旋钮块面积上限：超过这个量级说明是 HUD 罗盘 / 大红按钮之类。
const KNOB_MAX_AREA: u32 = 8_000;
/// 旋钮块填充率下限（圆盘在正方形外接框里约 0.78）。
const KNOB_MIN_FILL: f32 = 0.45;
/// 旋钮块宽高比区间。
const KNOB_ASPECT: (f32, f32) = (0.55, 1.8);

/// 屏幕轮廓的目标采样列数。
const SCREEN_SAMPLES: u32 = 400;
/// 屏幕搜索窗：旋钮左侧 0..0.75 屏宽。
const SCREEN_SIDE: f32 = 0.75;
/// 屏幕搜索窗：0.10..0.95 屏高。
const SCREEN_BAND: (f32, f32) = (0.10, 0.95);
/// 屏幕轮廓宽高比区间（CRT 是横向矩形）。
const SCREEN_ASPECT: (f32, f32) = (1.1, 3.0);
/// 屏幕轮廓的最小面积（降采样格子数，滤掉零碎青色噪点）。
const SCREEN_MIN_CELLS: u32 = 800;

/// LED 搜索窗：旋钮上方 0.10..0.55 屏高。
const LED_ABOVE: (f32, f32) = (0.10, 0.55);
/// LED 搜索窗：旋钮左右各 0.35 屏宽。
const LED_SIDE: f32 = 0.35;
/// LED 面板（暗底）宽高比区间。
const LED_PANEL_ASPECT: (f32, f32) = (2.5, 8.0);
/// LED 面板（暗底）最小面积。
const LED_PANEL_MIN_AREA: u32 = 6_000;
/// 亮字在字条外接框内的占比区间。
///
/// 720p 实机实测 0.262（spec 给的上限 0.30 只留 4% 余量，换一种数字字形就会顶出去），
/// 所以上限放宽到 0.40：仍能挡住「一整块亮板」这类假阳性。
const LED_GLYPH_RATIO: (f32, f32) = (0.02, 0.40);
/// 字条能切出的字符段数区间（`10-11-01-10` = 8 位 + 3 个横杠，实测正好 11）。
///
/// 上限放到 13：实测已经踩在 11 上，面板里多一列反光亮线不该让整条判据失效。
const LED_SEGMENTS: (usize, usize) = (7, 13);
/// 字条外接框宽高比下限。
const LED_MIN_ASPECT: f32 = 3.0;

/// 行带的合法高度区间（档位方块约 24px 高）。
const ROW_RUN: (usize, usize) = (10, 60);
/// 间距均匀度上限：`(max_gap - min_gap) / max_gap`。
const SPACING_SPREAD: f32 = 0.25;
/// 琥珀定位框的像素数下限（720p 实测 106~183）。
const MARKER_MIN_PIXELS: u32 = 40;

/// 定位结果：信号线台的全部几何都由这一帧量出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedLine {
    /// 青色 CRT 屏幕矩形（已剔除边框）。
    pub screen: PxRect,
    /// 4 列的 x 中心（自左向右）。
    pub columns: Vec<i32>,
    /// 4 行的 y 中心（自上而下，对应行标签 `10/11/01/00`）。
    pub rows: Vec<i32>,
    /// 16 个候选档位方块的外接框，**列优先、行次之**：`targets[col * 4 + row]`。
    pub targets: Vec<PxRect>,
    /// 每列当前被琥珀框圈住的档位行；`None` = 这一列看不出位置（不猜）。
    pub selected: Vec<Option<usize>>,
    /// 右上 LED 数字条（亮字外接框）。
    pub led: PxRect,
    /// 暗红旋钮中心。
    pub knob: PxPoint,
}

impl LocatedLine {
    /// 第 `column` 列第 `row` 行档位方块的中心（拖动的按下点与落点）。
    pub fn slot_center(&self, column: usize, row: usize) -> Option<PxPoint> {
        let slot = *self.targets.get(column.checked_mul(LINE_ROWS)?.checked_add(row)?)?;
        Some(PxPoint {
            x: slot.x + slot.w / 2,
            y: slot.y + slot.h / 2,
        })
    }

    /// 行间距（拖动一步的像素量级）。
    pub fn row_gap(&self) -> i32 {
        match (self.rows.first(), self.rows.get(1)) {
            (Some(top), Some(next)) => (next - top).max(1),
            _ => 1,
        }
    }
}

/// 定位失败的原因。文案直接进 Abort 提示给用户看，所以每条都要说清「卡在哪一项」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LocateError {
    #[error("画面太小（{w}x{h}），无法定位信号线台")]
    Frame { w: u32, h: u32 },
    #[error("没找到红色旋钮")]
    Knob,
    #[error("没找到左侧的操作台屏幕")]
    Screen,
    #[error("没找到右上角的 01-10 数字条")]
    Led,
    #[error("屏幕上数不到 4 列档位")]
    Columns,
}

/// 一帧定位：全部几何都由 `frame` 决定，不读配置、不碰 IO。
pub fn locate_line(frame: &Frame) -> Result<LocatedLine, LocateError> {
    if frame.w < MIN_FRAME_SIDE || frame.h < MIN_FRAME_SIDE {
        return Err(LocateError::Frame {
            w: frame.w,
            h: frame.h,
        });
    }
    let knob = find_knob(frame).ok_or(LocateError::Knob)?;
    let screen = find_screen(frame, knob).ok_or(LocateError::Screen)?;
    let led = find_led(frame, knob, screen).ok_or(LocateError::Led)?;
    let grid = find_grid(frame, screen).ok_or(LocateError::Columns)?;
    Ok(LocatedLine {
        screen,
        led,
        knob,
        columns: grid.columns,
        rows: grid.rows,
        targets: grid.targets,
        selected: grid.selected,
    })
}

// ── 1. 旋钮 ──

/// 暗红旋钮：等价于 OpenCV 标度 `H∈[0,14]∪[170,180] ∧ S≥90 ∧ V≥70`。
///
/// 展开成整数比较是因为粗扫要过十几万像素，逐像素 f32 HSV 太贵：
/// `V = r`（红为最大分量）、`S ≥ 90 ⇒ delta ≥ 0.353·r`、
/// `H ∈ [0,14] ⇒ (g−b) ≤ 0.467·delta`、`H ∈ [170,180] ⇒ (g−b) ≥ −0.334·delta`。
fn is_dark_red([b, g, r]: [u8; 3]) -> bool {
    let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
    if r < 70 || r < g || r < b {
        return false;
    }
    let delta = r - g.min(b);
    let warm = g - b;
    delta * 1000 >= 353 * r && warm * 1000 <= 467 * delta && warm * 1000 >= -334 * delta
}

/// 粗扫得到的一块暗红候选：像素外接框 + 命中的降采样格子数。
struct CoarseBlock {
    rect: PxRect,
    cells: u32,
}

fn find_knob(frame: &Frame) -> Option<PxPoint> {
    let step = (frame.w / KNOB_SAMPLES).max(2);
    let mut grid = CoarseGrid::new(frame, step);
    grid.blocks()
        .into_iter()
        .take(KNOB_CANDIDATES)
        .find_map(|block| refine_knob(frame, block, step))
}

/// 降采样网格上的暗红命中图：一次分配、一次遍历，连通域由 [`CoarseGrid::blocks`] 取。
struct CoarseGrid {
    hits: Vec<bool>,
    seen: Vec<bool>,
    w: u32,
    h: u32,
    step: u32,
    min_cells: u32,
}

impl CoarseGrid {
    fn new(frame: &Frame, step: u32) -> Self {
        let w = frame.w.div_ceil(step);
        let h = frame.h.div_ceil(step);
        let mut hits = vec![false; (w * h) as usize];
        for gy in 0..h {
            for gx in 0..w {
                let x = (gx * step).min(frame.w - 1);
                let y = (gy * step).min(frame.h - 1);
                hits[(gy * w + gx) as usize] = is_dark_red(frame.pixel(x, y));
            }
        }
        Self {
            seen: vec![false; hits.len()],
            hits,
            w,
            h,
            step,
            min_cells: (KNOB_MIN_AREA / (step * step)).max(4),
        }
    }

    /// 全部连通块，按命中格子数降序（面积大的在前）。
    fn blocks(&mut self) -> Vec<CoarseBlock> {
        let mut blocks = Vec::new();
        for gy in 0..self.h {
            for gx in 0..self.w {
                if let Some(block) = self.flood(gx, gy) {
                    blocks.push(block);
                }
            }
        }
        blocks.sort_by_key(|block| std::cmp::Reverse(block.cells));
        blocks
    }

    fn flood(&mut self, seed_x: u32, seed_y: u32) -> Option<CoarseBlock> {
        let index = (seed_y * self.w + seed_x) as usize;
        if self.seen[index] || !self.hits[index] {
            return None;
        }
        self.seen[index] = true;
        let mut stack = vec![(seed_x, seed_y)];
        let (mut left, mut right, mut top, mut bottom, mut cells) =
            (seed_x, seed_x, seed_y, seed_y, 0u32);
        while let Some((cx, cy)) = stack.pop() {
            cells += 1;
            left = left.min(cx);
            right = right.max(cx);
            top = top.min(cy);
            bottom = bottom.max(cy);
            for (nx, ny) in self.neighbours(cx, cy) {
                let next = (ny * self.w + nx) as usize;
                if self.seen[next] || !self.hits[next] {
                    continue;
                }
                self.seen[next] = true;
                stack.push((nx, ny));
            }
        }
        (cells >= self.min_cells).then(|| CoarseBlock {
            rect: PxRect {
                x: (left * self.step) as i32,
                y: (top * self.step) as i32,
                w: ((right + 1 - left) * self.step) as i32,
                h: ((bottom + 1 - top) * self.step) as i32,
            },
            cells,
        })
    }

    fn neighbours(&self, cx: u32, cy: u32) -> impl Iterator<Item = (u32, u32)> {
        let (w, h) = (self.w, self.h);
        [(-1, 0), (1, 0), (0, -1), (0, 1)]
            .into_iter()
            .filter_map(move |(dx, dy): (i32, i32)| {
                let nx = cx as i32 + dx;
                let ny = cy as i32 + dy;
                (nx >= 0 && ny >= 0 && (nx as u32) < w && (ny as u32) < h)
                    .then_some((nx as u32, ny as u32))
            })
    }
}

fn refine_knob(frame: &Frame, block: CoarseBlock, step: u32) -> Option<PxPoint> {
    let mask = masked(frame, grow(block.rect, step as i32 * 2), is_dark_red)?;
    let best = external_contours(&mask.binary)
        .into_iter()
        .filter(|contour| (KNOB_MIN_AREA..=KNOB_MAX_AREA).contains(&contour.area))
        .filter(|contour| within(KNOB_ASPECT, contour.aspect()))
        .filter(|contour| fill_of(contour) >= KNOB_MIN_FILL)
        .max_by_key(|contour| contour.area)?;
    let rect = mask.to_frame(best.region, 1);
    Some(PxPoint {
        x: rect.x + rect.w / 2,
        y: rect.y + rect.h / 2,
    })
}

// ── 2. 屏幕 ──

/// 屏幕青色边框：OpenCV 标度 `H∈[80,100] ∧ S≥60 ∧ V∈[60,200]`。
fn is_screen_cyan(color: [u8; 3]) -> bool {
    let (hue, sat, val) = hsv(color);
    (80.0..=100.0).contains(&hue) && sat >= 60.0 && (60.0..=200.0).contains(&val)
}

fn find_screen(frame: &Frame, knob: PxPoint) -> Option<PxRect> {
    let window = PxRect {
        x: 0,
        y: (frame.h as f32 * SCREEN_BAND.0) as i32,
        w: (frame.w as f32 * SCREEN_SIDE).min(knob.x as f32) as i32,
        h: (frame.h as f32 * (SCREEN_BAND.1 - SCREEN_BAND.0)) as i32,
    };
    let step = (frame.w / SCREEN_SAMPLES).max(2);
    let mask = sampled(frame, window, step, is_screen_cyan)?;
    let best = external_contours(&mask.binary)
        .into_iter()
        .filter(|contour| within(SCREEN_ASPECT, contour.aspect()))
        .filter(|contour| contour.area >= SCREEN_MIN_CELLS)
        .max_by_key(|contour| contour.area)?;
    let rect = mask.to_frame(best.region, step);
    Some(inset(rect, step as i32 * 2))
}

// ── 3. LED ──

/// LED 面板底色：近黑（`V ≤ 60`）。数字条是「暗底 + 亮字」，不是亮底暗字。
fn is_led_panel_dark(color: [u8; 3]) -> bool {
    color.iter().copied().max().unwrap_or(0) <= 60
}

/// LED 亮字：`V ≥ 200 ∧ S ≤ 60`（远高于机台外壳的灰 ~150，用来把字从金属面上切开）。
fn is_led_glyph(color: [u8; 3]) -> bool {
    let (max, delta) = brightness(color);
    max >= 200 && delta <= 60
}

fn find_led(frame: &Frame, knob: PxPoint, screen: PxRect) -> Option<PxRect> {
    let window = led_window(frame, knob, screen)?;
    let panel = masked(frame, window, is_led_panel_dark)?;
    let best = external_contours(&panel.binary)
        .into_iter()
        .filter(|contour| within(LED_PANEL_ASPECT, contour.aspect()))
        .filter(|contour| contour.area >= LED_PANEL_MIN_AREA)
        .max_by_key(|contour| contour.area)?;
    led_glyphs(frame, panel.to_frame(best.region, 1))
}

/// LED 搜索窗：**屏幕右缘**到旋钮右侧 0.35 屏宽、旋钮上方 0.10~0.55 屏高之间。
///
/// 左边界取屏幕右缘而不是「旋钮左侧」：屏幕标题栏与说明文字也是亮字，
/// 落在同一个高度带上会把字符段数顶到 11 以上。
fn led_window(frame: &Frame, knob: PxPoint, screen: PxRect) -> Option<PxRect> {
    let x0 = screen.right() + 2;
    let x1 = (knob.x as f32 + frame.w as f32 * LED_SIDE) as i32;
    let y0 = (knob.y as f32 - frame.h as f32 * LED_ABOVE.1) as i32;
    let y1 = (knob.y as f32 - frame.h as f32 * LED_ABOVE.0) as i32;
    (x1 > x0 && y1 > y0).then(|| PxRect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

fn led_glyphs(frame: &Frame, panel: PxRect) -> Option<PxRect> {
    let glyphs = masked(frame, panel, is_led_glyph)?;
    let rect = glyphs.bounding_box()?;
    let area = glyphs.binary.foreground_count();
    let ratio = area as f32 / (rect.w * rect.h).max(1) as f32;
    if !within(LED_GLYPH_RATIO, ratio) {
        return None;
    }
    let segments = column_segments(&glyphs.binary, 2);
    if !within_usize(LED_SEGMENTS, segments) {
        return None;
    }
    let glyph_box = glyphs.to_frame(rect, 1);
    (glyph_box.w as f32 / glyph_box.h.max(1) as f32 >= LED_MIN_ASPECT).then_some(grow(glyph_box, 2))
}

// ── 4/5/6. 列、行、选中档位 ──

/// 档位方块亮面：`V ≥ 150 ∧ S ≤ 60`（白或冷白青）。
fn is_slot_bright(color: [u8; 3]) -> bool {
    let (max, delta) = brightness(color);
    max >= 150 && delta <= 60
}

/// 选中档位的琥珀色定位框：与 OpenCV 标度 `H∈[15,40] ∧ S≥70 ∧ V≥110` 等价。
///
/// `H∈[15,40]` 只在「红为最大分量」时成立，此时 `hue = 30·(g−b)/delta`，
/// 于是 `(g−b)/delta ∈ [0.5, 1.333]`；`S≥70 ⇒ delta ≥ 0.275·max`。
/// 展开成整数比较是为了让整屏掩码（30 万像素）不必逐像素做 f32 HSV。
fn is_marker_amber([b, g, r]: [u8; 3]) -> bool {
    let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
    if r < 110 || r < g || r < b {
        return false;
    }
    let delta = r - g.min(b);
    let warm = (g - b) * 1000;
    delta * 1000 >= 275 * r && warm >= 500 * delta && warm <= 1333 * delta
}

struct Grid {
    columns: Vec<i32>,
    rows: Vec<i32>,
    targets: Vec<PxRect>,
    selected: Vec<Option<usize>>,
}

fn find_grid(frame: &Frame, screen: PxRect) -> Option<Grid> {
    let field = masked(frame, screen, is_slot_bright)?;
    let columns = field_columns(&field)?;
    let rows = field_rows(&field, &columns)?;
    let selected = field_selected(frame, &columns, &rows);
    let targets = slot_rects(&columns, &rows);
    Some(Grid {
        columns,
        rows,
        targets,
        selected,
    })
}

fn field_columns(field: &Masked) -> Option<Vec<i32>> {
    let profile = field.column_counts();
    let tallest = profile.iter().copied().max().unwrap_or(0);
    if tallest == 0 {
        return None;
    }
    let threshold = ((tallest as f32) * 0.25).max(6.0) as u32;
    let centers: Vec<i32> = runs(&profile, threshold)
        .into_iter()
        .map(|(start, end)| field.x0 as i32 + (start + end) as i32 / 2)
        .collect();
    let min_gap = (field.binary.width() as i32) / 8;
    most_even(&centers, min_gap).map(|picked| picked.to_vec())
}

/// 行带只在 4 个列窗内投影：信号线曲线在每列的穿越高度不同，横向求和会被稀释，
/// 而档位方块在四列里高度一致 —— 于是 4 条等距带自然浮出来。
fn field_rows(field: &Masked, columns: &[i32]) -> Option<Vec<i32>> {
    let half = (field.binary.width() as i32 / 90).max(7);
    let mut profile = vec![0u32; field.binary.height() as usize];
    for &column in columns {
        let start = (column - field.x0 as i32 - half).max(0) as u32;
        let end = (column - field.x0 as i32 + half + 1).max(0) as u32;
        for (y, slot) in profile.iter_mut().enumerate() {
            for x in start..end.min(field.binary.width()) {
                if field.binary.get(x, y as u32) {
                    *slot += 1;
                }
            }
        }
    }
    let centers: Vec<i32> = runs(&profile, 5)
        .into_iter()
        .filter(|(start, end)| within_usize(ROW_RUN, end - start))
        .map(|(start, end)| field.y0 as i32 + (start + end) as i32 / 2)
        .collect();
    most_even(&centers, 8).map(|picked| picked.to_vec())
}

/// 每列的选中行：档位阵里琥珀框像素最多的一行；不足 `MARKER_MIN_PIXELS` → `None`。
///
/// 只扫 16 个小窗（合计约 2 万像素），不为整屏建掩码 —— 1080p 下这一项差一个数量级。
fn field_selected(frame: &Frame, columns: &[i32], rows: &[i32]) -> Vec<Option<usize>> {
    let gap = row_gap_of(rows);
    let half_x = (gap / 5).max(6);
    let half_y = (gap / 3).max(8);
    columns
        .iter()
        .map(|&column| {
            rows.iter()
                .enumerate()
                .map(|(row, &y)| {
                    let window = PxRect {
                        x: column - half_x,
                        y: y - half_y,
                        w: half_x * 2 + 1,
                        h: half_y * 2 + 1,
                    };
                    (row, count_in(frame, window, is_marker_amber))
                })
                .max_by_key(|&(_, count)| count)
                .filter(|&(_, count)| count >= MARKER_MIN_PIXELS)
                .map(|(row, _)| row)
        })
        .collect()
}

fn slot_rects(columns: &[i32], rows: &[i32]) -> Vec<PxRect> {
    let gap = row_gap_of(rows);
    let w = (gap / 5).max(6);
    let h = (gap / 3).max(8);
    columns
        .iter()
        .flat_map(|&x| {
            rows.iter().map(move |&y| PxRect {
                x: x - w / 2,
                y: y - h / 2,
                w,
                h,
            })
        })
        .collect()
}

fn row_gap_of(rows: &[i32]) -> i32 {
    match (rows.first(), rows.get(1)) {
        (Some(top), Some(next)) => (next - top).max(1),
        _ => 1,
    }
}

// ── 像素原语 ──

/// 帧内窗口 + 该窗口的二值掩码。`to_frame` 负责把掩码里的局部坐标换算回帧坐标。
struct Masked {
    binary: BinaryImage,
    x0: u32,
    y0: u32,
}

impl Masked {
    fn to_frame(&self, region: Region, step: u32) -> PxRect {
        PxRect {
            x: self.x0 as i32 + (region.x * step) as i32,
            y: self.y0 as i32 + (region.y * step) as i32,
            w: (region.w * step) as i32,
            h: (region.h * step) as i32,
        }
    }

    fn column_counts(&self) -> Vec<u32> {
        (0..self.binary.width())
            .map(|x| {
                (0..self.binary.height())
                    .filter(|&y| self.binary.get(x, y))
                    .count() as u32
            })
            .collect()
    }

    /// 前景外接框（局部坐标，右下开区间）。
    fn bounding_box(&self) -> Option<Region> {
        let (mut left, mut right, mut top, mut bottom) = (None, 0u32, None, 0u32);
        for y in 0..self.binary.height() {
            for x in 0..self.binary.width() {
                if !self.binary.get(x, y) {
                    continue;
                }
                left = Some(left.map_or(x, |value: u32| value.min(x)));
                right = right.max(x);
                top = Some(top.map_or(y, |value: u32| value.min(y)));
                bottom = bottom.max(y);
            }
        }
        let x = left?;
        let y = top?;
        Some(Region {
            x,
            y,
            w: right - x + 1,
            h: bottom - y + 1,
        })
    }
}

/// 把矩形裁进帧内，返回右下开区间的像素边界；完全在帧外 → `None`。
fn clip(frame: &Frame, rect: PxRect) -> Option<(u32, u32, u32, u32)> {
    let x0 = rect.x.max(0).min(frame.w as i32) as u32;
    let y0 = rect.y.max(0).min(frame.h as i32) as u32;
    let x1 = rect.right().max(0).min(frame.w as i32) as u32;
    let y1 = rect.bottom().max(0).min(frame.h as i32) as u32;
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
}

/// 窗口内满足条件的像素数（越界自动裁掉）。
fn count_in(frame: &Frame, rect: PxRect, pred: impl Fn([u8; 3]) -> bool) -> u32 {
    let Some((x0, y0, x1, y1)) = clip(frame, rect) else {
        return 0;
    };
    (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .filter(|&(x, y)| pred(frame.pixel(x, y)))
        .count() as u32
}

/// 逐像素判定建掩码。
fn masked(frame: &Frame, rect: PxRect, pred: impl Fn([u8; 3]) -> bool) -> Option<Masked> {
    mask_with(frame, rect, 1, pred)
}

/// 降采样建掩码（`step` 像素取一个样本，掩码坐标 = 帧坐标 / step）。
fn sampled(
    frame: &Frame,
    rect: PxRect,
    step: u32,
    pred: impl Fn([u8; 3]) -> bool,
) -> Option<Masked> {
    mask_with(frame, rect, step, pred)
}

fn mask_with(
    frame: &Frame,
    rect: PxRect,
    step: u32,
    pred: impl Fn([u8; 3]) -> bool,
) -> Option<Masked> {
    let (x0, y0, x1, y1) = clip(frame, rect)?;
    let (w, h) = (
        (x1 - x0).div_ceil(step).max(1),
        (y1 - y0).div_ceil(step).max(1),
    );
    let mut binary = BinaryImage::new(w, h).ok()?;
    for gy in 0..h {
        for gx in 0..w {
            let x = (x0 + gx * step).min(frame.w - 1);
            let y = (y0 + gy * step).min(frame.h - 1);
            binary.set(gx, gy, pred(frame.pixel(x, y)));
        }
    }
    Some(Masked { binary, x0, y0 })
}

/// OpenCV 标度 HSV（H `0..180`、S/V `0..255`），与 `delta_flow_console::rgb_to_hsv` 同标度。
///
/// 这里是私有副本：`scene` 位于依赖图下方，不能反向依赖 `flow-console`。
fn hsv([b, g, r]: [u8; 3]) -> (f32, f32, f32) {
    let (rf, gf, bf) = (f32::from(r), f32::from(g), f32::from(b));
    let max = rf.max(gf).max(bf);
    let delta = max - rf.min(gf).min(bf);
    let sat = if max <= 0.0 { 0.0 } else { 255.0 * delta / max };
    if delta <= 0.0 {
        return (0.0, sat, max);
    }
    let raw = if max == rf {
        30.0 * ((gf - bf) / delta)
    } else if max == gf {
        30.0 * (2.0 + (bf - rf) / delta)
    } else {
        30.0 * (4.0 + (rf - gf) / delta)
    };
    (if raw < 0.0 { raw + 180.0 } else { raw }, sat, max)
}

/// 明度与饱和度分子（`S = 255·delta/max` 的 `(max, delta)`）。
fn brightness([b, g, r]: [u8; 3]) -> (u8, u8) {
    let max = r.max(g).max(b);
    (max, max - r.min(g).min(b))
}

fn runs(profile: &[u32], threshold: u32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (index, &value) in profile.iter().enumerate() {
        if value > threshold {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            out.push((begin, index));
        }
    }
    if let Some(begin) = start {
        out.push((begin, profile.len()));
    }
    out
}

/// 从候选值里挑 4 个间距最均匀的（滑动窗口即可：非连续组合只会让间距更不均匀）。
///
/// 这是「4 列 / 4 行」的判据本体：不等距就说明数歪了（把标签或边框算成了列），
/// 此时返回 `None`，调用方报错而不是猜。
fn most_even(values: &[i32], min_gap: i32) -> Option<[i32; 4]> {
    if values.len() < 4 {
        return None;
    }
    let mut best: Option<(f32, [i32; 4])> = None;
    for window in values.windows(4) {
        let gaps = [
            window[1] - window[0],
            window[2] - window[1],
            window[3] - window[2],
        ];
        if gaps.iter().any(|&gap| gap < min_gap) {
            continue;
        }
        let (low, high) = (
            *gaps.iter().min().unwrap_or(&0),
            *gaps.iter().max().unwrap_or(&0),
        );
        let spread = (high - low) as f32 / high as f32;
        if spread > SPACING_SPREAD {
            continue;
        }
        if best.is_none_or(|(current, _)| spread < current) {
            best = Some((spread, [window[0], window[1], window[2], window[3]]));
        }
    }
    best.map(|(_, picked)| picked)
}

fn within(range: (f32, f32), value: f32) -> bool {
    range.0 <= value && value <= range.1
}

fn within_usize(range: (usize, usize), value: usize) -> bool {
    value >= range.0 && value <= range.1
}

fn fill_of(contour: &Contour) -> f32 {
    contour.area as f32 / contour.region.area().max(1) as f32
}

fn grow(rect: PxRect, by: i32) -> PxRect {
    PxRect {
        x: rect.x - by,
        y: rect.y - by,
        w: rect.w + by * 2,
        h: rect.h + by * 2,
    }
}

fn inset(rect: PxRect, by: i32) -> PxRect {
    PxRect {
        x: rect.x + by,
        y: rect.y + by,
        w: (rect.w - by * 2).max(1),
        h: (rect.h - by * 2).max(1),
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::LINE_COLUMNS;
    use delta_capture::Frame;
    use delta_geom::PxRect;
    use std::time::Instant;

    /// 100% 脱敏的合成机台：无任何真实机位/坐标，只复刻「相对结构」。
    pub(crate) const W: u32 = 640;
    pub(crate) const H: u32 = 360;

    /// 机台金属面：亮灰（V≈160、低饱和）。
    pub(crate) const MACHINE: [u8; 3] = [148, 152, 150];
    /// 屏幕青色边框：OpenCV 标度 H≈90 / V≈190。
    pub(crate) const BEZEL: [u8; 3] = [10, 190, 190];
    /// 屏幕内容：深青（V≈60）。
    pub(crate) const PANEL: [u8; 3] = [10, 55, 60];
    /// 档位方块亮面：冷白青（V≈225、低饱和）。
    pub(crate) const SLOT: [u8; 3] = [190, 225, 225];
    /// 选中档位的琥珀定位框。
    pub(crate) const AMBER: [u8; 3] = [250, 210, 60];
    /// LED 面板底色：近黑。
    pub(crate) const LED_DARK: [u8; 3] = [25, 25, 25];
    /// LED 亮字。
    pub(crate) const LED_GLYPH: [u8; 3] = [240, 240, 240];
    /// 暗红旋钮。
    pub(crate) const KNOB: [u8; 3] = [140, 40, 30];

    pub(crate) const COLUMNS: [i32; 4] = [100, 180, 260, 340];
    pub(crate) const ROWS: [i32; 4] = [90, 140, 190, 240];
    pub(crate) const SCREEN: PxRect = PxRect {
        x: 40,
        y: 40,
        w: 380,
        h: 240,
    };
    pub(crate) const CONTENT: PxRect = PxRect {
        x: 52,
        y: 52,
        w: 356,
        h: 216,
    };
    pub(crate) const KNOB_RECT: PxRect = PxRect {
        x: 505,
        y: 235,
        w: 30,
        h: 30,
    };
    pub(crate) const LED_PANEL: PxRect = PxRect {
        x: 450,
        y: 60,
        w: 180,
        h: 50,
    };

    pub(crate) struct Canvas {
        data: Vec<u8>,
    }

    impl Canvas {
        pub(crate) fn new(fill: [u8; 3]) -> Self {
            let mut data = vec![0u8; (W * H * 4) as usize];
            for pixel in data.as_chunks_mut::<4>().0 {
                pixel[0] = fill[2];
                pixel[1] = fill[1];
                pixel[2] = fill[0];
                pixel[3] = 255;
            }
            Self { data }
        }

        pub(crate) fn rect(&mut self, rect: PxRect, rgb: [u8; 3]) -> &mut Self {
            for y in rect.y.max(0)..rect.bottom().min(H as i32) {
                for x in rect.x.max(0)..rect.right().min(W as i32) {
                    let index = ((y as u32 * W + x as u32) * 4) as usize;
                    self.data[index] = rgb[2];
                    self.data[index + 1] = rgb[1];
                    self.data[index + 2] = rgb[0];
                }
            }
            self
        }

        /// 空心中框（琥珀定位框就画成这种）。
        pub(crate) fn ring(&mut self, rect: PxRect, thickness: i32, rgb: [u8; 3]) -> &mut Self {
            let (left, top) = (PxRect {
                x: rect.x,
                y: rect.y,
                w: thickness,
                h: rect.h,
            }, PxRect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: thickness,
            });
            self.rect(left, rgb);
            self.rect(top, rgb);
            self.rect(
                PxRect {
                    x: rect.right() - thickness,
                    y: rect.y,
                    w: thickness,
                    h: rect.h,
                },
                rgb,
            );
            self.rect(
                PxRect {
                    x: rect.x,
                    y: rect.bottom() - thickness,
                    w: rect.w,
                    h: thickness,
                },
                rgb,
            )
        }

        /// 七段管字形：只画外框（真实亮字占比 ≈0.26，实心块会被占比判据挡住）。
        fn glyph(&mut self, rect: PxRect) -> &mut Self {
            self.ring(rect, 2, LED_GLYPH)
        }

        pub(crate) fn frame(&self) -> Frame {
            Frame::new(W, H, self.data.clone(), Instant::now()).expect("合法合成帧")
        }
    }

    pub(crate) fn slot_rect(column: i32, row: i32) -> PxRect {
        PxRect {
            x: column - 4,
            y: row - 9,
            w: 8,
            h: 18,
        }
    }

    /// 完整信号线台机台：金属面 + 青色 CRT + 4×4 档位阵 + LED 条 + 暗红旋钮。
    pub(crate) fn machine(selected: [Option<usize>; LINE_COLUMNS]) -> Canvas {
        let mut canvas = Canvas::new(MACHINE);
        canvas.rect(SCREEN, BEZEL);
        canvas.rect(CONTENT, PANEL);
        for column in COLUMNS {
            for row in ROWS {
                canvas.rect(slot_rect(column, row), SLOT);
            }
        }
        for (index, row) in selected.iter().enumerate() {
            if let Some(row) = row {
                let slot = slot_rect(COLUMNS[index], ROWS[*row]);
                canvas.ring(
                    PxRect {
                        x: slot.x - 6,
                        y: slot.y - 6,
                        w: slot.w + 12,
                        h: slot.h + 12,
                    },
                    3,
                    AMBER,
                );
            }
        }
        canvas.rect(LED_PANEL, LED_DARK);
        for index in 0..11 {
            let x = LED_PANEL.x + 12 + index * 14;
            let glyph = if index % 3 == 2 {
                PxRect {
                    x,
                    y: LED_PANEL.y + 22,
                    w: 5,
                    h: 6,
                }
            } else {
                PxRect {
                    x,
                    y: LED_PANEL.y + 12,
                    w: 9,
                    h: 26,
                }
            };
            canvas.glyph(glyph);
        }
        canvas.rect(KNOB_RECT, KNOB);
        canvas
    }

    /// 一帧完整的信号线台（场景判别与执行层测试共用）。
    pub(crate) fn machine_frame(selected: [Option<usize>; LINE_COLUMNS]) -> Frame {
        machine(selected).frame()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use std::time::Instant;


    #[test]
    fn locates_the_whole_signal_line_machine_in_one_frame() {
        let frame = machine([Some(0), Some(1), Some(2), Some(3)]).frame();
        let line = locate_line(&frame).expect("合成信号线台必须一次定位成功");

        assert_eq!(line.columns, COLUMNS.to_vec(), "4 列 x 中心");
        assert_eq!(line.rows, ROWS.to_vec(), "4 行 y 中心");
        assert_eq!(
            line.selected,
            vec![Some(0), Some(1), Some(2), Some(3)],
            "每列被琥珀框圈住的行"
        );
        assert_eq!(line.targets.len(), LINE_COLUMNS * LINE_ROWS);
        assert!(
            line.screen.right() < line.led.x,
            "LED 必须在屏幕右侧：screen={:?} led={:?}",
            line.screen,
            line.led
        );
        assert_eq!(line.knob, PxPoint { x: 520, y: 250 });
        assert!(
            (line.led.w as f32 / line.led.h as f32) >= 3.0,
            "数字条是长条：{:?}",
            line.led
        );
    }

    #[test]
    fn slot_centers_are_column_major() {
        let line = locate_line(&machine([None; 4]).frame()).expect("无琥珀框也能定位");
        assert_eq!(
            line.slot_center(0, 0),
            Some(PxPoint {
                x: COLUMNS[0],
                y: ROWS[0]
            })
        );
        assert_eq!(
            line.slot_center(3, 3),
            Some(PxPoint {
                x: COLUMNS[3],
                y: ROWS[3]
            })
        );
        assert_eq!(line.slot_center(4, 0), None, "只有 4 列，越界必须是 None");
        assert_eq!(line.row_gap(), 50);
    }

    #[test]
    fn slots_without_amber_marker_report_no_selection_instead_of_guessing() {
        let line = locate_line(&machine([None; 4]).frame()).expect("几何仍可定位");
        assert_eq!(
            line.selected,
            vec![None; 4],
            "没有琥珀框时绝不能猜一个档位出来"
        );
    }

    #[test]
    fn missing_knob_is_reported_before_anything_else() {
        let mut canvas = Canvas::new(MACHINE);
        canvas.rect(SCREEN, BEZEL);
        canvas.rect(inset(SCREEN, 12), PANEL);
        for column in COLUMNS {
            for row in ROWS {
                canvas.rect(slot_rect(column, row), SLOT);
            }
        }
        assert_eq!(locate_line(&canvas.frame()), Err(LocateError::Knob));
    }

    #[test]
    fn blank_machine_body_has_no_signal_line() {
        let frame = Canvas::new(MACHINE).frame();
        assert_eq!(locate_line(&frame), Err(LocateError::Knob));
    }

    #[test]
    fn screen_without_slot_columns_reports_columns() {
        let mut canvas = machine([None; 4]);
        // 把档位阵整块涂回面板底色：旋钮、屏幕、LED 都在，只剩 4 列数不出来。
        canvas.rect(inset(SCREEN, 12), PANEL);
        assert_eq!(locate_line(&canvas.frame()), Err(LocateError::Columns));
    }

    #[test]
    fn knob_without_screen_reports_screen() {
        let mut canvas = Canvas::new(MACHINE);
        canvas.rect(KNOB_RECT, KNOB);
        assert_eq!(locate_line(&canvas.frame()), Err(LocateError::Screen));
    }

    #[test]
    fn tiny_frame_is_rejected_without_scanning() {
        let frame = Frame::new(32, 32, vec![0u8; 32 * 32 * 4], Instant::now()).expect("合法帧");
        assert_eq!(locate_line(&frame), Err(LocateError::Frame { w: 32, h: 32 }));
    }

    #[test]
    fn most_even_rejects_uneven_picks() {
        // 标签框混进列候选（137/248/399/551/703）：只有后 4 个间距均匀。
        assert_eq!(
            most_even(&[137, 248, 399, 551, 703], 40),
            Some([248, 399, 551, 703])
        );
        assert_eq!(most_even(&[10, 400, 900], 40), None, "不足 4 个就没有 4 列");
        assert_eq!(most_even(&[0, 10, 20, 30], 40), None, "间距小于下限要拒绝");
    }
}
