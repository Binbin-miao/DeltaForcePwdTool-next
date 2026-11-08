//! 指纹密码主流程（spec D2/D3/D4/D5/D10 + §2.2 不变量 2/3/4/6/9）。
//!
//! 与上游 `core/fingerprint.py:121-197` 的对应关系：
//!
//! | 上游 | 新版 |
//! |---|---|
//! | 人名 OCR → 数字 OCR → 反推模式 | 全模板投票认角色（D2 主引擎）→ N = 角色模板张数（D3）→ 数字区只做交叉校验 |
//! | `range(1, N+1)` + 逐格独立 argmax + 同号去重 | 二分图最大权匹配（D4）→ 天然满足「同模板号只落一格」（不变量 2） |
//! | 按候选号点击 | **按模板号升序**点击（不变量 3，`fp:223`） |
//! | `[0]` 落进 4 模式 | 数字区读到 0 → 中止（D10 缺陷①）；认不出角色 → 报错留证（缺陷②） |
//!
//! 本层不重建截图/匹配/输入能力：截图用 [`ScreenSource`]（t2）、匹配用 `delta-templates`（t4）、
//! 输入用 [`InputSink`]（t5）、OCR 用 [`TextReader`]（t6）。测试用同 trait 的假实现全程离线。

mod annotate;
mod correction;
mod evidence;

use std::fmt;
use std::path::PathBuf;

use delta_capture::{Capturer, Frame};
use delta_config::{PxView, Timings};
use delta_geom::{PxPoint, PxRect};
use delta_ocr::OcrEngine;
use delta_templates::{
    assign, vote_role, CellAssignment, RoleLibrary, TemplateError, Vote, AMBIGUOUS_GAP,
};
use delta_vision::{GrayImage, Region};
use thiserror::Error;

pub use annotate::{annotate, MATCH_COLOR, NAME_COLOR, NUMBER_COLOR};
pub use correction::{
    correct_role_name, intersection_ratio, parse_numbers, EXACT_CORRECTIONS, FUZZY_MIN_RATIO,
};
pub use evidence::{save_failure, Evidence, EVIDENCE_DIR};

/// 截图来源（真机是 [`LiveScreen`]）。
pub trait ScreenSource {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String>;
}

/// 输入落地（真机是 [`LiveInput`]）。
pub trait InputSink {
    fn click(&mut self, at: PxPoint) -> Result<(), String>;
    fn sleep_ms(&mut self, ms: u64);
}

/// 文本识别（真机是 [`OcrReader`]；测试是固定字符串）。
pub trait TextReader {
    fn read_text(&mut self, frame: &Frame) -> Result<String, String>;
}

/// 真机截图适配（复用 t2 常驻 DC）。
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

/// 真机输入适配（复用 t5 `SendInput`）。
pub struct LiveInput;

impl InputSink for LiveInput {
    fn click(&mut self, at: PxPoint) -> Result<(), String> {
        delta_input::click(at).map_err(|error| error.to_string())
    }

    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// 系统 OCR 适配（`Windows.Media.Ocr`，t6）。引擎按需构造，失败仅降级不崩（spec §2.1）。
pub struct OcrReader {
    engine: OcrEngine,
}

impl OcrReader {
    pub fn new(engine: OcrEngine) -> Self {
        Self { engine }
    }
}

impl TextReader for OcrReader {
    fn read_text(&mut self, frame: &Frame) -> Result<String, String> {
        self.engine
            .recognize(frame)
            .map_err(|error| error.to_string())
    }
}

/// 角色是怎么认出来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleSource {
    /// 全模板投票（离线、快，D2 主引擎）。
    Template,
    /// `Windows.Media.Ocr` + 两级纠错（兜底）。
    Ocr,
}

/// 失败原因分类（每类都对应一条规格条款）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 截图失败。
    Capture,
    /// 投票未定且 OCR 兜底也认不出角色 —— **不得静默 return**（D10 缺陷②）。
    RoleUnrecognized,
    /// 数字区读到 0 —— **必须中止**，不得落进 4 模式（D10 缺陷①）。
    NumberReadingInvalid,
    /// 角色没有任何模板（N = 0）。
    NoTemplates,
    /// 候选格一个都裁不出来。
    NoCells,
    /// 一个模板都没配上。
    NoMatches,
}

/// 失败必留证（D10 缺陷③）：`detail` 是结构化原因，`evidence` 是落盘路径。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("[{kind:?}] {detail}")]
pub struct FingerprintFailure {
    pub kind: FailureKind,
    pub detail: String,
    pub evidence: Option<Evidence>,
    pub warnings: Vec<String>,
}

impl FingerprintFailure {
    /// 留证图片路径（UI 直接展示）。
    pub fn evidence_image(&self) -> Option<&PathBuf> {
        self.evidence.as_ref().map(|evidence| &evidence.image)
    }
}

