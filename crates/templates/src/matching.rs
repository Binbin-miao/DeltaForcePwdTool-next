//! 模板—候选格匹配（上游 `core/fingerprint.py:80-96` 的忠实移植）。
//!
//! ```text
//! margin_y = max(1, int(ch * 0.15))
//! margin_x = max(1, int(cw * 0.15))
//! for tmpl in templates:
//!     resized = resize(tmpl, (cw, ch), INTER_AREA)     # 非等比
//!     core    = resized[margin_y:ch-margin_y, margin_x:cw-margin_x]
//!     score   = max(matchTemplate(cand_gray, core, TM_CCOEFF_NORMED))
//! ```
//!
//! 注意：`threshold` 在这里**不参与**（上游把 0.45 硬编码在阈值门里，且配置项
//! `match_threshold` 是死配置，spec §2.2 第 10 条）。本函数只返回原始 NCC 得分，
//! 阈值判定统一交给 [`crate::assign`] 的 `min_score`。

use delta_vision::{match_template_ncc, GrayImage, Region};

use crate::TemplateError;

/// 上游 `int(dim * 0.15)` 的截断语义：单边 margin = `max(1, floor(dim*0.15))`。
///
/// 两侧各让出 margin，剩下正好是中心 70%×70%。
pub fn core_margin(dim: u32) -> u32 {
    ((dim as f64 * 0.15) as u32).max(1)
}

/// 把模板压到候选尺寸后取中心核心区。
///
/// 退化保护：候选极小（margin 吃光尺寸）时核心收缩为 1×1，而不是 0 面积 panic；
/// 这正是上游在 `cw*0.15 >= cw/2` 时会崩掉的分支。
pub fn template_core(
    tmpl: &GrayImage,
    cell_w: u32,
    cell_h: u32,
) -> Result<GrayImage, TemplateError> {
    if cell_w == 0 || cell_h == 0 {
        return Err(TemplateError::NoCells);
    }
    let resized = tmpl.resize_to(cell_w, cell_h)?; // 非等比：宽高各自独立
    let margin_x = core_margin(cell_w);
    let margin_y = core_margin(cell_h);
    let core_w = cell_w.saturating_sub(margin_x.saturating_mul(2)).max(1);
    let core_h = cell_h.saturating_sub(margin_y.saturating_mul(2)).max(1);
    Ok(resized.crop(Region {
        x: (cell_w - core_w) / 2,
        y: (cell_h - core_h) / 2,
        w: core_w,
        h: core_h,
    })?)
}

/// 单张模板对单个候选格的匹配得分 = 核心在候选全图上滑窗 NCC 的最大值。
pub fn match_cell(cell: &GrayImage, tmpl: &GrayImage) -> Result<f32, TemplateError> {
    let core = template_core(tmpl, cell.w, cell.h)?;
    Ok(match_template_ncc(cell, &core)?.score)
}

/// 按候选尺寸预缩放的核心缓存：同尺寸下只算一次，供 9 格 × N 模板复用。
pub(crate) struct CoreCache {
    size: (u32, u32),
    cores: Vec<GrayImage>,
}

impl CoreCache {
    pub(crate) fn build(role: &crate::Role, cell_w: u32, cell_h: u32) -> Result<Self, TemplateError> {
        if role.templates.is_empty() {
            return Err(TemplateError::NoTemplates {
                role: role.name.clone(),
            });
        }
        let cores = role
            .templates
            .iter()
            .map(|tmpl| template_core(&tmpl.image, cell_w, cell_h))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            size: (cell_w, cell_h),
            cores,
        })
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        self.size
    }

    pub(crate) fn cores(&self) -> &[GrayImage] {
        &self.cores
    }
}
