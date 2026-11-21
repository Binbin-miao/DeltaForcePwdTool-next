//! 角色模板库 + 全模板投票 + 二分图最大权匹配（spec D3/D4/D5）。
//!
//! 口径来源：`_upstream/core/fingerprint.py:86-96`（模板匹配语义）与 `:121-197`（主流程）。
//! 三处**不可"优化"**的语义：
//!
//! 1. 模板先**非等比** resize 到候选尺寸，再取中心 70%×70% 作核心，核心在候选全图上滑窗
//!    NCC 取最大 —— 这是跨分辨率可用的根因（spec §2.2 第 11 条）；
//! 2. 格子↔模板配对是**二分图最大权匹配**，不是逐格 argmax（spec D4）；
//! 3. 模式 N = 该角色模板张数（spec D3），投票得分要除以 N 归一化。
//!
//! 本 crate 是纯计算：不截图、不点击、不读游戏窗口。

mod hungarian;
mod loading;
mod matching;
mod vote;

use std::path::PathBuf;

use delta_vision::VisionError;
use thiserror::Error;

pub use loading::{decode_png_gray, Role, RoleLibrary, Template};
pub use matching::{core_margin, match_cell, template_core};
pub use vote::{
    assign, assign_with_min_score, score_matrix, vote_role, vote_role_with_gap, Assignment,
    CellAssignment, UnmatchedTemplate, Vote, Voted, AMBIGUOUS_GAP, MIN_SCORE,
};

/// 运行时模板素材目录（相对 crate 根；打包时由 delta-app 内嵌或随包分发）。
pub const IMAGES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../images");

/// 模板库错误。所有退化路径都必须显式报错或显式记录告警，禁止静默 `return`。
#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("模板根目录 {root} 下没有任何可用角色")]
    NoRoles { root: PathBuf },
    #[error("读取 {path} 失败：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("解码图片 {path} 失败：{source}")]
    Png {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("没有存活格可用于匹配")]
    NoCells,
    #[error("角色 {role} 没有任何模板")]
    NoTemplates { role: String },
    #[error(transparent)]
    Vision(#[from] VisionError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_vision::GrayImage;

    /// 纯函数校验：核心区恒为中心 70%（上游 `int(dim*0.15)` 截断 + `max(1, _)`）。
    #[test]
    fn template_core_is_centered_seventy_percent() {
        assert_eq!(core_margin(105), 15, "int(105*0.15) = 15");
        assert_eq!(core_margin(10), 1, "int(1.5) = 1");
        assert_eq!(core_margin(1), 1, "max(1, int(0.15)) = 1");

        let template = GrayImage::new(2, 1, vec![10, 200]).expect("合法模板");
        let core = template_core(&template, 100, 100).expect("核心提取");
        assert_eq!((core.w, core.h), (70, 70), "非等比 resize 后取中心 70%");
    }

    /// 非等比语义：候选是 100x50 时模板必须被拉成 100x50（而不是等比留边）。
    #[test]
    fn template_core_resizes_non_uniformly_to_candidate_size() {
        let template = GrayImage::filled(20, 20, 128).expect("合法模板");
        let core = template_core(&template, 100, 50).expect("核心提取");
        // 上游 margin = max(1, int(dim*0.15)) 是**截断**：100→15（核心 70），50→7（核心 36）。
        assert_eq!((core.w, core.h), (70, 36), "宽高各自独立缩放，且按上游截断口径");
        assert!(core.as_slice().iter().all(|&v| v == 128));
    }

    /// 极小的候选格不能让核心区退化成 0 面积。
    #[test]
    fn template_core_never_collapses_to_zero_area() {
        let template = GrayImage::filled(8, 8, 90).expect("合法模板");
        let core = template_core(&template, 2, 2).expect("核心提取");
        assert_eq!((core.w, core.h), (1, 1));
    }

    #[test]
    fn match_cell_prefers_identical_content_over_unrelated() {
        let template = GrayImage::new(4, 1, vec![0, 255, 0, 255]).expect("合法模板");
        let same = template.resize_to(40, 40).expect("放大");
        let noise = GrayImage::new(
            40,
            40,
            (0..1600).map(|i| ((i * 37) % 251) as u8).collect::<Vec<u8>>(),
        )
        .expect("合法图");

        let identical = match_cell(&same, &template).expect("匹配");
        let unrelated = match_cell(&noise, &template).expect("匹配");
        assert!(identical > 0.99, "同源内容得分应接近 1：{identical}");
        assert!(identical > unrelated, "{identical} vs {unrelated}");
    }
}
