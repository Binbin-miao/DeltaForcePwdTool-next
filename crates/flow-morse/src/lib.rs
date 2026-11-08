//! 摩斯密码流程（spec §2.1 `core/morse.py:24-55`、§2.2 不变量 5/6/7）。
//!
//! 三段式：
//! 1. [`decode_gray`]：Otsu → 外轮廓 → `w*h < 10` 过滤 → `aspect > 2.0` 判划
//!    （**`== 2.0` 判点**）→ 按 x 升序拼串 → 查 10 位数字表；
//! 2. **三态错误**：`Empty`（无轮廓）/ `Unmapped`（长度或组合不在表内）/ `Error`（截图或算法失败）。
//!    对用户依旧显示 `?`，但返回值可区分、可记日志 —— 不许像上游那样全部压成 `"?"`；
//! 3. **非对称副作用**（spec §2.2 第 5 条，`morse.py:87-107`）：任一格失败时**仍然敲出已成功的
//!    数字键**，只跳过确认点击。

use std::fmt;

use delta_capture::{Capturer, Frame};
use delta_config::{PxView, Timings};
use delta_geom::{PxPoint, PxRect};
use delta_vision::{
    external_contours, GrayImage, Region, DASH_ASPECT_THRESHOLD, NOISE_AREA_GATE,
};
use thiserror::Error;

/// 数字 0-9 的标准摩斯码（与 `morse.py:11-22` 逐条一致）。
pub const MORSE_TABLE: [(&str, u8); 10] = [
    ("-----", 0),
    (".----", 1),
    ("..---", 2),
    ("...--", 3),
    ("....-", 4),
    (".....", 5),
    ("-....", 6),
    ("--...", 7),
    ("---..", 8),
    ("----.", 9),
];

/// 摩斯确认点击的点击间间隔（`morse.py:105` 的 `delay=0.1`）。
pub const MORSE_CONFIRM_CLICK_INTERVAL_MS: u64 = 100;

/// 一个摩斯符号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symbol {
    Dot,
    Dash,
}

impl Symbol {
    /// 拼串用的字符。
    pub fn as_char(self) -> char {
        match self {
            Symbol::Dot => '.',
            Symbol::Dash => '-',
        }
    }
}

/// 摩斯解码三态错误。**不得**再压成 `"?"`（spec §2.1）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MorseError {
    #[error("区域内没有可用轮廓（可能未对准或画面全黑）")]
    Empty,
    #[error("摩斯串 {pattern:?} 不在数字表内")]
    Unmapped { pattern: String },
    #[error("{reason}")]
    Error { reason: String },
}

impl MorseError {
    /// 三态分类名（日志用，便于区分上游被压平的 `"?"`）。
    pub fn kind(&self) -> &'static str {
        match self {
            MorseError::Empty => "Empty",
            MorseError::Unmapped { .. } => "Unmapped",
            MorseError::Error { .. } => "Error",
        }
    }
}

/// 轮廓 → 点/划。噪点（`w*h < 10`）返回 `None`。
///
/// **边界语义**：`aspect == 2.0` 判点，只有**严格大于** 2.0 才判划（spec 不变量 7）。
pub fn classify_region(region: Region) -> Option<Symbol> {
    if region.area() < NOISE_AREA_GATE {
        return None;
    }
    if region.aspect() > DASH_ASPECT_THRESHOLD {
        Some(Symbol::Dash)
    } else {
        Some(Symbol::Dot)
    }
}

/// 点划串 → 数字。
pub fn digit_for_pattern(pattern: &str) -> Option<u8> {
    MORSE_TABLE
        .iter()
        .find(|(symbols, _)| *symbols == pattern)
        .map(|(_, digit)| *digit)
}

/// 单格摩斯解码。
pub fn decode_gray(gray: &GrayImage) -> Result<u8, MorseError> {
    let binary = gray.otsu().binary;
    let mut symbols: Vec<(u32, Symbol)> = Vec::new();
    for contour in external_contours(&binary) {
        if let Some(symbol) = classify_region(contour.region) {
            symbols.push((contour.region.x, symbol));
        }
    }
    if symbols.is_empty() {
        return Err(MorseError::Empty);
    }
    // 按 x 升序拼接（`morse.py:49`）。
    symbols.sort_by_key(|(x, _)| *x);
    let pattern: String = symbols.iter().map(|(_, symbol)| symbol.as_char()).collect();
    digit_for_pattern(&pattern).ok_or(MorseError::Unmapped { pattern })
}

/// 截图来源（真机是 [`LiveScreen`]，测试是内存画面）。
pub trait ScreenSource {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String>;
}

