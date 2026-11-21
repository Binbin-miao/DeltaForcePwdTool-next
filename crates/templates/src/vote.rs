//! 全模板投票认角色（spec D2 主引擎）+ 二进制最大权匹配式指派（spec D4/D5）。
//!
//! 得分归一化：角色得分 = 该角色最大权匹配总分 / N。不除 N 的话，模板张数多的角色
//! （格赫罗斯 N=8）天然比张数少的角色（埃德温 N=4）占便宜，投票就失真了。
//!
//! 置信度门：最高分与次高分差 < [`AMBIGUOUS_GAP`]（0.05）时返回 [`Vote::Ambiguous`]，
//! 交给 `delta-ocr` 兜底，**不得硬选**（spec D2 双引擎契约）。

use std::collections::HashMap;

use delta_vision::{match_template_ncc, GrayImage};
use rayon::prelude::*;

use crate::matching::CoreCache;
use crate::{Role, RoleLibrary, TemplateError};

/// 上游硬编码真阈值（`fingerprint.py:96`）。配置项 `match_threshold` 是死配置，
/// 不得拿来当阈值（spec §2.2 第 10 条）。
pub const MIN_SCORE: f32 = 0.45;

/// 投票置信度门：最高分与次高分差小于此值判 `Ambiguous`（t4 任务 3）。
pub const AMBIGUOUS_GAP: f32 = 0.05;

/// 一条「格子 ↔ 模板」配对。点击序按 `tmpl_no` 升序，不是格子位置序（spec §2.2 不变量 3）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellAssignment {
    pub cell_idx: usize,
    pub tmpl_no: u32,
    pub score: f32,
}

/// 最优解中低于 `min_score` 的模板：**不得静默丢弃**，必须供上层告警（t4 任务 4）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnmatchedTemplate {
    pub tmpl_no: u32,
    /// 该模板在本行里的最高分格子，仅用于留证展示。
    pub best_cell: Option<usize>,
    pub best_score: f32,
}

/// 一个角色的完整指派结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub role_name: String,
    /// 按 `tmpl_no` 升序排列，可直接喂给点击序列（spec D5）。
    pub pairs: Vec<CellAssignment>,
    /// 分数不足或没格子可配的模板。
    pub unmatched: Vec<UnmatchedTemplate>,
    /// 匹配总分的归一化值（除以 N），与投票口径一致。
    pub score: f32,
}

impl Assignment {
    /// 本轮要点击的格子数（= 已配对模板数）。
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// N 个模板是否全部配上（spec D4 的完整性判据）。
    pub fn is_complete(&self) -> bool {
        self.unmatched.is_empty()
    }

    /// `(cell_idx, tmpl_no)` 列表，按模板编号升序。
    pub fn pair_list(&self) -> Vec<(usize, u32)> {
        self.pairs.iter().map(|pair| (pair.cell_idx, pair.tmpl_no)).collect()
    }
}

/// 一次投票的胜出结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Voted<'a> {
    pub role: &'a Role,
    pub score: f32,
    pub per_cell: Vec<CellAssignment>,
}

/// 投票结论。三态而不是 `Option`：`Ambiguous` 必须携带双方分数，UI 才能留证。
#[derive(Debug, Clone, PartialEq)]
pub enum Vote<'a> {
    Chosen(Voted<'a>),
    Ambiguous {
        best: Voted<'a>,
        runner_up: Voted<'a>,
        gap: f32,
    },
}

impl<'a> Vote<'a> {
    /// 胜出方（`Ambiguous` 时返回最高分那一侧，供 OCR 兜底时做交叉校验）。
    pub fn leader(&self) -> &Voted<'a> {
        match self {
            Self::Chosen(voted) => voted,
            Self::Ambiguous { best, .. } => best,
        }
    }

    /// 置信度是否足够直接采信。
    pub fn is_confident(&self) -> bool {
        matches!(self, Self::Chosen(_))
    }
}