/// 一次流程的入参。
pub struct FingerprintJob<'a> {
    pub view: &'a PxView,
    pub library: &'a RoleLibrary,
    pub timings: &'a Timings,
    /// `false` → 只产出标注图，不点击（不变量 4）。
    pub auto_click: bool,
    /// `false` → 完全不走 OCR（投票失败即报错）。
    pub ocr_enabled: bool,
    pub evidence_dir: PathBuf,
}

/// 一次流程的完整结果。
#[derive(Debug)]
pub struct FingerprintOutcome {
    pub role: String,
    /// 模式 N = 该角色模板张数（D3）。
    pub n: u32,
    pub role_source: RoleSource,
    /// 参与匹配的存活格数（= N+1，除非候选框不足）。
    pub cells_used: usize,
    /// 命中配对，**按模板号升序**（不变量 3）。
    pub pairs: Vec<CellAssignment>,
    /// 实际点击的格中心（按模板号升序）。
    pub clicks: Vec<PxPoint>,
    pub confirm_clicks: Vec<PxPoint>,
    /// `auto_click == false` 时的标注图。
    pub annotated: Option<Frame>,
    /// 数字区读数（仅交叉校验，D3）。
    pub number_reading: Option<u32>,
    pub warnings: Vec<String>,
}

/// 主流程。
pub fn run<S: ScreenSource, I: InputSink>(
    source: &mut S,
    sink: &mut I,
    reader: Option<&mut dyn TextReader>,
    job: &FingerprintJob<'_>,
) -> Result<FingerprintOutcome, FingerprintFailure> {
    let mut warnings: Vec<String> = Vec::new();
    let frame = source.grab(full_screen(job.view)).map_err(|reason| {
        FingerprintFailure {
            kind: FailureKind::Capture,
            detail: reason,
            evidence: None,
            warnings: Vec::new(),
        }
    })?;
    let gray = frame.luma();
    let mut reader = reader;

    // ── 1. 数字区交叉校验（可选；D3 说不读数字属正常路径）──────
    let number_reading = read_number(job, &frame, &mut reader, &mut warnings)?;

    // ── 2. 候选格（N 未知，先按全部候选框切，供投票使用）────────
    let boxes = job.view.fingerprint.candidate_boxes.clone();
    let mut cell_images: Vec<GrayImage> = Vec::with_capacity(boxes.len());
    let mut cell_boxes: Vec<PxRect> = Vec::with_capacity(boxes.len());
    for (index, rect) in boxes.iter().enumerate() {
        match crop_gray(&gray, *rect) {
            Some(image) => {
                cell_images.push(image);
                cell_boxes.push(*rect);
            }
            None => warnings.push(format!("候选格 {} 裁剪失败，已跳过", index + 1)),
        }
    }
    if cell_images.is_empty() {
        return Err(failure(
            FailureKind::NoCells,
            "所有候选格都裁不出来（校准坐标可能越界）".to_string(),
            &frame,
            job,
            warnings,
        ));
    }

    // ── 3. 认角色：投票优先，OCR 兜底（D2）────────────────────
    let vote = vote_role(&cell_images, job.library);
    let vote_note = describe_vote(&vote);
    let (role, role_source) = match &vote {
        Ok(Vote::Chosen(chosen)) => (chosen.role, RoleSource::Template),
        _ => recognise_by_ocr(&frame, job, &mut reader, &vote_note, &mut warnings)?,
    };

    // ── 4. N 由模板张数给出，并与数字区交叉校验（D3）────────────
    let n = role.n();
    if n == 0 {
        return Err(failure(
            FailureKind::NoTemplates,
            format!("角色 {} 没有任何模板", role.name),
            &frame,
            job,
            warnings,
        ));
    }
    if let Some(max) = number_reading {
        if max != n {
            warnings.push(format!(
                "数字区读到 max={max}（按上游区间推得模式 {}），与模板张数 N={n} 不一致 —— 以 N 为准（spec D3）",
                mode_from_max(max)
            ));
        }
    }

    // ── 5. 存活格 = N+1，取候选框行主序前 N+1 个（不变量 9）──────
    let needed = (n as usize).saturating_add(1);
    if cell_images.len() < needed {
        warnings.push(format!(
            "可用候选格 {} 个，少于 N+1={needed}（继续按实有格数匹配）",
            cell_images.len()
        ));
    }
    let used = needed.min(cell_images.len());
    let cells = &cell_images[..used];
    let used_boxes = &cell_boxes[..used];

    // ── 6. 二分图最大权匹配（D4）─────────────────────────────
    let assignment = assign(cells, role).map_err(|error: TemplateError| {
        failure(
            FailureKind::NoMatches,
            format!("二分图匹配失败：{error}"),
            &frame,
            job,
            warnings.clone(),
        )
    })?;
    if assignment.pairs.is_empty() {
        return Err(failure(
            FailureKind::NoMatches,
            format!(
                "角色 {} 一个模板都没配上（检查坐标偏移或模板是否过期）",
                role.name
            ),
            &frame,
            job,
            warnings,
        ));
    }
    if !assignment.is_complete() {
        warnings.push(format!(
            "有 {} 个模板未达匹配门限：{:?}",
            assignment.unmatched.len(),
            assignment
                .unmatched
                .iter()
                .map(|entry| (entry.tmpl_no, entry.best_score))
                .collect::<Vec<_>>()
        ));
    }

    // ── 7. 按模板号升序点击（不变量 3），或只出标注图（不变量 4）──
    let mut clicks = Vec::with_capacity(assignment.pairs.len());
    let mut confirm_clicks = Vec::new();
    let mut annotated = None;

    if job.auto_click {
        for (index, pair) in assignment.pairs.iter().enumerate() {
            let point = center(used_boxes[pair.cell_idx]);
            if index > 0 {
                sink.sleep_ms(job.timings.click_interval_ms);
            }
            match sink.click(point) {
                Ok(()) => clicks.push(point),
                Err(reason) => {
                    warnings.push(format!("点击候选 {} 失败：{reason}", pair.cell_idx + 1));
                }
            }
        }
        // 确认点击只在 confirm_clicks 非空时触发（不变量 4 + D5）。
        if !job.view.confirm_clicks.is_empty() {
            sink.sleep_ms(job.timings.fingerprint_after_click_ms);
            for (index, point) in job.view.confirm_clicks.iter().enumerate() {
                if index > 0 {
                    sink.sleep_ms(job.timings.click_interval_ms);
                }
                match sink.click(*point) {
                    Ok(()) => confirm_clicks.push(*point),
                    Err(reason) => warnings.push(format!("确认点击失败：{reason}")),
                }
            }
        }
    } else {
        let matched: Vec<(PxRect, u32)> = assignment
            .pairs
            .iter()
            .map(|pair| (used_boxes[pair.cell_idx], pair.tmpl_no))
            .collect();
        annotated = Some(annotate(
            &frame,
            &matched,
            job.view.fingerprint.name_region,
            job.view.fingerprint.number_region,
        ));
    }

    Ok(FingerprintOutcome {
        role: role.name.clone(),
        n,
        role_source,
        cells_used: used,
        pairs: assignment.pairs,
        clicks,
        confirm_clicks,
        annotated,
        number_reading,
        warnings,
    })
}

