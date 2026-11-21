//! 二分图最大权匹配（匈牙利算法，O(n³) 手写，spec D4）。
//!
//! 为什么不能逐格 argmax：两个格子可能同时选中同一个模板号，而上游 `fingerprint.py:121-197`
//! 的「同模板号只能落一格、冲突取高分」本质就是指派问题。最大权匹配在数学上把这个
//! 约束吃进目标函数，天然满足 spec §2.2 不变量 2。
//!
//! 实现采用带势函数（`u` / `v`）的 JV 变体，方阵最小代价；最大权问题通过 `cost = -weight`
//! 转换。行/列数不等时补零代价哑元：
//!
//! - 本模块的行 = **模板**，列 = **候选格**；
//! - 真实场景存活格 = N+1 ≥ 模板数 N，所以每个模板都必须占一个**不同**的真实格，
//!   没有哑元列可用 —— 低分边由调用方按 `min_score` 标记为未匹配，而不是在这里跳过；
//! - 只有当模板数 > 格子数（异常输入）时才会出现哑元列，此时该模板返回 `None`。

/// 求解 `weights[行][列]` 的最大权和指派，返回每行匹配到的列（无则 `None`）。
///
/// 行数 = 模板数，列数 = 候选格数；每个行至多用一次、每个列至多用一次。
pub(crate) fn solve_max_weight(weights: &[Vec<f32>]) -> Vec<Option<usize>> {
    let rows = weights.len();
    let cols = weights.first().map_or(0, Vec::len);
    if rows == 0 || cols == 0 {
        return vec![None; rows];
    }
    let n = rows.max(cols);
    let mut cost = vec![vec![0.0f64; n]; n];
    for (r, row) in weights.iter().enumerate() {
        for (c, &weight) in row.iter().enumerate() {
            cost[r][c] = -f64::from(weight);
        }
    }
    let assignment = min_cost_assignment(&cost);
    (0..rows)
        .map(|r| {
            let c = assignment[r];
            (c < cols).then_some(c)
        })
        .collect()
}

/// 方阵最小代价指派（`p[j]` = 分给列 j 的行），返回 `row -> col`。
fn min_cost_assignment(cost: &[Vec<f64>]) -> Vec<usize> {
    let n = cost.len();
    const INF: f64 = f64::INFINITY;
    let mut row_potential = vec![0.0f64; n + 1];
    let mut col_potential = vec![0.0f64; n + 1];
    let mut row_of_col = vec![0usize; n + 1]; // 1-based 行号，0 = 空
    let mut previous_col = vec![0usize; n + 1];

    for row in 1..=n {
        row_of_col[0] = row;
        let mut current_col = 0usize;
        let mut min_reduced = vec![INF; n + 1];
        let mut visited = vec![false; n + 1];

        loop {
            visited[current_col] = true;
            let current_row = row_of_col[current_col];
            let mut delta = INF;
            let mut next_col = 0usize;
            for col in 1..=n {
                if visited[col] {
                    continue;
                }
                let reduced =
                    cost[current_row - 1][col - 1] - row_potential[current_row] - col_potential[col];
                if reduced < min_reduced[col] {
                    min_reduced[col] = reduced;
                    previous_col[col] = current_col;
                }
                if min_reduced[col] < delta {
                    delta = min_reduced[col];
                    next_col = col;
                }
            }
            for col in 0..=n {
                if visited[col] {
                    row_potential[row_of_col[col]] += delta;
                    col_potential[col] -= delta;
                } else {
                    min_reduced[col] -= delta;
                }
            }
            current_col = next_col;
            if row_of_col[current_col] == 0 {
                break;
            }
        }

        // 沿增广路回溯，重挂指派。
        loop {
            let parent = previous_col[current_col];
            row_of_col[current_col] = row_of_col[parent];
            current_col = parent;
            if current_col == 0 {
                break;
            }
        }
    }

    let mut row_to_col = vec![0usize; n];
    for col in 1..=n {
        if row_of_col[col] != 0 {
            row_to_col[row_of_col[col] - 1] = col - 1;
        }
    }
    row_to_col
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与「四个假设的全排列暴力枚举」对拍：最大权匹配的权和必须等于暴力最优。
    #[test]
    fn maximum_weight_matches_brute_force_on_permutations() {
        let cases: Vec<Vec<Vec<f32>>> = vec![
            // 逐格 argmax 会两次选同一列，最大权匹配不会。
            vec![vec![0.9, 0.8], vec![0.95, 0.1]],
            // 全负分：行 ≤ 列，仍必须强制指派，取「最不坏」的那个排列。
            vec![vec![-0.5, -0.7], vec![-0.2, -0.9]],
            // 方阵对角优势。
            vec![vec![0.1, 0.9, 0.2], vec![0.8, 0.1, 0.3], vec![0.2, 0.3, 0.95]],
            // 行多于列（模板多于格子）：必须有两行落空。
            vec![
                vec![0.9, 0.1],
                vec![0.85, 0.2],
                vec![0.1, 0.95],
                vec![0.2, 0.3],
            ],
            // 列多于行（格子多于模板 = 真实场景 N+1 格 / N 模板）。
            vec![
                vec![0.4, 0.9, 0.1],
                vec![0.95, 0.2, 0.3],
            ],
        ];

        for weights in cases {
            let got = solve_max_weight(&weights);
            let got_weight = total(&weights, &got);
            let best = brute_force(&weights);
            assert!(
                (got_weight - best).abs() < 1e-5,
                "最大权 {got_weight} 必须等于暴力最优 {best}：{weights:?} -> {got:?}"
            );
            // 列不重复。
            let mut used: Vec<usize> = got.iter().flatten().copied().collect();
            let before = used.len();
            used.sort_unstable();
            used.dedup();
            assert_eq!(before, used.len(), "同一列被用了两次：{got:?}");
        }
    }

    #[test]
    fn empty_and_degenerate_inputs_return_no_pairs() {
        assert!(solve_max_weight(&[]).is_empty());
        assert_eq!(solve_max_weight(&[vec![]]), vec![None]);
    }

    fn total(weights: &[Vec<f32>], assignment: &[Option<usize>]) -> f32 {
        assignment
            .iter()
            .enumerate()
            .filter_map(|(row, col)| col.map(|col| weights[row][col]))
            .sum()
    }

    /// 暴力法：深度优先枚举所有「列不重复」的指派。
    ///
    /// 与求解器同构：只有「剩余行数 > 空闲列数」时该行才允许落空（贡献 0），
    /// 也就是行 ≤ 列时每行都必须占一个不同列（真实场景 N 模板 / N+1 格）。
    fn brute_force(weights: &[Vec<f32>]) -> f32 {
        fn dfs(weights: &[Vec<f32>], row: usize, used: &mut [bool]) -> f32 {
            if row == weights.len() {
                return 0.0;
            }
            let free = used.iter().filter(|taken| !**taken).count();
            let mut best = f32::NEG_INFINITY;
            if weights.len() - row > free {
                best = dfs(weights, row + 1, used);
            }
            for col in 0..weights[row].len() {
                if used[col] {
                    continue;
                }
                used[col] = true;
                best = best.max(weights[row][col] + dfs(weights, row + 1, used));
                used[col] = false;
            }
            best
        }
        let mut used = vec![false; weights[0].len()];
        dfs(weights, 0, &mut used)
    }
}
