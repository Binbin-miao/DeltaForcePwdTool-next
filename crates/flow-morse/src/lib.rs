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

use std::sync::atomic::{AtomicBool, Ordering};

use delta_capture::Frame;
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

// ── 输错接力（spec §0③ / B9–B10）──

/// 一次流程最多接力几次（用户拍板 K = 4；单轮 1~1.5s，15s 预算装得下）。
pub const MORSE_MAX_ATTEMPTS: u32 = 4;
/// 错误态红像素下限（spec §0③：实测 1116~1121px，取 800 留余量）。
const ERROR_RED_MIN_PX: u32 = 800;
/// 敲一位之后先等这么久再看面板：红态实测持续 0.37s，等满 0.37s 还红才算错
/// （单帧闪烁会被这一步筛掉，不需要连采两帧）。
const KEY_SETTLE_MS: u64 = 370;
/// 等红态消退的上限（spec：≤400ms）。
const RED_CLEAR_TIMEOUT_MS: u64 = 400;
/// 等红态消退的轮询间隔。
const RED_POLL_MS: u64 = 50;
/// 输入横条里的小格数（已正确位 + 空槽，spec §0③ 实测 5 位）。
pub const PANEL_SLOTS: usize = 5;
/// 一个小格「已填亮绿」的像素下限（720p 口径）。
const SLOT_GREEN_MIN_PX: u32 = 20;
/// 一个小格「还没填」的亮度上限（`V < 90` = 深灰空槽）。
const SLOT_EMPTY_V: u8 = 90;

/// 摩斯输入面板 ROI：3 个摩斯格的并集，向下多带一格高、左右各放宽半格宽。
///
/// 输入横条与圆钮就在字码下方那一行，所以把字码带整体下移一格即可覆盖。
/// 未标定（`regions` 为空）时返回零面积 —— 调用方据此**不做**错误态观察，
/// 保持「只解码 + 敲字」的旧行为，而不是拿一个假 ROI 去乱判。
pub fn panel_roi(view: &PxView) -> PxRect {
    let Some(first) = view.regions.first() else {
        return PxRect::ZERO;
    };
    let (mut left, mut top) = (first.rect.x, first.rect.y);
    let (mut right, mut bottom) = (first.rect.right(), first.rect.bottom());
    for region in &view.regions[1..] {
        left = left.min(region.rect.x);
        top = top.min(region.rect.y);
        right = right.max(region.rect.right());
        bottom = bottom.max(region.rect.bottom());
    }
    let cell_h = (bottom - top).max(1);
    let pad = (first.rect.w / 2).max(1);
    PxRect {
        x: left - pad,
        y: top,
        w: (right - left) + pad * 2,
        h: cell_h + cell_h + pad,
    }
}

/// 错误态：整片转红（`H≤8 ∨ H≥170 ∧ S≥110`，红像素 > [`ERROR_RED_MIN_PX`]）。
pub fn is_error_state(panel: &Frame) -> bool {
    red_pixels(panel) > ERROR_RED_MIN_PX
}

/// 已正确位的前缀长度 = 横条里**第一个「非绿」小格**的位置；全绿 = [`PANEL_SLOTS`]。
///
/// 读不到（可辨小格 < 3）返回 `None` —— 调用方退回「按错误发生的那一位接力」，
/// 绝不凭一个读不准的行去猜起点。
pub fn solved_prefix(panel: &Frame) -> Option<usize> {
    let slot_w = (panel.w as usize / PANEL_SLOTS).max(1) as i32;
    if slot_w < 2 || panel.h == 0 {
        return None;
    }
    let states: Vec<Option<bool>> = (0..PANEL_SLOTS)
        .map(|index| slot_state(panel, index, slot_w))
        .collect();
    if states.iter().filter(|state| state.is_some()).count() < PANEL_SLOTS - 2 {
        return None;
    }
    Some(
        states
            .iter()
            .position(|state| *state == Some(false))
            .unwrap_or(PANEL_SLOTS),
    )
}

