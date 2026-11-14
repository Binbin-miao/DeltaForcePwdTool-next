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
//!
//! 同一个理由也适用于黄键台（[`locate_keys`]）：旧配置把 4×4 键阵写死成百分比矩形，
//! 换一版界面布局就整体偏出去，四等分切出来的格子落在键缝里 → 点空。这里逐帧重新量。

use delta_capture::Frame;
use delta_geom::{PxPoint, PxRect};
use delta_vision::{external_contours, BinaryImage, Contour, GrayImage, Region};
use thiserror::Error;

use crate::features::column_segments;

/// 档位列数。
pub const LINE_COLUMNS: usize = 4;
/// 档位行数（行标签 `10/11/00/01` 自上而下）。
pub const LINE_ROWS: usize = 4;

/// 低于这个边长的帧直接判「画面太小」，不做任何探测。
const MIN_FRAME_SIDE: u32 = 160;

/// 归一化基准高度：**所有像素阈值都按这个高度实测**。
///
/// 本文件里的 `KEY_SIDE = 18..70`、`KNOB_MIN_AREA = 300` 这类数字都是**绝对像素**，
/// 只有 720p 口径才成立。1080p 下同一颗键实测 86×83px，直接超出 `18..70` 上限 →
/// 定位全废、场景判 `Unknown`（实机日志 38/38 条 `场景: Unknown`，截图里明明就是黄键台）。
///
/// 与其给每个阈值乘一遍 `h/720`（几十处、极易漏），不如把帧统一缩到 720p 世界做判定，
/// 结果坐标乘回 `scale`。阈值只维护一套，且下采样本身还让定位更快。
pub const REFERENCE_HEIGHT: u32 = 720;

/// 等比缩放到 [`REFERENCE_HEIGHT`]，返回 `(缩放后帧, 原图/缩放 比例)`。
///
/// 幂等：帧高已 ≤ 720 时原样返回、`scale = 1.0`。比例恒 ≥ 1，调用方用
/// `x * scale` 把缩放世界坐标换回原图坐标。
pub fn normalize_frame(frame: &Frame) -> (Frame, f32) {
    let target_h = REFERENCE_HEIGHT;
    if frame.h <= target_h || frame.h == 0 {
        return (frame.clone(), 1.0);
    }
    let scale = frame.h as f32 / target_h as f32;
    let target_w = ((frame.w as f32) / scale).round().max(1.0) as u32;
    let Some(data) = downscale_bgra(frame, target_w, target_h) else {
        // 缓冲不自洽（不该发生）：宁可不动，也不要喂一张半成品给定位。
        return (frame.clone(), 1.0);
    };
    match Frame::new(target_w, target_h, data, frame.captured_at) {
        Ok(small) => (small, scale),
        Err(_) => (frame.clone(), 1.0),
    }
}

/// 按 `1/scale` 缩小一帧，其中 `scale = 全屏高 / 720`。
///
/// 给「只抓小区域」的流程用（大保险抓高亮带、任务保险抓滑槽）：区域帧的高度只有几十
/// 像素，直接 [`normalize_frame`] 会把它当成整屏**放大**到 720，判据全乱。这里按全屏
/// 比例缩，区域在 720p 世界里就是它在实机上该有的大小。
pub fn denormalize_region(frame: &Frame, scale: f32) -> Frame {
    if scale <= 1.0 {
        return frame.clone();
    }
    let target_w = ((frame.w as f32) / scale).round().max(1.0) as u32;
    let target_h = ((frame.h as f32) / scale).round().max(1.0) as u32;
    let Some(data) = downscale_bgra(frame, target_w, target_h) else {
        return frame.clone();
    };
    Frame::new(target_w, target_h, data, frame.captured_at).unwrap_or_else(|_| frame.clone())
}

/// BGRA 区域平均下采样（box filter）。
///
/// 不用最近邻：1.5 倍下采样时最近邻会整列丢弃，键块边缘与 4px 细白线会断裂，
/// 连通域数目虚高；box filter 对纯色块无损、对细线只做灰化，两者都更接近肉眼尺寸。
fn downscale_bgra(frame: &Frame, target_w: u32, target_h: u32) -> Option<Vec<u8>> {
    let (tw, th) = (target_w as usize, target_h as usize);
    let (sw, sh) = (frame.w as usize, frame.h as usize);
    let stride = frame.stride as usize;
    if tw == 0 || th == 0 || sw == 0 || sh == 0 {
        return None;
    }
    if frame.data.len() < stride * sh {
        return None;
    }
    let data = &frame.data;
    let mut out = Vec::with_capacity(tw * th * 4);
    for dy in 0..th {
        let y0 = dy * sh / th;
        let y1 = ((dy + 1) * sh / th).min(sh).max(y0 + 1);
        for dx in 0..tw {
            let x0 = dx * sw / tw;
            let x1 = ((dx + 1) * sw / tw).min(sw).max(x0 + 1);
            let mut sum = [0u32; 4];
            let mut count = 0u32;
            for y in y0..y1 {
                let base = y * stride;
                for x in x0..x1 {
                    let index = base + x * 4;
                    for channel in 0..4 {
                        sum[channel] += u32::from(data[index + channel]);
                    }
                    count += 1;
                }
            }
            let count = count.max(1);
            for channel in sum {
                out.push((channel / count) as u8);
            }
        }
    }
    Some(out)
}

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