/// 输入落地（真机是 [`LiveInput`]，测试是录音器）。
pub trait InputSink {
    fn press_digit(&mut self, digit: u8) -> Result<(), String>;
    fn click(&mut self, at: PxPoint) -> Result<(), String>;
    fn sleep_ms(&mut self, ms: u64);
}

/// 真机截图适配（复用 t2 的常驻 DC）。
pub struct LiveScreen<'a> {
    capturer: &'a mut Capturer,
}

impl<'a> LiveScreen<'a> {
    pub fn new(capturer: &'a mut Capturer) -> Self {
        Self { capturer }
    }
}

impl ScreenSource for LiveScreen<'_> {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
        self.capturer.grab(rect).map_err(|error| error.to_string())
    }
}

/// 真机输入适配（复用 t5 的 `SendInput`）。
pub struct LiveInput;

impl InputSink for LiveInput {
    fn press_digit(&mut self, digit: u8) -> Result<(), String> {
        let character = char::from(b'0' + digit);
        delta_input::key_digit(character).map_err(|error| error.to_string())
    }

    fn click(&mut self, at: PxPoint) -> Result<(), String> {
        delta_input::click(at).map_err(|error| error.to_string())
    }

    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// 单格解码结果（`digit` 与 `error` 互斥，`error` 保留三态原因供日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDecode {
    pub name: String,
    pub digit: Option<u8>,
    pub error: Option<MorseError>,
}

/// 确认点击未执行的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmSkipped {
    /// 有格子没识别出数字（spec 不变量 5）。
    DigitsFailed,
    /// `auto_click == false`。
    AutoClickDisabled,
    /// `confirm_clicks` 为空（spec 不变量 4）。
    NotConfigured,
}

impl ConfirmSkipped {
    pub fn reason(self) -> &'static str {
        match self {
            ConfirmSkipped::DigitsFailed => "存在识别失败的数字，跳过确认点击",
            ConfirmSkipped::AutoClickDisabled => "自动点击已禁用，请手动点击",
            ConfirmSkipped::NotConfigured => "未配置确认点击坐标",
        }
    }
}

/// 一次摩斯流程的完整留证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MorseOutcome {
    pub cells: Vec<CellDecode>,
    /// 给用户看的密码串（失败位为 `?`）。
    pub password: String,
    /// 实际敲出的数字键（非对称副作用留证）。
    pub pressed: Vec<u8>,
    pub confirm_clicked: bool,
    pub skipped: Option<ConfirmSkipped>,
    /// 输入注入失败等异常文本（失败必留证，spec D10）。
    pub errors: Vec<String>,
}

impl MorseOutcome {
    /// 三格是否都成功解出数字。
    pub fn digits_complete(&self) -> bool {
        !self.cells.is_empty() && self.cells.iter().all(|cell| cell.digit.is_some())
    }
}

/// 一次摩斯流程的入参。
pub struct MorseRun<'a> {
    pub view: &'a PxView,
    pub timings: &'a Timings,
    pub auto_click: bool,
}

/// 主流程：逐格截图 → 解码 → 敲键 → （全成功时）确认点击。
pub fn run<S: ScreenSource, I: InputSink>(
    source: &mut S,
    sink: &mut I,
    run: &MorseRun<'_>,
) -> MorseOutcome {
    let mut cells = Vec::with_capacity(run.view.regions.len());
    for region in &run.view.regions {
        let (digit, error) = match source.grab(region.rect) {
            Ok(frame) => match decode_gray(&frame.luma()) {
                Ok(digit) => (Some(digit), None),
                Err(error) => (None, Some(error)),
            },
            Err(reason) => (None, Some(MorseError::Error { reason })),
        };
        cells.push(CellDecode {
            name: region.name.clone(),
            digit,
            error,
        });
    }

    let password: String = cells
        .iter()
        .map(|cell| cell.digit.map_or('?', |digit| char::from(b'0' + digit)))
        .collect();

    // 非对称副作用：无论后面成败，已解出的数字都必须敲出去（`morse.py:89-95`）。
    let mut pressed = Vec::with_capacity(cells.len());
    let mut errors = Vec::new();
    for cell in &cells {
        if let Some(digit) = cell.digit {
            match sink.press_digit(digit) {
                Ok(()) => pressed.push(digit),
                Err(reason) => errors.push(format!("按下 {digit} 失败：{reason}")),
            }
        }
        sink.sleep_ms(run.timings.key_interval_ms);
    }

    let all_pressed = pressed.len() == cells.len() && !cells.is_empty();
    let (confirm_clicked, skipped) = if !run.auto_click {
        (false, Some(ConfirmSkipped::AutoClickDisabled))
    } else if !(all_pressed && cells.iter().all(|cell| cell.digit.is_some())) {
        (false, Some(ConfirmSkipped::DigitsFailed))
    } else if run.view.confirm_clicks.is_empty() {
        (false, Some(ConfirmSkipped::NotConfigured))
    } else {
        sink.sleep_ms(run.timings.morse_after_input_ms);
        for (index, point) in run.view.confirm_clicks.iter().enumerate() {
            if index > 0 {
                sink.sleep_ms(MORSE_CONFIRM_CLICK_INTERVAL_MS);
            }
            if let Err(reason) = sink.click(*point) {
                errors.push(format!("确认点击失败：{reason}"));
            }
        }
        (true, None)
    };

    MorseOutcome {
        cells,
        password,
        pressed,
        confirm_clicked,
        skipped,
        errors,
    }
}