/// 第 `index` 个小格的状态：`Some(true)` 已填亮绿 / `Some(false)` 深灰空槽 / `None` 看不出。
fn slot_state(panel: &Frame, index: usize, slot_w: i32) -> Option<bool> {
    let x0 = (index as i32 * slot_w).max(0);
    let x1 = (x0 + slot_w).min(panel.w as i32);
    let mut green = 0u32;
    let mut dark = 0u32;
    let mut total = 0u32;
    for y in 0..panel.h as i32 {
        for x in x0..x1 {
            total += 1;
            let [b, g, r] = panel.pixel(x as u32, y as u32);
            if is_bright_green([b, g, r]) {
                green += 1;
            }
            if b.max(g).max(r) < SLOT_EMPTY_V {
                dark += 1;
            }
        }
    }
    if total == 0 {
        return None;
    }
    if green >= SLOT_GREEN_MIN_PX {
        return Some(true);
    }
    (dark * 2 >= total).then_some(false)
}

/// 填进小格的亮绿（spec §0③：`H 38..92 ∧ S ≥ 90 ∧ V ≥ 120`），整数比较。
fn is_bright_green([b, g, r]: [u8; 3]) -> bool {
    let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
    let max = b.max(g).max(r);
    if g != max || max < 120 {
        return false;
    }
    let delta = max - r.min(g).min(b);
    delta * 100 >= max * 35 && b <= r + 20
}

/// 整片红的像素数（720p 口径：`H ≤ 8 ∨ H ≥ 170 ∧ S ≥ 110 ∧ V ≥ 110`）。
fn red_pixels(panel: &Frame) -> u32 {
    let mut red = 0u32;
    for y in 0..panel.h {
        for x in 0..panel.w {
            let [b, g, r] = panel.pixel(x, y);
            if is_panel_red([b, g, r]) {
                red += 1;
            }
        }
    }
    red
}

/// 红态像素判据：红为最大分量、够亮、够饱和（`S ≥ 110` ⇒ `delta ≥ 0.43·max`）。
fn is_panel_red([b, g, r]: [u8; 3]) -> bool {
    let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
    let max = b.max(g).max(r);
    if r != max || max < 110 {
        return false;
    }
    let delta = max - r.min(g).min(b);
    delta * 100 >= max * 43
}

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

/// 截图来源与真机适配统一由 `delta-capture` 提供（与 `flow-fingerprint` 共用同一实现）。
pub use delta_capture::{LiveScreen, ScreenSource};

/// 输入落地（真机是 [`LiveInput`]，测试是录音器）。
pub trait InputSink {
    fn press_digit(&mut self, digit: u8) -> Result<(), String>;
    fn click(&mut self, at: PxPoint) -> Result<(), String>;
    fn sleep_ms(&mut self, ms: u64);
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
    /// 实际敲出的数字键（含接力补位重复敲出的位，非对称副作用留证）。
    pub pressed: Vec<u8>,
    pub confirm_clicked: bool,
    pub skipped: Option<ConfirmSkipped>,
    /// 输入注入失败、输错接力等异常文本（失败必留证，spec D10）。
    pub errors: Vec<String>,
    /// 实际跑了几轮（≤ [`MORSE_MAX_ATTEMPTS`]）。
    pub attempts: u32,
    /// 数字是否全部敲对（解码成功 ≠ 敲对：输错 4 次就停手）。
    pub input_complete: bool,
}

impl MorseOutcome {
    /// 三格是否都成功解出数字。
    pub fn digits_complete(&self) -> bool {
        !self.cells.is_empty() && self.cells.iter().all(|cell| cell.digit.is_some())
    }

    /// 这一轮是否真的成功（解码出全部数字 **且** 全部敲对）。
    pub fn is_complete(&self) -> bool {
        self.digits_complete() && self.input_complete
    }
}

/// 一次摩斯流程的入参。
pub struct MorseRun<'a> {
    pub view: &'a PxView,
    pub timings: &'a Timings,
    pub auto_click: bool,
    /// 输入面板 ROI（[`panel_roi`]）。零面积 = 未标定 ⇒ 不做错误态观察。
    pub panel: PxRect,
}