/// 黄键台 4×4 键阵的边长。
pub const KEY_GRID_SIDE: usize = 4;
/// 4×4 组合里至少要命中的格数：允许 4 颗键正在发光/被 HUD 挡住。
const KEY_MIN_HITS: usize = 12;

/// 「暗键」的灰度上限（键面近黑）。
const KEY_DARK_LUMA: u8 = 95;
/// 单颗键的边长区间。720p 实机实测 55~57px。
const KEY_SIDE: (u32, u32) = (18, 70);
/// 单颗键的宽高比区间（近方形）。
const KEY_ASPECT: (f32, f32) = (0.6, 1.7);
/// 单颗键在自身外接框内的填充率下限。
///
/// 720p 实机实测 0.47~0.56 —— 键面被白色图标与下方小字挖掉约一半，
/// 按 0.55 会把 16 颗键只留下 1 颗，整台机器永远定位失败。
const KEY_MIN_FILL: f32 = 0.40;
/// 单颗键的最小面积。
const KEY_MIN_AREA: u32 = 200;
/// 4×4 键阵外接框的宽高比区间（键是方的、间距均匀 ⇒ 整阵近似正方形）。
const KEY_GRID_ASPECT: (f32, f32) = (0.72, 1.40);
/// 行/列聚类容差 = 该方向中位键边长 × 此系数。
const KEY_CLUSTER_TOL: f32 = 0.4;
/// 键块尺寸一致性区间（相对中位边长）：4×4 键阵的键大小一致。
const KEY_SIZE_SPREAD: (f32, f32) = (0.7, 1.4);
/// 命中判定容差 = 半格间距 × 此系数。
const KEY_HIT_TOL: f32 = 0.6;

/// 屏幕：灰度上限与「暗像素」占外接框的比例下限。
const KEY_SCREEN_LUMA: u8 = 80;
const KEY_SCREEN_DARK: f32 = 0.45;
/// 屏幕宽高比区间。
const KEY_SCREEN_ASPECT: (f32, f32) = (0.9, 2.2);
/// 屏幕搜索窗的横向范围（帧宽比例，另受键阵左缘约束）。
const KEY_SCREEN_SIDE: f32 = 0.55;
/// 屏幕与键阵的尺寸比区间：同一块面板上两者的尺寸同量级。
const KEY_SCREEN_SIZE: (f32, f32) = (0.5, 2.0);
/// 屏幕与键阵左缘的最大间距（占键阵宽度的比例）：屏幕就贴在键阵左边。
const KEY_SCREEN_GAP: f32 = 0.5;

/// 进度框（屏幕内浅色细边框）的亮度区间。
const KEY_BOX_LUMA: (u8, u8) = (150, 230);
/// 进度框的宽高比区间。
///
/// 实机实测（1080p 截屏）5 个输入格是**近方形**：70×77px ⇒ 0.91。
/// 旧下限 1.2 把它们全滤掉，于是 `progress` 恒为空、闭环退化成「16 格点完才算完」——
/// 那会多点一倍以上的键。下限放到 0.75 才容得下方格。
const KEY_BOX_ASPECT: (f32, f32) = (0.75, 3.0);
/// 进度框在屏幕内的宽/高占比区间。
const KEY_BOX_W: (f32, f32) = (0.06, 0.45);
const KEY_BOX_H: (f32, f32) = (0.05, 0.60);
/// 细边框在自身外接框内的占比区间（实心亮块不是边框）。
const KEY_BOX_FILL: (f32, f32) = (0.05, 0.70);
/// 进度框最多 5 个。
pub const KEY_PROGRESS_MAX: usize = 5;

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
    #[error("没找到右侧的 4×4 按键阵（能对上号的暗色键块不足 12 个）")]
    Keys,
}