/// 上游模式区间（spec §2.2 不变量 1）：`max∈[1,4]→4`、`[5,6]→6`、`[7,9]→8`。
///
/// 新版 N 由模板张数给出（D3），此函数**只用于交叉校验与告警**，不驱动任何流程。
pub fn mode_from_max(max: u32) -> u32 {
    if max > 6 {
        8
    } else if max > 4 {
        6
    } else {
        4
    }
}

/// 数字区读数：`Ok(None)` 表示「没读到 / 没开 OCR / 没校准」，都属正常路径（D3）。
/// 读到 0 → 中止（D10 缺陷①：不得落进 4 模式）。
fn read_number(
    job: &FingerprintJob<'_>,
    frame: &Frame,
    reader: &mut Option<&mut dyn TextReader>,
    warnings: &mut Vec<String>,
) -> Result<Option<u32>, FingerprintFailure> {
    if !job.ocr_enabled {
        return Ok(None);
    }
    let Some(slot) = reader.as_mut() else {
        return Ok(None);
    };
    let text_reader: &mut dyn TextReader = &mut **slot;
    let Some(number_frame) = crop_frame(frame, job.view.fingerprint.number_region) else {
        return Ok(None);
    };

    match text_reader.read_text(&number_frame) {
        Ok(text) => match parse_numbers(&text).iter().copied().max() {
            None => {
                warnings.push("数字区未读到数字（spec D3：属正常路径，不报错）".to_string());
                Ok(None)
            }
            Some(0) => Err(failure(
                FailureKind::NumberReadingInvalid,
                format!("数字区读到非法读数 {text:?}（D10 缺陷①：不得落进 4 模式，直接中止）"),
                frame,
                job,
                warnings.clone(),
            )),
            Some(max) => Ok(Some(max)),
        },
        Err(reason) => {
            warnings.push(format!("数字区 OCR 失败（不阻断主流程）：{reason}"));
            Ok(None)
        }
    }
}