/// 得分矩阵 `[模板下标][格子下标]`。按模板分片 rayon 并行，核心区按候选尺寸预缩放缓存。
pub fn score_matrix(cells: &[GrayImage], role: &Role) -> Result<Vec<Vec<f32>>, TemplateError> {
    if cells.is_empty() {
        return Err(TemplateError::NoCells);
    }
    if role.templates.is_empty() {
        return Err(TemplateError::NoTemplates {
            role: role.name.clone(),
        });
    }

    // 存活格尺寸通常一致；不一致时按尺寸分组建缓存，保持上游「按候选尺寸 resize」语义。
    let mut caches: HashMap<(u32, u32), CoreCache> = HashMap::new();
    for cell in cells {
        if let std::collections::hash_map::Entry::Vacant(slot) = caches.entry((cell.w, cell.h)) {
            slot.insert(CoreCache::build(role, cell.w, cell.h)?);
        }
    }

    let mut matrix = vec![vec![0.0f32; cells.len()]; role.templates.len()];
    // NCC 只在「核心区大于候选格」时报错，而核心区由候选尺寸派生，构造上不可能发生；
    // 仍不吞错：一旦发生就把第一个错误原样抛给调用方。
    let failure: std::sync::Mutex<Option<TemplateError>> = std::sync::Mutex::new(None);

    matrix.par_iter_mut().enumerate().for_each(|(tmpl_idx, row)| {
        for (cell_idx, cell) in cells.iter().enumerate() {
            let cache = &caches[&(cell.w, cell.h)];
            debug_assert_eq!(cache.size(), (cell.w, cell.h));
            match match_template_ncc(cell, &cache.cores()[tmpl_idx]) {
                Ok(found) => row[cell_idx] = found.score,
                Err(error) => {
                    let mut slot = failure.lock().unwrap_or_else(|poison| poison.into_inner());
                    slot.get_or_insert(TemplateError::Vision(error));
                    return;
                }
            }
        }
    });

    match failure.into_inner().unwrap_or_else(|poison| poison.into_inner()) {
        Some(error) => Err(error),
        None => Ok(matrix),
    }
}

/// 默认 `min_score = 0.45` 的指派。
pub fn assign(cells: &[GrayImage], role: &Role) -> Result<Assignment, TemplateError> {
    assign_with_min_score(cells, role, MIN_SCORE)
}

/// 二分图最大权匹配式指派：每个模板恰好用一次，得分不足者标记为未匹配。
pub fn assign_with_min_score(
    cells: &[GrayImage],
    role: &Role,
    min_score: f32,
) -> Result<Assignment, TemplateError> {
    let matrix = score_matrix(cells, role)?;
    let layout = crate::hungarian::solve_max_weight(&matrix);
    Ok(build_assignment(role, &matrix, &layout, min_score))
}

fn build_assignment(
    role: &Role,
    matrix: &[Vec<f32>],
    layout: &[Option<usize>],
    min_score: f32,
) -> Assignment {
    let template_count = role.templates.len();
    let mut pairs = Vec::with_capacity(template_count);
    let mut unmatched = Vec::new();
    let mut total = 0.0f32;

    for (tmpl_idx, slot) in layout.iter().enumerate() {
        let tmpl_no = role.templates[tmpl_idx].no;
        let row = &matrix[tmpl_idx];
        let best_cell = row
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.total_cmp(right))
            .map(|(idx, &score)| (idx, score));
        match slot {
            Some(cell_idx) if row[*cell_idx] >= min_score => {
                total += row[*cell_idx];
                pairs.push(CellAssignment {
                    cell_idx: *cell_idx,
                    tmpl_no,
                    score: row[*cell_idx],
                });
            }
            other => unmatched.push(UnmatchedTemplate {
                tmpl_no,
                best_cell: best_cell.map(|(idx, _)| idx).or(*other),
                best_score: other.map_or(0.0, |cell_idx| row[cell_idx]),
            }),
        }
    }

    pairs.sort_by_key(|pair| pair.tmpl_no);
    unmatched.sort_by_key(|entry| entry.tmpl_no);
    Assignment {
        role_name: role.name.clone(),
        pairs,
        unmatched,
        score: if template_count == 0 {
            0.0
        } else {
            total / template_count as f32
        },
    }
}

/// 对每个角色做全模板投票，返回胜者或 `Ambiguous`。
pub fn vote_role<'a>(
    cells: &[GrayImage],
    library: &'a RoleLibrary,
) -> Result<Vote<'a>, TemplateError> {
    vote_role_with_gap(cells, library, AMBIGUOUS_GAP)
}

