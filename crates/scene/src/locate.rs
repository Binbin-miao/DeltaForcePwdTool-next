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

/// 帧相对 720p 世界的缩放比（**唯一定义处**，[`normalize_frame`] 与 ready 闸共用）。
///
/// 帧高已 ≤720（含 0）时恒为 1.0：这类帧不做任何缩放，坐标就是 720p 世界坐标。
fn scale_of(frame: &Frame) -> f32 {
    if frame.h <= REFERENCE_HEIGHT {
        return 1.0;
    }
    frame.h as f32 / REFERENCE_HEIGHT as f32
}

/// 等比缩放到 [`REFERENCE_HEIGHT`]，返回 `(缩放后帧, 原图/缩放 比例)`。
///
/// 幂等：帧高已 ≤ 720 时原样返回、`scale = 1.0`。比例恒 ≥ 1，调用方用
/// `x * scale` 把缩放世界坐标换回原图坐标。
pub fn normalize_frame(frame: &Frame) -> (Frame, f32) {
    let target_h = REFERENCE_HEIGHT;
    let scale = scale_of(frame);
    if scale <= 1.0 {
        return (frame.clone(), 1.0);
    }
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

/// 相邻行中心的最小间距（720p 实测档位行距 50~110px，8 只是「别把两条带当一行」）。
const ROW_MIN_GAP: i32 = 8;

/// 行标签数字的亮度上限：深色数字实测均值 < 100，浅蓝绿底约 205。
const LABEL_DARK_LUMA: u8 = 100;
/// 行标签底色的亮度下限（182,220,215 一族均值约 205；深色背景约 30~60）。
const LABEL_BRIGHT_LUMA: u8 = 150;
/// 单个字形段的宽度区间（720p 世界）。
///
/// 实测：`1` 的笔画 7~8px、`0` 的环宽 13~16px；块边框/装饰线只有 4~5px，
/// 背景整片又远超上限 —— 下限 6 正好把「字」与「框」分开。
const LABEL_GLYPH_RUN: (usize, usize) = (6, 25);
/// 单段宽达到这个值就判 `0`（实测 `0` ≥13、`1` ≤8，两倍以上差距）。
const LABEL_ZERO_WIDTH: usize = 12;
/// 深色像素数达到这个值也判 `0`（宽判据的兜底：字形被压扁时宽度会失真）。
const LABEL_ZERO_PIXELS: u32 = 180;
/// 字形段必须占满窗口高度的比例（噪点/细线不满足）。
const LABEL_GLYPH_HEIGHT: f32 = 0.25;
/// 标签读数窗口的 x 范围（相对 `columns[0]`，单位 = 行距）。
///
/// 实机两个口径：左缘 `1.55~1.63` 个行距、右缘 `0.86~1.04` 个行距，取包住两者的区间。
const LABEL_WINDOW_X: (f32, f32) = (-1.75, -0.70);
/// 标签读数窗口的 y 半高（单位 = 行距）。
///
/// **必须 ≤ 0.25**：再大就会把标签块的上下边框收进窗口，列投影被那两条整宽横线
/// 连成一段，永远凑不出 2 段（实测 0.30 时四行全是「1 段」）。
const LABEL_WINDOW_Y: f32 = 0.25;
/// 字形左右各取多少像素作为「底色邻接」样本。
const LABEL_EDGE_SAMPLE: i32 = 3;
/// 邻接样本里浅色像素的最低占比：字形两侧必须是标签底色（框线两侧是深色背景）。
const LABEL_EDGE_PALE: f32 = 0.5;

/// 一行的标签读数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLabel {
    /// 标签值 `0..=3`（`10`=2、`11`=3、`01`=1、`00`=0）；`None` = 读不出。
    pub value: Option<u8>,
    /// 读数窗口（720p 世界坐标，供探针画框）；`None` = 连列都没定出来。
    pub window: Option<PxRect>,
}

/// 读左侧 4 行标签（`[行] -> 标签值`）。
///
/// 用户实测：右上角 LED 的 4 组数字与左侧 4 个行标签**每局都会变**，所以「第几行是哪个值」
/// 必须每帧从画面读出来，`line.rs` 再拿 LED 的目标值来这张表里查行。
///
/// 判据（`.scratch/line-probe/in/panel_clean.png` 等 5 张实测）：
/// 标签 = 浅蓝绿底 + 深色两位数字，只有 `0`/`1` 两种字形；`0` 的环宽 13~16px、
/// `1` 的笔画 7~8px，宽度（或深色像素数）单段判形。段数不是恰好 2 段就 `None`（绝不猜）。
///
/// `columns` / `rows` 是**720p 世界**的坐标（与本文件其余阈值同一口径）。
fn read_row_labels(frame: &Frame, columns: &[i32], rows: &[i32]) -> [RowLabel; LINE_ROWS] {
    let first = columns.first().copied();
    let gap = row_gap_of(rows);
    std::array::from_fn(|index| {
        let window = rows
            .get(index)
            .copied()
            .and_then(|y| label_window(first, y, gap));
        let value = window.and_then(|window| read_row_label(frame, window, gap));
        RowLabel { value, window }
    })
}

/// 第 `row_y` 行的标签读数窗口（720p 世界）；列没定出来就是 `None`。
fn label_window(first_column: Option<i32>, row_y: i32, gap: i32) -> Option<PxRect> {
    let first = first_column?;
    let gap = gap.max(1);
    let x0 = first + (gap as f32 * LABEL_WINDOW_X.0) as i32;
    let x1 = first + (gap as f32 * LABEL_WINDOW_X.1) as i32;
    let half = (gap as f32 * LABEL_WINDOW_Y) as i32;
    Some(PxRect {
        x: x0,
        y: row_y - half,
        w: (x1 - x0).max(1),
        h: (half * 2 + 1).max(1),
    })
}

/// 单行标签读数：窗内深色像素的列投影 → 恰好 2 段 → 两位数字 → 值。
fn read_row_label(frame: &Frame, window: PxRect, gap: i32) -> Option<u8> {
    let (x0, y0, x1, y1) = clip(frame, window)?;
    let width = (x1 - x0) as usize;
    let height = y1 - y0;
    if width == 0 || height == 0 {
        return None;
    }
    let mut profile = vec![0u32; width];
    for (index, count) in profile.iter_mut().enumerate() {
        let x = x0 + index as u32;
        *count = (y0..y1)
            .filter(|&y| luma_of(frame.pixel(x, y)) < LABEL_DARK_LUMA)
            .count() as u32;
    }
    let max_width = ((gap as f32) * 0.45) as usize;
    let glyphs: Vec<(usize, usize)> = runs(&profile, 0)
        .into_iter()
        .filter(|&(start, end)| within_usize(LABEL_GLYPH_RUN, end - start))
        .filter(|&(start, end)| end - start <= max_width.max(1))
        .filter(|&(start, end)| {
            profile[start..end].iter().copied().max().unwrap_or(0) as f32
                >= height as f32 * LABEL_GLYPH_HEIGHT
        })
        // 字形两侧必须是标签底色：块边框的右邻是深色背景，就在这里被挡掉。
        .filter(|&(start, end)| {
            let left = x0 as i32 + start as i32;
            let right = x0 as i32 + end as i32;
            pale_ratio(frame, left - LABEL_EDGE_SAMPLE, left, y0, y1) >= LABEL_EDGE_PALE
                && pale_ratio(frame, right, right + LABEL_EDGE_SAMPLE, y0, y1) >= LABEL_EDGE_PALE
        })
        .collect();
    if glyphs.len() != 2 {
        return None;
    }
    let tens = label_digit(&profile, glyphs[0]);
    let ones = label_digit(&profile, glyphs[1]);
    Some(tens * 2 + ones)
}

/// 单段字形是 `0` 还是 `1`：宽 ≥ [`LABEL_ZERO_WIDTH`] 或深色像素 ≥ [`LABEL_ZERO_PIXELS`] ⇒ `0`。
fn label_digit(profile: &[u32], glyph: (usize, usize)) -> u8 {
    let (start, end) = glyph;
    let pixels: u32 = profile
        .get(start..end)
        .map_or(0, |slice| slice.iter().sum());
    if end - start >= LABEL_ZERO_WIDTH || pixels >= LABEL_ZERO_PIXELS {
        0
    } else {
        1
    }
}

/// 窗口内一段竖条里「浅色像素」的占比（越界部分裁掉；裁空 → 0）。
fn pale_ratio(frame: &Frame, x_from: i32, x_to: i32, y0: u32, y1: u32) -> f32 {
    let left = x_from.max(0) as u32;
    let right = x_to.min(frame.w as i32).max(0) as u32;
    if right <= left {
        return 0.0;
    }
    let mut pale = 0u32;
    let mut total = 0u32;
    for x in left..right {
        for y in y0..y1 {
            total += 1;
            if luma_of(frame.pixel(x, y)) >= LABEL_BRIGHT_LUMA {
                pale += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        pale as f32 / total as f32
    }
}

/// BGRA 像素的亮度（三通道均值，与横线判据同一口径）。
fn luma_of([b, g, r]: [u8; 3]) -> u8 {
    ((u32::from(b) + u32::from(g) + u32::from(r)) / 3) as u8
}

/// 信号线的亮度下限：RGB 三通道均值 **> 170**。
///
/// 旧判据认中灰（`max 62..170`）是错的：四张拖动图实测横线是纯白 `(255,255,255)`。
/// 面板底色深蓝（约 40）够不到；档位条米黄均值约 190 会过线，靠下面的宽度判据排掉。
const WIRE_BRIGHT_MEAN: u32 = 170;
/// 横线在单列窗内至少要连续这么宽（720p 实测 110~170px）。
///
/// 档位条只有 10~12px 宽，无论多亮都凑不满这个长度 —— 这是「不与档位条混淆」的判据本体。
const WIRE_MIN_RUN: u32 = 40;
/// 横线的厚度区间（720p 实测 4~6px）。面板标题栏 8~16px 由此排掉。
const WIRE_THICKNESS: (i32, i32) = (2, 6);
/// 横线可搜范围 = 首行上方 / 末行下方各 `3/4` 个行距。
///
/// 实测三档取值：横线最远只停到末行下方 **0.51** 个行距（默认位置），面板标题栏在首行
/// 上方 **0.93**、面板底边框在末行下方 **0.90** —— 3/4 正落在两者之间，两头都不误收。
const WIRE_BAND_RATIO: (i32, i32) = (3, 4);

/// 黄键台 4×4 键阵的边长。
pub const KEY_GRID_SIDE: usize = 4;
/// 4×4 组合里至少要命中的格数：允许 5 颗键正在发光/被 HUD 挡住。
///
/// 实机 1080p 录像逐帧量过（230 个采样点）：面板完整可见的 6 秒里网格**只对上 11 颗**
/// （`n95 = 12~13`、`hit95 = 11`），12 会把这 6 秒整段判成失败。
/// 降到 11 的误判余量来自同一批采样点的反向统计：**非面板帧命中数最高 7**。
/// 不许再往下调：`t=32` 那帧（远景误报）命中数正好 10，降到 10 就会把它认成键阵。
const KEY_MIN_HITS: usize = 11;

/// 「暗键」的灰度上限（键面近黑）。
///
/// **取 120 而不是 95：游戏加加等滤镜会提亮暗部，把键面从 88~98 抬到 100~112。**
/// 用户现场：开滤镜后黄键台一颗键都认不出来，关掉就正常。滤镜的实测等效变换是
/// gamma≈0.85（`v' = 255·(v/255)^0.85`）。
///
/// 实测（真面板帧 `task_1794777686_ok.png` 归一化到 1280×720，16 颗键框内 `luma <= 阈值` 的占比）：
/// - 原图，阈值 95：填充率中位 **0.555**，满足 `fill >= 0.32` 的键 **16/16**
/// - 滤镜 gamma 0.85，阈值 95：**0.227**，**0/16** ← 用户报的「开滤镜就认不出」
/// - 滤镜 gamma 0.85，阈值 110：0.556，16/16
/// - 滤镜 gamma 0.80，阈值 120：0.688，16/16
/// - 滤镜 gamma 0.80，阈值 130：0.780，16/16
///
/// **上限也在这里**：同一张原图在阈值 95 / 110 / 120 下都恰好定出 **16** 颗键，
/// 阈值 130 时变成 **17**（面板上「亮一档的中灰块」也开始算暗键，开始增生）。
/// 所以 120 是「吃下滤镜」与「不增生」之间的取值，两侧都只有一档余量 ——
/// 改这个数必须先跑 `keys_panel_fixture` 的两条用例。
const KEY_DARK_LUMA: u8 = 120;
/// 单颗键的边长区间。720p 实机实测 55~57px。
const KEY_SIDE: (u32, u32) = (18, 70);
/// 单颗键的宽高比区间（近方形）。
const KEY_ASPECT: (f32, f32) = (0.6, 1.7);
/// 单颗键在自身外接框内的填充率下限。
///
/// 720p 近景实测 0.47~0.56 —— 键面被白色图标与下方小字挖掉约一半，
/// 按 0.55 会把 16 颗键只留下 1 颗，整台机器永远定位失败。
///
/// **远景/斜视会再低一档**：实测有 3~5 颗键恒为 **0.36 / 0.38 / 0.41**，在旧的 0.40 死线
/// 上下抖动 ⇒ 每帧漏 3~5 颗 ⇒ 撞 [`KEY_MIN_HITS`] ⇒ 整台判失败。取 0.32 留 0.04 余量。
const KEY_MIN_FILL: f32 = 0.32;
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

/// 「帧被下采样过」的判定线：`normalize_frame` 的 `scale > SCALE_LOOSE` 才算。
///
/// 720p 实机（`scale == 1.0`）与 1080p 实机（`scale == 1.5`）必须用**两套**阈值：
/// 盒式下采样把细边框与绿勾平均进背景，亮度差只剩原生的一半，用 720p 的严阈值
/// 会把整行读成「不可辨」；反过来把严阈值放宽到全局（105）又会让 720p 的背景
/// 也判成「可读」—— 那是「进度恒 1/5 超时」的根因。所以阈值只能是 `scale` 的函数。
const SCALE_LOOSE: f32 = 1.15;

/// 进度框（屏幕内浅色细边框）的亮度区间，**随缩放二选一**。
///
/// `(150, 230)` 是 720p 实测标定值，变更须重新标定；`(105, 235)` 只给下采样帧用。
/// 这是唯一允许出现这两个数字的地方 —— `find_progress_boxes` 与百分比行兜底
/// 必须共用同一套，绝不允许一处松一处严。
fn box_luma(scale: f32) -> (u8, u8) {
    if scale > SCALE_LOOSE {
        (105, 235)
    } else {
        (150, 230)
    }
}

/// 空槽「有边」的亮度差下限，**随缩放二选一**。
///
/// 720p 实测 12（槽内深灰 + 浅色细边，纯背景两者一样亮）；下采样后边缘被均值柔化，
/// 差值掉到 8 才读得到空槽。
fn ring_delta(scale: f32) -> i32 {
    if scale > SCALE_LOOSE {
        8
    } else {
        12
    }
}
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

/// 进度行的实测百分比矩形（spec §0②：整行 `x[28.28..48.20%], y[47.22..51.39%]`）。
///
/// 这组数字是**逐帧量出来的**，不是猜的：`annot` 帧上 5 个格子的并集。
/// 它只作**退化兜底** —— 自适应找框（[`find_progress_boxes`]）够 3 格时优先用自适应的。
const PROGRESS_ROW: (f32, f32, f32, f32) = (0.2828, 0.4722, 0.4820, 0.5139);
/// 单格宽度占整帧宽的比例（spec §0②：2.19%）。
const PROGRESS_SLOT_W: f32 = 0.0219;
/// 单格高度占整帧高的比例（spec §0②：2.78% ⇒ 720p 下 20px）。
///
/// 注意它**不等于** [`PROGRESS_ROW`] 的高：那是 5 格并集的整行 bbox 高
/// `51.39% - 47.22% = 4.17%`（30px @720）。把行高当格高会让每格多吞 10px 灰边。
const PROGRESS_SLOT_H: f32 = 0.0278;
/// 格距占整帧宽的比例（spec §0②：4.375% = 56px @1280）。
const PROGRESS_SLOT_PITCH: f32 = 0.04375;
/// 5 格进度行的**实测并集区域**（`x, y, w, h` 百分比）：**2026-11-16 1080p 实机实测**。
///
/// 与 `delta_config::builtin::AZ3_PROGRESS_REGION` **同值**。scene 不依赖 config
/// （依赖方向是 `config → geom`），这组实测数只能两处各存一份 —— 改一处必须同步改另一处。
///
/// 它在这里的职责是**上界**，不是锚点：格子仍按 [`PROGRESS_ROW`] 的锚点切，
/// 但凡有一角落到区域外的格子一律不产出。旧区域右边界只到 `0.417`（第 3 格），
/// 这正是「5 格只读出 3~4 格、却报 5/5」的根因。
const PROGRESS_REGION: (f32, f32, f32, f32) = (0.275, 0.453, 0.219, 0.075);
/// 进度框宽度相对中位宽的允许偏差：超出就不像同一排格子（覆盖到了别的东西）。
const PROGRESS_WIDTH_SPREAD: f32 = 0.25;
/// 同一行允许的纵向偏移（相对中位格高）。
const PROGRESS_ROW_SLACK: f32 = 0.3;
/// 一个格子至少要有这么多个绿 ✓ 像素才算「已勾选」。
const SLOT_CHECK_PIXELS: u32 = 6;
/// 少于这么多格「可辨」= 读不到进度（退化）。
const PROGRESS_MIN_READABLE: usize = 3;
/// 空槽判据：深灰空槽 `V < 90`（spec §0③）。
const SLOT_DARK_V: u8 = 90;

/// 绿 ✓ 判据：绿分量明显压过红蓝（`#3ddc4a` 一族）。
fn is_check_green([b, g, r]: [u8; 3]) -> bool {
    let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
    g >= 120 && g >= r + 30 && g >= b + 30
}

/// 定位结果：信号线台的全部几何都由这一帧量出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedLine {
    /// 青色 CRT 屏幕矩形（已剔除边框）。
    pub screen: PxRect,
    /// 4 列的 x 中心（自左向右）。
    pub columns: Vec<i32>,
    /// 4 行的 y 中心（自上而下，对应行标签 `10/11/01/00`）。
    pub rows: Vec<i32>,
    /// **每行左侧两位数字标签的值**（自上而下）：`10`=2、`11`=3、`01`=1、`00`=0。
    ///
    /// 用户实测：**右上角的四位数字与左侧四位数字每局都会变**，所以「第几行是哪个值」
    /// 不能写死成 `10/11/01/00`。`None` = 这一行的标签读不出（绝不猜）。
    pub row_labels: [Option<u8>; LINE_ROWS],
    /// 每行标签的读数窗口（原图坐标，供探针画框）。
    pub label_boxes: [Option<PxRect>; LINE_ROWS],
    /// 16 个候选档位方块的外接框，**列优先、行次之**：`targets[col * 4 + row]`。
    pub targets: Vec<PxRect>,
    /// 每列当前被琥珀框圈住的档位行；`None` = 这一列看不出位置（不猜）。
    pub selected: Vec<Option<usize>>,
    /// 每列**亮白信号线**的原始 y（`None` = 这一列没找到可信横线）。
    ///
    /// 只供调试可视化：`selected` 已经把它映射成档位行，执行层不需要看这个。
    pub wire_y: [Option<i32>; LINE_COLUMNS],
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
    /// **旧版合并变体**：`find_grid` 早先把「列没找到」与「行没找到」拍平成同一个
    /// `Columns`，`drag_row3` 因此卡在没有信息量的「数不到 4 列档位」上。
    /// 语义保持不变（留给下游既有匹配），但 [`locate_line`] 已不再产生它 —— 现在走
    /// [`LocateError::GridColumns`] / [`LocateError::GridRows`]。
    #[error("屏幕上数不到 4 列档位")]
    Columns,
    /// 列投影凑不出 4 个等距峰（候选条数见 [`diagnose_line_grid`]）。
    #[error("屏幕上数不到 4 列档位（列候选凑不出等距 4 列）")]
    GridColumns,
    /// 列已定、但列窗内的行投影凑不出 4 条等距带。
    #[error("屏幕上数不到 4 行档位（列已定位到，行候选凑不出等距 4 行）")]
    GridRows,
    #[error("没找到右侧的 4×4 按键阵（能对上号的暗色键块不足 12 个）")]
    Keys,
    /// 任务保险：整帧找不到那条连续长轨道。
    ///
    /// 与「轨道在、但还没有目标」**必须分开**：后者是「还没出游标」，继续等；
    /// 前者说明这不是任务保险界面（或镜头没对准），继续等只会空转到超时。
    #[error("没找到任务保险的水平长轨道")]
    Track,
}

/// 定位三态：**「还没长齐」与「真没有」必须分开**。
///
/// 这是本轮 Q1 的根因：面板 0.400s 才淡入、第一颗亮键 1.800s 才出现，
/// 而旧接口用 `Result<_, LocateError>` 把这两段时间里的「结构还没长齐」和
/// 「这个界面根本不存在」压成同一个 `Err` —— 于是按 F 的第一下必然判失败，
/// 用户只能按第二次。`NotYet` 存在的唯一理由就是**不消耗失败预算、继续等**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locate<T> {
    /// 结构齐全，可以动手。
    Found(T),
    /// 面板底色已经在画面里，但结构/亮键还没长齐 —— 继续等，**不计失败**。
    NotYet,
    /// 连面板底色都没有 —— 真的不在这个界面。
    Absent,
}

