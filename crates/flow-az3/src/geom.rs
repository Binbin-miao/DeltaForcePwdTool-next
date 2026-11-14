//! AZ3 四条流程共享的像素原语。
//!
//! 深模块：对外只有「裁剪窗口 / 占比 / 行投影 / 连通域」四件事，
//! 把越界裁剪、除零、连通域遍历全部收在内部。所有采样都限定在调用方给的矩形内，
//! 绝不整帧扫描（50ms 轮询下整帧扫描会拖垮帧率）。
//!
//! 颜色语义（`Frame::pixel` 返回 `[B, G, R]`）：
//! - [`is_green_glow`]：游戏里的亮绿色（高亮带、锁定条、成功圆钮）
//! - [`is_red_glow`]：整条高亮带变红（大保险按错的唯一信号）
//! - [`is_white`]：任务保险的白色游标与目标方块、信号线台的白竖线
//! - [`is_yellow_glow`]：黄键台待点按键的纯黄/琥珀高亮（排除已经变绿的完成态）

use delta_capture::Frame;
use delta_geom::PxRect;

/// 采样窗口：帧内坐标，右下开区间。裁空即 `is_empty()`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Clip {
    pub(crate) x0: u32,
    pub(crate) y0: u32,
    pub(crate) x1: u32,
    pub(crate) y1: u32,
}

impl Clip {
    pub(crate) fn w(self) -> u32 {
        self.x1.saturating_sub(self.x0)
    }

    pub(crate) fn h(self) -> u32 {
        self.y1.saturating_sub(self.y0)
    }

    pub(crate) fn is_empty(self) -> bool {
        self.w() == 0 || self.h() == 0
    }

    pub(crate) fn pixels(self) -> u32 {
        self.w() * self.h()
    }
}

/// 整帧窗口。
pub(crate) fn full(frame: &Frame) -> Clip {
    Clip {
        x0: 0,
        y0: 0,
        x1: frame.w,
        y1: frame.h,
    }
}

/// 把矩形裁进帧内；完全在帧外时返回零面积窗口（调用方按 `is_empty` 跳过）。
pub(crate) fn clip(frame: &Frame, rect: PxRect) -> Clip {
    let x0 = rect.x.max(0).min(frame.w as i32) as u32;
    let y0 = rect.y.max(0).min(frame.h as i32) as u32;
    let x1 = rect.right().max(0).min(frame.w as i32) as u32;
    let y1 = rect.bottom().max(0).min(frame.h as i32) as u32;
    Clip { x0, y0, x1, y1 }
}

/// 单行窗口（保留列范围，只换行区间）。
pub(crate) fn rows(outer: Clip, y0: u32, y1: u32) -> Clip {
    Clip {
        x0: outer.x0,
        y0: outer.y0 + y0,
        x1: outer.x1,
        y1: (outer.y0 + y1).min(outer.y1),
    }
}

pub(crate) fn is_green_glow([b, g, r]: [u8; 3]) -> bool {
    g > 120 && g > r.saturating_add(25) && g > b.saturating_add(25)
}

pub(crate) fn is_red_glow([b, g, r]: [u8; 3]) -> bool {
    r > 110 && r > g.saturating_add(40) && r > b.saturating_add(40)
}

pub(crate) fn is_white([b, g, r]: [u8; 3]) -> bool {
    r > 170 && g > 170 && b > 170
}

/// 纯黄/琥珀高亮（黄键台里**还没点**的亮键）。
///
/// 显式排除已激活变绿的按钮（[`is_check_green`]）：点亮的键会转成绿勾，
/// 若把绿也算成「还亮着」，ack 判据永远认为没吃下，同一颗键会被反复点。
///
/// `r + g > 3b` 必须在 **u16** 上比较：u8 里 `r+g` 会饱和到 255，只要 `b ≥ 86`
/// 两边就都是 255，亮黄键（240,200,105）会被判成「不是高亮」→ 亮键漏检。
/// `b` 的上限取 120 而非 115：米黄/琥珀旋钮实测 B≈120，115 会把它漏掉。
pub(crate) fn is_yellow_glow(color: [u8; 3]) -> bool {
    let [b, g, r] = color;
    !is_check_green(color)
        && r > 140
        && g > 120
        && b <= 120
        && (u16::from(r) + u16::from(g)) > u16::from(b) * 3
}

