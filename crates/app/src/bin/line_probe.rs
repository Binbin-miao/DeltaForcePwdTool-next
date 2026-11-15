//! 信号线台（横线台）定位探针：把 `locate_line` **实际识别到**的每一块区域画成框，
//! 输出成 PNG，供人眼判断算法对不对。
//!
//! 只读诊断工具：不写配置、不改定位链路、不为「图好看」补任何坐标。
//! 定位失败时如实报出卡在哪一环并以退出码 2 结束 —— **失败本身就是要看的信息**。
//!
//! ```text
//! cargo run -p delta-app --bin line_probe -- <输入.png> [输出.png]
//! ```
//!
//! 图例：绿=screen、黄=LED 数字条、红实心圆=knob、青线=4 列 / 4 行、
//! 灰框=16 个档位格、**橙粗框=每列命中的那一行**（「程序认为横线现在在哪」的唯一证据）、
//! **蓝框=左侧行标签的读数窗口**（标签值 → 行索引的映射来源）。

use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use delta_capture::Frame;
use delta_geom::PxRect;
use delta_scene::locate::{
    diagnose_line_grid, locate_line, LocateError, LocatedLine, LINE_COLUMNS, LINE_ROWS,
};

/// 用法错误 / 读写失败。
const EXIT_IO: u8 = 1;
/// 定位失败（[`LocateError`]）。
const EXIT_LOCATE: u8 = 2;

/// 屏幕矩形：绿。
const SCREEN_COLOR: [u8; 3] = [0, 220, 0];
/// LED 数字条：黄。
const LED_COLOR: [u8; 3] = [255, 220, 0];
/// 旋钮中心：红。
const KNOB_COLOR: [u8; 3] = [235, 35, 35];
/// 4 列 / 4 行的轴线：青。
const AXIS_COLOR: [u8; 3] = [0, 220, 220];
/// 16 个档位格：灰。
const TARGET_COLOR: [u8; 3] = [140, 140, 140];
/// 每列命中的档位格：橙（唯一需要一眼命中的信息，故最粗）。
const SELECTED_COLOR: [u8; 3] = [255, 140, 0];
/// 每列检测到的**亮白信号线** y：品红（与青轴线区分开，一眼看出程序认的横线在哪）。
const WIRE_COLOR: [u8; 3] = [255, 0, 255];

/// 屏幕 / LED 框线宽。
const BOX_THICKNESS: i32 = 3;
/// 档位格框线宽。
const TARGET_THICKNESS: i32 = 1;
/// 列 / 行轴线宽。
const AXIS_THICKNESS: i32 = 1;
/// 命中格框线宽。
const SELECTED_THICKNESS: i32 = 4;
/// 品红横线线宽。
const WIRE_THICKNESS: i32 = 2;
/// 行标签读数窗口：蓝。
const LABEL_COLOR: [u8; 3] = [45, 120, 255];
/// 行标签框线宽。
const LABEL_THICKNESS: i32 = 2;
/// 旋钮标记半径。
const KNOB_RADIUS: i32 = 10;

/// 通道下标表：`(R, G, B, 每像素字节数)`，按通道数减一查表。
///
/// 灰度 / 灰度+Alpha 的 R=G=B；Alpha 一律填 255（定位链路只看 RGB）。
const CHANNELS: [(usize, usize, usize, usize); 4] = [
    (0, 0, 0, 1),
    (0, 0, 0, 2),
    (0, 1, 2, 3),
    (0, 1, 2, 4),
];