impl<T> Locate<T> {
    /// 是否已经就绪。
    pub fn is_found(&self) -> bool {
        matches!(self, Self::Found(_))
    }

    /// 就绪时取出定位结果（否则 `None`）。
    pub fn found(self) -> Option<T> {
        match self {
            Self::Found(value) => Some(value),
            Self::NotYet | Self::Absent => None,
        }
    }

    /// 诊断用：三态名字（日志里区分「还没好」与「不在界面」）。
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Found(_) => "Found",
            Self::NotYet => "NotYet",
            Self::Absent => "Absent",
        }
    }
}

/// 面板底色（深蓝）的 HSV 区间，spec §0④ H3'：`H 95..135 ∧ S 45..120 ∧ V 70..125`。
const PANEL_BASE_V: (i32, i32) = (70, 125);
/// 面板底色一定落在这块 ROI 里（spec §0④ 同一组实测百分比）。
const PANEL_ROI: (f32, f32, f32, f32) = (0.25, 0.15, 0.75, 0.60);
/// 底色占比下限：低于 3% 说明画面里根本没有面板（spec H3' 的反向判据）。
const PANEL_MIN_RATIO: f32 = 0.03;
/// 探底色的采样步长（这一步只回答「在不在」，不需要逐像素）。
const PANEL_SAMPLE_STEP: i32 = 4;

/// 画面里有没有「面板」这一层底 —— 用来把 [`Locate::NotYet`]（面板已在、结构未齐）与
/// [`Locate::Absent`]（真的不在这个界面）分开。不做 4×4 判定，所以淡入的中间态也能量到。
///
/// 两条证据**任一成立**即可：
/// 1. **深蓝底色**（[`is_panel_base`]）：信号线台 / 操作台面板的底色判据；
/// 2. **暗色键阵**（[`key_grid_present`]）：黄键台的面板是**白色机箱**，深蓝占比实测
///    **0.0**（阈值 0.03）⇒ 只认底色时 `NotYet` 分支永不进入，一失败就报 `Absent`，
///    连撞失败上限后退出 —— 用户看到的就是「按第一次 F 必然失败」。
pub fn panel_present(frame: &Frame) -> bool {
    if frame.w < MIN_FRAME_SIDE || frame.h < MIN_FRAME_SIDE {
        return false;
    }
    let (small, _) = normalize_frame(frame);
    base_present(&small) || key_grid_present(&small)
}

/// 深蓝面板底色在 [`PANEL_ROI`] 里的占比是否达标。
fn base_present(frame: &Frame) -> bool {
    let roi = percent_rect_of(frame.w, frame.h, PANEL_ROI);
    let mut hit = 0u32;
    let mut total = 0u32;
    let mut y = roi.y;
    while y < roi.bottom() {
        let mut x = roi.x;
        while x < roi.right() {
            total += 1;
            if is_panel_base(frame.pixel(x as u32, y as u32)) {
                hit += 1;
            }
            x += PANEL_SAMPLE_STEP;
        }
        y += PANEL_SAMPLE_STEP;
    }
    total > 0 && hit as f32 / total as f32 >= PANEL_MIN_RATIO
}

/// 黄键台键阵的「结构证据」门槛：画面里至少这么多颗暗色键面。
///
/// 真面板一帧有 16 颗键面（发光/遮挡时才少），而全片 230 个**非面板**采样点上
/// 4×4 命中数最高只有 **7** —— 8 是「比误报高、比残缺面板低」的最小值，
/// 因此它不会把走廊/背包认成键阵，又容得下「11 颗键对上了、5 颗被挡住」那种帧。
const PANEL_KEYS_MIN_BLOCKS: usize = 8;

/// 画面里有没有「一片大小一致的暗色键面」——只看键面候选，**不要求凑出 4×4**。
fn key_grid_present(frame: &Frame) -> bool {
    let gray = frame.luma();
    uniform_blocks(&dark_blocks(&gray)).len() >= PANEL_KEYS_MIN_BLOCKS
}

/// 深蓝面板底色（整数比较，等价于 OpenCV 标度 `H 95..135 ∧ S 45..120 ∧ V 70..125`）。
///
/// 展开成整数是因为这条判据每次轮询都要过一万多个采样点：蓝为最大分量 ⇒
/// `H = 215..270°`，等价于 `|r−g| ≤ delta/6`；`S = 255·delta/max`。
fn is_panel_base([b, g, r]: [u8; 3]) -> bool {
    let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
    let max = b.max(g).max(r);
    if b != max || !within_i32(PANEL_BASE_V, max) {
        return false;
    }
    let delta = max - r.min(g).min(b);
    // S ∈ [45,120] ⇒ 255·delta/max ⇒ delta ∈ [0.176·max, 0.471·max]
    delta * 100 >= max * 18 && delta * 100 <= max * 47 && (r - g).abs() * 6 <= delta
}