/// 绿 ✓（进度格「已完成」的标志）：绿分量明显压过红蓝。
///
/// 与 [`is_green_glow`] 的区别：✓ 是**实心亮绿字形**，比半透明绿罩更饱和、更亮，
/// 且不允许偏黄（黄键台的亮键是黄绿，不能算成勾选）。
pub(crate) fn is_check_green([b, g, r]: [u8; 3]) -> bool {
    g >= 120 && g >= r.saturating_add(30) && g >= b.saturating_add(30)
}

/// 满足谓词的像素数。
pub(crate) fn count_in(frame: &Frame, clip: Clip, pred: impl Fn([u8; 3]) -> bool) -> u32 {
    if clip.is_empty() {
        return 0;
    }
    let mut hits = 0u32;
    for y in clip.y0..clip.y1 {
        for x in clip.x0..clip.x1 {
            if pred(frame.pixel(x, y)) {
                hits += 1;
            }
        }
    }
    hits
}

/// 满足谓词的像素占比（0..=1）；空窗口返回 0，不产生 NaN。
pub(crate) fn ratio_in(frame: &Frame, clip: Clip, pred: impl Fn([u8; 3]) -> bool) -> f32 {
    let total = clip.pixels();
    if total == 0 {
        return 0.0;
    }
    count_in(frame, clip, pred) as f32 / total as f32
}

/// 逐行占比（下标 0 = `clip.y0`）。
pub(crate) fn row_ratios(
    frame: &Frame,
    clip: Clip,
    pred: impl Fn([u8; 3]) -> bool,
) -> Vec<f32> {
    if clip.is_empty() {
        return Vec::new();
    }
    (clip.y0..clip.y1)
        .map(|y| {
            let line = Clip {
                y1: y + 1,
                y0: y,
                ..clip
            };
            ratio_in(frame, line, &pred)
        })
        .collect()
}

/// 连通域（4 邻接）外接矩形；`min_pixels` 以下的噪点直接丢弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Blob {
    pub(crate) clip: Clip,
    pub(crate) pixels: u32,
}

impl Blob {
    pub(crate) fn center_x(self) -> i32 {
        (self.clip.x0 + self.clip.x1) as i32 / 2
    }
}

/// 在窗口内做连通域分析，返回面积达标的所有块（按面积降序，最大块在前）。
pub(crate) fn blobs(
    frame: &Frame,
    clip: Clip,
    pred: impl Fn([u8; 3]) -> bool,
    min_pixels: u32,
) -> Vec<Blob> {
    if clip.is_empty() {
        return Vec::new();
    }
    let w = clip.w();
    let h = clip.h();
    let mut visited = vec![false; (w * h) as usize];
    let mut found = Vec::new();
    let holds = |x: u32, y: u32| pred(frame.pixel(clip.x0 + x, clip.y0 + y));

    for y in 0..h {
        for x in 0..w {
            let index = (y * w + x) as usize;
            if visited[index] || !holds(x, y) {
                continue;
            }
            visited[index] = true;
            let mut stack = vec![(x, y)];
            let (mut left, mut right, mut top, mut bottom, mut pixels) = (x, x, y, y, 0u32);
            while let Some((cx, cy)) = stack.pop() {
                pixels += 1;
                left = left.min(cx);
                right = right.max(cx);
                top = top.min(cy);
                bottom = bottom.max(cy);
                for (nx, ny) in neighbours(cx, cy, w, h) {
                    let next = (ny * w + nx) as usize;
                    if visited[next] || !holds(nx, ny) {
                        continue;
                    }
                    visited[next] = true;
                    stack.push((nx, ny));
                }
            }
            if pixels >= min_pixels {
                found.push(Blob {
                    clip: Clip {
                        x0: clip.x0 + left,
                        y0: clip.y0 + top,
                        x1: clip.x0 + right + 1,
                        y1: clip.y0 + bottom + 1,
                    },
                    pixels,
                });
            }
        }
    }
    found.sort_by_key(|blob| std::cmp::Reverse(blob.pixels));
    found
}