fn main() -> ExitCode {
    let Some((input, output)) = parse_args() else {
        eprintln!("用法: line_probe <输入.png> [输出.png]");
        return ExitCode::from(EXIT_IO);
    };

    let frame = match decode_frame(&input) {
        Ok(frame) => frame,
        Err(message) => {
            eprintln!("读取 {} 失败：{message}", input.display());
            return ExitCode::from(EXIT_IO);
        }
    };
    println!("输入: {} ({}x{})", input.display(), frame.w, frame.h);

    let located = match locate_line(&frame) {
        Ok(located) => located,
        Err(error) => {
            println!("定位失败: 卡在 {} —— {error}", stage_name(error));
            // 列/行没定出来时，把「各有多少条候选」也带出来 —— 只说「数不到 4 列」
            // 等于没说（`drag_row3` 就卡在这句没有信息量的文案上）。
            if matches!(
                error,
                LocateError::Columns | LocateError::GridColumns | LocateError::GridRows
            ) {
                report_grid(&frame);
            }
            return ExitCode::from(EXIT_LOCATE);
        }
    };

    report(&frame, &located);

    let canvas = draw_probe(&frame, &located);
    match canvas.save(&output) {
        Ok(()) => println!("已保存: {}", output.display()),
        Err(message) => {
            eprintln!("保存 {} 失败：{message}", output.display());
            return ExitCode::from(EXIT_IO);
        }
    }
    ExitCode::SUCCESS
}

/// 位置参数：`<输入> [输出]`，缺省输出为同目录的 `<输入名>_probe.png`。
fn parse_args() -> Option<(PathBuf, PathBuf)> {
    let mut args = std::env::args_os().skip(1);
    let input = PathBuf::from(args.next()?);
    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_output(&input));
    Some((input, output))
}

fn default_output(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "probe".to_owned());
    input.with_file_name(format!("{stem}_probe.png"))
}

/// [`LocateError`] 变体名：失败时直接告诉人「断在哪一步」。
fn stage_name(error: LocateError) -> &'static str {
    match error {
        LocateError::Frame { .. } => "Frame",
        LocateError::Knob => "Knob",
        LocateError::Screen => "Screen",
        LocateError::Led => "Led",
        LocateError::Columns => "Columns",
        LocateError::GridColumns => "GridColumns",
        LocateError::GridRows => "GridRows",
        LocateError::Keys => "Keys",
    }
}

/// 网格诊断：列/行各切出几条候选、`most_even` 选了哪 4 条。
fn report_grid(frame: &Frame) {
    match diagnose_line_grid(frame) {
        Ok(diagnosis) => {
            println!(
                "  列候选 {} 条: {:?}",
                diagnosis.column_candidates.len(),
                diagnosis.column_candidates
            );
            println!("  列选定: {:?}", diagnosis.columns);
            println!(
                "  行候选 {} 条: {:?}",
                diagnosis.row_candidates.len(),
                diagnosis.row_candidates
            );
            println!("  行选定: {:?}", diagnosis.rows);
        }
        Err(error) => println!("  网格诊断也失败: {} —— {error}", stage_name(error)),
    }
}

// ── 读图 ──

/// 解码 PNG 并组 BGRA 帧（与实机抓帧同口径）。
fn decode_frame(path: &Path) -> Result<Frame, String> {
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| error.to_string())?;
    let pixels = to_bgra(info.color_type, &buffer[..info.buffer_size()])?;
    Frame::new(info.width, info.height, pixels, Instant::now()).map_err(|error| error.to_string())
}

fn to_bgra(color: png::ColorType, data: &[u8]) -> Result<Vec<u8>, String> {
    let channels = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err("解码后仍是调色板格式".to_owned()),
    };
    let (r, g, b, step) = CHANNELS[channels - 1];
    if !data.len().is_multiple_of(step) {
        return Err(format!(
            "像素缓冲长度 {} 不是每像素 {step} 字节的整数倍",
            data.len()
        ));
    }
    let mut out = Vec::with_capacity(data.len() / step * 4);
    for chunk in data.chunks_exact(step) {
        out.extend_from_slice(&[chunk[b], chunk[g], chunk[r], 255]);
    }
    Ok(out)
}

// ── 打印 ──

fn report(frame: &Frame, located: &LocatedLine) {
    println!("screen:   {}", describe_rect(located.screen));
    println!("led:      {}", describe_rect(located.led));
    println!("knob:     x={} y={}", located.knob.x, located.knob.y);
    println!("columns:  {:?}", located.columns);
    println!("rows:     {:?}", located.rows);
    println!("selected: {:?}", located.selected);
    println!("wire_y:   {:?}", located.wire_y);
    println!(
        "row_labels: {:?}  ({:?})",
        located.row_labels,
        label_texts(&located.row_labels)
    );
    println!("命中行:   {:?}", row_labels(&located.selected));
    println!("targets:  {} 个格子", located.targets.len());
    match delta_flow_az3::line::read_led_sequence(frame, located.led) {
        Ok(sequence) => report_targets(&sequence, &located.row_labels),
        Err(message) => println!("警告: LED 数字条读不出目标序列 —— {message}"),
    }
}