/// OCR 兜底认角色（D2）：识别 → 两级纠错 → 查库；查不到就是硬失败，不静默 return。
fn recognise_by_ocr<'a>(
    frame: &Frame,
    job: &FingerprintJob<'a>,
    reader: &mut Option<&mut dyn TextReader>,
    vote_note: &str,
    warnings: &mut Vec<String>,
) -> Result<(&'a delta_templates::Role, RoleSource), FingerprintFailure> {
    if !job.ocr_enabled {
        return Err(failure(
            FailureKind::RoleUnrecognized,
            format!("{vote_note}；OCR 已关闭，无法兜底"),
            frame,
            job,
            warnings.clone(),
        ));
    }
    let Some(slot) = reader.as_mut() else {
        return Err(failure(
            FailureKind::RoleUnrecognized,
            format!("{vote_note}；OCR 引擎不可用，无法兜底"),
            frame,
            job,
            warnings.clone(),
        ));
    };
    let text_reader: &mut dyn TextReader = &mut **slot;

    let Some(name_frame) = crop_frame(frame, job.view.fingerprint.name_region) else {
        return Err(failure(
            FailureKind::RoleUnrecognized,
            format!("{vote_note}；名字区坐标非法"),
            frame,
            job,
            warnings.clone(),
        ));
    };
    let raw = match text_reader.read_text(&name_frame) {
        Ok(raw) => raw,
        Err(reason) => {
            return Err(failure(
                FailureKind::RoleUnrecognized,
                format!("{vote_note}；OCR 失败：{reason}"),
                frame,
                job,
                warnings.clone(),
            ));
        }
    };
    let corrected = correct_role_name(&raw, job.library);
    warnings.push(format!(
        "{vote_note} → OCR 读到 {raw:?}，纠正为 {corrected:?}（兜底引擎）"
    ));
    match job.library.find(&corrected) {
        Some(role) => Ok((role, RoleSource::Ocr)),
        None => Err(failure(
            FailureKind::RoleUnrecognized,
            format!("{vote_note}；库里没有角色 {corrected:?}（OCR 原文 {raw:?}）"),
            frame,
            job,
            warnings.clone(),
        )),
    }
}

fn describe_vote(vote: &Result<Vote<'_>, TemplateError>) -> String {
    match vote {
        Ok(Vote::Chosen(chosen)) => format!(
            "模板投票选中 {}（归一化得分 {:.3}）",
            chosen.role.name, chosen.score
        ),
        Ok(Vote::Ambiguous {
            best,
            runner_up,
            gap,
        }) => format!(
            "模板投票不确定：{} {:.3} vs {} {:.3}（gap {:.3} < {:.3}）",
            best.role.name,
            best.score,
            runner_up.role.name,
            runner_up.score,
            gap,
            AMBIGUOUS_GAP
        ),
        Err(error) => format!("模板投票失败：{error}"),
    }
}

fn failure(
    kind: FailureKind,
    detail: String,
    frame: &Frame,
    job: &FingerprintJob<'_>,
    mut warnings: Vec<String>,
) -> FingerprintFailure {
    let evidence = save_failure(frame, &job.evidence_dir, &format!("{kind:?}"), &detail);
    if evidence.is_none() {
        warnings.push(format!(
            "失败留证写入 {} 失败（不影响主流程退出）",
            job.evidence_dir.display()
        ));
    }
    FingerprintFailure {
        kind,
        detail,
        evidence,
        warnings,
    }
}

fn full_screen(view: &PxView) -> PxRect {
    PxRect {
        x: 0,
        y: 0,
        w: view.monitor.w,
        h: view.monitor.h,
    }
}

fn rect_to_region(rect: PxRect, image_w: u32, image_h: u32) -> Option<Region> {
    if rect.w <= 0 || rect.h <= 0 || rect.x < 0 || rect.y < 0 {
        return None;
    }
    let region = Region {
        x: rect.x as u32,
        y: rect.y as u32,
        w: rect.w as u32,
        h: rect.h as u32,
    };
    if region.x.checked_add(region.w)? > image_w || region.y.checked_add(region.h)? > image_h {
        return None;
    }
    Some(region)
}

fn crop_gray(gray: &GrayImage, rect: PxRect) -> Option<GrayImage> {
    let region = rect_to_region(rect, gray.w, gray.h)?;
    gray.crop(region).ok()
}

fn crop_frame(frame: &Frame, rect: PxRect) -> Option<Frame> {
    let region = rect_to_region(rect, frame.w, frame.h)?;
    let mut data = Vec::with_capacity((region.w * region.h * 4) as usize);
    for y in region.y..region.y + region.h {
        let start = (y as usize * frame.stride as usize) + region.x as usize * 4;
        data.extend_from_slice(&frame.data[start..start + region.w as usize * 4]);
    }
    Frame::new(region.w, region.h, data, std::time::Instant::now()).ok()
}

fn center(rect: PxRect) -> PxPoint {
    PxPoint {
        x: rect.x + rect.w / 2,
        y: rect.y + rect.h / 2,
    }
}