/// 可调置信度门的投票（测试与 UI 调参用）。
pub fn vote_role_with_gap<'a>(
    cells: &[GrayImage],
    library: &'a RoleLibrary,
    gap: f32,
) -> Result<Vote<'a>, TemplateError> {
    if cells.is_empty() {
        return Err(TemplateError::NoCells);
    }
    if library.roles().is_empty() {
        return Err(TemplateError::NoRoles {
            root: std::path::PathBuf::new(),
        });
    }

    // 角色数 ≤ 9，逐个角色独立计算得分矩阵；矩阵内部已 rayon 并行。
    let mut scored: Vec<Voted<'a>> = Vec::with_capacity(library.roles().len());
    for role in library.roles() {
        let assignment = assign(cells, role)?;
        scored.push(Voted {
            role,
            score: assignment.score,
            per_cell: assignment.pairs,
        });
    }
    scored.sort_by(|left, right| right.score.total_cmp(&left.score));

    if scored.len() < 2 {
        return Ok(Vote::Chosen(scored.swap_remove(0)));
    }
    let runner_up = scored.remove(1);
    let best = scored.remove(0);
    let separation = best.score - runner_up.score;
    if separation < gap {
        Ok(Vote::Ambiguous {
            best,
            runner_up,
            gap: separation,
        })
    } else {
        Ok(Vote::Chosen(best))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Role, Template};

    fn striped(w: u32, h: u32, period: u32, seed: u8) -> GrayImage {
        let data = (0..w * h)
            .map(|i| {
                let x = i % w;
                if (x / period).is_multiple_of(2) {
                    seed
                } else {
                    seed.wrapping_add(120)
                }
            })
            .collect();
        GrayImage::new(w, h, data).expect("合法图")
    }

    fn role(name: &str, patterns: &[(u32, u8)]) -> Role {
        Role {
            name: name.to_string(),
            templates: patterns
                .iter()
                .enumerate()
                .map(|(idx, &(period, seed))| Template {
                    no: idx as u32 + 1,
                    image: striped(105, 105, period, seed),
                })
                .collect(),
        }
    }

    /// 候选格必须由模板本身缩放而来 —— 否则条纹周期会变，NCC 必然低（手绘图案不可行）。
    fn cell_from(template: &GrayImage, side: u32) -> GrayImage {
        template.resize_to(side, side).expect("缩放候选格")
    }

    #[test]
    fn score_matrix_is_template_major_and_normalised_by_n() {
        let alpha = role("甲", &[(7, 20), (13, 60)]);
        let cells = vec![
            cell_from(&alpha.templates[0].image, 84),
            cell_from(&alpha.templates[1].image, 84),
        ];
        let matrix = score_matrix(&cells, &alpha).expect("矩阵");
        assert_eq!(matrix.len(), 2, "行 = 模板数");
        assert_eq!(matrix[0].len(), 2, "列 = 格子数");
        assert!(matrix[0][0] > matrix[0][1], "模板 1 更像格子 1");
        assert!(matrix[1][1] > matrix[1][0], "模板 2 更像格子 2");

        let assignment = assign(&cells, &alpha).expect("指派");
        assert_eq!(assignment.pair_list(), vec![(0, 1), (1, 2)], "按模板号升序");
        assert!(assignment.is_complete(), "两格两模板必须全配上");
        assert!(assignment.score > 0.5, "归一化得分 {:?}", assignment.score);
    }

    #[test]
    fn assign_marks_low_score_templates_instead_of_dropping_them() {
        let alpha = role("甲", &[(7, 20)]);
        // 纯色格子：NCC 无处可对齐，得分必然远低于 0.45。
        let flat = GrayImage::filled(84, 84, 128).expect("合法图");
        let assignment = assign(&[flat], &alpha).expect("指派");
        assert!(assignment.is_empty(), "低分边不得算作已匹配");
        assert_eq!(assignment.unmatched.len(), 1, "必须留证");
        assert_eq!(assignment.unmatched[0].tmpl_no, 1);
        assert!(assignment.unmatched[0].best_score < MIN_SCORE);
        assert!(!assignment.is_complete());
    }

    #[test]
    fn vote_picks_the_matching_role_and_reports_ambiguity() {
        let alpha = role("甲", &[(7, 20), (13, 60)]);
        let beta = role("乙", &[(3, 200), (5, 10)]);
        let library = RoleLibrary::from_roles(vec![alpha, beta]).expect("库");
        let alpha_role = library.find("甲").expect("角色甲");
        let cells: Vec<GrayImage> = alpha_role
            .templates
            .iter()
            .map(|template| cell_from(&template.image, 84))
            .collect();
        let vote = vote_role(&cells, &library).expect("投票");
        assert_eq!(vote.leader().role.name, "甲", "同源内容必须投给甲：{vote:?}");
        assert!(vote.is_confident(), "同源内容的置信度必须够：{vote:?}");

        // 门开得极大时，任何分差都算歧义 —— 用于验证门确实生效。
        let ambiguous = vote_role_with_gap(&cells, &library, 10.0).expect("投票");
        assert!(!ambiguous.is_confident());
        match ambiguous {
            Vote::Ambiguous { best, runner_up, gap } => {
                assert_eq!(best.role.name, "甲", "最高分仍是甲");
                assert_eq!(runner_up.role.name, "乙");
                assert_eq!(gap, best.score - runner_up.score);
            }
            Vote::Chosen(_) => panic!("门 10.0 下不可能 Chosen"),
        }
    }

    #[test]
    fn empty_cells_are_an_error() {
        let library = RoleLibrary::from_roles(vec![role("甲", &[(7, 20)])]).expect("库");
        let error = vote_role(&[], &library).expect_err("空格子必须报错");
        assert!(matches!(error, TemplateError::NoCells), "{error:?}");
    }
}