/// 目标值 → 命中行：**必须走生产同一份映射**（`resolve_target_rows`），不许探针另写一套。
fn report_targets(sequence: &delta_flow_console::TargetSequence, labels: &[Option<u8>; LINE_ROWS]) {
    println!(
        "目标序列: {}",
        (0..LINE_COLUMNS)
            .map(|column| sequence.label_for(column).to_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );
    match delta_flow_az3::line::resolve_target_rows(labels, sequence) {
        Ok(rows) => println!(
            "目标值→命中行: {}",
            (0..LINE_COLUMNS)
                .map(|column| format!("{}→行{}", sequence.label_for(column), rows[column]))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Err(error) => println!("警告: 目标值无法映射到档位行 —— {error}（该列不许动手）"),
    }
}

/// 标签值 → 两位数文本（`2` → `10`），读不出显示 `--`。
fn label_texts(labels: &[Option<u8>; LINE_ROWS]) -> Vec<&'static str> {
    labels
        .iter()
        .map(|label| match label {
            Some(0) => "00",
            Some(1) => "01",
            Some(2) => "10",
            Some(3) => "11",
            _ => "--",
        })
        .collect()
}

/// 档位行标签（`10/11/01/00`）—— 直接用 flow-console 那一份，不另起一套。
fn row_labels(selected: &[Option<usize>]) -> Vec<Option<&'static str>> {
    selected
        .iter()
        .map(|row| row.and_then(|index| delta_flow_console::ROW_LABELS.get(index).copied()))
        .collect()
}

fn describe_rect(rect: PxRect) -> String {
    format!(
        "x={} y={} w={} h={} (右={} 下={})",
        rect.x,
        rect.y,
        rect.w,
        rect.h,
        rect.right(),
        rect.bottom()
    )
}

// ── 画图 ──

/// 图层顺序：灰格 → 青轴线 → 绿/黄实框 → 品红横线 → 橙命中框 → 红旋钮。
///
/// 命中框放倒数第二，保证它不会被其他图层压住 —— 人眼要一眼看到的就是它。
fn draw_probe(frame: &Frame, located: &LocatedLine) -> Canvas {
    let mut canvas = Canvas::from_frame(frame);
    for &target in &located.targets {
        canvas.stroke_rect(target, TARGET_COLOR, TARGET_THICKNESS);
    }
    for &column in &located.columns {
        canvas.vline(column, located.screen.y, located.screen.bottom(), AXIS_COLOR);
    }
    for &row in &located.rows {
        canvas.hline(located.screen.x, located.screen.right(), row, AXIS_COLOR);
    }
    for box_ in located.label_boxes.iter().flatten() {
        canvas.stroke_rect(*box_, LABEL_COLOR, LABEL_THICKNESS);
    }
    canvas.stroke_rect(located.screen, SCREEN_COLOR, BOX_THICKNESS);
    canvas.stroke_rect(located.led, LED_COLOR, BOX_THICKNESS);
    let half_pitch = column_half_pitch(&located.columns);
    for (&column, wire) in located.columns.iter().zip(located.wire_y) {
        let Some(y) = wire else {
            continue;
        };
        canvas.thick_hline(column - half_pitch, column + half_pitch, y, WIRE_THICKNESS, WIRE_COLOR);
    }
    for (column, selected) in located.selected.iter().enumerate() {
        let Some(cell) = selected.and_then(|row| target_of(located, column, row)) else {
            continue;
        };
        canvas.stroke_rect(cell, SELECTED_COLOR, SELECTED_THICKNESS);
    }
    canvas.disc(located.knob.x, located.knob.y, KNOB_RADIUS, KNOB_COLOR);
    canvas
}