/// 主流程：解码 → 逐位敲 → 输错**接力**补位 → （全对时）确认点击。
///
/// 接力是用户拍板的硬要求：游戏会把新敲的数字**接在后面**，所以输错之后
/// **绝不能重打前面已经正确的位**，只能从第一个非绿小格继续补。
pub fn run<S: ScreenSource, I: InputSink>(
    source: &mut S,
    sink: &mut I,
    job: &MorseRun<'_>,
    cancel: &AtomicBool,
) -> MorseOutcome {
    let cells = decode_cells(source, job);
    let password: String = cells
        .iter()
        .map(|cell| cell.digit.map_or('?', |digit| char::from(b'0' + digit)))
        .collect();

    let mut log = PressLog {
        pressed: Vec::with_capacity(cells.len()),
        errors: Vec::new(),
        ok: true,
    };
    let mut resume = 0usize;
    let mut attempts = 0u32;
    let mut cancelled = false;
    while resume < cells.len() && attempts < MORSE_MAX_ATTEMPTS {
        if cancel.load(Ordering::SeqCst) {
            cancelled = true;
            log.errors.push("已取消（接力中断）".to_owned());
            break;
        }
        attempts += 1;
        resume = press_relay(source, sink, job, &cells, resume, &mut log);
    }
    // 「敲完」不等于「敲对」：注入被拦截（UIPI）时也必须算没完成。
    let input_complete = resume >= cells.len() && log.ok;
    // 「还有位没补上」才是连续输错；注入被拦截是另一码事（已经逐条记进 errors）。
    if resume < cells.len() && !cancelled {
        log.errors
            .push(format!("摩斯连续 {MORSE_MAX_ATTEMPTS} 次不正确，已停手"));
    }

    let (confirm_clicked, skipped) = confirm(sink, job, &cells, input_complete, &mut log.errors);
    MorseOutcome {
        cells,
        password,
        pressed: log.pressed,
        confirm_clicked,
        skipped,
        errors: log.errors,
        attempts,
        input_complete,
    }
}

