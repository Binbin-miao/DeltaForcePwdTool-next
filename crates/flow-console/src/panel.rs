//! 面板元素识别（彩色，spec §7.2 / §7.3 / §7.4）。
//!
//! 一次扫描同时产出三项观测，避免多次遍历彩色帧：
//! 1. **青色结构特征占比** —— 主完成判据（面板整体消失 → `< 8%`）；
//! 2. **米黄色块质心** —— 旋钮当前位置，映射为 `(列, 行)`；
//! 3. **黄黑斜纹浮层** —— 辅助判据（该列已被接受），先由亮黄像素包围盒定位候选，
//!    再用 [`delta_vision::detect_diagonal_stripe`] 验证方向性，避免把纯黄色块误判为浮层。
//!
//! 色域阈值全部来自 [`ConsoleParams`]（spec 标定值），本文件不含业务常数。

use delta_capture::Frame;
use delta_geom::{PxPoint, PxRect};
use delta_vision::{detect_diagonal_stripe, DiagonalStripeParams};

use crate::params::{rgb_to_hsv, ConsoleParams};

/// 米黄色块在面板内的质心（面板局部坐标，像素）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KnobBlock {
    pub cx: f32,
    pub cy: f32,
    pub pixels: u32,
}

/// 一帧面板观测。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelObservation {
    /// 青色结构特征像素占比（0..=100）。
    pub cyan_pct: f32,
    /// 米黄色块；未找到（例如被浮层遮住或面板已关闭）时为 `None`。
    pub knob: Option<KnobBlock>,
    /// 是否检测到黄黑斜纹浮层（= 该列已被接受）。
    pub overlay: bool,
}