/// 列窗半宽 = 相邻列中心最小间距的一半（与定位层同一口径，不越进邻居的档位条）。
fn column_half_pitch(columns: &[i32]) -> i32 {
    columns
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .min()
        .map_or(20, |pitch| (pitch / 2).max(1))
}

/// `targets` 是**列优先、行次之**（`targets[col * 4 + row]`）。
fn target_of(located: &LocatedLine, column: usize, row: usize) -> Option<PxRect> {
    let index = column.checked_mul(LINE_ROWS)?.checked_add(row)?;
    located.targets.get(index).copied()
}

/// RGBA 画布：落盘不再二次转换。
struct Canvas {
    w: u32,
    h: u32,
    data: Vec<u8>,
}

impl Canvas {
    /// 以帧为底（BGRA → RGBA），坐标保持原图口径。
    fn from_frame(frame: &Frame) -> Self {
        let mut data = Vec::with_capacity(frame.data.len());
        for pixel in frame.data.as_chunks::<4>().0 {
            data.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        Self {
            w: frame.w,
            h: frame.h,
            data,
        }
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let file = fs::File::create(path).map_err(|error| error.to_string())?;
        let mut encoder = png::Encoder::new(BufWriter::new(file), self.w, self.h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(&self.data)
            .map_err(|error| error.to_string())
    }

    /// 画布内像素下标；越界返回 `None`（**所有绘制都经由它裁掉画布外部分，绝不 panic**）。
    fn index_of(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return None;
        }
        Some((y as usize * self.w as usize + x as usize) * 4)
    }

    fn set(&mut self, x: i32, y: i32, color: [u8; 3]) {
        let Some(index) = self.index_of(x, y) else {
            return;
        };
        self.data[index] = color[0];
        self.data[index + 1] = color[1];
        self.data[index + 2] = color[2];
        self.data[index + 3] = 255;
    }

    /// 填充 `[x0, x1) × [y0, y1)`（右下开区间，越界自动裁掉）。
    fn fill_bar(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: [u8; 3]) {
        let Some((left, right)) = clamp_span(x0, x1, self.w) else {
            return;
        };
        let Some((top, bottom)) = clamp_span(y0, y1, self.h) else {
            return;
        };
        for y in top..bottom {
            for x in left..right {
                self.set(x, y, color);
            }
        }
    }

    /// 空心矩形（四条边各画一根粗 `thickness` 的实心条）。
    fn stroke_rect(&mut self, rect: PxRect, color: [u8; 3], thickness: i32) {
        let t = thickness.max(1);
        let (left, right) = (rect.x, rect.right());
        let (top, bottom) = (rect.y, rect.bottom());
        self.fill_bar(left, top, right, top + t, color);
        self.fill_bar(left, bottom - t, right, bottom, color);
        self.fill_bar(left, top, left + t, bottom, color);
        self.fill_bar(right - t, top, right, bottom, color);
    }

    fn hline(&mut self, x0: i32, x1: i32, y: i32, color: [u8; 3]) {
        self.thick_hline(x0, x1, y, AXIS_THICKNESS, color);
    }

    /// 以 `y` 为中线、指定粗细的横线。
    fn thick_hline(&mut self, x0: i32, x1: i32, y: i32, thickness: i32, color: [u8; 3]) {
        let t = thickness.max(1);
        let top = y - t / 2;
        self.fill_bar(x0, top, x1, top + t, color);
    }

    fn vline(&mut self, x: i32, y0: i32, y1: i32, color: [u8; 3]) {
        let t = AXIS_THICKNESS.max(1);
        let left = x - t / 2;
        self.fill_bar(left, y0, left + t, y1, color);
    }

    /// 实心圆（旋钮中心标记）。
    fn disc(&mut self, cx: i32, cy: i32, radius: i32, color: [u8; 3]) {
        let r = radius.max(1);
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    self.set(cx + dx, cy + dy, color);
                }
            }
        }
    }
}

/// 把 `[lo, hi)` 裁进 `[0, limit)`；裁空返回 `None`。
fn clamp_span(lo: i32, hi: i32, limit: u32) -> Option<(i32, i32)> {
    let limit = i32::try_from(limit).ok()?;
    let start = lo.max(0);
    let end = hi.min(limit);
    (end > start).then_some((start, end))
}
