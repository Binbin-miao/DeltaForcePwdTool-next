//! 标注图：`auto_click == false` 时**只产出标注图，不点击**（spec §2.2 不变量 4）。
//!
//! 上游 `_do_annotated` 用 PIL 画框 + `msyh.ttc` 写字；这里不引字体（体积预算），
//! 改用 3×5 自绘数字位图，文字内容为 `候选号>模板号`（如 `3>2`），语义与上游一致。

use delta_capture::Frame;
use delta_geom::PxRect;

/// 命中格的颜色（上游 `(120, 255, 120)`）。
pub const MATCH_COLOR: [u8; 3] = [120, 255, 120];
/// 名字区的颜色（上游 `(255, 120, 120)`）。
pub const NAME_COLOR: [u8; 3] = [255, 120, 120];
/// 数字区的颜色（上游 `(120, 255, 255)`）。
pub const NUMBER_COLOR: [u8; 3] = [120, 255, 255];

const GLYPH_WIDTH: i32 = 3;
const GLYPH_HEIGHT: i32 = 5;
const GLYPH_ADVANCE: i32 = GLYPH_WIDTH + 1;

/// 就地画笔（直接改 BGRA）。
struct Painter<'a> {
    frame: &'a mut Frame,
}

impl<'a> Painter<'a> {
    fn new(frame: &'a mut Frame) -> Self {
        Self { frame }
    }

    fn set(&mut self, x: i32, y: i32, color: [u8; 3]) {
        if x < 0 || y < 0 || x >= self.frame.w as i32 || y >= self.frame.h as i32 {
            return;
        }
        let index = (y as u32 * self.frame.stride + x as u32 * 4) as usize;
        if index + 3 >= self.frame.data.len() {
            return;
        }
        self.frame.data[index] = color[2];
        self.frame.data[index + 1] = color[1];
        self.frame.data[index + 2] = color[0];
        self.frame.data[index + 3] = 255;
    }

    fn outline(&mut self, rect: PxRect, color: [u8; 3], thickness: i32) {
        let thickness = thickness.max(1);
        for offset in 0..thickness {
            for x in rect.x..rect.right() {
                self.set(x, rect.y + offset, color);
                self.set(x, rect.bottom() - 1 - offset, color);
            }
            for y in rect.y..rect.bottom() {
                self.set(rect.x + offset, y, color);
                self.set(rect.right() - 1 - offset, y, color);
            }
        }
    }

    /// 画 `候选号>模板号` 这样的短标签（数字 + `>`）。
    fn label(&mut self, x: i32, y: i32, text: &str, color: [u8; 3]) {
        for (index, character) in text.chars().enumerate() {
            let origin = x + index as i32 * GLYPH_ADVANCE;
            self.glyph(origin, y, character, color);
        }
    }

    fn glyph(&mut self, x: i32, y: i32, character: char, color: [u8; 3]) {
        let rows = glyph_rows(character);
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..GLYPH_WIDTH as usize {
                if bits & (1 << (GLYPH_WIDTH as usize - 1 - col)) != 0 {
                    self.set(x + col as i32, y + row as i32, color);
                }
            }
        }
    }
}

/// 3×5 位图字模（数字与 `>`），够画出 `候选X→模板Y`。
fn glyph_rows(character: char) -> [u8; GLYPH_HEIGHT as usize] {
    match character {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b010, 0b010, 0b010, 0b010],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '>' => [0b100, 0b010, 0b001, 0b010, 0b100],
        '?' => [0b111, 0b001, 0b011, 0b000, 0b010],
        _ => [0b111, 0b111, 0b111, 0b111, 0b111],
    }
}

/// 生成标注图：名字区/数字区各画一圈框，每个命中格画粗框并在左上角标 `候选号>模板号`。
pub fn annotate(
    frame: &Frame,
    matched: &[(PxRect, u32)],
    name_region: PxRect,
    number_region: PxRect,
) -> Frame {
    let mut canvas = frame.clone();
    let mut painter = Painter::new(&mut canvas);

    if name_region.w > 0 && name_region.h > 0 {
        painter.outline(name_region, NAME_COLOR, 1);
    }
    if number_region.w > 0 && number_region.h > 0 {
        painter.outline(number_region, NUMBER_COLOR, 1);
    }
    for (index, (rect, template_no)) in matched.iter().enumerate() {
        painter.outline(*rect, MATCH_COLOR, 3);
        let label = format!("{}>{}", index + 1, template_no);
        let label_y = if rect.y >= GLYPH_HEIGHT + 2 {
            rect.y - GLYPH_HEIGHT - 2
        } else {
            rect.y + 2
        };
        painter.label(rect.x + 2, label_y, &label, MATCH_COLOR);
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn blank(w: u32, h: u32) -> Frame {
        Frame::new(w, h, vec![0; (w * h * 4) as usize], Instant::now()).expect("合法帧")
    }

    fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
        let index = (y as usize * frame.stride as usize) + (x as usize) * 4;
        frame.data[index..index + 4].try_into().expect("4 字节")
    }

    #[test]
    fn annotation_draws_box_outline_and_leaves_other_pixels_alone() {
        let frame = blank(200, 120);
        let cell = PxRect {
            x: 20,
            y: 40,
            w: 30,
            h: 30,
        };
        let annotated = annotate(&frame, &[(cell, 2)], PxRect::ZERO, PxRect::ZERO);

        // 框线是绿色（BGRA 顺序：B=120, G=255, R=120）。
        assert_eq!(pixel(&annotated, 20, 40)[..3], MATCH_COLOR);
        // 框内中心不被涂黑改色（原图本来就是黑）。
        assert_eq!(pixel(&annotated, 35, 55), [0, 0, 0, 0]);
        // 远处像素不变。
        assert_eq!(pixel(&annotated, 190, 110), [0, 0, 0, 0]);
        // 标签画在框上方（y = 40-7 = 33 附近有像素）。
        let labelled = (33..40).any(|y| (20..40).any(|x| pixel(&annotated, x, y)[1] == 255));
        assert!(labelled, "标签必须画出来");
    }

    #[test]
    fn outline_is_clipped_inside_the_frame() {
        let frame = blank(40, 40);
        // 越界的框不得 panic。
        let out_of_bounds = PxRect {
            x: 30,
            y: 30,
            w: 40,
            h: 40,
        };
        let annotated = annotate(&frame, &[(out_of_bounds, 9)], PxRect::ZERO, PxRect::ZERO);
        assert_eq!(annotated.w, 40);
    }

    #[test]
    fn glyph_table_covers_every_digit() {
        for digit in '0'..='9' {
            assert_ne!(glyph_rows(digit), glyph_rows('?'), "数字 {digit} 必须有字模");
        }
        assert_ne!(glyph_rows('>'), glyph_rows('?'));
    }
}