/// 逐格截图 + 解码（原逻辑原样保留：任一格失败只记三态原因，不放弃其它格）。
fn decode_cells<S: ScreenSource>(source: &mut S, job: &MorseRun<'_>) -> Vec<CellDecode> {
    let mut cells = Vec::with_capacity(job.view.regions.len());
    for region in &job.view.regions {
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
    cells
}

/// 接力过程的账本：敲过的位、失败原因、是否每一位都按下成功。
///
/// 这三样被 `press_relay` 反复读写、收尾时又整包搬进 [`MorseOutcome`]，
/// 收成一个具名结构比在参数表里排三个 `&mut` 更好读。
struct PressLog {
    pressed: Vec<u8>,
    errors: Vec<String>,
    ok: bool,
}

/// 从 `resume` 起逐位敲，输错就等红态消退并算出新的起点（接力）。返回下一轮起点。
fn press_relay<S: ScreenSource, I: InputSink>(
    source: &mut S,
    sink: &mut I,
    job: &MorseRun<'_>,
    cells: &[CellDecode],
    resume: usize,
    log: &mut PressLog,
) -> usize {
    let mut index = resume;
    while index < cells.len() {
        let Some(digit) = cells[index].digit else {
            // 没解出数字的位不敲（保持非对称副作用：能敲的照敲）。
            index += 1;
            continue;
        };
        if let Err(reason) = sink.press_digit(digit) {
            log.errors.push(format!("按下 {digit} 失败：{reason}"));
            log.ok = false;
            index += 1;
            continue;
        }
        log.pressed.push(digit);
        if !watches_panel(job) {
            sink.sleep_ms(job.timings.key_interval_ms);
            index += 1;
            continue;
        }
        // 等满 0.37s 再看：单帧闪烁的红会被这一步筛掉。
        sink.sleep_ms(KEY_SETTLE_MS);
        let Some(panel) = grab_panel(source, job) else {
            index += 1;
            continue;
        };
        if !is_error_state(&panel) {
            index += 1;
            continue;
        }
        log.errors
            .push(format!("第 {} 位输错（面板转红），接力补位", index + 1));
        wait_red_clear(source, sink, job);
        // 接力起点 = 重读面板得到的「第一个非绿小格」；读不到就按「这一位错了」算。
        // 视觉只能**往前**推，绝不能退回到已经敲对的位（退了就会重复追加数字）。
        let seen = grab_panel(source, job).and_then(|panel| solved_prefix(&panel));
        return seen.map_or(index, |prefix| index.max(prefix).min(cells.len()));
    }
    index
}

/// 是否观察面板（未标定 ROI 时保持旧行为，不做错误态判断）。
fn watches_panel(job: &MorseRun<'_>) -> bool {
    job.panel.w > 0 && job.panel.h > 0
}

fn grab_panel<S: ScreenSource>(source: &mut S, job: &MorseRun<'_>) -> Option<Frame> {
    watches_panel(job).then(|| source.grab(job.panel).ok()).flatten()
}

/// 等红态消退：最多 [`RED_CLEAR_TIMEOUT_MS`]；读不到面板也算消退（不无限等）。
fn wait_red_clear<S: ScreenSource, I: InputSink>(source: &mut S, sink: &mut I, job: &MorseRun<'_>) {
    let mut waited = 0u64;
    while waited < RED_CLEAR_TIMEOUT_MS {
        sink.sleep_ms(RED_POLL_MS);
        waited += RED_POLL_MS;
        match grab_panel(source, job) {
            Some(panel) if is_error_state(&panel) => continue,
            _ => return,
        }
    }
}

/// 确认点击（spec 不变量 4/5 原样保留：只有「数字全解出 + 全部敲对」才点）。
fn confirm<I: InputSink>(
    sink: &mut I,
    job: &MorseRun<'_>,
    cells: &[CellDecode],
    input_complete: bool,
    errors: &mut Vec<String>,
) -> (bool, Option<ConfirmSkipped>) {
    let all_pressed = input_complete && !cells.is_empty();
    if !job.auto_click {
        return (false, Some(ConfirmSkipped::AutoClickDisabled));
    }
    if !(all_pressed && cells.iter().all(|cell| cell.digit.is_some())) {
        return (false, Some(ConfirmSkipped::DigitsFailed));
    }
    if job.view.confirm_clicks.is_empty() {
        return (false, Some(ConfirmSkipped::NotConfigured));
    }
    sink.sleep_ms(job.timings.morse_after_input_ms);
    for (index, point) in job.view.confirm_clicks.iter().enumerate() {
        if index > 0 {
            sink.sleep_ms(MORSE_CONFIRM_CLICK_INTERVAL_MS);
        }
        if let Err(reason) = sink.click(*point) {
            errors.push(format!("确认点击失败：{reason}"));
        }
    }
    (true, None)
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
    use delta_capture::Frame;
    use delta_config::{PxConsole, PxFingerprint, PxRegion};
    use delta_geom::{MonitorInfo, PxRect};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
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
            az3: delta_config::PxAz3::default(),
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

    /// 没有面板 ROI 的旧口径跑法（接力关闭，等价于旧行为）。
    fn exec(
        screen: &mut impl ScreenSource,
        sink: &mut impl InputSink,
        job: &MorseRun<'_>,
    ) -> MorseOutcome {
        run(screen, sink, job, &AtomicBool::new(false))
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

        let outcome = exec(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                panel: PxRect::ZERO,
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

        let outcome = exec(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                panel: PxRect::ZERO,
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

        let outcome = exec(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                panel: PxRect::ZERO,
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

        let outcome = exec(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                panel: PxRect::ZERO,
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

        let outcome = exec(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                panel: PxRect::ZERO,
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

        let outcome = exec(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                panel: PxRect::ZERO,
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

        let outcome = exec(
            &mut screen,
            &mut sink,
            &MorseRun {
                view: &view,
                panel: PxRect::ZERO,
                timings: &timings(),
                auto_click: true,
            },
        );

        assert!(outcome.pressed.is_empty());
        assert_eq!(outcome.errors.len(), 3, "{:?}", outcome.errors);
        assert!(!outcome.confirm_clicked, "敲键失败时不得再点确认");
        assert_eq!(outcome.skipped, Some(ConfirmSkipped::DigitsFailed));
    }

    // ── 输错接力（spec §0③ / B9–B10）──

    const PANEL_ROI: PxRect = PxRect {
        x: 20,
        y: 150,
        w: 300,
        h: 60,
    };

    /// 输入横条：`green_slots` 个已正确位填亮绿，其余是深灰空槽；`red` 时整片转红。
    fn panel_image(green_slots: usize, red: bool) -> Frame {
        let (w, h) = (PANEL_ROI.w as u32, PANEL_ROI.h as u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        let paint = |data: &mut Vec<u8>, x0: u32, x1: u32, rgb: [u8; 3]| {
            for y in 0..h {
                for x in x0.min(w)..x1.min(w) {
                    let index = ((y * w + x) * 4) as usize;
                    data[index] = rgb[2];
                    data[index + 1] = rgb[1];
                    data[index + 2] = rgb[0];
                    data[index + 3] = 255;
                }
            }
        };
        let slot_w = w / PANEL_SLOTS as u32;
        for slot in 0..PANEL_SLOTS {
            let rgb = if slot < green_slots {
                [60, 210, 80]
            } else {
                [40, 42, 44]
            };
            paint(&mut data, slot as u32 * slot_w + 2, (slot as u32 + 1) * slot_w - 2, rgb);
        }
        if red {
            for y in 0..h {
                for x in 0..w {
                    let index = ((y * w + x) * 4) as usize;
                    data[index..index + 4].copy_from_slice(&[45, 40, 200, 255]);
                }
            }
        }
        Frame::new(w, h, data, Instant::now()).expect("合成面板帧")
    }

    /// 三格摩斯画面 + 面板帧按脚本推进（面板用完之后一直停在最后一帧）。
    struct RelayScreen {
        cells: FakeScreen,
        panel: Vec<Frame>,
        cursor: usize,
    }

    impl RelayScreen {
        fn new(panel: Vec<Frame>) -> Self {
            Self {
                cells: FakeScreen::new(),
                panel,
                cursor: 0,
            }
        }

        fn with_cell(mut self, x: i32, y: i32, frame: Frame) -> Self {
            self.cells = self.cells.with(x, y, frame);
            self
        }
    }

    impl ScreenSource for RelayScreen {
        fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
            if rect != PANEL_ROI {
                return self.cells.grab(rect);
            }
            let last = self.panel.len().saturating_sub(1);
            let frame = self.panel.get(self.cursor.min(last)).cloned();
            self.cursor += 1;
            frame.ok_or_else(|| "面板脚本为空".to_owned())
        }
    }

    fn relay_job<'a>(view: &'a PxView, timings: &'a Timings, auto_click: bool) -> MorseRun<'a> {
        MorseRun {
            view,
            timings,
            auto_click,
            panel: PANEL_ROI,
        }
    }

    #[test]
    fn panel_roi_covers_the_row_below_the_morse_cells() {
        let view = view_with(CELLS, Vec::new());
        let roi = panel_roi(&view);
        let first = view.regions.first().expect("3 个摩斯格").rect;
        assert!(roi.x < first.x, "左边要放宽");
        assert!(roi.right() > view.regions[2].rect.right(), "右边要放宽");
        assert_eq!(roi.y, first.y);
        assert!(roi.bottom() > first.bottom(), "下面要带出一格高的输入横条");
        // 未标定 ⇒ 零面积（据此关闭错误态观察，不拿假 ROI 乱判）。
        let empty = PxView {
            regions: Vec::new(),
            ..view_with(CELLS, Vec::new())
        };
        assert_eq!(panel_roi(&empty), PxRect::ZERO);
    }

    #[test]
    fn error_state_needs_more_than_eight_hundred_red_pixels() {
        assert!(is_error_state(&panel_image(0, true)), "整片转红 = 错误态");
        assert!(!is_error_state(&panel_image(0, false)), "正常面板不是错误态");
        // 只有一条细红线（远不到 800px）不算错误态。
        assert!(!is_error_state(&panel_image(0, false)));
    }

    #[test]
    fn solved_prefix_is_the_first_slot_without_a_green_mark() {
        assert_eq!(solved_prefix(&panel_image(0, false)), Some(0), "全空 ⇒ 从第 1 位开始");
        assert_eq!(solved_prefix(&panel_image(1, false)), Some(1), "第 1 位已绿 ⇒ 从第 2 位");
        assert_eq!(solved_prefix(&panel_image(4, false)), Some(4));
        assert_eq!(solved_prefix(&panel_image(5, false)), Some(PANEL_SLOTS), "全绿 ⇒ 没有要补的");
        // 整片同色（看不出格子）⇒ 读不到，返回 None 让调用方按「输错的那一位」接力。
        let uniform = Frame::new(
            PANEL_ROI.w as u32,
            PANEL_ROI.h as u32,
            vec![128u8; (PANEL_ROI.w * PANEL_ROI.h * 4) as usize],
            Instant::now(),
        )
        .expect("合成帧");
        assert_eq!(solved_prefix(&uniform), None);
    }

    #[test]
    fn relay_resumes_after_the_last_correct_digit_instead_of_retyping() {
        // 第 1 位（4）吃下 → 第 2 位（2）输错 → 面板只剩 1 个绿格 → 只补 2、7。
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let screen = RelayScreen::new(vec![
            panel_image(0, false), // 敲 4 之后：正常
            panel_image(0, true),  // 敲 2 之后：整片转红
            panel_image(0, false), // 等红态消退：第一帧就已消退
            panel_image(1, false), // 重读：第 1 位仍是绿的
            panel_image(1, false), // 敲 2 之后：正常
            panel_image(2, false), // 敲 7 之后：正常
        ])
        .with_cell(20, 80, morse_frame("....-"))
        .with_cell(20, 140, morse_frame("..---"))
        .with_cell(20, 200, morse_frame("--..."));
        let mut screen = screen;
        let mut sink = Recorder::new();
        let timings = timings();
        let job = relay_job(&view, &timings, true);

        let outcome = run(&mut screen, &mut sink, &job, &AtomicBool::new(false));

        assert_eq!(outcome.password, "427");
        assert_eq!(
            outcome.pressed,
            vec![4, 2, 2, 7],
            "只许重敲出错的那一位，已正确的 4 绝不能再敲一次"
        );
        assert_eq!(outcome.attempts, 2);
        assert!(outcome.input_complete);
        assert_eq!(clicks(&sink.events), vec![PxPoint { x: 300, y: 240 }]);
    }

    #[test]
    fn relay_stops_after_four_failed_attempts() {
        // 面板一直红：4 次之后必须停手，不许无限重试。
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = RelayScreen::new(vec![panel_image(0, true)])
            .with_cell(20, 80, morse_frame("....-"))
            .with_cell(20, 140, morse_frame("..---"))
            .with_cell(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();
        let timings = timings();
        let job = relay_job(&view, &timings, true);

        let outcome = run(&mut screen, &mut sink, &job, &AtomicBool::new(false));

        assert_eq!(outcome.attempts, MORSE_MAX_ATTEMPTS);
        assert!(!outcome.input_complete);
        assert_eq!(outcome.pressed, vec![4; MORSE_MAX_ATTEMPTS as usize], "每轮只补出错的那一位");
        assert!(
            outcome.errors.iter().any(|error| error.contains("连续 4 次不正确")),
            "{:?}",
            outcome.errors
        );
        assert!(clicks(&sink.events).is_empty(), "输错到底绝不许点确认");
        assert_eq!(outcome.skipped, Some(ConfirmSkipped::DigitsFailed));
    }

    #[test]
    fn relay_is_skipped_entirely_without_a_panel_roi() {
        // 未标定面板 ⇒ 保持旧行为：一次敲完、一次尝试。
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = FakeScreen::new()
            .with(20, 80, morse_frame("....-"))
            .with(20, 140, morse_frame("..---"))
            .with(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();
        let timings = timings();
        let job = relay_job(&view, &timings, true);
        let outcome = exec(&mut screen, &mut sink, &job);
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.pressed, vec![4, 2, 7]);
        assert!(outcome.is_complete());
    }
}