fn within_i32(range: (i32, i32), value: i32) -> bool {
    range.0 <= value && value <= range.1
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
    let grid = find_grid(frame, screen)?;
    // 行标签在这 720p 世界里读，窗口/框随结果一起乘回原图比例（与其他字段同一规则）。
    let labels = read_row_labels(frame, &grid.columns, &grid.rows);
    Ok(LocatedLine {
        screen: scale_rect(screen, scale),
        led: scale_rect(led, scale),
        knob: scale_point(knob, scale),
        columns: grid.columns.iter().map(|x| scale_axis(*x, scale)).collect(),
        rows: grid.rows.iter().map(|y| scale_axis(*y, scale)).collect(),
        row_labels: std::array::from_fn(|index| labels[index].value),
        label_boxes: std::array::from_fn(|index| {
            labels[index].window.map(|rect| scale_rect(rect, scale))
        }),
        targets: grid
            .targets
            .iter()
            .map(|rect| scale_rect(*rect, scale))
            .collect(),
        selected: grid.selected,
        wire_y: grid
            .wire_y
            .map(|y| y.map(|value| scale_axis(value, scale))),
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

/// 档位方块亮面：`V ≥ 150 ∧ S ≤ 90`（白 / 冷白青 / **米黄**）。
///
/// 上限从 60 放到 90 是被实机逼的：**当前受控的那一列档条是米黄色**（实测
/// R≈220 G≈200 B≈150 ⇒ delta 70），旧判据 `delta ≤ 60` 把它整列排除，于是屏幕里
/// 只剩 3 列白档条 → `most_even` 凑不出 4 列 → 报「屏幕上数不到 4 列档位」。
/// 屏幕背景是深蓝（max≈40）够不到 150，行标签是青色（delta≈100）超过 90，放开不误收。
fn is_slot_bright(color: [u8; 3]) -> bool {
    let (max, delta) = brightness(color);
    max >= 150 && delta <= 90
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
    wire_y: [Option<i32>; LINE_COLUMNS],
}

/// 列 / 行的投影候选与 `most_even` 的结果 —— **失败时唯一能说清「卡在哪一项」的东西**。
///
/// `drag_row3` 的教训：旧实现把「列没找到」和「行没找到」都塌成 `LocateError::Columns`，
/// 拿到的信息只有「数不到 4 列档位」，而真实原因可能是行投影凑不出等距 4 行。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GridDiagnosis {
    /// 列投影切出的全部候选中心（x）。
    pub column_candidates: Vec<i32>,
    /// 从候选里选出的 4 列；`None` = 凑不出。
    pub columns: Option<Vec<i32>>,
    /// 行投影切出的全部候选中心（y）；列没定出来时为空（行投影要用列窗）。
    pub row_candidates: Vec<i32>,
    /// 从候选里选出的 4 行；`None` = 凑不出。
    pub rows: Option<Vec<i32>>,
}

fn find_grid(frame: &Frame, screen: PxRect) -> Result<Grid, LocateError> {
    let field = masked(frame, screen, is_slot_bright).ok_or(LocateError::GridColumns)?;
    let diagnosis = grid_candidates(&field);
    let columns = diagnosis.columns.ok_or(LocateError::GridColumns)?;
    let rows = diagnosis.rows.ok_or(LocateError::GridRows)?;
    let wire_y = wire_lines(frame, screen, &columns, &rows);
    let selected = field_selected(frame, &columns, &rows, &wire_y);
    let targets = slot_rects(&columns, &rows);
    Ok(Grid {
        columns,
        rows,
        targets,
        selected,
        wire_y,
    })
}

/// 列 / 行投影的候选与选择结果（`find_grid` 与探针共用同一份判定）。
fn grid_candidates(field: &Masked) -> GridDiagnosis {
    let column_candidates = columns_of(field);
    let columns =
        most_even(&column_candidates, column_min_gap(field)).map(|picked| picked.to_vec());
    let row_candidates = match &columns {
        Some(columns) => rows_of(field, columns),
        None => Vec::new(),
    };
    let rows = most_even(&row_candidates, ROW_MIN_GAP).map(|picked| picked.to_vec());
    GridDiagnosis {
        column_candidates,
        columns,
        row_candidates,
        rows,
    }
}

/// 诊断入口：跑一遍「旋钮 → 屏幕 → 列/行投影」，把中间候选条数一并带出来。
///
/// 与 [`locate_line`] 的差别只有一处：**不因为凑不出 4 列 / 4 行而提前退出**，
/// 而是把候选原样返回，供探针说清「是列没找到，还是行没找到，各有多少条候选」。
pub fn diagnose_line_grid(frame: &Frame) -> Result<GridDiagnosis, LocateError> {
    if frame.w < MIN_FRAME_SIDE || frame.h < MIN_FRAME_SIDE {
        return Err(LocateError::Frame {
            w: frame.w,
            h: frame.h,
        });
    }
    let (small, _) = normalize_frame(frame);
    let frame = &small;
    let knob = find_knob(frame).ok_or(LocateError::Knob)?;
    let screen = find_screen(frame, knob).ok_or(LocateError::Screen)?;
    let field = masked(frame, screen, is_slot_bright).ok_or(LocateError::GridColumns)?;
    Ok(grid_candidates(&field))
}

/// 列投影切出的全部候选中心（只按亮度峰切段，不做等距筛选）。
fn columns_of(field: &Masked) -> Vec<i32> {
    let profile = field.column_counts();
    let tallest = profile.iter().copied().max().unwrap_or(0);
    if tallest == 0 {
        return Vec::new();
    }
    let threshold = ((tallest as f32) * 0.25).max(6.0) as u32;
    runs(&profile, threshold)
        .into_iter()
        .map(|(start, end)| field.x0 as i32 + (start + end) as i32 / 2)
        .collect()
}

/// 相邻列中心的最小间距（掩码宽度的 1/8）。
fn column_min_gap(field: &Masked) -> i32 {
    (field.binary.width() as i32) / 8
}

/// 行带只在 4 个列窗内投影：信号线曲线在每列的穿越高度不同，横向求和会被稀释，
/// 而档位方块在四列里高度一致 —— 于是 4 条等距带自然浮出来。
fn rows_of(field: &Masked, columns: &[i32]) -> Vec<i32> {
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
    runs(&profile, 5)
        .into_iter()
        .filter(|(start, end)| within_usize(ROW_RUN, end - start))
        .map(|(start, end)| field.y0 as i32 + (start + end) as i32 / 2)
        .collect()
}

/// 每列当前档位：优先读**琥珀定位框**，没有框就看**亮白信号线**在该列的穿越高度。
///
/// 旧实现只找琥珀框。实机 4 帧全部 `selected = [None; 4]` —— 因为游戏只在
/// **当前受控的那一列**画琥珀框，其余三列根本没有框，于是「每列现在是什么值」读不出来。
///
/// 而那条**亮白折线**逐列分段，第 N 段的 y 就是第 N 列的值 —— 能补上框的空白。
/// 两条通道的顺序不能反：受控列的琥珀框是「这一列此刻被选中」的直接证据，比线的高度更权威；
/// 而线在未对齐时可能落在两档之间（读成 `None`），正好由框兜住。
fn field_selected(
    frame: &Frame,
    columns: &[i32],
    rows: &[i32],
    wire_y: &[Option<i32>],
) -> Vec<Option<usize>> {
    let amber = amber_selected(frame, columns, rows);
    amber
        .into_iter()
        .zip(wire_y.iter().copied())
        .map(|(from_amber, from_wire)| {
            from_amber.or(from_wire.and_then(|y| nearest_row(rows, y)))
        })
        .collect()
}

/// 通道一：每列被**琥珀定位框**圈住的行（不足 `MARKER_MIN_PIXELS` → `None`）。
fn amber_selected(frame: &Frame, columns: &[i32], rows: &[i32]) -> Vec<Option<usize>> {
    let gap = row_gap_of(rows);
    let half_x = (gap / 5).max(6);
    let half_y = (gap / 3).max(8);
    columns
        .iter()
        .map(|&column| {
            let mut scored: Vec<(usize, u32)> = rows
                .iter()
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
                .collect();
            scored.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
            let (best_row, best) = *scored.first()?;
            let second = scored.get(1).map_or(0, |&(_, count)| count);
            // **必须单峰**（峰值 ≥ 次峰 ×3）才采信。
            // 受控那一列的整列档条是米黄（实测 R220/G200/B150），它同时满足琥珀判据
            // （delta=70 ≥ 0.275·R=60.5），于是 4 行计数几乎一样高 —— 取哪个都可能是错的。
            // 此时让位给信号线通道（它读的是线高度，不受颜色影响）。
            (best >= MARKER_MIN_PIXELS && best >= second.saturating_mul(3)).then_some(best_row)
        })
        .collect()
}

/// 通道二：每列**亮白信号线**的 y（列窗内水平亮段的中心）。
///
/// 折线是**逐列分段**的：第 N 段横在第 N 列窗内，y = 这一列的值。所以按列独立扫，
/// 不能整屏一条线读下来。
fn wire_lines(
    frame: &Frame,
    screen: PxRect,
    columns: &[i32],
    rows: &[i32],
) -> [Option<i32>; LINE_COLUMNS] {
    let mut wires = [None; LINE_COLUMNS];
    let (Some(half), Some(band)) = (column_half_pitch(columns), wire_band(rows)) else {
        return wires;
    };
    for (slot, &column) in wires.iter_mut().zip(columns) {
        *slot = wire_y_at(frame, screen, column, half, band);
    }
    wires
}

/// 列窗半宽 = 相邻列中心**最小**间距的一半（保证不伸进邻居的档位条）。
fn column_half_pitch(columns: &[i32]) -> Option<i32> {
    let pitch = columns.windows(2).map(|pair| pair[1] - pair[0]).min()?;
    (pitch > 1).then_some(pitch / 2)
}

/// 横线的可搜区间：首行上方、末行下方各 [`WIRE_BAND_RATIO`] 个行距。
///
/// `(上沿, 下沿)` 都是闭区间帧坐标。
fn wire_band(rows: &[i32]) -> Option<(i32, i32)> {
    let (first, last) = (*rows.first()?, *rows.last()?);
    let pad = row_gap_of(rows) * WIRE_BAND_RATIO.0 / WIRE_BAND_RATIO.1;
    Some((first - pad, last + pad))
}

/// 单列横线 y：列窗内取「厚度像横线、连续段最长」的那条带，返回带中心。
fn wire_y_at(
    frame: &Frame,
    screen: PxRect,
    column: i32,
    half: i32,
    band: (i32, i32),
) -> Option<i32> {
    let left = (column - half).max(screen.x);
    let right = (column + half).min(screen.right());
    let top = band.0.max(screen.y);
    let bottom = band.1.min(screen.bottom());
    if right <= left || bottom <= top {
        return None;
    }
    bright_bands(frame, left, right, top, bottom)
        .into_iter()
        .filter(|&(start, end, _)| is_wire_thickness(end - start + 1))
        .max_by_key(|&(_, _, run)| run)
        .map(|(start, end, _)| (start + end + 1) / 2)
}

/// `[left,right) × [top,bottom)` 里「单行最长连续亮段 ≥ [`WIRE_MIN_RUN`]」的纵向带。
///
/// 返回 `(上沿, 下沿, 带内最长连续亮段)`；上/下沿都是闭区间帧坐标。
fn bright_bands(frame: &Frame, left: i32, right: i32, top: i32, bottom: i32) -> Vec<(i32, i32, u32)> {
    let mut bands = Vec::new();
    let mut current: Option<(i32, i32, u32)> = None;
    for y in top..bottom {
        let run = longest_bright_run(frame, left, right, y);
        if run < WIRE_MIN_RUN {
            if let Some(band) = current.take() {
                bands.push(band);
            }
            continue;
        }
        current = Some(match current {
            Some((start, _, best)) => (start, y, best.max(run)),
            None => (y, y, run),
        });
    }
    bands.extend(current);
    bands
}

/// 该行在 `[left,right)` 内最长的连续亮段长度。
fn longest_bright_run(frame: &Frame, left: i32, right: i32, y: i32) -> u32 {
    let (mut best, mut run) = (0, 0);
    for x in left..right {
        if is_wire_bright(frame.pixel(x as u32, y as u32)) {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

/// 信号线的颜色：**亮白**（三通道均值 > [`WIRE_BRIGHT_MEAN`]）。
///
/// 旧判据认的是**中灰**（`max 62..170`），实机横线根本不是灰色 —— 四张拖动图实测
/// `(255,255,255)` 一族，整条被判据挡在门外，`selected` 于是读的是背景噪声。
fn is_wire_bright([b, g, r]: [u8; 3]) -> bool {
    let mean = (u32::from(b) + u32::from(g) + u32::from(r)) / 3;
    mean > WIRE_BRIGHT_MEAN
}

/// 厚度是否为横线：实测 4~6px；面板标题栏 8~16px 由此排掉。
fn is_wire_thickness(thickness: i32) -> bool {
    (WIRE_THICKNESS.0..=WIRE_THICKNESS.1).contains(&thickness)
}

/// 离 `y` 最近的档位行；超过行距的 **1/3** 就算「对不上任何一行」（不猜）。
///
/// 容差是 1/3 而不是 2/3：横线停在两档之间是「还没到位」的合法状态。
/// 旧容差 2/3 会把「差 32px、最近档位 487」的静止位置误读成最末一行。
fn nearest_row(rows: &[i32], y: i32) -> Option<usize> {
    let tolerance = (row_gap_of(rows) / 3).max(4);
    rows.iter()
        .enumerate()
        .map(|(index, &row)| (index, (row - y).abs()))
        .min_by_key(|&(_, distance)| distance)
        .filter(|&(_, distance)| distance <= tolerance)
        .map(|(index, _)| index)
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

    /// 该轮廓（换算回帧坐标后）里是否真有绿 ✓。
    ///
    /// 判据**只许复用** [`green_pixels`]/[`is_check_green`] 那一套，下限也是
    /// [`SLOT_CHECK_PIXELS`] —— 进度格的「已勾选」定义只此一份，绝不许这里另写一套阈值。
    fn has_check_green(&self, frame: &Frame, block: &Contour) -> bool {
        let rect = self.to_frame(block.region, 1);
        clip_to(frame.w, frame.h, rect)
            .is_some_and(|bounds| green_pixels(frame, bounds) >= SLOT_CHECK_PIXELS)
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

/// 从候选值里挑 4 个间距最均匀的。
///
/// **必须枚举组合，不能只看连续窗口**：实测 `drag_row3` 里亮线横段把第 2 根列切成
/// 417/426/435 三片，真正的 4 列（285/426/577/728）在候选表里因此不连续 ——
/// 只看 `windows(4)` 就只能全拒，报成 `GridColumns`，用户看到的是「横线台没反应」。
///
/// 这是「4 列 / 4 行」的判据本体：不等距就说明数歪了（把标签或边框算成了列），
/// 此时返回 `None`，调用方报错而不是猜。
fn most_even(values: &[i32], min_gap: i32) -> Option<[i32; 4]> {
    if values.len() < 4 {
        return None;
    }
    let mut best: Option<(f32, [i32; 4])> = None;
    let count = values.len();
    // 四层枚举 + 逐层 min_gap 剪枝：候选表实测 10~20 个，剪枝后组合很少。
    for a in 0..count - 3 {
        for b in (a + 1)..count - 2 {
            if values[b] - values[a] < min_gap {
                continue;
            }
            for c in (b + 1)..count - 1 {
                if values[c] - values[b] < min_gap {
                    continue;
                }
                for d in (c + 1)..count {
                    if values[d] - values[c] < min_gap {
                        continue;
                    }
                    let gaps = [
                        values[b] - values[a],
                        values[c] - values[b],
                        values[d] - values[c],
                    ];
                    let high = *gaps.iter().max().unwrap_or(&0);
                    if high <= 0 {
                        continue;
                    }
                    let low = *gaps.iter().min().unwrap_or(&0);
                    let spread = (high - low) as f32 / high as f32;
                    if spread > SPACING_SPREAD {
                        continue;
                    }
                    if best.is_none_or(|(current, _)| spread < current) {
                        best = Some((
                            spread,
                            [values[a], values[b], values[c], values[d]],
                        ));
                    }
                }
            }
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
    // 进度格里有「绿 ✓」判据 ⇒ 彩色帧必须一起带下去（灰度图上判绿恒假）。
    // `scale` 也一起带下去：它是**唯一**来源（`normalize_frame` 刚刚算出来的），
    // 下游的亮度阈值全靠它二选一，任何函数都不许自己再归一化一次。
    let progress = readable_progress(frame, &gray, screen, scale);
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

/// 黄键台三态定位：`locate_keys` 成功 = [`Locate::Found`]；失败但**面板已在** =
/// [`Locate::NotYet`]（继续等，不计失败）；连面板都没有 = [`Locate::Absent`]。
///
/// 「面板已在」由 [`panel_present`] 判：深蓝底色**或**暗色键阵任一成立。黄键台的面板是
/// 白色机箱、深蓝占比 0.0，只认底色会让这个分支对黄键台永远不成立。
pub fn locate_keys_state(frame: &Frame) -> Locate<LocatedKeys> {
    match locate_keys(frame) {
        Ok(keys) => Locate::Found(keys),
        Err(LocateError::Frame { .. }) => Locate::Absent,
        Err(_) if panel_present(frame) => Locate::NotYet,
        Err(_) => Locate::Absent,
    }
}

/// ready 判据里的亮键像素下限（spec §0①：噪声基线 950、首颗亮键 3767，取 3000 作中线）。
///
/// **仅 720p 口径的参考值**：就绪闸已改成只看面板结构（[`keys_panel_ready`]），
/// 亮键像素只作为 flow 内部的点击判据（[`lit_key_pixels`]）、以及 [`keys_ready`] 这条
/// 组合判据的组成部分，**不再决定「能不能开跑」**。
pub const READY_LIT_MIN_PX: u32 = 3000;
/// 亮键像素判据：够亮 + 饱和 + 蓝分量最小（黄绿高光）。暗键面近黑，不会命中。
fn is_lit_key([b, g, r]: [u8; 3]) -> bool {
    let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
    let max = b.max(g).max(r);
    if max < 150 || g < 110 {
        return false;
    }
    let delta = max - r.min(g).min(b);
    delta * 100 >= max * 45 && b <= r.min(g)
}

/// 16 颗键里「亮键」像素的总数。
///
/// `frame` 与 `cells` 必须**同一坐标口径**：720p 帧配 720p 口径的 [`READY_LIT_MIN_PX`]，
/// 原图帧配 [`ready_lit_min_px`]（面积随 `scale²` 放大，见 [`keys_ready`]）。
pub fn lit_key_pixels(frame: &Frame, cells: &[PxRect]) -> u32 {
    let mut total = 0u32;
    for cell in cells {
        for y in cell.y.max(0)..cell.bottom().min(frame.h as i32) {
            for x in cell.x.max(0)..cell.right().min(frame.w as i32) {
                if is_lit_key(frame.pixel(x as u32, y as u32)) {
                    total += 1;
                }
            }
        }
    }
    total
}

/// ready 闸的亮键像素下限：按 `scale²` 放大（[`READY_LIT_MIN_PX`] 是 **720p 口径**）。
///
/// 面积是二维量：1080p 下同一块区域像素多 2.25 倍，阈值不跟着放大就等于白送 2.25 倍余量。
fn ready_lit_min_px(scale: f32) -> u32 {
    if scale <= 1.0 {
        return READY_LIT_MIN_PX;
    }
    (READY_LIT_MIN_PX as f32 * scale * scale).round() as u32
}

/// 黄键台“开跑就绪”：**面板结构量得出来**就算就绪，亮键像素不参与。
///
/// 就绪闸（`worker::wait_keys_ready`）只认这一条。用它而不是 [`keys_ready`] 的原因：
/// 面板刚打开是 **1% 进度、一颗键都没亮**，把「亮键像素 ≥ 阈值」并进就绪闸等于要求
/// 「先亮起来才算打开」—— 于是整段 2100ms 都 `NotYet`、必然超时，日志里报成
/// 「就绪等待失败 / 面板已消失」，而同一帧的模板分是 0.95。
///
/// 亮键仍然是 flow 内部的判据（点哪颗、是否吃下），只是**不再决定能不能开跑**。
pub fn keys_panel_ready(frame: &Frame) -> Locate<()> {
    match locate_keys_state(frame) {
        Locate::Found(_) => Locate::Found(()),
        // 面板底色已在、键阵/进度格还没量全：这是「还没画完」，继续等，不判失败。
        Locate::NotYet => Locate::NotYet,
        Locate::Absent => Locate::Absent,
    }
}

/// 黄键台 ready：**面板可定位 且 亮键像素 ≥ [`ready_lit_min_px`]**，两条同时成立才算。
///
/// 只判「可定位」会把 0.4–1.8s 那段「面板已出现、一颗键都没亮」的空窗当成可以动手，
/// 于是点了个寂寞；只判亮键则会被别的界面上的黄绿光骗到。
///
/// **就绪闸已改用 [`keys_panel_ready`]**（亮键判据会让「1% 进度、零亮键」的真面板永远
/// 等不到就绪）。本函数保留为组合判据：需要「面板已在 **且** 已经亮起来」的场合
/// （例如无模板退化路径下的开跑确认）仍可用它。
///
/// **必须在原帧上判**：`locate_keys` 内部自己归一化一次并把坐标乘回原图，所以
/// `keys.cells` 是**原图坐标**、亮键像素也必须是原图口径。旧实现先 `normalize_frame` 拿
/// `small` 再喂给 `locate_keys_state(&small)`，等于在 720p 世界**再归一化一次**
/// （`h <= 720 ⇒ scale` 塌成 `1.0`）—— 缩放感知的阈值在最需要它的这条闸上永远不生效。
pub fn keys_ready(frame: &Frame) -> Locate<()> {
    let scale = scale_of(frame);
    match locate_keys_state(frame) {
        Locate::Found(keys) if lit_key_pixels(frame, &keys.cells) >= ready_lit_min_px(scale) => {
            Locate::Found(())
        }
        // 面板已在、亮键还没长齐：这段必须继续等（spec B2），不许判失败。
        Locate::Found(_) => Locate::NotYet,
        Locate::NotYet => Locate::NotYet,
        Locate::Absent => Locate::Absent,
    }
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

/// 进度行的读法：**先用自适应找框，读不到才退回实测百分比行**。
///
/// 返回 `< 3` 个格子 = 读不到进度（调用方据此退化，不猜）。
/// 颜色判据（绿 ✓）必须用彩色 `frame`：灰度图里 `r == g == b`，判绿恒假。
/// `scale` 是产生 `frame` 的缩放比，只喂阈值，**不在这里重新算**。
///
/// 读到 ≥3 格还要过 [`consistent_progress_row`]：**宁可退化成「读不到」，也不许报假的 5/5**。
fn readable_progress(frame: &Frame, gray: &GrayImage, screen: PxRect, scale: f32) -> Vec<PxRect> {
    let adaptive = find_progress_boxes(frame, gray, screen, box_luma(scale));
    if adaptive.len() >= PROGRESS_MIN_READABLE {
        return consistent_progress_row(adaptive);
    }
    let spec = spec_progress_slots(frame, gray, scale);
    if spec.len() >= PROGRESS_MIN_READABLE {
        return consistent_progress_row(spec);
    }
    // 1~2 格：本来就是退化档，没有「一排格子」可比，原样交给调用方去数。
    adaptive
}

/// 一排进度框的一致性校验：**不像同一排格子就整排作废**（返回空 ⇒ 调用方走退化档）。
///
/// 存在理由（实机故障）：同一张截图上 5 格是 `✓ ✓ ✓ ✓ ⬜`（逐格绿像素 174/164/185/239/**0**），
/// 日志却报「5/5 已勾选」—— 返回的 5 个矩形里有一个框到了别处（右侧绿色按键图标 / 屏幕绿字）。
/// 数一笔假账的代价是「一局明明没做完就报完成」，比读不到进度严重得多。
///
/// 两条判据（`< 3` 格不校验：那本来就不是「一排」）：
/// 1. 每格宽度与中位宽的偏差 ≤ [`PROGRESS_WIDTH_SPREAD`]；
/// 2. `max(y) - min(y) <= 中位格高 × [`PROGRESS_ROW_SLACK`]`（同一行）。
fn consistent_progress_row(rects: Vec<PxRect>) -> Vec<PxRect> {
    let widths: Vec<i32> = rects.iter().map(|rect| rect.w).collect();
    let heights: Vec<i32> = rects.iter().map(|rect| rect.h).collect();
    let width_median = median_of(&widths).max(1) as f32;
    let height_median = median_of(&heights).max(1) as f32;
    let widths_agree = rects
        .iter()
        .all(|rect| (rect.w as f32 - width_median).abs() <= width_median * PROGRESS_WIDTH_SPREAD);
    let (top, bottom) = rects
        .iter()
        .fold((i32::MAX, i32::MIN), |(top, bottom), rect| {
            (top.min(rect.y), bottom.max(rect.y))
        });
    let one_row = (bottom - top) as f32 <= height_median * PROGRESS_ROW_SLACK;
    if widths_agree && one_row {
        return rects;
    }
    Vec::new()
}

/// 中位数（偶数个取偏上的那一个）：只用于「同一排」的容差基准，不需要插值。
fn median_of(values: &[i32]) -> i32 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

/// 按 spec §0② 的实测百分比切出 5 个进度格；**只有 ≥3 格可辨才算读到**。
///
/// 「可辨」= 里面有绿 ✓，或者是一圈浅色细边的深灰空槽。纯背景两块都不满足，
/// 因此整片同色区域不会被误当成进度行（那会凭空报出 `0/5` 的假进度）。
///
/// `scale <= SCALE_LOOSE`（720p / 未下采样）时**逐字节走原路径**：整行只读一次、
/// 不试任何偏移。下采样世界里整行可能整体错位 1~2px，才需要 [`SPEC_DY_SCAN`] 那三个候选。
fn spec_progress_slots(frame: &Frame, gray: &GrayImage, scale: f32) -> Vec<PxRect> {
    if scale <= SCALE_LOOSE {
        return row_at(frame, gray, 0, scale).slots;
    }
    best_row(
        SPEC_DY_SCAN
            .iter()
            .map(|&dy| row_at(frame, gray, dy, scale))
            .collect(),
    )
}

/// 尝试的纵向偏移：下采样后整行可能整体偏 1~2px。
///
/// 范围必须窄：松阈值下每个偏移都能读出 4~5 格，候选越多越容易靠「格数」选错一格。
const SPEC_DY_SCAN: [i32; 3] = [0, -2, 2];

/// 某个 `dy` 偏移读出来的进度行。
struct RowReading {
    dy: i32,
    slots: Vec<PxRect>,
    /// 各格「边框 − 槽内 − 阈值」的余量之和：真槽与背景的区分量。
    margin: i32,
}

/// 在第 `dy` 行上班读一次。
fn row_at(frame: &Frame, gray: &GrayImage, dy: i32, scale: f32) -> RowReading {
    let mut slots = Vec::new();
    let mut margin = 0;
    for index in 0..KEY_PROGRESS_MAX {
        let mut slot = spec_slot_rect(gray.w, gray.h, index);
        slot.y += dy;
        // 落在实测并集区域外的格子一律不产出：那是把别处的东西（绿图标/绿字）当成了进度格。
        if !inside_progress_region(slot, gray.w, gray.h) {
            continue;
        }
        if let Some(evidence) = slot_evidence(frame, gray, slot, scale) {
            slots.push(slot);
            margin += evidence;
        }
    }
    RowReading { dy, slots, margin }
}

/// 格子是否完整落在 [`PROGRESS_REGION`] 里（四条边都要在，不能只判左上角）。
fn inside_progress_region(slot: PxRect, w: u32, h: u32) -> bool {
    let (left, top, width, height) = PROGRESS_REGION;
    let (w, h) = (w as f32, h as f32);
    (slot.x as f32) >= w * left
        && (slot.right() as f32) <= w * (left + width)
        && (slot.y as f32) >= h * top
        && (slot.bottom() as f32) <= h * (top + height)
}

/// 从偏移候选里选出该用哪一行。
///
/// 规则：格数多的赢；**但「多出来的格」必须真的有对比度** —— 若最强的偏移总余量还不如
/// `dy = 0`，说明多出的那一格是把背景当成了槽（错位），此时宁可少读一格也不许选错位。
/// 同格数、同余量时取 `|dy|` 最小的（并列优先 `dy = 0`，顺序稳定）。
fn best_row(readings: Vec<RowReading>) -> Vec<PxRect> {
    let strength = |reading: &RowReading| (reading.slots.len(), reading.margin, -reading.dy.abs());
    let Some(chosen) = (0..readings.len()).max_by_key(|&index| strength(&readings[index])) else {
        return Vec::new();
    };
    let Some(anchor) = readings.iter().position(|reading| reading.dy == 0) else {
        return readings[chosen].slots.clone();
    };
    // 多读出一格、总对比度却更低 ⇒ 那一格是背景不是槽，退回 dy = 0 的读数。
    let misaligned = readings[chosen].dy != 0 && readings[chosen].margin < readings[anchor].margin;
    let winner = if misaligned { anchor } else { chosen };
    readings
        .into_iter()
        .nth(winner)
        .map(|reading| reading.slots)
        .unwrap_or_default()
}

/// 实测百分比行上的第 `index` 个进度格（720p 世界，1280 宽时单格 28×20px、格距 56px）。
pub(crate) fn spec_slot_rect(w: u32, h: u32, index: usize) -> PxRect {
    let row = percent_rect_of(w, h, PROGRESS_ROW);
    let slot_w = (w as f32 * PROGRESS_SLOT_W).round().max(2.0) as i32;
    let slot_h = (h as f32 * PROGRESS_SLOT_H).round().max(2.0) as i32;
    let pitch = (w as f32 * PROGRESS_SLOT_PITCH).round().max(1.0) as i32;
    PxRect {
        x: row.x + index as i32 * pitch,
        y: row.y,
        w: slot_w,
        h: slot_h,
    }
}

/// 格子「可辨」的证据量：`Some(余量)` = 可辨，`None` = 不可辨。
///
/// 余量 = 边框平均亮度 − 槽内平均亮度 − [`ring_delta`]，负值记 0；绿 ✓ 格没有「边框余量」
/// 这个概念，按刚好够可辨（= 阈值本身）记，这样它不会凭余量压过真正的空槽。
fn slot_evidence(frame: &Frame, gray: &GrayImage, slot: PxRect, scale: f32) -> Option<i32> {
    let (x0, y0, x1, y1) = clip_to(gray.w, gray.h, slot)?;
    let threshold = ring_delta(scale);
    if green_pixels(frame, (x0, y0, x1, y1)) >= SLOT_CHECK_PIXELS {
        return Some(threshold);
    }
    // 空格子：外圈（边）比内圈（槽）亮 ⇒ 这是一个有边框的空槽，而不是背景。
    let (inner, ring) = slot_ring_contrast(gray, (x0, y0, x1, y1));
    let margin = ring - inner - threshold;
    (margin >= 0 && inner < i32::from(SLOT_DARK_V)).then_some(margin)
}

/// 矩形内绿 ✓ 的像素数（**必须读 BGRA**：灰度图三通道相等，判绿恒假）。
fn green_pixels(frame: &Frame, bounds: (u32, u32, u32, u32)) -> u32 {
    let (x0, y0, x1, y1) = bounds;
    let stride = frame.stride.max(frame.w * 4) as usize;
    let mut green = 0u32;
    for y in y0..y1 {
        let row = y as usize * stride;
        for x in x0..x1 {
            let index = row + x as usize * 4;
            let Some(pixel) = frame.data.get(index..index + 3) else {
                continue;
            };
            if is_check_green([pixel[0], pixel[1], pixel[2]]) {
                green += 1;
            }
        }
    }
    green
}

/// 格子的「边框平均亮度 − 槽内平均亮度」。
fn slot_ring_contrast(gray: &GrayImage, bounds: (u32, u32, u32, u32)) -> (i32, i32) {
    let (x0, y0, x1, y1) = bounds;
    let (w, h) = ((x1 - x0).max(1) as i32, (y1 - y0).max(1) as i32);
    let band = (w.min(h) / 5).max(1);
    let (mut inner, mut inner_n) = (0i64, 0i64);
    let (mut ring, mut ring_n) = (0i64, 0i64);
    for y in y0..y1 {
        let row = gray.row(y);
        for x in x0..x1 {
            let (dx, dy) = ((x - x0) as i32, (y - y0) as i32);
            let edge = dx < band || dy < band || dx >= w - band || dy >= h - band;
            let luma = i64::from(row[x as usize]);
            if edge {
                ring += luma;
                ring_n += 1;
            } else {
                inner += luma;
                inner_n += 1;
            }
        }
    }
    (
        (inner / inner_n.max(1)) as i32,
        (ring / ring_n.max(1)) as i32,
    )
}

/// 百分比矩形 → 像素矩形（只用到宽高，不需要 `Frame`）。
fn percent_rect_of(w: u32, h: u32, pct: (f32, f32, f32, f32)) -> PxRect {
    let (left, top, right, bottom) = pct;
    let x = (w as f32 * left) as i32;
    let y = (h as f32 * top) as i32;
    PxRect {
        x,
        y,
        w: ((w as f32 * right) as i32 - x).max(1),
        h: ((h as f32 * bottom) as i32 - y).max(1),
    }
}

/// 屏幕内排成一行、最多 [`KEY_PROGRESS_MAX`] 个浅色细边框矩形。
///
/// 少于 3 个不算失败：进度判据会退化（见 `flow_az3::keys`），绝不猜进度。
/// `luma` 由 [`box_luma`] 按缩放给出 —— 阈值只有那一个来源，这里不许写死数字。
/// `frame`（彩色）只为判绿 ✓：灰度图三通道相等，判绿恒假（见 [`green_pixels`]）。
fn find_progress_boxes(
    frame: &Frame,
    gray: &GrayImage,
    screen: PxRect,
    luma: (u8, u8),
) -> Vec<PxRect> {
    let Some(mask) = gray_mask(gray, screen, |value| within_u8(luma, value)) else {
        return Vec::new();
    };
    let boxes: Vec<PxRect> = external_contours(&mask.binary)
        .into_iter()
        .filter(|block| within(KEY_BOX_ASPECT, block.aspect()))
        .filter(|block| box_sized(block.region, mask.binary.width(), mask.binary.height()))
        // 有绿 ✓ 的格一律放行：下采样后的松散亮度带（105..235）会把「实心绿勾」的
        // 内腔（luma ≈145）也吃进掩码，整格连成一个实心域，填充率上限就把它判死。
        // 但这一格**确实有勾**，不能因为形状被判死。
        // 只有填充率上限被免掉：宽高比与尺寸两条在上一步已经过了，绿色墙面照样进不来。
        .filter(|block| mask.has_check_green(frame, block) || within(KEY_BOX_FILL, fill_of(block)))
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

// ══════════════════════════════════════════════════════════════════════════
// 任务保险（SafePins）：**一条连续长轨道** + 若干「白色目标方块」与「白色细竖线游标」
// ══════════════════════════════════════════════════════════════════════════
//
// 旧实现把界面当成「5 个固定滑槽」：拿配置里的 `track_band` 裁一条带，再按 5 个写死的
// `pin_center_x` 开 5 个搜索窗找白块。实机根本不是那样 —— 轨道是**一条连续长轨道**，
// 上面分散若干「白色细竖线（游标）」与「白色方块/短横条（目标）」，对数随进度变化（实测 2~5 对），
// 位置也不是任何固定百分比。于是固定槽中心必然错位：槽窗开在轨道空白处 ⇒ 一个白块都找不到。
//
// 现在几何全部每帧从画面量（[`locate_pins`]），与判别/执行共用同一份证据。

/// 轨道带的高度区间（720p 实测 17px，y187..203；面板上边框只有 1~3px，由此排掉）。
const PIN_TRACK_H: (i32, i32) = (8, 40);
/// 轨道至少要占帧宽的这么多：量到的只是某块面板的一段时不认。
const PIN_TRACK_MIN_W: f32 = 0.25;
/// 轨道**最多**占帧宽的这么多：真轨道被面板边框夹着，量出来是帧宽的 30%~75%。
///
/// 实测（720p 判定世界，帧宽 1280）：
/// - 真面板 `task_1794772921`：**640px = 50%**（x346..985）；
/// - 误判② `task_1794781313`（亮室外）：y638..649 那条 1034px 的亮段 **= 81%**，
///   它左右也各有暗背景（段内均值 92 vs 左 32 / 右 43），只有「宽度」这一条能把它和真轨道分开。
///
/// 这条上限是 2026-11-16 把「整行跨度」换成「被暗背景夹住的亮段」之后仍然必需的判据：
/// 换成段判据只解决了「整帧都算轨道行」，没解决「一条亮段铺满大半幅画面」。
const PIN_TRACK_MAX_W: f32 = 0.75;
/// 轨道底色的灰度区间（720p 实测 67；纯黑背景 0、亮面板边框 >140 都落在外面）。
const PIN_TRACK_LUMA: (u32, u32) = (55, 110);
/// 「非背景」像素的灰度下限：轨道底色 67 在内，面板深蓝底（约 40 以下）在外。
const PIN_TRACK_FLOOR: u8 = 40;
/// 段与段之间的暗缝至少要这么宽才算「断开」（帧宽比例，720p 实测 ≈26px）。
///
/// 比它窄的暗像素算**段内空洞**：轨道上白色目标块/游标把底色挖掉的那一两个像素、
/// 抗锯齿边都长这样，按像素断开会把一条 640px 的轨道切成十几段，段宽全不达标。
const PIN_TRACK_GAP_MIN: f32 = 0.02;
/// 判「左右被暗背景夹住」时，段的两侧各取这么宽的邻域（帧宽比例，720p 实测 38px）。
const PIN_TRACK_CONTEXT: f32 = 0.03;
/// 两侧邻域的均值必须 **< 段均值 × 此值** 才算「实质暗背景」。
///
/// 实测（真面板轨道 y187..203 x346..985，段均值 70）：左 27（比值 0.39）、右 28（0.40）
/// —— 真轨道留了近一倍余量，取 0.75 不动它。
///
/// **这条判据在现有三张真夹具上都不是决定性的一条**（`yellow_railing` /
/// `bright_outdoor` 是被宽度上限与亮像素占比拒掉的），它拦的是「同一条亮带向外连成亮区」：
/// 亮段紧邻着亮墙/灯箱、或干脆顶到画面边缘时，两侧邻域没有暗背景 —— 那不是被面板夹住的轨道。
/// 它有独立的合成帧反例守卫（`pins_tests::a_band_whose_right_neighbour_is_bright_is_not_a_track`），
/// 谁想删掉这条判据，那条用例会红。
const PIN_TRACK_CONTRAST: f32 = 0.75;
/// 「同一段位置」的判据：相邻两行的段 x 中心漂移 ≤ 帧宽的这个比例。
const PIN_TRACK_DRIFT: f32 = 0.02;
/// 「亮线」像素的灰度下限：真轨道的白色目标块/游标/亮头都在里面（实测明度中位 193）。
const PIN_TRACK_BRIGHT_LUMA: u8 = 170;
/// 候选带内亮像素占比下限 —— 轨道必须是**亮线**，不是一条中灰横带。
///
/// 实测（1280×720 归一化后，带内 `luma > 170` 的像素占比）：
/// - 真面板轨道带（`x 346..985, y 187..203`）：**9.5%**
/// - 误判：室内黄色金属栏杆横杆：**0.1%**（暗黄，明度中位 75，落在 [`PIN_TRACK_LUMA`] 里）
/// - 误判：另一帧逐行取最大：**3.0%**
///
/// 取 5%：真轨道留 1.9 倍余量，两种误判都拒。只看 [`PIN_TRACK_LUMA`] 的整带行均值
/// 挡不住黄色栏杆 —— 它是「同一条中灰横带」的形态复制品，只能靠**亮度**分辨。
const PIN_TRACK_BRIGHT_MIN: f32 = 0.05;
/// 轨道两端必须离画面边缘有空白：真面板的轨道被面板边框夹住，
/// 而「整幅画面都很亮」的行段（天空+厂房+栏杆连成一片）会从 x=0 铺到 x=帧宽 ——
/// 那不是轨道，是一片没有边界的亮区。这一条是 2026-11-16 两次实机误判的决定性差异。
///
/// 实测（720p 判定世界）：
/// - 真面板 `task_1794772921`：`x=346, w=640`（帧宽 1280），两端都留出 27% 帧宽；
/// - 误判① `task_1794781125`（黄栏杆）：`x=0, w=1280`，两端同时贴边；
/// - 误判② `task_1794781313`（亮室外）：`x=0, w=1280`，两端同时贴边。
///
/// 只拦「贴边」，**不是**「必须居中」：轨道在不同机位会左偏/右偏，唯一稳定的共性是
/// 「两端都有面板边框压着」，所以两侧各留 2% 帧宽即可。
const PIN_TRACK_EDGE_MARGIN: f32 = 0.02;
/// 轨道内白色元素（目标方块 / 游标）的灰度下限（实测纯白一族）。
const PIN_WHITE_LUMA: u8 = 140;
/// 目标方块：宽度区间（实测 10~40；轨道整条亮白头、面板亮边框都远超 40，由此排掉）。
///
/// 高度不设上限：目标只可能在**轨道带内**量（见 [`pin_targets`]），带高本身 ≤ 40，
/// 再叠一道高度上限只会把「游标正压在目标上」那一帧（白块被连成一片）误杀。
const PIN_TARGET_W: (u32, u32) = (10, 40);
/// 游标：宽度区间（实测 2~8）与高度下限（实测 20~30）。
const PIN_CURSOR_W: (u32, u32) = (1, 8);
const PIN_CURSOR_MIN_H: u32 = 18;
/// 纵向裁剪时轨道上下各放宽这么多：游标伸到带外的那一截必须一起量进来。
const PIN_VERTICAL_SLACK: i32 = 6;
/// 「伸出带外」的像素高度下限（≥3px 才算游标）。
///
/// 这是「游标 vs 目标方块」的**唯一分界线**，用实测数字定：目标方块（如 `(413,187,10,17)`）
/// 相对轨道带最多溢出 1~2px；游标（如 `(621,182,5,27)`）至少溢出 3px。
/// 按高度分类会在「游标正压在目标上」时失效 —— 两个白块连成一片，既不窄也不扁；
/// 按「谁伸出带外」分类则两种情形都成立（目标是带内的东西，游标天生比带高）。
const PIN_CURSOR_PROTRUDE: u32 = 3;
/// 白色块的连通面积下限（滤掉抗锯齿边与噪点）。
const PIN_MIN_BLOB: u32 = 8;

/// 任务保险一帧量出来的几何。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedPins {
    /// 连续长轨道（已剔除上下亮边框）。
    pub track: PxRect,
    /// 目标白块（短横条/方块），**按 x 升序**。
    pub targets: Vec<PxRect>,
    /// 游标细竖线，**按 x 升序**。
    pub cursors: Vec<PxRect>,
    /// `(目标下标, 游标下标, 两者中心的水平距离)`，按目标 x 升序。
    ///
    /// 配不上游标的目标**不进这里**，但仍留在 [`Self::targets`] 里供日志排查。
    pub pairs: Vec<(usize, usize, i32)>,
}

impl LocatedPins {
    /// 容差像素数：轨道宽的 0.6%（实测两像素级对齐，轨道 750px 时 ≈4.5px）。
    ///
    /// 容差按**轨道宽度**折算而不是写死像素：换分辨率、换机位时轨道会跟着缩放，
    /// 写死像素的容差会一边太松（点早了）一边太紧（永远等不到）。
    pub fn tolerance(&self) -> i32 {
        ((self.track.w as f32 * PIN_TOL_RATIO).round() as i32).max(1)
    }
}

/// 容差 = 轨道宽度 × 这个比例（0.6%）。
const PIN_TOL_RATIO: f32 = 0.006;

/// 一帧定位：轨道 → 带内白块二分类 → 目标/游标配对。全部几何由 `frame` 决定，不读配置。
///
/// 零值边界：**轨道在、但游标一个都还没出来**（刚开局配不上对）⇒ `Ok` 且 `pairs` 为空，
/// 调用方按「还没出游标」继续等；**轨道都找不到** ⇒ `Err(LocateError::Track)`，
/// 因为「没有轨道」= 这不是任务保险界面，继续等只会空转到超时。
///
/// 「轨道在」现在含一条亮度证据（[`PIN_TRACK_BRIGHT_MIN`]）：带内连一个亮像素都没有的
/// **全灰横带**不算轨道 —— 它是黄栏杆 / 地面警示条的形态复制品，而真轨道上永远挂着
/// 白色目标方块与游标。所以「板上什么白块都没有」会被判 `Err(Track)`，而不是「等着」。
pub fn locate_pins(frame: &Frame) -> Result<LocatedPins, LocateError> {
    if frame.w < MIN_FRAME_SIDE || frame.h < MIN_FRAME_SIDE {
        return Err(LocateError::Frame {
            w: frame.w,
            h: frame.h,
        });
    }
    // 与其他定位器同一口径：阈值全是 720p 绝对像素，判定在 720p 世界做，坐标乘回原图。
    let (small, scale) = normalize_frame(frame);
    let gray = small.luma();
    let track = find_pin_track(&gray).ok_or(LocateError::Track)?;
    let (targets, cursors) = pin_blobs(&gray, track);
    let pairs = pair_pins(&targets, &cursors);
    Ok(LocatedPins {
        track: scale_rect(track, scale),
        targets: targets.iter().map(|rect| scale_rect(*rect, scale)).collect(),
        cursors: cursors.iter().map(|rect| scale_rect(*rect, scale)).collect(),
        // 距离与矩形必须同一坐标口径，否则容差换算会差一个 `scale`。
        pairs: pairs
            .into_iter()
            .map(|(target, cursor, distance)| {
                (target, cursor, scale_axis(distance, scale))
            })
            .collect(),
    })
}

/// 任务保险三态：`Found` = 轨道 + 白块量到了；`NotYet` = 面板底色已在但轨道还没长出来；
/// `Absent` = 连面板都没有。判据与 [`locate_keys_state`] 完全同构（同一套 `panel_present`）。
pub fn locate_pins_state(frame: &Frame) -> Locate<LocatedPins> {
    match locate_pins(frame) {
        Ok(pins) => Locate::Found(pins),
        Err(LocateError::Frame { .. }) => Locate::Absent,
        Err(_) if panel_present(frame) => Locate::NotYet,
        Err(_) => Locate::Absent,
    }
}

/// 找轨道：逐行切出「被暗背景夹住的亮段」，同一位置的段连成纵向 run，取**最宽**的一条。
///
/// 为什么不能按「整行非背景像素数」判：真面板帧（`real_panel`）720 行里有 696 行都满足
/// 「整行 ≥ 25% 帧宽都是非背景」—— 整帧的金属柜与地面都 ≥ [`PIN_TRACK_FLOOR`]，
/// 于是真轨道（被面板边框夹住的 640px）被两侧环境稀释成一整帧跨度，从来不是独立候选。
/// 段判据把「段」和「段之外」分开，真轨道才第一次成为候选。
///
/// 每条段要同时满足四件事（缺一不可）：
/// 1. 段宽 ∈ [`PIN_TRACK_MIN_W`]..=[`PIN_TRACK_MAX_W`] 帧宽（面板上的一小段 / 一整帧亮区都不算）；
/// 2. 段内均值在 [`PIN_TRACK_LUMA`]（轨道是中灰，不是纯黑也不是亮白）；
/// 3. 段左右各 [`PIN_TRACK_CONTEXT`] 帧宽的邻域均值都 **< 段均值 × [`PIN_TRACK_CONTRAST`]**
///    —— 真轨道被面板的深色内壁夹在中间，环境亮区没有这个「夹心」结构；
/// 4. 段与段之间只要暗缝 ≥ [`PIN_TRACK_GAP_MIN`] 就断开（窄洞算段内空洞，见该常数）。
///
/// 连成的 run 还要过三条：高度 ∈ [`PIN_TRACK_H`]、两端不贴画面边缘
/// （[`PIN_TRACK_EDGE_MARGIN`]）、带内亮像素占比 ≥ [`PIN_TRACK_BRIGHT_MIN`]。
///
/// 取「最宽」而不是「最靠上」：底部指示条的绿条也是中灰、也能连成 8~40px 高的一段，
/// 但它窄（实测 5 格 + 间隙 < 轨道宽），轨道永远比它宽。
fn find_pin_track(gray: &GrayImage) -> Option<PxRect> {
    let gate = TrackGate::new(gray.w);
    let mut best: Option<PxRect> = None;
    let mut open: Vec<TrackRun> = Vec::new();
    for y in 0..gray.h {
        // 本行剩下来的段：没被任何 run 接走的，都在这一行新起一个 run。
        let mut segments = track_segments(gray, y, &gate);
        let mut still_open = Vec::new();
        for mut run in open {
            let Some(position) = take_near(&mut segments, run.center, gate.drift) else {
                if let Some(rect) = band_rect(gray, run.rect(), &gate) {
                    best = wider(best, rect);
                }
                continue;
            };
            run.grow(y, position);
            still_open.push(run);
        }
        still_open.extend(segments.into_iter().map(|position| TrackRun::new(y, position)));
        open = still_open;
    }
    for run in open {
        if let Some(rect) = band_rect(gray, run.rect(), &gate) {
            best = wider(best, rect);
        }
    }
    best
}

/// 逐行判据的几何门槛：按帧宽折算一次，避免每行重算。
struct TrackGate {
    /// 段宽下限 / 上限（像素）。
    min_w: u32,
    max_w: u32,
    /// 段间暗缝下限（像素）：比它窄的暗像素算段内空洞。
    gap: u32,
    /// 段左右各取多宽的邻域（像素）。
    context: u32,
    /// 「同一段位置」的 x 中心漂移容差（像素）。
    drift: u32,
}

impl TrackGate {
    fn new(frame_w: u32) -> Self {
        let scaled = |ratio: f32| (frame_w as f32 * ratio).round() as u32;
        Self {
            min_w: scaled(PIN_TRACK_MIN_W),
            max_w: scaled(PIN_TRACK_MAX_W),
            gap: scaled(PIN_TRACK_GAP_MIN).max(1),
            context: scaled(PIN_TRACK_CONTEXT).max(1),
            drift: scaled(PIN_TRACK_DRIFT).max(1),
        }
    }
}

/// 一个纵向 run：连续若干行里 x 中心几乎不动的那一段（并集即候选轨道带）。
#[derive(Debug, Clone, Copy)]
struct TrackRun {
    top: u32,
    bottom: u32,
    x0: u32,
    x1: u32,
    /// 最后一行的段 x 中心：下一行按它算漂移。
    center: u32,
}

impl TrackRun {
    fn new(y: u32, (x0, x1): (u32, u32)) -> Self {
        Self {
            top: y,
            bottom: y,
            x0,
            x1,
            center: (x0 + x1) / 2,
        }
    }

    fn grow(&mut self, y: u32, (x0, x1): (u32, u32)) {
        self.bottom = y;
        self.x0 = self.x0.min(x0);
        self.x1 = self.x1.max(x1);
        self.center = (x0 + x1) / 2;
    }

    fn rect(&self) -> PxRect {
        PxRect {
            x: self.x0 as i32,
            y: self.top as i32,
            w: (self.x1 + 1 - self.x0) as i32,
            h: (self.bottom + 1 - self.top) as i32,
        }
    }
}

/// 把一段连续合格行变成矩形；高度不在 [`PIN_TRACK_H`] 里、贴到画面边缘、或带内没有亮线证据，
/// 都不算轨道。
///
/// 高度判据同时挡掉「并集被漂移撑大」的情形：单段宽度在 [`segment_is_track`] 已过闸，
/// 这里再按**并集**宽度判一次，保证返回的矩形宽度永远是 `≤ PIN_TRACK_MAX_W`。
///
/// 「贴边」放在**亮线证据之前**：它比逐像素算亮占比便宜。
fn band_rect(gray: &GrayImage, rect: PxRect, gate: &TrackGate) -> Option<PxRect> {
    if !within_i32(PIN_TRACK_H, rect.h) || rect.w > gate.max_w as i32 {
        return None;
    }
    if touches_frame_edge(gray, rect) {
        return None;
    }
    if bright_ratio(gray, rect) < PIN_TRACK_BRIGHT_MIN {
        return None;
    }
    Some(rect)
}

/// 轨道两端是否贴到画面边缘（两侧各要求留出 [`PIN_TRACK_EDGE_MARGIN`] 的帧宽）。
fn touches_frame_edge(gray: &GrayImage, rect: PxRect) -> bool {
    let frame_w = gray.w as f32;
    let margin = frame_w * PIN_TRACK_EDGE_MARGIN;
    let (left, right) = (rect.x as f32, rect.right() as f32);
    left < margin || right > frame_w - margin
}

/// 矩形内 `luma > [`PIN_TRACK_BRIGHT_LUMA`]` 的像素占比（0..=1，越界自动裁剪）。
fn bright_ratio(gray: &GrayImage, rect: PxRect) -> f32 {
    let x0 = rect.x.max(0) as u32;
    let y0 = rect.y.max(0) as u32;
    let x1 = (rect.right().max(0) as u32).min(gray.w);
    let y1 = (rect.bottom().max(0) as u32).min(gray.h);
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let (mut bright, mut total) = (0u32, 0u32);
    for y in y0..y1 {
        for &luma in &gray.row(y)[x0 as usize..x1 as usize] {
            total += 1;
            if luma > PIN_TRACK_BRIGHT_LUMA {
                bright += 1;
            }
        }
    }
    if total == 0 {
        return 0.0;
    }
    bright as f32 / total as f32
}

/// 两个轨道候选里取更宽的那个（同宽取更高的，保证结果与遍历顺序无关）。
fn wider(current: Option<PxRect>, candidate: PxRect) -> Option<PxRect> {
    match current {
        Some(rect) if (rect.w, rect.h) >= (candidate.w, candidate.h) => Some(rect),
        _ => Some(candidate),
    }
}

/// 本行里够格当轨道的亮段（按 x 升序）。
fn track_segments(gray: &GrayImage, y: u32, gate: &TrackGate) -> Vec<(u32, u32)> {
    let row = gray.row(y);
    raw_segments(row, gate.gap)
        .into_iter()
        .filter(|&segment| segment_is_track(row, segment, gate))
        .collect()
}

/// 把一行切成若干「连续 `luma >= `[`PIN_TRACK_FLOOR`]」的段；暗缝窄于 `gap` 不算断开。
fn raw_segments(row: &[u8], gap: u32) -> Vec<(u32, u32)> {
    let mut segments = Vec::new();
    let mut start: Option<u32> = None;
    let mut dark = 0u32;
    for (index, &luma) in row.iter().enumerate() {
        let x = index as u32;
        if luma >= PIN_TRACK_FLOOR {
            start.get_or_insert(x);
            dark = 0;
            continue;
        }
        let Some(left) = start else { continue };
        dark += 1;
        if dark < gap {
            continue;
        }
        segments.push((left, x - dark));
        start = None;
        dark = 0;
    }
    if let Some(left) = start {
        segments.push((left, row.len() as u32 - 1));
    }
    segments
}

/// 单段够不够格当轨道：宽度、段内均值、左右被暗背景夹住，三件事缺一不可。
fn segment_is_track(row: &[u8], (x0, x1): (u32, u32), gate: &TrackGate) -> bool {
    let (start, end) = (x0 as usize, x1 as usize);
    let width = end + 1 - start;
    if width < gate.min_w as usize || width > gate.max_w as usize {
        return false;
    }
    let mean = mean_of(&row[start..=end]);
    if !(PIN_TRACK_LUMA.0..=PIN_TRACK_LUMA.1).contains(&mean) {
        return false;
    }
    let limit = mean as f32 * PIN_TRACK_CONTRAST;
    let left = context_mean(row, start.saturating_sub(gate.context as usize), start);
    let right = context_mean(row, end + 1, (end + 1 + gate.context as usize).min(row.len()));
    left < limit && right < limit
}

/// `row[from..to]` 的均值；空区间（段贴着画面边、没有可量的邻域）返回 255 判不合格。
fn context_mean(row: &[u8], from: usize, to: usize) -> f32 {
    if from >= to {
        return 255.0;
    }
    mean_of(&row[from..to]) as f32
}

fn mean_of(values: &[u8]) -> u32 {
    if values.is_empty() {
        return 0;
    }
    values.iter().map(|&luma| u32::from(luma)).sum::<u32>() / values.len() as u32
}

/// 从本行的段里取走「离 `center` 最近、且漂移 ≤ `drift`」的那一段。
fn take_near(segments: &mut Vec<(u32, u32)>, center: u32, drift: u32) -> Option<(u32, u32)> {
    let (index, _) = segments
        .iter()
        .enumerate()
        .map(|(index, &(x0, x1))| (index, ((x0 + x1) / 2).abs_diff(center)))
        .filter(|(_, distance)| *distance <= drift)
        .min_by_key(|(_, distance)| *distance)?;
    Some(segments.swap_remove(index))
}

/// 轨道内的白块二分类：**目标是「带内的宽块」，游标是「伸出带外的窄线」**。
///
/// 为什么不用「宽扁 vs 窄高」分类：游标压到目标上的那一帧（也就是**唯一该按空格的时刻**），
/// 两个白块在二值图上连成一片 —— 既不窄也不扁，按宽高比分类会把它整块丢掉，
/// 于是「对齐」这个状态永远观测不到。而「谁伸出轨道带」在两种情形下都成立：
/// 目标是带内的东西（最多溢出 1~2px），游标天生比带高（实测 h 20~30 vs 带高 15）。
fn pin_blobs(gray: &GrayImage, track: PxRect) -> (Vec<PxRect>, Vec<PxRect>) {
    (pin_targets(gray, track), pin_cursors(gray, track))
}

/// 目标方块：**轨道带内**宽度落在 [`PIN_TARGET_W`] 的白块（按 x 升序）。
///
/// 带内裁剪自带「高度 ≤ 带高」的效果，且游标单独立在带内时只有 2~8px 宽，被宽度下限排掉。
fn pin_targets(gray: &GrayImage, track: PxRect) -> Vec<PxRect> {
    let Some(mask) = gray_mask(gray, track, |luma| luma > PIN_WHITE_LUMA) else {
        return Vec::new();
    };
    let mut targets: Vec<PxRect> = external_contours(&mask.binary)
        .into_iter()
        .filter(|blob| blob.area >= PIN_MIN_BLOB)
        .map(|blob| mask.to_frame(blob.region, 1))
        .filter(|rect| (PIN_TARGET_W.0..=PIN_TARGET_W.1).contains(&(rect.w.max(0) as u32)))
        .collect();
    targets.sort_by_key(|rect| (rect.x + rect.w / 2, rect.y));
    targets
}

/// 游标：**伸出轨道带 ≥ [`PIN_CURSOR_PROTRUDE`] 像素**的白色列段（按 x 升序）。
///
/// 逐列判「带外有没有白像素」，再把连续列段合并成一根游标，纵向范围取该列段的白像素外接框。
fn pin_cursors(gray: &GrayImage, track: PxRect) -> Vec<PxRect> {
    let window = PxRect {
        x: track.x,
        y: track.y - PIN_VERTICAL_SLACK,
        w: track.w,
        h: track.h + PIN_VERTICAL_SLACK * 2,
    };
    let Some(mask) = gray_mask(gray, window, |luma| luma > PIN_WHITE_LUMA) else {
        return Vec::new();
    };
    let binary = &mask.binary;
    let band_top = (track.y - window.y).max(0) as u32;
    let band_bottom = band_top + track.h.max(0) as u32;
    let mut cursors = Vec::new();
    let mut start: Option<u32> = None;
    for x in 0..binary.width() {
        if column_protrudes(binary, x, band_top, band_bottom) {
            start.get_or_insert(x);
            continue;
        }
        if let Some(begin) = start.take() {
            cursors.extend(cursor_of(binary, begin, x, window));
        }
    }
    if let Some(begin) = start {
        cursors.extend(cursor_of(binary, begin, binary.width(), window));
    }
    cursors.sort_by_key(|rect| (rect.x + rect.w / 2, rect.y));
    cursors
}

/// 该列在轨道带外（上方或下方 ≥ [`PIN_CURSOR_PROTRUDE`] 像素处）有没有白像素。
fn column_protrudes(binary: &BinaryImage, x: u32, band_top: u32, band_bottom: u32) -> bool {
    let above = band_top
        .checked_sub(PIN_CURSOR_PROTRUDE)
        .is_some_and(|last| (0..=last).any(|y| binary.get(x, y)));
    let below_start = band_bottom + PIN_CURSOR_PROTRUDE - 1;
    above || (below_start..binary.height()).any(|y| binary.get(x, y))
}

/// 列段 `[start, end)` 的那根游标：纵向范围取段内白像素的外接框。
///
/// 宽度不在 [`PIN_CURSOR_W`] / 高度不到 [`PIN_CURSOR_MIN_H`] 的列段不是游标（噪点、装饰线）。
fn cursor_of(binary: &BinaryImage, start: u32, end: u32, window: PxRect) -> Option<PxRect> {
    let (mut top, mut bottom) = (None, 0u32);
    for x in start..end {
        for y in 0..binary.height() {
            if !binary.get(x, y) {
                continue;
            }
            top.get_or_insert(y);
            // 外接框的下沿 = 段内**最低**的白像素：必须取 max。写成 `bottom = y` 时，
            // 后一列只要在上方有白像素就会把下沿压到 `top` 之上（实机亮栏杆帧就是这样），
            // debug 下 `bottom + 1 - top` 直接 u32 下溢 panic，release 下则静默得到负数高度。
            bottom = bottom.max(y);
        }
    }
    let top = top?;
    let rect = PxRect {
        x: window.x + start as i32,
        y: window.y + top as i32,
        w: (end - start) as i32,
        h: (bottom + 1 - top) as i32,
    };
    let (w, h) = (rect.w.max(0) as u32, rect.h.max(0) as u32);
    ((PIN_CURSOR_W.0..=PIN_CURSOR_W.1).contains(&w) && h >= PIN_CURSOR_MIN_H).then_some(rect)
}

/// 每个目标配「水平距离最近、且没被别的目标占用」的游标；配不上的目标不进结果。
///
/// 目标已按 x 升序 ⇒ 结果天然按目标 x 升序（调用方按顺序去重/记账）。
fn pair_pins(targets: &[PxRect], cursors: &[PxRect]) -> Vec<(usize, usize, i32)> {
    let mut used = vec![false; cursors.len()];
    targets
        .iter()
        .enumerate()
        .filter_map(|(index, target)| {
            let target_x = target.x + target.w / 2;
            let (slot, distance) = cursors
                .iter()
                .enumerate()
                .filter(|(slot, _)| !used[*slot])
                .map(|(slot, cursor)| (slot, (cursor.x + cursor.w / 2 - target_x).abs()))
                .min_by_key(|&(_, distance)| distance)?;
            used[slot] = true;
            Some((index, slot, distance))
        })
        .collect()
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
    /// 信号线的**亮白**：实机实测纯白一族（三通道均值 > 170），比档位条更亮而非更暗。
    pub(crate) const WIRE: [u8; 3] = [236, 238, 240];
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

    /// 信号线的一段：亮白、4px 厚（实机 4~6px），横贯整列窗。
    pub(crate) fn wire_rect(column: i32, y: i32) -> PxRect {
        let half = column_half_pitch_of_fixture();
        PxRect {
            x: column - half,
            y: y - 2,
            w: half * 2,
            h: 4,
        }
    }

    fn column_half_pitch_of_fixture() -> i32 {
        ((COLUMNS[1] - COLUMNS[0]) / 2).max(20)
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
        // 信号线：**亮白折线**，在每列的高度 = 该列当前档位。四列可以不同高，所以按列画成阶梯。
        // 少了这条线，「每列现在是什么值」就没有观测通道（旧实现靠琥珀框，实机只有
        // 受控那一列有框，其余恒为 None）。
        for (index, row) in selected.iter().enumerate() {
            let Some(row) = row else {
                continue;
            };
            canvas.rect(wire_rect(COLUMNS[index], ROWS[*row]), WIRE);
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

    /// 横线画在**指定 y**（用于「停在两档之间」「粗亮带冒充横线」这类边界）。
    pub(crate) fn machine_with_wire_ys(wire_ys: [Option<i32>; LINE_COLUMNS]) -> Canvas {
        let mut canvas = machine([None; LINE_COLUMNS]);
        for (index, y) in wire_ys.iter().enumerate() {
            if let Some(y) = y {
                canvas.rect(wire_rect(COLUMNS[index], *y), WIRE);
            }
        }
        canvas
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
        assert_eq!(
            line.wire_y,
            [Some(ROWS[0]), Some(ROWS[1]), Some(ROWS[2]), Some(ROWS[3])],
            "每列横线 y = 该列档位行"
        );
    }

    #[test]
    fn wire_height_alone_is_enough_when_no_column_has_an_amber_frame() {
        let frame =
            machine_with_wire_ys([Some(ROWS[3]), Some(ROWS[1]), Some(ROWS[0]), Some(ROWS[2])])
                .frame();
        let line = locate_line(&frame).expect("没有任何琥珀框时，横线高度就是唯一观测通道");

        assert_eq!(
            line.wire_y,
            [Some(ROWS[3]), Some(ROWS[1]), Some(ROWS[0]), Some(ROWS[2])],
            "亮白横线的 y 必须逐列读出来"
        );
        assert_eq!(
            line.selected,
            vec![Some(3), Some(1), Some(0), Some(2)],
            "横线高度映射回档位行"
        );
    }

    #[test]
    fn wire_between_two_rows_reads_as_no_selection_but_keeps_its_height() {
        // 行距 50 ⇒ 容差 1/3 = 16px；差 20px 属于「还没到位」。
        let between = ROWS[1] + 20;
        let frame = machine_with_wire_ys([
            Some(ROWS[0]),
            Some(between),
            Some(ROWS[2]),
            Some(ROWS[3]),
        ])
        .frame();
        let line = locate_line(&frame).expect("几何仍可定位");

        assert_eq!(line.wire_y[1], Some(between), "横线本身必须看得见");
        assert_eq!(
            line.selected[1], None,
            "差 20px > 容差 16px ⇒ 未到位，绝不许四舍五入猜一行"
        );
    }

    #[test]
    fn thick_bright_band_is_not_mistaken_for_the_wire() {
        // 面板里的粗亮带：8px（> 横线厚度上限 6）、又矮于 ROW_RUN 下限，两层判据都该挡掉。
        let mut canvas = machine_with_wire_ys([
            Some(ROWS[0]),
            Some(ROWS[1]),
            Some(ROWS[2]),
            Some(ROWS[3]),
        ]);
        canvas.rect(
            PxRect {
                x: CONTENT.x,
                y: (ROWS[1] + ROWS[2]) / 2 - 4,
                w: CONTENT.w,
                h: 8,
            },
            WIRE,
        );

        let line = locate_line(&canvas.frame()).expect("粗亮带不该破坏定位");
        assert_eq!(
            line.wire_y,
            [Some(ROWS[0]), Some(ROWS[1]), Some(ROWS[2]), Some(ROWS[3])],
            "8px 粗亮带必须被厚度判据挡掉，不许抢走横线的位置"
        );
    }

    #[test]
    fn wire_band_keeps_the_default_position_but_drops_the_panel_border() {
        // 实测三个比例：横线默认位置在末行下方 0.51 行距、面板底边框在 0.90、标题栏在 -0.93。
        let rows = [100, 200, 300, 400];
        let (top, bottom) = wire_band(&rows).expect("4 行必有可搜带");

        assert_eq!((top, bottom), (25, 475), "首行上方与末行下方各 0.75 个行距");
        assert!(400 + 51 <= bottom, "横线默认位置（0.51 行距）必须收得住");
        assert!(400 + 90 > bottom, "面板底边框（0.90 行距）必须排得掉");
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
        assert_eq!(locate_line(&canvas.frame()), Err(LocateError::GridColumns));
    }

    #[test]
    fn a_screen_with_columns_but_no_rows_reports_rows() {
        // 列在、行数不出来：擦掉档位阵，只在 4 列各画一条 **8px 高**的亮条 ——
        // 低于 ROW_RUN 下限（10px），行投影一条带都切不出来。
        // 必须报 GridRows，而不是笼统的「数不到 4 列」。
        let mut canvas = machine([None; 4]);
        canvas.rect(CONTENT, PANEL);
        for column in COLUMNS {
            canvas.rect(
                PxRect {
                    x: column - 4,
                    y: ROWS[0] - 4,
                    w: 8,
                    h: 8,
                },
                SLOT,
            );
        }
        assert_eq!(locate_line(&canvas.frame()), Err(LocateError::GridRows));
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

    #[test]
    fn a_split_column_peak_is_tolerated_by_combining_non_contiguous_candidates() {
        // `drag_row3` 实测候选（720p，探针 `diagnose_line_grid` 原样输出）：
        // 亮线横段压在档位条上，把第 2 根切成 417/426/435；真正的 4 列
        // （285/426/577/728）因此在候选表里**不连续**。
        // 旧实现只看连续窗口 ⇒ 只能返回 None ⇒ 报 `GridColumns` ⇒ 用户看到「横线台没反应」。
        //
        // 断言按「落在哪一根列」判，不钉死单个像素：第 1 根被切成 267/274/285 三片，
        // 274 那组的间距反而更均匀（152/151/151），光凭候选表分不出谁是峰值 ——
        // 但两者相隔 11px，都落在同一根列条内，点下去效果一样。
        let candidates = [
            138, 157, 184, 212, 228, 267, 274, 285, 417, 426, 435, 577, 728, 772, 785,
        ];
        let picked = most_even(&candidates, 89).expect("碎峰不该让整张表判失败");
        for (got, want) in picked.iter().zip([285, 426, 577, 728]) {
            assert!(
                (got - want).abs() <= 15,
                "第 {} 列落到 {got}，离真列 {want} 超过 15px：{picked:?}",
                want
            );
        }
        // 碎峰还原成一根之后结论必须一致（否则说明是靠碎峰凑出来的）。
        let repaired = [138, 157, 184, 212, 228, 267, 274, 285, 426, 577, 728, 772, 785];
        assert_eq!(
            most_even(&repaired, 89),
            most_even(&candidates, 89),
            "还原碎峰不该改变选出来的那 4 列"
        );
    }

    // ── 行标签（左侧两位 0/1）──

    /// 标签底色：浅蓝绿（实测 180,220,215 一族，均值 ≈205）。
    const LABEL_BG: [u8; 3] = [180, 220, 215];
    /// 标签数字的深色墨（实测均值 <100）。
    const LABEL_INK: [u8; 3] = [30, 45, 45];

    /// 在 `(x, y-20)` 画一个标签块 + 两位数字（`1` 是 8px 竖条、`0` 是 15px 空心环）。
    ///
    /// 块宽 54px 铺满读数窗口（窗口 = `columns[0]` 左侧 0.70~1.75 个行距），
    /// 这样字形两侧都是标签底色 —— 与实机「数字落在浅色块里」同构。
    fn draw_label(canvas: &mut Canvas, x: i32, y: i32, text: &str) {
        canvas.rect(
            PxRect {
                x,
                y: y - 20,
                w: 54,
                h: 41,
            },
            LABEL_BG,
        );
        for (index, digit) in text.chars().enumerate() {
            let left = x + 14 + index as i32 * 19;
            let stem = PxRect {
                x: left,
                y: y - 9,
                w: 8,
                h: 19,
            };
            if digit == '1' {
                canvas.rect(stem, LABEL_INK);
            } else {
                canvas.ring(
                    PxRect {
                        w: 15,
                        ..stem
                    },
                    3,
                    LABEL_INK,
                );
            }
        }
    }

    /// 只在 `labels[index]` 有值的行画标签块（`None` = 那一行没有标签）。
    fn labelled_machine(labels: [Option<&str>; LINE_ROWS]) -> Canvas {
        let mut canvas = machine([None; LINE_COLUMNS]);
        for (index, label) in labels.iter().enumerate() {
            if let Some(text) = label {
                draw_label(&mut canvas, 12, ROWS[index], text);
            }
        }
        canvas
    }

    #[test]
    fn row_labels_are_read_as_binary_digits_from_the_left_column() {
        let canvas = labelled_machine([Some("10"), Some("11"), Some("01"), Some("00")]);
        let labels = read_row_labels(&canvas.frame(), &COLUMNS, &ROWS);
        assert_eq!(
            labels.map(|label| label.value),
            [Some(2), Some(3), Some(1), Some(0)],
            "10→2、11→3、01→1、00→0（自上而下）"
        );
        assert!(
            labels.iter().all(|label| label.window.is_some()),
            "读数窗口必须随结果一起带出来（探针要画框）"
        );
    }

    #[test]
    fn a_row_without_a_readable_label_stays_none_instead_of_guessing() {
        let canvas = labelled_machine([Some("10"), None, Some("01"), None]);
        let labels = read_row_labels(&canvas.frame(), &COLUMNS, &ROWS);
        assert_eq!(
            labels.map(|label| label.value),
            [Some(2), None, Some(1), None],
            "读不出的行必须是 None，绝不按 10/11/01/00 兜底"
        );
    }

    #[test]
    fn three_glyph_segments_are_rejected_instead_of_picking_two() {
        // 一行里画三根等宽竖条：段数不是 2 ⇒ 这一行整体判「读不出」。
        let mut canvas = machine([None; LINE_COLUMNS]);
        canvas.rect(
            PxRect {
                x: 12,
                y: ROWS[0] - 20,
                w: 54,
                h: 41,
            },
            LABEL_BG,
        );
        for left in [20, 32, 44] {
            canvas.rect(
                PxRect {
                    x: left,
                    y: ROWS[0] - 9,
                    w: 8,
                    h: 19,
                },
                LABEL_INK,
            );
        }
        let labels = read_row_labels(&canvas.frame(), &COLUMNS, &ROWS);
        assert_eq!(
            labels.map(|label| label.value),
            [None; LINE_ROWS],
            "3 段 ⇒ None（宁可读不出，也不许从 3 段里挑 2 段）"
        );
    }

    #[test]
    fn shuffled_labels_are_mapped_by_position_not_by_a_fixed_table() {
        // 每局标签顺序都会变：这里 `01/00/11/10` 从上往下。
        let canvas = labelled_machine([Some("01"), Some("00"), Some("11"), Some("10")]);
        let labels = read_row_labels(&canvas.frame(), &COLUMNS, &ROWS);
        assert_eq!(
            labels.map(|label| label.value),
            [Some(1), Some(0), Some(3), Some(2)],
            "值必须跟着画面走，不能写死成 10/11/01/00"
        );
    }

    #[test]
    fn locate_line_carries_the_labels_and_their_boxes() {
        let canvas = labelled_machine([Some("10"), Some("11"), Some("01"), Some("00")]);
        let line = locate_line(&canvas.frame()).expect("带标签的机台必须仍能定位");
        assert_eq!(
            line.row_labels,
            [Some(2), Some(3), Some(1), Some(0)],
            "定位结果要把标签值一起带出来"
        );
        assert!(
            line.label_boxes.iter().all(Option::is_some),
            "4 个标签框都要有：{:?}",
            line.label_boxes
        );
    }

    #[test]
    fn a_machine_without_labels_reports_all_none() {
        // 旧合成机台（与实机一样有屏幕/档位，但左侧没有标签块）⇒ 必须全是 None。
        let line = locate_line(&machine([None; 4]).frame()).expect("几何仍可定位");
        assert_eq!(line.row_labels, [None; LINE_ROWS], "没标签就是 None，不许猜");
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
        pub(crate) fn ring(&mut self, rect: PxRect, thickness: i32, rgb: [u8; 3]) -> &mut Self {
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

    /// 只画 `keep` 里的键（其余涂回台面）：用来造「缺了几格但四行四列都还在」的帧。
    pub(crate) fn panel_with_only(keep: &[usize]) -> Canvas {
        let mut canvas = Canvas::new(MACHINE);
        canvas.rect(SCREEN, PANEL);
        for &index in keep {
            canvas.rect(key_rect(index), KEY);
        }
        canvas
    }

    /// 16 颗键都在，但第 `index` 颗的键面被挖空成「只剩一圈 2px 边」。
    ///
    /// 实机远景/斜视下卡住的那 3~5 颗键就是这种样子：键面被白色图标与下方小字挖掉大半，
    /// 填充率掉到 **0.36~0.38**（spec §2.1）。外接框仍是 44×40（暗边是闭合的一圈），
    /// 只有填充率低于旧的 0.40 死线。
    pub(crate) fn panel_with_hollow_key(index: usize) -> Canvas {
        let mut canvas = panel_with_keys(16);
        let rect = key_rect(index);
        canvas.rect(
            PxRect {
                x: rect.x + 2,
                y: rect.y + 6,
                w: rect.w - 4,
                h: rect.h - 12,
            },
            MACHINE,
        );
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

    // ── t0：填充率 0.36 的键与 11 颗键（实机远景/斜视的真实现场）──

    /// 实机卡住的那 5 颗键的编号：去掉的正好让**每行每列都还有键**，
    /// 于是「凑不出 4×4」只能由命中数来解释，不会被行/列缺失掩盖。
    const ELEVEN_KEY_SPREAD: [usize; 11] = [0, 3, 4, 5, 6, 8, 9, 10, 12, 13, 14];
    /// 同上但只有 10 颗（`t=32` 那帧远景误报的命中数，spec §2.3）。
    const TEN_KEY_SPREAD: [usize; 10] = [0, 4, 6, 8, 9, 10, 12, 13, 14, 15];

    #[test]
    fn a_key_filled_down_to_thirty_six_percent_is_still_a_dark_block() {
        // 实测卡住的键填充率恒为 0.36/0.38（spec §2.1）。旧死线 0.40 把它整颗丢掉 ⇒
        // 每帧漏 3~5 颗 ⇒ 撞 KEY_MIN_HITS ⇒ 整台判失败。
        //
        // 断言必须打在 `dark_blocks` 这一层：4×4 里的键尺寸完全一致，被丢掉的那颗由
        // 「中位尺寸 + 交点中心」补出来，补出来的矩形与实测矩形**逐像素相同** ——
        // 在 `locate_keys` 那一层看不出区别，只有候选集本身能证伪。
        let key = key_rect(0);
        let hole = PxRect {
            x: key.x + 2,
            y: key.y + 6,
            w: key.w - 4,
            h: key.h - 12,
        };
        let fill = (key.w * key.h - hole.w * hole.h) as f32 / (key.w * key.h) as f32;
        assert!(
            (0.32..0.40).contains(&fill),
            "夹具的填充率必须落在实测量到的 0.36~0.38 区间（实际 {fill}），否则这条用例证不了什么"
        );

        let frame = panel_with_hollow_key(0).frame();
        let blocks = dark_blocks(&frame.luma());
        let kept = blocks
            .iter()
            .find(|block| block.x == key.x && block.y == key.y)
            .unwrap_or_else(|| panic!("填充率 {fill} 的键必须被 dark_blocks 收下：{blocks:?}"));
        assert_eq!((kept.w, kept.h), KEY_SIZE, "外接框仍是整颗键：{kept:?}");
        // 端到端：16 颗键都在，整台照样可定位。
        assert_eq!(
            locate_keys(&frame).expect("16 颗键都在").cells.len(),
            KEY_GRID_SIDE * KEY_GRID_SIDE
        );
    }

    #[test]
    fn eleven_keys_still_form_a_full_grid() {
        // 面板完整可见的那几秒里网格只对上 11 颗（spec §2.1：hit95 = 11）：
        // 缺的 5 格用中位键尺寸补出来，补出来的格照样要能点（spec B1）。
        let frame = panel_with_only(&ELEVEN_KEY_SPREAD).frame();
        let keys = locate_keys(&frame).expect("11 颗键必须仍能定出 4×4");
        assert_eq!(keys.cells.len(), 16, "补满 16 格");
        assert_eq!(keys.cells[0], key_rect(0), "命中格用实测矩形");
        let missing = keys.cells[6];
        assert_eq!(
            (missing.x + missing.w / 2, missing.y + missing.h / 2),
            key_center(6),
            "补出来的格必须落在该键的几何中心上（否则会点在键缝里）：{missing:?}"
        );
        assert_eq!((missing.w, missing.h), KEY_SIZE, "补出来的格用中位键尺寸");
    }

    #[test]
    fn ten_spread_keys_are_still_not_a_key_grid() {
        // 10 颗、四行四列都在（只是稀疏）—— 仍然不许当成键阵。
        // 这条是「KEY_MIN_HITS 不许再往下调」的回归钉：`t=32` 那帧远景误报命中 10。
        assert_eq!(
            locate_keys(&panel_with_only(&TEN_KEY_SPREAD).frame()),
            Err(LocateError::Keys)
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

    // ── t1：三态定位（NotYet 与 Absent 必须分开）──

    #[test]
    fn blue_panel_base_is_present_but_a_bare_wall_is_not() {
        // 面板底色（深蓝）：spec §0④ H3' 区间内。
        let panel_base = Canvas::new([68, 70, 110]);
        assert!(panel_present(&panel_base.frame()), "整屏深蓝底色必须判「面板已在」");
        // 纯灰墙面：没有任何蓝色面板。
        assert!(!panel_present(&Canvas::new(MACHINE).frame()), "灰墙不是面板");
        // 画面太小（判不了）：宁可说「没有」，也不许 panic。
        let tiny = Frame::new(32, 32, vec![0u8; 32 * 32 * 4], std::time::Instant::now())
            .expect("合法帧");
        assert!(!panel_present(&tiny));
    }

    #[test]
    fn key_state_is_absent_when_there_is_no_panel_base_at_all() {
        // 灰墙：既定位不到键阵，也没有面板底色 ⇒ Absent（立刻可以拒绝，不必等 2.1s）。
        assert_eq!(locate_keys_state(&Canvas::new(MACHINE).frame()).state_name(), "Absent");
    }

    #[test]
    fn key_state_is_not_yet_while_the_panel_base_is_up_but_the_keys_are_missing() {
        // 面板底色已淡入、16 颗键还没长出来 —— 这正是「首开慢」的那 1.4s。
        let frame = Canvas::new([68, 70, 110]).frame();
        assert_eq!(locate_keys_state(&frame).state_name(), "NotYet");
    }

    #[test]
    fn a_white_chassis_panel_with_a_half_grown_key_grid_is_not_yet_not_absent() {
        // 黄键台的面板是**白色机箱**：深蓝占比实测 0.0，只认底色时这里会报 Absent
        // ⇒ 定位一失败就收尾（用户实测：按第一次 F 必然失败，得再按一次）。
        // 键阵已经在画面里（只是这一帧没凑齐 4×4）⇒ 必须报 NotYet，继续等且不计失败。
        let half = panel_with_only(&TEN_KEY_SPREAD).frame();
        assert_eq!(locate_keys(&half), Err(LocateError::Keys), "前提：这一帧定位失败");
        assert_eq!(
            locate_keys_state(&half).state_name(),
            "NotYet",
            "白机箱 + 暗键阵 = 面板已在，结构没长齐"
        );
        // 反例：整片台面一颗暗键都没有 ⇒ 真的不在这个界面。
        assert_eq!(locate_keys_state(&Canvas::new(MACHINE).frame()).state_name(), "Absent");
    }

    #[test]
    fn keys_ready_needs_both_geometry_and_enough_lit_pixels() {
        // 面板可定位、但一颗亮键都没有 ⇒ 还没就绪（spec B2：0.4–1.8s 必须继续等）。
        assert_eq!(keys_ready(&panel(&[], 0).frame()).state_name(), "NotYet");
        // 一颗键的发光图标只有几百像素，仍不到 3000px 中线 ⇒ 继续等。
        assert_eq!(keys_ready(&panel(&[5], 0).frame()).state_name(), "NotYet");
        // 16 颗全亮（约 7000px）⇒ 真的可以动手了。
        let all: Vec<usize> = (0..16).collect();
        assert!(keys_ready(&panel(&all, 0).frame()).is_found());
    }

    // ── t2：进度行（实测百分比行的兜底读法）──

    #[test]
    fn spec_progress_row_reads_five_check_marks() {
        // 只画绿 ✓、不画细边框 ⇒ 自适应找框必然失败，只能靠实测百分比行。
        let mut canvas = panel_without_boxes();
        for stage in 0..KEY_PROGRESS_MAX {
            let slot = spec_slot_rect(W, H, stage);
            canvas.rect(inset(slot, 2), CHECK);
        }
        let frame = canvas.frame();
        let slots = spec_progress_slots(&frame, &frame.luma(), 1.0);
        assert_eq!(slots.len(), KEY_PROGRESS_MAX, "{slots:?}");
        let blank = Canvas::new(MACHINE).frame();
        assert_eq!(spec_progress_slots(&blank, &blank.luma(), 1.0), Vec::new(),
            "整片同色背景不是进度行，绝不能报出假进度");
    }

    #[test]
    fn a_green_check_is_readable_in_colour_and_never_in_grey() {
        // 判绿必须读 BGRA：灰度图里三通道相等，`g >= r+30` 永远不成立。
        let slot = spec_slot_rect(W, H, 0);
        let mut colour = Canvas::new(PANEL);
        colour.rect(inset(slot, 2), CHECK);

        let mut grey = Canvas::new([40, 40, 40]);
        grey.rect(inset(slot, 2), [142, 142, 142]);

        let colour_frame = colour.frame();
        assert!(
            slot_evidence(&colour_frame, &colour_frame.luma(), slot, 1.0).is_some(),
            "彩色帧里的绿 ✓ 必须算作已勾选"
        );
        let grey_frame = grey.frame();
        assert!(
            slot_evidence(&grey_frame, &grey_frame.luma(), slot, 1.0).is_none(),
            "三通道相等的灰块不是绿 ✓，不许当成已勾选"
        );
    }

    #[test]
    fn progress_falls_back_to_the_spec_row_when_no_border_is_found() {
        let mut canvas = panel_without_boxes();
        for stage in 0..KEY_PROGRESS_MAX {
            canvas.rect(inset(spec_slot_rect(W, H, stage), 2), CHECK);
        }
        let keys = locate_keys(&canvas.frame()).expect("几何完整");
        assert_eq!(keys.progress.len(), KEY_PROGRESS_MAX, "兜底必须给出 5 格");
        assert_eq!(
            keys.progress[0],
            spec_slot_rect(W, H, 0),
            "兜底格就是实测百分比行上的第一格"
        );
    }

    #[test]
    fn adaptive_boxes_win_over_the_spec_row() {
        let keys = locate_keys(&panel(&[], 3).frame()).expect("几何完整");
        assert_eq!(
            keys.progress,
            (0..KEY_PROGRESS_MAX).map(box_rect).collect::<Vec<_>>(),
            "自适应框够 3 格时不许退到百分比行"
        );
    }

    #[test]
    fn spec_row_geometry_matches_the_measured_pitch() {
        // spec §0②：1280 宽下单格 28px、格距 56px。
        let first = spec_slot_rect(1280, 720, 0);
        let second = spec_slot_rect(1280, 720, 1);
        assert_eq!(second.x - first.x, 56, "格距 4.375% × 1280 = 56px");
        assert_eq!(first.w, 28, "单格 2.19% × 1280 = 28px");
        assert_eq!(first.h, 20, "单格 2.78% × 720 = 20px");
        // 百分比行必须落在整帧左半侧（屏幕里），不能跑到右侧模块键区。
        assert!(first.x < 640 && first.right() < 640);
    }

    #[test]
    fn the_measured_region_fits_all_five_slots() {
        // 2026-11-16 1080p 实测：5 格跨 `x 0.275..0.494`、`y 0.453..0.528`。
        // 旧区域 `x 0.262..0.417` 右边界只到第 3 格 ⇒ 这里最多切出 3 格 ⇒ 这条必须红。
        for (w, h) in [(W, H), (1280, 720)] {
            let inside: Vec<PxRect> = (0..KEY_PROGRESS_MAX)
                .map(|stage| spec_slot_rect(w, h, stage))
                .filter(|slot| inside_progress_region(*slot, w, h))
                .collect();
            assert_eq!(
                inside.len(),
                KEY_PROGRESS_MAX,
                "{w}×{h} 上 5 格必须全落在实测区域内：{inside:?}"
            );
        }
    }

    #[test]
    fn an_off_row_or_off_width_box_invalidates_the_whole_row() {
        // 实机故障：同一张截图上 5 格是 `✓ ✓ ✓ ✓ ⬜`（逐格绿像素 174/164/185/239/0），
        // 日志却报「5/5 已勾选」⇒ 第 5 个「框」框到了别处。宁可退化成读不到，不许报假的 5/5。
        let row: Vec<PxRect> = (0..4).map(box_rect).collect();
        assert_eq!(consistent_progress_row(row.clone()).len(), 4, "正常一排必须原样返回");

        // ① 纵向偏 11px：仍在 `largest_row` 的同一带内（|Δ中心| = 11 ≤ 24/2），
        //    但超出「同一行」容差（24 × 0.3 = 7.2）⇒ 整排作废。
        let off_row = PxRect {
            y: BOX_Y + 11,
            ..box_rect(4)
        };
        let mut mixed = row.clone();
        mixed.push(off_row);
        assert!(
            consistent_progress_row(mixed).is_empty(),
            "偏到别处的第 5 框必须让整排作废，不许凑成 5/5"
        );

        // ② 宽度 20px vs 中位 30px：偏差 33% > 25% ⇒ 同样整排作废（框到了窄的绿图标上）。
        let narrow = PxRect {
            w: 20,
            ..box_rect(4)
        };
        let mut mixed = row;
        mixed.push(narrow);
        assert!(
            consistent_progress_row(mixed).is_empty(),
            "窄得不像同一排的框必须让整排作废"
        );
    }

    #[test]
    fn a_fifth_box_covering_something_else_never_reports_five_of_five() {
        // 端到端：4 个真勾 + 第 5 个框偏到下一行 ⇒ 进度必须退化（报 5/5 就是实机那个假账）。
        let mut canvas = panel_without_boxes();
        for stage in 0..4 {
            canvas.rect(inset(box_rect(stage), 2), CHECK);
            canvas.ring(box_rect(stage), 2, BORDER);
        }
        canvas.ring(
            PxRect {
                y: BOX_Y + 11,
                ..box_rect(4)
            },
            2,
            BORDER,
        );
        let keys = locate_keys(&canvas.frame()).expect("几何完整");
        assert!(
            keys.progress.len() < KEY_PROGRESS_MAX,
            "不许报出第 5 格：{:?}",
            keys.progress
        );
    }

    // ── t5：缩放感知阈值 ──

    #[test]
    fn scale_dependent_thresholds_keep_the_720p_values() {
        // B10：原生 720p 必须逐字节走原路径 —— 两个阈值一个数都不能变。
        assert_eq!(box_luma(1.0), (150, 230), "720p 进度框亮度带");
        assert_eq!(ring_delta(1.0), 12, "720p 空槽边框亮度差");
        // 下采样帧换松阈值（边缘被均值糊掉）。
        assert_eq!(box_luma(1.5), (105, 235), "1080p 进度框亮度带");
        assert_eq!(ring_delta(1.5), 8, "1080p 空槽边框亮度差");
        // 分界线本身：等于 1.15 仍算「原生」，超过才算下采样。
        assert_eq!(box_luma(SCALE_LOOSE), (150, 230));
        assert_eq!(ring_delta(SCALE_LOOSE), 12);
        assert_eq!(box_luma(SCALE_LOOSE + 0.01), (105, 235));
    }

    /// 1080p 版黄键台：把 640×360 夹具的几何按 1.5 倍画进 1920×1080（100% 脱敏）。
    ///
    /// 面板只占左上角，其余是台面底色 —— 归一化到 720p 世界后，面板几何与夹具**完全重合**，
    /// 因此「1080p 与 720p 该看到同一台机器」这件事是逐像素可比的。
    ///
    /// 已勾选的绿勾画成**居中字形块**而不是铺满内腔：铺满时整块（含边框）会连成一个实心
    /// 连通域，被「细边框填充率 ≤0.70」挡掉 —— 这在松散亮度带（105..235，绿勾亮度 ≈145
    /// 落在带内）下尤其致命。实机的 ✓ 本来就是字形，不是实心块。
    fn hd_frame(lit: &[usize], checked: usize, ring_thickness: i32) -> Frame {
        let (w, h) = (1920u32, 1080u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        for pixel in data.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[MACHINE[2], MACHINE[1], MACHINE[0], 255]);
        }
        let scale = |value: i32| (value as f32 * 1.5).round() as i32;
        let hd = |rect: PxRect| PxRect {
            x: scale(rect.x),
            y: scale(rect.y),
            w: scale(rect.w),
            h: scale(rect.h),
        };
        let mut shapes: Vec<(PxRect, [u8; 3])> = vec![(hd(SCREEN), PANEL)];
        for stage in 0..BOX_X.len() {
            let rect = hd(box_rect(stage));
            if stage < checked {
                let glyph = inset(rect, rect.w / 3);
                shapes.push((glyph, CHECK));
            }
            let thickness = ring_thickness;
            shapes.push((
                PxRect {
                    w: thickness,
                    ..rect
                },
                BORDER,
            ));
            shapes.push((
                PxRect {
                    x: rect.right() - thickness,
                    w: thickness,
                    ..rect
                },
                BORDER,
            ));
            shapes.push((
                PxRect {
                    h: thickness,
                    ..rect
                },
                BORDER,
            ));
            shapes.push((
                PxRect {
                    y: rect.bottom() - thickness,
                    h: thickness,
                    ..rect
                },
                BORDER,
            ));
        }
        for index in 0..16 {
            let rect = hd(key_rect(index));
            shapes.push((rect, KEY));
            if lit.contains(&index) {
                shapes.push((inset(rect, rect.w / 4), GLOW));
            }
        }
        for (rect, rgb) in shapes {
            for y in rect.y.max(0)..rect.bottom().min(h as i32) {
                for x in rect.x.max(0)..rect.right().min(w as i32) {
                    let index = ((y as u32 * w + x as u32) * 4) as usize;
                    data[index] = rgb[2];
                    data[index + 1] = rgb[1];
                    data[index + 2] = rgb[0];
                    data[index + 3] = 255;
                }
            }
        }
        Frame::new(w, h, data, std::time::Instant::now()).expect("合法 1080p 帧")
    }

    #[test]
    fn a_1080p_frame_still_locates_the_whole_panel_and_five_boxes() {
        // B11：1920×1080（scale = 1.5）必须一次定位成功、5 个进度格不少。
        // 细边框在盒式下采样后被均值糊掉，只有松阈值（105..235）读得到 ——
        // 严阈值那一版只能找到 0 个框，整行退化成「读不到进度」。
        let frame = hd_frame(&[5], 2, 3);
        let keys = locate_keys(&frame).expect("1080p 黄键台必须定位成功");
        assert_eq!(keys.cells.len(), KEY_GRID_SIDE * KEY_GRID_SIDE, "16 颗键");
        assert_eq!(
            keys.progress.len(),
            KEY_PROGRESS_MAX,
            "1080p 下采样后 5 个进度格必须全部识别：{:?}",
            keys.progress
        );
        // 坐标必须乘回原图：第 0 颗键的中心 = 720p 世界的 1.5 倍。
        let (cx, cy) = key_center(0);
        let cell = keys.cells[0];
        assert_eq!(
            (cell.x + cell.w / 2, cell.y + cell.h / 2),
            (
                (cx as f32 * 1.5).round() as i32,
                (cy as f32 * 1.5).round() as i32
            ),
            "点击落点必须回到原图坐标"
        );
    }

    /// 1080p 帧，把第 `stage` 个进度格的**内腔**涂成实心块 `rgb`（与细边框连成一体）。
    ///
    /// 实机上的 ✓ 是字形，但**实心勾**也会出现（勾填满内腔、连成一整块）；
    /// 松散亮度带（105..235）下这种格子的填充率是 1.0，必须靠「有绿」放行。
    fn hd_frame_with_solid(stage: usize, rgb: [u8; 3]) -> Frame {
        let mut frame = hd_frame(&[], 0, 3);
        let rect = inset(scale_rect(box_rect(stage), 1.5), 3);
        for y in rect.y.max(0)..rect.bottom().min(frame.h as i32) {
            for x in rect.x.max(0)..rect.right().min(frame.w as i32) {
                let index = ((y as u32 * frame.w + x as u32) * 4) as usize;
                frame.data[index..index + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
            }
        }
        frame
    }

    /// 该帧里第 `stage` 个进度格的中心（原图坐标）。
    fn progress_centre(stage: usize) -> (i32, i32) {
        let rect = scale_rect(box_rect(stage), 1.5);
        (rect.x + rect.w / 2, rect.y + rect.h / 2)
    }

    #[test]
    fn a_solid_green_check_box_is_still_a_progress_box_in_1080p() {
        // 1080p 松散亮度带把实心绿内腔（luma ≈145）也吃进掩码 ⇒ 整格成实心域（填充率 1.0）。
        // 这一格确实有勾，不许因为形状被判死。
        let frame = hd_frame_with_solid(1, CHECK);
        let keys = locate_keys(&frame).expect("实心绿勾不影响定位");
        assert_eq!(
            keys.progress.len(),
            KEY_PROGRESS_MAX,
            "实心绿勾的那一格必须仍算进度格：{:?}",
            keys.progress
        );
        let found = keys
            .progress
            .iter()
            .any(|rect| (rect.x + rect.w / 2, rect.y + rect.h / 2) == progress_centre(1));
        assert!(found, "放行的必须是第 1 格本身：{:?}", keys.progress);
    }

    #[test]
    fn a_solid_grey_block_without_a_check_is_still_dropped() {
        // 反证：同一条放行只认绿。实心浅灰块（无绿）填充率同样 1.0，必须照旧被丢弃。
        let frame = hd_frame_with_solid(1, BORDER);
        let keys = locate_keys(&frame).expect("灰块不影响其余几何");
        assert_eq!(
            keys.progress.len(),
            KEY_PROGRESS_MAX - 1,
            "无勾的实心灰块不许被当成进度格：{:?}",
            keys.progress
        );
        let centre = progress_centre(1);
        assert!(
            !keys
                .progress
                .iter()
                .any(|rect| (rect.x + rect.w / 2, rect.y + rect.h / 2) == centre),
            "第 1 格必须缺席：{:?}",
            keys.progress
        );
    }

    #[test]
    fn the_green_allowance_cannot_fire_on_the_720p_path() {
        // 720p 带宽 (150,230) 本来就不含绿内腔（CHECK luma ≈142）⇒ 实心绿块连掩码都进不去，
        // 「有绿就放行」在 720p 上无从触发。同一帧换松散带宽才捡得回来 ——
        // 差异只在带宽，不在放行本身。
        let mut canvas = Canvas::new(MACHINE);
        canvas.rect(SCREEN, PANEL);
        canvas.rect(box_rect(0), CHECK); // 只有实心绿块，没有浅色细边框
        let frame = canvas.frame();
        let gray = frame.luma();
        assert!(
            find_progress_boxes(&frame, &gray, SCREEN, box_luma(1.0)).is_empty(),
            "720p 带宽下绿块不进掩码 ⇒ 放行是 no-op"
        );
        assert_eq!(
            find_progress_boxes(&frame, &gray, SCREEN, box_luma(1.5)).len(),
            1,
            "换成松散带宽后同一块绿会被放行 —— 证明放行只在 1080p 世界生效"
        );
    }

    #[test]
    fn a_1080p_panel_with_zero_lit_keys_is_ready_to_start() {
        // 就绪闸（`worker::wait_keys_ready`）只等面板结构：面板已在、一颗键都没亮（1% 进度）
        // 才是真面板刚打开的样子。旧闸把「亮键像素」并进就绪 ⇒ 这一帧永远 NotYet ⇒ 2100ms
        // 超时，日志里报「就绪等待失败 / 面板已消失」，而模板分是 0.95。
        let frame = hd_frame(&[], 0, 3);
        assert_eq!(
            keys_panel_ready(&frame),
            Locate::Found(()),
            "1080p 面板在、零亮键 ⇒ 就绪"
        );
        // 对照：组合判据（面板 + 亮键）在同一帧上仍是 NotYet —— 差别就是本轮修的东西。
        assert_eq!(keys_ready(&frame).state_name(), "NotYet");
        // 面板彻底不在时仍然必须说清（fail-closed，绝不放行）。
        let blank = Canvas::new(MACHINE).frame();
        assert_eq!(keys_panel_ready(&blank), Locate::Absent);
    }

    #[test]
    fn keys_ready_scales_the_lit_pixel_threshold_with_the_frame_height() {
        // ready 闸必须在**原帧**上判：亮键像素与阈值同乘 scale²，判据才不会随分辨率漂移。
        // 一颗亮键（1080p ≈ 990px）远不够，16 颗（≈15840px）才够。
        assert_eq!(
            keys_ready(&hd_frame(&[5], 0, 3)).state_name(),
            "NotYet",
            "单颗亮键不够"
        );
        let all: Vec<usize> = (0..16).collect();
        assert!(
            keys_ready(&hd_frame(&all, 0, 3)).is_found(),
            "16 颗全亮必须判就绪（阈值按 scale² 放大后才成立）"
        );
        // 720p 一侧保持原判据（3000px 中线）。
        assert_eq!(keys_ready(&panel(&[5], 0).frame()).state_name(), "NotYet");
        assert!(keys_ready(&panel(&all, 0).frame()).is_found());
    }

    #[test]
    fn a_misaligned_row_never_wins_on_thin_contrast() {
        // B13：错位多读出一格、但那几格几乎没有对比度 ⇒ 必须回退 dy = 0，宁可少读一格。
        let slot = |x: i32| PxRect { x, y: 0, w: 10, h: 10 };
        let readings = vec![
            RowReading {
                dy: 0,
                slots: (0..4).map(slot).collect(),
                margin: 120,
            },
            RowReading {
                dy: -2,
                slots: (0..5).map(slot).collect(),
                margin: 9,
            },
        ];
        let chosen = best_row(readings);
        assert_eq!(chosen.len(), 4, "不许假装读到 5 格：{chosen:?}");
    }

    #[test]
    fn a_shifted_row_with_real_contrast_still_wins() {
        // 反面：真槽整体下移 2px 时，5 格 + 更高总余量必须赢（否则 1080p 永远读不到完整进度行）。
        let slot = |x: i32| PxRect { x, y: 0, w: 10, h: 10 };
        let readings = vec![
            RowReading {
                dy: 0,
                slots: (0..4).map(slot).collect(),
                margin: 40,
            },
            RowReading {
                dy: -2,
                slots: (0..5).map(slot).collect(),
                margin: 250,
            },
        ];
        let chosen = best_row(readings);
        assert_eq!(chosen.len(), 5, "真槽错位时必须跟着走：{chosen:?}");
    }
}


#[cfg(test)]
mod pins_tests {
    use super::*;
    use std::time::Instant;

    // ── 任务保险合成帧（100% 脱敏：只复刻相对结构，不含任何真实机位 / 账号 / 路径）──

    const PIN_W: u32 = 640;
    const PIN_H: u32 = 360;
    /// 轨道：横贯画面的中灰长条（实机 720p 实测 y188..202、高 15，这里同量级）。
    const TRACK: PxRect = PxRect {
        x: 100,
        y: 150,
        w: 400,
        h: 15,
    };
    /// 轨道底色：中灰（实测行均值 67）。
    const TRACK_GREY: [u8; 3] = [67, 67, 67];
    const WHITE: [u8; 3] = [255, 255, 255];

    fn blank() -> Vec<u8> {
        vec![0u8; (PIN_W * PIN_H * 4) as usize]
    }

    fn paint(data: &mut [u8], rect: PxRect, rgb: [u8; 3]) {
        for y in rect.y.max(0)..rect.bottom().min(PIN_H as i32) {
            for x in rect.x.max(0)..rect.right().min(PIN_W as i32) {
                let index = ((y as u32 * PIN_W + x as u32) * 4) as usize;
                data[index] = rgb[2];
                data[index + 1] = rgb[1];
                data[index + 2] = rgb[0];
                data[index + 3] = 255;
            }
        }
    }

    fn frame_of(data: Vec<u8>) -> Frame {
        Frame::new(PIN_W, PIN_H, data, Instant::now()).expect("合成帧长度正确")
    }

    /// 轨道 + 目标方块（宽扁、带内）+ 游标（窄高、伸出带外 6px）。
    fn pins_frame(targets: &[i32], cursors: &[i32]) -> Frame {
        let mut data = blank();
        paint(&mut data, TRACK, TRACK_GREY);
        for &center_x in targets {
            paint(
                &mut data,
                PxRect {
                    x: center_x - 5,
                    y: 149,
                    w: 10,
                    h: 17,
                },
                WHITE,
            );
        }
        for &center_x in cursors {
            paint(
                &mut data,
                PxRect {
                    x: center_x - 2,
                    y: 144,
                    w: 5,
                    h: 27,
                },
                WHITE,
            );
        }
        frame_of(data)
    }

    #[test]
    fn pairs_two_targets_with_two_cursors() {
        // 反例：旧的「5 个固定槽中心」判据在这张帧上一对都配不出来（压根没有槽位坐标）。
        let pins = locate_pins(&pins_frame(&[200, 400], &[160, 430]))
            .expect("轨道 + 2 对必须一次定位成功");
        assert_eq!(pins.track, TRACK, "轨道必须量回原矩形");
        assert_eq!(pins.targets.len(), 2, "目标白块：{:?}", pins.targets);
        assert_eq!(pins.cursors.len(), 2, "游标细竖线：{:?}", pins.cursors);
        assert_eq!(pins.pairs.len(), 2, "2 目标 2 游标必须配成 2 对：{:?}", pins.pairs);
        assert_eq!(pins.pairs[0].2, 40, "第 1 对水平距离：{:?}", pins.pairs);
        assert_eq!(pins.pairs[1].2, 30, "第 2 对水平距离：{:?}", pins.pairs);
    }

    #[test]
    fn an_aligned_cursor_is_still_seen_on_top_of_its_target() {
        // 对齐那一帧两个白块连成一片（既不窄也不扁）。按宽高比分类会把整块丢掉 ——
        // 于是「游标压住目标」这个**唯一该按空格**的时刻永远观测不到。
        let pins = locate_pins(&pins_frame(&[200, 400], &[200, 430]))
            .expect("游标压在目标上也必须定位成功");
        assert_eq!(pins.targets.len(), 2, "被压住的目标仍是目标：{:?}", pins.targets);
        assert_eq!(pins.cursors.len(), 2, "压住目标的游标仍是游标：{:?}", pins.cursors);
        assert_eq!(pins.pairs.len(), 2, "{:?}", pins.pairs);
        assert_eq!(pins.pairs[0].2, 0, "压住 = 水平距离 0：{:?}", pins.pairs);
        assert!(pins.pairs[0].2 <= pins.tolerance(), "必须落在容差内");
    }

    #[test]
    fn five_pairs_come_back_sorted_by_target_x() {
        // 反例：按 y 排序或配对顺序取决于遍历顺序 → 顺序断言红。
        let pins = locate_pins(&pins_frame(
            &[140, 220, 300, 380, 460],
            &[150, 240, 280, 400, 450],
        ))
        .expect("5 对必须一次定位成功");
        assert_eq!(pins.pairs.len(), 5, "{:?}", pins.pairs);
        let centers: Vec<i32> = pins
            .pairs
            .iter()
            .map(|(target, _, _)| {
                let rect = pins.targets[*target];
                rect.x + rect.w / 2
            })
            .collect();
        assert!(
            centers.windows(2).all(|pair| pair[0] < pair[1]),
            "必须按目标 x 升序：{centers:?}"
        );
        let distances: Vec<i32> = pins.pairs.iter().map(|(_, _, distance)| *distance).collect();
        assert_eq!(distances, vec![10, 20, 20, 20, 10], "每对取最近的空闲游标");
    }

    #[test]
    fn a_track_without_cursors_is_ok_and_has_no_pairs() {
        // 零值边界：轨道在、目标也在，但游标还没出来 ⇒ **不能报错**（调用方要继续等）。
        // 反例：把「没有游标」当成 Err ⇒ 一开局就 Abort。
        let pins = locate_pins(&pins_frame(&[200, 400], &[])).expect("没游标不是定位失败");
        assert_eq!(pins.targets.len(), 2, "{:?}", pins.targets);
        assert!(pins.cursors.is_empty(), "{:?}", pins.cursors);
        assert!(pins.pairs.is_empty(), "配不上游标就不许编造配对：{:?}", pins.pairs);
    }

    #[test]
    fn an_all_grey_band_carries_no_bright_line_and_is_not_a_track() {
        // 亮度证据的零值：一条**全灰**的中灰横带（高度、宽度、行均值全都合格）不是轨道。
        // 反例：去掉 `PIN_TRACK_BRIGHT_MIN` ⇒ 这里返回 Ok（空盘面）⇒ 红。
        // 现实对应：室内黄色金属栏杆横杆与地面黄黑警示条都是这种「形态复制品」，
        // 亮像素占比实测 0.1%，而真面板轨道带 7.9%。
        let empty = locate_pins(&pins_frame(&[], &[]));
        assert_eq!(empty, Err(LocateError::Track), "全灰横带必须不算轨道");
    }

    #[test]
    fn a_dim_band_with_only_a_few_bright_pixels_is_not_a_track() {
        // 模拟黄栏杆：**暗黄**中灰横带（明度 91，正落在 `PIN_TRACK_LUMA` 55..110 里）
        // + 带内 1.4% 的亮像素（< `PIN_TRACK_BRIGHT_MIN` 5%）。
        // 那块亮东西宽 65px：超出目标方块的宽度上限（≤40），也没伸出带外 ⇒ 配不出任何一对。
        // 反例：不判亮度 ⇒ 返回 Ok（轨道在、零对）⇒ 红。
        //
        // 亮块必须是**窄而少**的：带内亮像素会把该行的均值顶过 `PIN_TRACK_LUMA` 上限，
        // 行判据先一步失配、整段被切碎 —— 那就测不到亮度判据了（这一条正是真面板
        // 亮轨道行进不了候选段的原因，见 `PIN_TRACK_BRIGHT_MIN` 注释）。
        //
        // 带宽与亮块亮度都必须按新判据标定：600px（93% 帧宽）会先被宽度上限拒掉，
        // 而 255 的亮块会把该行均值顶到 117（>110）⇒ 同样测不到亮度判据。
        let dim_band = PxRect {
            x: 100,
            y: 150,
            w: 400,
            h: 15,
        };
        let mut data = blank();
        paint(&mut data, dim_band, [120, 90, 20]);
        paint(
            &mut data,
            PxRect {
                x: 250,
                y: 156,
                w: 60,
                h: 2,
            },
            [200, 200, 200],
        );
        assert_eq!(locate_pins(&frame_of(data)), Err(LocateError::Track));
    }

    #[test]
    fn a_bright_track_with_white_pins_clears_the_bright_line_quota() {
        // 正例守卫（防阈值定得过严）：真面板量级的亮像素占比必须过闸。
        // 这份合成帧的带内占比 ≈7.5%，与实机真轨道 7.9% 同量级 —— 阈值 5% 留 1.5 倍余量。
        let pins = locate_pins(&pins_frame(&[200, 400], &[160, 430])).expect("真轨道必须过闸");
        assert_eq!(pins.track, TRACK, "{:?}", pins.track);
        assert_eq!(pins.pairs.len(), 2, "{:?}", pins.pairs);
    }

    #[test]
    fn a_bright_thin_line_is_not_a_track() {
        // 面板上边框：行均值同样落在 55..110、宽度也够，**只有 3px 的高度**把它和轨道分开。
        // 反例：去掉高度门槛 ⇒ 这帧会返回一个 3px 高的「轨道」，后面一切判据全错位。
        //
        // 带宽必须落在 30~75% 帧宽内、且带内自带亮线证据：否则它会先被「宽度上限」或
        // 「亮像素占比」拒掉，那这条用例就证不到高度判据了（当时是 520px 宽、带内全无亮像素）。
        let mut data = blank();
        paint(&mut data, THIN_LINE, [90, 90, 90]);
        paint(
            &mut data,
            PxRect {
                x: 250,
                y: 150,
                w: 30,
                h: 3,
            },
            WHITE,
        );
        assert_eq!(locate_pins(&frame_of(data)), Err(LocateError::Track));
    }

    /// 一条 400×3 的中灰细线（= 面板上边框的形态）。带内自带 30×3 白块，
    /// 于是「亮像素占比 7.5%」过闸，唯一不合格的只剩高度。
    const THIN_LINE: PxRect = PxRect {
        x: 100,
        y: 150,
        w: 400,
        h: 3,
    };

    #[test]
    fn a_gap_wider_than_the_seam_splits_the_track_into_too_narrow_pieces() {
        // 段切分必须按**暗缝**断：轨道中央的支架/缺口是真实存在的，断成两截各 120px
        // ⇒ 都不到帧宽 25% ⇒ 不是轨道。
        // 反例：段切分忽略暗缝（整行连成一片）⇒ 300px 的段合格 ⇒ 返回 Ok ⇒ 红。
        let mut data = blank();
        paint(
            &mut data,
            PxRect {
                x: 170,
                y: 150,
                w: 300,
                h: 15,
            },
            TRACK_GREY,
        );
        paint(
            &mut data,
            PxRect {
                x: 290,
                y: 150,
                w: 60,
                h: 15,
            },
            [0, 0, 0],
        );
        paint(
            &mut data,
            PxRect {
                x: 200,
                y: 149,
                w: 10,
                h: 17,
            },
            WHITE,
        );
        assert_eq!(locate_pins(&frame_of(data)), Err(LocateError::Track));
    }

    #[test]
    fn a_notch_narrower_than_the_seam_does_not_split_the_track() {
        // 轨道上的白色目标块会挖出窄暗洞（10px < 帧宽 2% = 13px）——那是物件，不是缝隙。
        // 反例：把每个暗像素都当断点 ⇒ 轨道被切成两截各 200px ⇒ 量回的宽度对不上 ⇒ 红。
        let mut data = blank();
        paint(&mut data, TRACK, TRACK_GREY);
        paint(
            &mut data,
            PxRect {
                x: 300,
                y: 150,
                w: 10,
                h: 15,
            },
            [0, 0, 0],
        );
        // 两个白块（带内亮像素 10% ⇒ 过得了 `PIN_TRACK_BRIGHT_MIN`）。
        for center_x in [200, 400] {
            paint(
                &mut data,
                PxRect {
                    x: center_x - 10,
                    y: 149,
                    w: 20,
                    h: 17,
                },
                WHITE,
            );
        }
        let pins = locate_pins(&frame_of(data)).expect("窄洞不算断");
        assert_eq!(pins.track, TRACK, "窄洞不许把轨道切碎：{:?}", pins.track);
    }

    #[test]
    fn a_band_whose_right_neighbour_is_bright_is_not_a_track() {
        // 轨道右侧 13px 外就是一块亮板（机箱灯箱/白墙）：这条带的右边**不是暗背景**，
        // 说明它没有被面板夹住 —— 与「整幅画面都很亮」的行段同类。
        // 反例：不做左右暗背景判据 ⇒ 这帧返回 Ok（400×15 的带全项合格）⇒ 红。
        let mut data = blank();
        paint(&mut data, BAND_WITH_BRIGHT_NEIGHBOUR, TRACK_GREY);
        paint(
            &mut data,
            PxRect {
                x: 453,
                y: 150,
                w: 48,
                h: 15,
            },
            [255, 255, 255],
        );
        paint(
            &mut data,
            PxRect {
                x: 200,
                y: 149,
                w: 30,
                h: 17,
            },
            WHITE,
        );
        assert_eq!(locate_pins(&frame_of(data)), Err(LocateError::Track));
    }

    /// 上面那条用例的横带：右侧紧跟 13px 暗缝（正好等于帧宽 2% ⇒ 段在此断开），
    /// 缝后再是亮板 —— 于是「右侧 19px 邻域」里亮像素占了 6px。
    const BAND_WITH_BRIGHT_NEIGHBOUR: PxRect = PxRect {
        x: 40,
        y: 150,
        w: 400,
        h: 15,
    };

    #[test]
    fn a_band_spanning_most_of_the_frame_is_not_a_track() {
        // 「一整帧都亮」的环境（地面+墙面连成一片）也能凑出 8~40px 高的亮段，
        // 但真轨道被面板夹在中间，量出来只有帧宽的 30~75%。
        // 反例：不设宽度上限 ⇒ 这帧返回 Ok（590px = 92% 帧宽）⇒ 红。
        let mut data = blank();
        paint(
            &mut data,
            PxRect {
                x: 20,
                y: 150,
                w: 590,
                h: 15,
            },
            TRACK_GREY,
        );
        paint(
            &mut data,
            PxRect {
                x: 300,
                y: 149,
                w: 40,
                h: 17,
            },
            WHITE,
        );
        assert_eq!(locate_pins(&frame_of(data)), Err(LocateError::Track));
    }
}

/// 真实帧回归：把用户实机截图的判定结论钉死（防止每隔几天又靠猜阈值调一轮）。
///
/// 夹具全部是 720p 判定世界的灰度 PNG（来源与处理口径见 `tests/fixtures/pins/README.md`），
/// 因此 `locate_pins` 读它们时不会再缩放一次 —— 测的就是实机看到的那张图。
#[cfg(test)]
mod pins_fixtures {
    use super::*;
    use std::time::Instant;

    /// 夹具登记表（名字 → 内嵌 PNG 字节）。新增真实帧只在这里加一行。
    const FIXTURES: [(&str, &[u8]); 3] = [
        (
            "real_panel",
            include_bytes!("../tests/fixtures/pins/real_panel.png"),
        ),
        (
            "yellow_railing",
            include_bytes!("../tests/fixtures/pins/yellow_railing.png"),
        ),
        (
            "bright_outdoor",
            include_bytes!("../tests/fixtures/pins/bright_outdoor.png"),
        ),
    ];

    /// 夹具 → 帧：灰度 PNG 解成灰度后按 BGRA 三通道同值铺开，
    /// 于是 `Frame::luma()` 逐像素还原成 PNG 里的灰度（不引入任何色彩转换偏差）。
    fn fixture_frame(name: &str) -> Frame {
        let png = FIXTURES
            .iter()
            .find(|(fixture, _)| *fixture == name)
            .map(|(_, bytes)| *bytes)
            .expect("夹具名字必须在 FIXTURES 里登记");
        let gray = GrayImage::from_png(png).expect("夹具 PNG 必须可解码");
        let data = gray
            .as_slice()
            .iter()
            .flat_map(|&value| [value, value, value, 255])
            .collect();
        Frame::new(gray.w, gray.h, data, Instant::now()).expect("夹具帧长度正确")
    }

    /// 生产判据的复刻：`looks_like_pins` 就是「定位成功 **且** 至少 2 对」。
    fn looks_like_pins(frame: &Frame) -> bool {
        locate_pins(frame).is_ok_and(|pins| pins.pairs.len() >= 2)
    }

    #[test]
    fn real_panel_is_located_as_a_pin_track() {
        // 正例守卫：收紧判据不许把真面板挡掉（这是本轮所有改动的红线）。
        //
        // 旧判据（v6.12~v6.14）按「**整行**非背景像素数 ≥ 25% 帧宽」找轨道行：这张真面板帧
        // 720 行里有 696 行都满足（整帧深色金属柜/地面都 ≥ `PIN_TRACK_FLOOR`、行均值落在
        // `PIN_TRACK_LUMA` 内、跨度铺满整幅画面），连续段高达 284 / 316 行，全部被
        // `PIN_TRACK_H(8..=40)` 毙掉 ⇒ `locate_pins` 恒 `Err(Track)`。真轨道（被面板边框
        // 夹住的那 640px）在整行统计里被两侧环境稀释成整帧跨度，从来不是独立候选段。
        // 现在改成「逐行找被暗背景夹住的亮段」，这条用例才第一次真正跑绿。
        let frame = fixture_frame("real_panel");
        let width = frame.w as i32;
        let pins = locate_pins(&frame).expect("真面板必须定位出轨道");
        assert!(
            pins.track.x > 0 && pins.track.right() < width,
            "真面板的轨道必须被面板边框夹住、不贴画面边缘：{:?}（帧宽 {width}）",
            pins.track
        );
        assert!(
            (0.30..=0.75).contains(&(pins.track.w as f32 / width as f32)),
            "轨道宽度必须落在帧宽 30%~75%：{:?}（帧宽 {width}）",
            pins.track
        );
        // 断言用**生产判据**（`pairs >= 2`，见 `scene/src/lib.rs::looks_like_pins`），
        // 不是「至少 1 对」：只剩 1 对时用户侧照样判不成任务保险，而夹具却是绿的。
        assert!(
            looks_like_pins(&frame),
            "真面板必须过生产判据（轨道 + ≥2 对）：{pins:?}"
        );
    }

    #[test]
    fn yellow_railing_is_not_a_pin_track() {
        // 误判①：黄色金属天桥/楼梯的亮黄栏杆在行统计里长得像一条中灰长轨道。
        let frame = fixture_frame("yellow_railing");
        assert_eq!(
            locate_pins(&frame),
            Err(LocateError::Track),
            "黄栏杆必须连轨道都找不到（不是靠配对数凑不够）"
        );
    }

    #[test]
    fn bright_outdoor_is_not_a_pin_track() {
        // 误判②：室外亮场景（天空 + 厂房 + 栏杆）从左缘铺到右缘，行段没有边界。
        let frame = fixture_frame("bright_outdoor");
        assert_eq!(
            locate_pins(&frame),
            Err(LocateError::Track),
            "亮室外场景必须连轨道都找不到（不是靠配对数凑不够）"
        );
        assert!(
            !looks_like_pins(&fixture_frame("bright_outdoor")),
            "更不许判成任务保险"
        );
    }
}

/// 滤镜适配（游戏加加提亮暗部）与「阈值放宽后不许增生」的真实帧回归。
///
/// 夹具 `keys_panel.png`（640×360、8 位灰度、100% 合成、不含任何本机信息）复刻真面板
/// （`task_1794777686_ok.png` 归一化到 1280×720 后）的相对结构：
/// - 台面 151 / 屏幕内腔 31 / 进度框细边 200 —— 与 `keys_fixture` 同量级；
/// - 16 颗键面 **92**（真机键面是 88..98 的渐变，所以阈值 95 下实测填充率 0.555；
///   这里取均匀值让判据行为可复现），中央 30×26 的亮图标 242
///   ⇒ 填充率 (44×40 − 30×26) / (44×40) = **0.557**，与实测同量级；
/// - 键阵右上角一块 **125** 的方形机件 —— 真面板把阈值放到 130 时多出来的那第 17 颗键
///   就是这种「亮一档的中灰块」。
///
/// 同一张图的实测（`uniform_blocks` 之后的键块数 / 全部暗块数）：
/// - 阈值 95 → **16 / 21**（21 = 16 颗键 + 5 个进度框内腔，后者被尺寸一致性滤掉）；
/// - gamma 0.85 后阈值 95 → **5 / 5**：16 颗键一颗不剩，只剩 5 个进度框内腔 ⇒ 凑不出 4×4
///   ⇒ `locate_keys` 报 `Err(Keys)`（= 用户报的「开滤镜就认不出黄键台」）；
/// - gamma 0.85 后阈值 120 → **16 / 21**（修好）；
/// - 阈值 130 → **17 / 22**：那块 125 的方形机件也开始算暗键 = 增生。
#[cfg(test)]
mod keys_panel_fixture {
    use super::*;
    use std::time::Instant;

    const KEYS_PANEL_PNG: &[u8] = include_bytes!("../tests/fixtures/keys/keys_panel.png");

    fn gray_png() -> GrayImage {
        GrayImage::from_png(KEYS_PANEL_PNG).expect("夹具 PNG 必须可解码")
    }

    /// 灰度图 → 帧：三通道同值铺开，于是 `Frame::luma()` 逐像素还原成 PNG 里的灰度。
    fn frame_of(gray: &GrayImage) -> Frame {
        let data = gray
            .as_slice()
            .iter()
            .flat_map(|&value| [value, value, value, 255])
            .collect();
        Frame::new(gray.w, gray.h, data, Instant::now()).expect("夹具帧长度正确")
    }

    /// 复刻滤镜的提亮效果：`v' = 255·(v/255)^γ`（γ < 1 ⇒ 暗部被提亮）。
    fn gamma(gray: &GrayImage, value: f32) -> GrayImage {
        let data = gray
            .as_slice()
            .iter()
            .map(|&luma| (255.0 * (f32::from(luma) / 255.0).powf(value)).round() as u8)
            .collect();
        GrayImage::new(gray.w, gray.h, data).expect("gamma 图尺寸不变")
    }

    #[test]
    fn the_key_panel_still_counts_sixteen_keys() {
        // H7：阈值放宽后不许增生。反例：`KEY_DARK_LUMA` 改到 130 ⇒ 那块 125 的方形机件
        // 也算暗键 ⇒ 17 块 ⇒ 红。
        let frame = frame_of(&gray_png());
        let keys_only = uniform_blocks(&dark_blocks(&frame.luma()));
        assert_eq!(
            keys_only.len(),
            KEY_GRID_SIDE * KEY_GRID_SIDE,
            "原图必须恰好定出 16 颗键（多一颗 = 阈值放宽过头，少一颗 = 键面判据太严）：{keys_only:?}"
        );
        let keys = locate_keys(&frame).expect("原图必须定位成功");
        assert_eq!(keys.cells.len(), KEY_GRID_SIDE * KEY_GRID_SIDE);
    }

    #[test]
    fn a_gamma_brightened_key_panel_is_still_located() {
        // H6：游戏加加滤镜把暗部提亮一档（gamma 0.85）后键面 92 → 107。
        // 阈值停在 95 时 107 不算暗键 ⇒ 一颗键都定不出来 ⇒ `locate_keys` 报 `Err(Keys)`，
        // 这正是用户「开滤镜就认不出黄键台、关了就好」的现场。
        // 反例：`KEY_DARK_LUMA` 保持 95 ⇒ 这里 0 颗键 ⇒ 红。
        let frame = frame_of(&gamma(&gray_png(), 0.85));
        let keys_only = uniform_blocks(&dark_blocks(&frame.luma()));
        assert!(
            keys_only.len() >= KEY_MIN_HITS,
            "滤镜后至少要定出 {KEY_MIN_HITS} 颗键（阈值 95 时 16 颗键全丢、只剩 5 个进度框内腔）：{keys_only:?}"
        );
        let keys = locate_keys(&frame).expect("滤镜后仍必须定位出键阵");
        assert_eq!(keys.cells.len(), KEY_GRID_SIDE * KEY_GRID_SIDE);
    }
}