impl fmt::Display for RoleSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoleSource::Template => write!(formatter, "模板投票"),
            RoleSource::Ocr => write!(formatter, "OCR 兜底"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_config::{PxConsole, PxFingerprint};
    use delta_geom::MonitorInfo;
    use delta_templates::{Role, Template};
    use std::time::Instant;

    const CELL: u32 = 40;
    const W: u32 = 400;
    const H: u32 = 300;

    // ── 合成画面 ───────────────────────────────────────────

    fn shape(kind: u8) -> GrayImage {
        let mut data = vec![0u8; (CELL * CELL) as usize];
        for y in 0..CELL {
            for x in 0..CELL {
                let hit = match kind {
                    // 竖条
                    0 => (16..24).contains(&x),
                    // 横条
                    1 => (16..24).contains(&y),
                    // 十字
                    2 => (16..24).contains(&x) || (16..24).contains(&y),
                    // 方框
                    3 => !(8..32).contains(&x) || !(8..32).contains(&y),
                    // 斜条
                    _ => (x as i32 - y as i32).abs() < 4,
                };
                if hit {
                    data[(y * CELL + x) as usize] = 235;
                }
            }
        }
        GrayImage::new(CELL, CELL, data).expect("合法模板")
    }

    fn canvas(w: u32, h: u32) -> Frame {
        Frame::new(w, h, vec![0; (w * h * 4) as usize], Instant::now()).expect("合法帧")
    }

    fn paint(frame: &mut Frame, rect: PxRect, image: &GrayImage) {
        for y in 0..image.h {
            for x in 0..image.w {
                let target_x = rect.x + x as i32;
                let target_y = rect.y + y as i32;
                if target_x < 0
                    || target_y < 0
                    || target_x >= frame.w as i32
                    || target_y >= frame.h as i32
                {
                    continue;
                }
                let value = image.pixel(x, y);
                let index = (target_y as u32 * frame.stride + target_x as u32 * 4) as usize;
                frame.data[index..index + 3].fill(value);
                frame.data[index + 3] = 255;
            }
        }
    }

    fn cell_rect(index: usize) -> PxRect {
        PxRect {
            x: 40 + index as i32 * 80,
            y: 40,
            w: CELL as i32,
            h: CELL as i32,
        }
    }

    fn view_with(cells: usize, confirm: Vec<PxPoint>, number_region: PxRect) -> PxView {
        PxView {
            monitor: MonitorInfo::new(W as i32, H as i32).expect("合法显示器"),
            regions: Vec::new(),
            fingerprint: PxFingerprint {
                name_region: PxRect {
                    x: 40,
                    y: 140,
                    w: 200,
                    h: 40,
                },
                number_region,
                candidate_boxes: (0..cells).map(cell_rect).collect(),
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

    /// 甲：模板 1 = 竖条，模板 2 = 横条；乙：模板 1 = 方框，模板 2 = 斜条。
    fn two_role_library() -> RoleLibrary {
        RoleLibrary::from_roles(vec![
            Role {
                name: "甲".to_string(),
                templates: vec![
                    Template {
                        no: 1,
                        image: shape(0),
                    },
                    Template {
                        no: 2,
                        image: shape(1),
                    },
                ],
            },
            Role {
                name: "乙".to_string(),
                templates: vec![
                    Template {
                        no: 1,
                        image: shape(3),
                    },
                    Template {
                        no: 2,
                        image: shape(4),
                    },
                ],
            },
        ])
        .expect("非空角色库")
    }

    /// 两个角色模板完全相同 → 投票必然 Ambiguous，用来验证 OCR 兜底。
    fn ambiguous_library() -> RoleLibrary {
        let templates = || {
            vec![
                Template {
                    no: 1,
                    image: shape(0),
                },
                Template {
                    no: 2,
                    image: shape(1),
                },
            ]
        };
        RoleLibrary::from_roles(vec![
            Role {
                name: "克莱尔".to_string(),
                templates: templates(),
            },
            Role {
                name: "卢卡斯".to_string(),
                templates: templates(),
            },
        ])
        .expect("非空角色库")
    }

    /// 帧里第 0 格放横条（= 模板 2），第 1 格放竖条（= 模板 1），第 2 格放方框。
    fn fingerprint_frame() -> Frame {
        let mut frame = canvas(W, H);
        paint(&mut frame, cell_rect(0), &shape(1));
        paint(&mut frame, cell_rect(1), &shape(0));
        paint(&mut frame, cell_rect(2), &shape(3));
        frame
    }

    fn empty_frame() -> Frame {
        canvas(W, H)
    }

    struct FakeScreen {
        frame: Frame,
        fail: bool,
    }

    impl FakeScreen {
        fn new(frame: Frame) -> Self {
            Self { frame, fail: false }
        }
    }

    impl ScreenSource for FakeScreen {
        fn grab(&mut self, _rect: PxRect) -> Result<Frame, String> {
            if self.fail {
                return Err("屏幕 DC 获取失败".to_string());
            }
            Ok(self.frame.clone())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        Click(PxPoint),
        Sleep(u64),
    }

    struct Recorder {
        events: Vec<Event>,
    }

    impl Recorder {
        fn new() -> Self {
            Self { events: Vec::new() }
        }

        fn clicks(&self) -> Vec<PxPoint> {
            self.events
                .iter()
                .filter_map(|event| match event {
                    Event::Click(point) => Some(*point),
                    _ => None,
                })
                .collect()
        }
    }

    impl InputSink for Recorder {
        fn click(&mut self, at: PxPoint) -> Result<(), String> {
            self.events.push(Event::Click(at));
            Ok(())
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.events.push(Event::Sleep(ms));
        }
    }

    struct FixedReader {
        text: String,
    }

    impl TextReader for FixedReader {
        fn read_text(&mut self, _frame: &Frame) -> Result<String, String> {
            Ok(self.text.clone())
        }
    }

    /// 测试用时序（`OnceLock` 避免悬垂引用）：点击间 5ms、点击后 17ms。
    fn static_timings() -> &'static Timings {
        static TIMINGS: std::sync::OnceLock<Timings> = std::sync::OnceLock::new();
        TIMINGS.get_or_init(|| Timings {
            click_interval_ms: 5,
            fingerprint_after_click_ms: 17,
            ..Timings::default()
        })
    }

    fn evidence_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("delta-fp-run-{tag}-{}", std::process::id()))
    }

    fn job<'a>(view: &'a PxView, library: &'a RoleLibrary, dir: &str) -> FingerprintJob<'a> {
        FingerprintJob {
            view,
            library,
            timings: static_timings(),
            auto_click: true,
            ocr_enabled: false,
            evidence_dir: evidence_dir(dir),
        }
    }

    fn center_of(index: usize) -> PxPoint {
        center(cell_rect(index))
    }

    // ── 用例 ───────────────────────────────────────────────

    /// spec §2.2 不变量 1：模式区间 `[1,4]→4`、`[5,6]→6`、`[7,9]→8`（仅交叉校验）。
    #[test]
    fn mode_interval_matches_upstream() {
        assert_eq!(mode_from_max(1), 4);
        assert_eq!(mode_from_max(4), 4);
        assert_eq!(mode_from_max(5), 6);
        assert_eq!(mode_from_max(6), 6);
        assert_eq!(mode_from_max(7), 8);
        assert_eq!(mode_from_max(9), 8);
        assert_eq!(mode_from_max(0), 4, "上游对 0 也会落到 4，所以必须显式中止（D10①）");
    }

    #[test]
    fn clicks_follow_template_number_order_not_cell_order() {
        let library = two_role_library();
        let view = view_with(3, Vec::new(), PxRect::ZERO);
        let mut screen = FakeScreen::new(fingerprint_frame());
        let mut sink = Recorder::new();
        let plan = job(&view, &library, "order");

        let outcome = run(&mut screen, &mut sink, None, &plan).expect("应成功");

        assert_eq!(outcome.role, "甲");
        assert_eq!(outcome.n, 2, "N = 模板张数（D3）");
        assert_eq!(outcome.role_source, RoleSource::Template);
        assert_eq!(outcome.cells_used, 3, "存活格 = N+1");
        // 模板号升序：模板 1（在第 1 格）先点，模板 2（在第 0 格）后点。
        assert_eq!(outcome.pairs.len(), 2);
        assert_eq!(outcome.pairs[0].tmpl_no, 1);
        assert_eq!(outcome.pairs[0].cell_idx, 1);
        assert_eq!(outcome.pairs[1].tmpl_no, 2);
        assert_eq!(outcome.pairs[1].cell_idx, 0);
        assert_eq!(sink.clicks(), vec![center_of(1), center_of(0)]);
        assert_eq!(
            sink.events,
            vec![
                Event::Click(center_of(1)),
                Event::Sleep(5),
                Event::Click(center_of(0)),
            ],
            "点击间 50ms（配置值），模板号升序"
        );
        assert!(outcome.confirm_clicks.is_empty());
        assert!(outcome.annotated.is_none());
    }

    #[test]
    fn confirm_clicks_happen_after_1700ms() {
        let library = two_role_library();
        let confirm = vec![PxPoint { x: 380, y: 280 }];
        let view = view_with(3, confirm.clone(), PxRect::ZERO);
        let mut screen = FakeScreen::new(fingerprint_frame());
        let mut sink = Recorder::new();
        let plan = job(&view, &library, "confirm");

        let outcome = run(&mut screen, &mut sink, None, &plan).expect("应成功");

        assert_eq!(outcome.confirm_clicks, confirm);
        assert_eq!(
            sink.events,
            vec![
                Event::Click(center_of(1)),
                Event::Sleep(5),
                Event::Click(center_of(0)),
                Event::Sleep(17),
                Event::Click(confirm[0]),
            ],
            "点击完成后等 1700ms（配置值）再确认"
        );
    }

    #[test]
    fn auto_click_disabled_only_produces_annotated_frame() {
        let library = two_role_library();
        let view = view_with(3, vec![PxPoint { x: 380, y: 280 }], PxRect::ZERO);
        let mut screen = FakeScreen::new(fingerprint_frame());
        let mut sink = Recorder::new();
        let mut plan = job(&view, &library, "annotate");
        plan.auto_click = false;

        let outcome = run(&mut screen, &mut sink, None, &plan).expect("应成功");

        assert!(sink.events.is_empty(), "auto_click=false 不得产生任何输入");
        assert!(outcome.clicks.is_empty());
        assert!(outcome.confirm_clicks.is_empty());
        let annotated = outcome.annotated.expect("必须产出标注图");
        let original = fingerprint_frame();
        assert_ne!(annotated.data, original.data, "标注图必须被画过");
        assert_eq!(annotated.w, W);
        assert_eq!(outcome.pairs.len(), 2);
    }

    #[test]
    fn ambiguous_vote_falls_back_to_ocr_exact_correction() {
        let library = ambiguous_library();
        let view = view_with(3, Vec::new(), PxRect::ZERO);
        let mut screen = FakeScreen::new(fingerprint_frame());
        let mut sink = Recorder::new();
        let mut reader = FixedReader {
            text: "克菜尔".to_string(),
        };
        let mut plan = job(&view, &library, "ocr-exact");
        plan.ocr_enabled = true;

        let outcome = run(&mut screen, &mut sink, Some(&mut reader), &plan).expect("OCR 兜底应成功");

        assert_eq!(outcome.role_source, RoleSource::Ocr);
        assert_eq!(outcome.role, "克莱尔", "精确映射表必须命中");
        assert!(!outcome.pairs.is_empty());
        assert!(
            outcome.warnings.iter().any(|w| w.contains("OCR")),
            "兜底必须留痕：{:?}",
            outcome.warnings
        );
    }

    #[test]
    fn vote_failure_with_ocr_disabled_is_hard_error_with_evidence() {
        let library = two_role_library();
        let view = view_with(3, Vec::new(), PxRect::ZERO);
        let mut screen = FakeScreen::new(empty_frame());
        let mut sink = Recorder::new();
        let plan = job(&view, &library, "no-role");

        let failure = run(&mut screen, &mut sink, None, &plan).expect_err("必须报错");

        assert!(
            matches!(
                failure.kind,
                FailureKind::RoleUnrecognized | FailureKind::NoMatches
            ),
            "{:?}",
            failure.kind
        );
        let evidence = failure.evidence.expect("失败必留证（D10 缺陷③）");
        assert!(evidence.image.exists(), "{}", evidence.image.display());
        assert!(evidence.note.exists());
        assert!(sink.events.is_empty(), "认不出角色时不得发任何输入");
        let _ = std::fs::remove_dir_all(evidence.image.parent().unwrap());
    }

    #[test]
    fn number_reading_zero_aborts_before_clicking() {
        let library = two_role_library();
        let number_region = PxRect {
            x: 40,
            y: 220,
            w: 120,
            h: 40,
        };
        let view = view_with(3, Vec::new(), number_region);
        let mut screen = FakeScreen::new(fingerprint_frame());
        let mut sink = Recorder::new();
        let mut reader = FixedReader {
            text: "0".to_string(),
        };
        let mut plan = job(&view, &library, "number-zero");
        plan.ocr_enabled = true;

        let failure = run(&mut screen, &mut sink, Some(&mut reader), &plan).expect_err("必须中止");

        assert_eq!(failure.kind, FailureKind::NumberReadingInvalid, "{failure:?}");
        assert!(failure.evidence.is_some());
        assert!(sink.events.is_empty(), "中止时不得发任何输入");
        let directory = failure.evidence.expect("留证").image;
        let _ = std::fs::remove_dir_all(directory.parent().expect("有父目录"));
    }

    #[test]
    fn number_mismatch_defers_to_template_count() {
        let library = two_role_library();
        let number_region = PxRect {
            x: 40,
            y: 220,
            w: 120,
            h: 40,
        };
        let view = view_with(3, Vec::new(), number_region);
        let mut screen = FakeScreen::new(fingerprint_frame());
        let mut sink = Recorder::new();
        let mut reader = FixedReader {
            text: "9".to_string(),
        };
        let mut plan = job(&view, &library, "number-mismatch");
        plan.ocr_enabled = true;

        let outcome = run(&mut screen, &mut sink, Some(&mut reader), &plan).expect("应成功");

        assert_eq!(outcome.number_reading, Some(9));
        assert_eq!(outcome.n, 2, "以模板张数 N 为准（D3）");
        assert_eq!(outcome.role, "甲");
        assert!(
            outcome.warnings.iter().any(|w| w.contains("以 N 为准")),
            "{:?}",
            outcome.warnings
        );
    }

    #[test]
    fn unreadable_number_region_is_a_normal_path() {
        let library = two_role_library();
        let number_region = PxRect {
            x: 40,
            y: 220,
            w: 120,
            h: 40,
        };
        let view = view_with(3, Vec::new(), number_region);
        let mut screen = FakeScreen::new(fingerprint_frame());
        let mut sink = Recorder::new();
        let mut reader = FixedReader {
            text: "没有数字".to_string(),
        };
        let mut plan = job(&view, &library, "number-empty");
        plan.ocr_enabled = true;

        let outcome = run(&mut screen, &mut sink, Some(&mut reader), &plan).expect("读不到数字不报错");

        assert_eq!(outcome.number_reading, None);
        assert_eq!(outcome.n, 2);
        assert!(outcome.warnings.iter().any(|w| w.contains("正常路径")));
    }

    #[test]
    fn capture_failure_is_reported_without_evidence() {
        let library = two_role_library();
        let view = view_with(3, Vec::new(), PxRect::ZERO);
        let mut screen = FakeScreen::new(fingerprint_frame());
        screen.fail = true;
        let mut sink = Recorder::new();
        let plan = job(&view, &library, "capture");

        let failure = run(&mut screen, &mut sink, None, &plan).expect_err("截图失败必须报错");

        assert_eq!(failure.kind, FailureKind::Capture);
        assert_eq!(failure.evidence, None, "没画面就没法留图");
    }

    #[test]
    fn fewer_candidate_boxes_than_n_plus_one_still_runs_with_warning() {
        let library = two_role_library();
        // 只给 2 个候选框，而 N+1 = 3。
        let view = view_with(2, Vec::new(), PxRect::ZERO);
        let mut screen = FakeScreen::new(fingerprint_frame());
        let mut sink = Recorder::new();
        let plan = job(&view, &library, "short-boxes");

        let outcome = run(&mut screen, &mut sink, None, &plan).expect("应降级运行");

        assert_eq!(outcome.cells_used, 2);
        assert!(
            outcome.warnings.iter().any(|w| w.contains("N+1")),
            "{:?}",
            outcome.warnings
        );
    }

    /// 离线门禁（spec §4）：真实素材库必须正好 9 个角色，且 N 与模板张数一致（D3）。
    #[test]
    fn real_library_has_nine_roles_with_spec_template_counts() {
        let library = RoleLibrary::load_from_dir(delta_templates::IMAGES_DIR).expect("真实素材库");
        let mut counts: Vec<(String, u32)> = library
            .roles()
            .iter()
            .map(|role| (role.name.clone(), role.n()))
            .collect();
        counts.sort();

        let expected: Vec<(String, u32)> = [
            ("克莱尔", 6u32),
            ("卢卡斯", 6),
            ("埃德温", 4),
            ("巴斯特", 6),
            ("德穆兰", 6),
            ("无名", 6),
            ("格赫罗斯", 8),
            ("罗米修斯", 6),
            ("雅各布", 6),
        ]
        .iter()
        .map(|(name, n)| ((*name).to_string(), *n))
        .collect();

        assert_eq!(counts, expected, "9 角色 N 映射必须与 spec D3 一致");
        assert_eq!(
            counts.iter().map(|(_, n)| *n).sum::<u32>(),
            54,
            "共 54 张模板"
        );
    }

    /// 真实模板 + 合成候选格：投票必须认出该角色，且 N+1 格匹配出 N 个不同模板号。
    #[test]
    fn real_templates_identify_role_and_assign_distinct_template_numbers() {
        let library = RoleLibrary::load_from_dir(delta_templates::IMAGES_DIR).expect("真实素材库");
        let role = library.find("格赫罗斯").expect("格赫罗斯必须存在");
        let n = role.n();
        assert_eq!(n, 8);

        // 用真实模板缩放 0.8 倍当候选格（模拟游戏中不同尺寸），共 N+1 = 9 格。
        let mut cells: Vec<GrayImage> = role
            .templates
            .iter()
            .map(|template| {
                template
                    .image
                    .resize_to(84, 84)
                    .expect("非等比缩放（同比例但尺寸变）")
            })
            .collect();
        cells.push(shape(2));
        assert_eq!(cells.len(), 9);

        let vote = vote_role(&cells, &library).expect("投票");
        assert_eq!(vote.leader().role.name, "格赫罗斯", "投票必须认出角色");

        let assignment = assign(&cells, role).expect("最大权匹配");
        assert_eq!(assignment.pairs.len(), n as usize, "N 个配对");
        let mut numbers: Vec<u32> = assignment.pairs.iter().map(|pair| pair.tmpl_no).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), n as usize, "模板号必须互不重复（不变量 2）");
        assert!(
            assignment.pairs.windows(2).all(|pair| pair[0].tmpl_no < pair[1].tmpl_no),
            "配对必须按模板号升序（不变量 3）"
        );
    }
}
