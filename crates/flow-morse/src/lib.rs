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
// 这里原本有个 `KEY_SETTLE_MS = 370`：每敲一位都先硬等 0.37s 再看面板，
// 三位就是 1.1s 的固定延迟（用户反馈「摩斯变慢了」的直接原因）。
// 现在正常路径只用 `timings.key_interval_ms`（50ms），只有真的读到红态才进恢复流程。
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
/// 只要有一格读不准就返回 `None` —— 调用方据此**停手**，绝不拿「其余格都绿」
/// 去推定一个满前缀（未知格跳过会让满前缀伪成立，进而误判成敲完了）。
pub fn solved_prefix(panel: &Frame) -> Option<usize> {
    let slot_w = (panel.w as usize / PANEL_SLOTS).max(1) as i32;
    if slot_w < 2 || panel.h == 0 {
        return None;
    }
    let states: Vec<Option<bool>> = (0..PANEL_SLOTS)
        .map(|index| slot_state(panel, index, slot_w))
        .collect();
    if states.iter().any(|state| state.is_none()) {
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
    /// 确认点击被输入层拒绝（UIPI 等）：点了但没生效。
    Failed,
    /// 确认点击过程中被取消。
    Canceled,
}

impl ConfirmSkipped {
    pub fn reason(self) -> &'static str {
        match self {
            ConfirmSkipped::DigitsFailed => "存在识别失败的数字，跳过确认点击",
            ConfirmSkipped::AutoClickDisabled => "自动点击已禁用，请手动点击",
            ConfirmSkipped::NotConfigured => "未配置确认点击坐标",
            ConfirmSkipped::Failed => "确认点击失败，可能未生效",
            ConfirmSkipped::Canceled => "已取消，确认点击未完成",
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
    /// 确认点击是否真的失败过（点了但注入被拒/中途取消）。`true` 时不得算成功。
    pub confirm_failed: bool,
}

impl MorseOutcome {
    /// 三格是否都成功解出数字。
    pub fn digits_complete(&self) -> bool {
        !self.cells.is_empty() && self.cells.iter().all(|cell| cell.digit.is_some())
    }

    /// 这一轮是否真的成功（解码出全部数字 **且** 全部敲对 **且** 确认没失败）。
    ///
    /// 自动点击关掉时 `confirm_clicked` 本来就是 `false`，那是用户选择而非失败，
    /// 所以这里看的是 `confirm_failed`（真的点过并且失败）。
    pub fn is_complete(&self) -> bool {
        self.digits_complete() && self.input_complete && !self.confirm_failed
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
    let mut stopped = false;
    while resume < cells.len() && attempts < MORSE_MAX_ATTEMPTS {
        if cancel.load(Ordering::SeqCst) {
            log.errors.push("已取消（未开始输入）".to_owned());
            stopped = true;
            break;
        }
        attempts += 1;
        let relay = press_relay(source, sink, job, &cells, resume, &mut log, cancel);
        resume = relay.resume;
        if relay.stopped {
            stopped = true;
            break;
        }
    }
    // 「敲完」不等于「敲对」：注入被拦截（UIPI）时也必须算没完成。
    let input_complete = !stopped && resume >= cells.len() && log.ok;
    // 「还有位没补上」才是连续输错；停手/取消另有原因，不混成这一条。
    if !stopped && resume < cells.len() {
        log.errors
            .push(format!("摩斯连续 {MORSE_MAX_ATTEMPTS} 次不正确，已停手"));
    }

    let (confirm_clicked, skipped, confirm_failed) =
        confirm(sink, job, &cells, input_complete, cancel, &mut log.errors);
    MorseOutcome {
        cells,
        password,
        pressed: log.pressed,
        confirm_clicked,
        skipped,
        errors: log.errors,
        attempts,
        input_complete,
        confirm_failed,
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

/// 面板一次取样的结论。
enum PanelView {
    /// 正常（没转红）。
    Clear,
    /// 整片转红 = 刚敲的那一位被游戏判错。
    Error,
    /// 读不到（抓帧失败）—— 不算正常，也不算红态。
    Unreadable,
}

fn look_panel<S: ScreenSource>(source: &mut S, job: &MorseRun<'_>) -> PanelView {
    match grab_panel(source, job) {
        Some(panel) if is_error_state(&panel) => PanelView::Error,
        Some(_) => PanelView::Clear,
        None => PanelView::Unreadable,
    }
}

/// 一轮接力的结果。
struct Relay {
    /// 下一次从这里继续敲。
    resume: usize,
    /// 是否安全停手（取消 / 输入被拒 / 面板读不到 / 红态没退 / 前缀读不清）。
    /// 停手后**不再**补敲任何位，也不猜位置。
    stopped: bool,
}

/// 从 `resume` 起逐位敲，输错就等红态消退并算出新的起点（接力）。
fn press_relay<S: ScreenSource, I: InputSink>(
    source: &mut S,
    sink: &mut I,
    job: &MorseRun<'_>,
    cells: &[CellDecode],
    resume: usize,
    log: &mut PressLog,
    cancel: &AtomicBool,
) -> Relay {
    let mut index = resume;
    while index < cells.len() {
        if cancel.load(Ordering::SeqCst) {
            log.errors.push("已取消（输入中断）".to_owned());
            return Relay {
                resume: index,
                stopped: true,
            };
        }
        let Some(digit) = cells[index].digit else {
            // 没解出数字的位不敲（保持非对称副作用：能敲的照敲）。
            index += 1;
            continue;
        };
        if let Err(reason) = sink.press_digit(digit) {
            log.errors.push(format!("按下 {digit} 失败：{reason}，已停手"));
            log.ok = false;
            return Relay {
                resume: index,
                stopped: true,
            };
        }
        log.pressed.push(digit);
        if !watches_panel(job) {
            sink.sleep_ms(job.timings.key_interval_ms);
            index += 1;
            continue;
        }
        // 正常节奏 = 配置的数字间隔（50ms）；不再每位固定等 370ms 看红。
        sink.sleep_ms(job.timings.key_interval_ms);
        match look_panel(source, job) {
            PanelView::Clear => {
                index += 1;
                continue;
            }
            PanelView::Unreadable => {
                log.errors.push("按下后读不到输入面板，已停手".to_owned());
                return Relay {
                    resume: index,
                    stopped: true,
                };
            }
            PanelView::Error => {}
        }
        log.errors
            .push(format!("第 {} 位输错（面板转红），接力补位", index + 1));
        if !wait_red_clear(source, sink, job) {
            log.errors
                .push("红态未消退或读不到面板，已停手（不猜、不重打）".to_owned());
            return Relay {
                resume: index,
                stopped: true,
            };
        }
        // 接力起点 = 重读面板得到的「第一个非绿小格」；读不清就停手。
        // 视觉只能**往前**推，绝不能退回到已经敲对的位（退了就会重复追加数字）。
        let Some(prefix) = grab_panel(source, job).and_then(|panel| solved_prefix(&panel)) else {
            log.errors
                .push("已正确位读不清，已停手（不猜、不重打）".to_owned());
            return Relay {
                resume: index,
                stopped: true,
            };
        };
        index = index.max(prefix).min(cells.len());
        // 一次输错 = 一轮接力的结束（`attempts` 就是这么数的）。
        return Relay {
            resume: index,
            stopped: false,
        };
    }
    Relay {
        resume: index,
        stopped: false,
    }
}

/// 是否观察面板（未标定 ROI 时保持旧行为，不做错误态判断）。
fn watches_panel(job: &MorseRun<'_>) -> bool {
    job.panel.w > 0 && job.panel.h > 0
}

fn grab_panel<S: ScreenSource>(source: &mut S, job: &MorseRun<'_>) -> Option<Frame> {
    watches_panel(job).then(|| source.grab(job.panel).ok()).flatten()
}

/// 等红态消退：最多 [`RED_CLEAR_TIMEOUT_MS`]。
///
/// 返回 `false` 的两种都算「没恢复」：超时仍红、或中途读不到面板。
/// 读不到**不等于**消退（旧实现把它当消退，等于拿失败当成功）。
fn wait_red_clear<S: ScreenSource, I: InputSink>(
    source: &mut S,
    sink: &mut I,
    job: &MorseRun<'_>,
) -> bool {
    let mut waited = 0u64;
    while waited < RED_CLEAR_TIMEOUT_MS {
        sink.sleep_ms(RED_POLL_MS);
        waited += RED_POLL_MS;
        match look_panel(source, job) {
            PanelView::Error => continue,
            PanelView::Clear => return true,
            PanelView::Unreadable => return false,
        }
    }
    false
}

/// 确认点击（spec 不变量 4/5 原样保留：只有「数字全解出 + 全部敲对」才点）。
///
/// 返回值第三项 `failed`：真的点了但被拒绝 / 中途取消 —— 这种情况**不能**算确认成功。
fn confirm<I: InputSink>(
    sink: &mut I,
    job: &MorseRun<'_>,
    cells: &[CellDecode],
    input_complete: bool,
    cancel: &AtomicBool,
    errors: &mut Vec<String>,
) -> (bool, Option<ConfirmSkipped>, bool) {
    let all_pressed = input_complete && !cells.is_empty();
    if !job.auto_click {
        return (false, Some(ConfirmSkipped::AutoClickDisabled), false);
    }
    if !(all_pressed && cells.iter().all(|cell| cell.digit.is_some())) {
        return (false, Some(ConfirmSkipped::DigitsFailed), false);
    }
    if job.view.confirm_clicks.is_empty() {
        return (false, Some(ConfirmSkipped::NotConfigured), false);
    }
    sink.sleep_ms(job.timings.morse_after_input_ms);
    for (index, point) in job.view.confirm_clicks.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            errors.push("已取消（确认点击中断）".to_owned());
            return (false, Some(ConfirmSkipped::Canceled), true);
        }
        if index > 0 {
            sink.sleep_ms(MORSE_CONFIRM_CLICK_INTERVAL_MS);
        }
        if let Err(reason) = sink.click(*point) {
            errors.push(format!("确认点击失败：{reason}"));
            return (false, Some(ConfirmSkipped::Failed), true);
        }
    }
    (true, None, false)
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
    use std::sync::Arc;
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
        fail_click: bool,
        /// 敲下第一个数字时把 cancel 置位（模拟用户中途取消）。
        cancel_on_press: Option<Arc<AtomicBool>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                events: Vec::new(),
                fail_press: false,
                fail_click: false,
                cancel_on_press: None,
            }
        }
    }

    impl InputSink for Recorder {
        fn press_digit(&mut self, digit: u8) -> Result<(), String> {
            if self.fail_press {
                return Err("UIPI 拦截".to_string());
            }
            self.events.push(Event::Press(digit));
            if let Some(flag) = &self.cancel_on_press {
                flag.store(true, Ordering::SeqCst);
            }
            Ok(())
        }

        fn click(&mut self, at: PxPoint) -> Result<(), String> {
            if self.fail_click {
                return Err("UIPI 拦截".to_string());
            }
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
    fn morse_timing_keeps_the_upstream_python_speed() {
        // spec G4 / §5：用户拍板「按原本 python 版本的点击速度和开启延迟即可」——
        // 这三项与 `_upstream/core/morse.py` 逐项一致，任何一项被改慢/改快都必须在这里红。
        // 用户若仍觉得慢，下一轮查的是「按 F 后到第一帧」那段，不是这三个数。
        let timings = Timings::default();
        assert_eq!(
            timings.key_interval_ms, 50,
            "数字间间隔 = python `time.sleep(0.05)`"
        );
        assert_eq!(
            timings.morse_after_input_ms, 600,
            "按完到点确认 = python `time.sleep(0.6)`"
        );
        assert_eq!(
            MORSE_CONFIRM_CLICK_INTERVAL_MS, 100,
            "确认点击间隔 = python `click_sequence(delay=0.1)`"
        );
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
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        assert!(
            outcome.errors[0].contains("已停手"),
            "输入被拒必须立即停手，不再继续敲后面的位：{:?}",
            outcome.errors
        );
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
        // 每轮：敲 4 → 面板转红 → 红消退 → 前缀仍是 0 绿 ⇒ 重敲这一位。
        // 四轮之后必须停手，不许无限重试。
        let mut script = Vec::new();
        for _ in 0..MORSE_MAX_ATTEMPTS {
            script.push(panel_image(0, true)); // 敲完就读到红
            script.push(panel_image(0, false)); // 红消退
            script.push(panel_image(0, false)); // 重读前缀：0 绿 ⇒ 这一位重敲
        }
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = RelayScreen::new(script)
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
        // 未标定面板（零面积 ROI）⇒ 保持旧行为：一次敲完、一次尝试。
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = FakeScreen::new()
            .with(20, 80, morse_frame("....-"))
            .with(20, 140, morse_frame("..---"))
            .with(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();
        let timings = timings();
        let job = MorseRun {
            view: &view,
            timings: &timings,
            auto_click: true,
            panel: PxRect::ZERO,
        };
        let outcome = exec(&mut screen, &mut sink, &job);
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.pressed, vec![4, 2, 7]);
        assert!(outcome.is_complete());
    }

    // ── 速度 / 停手边界（spec §5.2 第 3 条）──

    /// 第 `bad` 格涂成亮灰：既不绿也不暗 ⇒ `slot_state` 读不准（`None`）。
    fn panel_with_unreadable_slot(bad: usize, green_slots: usize) -> Frame {
        let (w, h) = (PANEL_ROI.w as u32, PANEL_ROI.h as u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        let slot_w = w / PANEL_SLOTS as u32;
        for slot in 0..PANEL_SLOTS {
            let s = slot as u32;
            let rgb = if slot == bad {
                [150u8, 150, 150]
            } else if slot < green_slots {
                [60u8, 210, 80]
            } else {
                [40u8, 42, 44]
            };
            for y in 0..h {
                for x in (s * slot_w + 2)..((s + 1) * slot_w - 2) {
                    let index = ((y * w + x) * 4) as usize;
                    data[index] = rgb[2];
                    data[index + 1] = rgb[1];
                    data[index + 2] = rgb[0];
                    data[index + 3] = 255;
                }
            }
        }
        Frame::new(w, h, data, Instant::now()).expect("合成面板帧")
    }

    #[test]
    fn one_unreadable_slot_does_not_count_as_a_full_prefix() {
        // 4 格全绿、只有第 4 格读不准：绝不能因此推定「5 格都绿 = 敲完了」。
        assert_eq!(solved_prefix(&panel_with_unreadable_slot(3, 4)), None);
    }

    #[test]
    fn normal_path_keeps_the_fifty_ms_cadence() {
        let view = view_with(
            CELLS,
            vec![PxPoint { x: 300, y: 240 }, PxPoint { x: 320, y: 240 }],
        );
        let mut screen = RelayScreen::new(vec![
            panel_image(0, false),
            panel_image(1, false),
            panel_image(2, false),
        ])
        .with_cell(20, 80, morse_frame("....-"))
        .with_cell(20, 140, morse_frame("..---"))
        .with_cell(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();
        let timings = timings();
        let job = relay_job(&view, &timings, true);

        let outcome = run(&mut screen, &mut sink, &job, &AtomicBool::new(false));

        assert!(outcome.is_complete(), "{:?}", outcome.errors);
        let sleeps: Vec<u64> = sink
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Sleep(ms) => Some(*ms),
                _ => None,
            })
            .collect();
        assert_eq!(
            sleeps,
            vec![7, 7, 7, 11, MORSE_CONFIRM_CLICK_INTERVAL_MS],
            "正常路径只有数字间隔（测试配成 7ms），不许再出现每位 370ms 硬等"
        );
    }

    #[test]
    fn red_that_never_clears_stops_immediately() {
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = RelayScreen::new(vec![panel_image(0, true)])
            .with_cell(20, 80, morse_frame("....-"))
            .with_cell(20, 140, morse_frame("..---"))
            .with_cell(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();
        let timings = timings();
        let job = relay_job(&view, &timings, true);

        let outcome = run(&mut screen, &mut sink, &job, &AtomicBool::new(false));

        assert_eq!(outcome.attempts, 1, "红态 400ms 没消退就该停手，不再补敲");
        assert_eq!(outcome.pressed, vec![4]);
        assert!(!outcome.input_complete);
        assert!(outcome
            .errors
            .iter()
            .any(|error| error.contains("红态未消退")));
        assert!(clicks(&sink.events).is_empty());
    }

    #[test]
    fn unreadable_prefix_stops_instead_of_blind_retry() {
        let uniform = Frame::new(
            PANEL_ROI.w as u32,
            PANEL_ROI.h as u32,
            vec![128u8; (PANEL_ROI.w * PANEL_ROI.h * 4) as usize],
            Instant::now(),
        )
        .expect("合成帧");
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = RelayScreen::new(vec![
            panel_image(0, true),  // 敲完读到红
            panel_image(0, false), // 红消退
            uniform,               // 前缀读不清
        ])
        .with_cell(20, 80, morse_frame("....-"))
        .with_cell(20, 140, morse_frame("..---"))
        .with_cell(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();
        let timings = timings();
        let job = relay_job(&view, &timings, true);

        let outcome = run(&mut screen, &mut sink, &job, &AtomicBool::new(false));

        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.pressed, vec![4], "读不清就不许再敲，也不许退回重打 4");
        assert!(outcome.errors.iter().any(|error| error.contains("读不清")));
        assert!(clicks(&sink.events).is_empty());
    }

    #[test]
    fn cancel_during_input_stops_further_digits() {
        let flag = Arc::new(AtomicBool::new(false));
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = RelayScreen::new(vec![panel_image(0, false)])
            .with_cell(20, 80, morse_frame("....-"))
            .with_cell(20, 140, morse_frame("..---"))
            .with_cell(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();
        sink.cancel_on_press = Some(Arc::clone(&flag));
        let timings = timings();
        let job = relay_job(&view, &timings, true);

        let outcome = run(&mut screen, &mut sink, &job, &flag);

        assert_eq!(outcome.pressed, vec![4], "取消之后不许再敲 2 和 7");
        assert!(!outcome.input_complete);
        assert!(outcome.errors.iter().any(|error| error.contains("已取消")));
        assert!(clicks(&sink.events).is_empty());
    }

    #[test]
    fn confirm_click_failure_is_not_reported_as_success() {
        let view = view_with(CELLS, vec![PxPoint { x: 300, y: 240 }]);
        let mut screen = FakeScreen::new()
            .with(20, 80, morse_frame("....-"))
            .with(20, 140, morse_frame("..---"))
            .with(20, 200, morse_frame("--..."));
        let mut sink = Recorder::new();
        sink.fail_click = true;
        let timings = timings();
        let job = MorseRun {
            view: &view,
            timings: &timings,
            auto_click: true,
            panel: PxRect::ZERO,
        };

        let outcome = exec(&mut screen, &mut sink, &job);

        assert!(outcome.input_complete, "数字本身是敲对的");
        assert!(!outcome.confirm_clicked, "点击被拒不能算点过");
        assert_eq!(outcome.skipped, Some(ConfirmSkipped::Failed));
        assert!(outcome.confirm_failed);
        assert!(!outcome.is_complete(), "确认失败不得显示成功");
        assert!(outcome
            .errors
            .iter()
            .any(|error| error.contains("确认点击失败")));
    }
}