fn neighbours(x: u32, y: u32, w: u32, h: u32) -> impl Iterator<Item = (u32, u32)> {
    let mut out = [(0u32, 0u32); 4];
    let mut len = 0usize;
    if x > 0 {
        out[len] = (x - 1, y);
        len += 1;
    }
    if x + 1 < w {
        out[len] = (x + 1, y);
        len += 1;
    }
    if y > 0 {
        out[len] = (x, y - 1);
        len += 1;
    }
    if y + 1 < h {
        out[len] = (x, y + 1);
        len += 1;
    }
    out.into_iter().take(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn paint(w: u32, h: u32, rects: &[(PxRect, [u8; 3])]) -> Frame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for (r, rgb) in rects {
            for y in r.y.max(0)..r.bottom().min(h as i32) {
                for x in r.x.max(0)..r.right().min(w as i32) {
                    let i = ((y as u32 * w + x as u32) * 4) as usize;
                    data[i] = rgb[2];
                    data[i + 1] = rgb[1];
                    data[i + 2] = rgb[0];
                    data[i + 3] = 255;
                }
            }
        }
        Frame::new(w, h, data, Instant::now()).expect("合法测试帧")
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> PxRect {
        PxRect { x, y, w, h }
    }

    #[test]
    fn empty_window_returns_zero_ratio_without_nan() {
        let frame = paint(40, 40, &[]);
        let outside = clip(&frame, rect(200, 200, 10, 10));
        assert!(outside.is_empty());
        assert_eq!(count_in(&frame, outside, is_green_glow), 0);
        assert_eq!(ratio_in(&frame, outside, is_green_glow), 0.0);
        assert!(row_ratios(&frame, outside, is_green_glow).is_empty());
        assert!(blobs(&frame, outside, is_green_glow, 1).is_empty());
    }

    #[test]
    fn blobs_are_sorted_by_area_and_include_geometry() {
        let frame = paint(
            40,
            40,
            &[
                (rect(2, 2, 4, 4), [60, 220, 60]),
                (rect(20, 20, 8, 8), [60, 220, 60]),
            ],
        );
        let found = blobs(&frame, full(&frame), is_green_glow, 4);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].pixels, 64);
        assert_eq!(found[0].center_x(), 24);
        assert_eq!(found[0].clip.w(), 8);
        assert_eq!(found[1].pixels, 16);
    }

    #[test]
    fn yellow_glow_distinguishes_pending_yellow_from_activated_green() {
        // 谓词入参是 [B, G, R]。
        // u8 里 r+g 会饱和到 255，亮黄键（240,200,105）以前判不出来。
        assert!(is_yellow_glow([105, 200, 240]), "亮黄键必须判成高亮");
        assert!(is_yellow_glow([120, 200, 230]), "米黄旋钮也会命中（靠暗键阵拦截）");
        assert!(!is_yellow_glow([255, 255, 255]), "纯白不是高亮");
        // 点完变绿的键：必须不再算「待点黄键」，但仍要能被绿勾判据认出来。
        assert!(!is_yellow_glow([60, 220, 60]), "已变绿的完成态不算待点黄键");
        assert!(is_check_green([60, 220, 60]), "已变绿按键必须命中绿勾判据");
    }

    #[test]
    fn one_pixel_noise_is_dropped_by_min_pixels() {
        let frame = paint(20, 20, &[(rect(5, 5, 1, 1), [60, 220, 60])]);
        assert!(blobs(&frame, full(&frame), is_green_glow, 12).is_empty());
        assert_eq!(blobs(&frame, full(&frame), is_green_glow, 1).len(), 1);
    }

    #[test]
    fn row_ratios_respect_window_offsets() {
        let frame = paint(20, 10, &[(rect(0, 4, 20, 2), [60, 220, 60])]);
        let all = full(&frame);
        let ratios = row_ratios(&frame, all, is_green_glow);
        assert_eq!(ratios.len(), 10);
        assert_eq!(ratios[3], 0.0);
        assert_eq!(ratios[4], 1.0);
        assert_eq!(ratios[5], 1.0);
        assert_eq!(ratios[6], 0.0);

        // 只取 4..6 这两行 → 满绿；只取 0..2 → 全黑。
        assert_eq!(ratio_in(&frame, rows(all, 4, 6), is_green_glow), 1.0);
        assert_eq!(ratio_in(&frame, rows(all, 0, 2), is_green_glow), 0.0);
    }
}