/// 一帧定位：全部几何都由 `frame` 决定，不读配置、不碰 IO。
pub fn locate_line(frame: &Frame) -> Result<LocatedLine, LocateError> {
    if frame.w < MIN_FRAME_SIDE || frame.h < MIN_FRAME_SIDE {
        return Err(LocateError::Frame {
            w: frame.w,
            h: frame.h,
        });
    }
    // 统一在 720p 世界判定（阈值全是绝对像素），结果再乘回原图比例。
    let (small, scale) = normalize_frame(frame);
    let frame = &small;
    let knob = find_knob(frame).ok_or(LocateError::Knob)?;
    let screen = find_screen(frame, knob).ok_or(LocateError::Screen)?;
    let led = find_led(frame, knob, screen).ok_or(LocateError::Led)?;
    let grid = find_grid(frame, screen).ok_or(LocateError::Columns)?;
    Ok(LocatedLine {
        screen: scale_rect(screen, scale),
        led: scale_rect(led, scale),
        knob: scale_point(knob, scale),
        columns: grid.columns.iter().map(|x| scale_axis(*x, scale)).collect(),
        rows: grid.rows.iter().map(|y| scale_axis(*y, scale)).collect(),
        targets: grid
            .targets
            .iter()
            .map(|rect| scale_rect(*rect, scale))
            .collect(),
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
    clip_to(frame.w, frame.h, rect)
}

/// 同上，但画面尺寸由调用方给（灰度图与帧同尺寸，省掉一次 Frame 借用）。
fn clip_to(w: u32, h: u32, rect: PxRect) -> Option<(u32, u32, u32, u32)> {
    let x0 = rect.x.max(0).min(w as i32) as u32;
    let y0 = rect.y.max(0).min(h as i32) as u32;
    let x1 = rect.right().max(0).min(w as i32) as u32;
    let y1 = rect.bottom().max(0).min(h as i32) as u32;
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

fn within_u8(range: (u8, u8), value: u8) -> bool {
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

// ══════════════════════════════════════════════════════════════════════════
// 黄键台（ModuleKeys）：4×4 键阵 + 左侧屏幕 + 屏幕内 5 个进度框
// ══════════════════════════════════════════════════════════════════════════

/// 黄键台一帧量出来的全部几何。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedKeys {
    /// 4×4 键阵的外接框（= 16 个格的并集）。
    pub grid: PxRect,
    /// 16 颗键，**行优先**（左上 → 右下）；不足的格由中位键尺寸补出来。
    pub cells: Vec<PxRect>,
    /// 左侧深色屏幕。
    pub screen: PxRect,
    /// 屏幕内排成一行的进度框，自左向右；读不到就是空（调用方据此退化，不猜）。
    pub progress: Vec<PxRect>,
}

/// 一帧定位：键阵 → 屏幕 → 进度框，任一步失败即报错（调用方零输入退出）。
pub fn locate_keys(frame: &Frame) -> Result<LocatedKeys, LocateError> {
    if frame.w < MIN_FRAME_SIDE || frame.h < MIN_FRAME_SIDE {
        return Err(LocateError::Frame {
            w: frame.w,
            h: frame.h,
        });
    }
    // 统一在 720p 世界判定（`KEY_SIDE = 18..70` 是 720p 口径，1080p 的键 86px 会被滤光）。
    let (small, scale) = normalize_frame(frame);
    let frame = &small;
    // 只做一次灰度转换：三步判定全部复用同一份灰度图。
    let gray = frame.luma();
    let keys = find_key_grid(&gray).ok_or(LocateError::Keys)?;
    // 4×4 键阵近似正方形。环境里的墙面暗格阵多是长条形，这条把装饰物挡在门外，
    // 免得「误判成黄键台 → 拿暗格坐标去点背景」。
    let aspect = keys.grid.w as f32 / keys.grid.h.max(1) as f32;
    if !(KEY_GRID_ASPECT.0..=KEY_GRID_ASPECT.1).contains(&aspect) {
        return Err(LocateError::Keys);
    }
    let screen = find_key_screen(&gray, keys.grid).ok_or(LocateError::Screen)?;
    let progress = find_progress_boxes(&gray, screen);
    Ok(LocatedKeys {
        grid: scale_rect(keys.grid, scale),
        cells: keys
            .cells
            .iter()
            .map(|rect| scale_rect(*rect, scale))
            .collect(),
        screen: scale_rect(screen, scale),
        progress: progress
            .iter()
            .map(|rect| scale_rect(*rect, scale))
            .collect(),
    })
}

/// 缩放世界坐标 → 原图坐标（`scale == 1` 时原样返回，零开销）。
fn scale_axis(value: i32, scale: f32) -> i32 {
    if scale <= 1.0 {
        return value;
    }
    (value as f32 * scale).round() as i32
}

fn scale_rect(rect: PxRect, scale: f32) -> PxRect {
    if scale <= 1.0 {
        return rect;
    }
    PxRect {
        x: scale_axis(rect.x, scale),
        y: scale_axis(rect.y, scale),
        w: scale_axis(rect.w, scale).max(1),
        h: scale_axis(rect.h, scale).max(1),
    }
}

fn scale_point(point: PxPoint, scale: f32) -> PxPoint {
    if scale <= 1.0 {
        return point;
    }
    PxPoint {
        x: scale_axis(point.x, scale),
        y: scale_axis(point.y, scale),
    }
}

// ── 1. 暗键候选 ──

/// 键块候选：灰度低于 [`KEY_DARK_LUMA`]、近方形、够大、填充率达标的外轮廓。
fn dark_blocks(gray: &GrayImage) -> Vec<PxRect> {
    let whole = PxRect {
        x: 0,
        y: 0,
        w: gray.w as i32,
        h: gray.h as i32,
    };
    let Some(mask) = gray_mask(gray, whole, |luma| luma < KEY_DARK_LUMA) else {
        return Vec::new();
    };
    external_contours(&mask.binary)
        .into_iter()
        .filter(|block| (KEY_SIDE.0..=KEY_SIDE.1).contains(&block.region.w))
        .filter(|block| (KEY_SIDE.0..=KEY_SIDE.1).contains(&block.region.h))
        .filter(|block| within(KEY_ASPECT, block.aspect()))
        .filter(|block| block.area >= KEY_MIN_AREA)
        .filter(|block| fill_of(block) >= KEY_MIN_FILL)
        .map(|block| mask.to_frame(block.region, 1))
        .collect()
}

// ── 2. 聚成 4×4 ──

struct KeyGrid {
    grid: PxRect,
    cells: Vec<PxRect>,
}

/// 一个 4×4 判定结果：16 个格 + 命中数 + 间距均匀度。
struct Quad {
    cells: Vec<PxRect>,
    hits: usize,
    spread: f32,
}

impl Quad {
    /// 量一个 4×4 窗口：交点附近有键块就用它的**实测矩形**，没有就用中位键尺寸补。
    fn measure(
        blocks: &[PxRect],
        rows: [i32; KEY_GRID_SIDE],
        cols: [i32; KEY_GRID_SIDE],
        spread: f32,
    ) -> Self {
        let (fallback_w, fallback_h) = median_side(blocks);
        let tol_x = (cols[KEY_GRID_SIDE - 1] - cols[0]) as f32 / 6.0 * KEY_HIT_TOL;
        let tol_y = (rows[KEY_GRID_SIDE - 1] - rows[0]) as f32 / 6.0 * KEY_HIT_TOL;
        let mut cells = Vec::with_capacity(KEY_GRID_SIDE * KEY_GRID_SIDE);
        let mut hits = 0;
        for &row in &rows {
            for &col in &cols {
                match nearest_block(blocks, col, row, tol_x, tol_y) {
                    Some(block) => {
                        hits += 1;
                        cells.push(block);
                    }
                    None => cells.push(PxRect {
                        x: col - fallback_w / 2,
                        y: row - fallback_h / 2,
                        w: fallback_w,
                        h: fallback_h,
                    }),
                }
            }
        }
        Self { cells, hits, spread }
    }

    /// 命中更多的优先；同样多时取间距更均匀的。
    fn beats(&self, other: &Self) -> bool {
        self.hits > other.hits || (self.hits == other.hits && self.spread < other.spread)
    }
}

fn find_key_grid(gray: &GrayImage) -> Option<KeyGrid> {
    let blocks = uniform_blocks(&dark_blocks(gray));
    if blocks.len() < KEY_MIN_HITS {
        return None;
    }
    let quad = best_quad(&blocks)?;
    if quad.hits < KEY_MIN_HITS {
        return None;
    }
    Some(KeyGrid {
        grid: bounding(&quad.cells),
        cells: quad.cells,
    })
}

/// 只保留与中位尺寸一致的键块。
///
/// 4×4 键阵的 16 颗键大小一致；尺寸离群的暗块是**别的东西**：屏幕里空槽的内腔、
/// 背景上的暗斑。它们若混进行/列聚类，会把行距算歪（实测：5 个空槽内腔足以让整台机器
/// 定位失败）。
fn uniform_blocks(blocks: &[PxRect]) -> Vec<PxRect> {
    let (median_w, median_h) = median_side(blocks);
    blocks
        .iter()
        .copied()
        .filter(|block| {
            within(KEY_SIZE_SPREAD, block.w as f32 / median_w.max(1) as f32)
                && within(KEY_SIZE_SPREAD, block.h as f32 / median_h.max(1) as f32)
        })
        .collect()
}

/// 行/列各自聚类成 4 个中心，取「间距最均匀 + 命中最多」的那一组。
fn best_quad(blocks: &[PxRect]) -> Option<Quad> {
    let (median_w, median_h) = median_side(blocks);
    let tol_x = (median_w as f32 * KEY_CLUSTER_TOL) as i32;
    let tol_y = (median_h as f32 * KEY_CLUSTER_TOL) as i32;
    let cols = clusters(blocks.iter().map(center_x).collect(), tol_x);
    let rows = clusters(blocks.iter().map(center_y).collect(), tol_y);
    if cols.len() < KEY_GRID_SIDE || rows.len() < KEY_GRID_SIDE {
        return None;
    }

    let mut best: Option<Quad> = None;
    for row_window in rows.windows(KEY_GRID_SIDE) {
        for col_window in cols.windows(KEY_GRID_SIDE) {
            let spread = spacing_spread(row_window).max(spacing_spread(col_window));
            if spread > SPACING_SPREAD {
                continue;
            }
            let (Some(rows), Some(cols)) = (quad_of(row_window), quad_of(col_window)) else {
                continue;
            };
            let quad = Quad::measure(blocks, rows, cols, spread);
            if best.as_ref().is_none_or(|current| quad.beats(current)) {
                best = Some(quad);
            }
        }
    }
    best.filter(|quad| quad.hits >= KEY_MIN_HITS)
}

/// 一维中心值聚成若干簇，返回各簇的**中位数**（升序）。
///
/// 用中位数而不是均值：个别离群候选（背景上的暗斑）落在两行之间时，
/// 均值会把整行拽偏，中位数不动。
fn clusters(mut values: Vec<i32>, tol: i32) -> Vec<i32> {
    values.sort_unstable();
    let mut groups: Vec<Vec<i32>> = Vec::new();
    for value in values {
        let joins = groups
            .last()
            .and_then(|group| group.last())
            .is_some_and(|last| value - *last <= tol);
        match groups.last_mut() {
            Some(group) if joins => group.push(value),
            _ => groups.push(vec![value]),
        }
    }
    groups
        .iter()
        .filter_map(|group| group.get(group.len() / 2).copied())
        .collect()
}

/// 间距均匀度 `(max_gap - min_gap) / max_gap`；单值或退化间距按 0 处理。
fn spacing_spread(values: &[i32]) -> f32 {
    let gaps: Vec<f32> = values
        .windows(2)
        .map(|pair| (pair[1] - pair[0]) as f32)
        .collect();
    let max = gaps.iter().copied().fold(0.0f32, f32::max);
    if max <= 0.0 {
        return 0.0;
    }
    let min = gaps.iter().copied().fold(f32::INFINITY, f32::min);
    (max - min) / max
}

fn quad_of(window: &[i32]) -> Option<[i32; KEY_GRID_SIDE]> {
    match *window {
        [a, b, c, d] => Some([a, b, c, d]),
        _ => None,
    }
}

/// 离交点最近、且横纵偏差都在容差内的键块。
fn nearest_block(blocks: &[PxRect], x: i32, y: i32, tol_x: f32, tol_y: f32) -> Option<PxRect> {
    blocks
        .iter()
        .map(|block| {
            let dx = (center_x(block) - x).abs() as f32;
            let dy = (center_y(block) - y).abs() as f32;
            (block, dx, dy)
        })
        .filter(|(_, dx, dy)| *dx <= tol_x && *dy <= tol_y)
        .min_by(|left, right| (left.1 + left.2).total_cmp(&(right.1 + right.2)))
        .map(|(block, _, _)| *block)
}

fn median_side(blocks: &[PxRect]) -> (i32, i32) {
    let (mut widths, mut heights): (Vec<i32>, Vec<i32>) =
        blocks.iter().map(|block| (block.w, block.h)).unzip();
    widths.sort_unstable();
    heights.sort_unstable();
    let mid = blocks.len() / 2;
    (
        widths.get(mid).copied().unwrap_or(KEY_SIDE.0 as i32),
        heights.get(mid).copied().unwrap_or(KEY_SIDE.0 as i32),
    )
}

fn bounding(rects: &[PxRect]) -> PxRect {
    let left = rects.iter().map(|rect| rect.x).min().unwrap_or(0);
    let top = rects.iter().map(|rect| rect.y).min().unwrap_or(0);
    let right = rects.iter().map(|rect| rect.right()).max().unwrap_or(0);
    let bottom = rects.iter().map(|rect| rect.bottom()).max().unwrap_or(0);
    PxRect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    }
}

fn center_x(rect: &PxRect) -> i32 {
    rect.x + rect.w / 2
}

fn center_y(rect: &PxRect) -> i32 {
    rect.y + rect.h / 2
}

// ── 3. 屏幕 ──

/// 键阵左侧的深色大矩形（同一块面板上的操作屏）。
///
/// 三道硬门槛之外还有一条「相邻性」优先级：背景也是一大片暗色，
/// 但它不会紧贴键阵左缘；先挑贴得最近的那块，挑不到才退回最大面积。
fn find_key_screen(gray: &GrayImage, grid: PxRect) -> Option<PxRect> {
    let window = screen_window(gray, grid);
    let mask = gray_mask(gray, window, |luma| luma < KEY_SCREEN_LUMA)?;
    let candidates: Vec<(PxRect, u32)> = external_contours(&mask.binary)
        .into_iter()
        .filter(|block| within(KEY_SCREEN_ASPECT, block.aspect()))
        .filter(|block| dark_ratio(block) >= KEY_SCREEN_DARK)
        .map(|block| (mask.to_frame(block.region, 1), block.area))
        .filter(|(rect, _)| rect.right() <= grid.x)
        .filter(|(rect, _)| same_magnitude(*rect, grid) && overlaps_vertically(*rect, grid))
        .collect();

    let adjacent = candidates
        .iter()
        .copied()
        .filter(|(rect, _)| (grid.x - rect.right()) as f32 <= grid.w as f32 * KEY_SCREEN_GAP)
        .max_by_key(|(_, area)| *area);
    adjacent
        .or_else(|| candidates.iter().copied().max_by_key(|(_, area)| *area))
        .map(|(rect, _)| rect)
}

/// 屏幕搜索窗：帧左 0..[`KEY_SCREEN_SIDE`]、纵向比键阵上下各放宽 1/4。
fn screen_window(gray: &GrayImage, grid: PxRect) -> PxRect {
    let margin = grid.h / 4;
    let right = (gray.w as f32 * KEY_SCREEN_SIDE).min(grid.x as f32).max(0.0) as i32;
    PxRect {
        x: 0,
        y: grid.y - margin,
        w: right,
        h: grid.h + margin * 2,
    }
}

/// 暗像素占外接框的比例。
fn dark_ratio(block: &Contour) -> f32 {
    block.area as f32 / block.region.area().max(1) as f32
}

/// 屏幕与键阵尺寸同量级（同一台机器上两者不会差一个数量级）。
fn same_magnitude(rect: PxRect, grid: PxRect) -> bool {
    within(KEY_SCREEN_SIZE, rect.w as f32 / grid.w.max(1) as f32)
        && within(KEY_SCREEN_SIZE, rect.h as f32 / grid.h.max(1) as f32)
}

/// 与键阵的纵向重叠 ≥ 自身高度的一半。
fn overlaps_vertically(rect: PxRect, grid: PxRect) -> bool {
    let top = rect.y.max(grid.y);
    let bottom = rect.bottom().min(grid.bottom());
    (bottom - top).max(0) * 2 >= rect.h.max(1)
}

// ── 4. 进度框 ──

/// 屏幕内排成一行、最多 [`KEY_PROGRESS_MAX`] 个浅色细边框矩形。
///
/// 少于 3 个不算失败：进度判据会退化（见 `flow_az3::keys`），绝不猜进度。
fn find_progress_boxes(gray: &GrayImage, screen: PxRect) -> Vec<PxRect> {
    let Some(mask) = gray_mask(gray, screen, |luma| {
        within_u8(KEY_BOX_LUMA, luma)
    }) else {
        return Vec::new();
    };
    let boxes: Vec<PxRect> = external_contours(&mask.binary)
        .into_iter()
        .filter(|block| within(KEY_BOX_ASPECT, block.aspect()))
        .filter(|block| box_sized(block.region, mask.binary.width(), mask.binary.height()))
        .filter(|block| within(KEY_BOX_FILL, fill_of(block)))
        .map(|block| mask.to_frame(block.region, 1))
        .collect();
    largest_row(boxes)
}

/// 进度框相对屏幕的尺寸区间。
fn box_sized(region: Region, screen_w: u32, screen_h: u32) -> bool {
    let w = region.w as f32 / screen_w.max(1) as f32;
    let h = region.h as f32 / screen_h.max(1) as f32;
    within(KEY_BOX_W, w) && within(KEY_BOX_H, h)
}

/// 取成员最多的那一行（y 中心在同一带内），按 x 升序、截到 [`KEY_PROGRESS_MAX`]。
fn largest_row(rects: Vec<PxRect>) -> Vec<PxRect> {
    let mut rows: Vec<Vec<PxRect>> = Vec::new();
    for rect in sorted_by_center(rects) {
        let joins = rows
            .last()
            .is_some_and(|row| same_band(row, rect));
        match rows.last_mut() {
            Some(row) if joins => row.push(rect),
            _ => rows.push(vec![rect]),
        }
    }
    let mut best = rows.into_iter().max_by_key(Vec::len).unwrap_or_default();
    best.sort_by_key(|rect| rect.x);
    best.truncate(KEY_PROGRESS_MAX);
    best
}

fn sorted_by_center(mut rects: Vec<PxRect>) -> Vec<PxRect> {
    rects.sort_by_key(|rect| (rect.y + rect.h / 2, rect.x));
    rects
}

/// 与这一行的 y 中心差 ≤ 半格高（同一行的框高度一致）。
fn same_band(row: &[PxRect], rect: PxRect) -> bool {
    let anchor = row
        .first()
        .map_or(rect.y + rect.h / 2, |first| first.y + first.h / 2);
    (rect.y + rect.h / 2 - anchor).abs() <= rect.h.max(1) / 2
}

// ── 灰度原语（比逐像素 RGB 求亮度省两次乘法）──

/// 灰度阈值掩码。`gray` 是整帧灰度图，`rect` 是帧内窗口。
fn gray_mask(gray: &GrayImage, rect: PxRect, keep: impl Fn(u8) -> bool) -> Option<Masked> {
    let (x0, y0, x1, y1) = clip_to(gray.w, gray.h, rect)?;
    let mut binary = BinaryImage::new(x1 - x0, y1 - y0).ok()?;
    for y in y0..y1 {
        let row = gray.row(y);
        for x in x0..x1 {
            binary.set(x - x0, y - y0, keep(row[x as usize]));
        }
    }
    Some(Masked { binary, x0, y0 })
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

/// 黄键台的合成机台：100% 脱敏，只复刻「相对结构」（暗键阵 / 左侧屏幕 / 一行进度框）。
#[cfg(test)]
pub(crate) mod keys_fixture {
    use super::inset;
    use delta_capture::Frame;
    use delta_geom::PxRect;
    use std::time::Instant;

    pub(crate) const W: u32 = 640;
    pub(crate) const H: u32 = 360;
    /// 金属台面：亮灰（键块必须比它暗，暗键阵才数得出来）。
    pub(crate) const MACHINE: [u8; 3] = [152, 152, 150];
    /// 屏幕内部：近黑。
    pub(crate) const PANEL: [u8; 3] = [30, 32, 34];
    /// 未点亮的键面。
    pub(crate) const KEY: [u8; 3] = [36, 38, 40];
    /// 亮键的发光图标。
    pub(crate) const GLOW: [u8; 3] = [235, 200, 40];
    /// 已勾选的进度框内部（绿）。
    pub(crate) const CHECK: [u8; 3] = [40, 210, 60];
    /// 进度框的浅色细边框。
    pub(crate) const BORDER: [u8; 3] = [200, 200, 200];

    pub(crate) const SCREEN: PxRect = PxRect {
        x: 80,
        y: 100,
        w: 200,
        h: 180,
    };
    pub(crate) const KEY_SIZE: (i32, i32) = (44, 40);
    pub(crate) const COL_X: [i32; 4] = [322, 382, 442, 502];
    pub(crate) const ROW_Y: [i32; 4] = [92, 147, 202, 257];
    pub(crate) const BOX_SIZE: (i32, i32) = (30, 24);
    pub(crate) const BOX_X: [i32; 5] = [96, 134, 172, 210, 248];
    pub(crate) const BOX_Y: i32 = 190;

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

        /// 空心细边框（进度框就是这种）。
        fn ring(&mut self, rect: PxRect, thickness: i32, rgb: [u8; 3]) -> &mut Self {
            self.rect(PxRect { w: thickness, ..rect }, rgb);
            self.rect(
                PxRect {
                    x: rect.right() - thickness,
                    w: thickness,
                    ..rect
                },
                rgb,
            );
            self.rect(PxRect { h: thickness, ..rect }, rgb);
            self.rect(
                PxRect {
                    y: rect.bottom() - thickness,
                    h: thickness,
                    ..rect
                },
                rgb,
            )
        }

        pub(crate) fn frame(&self) -> Frame {
            Frame::new(W, H, self.data.clone(), Instant::now()).expect("合法合成帧")
        }
    }

    /// 第 `index` 颗键（行优先）的矩形。
    pub(crate) fn key_rect(index: usize) -> PxRect {
        PxRect {
            x: COL_X[index % 4],
            y: ROW_Y[index / 4],
            w: KEY_SIZE.0,
            h: KEY_SIZE.1,
        }
    }

    /// 第 `index` 颗键的几何中心（点击落点）。
    pub(crate) fn key_center(index: usize) -> (i32, i32) {
        let rect = key_rect(index);
        (rect.x + rect.w / 2, rect.y + rect.h / 2)
    }

    /// 第 `stage` 个进度框。
    pub(crate) fn box_rect(stage: usize) -> PxRect {
        PxRect {
            x: BOX_X[stage],
            y: BOX_Y,
            w: BOX_SIZE.0,
            h: BOX_SIZE.1,
        }
    }

    /// 一帧完整黄键台：金属台面 + 左侧屏幕 + 16 颗暗键（`lit` 里的发光）+ `checked` 个已勾选框。
    ///
    /// `lit` / `checked` 之外的键与框保持初始外观，因此同一份夹具能表达「只亮一格」
    /// 「同一格一直亮」「进度推进」等全部状态。
    pub(crate) fn panel(lit: &[usize], checked: usize) -> Canvas {
        let mut canvas = Canvas::new(MACHINE);
        canvas.rect(SCREEN, PANEL);
        for stage in 0..BOX_X.len() {
            let rect = box_rect(stage);
            if stage < checked {
                canvas.rect(inset(rect, 2), CHECK);
            }
            canvas.ring(rect, 2, BORDER);
        }
        for index in 0..16 {
            let rect = key_rect(index);
            canvas.rect(rect, KEY);
            if lit.contains(&index) {
                let icon = PxRect {
                    x: rect.x + rect.w / 4,
                    y: rect.y + rect.h / 4,
                    w: rect.w / 2,
                    h: rect.h / 2,
                };
                canvas.rect(icon, GLOW);
            }
        }
        canvas
    }

    /// 不带屏幕的黄键台（用来钉死「缺屏幕必须报错」）。
    pub(crate) fn panel_without_screen() -> Canvas {
        let mut canvas = Canvas::new(MACHINE);
        for index in 0..16 {
            canvas.rect(key_rect(index), KEY);
        }
        canvas
    }

    /// 只保留前 `count` 颗键（其余涂回台面），用来钉死「缺格子必须拒绝」。
    pub(crate) fn panel_with_keys(count: usize) -> Canvas {
        let mut canvas = Canvas::new(MACHINE);
        canvas.rect(SCREEN, PANEL);
        for index in 0..count.min(16) {
            canvas.rect(key_rect(index), KEY);
        }
        canvas
    }

    /// 没有进度框的屏幕（进度判据要退化）。
    pub(crate) fn panel_without_boxes() -> Canvas {
        let mut canvas = Canvas::new(MACHINE);
        canvas.rect(SCREEN, PANEL);
        for index in 0..16 {
            canvas.rect(key_rect(index), KEY);
        }
        canvas
    }
}

#[cfg(test)]
mod keys_tests {
    use super::keys_fixture::*;
    use super::*;

    #[test]
    fn locates_the_whole_module_keys_panel_in_one_frame() {
        let frame = panel(&[5], 0).frame();
        let keys = locate_keys(&frame).expect("合成黄键台必须一次定位成功");

        assert_eq!(keys.cells.len(), KEY_GRID_SIDE * KEY_GRID_SIDE, "16 颗键");
        assert_eq!(keys.screen, SCREEN, "屏幕必须量到同一块区域");
        assert_eq!(keys.progress.len(), KEY_PROGRESS_MAX, "5 个进度框");
        assert_eq!(
            keys.progress,
            (0..KEY_PROGRESS_MAX).map(box_rect).collect::<Vec<_>>(),
            "进度框必须自左向右排序"
        );
        assert_eq!(
            keys.grid,
            PxRect {
                x: COL_X[0],
                y: ROW_Y[0],
                w: COL_X[3] + KEY_SIZE.0 - COL_X[0],
                h: ROW_Y[3] + KEY_SIZE.1 - ROW_Y[0],
            },
            "外接框 = 16 颗键的并集"
        );
        for index in 0..16 {
            let cell = keys.cells[index];
            let (cx, cy) = key_center(index);
            assert_eq!(
                (cell.x + cell.w / 2, cell.y + cell.h / 2),
                (cx, cy),
                "第 {index} 颗键的中心（行优先）"
            );
        }
    }

    #[test]
    fn glowing_key_still_yields_a_cell_on_its_centre() {
        // 整颗键亮成黄块时它不再是「暗键」—— 这一格由中位键尺寸补齐，
        // 中心必须仍落在键的几何中心上（否则会点在键缝里）。
        let lit = locate_keys(&panel(&[6], 0).frame()).expect("亮键不影响定位");
        assert_eq!(lit.cells[6], key_rect(6));
        assert_eq!(lit.cells.len(), 16);
    }

    #[test]
    fn boxes_report_only_the_checked_count_through_their_pixels() {
        let frame = panel(&[], 3).frame();
        let keys = locate_keys(&frame).expect("勾选状态不影响几何");
        assert_eq!(keys.progress.len(), KEY_PROGRESS_MAX, "框本身一个不少");
    }

    #[test]
    fn panel_without_screen_reports_the_screen() {
        assert_eq!(
            locate_keys(&panel_without_screen().frame()),
            Err(LocateError::Screen)
        );
    }

    #[test]
    fn ten_keys_are_not_a_key_grid() {
        // 10 颗键对不上 4×4：宁可报错，也不许凑出一套假坐标乱点。
        assert_eq!(
            locate_keys(&panel_with_keys(10).frame()),
            Err(LocateError::Keys)
        );
    }

    #[test]
    fn thirteen_keys_still_form_a_full_grid() {
        // 13 颗键 = 3 颗正在发光/被遮挡：允许，缺的 3 格用中位键尺寸补出来。
        let keys = locate_keys(&panel_with_keys(13).frame()).expect("13 颗键仍能定出 4×4");
        assert_eq!(keys.cells.len(), 16);
        assert_eq!(keys.cells[0], key_rect(0), "实测格用实测矩形");
        assert_eq!(
            keys.cells[15].w, KEY_SIZE.0,
            "补出来的格用中位键尺寸：{:?}",
            keys.cells[15]
        );
    }

    #[test]
    fn missing_progress_boxes_degrade_to_an_empty_list() {
        // 读不到进度框不是定位失败：返回空，让执行层退化（绝不猜进度）。
        let keys = locate_keys(&panel_without_boxes().frame()).expect("几何仍然完整");
        assert!(keys.progress.is_empty(), "{:?}", keys.progress);
        assert_eq!(keys.cells.len(), 16);
    }

    #[test]
    fn blank_machine_body_has_no_key_grid() {
        assert_eq!(
            locate_keys(&Canvas::new(MACHINE).frame()),
            Err(LocateError::Keys)
        );
    }

    #[test]
    fn tiny_frame_is_rejected_before_scanning() {
        let frame =
            Frame::new(64, 64, vec![0u8; 64 * 64 * 4], std::time::Instant::now()).expect("合法帧");
        assert_eq!(locate_keys(&frame), Err(LocateError::Frame { w: 64, h: 64 }));
    }
}