/// 扫描面板帧，产出一次观测。
pub fn analyze_panel(frame: &Frame, params: &ConsoleParams) -> PanelObservation {
    let (width, height) = (frame.w as i32, frame.h as i32);
    let total = (width as u64) * (height as u64);
    if total == 0 {
        return PanelObservation {
            cyan_pct: 0.0,
            knob: None,
            overlay: false,
        };
    }

    let mut cyan = 0u64;
    let mut knob_sum = (0.0f64, 0.0f64);
    let mut knob_pixels = 0u32;
    // 亮黄像素的包围盒（先当作浮层候选，再由斜纹方向性验证）。
    let mut overlay_box: Option<(i32, i32, i32, i32)> = None;

    for y in 0..height {
        for x in 0..width {
            let [b, g, r] = frame.pixel(x as u32, y as u32);
            let color = rgb_to_hsv(r, g, b);
            if params.panel_hsv.contains(color) {
                cyan += 1;
            }
            if params.knob_hsv.contains(color) {
                knob_sum.0 += f64::from(x);
                knob_sum.1 += f64::from(y);
                knob_pixels += 1;
            }
            if params.overlay_hsv.contains(color) {
                overlay_box = Some(match overlay_box {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
    }

    let knob = (knob_pixels >= params.knob_min_pixels.max(1)).then(|| KnobBlock {
        cx: (knob_sum.0 / f64::from(knob_pixels)) as f32,
        cy: (knob_sum.1 / f64::from(knob_pixels)) as f32,
        pixels: knob_pixels,
    });

    PanelObservation {
        cyan_pct: cyan as f32 * 100.0 / total as f32,
        knob,
        overlay: detect_overlay(frame, params, overlay_box),
    }
}

/// 把旋钮块质心就近匹配到校准的 `col_x[4] × row_y[4]`，得到 `(列, 行)`。
///
/// 超出 [`ConsoleParams::grid_snap_max_px`] 的匹配视为无效（返回 `None`），
/// 避免把面板上其它米黄色元素误当成旋钮。
pub fn knob_to_grid(
    block: &KnobBlock,
    panel: PxRect,
    view: &delta_config::PxConsole,
    max_distance_px: i32,
) -> Option<(usize, usize)> {
    let absolute_x = block.cx + panel.x as f32;
    let absolute_y = block.cy + panel.y as f32;
    let column = nearest_axis(&view.col_x, absolute_x, max_distance_px)?;
    let row = nearest_axis(&view.row_y, absolute_y, max_distance_px)?;
    Some((column, row))
}

/// 旋钮块质心的绝对屏幕坐标（拖动的按下点）。
pub fn knob_center(block: &KnobBlock, panel: PxRect) -> PxPoint {
    PxPoint {
        x: panel.x.saturating_add(block.cx.round() as i32),
        y: panel.y.saturating_add(block.cy.round() as i32),
    }
}

fn nearest_axis(values: &[i32], target: f32, max_distance_px: i32) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (index, value) in values.iter().enumerate() {
        let distance = (*value as f32 - target).abs();
        if best.is_none_or(|(_, current)| distance < current) {
            best = Some((index, distance));
        }
    }
    let (index, distance) = best?;
    (distance <= max_distance_px as f32).then_some(index)
}

/// 验证浮层候选包围盒内确实存在 45° 黄黑斜纹。
///
/// 两道门：包围盒尺寸要够大（旋钮块只有 20×20，天然过不了），框内要真有斜纹方向性
/// （纯黄色块过不了）。因此「旋钮色域与浮层色域重叠」不会造成误判。
fn detect_overlay(
    frame: &Frame,
    params: &ConsoleParams,
    overlay_box: Option<(i32, i32, i32, i32)>,
) -> bool {
    let Some((x0, y0, x1, y1)) = overlay_box else {
        return false;
    };
    let box_w = (x1 - x0 + 1).max(0) as u32;
    let box_h = (y1 - y0 + 1).max(0) as u32;
    if box_w < params.overlay_min_w || box_h < params.overlay_min_h {
        return false;
    }
    let region = delta_vision::Region {
        x: x0.max(0) as u32,
        y: y0.max(0) as u32,
        w: box_w,
        h: box_h,
    };
    match frame.luma().crop(region) {
        Ok(cropped) => detect_diagonal_stripe(&cropped, &DiagonalStripeParams::default())
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::Canvas;

    fn params() -> ConsoleParams {
        ConsoleParams::default()
    }

    #[test]
    fn flat_cyan_panel_has_no_knob_and_no_overlay() {
        let frame = Canvas::filled(120, 90, [55, 95, 120]).into_frame();
        let observation = analyze_panel(&frame, &params());
        assert!(observation.cyan_pct > 90.0, "{observation:?}");
        assert!(observation.knob.is_none(), "纯色面板不该有旋钮块");
        assert!(!observation.overlay, "纯色面板不该有斜纹浮层");
    }

    #[test]
    fn knob_centroid_and_grid_snap() {
        let mut canvas = Canvas::filled(200, 120, [55, 95, 120]);
        canvas.rect(90, 40, 20, 20, [230, 200, 120]);
        let frame = canvas.into_frame();
        let block = analyze_panel(&frame, &params()).knob.expect("能找到米黄块");
        assert!((block.cx - 99.5).abs() < 1.0, "{block:?}");
        assert!((block.cy - 49.5).abs() < 1.0, "{block:?}");

        let panel = PxRect {
            x: 100,
            y: 100,
            w: 400,
            h: 500,
        };
        let mut view = delta_config::PxConsole {
            led_region: PxRect::ZERO,
            panel_region: panel,
            col_x: vec![160, 260, 360, 460],
            row_y: vec![240, 320, 400, 480],
            knob_center: PxPoint::ZERO,
        };
        // 质心 (99.5,49.5) + 面板原点 (100,100) → 绝对 (199.5,149.5)，就近匹配到 (0,0)。
        assert_eq!(knob_to_grid(&block, panel, &view, 160), Some((0, 0)));
        // 超出容差即拒绝匹配。
        assert_eq!(knob_to_grid(&block, panel, &view, 5), None);
        view.col_x = vec![160, 260];
        assert_eq!(knob_to_grid(&block, panel, &view, 160), Some((0, 0)));
    }

    #[test]
    fn diagonal_overlay_requires_stripes_not_just_yellow() {
        let mut solid = Canvas::filled(200, 120, [55, 95, 120]);
        // 纯亮黄矩形：色域命中但没有斜纹方向性 → 不得判为浮层。
        solid.rect(20, 10, 160, 40, [250, 220, 60]);
        assert!(
            !analyze_panel(&solid.into_frame(), &params()).overlay,
            "纯黄块不得判为黄黑斜纹浮层"
        );

        let mut striped = Canvas::filled(200, 120, [55, 95, 120]);
        for y in 10..50i32 {
            for x in 20..180i32 {
                let band = (x - y).div_euclid(8).rem_euclid(2);
                let rgb = if band == 0 { [250, 220, 60] } else { [20, 20, 20] };
                striped.set(x, y, rgb);
            }
        }
        assert!(
            analyze_panel(&striped.into_frame(), &params()).overlay,
            "黄黑 45° 斜纹必须判为浮层"
        );
    }

    #[test]
    fn knob_alone_is_not_mistaken_for_overlay() {
        // 旋钮块同时落在浮层色域里：只有它的时候必须否决。
        let mut canvas = Canvas::filled(200, 120, [55, 95, 120]);
        canvas.rect(90, 40, 20, 20, [230, 200, 120]);
        assert!(!analyze_panel(&canvas.into_frame(), &params()).overlay);
    }
}