impl fmt::Display for MorseOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "密码 {} → 已敲 {:?}", self.password, self.pressed)?;
        match self.skipped {
            Some(skipped) => write!(formatter, "（{}）", skipped.reason()),
            None => write!(formatter, "（已确认点击）"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_config::{PxConsole, PxFingerprint, PxRegion};
    use delta_geom::MonitorInfo;
    use std::collections::HashMap;
    use std::time::Instant;

    const W: u32 = 400;
    const H: u32 = 260;

    struct Canvas {
        data: Vec<u8>,
    }

    impl Canvas {
        fn new() -> Self {
            Self {
                data: vec![0; (W * H * 4) as usize],
            }
        }

        fn paint(&mut self, rect: PxRect) {
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    if x < 0 || y < 0 || x >= W as i32 || y >= H as i32 {
                        continue;
                    }
                    let index = ((y as u32 * W + x as u32) * 4) as usize;
                    self.data[index..index + 3].fill(255);
                    self.data[index + 3] = 255;
                }
            }
        }

        fn frame(self) -> Frame {
            Frame::new(W, H, self.data, Instant::now()).expect("合成帧长度正确")
        }
    }

    /// 按点划串画一格：`.` 是 12x12 方块，`-` 是 40x12 长条，间距 60px。
    fn morse_frame(pattern: &str) -> Frame {
        let mut canvas = Canvas::new();
        for (index, symbol) in pattern.chars().enumerate() {
            let left = 20 + index as i32 * 60;
            let width = if symbol == '-' { 40 } else { 12 };
            canvas.paint(PxRect {
                x: left,
                y: 100,
                w: width,
                h: 12,
            });
        }
        canvas.frame()
    }

    fn blank_frame() -> Frame {
        Canvas::new().frame()
    }

    /// 三格全空（用于 `Empty` 与「全失败」用例）。
    fn view_with(rects: [(i32, i32); 3], confirm: Vec<PxPoint>) -> PxView {
        PxView {
            monitor: MonitorInfo::new(W as i32, H as i32).expect("合法显示器"),
            regions: rects
                .iter()
                .enumerate()
                .map(|(index, (x, y))| PxRegion {
                    name: format!("morse_cell_{}", index + 1),
                    rect: PxRect {
                        x: *x,
                        y: *y,
                        w: 160,
                        h: 60,
                    },
                })
                .collect(),
            fingerprint: PxFingerprint {
                name_region: PxRect::ZERO,
                number_region: PxRect::ZERO,
                candidate_boxes: Vec::new(),
            },
            console: PxConsole {
                led_region: PxRect::ZERO,
                panel_region: PxRect::ZERO,
                col_x: Vec::new(),
                row_y: Vec::new(),
                knob_center: PxPoint { x: 0, y: 0 },
            },
            confirm_clicks: confirm,
        }
    }

    fn timings() -> Timings {
        Timings {
            key_interval_ms: 7,
            morse_after_input_ms: 11,
            ..Timings::default()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        Press(u8),
        Click(PxPoint),
        Sleep(u64),
    }

    struct Recorder {
        events: Vec<Event>,
        fail_press: bool,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                events: Vec::new(),
                fail_press: false,
            }
        }
    }

    impl InputSink for Recorder {
        fn press_digit(&mut self, digit: u8) -> Result<(), String> {
            if self.fail_press {
                return Err("UIPI 拦截".to_string());
            }
            self.events.push(Event::Press(digit));
            Ok(())
        }

        fn click(&mut self, at: PxPoint) -> Result<(), String> {
            self.events.push(Event::Click(at));
            Ok(())
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.events.push(Event::Sleep(ms));
        }
    }

    struct FakeScreen {
        frames: HashMap<(i32, i32), Frame>,
    }

    impl FakeScreen {
        fn new() -> Self {
            Self {
                frames: HashMap::new(),
            }
        }

        fn with(mut self, x: i32, y: i32, frame: Frame) -> Self {
            self.frames.insert((x, y), frame);
            self
        }
    }

    impl ScreenSource for FakeScreen {
        fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
            self.frames
                .get(&(rect.x, rect.y))
                .cloned()
                .ok_or_else(|| format!("区域 ({},{}) 没有画面", rect.x, rect.y))
        }
    }

    const CELLS: [(i32, i32); 3] = [(20, 80), (20, 140), (20, 200)];

    fn presses(events: &[Event]) -> Vec<u8> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Press(digit) => Some(*digit),
                _ => None,
            })
            .collect()
    }

    fn clicks(events: &[Event]) -> Vec<PxPoint> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Click(point) => Some(*point),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn morse_table_matches_upstream() {
        assert_eq!(MORSE_TABLE.len(), 10);
        assert_eq!(digit_for_pattern("-----"), Some(0));
        assert_eq!(digit_for_pattern("....-"), Some(4));
        assert_eq!(digit_for_pattern("----."), Some(9));
        assert_eq!(digit_for_pattern("......"), None, "长度不对必须查不到");
        assert_eq!(digit_for_pattern(""), None);
    }

    #[test]
    fn aspect_two_point_zero_is_a_dot() {
        // 20x10：aspect 恰为 2.0 → 点（严格大于才判划）。
        let exact = Region {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        };
        assert_eq!(exact.aspect(), 2.0);
        assert_eq!(classify_region(exact), Some(Symbol::Dot));

        let dash = Region {
            x: 0,
            y: 0,
            w: 21,
            h: 10,
        };
        assert_eq!(classify_region(dash), Some(Symbol::Dash));

        // 噪点门：3x3 = 9 < 10 → 丢弃。
        let noise = Region {
            x: 0,
            y: 0,
            w: 3,
            h: 3,
        };
        assert_eq!(classify_region(noise), None);
        let gate_edge = Region {
            x: 0,
            y: 0,
            w: 10,
            h: 1,
        };
        assert_eq!(classify_region(gate_edge), Some(Symbol::Dash), "area == 10 不得丢");
    }

    #[test]
    fn decode_reads_digits_left_to_right() {
        assert_eq!(decode_gray(&morse_frame("....-").luma()), Ok(4));
        assert_eq!(decode_gray(&morse_frame("..---").luma()), Ok(2));
        assert_eq!(decode_gray(&morse_frame("-----").luma()), Ok(0));
        // 顺序按 x，不按画图顺序。
        assert_eq!(decode_gray(&morse_frame("-....").luma()), Ok(6));
    }

    #[test]
    fn empty_contours_reports_empty_not_question_mark() {
        let error = decode_gray(&blank_frame().luma()).expect_err("空白格必须报错");
        assert_eq!(error, MorseError::Empty);
        assert_eq!(error.kind(), "Empty");
    }

    #[test]
    fn unknown_pattern_is_reported_as_unmapped() {
        // 单个点不成码，必须报 Unmapped 而不是静默返回 "?"。
        let error = decode_gray(&morse_frame(".").luma()).expect_err("单点不成码");
        assert_eq!(
            error,
            MorseError::Unmapped {
                pattern: ".".to_string()
            }
        );
        assert_eq!(error.kind(), "Unmapped");
    }

    #[test]
    fn full_success_presses_all_digits_then_confirms() {
        let confirm = vec![PxPoint { x: 300, y: 240 }, PxPoint { x: 320, y: 240 }];
        let view = view_with(CELLS, confirm.clone());
        let mut screen = FakeScreen::new()
            .with(20, 80, morse_frame("....-"))
            .with(20, 140, morse_frame("..---"))
            .with(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();

        let outcome = run(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                timings: &timings(),
                auto_click: true,
            },
        );

        assert_eq!(outcome.password, "427");
        assert_eq!(outcome.pressed, vec![4, 2, 7]);
        assert!(outcome.digits_complete());
        assert!(outcome.confirm_clicked);
        assert_eq!(outcome.skipped, None);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

        // 时序：每键后等 50ms（配置值），全成功 → 等 600ms（配置值）→ 逐个点击（间隔 100ms）。
        assert_eq!(
            sink.events,
            vec![
                Event::Press(4),
                Event::Sleep(7),
                Event::Press(2),
                Event::Sleep(7),
                Event::Press(7),
                Event::Sleep(7),
                Event::Sleep(11),
                Event::Click(PxPoint { x: 300, y: 240 }),
                Event::Sleep(MORSE_CONFIRM_CLICK_INTERVAL_MS),
                Event::Click(PxPoint { x: 320, y: 240 }),
            ]
        );
        assert_eq!(clicks(&sink.events), confirm);
    }

    #[test]
    fn partial_failure_still_presses_the_good_digits() {
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        // 中间格是空白 → Empty。
        let mut screen = FakeScreen::new()
            .with(20, 80, morse_frame("....-"))
            .with(20, 140, blank_frame())
            .with(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();

        let outcome = run(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                timings: &timings(),
                auto_click: true,
            },
        );

        assert_eq!(outcome.password, "4?7");
        assert_eq!(outcome.pressed, vec![4, 7], "非对称副作用：成功的仍要敲");
        assert!(!outcome.digits_complete());
        assert!(!outcome.confirm_clicked);
        assert_eq!(outcome.skipped, Some(ConfirmSkipped::DigitsFailed));
        assert!(clicks(&sink.events).is_empty(), "失败格必须跳过确认点击");
        assert_eq!(outcome.cells[1].error, Some(MorseError::Empty));
        assert_eq!(outcome.cells[1].error.as_ref().map(MorseError::kind), Some("Empty"));
    }

    #[test]
    fn all_failure_presses_nothing_and_skips_confirm() {
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = FakeScreen::new()
            .with(20, 80, blank_frame())
            .with(20, 140, blank_frame())
            .with(20, 200, blank_frame());
        let mut sink = Recorder::new();

        let outcome = run(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                timings: &timings(),
                auto_click: true,
            },
        );

        assert_eq!(outcome.password, "???");
        assert!(outcome.pressed.is_empty());
        assert!(presses(&sink.events).is_empty());
        assert!(!outcome.confirm_clicked);
        assert_eq!(outcome.skipped, Some(ConfirmSkipped::DigitsFailed));
    }

    #[test]
    fn capture_failure_is_reported_as_error_state() {
        // 没有任何画面 → grab 失败 → 三态里的 Error。
        let view = view_with(CELLS, Vec::new());
        let mut screen = FakeScreen::new();
        let mut sink = Recorder::new();

        let outcome = run(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                timings: &timings(),
                auto_click: true,
            },
        );

        assert_eq!(outcome.password, "???");
        assert_eq!(
            outcome.cells[0].error.as_ref().map(MorseError::kind),
            Some("Error")
        );
    }

    #[test]
    fn auto_click_disabled_skips_confirm_but_still_presses() {
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = FakeScreen::new()
            .with(20, 80, morse_frame("....-"))
            .with(20, 140, morse_frame("..---"))
            .with(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();

        let outcome = run(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                timings: &timings(),
                auto_click: false,
            },
        );

        assert_eq!(outcome.pressed, vec![4, 2, 7]);
        assert!(!outcome.confirm_clicked);
        assert_eq!(outcome.skipped, Some(ConfirmSkipped::AutoClickDisabled));
        assert!(clicks(&sink.events).is_empty());
    }

    #[test]
    fn empty_confirm_clicks_is_normal_path() {
        let view = view_with(CELLS, Vec::new());
        let mut screen = FakeScreen::new()
            .with(20, 80, morse_frame("....-"))
            .with(20, 140, morse_frame("..---"))
            .with(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();

        let outcome = run(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                timings: &timings(),
                auto_click: true,
            },
        );

        assert_eq!(outcome.pressed, vec![4, 2, 7]);
        assert!(!outcome.confirm_clicked);
        assert_eq!(outcome.skipped, Some(ConfirmSkipped::NotConfigured));
        assert!(
            !sink.events.contains(&Event::Sleep(11)),
            "没配确认点击就不该等那 600ms"
        );
    }

    #[test]
    fn input_failure_is_recorded_and_blocks_confirm() {
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = FakeScreen::new()
            .with(20, 80, morse_frame("....-"))
            .with(20, 140, morse_frame("..---"))
            .with(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();
        sink.fail_press = true;

        let outcome = run(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                timings: &timings(),
                auto_click: true,
            },
        );

        assert!(outcome.pressed.is_empty());
        assert_eq!(outcome.errors.len(), 3, "{:?}", outcome.errors);
        assert!(!outcome.confirm_clicked, "敲键失败时不得再点确认");
        assert_eq!(outcome.skipped, Some(ConfirmSkipped::DigitsFailed));
    }
}
